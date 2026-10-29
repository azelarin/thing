use std::{fs, path::PathBuf};

use tempfile::TempDir;
use thing_core::{
    KeyboardMode, PanelEdge, PanelInputRect, PanelInputRegion, PanelMargins, PanelProperties,
    SurfaceAnchors, SurfaceLayer,
};
use thing_runtime::{Event, Runtime, Scalar};

fn fixture(source: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

#[test]
fn loads_panel_surface_properties() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel {
            edge = 'bottom',
            margins = { top = 1, right = 2, bottom = 3, left = -4 },
            exclusive_zone = 36,
            input_region = { x = -2, y = 4, width = 480, height = 36 },
            style = { width = 480, height = 52 },
        }
        "#,
    );

    let runtime = Runtime::load(path).unwrap();
    assert_eq!(runtime.root().unwrap().style.width, Some(480.0));
    assert_eq!(runtime.root().unwrap().style.height, Some(52.0));
    assert_eq!(
        runtime.root().unwrap().panel,
        PanelProperties {
            edge: PanelEdge::Bottom,
            margins: PanelMargins {
                top: 1,
                right: 2,
                bottom: 3,
                left: -4,
            },
            exclusive_zone: Some(36),
            input_region: PanelInputRegion::Rectangles(vec![PanelInputRect {
                x: -2,
                y: 4,
                width: Some(480),
                height: Some(36),
            }]),
            ..PanelProperties::default()
        }
    );
}

#[test]
fn an_empty_input_rectangle_keeps_the_whole_surface_interactive() {
    let (_directory, path) = fixture("return ui.panel { input_region = {} }");
    let runtime = Runtime::load(path).unwrap();

    assert_eq!(
        runtime.root().unwrap().panel.input_region,
        PanelInputRegion::Rectangles(vec![PanelInputRect::default()])
    );
}

#[test]
fn an_explicit_empty_rectangle_union_is_click_through() {
    let (_directory, path) = fixture("return ui.panel { input_region = { rectangles = {} } }");
    let runtime = Runtime::load(path).unwrap();

    assert_eq!(
        runtime.root().unwrap().panel.input_region,
        PanelInputRegion::Rectangles(Vec::new())
    );
}

#[test]
fn panel_surface_properties_are_reactive() {
    let (_directory, path) = fixture(
        r#"
        local changed = ui.state(false)
        return ui.panel {
            edge = function() return if changed:get() then 'left' else 'top' end,
            margins = function()
                if changed:get() then return { left = -4, bottom = 8 } end
                return { top = 2, right = 3 }
            end,
            exclusive_zone = function()
                if changed:get() then return nil end
                return 36
            end,
            input_region = function()
                if changed:get() then return false end
                return { x = -2, height = 36 }
            end,
            children = {
                ui.button { on_click = function() changed:set(true) end },
            },
        }
        "#,
    );

    let mut runtime = Runtime::load(path).unwrap();
    assert_eq!(runtime.root().unwrap().panel.edge, PanelEdge::Top);
    assert_eq!(runtime.root().unwrap().panel.exclusive_zone, Some(36));
    assert_eq!(
        runtime.root().unwrap().panel.input_region,
        PanelInputRegion::Rectangles(vec![PanelInputRect {
            x: -2,
            height: Some(36),
            ..PanelInputRect::default()
        }])
    );

    let button = runtime.root().unwrap().children[0].id;
    assert!(runtime.dispatch(button, Event::Click).unwrap());
    assert_eq!(
        runtime.root().unwrap().panel,
        PanelProperties {
            edge: PanelEdge::Left,
            margins: PanelMargins {
                left: -4,
                bottom: 8,
                ..PanelMargins::default()
            },
            exclusive_zone: None,
            input_region: PanelInputRegion::Rectangles(Vec::new()),
            ..PanelProperties::default()
        }
    );
}

#[test]
fn loads_explicit_surface_properties_and_rectangle_unions() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel {
            anchors = { top = true, right = true, bottom = true, left = true },
            layer = 'overlay',
            keyboard_mode = 'exclusive',
            exclusive_zone = 0,
            input_region = {
                rectangles = {
                    { x = 2, width = 10, height = 12 },
                    { y = 20, width = 30, height = 8 },
                },
            },
        }
        "#,
    );

    let runtime = Runtime::load(path).unwrap();
    assert_eq!(
        runtime.root().unwrap().panel,
        PanelProperties {
            anchors: Some(SurfaceAnchors {
                top: true,
                right: true,
                bottom: true,
                left: true,
            }),
            layer: SurfaceLayer::Overlay,
            keyboard_mode: KeyboardMode::Exclusive,
            exclusive_zone: Some(0),
            input_region: PanelInputRegion::Rectangles(vec![
                PanelInputRect {
                    x: 2,
                    width: Some(10),
                    height: Some(12),
                    ..PanelInputRect::default()
                },
                PanelInputRect {
                    y: 20,
                    width: Some(30),
                    height: Some(8),
                    ..PanelInputRect::default()
                },
            ]),
            ..PanelProperties::default()
        }
    );
}

