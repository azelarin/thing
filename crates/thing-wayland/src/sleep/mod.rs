//! system sleep inhibitors held through logind on the system bus.
//!
//! the event loop owns the lease model; a lazily started worker makes the bus
//! calls and keeps nothing but the descriptors. a descriptor lives in shared
//! state so shutdown closes it even when the worker is stuck in a call.

mod model;
#[cfg(test)]
pub(crate) mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use async_io::{Async, Timer};
use calloop::ping::Ping;
use futures_lite::future;
use futures_util::{FutureExt, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
#[cfg(test)]
use thing_runtime::SleepInhibitorStatus;
use thing_runtime::{SleepInhibitorAction, SleepInhibitorDefinition, SleepInhibitorState};
use zbus::{Connection, MatchRule, MessageStream, fdo::DBusProxy, message::Type, names::BusName};

use model::{Command, Leases, Refusal};

const LOGIND_NAME: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_INTERFACE: &str = "org.freedesktop.login1.Manager";
/// the name logind shows in `systemd-inhibit --list`.
const WHO: &str = "thing";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// distinct keys with a pending call, command, report, or held lease.
const MAX_PENDING_KEYS: usize = 64;
const OWNER_QUEUE_CAPACITY: usize = 16;
const MAX_ERROR_BYTES: usize = 512;

/// where the worker finds logind. production uses the system bus; tests pass
/// the address of a private bus.
#[derive(Clone, Debug)]
pub(crate) enum Transport {
    System,
    #[cfg_attr(not(test), allow(dead_code))]
    Address(String),
}

#[derive(Debug)]
enum Report {
    Acquired(u64),
    Refused(u64, Refusal),
    Lost(u64),
}

struct Held {
    generation: u64,
    owner: String,
    _descriptor: OwnedFd,
}

#[derive(Default)]
struct Shared {
    stop: bool,
    outstanding: BTreeSet<String>,
    in_flight: BTreeMap<String, u64>,
    /// the newest command for each key. an acquire replaces any older lease
    /// for its key, and a release removes it, so older commands add nothing.
    commands: BTreeMap<String, Command>,
    reports: BTreeMap<String, Report>,
    daemon_appeared: bool,
    held: BTreeMap<String, Held>,
}

struct Worker {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    thread: Option<JoinHandle<()>>,
}

pub(crate) struct SleepInhibition {
    leases: Leases,
    transport: Transport,
    ping: Ping,
    worker: Option<Worker>,
    published: Vec<SleepInhibitorState>,
}

impl SleepInhibition {
    pub(crate) fn new(transport: Transport, ping: Ping) -> Self {
        Self {
            leases: Leases::default(),
            transport,
            ping,
            worker: None,
            published: Vec::new(),
        }
    }

    /// an inhibitor for host tests. its bus address does not exist, so it can
    /// never reach the desktop's buses.
    #[cfg(test)]
    pub(crate) fn inert() -> Self {
        let (ping, _) = calloop::ping::make_ping().expect("cannot create a test ping");
        Self::new(
            Transport::Address("unix:path=/nonexistent/thing-test-bus".into()),
            ping,
        )
    }

    /// applies the declarations of the accepted configurations.
    pub(crate) fn reconcile(&mut self, definitions: &[SleepInhibitorDefinition]) {
        let commands = self.leases.reconcile(definitions);
        self.send(commands, Instant::now());
        self.ping.ping();
    }

    pub(crate) fn act(&mut self, key: &str, action: SleepInhibitorAction) {
        let commands = self
            .leases
            .set_wanted(key, action == SleepInhibitorAction::Start);
        self.send(commands, Instant::now());
        self.ping.ping();
    }

    /// applies the worker's reports and due retries, and returns the states
    /// when they differ from the last ones returned.
    pub(crate) fn poll(&mut self, now: Instant) -> Option<Vec<SleepInhibitorState>> {
        if let Some(worker) = &self.worker {
            let (reports, appeared) = {
                let mut shared = lock(&worker.shared);
                (
                    std::mem::take(&mut shared.reports),
                    std::mem::take(&mut shared.daemon_appeared),
                )
            };
            if !reports.is_empty() {
                wake(&worker.wake);
            }
            let mut commands = Vec::new();
            for (key, report) in reports {
                match report {
                    Report::Acquired(generation) => self.leases.acquired(&key, generation),
                    Report::Refused(generation, refusal) => {
                        self.leases.refused(&key, generation, refusal, now)
                    }
                    Report::Lost(generation) => commands.extend(self.leases.lost(&key, generation)),
                }
            }
            if appeared {
                commands.extend(self.leases.daemon_appeared());
            }
            self.send(commands, now);
        }
        let due = self.leases.due(now);
        self.send(due, now);
        let states = self.leases.states();
        (states != self.published).then(|| {
            self.published = states.clone();
            states
        })
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.leases.next_deadline()
    }

    fn send(&mut self, commands: Vec<Command>, now: Instant) {
        for command in commands {
            if let Err(refused) = self.queue(command) {
                let (key, generation, error) = refused;
                self.leases
                    .refused(&key, generation, Refusal::Unavailable(error), now);
            }
        }
    }

    fn queue(&mut self, command: Command) -> Result<(), (String, u64, String)> {
        let (key, generation) = match &command {
            Command::Acquire {
                key, generation, ..
            } => (key.clone(), Some(*generation)),
            // without a worker no request was made, so nothing is held.
            Command::Release { .. } if self.worker.is_none() => return Ok(()),
            Command::Release { key } => (key.clone(), None),
        };
        if self.worker.is_none() {
            let worker =
                Worker::start(self.transport.clone(), self.ping.clone()).map_err(|error| {
                    (
                        key.clone(),
                        generation.unwrap_or_default(),
                        format!("cannot start the sleep inhibitor worker: {error}"),
                    )
                })?;
            self.worker = Some(worker);
        }
        let worker = self.worker.as_ref().expect("the worker was just started");
        {
            let mut shared = lock(&worker.shared);
            if let Some(generation) = generation {
                if !shared.outstanding.contains(&key)
                    && shared.outstanding.len() >= MAX_PENDING_KEYS
                {
                    return Err((
                        key,
                        generation,
                        "too many sleep inhibitors are active or waiting".into(),
                    ));
                }
                shared.outstanding.insert(key.clone());
            } else {
                // release is immediate even while the worker is connecting or
                // waiting for another call. unknown keys own no resources.
                shared.held.remove(&key);
                shared.reports.remove(&key);
                if !shared.outstanding.contains(&key) {
                    return Ok(());
                }
            }
            shared.commands.insert(key, command);
        }
        wake(&worker.wake);
        Ok(())
    }
}

impl Worker {
    fn start(transport: Transport, ping: Ping) -> io::Result<Self> {
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let thread = thread::Builder::new()
            .name("thing-sleep".into())
            .spawn(move || run(transport, &state, reader, &ping))?;
        Ok(Self {
            shared,
            wake,
            thread: Some(thread),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        {
            let mut shared = lock(&self.shared);
            shared.stop = true;
            // closing the descriptors here releases every lease even when
            // the worker cannot finish in time.
            shared.held.clear();
        }
        wake(&self.wake);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while !thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            eprintln!("thing: the sleep inhibitor worker did not stop in time");
        }
    }
}

fn run(transport: Transport, shared: &Mutex<Shared>, reader: UnixStream, ping: &Ping) {
    let Ok(wake_source) = Async::new(reader) else {
        fail_pending(shared, ping, "cannot watch the sleep inhibitor queue");
        return;
    };
    loop {
        if stopping(shared) {
            return;
        }
        if needs_connection(shared) {
            let mut requested = BTreeMap::new();
            let result = async_io::block_on(session(
                &transport,
                shared,
                &wake_source,
                ping,
                &mut requested,
            ));
            lock(shared).in_flight.clear();
            if let Err(error) = result
                && !stopping(shared)
            {
                lose_all(shared, ping, requested, &error);
            }
            continue;
        }
        async_io::block_on(async {
            let _ = wake_source.readable().await;
        });
        drain(wake_source.get_ref());
    }
}

/// one connection to the bus. it returns only when the connection fails or
/// the worker stops.
async fn session(
    transport: &Transport,
    shared: &Mutex<Shared>,
    wake_source: &Async<UnixStream>,
    ping: &Ping,
    requested: &mut Requested,
) -> Result<(), String> {
    let connection = match connect(transport, shared, wake_source).await {
        Ok(Some(connection)) => connection,
        Ok(None) => return Ok(()),
        Err(error) => {
            fail_pending(
                shared,
                ping,
                &format!("the system bus is unavailable: {error}"),
            );
            return Ok(());
        }
    };
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")
        .and_then(|rule| rule.interface("org.freedesktop.DBus"))
        .and_then(|rule| rule.member("NameOwnerChanged"))
        .and_then(|rule| rule.add_arg(LOGIND_NAME))
        .map_err(|error| error.to_string())?
        .build();
    let mut names = MessageStream::for_match_rule(rule, &connection, Some(OWNER_QUEUE_CAPACITY))
        .await
        .map_err(|error| error.to_string())?;
    let dbus = DBusProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    let name = BusName::try_from(LOGIND_NAME).map_err(|error| error.to_string())?;
    let mut owner = match dbus.get_name_owner(name).await {
        Ok(owner) => Some(owner.to_string()),
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut calls = FuturesUnordered::<Call>::new();
    loop {
        if stopping(shared) {
            return Ok(());
        }
        let commands = std::mem::take(&mut lock(shared).commands);
        for (key, command) in commands {
            let mut state = lock(shared);
            if state.commands.contains_key(&key) {
                continue;
            }
            state.held.remove(&key);
            requested.remove(&key);
            if matches!(&command, Command::Acquire { .. }) && state.in_flight.contains_key(&key) {
                // keep only the newest request while the old call finishes.
                state.commands.insert(key, command);
                continue;
            }
            let Command::Acquire {
                generation, reason, ..
            } = command
            else {
                continue;
            };
            let Some(current) = owner.clone() else {
                drop(state);
                report(
                    shared,
                    ping,
                    key,
                    Report::Refused(
                        generation,
                        Refusal::Unavailable("logind is not running".into()),
                    ),
                );
                continue;
            };
            state.in_flight.insert(key.clone(), generation);
            drop(state);
            requested.insert(key.clone(), (generation, current.clone()));
            calls.push(inhibit(
                connection.clone(),
                current,
                key,
                generation,
                reason,
            ));
        }
        retire_completed(&mut lock(shared));
        let input = future::race(
            async { Input::Owner(names.next().await) },
            future::race(
                async {
                    match calls.next().await {
                        Some(reply) => Input::Reply(Box::new(reply)),
                        None => future::pending().await,
                    }
                },
                async { Input::Wake(wake_source.readable().await) },
            ),
        )
        .await;
        match input {
            Input::Owner(Some(Ok(message))) => {
                let Ok((_, _, next)) = message.body().deserialize::<(String, String, String)>()
                else {
                    continue;
                };
                let next = (!next.is_empty()).then_some(next);
                if next == owner {
                    continue;
                }
                owner = next;
                // a lease belongs to the daemon that granted it.
                let lost: Vec<_> = {
                    let mut shared = lock(shared);
                    let keys: Vec<_> = shared
                        .held
                        .iter()
                        .filter(|(_, held)| Some(&held.owner) != owner.as_ref())
                        .map(|(key, held)| (key.clone(), held.generation))
                        .collect();
                    for (key, _) in &keys {
                        shared.held.remove(key);
                    }
                    keys
                };
                for (key, generation) in lost {
                    report(shared, ping, key, Report::Lost(generation));
                }
                let stale: Vec<_> = requested
                    .iter()
                    .filter(|(_, (_, asked))| Some(asked) != owner.as_ref())
                    .map(|(key, (generation, _))| (key.clone(), *generation))
                    .collect();
                for (key, generation) in stale {
                    requested.remove(&key);
                    report(shared, ping, key, Report::Lost(generation));
                }
                if owner.is_some() {
                    lock(shared).daemon_appeared = true;
                    ping.ping();
                }
            }
            Input::Owner(Some(Err(error))) => return Err(error.to_string()),
            Input::Owner(None) => return Err("the system bus connection ended".into()),
            Input::Reply(reply) => {
                let Reply {
                    key,
                    generation,
                    owner: asked,
                    result,
                } = *reply;
                let superseded = {
                    let mut state = lock(shared);
                    state.in_flight.remove(&key);
                    state.commands.contains_key(&key)
                };
                if superseded || requested.get(&key) != Some(&(generation, asked.clone())) {
                    // the request was released or replaced: dropping the
                    // descriptor closes the late lease.
                    continue;
                }
                requested.remove(&key);
                match result {
                    Ok(descriptor) => {
                        let mut state = lock(shared);
                        if state.stop {
                            return Ok(());
                        }
                        state.held.insert(
                            key.clone(),
                            Held {
                                generation,
                                owner: asked,
                                _descriptor: descriptor,
                            },
                        );
                        drop(state);
                        report(shared, ping, key, Report::Acquired(generation));
                    }
                    Err(refusal) => report(shared, ping, key, Report::Refused(generation, refusal)),
                }
            }
            Input::Wake(result) => {
                result.map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
            }
        }
    }
}

/// the generation and daemon of each key's newest request, so a late reply
/// for a released or replaced request closes its descriptor.
type Requested = BTreeMap<String, (u64, String)>;

enum Input {
    Owner(Option<zbus::Result<zbus::Message>>),
    Reply(Box<Reply>),
    Wake(io::Result<()>),
}

struct Reply {
    key: String,
    generation: u64,
    owner: String,
    result: Result<OwnedFd, Refusal>,
}

type Call = LocalBoxFuture<'static, Reply>;

fn inhibit(
    connection: Connection,
    owner: String,
    key: String,
    generation: u64,
    reason: String,
) -> Call {
    async move {
        let reply = connection
            .call_method(
                Some(owner.as_str()),
                LOGIND_PATH,
                Some(LOGIND_INTERFACE),
                "Inhibit",
                &("sleep", WHO, reason.as_str(), "block"),
            )
            .await;
        let result = match reply {
            Ok(message) => message
                .body()
                .deserialize::<zbus::zvariant::OwnedFd>()
                .map(OwnedFd::from)
                .map_err(|error| {
                    Refusal::Denied(sanitize(format!(
                        "logind returned an invalid reply: {error}"
                    )))
                }),
            Err(error) => Err(refusal(error)),
        };
        Reply {
            key,
            generation,
            owner,
            result,
        }
    }
    .boxed_local()
}

async fn connect(
    transport: &Transport,
    shared: &Mutex<Shared>,
    wake_source: &Async<UnixStream>,
) -> Result<Option<Connection>, String> {
    let builder = match transport {
        Transport::System => zbus::connection::Builder::system(),
        Transport::Address(address) => zbus::connection::Builder::address(address.as_str()),
    }
    .map_err(|error| error.to_string())?
    .method_timeout(CALL_TIMEOUT);
    future::race(
        async {
            builder
                .build()
                .await
                .map(Some)
                .map_err(|error| error.to_string())
        },
        future::race(
            async {
                Timer::after(CONNECT_TIMEOUT).await;
                Err("the connection timed out".to_owned())
            },
            async {
                loop {
                    let _ = wake_source.readable().await;
                    drain(wake_source.get_ref());
                    if stopping(shared) {
                        return Ok(None);
                    }
                }
            },
        ),
    )
    .await
}

/// errors that say the daemon or the bus is missing may pass; anything else
/// is logind's answer and would only repeat.
fn refusal(error: zbus::Error) -> Refusal {
    let message = sanitize(error.to_string());
    let zbus::Error::MethodError(name, _, _) = &error else {
        return Refusal::Unavailable(message);
    };
    match name.as_str() {
        "org.freedesktop.DBus.Error.ServiceUnknown"
        | "org.freedesktop.DBus.Error.NameHasNoOwner"
        | "org.freedesktop.DBus.Error.NoReply"
        | "org.freedesktop.DBus.Error.Timeout"
        | "org.freedesktop.DBus.Error.TimedOut"
        | "org.freedesktop.DBus.Error.Disconnected"
        | "org.freedesktop.DBus.Error.NoServer" => Refusal::Unavailable(message),
        _ => Refusal::Denied(message),
    }
}

fn report(shared: &Mutex<Shared>, ping: &Ping, key: String, report: Report) {
    let mut state = lock(shared);
    if state.stop {
        return;
    }
    state.reports.insert(key, report);
    debug_assert!(state.reports.len() <= MAX_PENDING_KEYS);
    drop(state);
    ping.ping();
}

/// whether waiting work needs the bus. releases alone are dropped in the same
/// lock, since nothing is held without a connection and an acquire queued
/// after the check must not be lost.
fn needs_connection(shared: &Mutex<Shared>) -> bool {
    let mut state = lock(shared);
    let acquire = state
        .commands
        .values()
        .any(|command| matches!(command, Command::Acquire { .. }));
    if !acquire {
        state.commands.clear();
    }
    retire_completed(&mut state);
    acquire
}

/// a slot remains occupied until its work finishes and the event loop has
/// consumed its report. only the worker calls this, outside a command batch.
fn retire_completed(state: &mut Shared) {
    state.outstanding.retain(|key| {
        state.commands.contains_key(key)
            || state.in_flight.contains_key(key)
            || state.held.contains_key(key)
            || state.reports.contains_key(key)
    });
}

/// refuses the waiting requests because no connection could be made.
fn fail_pending(shared: &Mutex<Shared>, ping: &Ping, error: &str) {
    let commands = std::mem::take(&mut lock(shared).commands);
    for (key, command) in commands {
        if let Command::Acquire { generation, .. } = command {
            report(
                shared,
                ping,
                key,
                Report::Refused(generation, Refusal::Unavailable(sanitize(error.to_owned()))),
            );
        }
    }
}

/// a lost connection cannot tell whether logind still knows our leases, so
/// they are closed and requested again, as are the unanswered requests.
fn lose_all(shared: &Mutex<Shared>, ping: &Ping, requested: Requested, error: &str) {
    eprintln!("thing: sleep inhibitors lost the system bus: {error}");
    let held = std::mem::take(&mut lock(shared).held);
    let lost = held
        .into_iter()
        .map(|(key, held)| (key, held.generation))
        .chain(
            requested
                .into_iter()
                .map(|(key, (generation, _))| (key, generation)),
        );
    for (key, generation) in lost {
        report(shared, ping, key, Report::Lost(generation));
    }
}

fn sanitize(value: String) -> String {
    let mut value: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if value.len() > MAX_ERROR_BYTES {
        let mut boundary = MAX_ERROR_BYTES;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    value
}

fn wake(mut stream: &UnixStream) {
    loop {
        match stream.write(&[1]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            _ => break,
        }
    }
}

fn drain(mut stream: &UnixStream) {
    let mut bytes = [0; 64];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stopping(shared: &Mutex<Shared>) -> bool {
    lock(shared).stop
}

impl SleepInhibition {
    /// whether the worker was started, which is the only way to reach a bus.
    #[cfg(test)]
    pub(crate) fn started(&self) -> bool {
        self.worker.is_some()
    }

    /// the keys whose lease descriptor the worker holds.
    #[cfg(test)]
    fn held(&self) -> Vec<String> {
        self.worker
            .as_ref()
            .map(|worker| lock(&worker.shared).held.keys().cloned().collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn status(&self, key: &str) -> Option<(SleepInhibitorStatus, String)> {
        self.leases
            .states()
            .into_iter()
            .find(|state| state.key == key)
            .map(|state| (state.status, state.error))
    }
}
