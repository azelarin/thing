//! bounded ownership and output capture for configuration-managed children.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            net::UnixStream,
            process::{CommandExt, ExitStatusExt},
        },
    },
    process::{Child, Command, ExitStatus, Stdio},
    ptr,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use calloop::ping::Ping;
use thing_runtime::{
    ManagedProcessDefinition, ManagedProcessShutdownSignal, ManagedProcessSnapshot,
    ManagedProcessState, ManagedProcessStatus,
};

const MAX_KEYS: usize = 32;
const MAX_UNREAPED: usize = 32;
const MAX_STARTS: usize = 128;
const MAX_ARGUMENTS: usize = 256;
const MAX_ARGUMENT_BYTES: usize = 4096;
const MAX_ARGUMENT_TOTAL_BYTES: usize = 64 * 1024;
const MAX_WORKING_DIRECTORY_BYTES: usize = 4096;
const MAX_STREAM_BYTES: usize = 64 * 1024;
const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
const RUN_BUFFER_RESERVATION: usize = MAX_STREAM_BYTES * 4;
const MAX_ERROR_BYTES: usize = 4096;
const MAX_PROC_MOUNTINFO_BYTES: u64 = 1024 * 1024;
const READ_CHUNK_BYTES: usize = 8192;
const MAX_READ_BYTES_PER_TURN: usize = 64 * 1024;
const PUBLISH_INTERVAL: Duration = Duration::from_millis(50);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const GROUP_SCAN_INTERVAL: Duration = Duration::from_millis(250);
const POST_KILL_GROUP_SCAN_INTERVAL: Duration = Duration::from_millis(75);
const STUCK_GROUP_SCAN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_STUCK_GROUP_SCAN_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(5);
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);
const DEFAULT_DRAIN_GRACE: Duration = Duration::from_millis(250);
const DEFAULT_DROP_WAIT: Duration = Duration::from_secs(1);
const MIN_SCHEDULE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_SCHEDULE_INTERVAL: Duration = Duration::from_secs(365 * 24 * 60 * 60);
const XDG_ACTIVATION_TOKEN: &str = "XDG_ACTIVATION_TOKEN";
const DESKTOP_STARTUP_ID: &str = "DESKTOP_STARTUP_ID";

static MANAGER_ACTIVE: AtomicBool = AtomicBool::new(false);

struct ManagerLease;

impl ManagerLease {
    fn acquire() -> io::Result<Self> {
        MANAGER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a managed process worker is already active",
                )
            })
    }
}

impl Drop for ManagerLease {
    fn drop(&mut self) {
        MANAGER_ACTIVE.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShutdownSignal {
    Terminate,
    Interrupt,
}

impl ShutdownSignal {
    fn raw(self) -> i32 {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Interrupt => libc::SIGINT,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Definition {
    pub key: String,
    pub shutdown_signal: ShutdownSignal,
    pub command: Option<ScheduledCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScheduledCommand {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub interval: Option<Duration>,
}

impl From<&ManagedProcessDefinition> for Definition {
    fn from(definition: &ManagedProcessDefinition) -> Self {
        Self {
            key: definition.key.clone(),
            shutdown_signal: match definition.shutdown_signal {
                ManagedProcessShutdownSignal::Terminate => ShutdownSignal::Terminate,
                ManagedProcessShutdownSignal::Interrupt => ShutdownSignal::Interrupt,
            },
            command: definition.command.as_ref().map(|command| ScheduledCommand {
                argv: command.argv.clone(),
                cwd: command.cwd.clone(),
                interval: command.interval_ms.map(Duration::from_millis),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    Idle,
    Starting,
    Running,
    Stopping,
    Exited,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct State {
    pub key: String,
    pub status: Status,
    pub stdout: Arc<str>,
    pub stderr: Arc<str>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub error: Arc<str>,
    token: Option<RunToken>,
    retained_bytes: usize,
    terminal_failure: bool,
}

impl State {
    fn idle(key: String) -> Self {
        Self {
            key,
            status: Status::Idle,
            stdout: Arc::from(""),
            stderr: Arc::from(""),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: None,
            exit_signal: None,
            error: Arc::from(""),
            token: None,
            retained_bytes: 0,
            terminal_failure: false,
        }
    }

    fn prepare_for_start(&mut self, token: RunToken, retain_result: bool) {
        self.status = Status::Starting;
        if !retain_result {
            self.stdout = Arc::from("");
            self.stderr = Arc::from("");
            self.stdout_truncated = false;
            self.stderr_truncated = false;
            self.exit_code = None;
            self.exit_signal = None;
            self.retained_bytes = 0;
        }
        self.error = Arc::from("");
        self.token = Some(token);
        self.terminal_failure = false;
    }

    pub(crate) fn running(&self) -> bool {
        matches!(
            self.status,
            Status::Starting | Status::Running | Status::Stopping
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Snapshot {
    pub states: Arc<[State]>,
}

impl Snapshot {
    pub(crate) fn runtime(&self) -> ManagedProcessSnapshot {
        ManagedProcessSnapshot {
            states: self
                .states
                .iter()
                .map(|state| ManagedProcessState {
                    key: state.key.clone(),
                    status: match state.status {
                        Status::Idle => ManagedProcessStatus::Idle,
                        Status::Starting => ManagedProcessStatus::Starting,
                        Status::Running => ManagedProcessStatus::Running,
                        Status::Stopping => ManagedProcessStatus::Stopping,
                        Status::Exited => ManagedProcessStatus::Exited,
                        Status::Failed => ManagedProcessStatus::Failed,
                    },
                    stdout: state.stdout.clone(),
                    stderr: state.stderr.clone(),
                    stdout_truncated: state.stdout_truncated,
                    stderr_truncated: state.stderr_truncated,
                    exit_code: state.exit_code,
                    exit_signal: state.exit_signal,
                    error: state.error.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RunToken {
    generation: u64,
    key_incarnation: u64,
    run_incarnation: u64,
}

#[derive(Clone)]
struct Registered {
    definition: Definition,
    incarnation: u64,
    schedule: Option<Schedule>,
}

#[derive(Clone)]
struct Schedule {
    deadline: ScheduleDeadline,
    pending_refresh: bool,
}

#[derive(Clone, Copy)]
enum ScheduleDeadline {
    Disarmed,
    Next(Instant),
    DueBlocked(u64),
    None,
}

struct Occupied {
    key: String,
    shutdown_signal: ShutdownSignal,
    reserved_bytes: usize,
    scheduled: bool,
}

struct Start {
    token: RunToken,
    key: String,
    argv: Vec<String>,
    cwd: Option<String>,
    scheduled: bool,
}

#[derive(Default)]
struct Shared {
    generation: u64,
    registrations: BTreeMap<String, Registered>,
    states: BTreeMap<String, State>,
    starts: VecDeque<Start>,
    occupied: BTreeMap<RunToken, Occupied>,
    stops: BTreeMap<RunToken, ShutdownSignal>,
    latest: Option<Snapshot>,
    next_key_incarnation: u64,
    next_run_incarnation: u64,
    capacity_epoch: u64,
    reserved_bytes: usize,
    shutdown: bool,
    #[cfg(test)]
    before_dequeue: Option<Arc<SpawnGate>>,
    #[cfg(test)]
    before_spawn: Option<Arc<SpawnGate>>,
}

#[derive(Clone, Copy)]
enum ScopeChange {
    Replace,
    Reconcile,
}

pub(crate) struct PreparedScope {
    expected_generation: u64,
    expected_next_key_incarnation: u64,
    expected_preserved_states: BTreeMap<String, State>,
    next_generation: u64,
    next_key_incarnation: u64,
    definitions: BTreeMap<String, Definition>,
    preserved: BTreeSet<String>,
    incarnations: BTreeMap<String, u64>,
    snapshot: Snapshot,
}

pub(crate) struct ScopeActivation {
    generation: u64,
    keys: Vec<String>,
}

impl PreparedScope {
    pub(crate) fn runtime(&self) -> ManagedProcessSnapshot {
        self.snapshot.runtime()
    }
}

pub(crate) fn initial_scope_snapshot(
    definitions: Vec<Definition>,
) -> io::Result<ManagedProcessSnapshot> {
    let definitions = validate_definitions(definitions)?;
    let states = definitions
        .into_iter()
        .map(|definition| (definition.key.clone(), State::idle(definition.key)))
        .collect::<BTreeMap<_, _>>();
    Ok(Snapshot {
        states: states.values().cloned().collect::<Vec<_>>().into(),
    }
    .runtime())
}

#[cfg(test)]
#[derive(Default)]
struct SpawnGate {
    state: Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
impl SpawnGate {
    fn arrive(&self) {
        let mut state = lock(&self.state);
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_until_reached(&self) {
        let mut state = lock(&self.state);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !state.0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for managed process gate"
            );
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            assert!(
                state.0 || !timeout.timed_out(),
                "timed out waiting for managed process gate"
            );
        }
    }

    fn release(&self) {
        lock(&self.state).1 = true;
        self.changed.notify_all();
    }
}

#[derive(Clone, Copy)]
struct ChildSetup {
    sigchld_blocked: bool,
    parent_pid: libc::pid_t,
}

impl ChildSetup {
    fn capture() -> io::Result<Self> {
        let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: a null set asks pthread_sigmask to copy the current mask.
        let result =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, ptr::null(), mask.as_mut_ptr()) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        // SAFETY: pthread_sigmask initialized mask after returning success.
        let member = unsafe { libc::sigismember(mask.assume_init_ref(), libc::SIGCHLD) };
        let sigchld_blocked = match member {
            0 => false,
            1 => true,
            _ => return Err(io::Error::last_os_error()),
        };
        // SAFETY: getpid has no preconditions.
        let parent_pid = unsafe { libc::getpid() };
        Ok(Self {
            sigchld_blocked,
            parent_pid,
        })
    }
}

#[derive(Clone, Copy)]
struct Limits {
    stop_grace: Duration,
    shutdown_grace: Duration,
    drain_grace: Duration,
    drop_wait: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            stop_grace: DEFAULT_STOP_GRACE,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
            drain_grace: DEFAULT_DRAIN_GRACE,
            drop_wait: DEFAULT_DROP_WAIT,
        }
    }
}

pub(crate) struct Manager {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    ping: Ping,
    worker: Option<JoinHandle<()>>,
    worker_done: Receiver<()>,
    limits: Limits,
}

impl Manager {
    pub(crate) fn start(ping: Ping) -> io::Result<Self> {
        Self::start_inner(
            ping,
            Limits::default(),
            Some(ManagerLease::acquire()?),
            true,
        )
    }

    #[cfg(test)]
    fn start_with_limits(ping: Ping, limits: Limits) -> io::Result<Self> {
        Self::start_inner(ping, limits, None, false)
    }

    #[cfg(test)]
    fn start_with_singleton(ping: Ping, limits: Limits) -> io::Result<Self> {
        Self::start_inner(ping, limits, Some(ManagerLease::acquire()?), false)
    }

    fn start_inner(
        ping: Ping,
        limits: Limits,
        lease: Option<ManagerLease>,
        prepare_sigchld: bool,
    ) -> io::Result<Self> {
        ensure_authoritative_procfs()?;
        if prepare_sigchld {
            crate::launch::prepare_managed_sigchld()?;
        }
        let setup = ChildSetup::capture()?;
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let worker_ping = ping.clone();
        let (done_tx, worker_done) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("thing-managed-processes".into())
            .spawn(move || {
                let _lease = lease;
                run_worker(state, reader, worker_ping, setup, limits);
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            shared,
            wake,
            ping,
            worker: Some(worker),
            worker_done,
            limits,
        })
    }

    pub(crate) fn prepare_replace_scope(
        &self,
        definitions: Vec<Definition>,
    ) -> io::Result<PreparedScope> {
        self.prepare_scope(definitions, ScopeChange::Replace)
    }

    pub(crate) fn prepare_reconcile_scope(
        &self,
        definitions: Vec<Definition>,
    ) -> io::Result<PreparedScope> {
        self.prepare_scope(definitions, ScopeChange::Reconcile)
    }

    pub(crate) fn project_replace_scope(
        &self,
        definitions: Vec<Definition>,
    ) -> io::Result<ManagedProcessSnapshot> {
        self.prepare_scope(definitions, ScopeChange::Replace)
            .map(|prepared| prepared.runtime())
    }

    pub(crate) fn project_reconcile_scope(
        &self,
        definitions: Vec<Definition>,
    ) -> io::Result<ManagedProcessSnapshot> {
        self.prepare_scope(definitions, ScopeChange::Reconcile)
            .map(|prepared| prepared.runtime())
    }

    fn prepare_scope(
        &self,
        definitions: Vec<Definition>,
        change: ScopeChange,
    ) -> io::Result<PreparedScope> {
        let definitions = validate_definitions(definitions)?;
        let definitions = definitions
            .into_iter()
            .map(|definition| (definition.key.clone(), definition))
            .collect::<BTreeMap<_, _>>();
        let shared = lock(&self.shared);
        ensure_active(&shared)?;
        if matches!(change, ScopeChange::Reconcile) {
            for (key, registered) in &shared.registrations {
                if let Some(definition) = definitions.get(key)
                    && definition != &registered.definition
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("managed process {key:?} has conflicting declarations"),
                    ));
                }
            }
        }
        let preserved = definitions
            .iter()
            .filter_map(|(key, definition)| {
                let registered = shared.registrations.get(key)?;
                (registered.definition == *definition
                    && (matches!(change, ScopeChange::Reconcile) || definition.command.is_some()))
                .then(|| key.clone())
            })
            .collect::<BTreeSet<_>>();
        let new_count = definitions.len().saturating_sub(preserved.len());
        let mut next_key_incarnation = shared.next_key_incarnation;
        let values = allocate_many(&mut next_key_incarnation, new_count)?;
        let mut values = values.into_iter();
        let incarnations = definitions
            .keys()
            .filter(|key| !preserved.contains(*key))
            .map(|key| {
                (
                    key.clone(),
                    values
                        .next()
                        .expect("every new process key must have an incarnation"),
                )
            })
            .collect();
        let expected_preserved_states = preserved
            .iter()
            .map(|key| {
                (
                    key.clone(),
                    shared
                        .states
                        .get(key)
                        .cloned()
                        .expect("every registered process must have state"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let states = definitions
            .keys()
            .map(|key| {
                expected_preserved_states
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| State::idle(key.clone()))
            })
            .collect::<Vec<_>>()
            .into();
        Ok(PreparedScope {
            expected_generation: shared.generation,
            expected_next_key_incarnation: shared.next_key_incarnation,
            expected_preserved_states,
            next_generation: advance(shared.generation)?,
            next_key_incarnation,
            definitions,
            preserved,
            incarnations,
            snapshot: Snapshot { states },
        })
    }

    pub(crate) fn commit_scope(&self, prepared: PreparedScope) -> io::Result<ScopeActivation> {
        let mut shared = lock(&self.shared);
        ensure_active(&shared)?;
        if shared.generation != prepared.expected_generation
            || shared.next_key_incarnation != prepared.expected_next_key_incarnation
            || prepared
                .expected_preserved_states
                .iter()
                .any(|(key, state)| shared.states.get(key) != Some(state))
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "managed process scope changed after preflight",
            ));
        }
        shared.generation = prepared.next_generation;
        shared.next_key_incarnation = prepared.next_key_incarnation;
        let retired = shared
            .registrations
            .keys()
            .filter(|key| !prepared.preserved.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in retired {
            retire_key(&mut shared, &key);
            shared.registrations.remove(&key);
            shared.states.remove(&key);
        }
        let mut keys = Vec::new();
        for (key, definition) in prepared.definitions {
            if prepared.preserved.contains(&key) {
                continue;
            }
            let incarnation = prepared.incarnations[&key];
            let schedule = definition.command.as_ref().map(|_| Schedule {
                deadline: ScheduleDeadline::Disarmed,
                pending_refresh: false,
            });
            if schedule.is_some() {
                keys.push(key.clone());
            }
            shared.states.insert(key.clone(), State::idle(key.clone()));
            shared.registrations.insert(
                key,
                Registered {
                    definition,
                    incarnation,
                    schedule,
                },
            );
        }
        shared.capacity_epoch = shared.capacity_epoch.wrapping_add(1);
        publish_locked(&mut shared);
        self.ping.ping();
        Ok(ScopeActivation {
            generation: prepared.next_generation,
            keys,
        })
    }

    pub(crate) fn arm_scope(&self, activation: ScopeActivation) {
        let now = Instant::now();
        let changed = {
            let mut shared = lock(&self.shared);
            if shared.generation != activation.generation || shared.shutdown {
                return;
            }
            let mut changed = false;
            for key in activation.keys {
                let Some(schedule) = shared
                    .registrations
                    .get_mut(&key)
                    .and_then(|registered| registered.schedule.as_mut())
                else {
                    continue;
                };
                if matches!(schedule.deadline, ScheduleDeadline::Disarmed) {
                    schedule.deadline = ScheduleDeadline::Next(now);
                    changed = true;
                }
            }
            if changed {
                shared.capacity_epoch = shared.capacity_epoch.wrapping_add(1);
            }
            changed
        };
        if changed {
            let _ = wake(&self.wake);
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_scope(&self, definitions: Vec<Definition>) -> io::Result<Snapshot> {
        let prepared = self.prepare_replace_scope(definitions)?;
        let snapshot = prepared.snapshot.clone();
        let activation = self.commit_scope(prepared)?;
        self.arm_scope(activation);
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(crate) fn reconcile_scope(&self, definitions: Vec<Definition>) -> io::Result<Snapshot> {
        let prepared = self.prepare_reconcile_scope(definitions)?;
        let snapshot = prepared.snapshot.clone();
        let activation = self.commit_scope(prepared)?;
        self.arm_scope(activation);
        Ok(snapshot)
    }

    pub(crate) fn start_process(
        &self,
        key: &str,
        argv: Vec<String>,
        cwd: Option<String>,
    ) -> io::Result<()> {
        validate_command(&argv, cwd.as_deref())?;
        let token = {
            let mut shared = lock(&self.shared);
            ensure_active(&shared)?;
            let registered = shared.registrations.get(key).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "unknown managed process")
            })?;
            if registered.definition.command.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "scheduled processes use refresh instead of start",
                ));
            }
            queue_start_locked(&mut shared, key, argv, cwd, false)?
        };
        #[cfg(test)]
        let before_spawn = lock(&self.shared).before_spawn.clone();
        self.ping.ping();
        #[cfg(test)]
        if let Some(gate) = before_spawn {
            gate.wait_until_reached();
        }
        if let Err(error) = wake(&self.wake) {
            retire_failed_start(&self.shared, token, &error.to_string(), &self.ping);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn refresh_process(&self, key: &str) -> io::Result<()> {
        let changed = {
            let mut shared = lock(&self.shared);
            ensure_active(&shared)?;
            let status = shared
                .states
                .get(key)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed process"))?
                .status;
            let schedule = shared
                .registrations
                .get_mut(key)
                .and_then(|registered| registered.schedule.as_mut())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "managed process is not scheduled",
                    )
                })?;
            match status {
                Status::Starting => false,
                Status::Running | Status::Stopping => {
                    let changed = !schedule.pending_refresh;
                    schedule.pending_refresh = true;
                    changed
                }
                Status::Idle | Status::Exited | Status::Failed => {
                    let changed = !schedule.pending_refresh;
                    schedule.pending_refresh = true;
                    changed
                }
            }
        };
        if changed {
            let _ = wake(&self.wake);
        }
        Ok(())
    }

    pub(crate) fn cancel_process(&self, key: &str) -> io::Result<()> {
        let now = Instant::now();
        let changed = {
            let mut shared = lock(&self.shared);
            ensure_active(&shared)?;
            let registered = shared.registrations.get(key).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "unknown managed process")
            })?;
            let interval = registered
                .definition
                .command
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "managed process is not scheduled",
                    )
                })?
                .interval;
            let token = shared.states[key].token;
            let schedule = shared
                .registrations
                .get_mut(key)
                .and_then(|registered| registered.schedule.as_mut())
                .expect("scheduled definitions must have schedule state");
            schedule.pending_refresh = false;
            schedule.deadline = interval.map_or(ScheduleDeadline::None, |interval| {
                let next = now.checked_add(interval).unwrap_or(now);
                match schedule.deadline {
                    ScheduleDeadline::Next(current) if current > next => {
                        ScheduleDeadline::Next(current)
                    }
                    _ => ScheduleDeadline::Next(next),
                }
            });
            if let Some(token) = token {
                let signal = shared
                    .occupied
                    .get(&token)
                    .map(|occupied| occupied.shutdown_signal)
                    .unwrap_or(ShutdownSignal::Terminate);
                shared.stops.insert(token, signal);
                if let Some(state) = shared.states.get_mut(key)
                    && state.token == Some(token)
                {
                    state.status = Status::Stopping;
                }
            }
            shared.latest = Some(snapshot_locked(&shared));
            true
        };
        if changed {
            self.ping.ping();
            let _ = wake(&self.wake);
        }
        Ok(())
    }

    pub(crate) fn stop_process(&self, key: &str) -> io::Result<()> {
        let changed = {
            let mut shared = lock(&self.shared);
            ensure_active(&shared)?;
            if !shared.registrations.contains_key(key) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "unknown managed process",
                ));
            }
            let Some(token) = shared.states[key].token else {
                return Ok(());
            };
            let signal = shared
                .occupied
                .get(&token)
                .map(|occupied| occupied.shutdown_signal)
                .unwrap_or(ShutdownSignal::Terminate);
            shared.stops.insert(token, signal);
            if let Some(state) = shared.states.get_mut(key)
                && state.token == Some(token)
            {
                state.status = Status::Stopping;
            }
            shared.latest = Some(snapshot_locked(&shared));
            true
        };
        if changed {
            self.ping.ping();
            let _ = wake(&self.wake);
        }
        Ok(())
    }

