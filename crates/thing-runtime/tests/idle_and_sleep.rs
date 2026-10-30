use std::fs;

use thing_runtime::{
    Action, IdleWatchDefinition, Runtime, ServiceSnapshot, SleepInhibitorAction,
    SleepInhibitorDefinition, SleepInhibitorState, SleepInhibitorStatus,
};

fn load(source: &str) -> (tempfile::TempDir, mlua::Result<Runtime>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(&path);
    (directory, runtime)
}

fn text(runtime: &Runtime) -> String {
    runtime.root().unwrap().children[0].text.clone()
}

const WATCHES: &str = r#"
local log = ui.state('')
local away = services.idle.watch {
    timeout = 300000,
    on_idle = function(watch)
        assert(watch.idle:get())
        log:set(log:get() .. 'idle;')
    end,
    on_resume = function(watch)
        assert(not watch.idle:get())
        log:set(log:get() .. 'resume;')
    end,
}
services.idle.watch { timeout = 1000 }
local broken = services.idle.watch {
    timeout = 2000,
    on_idle = function()
        services.launch({ 'must-not-run' })
        error('broken watch')
    end,
}
return ui.panel { children = {
    ui.text { text = function()
        return log:get() .. tostring(away.idle:get()) .. tostring(broken.idle:get())
    end },
} }
"#;

#[test]
fn idle_watches_dispatch_paired_transitions_through_checked_events() {
    let (_directory, runtime) = load(WATCHES);
    let mut runtime = runtime.unwrap();
    assert_eq!(
        runtime.idle_watches(),
        [300_000, 1_000, 2_000].map(|timeout_ms| IdleWatchDefinition { timeout_ms })
    );
    assert_eq!(text(&runtime), "falsefalse");

    assert_eq!(
        runtime.dispatch_idle_checked(0, true, |_| Ok(())).unwrap(),
        Some(true)
    );
    assert_eq!(text(&runtime), "idle;truefalse");
    // a repeated transition is stale and runs nothing.
    assert_eq!(
        runtime.dispatch_idle_checked(0, true, |_| Ok(())).unwrap(),
        None
    );
    // a resume before any idle is stale as well.
    assert_eq!(
        runtime.dispatch_idle_checked(1, false, |_| Ok(())).unwrap(),
        None
    );
    assert_eq!(
        runtime.dispatch_idle_checked(0, false, |_| Ok(())).unwrap(),
        Some(true)
    );
    assert_eq!(text(&runtime), "idle;resume;falsefalse");
    // a watch without callbacks still publishes its state.
    assert_eq!(
        runtime.dispatch_idle_checked(1, true, |_| Ok(())).unwrap(),
        Some(false)
    );
    assert_eq!(
        runtime.dispatch_idle_checked(9, true, |_| Ok(())).unwrap(),
        None
    );

    // a failing callback discards its queued actions; the fact stays visible.
    let error = runtime
        .dispatch_idle_checked(2, true, |_| Ok(()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("broken watch"), "{error}");
    assert!(runtime.take_actions().is_empty());
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime), "idle;resume;falsetrue");

    // a rejected update also discards the actions its callback queued.
    let (_directory, runtime) = load(
        r#"
        local away = services.idle.watch {
            timeout = 1000,
            on_idle = function() services.launch({ 'must-not-run' }) end,
        }
        return ui.panel { children = { ui.text { text = function()
            return tostring(away.idle:get())
        end } } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    assert!(
        runtime
            .dispatch_idle_checked(0, true, |_| Err("rejected layout".into()))
            .is_err()
    );
    assert!(runtime.take_actions().is_empty());
}

#[test]
fn idle_watch_declarations_are_validated_before_any_is_published() {
    for (source, message) in [
        ("services.idle.watch(1000)", "options table"),
        ("services.idle.watch { timeout = 999 }", "from 1000"),
        (
            "services.idle.watch { timeout = 86400001 }",
            "through 86400000",
        ),
        ("services.idle.watch { timeout = 1500.5 }", "whole number"),
        (
            "services.idle.watch { timeout = '1000' }",
            "must be a number",
        ),
        ("services.idle.watch { timeout = 0/0 }", "whole number"),
        (
            "services.idle.watch { timeout = 1000, on_idle = true }",
            "on_idle must be",
        ),
        (
            "services.idle.watch { timeout = 1000, input_only = true }",
            "accept only",
        ),
        (
            "for _ = 1, 17 do services.idle.watch { timeout = 1000 } end",
            "at most 16",
        ),
    ] {
        let (_directory, runtime) = load(&format!("{source}\nreturn ui.panel {{}}"));
        let error = runtime.err().unwrap().to_string();
        assert!(error.contains(message), "{source}: {error}");
    }

    // declarations close when loading finishes, and state stays read-only.
    let (_directory, runtime) = load(
        r#"
        local away = services.idle.watch { timeout = 1000 }
        return ui.panel { children = {
            ui.button { on_click = function() services.idle.watch { timeout = 1000 } end },
            ui.button { on_click = function() away.idle:set(true) end },
        } }
        "#,
    );
    let mut runtime = runtime.unwrap();
    let root = runtime.root().unwrap().clone();
    for (index, message) in [(0, "while the configuration loads"), (1, "read-only")] {
        let error = runtime
            .dispatch(root.children[index].id, thing_runtime::Event::Click)
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "{error}");
    }
    assert_eq!(runtime.idle_watches().len(), 1);
}

