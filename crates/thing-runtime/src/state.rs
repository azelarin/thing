use mlua::{Error, Lua, Result, UserData, UserDataMethods, Value};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

/// values supported by reactive state. table mutation would bypass tracking.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
    Nil,
    Boolean(bool),
    Number(f64),
    String(String),
}

impl Scalar {
    pub(crate) fn from_lua(value: Value) -> Result<Self> {
        match value {
            Value::Nil => Ok(Self::Nil),
            Value::Boolean(value) => Ok(Self::Boolean(value)),
            Value::Integer(value) => Ok(Self::Number(value as f64)),
            Value::Number(value) if value.is_finite() => Ok(Self::Number(value)),
            Value::String(value) => Ok(Self::String(value.to_str()?.to_owned())),
            _ => Err(Error::runtime(
                "state must be nil, a boolean, a finite number, or a UTF-8 string",
            )),
        }
    }
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        match self {
            Self::Nil => Ok(Value::Nil),
            Self::Boolean(value) => Ok(Value::Boolean(value)),
            Self::Number(value) => Ok(Value::Number(value)),
            Self::String(value) => Ok(Value::String(lua.create_string(value)?)),
        }
    }
}

#[derive(Default)]
pub(crate) struct Tracker {
    values: Vec<Scalar>,
    subscribers: BTreeMap<usize, BTreeSet<usize>>,
    evaluating: Option<usize>,
    reads: BTreeSet<usize>,
    pub dirty: BTreeSet<usize>,
}

impl Tracker {
    pub fn insert(&mut self, value: Scalar) -> Result<usize> {
        if self.evaluating.is_some() {
            return Err(Error::runtime(
                "create state before defining property functions",
            ));
        }
        if self.values.len() >= 10_000 {
            return Err(Error::runtime("configuration exceeds 10,000 state values"));
        }
        let id = self.values.len();
        self.values.push(value);
        Ok(id)
    }
    pub fn begin(&mut self, binding: usize) {
        self.evaluating = Some(binding);
        self.reads.clear();
    }
    pub fn finish(&mut self, binding: usize) {
        self.evaluating = None;
        for subscriptions in self.subscribers.values_mut() {
            subscriptions.remove(&binding);
        }
        for source in std::mem::take(&mut self.reads) {
            self.subscribers.entry(source).or_default().insert(binding);
        }
    }
    pub fn read(&mut self, id: usize) -> Scalar {
        if self.evaluating.is_some() {
            self.reads.insert(id);
        }
        self.values[id].clone()
    }
    pub fn forget(&mut self, binding: usize) {
        self.dirty.remove(&binding);
        self.subscribers.retain(|_, bindings| {
            bindings.remove(&binding);
            !bindings.is_empty()
        });
    }
    pub fn write(&mut self, id: usize, value: Scalar) -> Result<()> {
        if self.evaluating.is_some() {
            return Err(Error::runtime(
                "property functions must not change state; use an event handler",
            ));
        }
        if self.values[id] != value {
            self.values[id] = value;
            if let Some(bindings) = self.subscribers.get(&id) {
                self.dirty.extend(bindings);
            }
        }
        Ok(())
    }
}

pub(crate) struct Signal {
    pub id: usize,
    pub tracker: Rc<RefCell<Tracker>>,
    pub readonly: bool,
}

impl UserData for Signal {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.id).into_lua(lua)
        });
        methods.add_method("set", |_, this, value: Value| {
            if this.readonly {
                return Err(Error::runtime(
                    "service values are read-only; use service actions",
                ));
            }
            this.tracker
                .borrow_mut()
                .write(this.id, Scalar::from_lua(value)?)
        });
    }
}
