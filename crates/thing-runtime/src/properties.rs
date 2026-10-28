use mlua::{Error, Result, Table, Value};
use thing_core::{
    CrossAlign, ImageFit, KeyboardMode, Kind, MainAlign, Node, PanelEdge, PanelInputRect,
    PanelInputRegion, PanelMargins, PopupAlignment, PopupAnchor, PopupConstraints, PopupOffset,
    SurfaceAnchors, SurfaceLayer, TextAlign, TextOverflow,
};

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

fn integer(value: Value, name: &str) -> Result<i32> {
    let number = number(value, name)?;
    if number.fract() != 0.0 || number < i32::MIN as f64 || number > i32::MAX as f64 {
        return Err(Error::runtime(format!("{name} must be an integer")));
    }
    Ok(number as i32)
}

fn panel_edge(value: Value) -> Result<PanelEdge> {
    match string(value, "edge")?.as_str() {
        "top" => Ok(PanelEdge::Top),
        "bottom" => Ok(PanelEdge::Bottom),
        "left" => Ok(PanelEdge::Left),
        "right" => Ok(PanelEdge::Right),
        _ => Err(Error::runtime(
            "edge must be 'top', 'bottom', 'left', or 'right'",
        )),
    }
}

fn surface_anchors(value: Value) -> Result<Option<SurfaceAnchors>> {
    if value.is_nil() {
        return Ok(None);
    }
    let Value::Table(table) = value else {
        return Err(Error::runtime("anchors must be a table"));
    };
    let mut anchors = SurfaceAnchors::default();
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        let target = match name.as_str() {
            "top" => &mut anchors.top,
            "right" => &mut anchors.right,
            "bottom" => &mut anchors.bottom,
            "left" => &mut anchors.left,
            _ => return Err(Error::runtime(format!("unknown anchor {name:?}"))),
        };
        *target = boolean(value, &format!("anchors.{name}"))?;
    }
    Ok(Some(anchors))
}

fn surface_layer(value: Value) -> Result<SurfaceLayer> {
    match string(value, "layer")?.as_str() {
        "background" => Ok(SurfaceLayer::Background),
        "bottom" => Ok(SurfaceLayer::Bottom),
        "top" => Ok(SurfaceLayer::Top),
        "overlay" => Ok(SurfaceLayer::Overlay),
        _ => Err(Error::runtime(
            "layer must be 'background', 'bottom', 'top', or 'overlay'",
        )),
    }
}

fn keyboard_mode(value: Value) -> Result<KeyboardMode> {
    match string(value, "keyboard_mode")?.as_str() {
        "none" => Ok(KeyboardMode::None),
        "on_demand" => Ok(KeyboardMode::OnDemand),
        "exclusive" => Ok(KeyboardMode::Exclusive),
        _ => Err(Error::runtime(
            "keyboard_mode must be 'none', 'on_demand', or 'exclusive'",
        )),
    }
}

fn panel_margins(value: Value) -> Result<PanelMargins> {
    let Value::Table(table) = value else {
        return Err(Error::runtime("margins must be a table"));
    };
    let mut margins = PanelMargins::default();
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        let target = match name.as_str() {
            "top" => &mut margins.top,
            "right" => &mut margins.right,
            "bottom" => &mut margins.bottom,
            "left" => &mut margins.left,
            _ => return Err(Error::runtime(format!("unknown margin {name:?}"))),
        };
        *target = integer(value, &format!("margins.{name}"))?;
    }
    Ok(margins)
}

fn exclusive_zone(value: Value) -> Result<Option<i32>> {
    if value.is_nil() {
        Ok(None)
    } else {
        Ok(Some(integer(value, "exclusive_zone")?))
    }
}

fn panel_input_region(value: Value) -> Result<PanelInputRegion> {
    match value {
        Value::Nil => Ok(PanelInputRegion::Full),
        Value::Boolean(false) => Ok(PanelInputRegion::Rectangles(Vec::new())),
        Value::Table(table) => panel_input_rectangles(table),
        _ => Err(Error::runtime(
            "input_region must be false, a rectangle, or a rectangles table",
        )),
    }
}

