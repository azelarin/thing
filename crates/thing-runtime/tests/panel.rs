use std::{fs, path::PathBuf};

use tempfile::TempDir;
use thing_core::{PanelEdge, PanelInputRect, PanelInputRegion, PanelMargins, PanelProperties};
use thing_runtime::{Event, Runtime};

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
    assert_eq!(runtime.root().style.width, Some(480.0));
    assert_eq!(runtime.root().style.height, Some(52.0));
    assert_eq!(
        runtime.root().panel,
        PanelProperties {
            edge: PanelEdge::Bottom,
            margins: PanelMargins {
                top: 1,
                right: 2,
                bottom: 3,
                left: -4,
            },
            exclusive_zone: Some(36),
            input_region: PanelInputRegion::Rectangle(PanelInputRect {
                x: -2,
                y: 4,
                width: Some(480),
                height: Some(36),
            }),
        }
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
    assert_eq!(runtime.root().panel.edge, PanelEdge::Top);
    assert_eq!(runtime.root().panel.exclusive_zone, Some(36));
    assert_eq!(
        runtime.root().panel.input_region,
        PanelInputRegion::Rectangle(PanelInputRect {
            x: -2,
            height: Some(36),
            ..PanelInputRect::default()
        })
    );

    let button = runtime.root().children[0].id;
    assert!(runtime.dispatch(button, Event::Click).unwrap());
    assert_eq!(
        runtime.root().panel,
        PanelProperties {
            edge: PanelEdge::Left,
            margins: PanelMargins {
                left: -4,
                bottom: 8,
                ..PanelMargins::default()
            },
            exclusive_zone: None,
            input_region: PanelInputRegion::Empty,
        }
    );
}

#[test]
fn omitted_panel_properties_preserve_existing_defaults() {
    let (_directory, path) = fixture("return ui.panel {}");
    let runtime = Runtime::load(path).unwrap();
    assert_eq!(runtime.root().panel, PanelProperties::default());
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
            "input_region must be false or a rectangle table",
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
    ] {
        let (_directory, path) = fixture(source);
        let error = Runtime::load(path).err().unwrap().to_string();
        assert!(
            error.contains(expected),
            "{error:?} did not contain {expected:?}"
        );
    }
}
