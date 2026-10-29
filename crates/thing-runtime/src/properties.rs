use mlua::{Error, Result, Table, Value};
use thing_core::{
    Color, CrossAlign, ImageFit, ImageSource, KeyboardMode, Kind, MainAlign, Node, PanelEdge,
    PanelInputRect, PanelInputRegion, PanelMargins, PathFillRule, PathFit, PathStrokeCap,
    PathStrokeJoin, PopupAlignment, PopupAnchor, PopupConstraints, PopupOffset, SurfaceAnchors,
    SurfaceLayer, TextAlign, TextOverflow, TransformOrigin, ViewBox,
};

use crate::services::ImageHandle;
use crate::transition::Resolved;

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

fn color(value: Value, name: &str) -> Result<thing_core::Color> {
    string(value, name)?.parse().map_err(Error::external)
}

fn brush(value: Value, name: &str) -> Result<thing_core::Brush> {
    let Value::Table(table) = value else {
        return color(value, name).map(Into::into);
    };
    for field in table.clone().pairs::<String, Value>() {
        let (field, _) = field?;
        if !matches!(field.as_str(), "kind" | "from" | "to" | "stops") {
            return Err(Error::runtime(format!("unknown brush property {field:?}")));
        }
    }
    if string(table.raw_get("kind")?, "brush.kind")? != "linear" {
        return Err(Error::runtime("brush kind must be 'linear'"));
    }
    fn point(value: Value) -> Result<thing_core::GradientPoint> {
        let Value::Table(table) = value else {
            return Err(Error::runtime("gradient endpoints must be point tables"));
        };
        for pair in table.clone().pairs::<String, Value>() {
            let (field, _) = pair?;
            if !matches!(field.as_str(), "x" | "y") {
                return Err(Error::runtime(format!(
                    "unknown gradient endpoint property {field:?}"
                )));
            }
        }
        Ok(thing_core::GradientPoint {
            x: number(table.raw_get("x")?, "gradient x")?,
            y: number(table.raw_get("y")?, "gradient y")?,
        })
    }
    let from = point(table.raw_get("from")?)?;
    let to = point(table.raw_get("to")?)?;
    let stops: Table = table.raw_get("stops")?;
    let length = stops.raw_len();
    if !(2..=16).contains(&length) {
        return Err(Error::runtime("a gradient needs 2 through 16 stops"));
    }
    for pair in stops.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let index = match key {
            Value::Integer(index) => index as f64,
            Value::Number(index) => index,
            _ => return Err(Error::runtime("gradient stops must be a dense array")),
        };
        if !index.is_finite() || index.fract() != 0.0 || !(1.0..=length as f64).contains(&index) {
            return Err(Error::runtime("gradient stops must be a dense array"));
        }
    }
    let mut parsed = Vec::with_capacity(length);
    let mut previous_position = 0.0_f64;
    for index in 1..=length {
        let stop: Table = stops.raw_get(index)?;
        for pair in stop.clone().pairs::<String, Value>() {
            let (field, _) = pair?;
            if !matches!(field.as_str(), "position" | "color") {
                return Err(Error::runtime(format!(
                    "unknown gradient stop property {field:?}"
                )));
            }
        }
        let position = number(stop.raw_get("position")?, "gradient stop position")?;
        if !(0.0..=1.0).contains(&position) || position < previous_position {
            return Err(Error::runtime(
                "gradient stop positions must be ordered and between 0 and 1",
            ));
        }
        previous_position = position;
        parsed.push(thing_core::GradientStop {
            position,
            color: color(stop.raw_get("color")?, "gradient stop color")?,
        });
    }
    let brush = thing_core::Brush::Linear(thing_core::LinearGradient {
        from,
        to,
        stops: parsed,
    });
    brush.validate().map_err(Error::external)?;
    Ok(brush)
}

fn optional_brush(value: Value, name: &str) -> Result<Option<thing_core::Brush>> {
    if value.is_nil() {
        Ok(None)
    } else {
        brush(value, name).map(Some)
    }
}

fn path_view_box(value: Value) -> Result<ViewBox> {
    let Value::Table(table) = value else {
        return Err(Error::runtime("view_box must be a table"));
    };
    let mut x = None;
    let mut y = None;
    let mut width = None;
    let mut height = None;
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        let value = number(value, &format!("view_box.{name}"))? as f32;
        match name.as_str() {
            "x" => x = Some(value),
            "y" => y = Some(value),
            "width" => width = Some(value),
            "height" => height = Some(value),
            _ => {
                return Err(Error::runtime(format!(
                    "unknown view_box property {name:?}"
                )));
            }
        }
    }
    ViewBox::new(
        x.ok_or_else(|| Error::runtime("view_box.x is required"))?,
        y.ok_or_else(|| Error::runtime("view_box.y is required"))?,
        width.ok_or_else(|| Error::runtime("view_box.width is required"))?,
        height.ok_or_else(|| Error::runtime("view_box.height is required"))?,
    )
    .map_err(Error::external)
}

