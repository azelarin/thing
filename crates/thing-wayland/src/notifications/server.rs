use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use async_io::{Async, Timer};
use calloop::ping::Ping;
use futures_lite::future;
use thing_core::{ImageCandidate, ImageSource};
use thing_runtime::{
    Notification as RuntimeNotification, NotificationButton, NotificationCloseReason,
    NotificationSnapshot, NotificationUrgency,
};
use zbus::object_server::SignalEmitter;

use super::{
    model::{CloseReason, MAX_TEXT_BYTES, Notification, TimeoutPolicy, Urgency},
    protocol::{self, ChangeHook, OBJECT_PATH},
};

const MAX_COMMANDS: usize = 128;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone)]
enum Command {
    Dismiss {
        generation: u64,
        key: u64,
    },
    Invoke {
        generation: u64,
        key: u64,
        action: String,
    },
    Reply {
        generation: u64,
        key: u64,
        text: String,
    },
    Forget {
        generation: u64,
        key: u64,
    },
    ClearHistory {
        generation: u64,
    },
}

impl Command {
    fn generation(&self) -> u64 {
        match self {
            Self::Dismiss { generation, .. }
            | Self::Invoke { generation, .. }
            | Self::Reply { generation, .. }
            | Self::Forget { generation, .. }
            | Self::ClearHistory { generation } => *generation,
        }
    }
}

#[derive(Default)]
struct Shared {
    commands: VecDeque<Command>,
    targets: BTreeMap<u64, BTreeSet<String>>,
    replyable: BTreeSet<u64>,
    history: BTreeSet<u64>,
    expiry_holds: BTreeSet<u64>,
    generation: u64,
    active_generation: Option<u64>,
    stop: bool,
    current: NotificationSnapshot,
    latest: Option<NotificationSnapshot>,
}

pub(crate) struct Server {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    shutdown: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub(crate) fn start(ping: Ping) -> io::Result<Self> {
        Self::start_at(None, ping)
    }

    fn start_at(address: Option<String>, ping: Ping) -> io::Result<Self> {
        let (wake_stream, reader) = UnixStream::pair()?;
        let (shutdown, shutdown_reader) = UnixStream::pair()?;
        wake_stream.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        shutdown.set_nonblocking(true)?;
        shutdown_reader.set_nonblocking(true)?;
        let change_wake = wake_stream.try_clone()?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("thing-notifications".into())
            .spawn(move || worker(address, state, reader, change_wake, shutdown_reader, ping))?;
        Ok(Self {
            shared,
            wake: wake_stream,
            shutdown,
            worker: Some(worker),
        })
    }

    pub(crate) fn take_snapshot(&self) -> Option<NotificationSnapshot> {
        lock(&self.shared).latest.take()
    }

    pub(crate) fn set_expiry_holds(&self, mut holds: BTreeSet<u64>) {
        let mut shared = lock(&self.shared);
        holds.retain(|key| shared.targets.contains_key(key));
        if shared.expiry_holds != holds {
            shared.expiry_holds = holds;
            drop(shared);
            // latest desired state has its own mailbox, so cleanup cannot be
            // dropped when the ordinary action queue is full.
            wake(&self.wake);
        }
    }

