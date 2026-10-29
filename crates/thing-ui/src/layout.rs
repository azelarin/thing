use std::collections::{BTreeMap, BTreeSet};

use taffy::{
    geometry::Point as TaffyPoint,
    prelude as tf,
    style::{Overflow, Position},
};
use thing_core::{CrossAlign, Kind, MainAlign, Node, NodeId, Point, Rect};

use crate::Affine;

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

/// one visible widget in painting order with stable and transformed geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub id: NodeId,
    /// stable Taffy geometry in surface coordinates.
    pub bounds: Rect,
    /// the transformed border box without clipping.
    pub border_aabb: Rect,
    pub transform: Affine,
    pub clip: Rect,
    pub interactive: bool,
    pub pointer_events: bool,
    pub scroll_events: bool,
    pub enabled: bool,
    pub opacity: f32,
    pub(crate) disabled_opacity: f32,
    pub(crate) inverse: Option<Affine>,
    pub(crate) clip_chain: usize,
}

impl Placement {
    pub fn local_bounds(self) -> Rect {
        Rect {
            width: self.bounds.width,
            height: self.bounds.height,
            ..Rect::default()
        }
    }

    pub fn surface_to_local(self, point: Point) -> Option<Point> {
        self.inverse?.point(point)
    }

    pub fn magnification(self) -> f64 {
        self.transform.magnification()
    }

    pub fn is_invertible(self) -> bool {
        self.inverse.is_some()
    }
}

/// semantic geometry retained even when an ancestor clips all painting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SemanticPlacement {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub scroll_parent: Option<NodeId>,
    /// stable Taffy geometry in surface-local logical pixels.
    pub bounds: Rect,
    /// the transformed border box without clipping.
    pub border_aabb: Rect,
    pub transform: Affine,
    pub enabled: bool,
    pub opacity: f32,
    pub(crate) disabled_opacity: f32,
    pub(crate) shadow_clip_chain: usize,
    pub(crate) shadow_clip: Rect,
    pub scroll_offset: Option<f32>,
    pub max_scroll_offset: Option<f32>,
    inverse: Option<Affine>,
}

impl SemanticPlacement {
    pub fn local_bounds(self) -> Rect {
        Rect {
            width: self.bounds.width,
            height: self.bounds.height,
            ..Rect::default()
        }
    }

    pub fn is_invertible(self) -> bool {
        self.inverse.is_some()
    }
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
        if offset.is_finite() && offset > 0.0 {
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
    viewport: Rect,
    offset: f32,
    max_offset: f32,
}

#[derive(Clone, Copy, Debug)]
struct RevealRegion {
    id: NodeId,
    bounds: Rect,
    enabled: bool,
    focusable: bool,
    scroll_parent: Option<NodeId>,
}

#[derive(Clone, Copy, Debug)]
struct PointerRegion {
    /// the placement index of the parent widget.
    parent: Option<usize>,
    /// whether the widget is an enabled scroll viewport.
    viewport: bool,
}

/// one stop on a wheel event's route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollStep {
    /// a widget's `on_scroll` handler.
    Handler(NodeId),
    /// a scroll viewport's built-in vertical scrolling.
    Viewport(NodeId),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClipRegion {
    pub(crate) bounds: Rect,
    pub(crate) transform: Affine,
    pub(crate) inverse: Option<Affine>,
    pub(crate) aabb: Rect,
    pub(crate) parent: Option<usize>,
    pub(crate) depth: usize,
}

#[derive(Clone, Copy)]
struct Inherited {
    origin: Point,
    clip_chain: usize,
    clip_aabb: Rect,
    enabled: bool,
    parent: Option<usize>,
}

#[derive(Clone, Copy)]
struct RevealInherited {
    origin: Point,
    transform: Affine,
    enabled: bool,
    parent: Option<NodeId>,
    scroll_parent: Option<NodeId>,
    reveal_scroll_parent: Option<NodeId>,
    opacity: f32,
    disabled_opacity: f32,
    shadow_clip_chain: usize,
    shadow_clip: Rect,
}

/// resolved geometry for one surface. popup children have separate surfaces and
/// do not participate in their parent's flex layout.
#[derive(Debug, Default)]
pub struct Layout {
    placements: Vec<Placement>,
    placement_indices: BTreeMap<NodeId, usize>,
    semantic_placements: Vec<SemanticPlacement>,
    semantic_indices: BTreeMap<NodeId, usize>,
    pointer_regions: Vec<PointerRegion>,
    clip_regions: Vec<ClipRegion>,
    scroll_areas: Vec<ScrollArea>,
    reveal_regions: Vec<RevealRegion>,
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
                            Kind::TextInput | Kind::SecretInput => Size {
                                width: 160.0,
                                height: measurer
                                    .measure(node, None)
                                    .height
                                    .max(node.style.font_size * 1.2)
                                    .max(24.0),
                            },
                            Kind::Image => Size {
                                width: 32.0,
                                height: 32.0,
                            },
                            Kind::Path => node
                                .path
                                .view_box
                                .map(|view_box| Size {
                                    width: view_box.width,
                                    height: view_box.height,
                                })
                                .unwrap_or_default(),
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
        result.clip_regions.push(ClipRegion {
            bounds: clip,
            transform: Affine::IDENTITY,
            inverse: Some(Affine::IDENTITY),
            aabb: clip,
            parent: None,
            depth: 0,
        });
        let mut next_state = state.clone();
        next_state.retain_tree(root);
        result.collect_semantic_regions(
            root,
            &tree,
            &ids,
            &mut next_state,
            RevealInherited {
                origin: Point { x: 0.0, y: 0.0 },
                transform: Affine::IDENTITY,
                enabled: true,
                parent: None,
                scroll_parent: None,
                reveal_scroll_parent: None,
                opacity: 1.0,
                disabled_opacity: 1.0,
                shadow_clip_chain: 0,
                shadow_clip: clip,
            },
        )?;
        result.collect(
            root,
            &tree,
            &ids,
            &mut next_state,
            Inherited {
                origin: Point { x: 0.0, y: 0.0 },
                clip_chain: 0,
                clip_aabb: clip,
                enabled: true,
                parent: None,
            },
        )?;
        *state = next_state;
        Ok(result)
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub fn semantic_placements(&self) -> &[SemanticPlacement] {
        &self.semantic_placements
    }

    pub fn semantic_placement(&self, id: NodeId) -> Option<&SemanticPlacement> {
        self.semantic_indices
            .get(&id)
            .and_then(|index| self.semantic_placements.get(*index))
    }

    /// widgets whose presented geometry can begin pointer input.
    pub fn pointer_targets(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.placements
            .iter()
            .zip(&self.pointer_regions)
            .filter_map(|(placement, region)| {
                (placement.interactive
                    || placement.pointer_events
                    || placement.scroll_events
                    || region.viewport)
                    .then_some(placement)
                    .filter(|placement| placement.inverse.is_some())
                    .map(|placement| placement.id)
            })
    }

    pub fn pointer_target_is_eligible(&self, id: NodeId) -> bool {
        self.pointer_targets().any(|candidate| candidate == id)
    }

    pub fn get(&self, id: NodeId) -> Option<&Placement> {
        self.placement_indices
            .get(&id)
            .and_then(|index| self.placements.get(*index))
    }

    pub fn surface_to_local(&self, id: NodeId, point: Point) -> Option<Point> {
        self.get(id)?.surface_to_local(point)
    }

    /// hit test from front to back. text inside a button does not hide its parent.
    pub fn hit_test(&self, point: Point) -> Option<NodeId> {
        self.placements
            .iter()
            .enumerate()
            .rev()
            .find(|(index, placement)| {
                placement.interactive && self.placement_contains(*index, point)
            })
            .map(|(_, placement)| placement.id)
    }