fn path_fit(value: Value) -> Result<PathFit> {
    match string(value, "fit")?.as_str() {
        "contain" => Ok(PathFit::Contain),
        "cover" => Ok(PathFit::Cover),
        "fill" => Ok(PathFit::Fill),
        _ => Err(Error::runtime("fit must be 'contain', 'cover', or 'fill'")),
    }
}

fn path_fill_rule(value: Value) -> Result<PathFillRule> {
    match string(value, "fill_rule")?.as_str() {
        "nonzero" => Ok(PathFillRule::NonZero),
        "evenodd" => Ok(PathFillRule::EvenOdd),
        _ => Err(Error::runtime("fill_rule must be 'nonzero' or 'evenodd'")),
    }
}

fn path_stroke_cap(value: Value) -> Result<PathStrokeCap> {
    match string(value, "stroke_cap")?.as_str() {
        "butt" => Ok(PathStrokeCap::Butt),
        "round" => Ok(PathStrokeCap::Round),
        "square" => Ok(PathStrokeCap::Square),
        _ => Err(Error::runtime(
            "stroke_cap must be 'butt', 'round', or 'square'",
        )),
    }
}

fn path_stroke_join(value: Value) -> Result<PathStrokeJoin> {
    match string(value, "stroke_join")?.as_str() {
        "miter" => Ok(PathStrokeJoin::Miter),
        "round" => Ok(PathStrokeJoin::Round),
        "bevel" => Ok(PathStrokeJoin::Bevel),
        _ => Err(Error::runtime(
            "stroke_join must be 'miter', 'round', or 'bevel'",
        )),
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

fn transform_origin(value: Value) -> Result<TransformOrigin> {
    let Value::Table(table) = value else {
        return Err(Error::runtime("style.transform_origin must be a table"));
    };
    let mut origin = TransformOrigin::default();
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        let value = number(value, &format!("style.transform_origin.{name}"))? as f32;
        match name.as_str() {
            "x" => origin.x = value,
            "y" => origin.y = value,
            "offset_x" => origin.offset_x = value,
            "offset_y" => origin.offset_y = value,
            _ => {
                return Err(Error::runtime(format!(
                    "unknown transform_origin property {name:?}"
                )));
            }
        }
    }
    Ok(origin)
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

pub(crate) fn apply_transition(node: &mut Node, name: &str, value: Resolved) -> Result<()> {
    match value {
        Resolved::Number(value) => apply(node, name, Value::Number(f64::from(value))),
        Resolved::Color(value) => apply_color(node, name, value),
    }
}

pub(crate) fn apply_animation_frame(node: &mut Node, name: &str, value: Resolved) -> Result<()> {
    // keep spring momentum outside a property's range while publishing only
    // values the widget tree can represent.
    let value = match value {
        Resolved::Number(value) => Resolved::Number(match name {
            "style.opacity" => value.clamp(0.0, 1.0),
            "style.translate_x" | "style.translate_y" => value.clamp(-100_000.0, 100_000.0),
            "style.scale_x" | "style.scale_y" => value.clamp(-100.0, 100.0),
            "style.rotation" => value.clamp(-1_000_000.0, 1_000_000.0),
            "style.width" | "style.height" | "style.padding" | "style.padding_x"
            | "style.padding_y" | "style.gap" | "style.grow" | "style.radius"
            | "style.border_width" => value.clamp(0.0, 100_000.0),
            _ => value,
        }),
        color => color,
    };
    apply_transition(node, name, value)
}

pub(crate) fn apply(node: &mut Node, name: &str, value: Value) -> Result<()> {
    match name {
        "text" if node.kind == Kind::Text => node.text = string(value, name)?,
        "value" if node.kind == Kind::TextInput => node.text = string(value, name)?,
        "placeholder" if matches!(node.kind, Kind::TextInput | Kind::SecretInput) => {
            node.placeholder = string(value, name)?
        }
        "max_lines" if node.kind == Kind::Text => node.max_lines = max_lines(value, name)?,
        "overflow" if node.kind == Kind::Text => node.text_overflow = text_overflow(value, name)?,
        "source" if node.kind == Kind::Image => {
            let (source, memory) = image_source(value, name)?;
            node.image.source = source;
            node.image.memory = memory;
        }
        "icon" if node.kind == Kind::Image => node.image.icon = optional_string(value, name)?,
        "app_id" if node.kind == Kind::Image => node.image.app_id = optional_string(value, name)?,
        "icon_theme" if node.kind == Kind::Image => node.image.theme = string(value, name)?,
        "fit" if node.kind == Kind::Image => node.image_fit = image_fit(value, name)?,
        "data" if node.kind == Kind::Path => node
            .path
            .set_data(string(value, name)?)
            .map_err(Error::external)?,
        "view_box" if node.kind == Kind::Path => node.path.view_box = Some(path_view_box(value)?),
        "fit" if node.kind == Kind::Path => node.path.fit = path_fit(value)?,
        "fill" if node.kind == Kind::Path => node.path.fill = optional_brush(value, name)?,
        "fill_rule" if node.kind == Kind::Path => node.path.fill_rule = path_fill_rule(value)?,
        "stroke" if node.kind == Kind::Path => node.path.stroke = color(value, name)?,
        "stroke_width" if node.kind == Kind::Path => {
            node.path.stroke_width = number(value, name)? as f32
        }
        "stroke_cap" if node.kind == Kind::Path => node.path.stroke_cap = path_stroke_cap(value)?,
        "stroke_join" if node.kind == Kind::Path => {
            node.path.stroke_join = path_stroke_join(value)?
        }
        "visible" => node.visible = boolean(value, name)?,
        "enabled" => node.enabled = boolean(value, name)?,
        "value" if matches!(node.kind, Kind::Slider | Kind::Range) => {
            node.slider.value = number(value, name)?
        }
        "min" if matches!(node.kind, Kind::Slider | Kind::Range) => {
            node.slider.min = number(value, name)?
        }
        "max" if matches!(node.kind, Kind::Slider | Kind::Range) => {
            node.slider.max = number(value, name)?
        }
        "step" if matches!(node.kind, Kind::Slider | Kind::Range) => {
            node.slider.step = number(value, name)?
        }
        "inset" if node.kind == Kind::Range => node.range_inset = number(value, name)? as f32,
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
        "idle_inhibit" if matches!(node.kind, Kind::Panel | Kind::Popup) => {
            node.idle_inhibit = boolean(value, name)?
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
        "style.width_fraction" => node.style.width_fraction = optional_number(value, name)?,
        "style.height_fraction" => node.style.height_fraction = optional_number(value, name)?,
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
        "style.border_color" => node.style.border_color = color(value, name)?,
        "style.hover_overlay" => node.style.hover_overlay = color(value, name)?,
        "style.pressed_overlay" => node.style.pressed_overlay = color(value, name)?,
        "style.disabled_opacity" => node.style.disabled_opacity = number(value, name)? as f32,
        "style.focus_ring_color" => node.style.focus_ring_color = color(value, name)?,
        "style.focus_ring_width" => node.style.focus_ring_width = number(value, name)? as f32,
        "style.placeholder_color" if matches!(node.kind, Kind::TextInput | Kind::SecretInput) => {
            node.style.placeholder_color = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.selection_color" if matches!(node.kind, Kind::TextInput | Kind::SecretInput) => {
            node.style.selection_color = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.caret_color" if matches!(node.kind, Kind::TextInput | Kind::SecretInput) => {
            node.style.caret_color = string(value, name)?.parse().map_err(Error::external)?
        }
        "style.font_size" => node.style.font_size = number(value, name)? as f32,
        "style.font_weight" => node.style.font_weight = number(value, name)? as f32,
        "style.font_family" => node.style.font_family = string(value, name)?,
        "style.text_align"
            if matches!(node.kind, Kind::Text | Kind::TextInput | Kind::SecretInput) =>
        {
            node.style.text_align = text_align(value, name)?
        }
        "style.tabular_digits"
            if matches!(node.kind, Kind::Text | Kind::TextInput | Kind::SecretInput) =>
        {
            node.style.tabular_digits = boolean(value, name)?
        }
        "style.opacity" => node.style.opacity = number(value, name)? as f32,
        "style.background" => node.style.background = brush(value, name)?,
        "style.foreground" => node.style.foreground = color(value, name)?,
        "style.translate_x" => node.style.translate_x = number(value, name)? as f32,
        "style.translate_y" => node.style.translate_y = number(value, name)? as f32,
        "style.scale_x" => node.style.scale_x = number(value, name)? as f32,
        "style.scale_y" => node.style.scale_y = number(value, name)? as f32,
        "style.rotation" => node.style.rotation = number(value, name)? as f32,
        "style.transform_origin" => node.style.transform_origin = transform_origin(value)?,
        _ => {
            return Err(Error::runtime(format!(
                "unknown property {name:?} on {:?}",
                node.kind
            )));
        }
    }
    Ok(())
}

fn apply_color(node: &mut Node, name: &str, color: Color) -> Result<()> {
    match name {
        "style.border_color" => node.style.border_color = color,
        "style.hover_overlay" => node.style.hover_overlay = color,
        "style.pressed_overlay" => node.style.pressed_overlay = color,
        "style.focus_ring_color" => node.style.focus_ring_color = color,
        "style.background" => node.style.background = color.into(),
        "style.foreground" => node.style.foreground = color,
        _ => {
            return Err(Error::runtime(format!(
                "transition resolved a color for unsupported property {name:?}"
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

fn image_source(value: Value, name: &str) -> Result<(Option<String>, Option<ImageSource>)> {
    match value {
        Value::Nil => Ok((None, None)),
        Value::String(value) => Ok((Some(value.to_str()?.to_owned()), None)),
        Value::UserData(value) => value
            .borrow::<ImageHandle>()
            .map(|source| (None, Some(source.0.clone())))
            .map_err(|_| Error::runtime(format!("{name} must be a string, image source, or nil"))),
        _ => Err(Error::runtime(format!(
            "{name} must be a string, image source, or nil"
        ))),
    }
}
