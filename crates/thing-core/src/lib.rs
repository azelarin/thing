//! widget values, geometry, and validation.

mod color;
mod geometry;
mod widget;

pub use color::{Color, ParseColorError};
pub use geometry::{Point, Rect, Scale};
pub use widget::{
    CrossAlign, Image, Kind, MainAlign, Node, NodeId, PanelEdge, PanelInputRect, PanelInputRegion,
    PanelMargins, PanelProperties, PopupAlignment, PopupAnchor, PopupConstraints, PopupOffset,
    PopupOptions, Slider, Style, TextAlign, TextOverflow, TreeError,
};