    pub(crate) fn dismiss(&self, key: u64) -> Result<(), &'static str> {
        self.send(Command::Dismiss {
            generation: self.target_generation(key, None)?,
            key,
        })
    }

    pub(crate) fn invoke(&self, key: u64, action: String) -> Result<(), &'static str> {
        let generation = self.target_generation(key, Some(&action))?;
        self.send(Command::Invoke {
            generation,
            key,
            action,
        })
    }

    pub(crate) fn reply(&self, key: u64, text: String) -> Result<(), &'static str> {
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err("a reply must be nonempty and at most 64 kib");
        }
        let generation = self.target_generation(key, None)?;
        if !lock(&self.shared).replyable.contains(&key) {
            return Err("notification does not accept a reply");
        }
        self.send(Command::Reply {
            generation,
            key,
            text,
        })
    }

    pub(crate) fn forget(&self, key: u64) -> Result<(), &'static str> {
        let generation = {
            let shared = lock(&self.shared);
            let generation = shared
                .active_generation
                .ok_or("notification server is unavailable")?;
            if !shared.history.contains(&key) {
                return Err("notification is not in the history");
            }
            generation
        };
        self.send(Command::Forget { generation, key })
    }

    pub(crate) fn clear_history(&self) -> Result<(), &'static str> {
        let generation = lock(&self.shared)
            .active_generation
            .ok_or("notification server is unavailable")?;
        self.send(Command::ClearHistory { generation })
    }

    fn target_generation(&self, key: u64, action: Option<&str>) -> Result<u64, &'static str> {
        let shared = lock(&self.shared);
        let Some(generation) = shared.active_generation else {
            return Err("notification server is unavailable");
        };
        let Some(actions) = shared.targets.get(&key) else {
            return Err("notification is no longer active");
        };
        if action.is_some_and(|action| !actions.contains(action)) {
            return Err("notification action does not exist");
        }
        Ok(generation)
    }

    fn send(&self, command: Command) -> Result<(), &'static str> {
        let mut shared = lock(&self.shared);
        if shared.active_generation != Some(command.generation()) {
            return Err("notification is no longer active");
        }
        if shared.commands.len() >= MAX_COMMANDS {
            return Err("notification command queue is full");
        }
        shared.commands.push_back(command);
        drop(shared);
        wake(&self.wake);
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        wake(&self.wake);
        wake(&self.shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker(
    address: Option<String>,
    shared: Arc<Mutex<Shared>>,
    reader: UnixStream,
    change_wake: UnixStream,
    shutdown: UnixStream,
    ping: Ping,
) {
    let mut next_key = 1_u64;
    let mut last_error = String::new();
    loop {
        let generation = {
            let mut state = lock(&shared);
            if state.stop {
                break;
            }
            state.generation.checked_add(1).inspect(|&generation| {
                state.generation = generation;
                state.active_generation = Some(generation);
            })
        };
        let Some(generation) = generation else {
            publish_unavailable(
                &shared,
                &ping,
                "notification connection generation is exhausted".into(),
            );
            break;
        };
        let result = async_io::block_on(session(
            address.as_deref(),
            SessionContext {
                shared: &shared,
                wake_stream: &reader,
                change_wake: &change_wake,
                shutdown_stream: &shutdown,
                ping: &ping,
            },
            generation,
            &mut next_key,
        ));
        {
            let mut state = lock(&shared);
            if state.active_generation == Some(generation) {
                state.active_generation = None;
            }
            state.commands.clear();
            state.targets.clear();
            state.expiry_holds.clear();
            state.replyable.clear();
            state.history.clear();
            if state.stop {
                break;
            }
        }
        let failure = result
            .err()
            .unwrap_or_else(|| Failure::Retry("notification connection ended".into()));
        let terminal = matches!(failure, Failure::NameTaken);
        let message = match failure {
            Failure::NameTaken => {
                "another notification server already owns org.freedesktop.Notifications".into()
            }
            Failure::Retry(message) => message,
        };
        if message != last_error {
            if terminal {
                eprintln!("thing: notifications unavailable: {message}");
            } else {
                eprintln!("thing: notifications unavailable: {message}; retrying in 1 second");
            }
            last_error = message.clone();
        }
        publish_unavailable(&shared, &ping, message);
        if terminal {
            break;
        }
        retry_wait(&reader, RETRY_DELAY);
    }
}

#[derive(Debug)]
enum Failure {
    NameTaken,
    Retry(String),
}

struct SessionContext<'a> {
    shared: &'a Arc<Mutex<Shared>>,
    wake_stream: &'a UnixStream,
    change_wake: &'a UnixStream,
    shutdown_stream: &'a UnixStream,
    ping: &'a Ping,
}

async fn session(
    address: Option<&str>,
    context: SessionContext<'_>,
    generation: u64,
    next_key: &mut u64,
) -> Result<(), Failure> {
    let reader = context
        .wake_stream
        .try_clone()
        .map_err(|error| Failure::Retry(error.to_string()))?;
    let wake_source = Async::new(reader).map_err(|error| Failure::Retry(error.to_string()))?;
    let shutdown_reader = context
        .shutdown_stream
        .try_clone()
        .map_err(|error| Failure::Retry(error.to_string()))?;
    let shutdown_source =
        Async::new(shutdown_reader).map_err(|error| Failure::Retry(error.to_string()))?;
    let callback_stream = context
        .change_wake
        .try_clone()
        .map_err(|error| Failure::Retry(error.to_string()))?;
    let callback_shared = context.shared.clone();
    let callback_ping = context.ping.clone();
    let changed: ChangeHook = Arc::new(move |active, history, time_origin| {
        publish_owned(
            &callback_shared,
            &callback_ping,
            generation,
            active,
            history,
            time_origin,
        );
        wake(&callback_stream);
    });
    let built = future::race(
        async {
            protocol::serve_with(address, TimeoutPolicy::default(), *next_key, changed)
                .await
                .map(Some)
                .map_err(classify)
        },
        future::race(
            async {
                wake_source
                    .readable()
                    .await
                    .map_err(|error| Failure::Retry(error.to_string()))?;
                drain(wake_source.get_ref());
                Ok::<_, Failure>(None)
            },
            async {
                Timer::after(CONNECTION_TIMEOUT).await;
                Err::<Option<_>, Failure>(Failure::Retry(
                    "notification connection timed out".into(),
                ))
            },
        ),
    )
    .await?;
    let Some((connection, notifications)) = built else {
        return if stopping(context.shared) {
            Ok(())
        } else {
            Err(Failure::Retry(
                "notification connection was interrupted".into(),
            ))
        };
    };
    let result = connection_session(
        &connection,
        &notifications,
        context.shared,
        &wake_source,
        &shutdown_source,
        context.ping,
        generation,
    )
    .await;
    *next_key = notifications.next_key();
    result
}

