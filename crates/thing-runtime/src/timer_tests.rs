use std::{
    fs,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use super::{Event, Runtime};

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

fn activate(runtime: &mut Runtime) -> Instant {
    let now = Instant::now() + Duration::from_secs(3600);
    runtime.activate_timers(now);
    now
}

#[test]
fn autostart_waits_for_acceptance_and_a_one_shot_fires_once() {
    let (_directory, mut runtime) = load(
        r#"
        local count = ui.state(0)
        ui.timer { interval = 20, autostart = true,
            on_timeout = function() count:set(count:get() + 1) end }
        return ui.panel { children = { ui.text { text = function() return tostring(count:get()) end } } }
    "#,
    );
    assert!(runtime.next_timer_deadline().is_none());
    let now = activate(&mut runtime);
    let deadline = now + Duration::from_millis(20);
    assert!(
        runtime
            .due_timers(deadline - Duration::from_nanos(1))
            .is_empty()
    );
    let token = runtime.due_timers(deadline).pop().unwrap();
    assert_eq!(
        runtime
            .dispatch_timer_checked(&token, deadline, |_| Ok(()))
            .unwrap(),
        Some(true)
    );
    assert_eq!(runtime.root().unwrap().children[0].text, "1");
    assert!(runtime.next_timer_deadline().is_none());
    assert_eq!(
        runtime
            .dispatch_timer_checked(&token, deadline, |_| Ok(()))
            .unwrap(),
        None
    );
}

#[test]
fn repetition_skips_missed_ticks_and_can_stop_itself() {
    let (_directory, mut runtime) = load(
        r#"
        local count = ui.state(0)
        ui.timer { interval = 20, autostart = true, repeating = true,
            on_timeout = function(timer)
                count:set(count:get() + 1)
                if count:get() == 2 then timer:stop() end
            end }
        return ui.panel { children = { ui.text { text = function() return tostring(count:get()) end } } }
    "#,
    );
    let now = activate(&mut runtime);
    let late = now + Duration::from_secs(120);
    let token = runtime.due_timers(late).pop().unwrap();
    runtime
        .dispatch_timer_checked(&token, late, |_| Ok(()))
        .unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "1");
    assert!(runtime.due_timers(late).is_empty());
    let next = late + Duration::from_millis(20);
    assert_eq!(runtime.next_timer_deadline(), Some(next));
    let token = runtime.due_timers(next).pop().unwrap();
    runtime
        .dispatch_timer_checked(&token, next, |_| Ok(()))
        .unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "2");
    assert!(runtime.next_timer_deadline().is_none());
}

#[test]
fn a_one_shot_callback_can_start_another_interval() {
    let (_directory, mut runtime) = load(
        r#"
        local count = ui.state(0)
        ui.timer { interval = 5, autostart = true,
            on_timeout = function(timer)
                count:set(count:get() + 1)
                if count:get() < 2 then timer:start() end
            end }
        return ui.panel { children = { ui.text { text = function() return tostring(count:get()) end } } }
    "#,
    );
    activate(&mut runtime);
    for expected in ["1", "2"] {
        let deadline = runtime.next_timer_deadline().unwrap();
        let token = runtime.due_timers(deadline).pop().unwrap();
        runtime
            .dispatch_timer_checked(&token, deadline, |_| Ok(()))
            .unwrap();
        assert_eq!(runtime.root().unwrap().children[0].text, expected);
    }
    assert!(runtime.next_timer_deadline().is_none());
}