fn panel_input_rectangles(table: Table) -> Result<PanelInputRegion> {
    let entries = table
        .clone()
        .pairs::<Value, Value>()
        .collect::<Result<Vec<_>>>()?;
    if entries.is_empty() {
        return Ok(PanelInputRegion::Rectangles(
            vec![PanelInputRect::default()],
        ));
    }
    if entries.len() == 1
        && matches!(
            &entries[0].0,
            Value::String(name) if name.as_bytes() == b"rectangles"
        )
    {
        let Value::Table(rectangles) = table.raw_get::<Value>("rectangles")? else {
            return Err(Error::runtime(
                "input_region.rectangles must be a dense rectangle list",
            ));
        };
        return panel_input_rect_list(rectangles);
    }
    if entries
        .iter()
        .all(|(key, _)| matches!(key, Value::String(_)))
    {
        return panel_input_rect(table).map(|rect| PanelInputRegion::Rectangles(vec![rect]));
    }
    Err(Error::runtime(
        "input_region must be a rectangle or a rectangles table",
    ))
}

fn panel_input_rect_list(table: Table) -> Result<PanelInputRegion> {
    let length = table.raw_len();
    let entries = table
        .clone()
        .pairs::<Value, Value>()
        .collect::<Result<Vec<_>>>()?;
    if entries.len() != length
        || !entries.iter().all(|(key, _)| {
            matches!(key, Value::Integer(index) if *index >= 1 && (*index as usize) <= length)
        })
    {
        return Err(Error::runtime(
            "input_region rectangles must be a dense list",
        ));
    }
    if length > 256 {
        return Err(Error::runtime(
            "input_region accepts at most 256 rectangles",
        ));
    }
    let rectangles = (1..=length)
        .map(|index| {
            let value = table.raw_get(index)?;
            let Value::Table(rectangle) = value else {
                return Err(Error::runtime(
                    "input_region list entries must be rectangle tables",
                ));
            };
            panel_input_rect(rectangle)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PanelInputRegion::Rectangles(rectangles))
}

fn panel_input_rect(table: Table) -> Result<PanelInputRect> {
    let mut rect = PanelInputRect::default();
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        match name.as_str() {
            "x" => rect.x = integer(value, "input_region.x")?,
            "y" => rect.y = integer(value, "input_region.y")?,
            "width" => rect.width = Some(positive_dimension(value, "input_region.width")?),
            "height" => rect.height = Some(positive_dimension(value, "input_region.height")?),
            _ => {
                return Err(Error::runtime(format!(
                    "unknown input_region property {name:?}"
                )));
            }
        }
    }
    Ok(rect)
}

fn positive_dimension(value: Value, name: &str) -> Result<u32> {
    let value = integer(value, name)?;
    u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::runtime(format!("{name} must be a positive integer")))
}

fn max_lines(value: Value, name: &str) -> Result<Option<u32>> {
    if value.is_nil() {
        return Ok(None);
    }
    let value = number(value, name)?;
    if value.fract() != 0.0 || !(1.0..=u32::MAX as f64).contains(&value) {
        return Err(Error::runtime(format!(
            "{name} must be a positive whole number"
        )));
    }
    Ok(Some(value as u32))
}

fn text_align(value: Value, name: &str) -> Result<TextAlign> {
    match string(value, name)?.as_str() {
        "start" => Ok(TextAlign::Start),
        "center" => Ok(TextAlign::Center),
        "end" => Ok(TextAlign::End),
        _ => Err(Error::runtime(format!(
            "{name} must be \"start\", \"center\", or \"end\""
        ))),
    }
}

fn text_overflow(value: Value, name: &str) -> Result<TextOverflow> {
    match string(value, name)?.as_str() {
        "clip" => Ok(TextOverflow::Clip),
        "ellipsis" => Ok(TextOverflow::Ellipsis),
        _ => Err(Error::runtime(format!(
            "{name} must be \"clip\" or \"ellipsis\""
        ))),
    }
}

