use std::collections::BTreeMap;

use femtovg::{
    Canvas, Color, FillRule, FontId, LineCap, LineJoin, Paint, Path, PositionedGlyph, TextContext,
    renderer::{OpenGl, Renderer},
};
use parley::PositionedLayoutItem;
use thing_core::{
    HorizontalRange, ImageFit, Kind, Node, NodeId, PathFillRule, PathFit, PathStrokeCap,
    PathStrokeJoin, Rect, VectorSegment, ViewBox,
};

use crate::{Layout, ScrollState, Size, TextSystem};

/// transient input state belongs to the host, not to the widget snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct Interaction {
    pub hovered: Option<NodeId>,
    pub pressed: Option<NodeId>,
    pub focused: Option<NodeId>,
    focus_visible: bool,
}

impl Interaction {
    pub fn focus_from_pointer(&mut self, focused: Option<NodeId>) {
        self.focused = focused;
        self.focus_visible = false;
    }

    pub fn focus_from_keyboard(&mut self, focused: NodeId) {
        self.focused = Some(focused);
        self.focus_visible = true;
    }

    pub fn reveal_keyboard_focus(&mut self) {
        self.focus_visible = self.focused.is_some();
    }

    pub fn keyboard_focused(&self) -> Option<NodeId> {
        if self.focus_visible {
            self.focused
        } else {
            None
        }
    }
}

/// renderer-independent inputs for one frame.
pub struct PaintFrame<'a> {
    pub root: &'a Node,
    pub layout: &'a Layout,
    pub physical: (u32, u32),
    pub scale: f32,
    pub input: Interaction,
}

/// renders a widget tree through femtovg.
pub struct Painter<R: Renderer = OpenGl> {
    canvas: Canvas<R>,
    text_context: TextContext,
    fonts: BTreeMap<(u64, u32), FontId>,
    pub text: TextSystem,
    images: crate::images::Images,
}

impl<R: Renderer> Painter<R> {
    pub fn new(renderer: R, wake: impl Fn() + Send + 'static) -> Result<Self, String> {
        let text_context = TextContext::default();
        let canvas = Canvas::new_with_text_context(renderer, text_context.clone())
            .map_err(|e| e.to_string())?;
        Ok(Self {
            canvas,
            text_context,
            fonts: BTreeMap::new(),
            text: TextSystem::default(),
            images: crate::images::Images::new(wake)?,
        })
    }

    /// records and submits one frame.
    pub fn paint_to_output(
        &mut self,
        frame: PaintFrame<'_>,
        output: impl Into<R::RenderOutput>,
        submit: impl FnOnce(R::CommandBuffer),
    ) -> Result<(), String> {
        let result = self.draw(frame);
        self.finish_output(result, output, submit)
    }

