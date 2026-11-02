//! blur and saturation of an image's own pixels.
//!
//! blur uses a bounded layer; saturation alone filters the ordinary draw.
//! rotated and sheared blurs also budget skia's axis-aligned intermediate,
//! whose bounds can be much larger than the final device clip.

use std::collections::BTreeMap;

use skia_safe::{
    Canvas, ColorFilter, ColorMatrix, Image as SkiaImage, ImageFilter, Matrix, Paint,
    SamplingOptions, TileMode, canvas::SaveLayerRec, color_filters, image_filters,
};
use thing_core::{Kind, Node, NodeId, Rect};

use crate::{Layout, Placement};

const MAX_DEVICE_SIGMA: f32 = 128.0;
const MAX_BLURRED_IMAGES: usize = 256;
const MAX_EFFECT_BYTES: f64 = 512.0 * 1024.0 * 1024.0;
const MAX_FRAME_BYTES: f64 = 1024.0 * 1024.0 * 1024.0;
// the layer, skia's blur intermediates, and its downsampled passes.
const BYTES_PER_PIXEL: f64 = 32.0;

pub fn validate_image_effects(
    root: &Node,
    layout: &Layout,
    physical: (u32, u32),
    scale: f32,
) -> Result<(), String> {
    let nodes = crate::shadow::node_index(root);
    let plan = crate::composite::Plan::prepare(&nodes, layout, physical, scale)?;
    ImageEffectPlan::prepare(&nodes, layout, physical, scale, &plan).map(|_| ())
}

/// the effects that can reach the surface this frame. a non-neutral image
/// without an entry is invisible and is not painted.
#[derive(Default)]
pub(crate) struct ImageEffectPlan {
    nodes: BTreeMap<NodeId, PreparedImageEffect>,
}

#[derive(Clone, Copy)]
pub(crate) struct PreparedImageEffect {
    /// the gaussian sigma in local logical pixels; zero skips the blur.
    sigma: f32,
    saturation: f32,
}

impl ImageEffectPlan {
    pub(crate) fn prepare(
        nodes: &BTreeMap<NodeId, &Node>,
        layout: &Layout,
        physical: (u32, u32),
        scale: f32,
        composites: &crate::composite::Plan,
    ) -> Result<Self, String> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err("image effect output scale must be finite and positive".into());
        }
        let mut result = Self::default();
        let mut budget = Budget::default();
        for placement in layout.placements() {
            let Some(node) = nodes.get(&placement.id) else {
                continue;
            };
            if node.kind != Kind::Image || node.image_effects.is_neutral() {
                continue;
            }
            let prepared = budget
                .charge(
                    node,
                    placement,
                    physical,
                    scale,
                    composites.clip_bounds(layout, placement.id, placement.clip_chain, scale),
                )
                .map_err(|error| format!("image effect on widget {}: {error}", node.id.0))?;
            if let Some(prepared) = prepared {
                result.nodes.insert(node.id, prepared);
            }
        }
        Ok(result)
    }

    pub(crate) fn get(&self, id: NodeId) -> Option<PreparedImageEffect> {
        self.nodes.get(&id).copied()
    }
}

#[derive(Default)]
struct Budget {
    bytes: f64,
    blurred: usize,
}

