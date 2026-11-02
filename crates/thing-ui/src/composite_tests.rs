use thing_core::{Color, CrossAlign, Kind, MainAlign, Node, NodeId, Shadow};

use super::tests::raster_scene;
use crate::{Layout, Measurer, Size, validate_compositing};

fn stack(id: u64, width: f32, height: f32) -> Node {
    let mut node = Node::new(NodeId(id), Kind::Stack);
    node.style.width = Some(width);
    node.style.height = Some(height);
    node.style.align = CrossAlign::Start;
    node.style.justify = MainAlign::Start;
    node
}

fn overlapping(composite: bool) -> Node {
    let mut root = stack(1, 160.0, 100.0);
    root.style.background = Color::WHITE.into();
    let mut group = stack(2, 120.0, 60.0);
    group.style.opacity = 0.5;
    group.style.composite = composite;
    for (id, x) in [(3, 0.0), (4, 40.0)] {
        let mut child = stack(id, 80.0, 40.0);
        child.style.background = Color([255, 0, 0, 255]).into();
        child.style.translate_x = x;
        group.children.push(child);
    }
    root.children.push(group);
    root
}

#[test]
fn one_group_fade_keeps_overlap_at_the_same_opacity() {
    for scale in [1.0, 1.25] {
        let mut grouped = raster_scene(&overlapping(true), 160, 100, scale).unwrap();
        let pixels = grouped.peek_pixels().unwrap();
        let outside = pixels.get_color(((20.0 * scale) as i32, (20.0 * scale) as i32));
        let overlap = pixels.get_color(((60.0 * scale) as i32, (20.0 * scale) as i32));
        assert_eq!(outside, overlap);
        assert_eq!(outside.r(), 255);
        assert!((i32::from(outside.g()) - 128).abs() <= 2, "{outside:?}");
        let mut separate = raster_scene(&overlapping(false), 160, 100, scale).unwrap();
        let pixels = separate.peek_pixels().unwrap();
        assert!(
            pixels
                .get_color(((60.0 * scale) as i32, (20.0 * scale) as i32))
                .g()
                < outside.g()
        );
    }
}

#[test]
fn nested_groups_include_opacity_from_noncomposited_ancestors_once() {
    for outer_composite in [false, true] {
        let mut root = overlapping(true);
        let group = root.children.remove(0);
        let mut outer = stack(5, 120.0, 60.0);
        outer.style.opacity = 0.5;
        outer.style.composite = outer_composite;
        outer.children.push(group);
        root.children.push(outer);
        let mut image = raster_scene(&root, 160, 100, 1.0).unwrap();
        let pixels = image.peek_pixels().unwrap();
        let expected = pixels.get_color((20, 20));
        assert_eq!(expected, pixels.get_color((60, 20)));
        assert!((i32::from(expected.g()) - 191).abs() <= 1, "{expected:?}");
    }
}

#[test]
fn nested_layers_keep_rounded_clipping_and_following_siblings() {
    let mut root = stack(1, 160.0, 100.0);
    root.style.background = Color::WHITE.into();
    let mut card = stack(2, 100.0, 80.0);
    card.style.radius = 20.0;
    card.style.clip = thing_core::ClipMode::Rounded;
    card.style.opacity = 0.5;
    card.style.composite = true;
    let mut child = stack(3, 120.0, 60.0);
    child.style.translate_x = 20.0;
    child.style.translate_y = 10.0;
    child.style.background = Color([255, 0, 0, 255]).into();
    child.style.opacity = 0.5;
    child.style.composite = true;
    card.children.push(child);
    root.children.push(card);
    let mut sibling = stack(4, 40.0, 80.0);
    sibling.style.translate_x = 120.0;
    sibling.style.background = Color([0, 0, 255, 255]).into();
    root.children.push(sibling);
    let mut image = raster_scene(&root, 160, 100, 1.0).unwrap();
    let pixels = image.peek_pixels().unwrap();
    assert_eq!(pixels.get_color((0, 0)), skia_safe::Color::WHITE);
    assert_eq!(pixels.get_color((110, 30)), skia_safe::Color::WHITE);
    assert_eq!(pixels.get_color((99, 69)), skia_safe::Color::WHITE);
    assert_eq!(pixels.get_color((140, 30)), skia_safe::Color::BLUE);
    assert!((i32::from(pixels.get_color((40, 40)).g()) - 191).abs() <= 1);
}

#[test]
fn group_opacity_keeps_shadows_that_escape_the_groups_body() {
    let mut root = stack(1, 100.0, 100.0);
    root.style.background = Color::WHITE.into();
    let mut group = stack(2, 40.0, 20.0);
    group.style.opacity = 0.5;
    group.style.composite = true;
    let mut child = stack(3, 20.0, 20.0);
    child.style.background = Color([255, 0, 0, 255]).into();
    child.style.box_shadow = Some(Shadow {
        color: Color([0, 0, 0, 255]),
        offset: thing_core::Point { x: 0.0, y: 20.0 },
        blur: 0.0,
        spread: 0.0,
    });
    group.children.push(child);
    root.children.push(group);
    let mut image = raster_scene(&root, 100, 100, 1.0).unwrap();
    let shadow = image.peek_pixels().unwrap().get_color((10, 30));
    assert!((i32::from(shadow.r()) - 128).abs() <= 2, "{shadow:?}");
    assert_eq!(shadow.r(), shadow.g());
    assert_eq!(shadow.g(), shadow.b());
}

