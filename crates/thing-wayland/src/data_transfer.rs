//! bounded, nonblocking transfers over wayland-owned file descriptors.

use std::{
    collections::{BTreeSet, VecDeque},
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use calloop::ping::Ping;

const MAX_COMMANDS: usize = 64;
const MAX_ACTIVE: usize = 16;
const MAX_COMPLETIONS: usize = 64;
const MAX_BUFFERED_READ_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum Completion {
    Read {
        id: u64,
        result: Result<Vec<u8>, TransferError>,
    },
    Write {
        id: u64,
        result: Result<(), TransferError>,
    },
}

#[derive(Debug)]
pub(crate) enum TransferError {
    TooLarge,
    BufferLimit,
    TimedOut,
    Io(io::Error),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("transfer exceeds its size limit"),
            Self::BufferLimit => formatter.write_str("transfer buffer limit reached"),
            Self::TimedOut => formatter.write_str("transfer timed out"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for TransferError {}

enum Command {
    Read {
        id: u64,
        file: File,
        limit: usize,
        deadline: Instant,
    },
    Write {
        id: u64,
        file: File,
        bytes: Arc<[u8]>,
        deadline: Instant,
    },
    Cancel(u64),
}

impl Command {
    fn id(&self) -> u64 {
        match self {
            Self::Read { id, .. } | Self::Write { id, .. } | Self::Cancel(id) => *id,
        }
    }
}

impl Completion {
    fn id(&self) -> u64 {
        match self {
            Self::Read { id, .. } | Self::Write { id, .. } => *id,
        }
    }

    fn buffered(&self) -> usize {
        match self {
            Self::Read {
                result: Ok(bytes), ..
            } => bytes.capacity(),
            _ => 0,
        }
    }
}

#[derive(Default)]
struct Shared {
    commands: VecDeque<Command>,
    completions: VecDeque<Completion>,
    outstanding: BTreeSet<u64>,
    completed_read_bytes: usize,
    stop: bool,
}

pub(crate) struct Worker {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn start(ping: Ping) -> io::Result<Self> {
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("thing-data-transfer".into())
            .spawn(move || run(state, reader, ping))?;
        Ok(Self {
            shared,
            wake,
            worker: Some(worker),
        })
    }

    pub(crate) fn read(
        &self,
        id: u64,
        fd: OwnedFd,
        limit: usize,
        timeout: Duration,
    ) -> io::Result<()> {
        self.send(Command::Read {
            id,
            file: nonblocking(fd)?,
            limit,
            deadline: deadline(timeout),
        })
    }

    pub(crate) fn write(
        &self,
        id: u64,
        fd: OwnedFd,
        bytes: Arc<[u8]>,
        timeout: Duration,
    ) -> io::Result<()> {
        self.send(Command::Write {
            id,
            file: nonblocking(fd)?,
            bytes,
            deadline: deadline(timeout),
        })
    }

    pub(crate) fn cancel(&self, id: u64) -> io::Result<()> {
        self.send(Command::Cancel(id))
    }

    pub(crate) fn take_completions(&self) -> Vec<Completion> {
        let mut shared = lock(&self.shared);
        let completions = shared.completions.drain(..).collect::<Vec<_>>();
        for completion in &completions {
            shared.outstanding.remove(&completion.id());
            shared.completed_read_bytes = shared
                .completed_read_bytes
                .saturating_sub(completion.buffered());
        }
        completions
    }

    fn send(&self, command: Command) -> io::Result<()> {
        let mut shared = lock(&self.shared);
        if shared.stop {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "transfer worker has stopped",
            ));
        }
        if let Command::Cancel(id) = &command {
            if let Some(index) = shared
                .commands
                .iter()
                .position(|queued| !matches!(queued, Command::Cancel(_)) && queued.id() == *id)
            {
                shared.commands.remove(index);
                shared.outstanding.remove(id);
                return Ok(());
            }
            if !shared.outstanding.contains(id)
                || shared
                    .completions
                    .iter()
                    .any(|completion| completion.id() == *id)
            {
                return Ok(());
            }
        }
        if shared.commands.len() >= MAX_COMMANDS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "transfer command queue is full",
            ));
        }
        let transfer = matches!(command, Command::Read { .. } | Command::Write { .. });
        if transfer {
            if shared.outstanding.len() >= MAX_COMPLETIONS {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "too many unfinished transfers",
                ));
            }
            if !shared.outstanding.insert(command.id()) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "transfer id is already in use",
                ));
            }
        }
        shared.commands.push_back(command);
        if let Err(error) = wake(&self.wake) {
            let command = shared
                .commands
                .pop_back()
                .expect("new transfer command must remain queued while locked");
            if transfer {
                shared.outstanding.remove(&command.id());
            }
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        let _ = wake(&self.wake);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum Transfer {
    Read {
        id: u64,
        file: File,
        bytes: Vec<u8>,
        limit: usize,
        deadline: Instant,
    },
    Write {
        id: u64,
        file: File,
        bytes: Arc<[u8]>,
        offset: usize,
        deadline: Instant,
    },
}

impl Transfer {
    fn id(&self) -> u64 {
        match self {
            Self::Read { id, .. } | Self::Write { id, .. } => *id,
        }
    }

    fn fd(&self) -> i32 {
        match self {
            Self::Read { file, .. } | Self::Write { file, .. } => file.as_raw_fd(),
        }
    }

    fn events(&self) -> i16 {
        match self {
            Self::Read { .. } => libc::POLLIN,
            Self::Write { .. } => libc::POLLOUT,
        }
    }

    fn deadline(&self) -> Instant {
        match self {
            Self::Read { deadline, .. } | Self::Write { deadline, .. } => *deadline,
        }
    }

    fn buffered(&self) -> usize {
        match self {
            Self::Read { bytes, .. } => bytes.capacity(),
            Self::Write { .. } => 0,
        }
    }
}

enum Finished {
    Read(Result<(), TransferError>),
    Write(Result<(), TransferError>),
}

fn run(shared: Arc<Mutex<Shared>>, wake_stream: UnixStream, ping: Ping) {
    let mut active = Vec::<Transfer>::new();
    let mut pending = VecDeque::<Command>::new();
    let mut buffered = 0_usize;
    loop {
        if stopping(&shared) {
            break;
        }
        maintain(&shared, &mut pending, &mut active, &mut buffered, &ping);
        if stopping(&shared) {
            break;
        }

        let mut descriptors = Vec::with_capacity(active.len() + 1);
        descriptors.push(libc::pollfd {
            fd: wake_stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        descriptors.extend(active.iter().map(|transfer| libc::pollfd {
            fd: transfer.fd(),
            events: transfer.events(),
            revents: 0,
        }));
        let timeout = poll_timeout(&active, &pending, Instant::now());
        match poll(&mut descriptors, timeout) {
            Ok(()) => {}
            Err(error) => {
                let kind = error.kind();
                fail_all(&shared, &mut active, &mut buffered, &ping, error);
                fail_pending(&shared, &mut pending, &ping, kind);
                break;
            }
        }
        if stopping(&shared) {
            break;
        }
        let woke = descriptors[0].revents != 0;
        let readiness = descriptors
            .into_iter()
            .skip(1)
            .map(|descriptor| descriptor.revents)
            .collect::<Vec<_>>();
        for index in (0..readiness.len().min(active.len())).rev() {
            let events = readiness[index];
            if events == 0 {
                continue;
            }
            let finished = if events & libc::POLLNVAL != 0 {
                Some(io_failure(&active[index], io::ErrorKind::InvalidInput))
            } else if events & libc::POLLERR != 0 {
                Some(io_failure(&active[index], io::ErrorKind::BrokenPipe))
            } else {
                progress(&shared, &mut active[index], &mut buffered)
            };
            if let Some(finished) = finished {
                complete_at(&shared, &mut active, &mut buffered, index, finished, &ping);
            }
        }
        if woke {
            drain(&wake_stream);
        }
        maintain(&shared, &mut pending, &mut active, &mut buffered, &ping);
    }
    active.clear();
    pending.clear();
    lock(&shared).commands.clear();
}

fn maintain(
    shared: &Mutex<Shared>,
    pending: &mut VecDeque<Command>,
    active: &mut Vec<Transfer>,
    buffered: &mut usize,
    ping: &Ping,
) {
    apply_commands(shared, pending, active, buffered);
    let now = Instant::now();
    expire(shared, active, buffered, ping, now);
    expire_pending(shared, pending, ping, now);
    activate_pending(shared, pending, active, ping);
}

fn apply_commands(
    shared: &Mutex<Shared>,
    pending: &mut VecDeque<Command>,
    active: &mut Vec<Transfer>,
    buffered: &mut usize,
) {
    let commands = {
        let mut shared = lock(shared);
        shared.commands.drain(..).collect::<Vec<_>>()
    };
    for command in commands {
        match command {
            Command::Cancel(id) => {
                let mut removed = false;
                pending.retain(|command| {
                    let keep = command.id() != id;
                    removed |= !keep;
                    keep
                });
                for index in (0..active.len()).rev() {
                    if active[index].id() == id {
                        *buffered = buffered.saturating_sub(active[index].buffered());
                        active.swap_remove(index);
                        removed = true;
                    }
                }
                if removed {
                    lock(shared).outstanding.remove(&id);
                }
            }
            command @ (Command::Read { .. } | Command::Write { .. }) => {
                pending.push_back(command);
            }
        }
    }
}

fn expire_pending(
    shared: &Mutex<Shared>,
    pending: &mut VecDeque<Command>,
    ping: &Ping,
    now: Instant,
) {
    for index in (0..pending.len()).rev() {
        if command_deadline(&pending[index]) <= now {
            let command = pending
                .remove(index)
                .expect("pending transfer index must remain valid");
            publish(shared, ping, timed_out(command));
        }
    }
}

fn activate_pending(
    shared: &Mutex<Shared>,
    pending: &mut VecDeque<Command>,
    active: &mut Vec<Transfer>,
    ping: &Ping,
) {
    while active.len() < MAX_ACTIVE {
        let Some(command) = pending.pop_front() else {
            break;
        };
        if command_deadline(&command) <= Instant::now() {
            publish(shared, ping, timed_out(command));
            continue;
        }
        match command {
            Command::Read {
                id,
                file,
                limit,
                deadline,
            } => active.push(Transfer::Read {
                id,
                file,
                bytes: Vec::new(),
                limit,
                deadline,
            }),
            Command::Write {
                id,
                file,
                bytes,
                deadline: _,
            } if bytes.is_empty() => {
                drop(file);
                publish(shared, ping, Completion::Write { id, result: Ok(()) });
            }
            Command::Write {
                id,
                file,
                bytes,
                deadline,
            } => active.push(Transfer::Write {
                id,
                file,
                bytes,
                offset: 0,
                deadline,
            }),
            Command::Cancel(_) => unreachable!("cancellations are never queued as transfers"),
        }
    }
}

fn command_deadline(command: &Command) -> Instant {
    match command {
        Command::Read { deadline, .. } | Command::Write { deadline, .. } => *deadline,
        Command::Cancel(_) => unreachable!("cancellations have no deadline"),
    }
}

fn timed_out(command: Command) -> Completion {
    match command {
        Command::Read { id, .. } => Completion::Read {
            id,
            result: Err(TransferError::TimedOut),
        },
        Command::Write { id, .. } => Completion::Write {
            id,
            result: Err(TransferError::TimedOut),
        },
        Command::Cancel(_) => unreachable!("cancellations cannot time out"),
    }
}

fn progress(
    shared: &Mutex<Shared>,
    transfer: &mut Transfer,
    buffered: &mut usize,
) -> Option<Finished> {
    match transfer {
        Transfer::Read {
            file,
            bytes,
            limit,
            deadline,
            ..
        } => {
            let mut chunk = [0_u8; 8192];
            loop {
                if stopping(shared) {
                    return Some(Finished::Read(Err(stopped_error())));
                }
                if Instant::now() >= *deadline {
                    return Some(Finished::Read(Err(TransferError::TimedOut)));
                }
                let remaining = limit.saturating_sub(bytes.len());
                let requested = chunk.len().min(remaining.saturating_add(1)).max(1);
                match file.read(&mut chunk[..requested]) {
                    Ok(0) => return Some(Finished::Read(Ok(()))),
                    Ok(read) if read > remaining => {
                        return Some(Finished::Read(Err(TransferError::TooLarge)));
                    }
                    Ok(read) => {
                        let previous = bytes.capacity();
                        if bytes.try_reserve_exact(read).is_err() {
                            return Some(Finished::Read(Err(TransferError::BufferLimit)));
                        }
                        let retained = buffered
                            .saturating_sub(previous)
                            .saturating_add(bytes.capacity());
                        let completed = lock(shared).completed_read_bytes;
                        if completed.saturating_add(retained) > MAX_BUFFERED_READ_BYTES {
                            return Some(Finished::Read(Err(TransferError::BufferLimit)));
                        }
                        bytes.extend_from_slice(&chunk[..read]);
                        *buffered = retained;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Some(Finished::Read(Err(TransferError::Io(error)))),
                }
            }
        }
        Transfer::Write {
            file,
            bytes,
            offset,
            deadline,
            ..
        } => loop {
            if stopping(shared) {
                return Some(Finished::Write(Err(stopped_error())));
            }
            if Instant::now() >= *deadline {
                return Some(Finished::Write(Err(TransferError::TimedOut)));
            }
            match file.write(&bytes[*offset..]) {
                Ok(0) => {
                    return Some(Finished::Write(Err(TransferError::Io(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "transfer peer stopped reading",
                    )))));
                }
                Ok(written) => {
                    *offset += written;
                    if *offset == bytes.len() {
                        return Some(Finished::Write(Ok(())));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Some(Finished::Write(Err(TransferError::Io(error)))),
            }
        },
    }
}

fn complete_at(
    shared: &Mutex<Shared>,
    active: &mut Vec<Transfer>,
    buffered: &mut usize,
    index: usize,
    finished: Finished,
    ping: &Ping,
) {
    let transfer = active.swap_remove(index);
    *buffered = buffered.saturating_sub(transfer.buffered());
    let completion = match (transfer, finished) {
        (Transfer::Read { id, bytes, .. }, Finished::Read(result)) => Completion::Read {
            id,
            result: result.map(|()| bytes),
        },
        (Transfer::Write { id, .. }, Finished::Write(result)) => Completion::Write { id, result },
        _ => unreachable!("completion kind must match transfer kind"),
    };
    publish(shared, ping, completion);
}

fn expire(
    shared: &Mutex<Shared>,
    active: &mut Vec<Transfer>,
    buffered: &mut usize,
    ping: &Ping,
    now: Instant,
) {
    for index in (0..active.len()).rev() {
        if active[index].deadline() <= now {
            let finished = match active[index] {
                Transfer::Read { .. } => Finished::Read(Err(TransferError::TimedOut)),
                Transfer::Write { .. } => Finished::Write(Err(TransferError::TimedOut)),
            };
            complete_at(shared, active, buffered, index, finished, ping);
        }
    }
}

fn fail_all(
    shared: &Mutex<Shared>,
    active: &mut Vec<Transfer>,
    buffered: &mut usize,
    ping: &Ping,
    error: io::Error,
) {
    let kind = error.kind();
    for index in (0..active.len()).rev() {
        let finished = io_failure(&active[index], kind);
        complete_at(shared, active, buffered, index, finished, ping);
    }
}

fn fail_pending(
    shared: &Mutex<Shared>,
    pending: &mut VecDeque<Command>,
    ping: &Ping,
    kind: io::ErrorKind,
) {
    for command in pending.drain(..) {
        let error = TransferError::Io(io::Error::new(kind, "transfer worker failed"));
        let completion = match command {
            Command::Read { id, .. } => Completion::Read {
                id,
                result: Err(error),
            },
            Command::Write { id, .. } => Completion::Write {
                id,
                result: Err(error),
            },
            Command::Cancel(_) => unreachable!("cancellations are not pending transfers"),
        };
        publish(shared, ping, completion);
    }
}

fn io_failure(transfer: &Transfer, kind: io::ErrorKind) -> Finished {
    let error = || TransferError::Io(io::Error::new(kind, "transfer descriptor failed"));
    match transfer {
        Transfer::Read { .. } => Finished::Read(Err(error())),
        Transfer::Write { .. } => Finished::Write(Err(error())),
    }
}

fn stopped_error() -> TransferError {
    TransferError::Io(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "transfer worker stopped",
    ))
}

fn publish(shared: &Mutex<Shared>, ping: &Ping, completion: Completion) {
    let mut shared = lock(shared);
    shared.completed_read_bytes = shared
        .completed_read_bytes
        .saturating_add(completion.buffered());
    debug_assert!(shared.completions.len() < MAX_COMPLETIONS);
    shared.completions.push_back(completion);
    drop(shared);
    ping.ping();
}

fn poll_timeout(active: &[Transfer], pending: &VecDeque<Command>, now: Instant) -> i32 {
    let deadline = active
        .iter()
        .map(Transfer::deadline)
        .chain(pending.iter().map(command_deadline))
        .min();
    let Some(deadline) = deadline else {
        return -1;
    };
    let remaining = deadline.saturating_duration_since(now);
    let milliseconds = remaining.as_millis().saturating_add(1);
    i32::try_from(milliseconds).unwrap_or(i32::MAX)
}

fn deadline(timeout: Duration) -> Instant {
    Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now)
}

fn poll(descriptors: &mut [libc::pollfd], timeout: i32) -> io::Result<()> {
    // SAFETY: the slice describes initialized pollfd values and stays alive
    // for the duration of the call.
    let result = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len().try_into().unwrap_or(libc::nfds_t::MAX),
            timeout,
        )
    };
    if result >= 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted {
        for descriptor in descriptors {
            descriptor.revents = 0;
        }
        Ok(())
    } else {
        Err(error)
    }
}

fn nonblocking(fd: OwnedFd) -> io::Result<File> {
    let raw = fd.as_raw_fd();
    // SAFETY: fcntl only inspects or updates flags for this valid owned fd.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd remains owned by `fd`; setting O_NONBLOCK does not alter
    // its ownership or lifetime.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(File::from(fd))
}

fn wake(stream: &UnixStream) -> io::Result<()> {
    match (&*stream).write(&[1]) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error),
    }
}

