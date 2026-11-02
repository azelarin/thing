//! raster witnesses for image blur and saturation.

use std::{
    thread,
    time::{Duration, Instant},
};

use skia_safe::{Color as SkiaColor, FontMgr, Surface};
use thing_core::{
    ClipMode, CrossAlign, Image, ImageCandidate, ImageEffects, ImageFit, ImagePixmap, ImageSource,
    Kind, MainAlign, Node, NodeId,
};

use super::{Scene, TypefaceCache};
use crate::{Interaction, Layout, PaintFrame, Size, TextSystem, images::Images};

/// an in-memory image whose pixels come from `pixel(x, y)` as rgba.
fn image(id: &str, width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Image {
    let mut rgba = Vec::new();
    for y in 0..height {
        for x in 0..width {
            rgba.extend(pixel(x, y));
        }
    }
    let pixmap = ImagePixmap::new(width, height, rgba).unwrap();
    let candidate = ImageCandidate::new(None, vec![pixmap]).unwrap();
    Image {
        memory: Some(ImageSource::new(id.into(), vec![candidate]).unwrap()),
        ..Image::default()
    }
}

fn solid(id: &str, rgba: [u8; 4]) -> Image {
    image(id, 8, 8, |_, _| rgba)
}

/// black on the left half and white on the right, both opaque.
fn edge(id: &str) -> Image {
    image(id, 40, 20, |x, _| {
        if x < 20 {
            [0, 0, 0, 255]
        } else {
            [255, 255, 255, 255]
        }
    })
}

fn picture(source: Image, width: f32, height: f32, effects: ImageEffects) -> Node {
    let mut node = Node::new(NodeId(2), Kind::Image);
    node.image = source;
    node.image_effects = effects;
    node.style.width = Some(width);
    node.style.height = Some(height);
    node
}

fn stack(id: u64, children: Vec<Node>) -> Node {
    let mut node = Node::new(NodeId(id), Kind::Stack);
    node.style.align = CrossAlign::Start;
    node.style.justify = MainAlign::Start;
    node.children = children;
    node
}

fn effects(blur: f32, saturation: f32) -> ImageEffects {
    ImageEffects { blur, saturation }
}

/// draws until every image the scene asked for has been decoded.
fn paint(root: &Node, size: (i32, i32), scale: f32, images: &mut Images) -> Surface {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let surface = draw(root, size, scale, images).unwrap();
        let (requests, settled) = images.request_state();
        if requests > 0 && settled {
            return surface;
        }
        assert!(Instant::now() < deadline, "images never finished decoding");
        thread::sleep(Duration::from_millis(5));
    }
}

fn draw(
    root: &Node,
    (width, height): (i32, i32),
    scale: f32,
    images: &mut Images,
) -> Result<Surface, String> {
    let mut text = TextSystem::default();
    let layout = Layout::compute(
        root,
        Size {
            width: width as f32 / scale,
            height: height as f32 / scale,
        },
        &mut text,
    )?;
    let mut surface = skia_safe::surfaces::raster_n32_premul((width, height)).unwrap();
    Scene {
        typefaces: &mut TypefaceCache::default(),
        fonts: &FontMgr::default(),
        text: &mut text,
        images,
    }
    .draw(
        surface.canvas(),
        PaintFrame {
            root,
            layout: &layout,
            physical: (width as u32, height as u32),
            scale,
            input: Interaction::default(),
            text_input: None,
            secret_input: None,
            inspector: None,
        },
    )?;
    Ok(surface)
}

fn render(root: &Node, size: (i32, i32), scale: f32) -> Surface {
    paint(root, size, scale, &mut Images::new(|| {}).unwrap())
}

fn pixel(surface: &mut Surface, x: i32, y: i32) -> SkiaColor {
    surface.peek_pixels().unwrap().get_color((x, y))
}

fn bytes(surface: &mut Surface) -> Vec<u8> {
    surface.peek_pixels().unwrap().bytes().unwrap().to_vec()
}

fn spread(color: SkiaColor) -> i32 {
    let channels = [color.r(), color.g(), color.b()].map(i32::from);
    channels.iter().max().unwrap() - channels.iter().min().unwrap()
}

fn near(actual: u8, expected: u8, tolerance: u8) {
    assert!(
        actual.abs_diff(expected) <= tolerance,
        "{actual} is not within {tolerance} of {expected}"
    );
}

