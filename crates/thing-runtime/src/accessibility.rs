use mlua::{Error, Result, Value};
use thing_core::{
    AccessibilityChecked, AccessibilityLive, AccessibilityRole, MAX_ACCESSIBILITY_TEXT_BYTES, Node,
};

use crate::Runtime;

pub(crate) fn parse(runtime: &mut Runtime, node: &mut Node, value: Value) -> Result<()> {
    let Value::Table(properties) = value else {
        return Err(Error::runtime("accessibility must be a static table"));
    };
    for pair in properties.pairs::<String, Value>() {
        let (name, value) = pair?;
        if !matches!(
            name.as_str(),
            "label"
                | "description"
                | "role"
                | "selected"
                | "checked"
                | "expanded"
                | "value_text"
                | "live"
                | "hidden"
        ) {
            return Err(Error::runtime(format!(
                "unknown accessibility property {name:?}"
            )));
        }
        runtime.property(node, format!("accessibility.{name}"), value)?;
    }
    Ok(())
}

fn text(value: Value, name: &str) -> Result<Option<String>> {
    if value.is_nil() {
        return Ok(None);
    }
    let value = crate::properties::string(value, &format!("accessibility.{name}"))?;
    if value.len() > MAX_ACCESSIBILITY_TEXT_BYTES || value.contains('\0') {
        return Err(Error::runtime(format!(
            "accessibility.{name} must be at most 4096 bytes without NUL"
        )));
    }
    Ok(Some(value))
}

fn boolean(value: Value, name: &str) -> Result<Option<bool>> {
    match value {
        Value::Nil => Ok(None),
        Value::Boolean(value) => Ok(Some(value)),
        _ => Err(Error::runtime(format!(
            "accessibility.{name} must be a boolean or nil"
        ))),
    }
}

pub(crate) fn apply(node: &mut Node, name: &str, value: Value) -> Result<()> {
    let metadata = &mut node.accessibility;
    match name {
        "label" => metadata.label = text(value, name)?,
        "description" => metadata.description = text(value, name)?,
        "value_text" => metadata.value_text = text(value, name)?,
        "selected" => metadata.selected = boolean(value, name)?,
        "expanded" => metadata.expanded = boolean(value, name)?,
        "hidden" => metadata.hidden = boolean(value, name)?.unwrap_or(false),
        "checked" => {
            metadata.checked = match value {
                Value::Nil => None,
                Value::Boolean(true) => Some(AccessibilityChecked::Checked),
                Value::Boolean(false) => Some(AccessibilityChecked::Unchecked),
                Value::String(value) if value.to_str()?.as_ref() == "mixed" => {
                    Some(AccessibilityChecked::Mixed)
                }
                _ => {
                    return Err(Error::runtime(
                        "accessibility.checked must be a boolean, 'mixed', or nil",
                    ));
                }
            }
        }
        "role" => {
            metadata.role = match value {
                Value::Nil => None,
                value => {
                    let name = crate::properties::string(value, "accessibility.role")?;
                    Some(AccessibilityRole::from_name(&name).ok_or_else(|| {
                        Error::runtime(format!("unknown accessibility role {name:?}"))
                    })?)
                }
            }
        }
        "live" => {
            metadata.live = match value {
                Value::Nil => None,
                value => Some(
                    match crate::properties::string(value, "accessibility.live")?.as_str() {
                        "polite" => AccessibilityLive::Polite,
                        "assertive" => AccessibilityLive::Assertive,
                        _ => {
                            return Err(Error::runtime(
                                "accessibility.live must be 'polite' or 'assertive'",
                            ));
                        }
                    },
                ),
            }
        }
        _ => {
            return Err(Error::runtime(format!(
                "unknown accessibility property {name:?}"
            )));
        }
    }
    Ok(())
}
