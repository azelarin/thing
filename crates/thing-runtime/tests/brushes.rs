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

const RADIAL: &str = "{ kind = 'radial', center = { x = 0.5, y = 0.5 }, radius = { x = 0.5, y = 0.5 }, stops = { { position = 0, color = '#ffffff' }, { position = 1, color = '#00000000' } } }";

#[test]
fn reads_radial_gradients_with_off_box_centers_and_large_radii() {
    let source = RADIAL
        .replace(
            "center = { x = 0.5, y = 0.5 }",
            "center = { x = -2, y = 1.5 }",
        )
        .replace(
            "radius = { x = 0.5, y = 0.5 }",
            "radius = { x = 3, y = 0.25 }",
        );
    let runtime = runtime(&source).unwrap();
    let Brush::Radial(gradient) = &runtime.root().unwrap().style.background else {
        panic!("expected a radial gradient")
    };
    assert_eq!(gradient.center, GradientPoint { x: -2.0, y: 1.5 });
    assert_eq!(gradient.radius, GradientPoint { x: 3.0, y: 0.25 });
    assert_eq!(gradient.stops.len(), 2);
    assert_eq!(gradient.stops[1].color, "#00000000".parse().unwrap());
}

#[test]
fn rejects_malformed_radial_gradients() {
    for (from, to, message) in [
        ("kind = 'radial'", "kind = 'conic'", "'linear' or 'radial'"),
        // fields of the other kind are unknown here, and the reverse.
        (
            "kind = 'radial'",
            "kind = 'radial', from = { x = 0, y = 0 }",
            "unknown radial gradient property \"from\"",
        ),
        (
            "kind = 'radial'",
            "kind = 'radial', focal = { x = 0, y = 0 }",
            "unknown radial gradient property \"focal\"",
        ),
        (
            "kind = 'radial'",
            "kind = 'radial', tile = 'mirror'",
            "unknown radial gradient property",
        ),
        (
            "kind = 'radial'",
            "kind = 'linear', to = { x = 1, y = 0 }, from = { x = 0, y = 0 }",
            "unknown linear gradient property",
        ),
        (
            "center = { x = 0.5, y = 0.5 }, ",
            "",
            "gradient center must be a table",
        ),
        (
            "radius = { x = 0.5, y = 0.5 }, ",
            "",
            "gradient radius must be a table",
        ),
        (
            "center = { x = 0.5, y = 0.5 }",
            "center = { 0.5, 0.5 }",
            "unknown gradient center property",
        ),
        (
            "center = { x = 0.5, y = 0.5 }",
            "center = { x = 0.5 }",
            "gradient center y must be a number",
        ),
        (
            "center = { x = 0.5, y = 0.5 }",
            "center = { x = 0/0, y = 0.5 }",
            "gradient center x must be finite",
        ),
        (
            "radius = { x = 0.5, y = 0.5 }",
            "radius = { x = math.huge, y = 0.5 }",
            "gradient radius x must be finite",
        ),
        (
            "radius = { x = 0.5, y = 0.5 }",
            "radius = { x = 0, y = 0.5 }",
            "must be positive",
        ),
        (
            "radius = { x = 0.5, y = 0.5 }",
            "radius = { x = 0.5, y = -1 }",
            "must be positive",
        ),
        (
            "radius = { x = 0.5, y = 0.5 }",
            "radius = { x = 0.5, y = 0.5, z = 1 }",
            "unknown gradient radius property",
        ),
        (
            "radius = { x = 0.5, y = 0.5 }",
            "radius = 0.5",
            "gradient radius must be a table",
        ),
        (
            "position = 1",
            "position = 0/0",
            "gradient stop position must be finite",
        ),
        (
            "{ position = 0, color = '#ffffff' }, ",
            "",
            "2 through 16 stops",
        ),
        (
            "{ position = 1, color = '#00000000' } }",
            "{ position = 1, color = '#00000000' }, [4] = { position = 1, color = '#000000' } }",
            "dense array",
        ),
        (
            "color = '#00000000'",
            "color = '#00000000', offset = 1",
            "unknown gradient stop property",
        ),
    ] {
        let source = RADIAL.replacen(from, to, 1);
        assert_ne!(source, RADIAL, "{from}");
        let error = match runtime(&source) {
            Ok(_) => panic!("accepted {source}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(message), "{source}: {error}");
    }
}

#[test]
fn a_rejected_reactive_radial_update_keeps_the_brush_until_a_valid_one() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        local radius = ui.state(0.5)
        return ui.panel { children = { ui.button {
            on_click = function()
                services.media.next()
                radius:set(if radius:get() == 0.5 then 0 else 0.25)
            end,
            style = { background = function()
                return {
                    kind = 'radial',
                    center = { x = 0.5, y = 0.5 },
                    radius = { x = radius:get(), y = 0.5 },
                    stops = {
                        { position = 0, color = '#ffffff' },
                        { position = 1, color = '#000000' },
                    },
                }
            end },
        } } }
    "#,
    )
    .unwrap();
    let mut runtime = Runtime::load(path).unwrap();
    let old = runtime.root().unwrap().children[0].clone();
    // a zero radius rejects the whole update, including its action.
    let error = runtime
        .dispatch(old.id, Event::Click)
        .unwrap_err()
        .to_string();
    assert!(error.contains("must be positive"), "{error}");
    assert_eq!(
        runtime.root().unwrap().children[0].style.background,
        old.style.background
    );
    assert!(runtime.take_actions().is_empty());

    // the state was written before the update failed, so the next click
    // moves it to a valid radius that replaces the whole brush.
    runtime.dispatch(old.id, Event::Click).unwrap();
    let Brush::Radial(gradient) = &runtime.root().unwrap().children[0].style.background else {
        panic!("expected a radial gradient")
    };
    assert_eq!(gradient.radius, GradientPoint { x: 0.25, y: 0.5 });
    assert_eq!(runtime.take_actions().len(), 1);
}
