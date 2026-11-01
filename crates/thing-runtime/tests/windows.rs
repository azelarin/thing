use std::fs;

use thing_core::{Kind, WindowSize};
use thing_runtime::{Action, Event, Runtime, SurfaceKey, WindowRequest, WindowState};

fn load(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(&path);
    (directory, runtime)
}

fn error(source: &str) -> String {
    match load(source).1 {
        Ok(_) => panic!("accepted {source}"),
        Err(error) => error.to_string(),
    }
}

const SETTINGS: &str = r#"
local tab = ui.state('general')
local settings = ui.window_state('settings')
return {
    ui.panel { key = 'bar', style = { height = 32 } },
    ui.window {
        key = 'settings',
        title = function() return 'settings: ' .. tab:get() end,
        app_id = 'org.example.settings',
        size = { width = 480, height = 360 },
        min_size = { width = 320, height = 240 },
        on_close = function() tab:set('closed') end,
        children = { ui.column { children = {
            ui.button {
                on_click = function()
                    tab:set('display')
                    settings.set_maximized(not settings.maximized:get())
                end,
                children = { ui.text { text = function()
                    return string.format('%dx%d %s', settings.width:get(),
                        settings.height:get(), tostring(settings.open:get()))
                end } },
            },
        } } },
    },
}
"#;

fn window(runtime: &Runtime) -> &thing_core::Node {
    &runtime
        .surface(&SurfaceKey::Named("settings".into()))
        .unwrap()
        .root
}

fn text(runtime: &Runtime) -> String {
    window(runtime).children[0].children[0].children[0]
        .text
        .clone()
}

#[test]
fn windows_are_keyed_roots_with_reactive_properties_and_state_handles() {
    let (_directory, runtime) = load(SETTINGS);
    let mut runtime = runtime.unwrap();
    let root = window(&runtime).clone();
    assert_eq!(root.kind, Kind::Window);
    assert_eq!(root.window.title, "settings: general");
    assert_eq!(root.window.app_id, "org.example.settings");
    assert_eq!(
        root.window.size,
        WindowSize {
            width: 480,
            height: 360
        }
    );
    assert_eq!(
        root.window.min_size,
        Some(WindowSize {
            width: 320,
            height: 240
        })
    );
    assert_eq!(root.window.max_size, None);
    assert_eq!(text(&runtime), "0x0 false");

    // the host publishes what the compositor configured.
    runtime
        .set_window_state(
            "settings",
            WindowState {
                open: true,
                width: 800,
                height: 600,
                can_maximize: true,
                ..WindowState::default()
            },
        )
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime), "800x600 true");
    // a key without a handle is ignored.
    runtime
        .set_window_state("other", WindowState::default())
        .unwrap();

    // requests are event-only actions bound to the window's key.
    let button = window(&runtime).children[0].children[0].id;
    runtime.dispatch(button, Event::Click).unwrap();
    assert_eq!(window(&runtime).window.title, "settings: display");
    assert_eq!(
        runtime.take_actions(),
        [Action::Window {
            key: "settings".into(),
            request: WindowRequest::Maximized(true),
        }]
    );

    // the close callback is the configuration's decision.
    assert!(runtime.handles_close(root.id));
    runtime.dispatch(root.id, Event::Close).unwrap();
    assert_eq!(window(&runtime).window.title, "settings: closed");
}

#[test]
fn window_state_is_read_only_and_requests_need_an_event() {
    for (body, message) in [
        ("settings.width:set(1)", "read-only"),
        ("settings.width = 1", "readonly"),
        ("settings.set_fullscreen('yes')", "expects a boolean"),
    ] {
        let source = format!(
            r#"
            local settings = ui.window_state('settings')
            return ui.window {{ key = 'settings', children = {{ ui.button {{
                on_click = function() {body} end,
            }} }} }}
            "#
        );
        let (_directory, runtime) = load(&source);
        let mut runtime = runtime.unwrap();
        let button = runtime.root().unwrap().children[0].id;
        let error = runtime
            .dispatch(button, Event::Click)
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "{body}: {error}");
        assert!(runtime.take_actions().is_empty());
    }
    // outside an event handler a request is refused.
    let error = error(
        "local s = ui.window_state('settings'); s.set_maximized(true)\n\
         return ui.window { key = 'settings' }",
    );
    assert!(error.contains("only in event handlers"), "{error}");
}