fn drain(stream: &UnixStream) {
    let mut bytes = [0_u8; 64];
    loop {
        match (&*stream).read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

fn stopping(shared: &Mutex<Shared>) -> bool {
    lock(shared).stop
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::fd::FromRawFd,
        sync::Barrier,
        time::{Duration, Instant},
    };

    fn pipe() -> (OwnedFd, File) {
        let mut fds = [0_i32; 2];
        // SAFETY: `fds` points to storage for the two descriptors returned by
        // pipe2. both are immediately wrapped in owning types on success.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: pipe2 returned two new owned descriptors.
        let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: pipe2 returned two new owned descriptors.
        let writer = unsafe { File::from_raw_fd(fds[1]) };
        (reader, writer)
    }

    fn worker() -> Worker {
        let (ping, _) = calloop::ping::make_ping().unwrap();
        Worker::start(ping).unwrap()
    }

    fn fill_pipe(writer: &mut File) {
        // SAFETY: the file owns a valid descriptor for both fcntl calls.
        let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        // SAFETY: the file remains alive while its status flags are changed.
        let result =
            unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        assert!(result >= 0);
        let bytes = [0_u8; 8192];
        loop {
            match writer.write(&bytes) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("cannot fill test pipe: {error}"),
            }
        }
    }

    fn completion(worker: &Worker, id: u64) -> Completion {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(completion) =
                worker
                    .take_completions()
                    .into_iter()
                    .find(|completion| match completion {
                        Completion::Read { id: current, .. }
                        | Completion::Write { id: current, .. } => *current == id,
                    })
            {
                return completion;
            }
            assert!(Instant::now() < deadline, "transfer did not complete");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn reads_and_writes_without_blocking_the_caller() {
        let worker = worker();
        let (reader, mut writer) = pipe();
        worker.read(1, reader, 32, Duration::from_secs(1)).unwrap();
        writer.write_all(b"clipboard").unwrap();
        drop(writer);
        let Completion::Read { result, .. } = completion(&worker, 1) else {
            panic!("expected a read completion");
        };
        assert_eq!(result.unwrap(), b"clipboard");

        let (reader, writer) = pipe();
        worker
            .write(
                2,
                writer.into(),
                Arc::from(&b"served"[..]),
                Duration::from_secs(1),
            )
            .unwrap();
        let Completion::Write { result, .. } = completion(&worker, 2) else {
            panic!("expected a write completion");
        };
        result.unwrap();
        let mut reader = File::from(reader);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"served");
    }

    #[test]
    fn size_and_deadline_limits_are_enforced() {
        let worker = worker();
        let (reader, mut writer) = pipe();
        worker.read(1, reader, 4, Duration::from_secs(1)).unwrap();
        writer.write_all(b"large").unwrap();
        drop(writer);
        let Completion::Read { result, .. } = completion(&worker, 1) else {
            panic!("expected a read completion");
        };
        assert!(matches!(result, Err(TransferError::TooLarge)));

        let (reader, _writer) = pipe();
        worker
            .read(2, reader, 4, Duration::from_millis(20))
            .unwrap();
        let Completion::Read { result, .. } = completion(&worker, 2) else {
            panic!("expected a read completion");
        };
        assert!(matches!(result, Err(TransferError::TimedOut)));
    }

    #[test]
    fn queued_transfer_deadlines_start_when_submitted() {
        let worker = worker();
        let mut readers = Vec::new();
        for id in 1..=MAX_ACTIVE as u64 {
            let (reader, mut writer) = pipe();
            fill_pipe(&mut writer);
            worker
                .write(
                    id,
                    writer.into(),
                    Arc::from(&b"x"[..]),
                    Duration::from_secs(2),
                )
                .unwrap();
            readers.push(reader);
        }

        let (reader, _writer) = pipe();
        let started = Instant::now();
        worker
            .read(100, reader, 1, Duration::from_millis(30))
            .unwrap();
        let Completion::Read { result, .. } = completion(&worker, 100) else {
            panic!("expected a read completion");
        };
        assert!(matches!(result, Err(TransferError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(readers);
    }

    #[test]
    fn ready_transfers_honor_deadline_and_shutdown() {
        let shared = Mutex::new(Shared::default());
        let mut buffered = 0;
        let (reader, mut writer) = pipe();
        writer.write_all(b"ready").unwrap();
        let mut read = Transfer::Read {
            id: 1,
            file: nonblocking(reader).unwrap(),
            bytes: Vec::new(),
            limit: 32,
            deadline: Instant::now(),
        };
        assert!(matches!(
            progress(&shared, &mut read, &mut buffered),
            Some(Finished::Read(Err(TransferError::TimedOut)))
        ));

        let (reader, writer) = pipe();
        let mut write = Transfer::Write {
            id: 2,
            file: nonblocking(writer.into()).unwrap(),
            bytes: Arc::from(&b"ready"[..]),
            offset: 0,
            deadline: Instant::now(),
        };
        assert!(matches!(
            progress(&shared, &mut write, &mut buffered),
            Some(Finished::Write(Err(TransferError::TimedOut)))
        ));
        lock(&shared).stop = true;
        assert!(matches!(
            progress(&shared, &mut read, &mut buffered),
            Some(Finished::Read(Err(TransferError::Io(error))))
                if error.kind() == io::ErrorKind::BrokenPipe
        ));
        assert!(matches!(
            progress(&shared, &mut write, &mut buffered),
            Some(Finished::Write(Err(TransferError::Io(error))))
                if error.kind() == io::ErrorKind::BrokenPipe
        ));
        drop(reader);
    }

    #[test]
    fn aggregate_read_buffers_are_bounded() {
        let worker = worker();
        let barrier = Arc::new(Barrier::new(6));
        let mut writers = Vec::new();
        for id in 1..=5 {
            let (reader, mut writer) = pipe();
            worker
                .read(id, reader, 1024 * 1024, Duration::from_secs(2))
                .unwrap();
            let barrier = barrier.clone();
            writers.push(thread::spawn(move || {
                let _ = writer.write_all(&vec![b'x'; 1024 * 1024]);
                barrier.wait();
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut limited = false;
        while Instant::now() < deadline && !limited {
            limited = worker.take_completions().into_iter().any(|completion| {
                matches!(
                    completion,
                    Completion::Read {
                        result: Err(TransferError::BufferLimit),
                        ..
                    }
                )
            });
            thread::sleep(Duration::from_millis(2));
        }
        barrier.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        assert!(limited);
    }

    #[test]
    fn completed_reads_remain_inside_the_aggregate_limit() {
        let worker = worker();
        let mut writers = Vec::new();
        for id in 1..=5 {
            let (reader, mut writer) = pipe();
            worker
                .read(id, reader, 1024 * 1024, Duration::from_secs(2))
                .unwrap();
            writers.push(thread::spawn(move || {
                let _ = writer.write_all(&vec![b'x'; 1024 * 1024]);
            }));
        }
        for writer in writers {
            writer.join().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock(&worker.shared).completions.len() < 5 {
            assert!(Instant::now() < deadline, "transfers did not complete");
            thread::sleep(Duration::from_millis(2));
        }
        let completions = worker.take_completions();
        assert_eq!(
            completions
                .iter()
                .filter(|completion| matches!(completion, Completion::Read { result: Ok(_), .. }))
                .count(),
            4
        );
        assert!(completions.iter().any(|completion| matches!(
            completion,
            Completion::Read {
                result: Err(TransferError::BufferLimit),
                ..
            }
        )));
    }

    #[test]
    fn cancellation_and_shutdown_interrupt_pending_io() {
        let worker = worker();
        let (reader, _writer) = pipe();
        worker.read(1, reader, 32, Duration::from_secs(30)).unwrap();
        worker.cancel(1).unwrap();
        drop(worker);
    }
}