#[test]
fn start_preserves_the_deadline_while_restart_and_stop_invalidate_tokens() {
    let (_directory, mut runtime) = load(
        r#"
        local timer = ui.timer { interval = 20, autostart = true, on_timeout = function() error('stale callback') end }
        return ui.panel { children = {
            ui.button { on_click = function() timer:start() end },
            ui.button { on_click = function() timer:restart() end },
            ui.button { on_click = function() timer:stop() end },
        } }
    "#,
    );
    activate(&mut runtime);
    let deadline = runtime.next_timer_deadline().unwrap();
    let old = runtime.due_timers(deadline).pop().unwrap();
    let children = &runtime.root().unwrap().children;
    let ids = [children[0].id, children[1].id, children[2].id];
    runtime.dispatch(ids[0], Event::Click).unwrap();
    assert_eq!(runtime.next_timer_deadline(), Some(deadline));
    runtime.dispatch(ids[1], Event::Click).unwrap();
    assert_eq!(
        runtime
            .dispatch_timer_checked(&old, deadline, |_| Ok(()))
            .unwrap(),
        None
    );
    let deadline = runtime.next_timer_deadline().unwrap();
    let old = runtime.due_timers(deadline).pop().unwrap();
    runtime.dispatch(ids[2], Event::Click).unwrap();
    assert!(runtime.next_timer_deadline().is_none());
    assert_eq!(
        runtime
            .dispatch_timer_checked(&old, deadline, |_| Ok(()))
            .unwrap(),
        None
    );
}

#[test]
fn one_callback_can_control_another_callback_from_the_same_due_snapshot() {
    for control in ["start", "stop", "restart"] {
        let source = r#"
            local count = ui.state(0)
            local second
            ui.timer { interval = 10, autostart = true, on_timeout = function() second:CONTROL() end }
            second = ui.timer { interval = 10, autostart = true, on_timeout = function() count:set(count:get() + 1) end }
            return ui.panel { children = { ui.text { text = function() return tostring(count:get()) end } } }
        "#.replace("CONTROL", control);
        let (_directory, mut runtime) = load(&source);
        activate(&mut runtime);
        let deadline = runtime.next_timer_deadline().unwrap();
        let tokens = runtime.due_timers(deadline);
        assert_eq!(tokens.len(), 2);
        runtime
            .dispatch_timer_checked(&tokens[0], deadline, |_| Ok(()))
            .unwrap();
        let result = runtime
            .dispatch_timer_checked(&tokens[1], deadline, |_| Ok(()))
            .unwrap();
        assert_eq!(result, (control == "start").then_some(true));
        assert_eq!(
            runtime.root().unwrap().children[0].text,
            if control == "start" { "1" } else { "0" }
        );
        assert_eq!(
            runtime.next_timer_deadline(),
            (control == "restart").then_some(deadline + Duration::from_millis(10))
        );
    }
}

