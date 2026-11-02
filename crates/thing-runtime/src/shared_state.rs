//! named shared state for every output of one running instance.
//!
//! the host stages updates before publishing any tree. luau reads immutable
//! snapshots and queues replacements through accepted event handlers.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::Arc,
};

use mlua::{AnyUserData, Error, Lua, Result, Table, UserData, UserDataMethods, Value};

use crate::{
    Action, CallValue,
    state::{Scalar, Tracker},
};

/// the most shared state keys across all outputs.
pub const MAX_SHARED_STATES: usize = 64;
/// the longest shared state key.
pub const MAX_SHARED_STATE_KEY_BYTES: usize = 64;
/// the largest retained size of one shared value, as [`shared_state_bytes`]
/// counts it.
pub const MAX_SHARED_STATE_BYTES: usize = 128 * 1024;
/// the largest retained size of all shared values together.
pub const MAX_SHARED_STATE_TOTAL_BYTES: usize = 1024 * 1024;
/// the largest size of the shared values one event may queue.
pub const MAX_QUEUED_SHARED_STATE_BYTES: usize = 1024 * 1024;

/// a key and its initial value, declared while a configuration loads.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedStateDefinition {
    pub key: String,
    pub initial: Arc<CallValue>,
}

/// one accepted value and the declaration it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedStateEntry {
    pub initial: Arc<CallValue>,
    pub value: Arc<CallValue>,
}

/// the accepted shared values of one running instance.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SharedStates {
    entries: BTreeMap<String, SharedStateEntry>,
}

/// the retained size of a value: 16 bytes for each item and map key, plus the
/// bytes of every string and map key. it is deterministic, so every output and
/// the host agree on the limits.
pub fn shared_state_bytes(value: &CallValue) -> usize {
    16 + match value {
        CallValue::Nil | CallValue::Boolean(_) | CallValue::Number(_) => 0,
        CallValue::String(text) => text.len(),
        CallValue::List(items) => items.iter().map(shared_state_bytes).sum(),
        CallValue::Map(entries) => entries
            .iter()
            .map(|(key, item)| 16 + key.len() + shared_state_bytes(item))
            .sum(),
    }
}

impl SharedStates {
    pub fn get(&self, key: &str) -> Option<&SharedStateEntry> {
        self.entries.get(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// the retained size of every value.
    pub fn total_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|entry| retained_bytes(&entry.initial, &entry.value))
            .sum()
    }

    /// the accepted state for a replacement configuration. a key keeps its
    /// value when its declaration is unchanged and starts from `initial`
    /// otherwise. keys nobody declares are dropped.
    pub fn project(
        &self,
        definitions: &[SharedStateDefinition],
    ) -> std::result::Result<Self, String> {
        let entries = definitions
            .iter()
            .map(|definition| {
                let entry = self
                    .entries
                    .get(&definition.key)
                    .filter(|entry| entry.initial == definition.initial)
                    .cloned()
                    .unwrap_or_else(|| SharedStateEntry {
                        initial: definition.initial.clone(),
                        value: definition.initial.clone(),
                    });
                (definition.key.clone(), entry)
            })
            .collect();
        let projected = Self { entries };
        projected.check()?;
        Ok(projected)
    }

    /// like [`Self::project`], but keeps the values of keys no current output
    /// declares, so an output that leaves and returns finds its state again.
    /// those values are dropped instead when they would exceed the limits.
    pub fn project_retaining(
        &self,
        definitions: &[SharedStateDefinition],
    ) -> std::result::Result<Self, String> {
        let mut projected = self.project(definitions)?;
        let mut retained = projected.clone();
        for (key, entry) in &self.entries {
            retained
                .entries
                .entry(key.clone())
                .or_insert_with(|| entry.clone());
        }
        if retained.check().is_ok() {
            projected = retained;
        }
        Ok(projected)
    }

    /// replaces declared values. it returns `None` when every value is
    /// already current, so equivalent publications change nothing.
    pub fn with_values(
        &self,
        updates: &[(String, Arc<CallValue>)],
    ) -> std::result::Result<Option<Self>, String> {
        let mut next = self.clone();
        let mut changed = false;
        for (key, value) in updates {
            let entry = next
                .entries
                .get_mut(key)
                .ok_or_else(|| format!("shared state {key:?} is not declared"))?;
            if entry.value != *value {
                entry.value = if *value == entry.initial {
                    entry.initial.clone()
                } else {
                    value.clone()
                };
                changed = true;
            }
        }
        if !changed || next == *self {
            return Ok(None);
        }
        next.check()?;
        Ok(Some(next))
    }

