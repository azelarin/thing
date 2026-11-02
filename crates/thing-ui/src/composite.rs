//! isolated opacity for bounded widget subtrees.

use std::collections::BTreeMap;

use skia_safe::{Canvas, ClipOp, M44, Rect as SkiaRect, canvas::SaveLayerRec};
use thing_core::{Node, NodeId};

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
) -> Result<(), String> {
    fn requested(node: &Node) -> bool {
        node.style.composite
            || node
                .children
                .iter()
                .filter(|child| child.kind != thing_core::Kind::Popup)
                .any(requested)
    }
    if !requested(root) {
        return Ok(());
    }
    Plan::prepare(&crate::shadow::node_index(root), layout, physical).map(|_| ())
}

struct Layer {
    end: usize,
    opacity: f32,
    base: f32,
}

#[derive(Default)]
pub(crate) struct Plan {
    layers: BTreeMap<usize, Layer>,
}

impl Plan {
    pub(crate) fn prepare(
        nodes: &BTreeMap<NodeId, &Node>,
        layout: &Layout,
        physical: (u32, u32),
    ) -> Result<Self, String> {
        let mut result = Self::default();
        if !nodes.values().any(|node| node.style.composite) {
            return Ok(result);
        }
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
        let mut active: Vec<(usize, f32)> = Vec::new();
        let mut bytes = 0_u64;
        for (index, placement) in placements.iter().enumerate() {
            while active.last().is_some_and(|(end, _)| *end <= index) {
                active.pop();
            }
            if !nodes
                .get(&placement.id)
                .is_some_and(|node| node.style.composite)
            {
                continue;
            }
            let base = placement.opacity * placement.disabled_opacity;
            if !base.is_finite() || !(0.0..=1.0).contains(&base) {
                return Err("composited opacity must be finite and between 0 and 1".into());
            }
            if base == 0.0 {
                continue;
            }
            let parent = active.last().map_or(1.0, |(_, opacity)| *opacity);
            let opacity = (base / parent).clamp(0.0, 1.0);
            // an opaque group of source-over draws needs no temporary layer.
            if opacity == 1.0 {
                continue;
            }
            if physical.0 == 0
                || physical.1 == 0
                || physical.0 > MAX_LAYER_SIDE
                || physical.1 > MAX_LAYER_SIDE
            {
                return Err(
                    "composited surfaces must be between 1 and 16384 physical pixels per side"
                        .into(),
                );
            }
            let layer_bytes = u64::from(physical.0)
                .checked_mul(u64::from(physical.1))
                .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
                .ok_or("compositing scratch estimate exceeds renderer limits")?;
            bytes = bytes
                .checked_add(layer_bytes)
                .ok_or("compositing scratch estimate exceeds renderer limits")?;
            if layer_bytes > MAX_LAYER_BYTES
                || bytes > MAX_FRAME_BYTES
                || result.layers.len() == MAX_LAYERS
                || active.len() == MAX_NESTING
            {
                return Err(
                    "compositing scratch estimate exceeds the frame rendering budget".into(),
                );
            }
            result.layers.insert(
                index,
                Layer {
                    end: ends[index],
                    opacity,
                    base,
                },
            );
            active.push((ends[index], base));
        }
        Ok(result)
    }
}

struct ActiveLayer {
    end: usize,
    restore: usize,
    base: f32,
}

pub(crate) struct Layers {
    plan: Plan,
    active: Vec<ActiveLayer>,
    physical: SkiaRect,
}

impl Layers {
    pub(crate) fn new(plan: Plan, physical: (u32, u32)) -> Self {
        Self {
            plan,
            active: Vec::new(),
            physical: SkiaRect::from_wh(physical.0 as f32, physical.1 as f32),
        }
    }

    pub(crate) fn before(
        &mut self,
        index: usize,
        canvas: &Canvas,
        clips: &mut Vec<usize>,
        matrix: &M44,
    ) {
        while self.active.last().is_some_and(|layer| layer.end <= index) {
            self.close(canvas, clips);
        }
        let Some(layer) = self.plan.layers.get(&index) else {
            return;
        };
        // clip saves belong to the innermost layer. discard them before a
        // layer boundary so clip reconciliation cannot restore a layer.
        clear_clips(canvas, clips);
        let restore = canvas.save();
        canvas.reset_matrix();
        canvas.clip_rect(self.physical, ClipOp::Intersect, false);
        canvas.set_matrix(matrix);
        let paint = fill(skia_safe::Color::WHITE, layer.opacity);
        canvas.save_layer(&SaveLayerRec::default().paint(&paint));
        self.active.push(ActiveLayer {
            end: layer.end,
            restore,
            base: layer.base,
        });
    }

    pub(crate) fn opacity(&self, inherited: f32) -> f32 {
        self.active
            .last()
            .map_or(inherited, |layer| (inherited / layer.base).clamp(0.0, 1.0))
    }

    fn close(&mut self, canvas: &Canvas, clips: &mut Vec<usize>) {
        clear_clips(canvas, clips);
        let layer = self.active.pop().expect("only active layers close");
        canvas.restore_to_count(layer.restore);
    }

    pub(crate) fn finish(&mut self, canvas: &Canvas, clips: &mut Vec<usize>) {
        while !self.active.is_empty() {
            self.close(canvas, clips);
        }
    }
}

fn clear_clips(canvas: &Canvas, clips: &mut Vec<usize>) {
    while clips.pop().is_some() {
        canvas.restore();
    }
}