async fn connection_session(
    connection: &zbus::Connection,
    notifications: &protocol::Notifications,
    shared: &Mutex<Shared>,
    wake_source: &Async<UnixStream>,
    shutdown_source: &Async<UnixStream>,
    ping: &Ping,
    generation: u64,
) -> Result<(), Failure> {
    if stopping(shared) {
        return Ok(());
    }
    publish_owned(
        shared,
        ping,
        generation,
        notifications.active(),
        notifications.history(),
        notifications.time_origin(),
    );
    let emitter = SignalEmitter::new(connection, OBJECT_PATH)
        .map_err(|error| Failure::Retry(error.to_string()))?;

    loop {
        if stopping(shared) {
            return Ok(());
        }
        let holds = lock(shared).expiry_holds.clone();
        notifications.set_expiry_holds(&holds);
        let input = future::race(
            async {
                connection.closed().await;
                Input::Closed
            },
            future::race(
                async {
                    let result = wake_source.readable().await.map(|_| ());
                    Input::Wake(result)
                },
                async {
                    match notifications.next_expiry() {
                        Some(delay) => {
                            Timer::after(delay).await;
                        }
                        None => future::pending::<()>().await,
                    }
                    Input::Expire
                },
            ),
        )
        .await;
        match input {
            Input::Closed => {
                return Err(Failure::Retry("notification connection ended".into()));
            }
            Input::Wake(result) => {
                result.map_err(|error| Failure::Retry(error.to_string()))?;
                drain(wake_source.get_ref());
                if stopping(shared) {
                    continue;
                }
                for command in take_commands(shared, generation) {
                    let operation = async {
                        match command {
                            Command::Dismiss { key, .. } => {
                                notifications.dismiss(key, &emitter).await
                            }
                            Command::Invoke { key, action, .. } => {
                                notifications.invoke(key, &action, &emitter).await
                            }
                            Command::Reply { key, text, .. } => {
                                notifications.reply(key, &text, &emitter).await
                            }
                            Command::Forget { key, .. } => notifications.forget(key),
                            Command::ClearHistory { .. } => {
                                notifications.clear_history();
                                Ok(())
                            }
                        }
                    };
                    let Some(result) =
                        bounded_operation(operation, connection, shutdown_source, shared).await?
                    else {
                        return Ok(());
                    };
                    if let Err(error) = result {
                        eprintln!("thing: notification action failed: {error}");
                    }
                }
            }
            Input::Expire => {
                let Some(result) = bounded_operation(
                    notifications.expire(&emitter),
                    connection,
                    shutdown_source,
                    shared,
                )
                .await?
                else {
                    return Ok(());
                };
                if let Err(error) = result {
                    eprintln!("thing: notification expiry failed: {error}");
                }
            }
        }
    }
}

async fn bounded_operation<T>(
    operation: impl std::future::Future<Output = Result<T, String>>,
    connection: &zbus::Connection,
    shutdown_source: &Async<UnixStream>,
    shared: &Mutex<Shared>,
) -> Result<Option<Result<T, String>>, Failure> {
    future::race(
        async { Ok(Some(operation.await)) },
        future::race(
            async {
                connection.closed().await;
                Err(Failure::Retry("notification connection ended".into()))
            },
            future::race(
                async {
                    shutdown_source
                        .readable()
                        .await
                        .map_err(|error| Failure::Retry(error.to_string()))?;
                    drain(shutdown_source.get_ref());
                    if stopping(shared) {
                        Ok(None)
                    } else {
                        Err(Failure::Retry(
                            "notification operation was interrupted".into(),
                        ))
                    }
                },
                async {
                    Timer::after(CONNECTION_TIMEOUT).await;
                    Err(Failure::Retry("notification operation timed out".into()))
                },
            ),
        ),
    )
    .await
}

enum Input {
    Closed,
    Wake(io::Result<()>),
    Expire,
}

fn classify(error: zbus::Error) -> Failure {
    if matches!(error, zbus::Error::NameTaken) {
        Failure::NameTaken
    } else {
        Failure::Retry(error.to_string())
    }
}

fn take_commands(shared: &Mutex<Shared>, generation: u64) -> Vec<Command> {
    let mut shared = lock(shared);
    shared
        .commands
        .drain(..)
        .filter(|command| command.generation() == generation)
        .collect()
}

