use std::time::{Duration, Instant};

use thing_core::NodeId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HoverKey<K> {
    pub output: K,
    pub anchor: NodeId,
    pub popup: NodeId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HoverTarget<K> {
    pub key: HoverKey<K>,
    pub open_delay: Duration,
    pub close_delay: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command<K> {
    Open(HoverTarget<K>),
    Close(HoverKey<K>),
}

#[derive(Clone, Debug)]
struct Pending<K> {
    target: HoverTarget<K>,
    deadline: Instant,
}

#[derive(Clone, Debug)]
pub(crate) struct HoverState<K> {
    desired: Option<HoverKey<K>>,
    pending: Option<Pending<K>>,
    deferred: Option<HoverTarget<K>>,
    active: Option<HoverTarget<K>>,
    close_at: Option<Instant>,
    suppressed: Option<HoverKey<K>>,
}

impl<K> Default for HoverState<K> {
    fn default() -> Self {
        Self {
            desired: None,
            pending: None,
            deferred: None,
            active: None,
            close_at: None,
            suppressed: None,
        }
    }
}

impl<K: Clone + Eq> HoverState<K> {
    pub fn reconcile(&mut self, desired: Option<HoverTarget<K>>, now: Instant) {
        self.desired = desired.as_ref().map(|target| target.key.clone());
        if self.suppressed.as_ref() != self.desired.as_ref() {
            self.suppressed = None;
        }

        let desired = desired.filter(|target| self.suppressed.as_ref() != Some(&target.key));
        if self.pending.as_ref().is_some_and(|pending| {
            desired
                .as_ref()
                .is_none_or(|next| pending.target.key != next.key)
        }) {
            self.pending = None;
        }
        if self
            .deferred
            .as_ref()
            .is_some_and(|target| desired.as_ref().is_none_or(|next| target.key != next.key))
        {
            self.deferred = None;
        }

        match (&mut self.active, &desired) {
            (Some(active), Some(next)) if active.key == next.key => {
                active.close_delay = next.close_delay;
                self.close_at = None;
            }
            (Some(active), _) => {
                self.close_at.get_or_insert(now + active.close_delay);
            }
            (None, _) => self.close_at = None,
        }

        if let Some(target) = desired
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.key != target.key)
            && self
                .pending
                .as_ref()
                .is_none_or(|pending| pending.target.key != target.key)
            && self
                .deferred
                .as_ref()
                .is_none_or(|deferred| deferred.key != target.key)
        {
            self.pending = Some(Pending {
                deadline: now + target.open_delay,
                target,
            });
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.pending.as_ref().map(|pending| pending.deadline),
            self.close_at,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn take_due(&mut self, now: Instant) -> Option<Command<K>> {
        let open_at = self.pending.as_ref().map(|pending| pending.deadline);
        let close_first = self
            .close_at
            .is_some_and(|close_at| close_at <= now && open_at.is_none_or(|open| close_at < open));
        if close_first {
            self.close_at = None;
            return self.active.take().map(|active| Command::Close(active.key));
        }
        if open_at.is_some_and(|deadline| deadline <= now) {
            return self
                .pending
                .take()
                .map(|pending| Command::Open(pending.target));
        }
        None
    }

    pub fn opened(&mut self, target: HoverTarget<K>) -> Option<HoverKey<K>> {
        self.deferred = None;
        self.close_at = None;
        self.active.replace(target).map(|previous| previous.key)
    }

    pub fn close(&mut self, key: &HoverKey<K>, suppress: bool) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| &active.key == key)
        {
            self.active = None;
            self.close_at = None;
        }
        if suppress {
            self.suppressed = Some(key.clone());
            self.pending = None;
            self.deferred = None;
        }
    }

    pub fn defer(&mut self, target: HoverTarget<K>) {
        self.pending = None;
        self.deferred = Some(target);
    }

    pub fn resume(&mut self, key: &HoverKey<K>, now: Instant) {
        if self.desired.as_ref() != Some(key) || self.suppressed.as_ref() == Some(key) {
            return;
        }
        let Some(target) = self.deferred.take().filter(|target| &target.key == key) else {
            return;
        };
        self.pending = Some(Pending {
            target,
            deadline: now,
        });
    }

    #[cfg(test)]
    pub fn active(&self) -> Option<&HoverTarget<K>> {
        self.active.as_ref()
    }

    pub fn wants(&self, key: &HoverKey<K>) -> bool {
        self.desired.as_ref() == Some(key) && self.suppressed.as_ref() != Some(key)
    }

    pub fn clear(&mut self) -> Option<HoverKey<K>> {
        self.desired = None;
        self.pending = None;
        self.deferred = None;
        self.close_at = None;
        self.suppressed = None;
        self.active.take().map(|active| active.key)
    }

    pub fn remove_output(&mut self, output: &K) -> Option<HoverKey<K>> {
        if self
            .desired
            .as_ref()
            .is_some_and(|key| &key.output == output)
        {
            self.desired = None;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| &pending.target.key.output == output)
        {
            self.pending = None;
        }
        if self
            .deferred
            .as_ref()
            .is_some_and(|target| &target.key.output == output)
        {
            self.deferred = None;
        }
        if self
            .suppressed
            .as_ref()
            .is_some_and(|key| &key.output == output)
        {
            self.suppressed = None;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| &active.key.output == output)
        {
            self.close_at = None;
            return self.active.take().map(|active| active.key);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(output: u8, popup: u64, open: u64, close: u64) -> HoverTarget<u8> {
        HoverTarget {
            key: HoverKey {
                output,
                anchor: NodeId(popup - 1),
                popup: NodeId(popup),
            },
            open_delay: Duration::from_millis(open),
            close_delay: Duration::from_millis(close),
        }
    }

    fn open(state: &mut HoverState<u8>, target: &HoverTarget<u8>, at: Instant) {
        state.reconcile(Some(target.clone()), at);
        assert_eq!(
            state.take_due(at + target.open_delay),
            Some(Command::Open(target.clone()))
        );
        assert_eq!(state.opened(target.clone()), None);
    }

    #[test]
    fn waits_to_open_and_cancels_a_passed_trigger() {
        let now = Instant::now();
        let target = target(1, 2, 300, 250);
        let mut state = HoverState::default();

        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now + Duration::from_millis(299)), None);
        state.reconcile(None, now + Duration::from_millis(100));
        assert_eq!(state.take_due(now + Duration::from_secs(1)), None);
    }

    #[test]
    fn crossing_into_the_popup_cancels_its_delayed_close() {
        let now = Instant::now();
        let target = target(1, 2, 100, 250);
        let mut state = HoverState::default();
        open(&mut state, &target, now);

        state.reconcile(None, now + Duration::from_millis(150));
        state.reconcile(Some(target.clone()), now + Duration::from_millis(300));
        assert_eq!(state.take_due(now + Duration::from_secs(1)), None);
        assert_eq!(state.active(), Some(&target));
    }

    #[test]
    fn one_pointer_frame_cancels_a_zero_delay_close() {
        let now = Instant::now();
        let target = target(1, 2, 0, 0);
        let mut state = HoverState::default();
        open(&mut state, &target, now);

        state.reconcile(None, now);
        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now), None);
        assert_eq!(state.active(), Some(&target));
    }

    #[test]
    fn closes_after_leaving_the_whole_hover_family() {
        let now = Instant::now();
        let target = target(1, 2, 100, 250);
        let mut state = HoverState::default();
        open(&mut state, &target, now);

        state.reconcile(None, now + Duration::from_millis(150));
        assert_eq!(state.take_due(now + Duration::from_millis(399)), None);
        assert_eq!(
            state.take_due(now + Duration::from_millis(400)),
            Some(Command::Close(target.key))
        );
    }

    #[test]
    fn switching_targets_keeps_independent_open_and_close_deadlines() {
        let now = Instant::now();
        let first = target(1, 2, 100, 400);
        let second = target(1, 4, 200, 250);
        let mut state = HoverState::default();
        open(&mut state, &first, now);

        state.reconcile(Some(second.clone()), now + Duration::from_millis(100));
        assert_eq!(
            state.take_due(now + Duration::from_millis(300)),
            Some(Command::Open(second.clone()))
        );
        assert_eq!(state.opened(second.clone()), Some(first.key));
        assert_eq!(state.active(), Some(&second));
    }

    #[test]
    fn switching_before_open_cancels_the_stale_deadline() {
        let now = Instant::now();
        let first = target(1, 2, 100, 100);
        let second = target(1, 4, 200, 100);
        let mut state = HoverState::default();

        state.reconcile(Some(first), now);
        state.reconcile(Some(second.clone()), now + Duration::from_millis(50));
        assert_eq!(state.take_due(now + Duration::from_millis(100)), None);
        assert_eq!(
            state.take_due(now + Duration::from_millis(250)),
            Some(Command::Open(second))
        );
    }

    #[test]
    fn dismissal_suppresses_reopening_until_the_intent_leaves() {
        let now = Instant::now();
        let target = target(1, 2, 0, 0);
        let mut state = HoverState::default();
        open(&mut state, &target, now);

        state.close(&target.key, true);
        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now), None);
        state.reconcile(None, now);
        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now), Some(Command::Open(target)));
    }

    #[test]
    fn failed_open_can_be_suppressed_before_it_becomes_active() {
        let now = Instant::now();
        let target = target(1, 2, 0, 0);
        let mut state = HoverState::default();
        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now), Some(Command::Open(target.clone())));

        state.close(&target.key, true);
        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now), None);
        state.reconcile(None, now);
        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now), Some(Command::Open(target)));
    }

    #[test]
    fn a_missing_layout_defers_without_a_zero_timeout_loop() {
        let now = Instant::now();
        let target = target(1, 2, 0, 0);
        let mut state = HoverState::default();
        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now), Some(Command::Open(target.clone())));

        state.defer(target.clone());
        assert_eq!(state.next_deadline(), None);
        state.reconcile(Some(target.clone()), now);
        assert_eq!(state.take_due(now), None);
        state.resume(&target.key, now);
        assert_eq!(state.take_due(now), Some(Command::Open(target)));
    }

    #[test]
    fn output_and_global_cleanup_cancel_every_deadline() {
        let now = Instant::now();
        let first = target(1, 2, 100, 250);
        let second = target(2, 4, 100, 250);
        let mut state = HoverState::default();
        open(&mut state, &first, now);
        state.reconcile(Some(second.clone()), now);

        assert_eq!(state.remove_output(&1), Some(first.key));
        assert_eq!(state.clear(), None);
        assert_eq!(state.take_due(now + Duration::from_secs(1)), None);
    }

    #[test]
    fn removing_an_output_cancels_its_pending_open() {
        let now = Instant::now();
        let target = target(1, 2, 100, 250);
        let mut state = HoverState::default();
        state.reconcile(Some(target), now);

        assert_eq!(state.remove_output(&1), None);
        assert_eq!(state.take_due(now + Duration::from_secs(1)), None);
    }
}
