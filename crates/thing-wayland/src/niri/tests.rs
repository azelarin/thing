use super::*;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixListener,
    sync::mpsc,
    time::Duration,
};

fn workspaces(id: u64) -> Value {
    json!({"WorkspacesChanged": {"workspaces": [{
        "id": id, "idx": 1, "name": null, "output": "DP-1", "is_active": true, "is_focused": true,
    }]}})
}

fn window(id: u64, focused: bool) -> Value {
    json!({"id": id, "workspace_id": 1, "title": "Test", "app_id": "test",
        "is_focused": focused, "is_floating": false,
        "layout": {"pos_in_scrolling_layout": [1, 1], "tile_size": [960.0, 1000.0]}})
}

fn apply(model: &mut Model, event: Value) {
    model.apply(&serde_json::to_vec(&event).unwrap()).unwrap();
}

fn send(mut stream: &UnixStream, value: Value) {
    writeln!(stream, "{value}").unwrap();
}

fn initial_state(stream: &UnixStream, workspace: u64) {
    send(stream, workspaces(workspace));
    send(stream, json!({"WindowsChanged":{"windows":[]}}));
}

fn accept(listener: &UnixListener) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return stream;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "worker never connected");
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("accept: {e}"),
        }
    }
}

fn read(reader: &mut BufReader<&UnixStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn wait_for(niri: &Niri, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(snapshot) = niri.take_snapshot()
            && predicate(&snapshot)
        {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "snapshot never arrived");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn event_projection_tracks_focus_layout_urgency_and_removal() {
    let mut model = Model::default();
    apply(&mut model, workspaces(1));
    apply(
        &mut model,
        json!({"WindowsChanged": {"windows": [window(10, true)]}}),
    );
    apply(
        &mut model,
        json!({"WindowOpenedOrChanged": {"window": window(11, true)}}),
    );
    let state = model.snapshot();
    assert!(!state.windows[0].focused);
    assert!(state.windows[1].focused);
    apply(
        &mut model,
        json!({"WindowLayoutsChanged": {"changes": [[11,
        {"pos_in_scrolling_layout": [2, 3], "tile_size": [600, 400]}]]}}),
    );
    apply(
        &mut model,
        json!({"WindowUrgencyChanged": {"id": 11, "urgent": true}}),
    );
    apply(&mut model, json!({"WindowClosed": {"id": 10}}));
    let state = model.snapshot();
    assert_eq!(state.windows.len(), 1);
    assert_eq!(
        (state.windows[0].column, state.windows[0].row),
        (Some(2), Some(3))
    );
    assert_eq!(state.windows[0].width, 600.0);
    assert!(state.windows[0].urgent);
    assert!(!model.apply(br#"{"FutureEvent":{"value":1}}"#).unwrap());
    assert!(
        model
            .apply(br#"{"WorkspaceActivated":{"id":999,"focused":true}}"#)
            .is_err()
    );
}

#[test]
fn workspace_activation_changes_only_the_target_output() {
    let mut model = Model::default();
    apply(
        &mut model,
        json!({"WorkspacesChanged": {"workspaces": [
            {"id":1,"idx":1,"output":"DP-1","is_active":true,"is_focused":true},
            {"id":2,"idx":2,"output":"DP-1","is_active":false,"is_focused":false},
            {"id":3,"idx":1,"output":"DP-2","is_active":true,"is_focused":false}
        ]}}),
    );
    apply(
        &mut model,
        json!({"WorkspaceActivated":{"id":2,"focused":false}}),
    );
    let workspaces = model.snapshot().workspaces;
    assert!(!workspaces[0].active);
    assert!(workspaces[0].focused);
    assert!(workspaces[1].active);
    assert!(workspaces[2].active);
}

#[test]
fn worker_handles_split_frames_serializes_actions_and_stops_while_waiting_for_reply() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("niri.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (sent, received) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut events = accept(&listener);
        assert_eq!(read(&mut BufReader::new(&events)), json!("EventStream"));
        events.write_all(b"{\"Ok\":\"Han").unwrap();
        events.write_all(b"dled\"}\n").unwrap();
        initial_state(&events, u64::MAX);
        let commands = accept(&listener);
        let mut reader = BufReader::new(&commands);
        assert_eq!(
            read(&mut reader),
            json!({"Action":{"FocusWorkspace":{"reference":{"Id":u64::MAX}}}})
        );
        // the next action cannot arrive before this one's reply.
        commands.set_nonblocking(true).unwrap();
        let mut byte = [0];
        assert_eq!(
            (&commands).read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        commands.set_nonblocking(false).unwrap();
        send(&commands, json!({"Ok":"Handled"}));
        assert_eq!(
            read(&mut reader),
            json!({"Action":{"FocusWindow":{"id":42}}})
        );
        sent.send(()).unwrap();
        // deliberately withhold a reply. dropping the worker must close the socket.
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
    });
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let niri = Niri::start_at(Some(path), ping).unwrap();
    wait_for(&niri, |s| s.available && s.state.workspaces.len() == 1);
    niri.send(NiriAction::FocusWorkspace(u64::MAX)).unwrap();
    niri.send(NiriAction::FocusWindow(42)).unwrap();
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    drop(niri);
    assert!(start.elapsed() < Duration::from_secs(1));
    server.join().unwrap();
}

#[test]
fn subscription_does_not_publish_before_initial_state_is_complete() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("niri.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let events = accept(&listener);
        assert_eq!(read(&mut BufReader::new(&events)), json!("EventStream"));
        send(&events, json!({"Ok":"Handled"}));
        send(&events, workspaces(1));
    });
    let (_sender, wake) = UnixStream::pair().unwrap();
    wake.set_nonblocking(true).unwrap();
    let shared = Mutex::new(Shared {
        commands: VecDeque::from([NiriAction::FocusWindow(10)]),
        ..Shared::default()
    });
    let mut snapshots = Vec::new();

    assert!(session(&path, &shared, &wake, |snapshot| snapshots.push(snapshot)).is_err());
    server.join().unwrap();
    assert!(
        snapshots.is_empty(),
        "the service became available before workspaces and windows arrived"
    );
    assert_eq!(lock(&shared).commands.len(), 1);
}

#[test]
fn disconnect_clears_stale_data_and_reconnect_replaces_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("niri.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (disconnect, command) = mpsc::channel();
    let (done, finish) = mpsc::channel();
    let server = thread::spawn(move || {
        let events = accept(&listener);
        assert_eq!(read(&mut BufReader::new(&events)), json!("EventStream"));
        send(&events, json!({"Ok":"Handled"}));
        initial_state(&events, 1);
        command.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(events);
        let events = accept(&listener);
        read(&mut BufReader::new(&events));
        send(&events, json!({"Ok":"Handled"}));
        initial_state(&events, 2);
        finish.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let niri = Niri::start_at(Some(path), ping).unwrap();
    wait_for(&niri, |s| {
        s.available && s.state.workspaces.first().is_some_and(|w| w.id == 1)
    });
    disconnect.send(()).unwrap();
    let offline = wait_for(&niri, |s| !s.available);
    assert!(offline.state.workspaces.is_empty());
    assert!(niri.send(NiriAction::FocusWindow(1)).is_err());
    wait_for(&niri, |s| {
        s.available && s.state.workspaces.first().is_some_and(|w| w.id == 2)
    });
    done.send(()).unwrap();
    drop(niri);
    server.join().unwrap();
}

#[test]
fn missing_niri_environment_needs_no_worker_and_rejects_actions() {
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let niri = Niri::start_at(None, ping).unwrap();
    assert!(niri.worker.is_none());
    assert!(niri.send(NiriAction::FocusWindow(1)).is_err());
}

#[test]
fn partial_initial_state_times_out_and_shutdown_interrupts_reconnect_wait() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("niri.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let events = accept(&listener);
        let mut reader = BufReader::new(&events);
        assert_eq!(read(&mut reader), json!("EventStream"));
        send(&events, json!({"Ok":"Handled"}));
        send(&events, workspaces(1));
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
    });
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let niri = Niri::start_at(Some(path), ping).unwrap();
    let snapshot = wait_for(&niri, |s| !s.available && s.error.contains("timed out"));
    assert!(snapshot.state.workspaces.is_empty());
    let start = Instant::now();
    drop(niri);
    assert!(start.elapsed() < Duration::from_secs(1));
    server.join().unwrap();
}

#[test]
#[ignore = "requires a running niri session; reads compositor state only"]
fn reads_live_compositor_state_without_sending_actions() {
    assert!(std::env::var_os("NIRI_SOCKET").is_some());
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let niri = Niri::start(ping).unwrap();
    let snapshot = wait_for(&niri, |s| s.available && !s.state.workspaces.is_empty());
    assert!(snapshot.error.is_empty());
    assert!(
        snapshot
            .state
            .workspaces
            .iter()
            .filter(|w| w.focused)
            .count()
            <= 1
    );
    eprintln!(
        "Received {} workspaces and {} windows",
        snapshot.state.workspaces.len(),
        snapshot.state.windows.len()
    );
}

#[test]
fn optional_overview_and_keyboard_events_project_without_blocking_older_streams() {
    let mut model = Model::default();
    apply(&mut model, workspaces(1));
    apply(&mut model, json!({"WindowsChanged":{"windows":[]}}));
    assert!(model.is_initialized());
    assert_eq!(model.snapshot().overview_open, None);
    assert_eq!(model.snapshot().keyboard_layouts, None);
    apply(
        &mut model,
        json!({"OverviewOpenedOrClosed":{"is_open":true}}),
    );
    apply(
        &mut model,
        json!({"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["English","Hebrew"],"current_idx":0}}}),
    );
    apply(&mut model, json!({"KeyboardLayoutSwitched":{"idx":1}}));
    assert_eq!(model.snapshot().overview_open, Some(true));
    assert_eq!(model.snapshot().keyboard_layouts.unwrap().current_index, 1);
    let before = model.snapshot();
    for event in [
        json!({"KeyboardLayoutSwitched":{"idx":2}}),
        json!({"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["English"],"current_idx":1}}}),
        json!({"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["bad\0name"],"current_idx":0}}}),
    ] {
        assert!(model.apply(&serde_json::to_vec(&event).unwrap()).is_err());
        assert_eq!(model.snapshot(), before);
    }
    apply(
        &mut model,
        json!({"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":[],"current_idx":0}}}),
    );
    assert!(model.snapshot().keyboard_layouts.unwrap().names.is_empty());
}

#[test]
fn overview_and_layout_requests_match_the_compositor_protocol() {
    use thing_runtime::NiriLayoutTarget;
    for (action, expected) in [
        (NiriAction::SetOverview(true), json!({"OpenOverview":{}})),
        (NiriAction::SetOverview(false), json!({"CloseOverview":{}})),
        (NiriAction::ToggleOverview, json!({"ToggleOverview":{}})),
        (
            NiriAction::SwitchLayout(NiriLayoutTarget::Next),
            json!({"SwitchLayout":{"layout":"Next"}}),
        ),
        (
            NiriAction::SwitchLayout(NiriLayoutTarget::Previous),
            json!({"SwitchLayout":{"layout":"Prev"}}),
        ),
        (
            NiriAction::SwitchLayout(NiriLayoutTarget::Index(255)),
            json!({"SwitchLayout":{"layout":{"Index":255}}}),
        ),
    ] {
        assert_eq!(model::request(action), json!({"Action":expected}));
    }
}

#[test]
fn keyboard_and_overview_state_clear_on_disconnect_and_can_reconnect_without_new_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("niri.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (disconnect, disconnect_rx) = mpsc::channel();
    let (reconnect, reconnect_rx) = mpsc::channel();
    let (done, finish) = mpsc::channel();
    let server = thread::spawn(move || {
        let events = accept(&listener);
        assert_eq!(read(&mut BufReader::new(&events)), json!("EventStream"));
        send(&events, json!({"Ok":"Handled"}));
        initial_state(&events, 1);
        send(&events, json!({"OverviewOpenedOrClosed":{"is_open":true}}));
        send(
            &events,
            json!({"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["English","Hebrew"],"current_idx":1}}}),
        );
        let commands = accept(&listener);
        assert_eq!(
            read(&mut BufReader::new(&commands)),
            json!({"Action":{"SwitchLayout":{"layout":{"Index":0}}}})
        );
        send(&commands, json!({"Ok":"Handled"}));
        send(&events, json!({"KeyboardLayoutSwitched":{"idx":0}}));
        disconnect_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(commands);
        drop(events);
        reconnect_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let events = accept(&listener);
        assert_eq!(read(&mut BufReader::new(&events)), json!("EventStream"));
        send(&events, json!({"Ok":"Handled"}));
        initial_state(&events, 2);
        finish.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let niri = Niri::start_at(Some(path), ping).unwrap();
    wait_for(&niri, |s| {
        s.available
            && s.state.overview_open == Some(true)
            && s.state
                .keyboard_layouts
                .as_ref()
                .is_some_and(|l| l.current_index == 1)
    });
    niri.send(NiriAction::SwitchLayout(
        thing_runtime::NiriLayoutTarget::Index(0),
    ))
    .unwrap();
    wait_for(&niri, |s| {
        s.state
            .keyboard_layouts
            .as_ref()
            .is_some_and(|l| l.current_index == 0)
    });
    disconnect.send(()).unwrap();
    let offline = wait_for(&niri, |s| !s.available);
    assert_eq!(offline.state.overview_open, None);
    assert_eq!(offline.state.keyboard_layouts, None);
    assert!(niri.send(NiriAction::ToggleOverview).is_err());
    reconnect.send(()).unwrap();
    let restored = wait_for(&niri, |s| {
        s.available && s.state.workspaces.first().is_some_and(|w| w.id == 2)
    });
    assert_eq!(restored.state.keyboard_layouts, None);
    assert_eq!(restored.state.overview_open, None);
    done.send(()).unwrap();
    drop(niri);
    server.join().unwrap();
}

fn cast(id: u64) -> Value {
    json!({"stream_id":id,"session_id":u64::MAX,"kind":"PipeWire",
        "target":{"Window":{"id":u64::MAX}},"is_dynamic_target":true,
        "is_active":true,"pid":null,"pw_node_id":42})
}

#[test]
fn cast_snapshots_and_deltas_keep_paused_and_unknown_streams() {
    use thing_runtime::{NiriCastKind, NiriCastTarget};
    let mut model = Model::default();
    apply(&mut model, workspaces(1));
    apply(&mut model, json!({"WindowsChanged":{"windows":[]}}));
    assert!(model.is_initialized());
    assert_eq!(model.snapshot().casts, None);
    apply(&mut model, json!({"CastStartedOrChanged":{"cast":cast(7)}}));
    assert_eq!(model.snapshot().casts, None);
    apply(
        &mut model,
        json!({"CastsChanged":{"casts":[cast(u64::MAX),cast(1)]}}),
    );
    let casts = model.snapshot().casts.unwrap();
    assert_eq!(
        casts.iter().map(|c| c.stream_id).collect::<Vec<_>>(),
        vec![1, u64::MAX]
    );
    assert_eq!(casts[0].session_id, u64::MAX);
    assert_eq!(casts[0].target, NiriCastTarget::Window(u64::MAX));
    assert_eq!(casts[0].pipewire_node_id, Some(42));
    let mut paused = cast(1);
    paused["kind"] = json!("WlrScreencopy");
    paused["target"] = json!({"Output":{"name":"DP-1"}});
    paused["is_active"] = json!(false);
    paused["pid"] = json!(1234);
    paused["pw_node_id"] = Value::Null;
    apply(&mut model, json!({"CastStartedOrChanged":{"cast":paused}}));
    let stream = model.snapshot().casts.unwrap().remove(0);
    assert!(!stream.active);
    assert_eq!(stream.kind, NiriCastKind::WlrScreencopy);
    assert_eq!(stream.target, NiriCastTarget::Output("DP-1".into()));
    assert_eq!(stream.pid, Some(1234));
    assert_eq!(stream.pipewire_node_id, None);
    let mut future = cast(2);
    future["kind"] = json!("FutureProtocol");
    future["target"] = json!({"Region":{"future_data":[1,2]}});
    apply(&mut model, json!({"CastStartedOrChanged":{"cast":future}}));
    let stream = model.snapshot().casts.unwrap().remove(1);
    assert!(stream.active);
    assert_eq!(stream.kind, NiriCastKind::Unknown("FutureProtocol".into()));
    assert_eq!(stream.target, NiriCastTarget::Unknown("Region".into()));
    let mut no_target = cast(1);
    no_target["target"] = json!({"Nothing":{}});
    apply(
        &mut model,
        json!({"CastStartedOrChanged":{"cast":no_target}}),
    );
    assert_eq!(
        model.snapshot().casts.unwrap()[0].target,
        NiriCastTarget::Nothing
    );
    apply(&mut model, json!({"CastStopped":{"stream_id":u64::MAX}}));
    apply(&mut model, json!({"CastStopped":{"stream_id":123}}));
    assert_eq!(model.snapshot().casts.unwrap().len(), 2);
    apply(&mut model, json!({"CastsChanged":{"casts":[]}}));
    assert_eq!(model.snapshot().casts, Some(vec![]));
}

#[test]
fn malformed_cast_updates_leave_the_previous_snapshot_intact() {
    let mut model = Model::default();
    apply(&mut model, json!({"CastsChanged":{"casts":[cast(1)]}}));
    let before = model.snapshot();
    let mut malformed = cast(2);
    malformed["target"] = json!({"Output":{"name":"bad\0output"}});
    let mut malformed_target = cast(3);
    malformed_target["target"] = json!({"Window":{"id":"not an id"}});
    for event in [
        json!({"CastsChanged":{"casts":[cast(2),malformed.clone()]}}),
        json!({"CastsChanged":{"casts":[cast(2),cast(2)]}}),
        json!({"CastStartedOrChanged":{"cast":malformed}}),
        json!({"CastStartedOrChanged":{"cast":malformed_target}}),
    ] {
        assert!(model.apply(&serde_json::to_vec(&event).unwrap()).is_err());
        assert_eq!(model.snapshot(), before);
    }
    apply(
        &mut model,
        json!({"CastsChanged":{"casts":(0..10000).map(cast).collect::<Vec<_>>()}}),
    );
    assert!(
        model
            .apply(
                &serde_json::to_vec(&json!({"CastStartedOrChanged":{"cast":cast(10000)}})).unwrap()
            )
            .is_err()
    );
    assert_eq!(model.snapshot().casts.unwrap().len(), 10000);
}

#[test]
fn casts_and_moves_use_the_worker_and_clear_on_disconnect() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("niri.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (disconnect, disconnect_rx) = mpsc::channel();
    let (reconnect, reconnect_rx) = mpsc::channel();
    let (done, finish) = mpsc::channel();
    let server = thread::spawn(move || {
        let events = accept(&listener);
        assert_eq!(read(&mut BufReader::new(&events)), json!("EventStream"));
        send(&events, json!({"Ok":"Handled"}));
        initial_state(&events, 1);
        send(&events, json!({"CastsChanged":{"casts":[cast(u64::MAX)]}}));
        let commands = accept(&listener);
        let mut reader = BufReader::new(&commands);
        for expected in [
            json!({"MoveWindowToWorkspace":{"window_id":u64::MAX,"reference":{"Id":2},"focus":false}}),
            json!({"MoveWindowToMonitor":{"id":u64::MAX,"output":"DP-2"}}),
            json!({"MoveWorkspaceToMonitor":{"reference":{"Id":u64::MAX},"output":"DP-2"}}),
        ] {
            assert_eq!(read(&mut reader), json!({"Action":expected}));
            send(&commands, json!({"Ok":"Handled"}));
        }
        let mut paused = cast(u64::MAX);
        paused["is_active"] = json!(false);
        send(&events, json!({"CastStartedOrChanged":{"cast":paused}}));
        disconnect_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(reader);
        drop(commands);
        drop(events);
        reconnect_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let events = accept(&listener);
        assert_eq!(read(&mut BufReader::new(&events)), json!("EventStream"));
        send(&events, json!({"Ok":"Handled"}));
        initial_state(&events, 2);
        finish.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let niri = Niri::start_at(Some(path), ping).unwrap();
    wait_for(&niri, |s| {
        s.available
            && s.state
                .casts
                .as_ref()
                .is_some_and(|c| c.len() == 1 && c[0].active)
    });
    niri.send(NiriAction::MoveWindowToWorkspace {
        window_id: u64::MAX,
        workspace_id: 2,
        focus: false,
    })
    .unwrap();
    niri.send(NiriAction::MoveWindowToOutput {
        window_id: u64::MAX,
        output: "DP-2".into(),
    })
    .unwrap();
    niri.send(NiriAction::MoveWorkspaceToOutput {
        workspace_id: u64::MAX,
        output: "DP-2".into(),
    })
    .unwrap();
    wait_for(&niri, |s| {
        s.state
            .casts
            .as_ref()
            .is_some_and(|c| c.len() == 1 && !c[0].active)
    });
    disconnect.send(()).unwrap();
    let offline = wait_for(&niri, |s| !s.available);
    assert_eq!(offline.state.casts, None);
    assert!(
        niri.send(NiriAction::MoveWindowToOutput {
            window_id: 1,
            output: "DP-1".into()
        })
        .is_err()
    );
    reconnect.send(()).unwrap();
    let restored = wait_for(&niri, |s| {
        s.available && s.state.workspaces.first().is_some_and(|w| w.id == 2)
    });
    assert_eq!(restored.state.casts, None);
    done.send(()).unwrap();
    drop(niri);
    server.join().unwrap();
}
