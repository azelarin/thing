use std::collections::BTreeMap;

use skia_safe::{
    BlurStyle, Canvas, ClipOp, MaskFilter, Matrix, PaintJoin, PaintStyle, Path, PathBuilder,
    PathOp, RRect,
};
use thing_core::{Kind, Node, NodeId, Rect, Shadow};

use crate::skia::{color, fill, fitted_path, rounded_rect};
use crate::{Layout, SemanticPlacement};

const MAX_DEVICE_SIGMA: f32 = 128.0;
const MAX_PATH_SEGMENTS: usize = 512;
const MAX_FRAME_PATH_SEGMENTS: usize = 4096;
// clipping a path to its rounded content box splits curves where they cross
// it; one input segment rarely becomes more than two.
const MAX_SOURCE_VERBS: usize = 2 * MAX_PATH_SEGMENTS;
// a negative spread subtracts a round-join stroke outline, which has roughly
// thirteen verbs per source curve. its path op dominates preparation time,
// which grows faster than the outline itself, so these caps are lower than a
// full 512 segment source would need.
const MAX_OUTLINE_VERBS: usize = 1024;
const MAX_FRAME_OUTLINE_VERBS: usize = 4096;
const MAX_BLURRED_SHADOWS: usize = 1024;
const MAX_EFFECT_BYTES: f64 = 512.0 * 1024.0 * 1024.0;
const MAX_FRAME_BYTES: f64 = 1024.0 * 1024.0 * 1024.0;
// include the unclipped-cache choice and several rgba-equivalent work buffers.
const BYTES_PER_PIXEL: f64 = 32.0;

pub(crate) fn node_index(root: &Node) -> BTreeMap<NodeId, &Node> {
    fn collect<'a>(node: &'a Node, result: &mut BTreeMap<NodeId, &'a Node>) {
        result.insert(node.id, node);
        for child in &node.children {
            if child.kind != Kind::Popup {
                collect(child, result);
            }
        }
    }
    let mut result = BTreeMap::new();
    collect(root, &mut result);
    result
}

pub fn validate_shadows(
    root: &Node,
    layout: &Layout,
    physical: (u32, u32),
    scale: f32,
) -> Result<(), String> {
    ShadowPlan::prepare(&node_index(root), layout, physical, scale).map(|_| ())
}

#[derive(Default)]
pub(crate) struct ShadowPlan {
    pub(crate) nodes: BTreeMap<NodeId, NodeShadows>,
}

#[derive(Default)]
pub(crate) struct NodeShadows {
    pub(crate) box_shadow: Option<PreparedShadow>,
    pub(crate) path_shadow: Option<PreparedShadow>,
}

pub(crate) struct PreparedShadow {
    source: Shape,
    occluder: Shape,
    value: Shadow,
}

enum Shape {
    Box(RRect),
    Path(Path),
    /// a nonzero path grown by `spread`: its fill together with a round-join
    /// stroke twice as wide. drawing both at once covers the same union a path
    /// op would produce, without the op's cost.
    Grown(Path, f32),
}

impl Shape {
    fn bounds(&self) -> Rect {
        let (bounds, grow) = match self {
            Self::Box(shape) => (*shape.rect(), 0.0),
            Self::Path(shape) => (*shape.bounds(), 0.0),
            Self::Grown(shape, spread) => (*shape.bounds(), *spread),
        };
        Rect {
            x: bounds.left() - grow,
            y: bounds.top() - grow,
            width: bounds.width() + 2.0 * grow,
            height: bounds.height() + 2.0 * grow,
        }
    }
}

