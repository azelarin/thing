use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use zbus::{Connection, fdo, message::Header, object_server::SignalEmitter, zvariant::OwnedValue};

use super::model::{
    CloseReason, Model, ModelError, Notification, NotificationAction, NotificationHints,
    NotificationRequest, TimeoutPolicy,
};

pub(crate) const BUS_NAME: &str = "org.freedesktop.Notifications";
pub(crate) const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
const MAX_QUEUED_MESSAGES: usize = 4;
const MAX_NOTIFY_BODY_BYTES: u32 = 1024 * 1024;
const MAX_HINTS: usize = 64;
const MAX_RAW_ACTIONS: usize = 256;

pub(crate) type ChangeHook = Arc<dyn Fn(Vec<Notification>) + Send + Sync>;

struct State {
    model: Mutex<Model>,
    started: Instant,
    changed: ChangeHook,
}

/// the freedesktop notification interface without host event-loop policy.
#[derive(Clone)]
pub(crate) struct Notifications {
    state: Arc<State>,
}

impl Notifications {
    pub(crate) fn with_state(
        timeout_policy: TimeoutPolicy,
        next_key: u64,
        changed: ChangeHook,
    ) -> Self {
        Self {
            state: Arc::new(State {
                model: Mutex::new(Model::with_next_key(timeout_policy, next_key)),
                started: Instant::now(),
                changed,
            }),
        }
    }

    pub(crate) fn active(&self) -> Vec<Notification> {
        lock(&self.state.model).active().to_vec()
    }

    pub(crate) fn next_key(&self) -> u64 {
        lock(&self.state.model).next_key()
    }

    pub(crate) fn next_expiry(&self) -> Option<Duration> {
        let now = self.now_ms();
        lock(&self.state.model)
            .next_expiry_ms()
            .map(|deadline| Duration::from_millis(deadline.saturating_sub(now)))
    }

