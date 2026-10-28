use std::collections::{BTreeMap, BTreeSet};

use taffy::{
    geometry::Point as TaffyPoint,
    prelude as tf,
    style::{Overflow, Position},
};
use thing_core::{Kind, Node, NodeId, Point, Rect};

/// a size in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

/// text measurement must use the same shaping and fonts as the renderer.
pub trait Measurer {
    fn measure(&mut self, node: &Node, max_width: Option<f32>) -> Size;
}

/// one visible widget in painting order, with its inherited clipping rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub id: NodeId,
    pub bounds: Rect,
    pub clip: Rect,
    pub interactive: bool,
    pub enabled: bool,
    pub opacity: f32,
    pub(crate) disabled_opacity: f32,
}

/// scroll offsets for one surface, measured in logical pixels.
#[derive(Clone, Debug, Default)]
pub struct ScrollState {
    offsets: BTreeMap<NodeId, f32>,
}

impl ScrollState {
    fn offset(&self, id: NodeId) -> f32 {
        self.offsets.get(&id).copied().unwrap_or(0.0)
    }

    fn set(&mut self, id: NodeId, offset: f32) {
        if offset > 0.0 {
            self.offsets.insert(id, offset);
        } else {
            self.offsets.remove(&id);
        }
    }

    fn retain_tree(&mut self, root: &Node) {
        fn collect(node: &Node, ids: &mut BTreeSet<NodeId>) {
            if node.kind == Kind::Scroll {
                ids.insert(node.id);
            }
            for child in &node.children {
                collect(child, ids);
            }
        }

        let mut ids = BTreeSet::new();
        collect(root, &mut ids);
        self.offsets.retain(|id, _| ids.contains(id));
    }
}

#[derive(Clone, Copy, Debug)]
struct ScrollArea {
    id: NodeId,
    parent: Option<NodeId>,
    max_offset: f32,
}

#[derive(Clone, Copy, Debug)]
struct PointerRegion {
    bounds: Rect,
    clip: Rect,
    interactive: Option<NodeId>,
    scroll: Option<NodeId>,
}

#[derive(Clone, Copy)]
struct Inherited {
    origin: Point,
    clip: Rect,
    enabled: bool,
    opacity: f32,
    disabled_opacity: f32,
    interactive_parent: Option<NodeId>,
    scroll_parent: Option<NodeId>,
}

/// resolved geometry for one surface. popup children have separate surfaces and
/// do not participate in their parent's flex layout.
#[derive(Debug, Default)]
pub struct Layout {
    placements: Vec<Placement>,
    pointer_regions: Vec<PointerRegion>,
    scroll_areas: Vec<ScrollArea>,
}

impl Layout {
    pub fn compute(root: &Node, size: Size, measurer: &mut impl Measurer) -> Result<Self, String> {
        Self::compute_with_scroll(root, size, &mut ScrollState::default(), measurer)
    }

