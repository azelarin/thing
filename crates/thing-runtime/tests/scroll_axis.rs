use std::fs;

use thing_core::ScrollAxis;
use thing_runtime::{Event, Runtime};

fn runtime(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path);
    (directory, runtime)
}

#[test]
fn scroll_axis_defaults_to_vertical_and_updates_reactively() {
    let (_directory, runtime) = runtime(
        r#"
        local horizontal = ui.state(false)
        return ui.panel { children = {
            ui.button { on_click = function() horizontal:set(not horizontal:get()) end },
            ui.scroll { axis = function() return if horizontal:get() then 'horizontal' else 'vertical' end },
            ui.scroll {},
        } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[0].id;
    for axis in [
        ScrollAxis::Vertical,
        ScrollAxis::Horizontal,
        ScrollAxis::Vertical,
    ] {
        assert_eq!(runtime.root().unwrap().children[1].scroll_axis, axis);
        assert_eq!(
            runtime.root().unwrap().children[2].scroll_axis,
            ScrollAxis::Vertical
        );
        runtime.dispatch(button, Event::Click).unwrap();
    }
}

#[test]
fn invalid_axis_updates_keep_the_tree_and_discard_actions() {
    for value in ["false", "'both'", "{}"] {
        let source =
            format!("return ui.panel {{ children = {{ ui.scroll {{ axis = {value} }} }} }}");
        assert!(runtime(&source).1.is_err(), "accepted {value}");
    }
    assert!(
        runtime("return ui.panel { children = { ui.row { axis = 'horizontal' } } }")
            .1
            .is_err()
    );
    let (_directory, runtime) = runtime(
        r#"
        local bad = ui.state(false)
        return ui.panel { children = {
            ui.button { on_click = function() services.media.next(); bad:set(true) end },
            ui.scroll { axis = function() return if bad:get() then 'both' else 'horizontal' end },
        } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let before = runtime.root().unwrap().clone();
    assert!(
        runtime
            .dispatch(before.children[0].id, Event::Click)
            .is_err()
    );
    assert_eq!(runtime.root().unwrap(), &before);
    assert!(runtime.take_actions().is_empty());
}