    pub(crate) async fn dismiss(
        &self,
        key: u64,
        emitter: &SignalEmitter<'_>,
    ) -> Result<(), String> {
        let notification = lock(&self.state.model)
            .dismiss(key, self.now_ms())
            .map_err(model_message)?;
        self.publish();
        Self::notification_closed(emitter, notification.id, CloseReason::Dismissed.code())
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn invoke(
        &self,
        key: u64,
        action_key: &str,
        emitter: &SignalEmitter<'_>,
    ) -> Result<(), String> {
        let invoked = lock(&self.state.model)
            .invoke(key, action_key, self.now_ms())
            .map_err(model_message)?;
        self.publish();
        Self::action_invoked(emitter, invoked.id, &invoked.action_key)
            .await
            .map_err(|error| error.to_string())?;
        if invoked.closed {
            Self::notification_closed(emitter, invoked.id, CloseReason::Dismissed.code())
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) async fn expire(&self, emitter: &SignalEmitter<'_>) -> Result<usize, String> {
        let expired = lock(&self.state.model).expire(self.now_ms());
        if !expired.is_empty() {
            self.publish();
        }
        for notification in &expired {
            Self::notification_closed(emitter, notification.id, CloseReason::Expired.code())
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(expired.len())
    }

    fn now_ms(&self) -> u64 {
        self.state
            .started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    fn publish(&self) {
        (self.state.changed)(self.active());
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    fn get_capabilities(&self) -> Vec<String> {
        vec!["actions".into(), "body".into()]
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        #[zbus(header)] header: Header<'_>,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> fdo::Result<u32> {
        // zbus has already received and deserialized the message when this runs.
        // this rejects oversized requests before they enter retained state; the
        // transport's separate 128 MiB message limit remains the allocation bound.
        if header.primary().body_len() > MAX_NOTIFY_BODY_BYTES {
            return Err(fdo::Error::LimitsExceeded(
                "notification request body is too large".into(),
            ));
        }
        if hints.len() > MAX_HINTS {
            return Err(fdo::Error::LimitsExceeded(
                "notification request has too many hints".into(),
            ));
        }
        let actions = parse_actions(actions)?;
        let hints = parse_hints(&hints);
        let request = NotificationRequest {
            app_name,
            app_icon,
            summary,
            body,
            actions,
            hints,
            expire_timeout_ms: expire_timeout,
        };
        let id = lock(&self.state.model)
            .notify(replaces_id, request, self.now_ms())
            .map(|published| published.id)
            .map_err(model_error)?;
        self.publish();
        Ok(id)
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let notification = lock(&self.state.model)
            .close_by_id(id, CloseReason::ClosedByCall, self.now_ms())
            .map_err(|error| match error {
                ModelError::UnknownNotification => fdo::Error::Failed(String::new()),
                error => model_error(error),
            })?;
        self.publish();
        Self::notification_closed(&emitter, notification.id, CloseReason::ClosedByCall.code())
            .await?;
        Ok(())
    }

    #[zbus(out_args("name", "vendor", "version", "spec_version"))]
    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "thing".into(),
            "thing".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.3".into(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn activation_token(
        emitter: &SignalEmitter<'_>,
        id: u32,
        activation_token: &str,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
pub(crate) async fn serve(
    address: &str,
    timeout_policy: TimeoutPolicy,
) -> zbus::Result<(Connection, Notifications)> {
    serve_with(Some(address), timeout_policy, 1, Arc::new(|_| {})).await
}

pub(crate) async fn serve_with(
    address: Option<&str>,
    timeout_policy: TimeoutPolicy,
    next_key: u64,
    changed: ChangeHook,
) -> zbus::Result<(Connection, Notifications)> {
    let notifications = Notifications::with_state(timeout_policy, next_key, changed);
    let builder = match address {
        Some(address) => zbus::connection::Builder::address(address),
        None => zbus::connection::Builder::session(),
    }?;
    let connection = builder
        .max_queued(MAX_QUEUED_MESSAGES)
        .serve_at(OBJECT_PATH, notifications.clone())?
        .name(BUS_NAME)?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .build()
        .await?;
    Ok((connection, notifications))
}

fn parse_actions(actions: Vec<String>) -> fdo::Result<Vec<NotificationAction>> {
    if actions.len() > MAX_RAW_ACTIONS {
        return Err(fdo::Error::LimitsExceeded(
            "notification request has too many actions".into(),
        ));
    }
    if actions.len() % 2 != 0 {
        return Err(fdo::Error::InvalidArgs(
            "notification actions must be key/label pairs".into(),
        ));
    }
    let mut actions = actions.into_iter();
    let mut parsed = Vec::new();
    while let Some(key) = actions.next() {
        let label = actions
            .next()
            .expect("the even action count was checked above");
        parsed.push(NotificationAction { key, label });
    }
    Ok(parsed)
}

fn parse_hints(hints: &HashMap<String, OwnedValue>) -> NotificationHints {
    let x = hint_i32(hints, "x");
    let y = hint_i32(hints, "y");
    let (x, y) = if x.is_some() && y.is_some() {
        (x, y)
    } else {
        (None, None)
    };
    NotificationHints {
        urgency: hint_u8(hints, "urgency"),
        category: hint_string(hints, "category"),
        desktop_entry: hint_string(hints, "desktop-entry"),
        image_path: {
            let current = hint_string(hints, "image-path");
            if current.is_empty() {
                hint_string(hints, "image_path")
            } else {
                current
            }
        },
        resident: hint_bool(hints, "resident"),
        transient: hint_bool(hints, "transient"),
        action_icons: hint_bool(hints, "action-icons"),
        sound_file: hint_string(hints, "sound-file"),
        sound_name: hint_string(hints, "sound-name"),
        suppress_sound: hint_bool(hints, "suppress-sound"),
        x,
        y,
    }
}

fn hint_string(hints: &HashMap<String, OwnedValue>, name: &str) -> String {
    hints
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok())
        .unwrap_or_default()
        .to_owned()
}

fn hint_bool(hints: &HashMap<String, OwnedValue>, name: &str) -> bool {
    hints
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

fn hint_u8(hints: &HashMap<String, OwnedValue>, name: &str) -> Option<u8> {
    hints.get(name).and_then(|value| u8::try_from(value).ok())
}

fn hint_i32(hints: &HashMap<String, OwnedValue>, name: &str) -> Option<i32> {
    hints.get(name).and_then(|value| i32::try_from(value).ok())
}

fn model_error(error: ModelError) -> fdo::Error {
    match error {
        ModelError::InvalidRequest | ModelError::InvalidTimeout => {
            fdo::Error::InvalidArgs(model_message(error))
        }
        ModelError::Capacity => fdo::Error::LimitsExceeded(model_message(error)),
        ModelError::UnknownNotification | ModelError::UnknownAction => {
            fdo::Error::Failed(model_message(error))
        }
    }
}

fn model_message(error: ModelError) -> String {
    match error {
        ModelError::InvalidRequest => "invalid notification request",
        ModelError::InvalidTimeout => "notification timeout must be -1, 0, or positive",
        ModelError::Capacity => "notification capacity is exhausted",
        ModelError::UnknownNotification => "notification is no longer active",
        ModelError::UnknownAction => "notification action does not exist",
    }
    .into()
}

fn lock(model: &Mutex<Model>) -> MutexGuard<'_, Model> {
    model
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_arguments)]

    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command as ProcessCommand, Stdio},
    };

    use futures_util::StreamExt;
    use tempfile::TempDir;
    use zbus::zvariant::Value;

    use super::*;

    #[zbus::proxy(
        interface = "org.freedesktop.Notifications",
        default_service = "org.freedesktop.Notifications",
        default_path = "/org/freedesktop/Notifications"
    )]
    trait TestNotifications {
        fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

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

        fn get_server_information(&self) -> zbus::Result<(String, String, String, String)>;
    }

    #[test]
    fn protocol_lifecycle_uses_replacement_ids_and_standard_signals() {
        let mut bus = Bus::start();
        async_io::block_on(async {
            let (server_connection, server) =
                serve(&bus.address, TimeoutPolicy::default()).await.unwrap();
            let client = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();

            assert_eq!(proxy.get_capabilities().await.unwrap(), ["actions", "body"]);
            assert_eq!(
                proxy.get_server_information().await.unwrap(),
                (
                    "thing".into(),
                    "thing".into(),
                    env!("CARGO_PKG_VERSION").into(),
                    "1.3".into()
                )
            );
            let hints = HashMap::from([
                ("urgency".into(), owned(Value::from(0u8))),
                ("desktop-entry".into(), owned(Value::from("thing-test"))),
                ("unknown".into(), owned(Value::from(99u32))),
            ]);
            let id = proxy
                .notify(
                    "test app",
                    0,
                    "test-icon",
                    "first",
                    "body",
                    vec!["default".into(), "open".into()],
                    hints,
                    -1,
                )
                .await
                .unwrap();
            let first = server.active().pop().unwrap();
            assert_eq!(first.id, id);
            assert_eq!(first.revision, 1);
            assert_eq!(first.request.expire_timeout_ms, -1);
            assert_eq!(first.effective_timeout_ms, Some(5_000));
            assert_eq!(first.request.hints.desktop_entry, "thing-test");

            let replacement_id = proxy
                .notify(
                    "test app",
                    id,
                    "test-icon",
                    "replacement",
                    "new body",
                    vec!["default".into(), "open".into()],
                    HashMap::new(),
                    0,
                )
                .await
                .unwrap();
            let replacement = server.active().pop().unwrap();
            assert_eq!(replacement_id, id);
            assert_eq!(replacement.key, first.key);
            assert_eq!(replacement.revision, 2);
            assert_eq!(replacement.request.expire_timeout_ms, 0);
            assert_eq!(replacement.effective_timeout_ms, None);

            let mut closed = proxy.receive_notification_closed().await.unwrap();
            proxy.close_notification(id).await.unwrap();
            let signal = closed.next().await.unwrap();
            let args = signal.args().unwrap();
            assert_eq!(*args.id(), id);
            assert_eq!(*args.reason(), CloseReason::ClosedByCall.code());
            assert!(server.active().is_empty());
            assert!(proxy.close_notification(id).await.is_err());

            drop(server_connection);
        });
        bus.stop();
    }

    #[test]
    fn user_actions_emit_action_before_close_and_resident_actions_stay_active() {
        let mut bus = Bus::start();
        async_io::block_on(async {
            let (server_connection, server) =
                serve(&bus.address, TimeoutPolicy::default()).await.unwrap();
            let client = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();
            let id = proxy
                .notify(
                    "test app",
                    0,
                    "",
                    "action",
                    "",
                    vec!["open".into(), "open it".into()],
                    HashMap::new(),
                    0,
                )
                .await
                .unwrap();
            let key = server.active()[0].key;
            let mut invoked = proxy.receive_action_invoked().await.unwrap();
            let mut closed = proxy.receive_notification_closed().await.unwrap();
            let emitter = SignalEmitter::new(&server_connection, OBJECT_PATH).unwrap();
            server.invoke(key, "open", &emitter).await.unwrap();
            let signal = invoked.next().await.unwrap();
            let action = signal.args().unwrap();
            assert_eq!(*action.id(), id);
            assert_eq!(*action.action_key(), "open");
            let signal = closed.next().await.unwrap();
            let close = signal.args().unwrap();
            assert_eq!(*close.id(), id);
            assert_eq!(*close.reason(), CloseReason::Dismissed.code());

            let hints = HashMap::from([("resident".into(), owned(Value::from(true)))]);
            let resident_id = proxy
                .notify(
                    "test app",
                    0,
                    "",
                    "resident",
                    "",
                    vec!["open".into(), "open it".into()],
                    hints,
                    0,
                )
                .await
                .unwrap();
            let key = server.active()[0].key;
            server.invoke(key, "open", &emitter).await.unwrap();
            let signal = invoked.next().await.unwrap();
            let action = signal.args().unwrap();
            assert_eq!(*action.id(), resident_id);
            assert_eq!(server.active().len(), 1);
        });
        bus.stop();
    }

    #[test]
    fn shell_dismissal_and_expiry_emit_their_standard_reasons() {
        let mut bus = Bus::start();
        async_io::block_on(async {
            let (server_connection, server) =
                serve(&bus.address, TimeoutPolicy::default()).await.unwrap();
            let client = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();
            let mut closed = proxy.receive_notification_closed().await.unwrap();
            let emitter = SignalEmitter::new(&server_connection, OBJECT_PATH).unwrap();

            let dismissed_id = proxy
                .notify(
                    "test app",
                    0,
                    "",
                    "dismissed",
                    "",
                    Vec::new(),
                    HashMap::new(),
                    0,
                )
                .await
                .unwrap();
            let key = server.active()[0].key;
            server.dismiss(key, &emitter).await.unwrap();
            let signal = closed.next().await.unwrap();
            let args = signal.args().unwrap();
            assert_eq!(*args.id(), dismissed_id);
            assert_eq!(*args.reason(), CloseReason::Dismissed.code());

            let expired_id = proxy
                .notify(
                    "test app",
                    0,
                    "",
                    "expired",
                    "",
                    Vec::new(),
                    HashMap::new(),
                    1,
                )
                .await
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
            assert_eq!(server.expire(&emitter).await.unwrap(), 1);
            let signal = closed.next().await.unwrap();
            let args = signal.args().unwrap();
            assert_eq!(*args.id(), expired_id);
            assert_eq!(*args.reason(), CloseReason::Expired.code());
            assert!(server.active().is_empty());
        });
        bus.stop();
    }

    #[test]
    fn ownership_never_queues_or_replaces_an_existing_server() {
        let mut bus = Bus::start();
        async_io::block_on(async {
            let (first, _) = serve(&bus.address, TimeoutPolicy::default()).await.unwrap();
            let owner = zbus::fdo::DBusProxy::new(&first)
                .await
                .unwrap()
                .get_name_owner(BUS_NAME.try_into().unwrap())
                .await
                .unwrap();

            let error = match serve(&bus.address, TimeoutPolicy::default()).await {
                Ok(_) => panic!("a second server unexpectedly acquired the notification name"),
                Err(error) => error,
            };
            assert!(matches!(error, zbus::Error::NameTaken));
            let current = zbus::fdo::DBusProxy::new(&first)
                .await
                .unwrap()
                .get_name_owner(BUS_NAME.try_into().unwrap())
                .await
                .unwrap();
            assert_eq!(current, owner);
        });
        bus.stop();
    }

    #[test]
    fn malformed_actions_and_timeouts_return_errors_without_publishing() {
        let mut bus = Bus::start();
        async_io::block_on(async {
            let (_connection, server) =
                serve(&bus.address, TimeoutPolicy::default()).await.unwrap();
            let client = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = TestNotificationsProxy::new(&client).await.unwrap();

            assert!(
                proxy
                    .notify(
                        "test",
                        0,
                        "",
                        "bad actions",
                        "",
                        vec!["missing label".into()],
                        HashMap::new(),
                        -1,
                    )
                    .await
                    .is_err()
            );
            assert!(
                proxy
                    .notify(
                        "test",
                        0,
                        "",
                        "bad timeout",
                        "",
                        Vec::new(),
                        HashMap::new(),
                        -2,
                    )
                    .await
                    .is_err()
            );
            assert!(
                proxy
                    .notify(
                        "test",
                        0,
                        "",
                        "too many actions",
                        "",
                        (0..=MAX_RAW_ACTIONS)
                            .map(|index| index.to_string())
                            .collect(),
                        HashMap::new(),
                        0,
                    )
                    .await
                    .is_err()
            );
            let hints = (0..=MAX_HINTS)
                .map(|index| (format!("hint-{index}"), owned(Value::from(index as u32))))
                .collect();
            assert!(
                proxy
                    .notify("test", 0, "", "too many hints", "", Vec::new(), hints, 0,)
                    .await
                    .is_err()
            );
            let oversized = "x".repeat(MAX_NOTIFY_BODY_BYTES as usize);
            let error = proxy
                .notify(
                    "test",
                    0,
                    "",
                    "oversized body",
                    &oversized,
                    Vec::new(),
                    HashMap::new(),
                    0,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                zbus::Error::MethodError(name, _, _)
                    if name.as_str() == "org.freedesktop.DBus.Error.LimitsExceeded"
            ));
            assert!(server.active().is_empty());
        });
        bus.stop();
    }

    fn owned(value: Value<'_>) -> OwnedValue {
        value.try_into().unwrap()
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
            let mut child = ProcessCommand::new("dbus-daemon")
                .args([
                    "--session",
                    "--nofork",
                    "--nopidfile",
                    "--print-address=1",
                    &format!("--address={address}"),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("dbus-daemon must be installed for notification tests");
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let mut ready = String::new();
            output.read_line(&mut ready).unwrap();
            assert!(ready.trim().starts_with(&address));
            Self {
                address,
                child: Some(child),
                _directory: directory,
            }
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
