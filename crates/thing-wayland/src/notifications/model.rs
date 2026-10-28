use std::collections::BTreeSet;

const MAX_ACTIVE: usize = 1_024;
const MAX_ACTIONS: usize = 128;
const MAX_TEXT_BYTES: usize = 64 * 1024;

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
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NotificationRequest {
    pub(crate) app_name: String,
    pub(crate) app_icon: String,
    pub(crate) summary: String,
    pub(crate) body: String,
    pub(crate) actions: Vec<NotificationAction>,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Published {
    pub(crate) id: u32,
    pub(crate) key: u64,
    pub(crate) replaced: bool,
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
}

#[derive(Debug)]
pub(crate) struct Model {
    timeout_policy: TimeoutPolicy,
    next_id: u32,
    next_key: u64,
    active: Vec<Notification>,
}

impl Model {
    pub(crate) fn new(timeout_policy: TimeoutPolicy) -> Self {
        Self {
            timeout_policy,
            next_id: 1,
            next_key: 1,
            active: Vec::new(),
        }
    }

    pub(crate) fn active(&self) -> &[Notification] {
        &self.active
    }

    pub(crate) fn notify(
        &mut self,
        replaces_id: u32,
        request: NotificationRequest,
        now_ms: u64,
    ) -> Result<Published, ModelError> {
        validate_request(&request)?;
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
            let current = &mut self.active[index];
            current.revision = current.revision.saturating_add(1);
            current.request = request;
            current.urgency = urgency;
            current.effective_timeout_ms = effective_timeout_ms;
            current.updated_at_ms = now_ms;
            current.expires_at_ms = expires_at_ms;
            return Ok(Published {
                id: current.id,
                key: current.key,
                replaced: true,
            });
        }

        if self.active.len() >= MAX_ACTIVE {
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
            },
        );
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
        notification.closed = Some(Closed {
            reason,
            at_ms: now_ms,
        });
        notification
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

    fn allocate_key(&mut self) -> Result<u64, ModelError> {
        let key = self.next_key;
        self.next_key = self.next_key.checked_add(1).ok_or(ModelError::Capacity)?;
        Ok(key)
    }
}

fn validate_request(request: &NotificationRequest) -> Result<(), ModelError> {
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
    Ok(())
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
}
