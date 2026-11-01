use std::{collections::BTreeMap, fs, path::PathBuf, time::Instant};

use tempfile::TempDir;
use thing_runtime::{CallValue, MAX_CALL_DEPTH, MAX_COMMANDS, Runtime, SurfaceTree};

fn fixture(source: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

fn load(source: &str) -> (TempDir, Runtime) {
    let (directory, path) = fixture(source);
    (directory, Runtime::load(path).unwrap())
}

fn accept(_: &[SurfaceTree]) -> Result<(), String> {
    Ok(())
}

fn text(runtime: &Runtime) -> String {
    runtime.root().unwrap().children[0].text.clone()
}

fn map(entries: &[(&str, CallValue)]) -> CallValue {
    CallValue::Map(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    )
}

const LAUNCHER: &str = r#"
    local open = ui.state(false)
    local tab = ui.state('apps')
    local later = ui.timer { interval = 1000, on_timeout = function() end }
    ui.command('toggle-launcher', function(arguments)
        if arguments and arguments.tab then tab:set(arguments.tab) end
        open:set(not open:get())
        return { open = open:get(), tab = tab:get(), seen = arguments }
    end)
    ui.command('fail', function()
        services.media.next()
        later:start()
        open:set(true)
        error('nothing to toggle')
    end)
    ui.command('reject', function()
        tab:set('')
        return true
    end)
    ui.command('cycle', function() local t = {} t.self = t return t end)
    ui.command('function', function() return { run = print } end)
    ui.command('mixed', function() return { 1, 2, name = 'x' } end)
    ui.command('gap', function() return { [1] = 1, [3] = 3 } end)
    ui.command('infinite', function() return math.huge end)
    ui.command('deep', function()
        local value = 'leaf'
        for _ = 1, 20 do value = { value } end
        return value
    end)
    ui.command('list', function() return { 1, 2.5, 'three', false } end)
    ui.command('empty', function() return {} end)
    ui.command('nothing', function() end)
    return ui.panel { children = {
        ui.text { text = function() return `{open:get()} {tab:get()}` end },
    } }
"#;

#[test]
fn commands_preserve_nulls_inside_arguments() {
    let (_directory, mut runtime) =
        load("ui.command('echo', function(value) return value end)\nreturn ui.panel {}\n");
    for arguments in [
        map(&[
            ("missing", CallValue::Nil),
            ("present", CallValue::Boolean(true)),
        ]),
        CallValue::List(vec![CallValue::Nil, CallValue::Boolean(true)]),
        CallValue::Nil,
    ] {
        let result = runtime
            .call_command_checked("echo", &arguments, accept)
            .unwrap()
            .unwrap();
        assert_eq!(result.value, arguments);
    }
}

#[test]
fn commands_can_detect_and_return_null_table_entries() {
    let (_directory, mut runtime) = load(
        "ui.command('select', function(value)\n\
            assert(value.selection == ui.null)\n\
            return { ui.null, { selected = ui.null } }\n\
        end)\nreturn ui.panel {}\n",
    );
    let result = runtime
        .call_command_checked("select", &map(&[("selection", CallValue::Nil)]), accept)
        .unwrap()
        .unwrap();
    assert_eq!(
        result.value,
        CallValue::List(vec![CallValue::Nil, map(&[("selected", CallValue::Nil)])])
    );
}

#[test]
fn commands_receive_arguments_update_state_and_return_data() {
    let (_directory, mut runtime) = load(LAUNCHER);
    assert_eq!(
        runtime.command_names(),
        [
            "cycle",
            "deep",
            "empty",
            "fail",
            "function",
            "gap",
            "infinite",
            "list",
            "mixed",
            "nothing",
            "reject",
            "toggle-launcher"
        ]
    );
    let arguments = map(&[("tab", CallValue::String("files".into()))]);
    let call = runtime
        .call_command_checked("toggle-launcher", &arguments, accept)
        .unwrap()
        .unwrap();
    assert!(call.changed);
    assert_eq!(
        call.value,
        map(&[
            ("open", CallValue::Boolean(true)),
            ("tab", CallValue::String("files".into())),
            ("seen", arguments.clone()),
        ])
    );
    assert_eq!(text(&runtime), "true files");

    let call = runtime
        .call_command_checked("list", &CallValue::Nil, accept)
        .unwrap()
        .unwrap();
    assert!(!call.changed);
    assert_eq!(
        call.value,
        CallValue::List(vec![
            CallValue::Number(1.0),
            CallValue::Number(2.5),
            CallValue::String("three".into()),
            CallValue::Boolean(false),
        ])
    );
    let empty = runtime.call_command_checked("empty", &CallValue::Nil, accept);
    assert_eq!(
        empty.unwrap().unwrap().value,
        CallValue::Map(BTreeMap::new())
    );
    let nothing = runtime.call_command_checked("nothing", &CallValue::Nil, accept);
    assert_eq!(nothing.unwrap().unwrap().value, CallValue::Nil);
    assert!(
        runtime
            .call_command_checked("missing", &CallValue::Nil, accept)
            .unwrap()
            .is_none()
    );
}

#[test]
fn failed_handlers_discard_actions_and_controls_but_keep_state_writes() {
    let (_directory, mut runtime) = load(LAUNCHER);
    runtime.activate_timers(Instant::now());
    let error = runtime
        .call_command_checked("fail", &CallValue::Nil, accept)
        .unwrap_err()
        .to_string();
    assert!(error.contains("nothing to toggle"), "{error}");
    assert!(error.contains("command \"fail\""), "{error}");
    assert!(runtime.take_actions().is_empty());
    assert!(runtime.next_timer_deadline().is_none());
    // the state write already happened; it is not rolled back. the next
    // successful refresh publishes it.
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime), "true apps");
}

