use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::MutexGuard,
    thread,
    time::{Duration, Instant},
};

use super::*;

static WORKER_TEST: Mutex<()> = Mutex::new(());
pub(crate) struct WorkerTestGuard {
    _guard: MutexGuard<'static, ()>,
}
pub(crate) fn worker_guard() -> WorkerTestGuard {
    WorkerTestGuard {
        _guard: lock(&WORKER_TEST),
    }
}
impl Drop for WorkerTestGuard {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while worker::active() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(!worker::active(), "file worker did not stop");
    }
}

fn subscription(
    output: u32,
    consumer: &str,
    key: &str,
    path: &std::path::Path,
) -> Subscription<u32> {
    Subscription {
        output,
        consumer: consumer.into(),
        definition: FileWatchDefinition {
            key: key.into(),
            path: path.to_str().unwrap().into(),
        },
    }
}

struct Fixture {
    monitor: FileWatches<u32>,
    _source: calloop::ping::PingSource,
    _worker: WorkerTestGuard,
}
impl Fixture {
    fn new() -> Self {
        let guard = worker_guard();
        let (ping, source) = calloop::ping::make_ping().unwrap();
        Self {
            monitor: FileWatches::new(ping),
            _source: source,
            _worker: guard,
        }
    }
    fn wait(&mut self, expected: usize) -> Vec<Delivery<u32>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while seen.len() < expected {
            seen.extend(self.monitor.take_deliveries());
            assert!(Instant::now() < deadline, "file update was not delivered");
            thread::sleep(Duration::from_millis(2));
        }
        seen
    }
    fn wait_ready(&mut self, count: usize, expected: &str) -> Vec<Delivery<u32>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let expected = FileWatchSnapshot::Ready(Arc::from(expected));
        let mut seen = BTreeMap::new();
        while seen.len() < count {
            for update in self.monitor.take_deliveries() {
                if update.snapshot == expected {
                    let key = (update.output, update.consumer.clone(), update.key.clone());
                    seen.insert(key, update);
                }
            }
            assert!(
                Instant::now() < deadline,
                "expected file contents were not delivered"
            );
            thread::sleep(Duration::from_millis(2));
        }
        seen.into_values().collect()
    }
    fn ready(&mut self, expected: &str) {
        let updates = self.wait_ready(1, expected);
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].snapshot,
            FileWatchSnapshot::Ready(Arc::from(expected))
        );
    }
}

#[test]
fn changes_replacements_missing_parents_and_output_subscriptions_share_one_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("data");
    let path = parent.join("value.txt");
    let mut fixture = Fixture::new();
    fixture
        .monitor
        .reconcile(vec![
            subscription(1, "first", "data", &path),
            subscription(2, "second", "data", &path),
        ])
        .unwrap();
    let missing = fixture.wait(2);
    assert!(
        missing
            .iter()
            .all(|item| item.snapshot == FileWatchSnapshot::Missing)
    );
    fs::create_dir(&parent).unwrap();
    fs::write(&path, "one").unwrap();
    let first = fixture.wait_ready(2, "one");
    assert_eq!(first.len(), 2);
    let [FileWatchSnapshot::Ready(a), FileWatchSnapshot::Ready(b)] =
        [&first[0].snapshot, &first[1].snapshot]
    else {
        panic!("expected ready snapshots");
    };
    assert_eq!(&**a, "one");
    assert!(Arc::ptr_eq(a, b));
    assert_eq!((first[0].output, first[1].output), (1, 2));
    let replacement = parent.join("replacement");
    fs::write(&replacement, "two").unwrap();
    fs::rename(replacement, &path).unwrap();
    let next = fixture.wait_ready(2, "two");
    assert!(
        next.iter()
            .all(|item| item.snapshot == FileWatchSnapshot::Ready(Arc::from("two")))
    );
    // a replacement generation gets the current data even when it is equal.
    fixture
        .monitor
        .reconcile(vec![subscription(1, "replacement", "data", &path)])
        .unwrap();
    let current = fixture.wait(1);
    assert_eq!(current[0].consumer, "replacement");
    assert_eq!(
        current[0].snapshot,
        FileWatchSnapshot::Ready(Arc::from("two"))
    );
    let old = directory.path().join("old");
    fs::rename(&parent, &old).unwrap();
    assert_eq!(fixture.wait(1)[0].snapshot, FileWatchSnapshot::Missing);
    fs::create_dir(&parent).unwrap();
    fs::write(&path, "three").unwrap();
    fixture.ready("three");
    fixture.monitor.reconcile(Vec::new()).unwrap();
    fs::write(&path, "retired").unwrap();
    thread::sleep(Duration::from_millis(40));
    assert!(fixture.monitor.take_deliveries().is_empty());
}

