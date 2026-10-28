use thing_core::{
    Node, PanelEdge, PopupAlignment, PopupAnchor, PopupConstraints, PopupOffset, Rect,
};
use wayland_client::Proxy;
use wayland_protocols::xdg::shell::client::xdg_positioner::{
    self, Anchor, ConstraintAdjustment, Gravity, XdgPositioner,
};

const DEFAULT_WIDTH: f32 = 320.0;
const DEFAULT_HEIGHT: f32 = 200.0;
const MAX_SURFACE_DIMENSION: f32 = 4096.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub size: (i32, i32),
    pub parent_size: (i32, i32),
    pub anchor_rect: AnchorRect,
    pub anchor: Anchor,
    pub gravity: Gravity,
    pub offset: PopupOffset,
    pub constraints: PopupConstraints,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnchorRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepositionState {
    current: Position,
    next_token: u32,
}

impl RepositionState {
    pub(crate) fn new(current: Position) -> Self {
        Self {
            current,
            next_token: 1,
        }
    }

    pub(crate) fn current(self) -> Position {
        self.current
    }

    pub(crate) fn request(&mut self, next: Position, supported: bool) -> Option<u32> {
        if !supported || self.current == next {
            return None;
        }
        let token = self.next_token;
        self.next_token = token.checked_add(1).unwrap_or(1);
        self.current = next;
        Some(token)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

pub(crate) fn resolve(
    node: &Node,
    trigger: Rect,
    panel_edge: PanelEdge,
    parent_size: (u32, u32),
) -> Position {
    let side = side(node.popup.anchor, panel_edge);
    let (anchor, gravity) = anchors(side, node.popup.alignment);
    Position {
        size: (
            dimension(node.style.width, DEFAULT_WIDTH),
            dimension(node.style.height, DEFAULT_HEIGHT),
        ),
        parent_size: (
            parent_size.0.max(1).min(i32::MAX as u32) as i32,
            parent_size.1.max(1).min(i32::MAX as u32) as i32,
        ),
        anchor_rect: anchor_rect(trigger, parent_size),
        anchor,
        gravity,
        offset: node.popup.offset,
        constraints: node
            .popup
            .constraints
            .unwrap_or_else(|| default_constraints(side)),
    }
}

pub(crate) fn configure(positioner: &XdgPositioner, position: Position, reactive: bool) {
    positioner.set_size(position.size.0, position.size.1);
    positioner.set_anchor_rect(
        position.anchor_rect.x,
        position.anchor_rect.y,
        position.anchor_rect.width,
        position.anchor_rect.height,
    );
    positioner.set_anchor(position.anchor);
    positioner.set_gravity(position.gravity);
    positioner.set_constraint_adjustment(constraint_adjustment(position.constraints));
    if position.offset != PopupOffset::default() {
        positioner.set_offset(position.offset.x, position.offset.y);
    }
    if positioner.version() >= 3 {
        positioner.set_parent_size(position.parent_size.0, position.parent_size.1);
        if reactive {
            positioner.set_reactive();
        }
    }
}

fn side(anchor: PopupAnchor, panel_edge: PanelEdge) -> Side {
    match anchor {
        PopupAnchor::Auto => match panel_edge {
            PanelEdge::Top => Side::Bottom,
            PanelEdge::Bottom => Side::Top,
            PanelEdge::Left => Side::Right,
            PanelEdge::Right => Side::Left,
        },
        PopupAnchor::Top => Side::Top,
        PopupAnchor::Bottom => Side::Bottom,
        PopupAnchor::Left => Side::Left,
        PopupAnchor::Right => Side::Right,
    }
}

fn anchors(side: Side, alignment: PopupAlignment) -> (Anchor, Gravity) {
    match (side, alignment) {
        (Side::Top, PopupAlignment::Start) => (Anchor::TopLeft, Gravity::TopRight),
        (Side::Top, PopupAlignment::Center) => (Anchor::Top, Gravity::Top),
        (Side::Top, PopupAlignment::End) => (Anchor::TopRight, Gravity::TopLeft),
        (Side::Bottom, PopupAlignment::Start) => (Anchor::BottomLeft, Gravity::BottomRight),
        (Side::Bottom, PopupAlignment::Center) => (Anchor::Bottom, Gravity::Bottom),
        (Side::Bottom, PopupAlignment::End) => (Anchor::BottomRight, Gravity::BottomLeft),
        (Side::Left, PopupAlignment::Start) => (Anchor::TopLeft, Gravity::BottomLeft),
        (Side::Left, PopupAlignment::Center) => (Anchor::Left, Gravity::Left),
        (Side::Left, PopupAlignment::End) => (Anchor::BottomLeft, Gravity::TopLeft),
        (Side::Right, PopupAlignment::Start) => (Anchor::TopRight, Gravity::BottomRight),
        (Side::Right, PopupAlignment::Center) => (Anchor::Right, Gravity::Right),
        (Side::Right, PopupAlignment::End) => (Anchor::BottomRight, Gravity::TopRight),
    }
}

fn default_constraints(side: Side) -> PopupConstraints {
    PopupConstraints {
        flip_x: true,
        flip_y: true,
        slide_x: matches!(side, Side::Top | Side::Bottom),
        slide_y: matches!(side, Side::Left | Side::Right),
    }
}

fn constraint_adjustment(constraints: PopupConstraints) -> ConstraintAdjustment {
    let mut adjustment = ConstraintAdjustment::empty();
    if constraints.flip_x {
        adjustment |= xdg_positioner::ConstraintAdjustment::FlipX;
    }
    if constraints.flip_y {
        adjustment |= xdg_positioner::ConstraintAdjustment::FlipY;
    }
    if constraints.slide_x {
        adjustment |= xdg_positioner::ConstraintAdjustment::SlideX;
    }
    if constraints.slide_y {
        adjustment |= xdg_positioner::ConstraintAdjustment::SlideY;
    }
    adjustment
}

fn dimension(value: Option<f32>, default: f32) -> i32 {
    value
        .unwrap_or(default)
        .round()
        .clamp(1.0, MAX_SURFACE_DIMENSION) as i32
}

fn anchor_rect(rect: Rect, parent_size: (u32, u32)) -> AnchorRect {
    let parent_width = i64::from(parent_size.0.max(1).min(i32::MAX as u32));
    let parent_height = i64::from(parent_size.1.max(1).min(i32::MAX as u32));
    let (x, width) = clipped_axis(rect.x, rect.width, parent_width);
    let (y, height) = clipped_axis(rect.y, rect.height, parent_height);
    AnchorRect {
        x,
        y,
        width,
        height,
    }
}

fn clipped_axis(origin: f32, length: f32, parent_length: i64) -> (i32, i32) {
    let origin = rounded(origin);
    let length = rounded(length).max(1);
    let start = origin.clamp(0, parent_length - 1);
    let end = (origin.saturating_add(length)).clamp(start + 1, parent_length);
    (start as i32, (end - start) as i32)
}

fn rounded(value: f32) -> i64 {
    if value.is_finite() {
        value.round() as i64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thing_core::{Kind, NodeId};

    fn popup() -> Node {
        Node::new(NodeId(1), Kind::Popup)
    }

    #[test]
    fn default_top_panel_preserves_the_existing_positioner() {
        let position = resolve(
            &popup(),
            Rect {
                x: 10.0,
                y: 4.0,
                width: 30.0,
                height: 20.0,
            },
            PanelEdge::Top,
            (1920, 52),
        );

        assert_eq!(position.size, (320, 200));
        assert_eq!(position.anchor, Anchor::BottomRight);
        assert_eq!(position.gravity, Gravity::BottomLeft);
        assert_eq!(position.offset, PopupOffset::default());
        assert_eq!(
            position.constraints,
            PopupConstraints {
                flip_x: true,
                flip_y: true,
                slide_x: true,
                slide_y: false,
            }
        );
    }

    #[test]
    fn automatic_anchor_opens_away_from_every_panel_edge() {
        for (edge, anchor, gravity, slide_x) in [
            (
                PanelEdge::Top,
                Anchor::BottomRight,
                Gravity::BottomLeft,
                true,
            ),
            (PanelEdge::Bottom, Anchor::TopRight, Gravity::TopLeft, true),
            (
                PanelEdge::Left,
                Anchor::BottomRight,
                Gravity::TopRight,
                false,
            ),
            (
                PanelEdge::Right,
                Anchor::BottomLeft,
                Gravity::TopLeft,
                false,
            ),
        ] {
            let position = resolve(&popup(), Rect::default(), edge, (800, 600));
            assert_eq!(position.anchor, anchor);
            assert_eq!(position.gravity, gravity);
            assert_eq!(position.constraints.slide_x, slide_x);
            assert_eq!(position.constraints.slide_y, !slide_x);
        }
    }

    #[test]
    fn physical_alignment_maps_on_horizontal_and_vertical_sides() {
        let mut node = popup();
        for (side, alignment, anchor, gravity) in [
            (
                PopupAnchor::Top,
                PopupAlignment::Start,
                Anchor::TopLeft,
                Gravity::TopRight,
            ),
            (
                PopupAnchor::Top,
                PopupAlignment::Center,
                Anchor::Top,
                Gravity::Top,
            ),
            (
                PopupAnchor::Top,
                PopupAlignment::End,
                Anchor::TopRight,
                Gravity::TopLeft,
            ),
            (
                PopupAnchor::Bottom,
                PopupAlignment::Start,
                Anchor::BottomLeft,
                Gravity::BottomRight,
            ),
            (
                PopupAnchor::Bottom,
                PopupAlignment::Center,
                Anchor::Bottom,
                Gravity::Bottom,
            ),
            (
                PopupAnchor::Bottom,
                PopupAlignment::End,
                Anchor::BottomRight,
                Gravity::BottomLeft,
            ),
            (
                PopupAnchor::Left,
                PopupAlignment::Start,
                Anchor::TopLeft,
                Gravity::BottomLeft,
            ),
            (
                PopupAnchor::Left,
                PopupAlignment::Center,
                Anchor::Left,
                Gravity::Left,
            ),
            (
                PopupAnchor::Left,
                PopupAlignment::End,
                Anchor::BottomLeft,
                Gravity::TopLeft,
            ),
            (
                PopupAnchor::Right,
                PopupAlignment::Start,
                Anchor::TopRight,
                Gravity::BottomRight,
            ),
            (
                PopupAnchor::Right,
                PopupAlignment::Center,
                Anchor::Right,
                Gravity::Right,
            ),
            (
                PopupAnchor::Right,
                PopupAlignment::End,
                Anchor::BottomRight,
                Gravity::TopRight,
            ),
        ] {
            node.popup.anchor = side;
            node.popup.alignment = alignment;
            let position = resolve(&node, Rect::default(), PanelEdge::Top, (800, 600));
            assert_eq!((position.anchor, position.gravity), (anchor, gravity));
        }
    }

    #[test]
    fn explicit_constraints_and_offset_replace_automatic_values() {
        let mut node = popup();
        node.popup.offset = PopupOffset { x: -4, y: 8 };
        node.popup.constraints = Some(PopupConstraints {
            slide_y: true,
            ..PopupConstraints::default()
        });
        let position = resolve(&node, Rect::default(), PanelEdge::Top, (800, 600));

        assert_eq!(position.offset, PopupOffset { x: -4, y: 8 });
        assert_eq!(
            position.constraints,
            PopupConstraints {
                slide_y: true,
                ..PopupConstraints::default()
            }
        );
    }

    #[test]
    fn requested_size_and_trigger_geometry_participate_in_repositioning() {
        let mut node = popup();
        let first = resolve(
            &node,
            Rect {
                x: 10.0,
                y: 4.0,
                width: 20.0,
                height: 16.0,
            },
            PanelEdge::Top,
            (800, 52),
        );
        node.style.width = Some(240.0);
        let resized = resolve(
            &node,
            Rect {
                x: 10.0,
                y: 4.0,
                width: 20.0,
                height: 16.0,
            },
            PanelEdge::Top,
            (800, 52),
        );
        let moved = resolve(
            &node,
            Rect {
                x: 30.0,
                y: 4.0,
                width: 20.0,
                height: 16.0,
            },
            PanelEdge::Top,
            (800, 52),
        );

        assert_ne!(first, resized);
        assert_eq!(resized.size, (240, 200));
        assert_ne!(resized, moved);
        assert_eq!(moved.anchor_rect.x, 30);
    }

    #[test]
    fn trigger_rectangles_are_clipped_inside_the_parent() {
        let node = popup();
        assert_eq!(
            resolve(
                &node,
                Rect {
                    x: -20.0,
                    y: 70.0,
                    width: 5.0,
                    height: 8.0,
                },
                PanelEdge::Top,
                (100, 52),
            )
            .anchor_rect,
            AnchorRect {
                x: 0,
                y: 51,
                width: 1,
                height: 1,
            }
        );
    }

    #[test]
    fn reposition_state_keeps_only_the_latest_request() {
        let initial = resolve(&popup(), Rect::default(), PanelEdge::Top, (800, 600));
        let mut changed = popup();
        changed.popup.offset.x = 4;
        let changed = resolve(&changed, Rect::default(), PanelEdge::Top, (800, 600));
        let mut state = RepositionState::new(initial);

        assert_eq!(state.request(changed, false), None);
        assert_eq!(state.current(), initial);
        assert_eq!(state.request(changed, true), Some(1));
        assert_eq!(state.current(), changed);
        assert_eq!(state.request(changed, true), None);
    }

    #[test]
    fn reposition_tokens_wrap_without_growing_state() {
        let initial = resolve(&popup(), Rect::default(), PanelEdge::Top, (800, 600));
        let mut state = RepositionState {
            current: initial,
            next_token: u32::MAX,
        };
        let mut changed = popup();
        changed.popup.offset.x = 1;
        let changed = resolve(&changed, Rect::default(), PanelEdge::Top, (800, 600));
        assert_eq!(state.request(changed, true), Some(u32::MAX));

        let mut changed_again = popup();
        changed_again.popup.offset.x = 2;
        let changed_again = resolve(&changed_again, Rect::default(), PanelEdge::Top, (800, 600));
        assert_eq!(state.request(changed_again, true), Some(1));
    }
}
