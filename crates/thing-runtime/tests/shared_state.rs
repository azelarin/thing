use std::{collections::BTreeMap, fs, sync::Arc};

use thing_runtime::{
    Action, CallValue, MAX_SHARED_STATE_BYTES, OutputContext, Runtime, ServiceSnapshot,
    SharedStateDefinition, SharedStates, shared_state_bytes,
};

fn write(source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

fn load(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let (directory, path) = write(source);
    let runtime = Runtime::load(&path);
    (directory, runtime)
}

fn text(runtime: &Runtime) -> String {
    runtime.root().unwrap().children[0].text.clone()
}

fn call(runtime: &mut Runtime, name: &str, arguments: CallValue) -> mlua::Result<CallValue> {
    runtime
        .call_command_checked(name, &arguments, |_| Ok(()))
        .map(|call| call.unwrap().value)
}

fn map(entries: &[(&str, CallValue)]) -> CallValue {
    CallValue::Map(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    )
}

fn shared_actions(runtime: &mut Runtime) -> Vec<(String, CallValue)> {
    runtime
        .take_actions()
        .into_iter()
        .map(|action| match action {
            Action::SharedState { key, value } => (key, (*value).clone()),
            other => panic!("unexpected action {other:?}"),
        })
        .collect()
}

const WEATHER: &str = r#"
local weather = services.shared_state {
    key = 'weather',
    initial = { temperature = 18, conditions = { 'clear' } },
}
-- a repeated equal declaration returns a handle to the same value.
local again = services.shared_state {
    key = 'weather',
    initial = { temperature = 18, conditions = { 'clear' } },
}
ui.command('set', function(value)
    weather:set(value)
    -- the queued value is not visible until the host publishes it.
    return again:get().temperature
end)
ui.command('fail', function(value)
    weather:set(value)
    error('broken handler')
end)
ui.command('mutate', function()
    local current = weather:get()
    local ok = pcall(function() current.temperature = 99 end)
    local nested = pcall(function() current.conditions[1] = 'storm' end)
    local appended = pcall(function() table.insert(current.conditions, 'storm') end)
    return { ok, nested, appended, weather:get().conditions[1] }
end)
return ui.panel { children = { ui.text { text = function()
    local current = weather:get()
    return `{current.temperature} {table.concat(current.conditions, ',')}`
end } } }
"#;

#[test]
fn reads_are_frozen_copies_and_sets_are_queued_actions() {
    let (_directory, runtime) = load(WEATHER);
    let mut runtime = runtime.unwrap();
    let initial = map(&[
        ("temperature", CallValue::Number(18.0)),
        (
            "conditions",
            CallValue::List(vec![CallValue::String("clear".into())]),
        ),
    ]);
    assert_eq!(
        runtime.shared_state_definitions(),
        [SharedStateDefinition {
            key: "weather".into(),
            initial: Arc::new(initial.clone()),
        }]
    );
    assert_eq!(text(&runtime), "18 clear");

    // nested tables are read-only, and a failed write leaves the value alone.
    assert_eq!(
        call(&mut runtime, "mutate", CallValue::Nil).unwrap(),
        CallValue::List(vec![
            CallValue::Boolean(false),
            CallValue::Boolean(false),
            CallValue::Boolean(false),
            CallValue::String("clear".into()),
        ])
    );
    assert!(runtime.take_actions().is_empty());

    let next = map(&[
        ("temperature", CallValue::Number(21.0)),
        ("conditions", CallValue::Map(BTreeMap::new())),
    ]);
    assert_eq!(
        call(&mut runtime, "set", next.clone()).unwrap(),
        CallValue::Number(18.0)
    );
    assert_eq!(shared_actions(&mut runtime), [("weather".into(), next)]);
    assert_eq!(text(&runtime), "18 clear");

    // a failing handler discards the value it queued.
    let error = call(&mut runtime, "fail", CallValue::Number(1.0))
        .unwrap_err()
        .to_string();
    assert!(error.contains("broken handler"), "{error}");
    assert!(runtime.take_actions().is_empty());
}

#[test]
fn accepted_values_refresh_subscribers_once_and_equal_values_do_nothing() {
    let (_directory, runtime) = load(WEATHER);
    let mut runtime = runtime.unwrap();
    let definitions = runtime.shared_state_definitions();
    let accepted = SharedStates::default().project(&definitions).unwrap();
    // the same accepted values change nothing.
    runtime.set_shared_states(&accepted).unwrap();
    assert!(!runtime.refresh().unwrap());

    let next = map(&[
        ("temperature", CallValue::Number(5.0)),
        (
            "conditions",
            CallValue::List(vec![
                CallValue::String("snow".into()),
                CallValue::String("wind".into()),
            ]),
        ),
    ]);
    let published = accepted
        .with_values(&[("weather".into(), Arc::new(next.clone()))])
        .unwrap()
        .unwrap();
    runtime.set_shared_states(&published).unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(text(&runtime), "5 snow,wind");
    // an equal publication is not a change.
    assert_eq!(
        published
            .with_values(&[("weather".into(), Arc::new(next))])
            .unwrap(),
        None
    );
    runtime.set_shared_states(&published.clone()).unwrap();
    assert!(!runtime.refresh().unwrap());
}

#[test]
fn runtimes_load_the_accepted_value_of_an_unchanged_declaration() {
    let (_directory, path) = write(WEATHER);
    let first = Runtime::load(&path).unwrap();
    let accepted = SharedStates::default()
        .project(&first.shared_state_definitions())
        .unwrap()
        .with_values(&[(
            "weather".into(),
            Arc::new(map(&[
                ("temperature", CallValue::Number(30.0)),
                ("conditions", CallValue::List(Vec::new())),
            ])),
        )])
        .unwrap()
        .unwrap();
    let mut services = ServiceSnapshot::default();
    services.set_shared_states(accepted.clone());
    let output = OutputContext {
        name: Some("DP-2".into()),
        width: 1920,
        height: 1080,
        scale: Default::default(),
    };
    let second = Runtime::load_for_output(&path, output.clone(), &services).unwrap();
    assert_eq!(text(&second), "30 ");

    // a changed initial value starts from the new declaration.
    fs::write(
        &path,
        WEATHER.replace("temperature = 18", "temperature = 9"),
    )
    .unwrap();
    let changed = Runtime::load_for_output(&path, output, &services).unwrap();
    assert_eq!(text(&changed), "9 clear");
    // a projection for the changed declaration resets the value as well.
    let projected = accepted
        .project(&changed.shared_state_definitions())
        .unwrap();
    assert_eq!(
        projected.get("weather").unwrap().value,
        projected.get("weather").unwrap().initial
    );
}

#[test]
fn set_is_only_an_event_action() {
    let (_directory, runtime) = load(
        r#"
        local count = services.shared_state { key = 'count', initial = 0 }
        count:set(1)
        return ui.panel {}
        "#,
    );
    let error = runtime.err().unwrap().to_string();
    assert!(error.contains("only in event handlers"), "{error}");

    let (_directory, runtime) = load(
        r#"
        local count = services.shared_state { key = 'count', initial = 0 }
        return ui.panel { children = { ui.text { text = function()
            count:set(count:get() + 1)
            return 'never'
        end } } }
        "#,
    );
    let error = runtime.err().unwrap().to_string();
    assert!(error.contains("only in event handlers"), "{error}");

    let (_directory, runtime) = load(
        r#"
        ui.command('late', function()
            services.shared_state { key = 'late', initial = 0 }
        end)
        return ui.panel {}
        "#,
    );
    let mut runtime = runtime.unwrap();
    let error = call(&mut runtime, "late", CallValue::Nil)
        .unwrap_err()
        .to_string();
    assert!(error.contains("while the configuration loads"), "{error}");
}

#[test]
fn declarations_and_values_are_validated_before_retention() {
    for (source, expected) in [
        (
            "services.shared_state { key = 'a', initial = 1 }\n\
             services.shared_state { key = 'a', initial = 2 }",
            "conflicting declarations",
        ),
        (
            "services.shared_state { key = 'bad key', initial = 1 }",
            "ascii letters",
        ),
        (
            "services.shared_state { key = 'a', initial = 1, reset = true }",
            "only key and initial",
        ),
        (
            "local value = {}\nvalue.self = value\n\
             services.shared_state { key = 'a', initial = value }",
            "cycles",
        ),
        (
            "services.shared_state { key = 'a', initial = 0 / 0 }",
            "finite",
        ),
        (
            "services.shared_state { key = 'a', initial = '\\xff' }",
            "utf-8",
        ),
        (
            "services.shared_state { key = 'a', initial = { print } }",
            "function",
        ),
        (
            "local value = {}\nfor _ = 1, 9 do value = { value } end\n\
             for _ = 1, 9 do value = { value } end\n\
             services.shared_state { key = 'a', initial = value }",
            "nest",
        ),
        (
            "local parts = {}\nfor index = 1, 9 do parts[index] = string.rep('x', 16000) end\n\
             services.shared_state { key = 'a', initial = parts }",
            "128 KiB",
        ),
        (
            "for index = 1, 65 do services.shared_state { key = `k{index}`, initial = index } end",
            "64 shared states",
        ),
        (
            "local parts = {}\nfor index = 1, 7 do parts[index] = string.rep('x', 16000) end\n\
             for index = 1, 10 do services.shared_state { key = `k{index}`, initial = parts } end",
            "together",
        ),
    ] {
        let (_directory, runtime) = load(&format!("{source}\nreturn ui.panel {{}}"));
        let error = runtime.err().expect(source).to_string();
        assert!(error.contains(expected), "{source}: {error}");
    }
}

#[test]
fn queued_values_are_limited_per_event() {
    let (_directory, runtime) = load(
        r#"
        local parts = {}
        for index = 1, 7 do parts[index] = string.rep('x', 16000) end
        local big = services.shared_state { key = 'big', initial = {} }
        ui.command('flood', function(count)
            for _ = 1, count do big:set(parts) end
            return nil
        end)
        ui.command('large', function()
            big:set({ parts, parts })
        end)
        ui.command('empty', function()
            big:set({})
            big:set(nil)
        end)
        return ui.panel {}
        "#,
    );
    let mut runtime = runtime.unwrap();
    let error = call(&mut runtime, "large", CallValue::Nil)
        .unwrap_err()
        .to_string();
    assert!(error.contains("128 KiB"), "{error}");
    assert!(runtime.take_actions().is_empty());

    call(&mut runtime, "flood", CallValue::Number(8.0)).unwrap();
    assert_eq!(shared_actions(&mut runtime).len(), 8);
    let error = call(&mut runtime, "flood", CallValue::Number(10.0))
        .unwrap_err()
        .to_string();
    assert!(error.contains("per-event limit"), "{error}");
    assert!(runtime.take_actions().is_empty());

    // empty tables and nil are ordinary values.
    call(&mut runtime, "empty", CallValue::Nil).unwrap();
    assert_eq!(
        shared_actions(&mut runtime),
        [
            ("big".into(), CallValue::Map(BTreeMap::new())),
            ("big".into(), CallValue::Nil),
        ]
    );
}

#[test]
fn byte_accounting_is_deterministic() {
    let value = map(&[
        ("a", CallValue::String("xyz".into())),
        (
            "b",
            CallValue::List(vec![CallValue::Nil, CallValue::Number(1.0)]),
        ),
    ]);
    // the map, each key and item, the list items, and the string bytes.
    assert_eq!(
        shared_state_bytes(&value),
        16 + (16 + 1 + 16 + 3) + (16 + 1 + 16 + 32)
    );
    assert!(MAX_SHARED_STATE_BYTES >= shared_state_bytes(&value));
}

#[test]
fn lock_configurations_cannot_declare_shared_state() {
    let (_directory, path) =
        write("services.shared_state { key = 'a', initial = 1 }\nreturn ui.lock {}");
    let error = Runtime::load_lock(&path).err().unwrap().to_string();
    assert!(error.contains("unavailable in lock"), "{error}");
}

#[test]
fn retained_defaults_count_toward_the_total_after_replacement() {
    let payload = |byte| {
        Arc::new(CallValue::List(vec![
            CallValue::String(
                std::iter::repeat_n(byte, 15 * 1024).collect()
            );
            4
        ]))
    };
    let initial = payload('a');
    let definitions: Vec<_> = (0..10)
        .map(|index| SharedStateDefinition {
            key: format!("key-{index}"),
            initial: initial.clone(),
        })
        .collect();
    let states = SharedStates::default().project(&definitions).unwrap();
    let updates: Vec<_> = definitions
        .iter()
        .map(|definition| (definition.key.clone(), payload('b')))
        .collect();
    assert!(
        states
            .with_values(&updates)
            .unwrap_err()
            .contains("together")
    );
}

#[test]
fn dropping_a_prepared_update_preserves_handlers_and_subscriptions() {
    let (_directory, runtime) = load(
        r#"
        local count = services.shared_state { key = 'count', initial = 0 }
        local hits = ui.state(0)
        local old = ui.button { key = 'old', on_click = function() hits:set(hits:get() + 1) end }
        return ui.panel { children = {
            ui.text { text = function() return tostring(hits:get()) end },
            ui.column { children = function()
                if count:get() == 0 then return { old } end
                return { ui.button { key = 'new' } }
            end },
        } }
    "#,
    );
    let mut runtime = runtime.unwrap();
    let current = SharedStates::default()
        .project(&runtime.shared_state_definitions())
        .unwrap();
    runtime.set_shared_states(&current).unwrap();
    runtime.refresh().unwrap();
    let before = runtime.surfaces().to_vec();
    let old = runtime.root().unwrap().children[1].children[0].id;
    let next = current
        .with_values(&[("count".into(), Arc::new(CallValue::Number(1.0)))])
        .unwrap()
        .unwrap();
    let prepared = runtime
        .prepare_shared_states(&next, |surfaces| {
            assert_eq!(
                surfaces[0].root.children[1].children[0].key.as_deref(),
                Some("new")
            );
            Ok(())
        })
        .unwrap();
    drop(prepared);
    assert_eq!(runtime.surfaces(), before);
    runtime.dispatch(old, thing_runtime::Event::Click).unwrap();
    assert_eq!(text(&runtime), "1");
    assert!(
        runtime
            .prepare_shared_states(&next, |_| Ok(()))
            .unwrap()
            .commit()
    );
    assert_eq!(
        runtime.root().unwrap().children[1].children[0]
            .key
            .as_deref(),
        Some("new")
    );
    assert!(runtime.ui_target(old).is_none());
}

#[test]
fn equivalent_reload_reuses_defaults_and_net_zero_updates_change_nothing() {
    let definition = SharedStateDefinition {
        key: "name".into(),
        initial: Arc::new(CallValue::String("before".into())),
    };
    let current = SharedStates::default()
        .project(std::slice::from_ref(&definition))
        .unwrap();
    let retained = current.total_bytes();
    let reloaded = current
        .project(&[SharedStateDefinition {
            key: "name".into(),
            initial: Arc::new(CallValue::String("before".into())),
        }])
        .unwrap();
    assert_eq!(reloaded.total_bytes(), retained);
    let entry = reloaded.get("name").unwrap();
    assert!(Arc::ptr_eq(&entry.initial, &entry.value));
    let next = reloaded
        .with_values(&[
            ("name".into(), Arc::new(CallValue::String("after".into()))),
            ("name".into(), definition.initial),
        ])
        .unwrap();
    assert!(next.is_none());
}

#[test]
fn removing_a_key_then_redeclaring_it_starts_from_its_default() {
    const SOURCE: &str = "local count = services.shared_state { key = 'count', initial = 0 }\nreturn ui.panel { children = { ui.text { text = function() return tostring(count:get()) end } } }";
    let (_directory, path) = write(SOURCE);
    let mut runtime = Runtime::load(&path).unwrap();
    let current = SharedStates::default()
        .project(&runtime.shared_state_definitions())
        .unwrap();
    let changed = current
        .with_values(&[("count".into(), Arc::new(CallValue::Number(4.0)))])
        .unwrap()
        .unwrap();
    runtime.set_shared_states(&changed).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime), "4");
    fs::write(&path, "return ui.panel {}").unwrap();
    runtime.reload(&path).unwrap();
    fs::write(&path, SOURCE).unwrap();
    runtime.reload(&path).unwrap();
    assert_eq!(text(&runtime), "0");
}