#[test]
fn failed_callbacks_and_checked_updates_discard_controls_and_external_actions() {
    for rejection in [false, true] {
        let source = r#"
            local value = ui.state(0)
            local other = ui.timer { interval = 10, on_timeout = function() error('uncommitted timer ran') end }
            ui.timer { interval = 10, autostart = true, repeating = true, on_timeout = function()
                other:start()
                services.launch({ 'must-not-run' })
                value:set(value:get() + 1)
                if CALLBACK_FAILS then error('callback failed') end
            end }
            ui.timer { interval = 100, autostart = true, on_timeout = function() end }
            return ui.panel { children = { ui.text { text = function() return tostring(value:get()) end } } }
        "#.replace("CALLBACK_FAILS", if rejection { "false" } else { "true" });
        let (_directory, mut runtime) = load(&source);
        activate(&mut runtime);
        let deadline = runtime.next_timer_deadline().unwrap();
        let token = runtime.due_timers(deadline).pop().unwrap();
        let error = runtime
            .dispatch_timer_checked(&token, deadline, |_| {
                if rejection {
                    Err("layout rejected".into())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert!(error.to_string().contains(if rejection {
            "layout rejected"
        } else {
            "callback failed"
        }));
        assert_eq!(runtime.root().unwrap().children[0].text, "0");
        assert!(runtime.take_actions().is_empty());
        assert!(runtime.due_timers(deadline).is_empty());
        assert_eq!(
            runtime.next_timer_deadline(),
            Some(deadline + Duration::from_millis(if rejection { 10 } else { 90 }))
        );
    }
}

#[test]
fn rejected_hover_cleanup_does_not_commit_timer_controls() {
    let (_directory, mut runtime) = load(
        r#"
        local present = ui.state(true)
        local invalid = ui.state(false)
        local timer = ui.timer { interval = 10, on_timeout = function() error('uncommitted timer ran') end }
        return ui.panel {
            style = { width = function() return if invalid:get() then -1 else 100 end },
            children = { ui.row { children = function()
                if not present:get() then return {} end
                return { ui.button { on_hover = function(inside)
                    timer:start()
                    if inside then present:set(false) else invalid:set(true) end
                end } }
            end } },
        }
    "#,
    );
    activate(&mut runtime);
    let button = runtime.root().unwrap().children[0].children[0].id;
    assert!(runtime.dispatch(button, Event::Hover(true)).is_err());
    assert!(runtime.root().unwrap().children[0].children.is_empty());
    assert!(runtime.next_timer_deadline().is_none());
}

#[test]
fn an_unrelated_deferred_failure_does_not_stop_a_repeating_timer() {
    let (_directory, mut runtime) = load(
        r#"
        local invalid = ui.state(false)
        local count = ui.state(0)
        ui.timer { interval = 10, repeating = true, autostart = true,
            on_timeout = function() count:set(count:get() + 1) end }
        return ui.panel {
            style = { width = function() return if invalid:get() then -1 else 100 end },
            children = {
                ui.button { on_click = function() invalid:set(true) end },
                ui.button { on_click = function() invalid:set(false) end },
                ui.text { text = function() return tostring(count:get()) end },
            },
        }
    "#,
    );
    activate(&mut runtime);
    let children = &runtime.root().unwrap().children;
    let invalid = children[0].id;
    let repair = children[1].id;
    assert!(runtime.dispatch(invalid, Event::Click).is_err());
    let deadline = runtime.next_timer_deadline().unwrap();
    let token = runtime.due_timers(deadline).pop().unwrap();
    let error = runtime
        .dispatch_timer_checked(&token, deadline, |_| Ok(()))
        .unwrap_err();
    assert!(error.to_string().contains("timer callback at"));
    assert_eq!(runtime.root().unwrap().children[2].text, "0");
    assert_eq!(
        runtime.next_timer_deadline(),
        Some(deadline + Duration::from_millis(10))
    );
    runtime.dispatch(repair, Event::Click).unwrap();
    assert_eq!(runtime.root().unwrap().children[2].text, "1");
    let deadline = runtime.next_timer_deadline().unwrap();
    let token = runtime.due_timers(deadline).pop().unwrap();
    runtime
        .dispatch_timer_checked(&token, deadline, |_| Ok(()))
        .unwrap();
    assert_eq!(runtime.root().unwrap().children[2].text, "2");
}

#[test]
fn failed_reload_preserves_deadlines_and_replacements_reject_old_tokens() {
    let (directory, mut runtime) = load(
        r#"
        ui.timer { interval = 10, autostart = true, on_timeout = function() error('old timer ran') end }
        return ui.panel {}
    "#,
    );
    activate(&mut runtime);
    let deadline = runtime.next_timer_deadline().unwrap();
    let old = runtime.due_timers(deadline).pop().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, "return ui.panel {").unwrap();
    assert!(runtime.reload(&path).is_err());
    assert_eq!(runtime.next_timer_deadline(), Some(deadline));
    fs::write(&path, "ui.timer { interval = 20, autostart = true, on_timeout = function() end }; return ui.panel {}").unwrap();
    let candidate = runtime.load_replacement(&path).unwrap();
    assert!(candidate.next_timer_deadline().is_none());
    assert_eq!(runtime.next_timer_deadline(), Some(deadline));
    runtime.reload(&path).unwrap();
    assert!(runtime.next_timer_deadline().is_some());
    assert_eq!(
        runtime
            .dispatch_timer_checked(&old, deadline, |_| Ok(()))
            .unwrap(),
        None
    );
}

#[test]
fn invalid_declarations_and_late_timer_creation_are_rejected() {
    for declaration in [
        "ui.timer { interval = 0, on_timeout = function() end }",
        "ui.timer { interval = -1, on_timeout = function() end }",
        "ui.timer { interval = 1.5, on_timeout = function() end }",
        "ui.timer { interval = math.huge, on_timeout = function() end }",
        "ui.timer { interval = 0/0, on_timeout = function() end }",
        "ui.timer { interval = 31536000001, on_timeout = function() end }",
        "ui.timer { interval = 1 }",
        "ui.timer { interval = '10', on_timeout = function() end }",
        "ui.timer { interval = 1, on_timeout = function() end, autostart = 1 }",
        "ui.timer { interval = 1, on_timeout = function() end, repeating = 'yes' }",
        "ui.timer { interval = 1, on_timeout = function() end, typo = true }",
        "local timer = ui.timer { interval = 1, on_timeout = function() end }; timer:stop()",
        "for _ = 1, 129 do ui.timer { interval = 1, on_timeout = function() end } end",
    ] {
        let (_directory, path) = fixture(&format!("{declaration}; return ui.panel {{}}"));
        assert!(Runtime::load(path).is_err(), "accepted {declaration}");
    }
    let (_directory, mut runtime) = load(
        r#"
        return ui.panel { children = { ui.button { on_click = function()
            ui.timer { interval = 1, on_timeout = function() end }
        end } } }
    "#,
    );
    activate(&mut runtime);
    let button = runtime.root().unwrap().children[0].id;
    assert!(runtime.dispatch(button, Event::Click).is_err());
    let (_directory, path) = fixture(
        r#"return ui.panel { style = { width = function()
        ui.timer { interval = 1, on_timeout = function() end }; return 100
    end } }"#,
    );
    assert!(Runtime::load(path).is_err());
}