const INHIBITORS: &str = r#"
local download = services.sleep_inhibitor { key = 'download', reason = 'a download is running' }
services.sleep_inhibitor { key = 'download', reason = 'a download is running' }
return ui.panel { children = {
    ui.text { text = function() return download.state:get() .. '|' .. download.error:get() end },
    ui.button { on_click = function() download.start() end },
    ui.button { on_click = function() download.stop() end },
    ui.button { on_click = function() download.state:set('active') end },
} }
"#;

#[test]
fn sleep_inhibitors_are_keyed_declarations_with_event_only_actions() {
    let (_directory, runtime) = load(INHIBITORS);
    let mut runtime = runtime.unwrap();
    assert_eq!(
        runtime.sleep_inhibitor_definitions(),
        [SleepInhibitorDefinition {
            key: "download".into(),
            reason: "a download is running".into(),
        }]
    );
    assert_eq!(text(&runtime), "stopped|");
    let root = runtime.root().unwrap().clone();
    runtime
        .dispatch(root.children[1].id, thing_runtime::Event::Click)
        .unwrap();
    runtime
        .dispatch(root.children[2].id, thing_runtime::Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        [SleepInhibitorAction::Start, SleepInhibitorAction::Stop].map(|action| {
            Action::SleepInhibitor {
                key: "download".into(),
                action,
            }
        })
    );
    assert!(
        runtime
            .dispatch(root.children[3].id, thing_runtime::Event::Click)
            .unwrap_err()
            .to_string()
            .contains("read-only")
    );

    // published states update the handle reactively.
    runtime
        .set_sleep_inhibitors(vec![SleepInhibitorState {
            key: "download".into(),
            status: SleepInhibitorStatus::Unavailable,
            error: "logind is not running".into(),
        }])
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(text(&runtime), "unavailable|logind is not running");

    // a runtime loaded later starts from the shared state.
    let mut services = ServiceSnapshot::default();
    services
        .set_sleep_inhibitors(vec![SleepInhibitorState {
            key: "download".into(),
            status: SleepInhibitorStatus::Active,
            error: String::new(),
        }])
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, INHIBITORS).unwrap();
    let output = thing_runtime::OutputContext {
        name: Some("DP-1".into()),
        width: 1920,
        height: 1080,
        scale: thing_core::Scale::from_120ths(120).unwrap(),
    };
    let later = Runtime::load_for_output(&path, output, &services).unwrap();
    assert_eq!(text(&later), "active|");
}

#[test]
fn sleep_inhibitor_declarations_reject_conflicts_and_unreadable_reasons() {
    let long = "x".repeat(257);
    for (source, message) in [
        (
            "services.sleep_inhibitor { key = 'a', reason = 'one' }\n\
             services.sleep_inhibitor { key = 'a', reason = 'two' }"
                .to_string(),
            "conflicting declarations",
        ),
        ("services.sleep_inhibitor { key = 'a b', reason = 'r' }".into(), "keys must be"),
        ("services.sleep_inhibitor { key = '', reason = 'r' }".into(), "keys must be"),
        ("services.sleep_inhibitor { key = 'a', reason = ' ' }".into(), "readable text"),
        (
            "services.sleep_inhibitor { key = 'a', reason = 'line\\nbreak' }".into(),
            "control characters",
        ),
        (format!("services.sleep_inhibitor {{ key = 'a', reason = '{long}' }}"), "256 bytes"),
        (
            "services.sleep_inhibitor { key = 'a', reason = 'r', mode = 'delay' }".into(),
            "only key and reason",
        ),
        ("services.sleep_inhibitor { key = 'a' }".into(), "reason must be a string"),
        (
            "for index = 1, 17 do services.sleep_inhibitor { key = 'k' .. index, reason = 'r' } end"
                .into(),
            "at most 16",
        ),
        (
            "services.sleep_inhibitor { key = 'a', reason = 'r' }.start()".into(),
            "only in event handlers",
        ),
    ] {
        let (_directory, runtime) = load(&format!("{source}\nreturn ui.panel {{}}"));
        let error = runtime.err().unwrap().to_string();
        assert!(error.contains(message), "{source}: {error}");
    }
}

#[test]
fn lock_configurations_reject_idle_watches_and_sleep_inhibitors() {
    for (declaration, message) in [
        (
            "services.idle.watch { timeout = 1000 }",
            "idle watches are unavailable",
        ),
        (
            "services.sleep_inhibitor { key = 'a', reason = 'r' }",
            "sleep inhibitors are unavailable",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock.luau");
        fs::write(
            &path,
            format!("{declaration}\nreturn ui.lock {{ children = {{ ui.secret_input {{}} }} }}"),
        )
        .unwrap();
        let error = Runtime::load_lock(&path).err().unwrap().to_string();
        assert!(error.contains(message), "{error}");
    }
}