    pub(crate) fn take_snapshot(&self) -> Option<Snapshot> {
        lock(&self.shared).latest.take()
    }

    #[cfg(test)]
    fn snapshot(&self) -> Snapshot {
        snapshot_locked(&lock(&self.shared))
    }
}

fn queue_start_locked(
    shared: &mut Shared,
    key: &str,
    argv: Vec<String>,
    cwd: Option<String>,
    scheduled: bool,
) -> io::Result<RunToken> {
    let registered = shared
        .registrations
        .get(key)
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed process"))?;
    let state = shared
        .states
        .get(key)
        .expect("every registration must have state");
    if state.running() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "managed process is already active",
        ));
    }
    if shared.occupied.values().any(|occupied| occupied.key == key) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the previous managed process incarnation has not been reaped",
        ));
    }
    if shared.occupied.len() >= MAX_UNREAPED {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "too many managed process incarnations remain unreaped",
        ));
    }
    if shared.starts.len() >= MAX_STARTS {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "managed process start queue is full",
        ));
    }
    let retained = shared.states[key].retained_bytes;
    let reservation = RUN_BUFFER_RESERVATION;
    let base = if scheduled {
        shared.reserved_bytes
    } else {
        shared.reserved_bytes.saturating_sub(retained)
    };
    let next_retained = base
        .checked_add(reservation)
        .ok_or_else(|| io::Error::other("managed process buffer accounting overflow"))?;
    if next_retained > MAX_RETAINED_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "managed process output buffer limit reached",
        ));
    }
    let run_incarnation = allocate(&mut shared.next_run_incarnation)?;
    let token = RunToken {
        generation: shared.generation,
        key_incarnation: registered.incarnation,
        run_incarnation,
    };
    shared.reserved_bytes = next_retained;
    shared.occupied.insert(
        token,
        Occupied {
            key: key.to_owned(),
            shutdown_signal: registered.definition.shutdown_signal,
            reserved_bytes: reservation,
            scheduled,
        },
    );
    shared
        .states
        .get_mut(key)
        .expect("every registration must have state")
        .prepare_for_start(token, scheduled);
    shared.starts.push_back(Start {
        token,
        key: key.to_owned(),
        argv,
        cwd,
        scheduled,
    });
    shared.latest = Some(snapshot_locked(shared));
    Ok(token)
}

impl Drop for Manager {
    fn drop(&mut self) {
        {
            let mut shared = lock(&self.shared);
            shared.shutdown = true;
            let tokens = shared.occupied.keys().copied().collect::<Vec<_>>();
            for token in tokens {
                let signal = shared
                    .occupied
                    .get(&token)
                    .map(|occupied| occupied.shutdown_signal)
                    .unwrap_or(ShutdownSignal::Terminate);
                shared.stops.insert(token, signal);
            }
        }
        let _ = wake(&self.wake);
        let finished = self.worker_done.recv_timeout(self.limits.drop_wait).is_ok();
        if let Some(worker) = self.worker.take()
            && finished
        {
            let _ = worker.join();
        }
    }
}

struct Run {
    token: RunToken,
    key: String,
    scheduled: bool,
    child: Child,
    pid: u32,
    stdout: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
    stdout_tail: Utf8Tail,
    stderr_tail: Utf8Tail,
    exit_observed: bool,
    identity_lost: bool,
    next_group_check: Instant,
    group_scan_delay: Duration,
    wait_status: Option<ExitStatus>,
    drain_deadline: Option<Instant>,
    stop_deadline: Option<Instant>,
    kill_sent: bool,
    dirty: bool,
    next_publish: Instant,
}

impl Run {
    fn descriptors(&self) -> impl Iterator<Item = (Stream, i32)> {
        [
            self.stdout
                .as_ref()
                .map(|file| (Stream::Stdout, file.as_raw_fd())),
            self.stderr
                .as_ref()
                .map(|file| (Stream::Stderr, file.as_raw_fd())),
        ]
        .into_iter()
        .flatten()
    }

    fn tail_mut(&mut self, stream: Stream) -> &mut Utf8Tail {
        match stream {
            Stream::Stdout => &mut self.stdout_tail,
            Stream::Stderr => &mut self.stderr_tail,
        }
    }

