use std::fs;

use thing_core::ClipMode;
use thing_runtime::{Event, Runtime};

fn runtime(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path);
    (directory, runtime)
}

#[test]
fn clipping_and_radius_update_reactively() {
    let (_directory, runtime) = runtime(
        r#"
        local rounded = ui.state(false)
        return ui.panel { children = { ui.button {
            on_click = function() rounded:set(not rounded:get()) end,
            style = {
                clip = function() return if rounded:get() then 'rounded' else 'bounds' end,
                radius = function() return if rounded:get() then 24 else 4 end,
            },
        } } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[0].id;
    for (mode, radius) in [
        (ClipMode::Bounds, 4.0),
        (ClipMode::Rounded, 24.0),
        (ClipMode::Bounds, 4.0),
    ] {
        let style = &runtime.root().unwrap().children[0].style;
        assert_eq!((style.clip, style.radius), (mode, radius));
        runtime.dispatch(button, Event::Click).unwrap();
    }
}

#[test]
fn invalid_clip_updates_preserve_the_tree_and_discard_actions() {
    for value in [
        "false",
        "'circle'",
        "{}",
        "ui.transition { duration = 100, value = function() return 'rounded' end }",
    ] {
        let source = format!("return ui.panel {{ style = {{ clip = {value} }} }}");
        assert!(runtime(&source).1.is_err(), "accepted {value}");
    }
    let (_directory, runtime) = runtime(
        r#"
        local bad = ui.state(false)
        return ui.panel { children = { ui.button {
            on_click = function() services.media.next(); bad:set(true) end,
            style = { clip = function() return if bad:get() then 'circle' else 'rounded' end },
        } } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[0].clone();
    assert!(runtime.dispatch(button.id, Event::Click).is_err());
    assert_eq!(runtime.root().unwrap().children[0].style, button.style);
    assert!(runtime.take_actions().is_empty());
}
