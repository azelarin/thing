//! commands that local clients call in a running configuration.
//!
//! a configuration declares each command once while it loads. the host calls
//! one by name with a bounded data value, through the same event controls and
//! candidate validation as other handlers, and returns the handler's data
//! result.

use std::{cell::RefCell, collections::BTreeMap, ffi::c_void, rc::Rc};

use mlua::{Error, Function, Lua, Result, Table, Value};

/// the most commands one configuration may declare.
pub const MAX_COMMANDS: usize = 64;
/// the deepest nesting of lists and maps in a call value.
pub const MAX_CALL_DEPTH: usize = 16;
/// the most values, counting every nested item and map key, in a call value.
pub const MAX_CALL_VALUES: usize = 1024;
/// the longest string, or map key, in a call value.
pub const MAX_CALL_STRING_BYTES: usize = 16 * 1024;

/// plain data passed to a command and returned from it.
#[derive(Clone, Debug, PartialEq)]
pub enum CallValue {
    Nil,
    Boolean(bool),
    Number(f64),
    String(String),
    List(Vec<CallValue>),
    Map(BTreeMap<String, CallValue>),
}

impl CallValue {
    /// checks the nesting, size, and number limits.
    pub fn validate(&self) -> std::result::Result<(), String> {
        fn visit(
            value: &CallValue,
            depth: usize,
            count: &mut usize,
        ) -> std::result::Result<(), String> {
            *count += 1;
            if *count > MAX_CALL_VALUES {
                return Err(format!(
                    "call values may contain at most {MAX_CALL_VALUES} items"
                ));
            }
            match value {
                CallValue::Nil | CallValue::Boolean(_) => Ok(()),
                CallValue::Number(number) if number.is_finite() => Ok(()),
                CallValue::Number(_) => Err("call numbers must be finite".into()),
                CallValue::String(text) => string(text),
                CallValue::List(items) => {
                    nested(depth)?;
                    items
                        .iter()
                        .try_for_each(|item| visit(item, depth + 1, count))
                }
                CallValue::Map(entries) => {
                    nested(depth)?;
                    entries.iter().try_for_each(|(key, item)| {
                        string(key)?;
                        *count += 1;
                        visit(item, depth + 1, count)
                    })
                }
            }
        }
        fn nested(depth: usize) -> std::result::Result<(), String> {
            if depth >= MAX_CALL_DEPTH {
                Err(format!(
                    "call values may nest at most {MAX_CALL_DEPTH} levels"
                ))
            } else {
                Ok(())
            }
        }
        fn string(text: &str) -> std::result::Result<(), String> {
            if text.len() > MAX_CALL_STRING_BYTES {
                Err(format!(
                    "call strings may be at most {MAX_CALL_STRING_BYTES} bytes"
                ))
            } else {
                Ok(())
            }
        }
        visit(self, 0, &mut 0)
    }

    pub(crate) fn to_lua(&self, lua: &Lua) -> Result<Value> {
        Ok(match self {
            Self::Nil => Value::Nil,
            Self::Boolean(value) => Value::Boolean(*value),
            Self::Number(value) => Value::Number(*value),
            Self::String(value) => Value::String(lua.create_string(value)?),
            Self::List(items) => {
                let table = lua.create_table_with_capacity(items.len(), 0)?;
                for (index, item) in items.iter().enumerate() {
                    table.raw_set(index + 1, item.to_table_value(lua)?)?;
                }
                Value::Table(table)
            }
            Self::Map(entries) => {
                let table = lua.create_table_with_capacity(0, entries.len())?;
                for (key, item) in entries {
                    table.raw_set(key.as_str(), item.to_table_value(lua)?)?;
                }
                Value::Table(table)
            }
        })
    }

    fn to_table_value(&self, lua: &Lua) -> Result<Value> {
        // assigning nil would remove a map entry or leave a hole in a list.
        match self {
            Self::Nil => Ok(Value::NULL),
            _ => self.to_lua(lua),
        }
    }

    /// converts a handler's result. lists are tables with keys `1..n`, maps
    /// have only string keys, and an empty table is an empty map.
    pub(crate) fn from_lua(value: Value) -> Result<Self> {
        let mut path = Vec::new();
        let mut count = 0;
        let result = from_lua(value, &mut path, &mut count)?;
        result.validate().map_err(Error::runtime)?;
        Ok(result)
    }
}