    fn file_mut(&mut self, stream: Stream) -> &mut Option<std::fs::File> {
        match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        }
    }

    fn close_stream(&mut self, stream: Stream) {
        self.file_mut(stream).take();
        self.tail_mut(stream).finish();
        self.dirty = true;
        self.next_publish = Instant::now();
    }

    fn drained(&self) -> bool {
        self.stdout.is_none() && self.stderr.is_none()
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

fn run_worker(
    shared: Arc<Mutex<Shared>>,
    wake_stream: UnixStream,
    ping: Ping,
    setup: ChildSetup,
    limits: Limits,
) {
    let mut runs = BTreeMap::<RunToken, Run>::new();
    let mut listen_wake = true;
    loop {
        maintain(&shared, &mut runs, &ping, setup, limits);
        if should_finish(&shared, &runs) {
            break;
        }
        let mut descriptors = Vec::with_capacity(usize::from(listen_wake) + runs.len() * 2);
        let mut owners = Vec::with_capacity(runs.len() * 2);
        if listen_wake {
            descriptors.push(libc::pollfd {
                fd: wake_stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for (token, run) in &runs {
            for (stream, fd) in run.descriptors() {
                descriptors.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                owners.push((*token, stream));
            }
        }
        let now = Instant::now();
        let timeout = poll_timeout(&runs, next_schedule_deadline(&shared), now);
        if poll(&mut descriptors, timeout).is_err() {
            mark_worker_failure(&shared, &mut runs, &ping, "managed process poll failed");
            listen_wake = false;
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        let pipe_offset = usize::from(listen_wake);
        if listen_wake {
            let revents = descriptors[0].revents;
            if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                listen_wake = false;
            } else if revents & libc::POLLIN != 0 {
                drain_wake(&wake_stream);
            }
        }
        for (descriptor, (token, stream)) in descriptors.into_iter().skip(pipe_offset).zip(owners) {
            if descriptor.revents == 0 {
                continue;
            }
            let Some(run) = runs.get_mut(&token) else {
                continue;
            };
            if descriptor.revents & libc::POLLNVAL != 0 {
                set_run_error(&shared, run, "managed process pipe became invalid", &ping);
                run.close_stream(stream);
            } else if descriptor.revents & libc::POLLERR != 0 {
                set_run_error(&shared, run, "managed process pipe failed", &ping);
                run.close_stream(stream);
            } else {
                read_stream(&shared, run, stream, &ping);
            }
        }
        maintain(&shared, &mut runs, &ping, setup, limits);
    }
}

fn maintain(
    shared: &Mutex<Shared>,
    runs: &mut BTreeMap<RunToken, Run>,
    ping: &Ping,
    setup: ChildSetup,
    limits: Limits,
) {
    apply_schedules(shared, ping, Instant::now());
    apply_starts(shared, runs, ping, setup, limits);
    apply_stops(shared, runs, ping, limits);
    let now = Instant::now();
    let tokens = runs.keys().copied().collect::<Vec<_>>();
    for token in tokens {
        let Some(run) = runs.get_mut(&token) else {
            continue;
        };
        if run.wait_status.is_none() && !run.identity_lost && !run.exit_observed {
            match exit_observed(run.pid) {
                Ok(true) => {
                    run.exit_observed = true;
                    run.next_group_check = now;
                    run.drain_deadline = Some(now + limits.drain_grace);
                    if run.stop_deadline.is_none() {
                        signal_owned(run, shutdown_for(shared, run.token).raw());
                        run.stop_deadline = Some(now + stop_grace(shared, limits));
                        mark_stopping(shared, run, ping);
                    }
                }
                Ok(false) => {}
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    lose_child_identity(shared, run, ping, limits.drain_grace);
                }
                Err(error) => set_run_error(
                    shared,
                    run,
                    &format!("cannot inspect managed process: {error}"),
                    ping,
                ),
            }
        }
        if escalation_due(run, now) {
            signal_owned(run, libc::SIGKILL);
            run.kill_sent = true;
            run.group_scan_delay = POST_KILL_GROUP_SCAN_INTERVAL;
            run.next_group_check = run
                .next_group_check
                .min(now + POST_KILL_GROUP_SCAN_INTERVAL);
        }
        if run.wait_status.is_none()
            && !run.identity_lost
            && run.exit_observed
            && now >= run.next_group_check
        {
            match initial_group_has_other_members(run.pid) {
                Ok(true) => {
                    defer_group_scan(run, now, GROUP_SCAN_INTERVAL);
                }
                Ok(false) => match run.child.try_wait() {
                    Ok(Some(status)) => {
                        run.wait_status = Some(status);
                        run.stop_deadline = None;
                    }
                    Ok(None) => {
                        defer_group_scan(run, now, GROUP_SCAN_INTERVAL);
                        set_run_error(
                            shared,
                            run,
                            "managed process exit observation changed unexpectedly",
                            ping,
                        );
                    }
                    Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                        lose_child_identity(shared, run, ping, limits.drain_grace);
                    }
                    Err(error) => {
                        defer_group_scan(run, now, STUCK_GROUP_SCAN_INTERVAL);
                        set_run_error(
                            shared,
                            run,
                            &format!("cannot reap managed process: {error}"),
                            ping,
                        );
                    }
                },
                Err(error) => {
                    defer_group_scan(run, now, STUCK_GROUP_SCAN_INTERVAL);
                    set_run_error(
                        shared,
                        run,
                        &format!("cannot inspect managed process group: {error}"),
                        ping,
                    );
                }
            }
        }
        if run
            .drain_deadline
            .is_some_and(|deadline| deadline <= now && !run.drained())
        {
            if run.stdout.is_some() {
                run.close_stream(Stream::Stdout);
            }
            if run.stderr.is_some() {
                run.close_stream(Stream::Stderr);
            }
        }
        if run.dirty && now >= run.next_publish {
            publish_run(shared, run, ping);
            run.next_publish = now + PUBLISH_INTERVAL;
            run.dirty = false;
        }
    }
    let finished = runs
        .iter()
        .filter_map(|(token, run)| (run.wait_status.is_some() && run.drained()).then_some(*token))
        .collect::<Vec<_>>();
    for token in finished {
        if let Some(mut run) = runs.remove(&token) {
            finish_run(shared, &mut run, ping);
        }
    }
    apply_schedules(shared, ping, Instant::now());
    apply_starts(shared, runs, ping, setup, limits);
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScheduledCapacity {
    Ready,
    Wait,
    Buffers,
}

fn apply_schedules(shared: &Mutex<Shared>, ping: &Ping, now: Instant) {
    let mut shared = lock(shared);
    let keys = shared.registrations.keys().cloned().collect::<Vec<_>>();
    let capacity_epoch = shared.capacity_epoch;
    let mut changed = false;
    for key in keys {
        let Some(registered) = shared.registrations.get(&key) else {
            continue;
        };
        let Some(command) = registered.definition.command.clone() else {
            continue;
        };
        let schedule = registered
            .schedule
            .as_ref()
            .expect("scheduled definitions must have schedule state");
        let deadline_due = match schedule.deadline {
            ScheduleDeadline::Next(deadline) => deadline <= now,
            ScheduleDeadline::DueBlocked(epoch) => epoch != capacity_epoch,
            ScheduleDeadline::Disarmed | ScheduleDeadline::None => false,
        };
        if !deadline_due && !schedule.pending_refresh {
            continue;
        }
        let status = shared.states[&key].status;
        if matches!(status, Status::Starting) {
            if schedule.pending_refresh {
                shared
                    .registrations
                    .get_mut(&key)
                    .and_then(|registered| registered.schedule.as_mut())
                    .expect("scheduled definitions must have schedule state")
                    .pending_refresh = false;
                changed = true;
            }
            if deadline_due {
                advance_schedule_deadline(&mut shared, &key, &command, now);
            }
            continue;
        }
        if matches!(status, Status::Running | Status::Stopping) {
            if deadline_due {
                advance_schedule_deadline(&mut shared, &key, &command, now);
            }
            continue;
        }
        match scheduled_capacity(&shared, &key) {
            ScheduledCapacity::Wait => {
                let schedule = shared
                    .registrations
                    .get_mut(&key)
                    .and_then(|registered| registered.schedule.as_mut())
                    .expect("scheduled definitions must have schedule state");
                schedule.pending_refresh = false;
                schedule.deadline = ScheduleDeadline::DueBlocked(capacity_epoch);
            }
            ScheduledCapacity::Buffers => {
                let schedule = shared
                    .registrations
                    .get_mut(&key)
                    .and_then(|registered| registered.schedule.as_mut())
                    .expect("scheduled definitions must have schedule state");
                schedule.pending_refresh = false;
                schedule.deadline = command.interval.map_or(ScheduleDeadline::None, |interval| {
                    ScheduleDeadline::Next(now + interval)
                });
                let state = shared
                    .states
                    .get_mut(&key)
                    .expect("every registration must have state");
                state.status = Status::Failed;
                state.error = Arc::from("managed process output buffer limit reached");
                changed = true;
            }
            ScheduledCapacity::Ready => {
                match queue_start_locked(
                    &mut shared,
                    &key,
                    command.argv.clone(),
                    command.cwd.clone(),
                    true,
                ) {
                    Ok(_) => {
                        let schedule = shared
                            .registrations
                            .get_mut(&key)
                            .and_then(|registered| registered.schedule.as_mut())
                            .expect("scheduled definitions must have schedule state");
                        schedule.pending_refresh = false;
                        schedule.deadline =
                            command.interval.map_or(ScheduleDeadline::None, |interval| {
                                ScheduleDeadline::Next(now + interval)
                            });
                        changed = true;
                    }
                    Err(error) => {
                        let schedule = shared
                            .registrations
                            .get_mut(&key)
                            .and_then(|registered| registered.schedule.as_mut())
                            .expect("scheduled definitions must have schedule state");
                        schedule.pending_refresh = false;
                        schedule.deadline =
                            command.interval.map_or(ScheduleDeadline::None, |interval| {
                                ScheduleDeadline::Next(now + interval)
                            });
                        let state = shared
                            .states
                            .get_mut(&key)
                            .expect("every registration must have state");
                        state.status = Status::Failed;
                        state.error = Arc::from(bounded(&error.to_string(), MAX_ERROR_BYTES));
                        changed = true;
                    }
                }
            }
        }
    }
    if changed {
        shared.latest = Some(snapshot_locked(&shared));
        drop(shared);
        ping.ping();
    }
}

fn scheduled_capacity(shared: &Shared, key: &str) -> ScheduledCapacity {
    if shared.occupied.values().any(|occupied| occupied.key == key)
        || shared.occupied.len() >= MAX_UNREAPED
        || shared.starts.len() >= MAX_STARTS
    {
        return ScheduledCapacity::Wait;
    }
    let Some(bytes) = shared.reserved_bytes.checked_add(RUN_BUFFER_RESERVATION) else {
        return ScheduledCapacity::Buffers;
    };
    if bytes <= MAX_RETAINED_BYTES {
        return ScheduledCapacity::Ready;
    }
    let active_reservations = shared
        .occupied
        .values()
        .map(|occupied| occupied.reserved_bytes)
        .sum::<usize>();
    let retained = shared.reserved_bytes.saturating_sub(active_reservations);
    if retained.saturating_add(RUN_BUFFER_RESERVATION) <= MAX_RETAINED_BYTES {
        ScheduledCapacity::Wait
    } else {
        ScheduledCapacity::Buffers
    }
}

fn advance_schedule_deadline(
    shared: &mut Shared,
    key: &str,
    command: &ScheduledCommand,
    now: Instant,
) {
    let schedule = shared
        .registrations
        .get_mut(key)
        .and_then(|registered| registered.schedule.as_mut())
        .expect("scheduled definitions must have schedule state");
    schedule.deadline = command.interval.map_or(ScheduleDeadline::None, |interval| {
        let ScheduleDeadline::Next(deadline) = schedule.deadline else {
            return ScheduleDeadline::Next(now + interval);
        };
        let steps = now
            .saturating_duration_since(deadline)
            .as_nanos()
            .checked_div(interval.as_nanos())
            .and_then(|steps| steps.checked_add(1));
        let offset = steps
            .and_then(|steps| interval.as_nanos().checked_mul(steps))
            .and_then(|nanos| u64::try_from(nanos).ok())
            .map(Duration::from_nanos);
        offset
            .and_then(|offset| deadline.checked_add(offset))
            .map_or(
                ScheduleDeadline::Next(now + interval),
                ScheduleDeadline::Next,
            )
    });
}

fn next_schedule_deadline(shared: &Mutex<Shared>) -> Option<Instant> {
    lock(shared)
        .registrations
        .values()
        .filter_map(|registered| match registered.schedule.as_ref()?.deadline {
            ScheduleDeadline::Next(deadline) => Some(deadline),
            ScheduleDeadline::Disarmed
            | ScheduleDeadline::DueBlocked(_)
            | ScheduleDeadline::None => None,
        })
        .min()
}

fn apply_starts(
    shared: &Mutex<Shared>,
    runs: &mut BTreeMap<RunToken, Run>,
    ping: &Ping,
    setup: ChildSetup,
    limits: Limits,
) {
    #[cfg(test)]
    pause_before_dequeue(shared);
    let starts = {
        let mut shared = lock(shared);
        shared.starts.drain(..).collect::<Vec<_>>()
    };
    for start in starts {
        if cancelled(shared, start.token) {
            cancel_start(shared, start.token, ping);
            continue;
        }
        #[cfg(test)]
        pause_before_spawn(shared);
        match spawn(start, setup) {
            Ok((mut run, setup_error)) => {
                if cancelled(shared, run.token) {
                    signal_owned(&run, shutdown_for(shared, run.token).raw());
                    run.stop_deadline = Some(Instant::now() + stop_grace(shared, limits));
                    mark_stopping(shared, &run, ping);
                } else {
                    mark_running(shared, &run, ping);
                }
                if let Some(error) = setup_error {
                    set_run_error(shared, &run, &error, ping);
                }
                runs.insert(run.token, run);
            }
            Err((token, error)) => fail_start(shared, token, &error.to_string(), ping),
        }
    }
}

#[cfg(test)]
fn pause_before_dequeue(shared: &Mutex<Shared>) {
    let gate = lock(shared).before_dequeue.clone();
    if let Some(gate) = gate {
        gate.arrive();
    }
}

fn apply_stops(
    shared: &Mutex<Shared>,
    runs: &mut BTreeMap<RunToken, Run>,
    ping: &Ping,
    limits: Limits,
) {
    let (stops, grace) = {
        let shared = lock(shared);
        (
            shared.stops.clone(),
            if shared.shutdown {
                limits.shutdown_grace
            } else {
                limits.stop_grace
            },
        )
    };
    for (token, signal) in stops {
        let Some(run) = runs.get_mut(&token) else {
            continue;
        };
        if run.identity_lost || run.wait_status.is_some() {
            continue;
        }
        let deadline = Instant::now() + grace;
        if run.stop_deadline.is_none() {
            signal_owned(run, signal.raw());
            run.stop_deadline = Some(deadline);
            mark_stopping(shared, run, ping);
        } else if run.stop_deadline.is_some_and(|current| deadline < current) {
            run.stop_deadline = Some(deadline);
        }
    }
}

fn stop_grace(shared: &Mutex<Shared>, limits: Limits) -> Duration {
    if lock(shared).shutdown {
        limits.shutdown_grace
    } else {
        limits.stop_grace
    }
}

fn escalation_due(run: &Run, now: Instant) -> bool {
    run.wait_status.is_none()
        && !run.identity_lost
        && !run.kill_sent
        && run.stop_deadline.is_some_and(|deadline| deadline <= now)
}

fn defer_group_scan(run: &mut Run, now: Instant, ordinary_delay: Duration) {
    let delay = if run.kill_sent {
        let delay = run.group_scan_delay;
        run.group_scan_delay = if delay < STUCK_GROUP_SCAN_INTERVAL {
            STUCK_GROUP_SCAN_INTERVAL
        } else {
            delay.saturating_mul(2).min(MAX_STUCK_GROUP_SCAN_INTERVAL)
        };
        delay
    } else {
        ordinary_delay
    };
    run.next_group_check = now + delay;
}

#[cfg(test)]
fn pause_before_spawn(shared: &Mutex<Shared>) {
    let gate = lock(shared).before_spawn.clone();
    if let Some(gate) = gate {
        gate.arrive();
    }
}

fn spawn(start: Start, setup: ChildSetup) -> Result<(Run, Option<String>), (RunToken, io::Error)> {
    let token = start.token;
    let (program, arguments) = start.argv.split_first().ok_or_else(|| {
        (
            token,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed process has no program",
            ),
        )
    })?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove(XDG_ACTIVATION_TOKEN)
        .env_remove(DESKTOP_STARTUP_ID);
    if let Some(cwd) = &start.cwd {
        command.current_dir(cwd);
    }
    configure_child(&mut command, setup);
    let mut child = command.spawn().map_err(|error| (token, error))?;
    let pid = child.id();
    let mut setup_errors = Vec::new();
    let stdout = prepare_pipe(child.stdout.take(), "stdout", &mut setup_errors);
    let stderr = prepare_pipe(child.stderr.take(), "stderr", &mut setup_errors);
    let setup_error = (!setup_errors.is_empty()).then(|| setup_errors.join("; "));
    Ok((
        Run {
            token,
            key: start.key,
            scheduled: start.scheduled,
            child,
            pid,
            stdout,
            stderr,
            stdout_tail: Utf8Tail::new(MAX_STREAM_BYTES),
            stderr_tail: Utf8Tail::new(MAX_STREAM_BYTES),
            exit_observed: false,
            identity_lost: false,
            next_group_check: Instant::now(),
            group_scan_delay: STUCK_GROUP_SCAN_INTERVAL,
            wait_status: None,
            drain_deadline: None,
            stop_deadline: None,
            kill_sent: false,
            dirty: false,
            next_publish: Instant::now(),
        },
        setup_error,
    ))
}

fn prepare_pipe<T>(output: Option<T>, name: &str, errors: &mut Vec<String>) -> Option<std::fs::File>
where
    T: Into<OwnedFd>,
{
    let Some(output) = output else {
        errors.push(format!("managed process {name} is unavailable"));
        return None;
    };
    match nonblocking(output.into()) {
        Ok(file) => Some(file),
        Err(error) => {
            errors.push(format!("cannot configure managed process {name}: {error}"));
            None
        }
    }
}

fn configure_child(command: &mut Command, setup: ChildSetup) {
    // SAFETY: the closure uses only async-signal-safe libc calls before exec.
    unsafe {
        command.pre_exec(move || {
            if setup.sigchld_blocked {
                unblock_sigchld()?;
            }
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != setup.parent_pid {
                return Err(io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
}

fn unblock_sigchld() -> io::Result<()> {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the provided set.
    if unsafe { libc::sigemptyset(set.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the set was initialized above.
    let mut set = unsafe { set.assume_init() };
    // SAFETY: sigaddset updates an initialized set.
    if unsafe { libc::sigaddset(&mut set, libc::SIGCHLD) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pthread_sigmask reads the initialized set.
    let result = unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, ptr::null_mut()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

fn read_stream(shared: &Mutex<Shared>, run: &mut Run, stream: Stream, ping: &Ping) {
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let mut total = 0;
    loop {
        if total >= MAX_READ_BYTES_PER_TURN {
            return;
        }
        let result = {
            let Some(file) = run.file_mut(stream).as_mut() else {
                return;
            };
            file.read(&mut chunk)
        };
        match result {
            Ok(0) => {
                run.close_stream(stream);
                return;
            }
            Ok(read) => {
                run.tail_mut(stream).push(&chunk[..read]);
                run.dirty = true;
                total += read;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                set_run_error(
                    shared,
                    run,
                    &format!("cannot read managed process output: {error}"),
                    ping,
                );
                run.close_stream(stream);
                return;
            }
        }
    }
}

fn publish_run(shared: &Mutex<Shared>, run: &Run, ping: &Ping) {
    if run.scheduled {
        return;
    }
    let changed = {
        let mut shared = lock(shared);
        let Some(state) = shared.states.get_mut(&run.key) else {
            return;
        };
        if state.token != Some(run.token) {
            return;
        }
        let stdout: Arc<str> = Arc::from(run.stdout_tail.as_str());
        let stderr: Arc<str> = Arc::from(run.stderr_tail.as_str());
        let changed = state.stdout != stdout
            || state.stderr != stderr
            || state.stdout_truncated != run.stdout_tail.truncated()
            || state.stderr_truncated != run.stderr_tail.truncated();
        if changed {
            state.stdout = stdout;
            state.stderr = stderr;
            state.stdout_truncated = run.stdout_tail.truncated();
            state.stderr_truncated = run.stderr_tail.truncated();
            shared.latest = Some(snapshot_locked(&shared));
        }
        changed
    };
    if changed {
        ping.ping();
    }
}

fn finish_run(shared: &Mutex<Shared>, run: &mut Run, ping: &Ping) {
    run.stdout_tail.finish();
    run.stderr_tail.finish();
    let status = run
        .wait_status
        .take()
        .expect("finished run must have a wait status");
    let mut shared = lock(shared);
    let occupied = shared.occupied.remove(&run.token);
    let cancelled = shared.stops.remove(&run.token).is_some();
    let current = shared
        .states
        .get(&run.key)
        .is_some_and(|state| state.token == Some(run.token));
    if current {
        let reserved = occupied
            .as_ref()
            .map_or(RUN_BUFFER_RESERVATION, |occupied| occupied.reserved_bytes);
        shared.reserved_bytes = shared.reserved_bytes.saturating_sub(reserved);
        if run.scheduled && cancelled {
            let state = shared
                .states
                .get_mut(&run.key)
                .expect("current run must retain state");
            state.status = if state.terminal_failure {
                Status::Failed
            } else {
                Status::Exited
            };
            state.token = None;
            state.terminal_failure = false;
        } else {
            let stdout: Arc<str> = Arc::from(run.stdout_tail.as_str());
            let stderr: Arc<str> = Arc::from(run.stderr_tail.as_str());
            let retained = stdout.len().saturating_add(stderr.len());
            if run.scheduled {
                shared.reserved_bytes = shared
                    .reserved_bytes
                    .saturating_sub(shared.states[&run.key].retained_bytes);
            }
            shared.reserved_bytes = shared.reserved_bytes.saturating_add(retained);
            let state = shared
                .states
                .get_mut(&run.key)
                .expect("current run must retain state");
            state.status = if state.terminal_failure {
                Status::Failed
            } else {
                Status::Exited
            };
            state.stdout = stdout;
            state.stderr = stderr;
            state.stdout_truncated = run.stdout_tail.truncated();
            state.stderr_truncated = run.stderr_tail.truncated();
            state.exit_code = status.code();
            state.exit_signal = status.signal();
            state.token = None;
            state.retained_bytes = retained;
            state.terminal_failure = false;
        }
    } else if let Some(occupied) = occupied {
        shared.reserved_bytes = shared
            .reserved_bytes
            .saturating_sub(occupied.reserved_bytes);
    }
    shared.capacity_epoch = shared.capacity_epoch.wrapping_add(1);
    shared.latest = Some(snapshot_locked(&shared));
    drop(shared);
    ping.ping();
}

fn mark_running(shared: &Mutex<Shared>, run: &Run, ping: &Ping) {
    let mut shared = lock(shared);
    let Some(state) = shared.states.get_mut(&run.key) else {
        return;
    };
    if state.token == Some(run.token) {
        state.status = Status::Running;
        shared.latest = Some(snapshot_locked(&shared));
        drop(shared);
        ping.ping();
    }
}

fn mark_stopping(shared: &Mutex<Shared>, run: &Run, ping: &Ping) {
    let mut shared = lock(shared);
    let Some(state) = shared.states.get_mut(&run.key) else {
        return;
    };
    if state.token == Some(run.token) && state.status != Status::Stopping {
        state.status = Status::Stopping;
        shared.latest = Some(snapshot_locked(&shared));
        drop(shared);
        ping.ping();
    }
}

fn set_run_error(shared: &Mutex<Shared>, run: &Run, error: &str, ping: &Ping) {
    let mut shared = lock(shared);
    let Some(state) = shared.states.get_mut(&run.key) else {
        return;
    };
    if state.token == Some(run.token) {
        let error: Arc<str> = Arc::from(bounded(error, MAX_ERROR_BYTES));
        if state.error == error {
            return;
        }
        state.error = error;
        shared.latest = Some(snapshot_locked(&shared));
        drop(shared);
        ping.ping();
    }
}

fn lose_child_identity(shared: &Mutex<Shared>, run: &mut Run, ping: &Ping, drain_grace: Duration) {
    run.identity_lost = true;
    run.drain_deadline = Some(Instant::now() + drain_grace);
    let mut shared = lock(shared);
    let Some(state) = shared.states.get_mut(&run.key) else {
        return;
    };
    if state.token == Some(run.token) {
        state.status = Status::Failed;
        state.error = Arc::from("managed process child identity was lost before reap");
        state.terminal_failure = true;
        shared.latest = Some(snapshot_locked(&shared));
        drop(shared);
        ping.ping();
    }
}

fn fail_start(shared: &Mutex<Shared>, token: RunToken, error: &str, ping: &Ping) {
    let mut shared = lock(shared);
    let occupied = shared.occupied.remove(&token);
    shared.stops.remove(&token);
    if let Some(occupied) = occupied {
        shared.reserved_bytes = shared
            .reserved_bytes
            .saturating_sub(occupied.reserved_bytes);
        if let Some(state) = shared.states.get_mut(&occupied.key)
            && state.token == Some(token)
        {
            state.status = Status::Failed;
            state.error = Arc::from(bounded(error, MAX_ERROR_BYTES));
            state.token = None;
            if !occupied.scheduled {
                state.retained_bytes = 0;
            }
        }
    }
    shared.capacity_epoch = shared.capacity_epoch.wrapping_add(1);
    shared.latest = Some(snapshot_locked(&shared));
    drop(shared);
    ping.ping();
}

fn retire_failed_start(shared: &Mutex<Shared>, token: RunToken, error: &str, ping: &Ping) {
    let mut shared = lock(shared);
    let Some((key, signal)) = shared
        .occupied
        .get(&token)
        .map(|occupied| (occupied.key.clone(), occupied.shutdown_signal))
    else {
        return;
    };
    shared.stops.insert(token, signal);
    if let Some(state) = shared.states.get_mut(&key)
        && state.token == Some(token)
    {
        state.status = Status::Stopping;
        state.error = Arc::from(bounded(error, MAX_ERROR_BYTES));
        state.terminal_failure = true;
    }
    shared.latest = Some(snapshot_locked(&shared));
    drop(shared);
    ping.ping();
}

fn cancel_start(shared: &Mutex<Shared>, token: RunToken, ping: &Ping) {
    let mut shared = lock(shared);
    let occupied = shared.occupied.remove(&token);
    shared.stops.remove(&token);
    if let Some(occupied) = occupied {
        shared.reserved_bytes = shared
            .reserved_bytes
            .saturating_sub(occupied.reserved_bytes);
        if let Some(state) = shared.states.get_mut(&occupied.key)
            && state.token == Some(token)
        {
            state.status = if state.terminal_failure {
                Status::Failed
            } else {
                Status::Exited
            };
            state.token = None;
            if !occupied.scheduled {
                state.retained_bytes = 0;
            }
            state.terminal_failure = false;
        }
    }
    shared.capacity_epoch = shared.capacity_epoch.wrapping_add(1);
    shared.latest = Some(snapshot_locked(&shared));
    drop(shared);
    ping.ping();
}

fn mark_worker_failure(
    shared: &Mutex<Shared>,
    runs: &mut BTreeMap<RunToken, Run>,
    ping: &Ping,
    error: &str,
) {
    {
        let mut shared = lock(shared);
        shared.shutdown = true;
        let tokens = shared.occupied.keys().copied().collect::<Vec<_>>();
        for token in tokens {
            let signal = shared
                .occupied
                .get(&token)
                .map(|occupied| occupied.shutdown_signal)
                .unwrap_or(ShutdownSignal::Terminate);
            shared.stops.insert(token, signal);
        }
    }
    for run in runs.values_mut() {
        if !run.identity_lost && run.wait_status.is_none() {
            signal_owned(run, libc::SIGKILL);
            run.kill_sent = true;
        }
        if run.stdout.is_some() {
            run.close_stream(Stream::Stdout);
        }
        if run.stderr.is_some() {
            run.close_stream(Stream::Stderr);
        }
        set_run_error(shared, run, error, ping);
    }
}

fn cancelled(shared: &Mutex<Shared>, token: RunToken) -> bool {
    let shared = lock(shared);
    shared.shutdown || shared.stops.contains_key(&token) || !shared.occupied.contains_key(&token)
}

fn shutdown_for(shared: &Mutex<Shared>, token: RunToken) -> ShutdownSignal {
    let shared = lock(shared);
    shared
        .stops
        .get(&token)
        .copied()
        .or_else(|| {
            shared
                .occupied
                .get(&token)
                .map(|occupied| occupied.shutdown_signal)
        })
        .unwrap_or(ShutdownSignal::Terminate)
}

fn should_finish(shared: &Mutex<Shared>, runs: &BTreeMap<RunToken, Run>) -> bool {
    let shared = lock(shared);
    shared.shutdown && runs.is_empty() && shared.starts.is_empty() && shared.occupied.is_empty()
}

fn retire_key(shared: &mut Shared, key: &str) {
    if let Some(state) = shared.states.get(key) {
        shared.reserved_bytes = shared.reserved_bytes.saturating_sub(state.retained_bytes);
        if let Some(token) = state.token {
            let signal = shared
                .occupied
                .get(&token)
                .map(|occupied| occupied.shutdown_signal)
                .unwrap_or(ShutdownSignal::Terminate);
            shared.stops.insert(token, signal);
        }
    }
}

fn snapshot_locked(shared: &Shared) -> Snapshot {
    Snapshot {
        states: shared.states.values().cloned().collect::<Vec<_>>().into(),
    }
}

fn publish_locked(shared: &mut Shared) -> Snapshot {
    let snapshot = snapshot_locked(shared);
    shared.latest = Some(snapshot.clone());
    snapshot
}

fn validate_definitions(definitions: Vec<Definition>) -> io::Result<Vec<Definition>> {
    if definitions.len() > MAX_KEYS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many managed process keys",
        ));
    }
    let mut keys = BTreeSet::new();
    for definition in &definitions {
        validate_key(&definition.key)?;
        if let Some(command) = &definition.command {
            validate_command(&command.argv, command.cwd.as_deref())?;
            if command.interval.is_some_and(|interval| {
                !(MIN_SCHEDULE_INTERVAL..=MAX_SCHEDULE_INTERVAL).contains(&interval)
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid managed process interval",
                ));
            }
        }
        if !keys.insert(definition.key.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate managed process key {:?}", definition.key),
            ));
        }
    }
    Ok(definitions)
}

fn validate_key(key: &str) -> io::Result<()> {
    if key.is_empty() || key.len() > 128 || key.contains('\0') {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed process key must be 1 to 128 bytes without NUL",
        ))
    } else {
        Ok(())
    }
}

fn validate_command(argv: &[String], cwd: Option<&str>) -> io::Result<()> {
    let Some(program) = argv.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed process has no program",
        ));
    };
    if program.is_empty()
        || argv.len() > MAX_ARGUMENTS
        || argv
            .iter()
            .any(|argument| argument.len() > MAX_ARGUMENT_BYTES || argument.contains('\0'))
        || argv.iter().map(String::len).sum::<usize>() > MAX_ARGUMENT_TOTAL_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid managed process arguments",
        ));
    }
    if cwd.is_some_and(|cwd| cwd.len() > MAX_WORKING_DIRECTORY_BYTES || cwd.contains('\0')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid managed process working directory",
        ));
    }
    Ok(())
}

