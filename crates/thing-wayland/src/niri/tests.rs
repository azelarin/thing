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
        send(&events, workspaces(u64::MAX));
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
        send(&events, workspaces(1));
        command.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(events);
        let events = accept(&listener);
        read(&mut BufReader::new(&events));
        send(&events, json!({"Ok":"Handled"}));
        send(&events, workspaces(2));
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
fn stalled_subscription_times_out_and_shutdown_interrupts_reconnect_wait() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("niri.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let events = accept(&listener);
        let mut reader = BufReader::new(&events);
        assert_eq!(read(&mut reader), json!("EventStream"));
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
