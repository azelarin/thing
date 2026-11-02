use std::{
    fs,
    time::{Duration, Instant},
};

use thing_core::{Kind, Node};
use thing_runtime::{Event, Runtime};

fn load(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path);
    (directory, runtime)
}

fn node<'a>(runtime: &'a Runtime, key: &str) -> &'a Node {
    fn find<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
        if node.key.as_deref() == Some(key) {
            return Some(node);
        }
        node.children.iter().find_map(|child| find(child, key))
    }
    find(runtime.root().unwrap(), key).unwrap()
}

#[test]
fn reactive_progress_preserves_both_children_and_recovers_after_a_rejected_update() {
    let (_directory, runtime) = load(
        r#"
        local progress = ui.state(0)
        return ui.panel { children = {
            ui.crossfade { key = 'fade', progress = function() return progress:get() end,
                children = { ui.text { key = 'before', text = 'before' }, ui.text { key = 'after', text = 'after' } },
            },
            ui.button { key = 'half', on_click = function() progress:set(0.5) end },
            ui.button { key = 'bad', on_click = function() progress:set(1.1) end },
        } }
    "#,
    );
    let mut runtime = runtime.unwrap();
    let ids = (node(&runtime, "before").id, node(&runtime, "after").id);
    assert_eq!(node(&runtime, "fade").kind, Kind::Crossfade);
    assert_eq!(node(&runtime, "fade").crossfade_progress, 0.0);
    let bad = node(&runtime, "bad").id;
    let before = runtime.surfaces().to_vec();
    let error = runtime.dispatch(bad, Event::Click).unwrap_err().to_string();
    assert!(error.contains("between 0 and 1"), "{error}");
    assert_eq!(runtime.surfaces(), before);
    let half = node(&runtime, "half").id;
    runtime.dispatch(half, Event::Click).unwrap();
    assert_eq!(node(&runtime, "fade").crossfade_progress, 0.5);
    assert_eq!(
        (node(&runtime, "before").id, node(&runtime, "after").id),
        ids
    );
}

#[test]
fn crossfades_reject_bad_ranges_and_child_counts() {
    for progress in ["-0.1", "1.1", "0/0", "'half'", "{}"] {
        let source = format!(
            "return ui.panel {{ children = {{ ui.crossfade {{ progress = {progress}, children = {{ ui.row {{}}, ui.row {{}} }} }} }} }}"
        );
        assert!(load(&source).1.is_err(), "{progress}");
    }
    for children in ["{}", "{ ui.row {} }", "{ ui.row {}, ui.row {}, ui.row {} }"] {
        let source = format!(
            "return ui.panel {{ children = {{ ui.crossfade {{ children = {children} }} }} }}"
        );
        let error = load(&source).1.err().unwrap().to_string();
        assert!(error.contains("exactly two"), "{error}");
    }
    assert!(
        load("return ui.panel { children = { ui.row { progress = 0.5 } } }")
            .1
            .is_err()
    );
}

#[test]
fn transitions_and_springs_animate_progress_without_replacing_content() {
    let (_directory, runtime) = load(
        r#"
        local target = ui.state(0)
        return ui.panel { children = {
            ui.crossfade { key = 'transition',
                progress = ui.transition(function() return target:get() end, { duration = 100, easing = 'linear' }),
                children = { ui.row {}, ui.row {} },
            },
            ui.crossfade { key = 'spring',
                progress = ui.spring(function() return target:get() end, { stiffness = 100, damping = 0.1, epsilon = 0.001 }),
                children = { ui.row {}, ui.row {} },
            },
            ui.button { key = 'start', on_click = function() target:set(1) end },
        } }
    "#,
    );
    let mut runtime = runtime.unwrap();
    let ids: Vec<_> = node(&runtime, "transition")
        .children
        .iter()
        .map(|child| child.id)
        .collect();
    let id = node(&runtime, "start").id;
    runtime.dispatch(id, Event::Click).unwrap();
    let start = Instant::now();
    runtime.advance(start).unwrap();
    runtime.advance(start + Duration::from_millis(50)).unwrap();
    assert!((node(&runtime, "transition").crossfade_progress - 0.5).abs() < 0.02);
    for step in 1..200 {
        runtime
            .advance(start + Duration::from_millis(step * 10))
            .unwrap();
        assert!((0.0..=1.0).contains(&node(&runtime, "spring").crossfade_progress));
    }
    runtime.advance(start + Duration::from_secs(60)).unwrap();
    assert_eq!(node(&runtime, "transition").crossfade_progress, 1.0);
    assert_eq!(node(&runtime, "spring").crossfade_progress, 1.0);
    assert_eq!(
        node(&runtime, "transition")
            .children
            .iter()
            .map(|child| child.id)
            .collect::<Vec<_>>(),
        ids
    );
}
