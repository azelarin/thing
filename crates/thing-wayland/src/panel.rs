use smithay_client_toolkit::shell::wlr_layer::Anchor;
use thing_core::{Node, PanelEdge, PanelInputRect, PanelInputRegion, PanelMargins};

const DEFAULT_THICKNESS: u32 = 36;
const MAX_SURFACE_DIMENSION: f32 = 4096.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub anchors: Anchor,
    pub requested_size: (u32, u32),
    pub margins: PanelMargins,
    pub exclusive_zone: i32,
}

pub(crate) fn geometry(root: &Node, configured_size: Option<(u32, u32)>) -> Geometry {
    let panel = root.panel;
    let (requested_size, requested_thickness) = match panel.edge {
        PanelEdge::Top | PanelEdge::Bottom => {
            let height = dimension(root.style.height, DEFAULT_THICKNESS);
            ((optional_dimension(root.style.width), height), height)
        }
        PanelEdge::Left | PanelEdge::Right => {
            let width = dimension(root.style.width, DEFAULT_THICKNESS);
            ((width, optional_dimension(root.style.height)), width)
        }
    };
    let configured_thickness = configured_size.and_then(|(width, height)| {
        let value = match panel.edge {
            PanelEdge::Top | PanelEdge::Bottom => height,
            PanelEdge::Left | PanelEdge::Right => width,
        };
        (value > 0).then_some(value)
    });
    let thickness = configured_thickness.unwrap_or(requested_thickness);
    let opposite_margin = match panel.edge {
        PanelEdge::Top => panel.margins.bottom,
        PanelEdge::Bottom => panel.margins.top,
        PanelEdge::Left => panel.margins.right,
        PanelEdge::Right => panel.margins.left,
    };
    Geometry {
        anchors: anchors(panel.edge),
        requested_size,
        margins: panel.margins,
        exclusive_zone: panel
            .exclusive_zone
            .unwrap_or_else(|| automatic_exclusive_zone(thickness, opposite_margin)),
    }
}

fn anchors(edge: PanelEdge) -> Anchor {
    match edge {
        PanelEdge::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
        PanelEdge::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
        PanelEdge::Left => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
        PanelEdge::Right => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
    }
}

fn dimension(value: Option<f32>, default: u32) -> u32 {
    value
        .unwrap_or(default as f32)
        .round()
        .clamp(1.0, MAX_SURFACE_DIMENSION) as u32
}

fn optional_dimension(value: Option<f32>) -> u32 {
    value.map_or(0, |value| {
        value.round().clamp(1.0, MAX_SURFACE_DIMENSION) as u32
    })
}

