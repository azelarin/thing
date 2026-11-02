use std::{
    fs,
    time::{Duration, Instant},
};

use tempfile::TempDir;
use thing_core::{ImageEffects, Node};
use thing_runtime::{Event, Runtime};

fn load(source: &str) -> (TempDir, Result<Runtime, String>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).map_err(|error| error.to_string());
    (directory, runtime)
}

fn node<'a>(runtime: &'a Runtime, key: &str) -> &'a Node {
    fn find<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
        if node.key.as_deref() == Some(key) {
            return Some(node);
        }
        node.children.iter().find_map(|child| find(child, key))
    }
    runtime
        .surfaces()
        .iter()
        .find_map(|surface| find(&surface.root, key))
        .unwrap()
}

fn effects(runtime: &Runtime, key: &str) -> ImageEffects {
    node(runtime, key).image_effects
}

fn start(runtime: &mut Runtime) -> Instant {
    let now = Instant::now() + Duration::from_secs(3600);
    runtime.activate_timers(now);
    let now = now + Duration::from_millis(1);
    for token in runtime.due_timers(now) {
        runtime
            .dispatch_timer_checked(&token, now, |_| Ok(()))
            .unwrap();
    }
    now
}

fn close(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.01, "{actual} != {expected}");
}

#[test]
fn rendering_fixture_variants_load_or_fail_as_intended() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/rendering/image-effects"
    );
    for name in [
        "gallery.luau",
        "pattern.svg",
        "panel.luau",
        "panel-reload.luau",
        "panel-invalid.luau",
    ] {
        fs::copy(format!("{fixture}/{name}"), directory.path().join(name)).unwrap();
    }
    let load = |name: &str| Runtime::load(directory.path().join(name));
    let first = load("panel.luau").unwrap();
    let reload = load("panel-reload.luau").unwrap();
    let blurred = |runtime: &Runtime| {
        let mut values = Vec::new();
        let mut pending = vec![runtime.root().unwrap()];
        while let Some(node) = pending.pop() {
            values.push((node.image.clone(), node.image_effects));
            pending.extend(&node.children);
        }
        values
    };
    let (first, reload) = (blurred(&first), blurred(&reload));
    // the reload changes effects while keeping every source identity.
    assert_eq!(first.len(), reload.len());
    assert!(first.iter().zip(&reload).all(|(a, b)| a.0 == b.0));
    assert!(first.iter().zip(&reload).any(|(a, b)| a.1 != b.1));
    let error = load("panel-invalid.luau").err().unwrap().to_string();
    assert!(error.contains("between 0 and 64"), "{error}");
}

#[test]
fn image_effects_default_to_neutral_and_update_reactively() {
    let (_directory, runtime) = load(
        r#"
        local amount = ui.state(0)
        return ui.panel { children = {
            ui.image { key = 'plain', source = 'a.png' },
            ui.image { key = 'fixed', source = 'a.png', blur = 64, saturation = 0 },
            ui.image {
                key = 'live',
                source = 'a.png',
                fit = 'cover',
                blur = function() return amount:get() * 8 end,
                saturation = function() return 1 + amount:get() end,
            },
            ui.button { key = 'more', on_click = function() amount:set(1) end },
            ui.button { key = 'bad', on_click = function() amount:set(9) end },
        } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    assert_eq!(effects(&runtime, "plain"), ImageEffects::default());
    assert_eq!(
        effects(&runtime, "fixed"),
        ImageEffects {
            blur: 64.0,
            saturation: 0.0
        }
    );
    let image = node(&runtime, "live").image.clone();

    let more = node(&runtime, "more").id;
    assert!(runtime.dispatch(more, Event::Click).unwrap());
    assert_eq!(
        effects(&runtime, "live"),
        ImageEffects {
            blur: 8.0,
            saturation: 2.0
        }
    );
    // the source identity, which keys loading and decoding, is unchanged.
    assert_eq!(node(&runtime, "live").image, image);

    // an update outside the range is rejected and the published tree stays.
    let before = runtime.surfaces().to_vec();
    let bad = node(&runtime, "bad").id;
    let error = runtime.dispatch(bad, Event::Click).unwrap_err().to_string();
    assert!(
        // both properties are out of range; either may be evaluated first.
        error.contains("must be a finite number between 0 and"),
        "{error}"
    );
    assert_eq!(runtime.surfaces(), before.as_slice());
}

#[test]
fn image_effects_reject_bad_values_and_other_widgets() {
    for (source, message) in [
        ("ui.image { blur = -1 }", "between 0 and 64"),
        ("ui.image { blur = 64.5 }", "between 0 and 64"),
        ("ui.image { blur = 0/0 }", "blur must be finite"),
        ("ui.image { blur = '4' }", "blur must be a number"),
        ("ui.image { saturation = 2.5 }", "between 0 and 2"),
        ("ui.image { saturation = -0.1 }", "between 0 and 2"),
        (
            "ui.image { saturation = true }",
            "saturation must be a number",
        ),
        ("ui.row { blur = 4 }", "unknown property \"blur\""),
        (
            "ui.text { text = 'x', saturation = 0 }",
            "unknown property \"saturation\"",
        ),
        (
            "ui.image { style = { blur = 4 } }",
            "unknown property \"style.blur\"",
        ),
        (
            "ui.row { blur = ui.transition(function() return 4 end, { duration = 10 }) }",
            "unknown property \"blur\"",
        ),
        (
            "ui.image { blur = ui.transition(function() return 80 end, { duration = 10 }) }",
            "between 0 and 64",
        ),
    ] {
        let (_directory, runtime) =
            load(&format!("return ui.panel {{ children = {{ {source} }} }}"));
        let error = match runtime {
            Ok(_) => panic!("accepted {source}"),
            Err(error) => error,
        };
        assert!(error.contains(message), "{source}: {error}");
    }
}

#[test]
fn image_effects_follow_transitions_springs_and_motions() {
    let (_directory, runtime) = load(
        r#"
        local on = ui.state(false)
        local level = ui.motion(1, { kind = 'transition', duration = 100, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function()
            on:set(true)
            level:to(0)
        end }
        return ui.panel { children = {
            ui.image {
                key = 'eased',
                source = 'a.png',
                blur = ui.transition(function() return on:get() and 20 or 0 end,
                    { duration = 100, easing = 'linear' }),
                saturation = function() return level:get() end,
            },
            ui.image {
                key = 'sprung',
                source = 'a.png',
                blur = ui.spring(function() return on:get() and 64 or 0 end,
                    { stiffness = 100, damping = 0.1, epsilon = 0.001 }),
                saturation = ui.spring(function() return on:get() and 2 or 1 end,
                    { stiffness = 100, damping = 0.1, epsilon = 0.001 }),
            },
        } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let image = node(&runtime, "eased").image.clone();
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(50)).unwrap();
    close(effects(&runtime, "eased").blur, 10.0);
    close(effects(&runtime, "eased").saturation, 0.5);
    assert_eq!(node(&runtime, "eased").image, image);

    // an underdamped spring overshoots internally; published values stay valid.
    for step in 1..200 {
        runtime
            .advance(start + Duration::from_millis(step * 10))
            .unwrap();
        let sprung = effects(&runtime, "sprung");
        assert!(sprung.validate().is_ok(), "{sprung:?}");
    }
    runtime.advance(start + Duration::from_secs(60)).unwrap();
    close(effects(&runtime, "eased").blur, 20.0);
    close(effects(&runtime, "eased").saturation, 0.0);
    close(effects(&runtime, "sprung").blur, 64.0);
    close(effects(&runtime, "sprung").saturation, 2.0);
}
