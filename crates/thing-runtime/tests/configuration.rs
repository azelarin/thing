use std::{fs, path::PathBuf, time::Duration};
use tempfile::TempDir;
use thing_core::Kind;
use thing_runtime::{Action, AudioDevice, Event, Limits, Runtime, Scalar};

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