fn allocate(next: &mut u64) -> io::Result<u64> {
    let value = advance(*next)?;
    *next = value;
    Ok(value)
}

fn allocate_many(next: &mut u64, count: usize) -> io::Result<Vec<u64>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let count = u64::try_from(count)
        .map_err(|_| io::Error::other("managed process identity space exhausted"))?;
    let end = next
        .checked_add(count)
        .ok_or_else(|| io::Error::other("managed process identity space exhausted"))?;
    let values = ((*next + 1)..=end).collect();
    *next = end;
    Ok(values)
}

fn advance(value: u64) -> io::Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| io::Error::other("managed process identity space exhausted"))
}

fn ensure_active(shared: &Shared) -> io::Result<()> {
    if shared.shutdown {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "managed process worker is shutting down",
        ))
    } else {
        Ok(())
    }
}

fn bounded(value: &str, limit: usize) -> String {
    if value.len() <= limit && !value.contains('\0') {
        return value.to_owned();
    }
    let mut result = value.replace('\0', "�");
    if result.len() > limit {
        let mut end = limit;
        while !result.is_char_boundary(end) {
            end -= 1;
        }
        result.truncate(end);
    }
    result
}

fn signal_group(pid: u32, signal: i32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: a negative pid addresses only the child's process group.
    let result = unsafe { libc::kill(-pid, signal) };
    if result == -1 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        // Signal failures are reflected by the later wait/stop deadline.
    }
}

