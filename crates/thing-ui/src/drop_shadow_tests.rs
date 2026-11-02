use thing_core::{ClipMode, Color, CrossAlign, Kind, MainAlign, Node, NodeId, Point, Shadow};

use super::tests::raster_scene;
use crate::{Layout, Size, TextSystem, validate_compositing};

fn stack(id: u64, width: f32, height: f32) -> Node {
    let mut node = Node::new(NodeId(id), Kind::Stack);
    node.style.width = Some(width);
    node.style.height = Some(height);
    node.style.align = CrossAlign::Start;
    node.style.justify = MainAlign::Start;
    node
}

fn shadow(x: f32, y: f32, blur: f32) -> Shadow {
    Shadow {
        color: Color([0, 0, 0, 255]),
        offset: Point { x, y },
        blur,
        spread: 0.0,
    }
}

fn scene() -> Node {
    let mut root = stack(1, 160.0, 100.0);
    root.style.background = Color::WHITE.into();
    let mut group = stack(2, 100.0, 30.0);
    group.style.drop_shadow = Some(shadow(0.0, 40.0, 0.0));
    for (id, x) in [(3, 0.0), (4, 30.0)] {
        let mut child = stack(id, 60.0, 30.0);
        child.style.translate_x = x;
        child.style.background = Color([255, 0, 0, 255]).into();
        group.children.push(child);
    }
    root.children.push(group);
    root
}

#[test]
fn drop_shadows_follow_combined_alpha_and_group_opacity_once() {
    for (opacity, expected) in [(1.0, 0), (0.5, 128)] {
        for scale in [1.0, 1.25] {
            let mut root = scene();
            root.children[0].style.opacity = opacity;
            let mut surface =
                raster_scene(&root, (160.0 * scale) as i32, (100.0 * scale) as i32, scale).unwrap();
            let pixels = surface.peek_pixels().unwrap();
            let sample =
                |x: f32, y: f32| pixels.get_color(((x * scale) as i32, (y * scale) as i32));
            assert_eq!(sample(15.0, 50.0), sample(45.0, 50.0));
            assert!((i32::from(sample(45.0, 50.0).r()) - expected).abs() <= 1);
            assert_eq!(sample(95.0, 50.0), skia_safe::Color::WHITE);
            assert_eq!(sample(45.0, 15.0).g(), sample(45.0, 50.0).r());
        }
    }
}

#[test]
fn an_ancestor_clips_the_result_without_cutting_the_source() {
    let mut root = stack(1, 160.0, 100.0);
    root.style.background = Color::WHITE.into();
    let mut clip = stack(2, 80.0, 80.0);
    clip.style.clip = ClipMode::Rounded;
    clip.style.radius = 10.0;
    let mut group = stack(3, 40.0, 30.0);
    group.style.translate_x = 85.0;
    group.style.translate_y = 20.0;
    group.style.drop_shadow = Some(shadow(-40.0, 0.0, 0.0));
    let mut child = stack(4, 40.0, 30.0);
    child.style.background = Color([255, 0, 0, 255]).into();
    group.children.push(child);
    clip.children.push(group);
    root.children.push(clip);
    let mut surface = raster_scene(&root, 160, 100, 1.0).unwrap();
    let pixels = surface.peek_pixels().unwrap();
    assert_eq!(pixels.get_color((50, 30)), skia_safe::Color::BLACK);
    assert_eq!(pixels.get_color((82, 30)), skia_safe::Color::WHITE);
    assert_eq!(pixels.get_color((95, 30)), skia_safe::Color::WHITE);
}

#[test]
fn an_offsurface_source_can_cast_a_shadow_onto_the_surface() {
    let mut root = stack(1, 100.0, 80.0);
    root.style.background = Color::WHITE.into();
    let mut group = stack(2, 30.0, 30.0);
    group.style.translate_x = -40.0;
    group.style.translate_y = 20.0;
    group.style.drop_shadow = Some(shadow(60.0, 0.0, 0.0));
    let mut child = stack(3, 30.0, 30.0);
    child.style.background = Color([255, 0, 0, 255]).into();
    group.children.push(child);
    root.children.push(group);
    let mut surface = raster_scene(&root, 100, 80, 1.0).unwrap();
    let pixels = surface.peek_pixels().unwrap();
    assert_eq!(pixels.get_color((25, 30)), skia_safe::Color::BLACK);
    assert_eq!(pixels.get_color((60, 30)), skia_safe::Color::WHITE);
}

