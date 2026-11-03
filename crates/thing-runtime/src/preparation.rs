use crate::{ConfigurationMode, Limits, OutputContext, Runtime, ServiceSnapshot, modules::Modules};
use mlua::{Error, Result};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fs::OpenOptions,
    future::Future,
    io::Read,
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
const LOAD_SLICE: Duration = Duration::from_millis(2);

/// a bounded UTF-8 module and its canonical location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFile {
    pub(crate) path: PathBuf,
    pub(crate) text: Arc<str>,
}

impl SourceFile {
    pub fn size_bytes(&self) -> usize {
        self.text.len()
    }

    /// perform blocking filesystem work; asynchronous hosts call this on a worker.
    pub fn read(path: &Path) -> std::result::Result<Self, String> {
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        Self::read_canonical(canonical)
    }

    pub(crate) fn read_canonical(path: PathBuf) -> std::result::Result<Self, String> {
        if path.to_str().is_none() {
            return Err("configuration paths must be UTF-8".into());
        }
        let read = || -> std::io::Result<String> {
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // inspect special files without waiting for a fifo writer or
                // acquiring a terminal as the process's controlling terminal.
                options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
            }
            let file = options.open(&path)?;
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other(
                    "configuration source must be a regular file",
                ));
            }
            let mut bytes = Vec::new();
            file.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_SOURCE_BYTES {
                return Err(std::io::Error::other("exceeds the 1 MiB source limit"));
            }
            String::from_utf8(bytes).map_err(std::io::Error::other)
        };
        let text = read().map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self {
            path,
            text: text.into(),
        })
    }
}

pub type SourceFuture = Pin<Box<dyn Future<Output = std::result::Result<SourceFile, String>>>>;

/// supplies source files without blocking the thread polling a configuration.
pub trait SourceReader {
    fn read(&self, path: PathBuf) -> SourceFuture;

    /// immutable readers need no check; filesystem readers recheck their snapshot.
    fn validate(&self) -> Pin<Box<dyn Future<Output = std::result::Result<bool, String>>>> {
        Box::pin(async { Ok(true) })
    }
}

const MAX_SOURCE_FILES: usize = 1024;
const MAX_TOTAL_SOURCE_BYTES: usize = 16 * 1024 * 1024;

type SourceResult = std::result::Result<SourceFile, String>;

pub(crate) struct SourceCache {
    reader: Rc<dyn SourceReader>,
    files: RefCell<BTreeMap<PathBuf, SourceResult>>,
    bytes: Cell<usize>,
    missing: RefCell<Option<PathBuf>>,
}

impl SourceCache {
    fn new(reader: Rc<dyn SourceReader>) -> Self {
        Self {
            reader,
            files: RefCell::default(),
            bytes: Cell::new(0),
            missing: RefCell::default(),
        }
    }

    pub(crate) async fn read(&self, path: PathBuf) -> Result<SourceFile> {
        if let Some(source) = self.files.borrow().get(&path).cloned() {
            return source.map_err(Error::runtime);
        }
        if self.files.borrow().len() >= MAX_SOURCE_FILES {
            return Err(Error::runtime("configuration exceeds 1024 source files"));
        }
        let source = self.reader.read(path.clone()).await;
        let bytes = source.as_ref().map_or(0, |source| source.text.len());
        if self.bytes.get() + bytes > MAX_TOTAL_SOURCE_BYTES {
            return Err(Error::runtime(
                "configuration exceeds the 16 MiB total source limit",
            ));
        }
        self.bytes.set(self.bytes.get() + bytes);
        self.files.borrow_mut().insert(path, source.clone());
        source.map_err(Error::runtime)
    }

    pub(crate) fn synchronous(&self, path: &Path) -> Result<SourceFile> {
        if let Some(source) = self.files.borrow().get(path).cloned() {
            return source.map_err(Error::runtime);
        }
        *self.missing.borrow_mut() = Some(path.to_owned());
        Err(Error::runtime("module needs asynchronous preparation"))
    }
}