impl Budget {
    fn charge(
        &mut self,
        node: &Node,
        placement: &Placement,
        physical: (u32, u32),
        scale: f32,
        capture_clip: Option<Rect>,
    ) -> Result<Option<PreparedImageEffect>, String> {
        let image = &node.image;
        if (image.source.is_none()
            && image.memory.is_none()
            && image.icon.is_none()
            && image.app_id.is_none())
            || placement.opacity * placement.disabled_opacity == 0.0
            || !placement.is_invertible()
        {
            return Ok(None);
        }
        let area = crate::skia::image_area(node, placement.local_bounds());
        if area.width <= 0.0 || area.height <= 0.0 {
            return Ok(None);
        }
        let prepared = PreparedImageEffect {
            sigma: node.image_effects.blur / 2.0,
            saturation: node.image_effects.saturation,
        };
        // the f32 matrix skia receives, including the output scale.
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
            return Err("image transform exceeds renderer limits".into());
        }
        // skia blurs each axis of a layer by its own scale factor, so the
        // largest one bounds the device sigma.
        let stretch = matrix.max_scale();
        let device_sigma = prepared.sigma * stretch;
        // skia reports a failed decomposition as a negative scale.
        if stretch.is_nan() || stretch < 0.0 || !device_sigma.is_finite() {
            return Err("image blur exceeds renderer limits".into());
        }
        let local_reach = layer_reach(prepared.sigma);
        let device_pad = (3.0 * f64::from(device_sigma)).ceil() + 1.0;
        let source = outset(area, local_reach);
        let device = placement.transform.rect_aabb(source)?;
        let scale = f64::from(scale);
        let left = f64::from(device.x) * scale;
        let top = f64::from(device.y) * scale;
        let right = (f64::from(device.x) + f64::from(device.width)) * scale;
        let bottom = (f64::from(device.y) + f64::from(device.height)) * scale;
        let clip = capture_clip.unwrap_or_else(|| {
            placement
                .clip
                .intersection(Rect {
                    x: 0.0,
                    y: 0.0,
                    width: physical.0 as f32 / scale as f32,
                    height: physical.1 as f32 / scale as f32,
                })
                .unwrap_or_default()
        });
        let clip_left = f64::from(clip.x) * scale;
        let clip_top = f64::from(clip.y) * scale;
        let clip_right = (f64::from(clip.x) + f64::from(clip.width)) * scale;
        let clip_bottom = (f64::from(clip.y) + f64::from(clip.height)) * scale;
        let visible = |pad: f64| {
            clip_right > clip_left
                && clip_bottom > clip_top
                && right > clip_left - pad
                && bottom > clip_top - pad
                && left < clip_right + pad
                && top < clip_bottom + pad
        };
        if !visible(0.0) {
            return Ok(None);
        }
        if prepared.sigma == 0.0 {
            // a color filter alone changes each pixel in place without a layer.
            return Ok(Some(prepared));
        }
        if device_sigma > MAX_DEVICE_SIGMA {
            return Err("image blur exceeds the 128 pixel device sigma limit".into());
        }
        // reserve a second reach for intermediate passes. a rotated or sheared
        // blur also has a layer in skia's decomposed scale space.
        let width = (right.min(clip_right + device_pad) - left.max(clip_left - device_pad)
            + 2.0 * device_pad)
            .max(0.0);
        let height = (bottom.min(clip_bottom + device_pad) - top.max(clip_top - device_pad)
            + 2.0 * device_pad)
            .max(0.0);
        let layer_pixels = transformed_layer_pixels(
            &matrix,
            source,
            [clip_left, clip_top, clip_right, clip_bottom],
            prepared.sigma,
        )?;
        let bytes = (width * height).max(layer_pixels) * BYTES_PER_PIXEL;
        let surface_bytes = (f64::from(physical.0) + 4.0 * device_pad)
            * (f64::from(physical.1) + 4.0 * device_pad)
            * BYTES_PER_PIXEL;
        let per_effect = surface_bytes.clamp(4.0 * 1024.0 * 1024.0, MAX_EFFECT_BYTES);
        let total = (surface_bytes * 4.0).clamp(16.0 * 1024.0 * 1024.0, MAX_FRAME_BYTES);
        if !bytes.is_finite() || bytes > per_effect {
            return Err("image blur scratch estimate exceeds the per-effect budget".into());
        }
        if self.bytes + bytes > total || self.blurred == MAX_BLURRED_IMAGES {
            return Err("image blur scratch estimates exceed the frame rendering budget".into());
        }
        self.bytes += bytes;
        self.blurred += 1;
        Ok(Some(prepared))
    }
}

fn transformed_layer_pixels(
    matrix: &Matrix,
    source: Rect,
    clip: [f64; 4],
    sigma: f32,
) -> Result<f64, String> {
    if matrix.is_scale_translate() {
        return Ok(0.0);
    }
    let mut remainder = Matrix::new_identity();
    let scales = matrix
        .decompose_scale(Some(&mut remainder))
        .ok_or("image blur transform cannot be safely decomposed")?;
    let inverse = remainder
        .invert()
        .ok_or("image blur transform cannot be safely inverted")?;
    let [left, top, right, bottom] = clip;
    let clip = skia_safe::Rect::from_ltrb(
        (left.floor() as f32).next_down(),
        (top.floor() as f32).next_down(),
        (right.ceil() as f32).next_up(),
        (bottom.ceil() as f32).next_up(),
    );
    let target = inverse.map_rect(clip).0;
    let source = Matrix::scale((scales.width, scales.height))
        .map_rect(crate::skia::to_skia_rect(source))
        .0;
    if !target.is_finite() || !source.is_finite() {
        return Err("image blur layer exceeds renderer limits".into());
    }
    let pad = (3.0 * f64::from(sigma) * f64::from(scales.width.max(scales.height))).ceil() + 2.0;
    // getInputBounds expands the mapped output by the blur kernel before
    // intersecting with the supplied source bounds. round outward, and allow
    // another kernel reach for the blur intermediates. do not rely on skia's
    // emergency downsampling of extreme transforms to meet our budget.
    let width = (f64::from(target.right()).ceil() + pad).min(f64::from(source.right()).ceil())
        - (f64::from(target.left()).floor() - pad).max(f64::from(source.left()).floor());
    let height = (f64::from(target.bottom()).ceil() + pad).min(f64::from(source.bottom()).ceil())
        - (f64::from(target.top()).floor() - pad).max(f64::from(source.top()).floor());
    let pixels = (width.max(0.0) + 2.0 * pad) * (height.max(0.0) + 2.0 * pad);
    if !pixels.is_finite() {
        return Err("image blur layer exceeds renderer limits".into());
    }
    Ok(pixels)
}