#[test]
fn descendant_clips_shape_the_shadow_and_do_not_change_following_siblings() {
    let mut root = scene();
    let group = &mut root.children[0];
    group.style.clip = ClipMode::Rounded;
    group.style.radius = 15.0;
    group.style.width = Some(60.0);
    let mut next = stack(5, 20.0, 20.0);
    next.style.background = Color([0, 0, 255, 255]).into();
    next.style.translate_x = 130.0;
    root.children.push(next);
    let mut surface = raster_scene(&root, 160, 100, 1.0).unwrap();
    let pixels = surface.peek_pixels().unwrap();
    assert_eq!(pixels.get_color((0, 40)), skia_safe::Color::WHITE);
    assert_eq!(pixels.get_color((30, 50)), skia_safe::Color::BLACK);
    assert_eq!(pixels.get_color((80, 50)), skia_safe::Color::WHITE);
    assert_eq!(pixels.get_color((135, 10)), skia_safe::Color::BLUE);
}

#[test]
fn shadow_layers_are_bounded_even_at_full_opacity() {
    let root = scene();
    let layout = Layout::compute(
        &root,
        Size {
            width: 160.0,
            height: 100.0,
        },
        &mut TextSystem::default(),
    )
    .unwrap();
    assert!(validate_compositing(&root, &layout, (20_000, 20_000), 1.0).is_err());
    assert!(validate_compositing(&root, &layout, (160, 100), 1000.0).is_err());
    let mut invisible = root.clone();
    invisible.children[0]
        .style
        .drop_shadow
        .as_mut()
        .unwrap()
        .color = Color::TRANSPARENT;
    let layout = Layout::compute(
        &invisible,
        Size {
            width: 160.0,
            height: 100.0,
        },
        &mut TextSystem::default(),
    )
    .unwrap();
    validate_compositing(&invisible, &layout, (20_000, 20_000), 1.0).unwrap();
}

#[test]
fn glyph_alpha_is_reused_without_shadowing_the_text_box() {
    for scale in [1.0, 1.25] {
        let mut root = stack(1, 160.0, 100.0);
        root.style.background = Color::WHITE.into();
        let mut group = stack(2, 150.0, 30.0);
        group.style.drop_shadow = Some(shadow(0.0, 40.0, 0.0));
        let mut text = Node::new(NodeId(3), Kind::Text);
        text.text = "thing".into();
        text.style.foreground = Color([0, 0, 0, 255]);
        text.style.font_size = 20.0;
        group.children.push(text);
        root.children.push(group);
        let mut surface =
            raster_scene(&root, (160.0 * scale) as i32, (100.0 * scale) as i32, scale).unwrap();
        let pixels = surface.peek_pixels().unwrap();
        let mut ink = 0;
        for y in 0..(28.0 * scale) as i32 {
            for x in 0..(140.0 * scale) as i32 {
                let source = pixels.get_color((x, y));
                let shadow = pixels.get_color((x, y + (40.0 * scale) as i32));
                assert!(
                    (i32::from(source.r()) - i32::from(shadow.r())).abs() <= 1,
                    "{x},{y}: {source:?}, {shadow:?}"
                );
                ink += usize::from(source.r() < 240);
            }
        }
        assert!(ink > 20);
    }
}

#[test]
fn the_offset_follows_rotation_at_fractional_scale() {
    let mut root = stack(1, 160.0, 100.0);
    root.style.background = Color::WHITE.into();
    let mut group = stack(2, 20.0, 20.0);
    group.style.translate_x = 60.0;
    group.style.translate_y = 20.0;
    group.style.rotation = 90.0;
    group.style.drop_shadow = Some(shadow(0.0, 30.0, 4.0));
    group.style.background = Color([255, 0, 0, 255]).into();
    root.children.push(group);
    let mut surface = raster_scene(&root, 200, 125, 1.25).unwrap();
    let pixels = surface.peek_pixels().unwrap();
    assert!(pixels.get_color((50, 37)).r() < 10);
    assert_eq!(pixels.get_color((125, 37)), skia_safe::Color::WHITE);
    assert_eq!(pixels.get_color((87, 37)), skia_safe::Color::RED);
}
