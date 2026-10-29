use std::{
    collections::VecDeque,
    ffi::{CStr, c_void},
};

use parley::PositionedLayoutItem;
use skia_safe::{
    Canvas, ClipOp, Color as SkiaColor, ColorType, Font, FontArguments, FontMgr, FourByteTag, M44,
    Matrix, Paint, PaintCap, PaintJoin, PaintStyle, Path as SkiaPath, PathBuilder, PathFillType,
    Point, RRect, Rect as SkiaRect, SamplingOptions, Typeface,
    font_arguments::{VariationPosition, variation_position::Coordinate as VariationCoordinate},
    gpu::{
        self, DirectContext, SurfaceOrigin, backend_render_targets, direct_contexts,
        gl::{Format, FramebufferInfo},
    },
};
use skrifa::MetadataProvider;
use thing_core::{
    HorizontalRange, ImageFit, Kind, Node, PathFillRule, PathFit, PathStrokeCap, PathStrokeJoin,
    Rect, TextAlign, VectorSegment, ViewBox,
};

use crate::{
    Affine, Interaction, Layout, PaintFrame, ScrollState, SecretInputEditor, Size, TextInputEditor,
    TextSystem, images::Images,
};

const RESOURCE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const STROKE_MITER_LIMIT: f32 = 10.0;
const TYPEFACE_CACHE_LIMIT: usize = 64;
const VARIATION_AXIS_LIMIT: usize = 64;

#[derive(Clone, PartialEq, Eq)]
struct TypefaceKey {
    data: u64,
    index: u32,
    normalized: Box<[i16]>,
}

impl TypefaceKey {
    fn new(source: &parley::FontData, normalized: &[i16]) -> Self {
        Self {
            data: source.data.id(),
            index: source.index,
            normalized: normalized.into(),
        }
    }
}

#[derive(Clone, Copy)]
struct DesignCoordinate {
    axis: FourByteTag,
    value: f32,
}

#[derive(Default)]
struct TypefaceCache {
    entries: VecDeque<(TypefaceKey, Typeface)>,
}

fn configured_font(typeface: Typeface, size: f32, embolden: bool) -> Font {
    let mut font = Font::from_typeface(typeface, size);
    font.set_embolden(embolden);
    font
}

impl TypefaceCache {
    fn clear(&mut self) {
        self.entries.clear();
    }

    fn get(&mut self, key: &TypefaceKey) -> Option<Typeface> {
        let index = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == key)?;
        let entry = self.entries.remove(index)?;
        let typeface = entry.1.clone();
        self.entries.push_back(entry);
        Some(typeface)
    }

    fn insert(&mut self, key: TypefaceKey, typeface: Typeface) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == &key)
        {
            self.entries.remove(index);
        }
        if self.entries.len() == TYPEFACE_CACHE_LIMIT {
            self.entries.pop_front();
        }
        self.entries.push_back((key, typeface));
    }

    fn resolve(
        &mut self,
        fonts: &FontMgr,
        source: &parley::FontData,
        normalized: &[i16],
        design: &[DesignCoordinate],
    ) -> Result<Typeface, String> {
        if normalized.len() > VARIATION_AXIS_LIMIT || design.len() > VARIATION_AXIS_LIMIT {
            return Err(format!(
                "font face {} has too many variation axes",
                source.index
            ));
        }
        let key = TypefaceKey::new(source, normalized);
        if let Some(typeface) = self.get(&key) {
            return Ok(typeface);
        }

        let base_key = TypefaceKey::new(source, &[]);
        let base = match self.get(&base_key) {
            Some(typeface) => typeface,
            None => {
                let typeface = fonts
                    .new_from_bytes(source.data.as_ref(), Some(source.index))
                    .ok_or_else(|| format!("Skia rejected font face {}", source.index))?;
                self.insert(base_key, typeface.clone());
                typeface
            }
        };
        if normalized.iter().all(|coordinate| *coordinate == 0) {
            self.insert(key, base.clone());
            return Ok(base);
        }

        let axes = base.variation_design_parameters().ok_or_else(|| {
            format!(
                "Skia did not expose variation axes for font face {}",
                source.index
            )
        })?;
        if axes.len() != normalized.len() {
            return Err(format!(
                "font face {} exposed {} variation axes for {} normalized coordinates",
                source.index,
                axes.len(),
                normalized.len()
            ));
        }
        for (axis, coordinate) in axes.iter().zip(normalized) {
            if *coordinate != 0 && !design.iter().any(|setting| setting.axis == axis.tag) {
                return Err(format!(
                    "font face {} is missing the design value for a varied axis",
                    source.index
                ));
            }
        }
        if design
            .iter()
            .any(|coordinate| !coordinate.value.is_finite())
        {
            return Err(format!(
                "font face {} has a non-finite variation value",
                source.index
            ));
        }
        let font = skrifa::FontRef::from_index(source.data.as_ref(), source.index)
            .map_err(|_| format!("could not read font face {}", source.index))?;
        let expected = font.axes().location(
            design
                .iter()
                .map(|coordinate| (skrifa::Tag::from_u32(*coordinate.axis), coordinate.value)),
        );
        if expected
            .coords()
            .iter()
            .map(|coordinate| coordinate.to_bits())
            .ne(normalized.iter().copied())
        {
            return Err(format!(
                "font face {} has variation coordinates that are not represented by its synthesis settings",
                source.index
            ));
        }

        let coordinates = design
            .iter()
            .map(|coordinate| VariationCoordinate {
                axis: coordinate.axis,
                value: coordinate.value,
            })
            .collect::<Vec<_>>();
        let arguments = FontArguments::new().set_variation_design_position(VariationPosition {
            coordinates: &coordinates,
        });
        let typeface = base.clone_with_arguments(&arguments).ok_or_else(|| {
            format!(
                "Skia rejected variation coordinates for font face {}",
                source.index
            )
        })?;
        self.insert(key, typeface.clone());
        Ok(typeface)
    }
}

