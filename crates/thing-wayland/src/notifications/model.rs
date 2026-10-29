use std::{
    collections::{BTreeSet, VecDeque},
    mem::size_of,
};

use thing_core::ImagePixmap;
use thing_runtime::MAX_NOTIFICATION_HISTORY;
use zbus::names::OwnedUniqueName;

const MAX_ACTIVE: usize = 1_024;
const MAX_ACTIONS: usize = 128;
pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_NOTIFICATION_RETAINED_BYTES: usize = 512 * 1024;
pub(crate) const MAX_ACTIVE_RETAINED_BYTES: usize = 8 * 1024 * 1024;
// closed notifications kept for presentation, newest first.
pub(crate) const MAX_HISTORY_RETAINED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub(crate) fn from_hint(value: Option<u8>) -> Self {
        match value {
            Some(0) => Self::Low,
            Some(2) => Self::Critical,
            _ => Self::Normal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CriticalTimeout {
    Never,
    #[allow(dead_code)]
    HonorRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeoutPolicy {
    /// one caller-selected fallback for requests that use the server default.
    pub(crate) fallback_ms: Option<u32>,
    /// how critical urgency interacts with a client's requested timeout.
    pub(crate) critical: CriticalTimeout,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        // this adapter is intentionally separate from protocol parsing so a shell
        // policy can replace it without changing freedesktop request semantics.
        Self {
            fallback_ms: Some(5_000),
            critical: CriticalTimeout::Never,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NotificationAction {
    pub(crate) key: String,
    pub(crate) label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NotificationHints {
    pub(crate) urgency: Option<u8>,
    pub(crate) category: String,
    pub(crate) desktop_entry: String,
    pub(crate) image_path: String,
    pub(crate) resident: bool,
    pub(crate) transient: bool,
    pub(crate) action_icons: bool,
    pub(crate) sound_file: String,
    pub(crate) sound_name: String,
    pub(crate) suppress_sound: bool,
    pub(crate) x: Option<i32>,
    pub(crate) y: Option<i32>,
    /// placeholder text for an inline reply field.
    pub(crate) reply_placeholder: String,
    /// raw image data from the client, already converted to rgba.
    pub(crate) image: Option<ImagePixmap>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NotificationRequest {
    pub(crate) app_name: String,
    pub(crate) app_icon: String,
    pub(crate) summary: String,
    pub(crate) body: String,
    pub(crate) actions: Vec<NotificationAction>,
    /// whether the client offered the `inline-reply` action.
    pub(crate) can_reply: bool,
    /// the bus name that sent the latest revision. replies carry the user's
    /// text, so they go only to this name.
    pub(crate) sender: Option<OwnedUniqueName>,
    pub(crate) hints: NotificationHints,
    pub(crate) expire_timeout_ms: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    ClosedByCall = 3,
}

impl CloseReason {
    pub(crate) fn code(self) -> u32 {
        self as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Closed {
    pub(crate) reason: CloseReason,
    pub(crate) at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notification {
    pub(crate) id: u32,
    pub(crate) key: u64,
    pub(crate) revision: u64,
    pub(crate) request: NotificationRequest,
    pub(crate) urgency: Urgency,
    pub(crate) effective_timeout_ms: Option<u32>,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) expires_at_ms: Option<u64>,
    pub(crate) closed: Option<Closed>,
    retained_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Published {
    pub(crate) id: u32,
    pub(crate) key: u64,
    pub(crate) replaced: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Replied {
    pub(crate) id: u32,
    pub(crate) sender: OwnedUniqueName,
    pub(crate) closed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Invoked {
    pub(crate) id: u32,
    pub(crate) key: u64,
    pub(crate) action_key: String,
    pub(crate) closed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelError {
    InvalidRequest,
    InvalidTimeout,
    Capacity,
    UnknownNotification,
    UnknownAction,
    ReplyUnavailable,
    InvalidReply,
}

#[derive(Debug)]
pub(crate) struct Model {
    timeout_policy: TimeoutPolicy,
    next_id: u32,
    next_key: u64,
    retained_bytes: usize,
    active: Vec<Notification>,
    history: VecDeque<Notification>,
    history_bytes: usize,
}

impl Model {
    #[cfg(test)]
    pub(crate) fn new(timeout_policy: TimeoutPolicy) -> Self {
        Self::with_next_key(timeout_policy, 1)
    }

    pub(crate) fn with_next_key(timeout_policy: TimeoutPolicy, next_key: u64) -> Self {
        Self {
            timeout_policy,
            next_id: 1,
            next_key: next_key.max(1),
            retained_bytes: 0,
            active: Vec::new(),
            history: VecDeque::new(),
            history_bytes: 0,
        }
    }

    pub(crate) fn active(&self) -> &[Notification] {
        &self.active
    }

    pub(crate) fn history(&self) -> &VecDeque<Notification> {
        &self.history
    }

    pub(crate) fn next_key(&self) -> u64 {
        self.next_key
    }

    pub(crate) fn next_expiry_ms(&self) -> Option<u64> {
        self.active
            .iter()
            .filter_map(|notification| notification.expires_at_ms)
            .min()
    }

    pub(crate) fn notify(
        &mut self,
        replaces_id: u32,
        mut request: NotificationRequest,
        now_ms: u64,
    ) -> Result<Published, ModelError> {
        let retained_bytes = validate_request(&mut request)?;
        let urgency = Urgency::from_hint(request.hints.urgency);
        let effective_timeout_ms = effective_timeout(&request, urgency, self.timeout_policy)?;
        let expires_at_ms =
            effective_timeout_ms.map(|timeout| now_ms.saturating_add(timeout.into()));

        if replaces_id != 0
            && let Some(index) = self
                .active
                .iter()
                .position(|notification| notification.id == replaces_id)
        {
            let next_retained = self
                .retained_bytes
                .checked_sub(self.active[index].retained_bytes)
                .and_then(|retained| retained.checked_add(retained_bytes))
                .filter(|retained| *retained <= MAX_ACTIVE_RETAINED_BYTES)
                .ok_or(ModelError::Capacity)?;
            let current = &mut self.active[index];
            current.revision = current.revision.saturating_add(1);
            current.request = request;
            current.urgency = urgency;
            current.effective_timeout_ms = effective_timeout_ms;
            current.updated_at_ms = now_ms;
            current.expires_at_ms = expires_at_ms;
            current.retained_bytes = retained_bytes;
            self.retained_bytes = next_retained;
            return Ok(Published {
                id: current.id,
                key: current.key,
                replaced: true,
            });
        }

        if self.active.len() >= MAX_ACTIVE
            || self
                .retained_bytes
                .checked_add(retained_bytes)
                .is_none_or(|retained| retained > MAX_ACTIVE_RETAINED_BYTES)
        {
            return Err(ModelError::Capacity);
        }
        let id = if replaces_id == 0 {
            self.allocate_id()?
        } else {
            replaces_id
        };
        if self.active.iter().any(|notification| notification.id == id) {
            return Err(ModelError::Capacity);
        }
        let key = self.allocate_key()?;
        if replaces_id != 0 {
            self.reserve_id(replaces_id);
        }
        self.active.insert(
            0,
            Notification {
                id,
                key,
                revision: 1,
                request,
                urgency,
                effective_timeout_ms,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                expires_at_ms,
                closed: None,
                retained_bytes,
            },
        );
        self.retained_bytes += retained_bytes;
        Ok(Published {
            id,
            key,
            replaced: false,
        })
    }

    pub(crate) fn close_by_id(
        &mut self,
        id: u32,
        reason: CloseReason,
        now_ms: u64,
    ) -> Result<Notification, ModelError> {
        let Some(index) = self
            .active
            .iter()
            .position(|notification| notification.id == id)
        else {
            return Err(ModelError::UnknownNotification);
        };
        Ok(self.close_at(index, reason, now_ms))
    }

    pub(crate) fn dismiss(&mut self, key: u64, now_ms: u64) -> Result<Notification, ModelError> {
        let Some(index) = self
            .active
            .iter()
            .position(|notification| notification.key == key)
        else {
            return Err(ModelError::UnknownNotification);
        };
        Ok(self.close_at(index, CloseReason::Dismissed, now_ms))
    }

    pub(crate) fn invoke(
        &mut self,
        key: u64,
        action_key: &str,
        now_ms: u64,
    ) -> Result<Invoked, ModelError> {
        let Some(index) = self
            .active
            .iter()
            .position(|notification| notification.key == key)
        else {
            return Err(ModelError::UnknownNotification);
        };
        let notification = &self.active[index];
        if !notification
            .request
            .actions
            .iter()
            .any(|action| action.key == action_key)
        {
            return Err(ModelError::UnknownAction);
        }
        let id = notification.id;
        let resident = notification.request.hints.resident;
        if !resident {
            self.close_at(index, CloseReason::Dismissed, now_ms);
        }
        Ok(Invoked {
            id,
            key,
            action_key: action_key.to_owned(),
            closed: !resident,
        })
    }

    /// answers an inline reply. like an action, it closes the notification
    /// unless the client marked it resident.
    pub(crate) fn reply(
        &mut self,
        key: u64,
        text: &str,
        now_ms: u64,
    ) -> Result<Replied, ModelError> {
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(ModelError::InvalidReply);
        }
        let Some(index) = self
            .active
            .iter()
            .position(|notification| notification.key == key)
        else {
            return Err(ModelError::UnknownNotification);
        };
        let notification = &self.active[index];
        // without a sender the reply could only be broadcast, so it is refused
        // before the notification closes.
        let Some(sender) = notification
            .request
            .sender
            .clone()
            .filter(|_| notification.request.can_reply)
        else {
            return Err(ModelError::ReplyUnavailable);
        };
        let id = notification.id;
        let resident = notification.request.hints.resident;
        if !resident {
            self.close_at(index, CloseReason::Dismissed, now_ms);
        }
        Ok(Replied {
            id,
            sender,
            closed: !resident,
        })
    }

    /// removes one closed notification from the history.
    pub(crate) fn forget(&mut self, key: u64) -> Result<(), ModelError> {
        let index = self
            .history
            .iter()
            .position(|notification| notification.key == key)
            .ok_or(ModelError::UnknownNotification)?;
        let removed = self
            .history
            .remove(index)
            .expect("the index was found above");
        self.history_bytes -= removed.retained_bytes;
        Ok(())
    }

    pub(crate) fn clear_history(&mut self) -> bool {
        let changed = !self.history.is_empty();
        self.history.clear();
        self.history_bytes = 0;
        changed
    }

    pub(crate) fn expire(&mut self, now_ms: u64) -> Vec<Notification> {
        let mut expired = Vec::new();
        let mut index = 0;
        while index < self.active.len() {
            if self.active[index]
                .expires_at_ms
                .is_some_and(|deadline| deadline <= now_ms)
            {
                expired.push(self.close_at(index, CloseReason::Expired, now_ms));
            } else {
                index += 1;
            }
        }
        expired
    }

    fn close_at(&mut self, index: usize, reason: CloseReason, now_ms: u64) -> Notification {
        let mut notification = self.active.remove(index);
        self.retained_bytes -= notification.retained_bytes;
        notification.closed = Some(Closed {
            reason,
            at_ms: now_ms,
        });
        // transient notifications ask not to be kept after they close.
        if !notification.request.hints.transient {
            self.remember(notification.clone());
        }
        notification
    }

    fn remember(&mut self, notification: Notification) {
        self.history_bytes += notification.retained_bytes;
        self.history.push_front(notification);
        while self.history.len() > MAX_NOTIFICATION_HISTORY
            || self.history_bytes > MAX_HISTORY_RETAINED_BYTES
        {
            let oldest = self
                .history
                .pop_back()
                .expect("an over-limit history is not empty");
            self.history_bytes -= oldest.retained_bytes;
        }
    }

    fn allocate_id(&mut self) -> Result<u32, ModelError> {
        let start = self.next_id.max(1);
        let mut candidate = start;
        loop {
            self.next_id = candidate.checked_add(1).unwrap_or(1);
            if !self
                .active
                .iter()
                .any(|notification| notification.id == candidate)
            {
                return Ok(candidate);
            }
            candidate = self.next_id;
            if candidate == start {
                return Err(ModelError::Capacity);
            }
        }
    }

    fn reserve_id(&mut self, id: u32) {
        if id == u32::MAX {
            if self.next_id == u32::MAX {
                self.next_id = 1;
            }
        } else if id >= self.next_id {
            self.next_id = id + 1;
        }
    }

    fn allocate_key(&mut self) -> Result<u64, ModelError> {
        let key = self.next_key;
        self.next_key = self.next_key.checked_add(1).ok_or(ModelError::Capacity)?;
        Ok(key)
    }
}

fn validate_request(request: &mut NotificationRequest) -> Result<usize, ModelError> {
    let text = [
        &request.app_name,
        &request.app_icon,
        &request.summary,
        &request.body,
        &request.hints.category,
        &request.hints.desktop_entry,
        &request.hints.image_path,
        &request.hints.sound_file,
        &request.hints.sound_name,
        &request.hints.reply_placeholder,
    ];
    if request.actions.len() > MAX_ACTIONS || text.iter().any(|value| value.len() > MAX_TEXT_BYTES)
    {
        return Err(ModelError::InvalidRequest);
    }
    let mut keys = BTreeSet::new();
    if request.actions.iter().any(|action| {
        action.key.is_empty()
            || action.key.len() > MAX_TEXT_BYTES
            || action.label.len() > MAX_TEXT_BYTES
            || !keys.insert(action.key.as_str())
    }) {
        return Err(ModelError::InvalidRequest);
    }
    let mut retained_bytes = request_retained_bytes(request).ok_or(ModelError::InvalidRequest)?;
    // an image is optional presentation, so an oversized one is dropped rather
    // than rejecting the notification.
    if retained_bytes > MAX_NOTIFICATION_RETAINED_BYTES && request.hints.image.take().is_some() {
        retained_bytes = request_retained_bytes(request).ok_or(ModelError::InvalidRequest)?;
    }
    if retained_bytes > MAX_NOTIFICATION_RETAINED_BYTES {
        return Err(ModelError::InvalidRequest);
    }
    Ok(retained_bytes)
}

pub(crate) fn request_retained_bytes(request: &NotificationRequest) -> Option<usize> {
    let text = [
        &request.app_name,
        &request.app_icon,
        &request.summary,
        &request.body,
        &request.hints.category,
        &request.hints.desktop_entry,
        &request.hints.image_path,
        &request.hints.sound_file,
        &request.hints.sound_name,
        &request.hints.reply_placeholder,
    ];
    let image = request
        .hints
        .image
        .as_ref()
        .map_or(0, |image| image.rgba().len());
    let sender = request
        .sender
        .as_ref()
        .map_or(0, |sender| sender.as_str().len());
    let mut retained = size_of::<Notification>()
        .checked_add(image)?
        .checked_add(sender)?
        .checked_add(
            request
                .actions
                .capacity()
                .checked_mul(size_of::<NotificationAction>())?,
        )?;
    for value in text {
        retained = retained.checked_add(value.capacity())?;
    }
    for action in &request.actions {
        retained = retained.checked_add(action.key.capacity())?;
        retained = retained.checked_add(action.label.capacity())?;
    }
    Some(retained)
}

fn effective_timeout(
    request: &NotificationRequest,
    urgency: Urgency,
    policy: TimeoutPolicy,
) -> Result<Option<u32>, ModelError> {
    if request.expire_timeout_ms < -1 {
        return Err(ModelError::InvalidTimeout);
    }
    if urgency == Urgency::Critical && policy.critical == CriticalTimeout::Never {
        return Ok(None);
    }
    if request.expire_timeout_ms == 0 {
        return Ok(None);
    }
    if request.expire_timeout_ms > 0 {
        return Ok(Some(request.expire_timeout_ms as u32));
    }
    Ok(policy.fallback_ms.filter(|timeout| *timeout > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_notifications_enter_a_bounded_history_unless_transient() {
        let mut model = Model::new(TimeoutPolicy::default());
        let first = model.notify(0, request("first"), 0).unwrap();
        let mut transient = request("transient");
        transient.hints.transient = true;
        let skipped = model.notify(0, transient, 0).unwrap();
        model.dismiss(first.key, 10).unwrap();
        model.dismiss(skipped.key, 11).unwrap();
        assert_eq!(model.history().len(), 1);
        let kept = &model.history()[0];
        assert_eq!(kept.key, first.key);
        assert_eq!(
            kept.closed,
            Some(Closed {
                reason: CloseReason::Dismissed,
                at_ms: 10,
            })
        );

        // the oldest records leave first once the count limit is reached.
        for index in 0..MAX_NOTIFICATION_HISTORY + 5 {
            let published = model.notify(0, request(&format!("n{index}")), 20).unwrap();
            model.dismiss(published.key, 30).unwrap();
        }
        assert_eq!(model.history().len(), MAX_NOTIFICATION_HISTORY);
        assert_eq!(
            model.history()[0].request.summary,
            format!("n{}", MAX_NOTIFICATION_HISTORY + 4)
        );
        assert!(model.history().iter().all(|record| record.key != first.key));

        let newest = model.history()[0].key;
        model.forget(newest).unwrap();
        assert!(model.history().iter().all(|record| record.key != newest));
        assert_eq!(model.forget(newest), Err(ModelError::UnknownNotification));
        assert!(model.clear_history());
        assert!(model.history().is_empty() && !model.clear_history());
    }

    #[test]
    fn replies_need_the_reply_action_and_close_unless_resident() {
        let mut model = Model::new(TimeoutPolicy::default());
        let plain = model.notify(0, request("plain"), 0).unwrap();
        assert_eq!(
            model.reply(plain.key, "hi", 1),
            Err(ModelError::ReplyUnavailable)
        );
        let sender = OwnedUniqueName::try_from(":1.42").unwrap();
        let mut unaddressed = request("unaddressed");
        unaddressed.can_reply = true;
        let unaddressed = model.notify(0, unaddressed, 0).unwrap();
        assert_eq!(
            model.reply(unaddressed.key, "hi", 1),
            Err(ModelError::ReplyUnavailable)
        );
        assert!(
            model
                .active()
                .iter()
                .any(|record| record.key == unaddressed.key)
        );
        let mut replyable = request("chat");
        replyable.can_reply = true;
        replyable.sender = Some(sender.clone());
        let chat = model.notify(0, replyable.clone(), 0).unwrap();
        assert_eq!(model.reply(chat.key, "", 1), Err(ModelError::InvalidReply));
        assert_eq!(
            model.reply(chat.key, "hi", 2),
            Ok(Replied {
                id: chat.id,
                sender: sender.clone(),
                closed: true,
            })
        );
        assert_eq!(
            model.reply(chat.key, "again", 3),
            Err(ModelError::UnknownNotification)
        );
        replyable.hints.resident = true;
        let resident = model.notify(0, replyable, 0).unwrap();
        assert_eq!(
            model.reply(resident.key, "hi", 4),
            Ok(Replied {
                id: resident.id,
                sender,
                closed: false,
            })
        );
        assert!(
            model
                .active()
                .iter()
                .any(|record| record.key == resident.key)
        );
    }

    #[test]
    fn an_oversized_image_is_dropped_without_rejecting_the_notification() {
        let mut model = Model::new(TimeoutPolicy::default());
        let mut small = request("small");
        small.hints.image = Some(ImagePixmap::new(2, 2, vec![255; 16]).unwrap());
        let small = model.notify(0, small, 0).unwrap();
        assert!(model.active()[0].request.hints.image.is_some());
        assert_eq!(model.active()[0].key, small.key);

        let mut large = request_with_retained_bytes(MAX_NOTIFICATION_RETAINED_BYTES - 1024);
        large.hints.image = Some(ImagePixmap::new(64, 64, vec![255; 64 * 64 * 4]).unwrap());
        model.notify(0, large, 0).unwrap();
        assert!(model.active()[0].request.hints.image.is_none());
    }

    fn request(summary: &str) -> NotificationRequest {
        NotificationRequest {
            summary: summary.into(),
            actions: vec![NotificationAction {
                key: "default".into(),
                label: "open".into(),
            }],
            expire_timeout_ms: -1,
            ..NotificationRequest::default()
        }
    }

    fn request_with_retained_bytes(target: usize) -> NotificationRequest {
        let mut request = NotificationRequest::default();
        let base = request_retained_bytes(&request).unwrap();
        assert!(target >= base);
        let mut remaining = target - base;
        for field in [
            &mut request.app_name,
            &mut request.app_icon,
            &mut request.summary,
            &mut request.body,
            &mut request.hints.category,
            &mut request.hints.desktop_entry,
            &mut request.hints.image_path,
            &mut request.hints.sound_file,
            &mut request.hints.sound_name,
        ] {
            let bytes = remaining.min(MAX_TEXT_BYTES);
            *field = "x".repeat(bytes).into_boxed_str().into_string();
            remaining -= bytes;
        }
        assert_eq!(remaining, 0);
        assert_eq!(request_retained_bytes(&request), Some(target));
        request
    }

    #[test]
    fn replacement_preserves_lifetime_and_resets_expiry() {
        let mut model = Model::new(TimeoutPolicy::default());
        let first = model.notify(0, request("first"), 1_000).unwrap();
        let replacement = model
            .notify(first.id, request("replacement"), 2_000)
            .unwrap();

        assert_eq!(replacement.id, first.id);
        assert_eq!(replacement.key, first.key);
        assert!(replacement.replaced);
        assert_eq!(model.active().len(), 1);
        let notification = &model.active()[0];
        assert_eq!(notification.revision, 2);
        assert_eq!(notification.request.summary, "replacement");
        assert_eq!(notification.created_at_ms, 1_000);
        assert_eq!(notification.updated_at_ms, 2_000);
        assert_eq!(notification.expires_at_ms, Some(7_000));
        assert!(model.expire(6_999).is_empty());
        assert_eq!(model.expire(7_000)[0].id, first.id);
    }

    #[test]
    fn critical_and_zero_timeout_notifications_do_not_expire() {
        let mut model = Model::new(TimeoutPolicy::default());
        let mut critical = request("critical");
        critical.hints.urgency = Some(2);
        critical.expire_timeout_ms = 1;
        let mut persistent = request("persistent");
        persistent.expire_timeout_ms = 0;
        model.notify(0, critical, 0).unwrap();
        model.notify(0, persistent, 0).unwrap();

        assert!(model.expire(u64::MAX).is_empty());
        assert_eq!(model.active().len(), 2);
        assert!(
            model
                .active()
                .iter()
                .all(|notification| notification.expires_at_ms.is_none())
        );
    }

    #[test]
    fn injected_policy_can_honor_critical_requests_and_change_the_fallback() {
        let mut model = Model::new(TimeoutPolicy {
            fallback_ms: Some(2_500),
            critical: CriticalTimeout::HonorRequest,
        });
        let mut critical = request("critical");
        critical.hints.urgency = Some(2);
        critical.expire_timeout_ms = 900;
        let mut fallback = request("fallback");
        fallback.hints.urgency = Some(0);

        model.notify(0, critical, 100).unwrap();
        model.notify(0, fallback, 100).unwrap();

        assert_eq!(model.active()[0].effective_timeout_ms, Some(2_500));
        assert_eq!(model.active()[1].effective_timeout_ms, Some(900));
    }

    #[test]
    fn close_results_preserve_reason_and_lifetime_data() {
        let mut model = Model::new(TimeoutPolicy::default());
        let first = model.notify(0, request("one"), 0).unwrap();
        let second = model.notify(0, request("two"), 0).unwrap();

        let dismissed = model.dismiss(first.key, 1).unwrap();
        let recalled = model
            .close_by_id(second.id, CloseReason::ClosedByCall, 2)
            .unwrap();

        assert_eq!(dismissed.closed.unwrap().reason, CloseReason::Dismissed);
        assert_eq!(recalled.closed.unwrap().reason, CloseReason::ClosedByCall);
        assert_eq!(dismissed.key, first.key);
        assert_eq!(recalled.key, second.key);
        assert!(model.active().is_empty());
    }

    #[test]
    fn invoking_an_action_closes_unless_resident() {
        let mut model = Model::new(TimeoutPolicy::default());
        let ordinary = model.notify(0, request("ordinary"), 0).unwrap();
        let mut resident = request("resident");
        resident.hints.resident = true;
        let resident = model.notify(0, resident, 0).unwrap();

        let invocation = model.invoke(ordinary.key, "default", 1).unwrap();
        assert!(invocation.closed);
        assert_eq!(invocation.id, ordinary.id);
        let invocation = model.invoke(resident.key, "default", 2).unwrap();
        assert!(!invocation.closed);
        assert_eq!(model.active().len(), 1);
        assert_eq!(model.active()[0].key, resident.key);
        assert_eq!(
            model.invoke(resident.key, "missing", 3),
            Err(ModelError::UnknownAction)
        );
    }

    #[test]
    fn unknown_replacement_id_starts_a_new_lifetime_with_that_id() {
        let mut model = Model::new(TimeoutPolicy::default());
        let published = model.notify(42, request("replacement"), 0).unwrap();

        assert_eq!(published.id, 42);
        assert!(!published.replaced);
        assert_eq!(model.active()[0].id, 42);
    }

    #[test]
    fn unknown_replacement_ids_advance_the_protocol_allocator() {
        let mut model = Model::new(TimeoutPolicy::default());
        let adopted = model.notify(1, request("adopted"), 0).unwrap();
        model
            .close_by_id(adopted.id, CloseReason::ClosedByCall, 1)
            .unwrap();

        let allocated = model.notify(0, request("allocated"), 2).unwrap();
        assert_eq!(allocated.id, 2);

        let mut wrapping = Model::new(TimeoutPolicy::default());
        wrapping.next_id = u32::MAX;
        let maximum = wrapping.notify(u32::MAX, request("maximum"), 3).unwrap();
        wrapping
            .close_by_id(maximum.id, CloseReason::ClosedByCall, 4)
            .unwrap();
        let wrapped = wrapping.notify(0, request("wrapped"), 5).unwrap();
        assert_ne!(wrapped.id, maximum.id);
        assert_eq!(wrapped.id, 1);

        let mut below_maximum = Model::new(TimeoutPolicy::default());
        below_maximum.next_id = 101;
        let maximum = below_maximum
            .notify(u32::MAX, request("maximum"), 6)
            .unwrap();
        below_maximum
            .close_by_id(maximum.id, CloseReason::ClosedByCall, 7)
            .unwrap();
        let allocated = below_maximum.notify(0, request("next"), 8).unwrap();
        assert_eq!(allocated.id, 101);
    }

    #[test]
    fn request_and_active_retained_memory_are_bounded_atomically() {
        let mut model = Model::new(TimeoutPolicy::default());
        let oversized = request_with_retained_bytes(MAX_NOTIFICATION_RETAINED_BYTES + 1);
        assert_eq!(
            model.notify(0, oversized, 0),
            Err(ModelError::InvalidRequest)
        );
        assert!(model.active().is_empty());
        assert_eq!(model.retained_bytes, 0);

        let original = model.notify(0, request("original"), 1).unwrap();
        while model.retained_bytes + MAX_NOTIFICATION_RETAINED_BYTES <= MAX_ACTIVE_RETAINED_BYTES {
            model
                .notify(
                    0,
                    request_with_retained_bytes(MAX_NOTIFICATION_RETAINED_BYTES),
                    2,
                )
                .unwrap();
        }
        let remaining = MAX_ACTIVE_RETAINED_BYTES - model.retained_bytes;
        let smallest = request_retained_bytes(&request("sized")).unwrap();
        if remaining >= smallest {
            model
                .notify(0, request_with_retained_bytes(remaining), 3)
                .unwrap();
        }
        assert_eq!(model.retained_bytes, MAX_ACTIVE_RETAINED_BYTES);

        let before = model
            .active()
            .iter()
            .find(|notification| notification.id == original.id)
            .unwrap()
            .clone();
        assert_eq!(
            model.notify(
                original.id,
                request_with_retained_bytes(MAX_NOTIFICATION_RETAINED_BYTES),
                4,
            ),
            Err(ModelError::Capacity)
        );
        assert_eq!(model.retained_bytes, MAX_ACTIVE_RETAINED_BYTES);
        assert_eq!(
            model
                .active()
                .iter()
                .find(|notification| notification.id == original.id),
            Some(&before)
        );
        assert_eq!(
            model.notify(0, request("one more"), 5),
            Err(ModelError::Capacity)
        );
    }
}
