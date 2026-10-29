//! d-bus activation through `org.freedesktop.Application`, on a worker thread
//! so bus calls never block the wayland event loop.

use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::Duration,
};

use async_io::{Async, Timer};
use calloop::ping::Ping;
use futures_lite::future;
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use zbus::zvariant::Value;

use crate::launch::LaunchRequest;

const INTERFACE: &str = "org.freedesktop.Application";
const BUS_DRIVER: &str = "org.freedesktop.DBus";
const MAX_QUEUED: usize = 16;
const MAX_IN_FLIGHT: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
// the reference bus allows 25 seconds for a service to start.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);

/// errors from the bus itself that mean the application never received the
/// call and never ran. an activation that started a process, such as
/// `Spawn.ChildExited` or `Spawn.Timeout`, is not listed: that process may
/// already have acted.
const UNDELIVERED: &[&str] = &[
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
    "org.freedesktop.DBus.Error.AccessDenied",
    "org.freedesktop.DBus.Error.LimitsExceeded",
    "org.freedesktop.DBus.Error.Spawn.ExecFailed",
    "org.freedesktop.DBus.Error.Spawn.ForkFailed",
    "org.freedesktop.DBus.Error.Spawn.FailedToSetup",
    "org.freedesktop.DBus.Error.Spawn.ConfigInvalid",
    "org.freedesktop.DBus.Error.Spawn.ServiceNotValid",
    "org.freedesktop.DBus.Error.Spawn.ServiceNotFound",
    "org.freedesktop.DBus.Error.Spawn.PermissionsInvalid",
    "org.freedesktop.DBus.Error.Spawn.FileInvalid",
    "org.freedesktop.DBus.Error.Spawn.NoMemory",
];

/// replies from the application saying it does not implement the call, so it
/// did not act on it.
const UNSUPPORTED: &[&str] = &[
    "org.freedesktop.DBus.Error.UnknownMethod",
    "org.freedesktop.DBus.Error.UnknownInterface",
    "org.freedesktop.DBus.Error.UnknownObject",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Activate,
    Open(Vec<String>),
    ActivateAction(String),
}

/// one fully resolved activation. it never consults the catalog again, so a
/// later change cannot redirect it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ActivationCall {
    pub id: String,
    pub bus_name: String,
    pub path: String,
    pub method: Method,
    /// the `Exec` launch used when the call was definitely not delivered, or
    /// why there is none.
    pub fallback: Result<Vec<LaunchRequest>, String>,
}

/// processes to start because d-bus activation definitely failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Fallback {
    pub requests: Vec<LaunchRequest>,
    pub token: Option<String>,
}

impl Fallback {
    /// pairs each process with a token. the token belongs to one launch, so
    /// only its first process receives it.
    pub(crate) fn into_launches(self) -> Vec<(LaunchRequest, Option<String>)> {
        let mut token = self.token;
        self.requests
            .into_iter()
            .map(|request| (request, token.take()))
            .collect()
    }
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<(ActivationCall, Option<String>)>,
    fallbacks: VecDeque<Fallback>,
    // completed fallbacks retain their slot until the event loop takes them.
    outstanding: usize,
    stop: bool,
    #[cfg(test)]
    outcomes: Vec<(String, Outcome)>,
}

pub(crate) struct Activator {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    shutdown: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl Activator {
    /// starts the worker. it connects to the session bus only when the first
    /// activation arrives.
    pub(crate) fn start(ping: Ping) -> io::Result<Self> {
        Self::start_with(None, CALL_TIMEOUT, ping)
    }

    /// a worker for a private bus, for host tests.
    #[cfg(test)]
    pub(crate) fn start_on(address: &str, ping: Ping) -> io::Result<Self> {
        Self::start_with(Some(address.to_owned()), CALL_TIMEOUT, ping)
    }

