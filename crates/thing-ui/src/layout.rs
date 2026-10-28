use std::collections::BTreeMap;

use taffy::{prelude as tf, style::Position};
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
}

#[derive(Clone, Copy)]
struct Inherited {
    origin: Point,
    clip: Rect,
    enabled: bool,
    opacity: f32,
}

/// resolved geometry for one surface. popup children have separate surfaces and
/// do not participate in their parent's flex layout.
#[derive(Debug, Default)]
pub struct Layout {
    placements: Vec<Placement>,
}

impl Layout {
    pub fn compute(root: &Node, size: Size, measurer: &mut impl Measurer) -> Result<Self, String> {
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
        result.collect(
            root,
            &tree,
            &ids,
            Inherited {
                origin: Point { x: 0.0, y: 0.0 },
                clip,
                enabled: true,
                opacity: 1.0,
            },
        )?;
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

    fn collect(
        &mut self,
        node: &Node,
        tree: &tf::TaffyTree<NodeId>,
        ids: &BTreeMap<NodeId, tf::NodeId>,
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
        } = inherited;
        let geometry = tree.layout(ids[&node.id]).map_err(|e| e.to_string())?;
        let bounds = Rect {
            x: origin.x + geometry.location.x,
            y: origin.y + geometry.location.y,
            width: geometry.size.width,
            height: geometry.size.height,
        };
        let enabled = enabled && node.enabled;
        let opacity = opacity * node.style.opacity;
        self.placements.push(Placement {
            id: node.id,
            bounds,
            clip,
            interactive: enabled && node.kind.is_interactive(),
            enabled,
            opacity,
        });
        let Some(child_clip) = bounds.intersection(clip) else {
            return Ok(());
        };
        for child in &node.children {
            if child.kind != Kind::Popup {
                self.collect(
                    child,
                    tree,
                    ids,
                    Inherited {
                        origin: Point {
                            x: bounds.x,
                            y: bounds.y,
                        },
                        clip: child_clip,
                        enabled,
                        opacity,
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
        flex_shrink: 0.0,
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

    struct FixedText;
    impl Measurer for FixedText {
        fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
            Size {
                width: 30.0,
                height: 10.0,
            }
        }
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
}
