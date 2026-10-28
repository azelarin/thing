use std::{fs, path::PathBuf};

use tempfile::TempDir;
use thing_core::{PopupAlignment, PopupAnchor, PopupConstraints, PopupOffset, PopupOptions};
use thing_runtime::{Event, Runtime};

fn fixture(source: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

#[test]
fn loads_popup_placement_properties() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = { ui.button { children = { ui.popup {
            anchor = 'left',
            alignment = 'center',
            offset = { x = -12, y = 8 },
            constraints = { flip_x = true, slide_y = true },
        } } } } }
        "#,
    );

    let runtime = Runtime::load(path).unwrap();
    let popup = runtime.root().unwrap().children[0].children[0].popup;
    assert_eq!(popup.anchor, PopupAnchor::Left);
    assert_eq!(popup.alignment, PopupAlignment::Center);
    assert_eq!(popup.offset, PopupOffset { x: -12, y: 8 });
    assert_eq!(
        popup.constraints,
        Some(PopupConstraints {
            flip_x: true,
            slide_y: true,
            ..PopupConstraints::default()
        })
    );
}

#[test]
fn popup_placement_properties_are_reactive_whole_values() {
    let (_directory, path) = fixture(
        r#"
        local changed = ui.state(false)
        return ui.panel { children = { ui.button {
            on_click = function() changed:set(true) end,
            children = { ui.popup {
                anchor = function() return if changed:get() then 'right' else 'auto' end,
                alignment = function() return if changed:get() then 'start' else 'end' end,
                offset = function()
                    if changed:get() then return { x = 4 } end
                    return { y = 8 }
                end,
                constraints = function()
                    if changed:get() then return { slide_y = true } end
                    return nil
                end,
            } },
        } } }
        "#,
    );

    let mut runtime = Runtime::load(path).unwrap();
    let button = runtime.root().unwrap().children[0].id;
    assert_eq!(
        runtime.root().unwrap().children[0].children[0].popup,
        PopupOptions {
            offset: PopupOffset { x: 0, y: 8 },
            ..PopupOptions::default()
        }
    );

    assert!(runtime.dispatch(button, Event::Click).unwrap());
    let popup = runtime.root().unwrap().children[0].children[0].popup;
    assert_eq!(popup.anchor, PopupAnchor::Right);
    assert_eq!(popup.alignment, PopupAlignment::Start);
    assert_eq!(popup.offset, PopupOffset { x: 4, y: 0 });
    assert_eq!(
        popup.constraints,
        Some(PopupConstraints {
            slide_y: true,
            ..PopupConstraints::default()
        })
    );
}

#[test]
fn omitted_popup_placement_preserves_existing_defaults() {
    let (_directory, path) =
        fixture("return ui.panel { children = { ui.button { children = { ui.popup {} } } } }");
    let runtime = Runtime::load(path).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].children[0].popup,
        PopupOptions::default()
    );
}

#[test]
fn rejects_invalid_or_non_popup_placement_properties() {
    for (source, expected) in [
        (
            "return ui.panel { children = { ui.popup { anchor = 'middle' } } }",
            "anchor must be 'auto'",
        ),
        (
            "return ui.panel { children = { ui.popup { alignment = 'left' } } }",
            "alignment must be 'start'",
        ),
        (
            "return ui.panel { children = { ui.popup { offset = { z = 4 } } } }",
            "unknown offset property",
        ),
        (
            "return ui.panel { children = { ui.popup { offset = { x = 100001 } } } }",
            "popup offsets must be between",
        ),
        (
            "return ui.panel { children = { ui.popup { constraints = { resize_x = true } } } }",
            "unknown constraint property",
        ),
        (
            "return ui.panel { children = { ui.button { alignment = 'center' } } }",
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