fn signal_process(pid: u32, signal: i32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: the unreaped Child keeps this positive process identity stable.
    let result = unsafe { libc::kill(pid, signal) };
    if result == -1 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        // Signal failures are reflected by the later wait/stop deadline.
    }
}

fn signal_owned(run: &Run, signal: i32) {
    signal_process(run.pid, signal);
    signal_group(run.pid, signal);
}

fn exit_observed(pid: u32) -> io::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: waitid initializes siginfo on success and WNOWAIT leaves the
    // leader unreaped so its PID and initial process-group number stay pinned.
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
    // SAFETY: waitid initialized the value after returning success.
    Ok(unsafe { info.assume_init().si_pid() } != 0)
}

fn ensure_authoritative_procfs() -> io::Result<()> {
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the path is NUL-terminated and statfs initializes stats on
    // success.
    if unsafe { libc::statfs(c"/proc".as_ptr(), stats.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs initialized stats after returning success.
    let stats = unsafe { stats.assume_init() };
    if stats.f_type != libc::PROC_SUPER_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed processes require procfs mounted at /proc",
        ));
    }
    let mut path_stats = std::mem::MaybeUninit::<libc::statx>::uninit();
    // SAFETY: the path is NUL-terminated and statx initializes path_stats on
    // success.
    if unsafe {
        libc::statx(
            libc::AT_FDCWD,
            c"/proc".as_ptr(),
            libc::AT_STATX_SYNC_AS_STAT,
            libc::STATX_MNT_ID,
            path_stats.as_mut_ptr(),
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statx initialized path_stats after returning success.
    let path_stats = unsafe { path_stats.assume_init() };
    if path_stats.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed processes require procfs mount identity",
        ));
    }
    let mut mountinfo = String::new();
    fs::File::open("/proc/self/mountinfo")?
        .take(MAX_PROC_MOUNTINFO_BYTES + 1)
        .read_to_string(&mut mountinfo)?;
    if mountinfo.len() as u64 > MAX_PROC_MOUNTINFO_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "procfs mount information is too large",
        ));
    }
    if !procfs_mount_is_authoritative(&mountinfo, path_stats.stx_mnt_id)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed process groups require unrestricted procfs visibility",
        ));
    }
    Ok(())
}

fn procfs_mount_is_authoritative(mountinfo: &str, mount_id: u64) -> io::Result<bool> {
    for line in mountinfo.lines() {
        let Some((mount, filesystem)) = line.split_once(" - ") else {
            continue;
        };
        let fields = mount.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.get(4) != Some(&"/proc") {
            continue;
        }
        if fields.len() < 6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid /proc mount information",
            ));
        }
        let id = fields[0]
            .parse::<u64>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc mount id"))?;
        if id != mount_id {
            continue;
        }
        let filesystem = filesystem.split_ascii_whitespace().collect::<Vec<_>>();
        if filesystem.len() < 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid /proc filesystem information",
            ));
        }
        return Ok(fields[3] == "/"
            && filesystem[0] == "proc"
            && !hidepid_restricts(fields[5])
            && !hidepid_restricts(filesystem[2]));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "cannot identify the active /proc mount",
    ))
}

fn hidepid_restricts(options: &str) -> bool {
    options.split(',').any(|option| {
        option == "hidepid"
            || option
                .strip_prefix("hidepid=")
                .is_some_and(|value| value != "0" && value != "off")
    })
}

fn initial_group_has_other_members(pgid: u32) -> io::Result<bool> {
    ensure_authoritative_procfs()?;
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == pgid {
            continue;
        }
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if process_group_from_stat(&stat)? == pgid {
            return Ok(true);
        }
    }
    ensure_authoritative_procfs()?;
    Ok(false)
}

fn process_group_from_stat(stat: &str) -> io::Result<u32> {
    let fields = stat
        .rfind(") ")
        .and_then(|end| stat.get(end + 2..))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc stat"))?;
    fields
        .split_ascii_whitespace()
        .nth(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing /proc process group"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc process group"))
}

fn nonblocking(fd: OwnedFd) -> io::Result<std::fs::File> {
    let raw = fd.as_raw_fd();
    // SAFETY: fcntl only inspects or updates flags for this owned descriptor.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor remains owned by `fd`.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(std::fs::File::from(fd))
}

fn poll_timeout(runs: &BTreeMap<RunToken, Run>, schedule: Option<Instant>, now: Instant) -> i32 {
    let deadline = runs
        .values()
        .flat_map(|run| {
            [
                (run.wait_status.is_none() && !run.kill_sent && !run.identity_lost)
                    .then_some(run.stop_deadline)
                    .flatten(),
                (run.wait_status.is_none() && run.exit_observed && !run.identity_lost)
                    .then_some(run.next_group_check),
                (!run.drained()).then_some(run.drain_deadline).flatten(),
                run.dirty.then_some(run.next_publish),
            ]
        })
        .flatten()
        .chain(schedule)
        .min()
        .unwrap_or(now + POLL_INTERVAL)
        .min(now + POLL_INTERVAL);
    let millis = deadline
        .saturating_duration_since(now)
        .as_millis()
        .saturating_add(1);
    i32::try_from(millis).unwrap_or(i32::MAX)
}