pub struct Painter {
    typefaces: TypefaceCache,
    fonts: FontMgr,
    pub text: TextSystem,
    images: Images,
    context: DirectContext,
    context_abandoned: bool,
}

impl Painter {
    pub fn new_gl(
        load: impl FnMut(&CStr) -> *const c_void,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self, String> {
        let interface = gpu::gl::Interface::new_load_with_cstr(load)
            .ok_or("cannot load the Skia OpenGL interface")?;
        if !interface.validate() {
            return Err("the Skia OpenGL interface is incomplete".into());
        }
        let mut context = direct_contexts::make_gl(interface, None)
            .ok_or("cannot create the Skia OpenGL context")?;
        context.set_resource_cache_limit(RESOURCE_CACHE_BYTES);
        Ok(Self {
            typefaces: TypefaceCache::default(),
            fonts: FontMgr::default(),
            text: TextSystem::default(),
            images: Images::new(wake)?,
            context,
            context_abandoned: false,
        })
    }

    #[cfg(feature = "vulkan")]
    pub fn new_vulkan(
        mut context: DirectContext,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self, String> {
        context.set_resource_cache_limit(RESOURCE_CACHE_BYTES);
        Ok(Self {
            typefaces: TypefaceCache::default(),
            fonts: FontMgr::default(),
            text: TextSystem::default(),
            images: Images::new(wake)?,
            context,
            context_abandoned: false,
        })
    }

    pub fn reset(&mut self, wake: impl Fn() + Send + 'static) -> Result<(), String> {
        let replacement = Images::new(wake)?;
        self.images.clear();
        self.typefaces.clear();
        self.text = TextSystem::default();
        self.context.free_gpu_resources();
        self.images = replacement;
        Ok(())
    }

    pub fn abandon(&mut self) {
        if !self.context_abandoned {
            self.context.abandon();
            self.context_abandoned = true;
        }
    }

    pub fn layout(
        &mut self,
        root: &Node,
        size: Size,
        scroll: &mut ScrollState,
    ) -> Result<Layout, String> {
        Layout::compute_with_scroll(root, size, scroll, &mut self.text)
    }

    pub fn paint(&mut self, frame: PaintFrame<'_>, framebuffer: u32) -> Result<(), String> {
        let width = i32::try_from(frame.physical.0).map_err(|error| error.to_string())?;
        let height = i32::try_from(frame.physical.1).map_err(|error| error.to_string())?;
        let target = backend_render_targets::make_gl(
            (width, height),
            0,
            8,
            FramebufferInfo {
                fboid: framebuffer,
                format: Format::RGBA8.into(),
                ..FramebufferInfo::default()
            },
        );
        let mut surface = gpu::surfaces::wrap_backend_render_target(
            &mut self.context,
            &target,
            SurfaceOrigin::BottomLeft,
            ColorType::RGBA8888,
            None,
            None,
        )
        .ok_or("cannot wrap the EGL framebuffer for Skia")?;

        let drawn = self.draw(surface.canvas(), frame);
        let submitted = self
            .context
            .flush_and_submit_surface(&mut surface, None)
            .map_err(|error| format!("cannot submit the Skia frame: {error:?}"));
        self.images.release();
        drawn?;
        submitted.map(|_| ())
    }

    #[cfg(feature = "vulkan")]
    pub fn draw_surface(
        &mut self,
        surface: &mut skia_safe::Surface,
        frame: PaintFrame<'_>,
    ) -> Result<(), String> {
        self.draw(surface.canvas(), frame)
    }

    #[cfg(feature = "vulkan")]
    pub fn context_mut(&mut self) -> &mut DirectContext {
        &mut self.context
    }

    #[cfg(feature = "vulkan")]
    pub fn release_frame_resources(&mut self) {
        self.images.release();
    }

    fn draw(&mut self, canvas: &Canvas, frame: PaintFrame<'_>) -> Result<(), String> {
        let PaintFrame {
            root,
            layout,
            physical: _,
            scale,
            input,
            mut text_input,
            mut secret_input,
        } = frame;
        self.images.collect();
        canvas.clear(SkiaColor::TRANSPARENT);
        canvas.reset_matrix();
        canvas.scale((scale, scale));
        let surface_matrix = canvas.local_to_device();
        self.text.retain(root);
        let mut active_clips = Vec::new();

        for placement in layout.placements() {
            let Some(node) = root.find(placement.id) else {
                continue;
            };
            sync_clip_chain(
                canvas,
                layout,
                placement.clip_chain,
                &surface_matrix,
                &mut active_clips,
            )?;
            let saved = canvas.save();
            set_surface_transform(canvas, &surface_matrix, placement.transform)?;
            let opacity = placement.opacity * placement.disabled_opacity;
            let bounds = placement.local_bounds();
            let shape = rounded_rect(bounds, node.style.radius);
            canvas.draw_rrect(shape, &fill(color(node.style.background), opacity));

            if node.style.border_width > 0.0 {
                let width = node.style.border_width.min(bounds.width).min(bounds.height);
                let border = Rect {
                    x: bounds.x + width / 2.0,
                    y: bounds.y + width / 2.0,
                    width: (bounds.width - width).max(0.0),
                    height: (bounds.height - width).max(0.0),
                };
                let mut paint = stroke(color(node.style.border_color), width, opacity);
                paint.set_anti_alias(true);
                canvas.draw_rrect(
                    rounded_rect(border, (node.style.radius - width / 2.0).max(0.0)),
                    &paint,
                );
            }
            if let Some(overlay) = interaction_overlay(node, input) {
                canvas.draw_rrect(shape, &fill(color(overlay), opacity));
            }
            if let Some((focus_color, focus_width)) = focus_ring(node, input) {
                canvas.draw_rrect(shape, &stroke(color(focus_color), focus_width, opacity));
            }

            let drawn = match node.kind {
                Kind::Text => self.draw_text(canvas, node, bounds, opacity),
                Kind::TextInput => self.draw_text_input(
                    canvas,
                    node,
                    bounds,
                    opacity,
                    (input.focused == Some(node.id))
                        .then_some(text_input.as_deref_mut())
                        .flatten(),
                ),
                Kind::SecretInput => self.draw_text_input(
                    canvas,
                    node,
                    bounds,
                    placement.clip,
                    opacity,
                    (input.focused == Some(node.id))
                        .then_some(secret_input.as_deref_mut())
                        .flatten()
                        .map(SecretInputEditor::display),
                ),
                Kind::Slider => {
                    draw_slider(canvas, node, bounds, opacity);
                    Ok(())
                }
                Kind::Image => {
                    self.draw_image(
                        canvas,
                        node,
                        bounds,
                        f64::from(scale) * placement.magnification(),
                        opacity,
                    );
                    Ok(())
                }
                Kind::Path => draw_path(canvas, node, bounds, opacity),
                _ => Ok(()),
            };
            canvas.restore_to_count(saved);
            drawn?;
        }
        while active_clips.pop().is_some() {
            canvas.restore();
        }
        Ok(())
    }

    fn draw_text(
        &mut self,
        canvas: &Canvas,
        node: &Node,
        bounds: Rect,
        opacity: f32,
    ) -> Result<(), String> {
        let saved = canvas.save();
        if node.max_lines.is_some() {
            canvas.clip_rect(to_skia_rect(bounds), ClipOp::Intersect, false);
        }
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
        let layout = self
            .text
            .layout(node, Some((bounds.width - 2.0 * padding_x).max(0.0)));
        draw_layout(
            canvas,
            &mut self.typefaces,
            &self.fonts,
            layout,
            Point::new(bounds.x + padding_x, bounds.y + padding_y),
            node.style.foreground,
            opacity,
        )?;
        canvas.restore_to_count(saved);
        Ok(())
    }

    fn draw_text_input(
        &mut self,
        canvas: &Canvas,
        node: &Node,
        bounds: Rect,
        opacity: f32,
        active: Option<&mut TextInputEditor>,
    ) -> Result<(), String> {
        let content = text_input_content(node, bounds);
        if content.width <= 0.0 || content.height <= 0.0 {
            return Ok(());
        }
        let saved = canvas.save();
        canvas.clip_rect(to_skia_rect(content), ClipOp::Intersect, false);

        if let Some(editor) = active {
            editor.prepare(node, content.width);
            let layout = editor.layout();
            let offset_x = editor.text_offset_x();
            let offset_y = ((content.height - layout.height()) / 2.0).max(0.0);
            for (selection, _) in editor.selection_geometry() {
                canvas.draw_rect(
                    to_skia_rect(editor_rect(content, offset_x, offset_y, selection)),
                    &fill(color(node.style.selection_color), opacity),
                );
            }
            if editor.display_is_empty() && !node.placeholder.is_empty() {
                self.draw_text_input_value(
                    canvas,
                    node,
                    content,
                    &node.placeholder,
                    true,
                    opacity,
                )?;
            } else {
                draw_layout(
                    canvas,
                    &mut self.typefaces,
                    &self.fonts,
                    layout,
                    Point::new(content.x + offset_x, content.y + offset_y),
                    node.style.foreground,
                    opacity,
                )?;
            }
            for (preedit, _) in editor.preedit_geometry() {
                canvas.draw_rect(
                    to_skia_rect(preedit_underline_rect(content, offset_x, offset_y, preedit)),
                    &fill(color(node.style.foreground), opacity),
                );
            }
            if let Some(caret) = editor.cursor_geometry() {
                canvas.draw_rect(
                    to_skia_rect(editor_rect(content, offset_x, offset_y, caret)),
                    &fill(color(node.style.caret_color), opacity),
                );
            }
        } else {
            let (value, placeholder) = if node.text.is_empty() {
                (node.placeholder.as_str(), true)
            } else {
                (node.text.as_str(), false)
            };
            self.draw_text_input_value(canvas, node, content, value, placeholder, opacity)?;
        }

        canvas.restore_to_count(saved);
        Ok(())
    }

    fn draw_text_input_value(
        &mut self,
        canvas: &Canvas,
        node: &Node,
        content: Rect,
        value: &str,
        placeholder: bool,
        opacity: f32,
    ) -> Result<(), String> {
        let mut display = node.clone();
        display.text = value.to_owned();
        let layout = self.text.layout(&display, None);
        let x = content.x + text_alignment_offset(layout, content.width, node.style.text_align);
        let y = content.y + ((content.height - layout.height()) / 2.0).max(0.0);
        let foreground = if placeholder {
            node.style.placeholder_color
        } else {
            node.style.foreground
        };
        draw_layout(
            canvas,
            &mut self.typefaces,
            &self.fonts,
            layout,
            Point::new(x, y),
            foreground,
            opacity,
        )
    }

    fn draw_image(&mut self, canvas: &Canvas, node: &Node, bounds: Rect, scale: f64, opacity: f32) {
        let area = image_area(node, bounds);
        let Some((image, width, height)) =
            self.images.get(&node.image, node.image_fit, area, scale)
        else {
            return;
        };
        let geometry = image_geometry(area, width, height, node.image_fit);
        let saved = canvas.save();
        canvas.clip_rrect(
            rounded_rect(
                geometry.clip,
                image_radius(node.style.radius, geometry.clip),
            ),
            ClipOp::Intersect,
            true,
        );
        canvas.draw_image_rect_with_sampling_options(
            &image,
            None,
            to_skia_rect(geometry.paint),
            SamplingOptions::from(skia_safe::FilterMode::Linear),
            &fill(SkiaColor::WHITE, opacity),
        );
        canvas.restore_to_count(saved);
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.images.clear();
        self.typefaces.clear();
        if !self.context_abandoned {
            self.context.release_resources_and_abandon();
        }
    }
}

fn sync_clip_chain(
    canvas: &Canvas,
    layout: &Layout,
    target: usize,
    surface_matrix: &M44,
    active: &mut Vec<usize>,
) -> Result<(), String> {
    let mut cursor = Some(target);
    let mut tail = Vec::new();
    while let Some(index) = cursor {
        let region = layout
            .clip_region(index)
            .ok_or("layout refers to a missing clip region")?;
        while active.len() > region.depth + 1 {
            active.pop();
            canvas.restore();
        }
        if active.get(region.depth) == Some(&index) {
            break;
        }
        tail.push(index);
        cursor = region.parent;
    }
    if cursor.is_none() {
        while active.pop().is_some() {
            canvas.restore();
        }
    }
    for index in tail.into_iter().rev() {
        let region = layout
            .clip_region(index)
            .ok_or("layout refers to a missing clip region")?;
        canvas.save();
        set_surface_transform(canvas, surface_matrix, region.transform)?;
        canvas.clip_rect(to_skia_rect(region.bounds), ClipOp::Intersect, false);
        canvas.set_matrix(surface_matrix);
        active.push(index);
    }
    Ok(())
}

fn set_surface_transform(
    canvas: &Canvas,
    surface_matrix: &M44,
    transform: Affine,
) -> Result<(), String> {
    let [xx, xy, tx, yx, yy, ty] = transform.skia_values()?;
    canvas.set_matrix(surface_matrix);
    canvas.concat(&Matrix::new_all(xx, xy, tx, yx, yy, ty, 0.0, 0.0, 1.0));
    Ok(())
}

fn draw_layout(
    canvas: &Canvas,
    typefaces: &mut TypefaceCache,
    fonts: &FontMgr,
    layout: &parley::Layout<()>,
    origin: Point,
    foreground: thing_core::Color,
    opacity: f32,
) -> Result<(), String> {
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let run = glyph_run.run();
            let source = run.font();
            let synthesis = run.synthesis();
            let design = synthesis
                .variation_settings()
                .iter()
                .map(|(tag, value)| DesignCoordinate {
                    axis: FourByteTag::new(u32::from_be_bytes(tag.to_be_bytes())),
                    value: *value,
                })
                .collect::<Vec<_>>();
            let typeface = typefaces.resolve(fonts, source, run.normalized_coords(), &design)?;
            let font = configured_font(typeface, run.font_size(), synthesis.embolden());
            let positioned = glyph_run.positioned_glyphs().collect::<Vec<_>>();
            let glyphs = positioned
                .iter()
                .map(|glyph| glyph.id as u16)
                .collect::<Vec<_>>();
            let positions = positioned
                .iter()
                .map(|glyph| Point::new(glyph.x, glyph.y))
                .collect::<Vec<_>>();
            canvas.draw_glyphs_at(
                &glyphs,
                positions.as_slice(),
                origin,
                &font,
                &fill(color(foreground), opacity),
            );
        }
    }
    Ok(())
}

