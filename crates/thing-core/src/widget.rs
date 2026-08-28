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
    pub font_size: f32,
    pub font_weight: f32,
    pub font_family: String,
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
            font_size: 14.0,
            font_weight: 400.0,
            font_family: "sans-serif".into(),
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
    pub image: Image,
    pub slider: Slider,
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
            image: Image::default(),
            slider: Slider::default(),
            enabled: true,
            visible: true,
            key: None,
            children: Vec::new(),
        }
    }

    /// validates widget properties, sibling keys, and tree depth and size limits.
    pub fn validate(&self) -> Result<(), TreeError> {
        self.validate_inner(0, &mut HashSet::new())
    }

    fn validate_inner(&self, depth: usize, ids: &mut HashSet<NodeId>) -> Result<(), TreeError> {
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
        if !style.opacity.is_finite() || !(0.0..=1.0).contains(&style.opacity) {
            return Err(TreeError("opacity must be between 0 and 1".into()));
        }
        if !style.font_weight.is_finite() || !(1.0..=1000.0).contains(&style.font_weight) {
            return Err(TreeError("font weight must be between 1 and 1000".into()));
        }
        if self.kind == Kind::Slider {
            self.slider.validate()?;
        }
        let mut keys = HashSet::new();
        for child in &self.children {
            if let Some(key) = &child.key
                && !keys.insert(key)
            {
                return Err(TreeError(format!("duplicate sibling key {key:?}")));
            }
            child.validate_inner(depth + 1, ids)?;
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