fn publish_owned(
    shared: &Mutex<Shared>,
    ping: &Ping,
    generation: u64,
    active: Vec<Notification>,
    history: Vec<Notification>,
    time_origin: Instant,
) {
    let replyable = active
        .iter()
        .filter(|notification| notification.request.can_reply)
        .map(|notification| notification.key)
        .collect();
    let history_keys = history
        .iter()
        .map(|notification| notification.key)
        .collect();
    let targets = active
        .iter()
        .map(|notification| {
            (
                notification.key,
                notification
                    .request
                    .actions
                    .iter()
                    .map(|action| action.key.clone())
                    .collect(),
            )
        })
        .collect();
    let snapshot = NotificationSnapshot {
        owned: true,
        error: String::new(),
        active: active
            .into_iter()
            .map(|record| runtime_notification(record, time_origin))
            .collect(),
        history: history
            .into_iter()
            .map(|record| runtime_notification(record, time_origin))
            .collect(),
    };
    let changed = {
        let mut shared = lock(shared);
        if shared.active_generation != Some(generation) {
            return;
        }
        shared.targets = targets;
        let active_keys: BTreeSet<_> = shared.targets.keys().copied().collect();
        shared.expiry_holds.retain(|key| active_keys.contains(key));
        shared.replyable = replyable;
        shared.history = history_keys;
        replace_snapshot(&mut shared, snapshot)
    };
    if changed {
        ping.ping();
    }
}

fn publish_unavailable(shared: &Mutex<Shared>, ping: &Ping, error: String) {
    let snapshot = NotificationSnapshot {
        error: sanitize(error, 4096),
        ..NotificationSnapshot::default()
    };
    let changed = replace_snapshot(&mut lock(shared), snapshot);
    if changed {
        ping.ping();
    }
}

fn replace_snapshot(shared: &mut Shared, snapshot: NotificationSnapshot) -> bool {
    if shared.current == snapshot {
        return false;
    }
    shared.current = snapshot.clone();
    shared.latest = Some(snapshot);
    true
}

fn runtime_notification(notification: Notification, time_origin: Instant) -> RuntimeNotification {
    // a new revision may carry a different image, so the identity includes it.
    let image = notification.request.hints.image.clone().and_then(|pixmap| {
        let candidate = ImageCandidate::new(None, vec![pixmap]).ok()?;
        ImageSource::new(
            format!(
                "notification-image:{}:{}",
                notification.key, notification.revision
            ),
            vec![candidate],
        )
        .ok()
    });
    RuntimeNotification {
        key: notification.key.to_string(),
        id: notification.id,
        revision: notification.revision,
        app_name: notification.request.app_name,
        app_icon: notification.request.app_icon,
        summary: notification.request.summary,
        body: notification.request.body,
        actions: notification
            .request
            .actions
            .into_iter()
            .map(|action| NotificationButton {
                key: action.key,
                label: action.label,
            })
            .collect(),
        urgency: match notification.urgency {
            Urgency::Low => NotificationUrgency::Low,
            Urgency::Normal => NotificationUrgency::Normal,
            Urgency::Critical => NotificationUrgency::Critical,
        },
        raw_expire_timeout_ms: notification.request.expire_timeout_ms,
        effective_timeout_ms: notification.effective_timeout_ms,
        expiry_paused: notification.expiry_paused,
        paused_remaining_ms: notification.paused_remaining_ms,
        time_origin,
        created_at_ms: notification.created_at_ms,
        updated_at_ms: notification.updated_at_ms,
        expires_at_ms: notification.expires_at_ms,
        category: notification.request.hints.category,
        desktop_entry: notification.request.hints.desktop_entry,
        image_path: notification.request.hints.image_path,
        resident: notification.request.hints.resident,
        transient: notification.request.hints.transient,
        action_icons: notification.request.hints.action_icons,
        can_reply: notification.request.can_reply,
        reply_placeholder: notification.request.hints.reply_placeholder,
        image,
        closed: notification.closed.map(|closed| {
            (
                match closed.reason {
                    CloseReason::Expired => NotificationCloseReason::Expired,
                    CloseReason::Dismissed => NotificationCloseReason::Dismissed,
                    CloseReason::ClosedByCall => NotificationCloseReason::ClosedByApplication,
                },
                closed.at_ms,
            )
        }),
    }
}