#[test]
fn a_failing_close_callback_publishes_none_of_its_actions() {
    let (_directory, runtime) = load(
        r#"
        return ui.window {
            on_close = function()
                services.launch({ 'must-not-run' })
                error('not now')
            end,
        }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let root = runtime.root().unwrap().id;
    let error = runtime
        .dispatch(root, Event::Close)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not now"), "{error}");
    assert!(runtime.take_actions().is_empty());

    let (_directory, runtime) = load("return ui.window {}");
    let runtime = runtime.unwrap();
    assert!(!runtime.handles_close(runtime.root().unwrap().id));
}

#[test]
fn window_declarations_reject_panel_fields_and_malformed_values() {
    for (properties, message) in [
        ("layer = 'top'", "unknown property"),
        ("anchors = { top = true }", "unknown property"),
        ("edge = 'top'", "unknown property"),
        ("margins = { top = 1 }", "unknown property"),
        ("exclusive_zone = 0", "unknown property"),
        ("input_region = false", "unknown property"),
        ("keyboard_mode = 'none'", "unknown property"),
        (
            "namespace = 'thing-settings'",
            "windows identify themselves with app_id",
        ),
        (
            "style = { width = 400 }",
            "style width and height are not available",
        ),
        ("style = { min_width = 400 }", "min and max dimensions"),
        ("size = { width = 0, height = 300 }", "from 1 through 16384"),
        ("size = { width = 400.5, height = 300 }", "whole number"),
        ("size = { width = 400 }", "size.height must be a number"),
        (
            "size = { width = 400, height = 300, depth = 1 }",
            "unknown size property",
        ),
        ("size = 400", "must be a table"),
        (
            "size = { width = 100, height = 100 }, min_size = { width = 200, height = 50 }",
            "below min_size",
        ),
        (
            "size = { width = 900, height = 100 }, max_size = { width = 800, height = 800 }",
            "exceed max_size",
        ),
        ("title = 'line\\nbreak'", "control characters"),
        ("app_id = 'org example'", "without spaces"),
        ("title = 3", "title must be a string"),
        ("on_click = function() end", "on_click is not supported"),
    ] {
        let source = format!("return ui.window {{ {properties} }}");
        let error = error(&source);
        assert!(error.contains(message), "{properties}: {error}");
    }
    // a window is a root, and a state handle must name one.
    for (source, message) in [
        (
            "return ui.panel { children = { ui.window {} } }",
            "window is valid only as a surface root",
        ),
        (
            "ui.window_state('missing')\nreturn ui.window { key = 'other' }",
            "does not name a ui.window",
        ),
        (
            "ui.window_state('bar')\nreturn { ui.panel { key = 'bar' } }",
            "does not name a ui.window",
        ),
        (
            "ui.window_state(3)\nreturn ui.window {}",
            "expects a window key",
        ),
    ] {
        let error = error(source);
        assert!(error.contains(message), "{source}: {error}");
    }
    // handles are created only while loading.
    let (_directory, runtime) = load(
        r#"
        return ui.window { key = 'w', children = { ui.button {
            on_click = function() ui.window_state('w') end,
        } } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[0].id;
    let error = runtime
        .dispatch(button, Event::Click)
        .unwrap_err()
        .to_string();
    assert!(error.contains("while the configuration loads"), "{error}");
}

#[test]
fn lock_configurations_reject_windows() {
    for (source, message) in [
        (
            "return ui.window {}",
            "the lock surface root must be ui.lock",
        ),
        (
            "ui.window_state('w')\nreturn ui.lock { children = { ui.secret_input {} } }",
            "windows are unavailable in lock configurations",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock.luau");
        fs::write(&path, source).unwrap();
        let error = Runtime::load_lock(&path).err().unwrap().to_string();
        assert!(error.contains(message), "{source}: {error}");
    }
}
