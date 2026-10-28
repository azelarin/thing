use std::fs;
use tempfile::TempDir;
use thing_runtime::{Action, AudioDevice, Event, Runtime, Scalar};

fn runtime(source: &str) -> (TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn device(name: &str, description: &str) -> AudioDevice {
    AudioDevice {
        name: name.into(),
        description: description.into(),
    }
}

#[test]
fn reordering_preserves_keys_and_updates_handlers_and_nested_bindings() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = function()
            local children = {}
            for _, device in services.audio.devices:get() do
                table.insert(children, ui.button {
                    key = device.name,
                    on_click = function() services.audio.set_default_sink(device.name) end,
                    children = {
                        ui.text { text = device.description },
                        ui.text { text = function() return services.clock.time:get() end },
                    },
                })
            end
            return children
        end }
    "#,
    );
    runtime
        .set_audio_devices(vec![device("a", "A"), device("b", "B")])
        .unwrap();
    runtime.refresh().unwrap();
    let first = runtime.root().unwrap().children.clone();
    runtime
        .set_audio_devices(vec![device("b", "New B"), device("a", "A")])
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].id, first[1].id);
    assert_eq!(runtime.root().unwrap().children[1].id, first[0].id);
    assert_eq!(
        runtime.root().unwrap().children[0].children[0].id,
        first[1].children[0].id
    );
    assert_eq!(
        runtime.root().unwrap().children[0].children[0].text,
        "New B"
    );
    runtime.dispatch(first[1].id, Event::Click).unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::SetDefaultSink("b".into())]
    );
    runtime
        .set_service("clock.time", Scalar::String("14:20".into()))
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].children[1].text,
        "14:20"
    );
}

#[test]
fn invalid_replacement_preserves_the_old_tree_and_callbacks_then_recovers() {
    let (_directory, mut runtime) = runtime(
        r#"
        local bad = ui.state(false)
        return ui.panel { children = {
            ui.column { children = function()
                if bad:get() then
                    return {
                        ui.button { key = 'row', on_click = function() services.audio.set_muted(true) end },
                        ui.button { key = 'row' },
                    }
                end
                return { ui.button { key = 'row', on_click = function() services.audio.set_muted(false) end } }
            end },
            ui.button { on_click = function() bad:set(true) end },
            ui.button { on_click = function() bad:set(false) end },
        } }
    "#,
    );
    let before = runtime.root().unwrap().clone();
    assert!(
        runtime
            .dispatch(before.children[1].id, Event::Click)
            .is_err()
    );
    assert_eq!(runtime.root().unwrap(), &before);
    runtime
        .dispatch(before.children[0].children[0].id, Event::Click)
        .unwrap();
    assert_eq!(runtime.take_actions(), vec![Action::SetMuted(false)]);
    runtime
        .dispatch(before.children[2].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].children[0].id,
        before.children[0].children[0].id
    );
}

#[test]
fn removed_rows_stop_observing_state_and_cannot_receive_events() {
    let (_directory, mut runtime) = runtime(
        r#"
        local show = ui.state(true)
        return ui.panel { children = {
            ui.column { children = function()
                if not show:get() then return {} end
                return { ui.button {
                    key = 'row', on_click = function() error('removed row was clicked') end,
                    children = { ui.text { text = function()
                        assert(show:get(), 'removed binding was evaluated')
                        return services.clock.time:get()
                    end } },
                } }
            end },
            ui.button { on_click = function() show:set(false) end },
        } }
    "#,
    );
    let removed = runtime.root().unwrap().children[0].children[0].id;
    runtime
        .dispatch(runtime.root().unwrap().children[1].id, Event::Click)
        .unwrap();
    assert!(runtime.root().unwrap().children[0].children.is_empty());
    runtime
        .set_service("clock.time", Scalar::String("14:21".into()))
        .unwrap();
    assert!(!runtime.refresh().unwrap());
    assert!(!runtime.dispatch(removed, Event::Click).unwrap());
}

#[test]
fn changed_widget_kind_does_not_inherit_control_identity() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = function()
            if services.audio.available:get() then return { ui.slider { key = 'control' } } end
            return { ui.button { key = 'control' } }
        end }
    "#,
    );
    let old = runtime.root().unwrap().children[0].id;
    runtime
        .set_service("audio.available", Scalar::Boolean(true))
        .unwrap();
    runtime.refresh().unwrap();
    assert_ne!(runtime.root().unwrap().children[0].id, old);
}

#[test]
fn rejects_cached_widgets_reused_outside_the_replaced_subtree() {
    let (_directory, mut runtime) = runtime(
        r#"
        local existing = ui.button {}
        return ui.panel { children = {
            existing,
            ui.column { children = function()
                if services.audio.available:get() then return { existing } end
                return { ui.text { key = 'placeholder', text = services.clock.time:get() } }
            end },
        } }
    "#,
    );
    runtime
        .set_service("clock.time", Scalar::String("12:00".into()))
        .unwrap();
    runtime.refresh().unwrap();
    let before = runtime.root().unwrap().clone();
    runtime
        .set_service("audio.available", Scalar::Boolean(true))
        .unwrap();
    assert!(runtime.refresh().is_err());
    assert_eq!(runtime.root().unwrap(), &before);
}

#[test]
fn children_functions_must_not_allocate_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        "return ui.panel { children = function() ui.state(0); return {} end }",
    )
    .unwrap();
    assert!(Runtime::load(path).is_err());
}

#[test]
fn nested_lists_refresh_together_and_recover_from_a_partially_parsed_row() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = function()
            local time = services.clock.time:get()
            return { ui.column { key = 'outer', children = function()
                local current = services.clock.time:get()
                return { ui.text {
                    key = 'inner', text = function() return current .. time end,
                }, ui.text { text = if current == 'bad' then 42 else current } }
            end } }
        end }
    "#,
    );
    let original = runtime.root().unwrap().clone();
    for value in ["bad", "12:00", "12:01"] {
        runtime
            .set_service("clock.time", Scalar::String(value.into()))
            .unwrap();
        if value == "bad" {
            assert!(runtime.refresh().is_err());
            assert_eq!(runtime.root().unwrap(), &original);
        } else {
            runtime.refresh().unwrap();
            let child = &runtime.root().unwrap().children[0];
            assert_eq!(child.id, original.children[0].id);
            assert_eq!(child.children[0].id, original.children[0].children[0].id);
            assert_eq!(child.children[0].text, value.repeat(2));
        }
    }
}

#[test]
fn repeated_replacement_does_not_leave_stale_callbacks_or_subscriptions() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = function()
            local generation = services.clock.time:get()
            return { ui.button {
                key = 'row',
                on_click = function() services.audio.set_default_sink(generation) end,
                children = { ui.text { text = function()
                    assert(generation == services.clock.time:get(), 'stale subscription')
                    return generation
                end } },
            } }
        end }
    "#,
    );
    let id = runtime.root().unwrap().children[0].id;
    for generation in 0..500 {
        let value = generation.to_string();
        runtime
            .set_service("clock.time", Scalar::String(value.clone()))
            .unwrap();
        runtime.refresh().unwrap();
        assert_eq!(runtime.root().unwrap().children[0].id, id);
        runtime.dispatch(id, Event::Click).unwrap();
        assert_eq!(runtime.take_actions(), vec![Action::SetDefaultSink(value)]);
    }
}