fn from_lua(value: Value, path: &mut Vec<*const c_void>, count: &mut usize) -> Result<CallValue> {
    *count += 1;
    if *count > MAX_CALL_VALUES {
        return Err(Error::runtime(format!(
            "call values may contain at most {MAX_CALL_VALUES} items"
        )));
    }
    Ok(match value {
        Value::Nil => CallValue::Nil,
        Value::LightUserData(value) if value.0.is_null() => CallValue::Nil,
        Value::Boolean(value) => CallValue::Boolean(value),
        Value::Integer(value) => CallValue::Number(value as f64),
        Value::Number(value) if value.is_finite() => CallValue::Number(value),
        Value::Number(_) => return Err(Error::runtime("call numbers must be finite")),
        Value::String(value) => CallValue::String(
            value
                .to_str()
                .map_err(|_| Error::runtime("call strings must be valid utf-8"))?
                .to_owned(),
        ),
        Value::Table(table) => table_value(table, path, count)?,
        other => {
            return Err(Error::runtime(format!(
                "a {} cannot be passed to or from a command",
                other.type_name()
            )));
        }
    })
}

fn table_value(
    table: Table,
    path: &mut Vec<*const c_void>,
    count: &mut usize,
) -> Result<CallValue> {
    let pointer = table.to_pointer();
    if path.contains(&pointer) {
        return Err(Error::runtime("call values cannot contain cycles"));
    }
    if path.len() >= MAX_CALL_DEPTH {
        return Err(Error::runtime(format!(
            "call values may nest at most {MAX_CALL_DEPTH} levels"
        )));
    }
    if table.metatable().is_some() {
        return Err(Error::runtime(
            "tables with metatables cannot be passed to or from a command",
        ));
    }
    let mut integers = BTreeMap::new();
    let mut strings = BTreeMap::new();
    path.push(pointer);
    for pair in table.pairs::<Value, Value>() {
        let (key, item) = pair?;
        match key {
            Value::Integer(index) if index >= 1 => {
                integers.insert(index, from_lua(item, path, count)?);
            }
            Value::Number(index)
                if index >= 1.0 && index.fract() == 0.0 && index <= 2f64.powi(53) =>
            {
                integers.insert(index as i64, from_lua(item, path, count)?);
            }
            Value::String(key) => {
                let key = key
                    .to_str()
                    .map_err(|_| Error::runtime("call map keys must be valid utf-8"))?
                    .to_owned();
                *count += 1;
                strings.insert(key, from_lua(item, path, count)?);
            }
            _ => {
                path.pop();
                return Err(Error::runtime("call tables need keys 1..n or string keys"));
            }
        }
    }
    path.pop();
    match (integers.is_empty(), strings.is_empty()) {
        (true, _) => Ok(CallValue::Map(strings)),
        (false, true) if integers.keys().copied().eq(1..=integers.len() as i64) => {
            Ok(CallValue::List(integers.into_values().collect()))
        }
        _ => Err(Error::runtime(
            "call tables need keys 1..n or string keys, not both or gaps",
        )),
    }
}

/// a command name: 1 to 64 ascii letters, digits, `-`, `_`, or `.`.
pub fn validate_command_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "command name {name:?} must be 1 to 64 letters, digits, '-', '_', or '.'"
        ));
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct Commands {
    handlers: BTreeMap<String, Function>,
    frozen: bool,
}

impl Commands {
    pub(crate) fn install(owner: &Rc<RefCell<Self>>, lua: &Lua, ui: &Table) -> Result<()> {
        ui.set("null", Value::NULL)?;
        let owner = Rc::downgrade(owner);
        ui.set(
            "command",
            lua.create_function(move |_, (name, handler): (Value, Value)| {
                let owner = owner
                    .upgrade()
                    .ok_or_else(|| Error::runtime("command runtime has ended"))?;
                let mut commands = owner.borrow_mut();
                if commands.frozen {
                    return Err(Error::runtime(
                        "declare commands while loading the configuration",
                    ));
                }
                let Value::String(name) = name else {
                    return Err(Error::runtime("ui.command name must be a string"));
                };
                let name = name.to_str()?.to_owned();
                validate_command_name(&name).map_err(Error::runtime)?;
                let Value::Function(handler) = handler else {
                    return Err(Error::runtime("ui.command handler must be a function"));
                };
                if commands.handlers.contains_key(&name) {
                    return Err(Error::runtime(format!(
                        "command {name:?} is already declared"
                    )));
                }
                if commands.handlers.len() >= MAX_COMMANDS {
                    return Err(Error::runtime(format!(
                        "a configuration may declare at most {MAX_COMMANDS} commands"
                    )));
                }
                commands.handlers.insert(name, handler);
                Ok(())
            })?,
        )
    }

    pub(crate) fn freeze(&mut self) {
        self.frozen = true;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<Function> {
        self.handlers.get(name).cloned()
    }
}
