use std::collections::BTreeMap;

use femtovg::{Canvas, Color, FontId, Paint, Path, PositionedGlyph, TextContext, renderer::OpenGl};
use parley::PositionedLayoutItem;
use thing_core::{Kind, Node, NodeId, Rect};

use crate::{Layout, Size, TextSystem};

/// transient input state belongs to the host, not to the widget snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct Interaction {
    pub hovered: Option<NodeId>,
    pub pressed: Option<NodeId>,
    pub focused: Option<NodeId>,
}

/// an opengl es painter. the host must keep its context current during use and drop.
pub struct Painter {
    canvas: Canvas<OpenGl>,
    text_context: TextContext,
    fonts: BTreeMap<(u64, u32), FontId>,
    pub text: TextSystem,
    images: crate::images::Images,
}

impl Painter {
    pub fn new(renderer: OpenGl, wake: impl Fn() + Send + 'static) -> Result<Self, String> {
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

    pub fn paint(
        &mut self,
        root: &Node,
        layout: &Layout,
        physical: (u32, u32),
        scale: f32,
        input: Interaction,
    ) -> Result<(), String> {
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
                .set_global_alpha(placement.opacity * if placement.enabled { 1.0 } else { 0.45 });
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
            if node.kind.is_interactive()
                && (input.hovered == Some(node.id) || input.pressed == Some(node.id))
            {
                let alpha = if input.pressed == Some(node.id) {
                    40
                } else {
                    20
                };
                self.canvas
                    .fill_path(&path, &Paint::color(Color::rgba(255, 255, 255, alpha)));
            }
            if input.focused == Some(node.id) {
                let mut focus = Paint::color(Color::rgb(137, 180, 250));
                focus.set_line_width(2.0);
                self.canvas.stroke_path(&path, &focus);
            }
            match node.kind {
                Kind::Text => self.draw_text(node, bounds)?,
                Kind::Slider => self.draw_slider(node, bounds),
                Kind::Image => self.draw_image(node, bounds, scale),
                _ => {}
            }
        }
        self.canvas.reset_scissor();
        self.canvas.flush();
        Ok(())
    }

    pub fn layout(&mut self, root: &Node, size: Size) -> Result<Layout, String> {
        Layout::compute(root, size, &mut self.text)
    }

    fn draw_text(&mut self, node: &Node, bounds: Rect) -> Result<(), String> {
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
        let x = bounds.x + 8.0;
        let y = bounds.y + bounds.height / 2.0;
        let width = (bounds.width - 16.0).max(0.0);
        let value = node.slider.fraction() as f32;
        let mut track = Path::new();
        track.rounded_rect(x, y - 2.0, width, 4.0, 2.0);
        self.canvas
            .fill_path(&track, &Paint::color(Color::rgba(255, 255, 255, 50)));
        let mut active = Path::new();
        active.rounded_rect(x, y - 2.0, width * value, 4.0, 2.0);
        active.circle(x + width * value, y, 6.0);
        self.canvas
            .fill_path(&active, &Paint::color(color(node.style.foreground)));
    }

    fn draw_image(&mut self, node: &Node, bounds: Rect, scale: f32) {
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
        let area = Rect {
            x: bounds.x + padding_x,
            y: bounds.y + padding_y,
            width: (bounds.width - 2.0 * padding_x).max(0.0),
            height: (bounds.height - 2.0 * padding_y).max(0.0),
        };
        let Some((id, width, height)) = self.images.get(&mut self.canvas, &node.image, area, scale)
        else {
            return;
        };
        let factor = (area.width / width as f32).min(area.height / height as f32);
        let width = width as f32 * factor;
        let height = height as f32 * factor;
        let x = area.x + (area.width - width) / 2.0;
        let y = area.y + (area.height - height) / 2.0;
        let mut path = Path::new();
        path.rect(x, y, width, height);
        self.canvas
            .fill_path(&path, &Paint::image(id, x, y, width, height, 0.0, 1.0));
    }
}

fn color(value: thing_core::Color) -> Color {
    let [r, g, b, a] = value.0;
    Color::rgba(r, g, b, a)
}
