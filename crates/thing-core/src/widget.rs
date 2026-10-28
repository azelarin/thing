use std::{cmp::Ordering, collections::HashSet, error::Error, fmt, sync::Arc};

use crate::{Color, vector::normalize_path};

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
    TextInput,
    Image,
    Path,
    Button,
    PointerArea,
    Slider,
    Range,
}

/// placement of vector content inside its content box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathFit {
    #[default]
    Contain,
    Cover,
    Fill,
}

/// winding rule used to fill a vector path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathFillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// shape drawn at the open ends of a stroked path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathStrokeCap {
    #[default]
    Butt,
    Round,
    Square,
}

/// shape drawn where two stroked segments meet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathStrokeJoin {
    #[default]
    Miter,
    Round,
    Bevel,
}

/// source coordinates mapped into a path widget's content box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl ViewBox {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Result<Self, TreeError> {
        let view_box = Self {
            x,
            y,
            width,
            height,
        };
        view_box.validate()?;
        Ok(view_box)
    }

    fn validate(self) -> Result<(), TreeError> {
        let right = self.x + self.width;
        let bottom = self.y + self.height;
        if ![self.x, self.y, self.width, self.height, right, bottom]
            .into_iter()
            .all(f32::is_finite)
            || self.width <= 0.0
            || self.height <= 0.0
            || [self.x, self.y, right, bottom]
                .into_iter()
                .any(|value| !(-100_000.0..=100_000.0).contains(&value))
        {
            return Err(TreeError(
                "view_box must have positive dimensions and edges between -100,000 and 100,000"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// one normalized svg path command in absolute source coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VectorSegment {
    MoveTo {
        x: f32,
        y: f32,
    },
    LineTo {
        x: f32,
        y: f32,
    },
    CurveTo {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        x: f32,
        y: f32,
    },
    Quadratic {
        x1: f32,
        y1: f32,
        x: f32,
        y: f32,
    },
    Close,
}

/// resolved drawing properties for a `ui.path` widget.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorPath {
    pub data: Option<String>,
    pub segments: Vec<VectorSegment>,
    pub view_box: Option<ViewBox>,
    pub fit: PathFit,
    pub fill: Option<Color>,
    pub fill_rule: PathFillRule,
    pub stroke: Color,
    pub stroke_width: f32,
    pub stroke_cap: PathStrokeCap,
    pub stroke_join: PathStrokeJoin,
}

impl Default for VectorPath {
    fn default() -> Self {
        Self {
            data: None,
            segments: Vec::new(),
            view_box: None,
            fit: PathFit::Contain,
            fill: None,
            fill_rule: PathFillRule::NonZero,
            stroke: Color::TRANSPARENT,
            stroke_width: 1.0,
            stroke_cap: PathStrokeCap::Butt,
            stroke_join: PathStrokeJoin::Miter,
        }
    }
}

impl VectorPath {
    pub fn set_data(&mut self, data: String) -> Result<(), TreeError> {
        const MAX_BYTES: usize = 256 * 1024;

        if data.len() > MAX_BYTES {
            return Err(TreeError("path data exceeds 256 KiB".into()));
        }

        let segments = normalize_path(&data)?;
        self.data = Some(data);
        self.segments = segments;
        Ok(())
    }

    fn validate(&self) -> Result<(), TreeError> {
        if self.data.is_none() || self.segments.is_empty() {
            return Err(TreeError("path requires data".into()));
        }
        self.view_box
            .ok_or_else(|| TreeError("path requires view_box".into()))?
            .validate()?;
        if !self.stroke_width.is_finite() || !(0.0..=100_000.0).contains(&self.stroke_width) {
            return Err(TreeError(
                "path stroke_width must be finite and between 0 and 100,000".into(),
            ));
        }
        Ok(())
    }
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

/// explicit output-edge anchors for a surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfaceAnchors {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

impl SurfaceAnchors {
    /// returns the only edge that can own a positive exclusive zone.
    pub fn exclusive_edge(self) -> Option<PanelEdge> {
        let candidates = [
            (
                self.top && !self.bottom && self.left == self.right,
                PanelEdge::Top,
            ),
            (
                self.bottom && !self.top && self.left == self.right,
                PanelEdge::Bottom,
            ),
            (
                self.left && !self.right && self.top == self.bottom,
                PanelEdge::Left,
            ),
            (
                self.right && !self.left && self.top == self.bottom,
                PanelEdge::Right,
            ),
        ];
        let mut edges = candidates
            .into_iter()
            .filter_map(|(candidate, edge)| candidate.then_some(edge));
        let edge = edges.next()?;
        edges.next().is_none().then_some(edge)
    }
}

/// compositor stacking layer for a surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SurfaceLayer {
    Background,
    Bottom,
    #[default]
    Top,
    Overlay,
}

/// how a surface participates in keyboard focus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyboardMode {
    None,
    #[default]
    OnDemand,
    Exclusive,
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PanelInputRegion {
    #[default]
    Full,
    Rectangles(Vec<PanelInputRect>),
}

/// surface properties that are meaningful only on the root panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelProperties {
    pub edge: PanelEdge,
    /// explicit anchors replace the edge shorthand when present.
    pub anchors: Option<SurfaceAnchors>,
    pub layer: SurfaceLayer,
    pub keyboard_mode: KeyboardMode,
    pub margins: PanelMargins,
    /// `None` lets the surface host choose the anchor mode's default.
    /// `Some(-1)` requests placement without respecting other exclusive zones.
    pub exclusive_zone: Option<i32>,
    pub input_region: PanelInputRegion,
}

impl Default for PanelProperties {
    fn default() -> Self {
        Self {
            edge: PanelEdge::Top,
            anchors: None,
            layer: SurfaceLayer::Top,
            keyboard_mode: KeyboardMode::OnDemand,
            margins: PanelMargins::default(),
            exclusive_zone: None,
            input_region: PanelInputRegion::Full,
        }
    }
}

impl PanelProperties {
    fn validate(&self, style: &Style) -> Result<(), TreeError> {
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
        let rectangles = match &self.input_region {
            PanelInputRegion::Full => &[][..],
            PanelInputRegion::Rectangles(rectangles) => rectangles.as_slice(),
        };
        if rectangles.len() > 256 {
            return Err(TreeError(
                "panel input_region accepts at most 256 rectangles".into(),
            ));
        }
        for rect in rectangles {
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
        if let Some(anchors) = self.anchors {
            if style.width.is_none() && !(anchors.left && anchors.right) {
                return Err(TreeError(
                    "an explicitly anchored panel without a width must anchor left and right"
                        .into(),
                ));
            }
            if style.height.is_none() && !(anchors.top && anchors.bottom) {
                return Err(TreeError(
                    "an explicitly anchored panel without a height must anchor top and bottom"
                        .into(),
                ));
            }
            if self.exclusive_zone.is_some_and(|zone| zone > 0)
                && anchors.exclusive_edge().is_none()
            {
                return Err(TreeError(
                    "a positive panel exclusive_zone needs exactly one exclusive anchor edge"
                        .into(),
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

/// how an image is sized inside its content box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImageFit {
    #[default]
    Contain,
    Cover,
    Fill,
}

/// placement on the cross axis of a flex container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrossAlign {
    Start,
    #[default]
    Center,
    End,
    Stretch,
}

/// distribution on the main axis of a flex container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MainAlign {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
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
                | Self::PointerArea
                | Self::Range
        )
    }

    pub fn is_interactive(self) -> bool {
        matches!(
            self,
            Self::Button | Self::Slider | Self::Range | Self::TextInput
        )
    }

    fn is_range_visual(self) -> bool {
        matches!(
            self,
            Self::Row
                | Self::CenterBox
                | Self::Column
                | Self::Stack
                | Self::Text
                | Self::Image
                | Self::Path
        )
    }
}

/// layout and paint values in logical pixels. colors do not inherit implicitly.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub width_fraction: Option<f32>,
    pub height_fraction: Option<f32>,
    pub min_width: Option<f32>,
    pub max_width: Option<f32>,
    pub min_height: Option<f32>,
    pub max_height: Option<f32>,
    pub padding: f32,
    pub padding_x: Option<f32>,
    pub padding_y: Option<f32>,
    pub gap: f32,
    pub grow: f32,
    pub shrink: Option<f32>,
    pub align: CrossAlign,
    pub align_self: Option<CrossAlign>,
    pub align_content: CrossAlign,
    pub justify: MainAlign,
    pub wrap: bool,
    pub radius: f32,
    pub border_width: f32,
    pub border_color: Color,
    pub hover_overlay: Color,
    pub pressed_overlay: Color,
    pub disabled_opacity: f32,
    pub focus_ring_color: Color,
    pub focus_ring_width: f32,
    pub placeholder_color: Color,
    pub selection_color: Color,
    pub caret_color: Color,
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
            width_fraction: None,
            height_fraction: None,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            padding: 0.0,
            padding_x: None,
            padding_y: None,
            gap: 0.0,
            grow: 0.0,
            shrink: None,
            align: CrossAlign::Center,
            align_self: None,
            align_content: CrossAlign::Start,
            justify: MainAlign::Start,
            wrap: false,
            radius: 0.0,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
            hover_overlay: Color([255, 255, 255, 20]),
            pressed_overlay: Color([255, 255, 255, 40]),
            disabled_opacity: 0.45,
            focus_ring_color: Color([137, 180, 250, 255]),
            focus_ring_width: 2.0,
            placeholder_color: Color([255, 255, 255, 128]),
            selection_color: Color([137, 180, 250, 128]),
            caret_color: Color::WHITE,
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

/// a finite input range with optional quantization.
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
                "range input needs a finite increasing range and a nonnegative step".into(),
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
    pub placeholder: String,
    pub max_lines: Option<u32>,
    pub text_overflow: TextOverflow,
    pub image: Image,
    pub image_fit: ImageFit,
    pub path: VectorPath,
    pub popup: PopupOptions,
    pub slider: Slider,
    pub range_inset: f32,
    pub panel: PanelProperties,
    pub enabled: bool,
    pub visible: bool,
    pub autofocus: bool,
    pub pointer_events: bool,
    pub scroll_events: bool,
    pub key: Option<String>,
    pub children: Vec<Node>,
}

#[derive(Clone, Copy)]
struct ValidationScope {
    allow_external_anchor: bool,
    allow_automatic_popup_anchor: bool,
    inside_range: bool,
}

impl Node {
    pub fn new(id: NodeId, kind: Kind) -> Self {
        Self {
            id,
            kind,
            style: Style::default(),
            text: String::new(),
            placeholder: String::new(),
            max_lines: None,
            text_overflow: TextOverflow::Clip,
            image: Image::default(),
            image_fit: ImageFit::Contain,
            path: VectorPath::default(),
            popup: PopupOptions::default(),
            slider: Slider::default(),
            range_inset: 0.0,
            panel: PanelProperties::default(),
            enabled: true,
            visible: true,
            autofocus: false,
            pointer_events: false,
            scroll_events: false,
            key: None,
            children: Vec::new(),
        }
    }

    /// validates widget properties, sibling keys, and tree depth and size limits.
    pub fn validate(&self) -> Result<(), TreeError> {
        self.validate_inner(
            0,
            &mut HashSet::new(),
            None,
            &mut HashSet::new(),
            ValidationScope {
                allow_external_anchor: false,
                allow_automatic_popup_anchor: self.allows_automatic_popup_anchor(),
                inside_range: false,
            },
        )?;
        self.validate_autofocus_surfaces()
    }

    /// validates a tree rendered as one surface. an automatic popup root may
    /// inherit its button anchor from the parent surface.
    pub fn validate_surface(&self) -> Result<(), TreeError> {
        self.validate_inner(
            0,
            &mut HashSet::new(),
            None,
            &mut HashSet::new(),
            ValidationScope {
                allow_external_anchor: true,
                allow_automatic_popup_anchor: self.allows_automatic_popup_anchor(),
                inside_range: false,
            },
        )?;
        self.validate_autofocus_surfaces()
    }

    fn allows_automatic_popup_anchor(&self) -> bool {
        self.kind != Kind::Panel
            || self
                .panel
                .anchors
                .is_none_or(|anchors| anchors.exclusive_edge().is_some())
    }

    fn validate_inner(
        &self,
        depth: usize,
        ids: &mut HashSet<NodeId>,
        anchor: Option<NodeId>,
        automatic_popups: &mut HashSet<NodeId>,
        scope: ValidationScope,
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
        if scope.inside_range && !self.kind.is_range_visual() {
            return Err(TreeError(format!(
                "range children must be visual widgets, found {:?}",
                self.kind
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
        self.panel.validate(&self.style)?;
        if !self.kind.is_container() && !self.children.is_empty() {
            return Err(TreeError(format!(
                "{:?} cannot contain children",
                self.kind
            )));
        }
        if self.autofocus && !self.kind.is_interactive() {
            return Err(TreeError(
                "autofocus is available only on interactive widgets".into(),
            ));
        }
        if self.pointer_events && !matches!(self.kind, Kind::Button | Kind::PointerArea) {
            return Err(TreeError(
                "pointer events are available only on button and pointer_area widgets".into(),
            ));
        }
        if self.scroll_events && !matches!(self.kind, Kind::Button | Kind::PointerArea) {
            return Err(TreeError(
                "scroll events are available only on button and pointer_area widgets".into(),
            ));
        }
        if self.kind == Kind::PointerArea && !(self.pointer_events || self.scroll_events) {
            return Err(TreeError("pointer_area requires an event handler".into()));
        }
        let style = &self.style;
        if (self.kind == Kind::Popup || (depth == 0 && self.kind == Kind::Panel))
            && [
                style.min_width,
                style.max_width,
                style.min_height,
                style.max_height,
            ]
            .into_iter()
            .any(|value| value.is_some())
        {
            return Err(TreeError(
                "min and max dimensions are not available on panel or popup surfaces".into(),
            ));
        }
        if (self.kind == Kind::Popup || (depth == 0 && self.kind == Kind::Panel))
            && (style.width_fraction.is_some() || style.height_fraction.is_some())
        {
            return Err(TreeError(
                "fractional dimensions are not available on panel or popup surfaces".into(),
            ));
        }
        if [
            self.image.source.is_some(),
            self.image.memory.is_some(),
            self.image.icon.is_some(),
            self.image.app_id.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
            > 1
        {
            return Err(TreeError(
                "image accepts only one of a file source, image handle, icon, or app_id".into(),
            ));
        }
        if self.kind == Kind::CenterBox
            && (self.children.len() != 3 || self.children.iter().any(|n| n.kind == Kind::Popup))
        {
            return Err(TreeError(
                "centerbox needs exactly three non-popup children: start, center, end".into(),
            ));
        }
        if self.kind == Kind::CenterBox && style.justify != MainAlign::Start {
            return Err(TreeError(
                "centerbox owns main-axis placement; justify is not available".into(),
            ));
        }
        if style.wrap
            && !matches!(
                self.kind,
                Kind::Panel
                    | Kind::Popup
                    | Kind::Row
                    | Kind::Column
                    | Kind::Button
                    | Kind::PointerArea
            )
        {
            return Err(TreeError(
                "wrap is available only on panel, popup, row, column, button, and pointer_area containers"
                    .into(),
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
        if self.kind == Kind::TextInput {
            validate_single_line(&self.text, "text input value")?;
            validate_single_line(&self.placeholder, "text input placeholder")?;
        }
        for value in [
            style.width,
            style.height,
            style.min_width,
            style.max_width,
            style.min_height,
            style.max_height,
            Some(style.padding),
            style.padding_x,
            style.padding_y,
            Some(style.gap),
            Some(style.grow),
            style.shrink,
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
        if [style.width_fraction, style.height_fraction]
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(TreeError(
                "width_fraction and height_fraction must be between 0 and 1".into(),
            ));
        }
        if style.width.is_some() && style.width_fraction.is_some() {
            return Err(TreeError(
                "width and width_fraction cannot be used together".into(),
            ));
        }
        if style.height.is_some() && style.height_fraction.is_some() {
            return Err(TreeError(
                "height and height_fraction cannot be used together".into(),
            ));
        }
        if style
            .min_width
            .zip(style.max_width)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(TreeError(
                "min_width cannot be greater than max_width".into(),
            ));
        }
        if style
            .min_height
            .zip(style.max_height)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(TreeError(
                "min_height cannot be greater than max_height".into(),
            ));
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
        if matches!(self.kind, Kind::Slider | Kind::Range) {
            self.slider.validate()?;
        }
        if self.kind == Kind::Path {
            self.path.validate()?;
        }
        if self.kind == Kind::Range {
            if !self.range_inset.is_finite() || !(0.0..=100_000.0).contains(&self.range_inset) {
                return Err(TreeError(
                    "range inset must be finite and between 0 and 100,000".into(),
                ));
            }
        } else if self.range_inset != 0.0 {
            return Err(TreeError(
                "range inset is available only on range widgets".into(),
            ));
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
                None if !(scope.allow_external_anchor && depth == 0) => {
                    return Err(TreeError(
                        "an automatic popup needs a button ancestor".into(),
                    ));
                }
                _ => {}
            }
        }
        if self.kind == Kind::Popup
            && self.popup.anchor == PopupAnchor::Auto
            && !scope.allow_automatic_popup_anchor
        {
            return Err(TreeError(
                "popup anchor 'auto' needs an unambiguous panel anchor edge".into(),
            ));
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
                ValidationScope {
                    inside_range: scope.inside_range || self.kind == Kind::Range,
                    ..scope
                },
            )?;
        }
        Ok(())
    }

    fn validate_autofocus_surfaces(&self) -> Result<(), TreeError> {
        fn validate_surface(root: &Node) -> Result<(), TreeError> {
            fn collect(node: &Node, count: &mut usize) -> Result<(), TreeError> {
                *count += usize::from(node.autofocus);
                for child in &node.children {
                    if child.kind == Kind::Popup {
                        validate_surface(child)?;
                    } else {
                        collect(child, count)?;
                    }
                }
                Ok(())
            }

            let mut count = 0;
            collect(root, &mut count)?;
            if count > 1 {
                return Err(TreeError(
                    "a rendered surface can contain only one autofocus widget".into(),
                ));
            }
            Ok(())
        }

        validate_surface(self)
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

/// one validated rgba raster supplied directly by a desktop service.
#[derive(Clone, PartialEq, Eq)]
pub struct ImagePixmap {
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

impl ImagePixmap {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, String> {
        let expected = usize::try_from(width)
            .ok()
            .zip(usize::try_from(height).ok())
            .and_then(|(width, height)| width.checked_mul(height))
            .and_then(|pixels| pixels.checked_mul(4));
        if width == 0 || height == 0 || expected != Some(rgba.len()) {
            return Err("image pixmap dimensions do not match its rgba data".into());
        }
        Ok(Self {
            width,
            height,
            rgba: rgba.into(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

impl fmt::Debug for ImagePixmap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImagePixmap")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

/// one named icon with optional in-memory raster fallbacks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageCandidate {
    icon: Option<String>,
    pixmaps: Arc<[ImagePixmap]>,
}

impl ImageCandidate {
    pub fn new(icon: Option<String>, pixmaps: Vec<ImagePixmap>) -> Result<Self, String> {
        let icon = icon.filter(|icon| !icon.is_empty());
        if icon.is_none() && pixmaps.is_empty() {
            return Err("image candidate has no icon name or pixmap".into());
        }
        Ok(Self {
            icon,
            pixmaps: pixmaps.into(),
        })
    }

    pub fn icon(&self) -> Option<&str> {
        self.icon.as_deref()
    }

    pub fn pixmaps(&self) -> &[ImagePixmap] {
        &self.pixmaps
    }
}

/// an immutable in-memory image source with an application-scoped identity.
///
/// callers must issue a different identity whenever any candidate changes.
#[derive(Clone)]
pub struct ImageSource {
    id: Arc<str>,
    candidates: Arc<[ImageCandidate]>,
}

impl ImageSource {
    pub fn new(id: String, candidates: Vec<ImageCandidate>) -> Result<Self, String> {
        if id.is_empty() || id.contains('\0') {
            return Err("image source identity must be nonempty and contain no NUL".into());
        }
        if candidates.is_empty() {
            return Err("image source has no candidates".into());
        }
        Ok(Self {
            id: id.into(),
            candidates: candidates.into(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn candidates(&self) -> &[ImageCandidate] {
        &self.candidates
    }

    pub fn retained_bytes(&self) -> usize {
        self.candidates
            .iter()
            .flat_map(|candidate| candidate.pixmaps())
            .map(|pixmap| pixmap.rgba().len())
            .sum()
    }
}

impl fmt::Debug for ImageSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageSource")
            .field("id", &self.id)
            .field("candidates", &self.candidates)
            .finish()
    }
}

impl PartialEq for ImageSource {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for ImageSource {}

impl PartialOrd for ImageSource {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ImageSource {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

/// a local file, desktop icon, or immutable in-memory source to load.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Image {
    pub source: Option<String>,
    pub memory: Option<ImageSource>,
    pub icon: Option<String>,
    pub app_id: Option<String>,
    pub theme: String,
}

impl Default for Image {
    fn default() -> Self {
        Self {
            source: None,
            memory: None,
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

fn validate_single_line(value: &str, name: &str) -> Result<(), TreeError> {
    const MAX_BYTES: usize = 1024 * 1024;
    if value.len() > MAX_BYTES
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        return Err(TreeError(format!(
            "{name} must be at most 1 MiB and contain no control characters or line separators"
        )));
    }
    Ok(())
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
    fn autofocus_is_unique_per_rendered_surface() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut first = Node::new(NodeId(2), Kind::Button);
        first.autofocus = true;
        let mut second = Node::new(NodeId(3), Kind::TextInput);
        second.autofocus = true;
        root.children = vec![first, second];
        assert_eq!(
            root.validate().unwrap_err().to_string(),
            "a rendered surface can contain only one autofocus widget"
        );

        let second = root.children.pop().unwrap();
        let mut popup = Node::new(NodeId(4), Kind::Popup);
        popup.children.push(second);
        root.children.push(popup);
        assert!(root.validate().is_ok());

        let mut text = Node::new(NodeId(5), Kind::Text);
        text.autofocus = true;
        assert_eq!(
            text.validate().unwrap_err().to_string(),
            "autofocus is available only on interactive widgets"
        );
    }

    #[test]
    fn pointer_areas_are_containers_without_keyboard_focus() {
        let mut area = Node::new(NodeId(1), Kind::PointerArea);
        area.pointer_events = true;
        area.children.push(Node::new(NodeId(2), Kind::Text));
        assert!(area.validate().is_ok());
        assert!(!area.kind.is_interactive());

        area.autofocus = true;
        assert_eq!(
            area.validate().unwrap_err().to_string(),
            "autofocus is available only on interactive widgets"
        );
    }

    #[test]
    fn pointer_areas_accept_scroll_without_press_interaction() {
        let mut area = Node::new(NodeId(1), Kind::PointerArea);
        area.scroll_events = true;
        area.children.push(Node::new(NodeId(2), Kind::Text));
        assert!(area.validate().is_ok());
        assert!(!area.pointer_events);
        assert!(!area.kind.is_interactive());

        area.scroll_events = false;
        assert_eq!(
            area.validate().unwrap_err().to_string(),
            "pointer_area requires an event handler"
        );

        let mut row = Node::new(NodeId(3), Kind::Row);
        row.scroll_events = true;
        assert_eq!(
            row.validate().unwrap_err().to_string(),
            "scroll events are available only on button and pointer_area widgets"
        );
    }

    #[test]
    fn prevents_nonfinite_values_from_reaching_layout() {
        let mut node = Node::new(NodeId(1), Kind::Text);
        node.style.width = Some(f32::NAN);
        assert!(node.validate().is_err());
        node.style.width = None;
        node.style.width_fraction = Some(f32::NAN);
        assert!(node.validate().is_err());
        node.style.width_fraction = None;
        node.style.shrink = Some(f32::INFINITY);
        assert!(node.validate().is_err());
        node.style.shrink = None;
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
    fn validates_flex_controls_and_surface_size_boundaries() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut row = Node::new(NodeId(2), Kind::Row);
        row.style.wrap = true;
        row.style.min_width = Some(20.0);
        row.style.max_width = Some(80.0);
        row.style.align_self = Some(CrossAlign::End);
        root.children.push(row);
        assert!(root.validate().is_ok());

        root.children[0].style.min_width = Some(81.0);
        assert!(
            root.validate()
                .unwrap_err()
                .to_string()
                .contains("min_width")
        );
        root.children[0].style.min_width = Some(20.0);
        root.children[0].kind = Kind::Scroll;
        assert!(root.validate().unwrap_err().to_string().contains("wrap"));

        root.children.clear();
        root.style.max_width = Some(800.0);
        assert!(
            root.validate()
                .unwrap_err()
                .to_string()
                .contains("surfaces")
        );

        let mut popup = Node::new(NodeId(3), Kind::Popup);
        popup.style.min_height = Some(40.0);
        assert!(
            popup
                .validate_surface()
                .unwrap_err()
                .to_string()
                .contains("surfaces")
        );

        root.style.max_width = None;
        let mut button = Node::new(NodeId(4), Kind::Button);
        button.children.push(popup);
        root.children.push(button);
        assert!(
            root.validate()
                .unwrap_err()
                .to_string()
                .contains("surfaces")
        );
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
    fn text_inputs_are_single_line_and_bounded() {
        let mut input = Node::new(NodeId(1), Kind::TextInput);
        input.text = "naïve 👨‍👩‍👧".into();
        input.placeholder = "search".into();
        assert!(input.validate().is_ok());

        input.text = "two\nlines".into();
        assert!(input.validate().is_err());
        for invalid in ['\u{000b}', '\u{000c}', '\u{0085}', '\u{2028}', '\u{2029}'] {
            input.text = format!("before{invalid}after");
            assert!(input.validate().is_err());
        }
        input.text = "x".repeat(1024 * 1024 + 1);
        assert!(input.validate().is_err());
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
        root.panel.input_region = PanelInputRegion::Rectangles(vec![PanelInputRect {
            height: Some(36),
            ..PanelInputRect::default()
        }]);
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
    fn validates_explicit_anchor_geometry_and_exclusivity() {
        let mut root = Node::new(NodeId(0), Kind::Panel);
        root.panel.anchors = Some(SurfaceAnchors {
            top: true,
            left: true,
            right: true,
            ..SurfaceAnchors::default()
        });
        root.style.height = Some(36.0);
        root.panel.exclusive_zone = Some(36);
        assert_eq!(
            root.panel.anchors.unwrap().exclusive_edge(),
            Some(PanelEdge::Top)
        );
        assert!(root.validate().is_ok());

        root.style.height = None;
        assert!(root.validate().unwrap_err().to_string().contains("height"));
        root.style.height = Some(36.0);
        root.panel.anchors = Some(SurfaceAnchors {
            top: true,
            right: true,
            ..SurfaceAnchors::default()
        });
        root.style.width = Some(40.0);
        assert!(
            root.validate()
                .unwrap_err()
                .to_string()
                .contains("exclusive anchor edge")
        );
    }

    #[test]
    fn ambiguous_explicit_anchors_require_explicit_popup_anchors() {
        let mut root = Node::new(NodeId(0), Kind::Panel);
        root.panel.anchors = Some(SurfaceAnchors {
            top: true,
            right: true,
            bottom: true,
            left: true,
        });
        let mut popup = Node::new(NodeId(1), Kind::Popup);
        root.children.push(popup.clone());
        assert!(
            root.validate()
                .unwrap_err()
                .to_string()
                .contains("popup anchor 'auto'")
        );

        popup.popup.anchor = PopupAnchor::Right;
        root.children = vec![popup];
        assert!(root.validate().is_ok());
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

    #[test]
    fn normalizes_complete_svg_path_syntax() {
        let mut path = VectorPath::default();
        path.set_data(
            "M 0 0 h 10 v 10 l -10 0 z m 2 2 c 1 0 2 1 3 1 s 2 1 3 0 q 1 1 2 0 t 2 0 a 2 3 15 0 1 4 2"
                .into(),
        )
        .unwrap();

        assert!(path.segments.len() > 10);
        assert!(
            path.segments
                .iter()
                .any(|segment| matches!(segment, VectorSegment::CurveTo { .. }))
        );
        assert!(
            path.segments
                .iter()
                .any(|segment| matches!(segment, VectorSegment::Quadratic { .. }))
        );
        assert!(path.segments.contains(&VectorSegment::Close));
    }

    #[test]
    fn rejects_path_limits_without_replacing_valid_data() {
        let mut path = VectorPath::default();
        path.set_data("M 0 0 L 1 1".into()).unwrap();
        let previous = path.clone();
        assert!(path.set_data("M 0 0 L 100001 0".into()).is_err());
        assert_eq!(path, previous);

        let source = std::iter::once("M 0 0")
            .chain(std::iter::repeat_n("a 1 1 0 1 1 1 0", 4095))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            path.set_data(source)
                .unwrap_err()
                .to_string()
                .contains("normalized segments")
        );

        assert!(path.set_data("M 0 0 A -1 2 0 0 0 4 4".into()).is_err());
    }

    #[test]
    fn coordinate_limits_apply_after_relative_commands_are_resolved() {
        let mut path = VectorPath::default();
        path.set_data("M 100000 0 l -100001 0".into()).unwrap();
        assert_eq!(
            path.segments.last(),
            Some(&VectorSegment::LineTo { x: -1.0, y: 0.0 })
        );

        assert!(path.set_data("M 100000 0 l 1 0".into()).is_err());
        assert!(path.set_data("M 0 0 A 100001 1 0 0 0 1 1".into()).is_err());
    }

    #[test]
    fn subnormal_arc_endpoints_are_handled_without_panicking() {
        let mut path = VectorPath::default();
        path.set_data("M 0 0 A 1 1 0 0 1 5e-324 0".into()).unwrap();
        assert_eq!(
            path.segments,
            vec![VectorSegment::MoveTo { x: 0.0, y: 0.0 }]
        );
    }

    #[test]
    fn path_requires_valid_data_and_view_box() {
        let mut node = Node::new(NodeId(1), Kind::Path);
        assert_eq!(
            node.validate().unwrap_err().to_string(),
            "path requires data"
        );
        node.path.set_data("M 0 0 L 10 10".into()).unwrap();
        assert_eq!(
            node.validate().unwrap_err().to_string(),
            "path requires view_box"
        );
        node.path.view_box = Some(ViewBox::new(-10.0, -5.0, 20.0, 10.0).unwrap());
        assert!(node.validate().is_ok());

        assert!(ViewBox::new(99_999.0, 0.0, 2.0, 1.0).is_err());
        node.path.stroke_width = f32::INFINITY;
        assert!(node.validate().is_err());
    }

    #[test]
    fn ranges_accept_only_visual_descendants() {
        let mut range = Node::new(NodeId(1), Kind::Range);
        range.range_inset = 4.0;
        let mut row = Node::new(NodeId(2), Kind::Row);
        row.children.push(Node::new(NodeId(3), Kind::Text));
        row.children.push(Node::new(NodeId(4), Kind::Image));
        let mut path = Node::new(NodeId(5), Kind::Path);
        path.path.set_data("M 0 0 L 1 1".into()).unwrap();
        path.path.view_box = Some(ViewBox::new(0.0, 0.0, 1.0, 1.0).unwrap());
        row.children.push(path);
        range.children.push(row);
        assert!(range.validate().is_ok());

        for kind in [
            Kind::Panel,
            Kind::Popup,
            Kind::Scroll,
            Kind::Button,
            Kind::Slider,
            Kind::Range,
        ] {
            let mut candidate = range.clone();
            candidate.children[0]
                .children
                .push(Node::new(NodeId(10), kind));
            assert!(
                candidate
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("visual widgets"),
                "{kind:?} should not be accepted below a range"
            );
        }

        range.range_inset = -1.0;
        assert!(range.validate().unwrap_err().to_string().contains("inset"));
    }
}