    pub fn compute_with_scroll(
        root: &Node,
        size: Size,
        state: &mut ScrollState,
        measurer: &mut impl Measurer,
    ) -> Result<Self, String> {
        root.validate_surface().map_err(|error| error.to_string())?;
        if ![size.width, size.height]
            .iter()
            .all(|v| v.is_finite() && (0.0..=100_000.0).contains(v))
        {
            return Err("surface dimensions must be finite and between 0 and 100,000".into());
        }
        let mut tree = tf::TaffyTree::<NodeId>::new();
        tree.disable_rounding();
        let mut ids = BTreeMap::new();
        let root_id = build(&mut tree, root, &mut ids, false).map_err(|e| e.to_string())?;
        let mut root_style = tree.style(root_id).map_err(|e| e.to_string())?.clone();
        root_style.size = tf::Size {
            width: tf::Dimension::length(size.width),
            height: tf::Dimension::length(size.height),
        };
        tree.set_style(root_id, root_style)
            .map_err(|e| e.to_string())?;
        tree.compute_layout_with_measure(
            root_id,
            tf::Size {
                width: tf::AvailableSpace::Definite(size.width),
                height: tf::AvailableSpace::Definite(size.height),
            },
            |inputs, _, context, style| {
                taffy::compute_leaf_layout(
                    inputs,
                    style,
                    |_, _| 0.0,
                    |known, available| {
                        let Some(node) = context.and_then(|id| root.find(*id)) else {
                            return tf::Size::ZERO;
                        };
                        let max_width = known.width.or(match available.width {
                            tf::AvailableSpace::Definite(value) => Some(value),
                            _ => None,
                        });
                        let intrinsic = match node.kind {
                            Kind::Slider => Size {
                                width: 160.0,
                                height: 24.0,
                            },
                            Kind::Text => measurer.measure(node, max_width),
                            Kind::Image => Size {
                                width: 32.0,
                                height: 32.0,
                            },
                            _ => Size::default(),
                        };
                        tf::Size {
                            width: known.width.unwrap_or(intrinsic.width),
                            height: known.height.unwrap_or(intrinsic.height),
                        }
                    },
                )
            },
        )
        .map_err(|e| e.to_string())?;
        let clip = Rect {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
        };
        let mut result = Self::default();
        let mut next_state = state.clone();
        next_state.retain_tree(root);
        result.collect(
            root,
            &tree,
            &ids,
            &mut next_state,
            Inherited {
                origin: Point { x: 0.0, y: 0.0 },
                clip,
                enabled: true,
                opacity: 1.0,
                disabled_opacity: 1.0,
                interactive_parent: None,
                scroll_parent: None,
            },
        )?;
        *state = next_state;
        Ok(result)
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub fn get(&self, id: NodeId) -> Option<&Placement> {
        self.placements.iter().find(|placement| placement.id == id)
    }

    /// hit test from front to back. text inside a button does not hide its parent.
    pub fn hit_test(&self, point: Point) -> Option<NodeId> {
        self.placements
            .iter()
            .rev()
            .find(|p| p.interactive && p.bounds.contains(point) && p.clip.contains(point))
            .map(|p| p.id)
    }

    pub fn focus_order(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.placements
            .iter()
            .filter(|p| p.interactive && p.bounds.intersection(p.clip).is_some())
            .map(|p| p.id)
    }

    /// the nearest interactive widget on the topmost painted branch at `point`.
    pub fn scroll_event_target(&self, point: Point) -> Option<NodeId> {
        self.pointer_region(point)
            .and_then(|region| region.interactive)
    }

    /// scroll the innermost vertical viewport under `point`. when that viewport
    /// is already at its limit, the event bubbles through its scroll ancestors.
    pub fn scroll_y(&self, point: Point, delta: f32, state: &mut ScrollState) -> bool {
        if !delta.is_finite() || delta == 0.0 {
            return false;
        }
        let Some(id) = self.pointer_region(point).and_then(|region| region.scroll) else {
            return false;
        };
        let Some(mut area) = self.scroll_areas.iter().find(|area| area.id == id) else {
            return false;
        };
        loop {
            let current = state.offset(area.id).clamp(0.0, area.max_offset);
            let can_move = if delta > 0.0 {
                current < area.max_offset
            } else {
                current > 0.0
            };
            if can_move {
                let next = (current + delta).clamp(0.0, area.max_offset);
                state.set(area.id, next);
                return next != current;
            }
            let Some(parent) = area.parent else {
                return false;
            };
            let Some(parent_area) = self
                .scroll_areas
                .iter()
                .find(|candidate| candidate.id == parent)
            else {
                return false;
            };
            area = parent_area;
        }
    }

    fn pointer_region(&self, point: Point) -> Option<&PointerRegion> {
        self.pointer_regions
            .iter()
            .rev()
            .find(|region| region.bounds.contains(point) && region.clip.contains(point))
    }

    fn collect(
        &mut self,
        node: &Node,
        tree: &tf::TaffyTree<NodeId>,
        ids: &BTreeMap<NodeId, tf::NodeId>,
        state: &mut ScrollState,
        inherited: Inherited,
    ) -> Result<(), String> {
        if !node.visible {
            return Ok(());
        }
        let Inherited {
            origin,
            clip,
            enabled,
            opacity,
            disabled_opacity,
            interactive_parent,
            scroll_parent,
        } = inherited;
        let geometry = tree.layout(ids[&node.id]).map_err(|e| e.to_string())?;
        let bounds = Rect {
            x: origin.x + geometry.location.x,
            y: origin.y + geometry.location.y,
            width: geometry.size.width,
            height: geometry.size.height,
        };
        // a disabled branch inherits the factor chosen by the widget that
        // disables it instead of compounding a factor at every descendant.
        let disabled_opacity = if enabled && !node.enabled {
            node.style.disabled_opacity
        } else {
            disabled_opacity
        };
        let enabled = enabled && node.enabled;
        let opacity = opacity * node.style.opacity;
        let pointer_interactive = if enabled && node.kind.is_interactive() {
            Some(node.id)
        } else {
            interactive_parent
        };
        let pointer_scroll = if node.kind == Kind::Scroll && enabled {
            Some(node.id)
        } else {
            scroll_parent
        };
        self.placements.push(Placement {
            id: node.id,
            bounds,
            clip,
            interactive: enabled && node.kind.is_interactive(),
            enabled,
            opacity,
            disabled_opacity,
        });
        self.pointer_regions.push(PointerRegion {
            bounds,
            clip,
            interactive: pointer_interactive,
            scroll: pointer_scroll,
        });
        let Some(child_clip) = bounds.intersection(clip) else {
            return Ok(());
        };
        let mut child_origin = Point {
            x: bounds.x,
            y: bounds.y,
        };
        let mut child_scroll_parent = scroll_parent;
        if node.kind == Kind::Scroll {
            let max_offset = if geometry.scroll_height().is_finite() {
                geometry.scroll_height().max(0.0)
            } else {
                0.0
            };
            let offset = state.offset(node.id).clamp(0.0, max_offset);
            state.set(node.id, offset);
            if enabled {
                self.scroll_areas.push(ScrollArea {
                    id: node.id,
                    parent: scroll_parent,
                    max_offset,
                });
                child_scroll_parent = Some(node.id);
            }
            child_origin.y -= offset;
        }
        for child in &node.children {
            if child.kind != Kind::Popup {
                self.collect(
                    child,
                    tree,
                    ids,
                    state,
                    Inherited {
                        origin: child_origin,
                        clip: child_clip,
                        enabled,
                        opacity,
                        disabled_opacity,
                        interactive_parent: pointer_interactive,
                        scroll_parent: child_scroll_parent,
                    },
                )?;
            }
        }
        Ok(())
    }
}

fn build(
    tree: &mut tf::TaffyTree<NodeId>,
    node: &Node,
    ids: &mut BTreeMap<NodeId, tf::NodeId>,
    absolute: bool,
) -> Result<tf::NodeId, taffy::TaffyError> {
    let mut children = Vec::new();
    for (index, child) in node.children.iter().enumerate() {
        if child.kind != Kind::Popup {
            let id = build(
                tree,
                child,
                ids,
                matches!(node.kind, Kind::Stack | Kind::CenterBox),
            )?;
            if node.kind == Kind::CenterBox {
                let mut style = tree.style(id)?.clone();
                match index {
                    0 => {
                        style.inset.left = tf::LengthPercentageAuto::length(
                            node.style.padding_x.unwrap_or(node.style.padding),
                        )
                    }
                    2 => {
                        style.inset.right = tf::LengthPercentageAuto::length(
                            node.style.padding_x.unwrap_or(node.style.padding),
                        )
                    }
                    _ => {}
                }
                tree.set_style(id, style)?;
            }
            children.push(id);
        }
    }
    let style = &node.style;
    let taffy_style = tf::Style {
        display: if node.visible {
            tf::Display::Flex
        } else {
            tf::Display::None
        },
        position: if absolute {
            Position::Absolute
        } else {
            Position::Relative
        },
        size: tf::Size {
            width: style
                .width
                .map_or(tf::Dimension::auto(), tf::Dimension::length),
            height: style
                .height
                .map_or(tf::Dimension::auto(), tf::Dimension::length),
        },
        flex_direction: if matches!(node.kind, Kind::Column | Kind::Scroll) {
            tf::FlexDirection::Column
        } else {
            tf::FlexDirection::Row
        },
        flex_grow: style.grow,
        flex_shrink: if node.kind == Kind::Scroll { 1.0 } else { 0.0 },
        overflow: TaffyPoint {
            x: Overflow::Clip,
            y: if node.kind == Kind::Scroll {
                Overflow::Scroll
            } else {
                Overflow::Clip
            },
        },
        align_items: tf::AlignItems::CENTER,
        justify_content: if node.kind == Kind::CenterBox {
            tf::JustifyContent::CENTER
        } else {
            tf::JustifyContent::FLEX_START
        },
        gap: tf::Size {
            width: tf::LengthPercentage::length(style.gap),
            height: tf::LengthPercentage::length(style.gap),
        },
        padding: tf::Rect {
            left: tf::LengthPercentage::length(style.padding_x.unwrap_or(style.padding)),
            right: tf::LengthPercentage::length(style.padding_x.unwrap_or(style.padding)),
            top: tf::LengthPercentage::length(style.padding_y.unwrap_or(style.padding)),
            bottom: tf::LengthPercentage::length(style.padding_y.unwrap_or(style.padding)),
        },
        ..tf::Style::default()
    };
    let id = if children.is_empty() {
        tree.new_leaf_with_context(taffy_style, node.id)?
    } else {
        tree.new_with_children(taffy_style, &children)?
    };
    ids.insert(node.id, id);
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thing_core::Style;

    struct FixedText;
    impl Measurer for FixedText {
        fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
            Size {
                width: 30.0,
                height: 10.0,
            }
        }
    }