fn text_alignment_offset(layout: &parley::Layout<()>, width: f32, align: TextAlign) -> f32 {
    let content = layout.full_width();
    let rtl = layout.is_rtl();
    match (align, rtl) {
        (TextAlign::Start, true) | (TextAlign::End, false) => width - content,
        (TextAlign::Center, _) => (width - content) / 2.0,
        _ => 0.0,
    }
}

fn text_input_content(node: &Node, bounds: Rect) -> Rect {
    let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
    let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
    Rect {
        x: bounds.x + padding_x,
        y: bounds.y + padding_y,
        width: (bounds.width - 2.0 * padding_x).max(0.0),
        height: (bounds.height - 2.0 * padding_y).max(0.0),
    }
}

fn editor_rect(content: Rect, offset_x: f32, offset_y: f32, bounds: parley::BoundingBox) -> Rect {
    Rect {
        x: content.x + offset_x + bounds.x0 as f32,
        y: content.y + offset_y + bounds.y0 as f32,
        width: (bounds.x1 - bounds.x0).max(0.0) as f32,
        height: (bounds.y1 - bounds.y0).max(0.0) as f32,
    }
}

fn preedit_underline_rect(
    content: Rect,
    offset_x: f32,
    offset_y: f32,
    bounds: parley::BoundingBox,
) -> Rect {
    Rect {
        x: content.x + offset_x + bounds.x0 as f32,
        y: content.y + offset_y + bounds.y1 as f32 - 1.0,
        width: (bounds.x1 - bounds.x0).max(0.0) as f32,
        height: 1.0,
    }
}