#[test]
fn symlink_targets_intermediate_links_and_permission_changes_remain_observed() {
    let directory = tempfile::tempdir().unwrap();
    let a = directory.path().join("a");
    let b = directory.path().join("b");
    let alias = directory.path().join("alias");
    let middle = directory.path().join("middle");
    fs::write(&a, "a").unwrap();
    fs::write(&b, "b").unwrap();
    symlink(&a, &middle).unwrap();
    symlink(&middle, &alias).unwrap();
    let mut fixture = Fixture::new();
    fixture
        .monitor
        .reconcile(vec![subscription(1, "owner", "data", &alias)])
        .unwrap();
    fixture.ready("a");
    fs::write(&a, "a changed").unwrap();
    fixture.ready("a changed");
    let replacement = directory.path().join("replacement");
    symlink(&b, &replacement).unwrap();
    fs::rename(replacement, &middle).unwrap();
    fixture.ready("b");
    fs::write(&b, "b changed").unwrap();
    fixture.ready("b changed");
    fs::set_permissions(&b, fs::Permissions::from_mode(0o0)).unwrap();
    let unreadable = fixture.wait(1);
    assert!(matches!(
        unreadable[0].snapshot,
        FileWatchSnapshot::Failed(_)
    ));
    fs::set_permissions(&b, fs::Permissions::from_mode(0o600)).unwrap();
    fixture.ready("b changed");
}

#[test]
fn invalid_text_large_files_and_pipes_are_rejected_and_can_be_replaced() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data");
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a live nul-terminated path; mkfifo retains no pointers.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let mut fixture = Fixture::new();
    fixture
        .monitor
        .reconcile(vec![subscription(1, "owner", "data", &path)])
        .unwrap();
    assert!(matches!(
        fixture.wait(1)[0].snapshot,
        FileWatchSnapshot::Failed(_)
    ));
    for bytes in [
        b"\xff".to_vec(),
        b"a\0b".to_vec(),
        vec![b'x'; thing_runtime::MAX_WATCHED_FILE_BYTES + 1],
    ] {
        let next = directory.path().join("next");
        fs::write(&next, bytes).unwrap();
        fs::rename(next, &path).unwrap();
        assert!(matches!(
            fixture.wait(1)[0].snapshot,
            FileWatchSnapshot::Failed(_)
        ));
    }
    fs::write(&path, "recovered").unwrap();
    fixture.ready("recovered");
}

#[test]
fn publications_coalesce_and_superseded_workers_cannot_reach_new_consumers() {
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let mut monitor = FileWatches::new(ping.clone());
    monitor.subscriptions = vec![subscription(
        1,
        "new",
        "data",
        std::path::Path::new("/data"),
    )];
    lock(&monitor.shared).generation = 2;
    for n in 0..1000 {
        publish(
            &monitor.shared,
            &ping,
            2,
            "/data",
            FileWatchSnapshot::Ready(Arc::from(n.to_string())),
        );
    }
    publish(
        &monitor.shared,
        &ping,
        1,
        "/data",
        FileWatchSnapshot::Ready(Arc::from("stale")),
    );
    assert_eq!(lock(&monitor.shared).snapshots.len(), 1);
    let last = monitor.take_deliveries();
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].snapshot, FileWatchSnapshot::Ready(Arc::from("999")));
    assert!(monitor.take_deliveries().is_empty());
    publish(
        &monitor.shared,
        &ping,
        2,
        "/data",
        FileWatchSnapshot::Ready(Arc::from("999")),
    );
    assert!(monitor.take_deliveries().is_empty());
}

#[test]
fn a_busy_file_cannot_starve_other_callbacks_or_form_a_batch() {
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let mut monitor = FileWatches::new(ping.clone());
    lock(&monitor.shared).generation = 1;
    for output in 1..=3 {
        let path = format!("/data-{output}");
        monitor.subscriptions.push(subscription(
            output,
            &format!("owner-{output}"),
            "data",
            std::path::Path::new(&path),
        ));
        publish(
            &monitor.shared,
            &ping,
            1,
            &path,
            FileWatchSnapshot::Ready(Arc::from("initial")),
        );
    }
    let mut order = Vec::new();
    for revision in 0..3 {
        let delivered = monitor.take_deliveries();
        assert_eq!(delivered.len(), 1);
        order.push(delivered[0].output);
        publish(
            &monitor.shared,
            &ping,
            1,
            "/data-1",
            FileWatchSnapshot::Ready(Arc::from(revision.to_string())),
        );
    }
    assert_eq!(order, [1, 2, 3]);
}
