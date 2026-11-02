use skia_safe::Color as SkiaColor;
use thing_core::{Color, CrossAlign, Kind, MainAlign, Node, NodeId};

use super::tests::raster_scene;
use crate::{Layout, Size, TextSystem, validate_compositing, validate_image_effects};

fn box_node(id: u64, width: f32, height: f32, color: Color) -> Node {
    let mut node = Node::new(NodeId(id), Kind::Stack);
    node.style.width = Some(width);
    node.style.height = Some(height);
    node.style.align = CrossAlign::Start;
    node.style.justify = MainAlign::Start;
    node.style.background = color.into();
    node
}

fn pair(id: u64, color: Color) -> Node {
    let mut group = box_node(id, 120.0, 60.0, Color::TRANSPARENT);
    group.children.push(box_node(id + 1, 80.0, 40.0, color));
    let mut right = box_node(id + 2, 80.0, 40.0, color);
    right.style.translate_x = 40.0;
    group.children.push(right);
    group
}

fn fade() -> Node {
    let mut node = box_node(1, 160.0, 80.0, Color::TRANSPARENT);
    node.kind = Kind::Crossfade;
    node.crossfade_progress = 0.5;
    node.children = vec![
        pair(2, Color([255, 0, 0, 255])),
        pair(5, Color([0, 0, 255, 255])),
    ];
    node
}

fn bytes(root: &Node) -> Vec<u8> {
    raster_scene(root, 160, 80, 1.0)
        .unwrap()
        .peek_pixels()
        .unwrap()
        .bytes()
        .unwrap()
        .to_vec()
}

fn endpoint(root: &Node, slot: usize) -> Node {
    let mut reference = root.clone();
    reference.kind = Kind::Stack;
    reference.crossfade_progress = 0.0;
    reference.style.composite = true;
    reference.children = vec![reference.children.remove(slot)];
    reference
}

#[test]
fn opaque_groups_blend_without_adding_the_containers_background() {
    for scale in [1.0, 1.25] {
        let mut root = fade();
        root.style.background = Color([0, 255, 0, 255]).into();
        let mut image =
            raster_scene(&root, (160.0 * scale) as i32, (80.0 * scale) as i32, scale).unwrap();
        let pixels = image.peek_pixels().unwrap();
        let sample = |x, y| pixels.get_color(((x * scale) as i32, (y * scale) as i32));
        let left = sample(20.0, 20.0);
        assert_eq!(left, sample(60.0, 20.0));
        assert_eq!(left, sample(100.0, 20.0));
        assert!((i32::from(left.r()) - 128).abs() <= 2, "{left:?}");
        assert!((i32::from(left.b()) - 128).abs() <= 2, "{left:?}");
        assert_eq!(left.g(), 0);
        assert_eq!(left.a(), 255);
        assert_eq!(sample(140.0, 70.0), SkiaColor::GREEN);
    }
}

#[test]
fn transparent_groups_with_different_footprints_have_linear_alpha_and_exact_endpoints() {
    let mut root = fade();
    root.children[0] = box_node(2, 60.0, 40.0, Color([255, 0, 0, 128]));
    root.children[1] = box_node(3, 60.0, 40.0, Color([0, 0, 255, 255]));
    root.children[1].style.translate_x = 30.0;
    let mut image = raster_scene(&root, 160, 80, 1.0).unwrap();
    let pixels = image.peek_pixels().unwrap();
    for (point, alpha) in [((15, 20), 64), ((45, 20), 192), ((75, 20), 128)] {
        let actual = pixels.get_color(point);
        assert!(
            (i32::from(actual.a()) - alpha).abs() <= 2,
            "{point:?}: {actual:?}"
        );
    }
    let overlap = pixels.get_color((45, 20));
    assert!((i32::from(overlap.r()) - 85).abs() <= 2, "{overlap:?}");
    assert!((i32::from(overlap.b()) - 170).abs() <= 2, "{overlap:?}");
    for (progress, slot) in [(0.0, 0), (1.0, 1)] {
        root.crossfade_progress = progress;
        assert_eq!(bytes(&root), bytes(&endpoint(&root, slot)));
    }
}

