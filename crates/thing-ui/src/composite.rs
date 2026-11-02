//! bounded layers for group opacity, crossfades, and pixel-alpha shadows.

use std::collections::{BTreeMap, BTreeSet};

use skia_safe::{
    BlendMode, Canvas, ClipOp, IRect, ImageFilter, M44, Matrix, Rect as SkiaRect,
    canvas::SaveLayerRec, image_filter::MapDirection, image_filters,
};
use thing_core::{Kind, Node, NodeId, Rect};

use crate::{Layout, skia::fill};

const MAX_LAYER_SIDE: u32 = 16_384;
const MAX_LAYER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FRAME_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_LAYERS: usize = 256;
const MAX_NESTING: usize = 8;
// allow four rgba-equivalent buffers; bounds hints do not limit allocation.
const BYTES_PER_PIXEL: u64 = 16;

pub fn validate_compositing(
    root: &Node,
    layout: &Layout,
    physical: (u32, u32),
    scale: f32,
) -> Result<(), String> {
    fn requested(node: &Node) -> bool {
        (node.style.composite || node.style.drop_shadow.is_some() || node.kind == Kind::Crossfade)
            || node
                .children
                .iter()
                .filter(|child| child.kind != thing_core::Kind::Popup)
                .any(requested)
    }
    if !requested(root) {
        return Ok(());
    }
    Plan::prepare(&crate::shadow::node_index(root), layout, physical, scale).map(|_| ())
}

/// checks all scratch-backed effects with the same subtree capture bounds.
pub fn validate_effects(
    root: &Node,
    layout: &Layout,
    physical: (u32, u32),
    scale: f32,
) -> Result<(), String> {
    let nodes = crate::shadow::node_index(root);
    let plan = Plan::prepare(&nodes, layout, physical, scale)?;
    crate::shadow::ShadowPlan::prepare(&nodes, layout, physical, scale, &plan)?;
    crate::image_effects::ImageEffectPlan::prepare(&nodes, layout, physical, scale, &plan)?;
    Ok(())
}

struct Layer {
    end: usize,
    opacity: f32,
    base: f32,
    blend: BlendMode,
    shadow: Option<DropShadow>,
    output: IRect,
}

#[derive(Default)]
pub(crate) struct Plan {
    layers: BTreeMap<usize, Vec<Layer>>,
    captures: BTreeMap<NodeId, Capture>,
}

