use std::{
    collections::BTreeMap,
    fs, io,
    os::{fd::RawFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    ptr,
    time::{Duration, Instant},
};

use calloop::signals::{Signal, Signals};

const MAX_CHILDREN: usize = 128;
const MAX_PENDING_LAUNCHES: usize = 128;
const ACTIVATION_TIMEOUT: Duration = Duration::from_millis(250);
const XDG_ACTIVATION_TOKEN: &str = "XDG_ACTIVATION_TOKEN";
const DESKTOP_STARTUP_ID: &str = "DESKTOP_STARTUP_ID";

#[derive(Clone, Copy)]
struct ChildSignalMask {
    sigchld_blocked: bool,
}

impl ChildSignalMask {
    /// capture this before the event loop blocks `SIGCHLD` for its signal source.
    fn capture() -> io::Result<Self> {
        let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: a null set asks pthread_sigmask to copy the calling thread's
        // current mask into the initialized output storage.
        let result =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, ptr::null(), mask.as_mut_ptr()) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        // SAFETY: pthread_sigmask initialized mask after returning success.
        let member = unsafe { libc::sigismember(mask.assume_init_ref(), libc::SIGCHLD) };
        match member {
            0 => Ok(Self {
                sigchld_blocked: false,
            }),
            1 => Ok(Self {
                sigchld_blocked: true,
            }),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

pub(crate) struct Launcher {
    children: BTreeMap<u32, Child>,
    child_mask: ChildSignalMask,
    limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaunchRequest {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
}

struct PendingLaunch<T> {
    request: LaunchRequest,
    token: T,
    deadline: Instant,
}

pub(crate) struct PendingLaunches<T> {
    launches: BTreeMap<u64, PendingLaunch<T>>,
    next_id: u64,
    limit: usize,
    timeout: Duration,
}

pub(crate) struct PendingCompletion<T> {
    pub request: LaunchRequest,
    pub token: T,
    pub fresh: bool,
}

impl<T> PendingLaunches<T> {
    pub(crate) fn new() -> Self {
        Self::with_limits(MAX_PENDING_LAUNCHES, ACTIVATION_TIMEOUT)
    }

    fn with_limits(limit: usize, timeout: Duration) -> Self {
        Self {
            launches: BTreeMap::new(),
            next_id: 1,
            limit,
            timeout,
        }
    }

    pub(crate) fn push(
        &mut self,
        now: Instant,
        request: LaunchRequest,
        create_token: impl FnOnce(u64) -> T,
    ) -> Result<u64, LaunchRequest> {
        if self.launches.len() >= self.limit {
            return Err(request);
        }
        let id = self.next_id();
        let token = create_token(id);
        self.launches.insert(
            id,
            PendingLaunch {
                request,
                token,
                deadline: now + self.timeout,
            },
        );
        Ok(id)
    }

    pub(crate) fn complete(&mut self, id: u64, now: Instant) -> Option<PendingCompletion<T>> {
        let pending = self.launches.remove(&id)?;
        Some(PendingCompletion {
            request: pending.request,
            token: pending.token,
            fresh: now < pending.deadline,
        })
    }

    pub(crate) fn expire(&mut self, now: Instant) -> Vec<(LaunchRequest, T)> {
        let expired: Vec<_> = self
            .launches
            .iter()
            .filter_map(|(id, pending)| (pending.deadline <= now).then_some(*id))
            .collect();
        expired
            .into_iter()
            .filter_map(|id| self.launches.remove(&id))
            .map(|pending| (pending.request, pending.token))
            .collect()
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.launches.values().map(|pending| pending.deadline).min()
    }

    pub(crate) fn drain(&mut self) -> Vec<(LaunchRequest, T)> {
        std::mem::take(&mut self.launches)
            .into_values()
            .map(|pending| (pending.request, pending.token))
            .collect()
    }

    fn next_id(&mut self) -> u64 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if !self.launches.contains_key(&id) {
                return id;
            }
        }
    }
}

impl Launcher {
    fn new(child_mask: ChildSignalMask) -> Self {
        Self::with_limit(child_mask, MAX_CHILDREN)
    }

    fn with_limit(child_mask: ChildSignalMask, limit: usize) -> Self {
        Self {
            children: BTreeMap::new(),
            child_mask,
            limit,
        }
    }

    pub(crate) fn spawn(
        &mut self,
        argv: &[String],
        cwd: Option<&str>,
        activation_token: Option<&str>,
    ) -> io::Result<u32> {
        if self.children.len() >= self.limit {
            // An exited child may still be waiting for the next SIGCHLD
            // dispatch, so reap once before rejecting at capacity.
            let _ = self.reap();
        }
        if self.children.len() >= self.limit {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "too many launched processes are still running",
            ));
        }
        let mut command = command(argv, cwd, activation_token)?;
        configure_child(&mut command, self.child_mask, has_negative_oom_score());
        let child = command.spawn()?;
        let pid = child.id();
        self.children.insert(pid, child);
        Ok(pid)
    }

    pub(crate) fn reap(&mut self) -> Vec<(u32, io::Error)> {
        let mut errors = Vec::new();
        self.children.retain(|pid, child| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => false,
            Err(error) => {
                errors.push((*pid, error));
                false
            }
        });
        errors
    }
}

