//! widget values, geometry, and validation.

mod brush;
mod color;
mod cursor;
mod geometry;
mod shadow;
mod vector;
mod widget;

pub use brush::{Brush, GradientPoint, GradientStop, LinearGradient, RadialGradient};
pub use color::{Color, ParseColorError};
pub use cursor::{Cursor, own_cursor};
pub use geometry::{HorizontalRange, Point, Rect, Scale};
pub use shadow::{MAX_SHADOW_BLUR, MAX_SHADOW_OFFSET, MAX_SHADOW_SPREAD, Shadow};
pub use widget::{
    ClipMode, ClipPath, CrossAlign, Image, ImageCandidate, ImageFit, ImagePixmap, ImageSource,
    KeyboardMode, Kind, MAX_LINK_BYTES, MAX_TEXT_LINKS, MAX_TEXT_SPANS, MAX_WINDOW_APP_ID_BYTES,
    MAX_WINDOW_DIMENSION, MAX_WINDOW_TITLE_BYTES, MainAlign, Node, NodeId, PanelEdge,
    PanelInputRect, PanelInputRegion, PanelMargins, PanelProperties, PathFillRule, PathFit,
    PathStrokeCap, PathStrokeJoin, PopupAlignment, PopupAnchor, PopupConstraints, PopupOffset,
    PopupOptions, ScrollAxis, Slider, Style, SurfaceAnchors, SurfaceLayer, TextAlign, TextOverflow,
    TextSpan, TransformOrigin, TreeError, VectorPath, VectorSegment, ViewBox, WindowProperties,
    WindowSize,
};