impl Plan {
    pub(crate) fn prepare(
        nodes: &BTreeMap<NodeId, &Node>,
        layout: &Layout,
        physical: (u32, u32),
        scale: f32,
    ) -> Result<Self, String> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err("compositing output scale must be finite and positive".into());
        }
        let mut result = Self::default();
        if !nodes.values().any(|node| {
            node.style.composite || node.style.drop_shadow.is_some() || node.kind == Kind::Crossfade
        }) {
            return Ok(result);
        }
        let physical = IRect::from_wh(
            i32::try_from(physical.0).map_err(|_| "composited surface exceeds renderer limits")?,
            i32::try_from(physical.1).map_err(|_| "composited surface exceeds renderer limits")?,
        );
        let placements = layout.semantic_placements();
        let mut ends = vec![placements.len(); placements.len()];
        let mut ancestors: Vec<usize> = Vec::new();
        for (index, placement) in placements.iter().enumerate() {
            while let Some(&parent) = ancestors.last() {
                if Some(placements[parent].id) == placement.parent {
                    break;
                }
                ends[ancestors.pop().expect("the ancestor was present")] = index;
            }
            ancestors.push(index);
        }
        let indices: BTreeMap<_, _> = placements
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id, i))
            .collect();
        let mut scheduled: BTreeMap<usize, Vec<Layer>> = BTreeMap::new();
        for (index, placement) in placements.iter().enumerate() {
            let Some(node) = nodes
                .get(&placement.id)
                .filter(|node| node.kind == Kind::Crossfade)
            else {
                continue;
            };
            if !node.crossfade_progress.is_finite()
                || !(0.0..=1.0).contains(&node.crossfade_progress)
            {
                return Err("crossfade progress must be between 0 and 1".into());
            }
            // endpoints draw the one active child directly, preserving its
            // pixels without an extra round trip through a temporary layer.
            if node.crossfade_progress == 0.0 || node.crossfade_progress == 1.0 {
                continue;
            }
            let base = placement.opacity * placement.disabled_opacity;
            if base == 0.0 {
                continue;
            }
            let mut first = true;
            for (slot, child) in node
                .children
                .iter()
                .filter(|child| child.kind != Kind::Popup)
                .enumerate()
            {
                let Some(&child_index) = indices.get(&child.id) else {
                    continue;
                };
                let weight = node.child_weight(slot);
                if base * weight == 0.0 {
                    continue;
                }
                let layers = scheduled.entry(child_index).or_default();
                if first {
                    // the widget's own background and decorations have already
                    // painted. only its children enter the transparent sum.
                    layers.push(Layer {
                        end: ends[index],
                        opacity: 1.0,
                        base,
                        blend: BlendMode::SrcOver,
                        shadow: None,
                        output: physical,
                    });
                    first = false;
                }
                // layout includes the weight so invisible effects are culled.
                // remove it while painting this child, then weight its complete
                // source image once when it is added to the transparent sum.
                layers.push(Layer {
                    end: ends[child_index],
                    opacity: weight,
                    base: base * weight,
                    blend: BlendMode::Plus,
                    shadow: None,
                    output: physical,
                });
            }
        }
        let mut active: Vec<(usize, f32, Capture)> = Vec::new();
        let mut budget = Budget::default();
        for (index, placement) in placements.iter().enumerate() {
            while active.last().is_some_and(|(end, _, _)| *end <= index) {
                active.pop();
            }
            for layer in scheduled.remove(&index).unwrap_or_default() {
                result.add(index, layer, &mut active, &mut budget, physical)?;
            }
            if let Some(node) = nodes.get(&placement.id) {
                let base = placement.opacity * placement.disabled_opacity;
                if !base.is_finite() || !(0.0..=1.0).contains(&base) {
                    return Err("composited opacity must be finite and between 0 and 1".into());
                }
                if base > 0.0 {
                    let shadow = DropShadow::prepare(node, placement, layout, scale)?;
                    if node.style.composite || node.kind == Kind::Crossfade || shadow.is_some() {
                        let parent = active.last().map_or(1.0, |(_, opacity, _)| *opacity);
                        let opacity = (base / parent).clamp(0.0, 1.0);
                        if opacity != 1.0 || shadow.is_some() {
                            result.add(
                                index,
                                Layer {
                                    end: ends[index],
                                    opacity,
                                    base,
                                    blend: BlendMode::SrcOver,
                                    shadow,
                                    output: physical,
                                },
                                &mut active,
                                &mut budget,
                                physical,
                            )?;
                        }
                    }
                }
            }
            if let Some((_, _, capture)) = active.last().filter(|(_, _, capture)| capture.filtered)
            {
                result.captures.insert(placement.id, capture.clone());
            }
        }
        Ok(result)
    }

    fn add(
        &mut self,
        index: usize,
        mut layer: Layer,
        active: &mut Vec<(usize, f32, Capture)>,
        budget: &mut Budget,
        physical: IRect,
    ) -> Result<(), String> {
        let mut capture = active.last().map_or_else(
            || Capture {
                bounds: physical,
                excluded: BTreeSet::new(),
                filtered: false,
            },
            |(_, _, capture)| capture.clone(),
        );
        layer.output = capture.bounds;
        if let Some(shadow) = &layer.shadow {
            capture.bounds = shadow.input_bounds(capture.bounds)?;
            capture.excluded = shadow.excluded.clone();
            capture.filtered = true;
        }
        budget.charge(capture.bounds, active.len(), layer.shadow.is_some())?;
        active.push((layer.end, layer.base, capture));
        self.layers.entry(index).or_default().push(layer);
        Ok(())
    }

    pub(crate) fn clip_bounds(
        &self,
        layout: &Layout,
        id: NodeId,
        chain: usize,
        scale: f32,
    ) -> Option<Rect> {
        let capture = self.captures.get(&id)?;
        let scale = f64::from(scale);
        let mut clip = Rect {
            x: (f64::from(capture.bounds.left) / scale) as f32,
            y: (f64::from(capture.bounds.top) / scale) as f32,
            width: (capture.bounds.width() as f64 / scale) as f32,
            height: (capture.bounds.height() as f64 / scale) as f32,
        };
        let mut cursor = Some(chain);
        while let Some(index) = cursor {
            let region = layout.clip_region(index)?;
            if !capture.excluded.contains(&region.owner) {
                clip = clip.intersection(region.aabb).unwrap_or_default();
            }
            cursor = region.parent;
        }
        Some(clip)
    }
}

#[derive(Clone)]
struct Capture {
    bounds: IRect,
    excluded: BTreeSet<Option<NodeId>>,
    filtered: bool,
}

struct DropShadow {
    filter: ImageFilter,
    clip: usize,
    excluded: BTreeSet<Option<NodeId>>,
}