#[test]
fn disabling_a_group_dims_the_combined_contents_once() {
    let mut root = overlapping(true);
    root.children[0].enabled = false;
    root.children[0].style.disabled_opacity = 0.5;
    let mut image = raster_scene(&root, 160, 100, 1.0).unwrap();
    let pixels = image.peek_pixels().unwrap();
    assert_eq!(pixels.get_color((20, 20)), pixels.get_color((60, 20)));
    assert!((i32::from(pixels.get_color((20, 20)).g()) - 191).abs() <= 1);
}

#[test]
fn a_clipping_ancestor_contains_composited_content_and_shadows() {
    let mut root = stack(1, 160.0, 100.0);
    root.style.background = Color::WHITE.into();
    let mut ancestor = stack(2, 100.0, 80.0);
    ancestor.style.clip = thing_core::ClipMode::Rounded;
    ancestor.style.radius = 20.0;
    let mut group = stack(3, 120.0, 60.0);
    group.style.composite = true;
    group.style.opacity = 0.5;
    group.style.translate_y = 10.0;
    let mut child = stack(4, 80.0, 40.0);
    child.style.translate_x = 60.0;
    child.style.background = Color([255, 0, 0, 255]).into();
    child.style.box_shadow = Some(Shadow {
        offset: thing_core::Point { x: 0.0, y: 20.0 },
        color: Color([0, 0, 0, 255]),
        blur: 0.0,
        spread: 0.0,
    });
    group.children.push(child);
    ancestor.children.push(group);
    root.children.push(ancestor);
    let mut surface = raster_scene(&root, 160, 100, 1.0).unwrap();
    let pixels = surface.peek_pixels().unwrap();
    for point in [(99, 11), (110, 40), (110, 65), (99, 79)] {
        assert_eq!(
            pixels.get_color(point),
            skia_safe::Color::WHITE,
            "{point:?}"
        );
    }
    let shadow = pixels.get_color((90, 65));
    assert!((i32::from(shadow.r()) - 128).abs() <= 2, "{shadow:?}");
    assert_eq!(shadow.r(), shadow.g());
    assert_eq!(shadow.g(), shadow.b());
}

#[test]
fn a_disabled_ancestor_dims_nested_groups_without_repeating_disabled_opacity() {
    let mut root = overlapping(true);
    let mut group = root.children.remove(0);
    group.enabled = false;
    group.style.disabled_opacity = 0.1;
    let mut ancestor = stack(5, 120.0, 60.0);
    ancestor.enabled = false;
    ancestor.style.disabled_opacity = 0.5;
    ancestor.children.push(group);
    root.children.push(ancestor);
    let mut surface = raster_scene(&root, 160, 100, 1.0).unwrap();
    let pixels = surface.peek_pixels().unwrap();
    let outside = pixels.get_color((20, 20));
    assert_eq!(outside, pixels.get_color((60, 20)));
    assert!((i32::from(outside.g()) - 191).abs() <= 2, "{outside:?}");
}

struct NoText;

impl Measurer for NoText {
    fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
        Size::default()
    }
}

fn layout(root: &Node) -> Layout {
    Layout::compute(
        root,
        Size {
            width: 160.0,
            height: 100.0,
        },
        &mut NoText,
    )
    .unwrap()
}

#[test]
fn oversized_or_deep_layers_are_refused_before_allocation() {
    let root = overlapping(true);
    let error = validate_compositing(&root, &layout(&root), (16_384, 16_384)).unwrap_err();
    assert!(error.contains("budget"), "{error}");
    let mut root = stack(1, 100.0, 100.0);
    let mut nested = stack(2, 100.0, 100.0);
    nested.style.composite = true;
    nested.style.opacity = 0.9;
    for id in 3..=10 {
        let mut parent = stack(id, 100.0, 100.0);
        parent.style.composite = true;
        parent.style.opacity = 0.9;
        parent.children.push(nested);
        nested = parent;
    }
    root.children.push(nested);
    let error = validate_compositing(&root, &layout(&root), (160, 100)).unwrap_err();
    assert!(error.contains("budget"), "{error}");
    root.children[0].visible = false;
    validate_compositing(&root, &layout(&root), (16_384, 16_384)).unwrap();
}

#[test]
fn opacity_one_needs_no_temporary_layer_and_does_not_change_pixels() {
    let mut root = overlapping(true);
    root.children[0].style.opacity = 1.0;
    validate_compositing(&root, &layout(&root), (20_000, 20_000)).unwrap();
    let mut grouped = raster_scene(&root, 160, 100, 1.0).unwrap();
    let expected = grouped.peek_pixels().unwrap().bytes().unwrap().to_vec();
    root.children[0].style.composite = false;
    let mut separate = raster_scene(&root, 160, 100, 1.0).unwrap();
    assert_eq!(expected, separate.peek_pixels().unwrap().bytes().unwrap());
}