#[test]
fn rejected_candidates_fail_the_call_and_keep_the_published_tree() {
    let (_directory, mut runtime) = load(LAUNCHER);
    let reject_empty_tab = |surfaces: &[SurfaceTree]| {
        if surfaces[0].root.children[0].text.ends_with(' ') {
            Err("the tab label cannot be empty".to_owned())
        } else {
            Ok(())
        }
    };
    let error = runtime
        .call_command_checked("reject", &CallValue::Nil, reject_empty_tab)
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot be empty"), "{error}");
    assert_eq!(text(&runtime), "false apps");
    assert!(runtime.take_actions().is_empty());
}

#[test]
fn results_must_be_bounded_plain_data() {
    let (_directory, mut runtime) = load(LAUNCHER);
    for (command, message) in [
        ("cycle", "cycles"),
        ("function", "function cannot be passed"),
        ("mixed", "keys 1..n or string keys"),
        ("gap", "keys 1..n or string keys"),
        ("infinite", "finite"),
        ("deep", "nest at most"),
    ] {
        let error = runtime
            .call_command_checked(command, &CallValue::Nil, accept)
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "{command}: {error}");
    }
    let mut deep = CallValue::Nil;
    for _ in 0..=MAX_CALL_DEPTH {
        deep = CallValue::List(vec![deep]);
    }
    assert!(
        runtime
            .call_command_checked("list", &deep, accept)
            .unwrap_err()
            .to_string()
            .contains("nest at most")
    );
    let wide = CallValue::List(vec![CallValue::Nil; 2000]);
    assert!(
        runtime
            .call_command_checked("list", &wide, accept)
            .unwrap_err()
            .to_string()
            .contains("at most 1024 items")
    );
    let long = CallValue::String("x".repeat(20_000));
    assert!(
        runtime
            .call_command_checked("list", &long, accept)
            .unwrap_err()
            .to_string()
            .contains("bytes")
    );
}

#[test]
fn commands_are_declared_once_while_loading() {
    let many = (0..=MAX_COMMANDS)
        .map(|index| format!("ui.command('c{index}', function() end)"))
        .collect::<Vec<_>>()
        .join("\n");
    for (source, message) in [
        (
            "ui.command('twice', function() end) ui.command('twice', function() end) return ui.panel {}"
                .to_owned(),
            "already declared",
        ),
        (
            "ui.command('has space', function() end) return ui.panel {}".to_owned(),
            "must be 1 to 64",
        ),
        (
            "ui.command('', function() end) return ui.panel {}".to_owned(),
            "must be 1 to 64",
        ),
        (
            "ui.command('name', 'not a function') return ui.panel {}".to_owned(),
            "must be a function",
        ),
        (format!("{many} return ui.panel {{}}"), "at most 64 commands"),
    ] {
        let (_directory, path) = fixture(&source);
        let error = Runtime::load(path).err().unwrap().to_string();
        assert!(error.contains(message), "{source}: {error}");
    }
    // a handler cannot add commands after the configuration loaded.
    let (_directory, mut runtime) = load(
        "ui.command('late', function() ui.command('later', function() end) end) return ui.panel {}",
    );
    let error = runtime
        .call_command_checked("late", &CallValue::Nil, accept)
        .unwrap_err()
        .to_string();
    assert!(error.contains("while loading"), "{error}");
    assert_eq!(runtime.command_names(), ["late"]);
}

#[test]
fn lock_configurations_reject_commands() {
    let (_directory, path) =
        fixture("ui.command('unlock', function() end) return ui.lock { children = {} }");
    let error = Runtime::load_lock(path).err().unwrap().to_string();
    assert!(
        error.contains("commands are unavailable in lock configurations"),
        "{error}"
    );
    let (_directory, path) = fixture("return ui.lock { children = {} }");
    assert!(Runtime::load_lock(path).unwrap().command_names().is_empty());
}

#[test]
fn reloads_replace_commands_and_never_run_old_closures() {
    let (directory, path) = fixture(
        "local count = ui.state(0) ui.command('old', function() count:set(count:get() + 1) return count:get() end) return ui.panel {}",
    );
    let mut runtime = Runtime::load(&path).unwrap();
    assert_eq!(
        runtime
            .call_command_checked("old", &CallValue::Nil, accept)
            .unwrap()
            .unwrap()
            .value,
        CallValue::Number(1.0)
    );
    // a rejected reload keeps the running commands.
    fs::write(&path, "ui.command('new', function() end) return ui.panel {").unwrap();
    assert!(runtime.reload(&path).is_err());
    assert_eq!(runtime.command_names(), ["old"]);
    fs::write(
        &path,
        "ui.command('new', function() return 'new' end) return ui.panel {}",
    )
    .unwrap();
    runtime.reload(&path).unwrap();
    assert_eq!(runtime.command_names(), ["new"]);
    assert!(
        runtime
            .call_command_checked("old", &CallValue::Nil, accept)
            .unwrap()
            .is_none()
    );
    drop(directory);
}
