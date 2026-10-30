//! the worker against a fake logind on a private bus. every lease is a real
//! pipe: the fake keeps the read end, so closing the lease shows as hangup.

use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_io::Timer;
use calloop::ping::{PingSource, make_ping};
use thing_runtime::{SleepInhibitorAction, SleepInhibitorDefinition, SleepInhibitorStatus};
use zbus::fdo;

use super::{SleepInhibition, Transport, lock};
use crate::applications::test_bus::Bus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Behavior {
    Grant,
    Deny,
    /// answer only after `release` is set.
    Hold,
    /// delay the first call while later calls can complete.
    HoldFirst,
}

#[derive(Debug)]
struct Call {
    arguments: [String; 4],
    lease: Option<OwnedFd>,
}

#[derive(Default)]
struct Calls {
    calls: Mutex<Vec<Call>>,
    release: AtomicBool,
}

struct FakeLogind {
    calls: Arc<Calls>,
    behavior: Behavior,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl FakeLogind {
    async fn inhibit(
        &self,
        what: String,
        who: String,
        why: String,
        mode: String,
    ) -> fdo::Result<zbus::zvariant::OwnedFd> {
        let index = {
            let mut calls = lock(&self.calls.calls);
            calls.push(Call {
                arguments: [what, who, why, mode],
                lease: None,
            });
            calls.len() - 1
        };
        match self.behavior {
            Behavior::Deny => return Err(fdo::Error::AccessDenied("inhibition refused".into())),
            Behavior::Hold | Behavior::HoldFirst
                if self.behavior == Behavior::Hold || index == 0 =>
            {
                while !self.calls.release.load(Ordering::SeqCst) {
                    Timer::after(Duration::from_millis(5)).await;
                }
            }
            Behavior::Grant | Behavior::Hold | Behavior::HoldFirst => {}
        }
        let mut ends = [0; 2];
        // SAFETY: `ends` has room for the two descriptors pipe2 returns.
        let result = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) };
        assert_eq!(result, 0);
        // SAFETY: pipe2 returned two new descriptors this test now owns.
        let (read, write) =
            unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
        lock(&self.calls.calls)[index].lease = Some(read);
        Ok(write.into())
    }
}

pub(crate) struct Logind {
    _connection: zbus::Connection,
    calls: Arc<Calls>,
}

impl Logind {
    pub(crate) fn start(bus: &Bus, behavior: Behavior) -> Self {
        let calls = Arc::<Calls>::default();
        let builder = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at(
                "/org/freedesktop/login1",
                FakeLogind {
                    calls: calls.clone(),
                    behavior,
                },
            )
            .unwrap();
        Self {
            _connection: async_io::block_on(builder.build()).unwrap(),
            calls,
        }
    }

    pub(crate) fn count(&self) -> usize {
        lock(&self.calls.calls).len()
    }

    pub(crate) fn arguments(&self, index: usize) -> [String; 4] {
        lock(&self.calls.calls)[index].arguments.clone()
    }

    /// whether call `index` was granted and its lease is still open.
    pub(crate) fn open(&self, index: usize) -> bool {
        let calls = lock(&self.calls.calls);
        let Some(lease) = calls[index].lease.as_ref() else {
            return false;
        };
        let mut descriptor = libc::pollfd {
            fd: lease.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd for the duration of the call.
        assert!(unsafe { libc::poll(&mut descriptor, 1, 0) } >= 0);
        descriptor.revents & libc::POLLHUP == 0
    }

    fn granted(&self, index: usize) -> bool {
        lock(&self.calls.calls)[index].lease.is_some()
    }

    fn release(&self) {
        self.calls.release.store(true, Ordering::SeqCst);
    }
}

struct Fixture {
    inhibition: SleepInhibition,
    _source: PingSource,
}

impl Fixture {
    fn new(bus: &Bus) -> Self {
        Self::at(bus.address.clone())
    }

    fn at(address: String) -> Self {
        let (ping, source) = make_ping().unwrap();
        Self {
            inhibition: SleepInhibition::new(Transport::Address(address), ping),
            _source: source,
        }
    }

    fn declare(&mut self, definitions: &[(&str, &str)]) {
        let definitions: Vec<_> = definitions
            .iter()
            .map(|(key, reason)| SleepInhibitorDefinition {
                key: (*key).into(),
                reason: (*reason).into(),
            })
            .collect();
        self.inhibition.reconcile(&definitions);
    }