#[test]
fn neutral_effects_keep_identical_pixels() {
    let source = edge("neutral");
    let plain = stack(
        1,
        vec![picture(source.clone(), 40.0, 20.0, effects(0.0, 1.0))],
    );
    let mut plain = render(&plain, (40, 20), 1.0);
    let tiny = stack(
        1,
        vec![picture(source, 40.0, 20.0, effects(f32::from_bits(1), 1.0))],
    );
    let mut tiny = render(&tiny, (40, 20), 1.0);
    assert_eq!(bytes(&mut plain), bytes(&mut tiny));
    assert_eq!(pixel(&mut plain, 19, 10), SkiaColor::BLACK);
    assert_eq!(pixel(&mut plain, 20, 10), SkiaColor::WHITE);
}

#[test]
fn saturation_adjusts_color_and_preserves_alpha() {
    let source = solid("tinted", [200, 60, 60, 128]);
    let at = |saturation| {
        let root = stack(
            1,
            vec![picture(source.clone(), 8.0, 8.0, effects(0.0, saturation))],
        );
        let mut surface = render(&root, (8, 8), 1.0);
        pixel(&mut surface, 4, 4)
    };
    let neutral = at(1.0);
    let gray = at(0.0);
    let reduced = at(0.5);
    let increased = at(2.0);
    for color in [gray, reduced, increased] {
        near(color.a(), neutral.a(), 1);
    }
    // unpremultiplying a half-transparent pixel can round channels apart.
    assert!(spread(gray) <= 2, "{gray:?}");
    assert!(
        0 < spread(reduced) && spread(reduced) < spread(neutral),
        "{reduced:?}"
    );
    assert!(spread(increased) > spread(neutral), "{increased:?}");
}

#[test]
fn blur_softens_a_sharp_edge_and_keeps_image_edges_opaque() {
    let root = stack(
        1,
        vec![picture(edge("blurred"), 40.0, 20.0, effects(8.0, 1.0))],
    );
    let mut surface = render(&root, (40, 20), 1.0);
    let left = pixel(&mut surface, 18, 10);
    let right = pixel(&mut surface, 21, 10);
    assert!(30 < left.r() && left.r() < 128, "{left:?}");
    assert!(128 < right.r() && right.r() < 225, "{right:?}");
    // the blur extends edge pixels instead of fading into transparency.
    for (x, y) in [(0, 0), (39, 0), (0, 19), (39, 19)] {
        assert_eq!(pixel(&mut surface, x, y).a(), 255, "({x}, {y})");
    }
    near(pixel(&mut surface, 1, 10).r(), 0, 4);
    near(pixel(&mut surface, 38, 10).r(), 255, 4);
}

#[test]
fn blur_and_opacity_apply_opacity_once() {
    let source = solid("faded", [255, 255, 255, 255]);
    let at = |blur| {
        let mut node = picture(source.clone(), 8.0, 8.0, effects(blur, 1.0));
        node.style.opacity = 0.5;
        let mut surface = render(&stack(1, vec![node]), (8, 8), 1.0);
        pixel(&mut surface, 4, 4).a()
    };
    let plain = at(0.0);
    near(plain, 128, 1);
    near(at(6.0), plain, 1);
}

#[test]
fn blurred_images_stay_inside_rounded_and_ancestor_clips() {
    let mut rounded = picture(
        solid("rounded", [255, 0, 0, 255]),
        40.0,
        40.0,
        effects(8.0, 0.0),
    );
    rounded.style.radius = 20.0;
    let mut surface = render(&stack(1, vec![rounded]), (40, 40), 1.0);
    assert_eq!(pixel(&mut surface, 1, 1).a(), 0);
    assert_eq!(pixel(&mut surface, 38, 38).a(), 0);
    let center = pixel(&mut surface, 20, 20);
    assert_eq!(center.a(), 255);
    assert!(spread(center) <= 1, "{center:?}");

    // a 20 by 20 parent clips the 40 by 20 image, including the blur's reach.
    let mut child = picture(
        solid("clipped", [255, 255, 255, 255]),
        40.0,
        20.0,
        effects(16.0, 1.0),
    );
    child.style.shrink = Some(0.0);
    let mut parent = stack(3, vec![child]);
    parent.style.width = Some(20.0);
    parent.style.height = Some(20.0);
    parent.style.clip = ClipMode::Bounds;
    let mut surface = render(&stack(1, vec![parent]), (40, 20), 1.0);
    assert_eq!(pixel(&mut surface, 10, 10).a(), 255);
    assert_eq!(pixel(&mut surface, 19, 10).a(), 255);
    assert_eq!(pixel(&mut surface, 21, 10).a(), 0);
    assert_eq!(pixel(&mut surface, 35, 10).a(), 0);
}