#[test]
fn timer_controls_are_bounded_and_failed_batches_are_discarded() {
    let (_directory, mut runtime) = load(
        r#"
        local timer = ui.timer { interval = 10, on_timeout = function() end }
        return ui.panel { children = { ui.button { on_click = function()
            for _ = 1, 257 do timer:restart() end
        end } } }
    "#,
    );
    activate(&mut runtime);
    let button = runtime.root().unwrap().children[0].id;
    assert!(
        runtime
            .dispatch(button, Event::Click)
            .unwrap_err()
            .to_string()
            .contains("too many timer controls")
    );
    assert!(runtime.next_timer_deadline().is_none());
}

#[test]
fn lock_timers_can_update_presentation_but_cannot_queue_desktop_actions() {
    for action in ["", "services.launch({ 'must-not-run' })", "ui.focus(help)"] {
        let (_directory, path) = fixture(&format!(
            r#"
            local collapsed = ui.state(false)
            local help = ui.button {{}}
            ui.timer {{ interval = 10, autostart = true, repeating = true, on_timeout = function()
                collapsed:set(true)
                {action}
            end }}
            return ui.lock {{ children = {{ ui.secret_input {{ style = {{ scale_x = function()
                return if collapsed:get() then 0 else 1
            end }} }}, help }} }}
        "#
        ));
        let mut runtime = Runtime::load_lock(path).unwrap();
        activate(&mut runtime);
        let deadline = runtime.next_timer_deadline().unwrap();
        let token = runtime.due_timers(deadline).pop().unwrap();
        let result = runtime.dispatch_timer_checked(&token, deadline, |_| Ok(()));
        assert_eq!(result.is_ok(), action.is_empty());
        assert!(runtime.take_actions().is_empty());
        assert_eq!(
            runtime.root().unwrap().children[0].style.scale_x,
            if action.is_empty() { 0.0 } else { 1.0 }
        );
        assert_eq!(runtime.next_timer_deadline().is_some(), action.is_empty());
    }
}

#[test]
fn a_callback_capturing_its_timer_does_not_keep_the_runtime_alive() {
    let (_directory, runtime) = load(
        r#"
        local timer
        timer = ui.timer { interval = 10, on_timeout = function() timer:restart() end }
        return ui.panel {}
    "#,
    );
    let owner = Rc::downgrade(&runtime.timers);
    drop(runtime);
    assert!(owner.upgrade().is_none());
}
