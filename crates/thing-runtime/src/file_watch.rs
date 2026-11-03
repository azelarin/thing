//! file-change callbacks declared by a configuration and delivered by its host.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    path::Path,
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use mlua::{Error, ErrorContext, Function, Lua, Result, Table, Value};

pub const MAX_FILE_WATCHES: usize = 64;
pub const MAX_WATCHED_FILE_PATHS: usize = 32;
pub const MAX_WATCHED_FILE_BYTES: usize = 128 * 1024;
pub const MAX_FILE_WATCH_ERROR_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileWatchDefinition {
    pub key: String,
    pub path: String,
}

impl FileWatchDefinition {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.key.is_empty()
            || self.key.len() > 64
            || !self
                .key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(
                "file watch keys must be 1 to 64 ascii letters, digits, '-', '_', or '.'".into(),
            );
        }
        if !Path::new(&self.path).is_absolute()
            || self.path.len() > 4096
            || self.path.contains('\0')
        {
            return Err(
                "file watch paths must be absolute, without nul, and at most 4096 bytes".into(),
            );
        }
        Ok(())
    }
}

/// the latest read result. missing files remain watched for recreation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileWatchSnapshot {
    Ready(Arc<str>),
    Missing,
    Failed(Arc<str>),
}

impl FileWatchSnapshot {
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Self::Ready(text) if text.len() > MAX_WATCHED_FILE_BYTES || text.contains('\0') => {
                Err("watched text must be at most 128 KiB without nul bytes".into())
            }
            Self::Failed(error)
                if error.len() > MAX_FILE_WATCH_ERROR_BYTES || error.contains('\0') =>
            {
                Err("file watch errors must be at most 1024 bytes without nul".into())
            }
            _ => Ok(()),
        }
    }

    fn event(&self, lua: &Lua) -> Result<Table> {
        self.validate().map_err(Error::runtime)?;
        let event = lua.create_table()?;
        match self {
            Self::Ready(text) => {
                event.set("status", "ready")?;
                event.set("text", text.as_ref())?;
            }
            Self::Missing => event.set("status", "missing")?,
            Self::Failed(error) => {
                event.set("status", "failed")?;
                event.set("error", error.as_ref())?;
            }
        }
        event.set_readonly(true);
        Ok(event)
    }
}

struct FileWatch {
    definition: FileWatchDefinition,
    callback: Function,
}

#[derive(Default)]
pub(crate) struct FileWatches {
    watches: BTreeMap<String, FileWatch>,
}

impl FileWatches {
    pub(crate) fn definitions(&self) -> Vec<FileWatchDefinition> {
        self.watches
            .values()
            .map(|watch| watch.definition.clone())
            .collect()
    }
}

pub(crate) fn install(
    lua: &Lua,
    services: &Table,
    registry: Rc<RefCell<FileWatches>>,
    open: Rc<Cell<bool>>,
) -> Result<()> {
    services.set(
        "watch_file",
        lua.create_function(move |_, options: Table| {
            if !open.get() {
                return Err(Error::runtime(
                    "file watches must be declared while the configuration loads",
                ));
            }
            validate_options(&options)?;
            let definition = FileWatchDefinition {
                key: string(&options, "key")?,
                path: string(&options, "path")?,
            };
            definition.validate().map_err(Error::runtime)?;
            let Value::Function(callback) = options.raw_get("on_change")? else {
                return Err(Error::runtime("file watch on_change must be a function"));
            };
            let mut watches = registry.borrow_mut();
            if watches.watches.len() == MAX_FILE_WATCHES {
                return Err(Error::runtime(
                    "a configuration may declare at most 64 file watches",
                ));
            }
            if watches.watches.contains_key(&definition.key) {
                return Err(Error::runtime(
                    "file watch keys must be unique in each configuration",
                ));
            }
            watches.watches.insert(
                definition.key.clone(),
                FileWatch {
                    definition,
                    callback,
                },
            );
            Ok(())
        })?,
    )
}

pub(crate) fn validate_options(options: &Table) -> Result<()> {
    for pair in options.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if !matches!(&key, Value::String(key) if matches!(key.as_bytes().as_ref(), b"key"|b"path"|b"on_change"))
        {
            return Err(Error::runtime(
                "file watches accept only key, path, and on_change",
            ));
        }
    }
    Ok(())
}

pub(crate) fn string(options: &Table, key: &str) -> Result<String> {
    match options.raw_get::<Value>(key)? {
        Value::String(value) => value
            .to_str()
            .map(|s| s.to_owned())
            .map_err(|_| Error::runtime(format!("file watch {key} must be utf-8"))),
        _ => Err(Error::runtime(format!("file watch {key} must be a string"))),
    }
}

impl crate::Runtime {
    pub fn file_watches(&self) -> Vec<FileWatchDefinition> {
        self.services.file_watches().borrow().definitions()
    }

    /// delivers one host snapshot through the same controls as an input event.
    pub fn dispatch_file_watch_checked(
        &mut self,
        key: &str,
        snapshot: &FileWatchSnapshot,
        validate: impl Fn(&[crate::SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<Option<bool>> {
        self.with_event_controls(None, |runtime| {
            let Some(callback) = runtime
                .services
                .file_watches()
                .borrow()
                .watches
                .get(key)
                .map(|watch| watch.callback.clone())
            else {
                return Ok(None);
            };
            let event = snapshot.event(&runtime.lua)?;
            let checkpoint = runtime.action_checkpoint();
            runtime.begin_call();
            runtime.allow_actions(true);
            let result = callback.call::<()>(event);
            runtime.allow_actions(false);
            let result = result.and_then(|()| {
                let now = Instant::now();
                let changed = runtime.refresh_at_checked(now, &validate)?;
                let cleaned = runtime.cancel_unavailable_presses_checked(now, &validate)?;
                let pointer = runtime.reconcile_pointer_checked(&validate)?;
                Ok(Some(changed || cleaned || pointer.changed))
            });
            if result.is_err() {
                runtime.truncate_actions(checkpoint);
            }
            result.map_err(|error| error.context(format!("file watch {key:?}")))
        })
    }
}