    fn fixed(id: u64, kind: Kind, width: f32, height: f32) -> Node {
        let mut node = Node::new(NodeId(id), kind);
        node.style.width = Some(width);
        node.style.height = Some(height);
        node
    }

    fn scrolled(root: &Node, size: Size, state: &mut ScrollState) -> Layout {
        Layout::compute_with_scroll(root, size, state, &mut FixedText).unwrap()
    }

    #[test]
    fn centerbox_keeps_the_middle_centered_with_unequal_side_groups() {
        let mut root = Node::new(NodeId(1), Kind::CenterBox);
        root.style.padding_x = Some(10.0);
        for (id, width) in [(2, 120.0), (3, 80.0), (4, 200.0)] {
            let mut child = Node::new(NodeId(id), Kind::Button);
            child.style.width = Some(width);
            child.style.height = Some(28.0);
            root.children.push(child);
        }
        for width in [800.0, 1920.0] {
            let layout = Layout::compute(
                &root,
                Size {
                    width,
                    height: 36.0,
                },
                &mut FixedText,
            )
            .unwrap();
            assert_eq!(layout.get(NodeId(2)).unwrap().bounds.x, 10.0);
            assert_eq!(
                layout.get(NodeId(3)).unwrap().bounds.x,
                (width - 80.0) / 2.0
            );
            assert_eq!(layout.get(NodeId(4)).unwrap().bounds.x, width - 210.0);
            for id in [2, 3, 4] {
                assert_eq!(layout.get(NodeId(id)).unwrap().bounds.y, 4.0);
            }
        }
        root.children[0].visible = false;
        let layout = Layout::compute(
            &root,
            Size {
                width: 800.0,
                height: 36.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.x, 360.0);
    }

    #[test]
    fn directional_padding_overrides_the_uniform_value() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        let mut pill = Node::new(NodeId(2), Kind::Button);
        pill.style.padding = 3.0;
        pill.style.padding_x = Some(12.0);
        pill.children.push(Node::new(NodeId(3), Kind::Text));
        root.children.push(pill);
        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 36.0,
            },
            &mut FixedText,
        )
        .unwrap();
        let bounds = layout.get(NodeId(2)).unwrap().bounds;
        assert_eq!(bounds.width, 54.0);
        assert_eq!(bounds.height, 16.0);
    }

    #[test]
    fn buttons_use_text_measurement_padding_and_gap() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        root.style.gap = 5.0;
        for id in [2, 4] {
            let mut button = Node::new(NodeId(id), Kind::Button);
            button.style.padding = 2.0;
            button.children.push(Node::new(NodeId(id + 1), Kind::Text));
            root.children.push(button);
        }
        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 20.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.width, 34.0);
        assert_eq!(layout.get(NodeId(4)).unwrap().bounds.x, 39.0);
        assert_eq!(layout.hit_test(Point { x: 10.0, y: 10.0 }), Some(NodeId(2)));
        assert_eq!(layout.hit_test(Point { x: 36.0, y: 10.0 }), None);
    }

    #[test]
    fn resolved_style_values_drive_opacity_and_hit_geometry_together() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        root.style.opacity = 0.8;
        let mut button = fixed(2, Kind::Button, 40.0, 20.0);
        button.style.opacity = 0.5;
        root.children.push(button);
        let size = Size {
            width: 100.0,
            height: 20.0,
        };

        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        assert_eq!(layout.get(NodeId(2)).unwrap().opacity, 0.4);
        assert_eq!(layout.hit_test(Point { x: 39.0, y: 10.0 }), Some(NodeId(2)));
        assert_eq!(layout.hit_test(Point { x: 41.0, y: 10.0 }), None);

        root.children[0].style.width = Some(80.0);
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        assert_eq!(layout.hit_test(Point { x: 60.0, y: 10.0 }), Some(NodeId(2)));
    }

    #[test]
    fn clips_overflow_and_excludes_hidden_and_disabled_controls() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        root.style.gap = 0.0;
        for id in 2..5 {
            let mut button = Node::new(NodeId(id), Kind::Button);
            button.style.width = Some(30.0);
            button.style.height = Some(20.0);
            root.children.push(button);
        }
        root.children[1].visible = false;
        root.children[2].enabled = false;
        let layout = Layout::compute(
            &root,
            Size {
                width: 20.0,
                height: 20.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(layout.hit_test(Point { x: 25.0, y: 10.0 }), None);
        assert_eq!(layout.focus_order().collect::<Vec<_>>(), vec![NodeId(2)]);
    }

    #[test]
    fn disabled_opacity_fades_an_inherited_branch_once() {
        let mut root = fixed(1, Kind::Column, 100.0, 100.0);
        root.style.opacity = 0.8;
        let mut branch = fixed(2, Kind::Column, 100.0, 50.0);
        branch.enabled = false;
        branch.style.opacity = 0.5;
        branch.style.disabled_opacity = 0.25;
        let mut child = fixed(3, Kind::Button, 100.0, 20.0);
        child.enabled = false;
        child.style.opacity = 0.5;
        child.style.disabled_opacity = 0.1;
        branch.children.push(child);
        root.children.push(branch);

        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 100.0,
            },
            &mut FixedText,
        )
        .unwrap();

        let opacity = |id| {
            let placement = layout.get(NodeId(id)).unwrap();
            placement.opacity * placement.disabled_opacity
        };
        assert!((opacity(1) - 0.8).abs() < f32::EPSILON);
        assert!((opacity(2) - 0.1).abs() < f32::EPSILON);
        assert!((opacity(3) - 0.05).abs() < f32::EPSILON);

        root.style.opacity = 1.0;
        root.children[0].style.opacity = 1.0;
        root.children[0].style.disabled_opacity = Style::default().disabled_opacity;
        root.children[0].children[0].style.opacity = 1.0;
        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 100.0,
            },
            &mut FixedText,
        )
        .unwrap();
        let opacity = |id| {
            let placement = layout.get(NodeId(id)).unwrap();
            placement.opacity * placement.disabled_opacity
        };
        assert_eq!(opacity(2), 0.45);
        assert_eq!(opacity(3), 0.45);
    }

    #[test]
    fn lays_out_an_automatic_popup_surface_with_an_external_anchor() {
        let mut popup = Node::new(NodeId(1), Kind::Popup);
        popup.popup.open_on_hover = true;
        popup.style.width = Some(200.0);
        popup.style.height = Some(80.0);

        let layout = Layout::compute(
            &popup,
            Size {
                width: 200.0,
                height: 80.0,
            },
            &mut FixedText,
        )
        .unwrap();

        assert_eq!(layout.get(popup.id).unwrap().bounds.width, 200.0);
    }

    #[test]
    fn scroll_offsets_share_clipping_between_paint_and_hit_testing() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 40.0);
        for id in 3..=5 {
            scroll.children.push(fixed(id, Kind::Button, 100.0, 30.0));
        }
        root.children.push(scroll);
        let size = Size {
            width: 100.0,
            height: 40.0,
        };
        let point = Point { x: 10.0, y: 10.0 };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);

        assert!(layout.scroll_y(point, 25.0, &mut state));
        assert_eq!(state.offset(NodeId(2)), 25.0);
        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.y, -25.0);
        assert_eq!(layout.get(NodeId(4)).unwrap().bounds.y, 5.0);
        assert_eq!(layout.get(NodeId(3)).unwrap().clip.height, 40.0);
        assert_eq!(layout.hit_test(Point { x: 10.0, y: 4.0 }), Some(NodeId(3)));
        assert_eq!(layout.hit_test(Point { x: 10.0, y: 5.0 }), Some(NodeId(4)));
        assert_eq!(layout.hit_test(Point { x: 10.0, y: 45.0 }), None);

        assert!(layout.scroll_y(point, 1_000.0, &mut state));
        assert_eq!(state.offset(NodeId(2)), 50.0);
        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.get(NodeId(5)).unwrap().bounds.y, 10.0);
        assert_eq!(layout.hit_test(Point { x: 10.0, y: 39.0 }), Some(NodeId(5)));
        assert!(!layout.scroll_y(point, 1.0, &mut state));
    }

    #[test]
    fn scrolling_retargets_a_stationary_pointer_after_layout() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 40.0);
        scroll.children.push(fixed(3, Kind::Button, 100.0, 40.0));
        scroll.children.push(fixed(4, Kind::Button, 100.0, 40.0));
        root.children.push(scroll);
        let size = Size {
            width: 100.0,
            height: 40.0,
        };
        let point = Point { x: 10.0, y: 10.0 };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.hit_test(point), Some(NodeId(3)));

        assert!(layout.scroll_y(point, 40.0, &mut state));
        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.hit_test(point), Some(NodeId(4)));
    }

    #[test]
    fn scroll_limit_includes_gap_and_padding() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 50.0);
        scroll.style.padding = 10.0;
        scroll.style.gap = 5.0;
        scroll.children.push(fixed(3, Kind::Button, 80.0, 20.0));
        scroll.children.push(fixed(4, Kind::Button, 80.0, 20.0));
        root.children.push(scroll);
        let mut state = ScrollState::default();
        let layout = scrolled(
            &root,
            Size {
                width: 100.0,
                height: 50.0,
            },
            &mut state,
        );

        assert!(layout.scroll_y(Point { x: 1.0, y: 1.0 }, 100.0, &mut state));
        assert_eq!(state.offset(NodeId(2)), 15.0);
    }

    #[test]
    fn growing_scroll_viewport_shrinks_below_fixed_siblings() {
        let mut root = Node::new(NodeId(1), Kind::Column);
        root.children.push(fixed(2, Kind::Button, 100.0, 30.0));
        let mut scroll = Node::new(NodeId(3), Kind::Scroll);
        scroll.style.width = Some(100.0);
        scroll.style.grow = 1.0;
        scroll.children.push(fixed(4, Kind::Button, 100.0, 200.0));
        root.children.push(scroll);
        let mut state = ScrollState::default();
        let layout = scrolled(
            &root,
            Size {
                width: 100.0,
                height: 100.0,
            },
            &mut state,
        );

        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.height, 70.0);
        assert!(layout.scroll_y(Point { x: 1.0, y: 31.0 }, 500.0, &mut state));
        assert_eq!(state.offset(NodeId(3)), 130.0);
    }

    #[test]
    fn nested_scrolls_bubble_only_at_the_directional_limit() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut outer = fixed(2, Kind::Scroll, 100.0, 100.0);
        let mut page = fixed(3, Kind::Column, 100.0, 200.0);
        let mut inner = fixed(4, Kind::Scroll, 100.0, 50.0);
        inner.children.push(fixed(5, Kind::Button, 100.0, 50.0));
        inner.children.push(fixed(6, Kind::Button, 100.0, 50.0));
        page.children.push(inner);
        outer.children.push(page);
        root.children.push(outer);
        let size = Size {
            width: 100.0,
            height: 100.0,
        };
        let point = Point { x: 10.0, y: 10.0 };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);

        assert!(layout.scroll_y(point, 70.0, &mut state));
        assert_eq!(state.offset(NodeId(4)), 50.0);
        assert_eq!(state.offset(NodeId(2)), 0.0);
        assert!(layout.scroll_y(point, 20.0, &mut state));
        assert_eq!(state.offset(NodeId(2)), 20.0);

        let layout = scrolled(&root, size, &mut state);
        assert!(layout.scroll_y(point, -70.0, &mut state));
        assert_eq!(state.offset(NodeId(4)), 0.0);
        assert_eq!(state.offset(NodeId(2)), 20.0);
        assert!(layout.scroll_y(point, -70.0, &mut state));
        assert_eq!(state.offset(NodeId(2)), 0.0);
    }

    #[test]
    fn disabled_nested_scroll_uses_the_enabled_ancestor() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut outer = fixed(2, Kind::Scroll, 100.0, 50.0);
        let mut page = fixed(3, Kind::Column, 100.0, 100.0);
        let mut inner = fixed(4, Kind::Scroll, 100.0, 40.0);
        inner.enabled = false;
        inner.children.push(fixed(5, Kind::Button, 100.0, 80.0));
        page.children.push(inner);
        outer.children.push(page);
        root.children.push(outer);
        let mut state = ScrollState::default();
        let layout = scrolled(
            &root,
            Size {
                width: 100.0,
                height: 50.0,
            },
            &mut state,
        );

        assert!(layout.scroll_y(Point { x: 10.0, y: 10.0 }, 20.0, &mut state));
        assert_eq!(state.offset(NodeId(2)), 20.0);
        assert_eq!(state.offset(NodeId(4)), 0.0);
    }

    #[test]
    fn scroll_state_survives_visibility_and_clamps_or_prunes_with_the_tree() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 40.0);
        for id in 3..=5 {
            scroll.children.push(fixed(id, Kind::Button, 100.0, 30.0));
        }
        root.children.push(scroll);
        let size = Size {
            width: 100.0,
            height: 40.0,
        };
        let point = Point { x: 10.0, y: 10.0 };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);
        assert!(layout.scroll_y(point, 30.0, &mut state));

        root.children[0].visible = false;
        scrolled(&root, size, &mut state);
        assert_eq!(state.offset(NodeId(2)), 30.0);

        root.children[0].visible = true;
        root.children[0].children.pop();
        scrolled(&root, size, &mut state);
        assert_eq!(state.offset(NodeId(2)), 20.0);

        root.children.clear();
        scrolled(&root, size, &mut state);
        assert_eq!(state.offset(NodeId(2)), 0.0);
    }

    #[test]
    fn colliding_node_ids_have_independent_scroll_state() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 40.0);
        scroll.children.push(fixed(3, Kind::Button, 100.0, 80.0));
        root.children.push(scroll);
        let size = Size {
            width: 100.0,
            height: 40.0,
        };
        let point = Point { x: 10.0, y: 10.0 };
        let mut left = ScrollState::default();
        let mut right = ScrollState::default();
        let layout = scrolled(&root, size, &mut left);
        scrolled(&root, size, &mut right);

        assert!(layout.scroll_y(point, 20.0, &mut left));
        assert_eq!(left.offset(NodeId(2)), 20.0);
        assert_eq!(right.offset(NodeId(2)), 0.0);
    }

    #[test]
    fn overlapping_scrolls_use_frontmost_paint_order() {
        let mut root = Node::new(NodeId(1), Kind::Stack);
        for (scroll_id, child_id) in [(2, 3), (4, 5)] {
            let mut scroll = fixed(scroll_id, Kind::Scroll, 100.0, 40.0);
            scroll
                .children
                .push(fixed(child_id, Kind::Button, 100.0, 80.0));
            root.children.push(scroll);
        }
        let mut state = ScrollState::default();
        let layout = scrolled(
            &root,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut state,
        );

        assert!(layout.scroll_y(Point { x: 10.0, y: 10.0 }, 20.0, &mut state));
        assert_eq!(state.offset(NodeId(2)), 0.0);
        assert_eq!(state.offset(NodeId(4)), 20.0);
    }

    #[test]
    fn foreground_centerbox_group_blocks_an_overlapped_scroll_viewport() {
        let mut root = Node::new(NodeId(1), Kind::CenterBox);
        root.children.push(fixed(2, Kind::Row, 10.0, 40.0));
        let mut scroll = fixed(3, Kind::Scroll, 80.0, 40.0);
        scroll.children.push(fixed(4, Kind::Button, 80.0, 80.0));
        root.children.push(scroll);
        root.children.push(fixed(5, Kind::Row, 80.0, 40.0));
        let mut state = ScrollState::default();
        let layout = scrolled(
            &root,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut state,
        );

        let overlap = Point { x: 50.0, y: 10.0 };
        assert_eq!(layout.scroll_event_target(overlap), None);
        assert!(!layout.scroll_y(overlap, 20.0, &mut state));
        assert_eq!(state.offset(NodeId(3)), 0.0);
        let visible = Point { x: 15.0, y: 10.0 };
        assert_eq!(layout.scroll_event_target(visible), Some(NodeId(4)));
        assert!(layout.scroll_y(visible, 20.0, &mut state));
        assert_eq!(state.offset(NodeId(3)), 20.0);
    }
}