impl DropShadow {
    fn prepare(
        node: &Node,
        placement: &crate::SemanticPlacement,
        layout: &Layout,
        scale: f32,
    ) -> Result<Option<Self>, String> {
        let Some(value) = node.style.drop_shadow.filter(|value| value.color.0[3] != 0) else {
            return Ok(None);
        };
        value.validate().map_err(|error| error.to_string())?;
        if value.spread != 0.0 {
            return Err("drop shadows do not support spread".into());
        }
        if !placement.is_invertible() {
            return Ok(None);
        }
        let [xx, xy, _, yx, yy, _] = placement.transform.skia_values()?;
        let matrix = Matrix::new_all(
            xx * scale,
            xy * scale,
            0.0,
            yx * scale,
            yy * scale,
            0.0,
            0.0,
            0.0,
            1.0,
        );
        let sigma = matrix
            .map_radius(value.blur / 2.0)
            .ok_or("drop shadow transform exceeds renderer limits")?;
        if !sigma.is_finite() || sigma > 128.0 {
            return Err("drop shadow exceeds the 128 pixel device sigma limit".into());
        }
        let offset = (
            (xx * value.offset.x + xy * value.offset.y) * scale,
            (yx * value.offset.x + yy * value.offset.y) * scale,
        );
        if !offset.0.is_finite() || !offset.1.is_finite() {
            return Err("drop shadow offset exceeds renderer limits".into());
        }
        let filter = image_filters::drop_shadow(
            offset,
            (sigma, sigma),
            crate::skia::color(value.color),
            None,
            None,
            None,
        )
        .ok_or("cannot create drop shadow filter")?;
        let mut excluded = BTreeSet::from([None]);
        let mut cursor = placement.parent;
        while let Some(id) = cursor {
            excluded.insert(Some(id));
            cursor = layout
                .semantic_placement(id)
                .and_then(|parent| parent.parent);
        }
        Ok(Some(Self {
            filter,
            clip: placement.shadow_clip_chain,
            excluded,
        }))
    }

    fn input_bounds(&self, output: IRect) -> Result<IRect, String> {
        // with an identity layer matrix, this is skia's device-space input
        // reach. reserve its padding as well; a bounds hint is not a cap.
        let input =
            self.filter
                .filter_bounds(output, &Matrix::new_identity(), MapDirection::Reverse, None);
        let left = i64::from(input.left.min(output.left)) - 2;
        let top = i64::from(input.top.min(output.top)) - 2;
        let right = i64::from(input.right.max(output.right)) + 2;
        let bottom = i64::from(input.bottom.max(output.bottom)) + 2;
        if right - left > i64::from(MAX_LAYER_SIDE) || bottom - top > i64::from(MAX_LAYER_SIDE) {
            return Err("drop shadow input exceeds renderer limits".into());
        }
        Ok(IRect::new(
            i32::try_from(left).map_err(|_| "drop shadow input exceeds renderer limits")?,
            i32::try_from(top).map_err(|_| "drop shadow input exceeds renderer limits")?,
            i32::try_from(right).map_err(|_| "drop shadow input exceeds renderer limits")?,
            i32::try_from(bottom).map_err(|_| "drop shadow input exceeds renderer limits")?,
        ))
    }
}

#[derive(Default)]
struct Budget {
    bytes: u64,
    layers: usize,
}

impl Budget {
    fn charge(&mut self, bounds: IRect, depth: usize, filtered: bool) -> Result<(), String> {
        let physical = (bounds.width() as u32, bounds.height() as u32);
        if physical.0 == 0
            || physical.1 == 0
            || physical.0 > MAX_LAYER_SIDE
            || physical.1 > MAX_LAYER_SIDE
        {
            return Err(
                "composited surfaces must be between 1 and 16384 physical pixels per side".into(),
            );
        }
        let layer_bytes = u64::from(physical.0)
            .checked_mul(u64::from(physical.1))
            .and_then(|pixels| {
                pixels.checked_mul(if filtered {
                    2 * BYTES_PER_PIXEL
                } else {
                    BYTES_PER_PIXEL
                })
            })
            .ok_or("compositing scratch estimate exceeds renderer limits")?;
        self.bytes = self
            .bytes
            .checked_add(layer_bytes)
            .ok_or("compositing scratch estimate exceeds renderer limits")?;
        if layer_bytes > MAX_LAYER_BYTES
            || self.bytes > MAX_FRAME_BYTES
            || self.layers == MAX_LAYERS
            || depth == MAX_NESTING
        {
            return Err("compositing scratch estimate exceeds the frame rendering budget".into());
        }
        self.layers += 1;
        Ok(())
    }
}

struct ActiveLayer {
    end: usize,
    restore: usize,
    base: f32,
    excluded: BTreeSet<Option<NodeId>>,
}

pub(crate) struct Layers {
    plan: Plan,
    active: Vec<ActiveLayer>,
}