    fn draw(&mut self, frame: PaintFrame<'_>) -> Result<(), String> {
        let PaintFrame {
            root,
            layout,
            physical,
            scale,
            input,
        } = frame;
        self.canvas.set_size(physical.0, physical.1, scale);
        self.images.collect(&mut self.canvas);
        self.canvas
            .clear_rect(0, 0, physical.0, physical.1, Color::rgba(0, 0, 0, 0));
        // framebuffer dimensions are physical pixels; layout and hit testing
        // use logical pixels. set_size's dpi argument does not scale geometry.
        self.canvas.reset_transform();
        self.canvas.scale(scale, scale);
        self.text.retain(root);
        for placement in layout.placements() {
            let Some(node) = root.find(placement.id) else {
                continue;
            };
            let bounds = placement.bounds;
            let clip = placement.clip;
            self.canvas.scissor(clip.x, clip.y, clip.width, clip.height);
            self.canvas
                .set_global_alpha(placement.opacity * placement.disabled_opacity);
            let mut path = Path::new();
            path.rounded_rect(
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                node.style.radius,
            );
            self.canvas
                .fill_path(&path, &Paint::color(color(node.style.background)));
            if node.style.border_width > 0.0 {
                let width = node.style.border_width.min(bounds.width).min(bounds.height);
                let mut border = Path::new();
                border.rounded_rect(
                    bounds.x + width / 2.0,
                    bounds.y + width / 2.0,
                    (bounds.width - width).max(0.0),
                    (bounds.height - width).max(0.0),
                    (node.style.radius - width / 2.0).max(0.0),
                );
                let mut paint = Paint::color(color(node.style.border_color));
                paint.set_line_width(width);
                self.canvas.stroke_path(&border, &paint);
            }
            if let Some(overlay) = interaction_overlay(node, input) {
                self.canvas.fill_path(&path, &Paint::color(color(overlay)));
            }
            if let Some((focus_color, focus_width)) = focus_ring(node, input) {
                let mut focus = Paint::color(color(focus_color));
                focus.set_line_width(focus_width);
                self.canvas.stroke_path(&path, &focus);
            }
            match node.kind {
                Kind::Text => self.draw_text(node, bounds, clip)?,
                Kind::Slider => self.draw_slider(node, bounds),
                Kind::Image => self.draw_image(node, bounds, scale),
                Kind::Path => self.draw_path(node, bounds)?,
                _ => {}
            }
        }
        self.canvas.reset_scissor();
        Ok(())
    }

    fn finish_output(
        &mut self,
        result: Result<(), String>,
        output: impl Into<R::RenderOutput>,
        submit: impl FnOnce(R::CommandBuffer),
    ) -> Result<(), String> {
        let commands = self.canvas.flush_to_output(output);
        submit(commands);
        self.images.release(&mut self.canvas);
        result
    }

    pub fn layout(
        &mut self,
        root: &Node,
        size: Size,
        scroll: &mut ScrollState,
    ) -> Result<Layout, String> {
        Layout::compute_with_scroll(root, size, scroll, &mut self.text)
    }

