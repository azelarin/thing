use std::{
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, Stdio},
    ptr,
};

use socket2::{Domain, Socket, Type};

use super::{
    helper::HELPER_SENTINEL,
    protocol::{self, Packet, Receive},
};

pub struct ProcessContext {
    child_mask: libc::sigset_t,
    #[cfg(test)]
    disabled: bool,
}

impl ProcessContext {
    /// prepare child reaping before the event loop blocks `SIGCHLD`.
    pub fn prepare() -> io::Result<Self> {
        normalize_sigchld()?;
        check_pidfd_support()?;
        check_close_range_support()?;
        let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: a null input set asks pthread_sigmask to initialize mask.
        let status =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, ptr::null(), mask.as_mut_ptr()) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        Ok(Self {
            // SAFETY: successful pthread_sigmask initialized mask.
            child_mask: unsafe { mask.assume_init() },
            #[cfg(test)]
            disabled: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn disabled_for_test() -> Self {
        let mut mask = MaybeUninit::uninit();
        // SAFETY: mask points to writable storage for a sigset_t.
        let result = unsafe { libc::sigemptyset(mask.as_mut_ptr()) };
        assert_eq!(result, 0);
        Self {
            // SAFETY: successful sigemptyset initialized mask.
            child_mask: unsafe { mask.assume_init() },
            disabled: true,
        }
    }

    pub fn spawn(&self) -> io::Result<OsProcess> {
        #[cfg(test)]
        if self.disabled {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "authentication helpers are disabled in this test",
            ));
        }
        let (parent, child) = Socket::pair(Domain::UNIX, Type::SEQPACKET, None)?;
        parent.set_nonblocking(true)?;
        // SAFETY: getpid has no preconditions.
        let parent_pid = unsafe { libc::getpid() };
        let child_mask = self.child_mask;
        // keep the running executable's inode even if its installed path is replaced.
        let mut command = Command::new("/proc/self/exe");
        command
            .arg(HELPER_SENTINEL)
            .stdin(Stdio::from(OwnedFd::from(child)))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the closure uses only async-signal-safe libc calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                let status = libc::pthread_sigmask(libc::SIG_SETMASK, &child_mask, ptr::null_mut());
                if status != 0 {
                    return Err(io::Error::from_raw_os_error(status));
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                if libc::sigemptyset(&mut action.sa_mask) == -1
                    || libc::sigaction(libc::SIGTERM, &action, ptr::null_mut()) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    libc::_exit(126);
                }
                mark_unrelated_fds_close_on_exec()?;
                Ok(())
            });
        }
        let child = command.spawn()?;
        // ownership must not be lost if a per-child pidfd allocation fails after spawn.
        // the shared SIGCHLD source, socket, and timers still drive nonblocking reaping.
        let pidfd = pidfd_open(child.id()).ok();
        Ok(OsProcess {
            child,
            pidfd,
            socket: Some(parent),
            group_owned: true,
            #[cfg(test)]
            signal_group: None,
        })
    }
}

pub trait ProcessControl {
    fn send(&mut self, packet: &Packet) -> io::Result<()>;
    fn receive(&mut self) -> io::Result<Receive>;
    fn close_socket(&mut self);
    fn terminate_group(&mut self) -> io::Result<()>;
    fn kill_group(&mut self) -> io::Result<()>;
    fn try_wait(&mut self) -> io::Result<Option<bool>>;
}

#[cfg(test)]
type GroupSignaler = Box<dyn Fn(u32, libc::c_int) -> io::Result<()>>;

pub struct OsProcess {
    child: Child,
    pidfd: Option<OwnedFd>,
    socket: Option<Socket>,
    group_owned: bool,
    #[cfg(test)]
    signal_group: Option<GroupSignaler>,
}

impl OsProcess {
    pub fn pidfd(&self) -> Option<BorrowedFd<'_>> {
        self.pidfd.as_ref().map(AsFd::as_fd)
    }

    fn signal_group(&self, signal: libc::c_int) -> io::Result<()> {
        if !self.group_owned {
            return Ok(());
        }
        let child = self.child.id();
        #[cfg(test)]
        if let Some(signal_group) = &self.signal_group {
            return signal_group(child, signal);
        }
        let pid = i32::try_from(child)
            .map_err(|_| io::Error::other("authentication helper pid is out of range"))?;
        // the helper creates a session whose process group id is its pid.
        // SAFETY: pid is positive, so -pid addresses that dedicated process group.
        if unsafe { libc::kill(-pid, signal) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
}

impl ProcessControl for OsProcess {
    fn send(&mut self, packet: &Packet) -> io::Result<()> {
        let socket = self.socket.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "authentication socket is closed")
        })?;
        protocol::send(socket.as_fd(), packet)
    }

    fn receive(&mut self) -> io::Result<Receive> {
        let Some(socket) = &self.socket else {
            return Ok(Receive::Closed);
        };
        protocol::receive(socket.as_fd())
    }

    fn close_socket(&mut self) {
        self.socket = None;
    }

    fn terminate_group(&mut self) -> io::Result<()> {
        self.signal_group(libc::SIGTERM)
    }

    fn kill_group(&mut self) -> io::Result<()> {
        self.signal_group(libc::SIGKILL)
    }

    fn try_wait(&mut self) -> io::Result<Option<bool>> {
        if !self.group_owned {
            return self
                .child
                .try_wait()
                .map(|status| status.map(|status| status.success()));
        }
        match exit_observed(self.child.id()) {
            Ok(false) => return Ok(None),
            Ok(true) => {}
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.group_owned = false;
                return Ok(Some(false));
            }
            Err(error) => return Err(error),
        }
        self.kill_group()?;
        let status = match self.child.try_wait() {
            Ok(status) => status,
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.group_owned = false;
                return Ok(Some(false));
            }
            Err(error) => return Err(error),
        };
        if status.is_some() {
            self.group_owned = false;
        }
        Ok(status.map(|status| status.success()))
    }
}