fn automatic_exclusive_zone(thickness: u32, opposite_margin: i32) -> i32 {
    (i64::from(thickness) + i64::from(opposite_margin)).clamp(0, i64::from(i32::MAX)) as i32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedInputRegion {
    Full,
    Region(Option<InputRect>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub(crate) fn input_region(
    region: PanelInputRegion,
    surface_size: (u32, u32),
) -> ResolvedInputRegion {
    match region {
        PanelInputRegion::Full => ResolvedInputRegion::Full,
        PanelInputRegion::Empty => ResolvedInputRegion::Region(None),
        PanelInputRegion::Rectangle(rect) => {
            ResolvedInputRegion::Region(resolve_input_rect(rect, surface_size))
        }
    }
}

fn resolve_input_rect(rect: PanelInputRect, surface_size: (u32, u32)) -> Option<InputRect> {
    let (x, width) = resolve_axis(rect.x, rect.width, surface_size.0)?;
    let (y, height) = resolve_axis(rect.y, rect.height, surface_size.1)?;
    Some(InputRect {
        x,
        y,
        width,
        height,
    })
}

fn resolve_axis(origin: i32, length: Option<u32>, surface_length: u32) -> Option<(i32, i32)> {
    let surface_end = i64::from(surface_length).min(i64::from(i32::MAX));
    let start = i64::from(origin);
    let end = length.map_or(surface_end, |length| start + i64::from(length));
    let start = start.clamp(0, surface_end);
    let end = end.clamp(0, surface_end);
    (end > start).then_some((start as i32, (end - start) as i32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thing_core::{Kind, NodeId, PanelProperties, Style};

    fn panel() -> Node {
        Node::new(NodeId(1), Kind::Panel)
    }

    #[test]
    fn default_geometry_preserves_the_existing_top_bar() {
        let geometry = geometry(&panel(), None);
        assert_eq!(geometry.requested_size, (0, 36));
        assert_eq!(geometry.exclusive_zone, 36);
        assert_eq!(geometry.margins, PanelMargins::default());
        assert_eq!(geometry.anchors, Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    }

    #[test]
    fn top_geometry_uses_actual_thickness_and_opposite_margin_for_auto_exclusivity() {
        let mut root = panel();
        root.style = Style {
            width: Some(600.0),
            height: Some(52.0),
            ..Style::default()
        };
        root.panel.margins = PanelMargins {
            top: 4,
            right: 6,
            bottom: 8,
            left: 2,
        };

        let geometry = geometry(&root, Some((600, 50)));
        assert_eq!(geometry.requested_size, (600, 52));
        assert_eq!(geometry.exclusive_zone, 58);
        assert_eq!(geometry.anchors, Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    }

    #[test]
    fn side_geometry_swaps_fill_and_thickness_axes() {
        let mut root = panel();
        root.style.width = Some(48.0);
        root.panel.edge = PanelEdge::Left;
        root.panel.margins = PanelMargins {
            left: 3,
            right: 7,
            ..PanelMargins::default()
        };

        let geometry = geometry(&root, Some((50, 1080)));
        assert_eq!(geometry.requested_size, (48, 0));
        assert_eq!(geometry.exclusive_zone, 57);
        assert_eq!(
            geometry.anchors,
            Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM
        );
    }

    #[test]
    fn explicit_exclusive_zones_pass_through_and_auto_never_goes_negative() {
        let mut root = panel();
        root.panel.exclusive_zone = Some(-1);
        assert_eq!(geometry(&root, None).exclusive_zone, -1);
        root.panel.exclusive_zone = Some(0);
        assert_eq!(geometry(&root, None).exclusive_zone, 0);

        root.panel = PanelProperties {
            margins: PanelMargins {
                bottom: -100_000,
                ..PanelMargins::default()
            },
            ..PanelProperties::default()
        };
        assert_eq!(geometry(&root, None).exclusive_zone, 0);
    }

    #[test]
    fn input_regions_preserve_full_and_empty_states() {
        assert_eq!(
            input_region(PanelInputRegion::Full, (100, 50)),
            ResolvedInputRegion::Full
        );
        assert_eq!(
            input_region(PanelInputRegion::Empty, (100, 50)),
            ResolvedInputRegion::Region(None)
        );
    }

    #[test]
    fn omitted_input_extents_follow_configured_surface_size() {
        let region = PanelInputRegion::Rectangle(PanelInputRect {
            x: 10,
            height: Some(36),
            ..PanelInputRect::default()
        });
        assert_eq!(
            input_region(region, (100, 52)),
            ResolvedInputRegion::Region(Some(InputRect {
                x: 10,
                y: 0,
                width: 90,
                height: 36,
            }))
        );
        assert_eq!(
            input_region(region, (150, 52)),
            ResolvedInputRegion::Region(Some(InputRect {
                x: 10,
                y: 0,
                width: 140,
                height: 36,
            }))
        );
    }

    #[test]
    fn input_regions_clip_negative_and_outside_origins() {
        let region = PanelInputRegion::Rectangle(PanelInputRect {
            x: -10,
            y: -5,
            ..PanelInputRect::default()
        });
        assert_eq!(
            input_region(region, (100, 50)),
            ResolvedInputRegion::Region(Some(InputRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }))
        );

        let clipped = PanelInputRegion::Rectangle(PanelInputRect {
            x: -10,
            width: Some(20),
            height: Some(10),
            ..PanelInputRect::default()
        });
        assert_eq!(
            input_region(clipped, (100, 50)),
            ResolvedInputRegion::Region(Some(InputRect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            }))
        );

        let outside = PanelInputRegion::Rectangle(PanelInputRect {
            x: 101,
            ..PanelInputRect::default()
        });
        assert_eq!(
            input_region(outside, (100, 50)),
            ResolvedInputRegion::Region(None)
        );
    }
}