    fn start(&mut self, key: &str) {
        self.inhibition.act(key, SleepInhibitorAction::Start);
    }

    fn stop(&mut self, key: &str) {
        self.inhibition.act(key, SleepInhibitorAction::Stop);
    }

    fn status(&self, key: &str) -> SleepInhibitorStatus {
        self.inhibition.status(key).unwrap().0
    }

    fn error(&self, key: &str) -> String {
        self.inhibition.status(key).unwrap().1
    }

    /// polls at `now` until `done` holds.
    fn settle_at(&mut self, now: Instant, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.inhibition.poll(now);
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the sleep inhibitor did not settle"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn settle(&mut self, done: impl FnMut(&Self) -> bool) {
        self.settle_at(Instant::now(), done);
    }
}

fn wait(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "condition did not hold in time");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_started_inhibitor_holds_its_descriptor_until_it_stops() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::Grant);
    let mut fixture = Fixture::new(&bus);
    fixture.declare(&[("download", "a download is running")]);
    // a declaration alone requests nothing and starts no worker.
    assert_eq!(fixture.status("download"), SleepInhibitorStatus::Stopped);
    assert!(fixture.inhibition.worker.is_none());

    fixture.start("download");
    assert_eq!(fixture.status("download"), SleepInhibitorStatus::Requested);
    fixture.settle(|fixture| fixture.status("download") == SleepInhibitorStatus::Active);
    assert_eq!(
        logind.arguments(0),
        ["sleep", "thing", "a download is running", "block"].map(String::from)
    );
    assert!(logind.open(0));
    assert_eq!(fixture.inhibition.held(), ["download"]);

    // a repeated start keeps the lease and asks for nothing: a new request
    // would show as requested at once.
    fixture.start("download");
    assert_eq!(fixture.status("download"), SleepInhibitorStatus::Active);
    assert_eq!(logind.count(), 1);

    fixture.stop("download");
    assert_eq!(fixture.status("download"), SleepInhibitorStatus::Stopped);
    wait(|| !logind.open(0));
    assert!(fixture.inhibition.held().is_empty());
}

#[test]
fn held_leases_count_toward_worker_capacity() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::Grant);
    let mut fixture = Fixture::new(&bus);
    let keys: Vec<_> = (0..super::MAX_PENDING_KEYS)
        .map(|index| format!("lease-{index}"))
        .collect();
    let mut definitions: Vec<_> = keys.iter().map(|key| (key.as_str(), "work")).collect();
    definitions.push(("overflow", "more work"));
    fixture.declare(&definitions);
    for key in &keys {
        fixture.start(key);
    }
    fixture.settle(|fixture| {
        keys.iter()
            .all(|key| fixture.status(key) == SleepInhibitorStatus::Active)
    });
    fixture.start("overflow");
    fixture.settle(|fixture| fixture.status("overflow") != SleepInhibitorStatus::Requested);
    assert_eq!(
        fixture.status("overflow"),
        SleepInhibitorStatus::Unavailable
    );
    assert_eq!(logind.count(), super::MAX_PENDING_KEYS);

    fixture.stop("overflow");
    fixture.stop(&keys[0]);
    wait(|| {
        !super::lock(&fixture.inhibition.worker.as_ref().unwrap().shared)
            .outstanding
            .contains(&keys[0])
    });
    fixture.start("overflow");
    fixture.settle(|fixture| fixture.status("overflow") == SleepInhibitorStatus::Active);
    assert_eq!(fixture.inhibition.held().len(), super::MAX_PENDING_KEYS);
    assert_eq!(logind.count(), super::MAX_PENDING_KEYS + 1);
}

