//! installed desktop applications, rescanned when application directories change.

mod activation;
mod entry;
mod plan;
#[cfg(test)]
pub(crate) mod test_bus;

use std::{
    collections::BTreeMap,
    ffi::CString,
    fs, io,
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use calloop::ping::Ping;
use thing_runtime::ApplicationsSnapshot;

use entry::{Entry, Environment};

pub(crate) use activation::{ActivationCall, Activator};
pub(crate) use plan::{Plan, plan};

const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;
const MAX_WATCHES: usize = 1024;
const MAX_WATCH_DEPTH: usize = 8;
// package managers write many files at once, so wait for a quiet moment.
const QUIET_PERIOD: Duration = Duration::from_millis(250);
const MAX_SETTLE: Duration = Duration::from_secs(2);
// without working watches, such as at the inotify limit, rescan occasionally.
const UNWATCHED_RETRY: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Shared {
    latest: Option<Published>,
    stop: bool,
}

/// one generation of the list, with the launch data that belongs to it.
pub(crate) struct Published {
    pub snapshot: ApplicationsSnapshot,
    pub catalog: Arc<Catalog>,
}

/// the launch templates of every listed application, keyed by desktop file id.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    entries: BTreeMap<String, Entry>,
}

impl Catalog {
    fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id)
    }

    /// the revision of a listed entry, or `None` once it is gone.
    pub(crate) fn revision(&self, id: &str) -> Option<u64> {
        self.get(id).map(|entry| entry.application.revision)
    }
}

/// gives each entry a revision that survives rescans that leave it unchanged,
/// so a launch queued before an unrelated change still applies.
#[derive(Default)]
struct Revisions {
    known: BTreeMap<String, (Entry, u64)>,
    next: u64,
}

impl Revisions {
    fn assign(&mut self, entries: Vec<Entry>) -> Vec<Entry> {
        let mut known = BTreeMap::new();
        let entries = entries
            .into_iter()
            .map(|mut entry| {
                let id = entry.application.id.clone();
                let revision = match self.known.remove(&id) {
                    Some((previous, revision)) if previous == entry => revision,
                    _ => {
                        self.next += 1;
                        self.next
                    }
                };
                known.insert(id, (entry.clone(), revision));
                entry.application.revision = revision;
                entry
            })
            .collect();
        self.known = known;
        entries
    }
}

pub(crate) struct Applications {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl Applications {
    pub fn start(ping: Ping) -> io::Result<Self> {
        Self::start_with(Environment::from_process(), ping)
    }

    /// watches only the given application directories, for host tests.
    #[cfg(test)]
    pub(crate) fn start_in(directories: Vec<std::path::PathBuf>, ping: Ping) -> io::Result<Self> {
        Self::start_with(
            Environment {
                directories,
                ..Environment::default()
            },
            ping,
        )
    }

    fn start_with(environment: Environment, ping: Ping) -> io::Result<Self> {
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("thing-applications".into())
            .spawn(move || run(&environment, &state, &reader, &ping))?;
        Ok(Self {
            shared,
            wake,
            worker: Some(worker),
        })
    }

