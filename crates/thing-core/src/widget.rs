use std::{collections::HashSet, error::Error, fmt};

use crate::Color;

/// an identity scoped to one loaded configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// the supported widget primitives. containers own their children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Panel,
    Popup,
    Row,
    CenterBox,
    Column,
    Stack,
    Scroll,
    Text,
    Image,
    Button,
    Slider,
}

/// the output edge that owns a panel surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelEdge {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

/// distances between a panel surface and the corresponding output edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelMargins {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

/// one rectangular input area in surface-local logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelInputRect {
    pub x: i32,
    pub y: i32,
    /// `None` extends from x to the configured surface's right edge.
    pub width: Option<u32>,
    /// `None` extends from y to the configured surface's bottom edge.
    pub height: Option<u32>,
}

/// which part of a panel surface accepts pointer and touch input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelInputRegion {
    #[default]
    Full,
    Empty,
    Rectangle(PanelInputRect),
}

/// surface properties that are meaningful only on the root panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelProperties {
    pub edge: PanelEdge,
    pub margins: PanelMargins,
    /// None reserves the configured thickness plus the opposite-edge margin.
    /// Layer shell also adds the margin on the panel's own edge.
    /// Some(-1) uses layer-shell's ignore-exclusive-zones behavior.
    pub exclusive_zone: Option<i32>,
    pub input_region: PanelInputRegion,
}

impl PanelProperties {
    fn validate(self) -> Result<(), TreeError> {
        const LIMIT: i32 = 100_000;
        if [
            self.margins.top,
            self.margins.right,
            self.margins.bottom,
            self.margins.left,
        ]
        .iter()
        .any(|value| !(-LIMIT..=LIMIT).contains(value))
        {
            return Err(TreeError(
                "panel margins must be between -100,000 and 100,000".into(),
            ));
        }
        if self
            .exclusive_zone
            .is_some_and(|value| !(-1..=LIMIT).contains(&value))
        {
            return Err(TreeError(
                "panel exclusive_zone must be -1 or between 0 and 100,000".into(),
            ));
        }
        if let PanelInputRegion::Rectangle(rect) = self.input_region {
            if !(-LIMIT..=LIMIT).contains(&rect.x) || !(-LIMIT..=LIMIT).contains(&rect.y) {
                return Err(TreeError(
                    "panel input_region coordinates must be between -100,000 and 100,000".into(),
                ));
            }
            if [rect.width, rect.height]
                .into_iter()
                .flatten()
                .any(|value| value == 0 || value > LIMIT as u32)
            {
                return Err(TreeError(
                    "panel input_region dimensions must be between 1 and 100,000".into(),
                ));
            }
        }
        Ok(())
    }
}

/// horizontal placement within a text widget's width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextAlign {
    #[default]
    Start,
    Center,
    End,
}

/// behavior when a line limit omits part of the text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextOverflow {
    #[default]
    Clip,
    Ellipsis,
}

impl Kind {
    pub fn is_container(self) -> bool {
        matches!(
            self,
            Self::Panel
                | Self::Popup
                | Self::Row
                | Self::CenterBox
                | Self::Column
                | Self::Stack
                | Self::Scroll
                | Self::Button
        )
    }

    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Button | Self::Slider)
    }
}

/// layout and paint values in logical pixels. colors do not inherit implicitly.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub padding: f32,
    pub padding_x: Option<f32>,
    pub padding_y: Option<f32>,
    pub gap: f32,
    pub grow: f32,
    pub radius: f32,
    pub border_width: f32,
    pub border_color: Color,
    pub hover_overlay: Color,
    pub pressed_overlay: Color,
    pub disabled_opacity: f32,
    pub focus_ring_color: Color,
    pub focus_ring_width: f32,
    pub font_size: f32,
    pub font_weight: f32,
    pub font_family: String,
    pub text_align: TextAlign,
    pub tabular_digits: bool,
    pub background: Color,
    pub foreground: Color,
    pub opacity: f32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            padding: 0.0,
            padding_x: None,
            padding_y: None,
            gap: 0.0,
            grow: 0.0,
            radius: 0.0,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
            hover_overlay: Color([255, 255, 255, 20]),
            pressed_overlay: Color([255, 255, 255, 40]),
            disabled_opacity: 0.45,
            focus_ring_color: Color([137, 180, 250, 255]),
            focus_ring_width: 2.0,
            font_size: 14.0,
            font_weight: 400.0,
            font_family: "sans-serif".into(),
            text_align: TextAlign::Start,
            tabular_digits: false,
            background: Color::TRANSPARENT,
            foreground: Color::WHITE,
            opacity: 1.0,
        }
    }
}