#[test]
fn completed_calls_keep_capacity_until_their_reports_are_consumed() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::Deny);
    let mut fixture = Fixture::new(&bus);
    let keys: Vec<_> = (0..super::MAX_PENDING_KEYS)
        .map(|index| format!("lease-{index}"))
        .collect();
    let mut definitions: Vec<_> = keys.iter().map(|key| (key.as_str(), "work")).collect();
    definitions.push(("overflow", "more work"));
    fixture.declare(&definitions);
    for key in &keys {
        fixture.start(key);
    }
    wait(|| {
        super::lock(&fixture.inhibition.worker.as_ref().unwrap().shared)
            .reports
            .len()
            == super::MAX_PENDING_KEYS
    });
    fixture.start("overflow");
    assert_eq!(
        fixture.status("overflow"),
        SleepInhibitorStatus::Unavailable
    );
    assert_eq!(logind.count(), super::MAX_PENDING_KEYS);

    fixture.settle(|fixture| {
        keys.iter()
            .all(|key| fixture.status(key) == SleepInhibitorStatus::Failed)
    });
    wait(|| {
        super::lock(&fixture.inhibition.worker.as_ref().unwrap().shared)
            .outstanding
            .is_empty()
    });
    fixture.stop("overflow");
    fixture.start("overflow");
    fixture.settle(|fixture| fixture.status("overflow") == SleepInhibitorStatus::Failed);
    assert_eq!(logind.count(), super::MAX_PENDING_KEYS + 1);
}

#[test]
fn restarting_a_pending_key_coalesces_until_its_old_reply_arrives() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::HoldFirst);
    let mut fixture = Fixture::new(&bus);
    fixture.declare(&[("a", "work"), ("b", "barrier")]);
    fixture.start("a");
    wait(|| logind.count() == 1);
    fixture.stop("a");
    fixture.start("a");
    fixture.start("b");
    fixture.settle(|fixture| fixture.status("b") == SleepInhibitorStatus::Active);
    assert_eq!(
        logind.count(),
        2,
        "the old call and the barrier are pending or complete"
    );
    assert_eq!(fixture.status("a"), SleepInhibitorStatus::Requested);

    logind.release();
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Active);
    wait(|| logind.granted(0) && !logind.open(0));
    assert_eq!(logind.count(), 3);
    assert!(logind.open(2));
}

#[test]
fn reloads_keep_unchanged_leases_and_release_replaced_or_retired_ones() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::Grant);
    let mut fixture = Fixture::new(&bus);
    fixture.declare(&[("a", "one"), ("b", "two")]);
    fixture.start("a");
    fixture.start("b");
    fixture.settle(|fixture| {
        fixture.status("a") == SleepInhibitorStatus::Active
            && fixture.status("b") == SleepInhibitorStatus::Active
    });
    let index = |key: &str| {
        (0..logind.count())
            .find(|index| logind.arguments(*index)[2] == if key == "a" { "one" } else { "two" })
            .unwrap()
    };
    let (a, b) = (index("a"), index("b"));

    // an unchanged declaration keeps its lease without a new request.
    fixture.declare(&[("a", "one"), ("b", "two")]);
    assert_eq!(fixture.status("a"), SleepInhibitorStatus::Active);
    assert_eq!(fixture.status("b"), SleepInhibitorStatus::Active);
    assert_eq!(logind.count(), 2);
    assert!(logind.open(a) && logind.open(b));

    // a changed reason replaces the lease; the retired key releases its own.
    fixture.declare(&[("a", "changed")]);
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Active);
    wait(|| !logind.open(a) && !logind.open(b));
    assert_eq!(logind.arguments(2)[2], "changed");
    assert!(logind.open(2));
    assert!(fixture.inhibition.status("b").is_none());

    // the last owner leaving releases the rest.
    fixture.declare(&[]);
    wait(|| !logind.open(2));
}

#[test]
fn a_reply_that_arrives_after_stop_closes_its_descriptor() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::Hold);
    let mut fixture = Fixture::new(&bus);
    fixture.declare(&[("a", "reason")]);
    fixture.start("a");
    wait(|| logind.count() == 1);
    fixture.stop("a");
    logind.release();
    wait(|| logind.granted(0));
    wait(|| !logind.open(0));
    fixture.inhibition.poll(Instant::now());
    assert_eq!(fixture.status("a"), SleepInhibitorStatus::Stopped);
    assert!(fixture.inhibition.held().is_empty());
}