pub(crate) fn prepare() -> calloop::Result<(Launcher, Signals)> {
    reset_sigchld_disposition()?;
    let child_mask = ChildSignalMask::capture()?;
    match Signals::new(&[Signal::SIGCHLD]) {
        Ok(signals) => Ok((Launcher::new(child_mask), signals)),
        Err(error) => {
            if !child_mask.sigchld_blocked {
                unblock_sigchld()?;
            }
            Err(error)
        }
    }
}

pub(crate) fn sigchld_is_reapable() -> io::Result<bool> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: a null replacement asks sigaction to copy the current
    // disposition into the initialized output storage.
    if unsafe { libc::sigaction(libc::SIGCHLD, ptr::null(), action.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: sigaction initialized action after returning success.
    let action = unsafe { action.assume_init() };
    Ok(action.sa_sigaction == libc::SIG_DFL && action.sa_flags & libc::SA_NOCLDWAIT == 0)
}

fn reset_sigchld_disposition() -> io::Result<()> {
    // SAFETY: an all-zero sigaction is a valid starting value on Unix. The
    // handler, mask, and flags are initialized before the syscall reads them.
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = libc::SIG_DFL;
    action.sa_flags = 0;
    // SAFETY: sigemptyset initializes the embedded signal mask.
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: action is fully initialized and SIGCHLD is a valid signal.
    if unsafe { libc::sigaction(libc::SIGCHLD, &action, ptr::null_mut()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn command(
    argv: &[String],
    cwd: Option<&str>,
    activation_token: Option<&str>,
) -> io::Result<Command> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "launch has no program"))?;
    if program.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch program is empty",
        ));
    }
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove(XDG_ACTIVATION_TOKEN)
        .env_remove(DESKTOP_STARTUP_ID);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(token) = activation_token.filter(|token| !token.is_empty()) {
        command
            .env(XDG_ACTIVATION_TOKEN, token)
            .env(DESKTOP_STARTUP_ID, token);
    }
    Ok(command)
}

fn configure_child(command: &mut Command, child_mask: ChildSignalMask, reset_oom_score: bool) {
    // SAFETY: the closure calls only async-signal-safe libc functions between
    // fork and exec. it does not allocate, lock, or access shared rust state.
    unsafe {
        command.pre_exec(move || {
            if !child_mask.sigchld_blocked {
                unblock_sigchld()?;
            }
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if reset_oom_score {
                reset_child_oom_score();
            }
            Ok(())
        });
    }
}

fn has_negative_oom_score() -> bool {
    fs::read_to_string("/proc/self/oom_score_adj")
        .ok()
        .is_some_and(|value| negative_oom_score(&value))
}

fn negative_oom_score(value: &str) -> bool {
    value.trim().parse::<i16>().is_ok_and(|value| value < 0)
}

