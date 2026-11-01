//! http and https image downloads through a curl helper.
//!
//! curl is run directly with an argument vector, never through a shell. the
//! url travels through curl's `--config -` stdin, so it never appears in the
//! process list. curl verifies tls certificates against the system trust
//! store, ignores `~/.curlrc`, follows at most a few redirects, and only to
//! http and https. thing enforces its own byte limit and deadline as well, and
//! kills the helper when a download is cancelled or the shell stops.

use std::{
    collections::{BTreeSet, VecDeque},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

/// what to download: a url and the configuration's refresh token.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Identity {
    pub(super) url: String,
    pub(super) revision: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct Limits {
    pub(super) program: PathBuf,
    pub(super) workers: usize,
    pub(super) queue: usize,
    pub(super) max_bytes: u64,
    pub(super) redirects: u32,
    pub(super) connect: Duration,
    /// the whole download, including redirects.
    pub(super) overall: Duration,
    /// a transfer slower than one byte per second for this long fails.
    pub(super) stall: Duration,
    /// ignore proxy variables, so loopback fixtures stay private.
    pub(super) direct: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            program: PathBuf::from("curl"),
            workers: 2,
            queue: 16,
            max_bytes: super::MAX_INPUT,
            redirects: 3,
            connect: Duration::from_secs(5),
            overall: Duration::from_secs(20),
            stall: Duration::from_secs(10),
            direct: false,
        }
    }
}

pub(super) type Outcome = Result<Arc<[u8]>, String>;

#[derive(Default)]
struct State {
    queue: VecDeque<(u64, Identity)>,
    running: BTreeSet<u64>,
    cancelled: BTreeSet<u64>,
    done: Vec<(u64, Identity, Outcome)>,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// a small pool of download workers, started on the first remote image.
pub(super) struct Fetcher {
    shared: Arc<Shared>,
    limits: Limits,
    next_id: u64,
}

impl Fetcher {
    pub(super) fn start(limits: Limits, wake: Arc<dyn Fn() + Send + Sync>) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        // if a later thread cannot start, dropping the pool stops the workers
        // that already started instead of leaving them waiting forever.
        let fetcher = Self {
            shared: shared.clone(),
            limits: limits.clone(),
            next_id: 1,
        };
        for index in 0..limits.workers {
            let shared = shared.clone();
            let limits = limits.clone();
            let wake = wake.clone();
            thread::Builder::new()
                .name(format!("thing-image-fetch-{index}"))
                .spawn(move || work(&shared, &limits, wake.as_ref()))?;
        }
        Ok(fetcher)
    }

    /// queues a download, or returns `None` while the queue is full.
    pub(super) fn submit(&mut self, identity: Identity) -> Option<u64> {
        let mut state = lock(&self.shared.state);
        if state.queue.len() >= self.limits.queue {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        state.queue.push_back((id, identity));
        drop(state);
        self.shared.changed.notify_one();
        Some(id)
    }

    /// stops a download. a queued one never starts; a running one is killed
    /// and reports nothing.
    pub(super) fn cancel(&mut self, id: u64) {
        let mut state = lock(&self.shared.state);
        state.queue.retain(|(queued, _)| *queued != id);
        if state.running.contains(&id) {
            state.cancelled.insert(id);
        }
        state.done.retain(|(done, _, _)| *done != id);
        drop(state);
        self.shared.changed.notify_all();
    }

    pub(super) fn take_done(&mut self) -> Vec<(u64, Identity, Outcome)> {
        let done = std::mem::take(&mut lock(&self.shared.state).done);
        self.shared.changed.notify_all();
        done
    }

    #[cfg(test)]
    pub(super) fn active(&self) -> usize {
        let state = lock(&self.shared.state);
        state.queue.len() + state.running.len()
    }

    #[cfg(test)]
    pub(super) fn completed(&self) -> usize {
        lock(&self.shared.state).done.len()
    }
}

impl Drop for Fetcher {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        state.stop = true;
        state.queue.clear();
        drop(state);
        // running downloads see the flag within one poll interval and kill
        // their helpers; nothing here waits for the network.
        self.shared.changed.notify_all();
    }
}

