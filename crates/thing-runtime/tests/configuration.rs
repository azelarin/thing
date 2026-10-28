use std::{fs, path::PathBuf, time::Duration};
use tempfile::TempDir;
use thing_core::{Color, CrossAlign, Kind, MainAlign, TextAlign, TextOverflow};
use thing_runtime::{
    Action, AudioDevice, AudioSnapshot, AudioSource, AudioStream, Event, Limits, Runtime, Scalar,
};

fn fixture(source: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

#[test]
fn loads_typed_luau_and_relative_theme_modules() {
    let (directory, path) = fixture(
        r#"
        local theme = require('./theme')
        local count: number = 21
        return ui.panel { children = {
            ui.text { text = tostring(count * 2), style = theme },
        } }
    "#,
    );
    fs::write(
        directory.path().join("theme.luau"),
        "return { foreground = '#abcdef' }",
    )
    .unwrap();
    let runtime = Runtime::load(&path).unwrap();
    assert_eq!(runtime.root().children[0].text, "42");
    assert_eq!(runtime.files().len(), 2);
}

#[test]
fn updates_only_dependencies_that_were_read() {
    let (_directory, path) = fixture(
        r#"
        local count = ui.state(0)
        local evaluated = 0
        return ui.panel { children = {
            ui.text { text = function() return tostring(count:get()) end },
            ui.text { text = function()
                evaluated += 1
                assert(evaluated == 1, 'unrelated binding was reevaluated')
                return 'unchanged'
            end },
            ui.button { on_click = function() count:set(count:get() + 1) end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let button = runtime.root().children[2].id;
    assert!(runtime.dispatch(button, Event::Click).unwrap());
    assert_eq!(runtime.root().children[0].text, "1");
    assert!(!runtime.refresh().unwrap());
}

#[test]
fn interaction_styles_update_reactively() {
    let (_directory, path) = fixture(
        r#"
        local alternate = ui.state(false)
        return ui.panel { children = {
            ui.button {
                style = {
                    hover_overlay = function()
                        return if alternate:get() then '#11111122' else '#00000000'
                    end,
                    pressed_overlay = function()
                        return if alternate:get() then '#22222244' else '#00000000'
                    end,
                    disabled_opacity = function() return if alternate:get() then 0.25 else 1 end,
                    focus_ring_color = function()
                        return if alternate:get() then '#eeeeee' else '#00000000'
                    end,
                    focus_ring_width = function() return if alternate:get() then 1 else 0 end,
                },
                on_click = function() alternate:set(true) end,
            },
        } }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let button = &runtime.root().children[0];
    assert_eq!(button.style.hover_overlay, Color::TRANSPARENT);
    assert_eq!(button.style.pressed_overlay, Color::TRANSPARENT);
    assert_eq!(button.style.disabled_opacity, 1.0);
    assert_eq!(button.style.focus_ring_color, Color::TRANSPARENT);
    assert_eq!(button.style.focus_ring_width, 0.0);

    let id = button.id;
    assert!(runtime.dispatch(id, Event::Click).unwrap());
    let button = &runtime.root().children[0];
    assert_eq!(button.style.hover_overlay, Color([17, 17, 17, 34]));
    assert_eq!(button.style.pressed_overlay, Color([34, 34, 34, 68]));
    assert_eq!(button.style.disabled_opacity, 0.25);
    assert_eq!(button.style.focus_ring_color, Color([238, 238, 238, 255]));
    assert_eq!(button.style.focus_ring_width, 1.0);
}

#[test]
fn subscriptions_follow_conditional_reads() {
    let (_directory, path) = fixture(
        r#"
        local use_a, a, b = ui.state(true), ui.state('a'), ui.state('b')
        local calls = 0
        return ui.panel { children = {
            ui.text { text = function()
                calls += 1
                assert(calls <= 2, 'stale dependency')
                return if use_a:get() then a:get() else b:get()
            end },
            ui.button { on_click = function() use_a:set(false) end },
            ui.button { on_click = function() a:set('unused') end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    assert!(
        runtime
            .dispatch(runtime.root().children[1].id, Event::Click)
            .unwrap()
    );
    assert_eq!(runtime.root().children[0].text, "b");
    assert!(
        !runtime
            .dispatch(runtime.root().children[2].id, Event::Click)
            .unwrap()
    );
}

#[test]
fn writing_the_same_value_does_not_invalidate_bindings() {
    let (_directory, path) = fixture(
        r#"
        local count = ui.state(0)
        local calls = 0
        return ui.panel { children = {
            ui.text { text = function()
                calls += 1
                assert(calls == 1)
                return tostring(count:get())
            end },
            ui.button { on_click = function() count:set(0) end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    assert!(
        !runtime
            .dispatch(runtime.root().children[1].id, Event::Click)
            .unwrap()
    );
}

#[test]
fn hover_edges_update_state_even_after_the_widget_disables_itself() {
    let (_directory, path) = fixture(
        r#"
        local hovered = ui.state(false)
        return ui.panel { children = {
            ui.text { text = function() return tostring(hovered:get()) end },
            ui.button {
                enabled = function() return not hovered:get() end,
                on_hover = function(inside) hovered:set(inside) end,
            },
        } }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let button = runtime.root().children[1].id;

    assert!(runtime.dispatch(button, Event::Hover(true)).unwrap());
    assert_eq!(runtime.root().children[0].text, "true");
    assert!(!runtime.root().children[1].enabled);
    assert!(runtime.dispatch(button, Event::Hover(false)).unwrap());
    assert_eq!(runtime.root().children[0].text, "false");
    assert!(runtime.root().children[1].enabled);
}

#[test]
fn hover_cleanup_uses_the_retired_handler() {
    let (_directory, path) = fixture(
        r#"
        local hovered = ui.state(false)
        local enters = ui.state(0)
        local leaves = ui.state(0)
        return ui.panel { children = {
            ui.text { text = function()
                return `{enters:get()}:{leaves:get()}:{tostring(hovered:get())}`
            end },
            ui.row { children = function()
                if hovered:get() then
                    return { ui.button { key = 'target' } }
                end
                return { ui.button {
                    key = 'target',
                    on_hover = function(inside)
                        if inside then
                            enters:set(enters:get() + 1)
                        else
                            leaves:set(leaves:get() + 1)
                        end
                        hovered:set(inside)
                    end,
                } }
            end },
        } }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let button = runtime.root().children[1].children[0].id;

    assert!(runtime.dispatch(button, Event::Hover(true)).unwrap());
    assert_eq!(runtime.root().children[0].text, "1:1:false");
    assert_eq!(runtime.root().children[1].children[0].id, button);
    assert!(!runtime.dispatch(button, Event::Hover(false)).unwrap());
    assert_eq!(runtime.root().children[0].text, "1:1:false");
    assert!(runtime.dispatch(button, Event::Hover(true)).unwrap());
    assert_eq!(runtime.root().children[0].text, "2:2:false");
}

#[test]
fn hover_cleanup_runs_after_the_target_is_removed() {
    let (_directory, path) = fixture(
        r#"
        local present = ui.state(true)
        local hovered = ui.state(false)
        return ui.panel { children = {
            ui.text { text = function() return tostring(hovered:get()) end },
            ui.row { children = function()
                if not present:get() then return {} end
                return { ui.button {
                    on_hover = function(inside)
                        hovered:set(inside)
                        present:set(not inside)
                    end,
                } }
            end },
        } }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let old = runtime.root().children[1].children[0].id;

    assert!(runtime.dispatch(old, Event::Hover(true)).unwrap());
    assert_eq!(runtime.root().children[0].text, "false");
    let replacement = runtime.root().children[1].children[0].id;
    assert_ne!(replacement, old);
    assert!(!runtime.dispatch(old, Event::Hover(false)).unwrap());
}

#[test]
fn parses_automatic_popup_activation_and_delays() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = { ui.button { children = { ui.popup {
            open_on_hover = true,
            open_on_click = true,
            open_delay = 175,
            close_delay = 225,
            on_hover = function(_) end,
        } } } } }
        "#,
    );
    let runtime = Runtime::load(path).unwrap();
    let popup = &runtime.root().children[0].children[0];

    assert!(popup.popup.open_on_hover);
    assert!(popup.popup.open_on_click);
    assert_eq!(popup.popup.open_delay_ms, 175);
    assert_eq!(popup.popup.close_delay_ms, 225);
}

#[test]
fn rejects_ambiguous_or_unanchored_automatic_popups() {
    for source in [
        "return ui.panel { children = { ui.popup { open_on_hover = true } } }",
        r#"
        return ui.panel { children = { ui.button { children = {
            ui.popup { open_on_hover = true },
            ui.popup { open_on_click = true },
        } } } }
        "#,
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load(path).is_err());
    }
}

#[test]
fn rejects_invalid_popup_delays() {
    for delay in ["-1", "1.5", "60001"] {
        let (_directory, path) = fixture(&format!(
            "return ui.panel {{ children = {{ ui.button {{ children = {{ ui.popup {{ open_on_hover = true, open_delay = {delay} }} }} }} }} }} }}"
        ));
        assert!(Runtime::load(path).is_err());
    }
}

#[test]
fn reports_scroll_handlers_even_when_they_do_not_change_state() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = {
            ui.button { on_scroll = function() end },
            ui.button {},
            ui.button { enabled = false, on_scroll = function() end },
            ui.button { visible = false, on_scroll = function() end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let buttons = &runtime.root().children;
    assert!(runtime.has_scroll_handler(buttons[0].id));
    assert!(!runtime.has_scroll_handler(buttons[1].id));
    assert!(!runtime.has_scroll_handler(buttons[2].id));
    assert!(!runtime.has_scroll_handler(buttons[3].id));
    assert!(
        !runtime
            .dispatch(buttons[0].id, Event::Scroll { x: 0.0, y: 1.0 })
            .unwrap()
    );
}

#[test]
fn parses_text_overflow_alignment_and_tabular_digits() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = { ui.text {
            text = 'a long device description',
            max_lines = 1,
            overflow = 'ellipsis',
            style = { width = 96, text_align = 'end', tabular_digits = true },
        } } }
        "#,
    );
    let runtime = Runtime::load(path).unwrap();
    let text = &runtime.root().children[0];

    assert_eq!(text.max_lines, Some(1));
    assert_eq!(text.text_overflow, TextOverflow::Ellipsis);
    assert_eq!(text.style.text_align, TextAlign::End);
    assert!(text.style.tabular_digits);
}

#[test]
fn rejects_invalid_text_layout_options() {
    for source in [
        "return ui.panel { children = { ui.text { max_lines = 0 } } }",
        "return ui.panel { children = { ui.text { max_lines = 1.5 } } }",
        "return ui.panel { children = { ui.text { overflow = 'fade' } } }",
        "return ui.panel { children = { ui.text { overflow = 'ellipsis' } } }",
        "return ui.panel { children = { ui.text { max_lines = 2, overflow = 'ellipsis' } } }",
        "return ui.panel { children = { ui.text { style = { text_align = 'right' } } } }",
        "return ui.panel { children = { ui.text { style = { tabular_digits = 1 } } } }",
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load(path).is_err(), "accepted {source}");
    }
}

#[test]
fn parses_flex_layout_controls() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = { ui.row {
            style = {
                min_width = 40, max_width = 240,
                min_height = 20, max_height = 80,
                grow = 1, shrink = 2, wrap = true,
                align = 'end', align_self = 'stretch', align_content = 'center',
                justify = 'space_between',
            },
        } } }
        "#,
    );
    let runtime = Runtime::load(path).unwrap();
    let style = &runtime.root().children[0].style;

    assert_eq!(style.min_width, Some(40.0));
    assert_eq!(style.max_width, Some(240.0));
    assert_eq!(style.min_height, Some(20.0));
    assert_eq!(style.max_height, Some(80.0));
    assert_eq!(style.grow, 1.0);
    assert_eq!(style.shrink, Some(2.0));
    assert!(style.wrap);
    assert_eq!(style.align, CrossAlign::End);
    assert_eq!(style.align_self, Some(CrossAlign::Stretch));
    assert_eq!(style.align_content, CrossAlign::Center);
    assert_eq!(style.justify, MainAlign::SpaceBetween);
}

#[test]
fn flex_layout_properties_recover_after_an_invalid_reactive_value() {
    let (_directory, path) = fixture(
        r#"
        local align = ui.state('start')
        return ui.panel { children = {
            ui.row { style = { align = function() return align:get() end } },
            ui.button { on_click = function() align:set('baseline') end },
            ui.button { on_click = function() align:set('end') end },
        } }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    assert_eq!(runtime.root().children[0].style.align, CrossAlign::Start);

    let invalid = runtime.root().children[1].id;
    assert!(runtime.dispatch(invalid, Event::Click).is_err());
    assert_eq!(runtime.root().children[0].style.align, CrossAlign::Start);
    assert!(!runtime.refresh().unwrap());

    let valid = runtime.root().children[2].id;
    assert!(runtime.dispatch(valid, Event::Click).unwrap());
    assert_eq!(runtime.root().children[0].style.align, CrossAlign::End);
}

#[test]
fn rejects_invalid_flex_layout_options() {
    for source in [
        "return ui.panel { children = { ui.row { style = { align = 'baseline' } } } }",
        "return ui.panel { children = { ui.row { style = { align_content = 'space_between' } } } }",
        "return ui.panel { children = { ui.row { style = { justify = 'stretch' } } } }",
        "return ui.panel { children = { ui.row { style = { wrap = 1 } } } }",
        "return ui.panel { children = { ui.row { style = { shrink = -1 } } } }",
        "return ui.panel { children = { ui.row { style = { min_width = 20, max_width = 10 } } } }",
        "return ui.panel { children = { ui.scroll { style = { wrap = true } } } }",
        "return ui.panel { style = { max_width = 800 } }",
        "return ui.panel { children = { ui.button { children = { ui.popup { style = { min_width = 200 } } } } } }",
        "return ui.panel { children = { ui.centerbox { style = { justify = 'center' }, children = { ui.row {}, ui.row {}, ui.row {} } } } }",
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load(path).is_err(), "accepted {source}");
    }
}

#[test]
fn failed_reload_preserves_widgets_and_local_state() {
    let (_directory, path) = fixture(
        r#"
        local count = ui.state(0)
        return ui.panel { children = {
            ui.text { text = function() return tostring(count:get()) end },
            ui.button { on_click = function() count:set(count:get() + 1) end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(&path).unwrap();
    let button = runtime.root().children[1].id;
    runtime.dispatch(button, Event::Click).unwrap();
    let before = runtime.root().clone();
    fs::write(&path, "return ui.panel { nope = true }").unwrap();
    assert!(runtime.reload(&path).is_err());
    assert_eq!(runtime.root(), &before);
    runtime.dispatch(button, Event::Click).unwrap();
    assert_eq!(runtime.root().children[0].text, "2");
    fs::write(&path, "return ui.panel {}").unwrap();
    runtime.reload(&path).unwrap();
    assert!(runtime.root().children.is_empty());
}

#[test]
fn invalid_reactive_dimensions_do_not_reach_the_renderer() {
    let (_directory, path) = fixture(
        r#"
        local width = ui.state(100)
        return ui.panel { children = {
            ui.text { text = 'hello', style = { width = function() return width:get() end } },
            ui.slider { min = -10, max = 100, on_change = function(v) width:set(v) end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let slider = runtime.root().children[1].id;
    assert!(runtime.dispatch(slider, Event::Change(-1.0)).is_err());
    assert_eq!(runtime.root().children[0].style.width, Some(100.0));
    assert!(runtime.dispatch(slider, Event::Change(50.0)).unwrap());
    assert_eq!(runtime.root().children[0].style.width, Some(50.0));
}

#[test]
fn rejects_state_writes_inside_properties() {
    let (_directory, path) = fixture(
        r#"
        local state = ui.state(0)
        return ui.panel { children = { ui.text { text = function()
            state:set(1)
            return 'bad'
        end } } }
    "#,
    );
    let error = Runtime::load(path).err().unwrap().to_string();
    assert!(error.contains("must not change state"), "{error}");
}

#[test]
fn rejects_circular_imports_and_reports_the_file() {
    let (directory, path) = fixture("return require('./second')");
    fs::write(
        directory.path().join("second.luau"),
        "return require('./panel')",
    )
    .unwrap();
    let error = Runtime::load(&path).err().unwrap().to_string();
    assert!(error.contains("circular import"), "{error}");
    assert!(error.contains("panel.luau"), "{error}");
}

#[test]
fn rejects_cyclic_trees_and_sparse_children() {
    for source in [
        "local p = ui.panel {}; p.children = {p}; return p",
        "return ui.panel { children = { [2] = ui.text { text = 'hole' } } }",
        "return ui.panel { children = { unexpected = ui.text {} } }",
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load(path).is_err());
    }
}

#[test]
fn interrupts_runaway_scripts() {
    let (_directory, path) = fixture("while true do end");
    let error = Runtime::load_with_limits(
        path,
        Limits {
            execution_time: Duration::from_millis(10),
            ..Limits::default()
        },
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("execution time limit"), "{error}");
}

#[test]
fn disabled_parent_blocks_descendant_input() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { enabled = false, children = {
            ui.button { on_click = function() error('must not run') end }
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    assert_eq!(runtime.root().children[0].kind, Kind::Button);
    assert!(
        !runtime
            .dispatch(runtime.root().children[0].id, Event::Click)
            .unwrap()
    );
}

#[test]
fn failed_batch_retries_every_affected_property() {
    let (_directory, path) = fixture(
        r#"
        local width, label = ui.state(20), ui.state('before')
        return ui.panel { children = {
            ui.text { text = function() return label:get() end,
                style = { width = function() return width:get() end } },
            ui.button { on_click = function() width:set(-1); label:set('after') end },
            ui.button { on_click = function() width:set(30) end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    assert!(
        runtime
            .dispatch(runtime.root().children[1].id, Event::Click)
            .is_err()
    );
    assert_eq!(runtime.root().children[0].text, "before");
    assert!(!runtime.refresh().unwrap());
    runtime
        .dispatch(runtime.root().children[2].id, Event::Click)
        .unwrap();
    assert_eq!(runtime.root().children[0].text, "after");
    assert_eq!(runtime.root().children[0].style.width, Some(30.0));
}

#[test]
fn oversized_script_allocation_returns_an_error() {
    let (_directory, path) =
        fixture("return ui.panel { children = { ui.text { text = string.rep('x', 8000000) } } }");
    assert!(
        Runtime::load_with_limits(
            path,
            Limits {
                memory_bytes: 1024 * 1024,
                ..Limits::default()
            }
        )
        .is_err()
    );
}

#[test]
fn services_are_reactive_and_read_only() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = {
            ui.text { text = function() return services.clock.time:get() end },
            ui.button { on_click = function() services.clock.time:set('wrong') end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    runtime
        .set_service("clock.time", Scalar::String("12:34".into()))
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(runtime.root().children[0].text, "12:34");
    assert!(
        runtime
            .dispatch(runtime.root().children[1].id, Event::Click)
            .is_err()
    );
    assert!(
        runtime
            .set_service("audio.volume", Scalar::Number(f64::NAN))
            .is_err()
    );
    assert!(
        runtime
            .set_service("audio.available", Scalar::String("true".into()))
            .is_err()
    );
}

#[test]
fn failed_events_discard_their_service_actions() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = {
            ui.button { on_click = function() services.audio.set_muted(true) end },
            ui.button { on_click = function()
                services.audio.set_volume(0.2)
                error('broken handler')
            end },
            ui.button { on_click = function() services.audio.set_muted('false') end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    runtime
        .dispatch(runtime.root().children[0].id, Event::Click)
        .unwrap();
    assert!(
        runtime
            .dispatch(runtime.root().children[1].id, Event::Click)
            .is_err()
    );
    assert!(
        runtime
            .dispatch(runtime.root().children[2].id, Event::Click)
            .is_err()
    );
    assert_eq!(runtime.take_actions(), vec![Action::SetMuted(true)]);
}

#[test]
fn rejects_service_actions_during_load_and_property_evaluation() {
    for source in [
        "services.audio.set_muted(true); return ui.panel {}",
        "return ui.panel { visible = function() services.audio.set_muted(true); return true end }",
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load(path).is_err());
    }
}

#[test]
fn reload_exposes_current_services_before_initializing_script_state() {
    let (_directory, path) = fixture("return ui.panel {}");
    let mut runtime = Runtime::load(&path).unwrap();
    runtime
        .set_service("audio.volume", Scalar::Number(0.75))
        .unwrap();
    fs::write(
        &path,
        r#"
        local initial = ui.state(services.audio.volume:get())
        return ui.panel { children = { ui.text { text = tostring(initial:get()) } } }
    "#,
    )
    .unwrap();
    runtime.reload(path).unwrap();
    assert_eq!(runtime.root().children[0].text, "0.75");
}

#[test]
fn device_lists_are_immutable_reactive_and_preserved_on_reload() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = {
            ui.text { text = function()
                local devices = services.audio.devices:get()
                return if #devices == 0 then 'none' else devices[1].description
            end },
            ui.button { on_click = function()
                services.audio.devices:get()[1].name = 'modified'
            end },
        } }
    "#,
    );
    let mut runtime = Runtime::load(&path).unwrap();
    runtime
        .set_audio_devices(vec![AudioDevice {
            name: "speakers".into(),
            description: "Speakers".into(),
        }])
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(runtime.root().children[0].text, "Speakers");
    assert!(
        runtime
            .dispatch(runtime.root().children[1].id, Event::Click)
            .is_err()
    );
    runtime.reload(&path).unwrap();
    assert_eq!(runtime.root().children[0].text, "Speakers");
    runtime.set_audio_devices(Vec::new()).unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(runtime.root().children[0].text, "none");
}

#[test]
fn audio_controls_follow_server_updates_and_dismiss_popups() {
    fn key(node: &thing_core::Node, name: &str) -> Option<thing_core::NodeId> {
        if node.key.as_deref() == Some(name) {
            Some(node.id)
        } else {
            node.children.iter().find_map(|child| key(child, name))
        }
    }
    let (_directory, path) = fixture(
        r#"
        local audio = services.audio
        local open = ui.state(false)
        return ui.panel { children = { ui.button {
            key = 'audio-toggle',
            on_click = function() open:set(not open:get()) end,
            children = { ui.popup {
                key = 'audio-popup',
                visible = function() return open:get() end,
                on_close = function() open:set(false) end,
                children = {
                    ui.slider {
                        key = 'volume',
                        enabled = function() return audio.available:get() end,
                        value = function() return math.min(audio.volume:get(), 1) end,
                        on_change = function(value) audio.set_volume(value) end,
                    },
                    ui.button {
                        key = 'mute',
                        on_click = function() audio.set_muted(not audio.muted:get()) end,
                    },
                    ui.button {
                        key = 'next-output',
                        enabled = function() return #audio.devices:get() > 1 end,
                        on_click = function()
                            local devices = audio.devices:get()
                            for index, device in devices do
                                if device.name == audio.device:get() then
                                    audio.set_default_sink(devices[index % #devices + 1].name)
                                    return
                                end
                            end
                        end,
                    },
                },
            } },
        } } }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    runtime
        .set_service("audio.available", Scalar::Boolean(true))
        .unwrap();
    runtime
        .set_service("audio.volume", Scalar::Number(0.5))
        .unwrap();
    runtime
        .set_service("audio.device", Scalar::String("speakers".into()))
        .unwrap();
    runtime
        .set_audio_devices(vec![
            AudioDevice {
                name: "speakers".into(),
                description: "Speakers".into(),
            },
            AudioDevice {
                name: "headphones".into(),
                description: "Headphones".into(),
            },
        ])
        .unwrap();
    runtime.refresh().unwrap();
    runtime
        .dispatch(key(runtime.root(), "audio-toggle").unwrap(), Event::Click)
        .unwrap();
    let popup = key(runtime.root(), "audio-popup").unwrap();
    assert!(runtime.root().find(popup).unwrap().visible);
    runtime
        .dispatch(key(runtime.root(), "volume").unwrap(), Event::Change(0.7))
        .unwrap();
    runtime
        .dispatch(key(runtime.root(), "mute").unwrap(), Event::Click)
        .unwrap();
    runtime
        .dispatch(key(runtime.root(), "next-output").unwrap(), Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::SetVolume(0.7),
            Action::SetMuted(true),
            Action::SetDefaultSink("headphones".into())
        ]
    );
    runtime.dispatch(popup, Event::Close).unwrap();
    assert!(!runtime.root().find(popup).unwrap().visible);
}

#[test]
fn microphone_state_is_reactive_preserved_and_controllable() {
    let (_directory, path) = fixture(
        r#"
        local audio = services.audio
        return ui.panel { children = {
            ui.text { text = function()
                local sources = audio.sources:get()
                local description = if #sources == 0 then 'none' else sources[1].description
                return `{audio.source_available:get()}:{audio.source_volume:get()}:{audio.source_muted:get()}:{audio.source_device:get()}:{description}`
            end },
            ui.button { on_click = function() audio.set_source_volume(0.8) end },
            ui.button { on_click = function() audio.set_source_muted(false) end },
            ui.button { on_click = function() audio.set_default_source('webcam') end },
            ui.button { on_click = function() audio.set_source_volume(1.1) end },
            ui.button { on_click = function() audio.sources:get()[1].name = 'changed' end },
        } }
        "#,
    );
    let mut runtime = Runtime::load(&path).unwrap();
    runtime
        .set_audio(AudioSnapshot {
            source_available: true,
            source_volume: 1.25,
            source_muted: true,
            source_device: "microphone".into(),
            sources: vec![
                AudioDevice {
                    name: "microphone".into(),
                    description: "Microphone".into(),
                },
                AudioDevice {
                    name: "webcam".into(),
                    description: "Webcam".into(),
                },
            ],
            ..AudioSnapshot::default()
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().children[0].text,
        "true:1.25:true:microphone:Microphone"
    );

    for index in 1..=3 {
        runtime
            .dispatch(runtime.root().children[index].id, Event::Click)
            .unwrap();
    }
    assert!(
        runtime
            .dispatch(runtime.root().children[4].id, Event::Click)
            .is_err()
    );
    assert!(
        runtime
            .dispatch(runtime.root().children[5].id, Event::Click)
            .is_err()
    );
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::SetSourceVolume(0.8),
            Action::SetSourceMuted(false),
            Action::SetDefaultSource("webcam".into()),
        ]
    );

    runtime.reload(path).unwrap();
    assert_eq!(
        runtime.root().children[0].text,
        "true:1.25:true:microphone:Microphone"
    );
}

#[test]
fn playback_streams_are_reactive_preserved_and_controllable() {
    let (_directory, path) = fixture(
        r#"
        local audio = services.audio
        return ui.panel { children = {
            ui.text { text = function()
                local streams = audio.streams:get()
                if #streams == 0 then return 'none' end
                local stream = streams[1]
                return `{stream.id}:{stream.application}:{stream.app_id}:{stream.name}:{stream.icon}:{stream.volume}:{stream.muted}:{stream.volume_writable}`
            end },
            ui.button { on_click = function()
                local stream = audio.streams:get()[1]
                audio.set_stream_volume(stream.id, 0.7)
            end },
            ui.button { on_click = function()
                local stream = audio.streams:get()[1]
                audio.set_stream_muted(stream.id, not stream.muted)
            end },
            ui.button { on_click = function() audio.set_stream_volume('stream-1', 1.1) end },
            ui.button { on_click = function() audio.set_stream_muted('', true) end },
            ui.button { on_click = function() audio.streams:get()[1].name = 'changed' end },
        } }
        "#,
    );
    let stream = AudioStream {
        id: "stream-1".into(),
        application: "Firefox".into(),
        app_id: "firefox".into(),
        name: "Music".into(),
        icon: "firefox".into(),
        volume: 1.25,
        muted: true,
        volume_writable: true,
    };
    let mut runtime = Runtime::load(&path).unwrap();
    runtime
        .set_audio(AudioSnapshot {
            streams: vec![stream.clone()],
            ..AudioSnapshot::default()
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().children[0].text,
        "stream-1:Firefox:firefox:Music:firefox:1.25:true:true"
    );

    runtime
        .dispatch(runtime.root().children[1].id, Event::Click)
        .unwrap();
    runtime
        .dispatch(runtime.root().children[2].id, Event::Click)
        .unwrap();
    for index in 3..=5 {
        assert!(
            runtime
                .dispatch(runtime.root().children[index].id, Event::Click)
                .is_err()
        );
    }
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::SetStreamVolume {
                id: "stream-1".into(),
                volume: 0.7,
            },
            Action::SetStreamMuted {
                id: "stream-1".into(),
                muted: false,
            },
        ]
    );

    runtime.reload(&path).unwrap();
    assert_eq!(
        runtime.root().children[0].text,
        "stream-1:Firefox:firefox:Music:firefox:1.25:true:true"
    );
    runtime
        .set_audio(AudioSnapshot {
            streams: Vec::new(),
            ..AudioSnapshot::default()
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(runtime.root().children[0].text, "none");
}

#[test]
fn complete_audio_sources_are_reactive_bounded_and_read_only() {
    let (_directory, path) = fixture(
        r#"
        local audio = services.audio
        return ui.panel { children = {
            ui.text { text = function()
                local sources = audio.all_sources:get()
                local values = { tostring(#sources) }
                for _, source in sources do
                    table.insert(values, `{source.name}:{source.monitor}`)
                end
                return table.concat(values, ':')
            end },
            ui.button { on_click = function()
                audio.all_sources:get()[1].monitor = true
            end },
        } }
        "#,
    );
    let mut runtime = Runtime::load(&path).unwrap();
    runtime
        .set_audio(AudioSnapshot {
            all_sources: vec![
                AudioSource {
                    name: "microphone".into(),
                    description: "Microphone".into(),
                    monitor: false,
                },
                AudioSource {
                    name: "speakers.monitor".into(),
                    description: "Monitor of Speakers".into(),
                    monitor: true,
                },
            ],
            ..AudioSnapshot::default()
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().children[0].text,
        "2:microphone:false:speakers.monitor:true"
    );
    assert!(
        runtime
            .dispatch(runtime.root().children[1].id, Event::Click)
            .is_err()
    );

    runtime.reload(&path).unwrap();
    assert_eq!(
        runtime.root().children[0].text,
        "2:microphone:false:speakers.monitor:true"
    );
    assert!(
        runtime
            .set_all_audio_sources(vec![
                AudioSource {
                    name: "duplicate".into(),
                    description: String::new(),
                    monitor: false,
                },
                AudioSource {
                    name: "duplicate".into(),
                    description: String::new(),
                    monitor: true,
                },
            ])
            .is_err()
    );
    let too_many = (0..513)
        .map(|index| AudioSource {
            name: format!("source-{index}"),
            description: String::new(),
            monitor: false,
        })
        .collect();
    assert!(runtime.set_all_audio_sources(too_many).is_err());
    assert_eq!(
        runtime.root().children[0].text,
        "2:microphone:false:speakers.monitor:true"
    );
}
