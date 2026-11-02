use std::{fs, path::PathBuf};
use tempfile::TempDir;
use thing_core::DropType;
use thing_runtime::{
    DragAction, DragActions, DragData, DragOverInput, DragOverPhase, DragResult, DropInput,
    DropPosition, PointerButton, PointerEvent, PointerPhase, Runtime,
};

fn fixture(source: &str) -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

fn pointer(phase: PointerPhase, button: PointerButton) -> PointerEvent {
    PointerEvent {
        phase,
        button,
        x: 1.0,
        y: 2.0,
        surface_x: 3.0,
        surface_y: 4.0,
        shift: false,
        ctrl: false,
        alt: false,
        logo: false,
    }
}

fn text(runtime: &Runtime, index: usize) -> String {
    runtime.root().unwrap().children[index].text.clone()
}

const SOURCE: &str = r#"
    local log = ui.state('')
    local stored = nil
    local function note(value) log:set(log:get() .. value .. ';') end
    return ui.panel { children = {
        ui.pointer_area {
            on_pointer = function(event)
                if event.phase == 'press' and event.button == 'secondary' then
                    note(tostring(pcall(event.start_drag, event, { text = 'x' })))
                elseif event.phase == 'press' then
                    stored = event
                elseif event.phase == 'move' then
                    -- an earlier event table cannot start a drag.
                    note(tostring(pcall(stored.start_drag, stored, { text = 'x' })))
                    event:start_drag {
                        files = { 'file:///tmp/a%20b', '/tmp//c' },
                        actions = { 'copy', 'move' },
                        on_end = function(result) note('end ' .. result) end,
                    }
                    note(tostring(pcall(event.start_drag, event, { text = 'y' })))
                end
                return true
            end,
        },
        ui.text { text = function() return log:get() end },
    } }
"#;