fn sanitize(mut value: String, max: usize) -> String {
    value.retain(|character| character != '\0');
    if value.len() > max {
        let mut boundary = max;
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
    let mut bytes = [0; 256];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn retry_wait(stream: &UnixStream, timeout: Duration) {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
    loop {
        // SAFETY: the descriptor points to one initialized pollfd for this call.
        let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if result >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
    drain(stream);
}

fn stopping(shared: &Mutex<Shared>) -> bool {
    lock(shared).stop
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_arguments)]

    use std::{
        collections::HashMap,
        io::{BufRead, BufReader},
        process::{Child, Command as ProcessCommand, Stdio},
        time::Instant,
    };

    use futures_util::StreamExt;
    use tempfile::TempDir;
    use zbus::zvariant::OwnedValue;

    use super::*;
    use crate::notifications::model::{
        CloseReason, MAX_ACTIVE_RETAINED_BYTES, MAX_HISTORY_RETAINED_BYTES,
        MAX_NOTIFICATION_RETAINED_BYTES, Model, NotificationRequest, request_retained_bytes,
    };

    #[test]
    fn model_retained_limits_pass_runtime_validation() {
        // only fields the runtime record also carries are filled, so each side
        // counts the same text.
        let largest = || {
            let mut request = NotificationRequest::default();
            let mut remaining =
                MAX_NOTIFICATION_RETAINED_BYTES - request_retained_bytes(&request).unwrap();
            for field in [
                &mut request.app_name,
                &mut request.app_icon,
                &mut request.summary,
                &mut request.body,
                &mut request.hints.category,
                &mut request.hints.desktop_entry,
                &mut request.hints.image_path,
                &mut request.hints.reply_placeholder,
            ] {
                let bytes = remaining.min(MAX_TEXT_BYTES);
                *field = "x".repeat(bytes).into_boxed_str().into_string();
                remaining -= bytes;
            }
            assert_eq!(remaining, 0);
            request
        };
        let mut model = Model::new(TimeoutPolicy::default());
        let closed = MAX_HISTORY_RETAINED_BYTES / MAX_NOTIFICATION_RETAINED_BYTES;
        for _ in 0..closed {
            let published = model.notify(0, largest(), 0).unwrap();
            model.dismiss(published.key, 1).unwrap();
        }
        for _ in 0..MAX_ACTIVE_RETAINED_BYTES / MAX_NOTIFICATION_RETAINED_BYTES {
            model.notify(0, largest(), 2).unwrap();
        }
        // both lists sit exactly at the model's limits.
        assert_eq!(model.history().len(), closed);
        assert!(model.notify(0, NotificationRequest::default(), 3).is_err());

        let snapshot = NotificationSnapshot {
            owned: true,
            error: String::new(),
            active: model
                .active()
                .iter()
                .cloned()
                .map(|record| runtime_notification(record, Instant::now()))
                .collect(),
            history: model
                .history()
                .iter()
                .cloned()
                .map(|record| runtime_notification(record, Instant::now()))
                .collect(),
        };
        thing_runtime::ServiceSnapshot::default()
            .set_notifications(snapshot)
            .unwrap();
    }

    #[zbus::proxy(
        interface = "org.freedesktop.Notifications",
        default_service = "org.freedesktop.Notifications",
        default_path = "/org/freedesktop/Notifications"
    )]
    trait TestNotifications {
        fn notify(
            &self,
            app_name: &str,
            replaces_id: u32,
            app_icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<String>,
            hints: HashMap<String, OwnedValue>,
            expire_timeout: i32,
        ) -> zbus::Result<u32>;

        fn close_notification(&self, id: u32) -> zbus::Result<()>;

        #[zbus(signal)]
        fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;

        #[zbus(signal)]
        fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;
    }

    #[test]
    fn worker_publishes_actions_dismissals_and_expiry() {
        let mut bus = Bus::start();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let server = Server::start_at(Some(bus.address.clone()), ping).unwrap();
        wait_snapshot(&server, |snapshot| snapshot.owned);

        async_io::block_on(async {
            let client = connect(&bus.address).await;
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();
            let mut invoked = proxy.receive_action_invoked().await.unwrap();
            let mut closed = proxy.receive_notification_closed().await.unwrap();
            let id = notify(&proxy, "action", vec!["open".into(), "open it".into()], 0).await;
            let active = wait_snapshot(&server, |snapshot| !snapshot.active.is_empty());
            let key: u64 = active.active[0].key.parse().unwrap();
            let replacement_id = proxy
                .notify(
                    "test app",
                    id,
                    "",
                    "replacement",
                    "",
                    vec!["save".into(), "save it".into()],
                    HashMap::new(),
                    0,
                )
                .await
                .unwrap();
            assert_eq!(replacement_id, id);
            let replacement = wait_snapshot(&server, |snapshot| {
                snapshot
                    .active
                    .first()
                    .is_some_and(|notification| notification.revision == 2)
            });
            assert_eq!(replacement.active[0].key, key.to_string());
            assert_eq!(replacement.active[0].summary, "replacement");
            assert_eq!(replacement.active[0].actions[0].key, "save");
            assert_eq!(
                server.invoke(key, "open".into()),
                Err("notification action does not exist")
            );
            server.invoke(key, "save".into()).unwrap();
            let action_signal = invoked.next().await.unwrap();
            let action = action_signal.args().unwrap();
            assert_eq!(*action.id(), id);
            assert_eq!(*action.action_key(), "save");
            let close_signal = closed.next().await.unwrap();
            let close = close_signal.args().unwrap();
            assert_eq!(*close.id(), id);
            assert_eq!(*close.reason(), CloseReason::Dismissed.code());
            wait_snapshot(&server, |snapshot| snapshot.active.is_empty());

            let id = notify(&proxy, "client close", Vec::new(), 0).await;
            wait_snapshot(&server, |snapshot| !snapshot.active.is_empty());
            proxy.close_notification(id).await.unwrap();
            let close_signal = closed.next().await.unwrap();
            let close = close_signal.args().unwrap();
            assert_eq!(*close.id(), id);
            assert_eq!(*close.reason(), CloseReason::ClosedByCall.code());
            wait_snapshot(&server, |snapshot| snapshot.active.is_empty());

            let id = notify(&proxy, "dismiss", Vec::new(), 0).await;
            let active = wait_snapshot(&server, |snapshot| !snapshot.active.is_empty());
            server
                .dismiss(active.active[0].key.parse().unwrap())
                .unwrap();
            let close_signal = closed.next().await.unwrap();
            let close = close_signal.args().unwrap();
            assert_eq!(*close.id(), id);
            assert_eq!(*close.reason(), CloseReason::Dismissed.code());

            let id = notify(&proxy, "expiry", Vec::new(), 10).await;
            let close_signal = closed.next().await.unwrap();
            let close = close_signal.args().unwrap();
            assert_eq!(*close.id(), id);
            assert_eq!(*close.reason(), CloseReason::Expired.code());
            wait_snapshot(&server, |snapshot| snapshot.active.is_empty());
        });
        drop(server);
        bus.stop();
    }

    #[test]
    fn worker_publishes_history_and_routes_replies() {
        let mut bus = Bus::start();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let server = Server::start_at(Some(bus.address.clone()), ping).unwrap();
        wait_snapshot(&server, |snapshot| snapshot.owned);

        async_io::block_on(async {
            let client = connect(&bus.address).await;
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();
            let mut closed = proxy.receive_notification_closed().await.unwrap();
            notify(&proxy, "plain", Vec::new(), 0).await;
            let active = wait_snapshot(&server, |snapshot| !snapshot.active.is_empty());
            let plain: u64 = active.active[0].key.parse().unwrap();
            assert!(!active.active[0].can_reply);
            assert_eq!(
                server.reply(plain, "hi".into()),
                Err("notification does not accept a reply")
            );
            assert_eq!(
                server.forget(plain),
                Err("notification is not in the history")
            );

            notify(
                &proxy,
                "chat",
                vec!["inline-reply".into(), "Reply".into()],
                0,
            )
            .await;
            let active = wait_snapshot(&server, |snapshot| snapshot.active.len() == 2);
            let chat: u64 = active.active[0].key.parse().unwrap();
            assert!(active.active[0].can_reply && active.active[0].actions.is_empty());
            assert_eq!(
                server.reply(chat, String::new()),
                Err("a reply must be nonempty and at most 64 kib")
            );
            server.reply(chat, "on my way".into()).unwrap();
            closed.next().await.unwrap();
            server.dismiss(plain).unwrap();
            closed.next().await.unwrap();

            let snapshot = wait_snapshot(&server, |snapshot| {
                snapshot.active.is_empty() && snapshot.history.len() == 2
            });
            let keys: Vec<_> = snapshot
                .history
                .iter()
                .map(|record| record.key.clone())
                .collect();
            assert_eq!(keys, [plain.to_string(), chat.to_string()]);
            assert!(snapshot.history.iter().all(|record| matches!(
                record.closed,
                Some((NotificationCloseReason::Dismissed, _))
            )));
            assert_eq!(
                server.reply(chat, "late".into()),
                Err("notification is no longer active")
            );

            server.forget(plain).unwrap();
            wait_snapshot(&server, |snapshot| snapshot.history.len() == 1);
            server.clear_history().unwrap();
            wait_snapshot(&server, |snapshot| snapshot.history.is_empty());
        });
        drop(server);
        bus.stop();
    }

    #[test]
    fn worker_holds_replacement_expiry_until_released() {
        let mut bus = Bus::start();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let server = Server::start_at(Some(bus.address.clone()), ping).unwrap();
        wait_snapshot(&server, |snapshot| snapshot.owned);
        async_io::block_on(async {
            let client = connect(&bus.address).await;
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();
            let mut closed = proxy.receive_notification_closed().await.unwrap();
            let id = notify(&proxy, "timed", vec!["default".into(), "open".into()], 3000).await;
            let initial = wait_snapshot(&server, |snapshot| !snapshot.active.is_empty())
                .active
                .remove(0);
            let key = initial.key.parse().unwrap();
            server.set_expiry_holds(BTreeSet::from([key]));
            let paused = wait_snapshot(&server, |snapshot| {
                snapshot
                    .active
                    .first()
                    .is_some_and(|record| record.expiry_paused)
            });
            let remaining = paused.active[0].paused_remaining_ms.unwrap();
            assert!((1..=3000).contains(&remaining));
            assert_eq!(paused.active[0].revision, 1);
            assert_eq!(paused.active[0].expires_at_ms, None);
            // reaching the original deadline must not invalidate the live record.
            Timer::at(
                initial.time_origin + Duration::from_millis(initial.expires_at_ms.unwrap() + 50),
            )
            .await;
            assert!(server.invoke(key, "missing".into()).is_err());
            assert!(lock(&server.shared).targets.contains_key(&key));
            assert_eq!(
                proxy
                    .notify(
                        "test app",
                        id,
                        "",
                        "replacement",
                        "",
                        vec![],
                        HashMap::new(),
                        500
                    )
                    .await
                    .unwrap(),
                id
            );
            let replacement = wait_snapshot(&server, |snapshot| {
                snapshot
                    .active
                    .first()
                    .is_some_and(|record| record.revision == 2)
            });
            assert_eq!(replacement.active[0].key, initial.key);
            assert!(replacement.active[0].expiry_paused);
            assert_eq!(replacement.active[0].paused_remaining_ms, Some(500));
            server.set_expiry_holds(BTreeSet::new());
            let resumed = wait_snapshot(&server, |snapshot| {
                snapshot
                    .active
                    .first()
                    .is_some_and(|record| !record.expiry_paused)
            });
            assert!(resumed.active[0].remaining_at(Instant::now()).unwrap() <= 500);
            assert!(
                resumed.active[0].expires_at_ms.unwrap() >= resumed.active[0].updated_at_ms + 500
            );
            let signal = future::race(async { closed.next().await.unwrap() }, async {
                Timer::after(Duration::from_secs(5)).await;
                panic!("resumed notification never expired");
            })
            .await;
            let args = signal.args().unwrap();
            assert_eq!(
                (*args.id(), *args.reason()),
                (id, CloseReason::Expired.code())
            );
            let expired = wait_snapshot(&server, |snapshot| snapshot.active.is_empty());
            assert_eq!(expired.history[0].key, initial.key);
            assert!(lock(&server.shared).expiry_holds.is_empty());
            server.set_expiry_holds(BTreeSet::from([key]));
            assert!(lock(&server.shared).expiry_holds.is_empty());
        });
        drop(server);
        bus.stop();
    }

    #[test]
    fn name_conflict_is_terminal_for_the_running_server() {
        let mut bus = Bus::start();
        async_io::block_on(async {
            let (owner, _) = protocol::serve(&bus.address, TimeoutPolicy::default())
                .await
                .unwrap();
            let observer = connect(&bus.address).await;
            let (ping, _source) = calloop::ping::make_ping().unwrap();
            let server = Server::start_at(Some(bus.address.clone()), ping).unwrap();
            let snapshot = wait_snapshot(&server, |snapshot| !snapshot.error.is_empty());
            assert!(!snapshot.owned);
            assert!(snapshot.error.contains("already owns"));

            owner.close().await.unwrap();
            let dbus = zbus::fdo::DBusProxy::new(&observer).await.unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while dbus
                .name_has_owner(protocol::BUS_NAME.try_into().unwrap())
                .await
                .unwrap()
            {
                assert!(Instant::now() < deadline, "name release timed out");
                Timer::after(Duration::from_millis(5)).await;
            }
            std::thread::sleep(Duration::from_millis(1_200));
            assert!(
                !dbus
                    .name_has_owner(protocol::BUS_NAME.try_into().unwrap())
                    .await
                    .unwrap()
            );
            assert!(server.take_snapshot().is_none());
            drop(server);
        });
        bus.stop();
    }

    #[test]
    fn reconnect_keeps_lifetime_keys_monotonic_and_rejects_stale_actions() {
        let mut bus = Bus::start();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let server = Server::start_at(Some(bus.address.clone()), ping).unwrap();
        wait_snapshot(&server, |snapshot| snapshot.owned);
        let first_key = async_io::block_on(async {
            let client = connect(&bus.address).await;
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();
            notify(&proxy, "before restart", Vec::new(), 0).await;
            wait_snapshot(&server, |snapshot| !snapshot.active.is_empty()).active[0]
                .key
                .parse::<u64>()
                .unwrap()
        });

        server.set_expiry_holds(BTreeSet::from([first_key]));
        wait_snapshot(&server, |snapshot| {
            snapshot
                .active
                .first()
                .is_some_and(|record| record.expiry_paused)
        });

        bus.stop();
        wait_snapshot(&server, |snapshot| {
            !snapshot.owned && !snapshot.error.is_empty()
        });
        bus.restart();
        wait_snapshot(&server, |snapshot| snapshot.owned);
        let second_key = async_io::block_on(async {
            let client = connect(&bus.address).await;
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();
            notify(&proxy, "after restart", Vec::new(), 0).await;
            wait_snapshot(&server, |snapshot| !snapshot.active.is_empty()).active[0]
                .key
                .parse::<u64>()
                .unwrap()
        });
        assert!(lock(&server.shared).expiry_holds.is_empty());
        assert!(!lock(&server.shared).current.active[0].expiry_paused);
        assert!(second_key > first_key);
        assert_eq!(
            server.dismiss(first_key),
            Err("notification is no longer active")
        );
        drop(server);
        bus.stop();
    }

    #[test]
    fn drop_interrupts_a_connection_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", directory.path().join("missing").display());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let started = Instant::now();
        drop(Server::start_at(Some(address), ping).unwrap());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn shutdown_interrupts_a_stalled_established_operation() {
        let mut bus = Bus::start();
        async_io::block_on(async {
            let connection = connect(&bus.address).await;
            let (shutdown, reader) = UnixStream::pair().unwrap();
            shutdown.set_nonblocking(true).unwrap();
            reader.set_nonblocking(true).unwrap();
            let shutdown_source = Async::new(reader).unwrap();
            let shared = Arc::new(Mutex::new(Shared::default()));
            let state = shared.clone();
            let trigger = thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(10));
                lock(&state).stop = true;
                wake(&shutdown);
            });
            let started = Instant::now();
            let result = bounded_operation(
                future::pending::<Result<(), String>>(),
                &connection,
                &shutdown_source,
                &shared,
            )
            .await
            .unwrap();
            trigger.join().unwrap();
            assert!(result.is_none());
            assert!(started.elapsed() < Duration::from_secs(1));
        });
        bus.stop();
    }

    #[test]
    fn command_queue_is_bounded() {
        let (wake_stream, wake_reader) = UnixStream::pair().unwrap();
        let (shutdown, shutdown_reader) = UnixStream::pair().unwrap();
        wake_stream.set_nonblocking(true).unwrap();
        shutdown.set_nonblocking(true).unwrap();
        let server = Server {
            shared: Arc::new(Mutex::new(Shared::default())),
            wake: wake_stream,
            shutdown,
            worker: None,
        };
        {
            let mut shared = lock(&server.shared);
            shared.active_generation = Some(7);
            shared.targets.insert(1, BTreeSet::new());
            for _ in 0..MAX_COMMANDS {
                shared.commands.push_back(Command::Dismiss {
                    generation: 7,
                    key: 1,
                });
            }
        }
        assert_eq!(server.dismiss(1), Err("notification command queue is full"));
        server.set_expiry_holds(BTreeSet::from([1, 2]));
        assert_eq!(lock(&server.shared).expiry_holds, BTreeSet::from([1]));
        server.set_expiry_holds(BTreeSet::new());
        assert!(lock(&server.shared).expiry_holds.is_empty());
        assert_eq!(lock(&server.shared).commands.len(), MAX_COMMANDS);
        drop(server);
        drop(wake_reader);
        drop(shutdown_reader);
    }

    async fn connect(address: &str) -> zbus::Connection {
        zbus::connection::Builder::address(address)
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    async fn notify(
        proxy: &TestNotificationsProxy<'_>,
        summary: &str,
        actions: Vec<String>,
        timeout: i32,
    ) -> u32 {
        proxy
            .notify(
                "test app",
                0,
                "",
                summary,
                "",
                actions,
                HashMap::new(),
                timeout,
            )
            .await
            .unwrap()
    }

    fn wait_snapshot(
        server: &Server,
        predicate: impl Fn(&NotificationSnapshot) -> bool,
    ) -> NotificationSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(snapshot) = server.take_snapshot()
                && predicate(&snapshot)
            {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "notification snapshot timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct Bus {
        address: String,
        child: Option<Child>,
        _directory: TempDir,
    }

    impl Bus {
        fn start() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let address = format!("unix:path={}", directory.path().join("bus").display());
            let mut bus = Self {
                address,
                child: None,
                _directory: directory,
            };
            bus.restart();
            bus
        }

        fn restart(&mut self) {
            assert!(self.child.is_none());
            let mut child = ProcessCommand::new("dbus-daemon")
                .args([
                    "--session",
                    "--nofork",
                    "--nopidfile",
                    "--print-address=1",
                    &format!("--address={}", self.address),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("dbus-daemon must be installed for notification tests");
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let mut ready = String::new();
            output.read_line(&mut ready).unwrap();
            assert!(ready.trim().starts_with(&self.address));
            self.child = Some(child);
        }

        fn stop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            self.stop();
        }
    }
}
