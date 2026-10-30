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
        ..NiriState::default()
    };
    runtime.set_niri(snapshot.clone()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].children[0].text,
        u64::MAX.to_string()
    );
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
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
                windows: vec![],
                ..NiriState::default()
            })
            .is_err()
    );
    runtime.reload(&path).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].children[0].text,
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
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
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
            window_size: None,
        }],
        ..NiriState::default()
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
    let window = key(runtime.root().unwrap(), "42").unwrap();
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
    let volume = key(runtime.root().unwrap(), "volume").unwrap();
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
    let popup = key(runtime.root().unwrap(), "mixer").unwrap();
    assert!(runtime.root().unwrap().find(popup).unwrap().visible);
    let device = key(runtime.root().unwrap(), "speakers").unwrap();
    runtime.dispatch(device, Event::Click).unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![Action::SetDefaultSink("speakers".into())]
    );
    runtime.dispatch(popup, Event::Close).unwrap();
    assert!(!runtime.root().unwrap().find(popup).unwrap().visible);
}

fn key(node: &thing_core::Node, key: &str) -> Option<thing_core::NodeId> {
    if node.key.as_deref() == Some(key) {
        return Some(node.id);
    }
    node.children.iter().find_map(|child| self::key(child, key))
}

#[test]
fn overview_and_keyboard_layouts_are_reactive_immutable_and_preserved_on_reload() {
    use thing_runtime::NiriKeyboardLayouts;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, r#"
        local niri = services.niri
        local reads = 0
        return ui.panel { children = {
            ui.text { key = 'overview', text = function() return tostring(niri.overview_open:get()) end },
            ui.text { key = 'layout', text = function()
                local layouts = niri.keyboard_layouts:get()
                if not layouts then return 'unknown' end
                assert(not pcall(function() layouts.current_index = 100 end))
                assert(not pcall(function() layouts.names[1] = 'changed' end))
                return if layouts.current_index then layouts.names[layouts.current_index] else 'empty'
            end },
            ui.text { key = 'reads', text = function()
                niri.workspaces:get(); reads += 1; return tostring(reads)
            end },
        } }
    "#).unwrap();
    let mut runtime = Runtime::load(&path).unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "nil");
    assert_eq!(runtime.root().unwrap().children[1].text, "unknown");
    let mut state = NiriState {
        overview_open: Some(false),
        keyboard_layouts: Some(NiriKeyboardLayouts {
            names: vec!["English".into(), "עברית".into()],
            current_index: 1,
        }),
        ..NiriState::default()
    };
    runtime.set_niri(state.clone()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "false");
    assert_eq!(runtime.root().unwrap().children[1].text, "עברית");
    assert_eq!(runtime.root().unwrap().children[2].text, "1");
    state.overview_open = Some(true);
    state.keyboard_layouts.as_mut().unwrap().current_index = 0;
    runtime.set_niri(state.clone()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "true");
    assert_eq!(runtime.root().unwrap().children[1].text, "English");
    assert_eq!(runtime.root().unwrap().children[2].text, "1");
    runtime.set_niri(state).unwrap();
    assert!(!runtime.refresh().unwrap());
    runtime.reload(&path).unwrap();
    assert_eq!(runtime.root().unwrap().children[1].text, "English");
    runtime.set_niri(NiriState::default()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "nil");
    assert_eq!(runtime.root().unwrap().children[1].text, "unknown");
}

