//! source reads for unpublished configuration runtimes.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
};

use thing_runtime::{SourceFile, SourceReader};

static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Lease;

impl Drop for Lease {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

const MAX_FILES: usize = 1024;
const MAX_BYTES: usize = 16 * 1024 * 1024;

type ReadResult = Result<SourceFile, String>;

#[derive(Default)]
struct Delivery {
    result: Option<ReadResult>,
    waker: Option<Waker>,
}

impl Delivery {
    fn complete(slot: &Mutex<Self>, result: ReadResult) {
        let waker = {
            let mut delivery = slot.lock().unwrap();
            delivery.result = Some(result);
            delivery.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct Request {
    path: PathBuf,
    delivery: Weak<Mutex<Delivery>>,
}

#[derive(Default)]
struct Queue {
    pending: Option<Request>,
    stopped: bool,
}

#[derive(Default)]
struct Control {
    queue: Mutex<Queue>,
    wake: Condvar,
}

pub(crate) struct SourceWorker {
    control: Arc<Control>,
    // shutdown wakes the worker but does not wait for a kernel filesystem read.
    _thread: JoinHandle<()>,
}

impl SourceWorker {
    pub(crate) fn new() -> Result<Self, String> {
        Self::with_reader(SourceFile::read)
    }

    fn with_reader(read: impl Fn(&Path) -> ReadResult + Send + 'static) -> Result<Self, String> {
        ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "a configuration source worker is already active".to_owned())?;
        let lease = Lease;
        let control = Arc::new(Control::default());
        let worker = control.clone();
        let thread = thread::Builder::new()
            .name("thing-source".into())
            .spawn(move || {
                let _lease = lease;
                loop {
                    let request = {
                        let mut queue = worker.queue.lock().unwrap();
                        while queue.pending.is_none() && !queue.stopped {
                            queue = worker.wake.wait(queue).unwrap();
                        }
                        if queue.stopped {
                            return;
                        }
                        queue.pending.take().unwrap()
                    };
                    if request.delivery.strong_count() == 0 {
                        continue;
                    }
                    let result = read(&request.path);
                    if let Some(delivery) = request.delivery.upgrade() {
                        Delivery::complete(&delivery, result);
                    }
                }
            })
            .map_err(|error| format!("cannot start configuration reader: {error}"))?;
        Ok(Self {
            control,
            _thread: thread,
        })
    }

    pub(crate) fn session(self: &Rc<Self>) -> Rc<dyn SourceReader> {
        Rc::new(Session {
            worker: self.clone(),
            files: RefCell::default(),
            bytes: Rc::new(Cell::new(0)),
        })
    }

    fn read(&self, path: PathBuf) -> Read {
        let delivery = Arc::new(Mutex::new(Delivery::default()));
        let previous = self.control.queue.lock().unwrap().pending.replace(Request {
            path,
            delivery: Arc::downgrade(&delivery),
        });
        if let Some(previous) = previous.and_then(|request| request.delivery.upgrade()) {
            Delivery::complete(&previous, Err("configuration read was superseded".into()));
        }
        self.control.wake.notify_one();
        Read(delivery)
    }
}

impl Drop for SourceWorker {
    fn drop(&mut self) {
        let pending = {
            let mut queue = self.control.queue.lock().unwrap();
            queue.stopped = true;
            queue.pending.take()
        };
        if let Some(delivery) = pending.and_then(|request| request.delivery.upgrade()) {
            Delivery::complete(&delivery, Err("configuration reader stopped".into()));
        }
        self.control.wake.notify_one();
    }
}

struct Read(Arc<Mutex<Delivery>>);

impl Future for Read {
    type Output = ReadResult;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut delivery = self.0.lock().unwrap();
        match &delivery.result {
            Some(result) => Poll::Ready(result.clone()),
            None => {
                delivery.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[derive(Clone)]
struct CachedRead {
    delivery: Arc<Mutex<Delivery>>,
    charged: Rc<Cell<bool>>,
}

struct Session {
    worker: Rc<SourceWorker>,
    files: RefCell<BTreeMap<PathBuf, CachedRead>>,
    bytes: Rc<Cell<usize>>,
}

impl SourceReader for Session {
    fn read(&self, path: PathBuf) -> thing_runtime::SourceFuture {
        let mut files = self.files.borrow_mut();
        if files.len() >= MAX_FILES && !files.contains_key(&path) {
            return Box::pin(async { Err("configuration exceeds 1024 source files".into()) });
        }
        let cached = files
            .entry(path.clone())
            .or_insert_with(|| CachedRead {
                delivery: self.worker.read(path).0,
                charged: Rc::new(Cell::new(false)),
            })
            .clone();
        let slot = cached.delivery;
        let charged = cached.charged;
        let mut read = Read(slot.clone());
        let bytes = self.bytes.clone();
        Box::pin(async move {
            let result = Pin::new(&mut read).await;
            if !charged.replace(true)
                && let Ok(source) = &result
            {
                if bytes.get() + source.size_bytes() > MAX_BYTES {
                    let error = "configuration exceeds the 16 MiB total source limit";
                    Delivery::complete(&slot, Err(error.into()));
                    return Err(error.into());
                }
                bytes.set(bytes.get() + source.size_bytes());
            }
            result
        })
    }

    fn validate(&self) -> Pin<Box<dyn Future<Output = Result<bool, String>>>> {
        let snapshot = self
            .files
            .borrow()
            .iter()
            .map(|(path, cached)| {
                let result = cached
                    .delivery
                    .lock()
                    .unwrap()
                    .result
                    .clone()
                    .ok_or_else(|| "configuration read has not finished".to_owned())?;
                Ok((path.clone(), result))
            })
            .collect::<Result<Vec<_>, String>>();
        let worker = self.worker.clone();
        Box::pin(async move {
            for (path, original) in snapshot? {
                if worker.read(path).await != original {
                    return Ok(false);
                }
            }
            Ok(true)
        })
    }
}

#[cfg(test)]
pub(crate) mod tests;