#[test]
fn mixing_identical_rounded_edges_keeps_the_original_coverage() {
    let mut root = fade();
    let mut card = box_node(2, 80.0, 60.0, Color([255, 0, 0, 255]));
    card.style.radius = 20.0;
    card.style.clip = thing_core::ClipMode::Rounded;
    let mut other = card.clone();
    other.id = NodeId(3);
    root.children = vec![card, other];
    let original = bytes(&endpoint(&root, 0));
    assert!(
        original
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] > 0 && pixel[3] < 255)
    );
    for (actual, expected) in bytes(&root).into_iter().zip(original) {
        assert!(actual.abs_diff(expected) <= 2, "{actual} != {expected}");
    }
}

#[test]
fn child_opacity_clipping_and_nested_groups_match_a_lerp_of_their_ordinary_renders() {
    let mut root = fade();
    root.style.opacity = 0.5;
    root.enabled = false;
    root.style.disabled_opacity = 0.5;
    root.style.background = Color([0, 200, 0, 128]).into();
    root.children[0].style.opacity = 0.5;
    root.children[0].style.rotation = 15.0;
    root.children[0].style.radius = 16.0;
    root.children[0].style.clip = thing_core::ClipMode::Rounded;
    root.children[1].style.opacity = 0.6;
    root.children[1].style.composite = true;
    let left = bytes(&endpoint(&root, 0));
    let right = bytes(&endpoint(&root, 1));
    for progress in [0.25, 0.5, 0.75] {
        root.crossfade_progress = progress;
        for ((actual, left), right) in bytes(&root).into_iter().zip(&left).zip(&right) {
            let expected =
                (f32::from(*left) * (1.0 - progress) + f32::from(*right) * progress).round() as u8;
            assert!(
                actual.abs_diff(expected) <= 3,
                "{progress}: {actual} != {expected}"
            );
        }
    }
}

fn layout(root: &Node) -> Layout {
    Layout::compute(
        root,
        Size {
            width: 160.0,
            height: 80.0,
        },
        &mut TextSystem::default(),
    )
    .unwrap()
}

#[test]
fn a_fully_opaque_crossfade_still_needs_bounded_isolation() {
    let mut root = fade();
    let error = validate_compositing(&root, &layout(&root), (20_000, 20_000), 1.0).unwrap_err();
    assert!(error.contains("physical pixels"), "{error}");
    for progress in [0.0, 1.0] {
        root.crossfade_progress = progress;
        validate_compositing(&root, &layout(&root), (20_000, 20_000), 1.0).unwrap();
    }
}

#[test]
fn zero_weight_effects_cost_nothing_and_hidden_children_keep_their_slot() {
    let mut root = fade();
    let mut image = box_node(8, 20.0, 20.0, Color::TRANSPARENT);
    image.kind = Kind::Image;
    image.image.source = Some("missing.png".into());
    image.image_effects.blur = 64.0;
    image.style.scale_x = 5.0;
    root.children[1] = image;
    root.crossfade_progress = 0.0;
    validate_image_effects(&root, &layout(&root), (160, 80), 1.0).unwrap();
    root.crossfade_progress = 1.0;
    let error = validate_image_effects(&root, &layout(&root), (160, 80), 1.0).unwrap_err();
    assert!(error.contains("128 pixel device sigma"), "{error}");
    let mut root = fade();
    root.children[0].visible = false;
    let mut image = raster_scene(&root, 160, 80, 1.0).unwrap();
    let actual = image.peek_pixels().unwrap().get_color((20, 20));
    assert_eq!(actual.b(), 255);
    assert!((i32::from(actual.a()) - 128).abs() <= 2, "{actual:?}");
}

#[test]
fn nested_crossfades_keep_their_mixtures_isolated() {
    let mut root = fade();
    let inner = root.clone();
    root.id = NodeId(10);
    root.children = vec![inner, box_node(11, 120.0, 60.0, Color([0, 255, 0, 255]))];
    let mut image = raster_scene(&root, 160, 80, 1.0).unwrap();
    let actual = image.peek_pixels().unwrap().get_color((60, 20));
    for (actual, expected) in [(actual.r(), 64), (actual.g(), 128), (actual.b(), 64)] {
        assert!(
            (i32::from(actual) - expected).abs() <= 2,
            "{actual} != {expected}"
        );
    }
    assert_eq!(actual.a(), 255);
}