/// a finite slider range with optional quantization.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slider {
    pub min: f64,
    pub max: f64,
    pub value: f64,
    pub step: f64,
}

/// activation and timing for a popup managed by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PopupOptions {
    pub open_on_hover: bool,
    pub open_on_click: bool,
    pub open_delay_ms: u32,
    pub close_delay_ms: u32,
    pub anchor: PopupAnchor,
    pub alignment: PopupAlignment,
    pub offset: PopupOffset,
    /// `None` lets the host choose adjustments for the resolved anchor side.
    pub constraints: Option<PopupConstraints>,
}

/// the side of a trigger where a popup is placed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupAnchor {
    #[default]
    Auto,
    Top,
    Bottom,
    Left,
    Right,
}

/// physical alignment along a popup's anchor side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupAlignment {
    Start,
    Center,
    #[default]
    End,
}

/// an offset from the popup's anchored position in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PopupOffset {
    pub x: i32,
    pub y: i32,
}

/// ways a compositor may keep a popup inside its constraint boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PopupConstraints {
    pub flip_x: bool,
    pub flip_y: bool,
    pub slide_x: bool,
    pub slide_y: bool,
}

impl Default for PopupOptions {
    fn default() -> Self {
        Self {
            open_on_hover: false,
            open_on_click: false,
            open_delay_ms: 300,
            close_delay_ms: 250,
            anchor: PopupAnchor::Auto,
            alignment: PopupAlignment::End,
            offset: PopupOffset::default(),
            constraints: None,
        }
    }
}

impl PopupOptions {
    pub fn is_automatic(self) -> bool {
        self.open_on_hover || self.open_on_click
    }

    fn validate(self) -> Result<(), TreeError> {
        const LIMIT: i32 = 100_000;
        if self.open_delay_ms > 60_000 || self.close_delay_ms > 60_000 {
            return Err(TreeError(
                "popup delays must be between 0 and 60,000 milliseconds".into(),
            ));
        }
        if !(-LIMIT..=LIMIT).contains(&self.offset.x) || !(-LIMIT..=LIMIT).contains(&self.offset.y)
        {
            return Err(TreeError(
                "popup offsets must be between -100,000 and 100,000".into(),
            ));
        }
        Ok(())
    }
}

impl Default for Slider {
    fn default() -> Self {
        Self {
            min: 0.0,
            max: 1.0,
            value: 0.0,
            step: 0.01,
        }
    }
}

impl Slider {
    pub fn validate(self) -> Result<(), TreeError> {
        if ![self.min, self.max, self.value, self.step]
            .iter()
            .all(|v| v.is_finite())
            || self.max <= self.min
            || !(self.max - self.min).is_finite()
            || self.step < 0.0
        {
            return Err(TreeError(
                "slider needs a finite increasing range and a nonnegative step".into(),
            ));
        }
        Ok(())
    }

    /// convert pointer position to a clamped, quantized value.
    pub fn value_at(self, fraction: f64) -> f64 {
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let value = self.min + fraction * (self.max - self.min);
        if fraction == 0.0 {
            return self.min;
        }
        if fraction == 1.0 {
            return self.max;
        }
        if self.step > 0.0 {
            (self.min + ((value - self.min) / self.step).round() * self.step)
                .clamp(self.min, self.max)
        } else {
            value.clamp(self.min, self.max)
        }
    }