fn image_fit(value: Value, name: &str) -> Result<ImageFit> {
    match string(value, name)?.as_str() {
        "contain" => Ok(ImageFit::Contain),
        "cover" => Ok(ImageFit::Cover),
        "fill" => Ok(ImageFit::Fill),
        _ => Err(Error::runtime(format!(
            "{name} must be 'contain', 'cover', or 'fill'"
        ))),
    }
}

fn cross_align(value: Value, name: &str) -> Result<CrossAlign> {
    match string(value, name)?.as_str() {
        "start" => Ok(CrossAlign::Start),
        "center" => Ok(CrossAlign::Center),
        "end" => Ok(CrossAlign::End),
        "stretch" => Ok(CrossAlign::Stretch),
        _ => Err(Error::runtime(format!(
            "{name} must be 'start', 'center', 'end', or 'stretch'"
        ))),
    }
}

fn optional_cross_align(value: Value, name: &str) -> Result<Option<CrossAlign>> {
    if value.is_nil() {
        Ok(None)
    } else {
        cross_align(value, name).map(Some)
    }
}

fn main_align(value: Value, name: &str) -> Result<MainAlign> {
    match string(value, name)?.as_str() {
        "start" => Ok(MainAlign::Start),
        "center" => Ok(MainAlign::Center),
        "end" => Ok(MainAlign::End),
        "space_between" => Ok(MainAlign::SpaceBetween),
        "space_around" => Ok(MainAlign::SpaceAround),
        "space_evenly" => Ok(MainAlign::SpaceEvenly),
        _ => Err(Error::runtime(format!(
            "{name} must be 'start', 'center', 'end', 'space_between', 'space_around', or 'space_evenly'"
        ))),
    }
}

fn popup_anchor(value: Value) -> Result<PopupAnchor> {
    match string(value, "anchor")?.as_str() {
        "auto" => Ok(PopupAnchor::Auto),
        "top" => Ok(PopupAnchor::Top),
        "bottom" => Ok(PopupAnchor::Bottom),
        "left" => Ok(PopupAnchor::Left),
        "right" => Ok(PopupAnchor::Right),
        _ => Err(Error::runtime(
            "anchor must be 'auto', 'top', 'bottom', 'left', or 'right'",
        )),
    }
}

fn popup_alignment(value: Value) -> Result<PopupAlignment> {
    match string(value, "alignment")?.as_str() {
        "start" => Ok(PopupAlignment::Start),
        "center" => Ok(PopupAlignment::Center),
        "end" => Ok(PopupAlignment::End),
        _ => Err(Error::runtime(
            "alignment must be 'start', 'center', or 'end'",
        )),
    }
}

fn popup_offset(value: Value) -> Result<PopupOffset> {
    if value.is_nil() {
        return Ok(PopupOffset::default());
    }
    let Value::Table(table) = value else {
        return Err(Error::runtime("offset must be a table"));
    };
    let mut offset = PopupOffset::default();
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        let target = match name.as_str() {
            "x" => &mut offset.x,
            "y" => &mut offset.y,
            _ => return Err(Error::runtime(format!("unknown offset property {name:?}"))),
        };
        *target = integer(value, &format!("offset.{name}"))?;
    }
    Ok(offset)
}

fn popup_constraints(value: Value) -> Result<Option<PopupConstraints>> {
    if value.is_nil() {
        return Ok(None);
    }
    let Value::Table(table) = value else {
        return Err(Error::runtime("constraints must be a table"));
    };
    let mut constraints = PopupConstraints::default();
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        let target = match name.as_str() {
            "flip_x" => &mut constraints.flip_x,
            "flip_y" => &mut constraints.flip_y,
            "slide_x" => &mut constraints.slide_x,
            "slide_y" => &mut constraints.slide_y,
            _ => {
                return Err(Error::runtime(format!(
                    "unknown constraint property {name:?}"
                )));
            }
        };
        *target = boolean(value, &format!("constraints.{name}"))?;
    }
    Ok(Some(constraints))
}

