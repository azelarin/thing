use std::{fs, path::PathBuf};

use tempfile::TempDir;
use thing_core::Kind;
use thing_runtime::{KeyInput, Runtime, SurfaceKey};

fn fixture(source: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lock.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

#[test]
fn lock_runtime_accepts_one_host_owned_secret_field() {
    let (_directory, path) = fixture(
        r#"
        return ui.lock {
            style = { background = '#111111' },
            children = {
                ui.column {
                    children = {
                        ui.text { text = 'locked' },
                        ui.secret_input {
                            placeholder = 'password',
                            style = { width = 240, caret_color = '#ffffff' },
                        },
                    },
                },
            },
        }
        "#,
    );
    let runtime = Runtime::load_lock(path).unwrap();

    assert_eq!(runtime.surfaces().len(), 1);
    assert_eq!(runtime.surfaces()[0].key, SurfaceKey::Default);
    assert_eq!(runtime.surfaces()[0].namespace, "thing-lock");
    assert_eq!(runtime.root().unwrap().kind, Kind::Lock);
    assert_eq!(
        runtime.root().unwrap().children[0].children[1].kind,
        Kind::SecretInput
    );
    assert!(
        runtime.root().unwrap().children[0].children[1]
            .text
            .is_empty()
    );
}

#[test]
fn shell_and_lock_entry_points_are_not_interchangeable() {
    let (_directory, panel) = fixture("return ui.panel {}");
    assert!(
        Runtime::load_lock(panel)
            .err()
            .unwrap()
            .to_string()
            .contains("ui.lock")
    );

    let (_directory, lock) = fixture("return ui.lock {}");
    assert!(
        Runtime::load(lock)
            .err()
            .unwrap()
            .to_string()
            .contains("ui.panel")
    );
}

#[test]
fn lock_configuration_cannot_export_or_handle_a_secret() {
    for source in [
        "return ui.lock { children = { ui.secret_input { value = 'secret' } } }",
        "return ui.lock { children = { ui.secret_input { on_change = function() end } } }",
        "return ui.lock { children = { ui.secret_input { on_submit = function() end } } }",
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load_lock(path).is_err(), "accepted {source}");
    }
}

#[test]
fn lock_configuration_rejects_ambiguous_surface_ownership() {
    for source in [
        "return {}",
        "return { ui.lock {} }",
        "return ui.lock { key = 'one' }",
        "return ui.lock { namespace = 'custom' }",
        "return ui.lock { children = { ui.popup {} } }",
        "return ui.lock { children = { ui.secret_input {}, ui.secret_input {} } }",
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load_lock(path).is_err(), "accepted {source}");
    }
}

#[test]
fn lock_reload_preserves_the_configuration_mode() {
    let (_directory, lock) = fixture("return ui.lock {}");
    let mut runtime = Runtime::load_lock(&lock).unwrap();
    runtime.reload(&lock).unwrap();
    assert_eq!(runtime.root().unwrap().kind, Kind::Lock);

    let (_panel_directory, panel) = fixture("return ui.panel {}");
    assert!(runtime.reload(panel).is_err());
    assert_eq!(runtime.root().unwrap().kind, Kind::Lock);
}

#[test]
fn lock_root_key_handlers_receive_non_secret_input() {
    let (_directory, lock) = fixture(
        r#"
        local count = ui.state(0)
        return ui.lock {
            on_key = function(event)
                assert(event.key == 'F1')
                assert(event.text == nil)
                assert(event.ctrl and not event.repeated)
                count:set(count:get() + 1)
                return true
            end,
            children = {
                ui.secret_input {},
                ui.text { text = function() return tostring(count:get()) end },
            },
        }
        "#,
    );
    let mut runtime = Runtime::load_lock(lock).unwrap();
    let root = runtime.root().unwrap().id;
    let result = runtime
        .dispatch_key(
            root,
            KeyInput {
                key: "F1".into(),
                text: None,
                shift: false,
                ctrl: true,
                alt: false,
                logo: false,
                repeated: false,
            },
        )
        .unwrap()
        .unwrap();

    assert!(result.handled);
    assert!(result.changed);
    assert_eq!(runtime.root().unwrap().children[1].text, "1");
}