    pub fn fraction(self) -> f64 {
        ((self.value - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
    }
}

/// a widget and its resolved properties. script functions stay in the runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: NodeId,
    pub kind: Kind,
    pub style: Style,
    pub text: String,
    pub max_lines: Option<u32>,
    pub text_overflow: TextOverflow,
    pub image: Image,
    pub popup: PopupOptions,
    pub slider: Slider,
    pub panel: PanelProperties,
    pub enabled: bool,
    pub visible: bool,
    pub key: Option<String>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn new(id: NodeId, kind: Kind) -> Self {
        Self {
            id,
            kind,
            style: Style::default(),
            text: String::new(),
            max_lines: None,
            text_overflow: TextOverflow::Clip,
            image: Image::default(),
            popup: PopupOptions::default(),
            slider: Slider::default(),
            panel: PanelProperties::default(),
            enabled: true,
            visible: true,
            key: None,
            children: Vec::new(),
        }
    }

    /// validates widget properties, sibling keys, and tree depth and size limits.
    pub fn validate(&self) -> Result<(), TreeError> {
        self.validate_inner(0, &mut HashSet::new(), None, &mut HashSet::new(), false)
    }

    /// validates a tree rendered as one surface. an automatic popup root may
    /// inherit its button anchor from the parent surface.
    pub fn validate_surface(&self) -> Result<(), TreeError> {
        self.validate_inner(0, &mut HashSet::new(), None, &mut HashSet::new(), true)
    }

    fn validate_inner(
        &self,
        depth: usize,
        ids: &mut HashSet<NodeId>,
        anchor: Option<NodeId>,
        automatic_popups: &mut HashSet<NodeId>,
        allow_external_anchor: bool,
    ) -> Result<(), TreeError> {
        if depth > 64 || ids.len() >= 10_000 {
            return Err(TreeError(
                "widget tree exceeds 64 levels or 10,000 nodes".into(),
            ));
        }
        if !ids.insert(self.id) {
            return Err(TreeError(format!(
                "widget {:?} is used more than once; create a fresh widget for each position",
                self.id
            )));
        }
        if depth > 0 && self.panel != PanelProperties::default() {
            return Err(TreeError(
                "panel surface properties are only valid on the root panel".into(),
            ));
        }
        if self.kind != Kind::Panel && self.panel != PanelProperties::default() {
            return Err(TreeError(
                "panel surface properties are only valid on a panel".into(),
            ));
        }
        self.panel.validate()?;
        if !self.kind.is_container() && !self.children.is_empty() {
            return Err(TreeError(format!(
                "{:?} cannot contain children",
                self.kind
            )));
        }
        let style = &self.style;
        if [&self.image.source, &self.image.icon, &self.image.app_id]
            .iter()
            .filter(|v| v.is_some())
            .count()
            > 1
        {
            return Err(TreeError(
                "image accepts only one of source, icon, or app_id".into(),
            ));
        }
        if self.kind == Kind::CenterBox
            && (self.children.len() != 3 || self.children.iter().any(|n| n.kind == Kind::Popup))
        {
            return Err(TreeError(
                "centerbox needs exactly three non-popup children: start, center, end".into(),
            ));
        }
        if self.max_lines == Some(0) {
            return Err(TreeError(
                "max_lines must be a positive whole number".into(),
            ));
        }
        if self.kind != Kind::Text
            && (self.max_lines.is_some() || self.text_overflow != TextOverflow::Clip)
        {
            return Err(TreeError(
                "max_lines and overflow are available only on text widgets".into(),
            ));
        }
        if self.text_overflow == TextOverflow::Ellipsis && self.max_lines != Some(1) {
            return Err(TreeError(
                "ellipsis currently requires max_lines = 1".into(),
            ));
        }
        for value in [
            style.width,
            style.height,
            Some(style.padding),
            style.padding_x,
            style.padding_y,
            Some(style.gap),
            Some(style.grow),
            Some(style.radius),
            Some(style.border_width),
            Some(style.focus_ring_width),
            Some(style.font_size),
        ]
        .into_iter()
        .flatten()
        {
            if !value.is_finite() || !(0.0..=100_000.0).contains(&value) {
                return Err(TreeError(
                    "style dimensions must be finite and between 0 and 100,000".into(),
                ));
            }
        }
        if [style.opacity, style.disabled_opacity]
            .into_iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(TreeError(
                "opacity and disabled_opacity must be between 0 and 1".into(),
            ));
        }
        if !style.font_weight.is_finite() || !(1.0..=1000.0).contains(&style.font_weight) {
            return Err(TreeError("font weight must be between 1 and 1000".into()));
        }
        if self.kind == Kind::Slider {
            self.slider.validate()?;
        }
        let anchor = if self.kind == Kind::Button {
            Some(self.id)
        } else {
            anchor
        };
        if self.kind == Kind::Popup && self.popup.is_automatic() {
            match anchor {
                Some(anchor) if !automatic_popups.insert(anchor) => {
                    return Err(TreeError(
                        "a button can own only one automatic popup".into(),
                    ));
                }
                None if !(allow_external_anchor && depth == 0) => {
                    return Err(TreeError(
                        "an automatic popup needs a button ancestor".into(),
                    ));
                }
                _ => {}
            }
        }
        self.popup.validate()?;
        let mut keys = HashSet::new();
        for child in &self.children {
            if let Some(key) = &child.key
                && !keys.insert(key)
            {
                return Err(TreeError(format!("duplicate sibling key {key:?}")));
            }
            child.validate_inner(
                depth + 1,
                ids,
                anchor,
                automatic_popups,
                allow_external_anchor,
            )?;
        }
        Ok(())
    }