pub(crate) fn apply(node: &mut Node, name: &str, value: Value) -> Result<()> {
    match name {
        "text" if node.kind == Kind::Text => node.text = string(value, name)?,
        "max_lines" if node.kind == Kind::Text => node.max_lines = max_lines(value, name)?,
        "overflow" if node.kind == Kind::Text => node.text_overflow = text_overflow(value, name)?,
        "source" if node.kind == Kind::Image => node.image.source = optional_string(value, name)?,
        "icon" if node.kind == Kind::Image => node.image.icon = optional_string(value, name)?,
        "app_id" if node.kind == Kind::Image => node.image.app_id = optional_string(value, name)?,
        "icon_theme" if node.kind == Kind::Image => node.image.theme = string(value, name)?,
        "fit" if node.kind == Kind::Image => node.image_fit = image_fit(value, name)?,
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
        "anchor" if node.kind == Kind::Popup => node.popup.anchor = popup_anchor(value)?,
        "alignment" if node.kind == Kind::Popup => node.popup.alignment = popup_alignment(value)?,
        "offset" if node.kind == Kind::Popup => node.popup.offset = popup_offset(value)?,
        "constraints" if node.kind == Kind::Popup => {
            node.popup.constraints = popup_constraints(value)?
        }
        "edge" if node.kind == Kind::Panel => node.panel.edge = panel_edge(value)?,
        "anchors" if node.kind == Kind::Panel => node.panel.anchors = surface_anchors(value)?,
        "layer" if node.kind == Kind::Panel => node.panel.layer = surface_layer(value)?,
        "keyboard_mode" if node.kind == Kind::Panel => {
            node.panel.keyboard_mode = keyboard_mode(value)?
        }
        "margins" if node.kind == Kind::Panel => node.panel.margins = panel_margins(value)?,
        "exclusive_zone" if node.kind == Kind::Panel => {
            node.panel.exclusive_zone = exclusive_zone(value)?
        }
        "input_region" if node.kind == Kind::Panel => {
            node.panel.input_region = panel_input_region(value)?
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
        "style.min_width" => node.style.min_width = optional_number(value, name)?,
        "style.max_width" => node.style.max_width = optional_number(value, name)?,
        "style.min_height" => node.style.min_height = optional_number(value, name)?,
        "style.max_height" => node.style.max_height = optional_number(value, name)?,
        "style.padding" => node.style.padding = number(value, name)? as f32,
        "style.padding_x" => node.style.padding_x = optional_number(value, name)?,
        "style.padding_y" => node.style.padding_y = optional_number(value, name)?,
        "style.gap" => node.style.gap = number(value, name)? as f32,
        "style.grow" => node.style.grow = number(value, name)? as f32,
        "style.shrink" => node.style.shrink = optional_number(value, name)?,
        "style.align" => node.style.align = cross_align(value, name)?,
        "style.align_self" => node.style.align_self = optional_cross_align(value, name)?,
        "style.align_content" => node.style.align_content = cross_align(value, name)?,
        "style.justify" => node.style.justify = main_align(value, name)?,
        "style.wrap" => node.style.wrap = boolean(value, name)?,
        "style.radius" => node.style.radius = number(value, name)? as f32,
        "style.border_width" => node.style.border_width = number(value, name)? as f32,
        "style.border_color" => {
            node.style.border_color = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.hover_overlay" => {
            node.style.hover_overlay = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.pressed_overlay" => {
            node.style.pressed_overlay = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.disabled_opacity" => node.style.disabled_opacity = number(value, name)? as f32,
        "style.focus_ring_color" => {
            node.style.focus_ring_color = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.focus_ring_width" => node.style.focus_ring_width = number(value, name)? as f32,
        "style.font_size" => node.style.font_size = number(value, name)? as f32,
        "style.font_weight" => node.style.font_weight = number(value, name)? as f32,
        "style.font_family" => node.style.font_family = string(value, name)?,
        "style.text_align" if node.kind == Kind::Text => {
            node.style.text_align = text_align(value, name)?
        }
        "style.tabular_digits" if node.kind == Kind::Text => {
            node.style.tabular_digits = boolean(value, name)?
        }
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