    fn check(&self) -> std::result::Result<(), String> {
        if self.entries.len() > MAX_SHARED_STATES {
            return Err(format!(
                "a configuration may declare at most {MAX_SHARED_STATES} shared states"
            ));
        }
        let mut total = 0_usize;
        for (key, entry) in &self.entries {
            validate_key(key)?;
            validate_value(&entry.value)?;
            validate_value(&entry.initial)?;
            total += retained_bytes(&entry.initial, &entry.value);
        }
        if total > MAX_SHARED_STATE_TOTAL_BYTES {
            return Err(total_error());
        }
        Ok(())
    }
}

fn retained_bytes(initial: &Arc<CallValue>, value: &Arc<CallValue>) -> usize {
    shared_state_bytes(initial)
        + if Arc::ptr_eq(initial, value) {
            0
        } else {
            shared_state_bytes(value)
        }
}

fn total_error() -> String {
    format!(
        "shared states may retain at most {} KiB together",
        MAX_SHARED_STATE_TOTAL_BYTES / 1024
    )
}

fn validate_key(key: &str) -> std::result::Result<(), String> {
    if key.is_empty()
        || key.len() > MAX_SHARED_STATE_KEY_BYTES
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "shared state keys must be 1 to {MAX_SHARED_STATE_KEY_BYTES} ascii letters, \
             digits, '-', '_', or '.'"
        ));
    }
    Ok(())
}

fn validate_value(value: &CallValue) -> std::result::Result<usize, String> {
    value.validate()?;
    let bytes = shared_state_bytes(value);
    if bytes > MAX_SHARED_STATE_BYTES {
        return Err(format!(
            "a shared state value may retain at most {} KiB",
            MAX_SHARED_STATE_BYTES / 1024
        ));
    }
    Ok(bytes)
}

#[derive(Clone)]
struct Declared {
    initial: Arc<CallValue>,
    value: Arc<CallValue>,
    revision: usize,
}

/// the shared states one runtime declared and the values it currently shows.
#[derive(Clone, Default)]
pub(crate) struct SharedStateHandles {
    /// used only while resolving load-time declarations.
    initial_states: SharedStates,
    declared: BTreeMap<String, Declared>,
}

impl SharedStateHandles {
    pub(crate) fn definitions(&self) -> Vec<SharedStateDefinition> {
        self.declared
            .iter()
            .map(|(key, declared)| SharedStateDefinition {
                key: key.clone(),
                initial: declared.initial.clone(),
            })
            .collect()
    }

    pub(crate) fn load(&mut self, accepted: SharedStates) {
        self.initial_states = accepted;
    }

    pub(crate) fn snapshot(&self) -> SharedStates {
        SharedStates {
            entries: self
                .declared
                .iter()
                .map(|(key, state)| {
                    (
                        key.clone(),
                        SharedStateEntry {
                            initial: state.initial.clone(),
                            value: state.value.clone(),
                        },
                    )
                })
                .collect(),
        }
    }

    pub(crate) fn finish_loading(&mut self) {
        // declarations now retain exactly the keys this runtime uses.
        self.initial_states = SharedStates::default();
    }

