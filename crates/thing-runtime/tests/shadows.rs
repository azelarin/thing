use std::fs;

use thing_core::{Color, Point, Shadow};
use thing_runtime::{Event, Runtime};

fn runtime(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path);
    (directory, runtime)
}

#[test]
fn box_and_path_shadows_share_values_and_can_be_removed_reactively() {
    let (_directory, runtime) = runtime(
        r#"
        local enabled = ui.state(true)
        local function shadow()
            return if enabled:get() then { color = '#11223380', blur = 12, offset_y = 4, spread = -2 } else nil
        end
        return ui.panel { children = { ui.button {
            on_click = function() enabled:set(false) end,
            style = { box_shadow = shadow },
            children = { ui.path {
                data = 'M0 0H1V1H0Z',
                view_box = { x = 0, y = 0, width = 1, height = 1 },
                shadow = shadow,
            } },
        } } }
    "#,
    );
    let mut runtime = runtime.unwrap();
    let button = &runtime.root().unwrap().children[0];
    let expected = Some(Shadow {
        color: Color([0x11, 0x22, 0x33, 0x80]),
        offset: Point { x: 0.0, y: 4.0 },
        blur: 12.0,
        spread: -2.0,
    });
    assert_eq!(button.style.box_shadow, expected);
    assert_eq!(button.children[0].path.shadow, expected);
    let id = button.id;
    runtime.dispatch(id, Event::Click).unwrap();
    let button = &runtime.root().unwrap().children[0];
    assert_eq!(button.style.box_shadow, None);
    assert_eq!(button.children[0].path.shadow, None);
}

#[test]
fn validates_raw_shadow_values_before_narrowing_numbers() {
    for value in [
        "false",
        "{}",
        "{ color = 'red' }",
        "{ color = '#000000', blur = -1e-100 }",
        "{ color = '#000000', blur = 32.000000001 }",
        "{ color = '#000000', blur = math.huge }",
        "{ color = '#000000', spread = 33 }",
        "{ color = '#000000', offset_x = 100001 }",
        "{ color = '#000000', softness = 4 }",
    ] {
        let source = format!("return ui.panel {{ style = {{ box_shadow = {value} }} }}");
        assert!(runtime(&source).1.is_err(), "accepted {value}");
    }
    let (_directory, runtime) =
        runtime("return ui.panel { style = { box_shadow = { color = '#000000' } } }");
    let runtime = runtime.unwrap();
    let shadow = runtime.root().unwrap().style.box_shadow.unwrap();
    assert_eq!(
        (shadow.blur, shadow.spread, shadow.offset),
        (0.0, 0.0, Point::default())
    );
}

#[test]
fn rejected_shadow_updates_keep_the_tree_and_discard_actions() {
    let (_directory, runtime) = runtime(
        r#"
        local bad = ui.state(false)
        return ui.panel { children = { ui.button {
            on_click = function() services.media.next(); bad:set(true) end,
            style = { box_shadow = function()
                return { color = '#000000', blur = if bad:get() then -1 else 12 }
            end },
        } } }
    "#,
    );
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[0].clone();
    assert!(runtime.dispatch(button.id, Event::Click).is_err());
    assert_eq!(
        runtime.root().unwrap().children[0].style.box_shadow,
        button.style.box_shadow
    );
    assert!(runtime.take_actions().is_empty());
}

#[test]
fn the_guide_example_toggles_a_box_shadow_and_keeps_its_path_shadow() {
    // the shadow example from docs/configuration.md, inside a panel.
    let (_directory, runtime) = runtime(
        r#"
        local raised = ui.state(false)

        return ui.panel { children = {
        ui.button {
            on_click = function() raised:set(not raised:get()) end,
            style = {
                width = 120,
                height = 40,
                radius = 8,
                background = '#1e1e2e',
                box_shadow = function()
                    if not raised:get() then
                        return nil
                    end
                    return { color = '#00000080', blur = 12, offset_y = 4 }
                end,
            },
            children = {
                ui.path {
                    data = 'M 12 2 L 22 20 H 2 Z',
                    view_box = { x = 0, y = 0, width = 24, height = 24 },
                    fill = '#89b4fa',
                    shadow = { color = '#11111bcc', blur = 2, offset_x = 1, offset_y = 1 },
                    style = { width = 18, height = 18 },
                },
            },
        }
        } }
    "#,
    );
    let mut runtime = runtime.unwrap();
    let button = runtime.root().unwrap().children[0].clone();
    assert_eq!(button.style.box_shadow, None);
    assert_eq!(
        button.children[0].path.shadow,
        Some(Shadow {
            color: Color([0x11, 0x11, 0x1b, 0xcc]),
            offset: Point { x: 1.0, y: 1.0 },
            blur: 2.0,
            spread: 0.0,
        })
    );
    runtime.dispatch(button.id, Event::Click).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].style.box_shadow,
        Some(Shadow {
            color: Color([0, 0, 0, 0x80]),
            offset: Point { x: 0.0, y: 4.0 },
            blur: 12.0,
            spread: 0.0,
        })
    );
    runtime.dispatch(button.id, Event::Click).unwrap();
    assert_eq!(runtime.root().unwrap().children[0].style.box_shadow, None);
}