impl Drop for OsProcess {
    fn drop(&mut self) {
        self.close_socket();
        if self.group_owned {
            let _ = self.kill_group();
        }
        let _ = self.child.try_wait();
    }
}

fn exit_observed(pid: u32) -> io::Result<bool> {
    let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: waitid initializes siginfo on success and WNOWAIT leaves the
    // child unreaped, preserving its process and process-group identity.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid initialized info after returning success.
    Ok(unsafe { info.assume_init().si_pid() } != 0)
}

fn normalize_sigchld() -> io::Result<()> {
    let mut current = MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: a null action queries the current disposition into current.
    if unsafe { libc::sigaction(libc::SIGCHLD, ptr::null(), current.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful sigaction initialized current.
    let current = unsafe { current.assume_init() };
    if current.sa_sigaction != libc::SIG_IGN && current.sa_flags & libc::SA_NOCLDWAIT == 0 {
        return Ok(());
    }
    // SAFETY: a zeroed sigaction is a valid starting value before its fields are set.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_DFL;
    // SAFETY: sigemptyset initializes the mask before sigaction reads the structure.
    if unsafe {
        libc::sigemptyset(&mut action.sa_mask) == -1
            || libc::sigaction(libc::SIGCHLD, &action, ptr::null_mut()) == -1
    } {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn check_pidfd_support() -> io::Result<()> {
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() };
    // SAFETY: pidfd_open accepts the current pid and zero flags.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pidfd_open returned a new descriptor owned by this scope.
    drop(unsafe { OwnedFd::from_raw_fd(fd as i32) });
    Ok(())
}

fn check_close_range_support() -> io::Result<()> {
    // Probe an empty practical range without changing any live descriptor.
    // SAFETY: close_range accepts integer bounds and flags only.
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            u32::MAX,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mark_unrelated_fds_close_on_exec() -> io::Result<()> {
    // stdin is the protocol socket and stdout/stderr are /dev/null. Mark every
    // other descriptor close-on-exec, including descriptors inherited without
    // CLOEXEC from the caller. The command's exec-error pipe remains usable
    // until exec and closes with the rest on success.
    // SAFETY: close_range accepts integer bounds and flags only.
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3_u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open accepts a process id and zero flags.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful pidfd_open returns a new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        env,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        process::{Command, Stdio},
        rc::Rc,
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    const PROBE: &str = "THING_SIGCHLD_PROBE";
    const FD_PROBE: &str = "THING_AUTH_FD_PROBE";

    type SignalObservations = Rc<RefCell<Vec<(libc::c_int, bool)>>>;

    fn exit_process(signal_group: GroupSignaler) -> OsProcess {
        let child = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "authentication::process::tests::immediate_exit_probe",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        OsProcess {
            child,
            pidfd: None,
            socket: None,
            group_owned: true,
            signal_group: Some(signal_group),
        }
    }

    fn observed_exit_process() -> (OsProcess, SignalObservations) {
        let observations = Rc::new(RefCell::new(Vec::new()));
        let observed = observations.clone();
        (
            exit_process(Box::new(move |pid, signal| {
                observed
                    .borrow_mut()
                    .push((signal, exit_observed(pid).unwrap_or(false)));
                Ok(())
            })),
            observations,
        )
    }

    fn wait_for_exit(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if exit_observed(pid).unwrap() {
                return;
            }
            assert!(Instant::now() < deadline, "exit probe did not finish");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn reap(process: &mut OsProcess) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(clean) = process.try_wait().unwrap() {
                return clean;
            }
            assert!(Instant::now() < deadline, "exit probe was not reaped");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn reap_outside_process(pid: u32) {
        let mut status = 0;
        // SAFETY: pid names the direct child created by exit_process, and the
        // status pointer is valid for the duration of waitpid.
        let waited = unsafe { libc::waitpid(pid as i32, &mut status, 0) };
        assert_eq!(waited, pid as i32);
    }

    #[test]
    fn reaped_process_rejects_later_cancellation_and_drop_signals() {
        let (mut process, observations) = observed_exit_process();
        wait_for_exit(process.child.id());

        process.terminate_group().unwrap();
        assert!(reap(&mut process));
        process.terminate_group().unwrap();
        process.kill_group().unwrap();
        drop(process);

        assert_eq!(
            *observations.borrow(),
            [(libc::SIGTERM, true), (libc::SIGKILL, true)]
        );
    }

    #[test]
    fn waitid_echild_releases_group_ownership_without_signalling() {
        let (mut process, observations) = observed_exit_process();
        wait_for_exit(process.child.id());
        reap_outside_process(process.child.id());

        assert_eq!(process.try_wait().unwrap(), Some(false));
        process.terminate_group().unwrap();
        process.kill_group().unwrap();
        drop(process);

        assert!(observations.borrow().is_empty());
    }

    #[test]
    fn child_wait_echild_releases_group_ownership_after_cleanup() {
        let observations = Rc::new(RefCell::new(Vec::new()));
        let observed = observations.clone();
        let mut process = exit_process(Box::new(move |pid, signal| {
            observed
                .borrow_mut()
                .push((signal, exit_observed(pid).unwrap_or(false)));
            reap_outside_process(pid);
            Ok(())
        }));

        assert!(!reap(&mut process));
        process.terminate_group().unwrap();
        process.kill_group().unwrap();
        drop(process);

        assert_eq!(*observations.borrow(), [(libc::SIGKILL, true)]);
    }

    #[test]
    fn immediate_exit_probe() {}

    #[test]
    fn normalizes_inherited_ignored_and_auto_reaped_children() {
        let executable = env::current_exe().unwrap();
        let status = Command::new(executable)
            .args([
                "--exact",
                "authentication::process::tests::sigchld_normalization_probe",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn sigchld_normalization_probe() {
        if env::var_os(PROBE).is_none() {
            return;
        }
        // SAFETY: a zeroed sigaction is valid before its fields and mask are set.
        let mut ignored: libc::sigaction = unsafe { std::mem::zeroed() };
        ignored.sa_sigaction = libc::SIG_IGN;
        ignored.sa_flags = libc::SA_NOCLDWAIT;
        // SAFETY: ignored contains live writable mask storage.
        assert_eq!(unsafe { libc::sigemptyset(&mut ignored.sa_mask) }, 0);
        // SAFETY: ignored is fully initialized and the old action is not requested.
        let status = unsafe { libc::sigaction(libc::SIGCHLD, &ignored, ptr::null_mut()) };
        assert_eq!(status, 0);

        normalize_sigchld().unwrap();
        let mut current = MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: a null action queries the disposition into current.
        let status = unsafe { libc::sigaction(libc::SIGCHLD, ptr::null(), current.as_mut_ptr()) };
        assert_eq!(status, 0);
        // SAFETY: successful sigaction initialized current.
        let current = unsafe { current.assume_init() };
        assert_eq!(current.sa_sigaction, libc::SIG_DFL);
        assert_eq!(current.sa_flags & libc::SA_NOCLDWAIT, 0);
    }

    #[test]
    fn socket_pair_is_close_on_exec_until_stdin_is_mapped() {
        let (left, right) = Socket::pair(Domain::UNIX, Type::SEQPACKET, None).unwrap();
        for socket in [&left, &right] {
            // SAFETY: F_GETFD reads flags from the socket's live descriptor.
            let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn execed_child_closes_deliberately_inheritable_descriptors() {
        // SAFETY: F_DUPFD returns a new descriptor numbered at least three.
        let raw = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD, 3) };
        assert_ne!(raw, -1);
        // SAFETY: F_DUPFD returned a new descriptor owned by this scope.
        let inherited = unsafe { OwnedFd::from_raw_fd(raw) };
        // Deliberately remove CLOEXEC to model a descriptor created elsewhere.
        // SAFETY: inherited is live and F_SETFD accepts zero flags.
        let status = unsafe { libc::fcntl(inherited.as_raw_fd(), libc::F_SETFD, 0) };
        assert_ne!(status, -1);

        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "authentication::process::tests::inherited_fd_probe",
                "--nocapture",
            ])
            .env(FD_PROBE, inherited.as_raw_fd().to_string());
        // SAFETY: the closure uses one async-signal-safe system call.
        unsafe {
            command.pre_exec(mark_unrelated_fds_close_on_exec);
        }
        let status = command.status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn inherited_fd_probe() {
        let Some(raw) = env::var_os(FD_PROBE) else {
            return;
        };
        let raw: libc::c_int = raw.to_string_lossy().parse().unwrap();
        // SAFETY: F_GETFD only inspects the descriptor number.
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    }
}
