use std::fs;
use thing_runtime::{
    Action, AudioDevice, Event, NiriAction, NiriState, NiriWindow, Runtime, Scalar, Workspace,
};

fn workspace(id: u64) -> Workspace {
    Workspace {
        id,
        index: 1,
        name: None,
        output: Some("DP-1".into()),
        active: true,
        focused: true,
        urgent: false,
    }
}

#[test]
fn lists_preserve_full_ids_are_immutable_and_survive_reload() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        local niri = services.niri
        return ui.panel { children = function()
            local list = niri.workspaces:get()
            if #list == 0 then return {} end
            assert(not pcall(function() list[1].id = '0' end))
            assert(not pcall(function() list[2] = list[1] end))
            return { ui.button {
                key = list[1].id,
                on_click = function() niri.focus_workspace(list[1].id) end,
                children = { ui.text { text = list[1].id } },
            } }
        end }
    "#,
    )
    .unwrap();
    let mut runtime = Runtime::load(&path).unwrap();
    let snapshot = NiriState {
        workspaces: vec![workspace(u64::MAX)],
        windows: Vec::new(),
    };
    runtime.set_niri(snapshot.clone()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(
        runtime.root().children[0].children[0].text,
        u64::MAX.to_string()
    );
    runtime
        .dispatch(runtime.root().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::Niri(NiriAction::FocusWorkspace(u64::MAX))]
    );
    runtime.set_niri(snapshot).unwrap();
    assert!(!runtime.refresh().unwrap());
    assert!(
        runtime
            .set_niri(NiriState {
                workspaces: vec![workspace(1), workspace(1)],
                windows: vec![]
            })
            .is_err()
    );
    runtime.reload(&path).unwrap();
    assert_eq!(
        runtime.root().children[0].children[0].text,
        u64::MAX.to_string()
    );
}

#[test]
fn rejects_untracked_actions_and_imprecise_or_invalid_ids() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    for body in [
        "services.niri.focus_window('1'); return ui.panel {}",
        "return ui.panel { children = function() services.niri.close_window('1'); return {} end }",
    ] {
        fs::write(&path, body).unwrap();
        assert!(Runtime::load(&path).is_err());
    }
    for id in ["1", "'1.5'", "'-1'", "'18446744073709551616'", "''"] {
        fs::write(&path, format!("return ui.panel {{ children = {{ ui.button {{ on_click = function() services.niri.focus_window({id}) end }} }} }}")).unwrap();
        let mut runtime = Runtime::load(&path).unwrap();
        assert!(
            runtime
                .dispatch(runtime.root().children[0].id, Event::Click)
                .is_err()
        );
        assert!(runtime.take_actions().is_empty());
    }
}

#[test]
fn routes_window_clicks_volume_scroll_and_popup_controls() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        local audio = services.audio
        local open = ui.state(false)
        return ui.panel { children = {
            ui.row { children = function()
                local buttons = {}
                for _, window in services.niri.windows:get() do
                    table.insert(buttons, ui.button {
                        key = window.id,
                        on_click = function() services.niri.focus_window(window.id) end,
                        on_middle_click = function() services.niri.close_window(window.id) end,
                    })
                end
                return buttons
            end },
            ui.button {
                key = 'volume',
                on_click = function() audio.set_muted(not audio.muted:get()) end,
                on_scroll = function(_, y)
                    audio.set_muted(false)
                    audio.set_volume(math.clamp(audio.volume:get() - y * 0.05, 0, 1))
                end,
                on_secondary_click = function() open:set(not open:get()) end,
                children = { ui.popup {
                    key = 'mixer',
                    visible = function() return open:get() end,
                    on_close = function() open:set(false) end,
                    children = function()
                        local buttons = {}
                        for _, device in audio.devices:get() do
                            table.insert(buttons, ui.button {
                                key = device.name,
                                on_click = function() audio.set_default_sink(device.name) end,
                            })
                        end
                        return buttons
                    end,
                } },
            },
        } }
        "#,
    )
    .unwrap();
    let mut runtime = Runtime::load(path).unwrap();
    let snapshot = NiriState {
        workspaces: vec![workspace(7)],
        windows: vec![NiriWindow {
            id: 42,
            workspace_id: Some(7),
            title: Some("Terminal".into()),
            app_id: Some("foot".into()),
            focused: true,
            urgent: false,
            floating: false,
            column: Some(1),
            row: Some(1),
            width: 960.0,
            height: 1000.0,
        }],
    };
    runtime.set_niri(snapshot).unwrap();
    runtime
        .set_audio_devices(vec![AudioDevice {
            name: "speakers".into(),
            description: "Speakers".into(),
        }])
        .unwrap();
    for (name, value) in [
        ("niri.available", Scalar::Boolean(true)),
        ("audio.available", Scalar::Boolean(true)),
        ("audio.volume", Scalar::Number(0.5)),
    ] {
        runtime.set_service(name, value).unwrap();
    }
    runtime.refresh().unwrap();
    let window = key(runtime.root(), "42").unwrap();
    runtime.dispatch(window, Event::Click).unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::Niri(NiriAction::FocusWindow(42))]
    );
    runtime.dispatch(window, Event::MiddleClick).unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::Niri(NiriAction::CloseWindow(42))]
    );
    let volume = key(runtime.root(), "volume").unwrap();
    runtime.dispatch(volume, Event::Click).unwrap();
    assert_eq!(runtime.take_actions(), vec![Action::SetMuted(true)]);
    runtime
        .dispatch(volume, Event::Scroll { x: 0.0, y: -1.0 })
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::SetMuted(false), Action::SetVolume(0.55)]
    );
    assert!(
        runtime
            .dispatch(
                volume,
                Event::Scroll {
                    x: f64::NAN,
                    y: 0.0
                }
            )
            .is_err()
    );
    runtime.dispatch(volume, Event::SecondaryClick).unwrap();
    let popup = key(runtime.root(), "mixer").unwrap();
    assert!(runtime.root().find(popup).unwrap().visible);
    let device = key(runtime.root(), "speakers").unwrap();
    runtime.dispatch(device, Event::Click).unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::SetDefaultSink("speakers".into())]
    );
    runtime.dispatch(popup, Event::Close).unwrap();
    assert!(!runtime.root().find(popup).unwrap().visible);
}

fn key(node: &thing_core::Node, key: &str) -> Option<thing_core::NodeId> {
    if node.key.as_deref() == Some(key) {
        return Some(node.id);
    }
    node.children.iter().find_map(|child| self::key(child, key))
}