    /// shows the accepted values of the declared keys whose declaration
    /// matches. other keys keep their current value.
    pub(crate) fn set_states(
        &mut self,
        tracker: &RefCell<Tracker>,
        states: &SharedStates,
    ) -> Result<()> {
        states.check().map_err(Error::runtime)?;
        let mut tracker = tracker.borrow_mut();
        let mut updates = Vec::new();
        for (key, declared) in &mut self.declared {
            let Some(entry) = states
                .get(key)
                .filter(|entry| entry.initial == declared.initial)
            else {
                continue;
            };
            declared.initial = entry.initial.clone();
            if Arc::ptr_eq(&entry.value, &declared.value) || entry.value == declared.value {
                continue;
            }
            declared.value = entry.value.clone();
            let revision = tracker.peek(declared.revision);
            updates.push((
                declared.revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        tracker.write_batch(updates)?;
        drop(tracker);
        Ok(())
    }
}

pub(crate) fn install(
    lua: &Lua,
    services: &Table,
    tracker: Rc<RefCell<Tracker>>,
    registry: Rc<RefCell<SharedStateHandles>>,
    actions: Rc<RefCell<Vec<Action>>>,
    allowed: Rc<Cell<bool>>,
    open: Rc<Cell<bool>>,
) -> Result<()> {
    services.set(
        "shared_state",
        lua.create_function(move |lua, value: Value| {
            if !open.get() {
                return Err(Error::runtime(
                    "shared states must be declared while the configuration loads",
                ));
            }
            let definition = definition(value)?;
            let key = definition.key.clone();
            let mut handles = registry.borrow_mut();
            match handles.declared.get(&key) {
                Some(declared) if declared.initial != definition.initial => {
                    return Err(Error::runtime(format!(
                        "shared state {key:?} has conflicting declarations"
                    )));
                }
                Some(_) => {}
                None => {
                    if handles.declared.len() >= MAX_SHARED_STATES {
                        return Err(Error::runtime(format!(
                            "a configuration may declare at most {MAX_SHARED_STATES} shared states"
                        )));
                    }
                    let accepted = handles
                        .initial_states
                        .get(&key)
                        .filter(|entry| entry.initial == definition.initial);
                    let initial = accepted
                        .map_or_else(|| definition.initial.clone(), |entry| entry.initial.clone());
                    let value =
                        accepted.map_or_else(|| initial.clone(), |entry| entry.value.clone());
                    let total = handles
                        .declared
                        .values()
                        .map(|declared| retained_bytes(&declared.initial, &declared.value))
                        .sum::<usize>()
                        + retained_bytes(&initial, &value);
                    if total > MAX_SHARED_STATE_TOTAL_BYTES {
                        return Err(Error::runtime(total_error()));
                    }
                    let revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
                    handles.declared.insert(
                        key.clone(),
                        Declared {
                            initial,
                            value,
                            revision,
                        },
                    );
                }
            }
            drop(handles);
            handle(lua, key, &registry, &tracker, &actions, &allowed)
        })?,
    )
}

struct SharedStateHandle {
    key: String,
    registry: std::rc::Weak<RefCell<SharedStateHandles>>,
    tracker: Rc<RefCell<Tracker>>,
    actions: Rc<RefCell<Vec<Action>>>,
    allowed: Rc<Cell<bool>>,
}

impl UserData for SharedStateHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            let registry = this
                .registry
                .upgrade()
                .ok_or_else(|| Error::runtime("shared state runtime has ended"))?;
            let registry = registry.borrow();
            let declared = &registry.declared[&this.key];
            this.tracker.borrow_mut().read(declared.revision);
            frozen(lua, &declared.value)
        });
        methods.add_method("set", |_, this, value: Value| {
            if !this.allowed.get() {
                return Err(Error::runtime(
                    "service actions are allowed only in event handlers",
                ));
            }
            let value = CallValue::from_lua(value)
                .map_err(|error| Error::runtime(format!("shared state {:?}: {error}", this.key)))?;
            let bytes = validate_value(&value).map_err(Error::runtime)?;
            enqueue(&this.actions, &this.key, Arc::new(value), bytes)
        });
    }
}

fn handle(
    lua: &Lua,
    key: String,
    registry: &Rc<RefCell<SharedStateHandles>>,
    tracker: &Rc<RefCell<Tracker>>,
    actions: &Rc<RefCell<Vec<Action>>>,
    allowed: &Rc<Cell<bool>>,
) -> Result<AnyUserData> {
    lua.create_userdata(SharedStateHandle {
        key,
        registry: Rc::downgrade(registry),
        tracker: tracker.clone(),
        actions: actions.clone(),
        allowed: allowed.clone(),
    })
}

fn enqueue(
    actions: &RefCell<Vec<Action>>,
    key: &str,
    value: Arc<CallValue>,
    bytes: usize,
) -> Result<()> {
    let queued = actions
        .borrow()
        .iter()
        .filter_map(|action| match action {
            Action::SharedState { value, .. } => Some(shared_state_bytes(value)),
            _ => None,
        })
        .sum::<usize>();
    if queued + bytes > MAX_QUEUED_SHARED_STATE_BYTES {
        return Err(Error::runtime(format!(
            "shared state updates exceed the {} KiB per-event limit",
            MAX_QUEUED_SHARED_STATE_BYTES / 1024
        )));
    }
    let mut actions = actions.borrow_mut();
    if actions.len() >= 128 {
        return Err(Error::runtime("too many service actions in one event"));
    }
    actions.push(Action::SharedState {
        key: key.to_owned(),
        value,
    });
    Ok(())
}

