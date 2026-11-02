use std::fs;

use thing_runtime::{Event, Runtime};

fn load(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path);
    (directory, runtime)
}

#[test]
fn compositing_can_be_changed_without_changing_opacity_or_layout() {
    let (_directory, runtime) = load(
        r#"
        local grouped = ui.state(false)
        return ui.panel { children = { ui.button {
            on_click = function() grouped:set(not grouped:get()) end,
            style = { width = 100, height = 40, opacity = 0.5,
                composite = function() return grouped:get() end },
        } } }
    "#,
    );
    let mut runtime = runtime.unwrap();
    let id = runtime.root().unwrap().children[0].id;
    assert!(!runtime.root().unwrap().children[0].style.composite);
    runtime.dispatch(id, Event::Click).unwrap();
    let button = &runtime.root().unwrap().children[0];
    assert!(button.style.composite);
    assert_eq!(button.style.opacity, 0.5);
    assert_eq!(
        (button.style.width, button.style.height),
        (Some(100.0), Some(40.0))
    );
}

#[test]
fn invalid_compositing_updates_leave_the_published_tree() {
    for value in ["1", "'yes'", "{}"] {
        assert!(
            load(&format!(
                "return ui.panel {{ style = {{ composite = {value} }} }}"
            ))
            .1
            .is_err()
        );
    }
    let (_directory, runtime) = load(
        r#"
        local bad = ui.state(false)
        return ui.panel { children = { ui.button {
            on_click = function() bad:set(true) end,
            style = { composite = function() return if bad:get() then 'wrong' else true end },
        } } }
    "#,
    );
    let mut runtime = runtime.unwrap();
    let before = runtime.root().unwrap().children[0].clone();
    assert!(runtime.dispatch(before.id, Event::Click).is_err());
    assert_eq!(runtime.root().unwrap().children[0], before);
}
