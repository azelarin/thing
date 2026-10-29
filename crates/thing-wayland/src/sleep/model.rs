//! which sleep leases the shell wants, independent of the bus. the worker owns
//! the descriptors; this model decides what to request and what to report.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use thing_runtime::{SleepInhibitorDefinition, SleepInhibitorState, SleepInhibitorStatus};

/// unavailable requests are retried with doubling delays up to this cap, and
/// at most `MAX_ATTEMPTS` times. logind appearing on the bus retries at once.
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);
const MAX_ATTEMPTS: u32 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// request a lease; a newer generation replaces any older request.
    Acquire {
        key: String,
        generation: u64,
        reason: String,
    },
    /// close the lease for `key` and ignore any reply still in flight.
    Release { key: String },
}

/// why a request did not produce a lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// the bus or logind could not be reached; retrying may help.
    Unavailable(String),
    /// logind answered with an error; retrying would repeat it.
    Denied(String),
}

#[derive(Debug)]
struct Lease {
    reason: String,
    wanted: bool,
    generation: u64,
    status: SleepInhibitorStatus,
    error: String,
    attempts: u32,
    retry_at: Option<Instant>,
}

#[derive(Debug, Default)]
pub(crate) struct Leases {
    leases: BTreeMap<String, Lease>,
    next_generation: u64,
}

impl Leases {
    /// applies the declarations of the accepted configurations. unchanged keys
    /// keep their lease; removed keys and changed reasons release it.
    pub(crate) fn reconcile(&mut self, definitions: &[SleepInhibitorDefinition]) -> Vec<Command> {
        let mut commands = Vec::new();
        let declared: BTreeMap<_, _> = definitions
            .iter()
            .map(|definition| (definition.key.as_str(), definition.reason.as_str()))
            .collect();
        self.leases.retain(|key, lease| {
            let keep = declared.contains_key(key.as_str());
            if !keep && lease.wanted {
                commands.push(Command::Release { key: key.clone() });
            }
            keep
        });
        for (key, reason) in declared {
            match self.leases.get_mut(key) {
                Some(lease) if lease.reason == reason => {}
                Some(lease) => {
                    lease.reason = reason.to_owned();
                    if lease.wanted {
                        commands.push(Command::Release {
                            key: key.to_owned(),
                        });
                        let generation = next(&mut self.next_generation);
                        commands.push(lease.request(key, generation));
                    }
                }
                None => {
                    self.leases.insert(
                        key.to_owned(),
                        Lease {
                            reason: reason.to_owned(),
                            wanted: false,
                            generation: 0,
                            status: SleepInhibitorStatus::Stopped,
                            error: String::new(),
                            attempts: 0,
                            retry_at: None,
                        },
                    );
                }
            }
        }
        commands
    }

    /// starts or stops a declared lease. an unknown key does nothing, because
    /// its declaration was retired after the action was queued.
    pub(crate) fn set_wanted(&mut self, key: &str, wanted: bool) -> Vec<Command> {
        let Some(lease) = self.leases.get_mut(key) else {
            return Vec::new();
        };
        if !wanted {
            if !lease.wanted {
                return Vec::new();
            }
            lease.wanted = false;
            lease.status = SleepInhibitorStatus::Stopped;
            lease.error.clear();
            lease.retry_at = None;
            return vec![Command::Release {
                key: key.to_owned(),
            }];
        }
        // a running request or a held lease already satisfies a repeated start;
        // a failed one starts over.
        if lease.wanted && lease.status != SleepInhibitorStatus::Failed {
            return Vec::new();
        }
        lease.wanted = true;
        lease.attempts = 0;
        let generation = next(&mut self.next_generation);
        vec![lease.request(key, generation)]
    }

    pub(crate) fn acquired(&mut self, key: &str, generation: u64) {
        if let Some(lease) = current(&mut self.leases, key, generation) {
            lease.status = SleepInhibitorStatus::Active;
            lease.error.clear();
            lease.attempts = 0;
        }
    }