fn poll(descriptors: &mut [libc::pollfd], timeout: i32) -> io::Result<()> {
    // SAFETY: descriptors is initialized and remains alive for the call.
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

fn wake(stream: &UnixStream) -> io::Result<()> {
    loop {
        match (&*stream).write(&[1]) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn drain_wake(stream: &UnixStream) {
    let mut bytes = [0_u8; 256];
    loop {
        match (&*stream).read(&mut bytes) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

struct Utf8Tail {
    text: String,
    pending: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl Utf8Tail {
    fn new(limit: usize) -> Self {
        Self {
            text: String::with_capacity(limit),
            pending: Vec::with_capacity(4),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let mut input = Vec::with_capacity(self.pending.len() + bytes.len());
        input.extend_from_slice(&self.pending);
        input.extend_from_slice(bytes);
        self.pending.clear();
        let mut offset = 0;
        while offset < input.len() {
            match std::str::from_utf8(&input[offset..]) {
                Ok(valid) => {
                    self.push_valid(valid);
                    offset = input.len();
                }
                Err(error) => {
                    let valid_end = offset + error.valid_up_to();
                    if valid_end > offset {
                        // SAFETY: `valid_up_to` identifies a valid UTF-8 prefix.
                        let valid =
                            unsafe { std::str::from_utf8_unchecked(&input[offset..valid_end]) };
                        self.push_valid(valid);
                    }
                    match error.error_len() {
                        Some(length) => {
                            self.push_valid("�");
                            offset = valid_end + length;
                        }
                        None => {
                            self.pending.extend_from_slice(&input[valid_end..]);
                            offset = input.len();
                        }
                    }
                }
            }
        }
    }

    fn finish(&mut self) {
        if !self.pending.is_empty() {
            self.pending.clear();
            self.push_valid("�");
        }
    }

    fn push_valid(&mut self, value: &str) {
        for character in value.chars() {
            self.text
                .push(if character == '\0' { '�' } else { character });
        }
        self.trim();
    }

    fn trim(&mut self) {
        if self.text.len() <= self.limit {
            return;
        }
        let target = self.text.len() - self.limit;
        let mut start = target;
        while !self.text.is_char_boundary(start) {
            start += 1;
        }
        self.text.drain(..start);
        self.truncated = true;
    }

    fn as_str(&self) -> &str {
        &self.text
    }

    fn truncated(&self) -> bool {
        self.truncated
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, thread};

    fn definition(key: &str) -> Definition {
        Definition {
            key: key.into(),
            shutdown_signal: ShutdownSignal::Terminate,
            command: None,
        }
    }

    fn scheduled_definition(
        key: &str,
        mode: &str,
        cwd: Option<String>,
        interval: Option<Duration>,
    ) -> Definition {
        Definition {
            key: key.into(),
            shutdown_signal: ShutdownSignal::Terminate,
            command: Some(ScheduledCommand {
                argv: helper(mode),
                cwd,
                interval,
            }),
        }
    }

    fn manager() -> Manager {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        Manager::start_with_limits(ping, test_limits()).unwrap()
    }

    fn test_limits() -> Limits {
        Limits {
            stop_grace: Duration::from_millis(100),
            shutdown_grace: Duration::from_millis(100),
            drain_grace: Duration::from_millis(50),
            drop_wait: Duration::from_secs(1),
        }
    }

    fn helper(mode: &str) -> Vec<String> {
        let test = match mode {
            "output" => "managed_process::tests::fake_child_output",
            "ignore-term" => "managed_process::tests::fake_child_ignore_term",
            "flood" => "managed_process::tests::fake_child_flood",
            "group" => "managed_process::tests::fake_child_group",
            "escape-group" => "managed_process::tests::fake_child_escape_group",
            "report-ignore-term" => "managed_process::tests::fake_child_report_ignore_term",
            "staged-output" => "managed_process::tests::fake_child_staged_output",
            "counted-output" => "managed_process::tests::fake_child_counted_output",
            _ => panic!("unknown fake child mode"),
        };
        vec![
            env::current_exe().unwrap().to_string_lossy().into_owned(),
            "--ignored".into(),
            "--exact".into(),
            test.into(),
            "--nocapture".into(),
            "--quiet".into(),
            "--test-threads=1".into(),
        ]
    }

    fn wait_for(manager: &Manager, mut predicate: impl FnMut(&State) -> bool) -> State {
        wait_for_key(manager, None, |state| predicate(state))
    }

    fn wait_for_key(
        manager: &Manager,
        key: Option<&str>,
        mut predicate: impl FnMut(&State) -> bool,
    ) -> State {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = manager.snapshot();
            let state = key.map_or_else(
                || snapshot.states[0].clone(),
                |key| {
                    snapshot
                        .states
                        .iter()
                        .find(|state| state.key == key)
                        .unwrap_or_else(|| panic!("missing managed process {key:?}"))
                        .clone()
                },
            );
            if predicate(&state) {
                return state;
            }
            assert!(Instant::now() < deadline, "timed out with {state:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_run_count(directory: &std::path::Path) -> u32 {
        std::fs::read_to_string(directory.join("runs"))
            .unwrap()
            .parse()
            .unwrap()
    }

    fn fixture(test: &str) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command.args([
            "--ignored",
            "--exact",
            test,
            "--nocapture",
            "--quiet",
            "--test-threads=1",
        ]);
        command
    }

    fn marker_pid(output: &str, marker: &str) -> u32 {
        output
            .lines()
            .find_map(|line| line.strip_prefix(marker))
            .unwrap_or_else(|| panic!("missing {marker:?} in {output:?}"))
            .parse()
            .unwrap()
    }

    fn process_exists(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: signal zero only checks whether the positive pid exists.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[test]
    fn utf8_tail_preserves_split_sequences_and_replaces_only_invalid_input() {
        let mut tail = Utf8Tail::new(64);
        tail.push(&[b'a', 0xe2]);
        assert_eq!(tail.as_str(), "a");
        tail.push(&[0x82]);
        assert_eq!(tail.as_str(), "a");
        tail.push(&[0xac, b'b', 0xff, b'c', 0]);
        assert_eq!(tail.as_str(), "a€b�c�");
        tail.finish();
        assert_eq!(tail.as_str(), "a€b�c�");
    }

    #[test]
    fn utf8_tail_evicts_only_whole_codepoints() {
        let mut tail = Utf8Tail::new(7);
        tail.push("a€b€c".as_bytes());
        assert_eq!(tail.as_str(), "b€c");
        assert!(tail.truncated());
        assert!(tail.as_str().is_char_boundary(0));
    }

    #[test]
    fn proc_stat_parser_handles_spaces_and_parentheses_in_the_name() {
        assert_eq!(
            process_group_from_stat("42 (a tricky) name) S 7 19 3").unwrap(),
            19
        );
        assert!(process_group_from_stat("42 broken").is_err());
    }

    #[test]
    fn procfs_mount_parser_rejects_hidden_or_partial_process_views() {
        let mountinfo = concat!(
            "10 1 0:1 / /proc rw - proc proc rw,hidepid=0\n",
            "20 1 0:2 / /proc rw - proc proc rw,hidepid=2\n",
            "30 1 0:3 /123 /proc rw - proc proc rw\n",
        );
        assert!(procfs_mount_is_authoritative(mountinfo, 10).unwrap());
        assert!(!procfs_mount_is_authoritative(mountinfo, 20).unwrap());
        assert!(!procfs_mount_is_authoritative(mountinfo, 30).unwrap());
        assert!(procfs_mount_is_authoritative(mountinfo, 40).is_err());
        assert!(hidepid_restricts("rw,hidepid=invisible"));
        assert!(!hidepid_restricts("rw,hidepid=off"));
    }

    #[test]
    fn expired_stop_does_not_resignal_a_reaped_identity_and_scans_back_off() {
        let token = RunToken {
            generation: 1,
            key_incarnation: 1,
            run_incarnation: 1,
        };
        let (mut run, _) = spawn(
            Start {
                token,
                key: "child".into(),
                argv: helper("output"),
                cwd: None,
                scheduled: false,
            },
            ChildSetup::capture().unwrap(),
        )
        .unwrap();
        run.wait_status = Some(run.child.wait().unwrap());
        run.stop_deadline = Some(Instant::now() - Duration::from_secs(1));
        assert!(!escalation_due(&run, Instant::now()));
        run.stop_deadline = None;

        let mut runs = BTreeMap::new();
        runs.insert(token, run);
        assert!(poll_timeout(&runs, None, Instant::now()) > 1);

        let mut shared = Shared::default();
        shared.stops.insert(token, ShutdownSignal::Terminate);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        apply_stops(&Mutex::new(shared), &mut runs, &ping, test_limits());
        assert!(runs[&token].stop_deadline.is_none());

        let run = runs.get_mut(&token).unwrap();
        run.kill_sent = true;
        run.group_scan_delay = POST_KILL_GROUP_SCAN_INTERVAL;
        let now = Instant::now();
        defer_group_scan(run, now, GROUP_SCAN_INTERVAL);
        assert_eq!(run.next_group_check, now + POST_KILL_GROUP_SCAN_INTERVAL);
        assert_eq!(run.group_scan_delay, STUCK_GROUP_SCAN_INTERVAL);
        let next = run.next_group_check;
        defer_group_scan(run, next, GROUP_SCAN_INTERVAL);
        assert_eq!(run.group_scan_delay, Duration::from_secs(2));
    }

    #[test]
    fn default_manager_publishes_and_reconciles_one_shared_scope() {
        let manager = manager();
        manager.replace_scope(vec![definition("one")]).unwrap();
        assert_eq!(manager.take_snapshot().unwrap().states.len(), 1);
        let snapshot = manager
            .reconcile_scope(vec![definition("one"), definition("two")])
            .unwrap();
        assert_eq!(snapshot.states.len(), 2);
        assert!(manager.take_snapshot().is_some());
    }

    #[test]
    fn scheduled_scope_stays_disarmed_until_publication_completes() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager();
        let definition = scheduled_definition(
            "query",
            "counted-output",
            Some(directory.path().to_string_lossy().into_owned()),
            None,
        );
        let prepared = manager.prepare_replace_scope(vec![definition]).unwrap();
        assert_eq!(
            prepared.runtime().states[0].status,
            ManagedProcessStatus::Idle
        );
        let activation = manager.commit_scope(prepared).unwrap();

        thread::sleep(Duration::from_millis(100));
        assert!(!directory.path().join("runs").exists());
        assert_eq!(manager.snapshot().states[0].status, Status::Idle);

        manager.arm_scope(activation);
        let state = wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(read_run_count(directory.path()), 1);
        assert!(state.stdout.contains("complete=1"));
        thread::sleep(Duration::from_millis(350));
        assert_eq!(read_run_count(directory.path()), 1);
    }

    #[test]
    fn scheduled_refresh_keeps_the_completed_result_and_cancel_discards_capture() {
        let manager = manager();
        manager
            .replace_scope(vec![scheduled_definition(
                "query",
                "staged-output",
                None,
                None,
            )])
            .unwrap();
        let completed = wait_for(&manager, |state| state.status == Status::Exited);
        assert!(completed.stdout.contains("first"));
        assert!(completed.stdout.contains("second"));

        let gate = Arc::new(SpawnGate::default());
        lock(&manager.shared).before_spawn = Some(gate.clone());
        manager.refresh_process("query").unwrap();
        gate.wait_until_reached();
        let starting = manager.snapshot().states[0].clone();
        assert_eq!(starting.status, Status::Starting);
        assert_eq!(starting.stdout, completed.stdout);
        assert_eq!(starting.exit_code, completed.exit_code);

        manager.refresh_process("query").unwrap();
        gate.release();
        wait_for(&manager, |state| state.status == Status::Running);
        thread::sleep(Duration::from_millis(80));
        assert_eq!(manager.snapshot().states[0].stdout, completed.stdout);

        manager.cancel_process("query").unwrap();
        let cancelled = wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(cancelled.stdout, completed.stdout);
        assert_eq!(cancelled.stderr, completed.stderr);
        assert_eq!(cancelled.exit_code, completed.exit_code);
        assert_eq!(cancelled.exit_signal, completed.exit_signal);
    }

    #[test]
    fn refresh_during_a_running_job_coalesces_to_one_follow_up() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager();
        manager
            .replace_scope(vec![scheduled_definition(
                "query",
                "counted-output",
                Some(directory.path().to_string_lossy().into_owned()),
                None,
            )])
            .unwrap();
        wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(read_run_count(directory.path()), 1);

        manager.refresh_process("query").unwrap();
        wait_for(&manager, |state| {
            state.status == Status::Running && read_run_count(directory.path()) == 2
        });
        manager.refresh_process("query").unwrap();
        manager.refresh_process("query").unwrap();

        wait_for(&manager, |state| {
            state.status == Status::Exited && read_run_count(directory.path()) == 3
        });
        thread::sleep(Duration::from_millis(350));
        assert_eq!(read_run_count(directory.path()), 3);
    }

    #[test]
    fn recurring_jobs_use_one_monotonic_deadline_without_catch_up() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager();
        manager
            .replace_scope(vec![scheduled_definition(
                "query",
                "counted-output",
                Some(directory.path().to_string_lossy().into_owned()),
                Some(Duration::from_secs(1)),
            )])
            .unwrap();
        wait_for(&manager, |state| state.status == Status::Exited);
        let first_completed = Instant::now();
        wait_for(&manager, |_| read_run_count(directory.path()) == 2);
        assert!(first_completed.elapsed() >= Duration::from_millis(500));

        manager.cancel_process("query").unwrap();
        wait_for(&manager, |state| state.status == Status::Exited);
        thread::sleep(Duration::from_millis(400));
        assert_eq!(read_run_count(directory.path()), 2);
    }

    #[test]
    fn exact_scheduled_reloads_preserve_the_live_incarnation_and_result() {
        let definition = scheduled_definition("query", "staged-output", None, None);
        let manager = manager();
        manager.replace_scope(vec![definition.clone()]).unwrap();
        let running = wait_for(&manager, |state| state.status == Status::Running);
        let (token, incarnation) = {
            let shared = lock(&manager.shared);
            (
                shared.states["query"].token,
                shared.registrations["query"].incarnation,
            )
        };

        let prepared = manager
            .prepare_replace_scope(vec![definition.clone()])
            .unwrap();
        assert_eq!(
            prepared.runtime().states[0].status,
            ManagedProcessStatus::Running
        );
        let activation = manager.commit_scope(prepared).unwrap();
        manager.arm_scope(activation);
        let shared = lock(&manager.shared);
        assert_eq!(shared.states["query"].token, token);
        assert_eq!(shared.states["query"].stdout, running.stdout);
        assert_eq!(shared.registrations["query"].incarnation, incarnation);
        drop(shared);

        let completed = wait_for(&manager, |state| state.status == Status::Exited);
        let prepared = manager.prepare_replace_scope(vec![definition]).unwrap();
        let projected = &prepared.runtime().states[0];
        assert_eq!(projected.status, ManagedProcessStatus::Exited);
        assert_eq!(projected.stdout, completed.stdout);
    }

    #[test]
    fn a_stale_prepared_scope_cannot_replace_the_committed_scope() {
        let manager = manager();
        let stale = manager
            .prepare_replace_scope(vec![scheduled_definition("stale", "output", None, None)])
            .unwrap();
        let current = manager
            .prepare_replace_scope(vec![scheduled_definition("current", "output", None, None)])
            .unwrap();
        let _activation = manager.commit_scope(current).unwrap();

        let error = match manager.commit_scope(stale) {
            Ok(_) => panic!("stale managed process scope was committed"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let shared = lock(&manager.shared);
        assert!(shared.registrations.contains_key("current"));
        assert!(!shared.registrations.contains_key("stale"));
        assert!(matches!(
            shared.registrations["current"]
                .schedule
                .as_ref()
                .unwrap()
                .deadline,
            ScheduleDeadline::Disarmed
        ));
    }

    #[test]
    fn a_preserved_state_change_invalidates_a_prepared_scope() {
        let manager = manager();
        manager.replace_scope(vec![definition("current")]).unwrap();
        let prepared = manager
            .prepare_reconcile_scope(vec![definition("current")])
            .unwrap();
        let gate = Arc::new(SpawnGate::default());
        lock(&manager.shared).before_dequeue = Some(gate.clone());
        gate.wait_until_reached();

        manager
            .start_process("current", helper("output"), None)
            .unwrap();
        let result = manager.commit_scope(prepared);
        let (has_current, status) = {
            let shared = lock(&manager.shared);
            (
                shared.registrations.contains_key("current"),
                shared.states["current"].status,
            )
        };
        gate.release();

        let error = match result {
            Ok(_) => panic!("scope preflight survived a process state change"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(has_current);
        assert_eq!(status, Status::Starting);
    }

    #[test]
    fn a_non_preserved_manual_state_change_does_not_block_replacement() {
        let manager = manager();
        manager.replace_scope(vec![definition("current")]).unwrap();
        let prepared = manager
            .prepare_replace_scope(vec![definition("replacement")])
            .unwrap();
        let gate = Arc::new(SpawnGate::default());
        lock(&manager.shared).before_dequeue = Some(gate.clone());
        gate.wait_until_reached();

        manager
            .start_process("current", helper("output"), None)
            .unwrap();
        let result = manager.commit_scope(prepared);
        let (has_current, has_replacement) = {
            let shared = lock(&manager.shared);
            (
                shared.registrations.contains_key("current"),
                shared.registrations.contains_key("replacement"),
            )
        };
        gate.release();

        let activation = result.expect("unrelated manual state rejected a replacement scope");
        manager.arm_scope(activation);
        assert!(!has_current);
        assert!(has_replacement);
    }

    #[test]
    fn due_blocked_waits_for_a_capacity_epoch_change() {
        let definition = scheduled_definition("query", "output", None, None);
        let token = RunToken {
            generation: 0,
            key_incarnation: 0,
            run_incarnation: 0,
        };
        let mut state = Shared::default();
        state
            .states
            .insert("query".into(), State::idle("query".into()));
        state.registrations.insert(
            "query".into(),
            Registered {
                definition,
                incarnation: 1,
                schedule: Some(Schedule {
                    deadline: ScheduleDeadline::Next(Instant::now()),
                    pending_refresh: false,
                }),
            },
        );
        state.occupied.insert(
            token,
            Occupied {
                key: "query".into(),
                shutdown_signal: ShutdownSignal::Terminate,
                reserved_bytes: RUN_BUFFER_RESERVATION,
                scheduled: false,
            },
        );
        state.reserved_bytes = RUN_BUFFER_RESERVATION;
        let state = Mutex::new(state);
        let (ping, _source) = calloop::ping::make_ping().unwrap();

        apply_schedules(&state, &ping, Instant::now());
        {
            let state = lock(&state);
            assert!(state.starts.is_empty());
            assert!(matches!(
                state.registrations["query"]
                    .schedule
                    .as_ref()
                    .unwrap()
                    .deadline,
                ScheduleDeadline::DueBlocked(0)
            ));
        }
        apply_schedules(&state, &ping, Instant::now() + Duration::from_secs(10));
        assert!(lock(&state).starts.is_empty());

        {
            let mut state = lock(&state);
            state.occupied.remove(&token);
            state.reserved_bytes = 0;
            state.capacity_epoch += 1;
        }
        apply_schedules(&state, &ping, Instant::now());
        let state = lock(&state);
        assert_eq!(state.starts.len(), 1);
        assert_eq!(state.states["query"].status, Status::Starting);
    }

    #[test]
    fn missed_active_deadlines_advance_to_one_future_slot() {
        let start = Instant::now();
        let command = ScheduledCommand {
            argv: helper("output"),
            cwd: None,
            interval: Some(Duration::from_secs(1)),
        };
        let mut shared = Shared::default();
        shared.registrations.insert(
            "query".into(),
            Registered {
                definition: Definition {
                    key: "query".into(),
                    shutdown_signal: ShutdownSignal::Terminate,
                    command: Some(command.clone()),
                },
                incarnation: 1,
                schedule: Some(Schedule {
                    deadline: ScheduleDeadline::Next(start),
                    pending_refresh: false,
                }),
            },
        );

        advance_schedule_deadline(
            &mut shared,
            "query",
            &command,
            start + Duration::from_millis(3400),
        );
        let deadline = match shared.registrations["query"]
            .schedule
            .as_ref()
            .unwrap()
            .deadline
        {
            ScheduleDeadline::Next(deadline) => deadline,
            _ => panic!("active recurring job lost its next deadline"),
        };
        assert_eq!(deadline, start + Duration::from_secs(4));
    }

    #[test]
    fn scheduled_results_leave_room_for_sixteen_active_captures() {
        let mut shared = Shared {
            reserved_bytes: MAX_KEYS * MAX_STREAM_BYTES * 2,
            ..Shared::default()
        };
        for _ in 0..16 {
            assert!(matches!(
                scheduled_capacity(&shared, "query"),
                ScheduledCapacity::Ready
            ));
            shared.reserved_bytes += RUN_BUFFER_RESERVATION;
        }
        assert_eq!(shared.reserved_bytes, MAX_RETAINED_BYTES);
        assert!(matches!(
            scheduled_capacity(&shared, "query"),
            ScheduledCapacity::Buffers
        ));

        for run_incarnation in 0..16 {
            shared.occupied.insert(
                RunToken {
                    generation: 1,
                    key_incarnation: run_incarnation,
                    run_incarnation,
                },
                Occupied {
                    key: format!("active-{run_incarnation}"),
                    shutdown_signal: ShutdownSignal::Terminate,
                    reserved_bytes: RUN_BUFFER_RESERVATION,
                    scheduled: true,
                },
            );
        }
        assert!(matches!(
            scheduled_capacity(&shared, "query"),
            ScheduledCapacity::Wait
        ));
    }

    #[test]
    fn one_shot_spawn_failure_waits_for_an_explicit_refresh() {
        let mut definition = scheduled_definition("query", "output", None, None);
        definition.command.as_mut().unwrap().argv =
            vec!["/thing/scheduled-process-fixture-does-not-exist".into()];
        let manager = manager();
        manager.replace_scope(vec![definition]).unwrap();
        wait_for(&manager, |state| state.status == Status::Failed);
        let attempts = lock(&manager.shared).next_run_incarnation;
        thread::sleep(Duration::from_millis(200));
        assert_eq!(lock(&manager.shared).next_run_incarnation, attempts);

        manager.refresh_process("query").unwrap();
        wait_for(&manager, |state| {
            state.status == Status::Failed && lock(&manager.shared).next_run_incarnation > attempts
        });
    }

    #[test]
    fn changing_a_scheduled_command_retires_before_the_replacement_runs() {
        let manager = manager();
        manager
            .replace_scope(vec![scheduled_definition(
                "query",
                "staged-output",
                None,
                None,
            )])
            .unwrap();
        wait_for(&manager, |state| state.status == Status::Running);
        let previous_incarnation = lock(&manager.shared).registrations["query"].incarnation;

        let replacement = scheduled_definition("query", "output", None, None);
        let prepared = manager.prepare_replace_scope(vec![replacement]).unwrap();
        assert_eq!(
            prepared.runtime().states[0].status,
            ManagedProcessStatus::Idle
        );
        let activation = manager.commit_scope(prepared).unwrap();
        assert_ne!(
            lock(&manager.shared).registrations["query"].incarnation,
            previous_incarnation
        );
        manager.arm_scope(activation);

        let completed = wait_for(&manager, |state| {
            state.status == Status::Exited && state.stdout.contains("hello from child")
        });
        assert_eq!(completed.exit_code, Some(0));
    }

    #[test]
    fn reconciling_a_matching_scheduled_key_does_not_rerun_startup() {
        let directory = tempfile::tempdir().unwrap();
        let definition = scheduled_definition(
            "query",
            "counted-output",
            Some(directory.path().to_string_lossy().into_owned()),
            None,
        );
        let manager = manager();
        manager.replace_scope(vec![definition.clone()]).unwrap();
        wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(read_run_count(directory.path()), 1);

        manager.reconcile_scope(vec![definition]).unwrap();
        thread::sleep(Duration::from_millis(350));
        assert_eq!(read_run_count(directory.path()), 1);
        assert_eq!(manager.snapshot().states[0].status, Status::Exited);

        manager.reconcile_scope(Vec::new()).unwrap();
        assert!(manager.snapshot().states.is_empty());
    }

    #[test]
    fn identity_exhaustion_does_not_partially_reconcile_the_scope() {
        let manager = manager();
        manager.replace_scope(vec![definition("old")]).unwrap();
        lock(&manager.shared).next_key_incarnation = u64::MAX;
        let error = manager
            .reconcile_scope(vec![definition("new")])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        let shared = lock(&manager.shared);
        assert!(shared.registrations.contains_key("old"));
        assert!(!shared.registrations.contains_key("new"));
    }

    #[test]
    fn process_state_is_shared_and_captures_output_and_exit() {
        let manager = manager();
        manager.replace_scope(vec![definition("query")]).unwrap();
        manager
            .start_process("query", helper("output"), None)
            .unwrap();
        let state = wait_for(&manager, |state| state.status == Status::Exited);
        assert!(state.stdout.contains("hello from child"));
        assert!(state.stderr.contains("child warning"));
        assert_eq!(state.exit_code, Some(0));
        assert_eq!(state.exit_signal, None);
        assert!(!state.running());
    }

    #[test]
    fn spawn_failure_is_terminal_and_does_not_occupy_the_key() {
        let manager = manager();
        manager.replace_scope(vec![definition("query")]).unwrap();
        manager
            .start_process(
                "query",
                vec!["/thing/managed-process-fixture-does-not-exist".into()],
                None,
            )
            .unwrap();
        let failed = wait_for(&manager, |state| state.status == Status::Failed);
        assert!(!failed.error.is_empty());
        manager
            .start_process("query", helper("output"), None)
            .unwrap();
        wait_for(&manager, |state| state.status == Status::Exited);
    }

    #[test]
    fn wake_failure_retires_a_queued_start() {
        let mut manager = manager();
        manager.replace_scope(vec![definition("query")]).unwrap();
        let gate = Arc::new(SpawnGate::default());
        lock(&manager.shared).before_dequeue = Some(gate.clone());
        let (broken_wake, peer) = UnixStream::pair().unwrap();
        drop(peer);
        manager.wake = broken_wake;
        gate.wait_until_reached();
        let error = manager
            .start_process("query", helper("ignore-term"), None)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        {
            let shared = lock(&manager.shared);
            assert_eq!(shared.starts.len(), 1);
            assert_eq!(shared.occupied.len(), 1);
        }
        gate.release();
        let state = wait_for(&manager, |state| state.status == Status::Failed);
        assert!(!state.error.is_empty());
        assert_eq!(state.exit_signal, None);
        assert!(lock(&manager.shared).occupied.is_empty());
    }

    #[test]
    fn wake_failure_after_dequeue_still_reaps_the_spawned_child() {
        let mut manager = manager();
        manager.replace_scope(vec![definition("query")]).unwrap();
        let gate = Arc::new(SpawnGate::default());
        lock(&manager.shared).before_spawn = Some(gate.clone());
        let (broken_wake, peer) = UnixStream::pair().unwrap();
        drop(peer);
        manager.wake = broken_wake;

        let error = manager
            .start_process("query", helper("ignore-term"), None)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        {
            let shared = lock(&manager.shared);
            assert!(shared.starts.is_empty());
            assert_eq!(shared.occupied.len(), 1);
        }
        gate.release();

        let state = wait_for(&manager, |state| state.status == Status::Failed);
        assert!(!state.error.is_empty());
        assert!(state.exit_signal.is_some());
        assert!(lock(&manager.shared).occupied.is_empty());
    }

    #[test]
    fn stop_escalates_when_child_ignores_termination() {
        let manager = manager();
        manager.replace_scope(vec![definition("stubborn")]).unwrap();
        manager
            .start_process("stubborn", helper("ignore-term"), None)
            .unwrap();
        wait_for(&manager, |state| state.stdout.contains("ready"));
        manager.stop_process("stubborn").unwrap();
        let state = wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(state.exit_signal, Some(libc::SIGKILL));
    }

    #[test]
    fn direct_child_is_reaped_after_leaving_its_initial_group() {
        let manager = manager();
        manager.replace_scope(vec![definition("stubborn")]).unwrap();
        manager
            .start_process("stubborn", helper("escape-group"), None)
            .unwrap();
        wait_for(&manager, |state| state.stdout.contains("escaped_group="));
        manager.stop_process("stubborn").unwrap();
        let state = wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(state.exit_signal, Some(libc::SIGKILL));
    }

    #[test]
    fn configured_interrupt_is_used_for_cooperative_shutdown() {
        let manager = manager();
        manager
            .replace_scope(vec![Definition {
                key: "interruptible".into(),
                shutdown_signal: ShutdownSignal::Interrupt,
                command: None,
            }])
            .unwrap();
        manager
            .start_process("interruptible", helper("ignore-term"), None)
            .unwrap();
        wait_for(&manager, |state| state.stdout.contains("ready"));
        manager.stop_process("interruptible").unwrap();
        let state = wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(state.exit_signal, Some(libc::SIGINT));
    }

    #[test]
    fn output_tail_is_globally_reserved_and_bounded_per_stream() {
        let manager = manager();
        manager.replace_scope(vec![definition("flood")]).unwrap();
        manager
            .start_process("flood", helper("flood"), None)
            .unwrap();
        let state = wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(state.stdout.len(), MAX_STREAM_BYTES);
        assert!(state.stdout_truncated);
    }

    #[test]
    fn child_process_group_is_retired_when_its_leader_exits() {
        let manager = manager();
        manager.replace_scope(vec![definition("group")]).unwrap();
        manager
            .start_process("group", helper("group"), None)
            .unwrap();
        let state = wait_for(&manager, |state| state.status == Status::Exited);
        assert!(state.stdout.contains("descendant="));
        assert_eq!(state.exit_code, Some(0));
    }

    #[test]
    fn replacing_scope_does_not_allow_an_unreaped_key_replacement() {
        let manager = manager();
        manager.replace_scope(vec![definition("same")]).unwrap();
        manager
            .start_process("same", helper("ignore-term"), None)
            .unwrap();
        wait_for(&manager, |state| state.stdout.contains("ready"));
        manager.replace_scope(vec![definition("same")]).unwrap();
        let error = manager
            .start_process("same", helper("output"), None)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        wait_for(&manager, |state| state.status == Status::Idle);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match manager.start_process("same", helper("output"), None) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("unexpected start error: {error}"),
            }
        }
        wait_for(&manager, |state| state.status == Status::Exited);
    }

    #[test]
    fn retiring_generation_keeps_its_buffer_reservations_until_reap() {
        let mut limits = test_limits();
        limits.stop_grace = Duration::from_secs(5);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let manager = Manager::start_with_limits(ping, limits).unwrap();
        let definitions = (0..16)
            .map(|index| definition(&format!("old-{index}")))
            .collect::<Vec<_>>();
        manager.replace_scope(definitions).unwrap();
        for index in 0..16 {
            manager
                .start_process(&format!("old-{index}"), helper("ignore-term"), None)
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = manager.snapshot();
            if snapshot
                .states
                .iter()
                .all(|state| state.stdout.contains("ready"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "timed out with {snapshot:?}");
            thread::sleep(Duration::from_millis(10));
        }
        let gate = Arc::new(SpawnGate::default());
        lock(&manager.shared).before_dequeue = Some(gate.clone());
        gate.wait_until_reached();
        manager.replace_scope(vec![definition("new")]).unwrap();
        assert_eq!(
            lock(&manager.shared).reserved_bytes,
            16 * RUN_BUFFER_RESERVATION
        );
        manager
            .start_process("new", helper("output"), None)
            .unwrap();
        assert_eq!(
            lock(&manager.shared).reserved_bytes,
            17 * RUN_BUFFER_RESERVATION
        );
        gate.release();
    }

    #[test]
    fn manager_drop_is_bounded_for_a_controlled_child() {
        let manager = manager();
        manager.replace_scope(vec![definition("child")]).unwrap();
        manager
            .start_process("child", helper("ignore-term"), None)
            .unwrap();
        wait_for(&manager, |state| state.stdout.contains("ready"));
        let start = Instant::now();
        drop(manager);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn detached_worker_keeps_the_process_wide_lease_until_it_finishes() {
        let limits = Limits {
            stop_grace: Duration::from_secs(5),
            shutdown_grace: Duration::from_millis(200),
            drain_grace: Duration::from_millis(50),
            drop_wait: Duration::from_millis(10),
        };
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let manager = Manager::start_with_singleton(ping, limits).unwrap();
        manager.replace_scope(vec![definition("child")]).unwrap();
        manager
            .start_process("child", helper("ignore-term"), None)
            .unwrap();
        wait_for(&manager, |state| state.stdout.contains("ready"));
        drop(manager);

        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let error = match Manager::start_with_singleton(ping, limits) {
            Ok(manager) => {
                drop(manager);
                panic!("a replacement manager started before the old worker retired")
            }
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        let deadline = Instant::now() + Duration::from_secs(3);
        let replacement = loop {
            let (ping, _source) = calloop::ping::make_ping().unwrap();
            match Manager::start_with_singleton(ping, limits) {
                Ok(manager) => break manager,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("unexpected manager start error: {error}"),
            }
        };
        drop(replacement);
    }

    #[test]
    fn manager_start_restores_reapable_sigchld_for_managed_children() {
        let mut command =
            fixture("managed_process::tests::fake_manager_with_inherited_sigchld_ignore");
        // SAFETY: the closure changes only its own signal disposition with an
        // async-signal-safe syscall before exec.
        unsafe {
            command.pre_exec(|| {
                let mut action = std::mem::zeroed::<libc::sigaction>();
                action.sa_sigaction = libc::SIG_IGN;
                action.sa_flags = libc::SA_NOCLDWAIT;
                if libc::sigemptyset(&mut action.sa_mask) == -1
                    || libc::sigaction(libc::SIGCHLD, &action, ptr::null_mut()) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("reaped_exit=0"));
    }

    #[test]
    fn production_shutdown_reaps_a_term_ignoring_child_before_parent_exit() {
        let output = fixture("managed_process::tests::fake_manager_shutdown_parent")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let output = String::from_utf8(output.stdout).unwrap();
        let pid = marker_pid(&output, "retired_pid=");
        assert!(
            !process_exists(pid),
            "managed child {pid} survived its parent"
        );
    }

    #[test]
    fn production_shutdown_reaps_a_group_after_its_leader_exits() {
        let output = fixture("managed_process::tests::fake_manager_group_shutdown_parent")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let output = String::from_utf8(output.stdout).unwrap();
        let pid = marker_pid(&output, "retired_descendant=");
        assert!(
            !process_exists(pid),
            "managed descendant {pid} survived its parent"
        );
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_child_output() {
        println!("hello from child");
        eprintln!("child warning");
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_child_staged_output() {
        println!("first");
        std::io::stdout().flush().unwrap();
        thread::sleep(Duration::from_millis(250));
        println!("second");
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_child_counted_output() {
        let path = env::current_dir().unwrap().join("runs");
        let count = std::fs::read_to_string(&path)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0)
            + 1;
        std::fs::write(path, count.to_string()).unwrap();
        println!("run={count}");
        std::io::stdout().flush().unwrap();
        thread::sleep(Duration::from_millis(250));
        println!("complete={count}");
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_child_ignore_term() {
        // SAFETY: SIG_IGN is a process-global, async-signal-safe disposition.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        println!("ready");
        std::io::stdout().flush().unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_child_escape_group() {
        // SAFETY: the parent remains in a process group in this session.
        let parent_group = unsafe { libc::getpgid(libc::getppid()) };
        assert!(parent_group > 0);
        // SAFETY: both this process and the target group are in the inherited
        // session; moving only this process does not affect the test parent.
        assert_eq!(unsafe { libc::setpgid(0, parent_group) }, 0);
        // SAFETY: SIG_IGN is a process-global, async-signal-safe disposition.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        println!("escaped_group={parent_group}");
        std::io::stdout().flush().unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_child_report_ignore_term() {
        // SAFETY: SIG_IGN is a process-global, async-signal-safe disposition.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        // SAFETY: getpid has no preconditions.
        println!("controlled_pid={}", unsafe { libc::getpid() });
        std::io::stdout().flush().unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_child_flood() {
        let bytes = vec![b'x'; MAX_STREAM_BYTES * 2];
        std::io::stdout().write_all(&bytes).unwrap();
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_child_group() {
        let mut pipe = [0; 2];
        // SAFETY: pipe is valid output storage for two descriptors.
        let pipe_result = unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) };
        assert_eq!(pipe_result, 0);
        // SAFETY: the child branch never returns to Rust or touches shared
        // runtime state; the parent continues the isolated fixture normally.
        let descendant = unsafe { libc::fork() };
        if descendant == -1 {
            panic!(
                "cannot fork managed process group fixture: {}",
                io::Error::last_os_error()
            );
        }
        if descendant == 0 {
            // SAFETY: these descriptor, disposition, write, and wait operations
            // are async-signal-safe after fork.
            unsafe {
                libc::close(pipe[0]);
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                let ready = [1_u8];
                let _ = libc::write(pipe[1], ready.as_ptr().cast(), ready.len());
                libc::close(pipe[1]);
                loop {
                    libc::pause();
                }
            }
        }
        // SAFETY: the parent owns both pipe descriptors and waits only for the
        // child's one-byte readiness signal.
        unsafe {
            libc::close(pipe[1]);
            let mut ready = [0_u8];
            loop {
                let read = libc::read(pipe[0], ready.as_mut_ptr().cast(), ready.len());
                if read == 1 {
                    break;
                }
                assert!(
                    read == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
                );
            }
            libc::close(pipe[0]);
        }
        println!("descendant={descendant}");
        std::io::stdout().flush().unwrap();
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_manager_with_inherited_sigchld_ignore() {
        assert!(!crate::launch::sigchld_is_reapable().unwrap());
        let (_launcher, _signals) = crate::launch::prepare().unwrap();
        assert!(!crate::launch::sigchld_is_reapable().unwrap());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let manager = Manager::start(ping).unwrap();
        assert!(crate::launch::sigchld_is_reapable().unwrap());
        manager.replace_scope(vec![definition("query")]).unwrap();
        manager
            .start_process("query", helper("output"), None)
            .unwrap();
        let state = wait_for(&manager, |state| state.status == Status::Exited);
        assert_eq!(state.exit_code, Some(0));
        println!("reaped_exit={}", state.exit_code.unwrap());
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_manager_shutdown_parent() {
        let (_launcher, _signals) = crate::launch::prepare().unwrap();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let manager = Manager::start(ping).unwrap();
        manager.replace_scope(vec![definition("child")]).unwrap();
        manager
            .start_process("child", helper("report-ignore-term"), None)
            .unwrap();
        let state = wait_for(&manager, |state| state.stdout.contains("controlled_pid="));
        let pid = marker_pid(&state.stdout, "controlled_pid=");
        drop(manager);
        assert!(
            !process_exists(pid),
            "managed child {pid} survived manager drop"
        );
        println!("retired_pid={pid}");
    }

    #[test]
    #[ignore = "managed process fixture"]
    fn fake_manager_group_shutdown_parent() {
        let (_launcher, _signals) = crate::launch::prepare().unwrap();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let manager = Manager::start(ping).unwrap();
        manager.replace_scope(vec![definition("group")]).unwrap();
        manager
            .start_process("group", helper("group"), None)
            .unwrap();
        let state = wait_for(&manager, |state| {
            state.status == Status::Stopping && state.stdout.contains("descendant=")
        });
        let pid = marker_pid(&state.stdout, "descendant=");
        drop(manager);
        assert!(
            !process_exists(pid),
            "managed descendant {pid} survived manager drop"
        );
        println!("retired_descendant={pid}");
    }
}
