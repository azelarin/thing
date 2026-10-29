use std::fs;

use thing_core::{Brush, GradientPoint};
use thing_runtime::{Event, Runtime};

fn runtime(background: &str) -> mlua::Result<Runtime> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        format!("return ui.panel {{ style = {{ background = {background} }} }}"),
    )
    .unwrap();
    Runtime::load(path)
}

#[test]
fn reads_relative_endpoints_without_losing_far_coordinate_precision() {
    let runtime = runtime(
        r#"{
        kind = 'linear', from = { x = -100000000, y = 0 }, to = { x = 100000001, y = 0 },
        stops = {
            { position = 0.5 - 1e-9, color = '#000000' },
            { position = 0.5 + 1e-9, color = '#ffffff' },
        },
    }"#,
    )
    .unwrap();
    let Brush::Linear(gradient) = &runtime.root().unwrap().style.background else {
        panic!("expected a gradient")
    };
    assert_eq!(
        gradient.to,
        GradientPoint {
            x: 100_000_001.0,
            y: 0.0
        }
    );
    assert!(gradient.stops[0].position < 0.5);
    assert!(gradient.stops[1].position > 0.5);
}

#[test]
fn rejects_malformed_brushes() {
    let valid = "{ kind = 'linear', from = { x = 0, y = 0 }, to = { x = 1, y = 0 }, stops = { { position = 0, color = '#000000' }, { position = 1, color = '#ffffff' } } }";
    for (from, to) in [
        ("kind = 'linear'", "kind = 'radial'"),
        ("x = 1, y = 0", "x = 0, y = 0"),
        ("x = 1, y = 0", "x = math.huge, y = 0"),
        ("x = 1, y = 0", "x = 1, y = 0, offset = 1"),
        ("position = 1", "position = -1e-100"),
        ("position = 1", "position = 1.00000000001"),
        ("position = 1", "offset = 1"),
        ("position = 1", "position = '1'"),
        ("color = '#ffffff'", "color = 'white'"),
        ("kind = 'linear'", "kind = 'linear', extra = true"),
        ("stops = {", "stops = { extra = true,"),
    ] {
        let source = valid.replace(from, to);
        assert!(runtime(&source).is_err(), "accepted {source}");
    }
    for stops in [
        "{}",
        "{ { position = 0, color = '#000000' } }",
        "{ [1] = { position = 0, color = '#000000' }, [3] = { position = 1, color = '#ffffff' } }",
    ] {
        let source = format!(
            "{{ kind = 'linear', from = {{ x = 0, y = 0 }}, to = {{ x = 1, y = 0 }}, stops = {stops} }}"
        );
        assert!(runtime(&source).is_err(), "accepted {source}");
    }
}

#[test]
fn rejected_reactive_brush_preserves_the_tree_and_discards_actions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        local bad = ui.state(false)
        return ui.panel { children = { ui.button {
            on_click = function()
                services.media.next()
                bad:set(true)
            end,
            style = { background = function()
                return {
                    kind = 'linear', from = { x = 0, y = 0 }, to = { x = 1, y = 0 },
                    stops = {
                        { position = 0.5, color = '#ff0000' },
                        { position = if bad:get() then 0.25 else 1, color = '#0000ff' },
                    },
                }
            end },
        } } }
    "#,
    )
    .unwrap();
    let mut runtime = Runtime::load(path).unwrap();
    let old = runtime.root().unwrap().children[0].clone();
    assert!(runtime.dispatch(old.id, Event::Click).is_err());
    assert_eq!(
        runtime.root().unwrap().children[0].style.background,
        old.style.background
    );
    assert!(runtime.take_actions().is_empty());
}

#[test]
fn path_fill_accepts_a_brush_and_three_coincident_stops() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        return ui.panel { children = { ui.path {
            data = 'M0 0H1V1H0Z', view_box = { x = 0, y = 0, width = 1, height = 1 },
            fill = { kind = 'linear', from = { x = 0, y = 0 }, to = { x = 0, y = 1 },
                stops = {
                    { position = 0.5, color = '#ff0000' },
                    { position = 0.5, color = '#00ff00' },
                    { position = 0.5, color = '#0000ff' },
                },
            },
        } } }
    "#,
    )
    .unwrap();
    let runtime = Runtime::load(path).unwrap();
    assert!(matches!(
        runtime.root().unwrap().children[0].path.fill,
        Some(Brush::Linear(_))
    ));
}
