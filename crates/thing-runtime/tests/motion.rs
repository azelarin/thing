use std::{
    fs,
    time::{Duration, Instant},
};

use tempfile::TempDir;
use thing_core::{Node, VectorSegment};
use thing_runtime::{Event, Runtime};

fn load(source: &str) -> (TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn node<'a>(runtime: &'a Runtime, key: &str) -> &'a Node {
    fn find<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
        if node.key.as_deref() == Some(key) {
            return Some(node);
        }
        node.children.iter().find_map(|child| find(child, key))
    }
    runtime
        .surfaces()
        .iter()
        .find_map(|surface| find(&surface.root, key))
        .unwrap()
}

fn start(runtime: &mut Runtime) -> Instant {
    let now = Instant::now() + Duration::from_secs(3600);
    runtime.activate_timers(now);
    let now = now + Duration::from_millis(1);
    fire(runtime, now);
    now
}

fn fire(runtime: &mut Runtime, now: Instant) {
    for token in runtime.due_timers(now) {
        runtime
            .dispatch_timer_checked(&token, now, |_| Ok(()))
            .unwrap();
    }
}

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 0.002, "{actual} != {expected}");
}

#[test]
fn one_sample_drives_a_path_and_content_across_surfaces() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        return {
            ui.panel { key = 'first', children = {
                ui.path { key = 'shape', data = function()
                    return string.format('M0 0 L%f 0 L0 10 Z', x:get())
                end, view_box = { x = 0, y = 0, width = 100, height = 10 } },
                ui.text { key = 'content', text = 'content',
                    style = { translate_x = function() return x:get() end } },
                ui.text { key = 'velocity', text = function() return tostring(x:velocity()) end },
                ui.text { key = 'unrelated', text = 'still' },
            } },
            ui.panel { key = 'second', children = {
                ui.row { key = 'follower', style = { width = function() return x:get() end } },
            } },
        }
    "#,
    );
    assert!(!runtime.is_animating(node(&runtime, "shape").id));
    let start = start(&mut runtime);
    assert!(runtime.is_animating(node(&runtime, "shape").id));
    assert!(runtime.is_animating(node(&runtime, "follower").id));
    assert!(runtime.is_any_animating([runtime.root().unwrap().id]));
    assert!(!runtime.is_animating(node(&runtime, "unrelated").id));
    runtime.advance(start + Duration::from_millis(250)).unwrap();
    assert_eq!(
        node(&runtime, "shape").path.segments[1],
        VectorSegment::LineTo { x: 25.0, y: 0.0 }
    );
    close(node(&runtime, "content").style.translate_x.into(), 25.0);
    close(node(&runtime, "follower").style.width.unwrap().into(), 25.0);
    close(node(&runtime, "velocity").text.parse().unwrap(), 100.0);
    runtime.advance(start + Duration::from_secs(1)).unwrap();
    close(
        node(&runtime, "follower").style.width.unwrap().into(),
        100.0,
    );
    close(node(&runtime, "velocity").text.parse().unwrap(), 0.0);
    assert!(!runtime.is_animating(node(&runtime, "shape").id));
    assert!(!runtime.advance(start + Duration::from_secs(2)).unwrap());
}

#[test]
fn a_retargeted_spring_preserves_its_sample_and_velocity() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'spring', stiffness = 100, damping = 1, epsilon = 0.001 })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        ui.timer { interval = 101, autostart = true, on_timeout = function() x:to(0) end }
        return ui.panel { children = {
            ui.text { key = 'position', text = function() return tostring(x:get()) end },
            ui.text { key = 'velocity', text = function() return tostring(x:velocity()) end },
        } }
    "#,
    );
    let start = start(&mut runtime);
    let reverse = start + Duration::from_millis(100);
    runtime.advance(reverse).unwrap();
    let position: f64 = node(&runtime, "position").text.parse().unwrap();
    let velocity: f64 = node(&runtime, "velocity").text.parse().unwrap();
    close(position, 100.0 * (1.0 - 2.0 * (-1.0_f64).exp()));
    close(velocity, 1000.0 * (-1.0_f64).exp());
    fire(&mut runtime, reverse);
    runtime.advance(reverse).unwrap();
    close(node(&runtime, "position").text.parse().unwrap(), position);
    close(node(&runtime, "velocity").text.parse().unwrap(), velocity);
    runtime.advance(reverse + Duration::from_millis(5)).unwrap();
    assert!(node(&runtime, "position").text.parse::<f64>().unwrap() > position);
    runtime.advance(reverse + Duration::from_secs(10)).unwrap();
    close(node(&runtime, "position").text.parse().unwrap(), 0.0);
    assert!(!runtime.is_animating(node(&runtime, "position").id));
}