#[test]
fn overview_and_layout_actions_are_checked_and_do_not_change_reported_state() {
    use thing_runtime::NiriLayoutTarget;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        local niri = services.niri
        return ui.panel { children = {
            ui.button { on_click = function()
                niri.set_overview(true); niri.toggle_overview(); niri.set_overview(false)
                niri.switch_layout('next'); niri.switch_layout('prev')
                niri.switch_layout(1); niri.switch_layout(256)
                assert(niri.overview_open:get() == nil)
            end },
        } }
    "#,
    )
    .unwrap();
    let mut runtime = Runtime::load(&path).unwrap();
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::Niri(NiriAction::SetOverview(true)),
            Action::Niri(NiriAction::ToggleOverview),
            Action::Niri(NiriAction::SetOverview(false)),
            Action::Niri(NiriAction::SwitchLayout(NiriLayoutTarget::Next)),
            Action::Niri(NiriAction::SwitchLayout(NiriLayoutTarget::Previous)),
            Action::Niri(NiriAction::SwitchLayout(NiriLayoutTarget::Index(0))),
            Action::Niri(NiriAction::SwitchLayout(NiriLayoutTarget::Index(255))),
        ]
    );
    for action in [
        "niri.switch_layout(0)",
        "niri.switch_layout(257)",
        "niri.switch_layout(1.5)",
        "niri.switch_layout(0/0)",
        "niri.switch_layout('previous')",
        "niri.switch_layout(false)",
        "niri.set_overview(1)",
        "niri.overview_open:set(true)",
        "niri.toggle_overview(); error('callback failed')",
    ] {
        fs::write(&path, format!("local niri=services.niri; return ui.panel {{ children = {{ ui.button {{ on_click = function() {action} end }} }} }}")).unwrap();
        let mut runtime = Runtime::load(&path).unwrap();
        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "{action}"
        );
        assert!(runtime.take_actions().is_empty());
    }
    for action in [
        "services.niri.toggle_overview()",
        "services.niri.switch_layout('next')",
        "services.niri.set_overview(true)",
    ] {
        fs::write(&path, format!("{action}; return ui.panel {{}}")).unwrap();
        assert!(
            Runtime::load(&path).is_err(),
            "accepted {action} while loading"
        );
    }
}

#[test]
fn invalid_layout_snapshots_leave_the_previous_value_intact() {
    use thing_runtime::NiriKeyboardLayouts;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, "return ui.panel {children={ui.text {text=function() local x=services.niri.keyboard_layouts:get(); return if not x then 'unknown' elseif not x.current_index then 'empty' else x.names[x.current_index] end}}}").unwrap();
    let mut runtime = Runtime::load(path).unwrap();
    runtime
        .set_niri(NiriState {
            keyboard_layouts: Some(NiriKeyboardLayouts {
                names: vec![],
                current_index: 0,
            }),
            ..NiriState::default()
        })
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "empty");
    for layouts in [
        NiriKeyboardLayouts {
            names: vec![],
            current_index: 1,
        },
        NiriKeyboardLayouts {
            names: vec!["English".into()],
            current_index: 1,
        },
        NiriKeyboardLayouts {
            names: vec!["English".into(); 257],
            current_index: 0,
        },
        NiriKeyboardLayouts {
            names: vec!["bad\0name".into()],
            current_index: 0,
        },
    ] {
        assert!(
            runtime
                .set_niri(NiriState {
                    keyboard_layouts: Some(layouts),
                    ..NiriState::default()
                })
                .is_err()
        );
        assert!(!runtime.refresh().unwrap());
        assert_eq!(runtime.root().unwrap().children[0].text, "empty");
    }
}

fn cast(id: u64) -> thing_runtime::NiriCast {
    use thing_runtime::{NiriCast, NiriCastKind, NiriCastTarget};
    NiriCast {
        stream_id: id,
        session_id: u64::MAX,
        kind: NiriCastKind::PipeWire,
        target: NiriCastTarget::Window(u64::MAX),
        dynamic_target: true,
        active: true,
        pid: None,
        pipewire_node_id: Some(u32::MAX),
    }
}