#[test]
fn explicit_surface_properties_are_reactive_whole_values() {
    let (_directory, path) = fixture(
        r#"
        local changed = ui.state(false)
        return ui.panel {
            style = { width = 80, height = 40 },
            anchors = function()
                if changed:get() then return { right = true } end
                return { left = true }
            end,
            layer = function() return if changed:get() then 'bottom' else 'top' end,
            keyboard_mode = function()
                return if changed:get() then 'none' else 'on_demand'
            end,
            children = {
                ui.button { on_click = function() changed:set(true) end },
            },
        }
        "#,
    );

    let mut runtime = Runtime::load(path).unwrap();
    assert_eq!(
        runtime.root().unwrap().panel.anchors,
        Some(SurfaceAnchors {
            left: true,
            ..SurfaceAnchors::default()
        })
    );
    let button = runtime.root().unwrap().children[0].id;
    assert!(runtime.dispatch(button, Event::Click).unwrap());
    assert_eq!(
        runtime.root().unwrap().panel.anchors,
        Some(SurfaceAnchors {
            right: true,
            ..SurfaceAnchors::default()
        })
    );
    assert_eq!(runtime.root().unwrap().panel.layer, SurfaceLayer::Bottom);
    assert_eq!(
        runtime.root().unwrap().panel.keyboard_mode,
        KeyboardMode::None
    );
}

#[test]
fn rejects_sparse_or_oversized_input_region_lists() {
    let (_directory, path) = fixture(
        "return ui.panel { input_region = { rectangles = { [1] = { width = 1 }, [3] = { width = 1 } } } }",
    );
    assert!(
        Runtime::load(path)
            .err()
            .unwrap()
            .to_string()
            .contains("dense list")
    );

    let (_directory, path) = fixture(
        r#"
        local regions = {}
        for index = 1, 257 do regions[index] = { width = 1, height = 1 } end
        return ui.panel { input_region = { rectangles = regions } }
        "#,
    );
    assert!(
        Runtime::load(path)
            .err()
            .unwrap()
            .to_string()
            .contains("at most 256")
    );

    let (_directory, path) =
        fixture("return ui.panel { input_region = { { width = 1, height = 1 } } }");
    assert!(
        Runtime::load(path)
            .err()
            .unwrap()
            .to_string()
            .contains("rectangles table")
    );
}

#[test]
fn omitted_panel_properties_preserve_existing_defaults() {
    let (_directory, path) = fixture("return ui.panel {}");
    let runtime = Runtime::load(path).unwrap();
    assert_eq!(runtime.root().unwrap().panel, PanelProperties::default());
}

#[test]
fn idle_inhibition_is_reactive_on_panel_and_popup_surfaces() {
    let (_directory, path) = fixture(
        r#"
        local available = services.idle.inhibit_available
        return ui.panel {
            idle_inhibit = function() return available:get() end,
            children = {
                ui.button { children = {
                    ui.popup {
                        idle_inhibit = function() return available:get() end,
                    },
                } },
            },
        }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    assert!(!runtime.root().unwrap().idle_inhibit);
    assert!(!runtime.root().unwrap().children[0].children[0].idle_inhibit);

    runtime
        .set_service("idle.inhibit_available", Scalar::Boolean(true))
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert!(runtime.root().unwrap().idle_inhibit);
    assert!(runtime.root().unwrap().children[0].children[0].idle_inhibit);
}

#[test]
fn idle_inhibition_rejects_invalid_values_and_non_surface_widgets() {
    for (source, expected) in [
        (
            "return ui.panel { idle_inhibit = 'yes' }",
            "idle_inhibit must be a boolean",
        ),
        (
            "return ui.panel { children = { ui.text { idle_inhibit = true } } }",
            "unknown property",
        ),
    ] {
        let (_directory, path) = fixture(source);
        let error = Runtime::load(path).err().unwrap().to_string();
        assert!(
            error.contains(expected),
            "{error:?} did not contain {expected:?}"
        );
    }
}

#[test]
fn rejects_invalid_or_nested_panel_surface_properties() {
    for (source, expected) in [
        (
            "return ui.panel { edge = 'center' }",
            "edge must be 'top', 'bottom', 'left', or 'right'",
        ),
        (
            "return ui.panel { margins = { horizontal = 4 } }",
            "unknown margin",
        ),
        (
            "return ui.panel { exclusive_zone = -2 }",
            "exclusive_zone must be -1",
        ),
        (
            "return ui.panel { input_region = true }",
            "input_region must be false, a rectangle",
        ),
        (
            "return ui.panel { input_region = { width = 0 } }",
            "input_region.width must be a positive integer",
        ),
        (
            "return ui.panel { input_region = { radius = 4 } }",
            "unknown input_region property",
        ),
        (
            "return ui.panel { children = { ui.panel { edge = 'left' } } }",
            "edge is only supported on the root panel",
        ),
        (
            "return ui.panel { edge = 'top', anchors = { top = true } }",
            "cannot contain both edge and anchors",
        ),
        (
            "return ui.panel { style = { width = 40, height = 40 }, anchors = { diagonal = true } }",
            "unknown anchor",
        ),
        (
            "return ui.panel { style = { width = 40, height = 40 }, layer = 'middle' }",
            "layer must be",
        ),
        (
            "return ui.panel { style = { width = 40, height = 40 }, keyboard_mode = 'sometimes' }",
            "keyboard_mode must be",
        ),
    ] {
        let (_directory, path) = fixture(source);
        let error = Runtime::load(path).err().unwrap().to_string();
        assert!(
            error.contains(expected),
            "{error:?} did not contain {expected:?}"
        );
    }
}
