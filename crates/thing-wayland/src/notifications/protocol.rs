use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use thing_core::ImagePixmap;
use zbus::{
    Connection, fdo,
    message::Header,
    names::OwnedUniqueName,
    object_server::SignalEmitter,
    zvariant::{OwnedValue, Structure},
};

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
/// the action key that asks for an inline reply field, from the kde extension
/// that telegram, discord, and other clients use.
const INLINE_REPLY: &str = "inline-reply";
// larger images are downscaled, which bounds retained memory per notification.
const MAX_IMAGE_EDGE: u32 = 256;
const MAX_SOURCE_IMAGE_EDGE: i32 = 8192;

/// receives the active notifications and the history, newest first.
pub(crate) type ChangeHook =
    Arc<dyn Fn(Vec<Notification>, Vec<Notification>, Instant) + Send + Sync>;

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

    pub(crate) fn history(&self) -> Vec<Notification> {
        lock(&self.state.model).history().iter().cloned().collect()
    }

    pub(crate) fn time_origin(&self) -> Instant {
        self.state.started
    }

    pub(crate) fn set_expiry_holds(&self, holds: &BTreeSet<u64>) {
        if lock(&self.state.model).set_expiry_holds(holds, self.now_ms()) {
            self.publish();
        }
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

    pub(crate) async fn reply(
        &self,
        key: u64,
        text: &str,
        emitter: &SignalEmitter<'_>,
    ) -> Result<(), String> {
        let replied = lock(&self.state.model)
            .reply(key, text, self.now_ms())
            .map_err(model_message)?;
        self.publish();
        // unlike the close signal, the reply holds the user's text, so only the
        // client that sent the notification receives it.
        let client = emitter.clone().set_destination(replied.sender.into());
        Self::notification_replied(&client, replied.id, text)
            .await
            .map_err(|error| error.to_string())?;
        if replied.closed {
            Self::notification_closed(emitter, replied.id, CloseReason::Dismissed.code())
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn forget(&self, key: u64) -> Result<(), String> {
        lock(&self.state.model).forget(key).map_err(model_message)?;
        self.publish();
        Ok(())
    }

    pub(crate) fn clear_history(&self) {
        if lock(&self.state.model).clear_history() {
            self.publish();
        }
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
        let (active, history) = {
            let model = lock(&self.state.model);
            (
                model.active().to_vec(),
                model.history().iter().cloned().collect(),
            )
        };
        (self.state.changed)(active, history, self.state.started);
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    fn get_capabilities(&self) -> Vec<String> {
        vec![
            "actions".into(),
            "body".into(),
            "icon-static".into(),
            INLINE_REPLY.into(),
        ]
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
        let mut actions = parse_actions(actions)?;
        // a message bus always names the sender. without one, a reply could only
        // be broadcast, so the reply field is not offered.
        let sender = header.sender().cloned().map(OwnedUniqueName::from);
        // the reply action is a field, not a button, so it is kept apart.
        let can_reply = sender.is_some() && actions.iter().any(|action| action.key == INLINE_REPLY);
        actions.retain(|action| action.key != INLINE_REPLY);
        let hints = parse_hints(&hints);
        let request = NotificationRequest {
            app_name,
            app_icon,
            summary,
            body,
            actions,
            can_reply,
            sender,
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
    async fn notification_replied(
        emitter: &SignalEmitter<'_>,
        id: u32,
        text: &str,
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
    serve_with(Some(address), timeout_policy, 1, Arc::new(|_, _, _| {})).await
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
    if !actions.len().is_multiple_of(2) {
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
        reply_placeholder: hint_string(hints, "x-kde-reply-placeholder-text"),
        // the hint was renamed twice; the newest name wins.
        image: ["image-data", "image_data", "icon_data"]
            .into_iter()
            .find_map(|name| hints.get(name))
            .and_then(parse_image),
    }
}

/// converts the specification's `(iiibiiay)` image into rgba, downscaling an
/// image larger than `MAX_IMAGE_EDGE` by averaging blocks of pixels.
fn parse_image(value: &OwnedValue) -> Option<ImagePixmap> {
    let structure = <&Structure>::try_from(value).ok()?;
    let [width, height, stride, alpha, bits, channels, data] = structure.fields() else {
        return None;
    };
    let width = i32::try_from(width).ok()?;
    let height = i32::try_from(height).ok()?;
    let stride = i32::try_from(stride).ok()?;
    let alpha = bool::try_from(alpha).ok()?;
    let bits = i32::try_from(bits).ok()?;
    let channels = i32::try_from(channels).ok()?;
    // the request body is already limited to 1 mib, which bounds this copy.
    let data = Vec::<u8>::try_from(data.try_clone().ok()?).ok()?;
    let expected_channels = if alpha { 4 } else { 3 };
    if !(1..=MAX_SOURCE_IMAGE_EDGE).contains(&width)
        || !(1..=MAX_SOURCE_IMAGE_EDGE).contains(&height)
        || bits != 8
        || channels != expected_channels
        || stride < width * channels
    {
        return None;
    }
    let (width, height, stride, channels) = (
        width as usize,
        height as usize,
        stride as usize,
        channels as usize,
    );
    // the last row may omit its padding.
    let needed = stride
        .checked_mul(height - 1)?
        .checked_add(width.checked_mul(channels)?)?;
    if data.len() < needed {
        return None;
    }
    let factor = width.max(height).div_ceil(MAX_IMAGE_EDGE as usize);
    let out_width = width.div_ceil(factor);
    let out_height = height.div_ceil(factor);
    let mut rgba = Vec::with_capacity(out_width * out_height * 4);
    for out_y in 0..out_height {
        for out_x in 0..out_width {
            let mut sum = [0_u32; 4];
            let mut count = 0_u32;
            for y in out_y * factor..((out_y + 1) * factor).min(height) {
                for x in out_x * factor..((out_x + 1) * factor).min(width) {
                    let pixel = &data[y * stride + x * channels..][..channels];
                    let weight = if alpha { u32::from(pixel[3]) } else { 255 };
                    for (total, value) in sum.iter_mut().zip(&pixel[..3]) {
                        *total += u32::from(*value) * weight;
                    }
                    sum[3] += weight;
                    count += 1;
                }
            }
            // averaging premultiplied colors keeps transparent pixels from
            // darkening edges. a block of at most 32x32 pixels fits in u32, and
            // adding half the divisor rounds to the nearest value.
            let coverage = sum[3];
            // zero coverage also means zero weighted colors, so a divisor of
            // one keeps the block transparent without a special output path.
            let divisor = coverage.max(1);
            rgba.extend([
                ((sum[0] + divisor / 2) / divisor) as u8,
                ((sum[1] + divisor / 2) / divisor) as u8,
                ((sum[2] + divisor / 2) / divisor) as u8,
                ((coverage + count / 2) / count) as u8,
            ]);
        }
    }
    ImagePixmap::new(out_width as u32, out_height as u32, rgba).ok()
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
        ModelError::UnknownNotification
        | ModelError::UnknownAction
        | ModelError::ReplyUnavailable
        | ModelError::InvalidReply => fdo::Error::Failed(model_message(error)),
    }
}

fn model_message(error: ModelError) -> String {
    match error {
        ModelError::InvalidRequest => "invalid notification request",
        ModelError::InvalidTimeout => "notification timeout must be -1, 0, or positive",
        ModelError::Capacity => "notification capacity is exhausted",
        ModelError::UnknownNotification => "notification is no longer active",
        ModelError::UnknownAction => "notification action does not exist",
        ModelError::ReplyUnavailable => "notification does not accept a reply",
        ModelError::InvalidReply => "a reply must be nonempty and at most 64 kib",
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

        #[zbus(signal)]
        fn notification_replied(&self, id: u32, text: &str) -> zbus::Result<()>;

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

            assert_eq!(
                proxy.get_capabilities().await.unwrap(),
                ["actions", "body", "icon-static", "inline-reply"]
            );
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
    fn inline_replies_and_image_data_reach_the_model_and_the_client() {
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
            // a 2x1 rgb image with two bytes of row padding.
            let image = Value::from(Structure::from((
                2_i32,
                1_i32,
                8_i32,
                false,
                8_i32,
                3_i32,
                vec![10_u8, 20, 30, 40, 50, 60, 0, 0],
            )));
            let hints = HashMap::from([
                (
                    "x-kde-reply-placeholder-text".into(),
                    owned(Value::from("Reply to Ada")),
                ),
                ("image-data".into(), owned(image)),
            ]);
            let id = proxy
                .notify(
                    "chat",
                    0,
                    "",
                    "Ada",
                    "are you there?",
                    vec![
                        "default".into(),
                        "Open".into(),
                        "inline-reply".into(),
                        "Reply".into(),
                    ],
                    hints,
                    0,
                )
                .await
                .unwrap();
            let active = server.active();
            let request = &active[0].request;
            assert!(request.can_reply);
            assert_eq!(request.hints.reply_placeholder, "Reply to Ada");
            assert_eq!(
                request
                    .actions
                    .iter()
                    .map(|a| a.key.as_str())
                    .collect::<Vec<_>>(),
                ["default"]
            );
            let image = request.hints.image.as_ref().unwrap();
            assert_eq!((image.width(), image.height()), (2, 1));
            assert_eq!(image.rgba(), [10, 20, 30, 255, 40, 50, 60, 255]);

            let key = active[0].key;
            let mut replied = proxy.receive_notification_replied().await.unwrap();
            let mut closed = proxy.receive_notification_closed().await.unwrap();
            let emitter = SignalEmitter::new(&server_connection, OBJECT_PATH).unwrap();
            // the reply field is not an ordinary action.
            assert!(server.invoke(key, "inline-reply", &emitter).await.is_err());
            server.reply(key, "yes!", &emitter).await.unwrap();
            let signal = replied.next().await.unwrap();
            let reply = signal.args().unwrap();
            assert_eq!((*reply.id(), *reply.text()), (id, "yes!"));
            let signal = closed.next().await.unwrap();
            assert_eq!(
                *signal.args().unwrap().reason(),
                CloseReason::Dismissed.code()
            );
            assert!(server.active().is_empty());
            let history = server.history();
            assert_eq!(history[0].key, key);
            assert!(server.reply(key, "late", &emitter).await.is_err());
            server.forget(key).unwrap();
            assert!(server.history().is_empty());
            drop(server_connection);
        });
        bus.stop();
    }

    #[test]
    fn replies_reach_only_the_latest_sender() {
        let mut bus = Bus::start();
        async_io::block_on(async {
            let (server_connection, server) =
                serve(&bus.address, TimeoutPolicy::default()).await.unwrap();
            let emitter = SignalEmitter::new(&server_connection, OBJECT_PATH).unwrap();
            let mut first = Client::connect(&bus.address).await;
            let mut second = Client::connect(&bus.address).await;
            let mut observer = Client::connect(&bus.address).await;

            // the sender hears the reply, while everyone still hears the close.
            let id = first.notify_replyable(0).await;
            server
                .reply(server.active()[0].key, "first", &emitter)
                .await
                .unwrap();
            assert_eq!(first.next_reply().await, (id, "first".into()));
            for client in [&mut first, &mut second, &mut observer] {
                client.expect_close_without_reply(id).await;
            }

            // a replacement moves replies to the client that sent it.
            let id = first.notify_replyable(0).await;
            assert_eq!(second.notify_replyable(id).await, id);
            server
                .reply(server.active()[0].key, "second", &emitter)
                .await
                .unwrap();
            assert_eq!(second.next_reply().await, (id, "second".into()));
            for client in [&mut first, &mut second, &mut observer] {
                client.expect_close_without_reply(id).await;
            }

            // after the sender disconnects, its unique name is never reassigned,
            // so the reply reaches nobody and the close is still broadcast.
            let id = second.notify_replyable(0).await;
            let departed = second.unique_name();
            drop(second);
            let dbus = zbus::fdo::DBusProxy::new(&observer.connection)
                .await
                .unwrap();
            let mut attempts = 0;
            while dbus.name_has_owner((&departed).into()).await.unwrap() {
                attempts += 1;
                assert!(attempts < 400, "the bus did not notice the disconnect");
                async_io::Timer::after(Duration::from_millis(5)).await;
            }
            server
                .reply(server.active()[0].key, "gone", &emitter)
                .await
                .unwrap();
            for client in [&mut first, &mut observer] {
                client.expect_close_without_reply(id).await;
            }
            assert!(server.active().is_empty());
            drop(server_connection);
        });
        bus.stop();
    }

    struct Client {
        connection: zbus::Connection,
        proxy: TestNotificationsProxy<'static>,
        replies: NotificationRepliedStream,
        closes: NotificationClosedStream,
    }

    impl Client {
        async fn connect(address: &str) -> Self {
            let connection = zbus::connection::Builder::address(address)
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = TestNotificationsProxy::new(&connection).await.unwrap();
            let replies = proxy.receive_notification_replied().await.unwrap();
            let closes = proxy.receive_notification_closed().await.unwrap();
            Self {
                connection,
                proxy,
                replies,
                closes,
            }
        }

        fn unique_name(&self) -> zbus::names::OwnedUniqueName {
            self.connection.unique_name().unwrap().clone()
        }

        async fn notify_replyable(&self, replaces_id: u32) -> u32 {
            self.proxy
                .notify(
                    "chat",
                    replaces_id,
                    "",
                    "message",
                    "",
                    vec!["inline-reply".into(), "Reply".into()],
                    HashMap::new(),
                    0,
                )
                .await
                .unwrap()
        }

        async fn next_reply(&mut self) -> (u32, String) {
            let signal = self.replies.next().await.unwrap();
            let args = signal.args().unwrap();
            (*args.id(), args.text().to_string())
        }

        /// the bus keeps one sender's messages in order, and the reply is sent
        /// before the close, so any reply meant for this client is queued by now.
        async fn expect_close_without_reply(&mut self, id: u32) {
            let signal = self.closes.next().await.unwrap();
            let args = signal.args().unwrap();
            assert_eq!(
                (*args.id(), *args.reason()),
                (id, CloseReason::Dismissed.code())
            );
            assert!(
                futures_util::FutureExt::now_or_never(self.replies.next()).is_none(),
                "a reply reached a client that did not send the notification"
            );
        }
    }

    #[test]
    fn image_data_is_validated_converted_and_downscaled() {
        let rgba = parse_image(&image(1, 2, 4, true, 8, 4, vec![1, 2, 3, 4, 5, 6, 7, 8])).unwrap();
        assert_eq!(rgba.rgba(), [1, 2, 3, 4, 5, 6, 7, 8]);

        // 512x2 becomes 256x1, each output pixel averaging a 2x2 block.
        let mut data = Vec::new();
        for y in 0..2 {
            for x in 0..512 {
                data.extend([if y == 0 { 0 } else { 200 }, (x % 2) as u8 * 100, 0]);
            }
        }
        let scaled = parse_image(&image(512, 2, 1536, false, 8, 3, data)).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (256, 1));
        assert_eq!(&scaled.rgba()[..4], [100, 50, 0, 255]);

        for invalid in [
            image(2, 1, 8, false, 16, 3, vec![0; 8]),
            image(2, 1, 8, true, 8, 3, vec![0; 8]),
            image(2, 1, 4, false, 8, 3, vec![0; 8]),
            image(2, 2, 8, false, 8, 3, vec![0; 10]),
            image(0, 1, 0, false, 8, 3, Vec::new()),
            owned(Value::from(7_u32)),
        ] {
            assert!(parse_image(&invalid).is_none());
        }
    }

    #[test]
    fn downscaling_averages_premultiplied_colors() {
        let pairs = |first: [u8; 4], second: [u8; 4]| [first, second].concat().repeat(256);
        let first_pixel = |data: Vec<u8>| {
            let scaled = parse_image(&image(512, 1, 2048, true, 8, 4, data)).unwrap();
            assert_eq!((scaled.width(), scaled.height()), (256, 1));
            <[u8; 4]>::try_from(&scaled.rgba()[..4]).unwrap()
        };

        // straight averaging would darken the edge to [127, 0, 0, 127].
        assert_eq!(
            first_pixel(pairs([255, 0, 0, 255], [0, 0, 0, 0])),
            [255, 0, 0, 128]
        );
        // premultiplied (127.5, 0, 42.5, 170) is straight (191.25, 0, 63.75, 170).
        assert_eq!(
            first_pixel(pairs([255, 0, 0, 255], [0, 0, 255, 85])),
            [191, 0, 64, 170]
        );
        // hidden colors under zero alpha contribute nothing.
        assert_eq!(
            first_pixel(pairs([0, 0, 0, 255], [255, 255, 255, 0])),
            [0, 0, 0, 128]
        );
        assert_eq!(
            first_pixel(pairs([90, 80, 70, 0], [10, 20, 30, 0])),
            [0, 0, 0, 0]
        );

        // opaque rgb rounds halves up: (1.5, 3, 4.5) becomes (2, 3, 5).
        let opaque = [[1_u8, 2, 3], [2, 4, 6]].concat().repeat(256);
        let scaled = parse_image(&image(512, 1, 1536, false, 8, 3, opaque)).unwrap();
        assert_eq!(&scaled.rgba()[..4], [2, 3, 5, 255]);

        // a padded 257x2 image leaves a final block one column wide. the padding
        // and the shorter last row must not be read.
        let stride = 257 * 4 + 3;
        let mut data = vec![0xee; stride + 257 * 4];
        for y in 0..2 {
            for x in 0..257 {
                data[y * stride + x * 4..][..4].copy_from_slice(&[0, 0, 255, 255]);
            }
        }
        data[256 * 4..][..4].copy_from_slice(&[255, 0, 0, 255]);
        data[stride + 256 * 4..][..4].copy_from_slice(&[0, 255, 0, 0]);
        let scaled = parse_image(&image(257, 2, stride as i32, true, 8, 4, data)).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (129, 1));
        assert_eq!(&scaled.rgba()[..4], [0, 0, 255, 255]);
        assert_eq!(&scaled.rgba()[128 * 4..], [255, 0, 0, 128]);

        // one-pixel blocks keep every visible color and alpha exactly.
        let exhaustive = (1..=255_u8)
            .flat_map(|alpha| {
                (0..=255_u8).flat_map(move |color| [color, 255 - color, color / 2, alpha])
            })
            .collect::<Vec<_>>();
        let copied = parse_image(&image(256, 255, 1024, true, 8, 4, exhaustive.clone())).unwrap();
        assert_eq!((copied.width(), copied.height()), (256, 255));
        assert_eq!(copied.rgba(), exhaustive);
    }

    fn image(
        width: i32,
        height: i32,
        stride: i32,
        alpha: bool,
        bits: i32,
        channels: i32,
        data: Vec<u8>,
    ) -> OwnedValue {
        owned(Value::from(Structure::from((
            width, height, stride, alpha, bits, channels, data,
        ))))
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
