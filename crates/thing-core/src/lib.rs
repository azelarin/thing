//! widget values, geometry, and validation.

mod color;
mod geometry;
mod vector;
mod widget;

pub use color::{Color, ParseColorError};
pub use geometry::{HorizontalRange, Point, Rect, Scale};
pub use widget::{
    CrossAlign, Image, ImageCandidate, ImageFit, ImagePixmap, ImageSource, KeyboardMode, Kind,
    MainAlign, Node, NodeId, PanelEdge, PanelInputRect, PanelInputRegion, PanelMargins,
    PanelProperties, PathFillRule, PathFit, PathStrokeCap, PathStrokeJoin, PopupAlignment,
    PopupAnchor, PopupConstraints, PopupOffset, PopupOptions, Slider, Style, SurfaceAnchors,
    SurfaceLayer, TextAlign, TextOverflow, TreeError, VectorPath, VectorSegment, ViewBox,
};