#[test]
fn casts_preserve_ids_and_paused_streams_with_independent_reactive_updates() {
    use thing_runtime::{NiriCastKind, NiriCastTarget};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        local niri = services.niri
        local reads = 0
        return ui.panel { children = {
            ui.text { text = function()
                local casts = niri.casts:get()
                if not casts then return 'unknown' end
                assert(not pcall(function() casts[1] = nil end))
                if #casts == 0 then return 'empty' end
                local cast = casts[1]
                assert(not pcall(function() cast.active = false end))
                assert(not pcall(function() cast.target.kind = 'none' end))
                assert(cast.stream_id == '18446744073709551615')
                assert(cast.session_id == '18446744073709551615')
                if cast.kind == 'pipewire' then
                    assert(cast.target.window_id == '18446744073709551615')
                    assert(cast.pipewire_node_id == 4294967295 and cast.pid == nil)
                end
                return cast.kind .. ':' .. cast.target.kind .. ':' .. tostring(cast.active)
                    .. ':' .. tostring(cast.target.output or cast.protocol_kind or '')
            end },
            ui.text { text = function()
                niri.windows:get(); niri.workspaces:get(); niri.keyboard_layouts:get()
                niri.overview_open:get(); reads += 1; return tostring(reads)
            end },
        } }
    "#,
    )
    .unwrap();
    let mut runtime = Runtime::load(&path).unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "unknown");
    let mut state = NiriState {
        casts: Some(vec![cast(u64::MAX)]),
        ..NiriState::default()
    };
    runtime.set_niri(state.clone()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "pipewire:window:true:"
    );
    state.casts.as_mut().unwrap()[0].active = false;
    state.casts.as_mut().unwrap()[0].kind = NiriCastKind::WlrScreencopy;
    state.casts.as_mut().unwrap()[0].target = NiriCastTarget::Output("DP-1".into());
    runtime.set_niri(state.clone()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "wlr_screencopy:output:false:DP-1"
    );
    assert_eq!(runtime.root().unwrap().children[1].text, "1");
    runtime.set_niri(state.clone()).unwrap();
    assert!(!runtime.refresh().unwrap());
    runtime.reload(&path).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "wlr_screencopy:output:false:DP-1"
    );
    state.casts.as_mut().unwrap()[0].kind = NiriCastKind::Unknown("FutureProtocol".into());
    state.casts.as_mut().unwrap()[0].target = NiriCastTarget::Unknown("Region".into());
    runtime.set_niri(state).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "unknown:unknown:false:FutureProtocol"
    );
    runtime
        .set_niri(NiriState {
            casts: Some(vec![]),
            ..NiriState::default()
        })
        .unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "empty");
    runtime.set_niri(NiriState::default()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "unknown");
}

#[test]
fn invalid_cast_snapshots_do_not_replace_accepted_state() {
    use thing_runtime::NiriCastTarget;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, "return ui.panel {children={ui.text {text=function() local c=services.niri.casts:get(); return if c then tostring(#c) else 'unknown' end}}}").unwrap();
    let mut runtime = Runtime::load(path).unwrap();
    runtime
        .set_niri(NiriState {
            casts: Some(vec![cast(1)]),
            ..NiriState::default()
        })
        .unwrap();
    runtime.refresh().unwrap();
    let mut bad_text = cast(2);
    bad_text.target = NiriCastTarget::Output("bad\0output".into());
    for casts in [
        vec![cast(1), cast(1)],
        vec![bad_text],
        (0..10001).map(cast).collect(),
    ] {
        assert!(
            runtime
                .set_niri(NiriState {
                    casts: Some(casts),
                    ..NiriState::default()
                })
                .is_err()
        );
        assert!(!runtime.refresh().unwrap());
        assert_eq!(runtime.root().unwrap().children[0].text, "1");
    }
}

