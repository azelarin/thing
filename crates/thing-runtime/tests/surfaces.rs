use std::{fs, path::PathBuf};

use tempfile::TempDir;
use thing_runtime::{Event, Runtime, SurfaceKey};

fn fixture(source: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

#[test]
fn legacy_single_panel_keeps_the_default_surface() {
    let (_directory, path) = fixture("return ui.panel { children = { ui.text { text = 'ok' } } }");
    let runtime = Runtime::load(path).unwrap();

    assert_eq!(runtime.surfaces().len(), 1);
    assert_eq!(runtime.surfaces()[0].key, SurfaceKey::Default);
    assert_eq!(runtime.surfaces()[0].namespace, "thing");
    assert_eq!(runtime.root().unwrap(), &runtime.surfaces()[0].root);
    assert_eq!(runtime.root().unwrap().children[0].text, "ok");
}

#[test]
fn keyed_surfaces_share_state_and_refresh_together() {
    let (_directory, path) = fixture(
        r#"
        local count = ui.state(0)
        return {
            ui.panel {
                key = 'bar',
                namespace = 'shell-bar',
                children = {
                    ui.text { text = function() return tostring(count:get()) end },
                    ui.button { on_click = function() count:set(count:get() + 1) end },
                },
            },
            ui.panel {
                key = 'dock',
                children = {
                    ui.text { text = function() return `dock:{count:get()}` end },
                },
            },
        }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let bar = SurfaceKey::Named("bar".into());
    let dock = SurfaceKey::Named("dock".into());

    assert_eq!(runtime.surface(&bar).unwrap().namespace, "shell-bar");
    assert_eq!(runtime.surface(&dock).unwrap().namespace, "thing-dock");
    let button = runtime.surface(&bar).unwrap().root.children[1].id;
    assert!(runtime.dispatch(button, Event::Click).unwrap());
    assert_eq!(runtime.surface(&bar).unwrap().root.children[0].text, "1");
    assert_eq!(
        runtime.surface(&dock).unwrap().root.children[0].text,
        "dock:1"
    );
}

#[test]
fn an_empty_surface_array_is_a_valid_loaded_runtime() {
    let (_directory, path) = fixture("return {}");
    let runtime = Runtime::load(path).unwrap();

    assert!(runtime.surfaces().is_empty());
    assert!(runtime.root().is_none());
    assert_eq!(runtime.files().len(), 1);
    assert!(runtime.surface(&SurfaceKey::Default).is_none());
}

#[test]
fn surface_arrays_require_unique_nonempty_static_keys() {
    for (source, expected) in [
        (
            "return { ui.panel {}, ui.panel { key = 'other' } }",
            "needs a nonempty static key",
        ),
        (
            "return { ui.panel { key = 'same' }, ui.panel { key = 'same' } }",
            "duplicate surface key",
        ),
        ("return { ui.panel { key = '' } }", "must be 1 to 256 bytes"),
        (
            "local key = ui.state('bar'); return { ui.panel { key = function() return key:get() end } }",
            "key must be a static string",
        ),
        (
            "return ui.panel { namespace = function() return 'later' end }",
            "namespace must be a static string",
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
fn a_maximum_length_key_has_a_default_namespace() {
    let key = "x".repeat(256);
    let (_directory, path) = fixture(&format!("return {{ ui.panel {{ key = '{key}' }} }}"));
    let runtime = Runtime::load(path).unwrap();

    assert_eq!(runtime.surfaces()[0].namespace, format!("thing-{key}"));
}

#[test]
fn widget_identity_and_node_limits_span_every_surface() {
    let (_directory, path) = fixture(
        r#"
        local shared = ui.text { text = 'shared' }
        return {
            ui.panel { key = 'one', children = { shared } },
            ui.panel { key = 'two', children = { shared } },
        }
        "#,
    );
    let error = Runtime::load(path).err().unwrap().to_string();
    assert!(error.contains("reused") || error.contains("cycle"));

    let (_directory, path) = fixture(
        r#"
        local one, two = {}, {}
        for index = 1, 5000 do
            one[index] = ui.text {}
            two[index] = ui.text {}
        end
        return {
            ui.panel { key = 'one', children = one },
            ui.panel { key = 'two', children = two },
        }
        "#,
    );
    let error = Runtime::load(path).err().unwrap().to_string();
    assert!(error.contains("10,000 nodes"), "{error}");
}

#[test]
fn an_invalid_second_root_update_preserves_every_surface() {
    let (_directory, path) = fixture(
        r#"
        local changed = ui.state(false)
        return {
            ui.panel {
                key = 'one',
                children = {
                    ui.text { text = function() return if changed:get() then 'new' else 'old' end },
                    ui.button { on_click = function() changed:set(true) end },
                },
            },
            ui.panel {
                key = 'two',
                style = {
                    width = function() return if changed:get() then -1 else 40 end,
                },
                children = { ui.text { text = 'second' } },
            },
        }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let one = SurfaceKey::Named("one".into());
    let two = SurfaceKey::Named("two".into());
    let button = runtime.surface(&one).unwrap().root.children[1].id;

    assert!(runtime.dispatch(button, Event::Click).is_err());
    assert_eq!(runtime.surface(&one).unwrap().root.children[0].text, "old");
    assert_eq!(runtime.surface(&two).unwrap().root.style.width, Some(40.0));
}