    pub fn find(&self, id: NodeId) -> Option<&Self> {
        if self.id == id {
            Some(self)
        } else {
            self.children.iter().find_map(|child| child.find(id))
        }
    }

    pub fn find_mut(&mut self, id: NodeId) -> Option<&mut Self> {
        if self.id == id {
            Some(self)
        } else {
            self.children
                .iter_mut()
                .find_map(|child| child.find_mut(id))
        }
    }
}

/// a local file or desktop icon to load.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Image {
    pub source: Option<String>,
    pub icon: Option<String>,
    pub app_id: Option<String>,
    pub theme: String,
}

impl Default for Image {
    fn default() -> Self {
        Self {
            source: None,
            icon: None,
            app_id: None,
            theme: "hicolor".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeError(pub String);

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for TreeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_shared_widgets_and_duplicate_keys() {
        let mut root = Node::new(NodeId(0), Kind::Row);
        let child = Node::new(NodeId(1), Kind::Text);
        root.children = vec![child.clone(), child];
        assert!(root.validate().is_err());
        root.children[1].id = NodeId(2);
        root.children[0].key = Some("device".into());
        root.children[1].key = Some("device".into());
        assert!(root.validate().is_err());
    }

    #[test]
    fn prevents_nonfinite_values_from_reaching_layout() {
        let mut node = Node::new(NodeId(1), Kind::Text);
        node.style.width = Some(f32::NAN);
        assert!(node.validate().is_err());
        node.style.width = None;
        node.style.opacity = f32::INFINITY;
        assert!(node.validate().is_err());

        node.style.opacity = 1.0;
        node.style.disabled_opacity = -0.1;
        assert!(node.validate().is_err());

        node.style.disabled_opacity = 1.0;
        node.style.focus_ring_width = f32::NAN;
        assert!(node.validate().is_err());
    }

    #[test]
    fn validates_text_line_limits_and_ellipsis() {
        let mut text = Node::new(NodeId(1), Kind::Text);
        text.max_lines = Some(0);
        assert_eq!(
            text.validate().unwrap_err().to_string(),
            "max_lines must be a positive whole number"
        );

        text.max_lines = Some(2);
        text.text_overflow = TextOverflow::Ellipsis;
        assert_eq!(
            text.validate().unwrap_err().to_string(),
            "ellipsis currently requires max_lines = 1"
        );

        text.max_lines = Some(1);
        assert!(text.validate().is_ok());

        let mut row = Node::new(NodeId(2), Kind::Row);
        row.max_lines = Some(1);
        assert_eq!(
            row.validate().unwrap_err().to_string(),
            "max_lines and overflow are available only on text widgets"
        );
    }

    #[test]
    fn surface_validation_accepts_only_the_root_popup_external_anchor() {
        let mut popup = Node::new(NodeId(1), Kind::Popup);
        popup.popup.open_on_hover = true;
        assert!(popup.validate().is_err());
        assert!(popup.validate_surface().is_ok());

        let mut nested = Node::new(NodeId(2), Kind::Popup);
        nested.popup.open_on_hover = true;
        popup.children.push(nested);
        assert!(popup.validate_surface().is_err());
    }

    #[test]
    fn validates_root_panel_surface_properties() {
        let mut root = Node::new(NodeId(0), Kind::Panel);
        root.panel.input_region = PanelInputRegion::Rectangle(PanelInputRect {
            height: Some(36),
            ..PanelInputRect::default()
        });
        assert!(root.validate().is_ok());

        root.panel.exclusive_zone = Some(-2);
        assert!(
            root.validate()
                .unwrap_err()
                .to_string()
                .contains("exclusive_zone")
        );

        let mut nested = Node::new(NodeId(1), Kind::Panel);
        nested.panel.edge = PanelEdge::Left;
        root.panel = PanelProperties::default();
        root.children = vec![nested];
        assert!(
            root.validate()
                .unwrap_err()
                .to_string()
                .contains("only valid on the root panel")
        );
    }

    #[test]
    fn sliders_quantize_relative_to_their_minimum() {
        let slider = Slider {
            min: 10.0,
            max: 20.0,
            value: 10.0,
            step: 3.0,
        };
        assert_eq!(slider.value_at(0.5), 16.0);
        assert_eq!(slider.value_at(-1.0), 10.0);
        assert_eq!(slider.value_at(2.0), 20.0);
        assert_eq!(slider.value_at(f64::NAN), 10.0);
        assert!(
            Slider {
                max: 0.0,
                ..Slider::default()
            }
            .validate()
            .is_err()
        );
    }
}