#[test]
fn callback_errors_and_rejected_updates_discard_targets() {
    for handler in ["x:to(100); error('failed')", "x:to(100); bad:set(true)"] {
        let source = format!(
            r#"
            local x = ui.motion(0, {{ kind = 'transition', duration = 100 }})
            local bad = ui.state(false)
            return ui.panel {{ children = {{
                ui.button {{ key = 'start', on_click = function() {handler} end }},
                ui.row {{ key = 'value', style = {{
                    width = function() return x:get() end,
                    opacity = function() return bad:get() and 2 or 1 end,
                }} }},
            }} }}
        "#
        );
        let (_directory, mut runtime) = load(&source);
        let before = runtime.surfaces().to_vec();
        assert!(
            runtime
                .dispatch(node(&runtime, "start").id, Event::Click)
                .is_err()
        );
        assert_eq!(runtime.surfaces(), before);
        assert!(!runtime.is_animating(node(&runtime, "value").id));
        runtime
            .advance(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(runtime.surfaces(), before);
    }
}

#[test]
fn rejected_frames_restore_all_readers_and_cancel_the_motion() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        return ui.panel { children = {
            ui.row { key = 'value', style = { width = function() return x:get() end } },
            ui.text { key = 'sample', text = function() return tostring(x:get()) end },
            ui.button { key = 'retry', on_click = function() x:to(20) end },
        } }
    "#,
    );
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(100)).unwrap();
    let before = runtime.surfaces().to_vec();
    let error = runtime
        .advance_checked(start + Duration::from_millis(200), |_| {
            Err("too costly".into())
        })
        .unwrap_err();
    assert!(error.to_string().contains("too costly"));
    assert_eq!(runtime.surfaces(), before);
    assert!(!runtime.is_animating(node(&runtime, "value").id));
    runtime.advance(start + Duration::from_millis(300)).unwrap();
    assert_eq!(runtime.surfaces(), before);
    runtime
        .dispatch(node(&runtime, "retry").id, Event::Click)
        .unwrap();
    runtime.advance(start + Duration::from_secs(2)).unwrap();
    close(node(&runtime, "value").style.width.unwrap().into(), 20.0);
    assert_eq!(node(&runtime, "sample").text, "20");
}

#[test]
fn an_invalid_generated_path_keeps_the_last_valid_shape() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        return ui.panel { children = { ui.path { key = 'shape', data = function()
            if x:get() > 15 then return 'not a path' end
            return string.format('M0 0 L%f 0 L0 10 Z', x:get())
        end, view_box = { x = 0, y = 0, width = 100, height = 10 } } } }
    "#,
    );
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(100)).unwrap();
    let before = runtime.surfaces().to_vec();
    assert!(runtime.advance(start + Duration::from_millis(200)).is_err());
    assert_eq!(runtime.surfaces(), before);
    assert!(!runtime.is_animating(node(&runtime, "shape").id));
}

#[test]
fn zero_duration_updates_once_without_requesting_more_frames() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 0 })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(42) end }
        return ui.panel { children = { ui.text { key = 'value', text = function() return tostring(x:get()) end } } }
    "#,
    );
    let start = start(&mut runtime);
    assert!(runtime.is_animating(node(&runtime, "value").id));
    runtime.advance(start).unwrap();
    assert_eq!(node(&runtime, "value").text, "42");
    assert!(!runtime.is_animating(node(&runtime, "value").id));
}