    pub fn take_snapshot(&self) -> Option<Published> {
        lock(&self.shared).latest.take()
    }
}

impl Drop for Applications {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        let _ = (&self.wake).write(&[1]);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run(environment: &Environment, shared: &Mutex<Shared>, wake: &UnixStream, ping: &Ping) {
    let mut previous = None;
    let mut revisions = Revisions::default();
    loop {
        if lock(shared).stop {
            return;
        }
        // install watches before scanning, so a change during the scan
        // triggers another one rather than being missed.
        let watches = Watches::install(environment);
        let entries = revisions.assign(entry::scan(environment));
        let snapshot = ApplicationsSnapshot {
            available: true,
            error: watches
                .as_ref()
                .err()
                .map(|error| format!("cannot watch application directories: {error}"))
                .unwrap_or_default(),
            applications: entries
                .iter()
                .map(|entry| entry.application.clone())
                .collect(),
        };
        if previous.as_ref() != Some(&snapshot) {
            previous = Some(snapshot.clone());
            let catalog = Catalog {
                entries: entries
                    .into_iter()
                    .map(|entry| (entry.application.id.clone(), entry))
                    .collect(),
            };
            lock(shared).latest = Some(Published {
                snapshot,
                catalog: Arc::new(catalog),
            });
            ping.ping();
        }
        let inotify = watches.as_ref().ok().map(|watches| watches.fd.as_raw_fd());
        if !wait(inotify, wake) {
            return;
        }
        let started = Instant::now();
        while started.elapsed() < MAX_SETTLE {
            if let Some(fd) = inotify {
                drain(fd);
            }
            let remaining = MAX_SETTLE.saturating_sub(started.elapsed());
            let quiet = QUIET_PERIOD.min(remaining);
            match poll(inotify, wake, Some(quiet)) {
                Poll::Timeout => break,
                Poll::Wake => {
                    if lock(shared).stop {
                        return;
                    }
                    drain(wake.as_raw_fd());
                }
                Poll::Changed => {}
            }
        }
    }
}

/// waits for a directory change, or for a retry when nothing is watched. it
/// returns false when the service stops.
fn wait(inotify: Option<i32>, wake: &UnixStream) -> bool {
    let timeout = inotify.is_none().then_some(UNWATCHED_RETRY);
    match poll(inotify, wake, timeout) {
        Poll::Changed | Poll::Timeout => true,
        Poll::Wake => false,
    }
}

enum Poll {
    Changed,
    Wake,
    Timeout,
}

fn poll(inotify: Option<i32>, wake: &UnixStream, timeout: Option<Duration>) -> Poll {
    let mut descriptors = [
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            // poll ignores negative descriptors, which covers a failed watch.
            fd: inotify.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let milliseconds = timeout.map_or(-1, |timeout| {
        timeout.as_millis().clamp(0, i32::MAX as u128) as i32
    });
    loop {
        // SAFETY: descriptors is an initialized array of two pollfd values.
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, milliseconds) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // a broken poll would spin, so treat it like a stop request.
            return Poll::Wake;
        }
        if result == 0 {
            return Poll::Timeout;
        }
        if descriptors[0].revents != 0 {
            return Poll::Wake;
        }
        return Poll::Changed;
    }
}

fn drain(fd: i32) {
    let mut buffer = [0_u8; 8192];
    loop {
        // SAFETY: buffer is writable for its full length and fd is open.
        let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count > 0 {
            continue;
        }
        if count < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return;
    }
}

struct Watches {
    fd: OwnedFd,
}

impl Watches {
    /// watches every existing application directory and its subdirectories,
    /// and the nearest existing ancestor of missing ones to see their creation.
    fn install(environment: &Environment) -> io::Result<Self> {
        // SAFETY: inotify_init1 takes only flags and returns a new owned fd.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let watches = Self {
            // SAFETY: fd is a successful, uniquely owned inotify descriptor.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        };
        let mut count = 0;
        for directory in &environment.directories {
            if directory.is_dir() {
                watches.add_tree(directory, 0, &mut count)?;
            } else if let Some(parent) =
                directory.ancestors().skip(1).find(|parent| parent.is_dir())
            {
                watches.add(parent, &mut count)?;
            }
        }
        Ok(watches)
    }