impl Layers {
    pub(crate) fn new(plan: Plan) -> Self {
        Self {
            plan,
            active: Vec::new(),
        }
    }

    pub(crate) fn before(
        &mut self,
        index: usize,
        canvas: &Canvas,
        layout: &Layout,
        clips: &mut crate::skia::Clips,
        matrix: &M44,
    ) -> Result<(), String> {
        while self.active.last().is_some_and(|layer| layer.end <= index) {
            self.close(canvas, clips);
        }
        let Some(layers) = self.plan.layers.get(&index) else {
            return Ok(());
        };
        for layer in layers {
            // clip saves belong to the innermost layer. discard them before a
            // layer boundary so clip reconciliation cannot restore a layer.
            clips.clear(canvas);
            let restore = canvas.save();
            canvas.reset_matrix();
            canvas.clip_rect(SkiaRect::from(layer.output), ClipOp::Intersect, false);
            canvas.set_matrix(matrix);
            if let Some(shadow) = &layer.shadow {
                let mut result_clips = crate::skia::Clips::default();
                result_clips.excluded = clips.excluded.clone();
                crate::skia::sync_clip_chain(
                    canvas,
                    layout,
                    shadow.clip,
                    matrix,
                    &mut result_clips,
                )?;
            }
            canvas.reset_matrix();
            let mut paint = fill(skia_safe::Color::WHITE, layer.opacity);
            paint.set_blend_mode(layer.blend);
            if let Some(shadow) = &layer.shadow {
                paint.set_image_filter(shadow.filter.clone());
                clips.excluded = shadow.excluded.clone();
            }
            canvas.save_layer(&SaveLayerRec::default().paint(&paint));
            canvas.set_matrix(matrix);
            self.active.push(ActiveLayer {
                end: layer.end,
                restore,
                base: layer.base,
                excluded: clips.excluded.clone(),
            });
        }
        Ok(())
    }

    pub(crate) fn opacity(&self, inherited: f32) -> f32 {
        self.active
            .last()
            .map_or(inherited, |layer| (inherited / layer.base).clamp(0.0, 1.0))
    }

    fn close(&mut self, canvas: &Canvas, clips: &mut crate::skia::Clips) {
        clips.clear(canvas);
        let layer = self.active.pop().expect("only active layers close");
        canvas.restore_to_count(layer.restore);
        clips.excluded = self
            .active
            .last()
            .map_or_else(BTreeSet::new, |layer| layer.excluded.clone());
    }

    pub(crate) fn finish(&mut self, canvas: &Canvas, clips: &mut crate::skia::Clips) {
        while !self.active.is_empty() {
            self.close(canvas, clips);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Size, TextSystem};
    use skia_safe::Contains;
    use thing_core::{Color, Point, Shadow};

    #[test]
    fn filtered_device_clips_fit_the_planned_input_at_every_layer() {
        for (blur, offset, scale) in [(0.0, -40.0, 1.0), (12.0, 30.5, 1.25), (32.0, -100.0, 2.0)] {
            let mut root = Node::new(NodeId(1), Kind::Stack);
            let mut child = Node::new(NodeId(2), Kind::Stack);
            root.style.drop_shadow = Some(Shadow {
                color: Color([0, 0, 0, 255]),
                blur,
                offset: Point {
                    x: offset,
                    y: offset,
                },
                spread: 0.0,
            });
            child.style.drop_shadow = root.style.drop_shadow;
            child.style.width = Some(50.0);
            child.style.height = Some(30.0);
            child.style.opacity = 0.5;
            root.children.push(child);
            let physical = (160, 100);
            let layout = Layout::compute(
                &root,
                Size {
                    width: 160.0 / scale,
                    height: 100.0 / scale,
                },
                &mut TextSystem::default(),
            )
            .unwrap();
            let plan =
                Plan::prepare(&crate::shadow::node_index(&root), &layout, physical, scale).unwrap();
            let expected = plan.captures.clone();
            let mut layers = Layers::new(plan);
            let mut surface = skia_safe::surfaces::raster_n32_premul((160, 100)).unwrap();
            let canvas = surface.canvas();
            canvas.scale((scale, scale));
            let matrix = canvas.local_to_device();
            let mut clips = crate::skia::Clips::default();
            for (index, placement) in layout.semantic_placements().iter().enumerate() {
                layers
                    .before(index, canvas, &layout, &mut clips, &matrix)
                    .unwrap();
                let actual = canvas.device_clip_bounds().unwrap();
                let expected = expected[&placement.id].bounds;
                assert!(
                    expected.contains(actual),
                    "{blur},{offset},{scale}: {actual:?} exceeds {expected:?}"
                );
            }
            layers.finish(canvas, &mut clips);
            assert_eq!(canvas.save_count(), 1);
        }
    }
}