#[test]
fn denials_fail_without_retrying() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::Deny);
    let mut fixture = Fixture::new(&bus);
    fixture.declare(&[("a", "reason")]);
    fixture.start("a");
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Failed);
    assert!(
        fixture.error("a").contains("AccessDenied"),
        "{}",
        fixture.error("a")
    );
    assert_eq!(fixture.inhibition.next_deadline(), None);
    // on the controlled clock, an hour later nothing is due.
    fixture
        .inhibition
        .poll(Instant::now() + Duration::from_secs(3600));
    assert_eq!(fixture.status("a"), SleepInhibitorStatus::Failed);
    assert_eq!(logind.count(), 1);
    // starting again is an explicit new request.
    fixture.start("a");
    wait(|| logind.count() == 2);
}

#[test]
fn a_missing_or_replaced_logind_is_unavailable_until_one_appears() {
    let bus = Bus::start(&[]);
    let mut fixture = Fixture::new(&bus);
    fixture.declare(&[("a", "reason")]);
    fixture.start("a");
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Unavailable);
    assert_eq!(fixture.error("a"), "logind is not running");
    assert!(fixture.inhibition.next_deadline().is_some());

    // logind appearing retries at once rather than at the deadline.
    let first = Logind::start(&bus, Behavior::Grant);
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Active);
    assert!(first.open(0));

    // the daemon leaving takes its lease with it.
    let first_calls = first.calls.clone();
    drop(first);
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Unavailable);
    assert!(fixture.inhibition.held().is_empty());
    let hung_up = {
        let calls = lock(&first_calls.calls);
        let mut descriptor = libc::pollfd {
            fd: calls[0].lease.as_ref().unwrap().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd for the duration of the call.
        unsafe { libc::poll(&mut descriptor, 1, 0) };
        descriptor.revents & libc::POLLHUP != 0
    };
    assert!(hung_up);

    // a replacement daemon receives a new request.
    let second = Logind::start(&bus, Behavior::Grant);
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Active);
    assert!(second.open(0));
}

#[test]
fn a_lost_bus_releases_leases_and_retries_on_the_controlled_clock() {
    let mut bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::Grant);
    let mut fixture = Fixture::new(&bus);
    fixture.declare(&[("a", "reason")]);
    fixture.start("a");
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Active);

    bus.stop();
    fixture.settle(|fixture| fixture.status("a") == SleepInhibitorStatus::Unavailable);
    wait(|| !logind.open(0));
    assert!(
        fixture.error("a").contains("system bus"),
        "{}",
        fixture.error("a")
    );
    drop(logind);

    bus.spawn();
    let logind = Logind::start(&bus, Behavior::Grant);
    // nothing is retried before the deadline.
    let deadline = fixture.inhibition.next_deadline().unwrap();
    fixture.inhibition.poll(deadline - Duration::from_millis(1));
    assert_eq!(fixture.status("a"), SleepInhibitorStatus::Unavailable);
    fixture.settle_at(deadline, |fixture| {
        fixture.status("a") == SleepInhibitorStatus::Active
    });
    assert!(logind.open(0));
}

#[test]
fn retries_stop_after_a_bounded_number_of_attempts() {
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::at(format!(
        "unix:path={}",
        directory.path().join("missing").display()
    ));
    fixture.declare(&[("a", "reason")]);
    fixture.start("a");
    let mut attempts = 0;
    while fixture.status("a") != SleepInhibitorStatus::Failed {
        let now = fixture
            .inhibition
            .next_deadline()
            .unwrap_or_else(Instant::now);
        let before = attempts;
        fixture.settle_at(now, |fixture| {
            fixture.status("a") != SleepInhibitorStatus::Requested
        });
        attempts = before + 1;
        assert!(attempts <= 8, "retries are not bounded");
    }
    assert_eq!(attempts, 8);
    assert!(fixture.error("a").ends_with("stopped retrying"));
    assert_eq!(fixture.inhibition.next_deadline(), None);
}

#[test]
fn shutdown_is_bounded_and_closes_every_lease() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, Behavior::Hold);
    logind.release();
    let mut fixture = Fixture::new(&bus);
    fixture.declare(&[("granted", "reason"), ("pending", "reason")]);
    fixture.start("granted");
    fixture.settle(|fixture| fixture.status("granted") == SleepInhibitorStatus::Active);
    // the next request is never answered while the worker shuts down.
    logind.calls.release.store(false, Ordering::SeqCst);
    fixture.start("pending");
    wait(|| logind.count() == 2);

    let started = Instant::now();
    drop(fixture);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!logind.open(0));
    logind.release();
}