    /// how many calls have finished, delivered or not.
    #[cfg(test)]
    pub(crate) fn finished(&self) -> usize {
        lock(&self.shared).outcomes.len()
    }

    fn start_with(address: Option<String>, timeout: Duration, ping: Ping) -> io::Result<Self> {
        let (wake, wake_reader) = UnixStream::pair()?;
        let (shutdown, shutdown_reader) = UnixStream::pair()?;
        for stream in [&wake, &wake_reader, &shutdown, &shutdown_reader] {
            stream.set_nonblocking(true)?;
        }
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("thing-activation".into())
            .spawn(move || {
                let context = Context {
                    address,
                    timeout,
                    shared: state,
                    ping,
                };
                if let Err(error) =
                    async_io::block_on(serve(&context, wake_reader, shutdown_reader))
                {
                    eprintln!("thing: application activation stopped: {error}");
                }
            })?;
        Ok(Self {
            shared,
            wake,
            shutdown,
            worker: Some(worker),
        })
    }

    /// queues one activation. the token, if any, is used for this call and its
    /// fallback only.
    pub(crate) fn send(&self, call: ActivationCall, token: Option<String>) -> Result<(), String> {
        let mut shared = lock(&self.shared);
        if shared.stop {
            return Err("application activation has stopped".into());
        }
        if shared.queue.len() >= MAX_QUEUED || shared.outstanding >= MAX_QUEUED + MAX_IN_FLIGHT {
            return Err(format!(
                "cannot activate {:?}: too many activations are waiting",
                call.id
            ));
        }
        shared.queue.push_back((call, token));
        shared.outstanding += 1;
        drop(shared);
        let _ = (&self.wake).write(&[1]);
        Ok(())
    }