#[test]
fn stop_and_jump_publish_once_and_discard_failed_controls() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        ui.timer { interval = 101, autostart = true, on_timeout = function() x:stop() end }
        ui.timer { interval = 201, autostart = true, on_timeout = function() x:jump(42) end }
        return ui.panel { children = {
            ui.text { key = 'value', text = function() return tostring(x:get()) end },
            ui.text { key = 'velocity', text = function() return tostring(x:velocity()) end },
            ui.button { key = 'fail', on_click = function() x:jump(0); error('failed') end },
        } }
    "#,
    );
    let start = start(&mut runtime);
    let stopped = start + Duration::from_millis(100);
    fire(&mut runtime, stopped);
    runtime.advance(stopped).unwrap();
    assert_eq!(node(&runtime, "value").text, "10");
    assert_eq!(node(&runtime, "velocity").text, "0");
    assert!(!runtime.is_animating(node(&runtime, "value").id));
    let jumped = start + Duration::from_millis(200);
    fire(&mut runtime, jumped);
    assert_eq!(node(&runtime, "value").text, "10");
    assert!(runtime.is_animating(node(&runtime, "value").id));
    runtime.advance(jumped).unwrap();
    assert_eq!(node(&runtime, "value").text, "42");
    assert!(!runtime.is_animating(node(&runtime, "value").id));
    assert!(
        runtime
            .dispatch(node(&runtime, "fail").id, Event::Click)
            .is_err()
    );
    runtime.advance(jumped + Duration::from_secs(1)).unwrap();
    assert_eq!(node(&runtime, "value").text, "42");
}

#[test]
fn extreme_finite_values_do_not_overflow_interpolation() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(3e38, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(-3e38) end }
        return ui.panel { children = { ui.text { key = 'value', text = function() return tostring(x:get()) end } } }
    "#,
    );
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(500)).unwrap();
    assert_eq!(node(&runtime, "value").text, "0");
}

#[test]
fn declarations_and_controls_are_bounded_and_have_separate_phases() {
    for source in [
        "for i=1,129 do ui.motion(0, {kind='transition',duration=1}) end; return ui.panel {}",
        "local x=ui.motion(0, {kind='transition',duration=1}); x:to(1); return ui.panel {}",
        "return ui.panel {style={width=function() ui.motion(0, {kind='transition',duration=1}); return 1 end}}",
        "local x=ui.motion(0, {kind='transition',duration=1}); return ui.panel {style={width=function() x:to(1); return 1 end}}",
        "ui.motion(0/0, {kind='transition',duration=1}); return ui.panel {}",
        "ui.motion(0, {kind='spring',stiffness=100,damping=0}); return ui.panel {}",
        "ui.motion(0, {kind='transition',duration=1,extra=true}); return ui.panel {}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        assert!(Runtime::load(path).is_err(), "{source}");
    }
    let (_directory, mut runtime) = load(
        r#"
        local x=ui.motion(0, {kind='transition',duration=100})
        return ui.panel {children={
            ui.button {key='start',on_click=function() for i=1,257 do x:to(i) end end},
            ui.text {key='value',text=function() return tostring(x:get()) end},
        }}
    "#,
    );
    assert!(
        runtime
            .dispatch(node(&runtime, "start").id, Event::Click)
            .is_err()
    );
    assert!(!runtime.is_animating(node(&runtime, "value").id));
}

#[test]
fn hidden_children_keep_their_mapped_surface_animating() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        return ui.panel { children = { ui.text { key = 'child', text = 'arrived',
            visible = function() return x:get() > 50 end,
        } } }
    "#,
    );
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(250)).unwrap();
    assert!(!node(&runtime, "child").visible);
    assert!(runtime.is_any_animating([runtime.root().unwrap().id]));
    runtime.advance(start + Duration::from_millis(750)).unwrap();
    assert!(node(&runtime, "child").visible);
    runtime.advance(start + Duration::from_secs(1)).unwrap();
    assert!(!runtime.is_any_animating([runtime.root().unwrap().id]));
}

#[test]
fn motion_driven_child_lists_keep_identity_and_subscription() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        return ui.panel { children = function()
            return { ui.text { key = 'child', text = tostring(x:get()) } }
        end }
    "#,
    );
    let id = node(&runtime, "child").id;
    let start = start(&mut runtime);
    for time in [250, 500, 750] {
        runtime
            .advance(start + Duration::from_millis(time))
            .unwrap();
        assert_eq!(node(&runtime, "child").id, id);
        close(
            node(&runtime, "child").text.parse().unwrap(),
            time as f64 / 10.0,
        );
        assert!(runtime.is_any_animating([runtime.root().unwrap().id]));
    }
    runtime.advance(start + Duration::from_secs(1)).unwrap();
    assert!(!runtime.is_any_animating([runtime.root().unwrap().id]));
}