    pub(crate) fn refused(&mut self, key: &str, generation: u64, refusal: Refusal, now: Instant) {
        let Some(lease) = current(&mut self.leases, key, generation) else {
            return;
        };
        match refusal {
            Refusal::Denied(error) => {
                lease.status = SleepInhibitorStatus::Failed;
                lease.error = error;
            }
            Refusal::Unavailable(error) => {
                lease.attempts += 1;
                if lease.attempts >= MAX_ATTEMPTS {
                    lease.status = SleepInhibitorStatus::Failed;
                    lease.error = format!("{error}; stopped retrying");
                } else {
                    lease.status = SleepInhibitorStatus::Unavailable;
                    lease.error = error;
                    let delay = FIRST_RETRY
                        .saturating_mul(1 << (lease.attempts - 1).min(5))
                        .min(MAX_RETRY);
                    lease.retry_at = Some(now + delay);
                }
            }
        }
    }

    /// the worker dropped a lease because logind left or was replaced. the
    /// request starts again at once, so a new daemon gets it promptly.
    pub(crate) fn lost(&mut self, key: &str, generation: u64) -> Vec<Command> {
        let Some(lease) = current(&mut self.leases, key, generation) else {
            return Vec::new();
        };
        lease.attempts = 0;
        let generation = next(&mut self.next_generation);
        vec![lease.request(key, generation)]
    }

    /// logind gained an owner: unavailable requests retry without waiting.
    pub(crate) fn daemon_appeared(&mut self) -> Vec<Command> {
        let mut commands = Vec::new();
        for (key, lease) in &mut self.leases {
            if lease.wanted && lease.status == SleepInhibitorStatus::Unavailable {
                lease.attempts = 0;
                let generation = next(&mut self.next_generation);
                commands.push(lease.request(key, generation));
            }
        }
        commands
    }

    pub(crate) fn due(&mut self, now: Instant) -> Vec<Command> {
        let mut commands = Vec::new();
        for (key, lease) in &mut self.leases {
            if lease.wanted && lease.retry_at.is_some_and(|deadline| deadline <= now) {
                let generation = next(&mut self.next_generation);
                commands.push(lease.request(key, generation));
            }
        }
        commands
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.leases
            .values()
            .filter(|lease| lease.wanted)
            .filter_map(|lease| lease.retry_at)
            .min()
    }

    pub(crate) fn states(&self) -> Vec<SleepInhibitorState> {
        self.leases
            .iter()
            .map(|(key, lease)| SleepInhibitorState {
                key: key.clone(),
                status: lease.status,
                error: lease.error.clone(),
            })
            .collect()
    }
}

impl Lease {
    fn request(&mut self, key: &str, generation: u64) -> Command {
        self.generation = generation;
        self.status = SleepInhibitorStatus::Requested;
        self.retry_at = None;
        Command::Acquire {
            key: key.to_owned(),
            generation,
            reason: self.reason.clone(),
        }
    }
}

/// the lease a report is about, unless it was stopped or asked again since.
fn current<'a>(
    leases: &'a mut BTreeMap<String, Lease>,
    key: &str,
    generation: u64,
) -> Option<&'a mut Lease> {
    leases
        .get_mut(key)
        .filter(|lease| lease.wanted && lease.generation == generation)
}

