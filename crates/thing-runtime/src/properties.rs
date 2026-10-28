use mlua::{Error, Result, Value};
use thing_core::{Kind, Node};

pub(crate) fn string(value: Value, name: &str) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.to_str()?.to_owned()),
        _ => Err(Error::runtime(format!("{name} must be a string"))),
    }
}

fn number(value: Value, name: &str) -> Result<f64> {
    let number = match value {
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => return Err(Error::runtime(format!("{name} must be a number"))),
    };
    if !number.is_finite() {
        return Err(Error::runtime(format!("{name} must be finite")));
    }
    Ok(number)
}

fn boolean(value: Value, name: &str) -> Result<bool> {
    match value {
        Value::Boolean(value) => Ok(value),
        _ => Err(Error::runtime(format!("{name} must be a boolean"))),
    }
}

fn milliseconds(value: Value, name: &str) -> Result<u32> {
    let value = number(value, name)?;
    if value.fract() != 0.0 || !(0.0..=60_000.0).contains(&value) {
        return Err(Error::runtime(format!(
            "{name} must be a whole number between 0 and 60,000"
        )));
    }
    Ok(value as u32)
}

pub(crate) fn apply(node: &mut Node, name: &str, value: Value) -> Result<()> {
    match name {
        "text" if node.kind == Kind::Text => node.text = string(value, name)?,
        "source" if node.kind == Kind::Image => node.image.source = optional_string(value, name)?,
        "icon" if node.kind == Kind::Image => node.image.icon = optional_string(value, name)?,
        "app_id" if node.kind == Kind::Image => node.image.app_id = optional_string(value, name)?,
        "icon_theme" if node.kind == Kind::Image => node.image.theme = string(value, name)?,
        "visible" => node.visible = boolean(value, name)?,
        "enabled" => node.enabled = boolean(value, name)?,
        "value" if node.kind == Kind::Slider => node.slider.value = number(value, name)?,
        "min" if node.kind == Kind::Slider => node.slider.min = number(value, name)?,
        "max" if node.kind == Kind::Slider => node.slider.max = number(value, name)?,
        "step" if node.kind == Kind::Slider => node.slider.step = number(value, name)?,
        "open_on_hover" if node.kind == Kind::Popup => {
            node.popup.open_on_hover = boolean(value, name)?
        }
        "open_on_click" if node.kind == Kind::Popup => {
            node.popup.open_on_click = boolean(value, name)?
        }
        "open_delay" if node.kind == Kind::Popup => {
            node.popup.open_delay_ms = milliseconds(value, name)?
        }
        "close_delay" if node.kind == Kind::Popup => {
            node.popup.close_delay_ms = milliseconds(value, name)?
        }
        "style.width" => {
            node.style.width = if value.is_nil() {
                None
            } else {
                Some(number(value, name)? as f32)
            }
        }
        "style.height" => {
            node.style.height = if value.is_nil() {
                None
            } else {
                Some(number(value, name)? as f32)
            }
        }
        "style.padding" => node.style.padding = number(value, name)? as f32,
        "style.padding_x" => node.style.padding_x = optional_number(value, name)?,
        "style.padding_y" => node.style.padding_y = optional_number(value, name)?,
        "style.gap" => node.style.gap = number(value, name)? as f32,
        "style.grow" => node.style.grow = number(value, name)? as f32,
        "style.radius" => node.style.radius = number(value, name)? as f32,
        "style.border_width" => node.style.border_width = number(value, name)? as f32,
        "style.border_color" => {
            node.style.border_color = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.font_size" => node.style.font_size = number(value, name)? as f32,
        "style.font_weight" => node.style.font_weight = number(value, name)? as f32,
        "style.font_family" => node.style.font_family = string(value, name)?,
        "style.opacity" => node.style.opacity = number(value, name)? as f32,
        "style.background" => {
            node.style.background = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.foreground" => {
            node.style.foreground = string(value, name)?.parse().map_err(Error::external)?
        }
        _ => {
            return Err(Error::runtime(format!(
                "unknown property {name:?} on {:?}",
                node.kind
            )));
        }
    }
    Ok(())
}

fn optional_number(value: Value, name: &str) -> Result<Option<f32>> {
    if value.is_nil() {
        Ok(None)
    } else {
        Ok(Some(number(value, name)? as f32))
    }
}

fn optional_string(value: Value, name: &str) -> Result<Option<String>> {
    if value.is_nil() {
        Ok(None)
    } else {
        string(value, name).map(Some)
    }
}