impl PreparedShadow {
    pub(crate) fn draw(&self, canvas: &Canvas, opacity: f32) -> Result<(), String> {
        let saved = canvas.save();
        match &self.occluder {
            Shape::Box(shape) => canvas.clip_rrect(shape, ClipOp::Difference, true),
            Shape::Path(shape) | Shape::Grown(shape, _) => {
                canvas.clip_path(shape, ClipOp::Difference, true)
            }
        };
        let mut paint = fill(color(self.value.color), opacity);
        let sigma = self.value.blur / 2.0;
        // halving a positive subnormal blur can round it to zero.
        if sigma > 0.0 {
            let filter = MaskFilter::blur(BlurStyle::Normal, sigma, true)
                .ok_or("cannot create shadow blur filter")?;
            paint.set_mask_filter(filter);
        }
        match &self.source {
            Shape::Box(shape) => canvas.draw_rrect(shape, &paint),
            Shape::Path(shape) => canvas.draw_path(shape, &paint),
            Shape::Grown(shape, spread) => {
                paint.set_style(PaintStyle::StrokeAndFill);
                paint.set_stroke_width(2.0 * spread);
                paint.set_stroke_join(PaintJoin::Round);
                canvas.draw_path(shape, &paint)
            }
        };
        canvas.restore_to_count(saved);
        Ok(())
    }
}

#[derive(Default)]
struct Budget {
    bytes: f64,
    blurred: usize,
    segments: usize,
    outline_verbs: usize,
}

/// the device pixels an effect can touch, after transforms, blur support,
/// and its scroll and surface clips.
struct Reach {
    sigma: f32,
    pad: f64,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    clip_left: f64,
    clip_top: f64,
    clip_right: f64,
    clip_bottom: f64,
}

impl Reach {
    /// measures an effect whose source lies within `local`. it returns `None`
    /// when no part of the effect can reach the visible clip.
    fn measure(
        local: Rect,
        value: Shadow,
        placement: &SemanticPlacement,
        physical: (u32, u32),
        scale: f32,
    ) -> Result<Option<Self>, String> {
        if value.color.0[3] == 0
            || placement.opacity * placement.disabled_opacity == 0.0
            || !placement.is_invertible()
        {
            return Ok(None);
        }
        let [xx, xy, tx, yx, yy, ty] = placement.transform.skia_values()?;
        let matrix = Matrix::new_all(
            xx * scale,
            xy * scale,
            tx * scale,
            yx * scale,
            yy * scale,
            ty * scale,
            0.0,
            0.0,
            1.0,
        );
        if !matrix.is_finite() {
            return Err("shadow transform exceeds renderer limits".into());
        }
        // use the same radius mapping as skia, including nonuniform transforms.
        let sigma = matrix
            .map_radius(value.blur / 2.0)
            .ok_or("shadow blur exceeds renderer limits")?;
        if !sigma.is_finite() {
            return Err("shadow blur exceeds the 128 pixel device sigma limit".into());
        }
        if local.width <= 0.0 || local.height <= 0.0 {
            return Ok(None);
        }
        let device = placement.transform.rect_aabb(local)?;
        let scale = f64::from(scale);
        let pad = (3.0 * f64::from(sigma)).ceil() + 1.0;
        let clip = placement.shadow_clip;
        let left = f64::from(device.x) * scale - pad;
        let top = f64::from(device.y) * scale - pad;
        let right = (f64::from(device.x) + f64::from(device.width)) * scale + pad;
        let bottom = (f64::from(device.y) + f64::from(device.height)) * scale + pad;
        let clip_left = (f64::from(clip.x) * scale).max(0.0);
        let clip_top = (f64::from(clip.y) * scale).max(0.0);
        let clip_right =
            ((f64::from(clip.x) + f64::from(clip.width)) * scale).min(f64::from(physical.0));
        let clip_bottom =
            ((f64::from(clip.y) + f64::from(clip.height)) * scale).min(f64::from(physical.1));
        if clip_right <= clip_left
            || clip_bottom <= clip_top
            || right <= clip_left
            || bottom <= clip_top
            || left >= clip_right
            || top >= clip_bottom
        {
            return Ok(None);
        }
        Ok(Some(Self {
            sigma,
            pad,
            left,
            top,
            right,
            bottom,
            clip_left,
            clip_top,
            clip_right,
            clip_bottom,
        }))
    }
}