#[test]
fn moves_use_explicit_ids_and_validate_arguments_before_queuing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        local niri = services.niri
        return ui.panel { children = { ui.button { on_click = function()
            niri.move_window_to_workspace('18446744073709551615', '2')
            niri.move_window_to_workspace('3', '18446744073709551615', false)
            niri.move_window_to_output('4', 'DP-1')
            niri.move_workspace_to_output('5', 'מסך 2')
            assert(#niri.windows:get() == 0 and #niri.workspaces:get() == 0)
        end } } }
    "#,
    )
    .unwrap();
    let mut runtime = Runtime::load(&path).unwrap();
    runtime
        .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
        .unwrap();
    assert_eq!(
        runtime.take_actions(),
        vec![
            Action::Niri(NiriAction::MoveWindowToWorkspace {
                window_id: u64::MAX,
                workspace_id: 2,
                focus: true
            }),
            Action::Niri(NiriAction::MoveWindowToWorkspace {
                window_id: 3,
                workspace_id: u64::MAX,
                focus: false
            }),
            Action::Niri(NiriAction::MoveWindowToOutput {
                window_id: 4,
                output: "DP-1".into()
            }),
            Action::Niri(NiriAction::MoveWorkspaceToOutput {
                workspace_id: 5,
                output: "מסך 2".into()
            }),
        ]
    );
    for action in [
        "niri.move_window_to_workspace(1, '2')",
        "niri.move_window_to_workspace('1', '18446744073709551616')",
        "niri.move_window_to_workspace('1', '2', 1)",
        "niri.move_window_to_output('1', 3)",
        "niri.move_window_to_output('1', '')",
        "niri.move_window_to_output('1', 'DP\\0-1')",
        "niri.move_workspace_to_output('1', string.rep('a', 4097))",
        "niri.move_workspace_to_output('-1', 'DP-1')",
        "niri.move_window_to_output('1', 'DP-1'); error('failed callback')",
    ] {
        fs::write(&path, format!("local niri=services.niri; return ui.panel {{ children = {{ ui.button {{ on_click = function() {action} end }} }} }}")).unwrap();
        let mut runtime = Runtime::load(&path).unwrap();
        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "{action}"
        );
        assert!(runtime.take_actions().is_empty());
    }
    for action in [
        "services.niri.move_window_to_workspace('1', '2')",
        "services.niri.move_window_to_output('1', 'DP-1')",
        "services.niri.move_workspace_to_output('1', 'DP-1')",
    ] {
        fs::write(&path, format!("{action}; return ui.panel {{}}")).unwrap();
        assert!(
            Runtime::load(&path).is_err(),
            "accepted {action} while loading"
        );
    }
}

#[test]
fn window_geometry_is_optional_reactive_and_validated_before_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
        return ui.panel { children = { ui.text { text = function()
            local window = services.niri.windows:get()[1]
            if not window then return 'no window' end
            return tostring(window.width) .. ':' .. tostring(window.height) .. ':'
                .. tostring(window.window_width) .. ':' .. tostring(window.window_height)
        end } } }
    "#,
    )
    .unwrap();
    let mut runtime = Runtime::load(&path).unwrap();
    let mut state = NiriState {
        windows: vec![NiriWindow {
            id: 1,
            workspace_id: Some(1),
            title: None,
            app_id: None,
            focused: true,
            urgent: false,
            floating: true,
            column: None,
            row: None,
            width: 960.0,
            height: 1000.0,
            window_size: Some((928, 968)),
        }],
        ..NiriState::default()
    };
    runtime.set_niri(state.clone()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "960:1000:928:968");
    state.windows[0].window_size = Some((1200, 800));
    runtime.set_niri(state.clone()).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "960:1000:1200:800"
    );
    runtime.reload(&path).unwrap();
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "960:1000:1200:800"
    );
    let mut invalid = state.clone();
    invalid.windows[0].window_size = Some((-1, 800));
    assert!(runtime.set_niri(invalid).is_err());
    assert!(!runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "960:1000:1200:800"
    );
    state.windows[0].window_size = None;
    runtime.set_niri(state).unwrap();
    runtime.refresh().unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "960:1000:nil:nil");
}