#[test]
fn fit_modes_keep_their_geometry_with_effects() {
    // a 2:1 image, red on the left and blue on the right, in a square box.
    let wide = image("wide", 40, 20, |x, _| {
        if x < 20 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    let at = |fit, blur| {
        let mut node = picture(wide.clone(), 40.0, 40.0, effects(blur, 1.0));
        node.image_fit = fit;
        render(&stack(1, vec![node]), (40, 40), 1.0)
    };
    for blur in [0.0, 4.0] {
        // contain paints a centered 40 by 20 band.
        let mut contain = at(ImageFit::Contain, blur);
        assert_eq!(pixel(&mut contain, 20, 5).a(), 0, "blur {blur}");
        assert_eq!(pixel(&mut contain, 20, 34).a(), 0, "blur {blur}");
        assert_eq!(pixel(&mut contain, 2, 20).a(), 255, "blur {blur}");
        assert!(pixel(&mut contain, 2, 20).r() > 200, "blur {blur}");
        // cover crops the middle, so both colors fill the whole box.
        let mut cover = at(ImageFit::Cover, blur);
        for (x, y) in [(0, 0), (39, 39)] {
            assert_eq!(pixel(&mut cover, x, y).a(), 255, "blur {blur}");
        }
        assert!(pixel(&mut cover, 1, 20).r() > 200, "blur {blur}");
        assert!(pixel(&mut cover, 38, 20).b() > 200, "blur {blur}");
        // fill stretches the image across the box.
        let mut fill = at(ImageFit::Fill, blur);
        assert_eq!(pixel(&mut fill, 20, 1).a(), 255, "blur {blur}");
        assert!(pixel(&mut fill, 1, 1).r() > 200, "blur {blur}");
        assert!(pixel(&mut fill, 38, 38).b() > 200, "blur {blur}");
    }
}

#[test]
fn transformed_images_blur_inside_their_rotated_box_at_fractional_scale() {
    // a 20 by 20 image rotated 45 degrees about its center, drawn at 1.5x.
    let mut node = picture(
        solid("turned", [255, 255, 255, 255]),
        20.0,
        20.0,
        effects(4.0, 1.0),
    );
    node.style.rotation = 45.0;
    let mut root = stack(1, vec![node]);
    root.style.padding = 10.0;
    let mut surface = render(&root, (60, 60), 1.5);
    // the center stays opaque and the unrotated corners lie outside the clip.
    assert_eq!(pixel(&mut surface, 30, 30).a(), 255);
    for (x, y) in [(17, 17), (42, 17), (17, 42), (42, 42)] {
        assert_eq!(pixel(&mut surface, x, y).a(), 0, "({x}, {y})");
    }
    // the rotated tips reach beyond the unrotated box.
    assert!(pixel(&mut surface, 30, 12).a() > 128);
    assert!(pixel(&mut surface, 12, 30).a() > 128);
}

#[test]
fn device_sigma_counts_output_scale_and_transforms() {
    let mut text = TextSystem::default();
    let check = |value: ImageEffects, scale_x: f32, scale: f32, text: &mut TextSystem| {
        let mut node = picture(edge("limit"), 20.0, 20.0, value);
        node.style.scale_x = scale_x;
        let root = stack(1, vec![node]);
        let layout = Layout::compute(
            &root,
            Size {
                width: 40.0,
                height: 40.0,
            },
            text,
        )
        .unwrap();
        let physical = ((40.0 * scale).ceil() as u32, (40.0 * scale).ceil() as u32);
        crate::validate_image_effects(&root, &layout, physical, scale)
    };
    // sigma 32 times 2 times 2 is the 128 pixel limit; one more step is not.
    assert!(check(effects(64.0, 1.0), 2.0, 2.0, &mut text).is_ok());
    let error = check(effects(64.0, 1.0), 2.0, 2.5, &mut text).unwrap_err();
    assert!(error.contains("128 pixel device sigma"), "{error}");
    let error = check(effects(64.0, 1.0), 2.5, 2.0, &mut text).unwrap_err();
    assert!(error.contains("128 pixel device sigma"), "{error}");
    // the painter rejects the same frame rather than drawing it.
    let mut node = picture(edge("direct"), 20.0, 20.0, effects(64.0, 1.0));
    node.style.scale_x = 5.0;
    let error = draw(
        &stack(1, vec![node]),
        (40, 40),
        1.0,
        &mut Images::new(|| {}).unwrap(),
    )
    .err()
    .unwrap();
    assert!(error.contains("128 pixel device sigma"), "{error}");
    // saturation alone needs no blur budget, and an invisible effect is skipped.
    assert!(check(effects(0.0, 0.0), 100.0, 4.0, &mut text).is_ok());
    let mut hidden = picture(edge("hidden"), 20.0, 20.0, effects(64.0, 1.0));
    hidden.style.scale_x = 5.0;
    hidden.style.opacity = 0.0;
    let root = stack(1, vec![hidden]);
    let layout = Layout::compute(
        &root,
        Size {
            width: 40.0,
            height: 40.0,
        },
        &mut text,
    )
    .unwrap();
    assert!(crate::validate_image_effects(&root, &layout, (40, 40), 1.0).is_ok());
}

#[test]
fn frame_scratch_estimates_are_bounded() {
    let mut text = TextSystem::default();
    let frame = |count: u64, text: &mut TextSystem| {
        let children = (0..count)
            .map(|index| {
                let mut node = picture(edge("budget"), 400.0, 400.0, effects(8.0, 1.0));
                node.id = NodeId(10 + index);
                node
            })
            .collect();
        let root = stack(1, children);
        let layout = Layout::compute(
            &root,
            Size {
                width: 400.0,
                height: 400.0,
            },
            text,
        )
        .unwrap();
        crate::validate_image_effects(&root, &layout, (400, 400), 1.0)
    };
    // each full-surface blur charges about one surface of scratch; a frame
    // allows four.
    assert!(frame(3, &mut text).is_ok());
    let error = frame(5, &mut text).unwrap_err();
    assert!(error.contains("frame rendering budget"), "{error}");
}

#[test]
fn a_thin_device_clip_does_not_hide_a_large_rotated_blur_layer() {
    let mut child = picture(edge("rotated-budget"), 4096.0, 4096.0, effects(8.0, 1.0));
    child.style.shrink = Some(0.0);
    child.style.rotation = 45.0;
    child.style.transform_origin.x = 0.0;
    child.style.transform_origin.y = 0.0;
    let mut root = stack(1, vec![child]);
    root.style.width = Some(10.0);
    root.style.height = Some(2048.0);
    // skia blurs in an axis-aligned intermediate, then rotates it into the
    // narrow device clip. nonuniform ancestor scales also shear that mapping.
    for ancestor_scale in [1.0, 2.0, 0.001] {
        root.style.scale_x = ancestor_scale;
        let layout = Layout::compute(
            &root,
            Size {
                width: 10.0,
                height: 2048.0,
            },
            &mut TextSystem::default(),
        )
        .unwrap();
        for scale in [1.0, 1.25] {
            let physical = (
                (10.0_f32 * scale).ceil() as u32,
                (2048.0_f32 * scale).ceil() as u32,
            );
            let error = crate::validate_image_effects(&root, &layout, physical, scale).unwrap_err();
            assert!(
                error.contains("per-effect budget"),
                "{ancestor_scale}, {scale}: {error}"
            );
        }
    }
}

#[test]
fn changing_effects_reuses_the_decoded_source() {
    let source = image("reused", 40, 20, |x, _| {
        if x < 20 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    let mut images = Images::new(|| {}).unwrap();
    let root = |effects| stack(1, vec![picture(source.clone(), 40.0, 20.0, effects)]);
    let mut plain = paint(&root(ImageEffects::default()), (40, 20), 1.0, &mut images);
    let (requests, _) = images.request_state();
    let mut changed = Vec::new();
    for value in [effects(8.0, 1.0), effects(0.0, 0.0), effects(32.0, 2.0)] {
        let mut surface = draw(&root(value), (40, 20), 1.0, &mut images).unwrap();
        // the cached source is ready on the first draw; nothing new is queued.
        assert_eq!(images.request_state(), (requests, true));
        changed.push(bytes(&mut surface));
    }
    assert!(changed.iter().all(|pixels| *pixels != bytes(&mut plain)));
}