impl Budget {
    fn charge(
        &mut self,
        shape: &Shape,
        value: Shadow,
        placement: &SemanticPlacement,
        physical: (u32, u32),
        scale: f32,
    ) -> Result<bool, String> {
        let Some(Reach {
            sigma,
            pad,
            left,
            top,
            right,
            bottom,
            clip_left,
            clip_top,
            clip_right,
            clip_bottom,
        }) = Reach::measure(shape.bounds(), value, placement, physical, scale)?
        else {
            return Ok(false);
        };
        if sigma > MAX_DEVICE_SIGMA {
            return Err("shadow blur exceeds the 128 pixel device sigma limit".into());
        }
        if value.blur == 0.0 {
            return Ok(true);
        }
        // reserve a second halo for blur work beyond the visible result.
        let width = (right.min(clip_right + pad) - left.max(clip_left - pad) + 2.0 * pad).max(0.0);
        let height = (bottom.min(clip_bottom + pad) - top.max(clip_top - pad) + 2.0 * pad).max(0.0);
        let bytes = width * height * BYTES_PER_PIXEL;
        let surface_bytes = (f64::from(physical.0) + 4.0 * pad)
            * (f64::from(physical.1) + 4.0 * pad)
            * BYTES_PER_PIXEL;
        let per_effect = surface_bytes.clamp(4.0 * 1024.0 * 1024.0, MAX_EFFECT_BYTES);
        let total = (surface_bytes * 4.0).clamp(16.0 * 1024.0 * 1024.0, MAX_FRAME_BYTES);
        if !bytes.is_finite()
            || bytes > per_effect
            || self.bytes + bytes > total
            || self.blurred == MAX_BLURRED_SHADOWS
        {
            return Err("shadow scratch estimate exceeds the frame rendering budget".into());
        }
        self.bytes += bytes;
        self.blurred += 1;
        Ok(true)
    }
}