fn work(shared: &Shared, limits: &Limits, wake: &(dyn Fn() + Send + Sync)) {
    loop {
        let (id, identity) = {
            let mut state = lock(&shared.state);
            loop {
                if state.stop {
                    return;
                }
                // completed bytes still occupy a worker's slot until the UI
                // takes them. a stalled UI cannot accumulate download bodies.
                if state.done.len() + state.running.len() < limits.workers
                    && let Some(next) = state.queue.pop_front()
                {
                    state.running.insert(next.0);
                    break next;
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        let outcome = download(&identity.url, limits, || {
            let state = lock(&shared.state);
            state.stop || state.cancelled.contains(&id)
        });
        let mut state = lock(&shared.state);
        state.running.remove(&id);
        let cancelled = state.cancelled.remove(&id);
        if state.stop {
            return;
        }
        if !cancelled {
            state.done.push((id, identity, outcome));
            drop(state);
            wake();
        }
    }
}

/// the argument vector, without the url, which goes through stdin.
pub(super) fn arguments(limits: &Limits) -> Vec<String> {
    let seconds = |duration: Duration| format!("{:.3}", duration.as_secs_f64());
    [
        // `-q` must come first: it skips `~/.curlrc`.
        "-q",
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--proto",
        "=http,https",
        "--proto-redir",
        "=http,https",
        "--max-redirs",
    ]
    .into_iter()
    .map(String::from)
    .chain([limits.redirects.to_string()])
    .chain(["--connect-timeout".into(), seconds(limits.connect)])
    .chain(["--max-time".into(), seconds(limits.overall)])
    .chain(["--speed-limit".into(), "1".into()])
    .chain([
        "--speed-time".into(),
        limits.stall.as_secs().max(1).to_string(),
    ])
    .chain(["--max-filesize".into(), limits.max_bytes.to_string()])
    .chain(
        limits
            .direct
            .then(|| ["--noproxy".to_owned(), "*".to_owned()])
            .into_iter()
            .flatten(),
    )
    .chain(["--config".into(), "-".into(), "--output".into(), "-".into()])
    .collect()
}

/// a curl config line naming the url, quoted so no option can be injected.
pub(super) fn config(url: &str) -> Result<String, String> {
    if url.chars().any(|character| character.is_control()) {
        return Err("image url contains control characters".into());
    }
    let mut line = String::from("url = \"");
    for character in url.chars() {
        if matches!(character, '"' | '\\') {
            line.push('\\');
        }
        line.push(character);
    }
    line.push_str("\"\n");
    Ok(line)
}

fn download(url: &str, limits: &Limits, cancelled: impl Fn() -> bool) -> Outcome {
    let config = config(url)?;
    let mut command = Command::new(&limits.program);
    command
        .args(arguments(limits))
        .env_remove("CURL_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // helper diagnostics can contain a redirect's credentials or query.
        // report the exit category instead of forwarding arbitrary stderr.
        .stderr(Stdio::null());
    // SAFETY: the closure runs in the forked child before exec and calls only
    // async-signal-safe functions. the helper starts with no blocked signals
    // and dies with the thread that started it.
    unsafe {
        command.pre_exec(|| {
            let mut empty = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(empty.as_mut_ptr());
            libc::pthread_sigmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut());
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("remote images need curl: {error}"))?;
    let result = transfer(&mut child, &config, limits, cancelled);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = finish(&mut child);
    let bytes = result?;
    match status {
        Some(status) if status.success() => Ok(bytes.into()),
        Some(status) => Err(match status.code() {
            Some(6) => "cannot resolve the image host".into(),
            Some(7) => "cannot connect to the image host".into(),
            Some(22) => "the image server returned an HTTP error".into(),
            Some(28) => "the image download timed out".into(),
            Some(47) => "the image download exceeded the redirect limit".into(),
            Some(60) => "the image server's TLS certificate could not be verified".into(),
            Some(63) => format!("image exceeds {} MiB", limits.max_bytes / (1024 * 1024)),
            Some(code) => format!("curl exited with status {code}"),
            None => "the image download was interrupted".into(),
        }),
        None => Err("the image download did not finish".into()),
    }
}

fn transfer(
    child: &mut Child,
    config: &str,
    limits: &Limits,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>, String> {
    // include the stdin handoff in the deadline: a stuck helper may stop
    // reading before curl can install its own network timeouts.
    let deadline = Instant::now() + limits.overall + limits.connect + Duration::from_secs(1);
    let mut stdin = child.stdin.take().ok_or("curl has no stdin")?;
    nonblocking(stdin.as_raw_fd())?;
    let mut remaining = config.as_bytes();
    while !remaining.is_empty() {
        wait_for_pipe(stdin.as_raw_fd(), libc::POLLOUT, deadline, &cancelled)?;
        match stdin.write(remaining) {
            Ok(0) => return Err("curl closed its input".into()),
            Ok(written) => remaining = &remaining[written..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(format!("cannot pass the url to curl: {error}")),
        }
    }
    drop(stdin);
    let mut stdout = child.stdout.take().ok_or("curl has no stdout")?;
    nonblocking(stdout.as_raw_fd())?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        wait_for_pipe(stdout.as_raw_fd(), libc::POLLIN, deadline, &cancelled)?;
        match stdout.read(&mut buffer) {
            Ok(0) => return Ok(bytes),
            Ok(read) => {
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.len() as u64 > limits.max_bytes {
                    return Err(format!(
                        "image exceeds {} MiB",
                        limits.max_bytes / (1024 * 1024)
                    ));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn nonblocking(fd: std::os::fd::RawFd) -> Result<(), String> {
    // SAFETY: each caller owns the pipe and keeps it alive for this call.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(format!(
                "cannot monitor the image download: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn wait_for_pipe(
    fd: std::os::fd::RawFd,
    events: i16,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<(), String> {
    if cancelled() {
        return Err("the image download was cancelled".into());
    }
    let now = Instant::now();
    if now >= deadline {
        return Err("the image download timed out".into());
    }
    let wait = (deadline - now).min(Duration::from_millis(50));
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one initialized pollfd, with its pipe alive until the call returns.
    if unsafe { libc::poll(&mut descriptor, 1, wait.as_millis() as i32) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(format!("cannot monitor the image download: {error}"));
        }
    }
    Ok(())
}

/// reaps the helper, killing it if it lingers after its output closed.
fn finish(child: &mut Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                return child.wait().ok();
            }
            Err(_) => return None,
        }
    }
}

/// a url safe to log: the scheme, host, port, and path, without credentials,
/// query, or fragment.
pub(super) fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..end];
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_interrupts_a_full_helper_input_pipe() {
        let mut child = Command::new("/bin/sleep")
            .arg("2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let fd = child.stdin.as_ref().unwrap().as_raw_fd();
        // SAFETY: the live child owns this pipe. reducing its capacity makes
        // the blocked-writer case independent of the machine's pipe defaults.
        assert!(unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, 4096) } >= 4096);
        let started = Instant::now();
        let result = transfer(
            &mut child,
            &"x".repeat(16 * 1024),
            &Limits::default(),
            || started.elapsed() >= Duration::from_millis(50),
        );
        let elapsed = started.elapsed();
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            elapsed < Duration::from_secs(1),
            "cancellation took {elapsed:?}"
        );
        assert!(result.unwrap_err().contains("cancelled"));
    }

    #[test]
    fn urls_are_redacted_and_quoted_without_option_injection() {
        assert_eq!(
            redact("https://user:secret@art.example:8443/a/b.png?token=x#frag"),
            "https://art.example:8443/a/b.png"
        );
        assert_eq!(redact("http://host"), "http://host");
        assert_eq!(
            config("http://h/a\"b\\c").unwrap(),
            "url = \"http://h/a\\\"b\\\\c\"\n"
        );
        assert!(config("http://h/\nproto = file").is_err());
        let arguments = arguments(&Limits::default());
        assert_eq!(arguments[0], "-q");
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--proto-redir", "=http,https"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--max-redirs", "3"])
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "-k" || argument == "--insecure")
        );
        assert!(!arguments.iter().any(|argument| argument.contains("://")));
    }
}