    pub(crate) fn take_fallbacks(&self) -> Vec<Fallback> {
        let mut shared = lock(&self.shared);
        let fallbacks: Vec<_> = shared.fallbacks.drain(..).collect();
        shared.outstanding -= fallbacks.len();
        fallbacks
    }
}

impl Drop for Activator {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        let _ = (&self.shutdown).write(&[1]);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Context {
    address: Option<String>,
    timeout: Duration,
    shared: Arc<Mutex<Shared>>,
    ping: Ping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Delivered,
    /// the application did not receive the call, so `Exec` may run instead.
    Undelivered(String),
    /// the application may have received the call. falling back could start
    /// it twice, so nothing is retried.
    Ambiguous(String),
}

enum Event {
    Wake,
    Stop,
    Done(ActivationCall, Option<String>, Outcome, bool),
    Closed,
}

type Pending = BoxFuture<'static, (ActivationCall, Option<String>, Outcome, bool)>;

async fn serve(context: &Context, wake: UnixStream, shutdown: UnixStream) -> io::Result<()> {
    let wake = Async::new(wake)?;
    let shutdown = Async::new(shutdown)?;
    let mut connection: Option<zbus::Connection> = None;
    let mut in_flight = FuturesUnordered::<Pending>::new();
    loop {
        while in_flight.len() < MAX_IN_FLIGHT {
            let Some((call, token)) = lock(&context.shared).queue.pop_front() else {
                break;
            };
            if connection.is_none() {
                match connect(context.address.as_deref(), &shutdown).await {
                    Connect::Connected(established) => connection = Some(established),
                    Connect::Failed(error) => {
                        let reason = format!("cannot connect to the session bus: {error}");
                        finish(context, call, token, Outcome::Undelivered(reason));
                        continue;
                    }
                    Connect::Stopped => return Ok(()),
                }
            }
            let established = connection.clone().expect("a connection was just ensured");
            let timeout = context.timeout;
            in_flight.push(
                async move {
                    let (outcome, lost) =
                        deliver(&established, &call, token.as_deref(), timeout).await;
                    (call, token, outcome, lost)
                }
                .boxed(),
            );
        }
        let event = future::or(
            async {
                match shutdown.readable().await {
                    Ok(()) => Event::Stop,
                    Err(_) => Event::Stop,
                }
            },
            future::or(
                async {
                    let _ = wake.readable().await;
                    Event::Wake
                },
                future::or(
                    async {
                        match in_flight.next().await {
                            Some((call, token, outcome, lost)) => {
                                Event::Done(call, token, outcome, lost)
                            }
                            None => future::pending().await,
                        }
                    },
                    async {
                        match &connection {
                            Some(connection) => connection.closed().await,
                            None => future::pending().await,
                        }
                        Event::Closed
                    },
                ),
            ),
        )
        .await;
        match event {
            // unsent calls are dropped rather than replayed by a later instance.
            Event::Stop => return Ok(()),
            Event::Wake => {
                drain(wake.get_ref());
                if lock(&context.shared).stop {
                    return Ok(());
                }
            }
            Event::Done(call, token, outcome, lost) => {
                if lost {
                    connection = None;
                }
                finish(context, call, token, outcome);
            }
            Event::Closed => connection = None,
        }
    }
}

enum Connect {
    Connected(zbus::Connection),
    Failed(String),
    Stopped,
}

async fn connect(address: Option<&str>, shutdown: &Async<UnixStream>) -> Connect {
    let build = async {
        let builder = match address {
            Some(address) => zbus::connection::Builder::address(address),
            None => zbus::connection::Builder::session(),
        };
        match builder {
            Ok(builder) => match builder.build().await {
                Ok(connection) => Connect::Connected(connection),
                Err(error) => Connect::Failed(error.to_string()),
            },
            Err(error) => Connect::Failed(error.to_string()),
        }
    };
    future::or(
        build,
        future::or(
            async {
                Timer::after(CONNECT_TIMEOUT).await;
                Connect::Failed("the connection timed out".into())
            },
            async {
                let _ = shutdown.readable().await;
                Connect::Stopped
            },
        ),
    )
    .await
}

/// sends one call and classifies its result. the flag reports a connection
/// that should not be reused.
async fn deliver(
    connection: &zbus::Connection,
    call: &ActivationCall,
    token: Option<&str>,
    timeout: Duration,
) -> (Outcome, bool) {
    let mut platform_data = HashMap::<&str, Value<'_>>::new();
    if let Some(token) = token {
        platform_data.insert("activation-token", Value::from(token));
        platform_data.insert("desktop-startup-id", Value::from(token));
    }
    let destination = Some(call.bus_name.as_str());
    let path = call.path.as_str();
    let reply = async {
        match &call.method {
            Method::Activate => {
                connection
                    .call_method(
                        destination,
                        path,
                        Some(INTERFACE),
                        "Activate",
                        &(platform_data,),
                    )
                    .await
            }
            Method::Open(uris) => {
                connection
                    .call_method(
                        destination,
                        path,
                        Some(INTERFACE),
                        "Open",
                        &(uris, platform_data),
                    )
                    .await
            }
            Method::ActivateAction(action) => {
                connection
                    .call_method(
                        destination,
                        path,
                        Some(INTERFACE),
                        "ActivateAction",
                        &(action.as_str(), Vec::<Value<'_>>::new(), platform_data),
                    )
                    .await
            }
        }
    };
    let result = future::or(async { Some(reply.await) }, async {
        Timer::after(timeout).await;
        None
    })
    .await;
    match result {
        None => (
            Outcome::Ambiguous(format!("no reply within {} seconds", timeout.as_secs_f32())),
            false,
        ),
        Some(Ok(_)) => (Outcome::Delivered, false),
        Some(Err(zbus::Error::MethodError(name, detail, reply))) => {
            let name = name.as_str();
            let from_bus = reply
                .header()
                .sender()
                .is_some_and(|sender| sender.as_str() == BUS_DRIVER);
            let reason = match detail {
                Some(detail) => format!("{name}: {detail}"),
                None => name.to_owned(),
            };
            if (from_bus && UNDELIVERED.contains(&name)) || UNSUPPORTED.contains(&name) {
                (Outcome::Undelivered(reason), false)
            } else {
                (Outcome::Ambiguous(reason), false)
            }
        }
        Some(Err(error)) => (Outcome::Ambiguous(error.to_string()), true),
    }
}

fn finish(context: &Context, call: ActivationCall, token: Option<String>, outcome: Outcome) {
    let id = &call.id;
    #[cfg(test)]
    let observed = (id.clone(), outcome.clone());
    let fallback = match outcome {
        Outcome::Delivered => None,
        Outcome::Undelivered(reason) => match call.fallback {
            Ok(requests) => Some(Fallback { requests, token }),
            Err(error) => {
                eprintln!("thing: cannot activate {id:?} ({reason}) or start it: {error}");
                None
            }
        },
        Outcome::Ambiguous(reason) => {
            eprintln!(
                "thing: activating {id:?} did not complete ({reason}); it is not retried because the application may have received it"
            );
            None
        }
    };
    let mut shared = lock(&context.shared);
    #[cfg(test)]
    shared.outcomes.push(observed);
    if let Some(fallback) = fallback {
        shared.fallbacks.push_back(fallback);
        drop(shared);
        context.ping.ping();
    } else {
        shared.outstanding -= 1;
    }
}

fn drain(stream: &UnixStream) {
    let mut buffer = [0_u8; 64];
    let mut stream = stream;
    while matches!(stream.read(&mut buffer), Ok(count) if count > 0) {}
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::applications::test_bus::{Application, Behavior, Bus, Received, path};

    fn call(name: &str, method: Method) -> ActivationCall {
        ActivationCall {
            id: format!("{name}.desktop"),
            bus_name: name.into(),
            path: path(name),
            method,
            fallback: Ok(vec![LaunchRequest {
                argv: vec![format!("{name}-exec")],
                cwd: None,
            }]),
        }
    }

    fn start(bus: &Bus, timeout: Duration) -> Activator {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        Activator::start_with(Some(bus.address.clone()), timeout, ping).unwrap()
    }

    fn outcome(activator: &Activator, name: &str) -> Outcome {
        let id = format!("{name}.desktop");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((_, outcome)) = lock(&activator.shared)
                .outcomes
                .iter()
                .find(|(candidate, _)| *candidate == id)
            {
                return outcome.clone();
            }
            assert!(Instant::now() < deadline, "no outcome for {id}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn token_data(token: &str) -> Vec<(String, String)> {
        vec![
            ("activation-token".into(), token.into()),
            ("desktop-startup-id".into(), token.into()),
        ]
    }

    #[test]
    fn methods_arguments_and_tokens_reach_the_application() {
        let bus = Bus::start(&[]);
        let application = Application::start(&bus, "org.example.App", Some(Behavior::Reply));
        let activator = start(&bus, CALL_TIMEOUT);
        let mut action = call(
            "org.example.App",
            Method::ActivateAction("new-window".into()),
        );
        action.id = "action".into();
        let mut open = call(
            "org.example.App",
            Method::Open(vec![
                "file:///tmp/a%20b".into(),
                "https://example.org".into(),
            ]),
        );
        open.id = "open".into();
        activator
            .send(
                call("org.example.App", Method::Activate),
                Some("token-1".into()),
            )
            .unwrap();
        wait_until(|| application.received().len() == 1);
        activator.send(open, None).unwrap();
        wait_until(|| application.received().len() == 2);
        activator.send(action, Some("token-2".into())).unwrap();
        wait_until(|| application.received().len() == 3);

        assert_eq!(
            application.received(),
            [
                Received::Activate(token_data("token-1")),
                Received::Open(
                    vec!["file:///tmp/a%20b".into(), "https://example.org".into()],
                    Vec::new(),
                ),
                Received::ActivateAction("new-window".into(), 0, token_data("token-2")),
            ]
        );
        assert_eq!(outcome(&activator, "org.example.App"), Outcome::Delivered);
        wait_until(|| lock(&activator.shared).outcomes.len() == 3);
        assert!(
            lock(&activator.shared)
                .outcomes
                .iter()
                .all(|(_, outcome)| *outcome == Outcome::Delivered)
        );
        assert!(activator.take_fallbacks().is_empty());
    }

    #[test]
    fn definite_failures_fall_back_once_with_the_launch_token() {
        let mut bus = Bus::start(&[(
            "org.example.Unrunnable",
            "#!/definitely/missing/interpreter\n",
        )]);
        // owns the name but implements nothing, as a misdeclared entry would.
        let _bare = Application::start(&bus, "org.example.Bare", None);
        let activator = start(&bus, CALL_TIMEOUT);
        for (name, error) in [
            ("org.example.Missing", "ServiceUnknown"),
            ("org.example.Bare", "UnknownObject"),
            ("org.example.Unrunnable", "Spawn.ExecFailed"),
        ] {
            activator
                .send(call(name, Method::Activate), Some(format!("{name}-token")))
                .unwrap();
            let outcome = outcome(&activator, name);
            assert!(
                matches!(&outcome, Outcome::Undelivered(reason) if reason.contains(error)),
                "{name}: {outcome:?}"
            );
        }
        let fallbacks = activator.take_fallbacks();
        assert_eq!(
            fallbacks,
            [
                "org.example.Missing",
                "org.example.Bare",
                "org.example.Unrunnable"
            ]
            .map(|name| {
                Fallback {
                    requests: call(name, Method::Activate).fallback.unwrap(),
                    token: Some(format!("{name}-token")),
                }
            })
        );
        assert!(activator.take_fallbacks().is_empty());

        // without a bus, the call cannot have been delivered either.
        bus.stop();
        let activator = start(&bus, CALL_TIMEOUT);
        activator
            .send(call("org.example.Offline", Method::Activate), None)
            .unwrap();
        assert!(matches!(
            outcome(&activator, "org.example.Offline"),
            Outcome::Undelivered(reason) if reason.contains("cannot connect")
        ));
        assert_eq!(activator.take_fallbacks().len(), 1);
    }

    #[test]
    fn ambiguous_failures_never_fall_back() {
        let markers = tempfile::tempdir().unwrap();
        let marker = markers.path().join("started");
        // this service runs and fails without taking its name.
        let bus = Bus::start(&[(
            "org.example.Exits",
            &format!("#!/bin/sh\ntouch '{}'\nexit 3\n", marker.display()),
        )]);
        let _failing = Application::start(&bus, "org.example.Fails", Some(Behavior::Fail));
        let _hanging = Application::start(&bus, "org.example.Hangs", Some(Behavior::Hang));
        let leaving = Application::start(&bus, "org.example.Leaves", Some(Behavior::Hang));
        let activator = start(&bus, CALL_TIMEOUT);
        let impatient = start(&bus, Duration::from_millis(300));

        for name in ["org.example.Fails", "org.example.Exits"] {
            activator.send(call(name, Method::Activate), None).unwrap();
        }
        impatient
            .send(call("org.example.Hangs", Method::Activate), None)
            .unwrap();
        activator
            .send(call("org.example.Leaves", Method::Activate), None)
            .unwrap();
        // the application receives the call and then disconnects without a reply.
        wait_until(|| leaving.received().len() == 1);
        async_io::block_on(leaving.connection.clone().close()).unwrap();

        for (name, error) in [
            ("org.example.Fails", "Error.Failed"),
            ("org.example.Exits", "Spawn.ChildExited"),
            ("org.example.Hangs", "no reply"),
            ("org.example.Leaves", "NoReply"),
        ] {
            let activator = if name == "org.example.Hangs" {
                &impatient
            } else {
                &activator
            };
            let outcome = outcome(activator, name);
            assert!(
                matches!(&outcome, Outcome::Ambiguous(reason) if reason.contains(error)),
                "{name}: {outcome:?}"
            );
        }
        // the activated program ran, so starting `Exec` as well could open it twice.
        assert!(marker.exists());
        assert!(activator.take_fallbacks().is_empty());
        assert!(impatient.take_fallbacks().is_empty());
    }

    #[test]
    fn a_new_bus_connection_does_not_replay_calls() {
        let mut bus = Bus::start(&[]);
        let first = Application::start(&bus, "org.example.First", Some(Behavior::Hang));
        let activator = start(&bus, CALL_TIMEOUT);
        activator
            .send(call("org.example.First", Method::Activate), None)
            .unwrap();
        wait_until(|| first.received().len() == 1);
        bus.stop();
        assert!(matches!(
            outcome(&activator, "org.example.First"),
            Outcome::Ambiguous(_)
        ));
        drop(first);

        bus.spawn();
        let second = Application::start(&bus, "org.example.First", Some(Behavior::Reply));
        let mut again = call("org.example.First", Method::Activate);
        again.id = "again".into();
        activator.send(again, None).unwrap();
        wait_until(|| lock(&activator.shared).outcomes.len() == 2);
        assert_eq!(
            lock(&activator.shared).outcomes[1],
            ("again".into(), Outcome::Delivered)
        );
        assert_eq!(second.received(), [Received::Activate(Vec::new())]);
        assert!(activator.take_fallbacks().is_empty());
    }

    #[test]
    fn a_fallback_token_reaches_only_the_first_process() {
        let request = |program: &str| LaunchRequest {
            argv: vec![program.into()],
            cwd: None,
        };
        let fallback = Fallback {
            requests: vec![request("a"), request("b")],
            token: Some("token".into()),
        };
        assert_eq!(
            fallback.into_launches(),
            [(request("a"), Some("token".into())), (request("b"), None)]
        );
    }

    #[test]
    fn completed_fallbacks_keep_the_activation_queue_bounded() {
        let bus = Bus::start(&[]);
        let activator = start(&bus, CALL_TIMEOUT);
        let limit = MAX_QUEUED + MAX_IN_FLIGHT;
        for index in 0..limit {
            let mut request = call("org.example.Missing", Method::Activate);
            request.id = format!("missing-{index}");
            activator.send(request, None).unwrap();
            wait_until(|| lock(&activator.shared).fallbacks.len() == index + 1);
        }
        assert!(
            activator
                .send(call("org.example.Missing", Method::Activate), None)
                .is_err()
        );
        assert_eq!(activator.take_fallbacks().len(), limit);
        assert!(
            activator
                .send(call("org.example.Missing", Method::Activate), None)
                .is_ok()
        );
    }

    #[test]
    fn queues_are_bounded_and_shutdown_does_not_wait_for_calls() {
        let bus = Bus::start(&[]);
        let hanging = Application::start(&bus, "org.example.Hangs", Some(Behavior::Hang));
        let activator = start(&bus, CALL_TIMEOUT);
        let accepted = (0..40)
            .filter(|_| {
                activator
                    .send(call("org.example.Hangs", Method::Activate), None)
                    .is_ok()
            })
            .count();
        assert!(
            (MAX_QUEUED..=MAX_QUEUED + MAX_IN_FLIGHT).contains(&accepted),
            "{accepted}"
        );
        wait_until(|| hanging.received().len() == MAX_IN_FLIGHT.min(accepted));
        let started = Instant::now();
        drop(activator);
        assert!(started.elapsed() < Duration::from_secs(1));
        // calls that never left the queue are dropped, not sent later.
        thread::sleep(Duration::from_millis(100));
        assert_eq!(hanging.received().len(), MAX_IN_FLIGHT.min(accepted));
    }
}