    fn draw_text(&mut self, node: &Node, bounds: Rect, inherited_clip: Rect) -> Result<(), String> {
        let clip = if node.max_lines.is_some() {
            let Some(clip) = bounds.intersection(inherited_clip) else {
                return Ok(());
            };
            clip
        } else {
            inherited_clip
        };
        self.canvas.scissor(clip.x, clip.y, clip.width, clip.height);
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
        let layout = self
            .text
            .layout(node, Some((bounds.width - 2.0 * padding_x).max(0.0)));
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let run = glyph_run.run();
                let font = run.font();
                let key = (font.data.id(), font.index);
                let id = match self.fonts.get(&key) {
                    Some(id) => *id,
                    None => {
                        let id = self
                            .text_context
                            .add_shared_font_with_index(font.data.clone(), font.index)
                            .map_err(|e| e.to_string())?;
                        self.fonts.insert(key, id);
                        id
                    }
                };
                let mut paint = Paint::color(color(node.style.foreground));
                paint.set_font_size(run.font_size());
                let glyphs = glyph_run.positioned_glyphs().map(|glyph| PositionedGlyph {
                    x: bounds.x + padding_x + glyph.x,
                    y: bounds.y + padding_y + glyph.y,
                    glyph_id: glyph.id as u16,
                });
                self.canvas
                    .fill_glyph_run(id, run.normalized_coords(), glyphs, &paint)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn draw_slider(&mut self, node: &Node, bounds: Rect) {
        let span = HorizontalRange::unclamped_inset(bounds, 8.0);
        let x = span.start();
        let y = bounds.y + bounds.height / 2.0;
        let width = span.length();
        let value = node.slider.fraction() as f32;
        let mut track = Path::new();
        track.rounded_rect(x, y - 2.0, width, 4.0, 2.0);
        self.canvas
            .fill_path(&track, &Paint::color(Color::rgba(255, 255, 255, 50)));
        let mut active = Path::new();
        active.rounded_rect(x, y - 2.0, width * value, 4.0, 2.0);
        active.circle(span.position(value), y, 6.0);
        self.canvas
            .fill_path(&active, &Paint::color(color(node.style.foreground)));
    }

    fn draw_image(&mut self, node: &Node, bounds: Rect, scale: f32) {
        let area = image_area(node, bounds);
        let Some((id, width, height)) = self.images.get(&node.image, node.image_fit, area, scale)
        else {
            return;
        };
        let geometry = image_geometry(area, width, height, node.image_fit);
        let mut path = Path::new();
        let radius = image_radius(node.style.radius, geometry.clip);
        if radius > 0.0 {
            path.rounded_rect(
                geometry.clip.x,
                geometry.clip.y,
                geometry.clip.width,
                geometry.clip.height,
                radius,
            );
        } else {
            path.rect(
                geometry.clip.x,
                geometry.clip.y,
                geometry.clip.width,
                geometry.clip.height,
            );
        }
        self.canvas.fill_path(
            &path,
            &Paint::image(
                id,
                geometry.paint.x,
                geometry.paint.y,
                geometry.paint.width,
                geometry.paint.height,
                0.0,
                1.0,
            ),
        );
    }

    fn draw_path(&mut self, node: &Node, bounds: Rect) -> Result<(), String> {
        let Some(view_box) = node.path.view_box else {
            return Ok(());
        };
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
        let area = Rect {
            x: bounds.x + padding_x,
            y: bounds.y + padding_y,
            width: (bounds.width - 2.0 * padding_x).max(0.0),
            height: (bounds.height - 2.0 * padding_y).max(0.0),
        };
        let Some(transform) = PathTransform::new(view_box, area, node.path.fit) else {
            return Ok(());
        };
        let path = transformed_path(&node.path.segments, transform)?;
        self.canvas.intersect_rounded_scissor(
            area.x,
            area.y,
            area.width,
            area.height,
            node.style
                .radius
                .min(area.width / 2.0)
                .min(area.height / 2.0),
        );

        let fill = node.path.fill.unwrap_or(node.style.foreground);
        if fill.0[3] > 0 {
            let mut paint = Paint::color(color(fill));
            paint.set_fill_rule(match node.path.fill_rule {
                PathFillRule::NonZero => FillRule::NonZero,
                PathFillRule::EvenOdd => FillRule::EvenOdd,
            });
            self.canvas.fill_path(&path, &paint);
        }
        if node.path.stroke_width > 0.0 && node.path.stroke.0[3] > 0 {
            let mut paint = Paint::color(color(node.path.stroke));
            paint.set_line_width(node.path.stroke_width);
            paint.set_line_cap(match node.path.stroke_cap {
                PathStrokeCap::Butt => LineCap::Butt,
                PathStrokeCap::Round => LineCap::Round,
                PathStrokeCap::Square => LineCap::Square,
            });
            paint.set_line_join(match node.path.stroke_join {
                PathStrokeJoin::Miter => LineJoin::Miter,
                PathStrokeJoin::Round => LineJoin::Round,
                PathStrokeJoin::Bevel => LineJoin::Bevel,
            });
            self.canvas.stroke_path(&path, &paint);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PathTransform {
    scale_x: f64,
    scale_y: f64,
    translate_x: f64,
    translate_y: f64,
}

// femtovg computes a fourth-order flatness term in f32 after output scaling.
// at the maximum scale of 16, this leaves more than three orders of magnitude
// between the worst-case intermediate and f32::MAX.
const MAX_RENDER_COORDINATE: f64 = 10_000_000.0;

impl PathTransform {
    fn new(view_box: ViewBox, area: Rect, fit: PathFit) -> Option<Self> {
        if area.width <= 0.0 || area.height <= 0.0 {
            return None;
        }
        let scale_x = f64::from(area.width) / f64::from(view_box.width);
        let scale_y = f64::from(area.height) / f64::from(view_box.height);
        let (scale_x, scale_y) = match fit {
            PathFit::Contain => {
                let scale = scale_x.min(scale_y);
                (scale, scale)
            }
            PathFit::Cover => {
                let scale = scale_x.max(scale_y);
                (scale, scale)
            }
            PathFit::Fill => (scale_x, scale_y),
        };
        let width = f64::from(view_box.width) * scale_x;
        let height = f64::from(view_box.height) * scale_y;
        let transform = Self {
            scale_x,
            scale_y,
            translate_x: f64::from(area.x) + (f64::from(area.width) - width) / 2.0
                - f64::from(view_box.x) * scale_x,
            translate_y: f64::from(area.y) + (f64::from(area.height) - height) / 2.0
                - f64::from(view_box.y) * scale_y,
        };
        [
            transform.scale_x,
            transform.scale_y,
            transform.translate_x,
            transform.translate_y,
        ]
        .into_iter()
        .all(f64::is_finite)
        .then_some(transform)
    }

    fn point(self, x: f32, y: f32) -> Option<(f32, f32)> {
        let x = f64::from(x) * self.scale_x + self.translate_x;
        let y = f64::from(y) * self.scale_y + self.translate_y;
        (x.is_finite()
            && y.is_finite()
            && x.abs() <= MAX_RENDER_COORDINATE
            && y.abs() <= MAX_RENDER_COORDINATE)
            .then_some((x as f32, y as f32))
    }
}

fn transformed_path(segments: &[VectorSegment], transform: PathTransform) -> Result<Path, String> {
    let mut path = Path::new();
    for segment in segments {
        match *segment {
            VectorSegment::MoveTo { x, y } => {
                let (x, y) = transform.point(x, y).ok_or_else(path_range_error)?;
                path.move_to(x, y);
            }
            VectorSegment::LineTo { x, y } => {
                let (x, y) = transform.point(x, y).ok_or_else(path_range_error)?;
                path.line_to(x, y);
            }
            VectorSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                let (x1, y1) = transform.point(x1, y1).ok_or_else(path_range_error)?;
                let (x2, y2) = transform.point(x2, y2).ok_or_else(path_range_error)?;
                let (x, y) = transform.point(x, y).ok_or_else(path_range_error)?;
                path.bezier_to(x1, y1, x2, y2, x, y);
            }
            VectorSegment::Quadratic { x1, y1, x, y } => {
                let (x1, y1) = transform.point(x1, y1).ok_or_else(path_range_error)?;
                let (x, y) = transform.point(x, y).ok_or_else(path_range_error)?;
                path.quad_to(x1, y1, x, y);
            }
            VectorSegment::Close => path.close(),
        }
    }
    Ok(path)
}

fn path_range_error() -> String {
    "path coordinates exceed renderer limits after fitting".into()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ImageGeometry {
    paint: Rect,
    clip: Rect,
}

fn image_area(node: &Node, bounds: Rect) -> Rect {
    let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
    let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
    Rect {
        x: bounds.x + padding_x,
        y: bounds.y + padding_y,
        width: (bounds.width - 2.0 * padding_x).max(0.0),
        height: (bounds.height - 2.0 * padding_y).max(0.0),
    }
}

fn image_geometry(area: Rect, width: u32, height: u32, fit: ImageFit) -> ImageGeometry {
    match fit {
        ImageFit::Contain => {}
        ImageFit::Cover | ImageFit::Fill => {
            return ImageGeometry {
                paint: area,
                clip: area,
            };
        }
    }
    let width = width as f32;
    let height = height as f32;
    let factor = (area.width / width).min(area.height / height);
    let width = width * factor;
    let height = height * factor;
    let paint = Rect {
        x: area.x + (area.width - width) / 2.0,
        y: area.y + (area.height - height) / 2.0,
        width,
        height,
    };
    ImageGeometry { paint, clip: paint }
}

fn image_radius(radius: f32, clip: Rect) -> f32 {
    radius.min(clip.width / 2.0).min(clip.height / 2.0)
}

impl Painter<OpenGl> {
    pub fn paint(
        &mut self,
        root: &Node,
        layout: &Layout,
        physical: (u32, u32),
        scale: f32,
        input: Interaction,
    ) -> Result<(), String> {
        self.paint_to_output(
            PaintFrame {
                root,
                layout,
                physical,
                scale,
                input,
            },
            (),
            |_| {},
        )
    }
}

fn color(value: thing_core::Color) -> Color {
    let [r, g, b, a] = value.0;
    Color::rgba(r, g, b, a)
}

fn interaction_overlay(node: &Node, input: Interaction) -> Option<thing_core::Color> {
    if !node.kind.is_interactive() {
        return None;
    }
    let overlay = if input.pressed == Some(node.id) {
        node.style.pressed_overlay
    } else if input.hovered == Some(node.id) {
        node.style.hover_overlay
    } else {
        return None;
    };
    (overlay.0[3] > 0).then_some(overlay)
}

fn focus_ring(node: &Node, input: Interaction) -> Option<(thing_core::Color, f32)> {
    (input.focus_visible
        && input.focused == Some(node.id)
        && node.style.focus_ring_width > 0.0
        && node.style.focus_ring_color.0[3] > 0)
        .then_some((node.style.focus_ring_color, node.style.focus_ring_width))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use femtovg::renderer::Void;
    use thing_core::Color as ThingColor;

    use super::*;

    #[test]
    fn focus_indicator_follows_the_input_method() {
        let mut interaction = Interaction::default();
        interaction.focus_from_pointer(Some(NodeId(1)));
        assert_eq!(interaction.focused, Some(NodeId(1)));
        assert!(!interaction.focus_visible);

        interaction.reveal_keyboard_focus();
        assert!(interaction.focus_visible);

        interaction.focus_from_pointer(Some(NodeId(2)));
        assert_eq!(interaction.focused, Some(NodeId(2)));
        assert!(!interaction.focus_visible);

        interaction.focus_from_keyboard(NodeId(3));
        assert_eq!(interaction.focused, Some(NodeId(3)));
        assert!(interaction.focus_visible);
    }

    #[test]
    fn pressed_overlay_takes_precedence_and_transparency_disables_it() {
        let mut button = Node::new(NodeId(1), Kind::Button);
        button.style.hover_overlay = ThingColor([1, 2, 3, 4]);
        button.style.pressed_overlay = ThingColor([5, 6, 7, 8]);
        let input = Interaction {
            hovered: Some(button.id),
            pressed: Some(button.id),
            ..Interaction::default()
        };
        assert_eq!(
            interaction_overlay(&button, input),
            Some(button.style.pressed_overlay)
        );

        button.style.pressed_overlay = ThingColor::TRANSPARENT;
        assert_eq!(interaction_overlay(&button, input), None);
        let hover = Interaction {
            hovered: Some(button.id),
            ..Interaction::default()
        };
        assert_eq!(
            interaction_overlay(&button, hover),
            Some(button.style.hover_overlay)
        );
        assert_eq!(
            interaction_overlay(&Node::new(NodeId(2), Kind::Text), hover),
            None
        );
    }

    #[test]
    fn focus_ring_uses_keyboard_visibility_and_accepts_zero_width() {
        let mut button = Node::new(NodeId(1), Kind::Button);
        button.style.focus_ring_color = ThingColor([12, 12, 12, 255]);
        button.style.focus_ring_width = 1.0;
        let mut input = Interaction::default();
        input.focus_from_pointer(Some(button.id));
        assert_eq!(focus_ring(&button, input), None);

        input.reveal_keyboard_focus();
        assert_eq!(
            focus_ring(&button, input),
            Some((button.style.focus_ring_color, 1.0))
        );
        button.style.focus_ring_width = 0.0;
        assert_eq!(focus_ring(&button, input), None);
    }

    #[test]
    fn failed_frame_still_runs_submission_cleanup() {
        let mut painter = Painter::new(Void, || {}).unwrap();
        let submitted = Cell::new(false);

        let result = painter.finish_output(Err("paint failed".into()), (), |_| {
            submitted.set(true);
        });

        assert!(submitted.get());
        assert_eq!(result.unwrap_err(), "paint failed");
    }

    #[test]
    fn image_area_applies_axis_padding_without_rounding() {
        let mut image = Node::new(NodeId(1), Kind::Image);
        image.style.padding = 5.0;
        image.style.padding_x = Some(10.0);
        let bounds = Rect {
            x: 2.5,
            y: 4.0,
            width: 100.0,
            height: 50.0,
        };

        assert_eq!(
            image_area(&image, bounds),
            Rect {
                x: 12.5,
                y: 9.0,
                width: 80.0,
                height: 40.0,
            }
        );
    }

    #[test]
    fn contain_centers_landscape_and_portrait_images() {
        let area = Rect {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 100.0,
        };
        let landscape = image_geometry(area, 200, 100, ImageFit::Contain);
        assert_eq!(
            landscape,
            ImageGeometry {
                paint: Rect {
                    x: 10.0,
                    y: 45.0,
                    width: 100.0,
                    height: 50.0,
                },
                clip: Rect {
                    x: 10.0,
                    y: 45.0,
                    width: 100.0,
                    height: 50.0,
                },
            }
        );
        let portrait = image_geometry(area, 100, 200, ImageFit::Contain);
        assert_eq!(
            portrait.paint,
            Rect {
                x: 35.0,
                y: 20.0,
                width: 50.0,
                height: 100.0,
            }
        );
    }

    #[test]
    fn path_fit_maps_negative_view_boxes_into_the_content_area() {
        let view_box = ViewBox::new(-10.0, 20.0, 20.0, 10.0).unwrap();
        let area = Rect {
            x: 100.0,
            y: 50.0,
            width: 100.0,
            height: 100.0,
        };

        let contain = PathTransform::new(view_box, area, PathFit::Contain).unwrap();
        assert_eq!(contain.point(-10.0, 20.0), Some((100.0, 75.0)));
        assert_eq!(contain.point(10.0, 30.0), Some((200.0, 125.0)));

        let cover = PathTransform::new(view_box, area, PathFit::Cover).unwrap();
        assert_eq!(cover.point(-10.0, 20.0), Some((50.0, 50.0)));
        assert_eq!(cover.point(10.0, 30.0), Some((250.0, 150.0)));

        let fill = PathTransform::new(view_box, area, PathFit::Fill).unwrap();
        assert_eq!(fill.point(-10.0, 20.0), Some((100.0, 50.0)));
        assert_eq!(fill.point(10.0, 30.0), Some((200.0, 150.0)));
        assert!(
            PathTransform::new(view_box, Rect { width: 0.0, ..area }, PathFit::Contain).is_none()
        );
    }

    #[test]
    fn cover_and_fill_use_the_fit_aware_texture_without_a_second_crop() {
        let area = Rect {
            x: 1.25,
            y: 2.5,
            width: 100.5,
            height: 40.25,
        };
        for fit in [ImageFit::Cover, ImageFit::Fill] {
            assert_eq!(
                image_geometry(area, 1000, 100, fit),
                ImageGeometry {
                    paint: area,
                    clip: area,
                }
            );
        }
    }

    #[test]
    fn image_radius_is_clamped_to_the_visible_path() {
        let clip = Rect {
            width: 100.0,
            height: 20.0,
            ..Rect::default()
        };
        assert_eq!(image_radius(6.0, clip), 6.0);
        assert_eq!(image_radius(30.0, clip), 10.0);
    }

    #[test]
    fn tiny_view_boxes_never_send_nonfinite_coordinates_to_the_renderer() {
        let dimension = f32::from_bits(1);
        let view_box = ViewBox::new(0.0, 0.0, dimension, dimension).unwrap();
        let area = Rect {
            x: 0.0,
            y: 0.0,
            width: 100_000.0,
            height: 100_000.0,
        };
        let transform = PathTransform::new(view_box, area, PathFit::Contain).unwrap();

        assert_eq!(
            transform.point(dimension, dimension),
            Some((100_000.0, 100_000.0))
        );
        assert!(
            transformed_path(
                &[
                    VectorSegment::MoveTo { x: 0.0, y: 0.0 },
                    VectorSegment::LineTo {
                        x: dimension,
                        y: dimension,
                    },
                ],
                transform,
            )
            .is_ok()
        );
        assert_eq!(transform.point(1.0, 1.0), None);
        assert!(transformed_path(&[VectorSegment::MoveTo { x: 1.0, y: 1.0 }], transform).is_err());

        let mut root = Node::new(NodeId(1), Kind::Path);
        root.path
            .set_data("M 0 0 L 1.40129846e-45 0 L 1.40129846e-45 1.40129846e-45 Z".into())
            .unwrap();
        root.path.view_box = Some(view_box);
        root.style.width = Some(100.0);
        root.style.height = Some(100.0);
        let mut painter = Painter::new(Void, || {}).unwrap();
        let mut scroll = ScrollState::default();
        let layout = painter
            .layout(
                &root,
                Size {
                    width: 100.0,
                    height: 100.0,
                },
                &mut scroll,
            )
            .unwrap();
        painter
            .paint_to_output(
                PaintFrame {
                    root: &root,
                    layout: &layout,
                    physical: (100, 100),
                    scale: 1.0,
                    input: Interaction::default(),
                },
                (),
                |_| {},
            )
            .unwrap();
    }

    #[test]
    fn path_coordinate_limit_leaves_curve_flattening_headroom() {
        let maximum_physical_coordinate = 16.0 * MAX_RENDER_COORDINATE;
        let maximum_flatness_term = 256.0 * maximum_physical_coordinate.powi(4);
        assert!(maximum_flatness_term < f64::from(f32::MAX));

        let transform = PathTransform {
            scale_x: 1.0,
            scale_y: 1.0,
            translate_x: 0.0,
            translate_y: 0.0,
        };
        let boundary = MAX_RENDER_COORDINATE as f32;
        assert_eq!(
            transform.point(boundary, -boundary),
            Some((boundary, -boundary))
        );
        assert_eq!(transform.point(boundary * 2.0, 0.0), None);
    }

    #[test]
    fn vector_path_tessellates_with_fill_stroke_and_rounded_clipping() {
        let mut root = Node::new(NodeId(1), Kind::Path);
        root.path
            .set_data("M 0 0 C 10 0 10 20 20 20 A 8 8 0 0 1 0 0 Z".into())
            .unwrap();
        root.path.view_box = Some(ViewBox::new(0.0, 0.0, 20.0, 20.0).unwrap());
        root.path.fit = PathFit::Fill;
        root.path.fill_rule = PathFillRule::EvenOdd;
        root.path.stroke = ThingColor([255, 255, 255, 255]);
        root.path.stroke_width = 2.0;
        root.path.stroke_cap = PathStrokeCap::Round;
        root.path.stroke_join = PathStrokeJoin::Bevel;
        root.style.width = Some(80.0);
        root.style.height = Some(40.0);
        root.style.padding = 4.0;
        root.style.radius = 8.0;

        let mut painter = Painter::new(Void, || {}).unwrap();
        let mut scroll = ScrollState::default();
        let layout = painter
            .layout(
                &root,
                Size {
                    width: 80.0,
                    height: 40.0,
                },
                &mut scroll,
            )
            .unwrap();
        painter
            .paint_to_output(
                PaintFrame {
                    root: &root,
                    layout: &layout,
                    physical: (80, 40),
                    scale: 1.0,
                    input: Interaction::default(),
                },
                (),
                |_| {},
            )
            .unwrap();
    }
}