    /// find the deepest semantic control or custom pointer target under a point.
    pub fn pointer_event_target(&self, point: Point) -> Option<NodeId> {
        self.placements
            .iter()
            .enumerate()
            .rev()
            .find(|(index, placement)| {
                (placement.interactive || placement.pointer_events)
                    && self.placement_contains(*index, point)
            })
            .map(|(_, placement)| placement.id)
    }

    pub fn focus_order(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.reveal_regions
            .iter()
            .filter(|region| region.focusable)
            .map(|region| region.id)
    }

    /// the wheel route at `point`: the topmost painted widget and then its
    /// ancestors. a widget's handler comes before its own viewport scrolling.
    /// every stop contains the point after transforms and clipping.
    pub fn scroll_route(&self, point: Point) -> Vec<ScrollStep> {
        let mut route = Vec::new();
        let mut current = (0..self.placements.len())
            .rev()
            .find(|index| self.placement_contains(*index, point));
        while let Some(index) = current {
            let placement = &self.placements[index];
            let region = &self.pointer_regions[index];
            if self.placement_contains(index, point) {
                if placement.scroll_events {
                    route.push(ScrollStep::Handler(placement.id));
                }
                if region.viewport {
                    route.push(ScrollStep::Viewport(placement.id));
                }
            }
            current = region.parent;
        }
        route
    }

    /// scroll the viewports on the route at `point`, innermost first. a
    /// viewport at its limit lets the movement continue outward.
    pub fn scroll_y(&self, point: Point, delta: f32, state: &mut ScrollState) -> bool {
        self.scroll_route(point).into_iter().any(|step| match step {
            ScrollStep::Viewport(id) => self.scroll_view_y(id, delta, state),
            ScrollStep::Handler(_) => false,
        })
    }

    /// move one enabled viewport by `delta`, returning whether it moved.
    pub fn scroll_view_y(&self, id: NodeId, delta: f32, state: &mut ScrollState) -> bool {
        if !delta.is_finite() || delta == 0.0 {
            return false;
        }
        let Some(area) = self.scroll_areas.iter().find(|area| area.id == id) else {
            return false;
        };
        let current = state.offset(area.id).clamp(0.0, area.max_offset);
        let next = (current + delta).clamp(0.0, area.max_offset);
        if next == current {
            return false;
        }
        state.set(area.id, next);
        true
    }

    /// make a visible, enabled widget vertically visible in every enclosing
    /// scroll viewport, using the smallest movement at each level.
    pub fn reveal_y(&self, id: NodeId, state: &mut ScrollState) -> bool {
        let Some(region) = self
            .reveal_regions
            .iter()
            .find(|region| region.id == id && region.enabled)
        else {
            return false;
        };
        if vertical_edges(region.bounds).is_none() {
            return false;
        }

        let mut bounds = region.bounds;
        let mut ancestor = region.scroll_parent;
        while let Some(id) = ancestor {
            let Some(area) = self.scroll_areas.iter().find(|area| area.id == id) else {
                break;
            };
            bounds.y -= state.offset(area.id).clamp(0.0, area.max_offset) - area.offset;
            ancestor = area.parent;
        }
        let mut parent = region.scroll_parent;
        let mut changed = false;
        while let Some(id) = parent {
            let Some(area) = self.scroll_areas.iter().find(|area| area.id == id) else {
                break;
            };
            let mut viewport = area.viewport;
            let mut ancestor = area.parent;
            while let Some(id) = ancestor {
                let Some(parent) = self.scroll_areas.iter().find(|area| area.id == id) else {
                    break;
                };
                viewport.y -= state.offset(parent.id).clamp(0.0, parent.max_offset) - parent.offset;
                ancestor = parent.parent;
            }
            let current = state.offset(area.id).clamp(0.0, area.max_offset);
            let delta = reveal_delta(bounds, viewport).unwrap_or(0.0);
            let next = (current + delta).clamp(0.0, area.max_offset);
            let applied = next - current;
            if applied != 0.0 {
                state.set(area.id, next);
                bounds.y -= applied;
                changed = true;
            }
            let Some(visible) = vertical_intersection(bounds, viewport) else {
                break;
            };
            bounds = visible;
            parent = area.parent;
        }
        changed
    }

    fn placement_contains(&self, index: usize, point: Point) -> bool {
        let Some(placement) = self.placements.get(index) else {
            return false;
        };
        placement
            .surface_to_local(point)
            .is_some_and(|local| placement.local_bounds().contains(local))
            && self.clip_contains(placement.clip_chain, point)
    }

    fn clip_contains(&self, mut index: usize, point: Point) -> bool {
        loop {
            let Some(region) = self.clip_regions.get(index) else {
                return false;
            };
            if !region.aabb.contains(point)
                || region
                    .inverse
                    .and_then(|inverse| inverse.point(point))
                    .is_none_or(|local| !region.bounds.contains(local))
            {
                return false;
            }
            let Some(parent) = region.parent else {
                return true;
            };
            index = parent;
        }
    }

    pub(crate) fn clip_region(&self, index: usize) -> Option<&ClipRegion> {
        self.clip_regions.get(index)
    }