impl ShadowPlan {
    pub(crate) fn prepare(
        nodes: &BTreeMap<NodeId, &Node>,
        layout: &Layout,
        physical: (u32, u32),
        scale: f32,
    ) -> Result<Self, String> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err("shadow output scale must be finite and positive".into());
        }
        let mut result = Self::default();
        let mut budget = Budget::default();
        for placement in layout.semantic_placements() {
            let Some(node) = nodes.get(&placement.id) else {
                continue;
            };
            if placement.opacity * placement.disabled_opacity == 0.0 || !placement.is_invertible() {
                continue;
            }
            let mut shadows = NodeShadows::default();
            let prepare = |budget: &mut Budget,
                           source,
                           occluder,
                           value|
             -> Result<Option<PreparedShadow>, String> {
                budget
                    .charge(&source, value, placement, physical, scale)
                    .map(|visible| {
                        visible.then_some(PreparedShadow {
                            source,
                            occluder,
                            value,
                        })
                    })
                    .map_err(|error| format!("shadow on widget {}: {error}", node.id.0))
            };
            let bounds = placement.local_bounds();
            if let Some(value) = node.style.box_shadow {
                let source = Rect {
                    x: bounds.x - value.spread + value.offset.x,
                    y: bounds.y - value.spread + value.offset.y,
                    width: bounds.width + 2.0 * value.spread,
                    height: bounds.height + 2.0 * value.spread,
                };
                if source.width > 0.0 && source.height > 0.0 {
                    shadows.box_shadow = prepare(
                        &mut budget,
                        Shape::Box(rounded_rect(
                            source,
                            (node.style.radius + value.spread).max(0.0),
                        )),
                        Shape::Box(rounded_rect(bounds, node.style.radius)),
                        value,
                    )?;
                }
            }
            if let Some(value) = node.path.shadow.filter(|value| value.color.0[3] != 0)
                && reaches_surface(
                    path_source_limit(node, bounds, value),
                    value,
                    placement,
                    physical,
                    scale,
                )
                .map_err(|error| format!("shadow on widget {}: {error}", node.id.0))?
            {
                let budget_error = || {
                    format!(
                        "path shadow on widget {} exceeds the path geometry budget",
                        node.id.0
                    )
                };
                if node.path.segments.len() > MAX_PATH_SEGMENTS
                    || budget.segments + node.path.segments.len() > MAX_FRAME_PATH_SEGMENTS
                {
                    return Err(budget_error());
                }
                budget.segments += node.path.segments.len();
                if let Some((path, area)) = fitted_path(node, bounds)? {
                    let clip = Path::rrect(rounded_rect(area, node.style.radius), None);
                    let original = path
                        .op(&clip, PathOp::Intersect)
                        .ok_or("cannot clip the shadow's path source")?;
                    if original.count_verbs() > MAX_SOURCE_VERBS {
                        return Err(budget_error());
                    }
                    let offset = (value.offset.x, value.offset.y);
                    let source = if value.spread > 0.0 && !original.is_empty() {
                        // stroke-and-fill adds the fill under the nonzero rule,
                        // so holes need the opposite winding of their outlines.
                        let winding = original
                            .as_winding()
                            .ok_or("cannot spread the shadow's path source")?;
                        Shape::Grown(winding.with_offset(offset), value.spread)
                    } else if value.spread < 0.0 && !original.is_empty() {
                        let outline = stroke_outline(&original, value.spread)?;
                        let verbs = outline.count_verbs();
                        if verbs > MAX_OUTLINE_VERBS
                            || budget.outline_verbs + verbs > MAX_FRAME_OUTLINE_VERBS
                        {
                            return Err(budget_error());
                        }
                        budget.outline_verbs += verbs;
                        let shrunk = original
                            .op(&outline, PathOp::Difference)
                            .ok_or("cannot spread the shadow's path source")?;
                        Shape::Path(shrunk.with_offset(offset))
                    } else {
                        Shape::Path(original.with_offset(offset))
                    };
                    shadows.path_shadow =
                        prepare(&mut budget, source, Shape::Path(original), value)?;
                }
            }
            if shadows.box_shadow.is_some() || shadows.path_shadow.is_some() {
                result.nodes.insert(node.id, shadows);
            }
        }
        Ok(result)
    }
}

/// a region that contains a path shadow's source in local coordinates. the
/// source never leaves the content box, a positive spread grows it, and the
/// offset moves it. this needs no path work, so it can cull an effect first.
fn path_source_limit(node: &Node, bounds: Rect, value: Shadow) -> Rect {
    let area = crate::brush::path_area(bounds, &node.style);
    let grow = value.spread.max(0.0);
    Rect {
        x: area.x - grow + value.offset.x,
        y: area.y - grow + value.offset.y,
        width: area.width + 2.0 * grow,
        height: area.height + 2.0 * grow,
    }
}

fn reaches_surface(
    local: Rect,
    value: Shadow,
    placement: &SemanticPlacement,
    physical: (u32, u32),
    scale: f32,
) -> Result<bool, String> {
    Reach::measure(local, value, placement, physical, scale).map(|reach| reach.is_some())
}

/// the round-join stroke whose removal shrinks a path by `spread`.
fn stroke_outline(path: &Path, spread: f32) -> Result<Path, String> {
    let mut paint = skia_safe::Paint::default();
    paint.set_style(PaintStyle::Stroke);
    paint.set_stroke_width(spread.abs() * 2.0);
    paint.set_stroke_join(PaintJoin::Round);
    let mut outline = PathBuilder::new();
    if !skia_safe::path_utils::fill_path_with_paint(path, &paint, &mut outline, None, None) {
        return Err("cannot spread the shadow's path source".into());
    }
    Ok(outline.detach())
}