#[test]
fn rebuilt_children_keep_style_animation_in_the_same_frame() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        local width = ui.state(0)
        ui.timer { interval = 1, autostart = true, on_timeout = function()
            x:to(100)
            width:set(100)
        end }
        return ui.panel { children = function()
            local value = x:get()
            return { ui.row { key = 'child', style = {
                translate_x = value,
                width = ui.transition(function() return width:get() end,
                    { duration = 1000, easing = 'linear' }),
            } } }
        end }
    "#,
    );
    let id = node(&runtime, "child").id;
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(250)).unwrap();
    let child = node(&runtime, "child");
    assert_eq!(child.id, id);
    close(child.style.width.unwrap().into(), 25.0);
    close(child.style.translate_x.into(), 25.0);
}

#[test]
fn failed_reloads_keep_motion_and_successful_reloads_replace_it() {
    let source = r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        return ui.panel { children = { ui.text { key = 'value', text = function() return tostring(x:get()) end } } }
    "#;
    let (directory, mut runtime) = load(source);
    let path = directory.path().join("panel.luau");
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(250)).unwrap();
    fs::write(&path, "error('bad edit')").unwrap();
    assert!(runtime.reload(&path).is_err());
    runtime.advance(start + Duration::from_millis(500)).unwrap();
    assert_eq!(node(&runtime, "value").text, "50");
    fs::write(&path, source).unwrap();
    runtime.reload(&path).unwrap();
    assert_eq!(node(&runtime, "value").text, "0");
    assert!(!runtime.is_animating(node(&runtime, "value").id));
}

#[test]
fn lock_presentation_can_animate_without_desktop_actions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lock.luau");
    fs::write(&path, r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100) end }
        return ui.lock { children = { ui.text { key = 'value', text = function() return tostring(x:get()) end } } }
    "#).unwrap();
    let mut runtime = Runtime::load_lock(&path).unwrap();
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(250)).unwrap();
    assert_eq!(node(&runtime, "value").text, "25");
    assert!(runtime.take_actions().is_empty());
}

#[test]
fn running_only_readers_drive_frames_until_the_accepted_completion_sample() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'in_cubic' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(1) end }
        return ui.panel { children = {
            ui.text { key = 'status', text = function() return tostring(x:running()) end },
        } }
    "#,
    );
    assert_eq!(node(&runtime, "status").text, "false");
    let start = start(&mut runtime);
    assert_eq!(node(&runtime, "status").text, "false");
    let root = runtime.root().unwrap().id;
    assert!(runtime.is_any_animating([root]));
    runtime.advance(start).unwrap();
    assert_eq!(node(&runtime, "status").text, "true");
    assert!(!runtime.advance(start + Duration::from_millis(500)).unwrap());
    assert!(runtime.is_any_animating([root]));
    runtime.advance(start + Duration::from_secs(1)).unwrap();
    assert_eq!(node(&runtime, "status").text, "false");
    assert!(!runtime.is_any_animating([root]));
}

#[test]
fn spring_exit_retains_content_through_overshoot_and_reopening() {
    let (_directory, mut runtime) = load(
        r#"
        local opened = ui.state(false)
        local progress = ui.motion(1, { kind = 'spring', stiffness = 100, damping = 0.1, epsilon = 0.001 })
        ui.timer { interval = 1, autostart = true, on_timeout = function() progress:to(0) end }
        ui.timer { interval = 151, autostart = true,
            on_timeout = function() opened:set(true); progress:to(1) end }
        ui.timer { interval = 301, autostart = true,
            on_timeout = function() opened:set(false); progress:to(0) end }
        return ui.panel { children = {
            ui.text { key = 'status', text = function() return tostring(progress:running()) end },
            ui.text { key = 'position', text = function() return tostring(progress:get()) end },
            ui.column { key = 'holder', children = function()
                if not opened:get() and not progress:running() and progress:get() == 0 then
                    return {}
                end
                return { ui.button { key = 'body', style = {
                    opacity = function() return math.clamp(progress:get(), 0, 1) end,
                } } }
            end },
        } }
    "#,
    );
    let id = node(&runtime, "body").id;
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(200)).unwrap();
    assert!(node(&runtime, "position").text.parse::<f64>().unwrap() < 0.0);
    assert_eq!(node(&runtime, "status").text, "true");
    assert_eq!(node(&runtime, "body").id, id);
    fire(&mut runtime, start + Duration::from_millis(250));
    runtime.advance(start + Duration::from_millis(450)).unwrap();
    assert_eq!(node(&runtime, "body").id, id);
    fire(&mut runtime, start + Duration::from_millis(500));
    runtime.advance(start + Duration::from_millis(700)).unwrap();
    assert_eq!(node(&runtime, "body").id, id);
    runtime.advance(start + Duration::from_secs(100)).unwrap();
    assert_eq!(node(&runtime, "position").text, "0");
    assert_eq!(node(&runtime, "status").text, "false");
    assert!(node(&runtime, "holder").children.is_empty());
    assert!(!runtime.is_any_animating([runtime.root().unwrap().id]));
}