    fn collect_semantic_regions(
        &mut self,
        node: &Node,
        tree: &tf::TaffyTree<NodeId>,
        ids: &BTreeMap<NodeId, tf::NodeId>,
        state: &mut ScrollState,
        inherited: RevealInherited,
    ) -> Result<(), String> {
        if !node.visible {
            return Ok(());
        }
        let geometry = tree
            .layout(ids[&node.id])
            .map_err(|error| error.to_string())?;
        let bounds = Rect {
            x: inherited.origin.x + geometry.location.x,
            y: inherited.origin.y + geometry.location.y,
            width: geometry.size.width,
            height: geometry.size.height,
        };
        let local_bounds = Rect {
            width: bounds.width,
            height: bounds.height,
            ..Rect::default()
        };
        // Skia consumes f32 coefficients. Every downstream geometry operation
        // uses those exact coefficients so painting and input cannot diverge.
        let transform = inherited
            .transform
            .compose(Affine::translation(
                f64::from(geometry.location.x),
                f64::from(geometry.location.y),
            ))?
            .compose(Affine::widget(
                &node.style,
                geometry.size.width,
                geometry.size.height,
            )?)?
            .renderer_canonical()?;
        let inverse = transform.inverse()?;
        let border_aabb = transform.rect_aabb(local_bounds)?;
        let enabled = inherited.enabled && node.enabled;
        let opacity = inherited.opacity * node.style.opacity;
        let disabled_opacity = if inherited.enabled && !node.enabled {
            node.style.disabled_opacity
        } else {
            inherited.disabled_opacity
        };
        let scroll = (node.kind == Kind::Scroll).then(|| {
            let max_offset = if geometry.scroll_height().is_finite() {
                geometry.scroll_height().max(0.0)
            } else {
                0.0
            };
            (state.offset(node.id).clamp(0.0, max_offset), max_offset)
        });
        self.semantic_indices
            .insert(node.id, self.semantic_placements.len());
        self.semantic_placements.push(SemanticPlacement {
            id: node.id,
            parent: inherited.parent,
            scroll_parent: inherited.scroll_parent,
            bounds,
            border_aabb,
            transform,
            enabled,
            opacity,
            disabled_opacity,
            shadow_clip_chain: inherited.shadow_clip_chain,
            shadow_clip: inherited.shadow_clip,
            scroll_offset: scroll.map(|(offset, _)| offset),
            max_scroll_offset: scroll.map(|(_, max_offset)| max_offset),
            inverse,
        });
        self.reveal_regions.push(RevealRegion {
            id: node.id,
            bounds,
            enabled,
            focusable: enabled
                && node.kind.is_interactive()
                && [bounds.x, bounds.y, bounds.width, bounds.height]
                    .into_iter()
                    .all(f32::is_finite)
                && bounds.width > 0.0
                && bounds.height > 0.0,
            scroll_parent: inherited.reveal_scroll_parent,
        });

        let mut child_origin = Point {
            x: bounds.x,
            y: bounds.y,
        };
        let mut child_transform = transform;
        let mut child_scroll_parent = inherited.scroll_parent;
        let mut child_reveal_scroll_parent = inherited.reveal_scroll_parent;
        if let Some((offset, max_offset)) = scroll {
            state.set(node.id, offset);
            child_scroll_parent = Some(node.id);
            if enabled {
                self.scroll_areas.push(ScrollArea {
                    id: node.id,
                    parent: inherited.reveal_scroll_parent,
                    viewport: bounds,
                    offset,
                    max_offset,
                });
                child_reveal_scroll_parent = Some(node.id);
            }
            child_origin.y -= offset;
            child_transform =
                child_transform.compose(Affine::translation(0.0, -f64::from(offset)))?;
        }

        let (shadow_clip_chain, shadow_clip) = if node.kind == Kind::Scroll {
            let index = self.clip_regions.len();
            self.clip_regions.push(ClipRegion {
                bounds: local_bounds,
                transform,
                inverse,
                aabb: border_aabb,
                parent: Some(inherited.shadow_clip_chain),
                depth: self.clip_regions[inherited.shadow_clip_chain].depth + 1,
            });
            (
                index,
                inherited
                    .shadow_clip
                    .intersection(border_aabb)
                    .unwrap_or_default(),
            )
        } else {
            (inherited.shadow_clip_chain, inherited.shadow_clip)
        };
        for child in &node.children {
            if child.kind != Kind::Popup {
                self.collect_semantic_regions(
                    child,
                    tree,
                    ids,
                    state,
                    RevealInherited {
                        origin: child_origin,
                        transform: child_transform,
                        enabled,
                        parent: Some(node.id),
                        scroll_parent: child_scroll_parent,
                        reveal_scroll_parent: child_reveal_scroll_parent,
                        opacity,
                        disabled_opacity,
                        shadow_clip_chain,
                        shadow_clip,
                    },
                )?;
            }
        }
        Ok(())
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
            clip_chain,
            clip_aabb,
            enabled,
            parent,
        } = inherited;
        let geometry = tree.layout(ids[&node.id]).map_err(|e| e.to_string())?;
        let bounds = Rect {
            x: origin.x + geometry.location.x,
            y: origin.y + geometry.location.y,
            width: geometry.size.width,
            height: geometry.size.height,
        };
        let local_bounds = Rect {
            width: bounds.width,
            height: bounds.height,
            ..Rect::default()
        };
        let semantic = *self
            .semantic_placement(node.id)
            .ok_or("visual layout is missing semantic geometry")?;
        let transform = semantic.transform;
        let inverse = semantic.inverse;
        let border_aabb = semantic.border_aabb;
        let enabled = enabled && node.enabled;
        let opacity = semantic.opacity;
        let disabled_opacity = semantic.disabled_opacity;
        crate::brush::validate(node, local_bounds)?;
        let index = self.placements.len();
        self.placement_indices.insert(node.id, index);
        self.placements.push(Placement {
            id: node.id,
            bounds,
            border_aabb,
            transform,
            clip: clip_aabb,
            interactive: enabled && node.kind.is_interactive(),
            pointer_events: enabled && node.pointer_events,
            scroll_events: enabled && node.scroll_events,
            enabled,
            opacity,
            disabled_opacity,
            inverse,
            clip_chain,
        });
        self.pointer_regions.push(PointerRegion {
            parent,
            viewport: enabled && node.kind == Kind::Scroll,
        });
        let Some(child_clip_aabb) = border_aabb.intersection(clip_aabb) else {
            return Ok(());
        };
        let child_clip_chain = self.clip_regions.len();
        self.clip_regions.push(ClipRegion {
            bounds: local_bounds,
            transform,
            inverse,
            aabb: border_aabb,
            parent: Some(clip_chain),
            depth: self.clip_regions[clip_chain].depth + 1,
        });
        let mut child_origin = Point {
            x: bounds.x,
            y: bounds.y,
        };
        if node.kind == Kind::Scroll {
            let max_offset = if geometry.scroll_height().is_finite() {
                geometry.scroll_height().max(0.0)
            } else {
                0.0
            };
            child_origin.y -= state.offset(node.id).clamp(0.0, max_offset);
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
                        clip_chain: child_clip_chain,
                        clip_aabb: child_clip_aabb,
                        enabled,
                        parent: Some(index),
                    },
                )?;
            }
        }
        Ok(())
    }
}

fn vertical_edges(rect: Rect) -> Option<(f32, f32)> {
    let bottom = rect.y + rect.height;
    (rect.y.is_finite() && rect.height.is_finite() && rect.height > 0.0 && bottom.is_finite())
        .then_some((rect.y, bottom))
}

fn reveal_delta(target: Rect, viewport: Rect) -> Option<f32> {
    let (target_top, target_bottom) = vertical_edges(target)?;
    let (viewport_top, viewport_bottom) = vertical_edges(viewport)?;
    Some(
        if target.height > viewport.height || target_top < viewport_top {
            target_top - viewport_top
        } else if target_bottom > viewport_bottom {
            target_bottom - viewport_bottom
        } else {
            0.0
        },
    )
}