fn draw_slider(canvas: &Canvas, node: &Node, bounds: Rect, opacity: f32) {
    let span = HorizontalRange::unclamped_inset(bounds, 8.0);
    let y = bounds.y + bounds.height / 2.0;
    let value = node.slider.fraction() as f32;
    canvas.draw_rrect(
        rounded_rect(
            Rect {
                x: span.start(),
                y: y - 2.0,
                width: span.length(),
                height: 4.0,
            },
            2.0,
        ),
        &fill(SkiaColor::from_argb(50, 255, 255, 255), opacity),
    );
    let mut active = PathBuilder::new();
    active.add_rrect(
        rounded_rect(
            Rect {
                x: span.start(),
                y: y - 2.0,
                width: span.length() * value,
                height: 4.0,
            },
            2.0,
        ),
        None,
        None,
    );
    active.add_circle((span.position(value), y), 6.0, None);
    canvas.draw_path(
        &active.detach(),
        &fill(color(node.style.foreground), opacity),
    );
}

fn draw_path(canvas: &Canvas, node: &Node, bounds: Rect, opacity: f32) -> Result<(), String> {
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
    let mut path = transformed_path(&node.path.segments, transform)?;
    path.set_fill_type(match node.path.fill_rule {
        PathFillRule::NonZero => PathFillType::Winding,
        PathFillRule::EvenOdd => PathFillType::EvenOdd,
    });
    canvas.clip_rrect(
        rounded_rect(
            area,
            node.style
                .radius
                .min(area.width / 2.0)
                .min(area.height / 2.0),
        ),
        ClipOp::Intersect,
        true,
    );

    let path_fill = node.path.fill.unwrap_or(node.style.foreground);
    if path_fill.0[3] > 0 {
        canvas.draw_path(&path, &fill(color(path_fill), opacity));
    }
    if node.path.stroke_width > 0.0 && node.path.stroke.0[3] > 0 {
        let mut paint = stroke(color(node.path.stroke), node.path.stroke_width, opacity);
        paint.set_stroke_cap(match node.path.stroke_cap {
            PathStrokeCap::Butt => PaintCap::Butt,
            PathStrokeCap::Round => PaintCap::Round,
            PathStrokeCap::Square => PaintCap::Square,
        });
        paint.set_stroke_join(match node.path.stroke_join {
            PathStrokeJoin::Miter => PaintJoin::Miter,
            PathStrokeJoin::Round => PaintJoin::Round,
            PathStrokeJoin::Bevel => PaintJoin::Bevel,
        });
        canvas.draw_path(&path, &paint);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct PathTransform {
    scale_x: f64,
    scale_y: f64,
    translate_x: f64,
    translate_y: f64,
}

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

    fn point(self, x: f32, y: f32) -> Option<Point> {
        let x = f64::from(x) * self.scale_x + self.translate_x;
        let y = f64::from(y) * self.scale_y + self.translate_y;
        (x.is_finite() && y.is_finite() && x.abs() <= 10_000_000.0 && y.abs() <= 10_000_000.0)
            .then_some(Point::new(x as f32, y as f32))
    }
}

fn transformed_path(
    segments: &[VectorSegment],
    transform: PathTransform,
) -> Result<SkiaPath, String> {
    let mut path = PathBuilder::new();
    for segment in segments {
        match *segment {
            VectorSegment::MoveTo { x, y } => {
                path.move_to(transform.point(x, y).ok_or_else(path_range_error)?);
            }
            VectorSegment::LineTo { x, y } => {
                path.line_to(transform.point(x, y).ok_or_else(path_range_error)?);
            }
            VectorSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                path.cubic_to(
                    transform.point(x1, y1).ok_or_else(path_range_error)?,
                    transform.point(x2, y2).ok_or_else(path_range_error)?,
                    transform.point(x, y).ok_or_else(path_range_error)?,
                );
            }
            VectorSegment::Quadratic { x1, y1, x, y } => {
                path.quad_to(
                    transform.point(x1, y1).ok_or_else(path_range_error)?,
                    transform.point(x, y).ok_or_else(path_range_error)?,
                );
            }
            VectorSegment::Close => {
                path.close();
            }
        }
    }
    Ok(path.detach())
}