fn unblock_sigchld() -> io::Result<()> {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: the sigset is initialized before use, and sigprocmask only reads
    // it for the duration of the call.
    unsafe {
        if libc::sigemptyset(set.as_mut_ptr()) == -1
            || libc::sigaddset(set.as_mut_ptr(), libc::SIGCHLD) == -1
        {
            return Err(io::Error::last_os_error());
        }
        if libc::sigprocmask(libc::SIG_UNBLOCK, set.as_ptr(), ptr::null_mut()) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn reset_child_oom_score() {
    const PATH: &[u8] = b"/proc/self/oom_score_adj\0";
    const VALUE: &[u8] = b"0\n";
    // SAFETY: PATH is NUL-terminated, and the descriptor and byte slice remain
    // valid for these async-signal-safe syscalls. failure is intentionally
    // ignored because OOM policy must not prevent an otherwise valid launch.
    unsafe {
        let fd = libc::open(PATH.as_ptr().cast(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd == -1 {
            return;
        }
        write_all(fd, VALUE);
        let _ = libc::close(fd);
    }
}

unsafe fn write_all(fd: RawFd, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: bytes points to readable memory for its reported length.
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
        } else if written == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsStr,
        fs,
        os::unix::fs::PermissionsExt,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn command_keeps_argv_cwd_and_activation_environment_literal() {
        let argv = strings(&["program", "with space", "'quoted'", "$HOME", "one;two", ""]);
        let command = command(&argv, Some("/tmp/work dir"), Some("token value")).unwrap();

        assert_eq!(command.get_program(), OsStr::new("program"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            argv[1..].iter().map(OsStr::new).collect::<Vec<_>>()
        );
        assert_eq!(
            command.get_current_dir(),
            Some(std::path::Path::new("/tmp/work dir"))
        );
        let environment = command.get_envs().collect::<BTreeMap<_, _>>();
        assert_eq!(
            environment.get(OsStr::new(XDG_ACTIVATION_TOKEN)),
            Some(&Some(OsStr::new("token value")))
        );
        assert_eq!(
            environment.get(OsStr::new(DESKTOP_STARTUP_ID)),
            Some(&Some(OsStr::new("token value")))
        );
    }

    #[test]
    fn command_removes_stale_activation_environment() {
        let command = command(&strings(&["program"]), None, None).unwrap();
        let environment = command.get_envs().collect::<BTreeMap<_, _>>();

        assert_eq!(
            environment.get(OsStr::new(XDG_ACTIVATION_TOKEN)),
            Some(&None)
        );
        assert_eq!(environment.get(OsStr::new(DESKTOP_STARTUP_ID)), Some(&None));
    }

    #[test]
    fn prepare_captures_the_mask_before_blocking_sigchld() {
        let before = ChildSignalMask::capture().unwrap();
        if before.sigchld_blocked {
            return;
        }

        let (launcher, signals) = prepare().unwrap();

        assert!(ChildSignalMask::capture().unwrap().sigchld_blocked);
        assert!(!launcher.child_mask.sigchld_blocked);
        drop(signals);
        assert!(!ChildSignalMask::capture().unwrap().sigchld_blocked);
    }

    #[test]
    fn helper_observes_literal_arguments_working_directory_and_path() {
        let directory = tempfile::tempdir().unwrap();
        let working = directory.path().join("working directory");
        fs::create_dir(&working).unwrap();
        let output = directory.path().join("output");
        let helper = directory.path().join("helper");
        fs::write(
            &helper,
            "#!/bin/sh\nout=$1\nshift\n{ printf 'cwd=<%s>\\n' \"$PWD\"; printf 'path=<%s>\\n' \"$PATH\"; for arg do printf 'arg=<%s>\\n' \"$arg\"; done; } > \"$out\"\n",
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let mut launcher = Launcher::new(ChildSignalMask::capture().unwrap());
        launcher
            .spawn(
                &[
                    helper.to_string_lossy().into_owned(),
                    output.to_string_lossy().into_owned(),
                    "with space".into(),
                    "$HOME".into(),
                    "one;two".into(),
                ],
                Some(working.to_str().unwrap()),
                None,
            )
            .unwrap();

        wait_until(|| output.exists());
        wait_until(|| {
            launcher.reap();
            launcher.children.is_empty()
        });
        let output = fs::read_to_string(output).unwrap();
        assert!(output.contains(&format!("cwd=<{}>", working.display())));
        assert!(output.contains(&format!("path=<{}>", std::env::var("PATH").unwrap())));
        assert!(output.contains("arg=<with space>"));
        assert!(output.contains("arg=<$HOME>"));
        assert!(output.contains("arg=<one;two>"));
    }

    #[test]
    fn child_restores_sigchld_mask_and_uses_its_own_process_group() {
        let _mask = SigchldGuard::block();
        let mut launcher = Launcher::new(ChildSignalMask {
            sigchld_blocked: false,
        });
        let pid = launcher
            .spawn(&strings(&["/bin/sleep", "10"]), None, None)
            .unwrap();

        let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let blocked = status
            .lines()
            .find_map(|line| line.strip_prefix("SigBlk:\t"))
            .and_then(|value| u64::from_str_radix(value, 16).ok())
            .unwrap();
        assert_eq!(blocked & (1 << (libc::SIGCHLD - 1)), 0);
        // SAFETY: pid names a live child owned by this test.
        assert_eq!(unsafe { libc::getpgid(pid as i32) }, pid as i32);
        kill(pid);
        wait_until(|| {
            launcher.reap();
            launcher.children.is_empty()
        });
    }

    #[test]
    fn reaps_finished_children() {
        let mut launcher = Launcher::new(ChildSignalMask::capture().unwrap());
        let pid = launcher
            .spawn(&strings(&["/bin/true"]), None, None)
            .unwrap();

        wait_until(|| {
            launcher.reap();
            launcher.children.is_empty()
        });
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn an_already_reaped_child_is_not_reported_as_an_error() {
        let mut launcher = Launcher::new(ChildSignalMask::capture().unwrap());
        let pid = launcher
            .spawn(&strings(&["/bin/true"]), None, None)
            .unwrap();
        // SAFETY: pid names a child created by this test; waiting here models
        // SIG_IGN or SA_NOCLDWAIT reaping it before the launcher observes it.
        let waited = unsafe { libc::waitpid(pid as i32, ptr::null_mut(), 0) };
        assert_eq!(waited, pid as i32);

        assert!(launcher.reap().is_empty());
        assert!(launcher.children.is_empty());
    }

    #[test]
    fn dropping_launcher_does_not_wait_for_or_kill_children() {
        let mut launcher = Launcher::new(ChildSignalMask::capture().unwrap());
        let pid = launcher
            .spawn(&strings(&["/bin/sleep", "10"]), None, None)
            .unwrap();
        let started = Instant::now();

        drop(launcher);

        assert!(started.elapsed() < Duration::from_secs(1));
        // SAFETY: signal zero checks the process without modifying it.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        kill(pid);
        // SAFETY: the dropped Child did not reap this process; waitpid prevents
        // the test itself from leaving a zombie behind.
        let waited = unsafe { libc::waitpid(pid as i32, ptr::null_mut(), 0) };
        assert_eq!(waited, pid as i32);
    }

    #[test]
    fn errors_do_not_consume_tracking_capacity() {
        let mask = ChildSignalMask::capture().unwrap();
        let mut launcher = Launcher::with_limit(mask, 1);
        assert!(launcher.spawn(&[], None, None).is_err());
        assert!(launcher.spawn(&[String::new()], None, None).is_err());
        assert!(
            launcher
                .spawn(&strings(&["/definitely/missing"]), None, None)
                .is_err()
        );
        assert!(
            launcher
                .spawn(&strings(&["/bin/true"]), Some("/definitely/missing"), None)
                .is_err()
        );
        let pid = launcher
            .spawn(&strings(&["/bin/sleep", "10"]), None, None)
            .unwrap();
        let error = launcher
            .spawn(&strings(&["/bin/true"]), None, None)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        kill(pid);
        wait_until(|| {
            launcher.reap();
            launcher.children.is_empty()
        });
    }

    #[test]
    fn a_full_launcher_reaps_finished_children_before_rejecting() {
        let mask = ChildSignalMask::capture().unwrap();
        let mut launcher = Launcher::with_limit(mask, 1);
        launcher
            .spawn(&strings(&["/bin/true"]), None, None)
            .unwrap();
        thread::sleep(Duration::from_millis(10));

        let pid = launcher
            .spawn(&strings(&["/bin/sleep", "10"]), None, None)
            .unwrap();

        kill(pid);
        wait_until(|| {
            launcher.reap();
            launcher.children.is_empty()
        });
    }

    #[test]
    fn resets_only_inherited_negative_oom_scores() {
        assert!(negative_oom_score("-100\n"));
        assert!(!negative_oom_score("0\n"));
        assert!(!negative_oom_score("250\n"));
        assert!(!negative_oom_score("invalid"));
    }

    #[test]
    fn pending_launches_complete_out_of_order_once() {
        let now = Instant::now();
        let mut pending = PendingLaunches::with_limits(3, Duration::from_millis(250));
        let first = pending
            .push(now, request("first"), |id| format!("token-{id}"))
            .unwrap();
        let second = pending
            .push(now, request("second"), |id| format!("token-{id}"))
            .unwrap();

        let completed = pending
            .complete(second, now + Duration::from_millis(20))
            .unwrap();
        assert_eq!(completed.request, request("second"));
        assert_eq!(completed.token, format!("token-{second}"));
        assert!(completed.fresh);
        assert!(pending.complete(second, now).is_none());
        assert_eq!(
            pending.complete(first, now).unwrap().request,
            request("first")
        );
    }

    #[test]
    fn pending_launches_expire_at_the_exact_deadline() {
        let now = Instant::now();
        let timeout = Duration::from_millis(250);
        let mut pending = PendingLaunches::with_limits(2, timeout);
        let id = pending.push(now, request("app"), |_| "proxy").unwrap();

        assert_eq!(pending.next_deadline(), Some(now + timeout));
        assert!(
            pending
                .expire(now + timeout - Duration::from_nanos(1))
                .is_empty()
        );
        assert!(!pending.complete(id, now + timeout).unwrap().fresh);
    }

    #[test]
    fn timeout_then_late_completion_cannot_launch_twice() {
        let now = Instant::now();
        let timeout = Duration::from_millis(250);
        let mut pending = PendingLaunches::with_limits(2, timeout);
        let id = pending.push(now, request("app"), |_| "proxy").unwrap();

        assert_eq!(
            pending.expire(now + timeout),
            vec![(request("app"), "proxy")]
        );
        assert!(pending.complete(id, now + timeout).is_none());
        assert!(pending.expire(now + timeout).is_empty());
        assert_eq!(pending.next_deadline(), None);
    }

    #[test]
    fn pending_launches_are_bounded_without_creating_a_token() {
        let now = Instant::now();
        let mut pending = PendingLaunches::with_limits(1, Duration::from_secs(1));
        pending.push(now, request("first"), |_| "proxy").unwrap();
        let mut created = false;

        let rejected = pending
            .push(now, request("second"), |_| {
                created = true;
                "unused"
            })
            .unwrap_err();

        assert_eq!(rejected, request("second"));
        assert!(!created);
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    fn request(program: &str) -> LaunchRequest {
        LaunchRequest {
            argv: vec![program.into()],
            cwd: None,
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn kill(pid: u32) {
        // SAFETY: pid names a child created by this test.
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    }

    struct SigchldGuard {
        blocked: bool,
    }

    impl SigchldGuard {
        fn block() -> Self {
            let before = ChildSignalMask::capture().unwrap().sigchld_blocked;
            set_sigchld(libc::SIG_BLOCK);
            Self { blocked: before }
        }
    }

    impl Drop for SigchldGuard {
        fn drop(&mut self) {
            if !self.blocked {
                set_sigchld(libc::SIG_UNBLOCK);
            }
        }
    }

    fn set_sigchld(how: libc::c_int) {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: the set is initialized before pthread_sigmask reads it.
        unsafe {
            assert_eq!(libc::sigemptyset(set.as_mut_ptr()), 0);
            assert_eq!(libc::sigaddset(set.as_mut_ptr(), libc::SIGCHLD), 0);
            assert_eq!(libc::pthread_sigmask(how, set.as_ptr(), ptr::null_mut()), 0);
        }
    }
}