fn next(counter: &mut u64) -> u64 {
    *counter += 1;
    *counter
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(key: &str, reason: &str) -> SleepInhibitorDefinition {
        SleepInhibitorDefinition {
            key: key.into(),
            reason: reason.into(),
        }
    }

    fn status(leases: &Leases, key: &str) -> (SleepInhibitorStatus, String) {
        let state = leases
            .states()
            .into_iter()
            .find(|state| state.key == key)
            .unwrap();
        (state.status, state.error)
    }

    fn acquire(command: &Command) -> u64 {
        match command {
            Command::Acquire { generation, .. } => *generation,
            other => panic!("expected an acquire, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_declarations_keep_their_lease_and_changes_replace_it() {
        let mut leases = Leases::default();
        assert!(leases.reconcile(&[definition("a", "one")]).is_empty());
        let start = leases.set_wanted("a", true);
        let generation = acquire(&start[0]);
        assert!(leases.set_wanted("a", true).is_empty());
        leases.acquired("a", generation);
        assert_eq!(status(&leases, "a").0, SleepInhibitorStatus::Active);

        // an unchanged reload sends nothing, so the lease has no gap.
        assert!(leases.reconcile(&[definition("a", "one")]).is_empty());
        // a changed reason releases the old lease and requests a new one.
        let commands = leases.reconcile(&[definition("a", "two")]);
        assert_eq!(commands[0], Command::Release { key: "a".into() });
        assert!(matches!(&commands[1], Command::Acquire { reason, .. } if reason == "two"));
        // the old generation's late answer cannot revive anything.
        leases.acquired("a", generation);
        assert_eq!(status(&leases, "a").0, SleepInhibitorStatus::Requested);
        // the last owner leaving releases the lease.
        assert_eq!(
            leases.reconcile(&[]),
            [Command::Release { key: "a".into() }]
        );
        assert!(leases.states().is_empty());
        assert!(leases.set_wanted("a", true).is_empty());
    }

    #[test]
    fn stopping_ignores_late_answers_and_retries_follow_a_bounded_policy() {
        let mut leases = Leases::default();
        leases.reconcile(&[definition("a", "reason")]);
        let generation = acquire(&leases.set_wanted("a", true)[0]);
        assert_eq!(
            leases.set_wanted("a", false),
            [Command::Release { key: "a".into() }]
        );
        leases.acquired("a", generation);
        assert_eq!(status(&leases, "a").0, SleepInhibitorStatus::Stopped);

        let now = Instant::now();
        let mut generation = acquire(&leases.set_wanted("a", true)[0]);
        let mut expected = now;
        for attempt in 1..MAX_ATTEMPTS {
            leases.refused(
                "a",
                generation,
                Refusal::Unavailable("no logind".into()),
                expected,
            );
            assert_eq!(status(&leases, "a").0, SleepInhibitorStatus::Unavailable);
            let delay = FIRST_RETRY
                .saturating_mul(1 << (attempt - 1).min(5))
                .min(MAX_RETRY);
            assert_eq!(leases.next_deadline(), Some(expected + delay));
            // nothing is due one instant before the deadline.
            assert!(
                leases
                    .due(expected + delay - Duration::from_nanos(1))
                    .is_empty()
            );
            expected += delay;
            generation = acquire(&leases.due(expected)[0]);
        }
        leases.refused(
            "a",
            generation,
            Refusal::Unavailable("no logind".into()),
            expected,
        );
        let (state, error) = status(&leases, "a");
        assert_eq!(state, SleepInhibitorStatus::Failed);
        assert!(error.ends_with("stopped retrying"), "{error}");
        assert_eq!(leases.next_deadline(), None);
        // a new start begins a fresh request after a failure.
        assert_eq!(leases.set_wanted("a", true).len(), 1);
    }

    #[test]
    fn denials_fail_without_retry_and_lost_leases_start_again() {
        let mut leases = Leases::default();
        leases.reconcile(&[definition("a", "reason"), definition("b", "reason")]);
        let now = Instant::now();
        let a = acquire(&leases.set_wanted("a", true)[0]);
        leases.refused("a", a, Refusal::Denied("access denied".into()), now);
        assert_eq!(
            status(&leases, "a"),
            (SleepInhibitorStatus::Failed, "access denied".into())
        );
        assert_eq!(leases.next_deadline(), None);

        let b = acquire(&leases.set_wanted("b", true)[0]);
        leases.acquired("b", b);
        let again = leases.lost("b", b);
        assert_ne!(acquire(&again[0]), b);
        assert_eq!(status(&leases, "b").0, SleepInhibitorStatus::Requested);
        // a stale loss report changes nothing.
        assert!(leases.lost("b", b).is_empty());

        let unavailable = acquire(&leases.set_wanted("a", true)[0]);
        leases.refused("a", unavailable, Refusal::Unavailable("gone".into()), now);
        let commands = leases.daemon_appeared();
        assert_eq!(commands.len(), 1);
        assert!(matches!(&commands[0], Command::Acquire { key, .. } if key == "a"));
    }
}