    fn add_tree(&self, directory: &Path, depth: usize, count: &mut usize) -> io::Result<()> {
        self.add(directory, count)?;
        if depth >= MAX_WATCH_DEPTH {
            return Ok(());
        }
        let Ok(entries) = fs::read_dir(directory) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                self.add_tree(&entry.path(), depth + 1, count)?;
            }
        }
        Ok(())
    }

    fn add(&self, directory: &Path, count: &mut usize) -> io::Result<()> {
        if *count >= MAX_WATCHES {
            return Err(io::Error::other("too many application directories"));
        }
        let name = CString::new(directory.as_os_str().as_bytes())?;
        // SAFETY: fd is open and name is a nul-terminated path for this call.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), name.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            let error = io::Error::last_os_error();
            // a directory removed since it was listed is simply not watched.
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) {
                return Ok(());
            }
            return Err(error);
        }
        *count += 1;
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn wait_for(
        applications: &Applications,
        predicate: impl Fn(&ApplicationsSnapshot) -> bool,
    ) -> ApplicationsSnapshot {
        wait_for_published(applications, predicate).snapshot
    }

    fn wait_for_published(
        applications: &Applications,
        predicate: impl Fn(&ApplicationsSnapshot) -> bool,
    ) -> Published {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut observed = Vec::new();
        while Instant::now() < deadline {
            if let Some(published) = applications.take_snapshot() {
                if predicate(&published.snapshot) {
                    return published;
                }
                observed.push(published.snapshot);
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for applications; observed {observed:?}");
    }

    fn names(snapshot: &ApplicationsSnapshot) -> Vec<&str> {
        snapshot
            .applications
            .iter()
            .map(|application| application.name.as_str())
            .collect()
    }

    fn entry(name: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec=run\n")
    }

    #[test]
    fn directory_changes_are_rescanned_without_polling() {
        let data = tempfile::TempDir::new().unwrap();
        let system = tempfile::TempDir::new().unwrap();
        let user_applications = data.path().join("applications");
        let system_applications: PathBuf = system.path().join("applications");
        fs::create_dir_all(system_applications.join("vendor")).unwrap();
        fs::write(
            system_applications.join("editor.desktop"),
            entry("system editor"),
        )
        .unwrap();
        let environment = Environment {
            directories: vec![user_applications.clone(), system_applications.clone()],
            ..Environment::default()
        };
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let applications = Applications::start_with(environment, ping).unwrap();

        let snapshot = wait_for(&applications, |snapshot| snapshot.available);
        assert!(snapshot.error.is_empty());
        assert_eq!(names(&snapshot), ["system editor"]);

        // a file in an existing subdirectory, then a directory that did not
        // exist when the service started.
        fs::write(
            system_applications.join("vendor/tool.desktop"),
            entry("tool"),
        )
        .unwrap();
        wait_for(&applications, |snapshot| names(snapshot).contains(&"tool"));
        fs::create_dir(&user_applications).unwrap();
        fs::write(
            user_applications.join("editor.desktop"),
            entry("user editor"),
        )
        .unwrap();
        let snapshot = wait_for(&applications, |snapshot| {
            names(snapshot).contains(&"user editor")
        });
        assert_eq!(names(&snapshot), ["user editor", "tool"]);

        fs::remove_file(user_applications.join("editor.desktop")).unwrap();
        wait_for(&applications, |snapshot| {
            names(snapshot) == ["system editor", "tool"]
        });
        drop(applications);
    }

    #[test]
    fn application_directories_created_below_missing_parents_are_seen() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("new/share/applications");
        let environment = Environment {
            directories: vec![path.clone()],
            ..Environment::default()
        };
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let applications = Applications::start_with(environment, ping).unwrap();
        wait_for(&applications, |snapshot| snapshot.available);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("editor.desktop"), entry("editor")).unwrap();
        wait_for(&applications, |snapshot| names(snapshot) == ["editor"]);
    }

    #[test]
    fn revisions_survive_unrelated_changes_and_follow_edits() {
        let directory = tempfile::TempDir::new().unwrap();
        let write = |name: &str, exec: &str| {
            fs::write(
                directory.path().join(name),
                format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n"),
            )
            .unwrap();
        };
        write("editor.desktop", "edit %d");
        write("viewer.desktop", "view %f");
        let environment = Environment {
            directories: vec![directory.path().into()],
            ..Environment::default()
        };
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let applications = Applications::start_with(environment, ping).unwrap();
        let revisions = |published: &Published| {
            published
                .snapshot
                .applications
                .iter()
                .map(|application| {
                    let entry = published.catalog.get(&application.id).unwrap();
                    assert_eq!(entry.application, *application);
                    (application.id.clone(), application.revision)
                })
                .collect::<BTreeMap<_, _>>()
        };
        let first = revisions(&wait_for_published(&applications, |snapshot| {
            snapshot.applications.len() == 2
        }));

        // another entry appearing leaves existing revisions alone.
        write("tool.desktop", "tool");
        let second = revisions(&wait_for_published(&applications, |snapshot| {
            snapshot.applications.len() == 3
        }));
        assert_eq!(second["editor.desktop"], first["editor.desktop"]);
        assert_eq!(second["viewer.desktop"], first["viewer.desktop"]);

        // a deprecated code changes the template but not the listed command,
        // and the revision still follows it.
        write("editor.desktop", "edit %n");
        let third = wait_for_published(&applications, |snapshot| {
            snapshot.applications[0].revision != first["editor.desktop"]
        });
        assert_eq!(third.snapshot.applications[0].command, ["edit"]);
        let third = revisions(&third);
        assert_eq!(third["viewer.desktop"], first["viewer.desktop"]);
        assert_eq!(third["tool.desktop"], second["tool.desktop"]);
    }

    #[test]
    fn shutdown_is_prompt_while_waiting() {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let applications = Applications::start_with(Environment::default(), ping).unwrap();
        wait_for(&applications, |snapshot| snapshot.available);
        let started = Instant::now();
        drop(applications);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
