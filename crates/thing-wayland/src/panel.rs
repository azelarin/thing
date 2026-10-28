use smithay_client_toolkit::shell::wlr_layer::Anchor;
use thing_core::{Node, PanelEdge, PanelInputRect, PanelInputRegion, PanelMargins, SurfaceAnchors};

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
    let panel = &root.panel;
    let (anchors, requested_size, automatic_zone) = match panel.anchors {
        Some(anchors) => (
            surface_anchors(anchors),
            (
                optional_dimension(root.style.width),
                optional_dimension(root.style.height),
            ),
            0,
        ),
        None => {
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
            (
                edge_anchors(panel.edge),
                requested_size,
                automatic_exclusive_zone(thickness, opposite_margin),
            )
        }
    };
    Geometry {
        anchors,
        requested_size,
        margins: panel.margins,
        exclusive_zone: panel.exclusive_zone.unwrap_or(automatic_zone),
    }
}

fn edge_anchors(edge: PanelEdge) -> Anchor {
    match edge {
        PanelEdge::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
        PanelEdge::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
        PanelEdge::Left => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
        PanelEdge::Right => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
    }
}

fn surface_anchors(anchors: SurfaceAnchors) -> Anchor {
    let mut result = Anchor::empty();
    if anchors.top {
        result |= Anchor::TOP;
    }
    if anchors.right {
        result |= Anchor::RIGHT;
    }
    if anchors.bottom {
        result |= Anchor::BOTTOM;
    }
    if anchors.left {
        result |= Anchor::LEFT;
    }
    result
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedInputRegion {
    Full,
    Region(Vec<InputRect>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub(crate) fn input_region(
    region: &PanelInputRegion,
    surface_size: (u32, u32),
) -> ResolvedInputRegion {
    match region {
        PanelInputRegion::Full => ResolvedInputRegion::Full,
        PanelInputRegion::Rectangles(rectangles) => ResolvedInputRegion::Region(
            rectangles
                .iter()
                .filter_map(|rect| resolve_input_rect(*rect, surface_size))
                .collect(),
        ),
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
    use thing_core::{Kind, NodeId, PanelProperties, Style, SurfaceAnchors};

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
    fn painted_reserved_and_interactive_heights_are_independent() {
        let mut root = panel();
        root.style.height = Some(52.0);
        root.panel.exclusive_zone = Some(36);
        root.panel.input_region = PanelInputRegion::Rectangles(vec![PanelInputRect {
            height: Some(36),
            ..PanelInputRect::default()
        }]);

        let geometry = geometry(&root, Some((1920, 52)));
        assert_eq!(geometry.requested_size, (0, 52));
        assert_eq!(geometry.exclusive_zone, 36);
        assert_eq!(
            input_region(&root.panel.input_region, (1920, 52)),
            ResolvedInputRegion::Region(vec![InputRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 36,
            }])
        );
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
    fn explicit_anchors_support_fixed_and_fill_surfaces_without_reserving_space() {
        let mut toast = panel();
        toast.style.width = Some(360.0);
        toast.style.height = Some(80.0);
        toast.panel.anchors = Some(SurfaceAnchors {
            bottom: true,
            ..SurfaceAnchors::default()
        });
        let toast_geometry = geometry(&toast, None);
        assert_eq!(toast_geometry.anchors, Anchor::BOTTOM);
        assert_eq!(toast_geometry.requested_size, (360, 80));
        assert_eq!(toast_geometry.exclusive_zone, 0);

        let mut overlay = panel();
        overlay.panel.anchors = Some(SurfaceAnchors {
            top: true,
            right: true,
            bottom: true,
            left: true,
        });
        let overlay_geometry = geometry(&overlay, None);
        assert_eq!(overlay_geometry.anchors, Anchor::all());
        assert_eq!(overlay_geometry.requested_size, (0, 0));
        assert_eq!(overlay_geometry.exclusive_zone, 0);
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

        root.panel.margins.bottom = i32::MAX;
        assert_eq!(geometry(&root, None).exclusive_zone, i32::MAX);
    }

    #[test]
    fn input_regions_preserve_full_and_empty_states() {
        assert_eq!(
            input_region(&PanelInputRegion::Full, (100, 50)),
            ResolvedInputRegion::Full
        );
        assert_eq!(
            input_region(&PanelInputRegion::Rectangles(Vec::new()), (100, 50)),
            ResolvedInputRegion::Region(Vec::new())
        );
    }

    #[test]
    fn omitted_input_extents_follow_configured_surface_size() {
        let region = PanelInputRegion::Rectangles(vec![PanelInputRect {
            x: 10,
            height: Some(36),
            ..PanelInputRect::default()
        }]);
        assert_eq!(
            input_region(&region, (100, 52)),
            ResolvedInputRegion::Region(vec![InputRect {
                x: 10,
                y: 0,
                width: 90,
                height: 36,
            }])
        );
        assert_eq!(
            input_region(&region, (150, 52)),
            ResolvedInputRegion::Region(vec![InputRect {
                x: 10,
                y: 0,
                width: 140,
                height: 36,
            }])
        );
    }

    #[test]
    fn input_regions_clip_negative_and_outside_origins() {
        let region = PanelInputRegion::Rectangles(vec![PanelInputRect {
            x: -10,
            y: -5,
            ..PanelInputRect::default()
        }]);
        assert_eq!(
            input_region(&region, (100, 50)),
            ResolvedInputRegion::Region(vec![InputRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }])
        );

        let clipped = PanelInputRegion::Rectangles(vec![PanelInputRect {
            x: -10,
            width: Some(20),
            height: Some(10),
            ..PanelInputRect::default()
        }]);
        assert_eq!(
            input_region(&clipped, (100, 50)),
            ResolvedInputRegion::Region(vec![InputRect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            }])
        );

        let outside = PanelInputRegion::Rectangles(vec![PanelInputRect {
            x: 101,
            ..PanelInputRect::default()
        }]);
        assert_eq!(
            input_region(&outside, (100, 50)),
            ResolvedInputRegion::Region(Vec::new())
        );
    }

    #[test]
    fn input_rectangles_are_clipped_and_unioned_in_order() {
        let region = PanelInputRegion::Rectangles(vec![
            PanelInputRect {
                width: Some(20),
                height: Some(10),
                ..PanelInputRect::default()
            },
            PanelInputRect {
                x: 90,
                y: 40,
                width: Some(20),
                height: Some(20),
            },
        ]);
        assert_eq!(
            input_region(&region, (100, 50)),
            ResolvedInputRegion::Region(vec![
                InputRect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 10,
                },
                InputRect {
                    x: 90,
                    y: 40,
                    width: 10,
                    height: 10,
                },
            ])
        );
    }
}