#[test]
fn stop_and_jump_clear_running_on_their_next_sample() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        local y = ui.motion(0, { kind = 'transition', duration = 1000, easing = 'linear' })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(100); y:to(100) end }
        ui.timer { interval = 251, autostart = true, on_timeout = function() x:stop(); y:jump(42) end }
        return ui.panel { children = {
            ui.text { key = 'x', text = function() return tostring(x:get()) .. ':' .. tostring(x:running()) end },
            ui.text { key = 'y', text = function() return tostring(y:get()) .. ':' .. tostring(y:running()) end },
        } }
    "#,
    );
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(100)).unwrap();
    assert_eq!(node(&runtime, "x").text, "10:true");
    let stopped = start + Duration::from_millis(250);
    fire(&mut runtime, stopped);
    assert_eq!(node(&runtime, "x").text, "10:true");
    runtime.advance(stopped).unwrap();
    assert_eq!(node(&runtime, "x").text, "25:false");
    assert_eq!(node(&runtime, "y").text, "42:false");
}

#[test]
fn immediate_and_unchanged_targets_never_report_a_running_sample() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 0 })
        local y = ui.motion(0, { kind = 'transition', duration = 1000 })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(1); y:to(0) end }
        return ui.panel { children = {
            ui.text { key = 'x', text = function() return tostring(x:get()) .. ':' .. tostring(x:running()) end },
            ui.text { key = 'y', text = function() return tostring(y:running()) end },
        } }
    "#,
    );
    let start = start(&mut runtime);
    runtime.advance(start).unwrap();
    assert_eq!(node(&runtime, "x").text, "1:false");
    assert_eq!(node(&runtime, "y").text, "false");
    assert!(!runtime.is_any_animating([runtime.root().unwrap().id]));
}

#[test]
fn rejected_completion_preserves_the_running_sample_until_recovery() {
    let (_directory, mut runtime) = load(
        r#"
        local x = ui.motion(0, { kind = 'transition', duration = 1000 })
        ui.timer { interval = 1, autostart = true, on_timeout = function() x:to(1) end }
        return ui.panel { children = {
            ui.text { key = 'status', text = function() return tostring(x:running()) end },
            ui.button { key = 'reset', on_click = function() x:jump(0) end },
            ui.row { key = 'holder', children = function()
                return if x:running() then { ui.text { key = 'retained', text = 'leaving' } } else {}
            end },
        } }
    "#,
    );
    let start = start(&mut runtime);
    runtime.advance(start + Duration::from_millis(100)).unwrap();
    assert_eq!(node(&runtime, "status").text, "true");
    let retained = node(&runtime, "retained").id;
    assert!(
        runtime
            .advance_checked(start + Duration::from_secs(1), |_| Err(
                "unsupported frame".into()
            ))
            .is_err()
    );
    assert_eq!(node(&runtime, "status").text, "true");
    assert_eq!(node(&runtime, "retained").id, retained);
    assert!(!runtime.is_any_animating([runtime.root().unwrap().id]));
    runtime
        .dispatch(node(&runtime, "reset").id, Event::Click)
        .unwrap();
    runtime.advance(start + Duration::from_secs(2)).unwrap();
    assert_eq!(node(&runtime, "status").text, "false");
    assert!(node(&runtime, "holder").children.is_empty());
}
