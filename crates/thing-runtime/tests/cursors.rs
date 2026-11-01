use std::fs;

use thing_core::Cursor;
use thing_runtime::{Event, Runtime};

fn load(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(&path);
    (directory, runtime)
}

#[test]
fn cursors_are_reactive_names_on_any_widget() {
    let (_directory, runtime) = load(
        r#"
        local busy = ui.state(false)
        return ui.panel {
            cursor = 'default',
            children = {
                ui.button {
                    cursor = function() return if busy:get() then 'wait' else nil end,
                    on_click = function() busy:set(true) end,
                },
                ui.row { cursor = 'nwse_resize' },
                ui.text { text = 'plain' },
            },
        }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let root = runtime.root().unwrap().clone();
    assert_eq!(root.cursor, Some(Cursor::Default));
    assert_eq!(root.children[0].cursor, None);
    assert_eq!(root.children[1].cursor, Some(Cursor::NwseResize));
    assert_eq!(root.children[2].cursor, None);
    runtime.dispatch(root.children[0].id, Event::Click).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].cursor,
        Some(Cursor::Wait)
    );
}

#[test]
fn unknown_cursor_names_are_rejected_and_a_rejected_update_keeps_the_tree() {
    for (value, message) in [
        ("'hand'", "unknown cursor \"hand\""),
        ("'not-allowed'", "unknown cursor"),
        ("'Pointer'", "unknown cursor"),
        ("3", "cursor must be a string"),
    ] {
        let (_directory, runtime) = load(&format!("return ui.panel {{ cursor = {value} }}"));
        let error = runtime.err().unwrap().to_string();
        assert!(error.contains(message), "{value}: {error}");
    }

    let (_directory, runtime) = load(
        r#"
        local broken = ui.state(false)
        return ui.panel { children = { ui.button {
            cursor = function() return if broken:get() then 'hand' else 'grab' end,
            on_click = function()
                services.media.next()
                broken:set(true)
            end,
        } } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[0].clone();
    assert!(runtime.dispatch(button.id, Event::Click).is_err());
    assert_eq!(
        runtime.root().unwrap().children[0].cursor,
        Some(Cursor::Grab)
    );
    assert!(runtime.take_actions().is_empty());
}
