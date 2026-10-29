#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Requesting,
    Locked,
    FailedLocked,
    UnlockSync,
    Finished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Action {
    None,
    DestroyAndExit,
    UnlockAndSync,
}

pub(super) struct Lifecycle {
    phase: Phase,
}

impl Lifecycle {
    pub(super) fn new() -> Self {
        Self {
            phase: Phase::Requesting,
        }
    }

    pub(super) fn phase(&self) -> Phase {
        self.phase
    }

    pub(super) fn locked(&mut self) {
        if self.phase == Phase::Requesting {
            self.phase = Phase::Locked;
        }
    }

    pub(super) fn authenticated(&mut self) -> Action {
        if self.phase != Phase::Locked {
            return Action::None;
        }
        self.phase = Phase::UnlockSync;
        Action::UnlockAndSync
    }

    pub(super) fn failed(&mut self) -> Action {
        match self.phase {
            Phase::Requesting => {
                self.phase = Phase::Finished;
                Action::DestroyAndExit
            }
            Phase::Locked => {
                self.phase = Phase::FailedLocked;
                Action::None
            }
            Phase::FailedLocked | Phase::UnlockSync | Phase::Finished => Action::None,
        }
    }

    pub(super) fn compositor_finished(&mut self) -> Action {
        match self.phase {
            Phase::Requesting => {
                self.phase = Phase::Finished;
                Action::DestroyAndExit
            }
            Phase::Locked | Phase::FailedLocked => {
                self.phase = Phase::UnlockSync;
                Action::UnlockAndSync
            }
            Phase::UnlockSync | Phase::Finished => Action::None,
        }
    }

    pub(super) fn sync_done(&mut self) -> bool {
        if self.phase != Phase::UnlockSync {
            return false;
        }
        self.phase = Phase::Finished;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_unlocks_only_an_active_healthy_lock() {
        let mut lifecycle = Lifecycle::new();
        assert_eq!(lifecycle.authenticated(), Action::None);
        lifecycle.locked();
        assert_eq!(lifecycle.authenticated(), Action::UnlockAndSync);
        assert_eq!(lifecycle.phase(), Phase::UnlockSync);
        assert!(lifecycle.sync_done());
        assert_eq!(lifecycle.phase(), Phase::Finished);
    }

    #[test]
    fn a_failure_after_locking_stays_locked() {
        let mut lifecycle = Lifecycle::new();
        lifecycle.locked();
        assert_eq!(lifecycle.failed(), Action::None);
        assert_eq!(lifecycle.phase(), Phase::FailedLocked);
        assert_eq!(lifecycle.authenticated(), Action::None);
        assert!(!lifecycle.sync_done());
    }

    #[test]
    fn a_failure_before_locking_destroys_without_unlocking() {
        let mut lifecycle = Lifecycle::new();
        assert_eq!(lifecycle.failed(), Action::DestroyAndExit);
        assert_eq!(lifecycle.phase(), Phase::Finished);
    }

    #[test]
    fn compositor_finished_follows_the_protocol_state() {
        let mut denied = Lifecycle::new();
        assert_eq!(denied.compositor_finished(), Action::DestroyAndExit);

        let mut alternate_unlock = Lifecycle::new();
        alternate_unlock.locked();
        assert_eq!(
            alternate_unlock.compositor_finished(),
            Action::UnlockAndSync
        );
        assert_eq!(alternate_unlock.phase(), Phase::UnlockSync);
    }

    #[test]
    fn late_events_cannot_reverse_a_terminal_transition() {
        let mut failed = Lifecycle::new();
        assert_eq!(failed.failed(), Action::DestroyAndExit);
        failed.locked();
        assert_eq!(failed.authenticated(), Action::None);
        assert_eq!(failed.compositor_finished(), Action::None);
        assert_eq!(failed.phase(), Phase::Finished);

        let mut unlocking = Lifecycle::new();
        unlocking.locked();
        assert_eq!(unlocking.authenticated(), Action::UnlockAndSync);
        assert_eq!(unlocking.failed(), Action::None);
        assert_eq!(unlocking.authenticated(), Action::None);
        assert!(unlocking.sync_done());
        assert!(!unlocking.sync_done());
    }
}
