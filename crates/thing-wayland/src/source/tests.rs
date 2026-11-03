use super::*;
use std::sync::{atomic::AtomicUsize, mpsc};
use tempfile::tempdir;

static TEST_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn worker_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while ACTIVE.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "previous source worker did not stop"
        );
        std::thread::yield_now();
    }
    guard
}

#[test]
fn slow_and_retired_reads_leave_one_worker_and_one_pending_request() {
    let _guard = worker_guard();
    let directory = tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    std::fs::write(&path, "return ui.panel {}").unwrap();
    let (entered, active) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let worker = Rc::new(
        SourceWorker::with_reader(move |path| {
            assert_eq!(std::thread::current().name(), Some("thing-source"));
            count.fetch_add(1, Ordering::Relaxed);
            if path == Path::new("blocked") {
                entered.send(()).unwrap();
                wait.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                Err("retired read".into())
            } else {
                SourceFile::read(path)
            }
        })
        .unwrap(),
    );
    let first = worker.session();
    let old = first.read("blocked".into());
    active
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    drop(old);
    drop(first);
    let second = worker.session();
    let superseded = second.read("superseded".into());
    let latest = worker.session();
    let current = latest.read(path.clone());
    assert!(
        futures_lite::future::block_on(superseded)
            .unwrap_err()
            .contains("superseded")
    );
    release.send(()).unwrap();
    let source = futures_lite::future::block_on(current).unwrap();
    assert_eq!(source.size_bytes(), 18);
    futures_lite::future::block_on(latest.read(path)).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert!(futures_lite::future::block_on(latest.validate()).unwrap());
    std::fs::write(
        directory.path().join("panel.luau"),
        "return ui.panel { namespace='new' }",
    )
    .unwrap();
    assert!(!futures_lite::future::block_on(latest.validate()).unwrap());
    drop(second);
    drop(latest);
    let thread = worker._thread.thread().clone();
    drop(worker);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while ACTIVE.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "source worker did not stop"
        );
        thread.unpark();
        std::thread::yield_now();
    }
}

#[test]
fn sessions_bound_cached_source_bytes_without_charging_repeated_reads() {
    let _guard = worker_guard();
    let directory = tempdir().unwrap();
    let path = directory.path().join("large.luau");
    std::fs::write(&path, vec![b' '; 1024 * 1024]).unwrap();
    let source = SourceFile::read(&path).unwrap();
    let worker = Rc::new(SourceWorker::with_reader(move |_| Ok(source.clone())).unwrap());
    let session = worker.session();
    for index in 0..16 {
        let path = PathBuf::from(index.to_string());
        futures_lite::future::block_on(session.read(path.clone())).unwrap();
        futures_lite::future::block_on(session.read(path)).unwrap();
    }
    assert!(
        futures_lite::future::block_on(session.read("overflow".into()))
            .unwrap_err()
            .contains("16 MiB")
    );
    assert!(
        futures_lite::future::block_on(session.read("overflow".into()))
            .unwrap_err()
            .contains("16 MiB")
    );
}

#[test]
fn dropping_the_worker_does_not_wait_for_a_blocked_filesystem_read() {
    let _guard = worker_guard();
    let (entered, active) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let worker = Rc::new(
        SourceWorker::with_reader(move |_| {
            entered.send(()).unwrap();
            wait.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            Err("released".into())
        })
        .unwrap(),
    );
    let session = worker.session();
    let read = session.read("blocked".into());
    active
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    drop(read);
    drop(session);
    drop(worker);
    assert!(ACTIVE.load(Ordering::Acquire));
    assert!(SourceWorker::new().is_err());
    release.send(()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while ACTIVE.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "retired reader did not release its slot"
        );
        std::thread::yield_now();
    }
}
