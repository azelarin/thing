use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, RawFd},
        unix::{fs::OpenOptionsExt, net::UnixStream},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use calloop::ping::Ping;
use thing_runtime::{FileWatchSnapshot, MAX_WATCHED_FILE_BYTES};

use super::{Shared, cancelled, failed, lock, publish};
use crate::watch::Watch;

// a stopped instance can still have an uninterruptible read. retain its
// worker slot until it exits instead of accumulating replacement threads.
static ACTIVE: AtomicBool = AtomicBool::new(false);
const FIRST_RETRY: Duration = Duration::from_millis(100);
const MAX_RETRY: Duration = Duration::from_secs(30);
const MAX_LOCATIONS: usize = 256;

struct Lease;
impl Lease {
    fn acquire() -> io::Result<Self> {
        ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| io::Error::other("a file watch worker is already active"))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

pub(super) struct Worker {
    control: UnixStream,
    // dropping a join handle detaches it. shutdown wakes poll but never waits
    // on a filesystem read that the kernel might not let us interrupt.
    _thread: JoinHandle<()>,
}

impl Worker {
    pub(super) fn new(shared: Arc<Mutex<Shared>>, ping: Ping) -> io::Result<Self> {
        let lease = Lease::acquire()?;
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        let thread = thread::Builder::new()
            .name("thing-files".into())
            .spawn(move || {
                let _lease = lease;
                run(reader, shared, ping);
            })?;
        Ok(Self {
            control: writer,
            _thread: thread,
        })
    }
    pub(super) fn wake(&mut self) {
        match self.control.write(&[1]) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => eprintln!("thing: cannot wake file watcher: {error}"),
        }
    }
}

struct File {
    watch: Option<Watch>,
    snapshot: Option<FileWatchSnapshot>,
    dirty: bool,
    retry: Option<Instant>,
    delay: Duration,
}
impl Default for File {
    fn default() -> Self {
        Self {
            watch: None,
            snapshot: None,
            dirty: true,
            retry: None,
            delay: FIRST_RETRY,
        }
    }
}
impl File {
    fn refresh(&mut self, path: &str) -> io::Result<FileWatchSnapshot> {
        if self.watch.is_none() {
            self.watch = Some(Watch::with_attributes()?);
        }
        let watch = self.watch.as_ref().expect("the watch was installed");
        for _ in 0..4 {
            let before = locations(Path::new(path))?;
            watch.set_files(&before.iter().cloned().collect::<Vec<_>>())?;
            let snapshot = read(Path::new(path));
            if before == locations(Path::new(path))? {
                return Ok(snapshot);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the watched path keeps changing",
        ))
    }
    fn failed(&mut self, error: &io::Error) {
        self.snapshot = Some(failed(error));
        self.retry = Some(Instant::now() + self.delay);
        self.delay = (self.delay * 2).min(MAX_RETRY);
    }
}