/// how far a blur with `sigma` reads and writes beyond its source, in the
/// same units as `sigma`.
fn layer_reach(sigma: f32) -> f32 {
    if sigma == 0.0 {
        0.0
    } else {
        (3.0 * sigma).ceil() + 1.0
    }
}

fn outset(rect: Rect, amount: f32) -> Rect {
    Rect {
        x: rect.x - amount,
        y: rect.y - amount,
        width: rect.width + 2.0 * amount,
        height: rect.height + 2.0 * amount,
    }
}

fn saturation_filter(saturation: f32) -> Option<ColorFilter> {
    if saturation == 1.0 {
        return None;
    }
    // skia's saturation matrix leaves the alpha row unchanged and works on
    // unpremultiplied color, so transparency is preserved.
    let mut matrix = ColorMatrix::default();
    matrix.set_saturation(saturation);
    Some(color_filters::matrix(&matrix, None))
}

fn blur_filter(sigma: f32) -> Result<Option<ImageFilter>, String> {
    if sigma == 0.0 {
        return Ok(None);
    }
    image_filters::blur((sigma, sigma), None, None, None)
        .map(Some)
        .ok_or_else(|| "cannot create image blur filter".into())
}

impl PreparedImageEffect {
    /// draws `image` into `paint_rect` with the effect, clipped to `clip`. `clip` is
    /// the rounded visible box and must already be applied to the canvas.
    pub(crate) fn draw(
        self,
        canvas: &Canvas,
        image: &SkiaImage,
        paint_rect: Rect,
        clip: Rect,
        opacity: f32,
    ) -> Result<(), String> {
        let mut image_paint = crate::skia::fill(skia_safe::Color::WHITE, 1.0);
        image_paint.set_color_filter(saturation_filter(self.saturation));
        let sampling = SamplingOptions::from(skia_safe::FilterMode::Linear);
        let target = crate::skia::to_skia_rect(paint_rect);
        let Some(filter) = blur_filter(self.sigma)? else {
            image_paint.set_alpha_f(opacity);
            canvas.draw_image_rect_with_sampling_options(
                image,
                None,
                target,
                sampling,
                &image_paint,
            );
            return Ok(());
        };
        // only pixels within the blur's reach of the visible box can change
        // the result, so the layer covers that much and no more. the image's
        // edge pixels extend into the reach, so the blur keeps the edges
        // opaque instead of fading them into transparency.
        let bounds = crate::skia::to_skia_rect(outset(clip, layer_reach(self.sigma)));
        let (width, height) = (image.width() as f32, image.height() as f32);
        let (scale_x, scale_y) = (paint_rect.width / width, paint_rect.height / height);
        if !(scale_x.is_finite() && scale_y.is_finite() && scale_x > 0.0 && scale_y > 0.0) {
            return Ok(());
        }
        let placement = Matrix::new_all(
            scale_x,
            0.0,
            paint_rect.x,
            0.0,
            scale_y,
            paint_rect.y,
            0.0,
            0.0,
            1.0,
        );
        let shader = image
            .to_shader(
                Some((TileMode::Clamp, TileMode::Clamp)),
                sampling,
                Some(&placement),
            )
            .ok_or("cannot create image blur source")?;
        image_paint.set_shader(shader);
        let mut layer = Paint::default();
        layer.set_alpha_f(opacity);
        layer.set_image_filter(filter);
        let saved = canvas.save();
        canvas.save_layer(&SaveLayerRec::default().bounds(&bounds).paint(&layer));
        canvas.draw_rect(bounds, &image_paint);
        canvas.restore_to_count(saved);
        Ok(())
    }
}