fn vertical_intersection(target: Rect, viewport: Rect) -> Option<Rect> {
    let (target_top, target_bottom) = vertical_edges(target)?;
    let (viewport_top, viewport_bottom) = vertical_edges(viewport)?;
    let top = target_top.max(viewport_top);
    let bottom = target_bottom.min(viewport_bottom);
    (bottom > top).then_some(Rect {
        y: top,
        height: bottom - top,
        ..target
    })
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
    let explicit_shrink = style.shrink.is_some_and(|value| value > 0.0);
    let min_width = style.min_width.or_else(|| explicit_shrink.then_some(0.0));
    let min_height = style.min_height.or_else(|| explicit_shrink.then_some(0.0));
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
            width: style.width.map_or_else(
                || {
                    style
                        .width_fraction
                        .map_or(tf::Dimension::auto(), tf::Dimension::percent)
                },
                tf::Dimension::length,
            ),
            height: style.height.map_or_else(
                || {
                    style
                        .height_fraction
                        .map_or(tf::Dimension::auto(), tf::Dimension::percent)
                },
                tf::Dimension::length,
            ),
        },
        min_size: tf::Size {
            width: min_width.map_or(
                tf::LengthPercentageAuto::auto(),
                tf::LengthPercentageAuto::length,
            ),
            height: min_height.map_or(
                tf::LengthPercentageAuto::auto(),
                tf::LengthPercentageAuto::length,
            ),
        },
        max_size: tf::Size {
            width: style.max_width.map_or(
                tf::LengthPercentageAuto::auto(),
                tf::LengthPercentageAuto::length,
            ),
            height: style.max_height.map_or(
                tf::LengthPercentageAuto::auto(),
                tf::LengthPercentageAuto::length,
            ),
        },
        flex_direction: if matches!(node.kind, Kind::Column | Kind::Scroll) {
            tf::FlexDirection::Column
        } else {
            tf::FlexDirection::Row
        },
        flex_wrap: if style.wrap {
            tf::FlexWrap::Wrap
        } else {
            tf::FlexWrap::NoWrap
        },
        flex_grow: style.grow,
        flex_shrink: style
            .shrink
            .unwrap_or(if node.kind == Kind::Scroll { 1.0 } else { 0.0 }),
        overflow: TaffyPoint {
            x: Overflow::Clip,
            y: if node.kind == Kind::Scroll {
                Overflow::Scroll
            } else {
                Overflow::Clip
            },
        },
        align_items: align_items(style.align),
        align_self: style.align_self.map(align_items),
        align_content: align_content(style.align_content),
        justify_content: if node.kind == Kind::CenterBox {
            tf::JustifyContent::CENTER
        } else {
            justify_content(style.justify, node.kind == Kind::Scroll)
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

fn align_items(value: CrossAlign) -> tf::AlignItems {
    match value {
        CrossAlign::Start => tf::AlignItems::FLEX_START,
        CrossAlign::Center => tf::AlignItems::CENTER,
        CrossAlign::End => tf::AlignItems::FLEX_END,
        CrossAlign::Stretch => tf::AlignItems::STRETCH,
    }
}

fn align_content(value: CrossAlign) -> tf::AlignContent {
    match value {
        CrossAlign::Start => tf::AlignContent::FLEX_START,
        CrossAlign::Center => tf::AlignContent::SAFE_CENTER,
        CrossAlign::End => tf::AlignContent::SAFE_FLEX_END,
        CrossAlign::Stretch => tf::AlignContent::STRETCH,
    }
}

fn justify_content(value: MainAlign, safe: bool) -> tf::JustifyContent {
    match value {
        MainAlign::Start if safe => tf::JustifyContent::SAFE_FLEX_START,
        MainAlign::Center if safe => tf::JustifyContent::SAFE_CENTER,
        MainAlign::End if safe => tf::JustifyContent::SAFE_FLEX_END,
        MainAlign::Start => tf::JustifyContent::FLEX_START,
        MainAlign::Center => tf::JustifyContent::CENTER,
        MainAlign::End => tf::JustifyContent::FLEX_END,
        MainAlign::SpaceBetween => tf::JustifyContent::SPACE_BETWEEN,
        MainAlign::SpaceAround => tf::JustifyContent::SPACE_AROUND,
        MainAlign::SpaceEvenly => tf::JustifyContent::SPACE_EVENLY,
    }
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

    struct WrappingText;
    impl Measurer for WrappingText {
        fn measure(&mut self, _: &Node, max_width: Option<f32>) -> Size {
            let width = max_width.unwrap_or(80.0).min(80.0);
            let lines = (80.0 / width.max(1.0)).ceil();
            Size {
                width,
                height: lines * 10.0,
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
    fn centerbox_keeps_main_axis_placement_with_cross_axis_overrides() {
        let mut root = Node::new(NodeId(1), Kind::CenterBox);
        root.style.align = CrossAlign::End;
        for id in 2..=4 {
            root.children.push(fixed(id, Kind::Button, 10.0, 10.0));
        }
        root.children[1].style.align_self = Some(CrossAlign::Start);
        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 60.0,
            },
            &mut FixedText,
        )
        .unwrap();

        assert_eq!(
            layout.get(NodeId(2)).unwrap().bounds,
            Rect {
                x: 0.0,
                y: 50.0,
                width: 10.0,
                height: 10.0,
            }
        );
        assert_eq!(
            layout.get(NodeId(3)).unwrap().bounds,
            Rect {
                x: 45.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            }
        );
        assert_eq!(
            layout.get(NodeId(4)).unwrap().bounds,
            Rect {
                x: 90.0,
                y: 50.0,
                width: 10.0,
                height: 10.0,
            }
        );
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
    fn paths_use_the_view_box_as_their_intrinsic_size() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        let mut path = Node::new(NodeId(2), Kind::Path);
        path.path.set_data("M 0 0 L 40 20".into()).unwrap();
        path.path.view_box = Some(thing_core::ViewBox::new(0.0, 0.0, 40.0, 20.0).unwrap());
        path.style.padding = 3.0;
        root.children.push(path);

        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 50.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.width, 46.0);
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.height, 26.0);
    }

    #[test]
    fn text_inputs_have_stable_intrinsics_and_join_focus_order() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        root.children.push(Node::new(NodeId(2), Kind::TextInput));
        let layout = Layout::compute(
            &root,
            Size {
                width: 300.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();

        let input = layout.get(NodeId(2)).unwrap();
        assert_eq!(input.bounds.width, 160.0);
        assert_eq!(input.bounds.height, 24.0);
        assert!(input.interactive);
        assert_eq!(
            layout.hit_test(Point {
                x: input.bounds.x + input.bounds.width / 2.0,
                y: input.bounds.y + input.bounds.height / 2.0,
            }),
            Some(NodeId(2))
        );
        assert_eq!(layout.focus_order().collect::<Vec<_>>(), vec![NodeId(2)]);
    }

    #[test]
    fn pointer_areas_route_to_the_deepest_target_without_joining_focus_order() {
        let mut root = fixed(1, Kind::PointerArea, 100.0, 40.0);
        root.pointer_events = true;
        root.children.push(fixed(2, Kind::Button, 20.0, 20.0));
        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();

        assert_eq!(
            layout.pointer_event_target(Point { x: 10.0, y: 10.0 }),
            Some(NodeId(2))
        );
        assert_eq!(
            layout.pointer_event_target(Point { x: 50.0, y: 10.0 }),
            Some(NodeId(1))
        );
        assert_eq!(layout.hit_test(Point { x: 50.0, y: 10.0 }), None);
        assert_eq!(layout.focus_order().collect::<Vec<_>>(), vec![NodeId(2)]);
        assert_eq!(
            layout.pointer_targets().collect::<Vec<_>>(),
            [NodeId(1), NodeId(2)],
        );

        let mut button = fixed(3, Kind::Button, 100.0, 40.0);
        let mut nested = fixed(4, Kind::PointerArea, 20.0, 20.0);
        nested.pointer_events = true;
        button.children.push(nested);
        let layout = Layout::compute(
            &button,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(
            layout.pointer_event_target(Point { x: 10.0, y: 10.0 }),
            Some(NodeId(4))
        );
        assert_eq!(layout.hit_test(Point { x: 10.0, y: 10.0 }), Some(NodeId(3)));
    }

    #[test]
    fn scroll_only_pointer_areas_route_without_press_or_focus_semantics() {
        let mut area = fixed(1, Kind::PointerArea, 100.0, 40.0);
        area.scroll_events = true;
        area.children.push(fixed(2, Kind::Row, 20.0, 20.0));
        let layout = Layout::compute(
            &area,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();

        let point = Point { x: 10.0, y: 10.0 };
        assert_eq!(layout.scroll_route(point), [ScrollStep::Handler(NodeId(1))]);
        assert_eq!(layout.pointer_event_target(point), None);
        assert_eq!(layout.hit_test(point), None);
        assert!(layout.focus_order().next().is_none());
        assert_eq!(layout.pointer_targets().collect::<Vec<_>>(), [NodeId(1)]);

        let mut nested = fixed(3, Kind::PointerArea, 20.0, 20.0);
        nested.scroll_events = true;
        area.children.clear();
        area.children.push(nested);
        let layout = Layout::compute(
            &area,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        // an inner handler comes first, and its ancestor may follow.
        assert_eq!(
            layout.scroll_route(point),
            [
                ScrollStep::Handler(NodeId(3)),
                ScrollStep::Handler(NodeId(1))
            ]
        );
        assert_eq!(
            layout.scroll_route(Point { x: 50.0, y: 10.0 }),
            [ScrollStep::Handler(NodeId(1))]
        );
    }

    #[test]
    fn container_backgrounds_route_from_child_to_ancestors() {
        let size = Size {
            width: 200.0,
            height: 40.0,
        };
        let mut root = fixed(1, Kind::Panel, 200.0, 40.0);
        root.scroll_events = true;
        let mut row = fixed(2, Kind::Row, 200.0, 40.0);
        row.scroll_events = true;
        row.children.push(fixed(3, Kind::Text, 20.0, 20.0));
        let mut scroll = fixed(4, Kind::Scroll, 40.0, 40.0);
        scroll.scroll_events = true;
        scroll.children.push(fixed(5, Kind::Column, 40.0, 80.0));
        row.children.push(scroll);
        root.children.push(row);
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();

        // content without a handler and unused background both route to the
        // container, which stays out of clicks and focus.
        for point in [Point { x: 10.0, y: 10.0 }, Point { x: 150.0, y: 30.0 }] {
            assert_eq!(
                layout.scroll_route(point),
                [
                    ScrollStep::Handler(NodeId(2)),
                    ScrollStep::Handler(NodeId(1))
                ]
            );
            assert_eq!(layout.hit_test(point), None);
            assert_eq!(layout.pointer_event_target(point), None);
        }
        assert!(layout.focus_order().next().is_none());
        // a viewport's own handler runs before the viewport scrolls.
        assert_eq!(
            layout.scroll_route(Point { x: 30.0, y: 10.0 }),
            [
                ScrollStep::Handler(NodeId(4)),
                ScrollStep::Viewport(NodeId(4)),
                ScrollStep::Handler(NodeId(2)),
                ScrollStep::Handler(NodeId(1)),
            ]
        );

        // disabled branches neither handle nor scroll, and hidden ones are
        // not painted at all.
        root.children[0].enabled = false;
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        assert_eq!(
            layout.scroll_route(Point { x: 30.0, y: 10.0 }),
            [ScrollStep::Handler(NodeId(1))]
        );
        root.children[0].enabled = true;
        root.children[0].children[1].visible = false;
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        assert_eq!(
            layout.scroll_route(Point { x: 30.0, y: 10.0 }),
            [
                ScrollStep::Handler(NodeId(2)),
                ScrollStep::Handler(NodeId(1))
            ]
        );
    }

    #[test]
    fn clipped_and_noninvertible_geometry_never_joins_a_wheel_route() {
        let size = Size {
            width: 200.0,
            height: 40.0,
        };
        let mut root = fixed(1, Kind::Stack, 200.0, 40.0);
        let mut frame = fixed(2, Kind::Column, 40.0, 40.0);
        frame.scroll_events = true;
        // the child paints past its parent, where the parent clips it.
        let mut moved = fixed(3, Kind::Column, 40.0, 40.0);
        moved.scroll_events = true;
        moved.style.translate_x = 60.0;
        frame.children.push(moved);
        root.children.push(frame);
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        assert!(layout.scroll_route(Point { x: 80.0, y: 10.0 }).is_empty());
        assert_eq!(
            layout.scroll_route(Point { x: 10.0, y: 10.0 }),
            [ScrollStep::Handler(NodeId(2))]
        );

        // a collapsed container cannot map the pointer into its coordinates.
        root.children[0].style.scale_x = 0.0;
        root.children[0].children[0].style.translate_x = 0.0;
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        assert!(layout.scroll_route(Point { x: 10.0, y: 10.0 }).is_empty());
    }

    #[test]
    fn frontmost_noninteractive_content_occludes_scroll_handlers() {
        let mut stack = fixed(1, Kind::Stack, 100.0, 40.0);
        let mut background = fixed(2, Kind::PointerArea, 100.0, 40.0);
        background.scroll_events = true;
        stack.children.push(background);
        stack.children.push(fixed(3, Kind::Row, 100.0, 40.0));

        let layout = Layout::compute(
            &stack,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        // the background is an overlapped sibling, not an ancestor.
        assert!(layout.scroll_route(Point { x: 10.0, y: 10.0 }).is_empty());
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
    fn wraps_rows_and_columns_without_counting_hidden_children() {
        let mut row = Node::new(NodeId(1), Kind::Row);
        row.style.wrap = true;
        row.style.align = CrossAlign::Start;
        row.style.gap = 5.0;
        for id in 2..=4 {
            row.children.push(fixed(id, Kind::Button, 30.0, 10.0));
        }
        let size = Size {
            width: 70.0,
            height: 100.0,
        };
        let layout = Layout::compute(&row, size, &mut FixedText).unwrap();
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.x, 0.0);
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.x, 35.0);
        assert_eq!(
            layout.get(NodeId(4)).unwrap().bounds,
            Rect {
                x: 0.0,
                y: 15.0,
                width: 30.0,
                height: 10.0,
            }
        );

        row.children[1].visible = false;
        let layout = Layout::compute(&row, size, &mut FixedText).unwrap();
        assert!(layout.get(NodeId(3)).is_none());
        assert_eq!(layout.get(NodeId(4)).unwrap().bounds.x, 35.0);
        assert_eq!(layout.get(NodeId(4)).unwrap().bounds.y, 0.0);

        let mut column = Node::new(NodeId(5), Kind::Column);
        column.style.wrap = true;
        column.style.align = CrossAlign::Start;
        column.style.gap = 5.0;
        for id in 6..=8 {
            column.children.push(fixed(id, Kind::Button, 10.0, 30.0));
        }
        let layout = Layout::compute(
            &column,
            Size {
                width: 100.0,
                height: 70.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(layout.get(NodeId(6)).unwrap().bounds.y, 0.0);
        assert_eq!(layout.get(NodeId(7)).unwrap().bounds.y, 35.0);
        assert_eq!(layout.get(NodeId(8)).unwrap().bounds.x, 15.0);
        assert_eq!(layout.get(NodeId(8)).unwrap().bounds.y, 0.0);
    }

    #[test]
    fn aligns_items_lines_and_main_axis_independently() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        root.style.width = Some(100.0);
        root.style.height = Some(60.0);
        root.style.align = CrossAlign::Start;
        root.style.justify = MainAlign::SpaceBetween;
        for id in 2..=4 {
            root.children.push(fixed(id, Kind::Button, 10.0, 10.0));
        }
        root.children[1].style.align_self = Some(CrossAlign::End);
        root.children[2].style.height = None;
        root.children[2].style.align_self = Some(CrossAlign::Stretch);
        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 60.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.x, 0.0);
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.x, 45.0);
        assert_eq!(layout.get(NodeId(4)).unwrap().bounds.x, 90.0);
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.y, 0.0);
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.y, 50.0);
        assert_eq!(layout.get(NodeId(4)).unwrap().bounds.height, 60.0);

        root.style.width = Some(25.0);
        root.style.wrap = true;
        root.style.justify = MainAlign::Start;
        root.style.align_content = CrossAlign::End;
        root.children[2].style.height = Some(10.0);
        root.children[2].style.align_self = None;
        let layout = Layout::compute(
            &root,
            Size {
                width: 25.0,
                height: 60.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.y, 40.0);
        assert_eq!(layout.get(NodeId(4)).unwrap().bounds.y, 50.0);
    }

    #[test]
    fn shrink_remeasures_text_and_respects_minimums() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        root.children.push(fixed(2, Kind::Button, 60.0, 20.0));
        root.children.push(Node::new(NodeId(3), Kind::Text));
        let size = Size {
            width: 100.0,
            height: 50.0,
        };

        let layout = Layout::compute(&root, size, &mut WrappingText).unwrap();
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.width, 80.0);

        root.children[1].style.shrink = Some(1.0);
        let layout = Layout::compute(&root, size, &mut WrappingText).unwrap();
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.width, 40.0);
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.height, 20.0);

        root.children[1].style.min_width = Some(50.0);
        let layout = Layout::compute(&root, size, &mut WrappingText).unwrap();
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.width, 50.0);
    }

    #[test]
    fn max_size_limits_growth_and_oversized_alignment_keeps_hit_clipping() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        let mut growing = fixed(2, Kind::Button, 10.0, 20.0);
        growing.style.grow = 1.0;
        growing.style.max_width = Some(70.0);
        root.children.push(growing);
        root.children.push(fixed(3, Kind::Button, 20.0, 20.0));
        let layout = Layout::compute(
            &root,
            Size {
                width: 200.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.width, 70.0);

        root.children.truncate(1);
        root.children[0].style.width = Some(10.0);
        root.children[0].style.min_width = Some(150.0);
        root.children[0].style.max_width = None;
        root.style.justify = MainAlign::Center;
        let layout = Layout::compute(
            &root,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        let child = layout.get(NodeId(2)).unwrap();
        assert_eq!(child.bounds.x, -25.0);
        assert_eq!(child.bounds.width, 150.0);
        assert_eq!(child.clip.width, 100.0);
        assert_eq!(layout.hit_test(Point { x: 0.0, y: 20.0 }), Some(NodeId(2)));
        assert_eq!(layout.hit_test(Point { x: -1.0, y: 20.0 }), None);
        assert_eq!(layout.hit_test(Point { x: 101.0, y: 20.0 }), None);
    }

    #[test]
    fn fractional_dimensions_follow_the_parent_and_respect_pixel_minimums() {
        let mut root = Node::new(NodeId(1), Kind::Row);
        let mut child = Node::new(NodeId(2), Kind::Row);
        child.style.width_fraction = Some(0.62);
        child.style.height_fraction = Some(0.5);
        child.style.min_width = Some(16.0);
        root.children.push(child);
        let size = Size {
            width: 240.0,
            height: 80.0,
        };

        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        let bounds = layout.get(NodeId(2)).unwrap().bounds;
        assert!((bounds.width - 148.8).abs() < 0.001, "{bounds:?}");
        assert_eq!(bounds.height, 40.0);

        root.children[0].style.width_fraction = Some(0.0);
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        assert_eq!(layout.get(NodeId(2)).unwrap().bounds.width, 16.0);
    }

    #[test]
    fn stack_alignment_keeps_children_absolute_and_scroll_justification_is_safe() {
        let mut stack = Node::new(NodeId(1), Kind::Stack);
        stack.style.align = CrossAlign::End;
        stack.style.justify = MainAlign::End;
        let mut overlay = fixed(2, Kind::Button, 80.0, 40.0);
        overlay.style.max_width = Some(20.0);
        overlay.style.max_height = Some(10.0);
        stack.children.push(overlay);
        let layout = Layout::compute(
            &stack,
            Size {
                width: 100.0,
                height: 60.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(
            layout.get(NodeId(2)).unwrap().bounds,
            Rect {
                x: 80.0,
                y: 50.0,
                width: 20.0,
                height: 10.0,
            }
        );

        stack.style.align = CrossAlign::Center;
        stack.style.justify = MainAlign::End;
        stack.children[0] = fixed(2, Kind::Button, 150.0, 80.0);
        let layout = Layout::compute(
            &stack,
            Size {
                width: 100.0,
                height: 60.0,
            },
            &mut FixedText,
        )
        .unwrap();
        let overlay = layout.get(NodeId(2)).unwrap();
        assert_eq!(overlay.bounds.width, 150.0);
        assert_eq!(overlay.bounds.height, 80.0);
        assert_eq!(overlay.bounds.x, -50.0);
        assert_eq!(overlay.bounds.y, -10.0);
        assert_eq!(overlay.clip.width, 100.0);
        assert_eq!(overlay.clip.height, 60.0);
        assert_eq!(layout.hit_test(Point { x: 0.0, y: 0.0 }), Some(NodeId(2)));
        assert_eq!(layout.hit_test(Point { x: -1.0, y: 0.0 }), None);

        let mut scroll = fixed(3, Kind::Scroll, 100.0, 40.0);
        scroll.children.push(fixed(4, Kind::Button, 100.0, 80.0));
        let point = Point { x: 10.0, y: 10.0 };
        for justify in [
            MainAlign::Center,
            MainAlign::SpaceAround,
            MainAlign::SpaceEvenly,
        ] {
            scroll.style.justify = justify;
            let mut state = ScrollState::default();
            let layout = scrolled(
                &scroll,
                Size {
                    width: 100.0,
                    height: 40.0,
                },
                &mut state,
            );
            assert_eq!(layout.get(NodeId(4)).unwrap().bounds.y, 0.0);
            assert!(layout.scroll_y(point, 100.0, &mut state));
            assert_eq!(state.offset(NodeId(3)), 40.0);
        }
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
    fn transformed_hit_testing_uses_local_geometry_and_exact_ancestor_clips() {
        let mut root = fixed(1, Kind::Row, 100.0, 100.0);
        root.style.align = CrossAlign::Start;
        root.style.justify = MainAlign::Start;
        root.style.rotation = 45.0;
        root.children.push(fixed(2, Kind::Button, 40.0, 40.0));

        let size = Size {
            width: 100.0,
            height: 100.0,
        };
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        let point = layout
            .get(NodeId(2))
            .unwrap()
            .transform
            .point(Point { x: 20.0, y: 20.0 })
            .unwrap();
        assert_eq!(layout.hit_test(point), Some(NodeId(2)));

        root.children[0].style.translate_x = 90.0;
        root.children[0].style.translate_y = 30.0;
        let layout = Layout::compute(&root, size, &mut FixedText).unwrap();
        let child = layout.get(NodeId(2)).unwrap();
        let point = child.transform.point(Point { x: 20.0, y: 20.0 }).unwrap();
        assert!((0.0..100.0).contains(&point.x));
        assert!((0.0..100.0).contains(&point.y));
        let local = layout.surface_to_local(NodeId(2), point).unwrap();
        assert!((local.x - 20.0).abs() < 0.001);
        assert!((local.y - 20.0).abs() < 0.001);
        assert_eq!(layout.hit_test(point), None);
    }

    #[test]
    fn singular_controls_keep_semantic_focus_but_cannot_receive_pointer_input() {
        let mut button = fixed(1, Kind::Button, 80.0, 30.0);
        button.style.scale_x = 0.0;
        let layout = Layout::compute(
            &button,
            Size {
                width: 80.0,
                height: 30.0,
            },
            &mut FixedText,
        )
        .unwrap();

        assert_eq!(layout.focus_order().collect::<Vec<_>>(), [NodeId(1)]);
        assert!(layout.pointer_targets().next().is_none());
        assert_eq!(layout.hit_test(Point { x: 40.0, y: 15.0 }), None);
        assert!(
            layout
                .surface_to_local(NodeId(1), Point::default())
                .is_none()
        );
    }

    #[test]
    fn singular_ancestors_keep_semantic_descendants_outside_the_paint_walk() {
        let mut panel = fixed(1, Kind::Panel, 200.0, 50.0);
        let mut branch = fixed(2, Kind::Row, 160.0, 30.0);
        branch.style.scale_x = 0.0;
        branch.children.push(fixed(3, Kind::TextInput, 160.0, 30.0));
        panel.children.push(branch);

        let layout = Layout::compute(
            &panel,
            Size {
                width: 200.0,
                height: 50.0,
            },
            &mut FixedText,
        )
        .unwrap();

        assert!(layout.get(NodeId(3)).is_none());
        let input = layout.semantic_placement(NodeId(3)).unwrap();
        assert_eq!(input.parent, Some(NodeId(2)));
        assert!(input.enabled);
        assert!(!input.is_invertible());
        assert_eq!(input.scroll_offset, None);
        assert!(layout.focus_order().any(|id| id == NodeId(3)));
    }

    #[test]
    fn renderer_rounded_nested_transform_is_canonical_for_input() {
        let mut leaf = fixed(6, Kind::Button, 10.0, 10.0);
        leaf.style.rotation = -45.0;
        leaf.style.transform_origin.x = 0.0;
        leaf.style.transform_origin.y = 0.0;
        for id in (2..=5).rev() {
            let mut scale = fixed(id, Kind::Row, 10.0, 10.0);
            scale.style.scale_x = 100.0;
            scale.style.scale_y = 0.01;
            scale.style.transform_origin.x = 0.0;
            scale.style.transform_origin.y = 0.0;
            scale.children.push(leaf);
            leaf = scale;
        }
        let mut root = fixed(1, Kind::Panel, 10.0, 10.0);
        root.style.rotation = 45.0;
        root.style.transform_origin.x = 0.0;
        root.style.transform_origin.y = 0.0;
        root.children.push(leaf);

        let layout = Layout::compute(
            &root,
            Size {
                width: 10.0,
                height: 10.0,
            },
            &mut FixedText,
        )
        .unwrap();
        let leaf = layout.semantic_placement(NodeId(6)).unwrap();
        let [xx, xy, _, yx, yy, _] = leaf.transform.skia_values().unwrap();

        assert_eq!(xx * yy - xy * yx, 0.0);
        assert!(!leaf.is_invertible());
        assert!(!layout.pointer_targets().any(|id| id == NodeId(6)));
    }

    #[test]
    fn unrepresentable_nested_transforms_are_rejected_before_layout_publication() {
        let mut branch = fixed(21, Kind::Button, 10.0, 10.0);
        for id in (1..=20).rev() {
            let mut parent = fixed(id, Kind::Row, 10.0, 10.0);
            parent.style.scale_x = 100.0;
            parent.style.scale_y = 100.0;
            parent.style.transform_origin.x = 0.0;
            parent.style.transform_origin.y = 0.0;
            parent.children.push(branch);
            branch = parent;
        }

        let error = Layout::compute(
            &branch,
            Size {
                width: 10.0,
                height: 10.0,
            },
            &mut FixedText,
        )
        .unwrap_err();
        assert!(error.contains("renderer coordinate range"));
    }

    #[test]
    fn scroll_offsets_follow_the_transformed_branch_without_changing_reveal_geometry() {
        let mut root = fixed(1, Kind::Panel, 120.0, 40.0);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 40.0);
        scroll.style.translate_x = 20.0;
        scroll.children.push(fixed(3, Kind::Button, 100.0, 40.0));
        scroll.children.push(fixed(4, Kind::Button, 100.0, 40.0));
        root.children.push(scroll);
        let size = Size {
            width: 120.0,
            height: 40.0,
        };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);
        assert!(layout.scroll_y(Point { x: 25.0, y: 10.0 }, 40.0, &mut state));

        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.get(NodeId(4)).unwrap().bounds.y, 0.0);
        let scroll = layout.semantic_placement(NodeId(2)).unwrap();
        assert_eq!(scroll.scroll_offset, Some(40.0));
        assert_eq!(scroll.max_scroll_offset, Some(40.0));
        assert_eq!(
            layout.semantic_placement(NodeId(4)).unwrap().scroll_parent,
            Some(NodeId(2))
        );
        assert_eq!(layout.hit_test(Point { x: 25.0, y: 10.0 }), Some(NodeId(4)));
        assert_eq!(layout.hit_test(Point { x: 5.0, y: 10.0 }), None);
        assert!(!layout.reveal_y(NodeId(4), &mut state));
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
    fn reveal_uses_the_nearest_vertical_edge_and_is_idempotent() {
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
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);

        assert!(layout.reveal_y(NodeId(5), &mut state));
        assert_eq!(state.offset(NodeId(2)), 50.0);
        assert!(!layout.reveal_y(NodeId(5), &mut state));
        assert!(layout.reveal_y(NodeId(3), &mut state));
        assert_eq!(state.offset(NodeId(2)), 0.0);

        assert!(layout.reveal_y(NodeId(5), &mut state));
        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.get(NodeId(5)).unwrap().bounds.y, 10.0);
        assert!(!layout.reveal_y(NodeId(5), &mut state));

        assert!(layout.reveal_y(NodeId(3), &mut state));
        assert_eq!(state.offset(NodeId(2)), 0.0);
        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.y, 0.0);
    }

    #[test]
    fn reveal_aligns_an_oversized_widget_to_the_viewport_start() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 40.0);
        scroll.children.push(fixed(3, Kind::Button, 100.0, 80.0));
        root.children.push(scroll);
        let size = Size {
            width: 100.0,
            height: 40.0,
        };
        let point = Point { x: 10.0, y: 10.0 };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);
        assert!(layout.scroll_y(point, 25.0, &mut state));
        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.y, -25.0);

        assert!(layout.reveal_y(NodeId(3), &mut state));
        assert_eq!(state.offset(NodeId(2)), 0.0);
        let layout = scrolled(&root, size, &mut state);
        assert_eq!(layout.get(NodeId(3)).unwrap().bounds.y, 0.0);
    }

    #[test]
    fn reveal_updates_every_nested_scroll_ancestor() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut outer = fixed(2, Kind::Scroll, 100.0, 100.0);
        let mut page = fixed(3, Kind::Column, 100.0, 200.0);
        page.children.push(fixed(4, Kind::Row, 100.0, 100.0));
        let mut inner = fixed(5, Kind::Scroll, 100.0, 50.0);
        inner.children.push(fixed(6, Kind::Button, 100.0, 50.0));
        inner.children.push(fixed(7, Kind::Button, 100.0, 50.0));
        page.children.push(inner);
        outer.children.push(page);
        root.children.push(outer);
        let size = Size {
            width: 100.0,
            height: 100.0,
        };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);

        // the inner viewport starts entirely below the outer one, so the
        // ordinary paint layout does not descend to this target yet.
        assert!(layout.get(NodeId(7)).is_none());
        assert!(layout.focus_order().any(|id| id == NodeId(7)));
        assert!(layout.reveal_y(NodeId(7), &mut state));
        assert_eq!(state.offset(NodeId(5)), 50.0);
        assert_eq!(state.offset(NodeId(2)), 50.0);

        let layout = scrolled(&root, size, &mut state);
        let target = layout.get(NodeId(7)).unwrap();
        assert_eq!(target.bounds.y, 50.0);
        assert_eq!(target.bounds.intersection(target.clip), Some(target.bounds));
        assert!(!layout.reveal_y(NodeId(7), &mut state));
    }

    #[test]
    fn nested_reveal_exposes_only_the_inner_visible_slice() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut outer = fixed(2, Kind::Scroll, 100.0, 100.0);
        let mut page = fixed(3, Kind::Column, 100.0, 180.0);
        page.children.push(fixed(4, Kind::Row, 100.0, 100.0));
        let mut inner = fixed(5, Kind::Scroll, 100.0, 40.0);
        inner.children.push(fixed(6, Kind::Button, 100.0, 80.0));
        page.children.push(inner);
        page.children.push(fixed(7, Kind::Row, 100.0, 40.0));
        outer.children.push(page);
        root.children.push(outer);
        let size = Size {
            width: 100.0,
            height: 100.0,
        };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);

        assert!(layout.reveal_y(NodeId(6), &mut state));
        assert_eq!(state.offset(NodeId(5)), 0.0);
        assert_eq!(state.offset(NodeId(2)), 40.0);

        let layout = scrolled(&root, size, &mut state);
        let target = layout.get(NodeId(6)).unwrap();
        assert_eq!(target.bounds.y, 60.0);
        assert_eq!(
            target.bounds.intersection(target.clip).unwrap().height,
            40.0
        );
    }

    #[test]
    fn repeated_reveal_uses_current_scroll_offsets() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 40.0);
        scroll.children.push(fixed(3, Kind::Row, 100.0, 60.0));
        scroll.children.push(fixed(4, Kind::Button, 100.0, 10.0));
        scroll.children.push(fixed(5, Kind::Row, 100.0, 10.0));
        scroll.children.push(fixed(6, Kind::Button, 100.0, 10.0));
        scroll.children.push(fixed(7, Kind::Row, 100.0, 100.0));
        root.children.push(scroll);
        let size = Size {
            width: 100.0,
            height: 40.0,
        };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);

        assert!(layout.reveal_y(NodeId(4), &mut state));
        assert_eq!(state.offset(NodeId(2)), 30.0);
        assert!(!layout.reveal_y(NodeId(4), &mut state));
        assert_eq!(state.offset(NodeId(2)), 30.0);

        assert!(layout.reveal_y(NodeId(6), &mut state));
        assert_eq!(state.offset(NodeId(2)), 50.0);
        assert!(!layout.reveal_y(NodeId(6), &mut state));
        assert_eq!(state.offset(NodeId(2)), 50.0);
    }

    #[test]
    fn reveal_ignores_unavailable_and_zero_height_targets() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut scroll = fixed(2, Kind::Scroll, 100.0, 40.0);
        scroll.children.push(fixed(3, Kind::Button, 100.0, 60.0));
        let mut target = fixed(4, Kind::Button, 100.0, 20.0);
        target.enabled = false;
        scroll.children.push(target);
        root.children.push(scroll);
        let size = Size {
            width: 100.0,
            height: 40.0,
        };
        let mut state = ScrollState::default();
        let layout = scrolled(&root, size, &mut state);
        assert!(!layout.reveal_y(NodeId(4), &mut state));
        assert_eq!(layout.focus_order().collect::<Vec<_>>(), vec![NodeId(3)]);
        assert_eq!(state.offset(NodeId(2)), 0.0);

        root.children[0].children[1].enabled = true;
        root.children[0].children[1].style.height = Some(0.0);
        let layout = scrolled(&root, size, &mut state);
        assert!(!layout.reveal_y(NodeId(4), &mut state));
        assert_eq!(layout.focus_order().collect::<Vec<_>>(), vec![NodeId(3)]);

        root.children[0].children[1].style.height = Some(20.0);
        root.children[0].children[1].visible = false;
        let layout = scrolled(&root, size, &mut state);
        assert!(!layout.reveal_y(NodeId(4), &mut state));
        assert_eq!(layout.focus_order().collect::<Vec<_>>(), vec![NodeId(3)]);

        state.set(NodeId(2), f32::INFINITY);
        assert_eq!(state.offset(NodeId(2)), 0.0);
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

        assert_eq!(
            layout.scroll_route(point),
            [
                ScrollStep::Viewport(NodeId(4)),
                ScrollStep::Viewport(NodeId(2))
            ]
        );
        assert!(!layout.scroll_view_y(NodeId(99), 20.0, &mut state));
        assert!(!layout.scroll_view_y(NodeId(4), f32::NAN, &mut state));
        assert_eq!(state.offset(NodeId(4)), 0.0);
        assert_eq!(state.offset(NodeId(2)), 0.0);

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

        assert_eq!(
            layout.scroll_route(Point { x: 10.0, y: 10.0 }),
            [ScrollStep::Viewport(NodeId(2))]
        );
        assert_eq!(layout.pointer_targets().collect::<Vec<_>>(), [NodeId(2)]);
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

        assert_eq!(
            layout.scroll_route(Point { x: 10.0, y: 10.0 }).first(),
            Some(&ScrollStep::Viewport(NodeId(4)))
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
        let mut handler = fixed(4, Kind::Button, 80.0, 80.0);
        handler.scroll_events = true;
        scroll.children.push(handler);
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
        assert!(layout.scroll_route(overlap).is_empty());
        assert!(!layout.scroll_y(overlap, 20.0, &mut state));
        assert_eq!(state.offset(NodeId(3)), 0.0);
        let visible = Point { x: 15.0, y: 10.0 };
        assert_eq!(
            layout.scroll_route(visible),
            [
                ScrollStep::Handler(NodeId(4)),
                ScrollStep::Viewport(NodeId(3))
            ]
        );
        assert!(layout.scroll_y(visible, 20.0, &mut state));
        assert_eq!(state.offset(NodeId(3)), 20.0);
    }

    fn pill_range(fraction: f32) -> Layout {
        let mut panel = fixed(1, Kind::Panel, 240.0, 36.0);
        let mut range = fixed(2, Kind::Range, 240.0, 36.0);
        let mut track = fixed(3, Kind::Row, 0.0, 16.0);
        track.style.grow = 1.0;

        let mut fill = fixed(4, Kind::Row, 0.0, 16.0);
        fill.style.width = None;
        fill.style.width_fraction = Some(fraction);
        fill.style.min_width = Some(16.0);
        fill.style.padding_x = Some(5.0);
        fill.style.justify = MainAlign::End;
        fill.children.push(fixed(5, Kind::Row, 6.0, 6.0));

        let mut remainder = fixed(6, Kind::Row, 0.0, 16.0);
        remainder.style.grow = 1.0;
        track.children = vec![fill, remainder];
        range.children.push(track);
        panel.children.push(range);
        Layout::compute(
            &panel,
            Size {
                width: 240.0,
                height: 36.0,
            },
            &mut FixedText,
        )
        .unwrap()
    }

    #[test]
    fn custom_range_flex_recreates_a_full_track_fill_and_inset_dot() {
        let layout = pill_range(0.62);
        let track = layout.get(NodeId(3)).unwrap().bounds;
        let fill = layout.get(NodeId(4)).unwrap().bounds;
        let dot = layout.get(NodeId(5)).unwrap().bounds;
        let remainder = layout.get(NodeId(6)).unwrap().bounds;
        assert_eq!(
            track,
            Rect {
                y: 10.0,
                width: 240.0,
                height: 16.0,
                ..Rect::default()
            }
        );
        assert!((fill.width - 148.8).abs() < 0.01, "{fill:?}");
        assert!((remainder.width - 91.2).abs() < 0.01, "{remainder:?}");
        assert!((dot.x + dot.width / 2.0 - (fill.x + fill.width - 8.0)).abs() < 0.01);
        assert_eq!(
            layout.hit_test(Point {
                x: dot.x + 1.0,
                y: 18.0
            }),
            Some(NodeId(2))
        );
        assert_eq!(layout.focus_order().collect::<Vec<_>>(), vec![NodeId(2)]);

        let empty = pill_range(0.0);
        assert!((empty.get(NodeId(4)).unwrap().bounds.width - 16.0).abs() < 0.01);
        assert!((empty.get(NodeId(6)).unwrap().bounds.width - 224.0).abs() < 0.01);
    }

    #[test]
    fn custom_range_flex_supports_a_segmented_meter() {
        let mut panel = fixed(1, Kind::Panel, 180.0, 40.0);
        let mut range = fixed(2, Kind::Range, 180.0, 40.0);
        range.style.padding_x = Some(8.0);
        range.style.padding_y = Some(4.0);
        range.style.gap = 5.0;
        range.style.align = CrossAlign::End;
        for (index, height) in [12.0, 16.0, 20.0, 24.0, 28.0].into_iter().enumerate() {
            let mut segment = fixed(index as u64 + 3, Kind::Row, 0.0, height);
            segment.style.grow = 1.0;
            range.children.push(segment);
        }
        panel.children.push(range);
        let layout = Layout::compute(
            &panel,
            Size {
                width: 180.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();

        for (index, expected_y) in [24.0, 20.0, 16.0, 12.0, 8.0].into_iter().enumerate() {
            let bounds = layout.get(NodeId(index as u64 + 3)).unwrap().bounds;
            assert!((bounds.width - 28.8).abs() < 0.01);
            assert!((bounds.y - expected_y).abs() < 0.01);
        }
        assert_eq!(layout.hit_test(Point { x: 90.0, y: 20.0 }), Some(NodeId(2)));
    }
}
