//! keyed sleep inhibitor declarations. a declaration only names a lease; the
//! lease is requested by the `start` action and released by `stop`.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use mlua::{Error, Lua, Result, Table, Value};

use crate::{
    Action,
    state::{Scalar, Signal, Tracker},
};

pub const MAX_SLEEP_INHIBITORS: usize = 16;
pub const MAX_SLEEP_INHIBITOR_KEY_BYTES: usize = 64;
pub const MAX_SLEEP_INHIBITOR_REASON_BYTES: usize = 256;
const MAX_ERROR_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SleepInhibitorDefinition {
    pub key: String,
    /// shown by logind as the reason for the inhibition.
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SleepInhibitorStatus {
    /// no lease is wanted.
    #[default]
    Stopped,
    /// a lease is wanted and its request has not completed.
    Requested,
    /// the shell holds a lease from logind.
    Active,
    /// the system bus or logind cannot be reached; the request is retried.
    Unavailable,
    /// logind refused the request, or retries are exhausted.
    Failed,
}

impl SleepInhibitorStatus {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Requested => "requested",
            Self::Active => "active",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SleepInhibitorState {
    pub key: String,
    pub status: SleepInhibitorStatus,
    pub error: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepInhibitorAction {
    Start,
    Stop,
}

#[derive(Clone)]
pub(crate) struct SleepInhibitorSignals {
    pub(crate) definition: SleepInhibitorDefinition,
    state: usize,
    error: usize,
}

#[derive(Default)]
pub(crate) struct SleepInhibitors {
    pub(crate) declared: BTreeMap<String, SleepInhibitorSignals>,
    pub(crate) states: BTreeMap<String, SleepInhibitorState>,
}

impl SleepInhibitors {
    pub(crate) fn definitions(&self) -> Vec<SleepInhibitorDefinition> {
        self.declared
            .values()
            .map(|signals| signals.definition.clone())
            .collect()
    }

    /// replaces the published states and updates the declared handles.
    pub(crate) fn set_states(
        &mut self,
        tracker: &RefCell<Tracker>,
        states: Vec<SleepInhibitorState>,
    ) -> Result<()> {
        validate_states(&states)?;
        let states: BTreeMap<_, _> = states
            .into_iter()
            .map(|state| (state.key.clone(), state))
            .collect();
        let updates = self.declared.iter().flat_map(|(key, signals)| {
            let state = states.get(key).cloned().unwrap_or_default();
            [
                (signals.state, Scalar::String(state.status.name().into())),
                (signals.error, Scalar::String(state.error)),
            ]
        });
        tracker
            .borrow_mut()
            .write_batch(updates.collect::<Vec<_>>())?;
        self.states = states;
        Ok(())
    }
}

pub(crate) fn validate_states(states: &[SleepInhibitorState]) -> Result<()> {
    let mut keys = std::collections::BTreeSet::new();
    for state in states {
        validate_key(&state.key)?;
        if !keys.insert(state.key.as_str()) {
            return Err(Error::runtime("sleep inhibitor states repeat a key"));
        }
        if state.error.len() > MAX_ERROR_BYTES || state.error.contains('\0') {
            return Err(Error::runtime("sleep inhibitor error text is invalid"));
        }
    }
    Ok(())
}

pub(crate) fn install(
    lua: &Lua,
    services: &Table,
    tracker: Rc<RefCell<Tracker>>,
    registry: Rc<RefCell<SleepInhibitors>>,
    actions: Rc<RefCell<Vec<Action>>>,
    allowed: Rc<Cell<bool>>,
    open: Rc<Cell<bool>>,
) -> Result<()> {
    services.set(
        "sleep_inhibitor",
        lua.create_function(move |lua, value: Value| {
            if !open.get() {
                return Err(Error::runtime(
                    "sleep inhibitors must be declared while the configuration loads",
                ));
            }
            let definition = definition(value)?;
            let existing = registry.borrow().declared.get(&definition.key).cloned();
            let signals = match existing {
                Some(signals) if signals.definition != definition => {
                    return Err(Error::runtime(format!(
                        "sleep inhibitor {:?} has conflicting declarations",
                        definition.key
                    )));
                }
                Some(signals) => signals,
                None => {
                    let mut registry = registry.borrow_mut();
                    if registry.declared.len() >= MAX_SLEEP_INHIBITORS {
                        return Err(Error::runtime(format!(
                            "a configuration may declare at most {MAX_SLEEP_INHIBITORS} sleep inhibitors"
                        )));
                    }
                    let state = registry
                        .states
                        .get(&definition.key)
                        .cloned()
                        .unwrap_or_default();
                    let mut tracker = tracker.borrow_mut();
                    let signals = SleepInhibitorSignals {
                        definition: definition.clone(),
                        state: tracker.insert(Scalar::String(state.status.name().into()))?,
                        error: tracker.insert(Scalar::String(state.error))?,
                    };
                    registry
                        .declared
                        .insert(definition.key.clone(), signals.clone());
                    signals
                }
            };
            handle(lua, &signals, &tracker, &actions, &allowed)
        })?,
    )
}

fn handle(
    lua: &Lua,
    signals: &SleepInhibitorSignals,
    tracker: &Rc<RefCell<Tracker>>,
    actions: &Rc<RefCell<Vec<Action>>>,
    allowed: &Rc<Cell<bool>>,
) -> Result<Table> {
    let handle = lua.create_table()?;
    for (name, id) in [("state", signals.state), ("error", signals.error)] {
        handle.set(
            name,
            Signal {
                id,
                tracker: tracker.clone(),
                readonly: true,
            },
        )?;
    }
    for (name, action) in [
        ("start", SleepInhibitorAction::Start),
        ("stop", SleepInhibitorAction::Stop),
    ] {
        let key = signals.definition.key.clone();
        let actions = actions.clone();
        let allowed = allowed.clone();
        handle.set(
            name,
            lua.create_function(move |_, ()| {
                crate::services::enqueue(
                    &actions,
                    &allowed,
                    Action::SleepInhibitor {
                        key: key.clone(),
                        action,
                    },
                )
            })?,
        )?;
    }
    handle.set_readonly(true);
    Ok(handle)
}

fn definition(value: Value) -> Result<SleepInhibitorDefinition> {
    let Value::Table(options) = value else {
        return Err(Error::runtime(
            "services.sleep_inhibitor expects an options table",
        ));
    };
    for pair in options.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let known = matches!(&key, Value::String(key)
            if matches!(key.as_bytes().as_ref(), b"key" | b"reason"));
        if !known {
            return Err(Error::runtime(
                "sleep inhibitors accept only key and reason",
            ));
        }
    }
    let key = text(options.raw_get("key")?, "key")?;
    validate_key(&key)?;
    let reason = text(options.raw_get("reason")?, "reason")?;
    if reason.trim().is_empty()
        || reason.len() > MAX_SLEEP_INHIBITOR_REASON_BYTES
        || reason.chars().any(char::is_control)
    {
        return Err(Error::runtime(format!(
            "sleep inhibitor reason must be readable text of at most \
             {MAX_SLEEP_INHIBITOR_REASON_BYTES} bytes without control characters"
        )));
    }
    Ok(SleepInhibitorDefinition { key, reason })
}

fn text(value: Value, name: &str) -> Result<String> {
    let Value::String(value) = value else {
        return Err(Error::runtime(format!(
            "sleep inhibitor {name} must be a string"
        )));
    };
    Ok(value
        .to_str()
        .map_err(|_| Error::runtime(format!("sleep inhibitor {name} must be valid utf-8")))?
        .to_owned())
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > MAX_SLEEP_INHIBITOR_KEY_BYTES
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::runtime(format!(
            "sleep inhibitor keys must be 1 to {MAX_SLEEP_INHIBITOR_KEY_BYTES} ascii \
             letters, digits, '-', '_', or '.'"
        )));
    }
    Ok(())
}