fn run(mut control: UnixStream, shared: Arc<Mutex<Shared>>, ping: Ping) {
    let mut files = BTreeMap::<String, File>::new();
    let mut generation = 0;
    loop {
        let request = {
            let mut shared = lock(&shared);
            if shared.stopped {
                return;
            }
            shared.request.take()
        };
        if let Some(request) = request {
            generation = request.generation;
            files.retain(|path, _| request.paths.contains(path));
            for path in request.paths {
                let file = files.entry(path.clone()).or_default();
                if let Some(snapshot) = &file.snapshot {
                    publish(&shared, &ping, generation, &path, snapshot.clone());
                }
                file.dirty = true;
            }
        }
        if cancelled(&shared, generation) {
            continue;
        }
        for (path, file) in &mut files {
            if cancelled(&shared, generation) {
                break;
            }
            if file.dirty
                || file
                    .retry
                    .is_some_and(|deadline| deadline <= Instant::now())
            {
                file.dirty = false;
                match file.refresh(path) {
                    Ok(snapshot) => {
                        file.snapshot = Some(snapshot);
                        file.retry = None;
                        file.delay = FIRST_RETRY;
                    }
                    Err(error) => file.failed(&error),
                }
                if let Some(snapshot) = &file.snapshot {
                    publish(&shared, &ping, generation, path, snapshot.clone());
                }
            }
        }
        if cancelled(&shared, generation) {
            continue;
        }
        let mut polls = vec![poll_fd(control.as_raw_fd())];
        let mut paths = Vec::new();
        for (path, file) in &files {
            if let Some(watch) = &file.watch {
                polls.push(poll_fd(watch.as_fd().as_raw_fd()));
                paths.push(path.clone());
            }
        }
        let timeout = files
            .values()
            .filter_map(|file| file.retry)
            .min()
            .map_or(-1, |deadline| {
                let wait = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis();
                (wait + 1).min(i32::MAX as u128) as i32
            });
        // SAFETY: polls is initialized and writable for the duration of poll;
        // every descriptor is held by control or a live file watch.
        let count = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("thing: file watcher poll failed: {error}");
            return;
        }
        if polls[0].revents != 0 {
            let mut buffer = [0u8; 128];
            loop {
                match control.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        for (path, poll) in paths.into_iter().zip(polls.into_iter().skip(1)) {
            if poll.revents == 0 {
                continue;
            }
            let file = files.get_mut(&path).expect("the polled file is current");
            let changed = file
                .watch
                .as_ref()
                .expect("only live watches were polled")
                .changed();
            match changed {
                Ok(true) => file.dirty = true,
                Ok(false) => {}
                Err(error) => {
                    file.watch = None;
                    file.failed(&error);
                    publish(
                        &shared,
                        &ping,
                        generation,
                        &path,
                        file.snapshot.clone().expect("a failure has a snapshot"),
                    );
                }
            }
        }
    }
}

fn poll_fd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

fn read(path: &Path) -> FileWatchSnapshot {
    let result = (|| -> io::Result<String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("watched data must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_WATCHED_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_WATCHED_FILE_BYTES {
            return Err(io::Error::other("watched data exceeds 128 KiB"));
        }
        let text =
            String::from_utf8(bytes).map_err(|_| io::Error::other("watched text must be utf-8"))?;
        if text.contains('\0') {
            return Err(io::Error::other("watched text must not contain nul"));
        }
        Ok(text)
    })();
    match result {
        Ok(text) => FileWatchSnapshot::Ready(Arc::from(text)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            FileWatchSnapshot::Missing
        }
        Err(error) => failed(&error),
    }
}

enum Part {
    Root,
    Parent,
    Name(OsString),
}
fn parts(path: &Path) -> io::Result<VecDeque<Part>> {
    path.components()
        .filter_map(|component| match component {
            Component::CurDir => None,
            Component::RootDir => Some(Ok(Part::Root)),
            Component::ParentDir => Some(Ok(Part::Parent)),
            Component::Normal(name) => Some(Ok(Part::Name(name.to_owned()))),
            Component::Prefix(_) => Some(Err(io::Error::other("unsupported path prefix"))),
        })
        .collect()
}

/// include intermediate links and missing parents, so changing either a
/// symlink or its target wakes the same file subscription.
fn locations(path: &Path) -> io::Result<BTreeSet<PathBuf>> {
    let mut pending = parts(path)?;
    let mut cursor = PathBuf::from("/");
    let mut result = BTreeSet::new();
    let mut links = 0;
    let mut steps = 0;
    while let Some(part) = pending.pop_front() {
        steps += 1;
        if steps > 512 {
            return Err(io::Error::other(
                "watched path resolution exceeds 512 steps",
            ));
        }
        match part {
            Part::Root => cursor = PathBuf::from("/"),
            Part::Parent => {
                cursor.pop();
            }
            Part::Name(name) => {
                cursor.push(name);
                result.insert(cursor.clone());
                match fs::symlink_metadata(&cursor) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        links += 1;
                        if links > 40 {
                            return Err(io::Error::other("watched path exceeds 40 symlinks"));
                        }
                        let target = fs::read_link(&cursor)?;
                        cursor.pop();
                        let mut prefix = parts(&target)?;
                        prefix.append(&mut pending);
                        pending = prefix;
                    }
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if result.len() > MAX_LOCATIONS {
            return Err(io::Error::other(
                "watched path exceeds 256 observed locations",
            ));
        }
    }
    Ok(result)
}

#[cfg(test)]
pub(super) fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}