pub(crate) fn install_interrupt(
    lua: &mlua::Lua,
    deadline: Rc<Cell<Instant>>,
    slice: Rc<Cell<Option<Instant>>>,
) {
    lua.set_interrupt(move |_| {
        if Instant::now() >= deadline.get() {
            Err(Error::runtime("script exceeded its execution time limit"))
        } else if slice.get().is_some_and(|end| Instant::now() >= end) {
            Ok(mlua::VmState::Yield)
        } else {
            Ok(mlua::VmState::Continue)
        }
    });
}

/// an unpublished runtime, cooperatively evaluated on its owning thread.
/// dropping this future discards the candidate without activating its services.
pub struct RuntimeLoad {
    future: Pin<Box<dyn Future<Output = Result<Runtime>>>>,
    deadline: Rc<Cell<Instant>>,
    slice: Rc<Cell<Option<Instant>>>,
    remaining: Duration,
}

impl RuntimeLoad {
    /// prepare one output while the host keeps dispatching its active shell.
    pub fn for_output(
        path: PathBuf,
        output: OutputContext,
        services: ServiceSnapshot,
        reader: Rc<dyn SourceReader>,
    ) -> Result<Self> {
        output.validate()?;
        Self::new(path, Some(output), services, reader, Limits::default())
    }

    fn new(
        path: PathBuf,
        output: Option<OutputContext>,
        services: ServiceSnapshot,
        reader: Rc<dyn SourceReader>,
        limits: Limits,
    ) -> Result<Self> {
        let sources = Rc::new(SourceCache::new(reader));
        let deadline = Rc::new(Cell::new(Instant::now() + limits.execution_time));
        let slice = Rc::new(Cell::new(None));
        let runtime_deadline = deadline.clone();
        let runtime_slice = slice.clone();
        let future = Box::pin(async move {
            loop {
                let modules = Modules::with_reader(sources.clone());
                let mut runtime = Runtime::create(
                    limits,
                    services.clone(),
                    output.clone(),
                    ConfigurationMode::Shell,
                    modules.clone(),
                )?;
                runtime.deadline = runtime_deadline.clone();
                runtime.loading_slice = runtime_slice.clone();
                install_interrupt(
                    &runtime.lua,
                    runtime_deadline.clone(),
                    runtime_slice.clone(),
                );
                let exported = modules
                    .load_async(runtime.lua.clone(), path.clone())
                    .await?;
                modules.synchronous_properties();
                let result = runtime.finish_load(exported);
                let missing = sources.missing.borrow_mut().take();
                if let Some(path) = missing {
                    // a property can import a module too. discard the candidate,
                    // fetch that source, and repeat against the same source snapshot.
                    // cache read errors as well, so Lua pcall keeps its usual meaning.
                    drop(result);
                    let read = sources.read(path.clone()).await;
                    if !sources.files.borrow().contains_key(&path) {
                        return Err(read.expect_err("uncached source reads must fail"));
                    }
                    let mut yielded = false;
                    std::future::poll_fn(|context| {
                        if std::mem::replace(&mut yielded, true) {
                            Poll::Ready(())
                        } else {
                            context.waker().wake_by_ref();
                            Poll::Pending
                        }
                    })
                    .await;
                    continue;
                }
                modules.finish_loading();
                return result;
            }
        });
        Ok(Self {
            future,
            deadline,
            slice,
            remaining: limits.execution_time,
        })
    }
}

impl Future for RuntimeLoad {
    type Output = Result<Runtime>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.remaining.is_zero() {
            return Poll::Ready(Err(Error::runtime(
                "script exceeded its execution time limit",
            )));
        }
        let start = Instant::now();
        self.deadline.set(start + self.remaining);
        self.slice.set(Some(start + LOAD_SLICE));
        let result = self.future.as_mut().poll(context);
        self.slice.set(None);
        self.remaining = self.remaining.saturating_sub(start.elapsed());
        result
    }
}

#[cfg(test)]
mod tests;