/// a read-only luau snapshot, so table mutation cannot bypass `set`.
fn frozen(lua: &Lua, value: &CallValue) -> Result<Value> {
    fn item(lua: &Lua, value: &CallValue) -> Result<Value> {
        match value {
            // a nil would remove a map entry or leave a hole in a list.
            CallValue::Nil => Ok(Value::NULL),
            _ => frozen(lua, value),
        }
    }
    Ok(match value {
        CallValue::List(items) => {
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            for (index, value) in items.iter().enumerate() {
                table.raw_set(index + 1, item(lua, value)?)?;
            }
            table.set_readonly(true);
            Value::Table(table)
        }
        CallValue::Map(entries) => {
            let table = lua.create_table_with_capacity(0, entries.len())?;
            for (key, value) in entries {
                table.raw_set(key.as_str(), item(lua, value)?)?;
            }
            table.set_readonly(true);
            Value::Table(table)
        }
        scalar => scalar.to_lua(lua)?,
    })
}

fn definition(value: Value) -> Result<SharedStateDefinition> {
    let Value::Table(options) = value else {
        return Err(Error::runtime(
            "services.shared_state expects an options table",
        ));
    };
    for pair in options.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let known = matches!(&key, Value::String(key)
            if matches!(key.as_bytes().as_ref(), b"key" | b"initial"));
        if !known {
            return Err(Error::runtime("shared states accept only key and initial"));
        }
    }
    let Value::String(key) = options.raw_get("key")? else {
        return Err(Error::runtime("shared state key must be a string"));
    };
    let key = key
        .to_str()
        .map_err(|_| Error::runtime("shared state key must be valid utf-8"))?
        .to_owned();
    validate_key(&key).map_err(Error::runtime)?;
    let initial = CallValue::from_lua(options.raw_get("initial")?)
        .map_err(|error| Error::runtime(format!("shared state {key:?} initial: {error}")))?;
    validate_value(&initial).map_err(Error::runtime)?;
    Ok(SharedStateDefinition {
        key,
        initial: Arc::new(initial),
    })
}

struct SharedStateCheckpoint {
    registry: SharedStateHandles,
    tracker: Tracker,
    deferred: std::collections::BTreeSet<usize>,
    binding: usize,
}

impl SharedStateCheckpoint {
    fn capture(runtime: &crate::Runtime) -> Self {
        Self {
            registry: runtime.services.shared_states.borrow().clone(),
            tracker: runtime.tracker.borrow().clone(),
            deferred: runtime.deferred.clone(),
            binding: runtime.next_binding,
        }
    }

    fn restore(self, runtime: &mut crate::Runtime) {
        runtime.bindings.split_off(&self.binding);
        *runtime.services.shared_states.borrow_mut() = self.registry;
        *runtime.tracker.borrow_mut() = self.tracker;
        runtime.deferred = self.deferred;
    }
}

/// one output's validated shared-state update. keep every output's guard
/// until all candidates are ready; dropping a guard discards its candidate
/// without evaluating the previous values again.
#[must_use = "commit the prepared update to publish it"]
pub struct PreparedSharedState<'a> {
    runtime: &'a mut crate::Runtime,
    checkpoint: Option<SharedStateCheckpoint>,
    frame: Option<crate::PreparedFrame>,
}

impl PreparedSharedState<'_> {
    /// publishes the already validated frame without calling luau again.
    pub fn commit(mut self) -> bool {
        let changed = self
            .runtime
            .commit_frame(self.frame.take().expect("a prepared update has a frame"));
        self.checkpoint = None;
        changed
    }
}

impl Drop for PreparedSharedState<'_> {
    fn drop(&mut self) {
        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.restore(self.runtime);
        }
    }
}

impl crate::Runtime {
    /// prepares a shared snapshot without publishing a tree or retiring its
    /// handlers and geometry. the guard borrows the runtime until it commits
    /// or rolls back, so events cannot interleave with a pending update.
    pub fn prepare_shared_states(
        &mut self,
        states: &SharedStates,
        validate: impl Fn(&[crate::SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PreparedSharedState<'_>> {
        let checkpoint = SharedStateCheckpoint::capture(self);
        let frame = (|| {
            self.set_shared_states(states)?;
            self.prepare_frame_checked(std::time::Instant::now(), &validate, false)
        })();
        match frame {
            Ok(frame) => Ok(PreparedSharedState {
                runtime: self,
                checkpoint: Some(checkpoint),
                frame: Some(frame),
            }),
            Err(error) => {
                checkpoint.restore(self);
                Err(error)
            }
        }
    }
}
