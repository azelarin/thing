//! idle watches a configuration declares while it loads. the host arms them
//! with the compositor and reports transitions through the checked event path.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use mlua::{Error, Function, Lua, Result, Table, Value};

use crate::state::{Scalar, Signal, Tracker};

pub const MAX_IDLE_WATCHES: usize = 16;
pub const MIN_IDLE_TIMEOUT_MS: u32 = 1_000;
pub const MAX_IDLE_TIMEOUT_MS: u32 = 86_400_000;

/// what the host needs to arm one watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdleWatchDefinition {
    pub timeout_ms: u32,
}

pub(crate) struct IdleWatch {
    pub(crate) definition: IdleWatchDefinition,
    pub(crate) on_idle: Option<Function>,
    pub(crate) on_resume: Option<Function>,
    pub(crate) signal: usize,
    pub(crate) handle: Table,
}

#[derive(Default)]
pub(crate) struct IdleWatches {
    pub(crate) watches: Vec<IdleWatch>,
}

impl IdleWatches {
    pub(crate) fn definitions(&self) -> Vec<IdleWatchDefinition> {
        self.watches.iter().map(|watch| watch.definition).collect()
    }
}

pub(crate) fn install(
    lua: &Lua,
    idle: &Table,
    tracker: Rc<RefCell<Tracker>>,
    registry: Rc<RefCell<IdleWatches>>,
    open: Rc<Cell<bool>>,
) -> Result<()> {
    idle.set(
        "watch",
        lua.create_function(move |lua, value: Value| {
            if !open.get() {
                return Err(Error::runtime(
                    "idle watches must be declared while the configuration loads",
                ));
            }
            let Value::Table(options) = value else {
                return Err(Error::runtime(
                    "services.idle.watch expects an options table",
                ));
            };
            for pair in options.clone().pairs::<Value, Value>() {
                let (key, _) = pair?;
                let known = matches!(&key, Value::String(key)
                    if matches!(key.as_bytes().as_ref(), b"timeout" | b"on_idle" | b"on_resume"));
                if !known {
                    return Err(Error::runtime(
                        "idle watches accept only timeout, on_idle, and on_resume",
                    ));
                }
            }
            let timeout_ms = timeout(options.raw_get("timeout")?)?;
            let on_idle = callback(options.raw_get("on_idle")?, "on_idle")?;
            let on_resume = callback(options.raw_get("on_resume")?, "on_resume")?;
            if registry.borrow().watches.len() >= MAX_IDLE_WATCHES {
                return Err(Error::runtime(format!(
                    "a configuration may declare at most {MAX_IDLE_WATCHES} idle watches"
                )));
            }
            let signal = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
            let handle = lua.create_table()?;
            handle.set(
                "idle",
                Signal {
                    id: signal,
                    tracker: tracker.clone(),
                    readonly: true,
                },
            )?;
            handle.set_readonly(true);
            registry.borrow_mut().watches.push(IdleWatch {
                definition: IdleWatchDefinition { timeout_ms },
                on_idle,
                on_resume,
                signal,
                handle: handle.clone(),
            });
            Ok(handle)
        })?,
    )
}

fn timeout(value: Value) -> Result<u32> {
    let milliseconds = match value {
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => return Err(Error::runtime("idle watch timeout must be a number")),
    };
    if !(milliseconds.is_finite()
        && milliseconds.fract() == 0.0
        && (f64::from(MIN_IDLE_TIMEOUT_MS)..=f64::from(MAX_IDLE_TIMEOUT_MS))
            .contains(&milliseconds))
    {
        return Err(Error::runtime(format!(
            "idle watch timeout must be a whole number of milliseconds from \
             {MIN_IDLE_TIMEOUT_MS} through {MAX_IDLE_TIMEOUT_MS}"
        )));
    }
    Ok(milliseconds as u32)
}

fn callback(value: Value, name: &str) -> Result<Option<Function>> {
    match value {
        Value::Nil => Ok(None),
        Value::Function(function) => Ok(Some(function)),
        _ => Err(Error::runtime(format!(
            "idle watch {name} must be a function or nil"
        ))),
    }
}
