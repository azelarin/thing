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
        assert_eq!((style.clip.clone(), style.radius), (mode, radius));
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

const RING: &str = "M50 0 A50 50 0 1 1 50 100 A50 50 0 1 1 50 0 Z M35 35 H65 V65 H35 Z";

#[test]
fn path_clips_are_reactive_tables_using_path_terms() {
    let (_directory, runtime) = runtime(&format!(
        r#"
        local holed = ui.state(true)
        return ui.panel {{ children = {{ ui.button {{
            on_click = function() holed:set(not holed:get()) end,
            style = {{ clip = function()
                return {{
                    path = '{RING}',
                    view_box = {{ x = 0, y = 0, width = 100, height = 100 }},
                    fit = 'cover',
                    fill_rule = if holed:get() then 'evenodd' else 'nonzero',
                }}
            end }},
        }} }} }}
        "#
    ));
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[0].id;
    let clip = |runtime: &Runtime| match &runtime.root().unwrap().children[0].style.clip {
        ClipMode::Path(path) => (path.fit, path.fill_rule, path.data.clone()),
        other => panic!("expected a path clip, found {other:?}"),
    };
    assert_eq!(
        clip(&runtime),
        (
            thing_core::PathFit::Cover,
            thing_core::PathFillRule::EvenOdd,
            RING.to_owned()
        )
    );
    runtime.dispatch(button, Event::Click).unwrap();
    assert_eq!(clip(&runtime).1, thing_core::PathFillRule::NonZero);

    // fit and fill rule default like ui.path's.
    let (_defaults_directory, defaults) = self::runtime(
        "return ui.panel { style = { clip = { path = 'M0 0 H1 V1 Z', view_box = { x = 0, y = 0, width = 1, height = 1 } } } }",
    );
    let ClipMode::Path(path) = defaults.unwrap().root().unwrap().style.clip.clone() else {
        panic!("expected a path clip");
    };
    assert_eq!(
        (path.fit, path.fill_rule),
        (
            thing_core::PathFit::Contain,
            thing_core::PathFillRule::NonZero
        )
    );
}

#[test]
fn malformed_or_excessive_path_clips_are_rejected_before_publication() {
    let view_box = "view_box = { x = 0, y = 0, width = 10, height = 10 }";
    let mut long = String::from("M0 0");
    for index in 0..1100 {
        long.push_str(&format!(" L{} {}", index % 10, index % 7));
    }
    for (clip, message) in [
        (format!("{{ {view_box} }}"), "clip.path must be a string"),
        ("{ path = 'M0 0 H1 V1 Z' }".to_owned(), "view_box"),
        (
            format!("{{ path = 'M0 0 Q', {view_box} }}"),
            "invalid path data",
        ),
        (
            format!("{{ path = '', {view_box} }}"),
            "at least one command",
        ),
        (
            format!("{{ path = '{long}', {view_box} }}"),
            "at most 1024 segments",
        ),
        (
            format!("{{ path = 'M0 0 H1 V1 Z', {view_box}, fit = 'stretch' }}"),
            "fit",
        ),
        (
            format!("{{ path = 'M0 0 H1 V1 Z', {view_box}, inset = 2 }}"),
            "unknown clip property",
        ),
        (
            "{ path = 'M0 0 H1 V1 Z', view_box = { x = 0, y = 0, width = 0, height = 10 } }"
                .to_owned(),
            "view_box",
        ),
    ] {
        let source = format!("return ui.panel {{ style = {{ clip = {clip} }} }}");
        let error = match runtime(&source).1 {
            Ok(_) => panic!("accepted {clip}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(message), "{clip}: {error}");
    }

    // a rejected update keeps the accepted shape and discards the action.
    let (_directory, runtime) = runtime(&format!(
        r#"
        local bad = ui.state(false)
        return ui.panel {{ children = {{ ui.button {{
            on_click = function() services.media.next(); bad:set(true) end,
            style = {{ clip = function()
                return {{ path = if bad:get() then 'M0 0 Q' else '{RING}', {view_box} }}
            end }},
        }} }} }}
        "#
    ));
    let mut runtime = runtime.unwrap();
    let before = runtime.root().unwrap().children[0].clone();
    assert!(runtime.dispatch(before.id, Event::Click).is_err());
    assert_eq!(
        runtime.root().unwrap().children[0].style.clip,
        before.style.clip
    );
    assert!(runtime.take_actions().is_empty());
}