#[test]
fn start_drag_needs_the_current_primary_press_or_move() {
    let (_directory, path) = fixture(SOURCE);
    let mut runtime = Runtime::load(path).unwrap();
    let area = runtime.root().unwrap().children[0].id;
    let target = runtime.pointer_target(area).unwrap();

    runtime
        .begin_pointer_checked(
            &target,
            pointer(PointerPhase::Press, PointerButton::Secondary),
            |_| Ok(()),
        )
        .unwrap();
    runtime
        .dispatch_pointer_checked(
            pointer(PointerPhase::Release, PointerButton::Secondary),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(text(&runtime, 1), "false;");
    assert!(runtime.take_drag_request().is_none());

    runtime
        .begin_pointer_checked(
            &target,
            pointer(PointerPhase::Press, PointerButton::Primary),
            |_| Ok(()),
        )
        .unwrap();
    runtime
        .dispatch_pointer_checked(pointer(PointerPhase::Move, PointerButton::Primary), |_| {
            Ok(())
        })
        .unwrap();
    // the stored event failed, then a second start in the same callback
    // failed because one drag was already requested.
    assert_eq!(text(&runtime, 1), "false;false;false;");
    let request = runtime.take_drag_request().unwrap();
    assert_eq!(
        request.data,
        DragData::Files(vec!["/tmp/a b".into(), "/tmp/c".into()])
    );
    assert_eq!(
        request.actions,
        DragActions {
            copy: true,
            moves: true
        }
    );
    assert_eq!(runtime.active_drag(), Some(request.id));

    runtime
        .finish_drag_checked(request.id, DragResult::Move, |_| Ok(()))
        .unwrap();
    assert!(text(&runtime, 1).ends_with("end move;"));
    // a repeated or stale end does nothing.
    assert!(
        !runtime
            .finish_drag_checked(request.id, DragResult::Cancelled, |_| Ok(()))
            .unwrap()
    );
    assert_eq!(text(&runtime, 1).matches("end").count(), 1);
    assert_eq!(runtime.active_drag(), None);
}

#[test]
fn start_drag_rejects_invalid_payloads() {
    for options in [
        "{ text = 'a', files = { '/a' } }",
        "{}",
        "{ files = {} }",
        "{ files = { 'https://example.com/a' } }",
        "{ files = { 'file://host/a' } }",
        "{ files = { 'relative' } }",
        "{ files = { '/a/../b' } }",
        "{ text = 'a\\0b' }",
        "{ text = '\\255' }",
        "{ text = 'a', actions = { 'ask' } }",
        "{ text = 'a', actions = {} }",
        "{ text = 'a', extra = true }",
    ] {
        let (_directory, path) = fixture(&format!(
            r#"
            local result = ui.state('')
            return ui.panel {{ children = {{
                ui.pointer_area {{
                    on_pointer = function(event)
                        if event.phase == 'press' then
                            result:set(tostring(pcall(event.start_drag, event, {options})))
                        end
                    end,
                }},
                ui.text {{ text = function() return result:get() end }},
            }} }}
            "#
        ));
        let mut runtime = Runtime::load(path).unwrap();
        let area = runtime.root().unwrap().children[0].id;
        let target = runtime.pointer_target(area).unwrap();
        runtime
            .begin_pointer_checked(
                &target,
                pointer(PointerPhase::Press, PointerButton::Primary),
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(text(&runtime, 1), "false", "accepted {options}");
        assert!(runtime.take_drag_request().is_none());
    }
}

#[test]
fn drop_targets_choose_type_and_action_and_report_acceptance() {
    let (_directory, path) = fixture(
        r#"
        local log = ui.state('')
        local function note(value) log:set(log:get() .. value .. ';') end
        return ui.panel { children = {
            ui.pointer_area {
                drop_types = { 'text', 'files' },
                on_drag_over = function(event)
                    note(event.phase .. ' ' .. table.concat(event.types, ','))
                    if event.phase == 'move' then return 'move' end
                    return if table.find(event.actions, 'copy') then 'copy' else nil
                end,
                on_drop = function(event)
                    note('drop ' .. event.type .. ' ' .. event.action .. ' ' .. event.text)
                    return event.text == 'keep'
                end,
                children = { ui.text { text = 'target' } },
            },
            ui.text { text = function() return log:get() end },
        } }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    let area = runtime.root().unwrap().children[0].id;
    let target = runtime.drop_target(area).unwrap();
    let both = DragActions {
        copy: true,
        moves: true,
    };
    let over = |phase, actions| DragOverInput {
        phase,
        position: DropPosition::default(),
        offered: vec![DropType::Files, DropType::Text],
        actions,
    };

    let entered = runtime
        .dispatch_drag_over_checked(&target, over(DragOverPhase::Enter, both), |_| Ok(()))
        .unwrap()
        .unwrap();
    // the target's own preference picks text over files.
    assert_eq!(entered.accepted, Some((DropType::Text, DragAction::Copy)));
    let moved = runtime
        .dispatch_drag_over_checked(&target, over(DragOverPhase::Move, both), |_| Ok(()))
        .unwrap()
        .unwrap();
    assert_eq!(moved.accepted, Some((DropType::Text, DragAction::Move)));
    // returning an action the source does not offer is an error.
    let copy_only = DragActions {
        copy: true,
        moves: false,
    };
    assert!(
        runtime
            .dispatch_drag_over_checked(&target, over(DragOverPhase::Move, copy_only), |_| Ok(()))
            .is_err()
    );
    let left = runtime
        .dispatch_drag_over_checked(&target, over(DragOverPhase::Leave, both), |_| Ok(()))
        .unwrap()
        .unwrap();
    assert_eq!(left.accepted, None);

    let drop = |text: &str| DropInput {
        position: DropPosition::default(),
        action: DragAction::Move,
        data: DragData::Text(text.into()),
    };
    let rejected = runtime
        .dispatch_drop_checked(&target, drop("discard"), |_| Ok(()))
        .unwrap()
        .unwrap();
    assert!(rejected.changed && !rejected.accepted);
    let accepted = runtime
        .dispatch_drop_checked(&target, drop("keep"), |_| Ok(()))
        .unwrap()
        .unwrap();
    assert!(accepted.accepted);
    let log = text(&runtime, 1);
    assert!(
        log.starts_with("enter text,files;move text,files;"),
        "{log}"
    );
    assert!(
        log.ends_with("leave text,files;drop text move discard;drop text move keep;"),
        "{log}"
    );
}

#[test]
fn drop_handlers_are_validated_when_the_tree_is_built() {
    for source in [
        "return ui.panel { children = { ui.pointer_area { on_drop = function() end } } }",
        "return ui.panel { children = { ui.pointer_area { on_drag_over = function() end, on_pointer = function() end } } }",
        "return ui.panel { children = { ui.button { on_drop = function() end, on_drag_over = function() end } } }",
        "return ui.panel { children = { ui.pointer_area { on_pointer = function() end, drop_types = { 'text' } } } }",
        "return ui.panel { children = { ui.pointer_area { on_drop = function() end, on_drag_over = function() end, drop_types = { 'text', 'text' } } } }",
        "return ui.panel { children = { ui.pointer_area { on_drop = function() end, on_drag_over = function() end, drop_types = { 'image' } } } }",
    ] {
        let (_directory, path) = fixture(source);
        assert!(Runtime::load(path).is_err(), "accepted {source}");
    }
}