fn path_range_error() -> String {
    "path coordinates exceed renderer limits after fitting".into()
}

#[derive(Clone, Copy)]
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
    if fit != ImageFit::Contain {
        return ImageGeometry {
            paint: area,
            clip: area,
        };
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

fn rounded_rect(bounds: Rect, radius: f32) -> RRect {
    let radius = radius
        .max(0.0)
        .min(bounds.width / 2.0)
        .min(bounds.height / 2.0);
    RRect::new_rect_xy(to_skia_rect(bounds), radius, radius)
}

fn to_skia_rect(bounds: Rect) -> SkiaRect {
    SkiaRect::from_xywh(bounds.x, bounds.y, bounds.width, bounds.height)
}

fn color(value: thing_core::Color) -> SkiaColor {
    let [r, g, b, a] = value.0;
    SkiaColor::from_argb(a, r, g, b)
}

fn fill(color: SkiaColor, opacity: f32) -> Paint {
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(color);
    paint.set_alpha_f(f32::from(color.a()) / 255.0 * opacity);
    paint.set_style(PaintStyle::Fill);
    paint
}

fn stroke(color: SkiaColor, width: f32, opacity: f32) -> Paint {
    let mut paint = fill(color, opacity);
    paint.set_style(PaintStyle::Stroke);
    paint.set_stroke_width(width);
    paint.set_stroke_miter(STROKE_MITER_LIMIT);
    paint
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
    (input.keyboard_focused() == Some(node.id)
        && node.style.focus_ring_width > 0.0
        && node.style.focus_ring_color.0[3] > 0)
        .then_some((node.style.focus_ring_color, node.style.focus_ring_width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    use parley::{FontContext, FontStack, LayoutContext, StyleProperty};
    use thing_core::NodeId;

    use crate::{TextInputImeBatch, TextInputPreedit};

    const VARIABLE_FONT: &[u8] = include_bytes!("../tests/data/RobotoSlab-VariableFont_wght.ttf");
    const STATIC_FONT: &[u8] = include_bytes!("../tests/data/Bungee-Regular.ttf");
    const COLLECTION_FONT: &[u8] = include_bytes!("../tests/data/NotoSansCJK-VF-subset.otf.ttc");

    fn shaped_run(
        fonts: &mut FontContext,
        context: &mut LayoutContext<()>,
        family: &str,
        text: &str,
        weight: f32,
    ) -> (parley::FontData, Vec<i16>, Vec<DesignCoordinate>, bool) {
        shaped_run_with_variations(fonts, context, family, text, weight, &[])
    }

    fn shaped_run_with_variations(
        fonts: &mut FontContext,
        context: &mut LayoutContext<()>,
        family: &str,
        text: &str,
        weight: f32,
        variations: &[parley::FontVariation],
    ) -> (parley::FontData, Vec<i16>, Vec<DesignCoordinate>, bool) {
        let mut builder = context.ranged_builder(fonts, text, 1.0, false);
        builder.push_default(StyleProperty::FontStack(FontStack::Source(Cow::Borrowed(
            family,
        ))));
        builder.push_default(StyleProperty::FontWeight(parley::FontWeight::new(weight)));
        if !variations.is_empty() {
            builder.push_default(StyleProperty::FontVariations(parley::FontSettings::List(
                Cow::Owned(variations.to_vec()),
            )));
        }
        let mut layout = builder.build(text);
        layout.break_all_lines(None);
        let glyph_run = layout
            .lines()
            .flat_map(|line| line.items())
            .find_map(|item| match item {
                PositionedLayoutItem::GlyphRun(run) => Some(run),
                PositionedLayoutItem::InlineBox(_) => None,
            })
            .unwrap();
        let run = glyph_run.run();
        let synthesis = run.synthesis();
        let design = synthesis
            .variation_settings()
            .iter()
            .map(|(tag, value)| DesignCoordinate {
                axis: FourByteTag::new(u32::from_be_bytes(tag.to_be_bytes())),
                value: *value,
            })
            .collect();
        (
            run.font().clone(),
            run.normalized_coords().to_vec(),
            design,
            synthesis.embolden(),
        )
    }

    fn variable_run(
        fonts: &mut FontContext,
        context: &mut LayoutContext<()>,
        weight: f32,
    ) -> (parley::FontData, Vec<i16>, Vec<DesignCoordinate>) {
        let (font, normalized, design, _) =
            shaped_run(fonts, context, "Roboto Slab", "Variable", weight);
        (font, normalized, design)
    }

    fn axis_value(typeface: &Typeface, axis: FourByteTag) -> f32 {
        typeface
            .variation_design_position()
            .unwrap()
            .into_iter()
            .find(|coordinate| coordinate.axis == axis)
            .unwrap()
            .value
    }

    #[test]
    fn path_fits_match_the_existing_renderer() {
        let view_box = ViewBox::new(-10.0, 20.0, 20.0, 10.0).unwrap();
        let area = Rect {
            x: 100.0,
            y: 50.0,
            width: 100.0,
            height: 100.0,
        };
        let contain = PathTransform::new(view_box, area, PathFit::Contain).unwrap();
        assert_eq!(contain.point(-10.0, 20.0), Some(Point::new(100.0, 75.0)));
        assert_eq!(contain.point(10.0, 30.0), Some(Point::new(200.0, 125.0)));
        let cover = PathTransform::new(view_box, area, PathFit::Cover).unwrap();
        assert_eq!(cover.point(-10.0, 20.0), Some(Point::new(50.0, 50.0)));
        assert_eq!(cover.point(10.0, 30.0), Some(Point::new(250.0, 150.0)));
        let fill = PathTransform::new(view_box, area, PathFit::Fill).unwrap();
        assert_eq!(fill.point(-10.0, 20.0), Some(Point::new(100.0, 50.0)));
        assert_eq!(fill.point(10.0, 30.0), Some(Point::new(200.0, 150.0)));
    }

    #[test]
    fn paint_alpha_and_miter_match_the_existing_renderer() {
        let color = SkiaColor::from_argb(128, 20, 40, 60);
        let paint = fill(color, 0.5);
        assert!((paint.alpha_f() - (128.0 / 255.0 * 0.5)).abs() < f32::EPSILON);

        let paint = stroke(color, 2.0, 0.5);
        assert_eq!(paint.stroke_miter(), STROKE_MITER_LIMIT);
    }

    #[test]
    fn node_opacity_is_applied_to_each_draw() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((3, 3)).unwrap();
        let bounds = SkiaRect::from_xywh(0.0, 0.0, 3.0, 3.0);
        surface
            .canvas()
            .draw_rect(bounds, &fill(SkiaColor::from_argb(255, 255, 0, 0), 0.5));
        surface
            .canvas()
            .draw_rect(bounds, &fill(SkiaColor::from_argb(255, 0, 0, 255), 0.5));

        let pixel = surface.peek_pixels().unwrap().get_color((1, 1));
        assert!((190..=192).contains(&pixel.a()));
        assert!((80..=90).contains(&pixel.r()));
        assert!((165..=175).contains(&pixel.b()));
    }

    #[test]
    fn variable_typefaces_use_design_values_and_normalized_cache_keys() {
        let mut fonts = FontContext::default();
        let registered = fonts
            .collection
            .register_fonts(VARIABLE_FONT.to_vec().into(), None);
        assert!(!registered.is_empty());
        let mut context = LayoutContext::new();
        let (source, middle_coords, middle_design) = variable_run(&mut fonts, &mut context, 500.0);
        let (_, heavy_coords, heavy_design) = variable_run(&mut fonts, &mut context, 900.0);
        assert_ne!(middle_coords, heavy_coords);
        assert_ne!(middle_coords[0], 500);

        let manager = FontMgr::default();
        let mut cache = TypefaceCache::default();
        let middle = cache
            .resolve(&manager, &source, &middle_coords, &middle_design)
            .unwrap();
        let heavy = cache
            .resolve(&manager, &source, &heavy_coords, &heavy_design)
            .unwrap();
        let weight = VariationCoordinate::wght;
        assert!((axis_value(&middle, weight) - 500.0).abs() < 0.01);
        assert!((axis_value(&heavy, weight) - 900.0).abs() < 0.01);
        assert_ne!(middle.unique_id(), heavy.unique_id());

        let cached = cache
            .resolve(&manager, &source, &middle_coords, &middle_design)
            .unwrap();
        assert_eq!(middle.unique_id(), cached.unique_id());
        let middle_width = Font::from_typeface(middle, 40.0)
            .measure_str("Variable", None)
            .0;
        let heavy_width = Font::from_typeface(heavy, 40.0)
            .measure_str("Variable", None)
            .0;
        assert_ne!(middle_width, heavy_width);
    }

    #[test]
    fn explicit_variation_overrides_do_not_use_stale_synthesis_values() {
        let mut fonts = FontContext::default();
        let registered = fonts
            .collection
            .register_fonts(VARIABLE_FONT.to_vec().into(), None);
        assert!(!registered.is_empty());
        let mut context = LayoutContext::new();
        let variation = parley::FontVariation {
            tag: u32::from_be_bytes(*b"wght"),
            value: 900.0,
        };
        let (source, normalized, design, _) = shaped_run_with_variations(
            &mut fonts,
            &mut context,
            "Roboto Slab",
            "Variable",
            500.0,
            &[variation],
        );
        let (_, heavy, _) = variable_run(&mut fonts, &mut context, 900.0);
        assert_eq!(normalized, heavy);
        assert_eq!(design.len(), 1);
        assert!((design[0].value - 500.0).abs() < f32::EPSILON);

        let manager = FontMgr::default();
        let error = TypefaceCache::default()
            .resolve(&manager, &source, &normalized, &design)
            .expect_err("an explicit override is not represented by synthesis settings");
        assert!(error.contains("not represented by its synthesis settings"));
    }

    #[test]
    fn static_bold_fallback_emboldens_the_skia_font() {
        let mut fonts = FontContext::default();
        let registered = fonts
            .collection
            .register_fonts(STATIC_FONT.to_vec().into(), None);
        assert!(!registered.is_empty());
        let mut context = LayoutContext::new();
        let (_, regular_coords, regular_design, regular_embolden) =
            shaped_run(&mut fonts, &mut context, "Bungee", "abc", 400.0);
        let (source, bold_coords, bold_design, bold_embolden) =
            shaped_run(&mut fonts, &mut context, "Bungee", "abc", 900.0);
        assert!(regular_coords.is_empty());
        assert!(regular_design.is_empty());
        assert!(!regular_embolden);
        assert!(bold_coords.is_empty());
        assert!(bold_design.is_empty());
        assert!(bold_embolden);

        let manager = FontMgr::default();
        let typeface = TypefaceCache::default()
            .resolve(&manager, &source, &bold_coords, &bold_design)
            .unwrap();
        let font = configured_font(typeface, 24.0, bold_embolden);
        assert!(font.is_embolden());
    }

    #[test]
    fn varied_collection_faces_keep_their_collection_index() {
        let first = parley::FontData::new(COLLECTION_FONT.to_vec().into(), 3);
        let second = parley::FontData::new(first.data.clone(), 4);
        let normalized = [16_384];
        let design = [DesignCoordinate {
            axis: VariationCoordinate::wght,
            value: 900.0,
        }];
        let manager = FontMgr::default();
        let mut cache = TypefaceCache::default();
        let default = cache.resolve(&manager, &first, &[0], &[]).unwrap();
        assert!((axis_value(&default, VariationCoordinate::wght) - 100.0).abs() < 0.01);
        let first = cache
            .resolve(&manager, &first, &normalized, &design)
            .unwrap();
        let second = cache
            .resolve(&manager, &second, &normalized, &design)
            .unwrap();
        assert_eq!(first.family_name(), "Noto Sans CJK TC");
        assert_eq!(second.family_name(), "Noto Sans CJK HK");
        assert!((axis_value(&first, VariationCoordinate::wght) - 900.0).abs() < 0.01);
        assert!((axis_value(&second, VariationCoordinate::wght) - 900.0).abs() < 0.01);
    }

    #[test]
    fn typeface_cache_has_a_fixed_entry_limit() {
        let manager = FontMgr::default();
        let source = parley::FontData::new(VARIABLE_FONT.to_vec().into(), 0);
        let typeface = manager
            .new_from_bytes(source.data.as_ref(), Some(0))
            .unwrap();
        let mut cache = TypefaceCache::default();
        for data in 0..=TYPEFACE_CACHE_LIMIT as u64 {
            cache.insert(
                TypefaceKey {
                    data,
                    index: 0,
                    normalized: Box::new([]),
                },
                typeface.clone(),
            );
        }
        assert_eq!(cache.entries.len(), TYPEFACE_CACHE_LIMIT);
        assert!(
            cache
                .get(&TypefaceKey {
                    data: 0,
                    index: 0,
                    normalized: Box::new([]),
                })
                .is_none()
        );
        assert!(
            cache
                .get(&TypefaceKey {
                    data: TYPEFACE_CACHE_LIMIT as u64,
                    index: 0,
                    normalized: Box::new([]),
                })
                .is_some()
        );
    }

    #[test]
    fn focused_text_input_geometry_uses_content_padding_and_editor_scroll() {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "a long value that needs horizontal scrolling".into();
        node.style.padding_x = Some(4.0);
        node.style.padding_y = Some(3.0);
        let content = text_input_content(
            &node,
            Rect {
                x: 10.0,
                y: 20.0,
                width: 48.0,
                height: 30.0,
            },
        );
        assert_eq!(
            content,
            Rect {
                x: 14.0,
                y: 23.0,
                width: 40.0,
                height: 24.0,
            }
        );

        let mut editor = TextInputEditor::new(&node, content.width);
        editor.prepare(&node, content.width);
        let offset_x = editor.text_offset_x();
        assert!(offset_x < 0.0);
        let offset_y = ((content.height - editor.layout().height()) / 2.0).max(0.0);
        let caret = editor_rect(
            content,
            offset_x,
            offset_y,
            editor.cursor_geometry().unwrap(),
        );
        assert!((caret.width - 1.0).abs() < f32::EPSILON);
        assert!((caret.x + caret.width - (content.x + content.width)).abs() < 0.01);

        editor.move_left(false, false);
        let caret = editor_rect(
            content,
            editor.text_offset_x(),
            offset_y,
            editor.cursor_geometry().unwrap(),
        );
        assert!(caret.x >= content.x);
        assert!(caret.x < content.x + content.width);

        editor.move_to_start(false);
        assert_eq!(editor.text_offset_x(), 0.0);
    }

    #[test]
    fn focused_text_input_preedit_uses_display_state_and_one_pixel_underline() {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.placeholder = "placeholder".into();
        let content = Rect {
            x: 14.0,
            y: 23.0,
            width: 120.0,
            height: 24.0,
        };
        let mut editor = TextInputEditor::new(&node, content.width);
        editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "候補".into(),
                    cursor: Some((6, 0)),
                }),
                ..TextInputImeBatch::default()
            })
            .unwrap();
        editor.prepare(&node, content.width);

        assert!(node.text.is_empty());
        assert!(!editor.display_is_empty());
        assert_eq!(editor.value(), "");
        let offset_y = ((content.height - editor.layout().height()) / 2.0).max(0.0);
        let bounds = editor.preedit_geometry()[0].0;
        let underline = preedit_underline_rect(content, editor.text_offset_x(), offset_y, bounds);
        assert_eq!(underline.height, 1.0);
        assert!(underline.width > 0.0);
        assert_eq!(underline.y, content.y + offset_y + bounds.y1 as f32 - 1.0);
    }

    #[test]
    fn text_input_alignment_follows_the_paragraph_direction() {
        let mut text = TextSystem::default();
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "abc".into();
        let ltr = text.layout(&node, None);
        let ltr_width = ltr.full_width() + 20.0;
        assert!((text_alignment_offset(ltr, ltr_width, TextAlign::End) - 20.0).abs() < 0.01);
        assert_eq!(text_alignment_offset(ltr, ltr_width, TextAlign::Start), 0.0);

        node.text = "אבג".into();
        let rtl = text.layout(&node, None);
        let rtl_width = rtl.full_width() + 20.0;
        assert!((text_alignment_offset(rtl, rtl_width, TextAlign::Start) - 20.0).abs() < 0.01);
        assert_eq!(text_alignment_offset(rtl, rtl_width, TextAlign::End), 0.0);
    }
}
