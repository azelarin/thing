//! commands reach a real app through its socket, layout preflight, and
//! dispatch, on a private compositor.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use thing_runtime::CallValue;

use super::wheel_tests::Host;
use crate::command::{CallOptions, CommandServer, OutputSelection, call_at};

const PANEL: &str = r#"
local width = ui.state(320)
local inhibit = ui.state(false)
ui.command('resize', function(arguments)
    width:set(arguments.width)
    return { width = width:get() }
end)
-- the private compositor offers no idle inhibitor, so layout preflight
-- rejects this candidate.
ui.command('inhibit', function() inhibit:set(true) end)
ui.command('broken', function() error('broken on purpose') end)
return ui.panel {
    style = { width = 640, height = 80 },
    idle_inhibit = function() return inhibit:get() end,
    children = { ui.row {
        key = 'bar',
        style = { width = function() return width:get() end, height = 20 },
    } },
}
"#;

fn spawn(
    runtime: &Path,
    command: &str,
    arguments: CallValue,
    selection: OutputSelection,
) -> JoinHandle<Result<String, (u8, String)>> {
    let directory = runtime.to_owned();
    let options = CallOptions {
        instance: "app".into(),
        selection,
        command: command.into(),
        arguments,
        timeout: Duration::from_secs(10),
    };
    thread::spawn(move || call_at(&directory, &options))
}

fn finish(
    host: &mut Host,
    client: JoinHandle<Result<String, (u8, String)>>,
) -> Result<String, (u8, String)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !client.is_finished() {
        assert!(Instant::now() < deadline, "the command never finished");
        host.turn();
    }
    client.join().unwrap()
}

fn bar_width(host: &Host) -> Option<f32> {
    let output = host.app.configurations.keys().next()?.clone();
    host.app.configurations.root(&output)?.children[0]
        .style
        .width
}

#[test]
fn commands_run_through_the_app_preflight_and_dispatch() {
    let mut host = Host::start_with(|_| PANEL.to_owned());
    // a private stand-in for XDG_RUNTIME_DIR.
    let runtime = tempfile::tempdir().unwrap();
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = runtime.path();
    let server = CommandServer::start(&host.event_loop.handle(), runtime, "app").unwrap();
    host.app.commands = Some(server);
    let socket = crate::command::socket_path(runtime, "app");
    assert!(socket.exists());

    let arguments = crate::command::parse_arguments(r#"{"width": 480}"#).unwrap();
    let client = spawn(runtime, "resize", arguments, OutputSelection::Implicit);
    assert_eq!(finish(&mut host, client).unwrap(), r#"{"width":480}"#);
    assert_eq!(bar_width(&host), Some(480.0));
    // the accepted candidate reaches the compositor as a new frame.
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.app.surfaces[0].layouts.presented().and_then(|layout| {
        let output = host.app.configurations.keys().next()?.clone();
        let bar = host.app.configurations.root(&output)?.children[0].id;
        layout.get(bar).map(|placement| placement.bounds.width)
    }) != Some(480.0)
    {
        assert!(
            Instant::now() < deadline,
            "the resized bar was never presented"
        );
        host.turn();
    }

    let client = spawn(
        runtime,
        "inhibit",
        CallValue::Nil,
        OutputSelection::Implicit,
    );
    let (code, error) = finish(&mut host, client).unwrap_err();
    assert_eq!(code, 1);
    assert!(error.contains("idle"), "{error}");
    let output = host.app.configurations.keys().next().unwrap().clone();
    assert!(!host.app.configurations.root(&output).unwrap().idle_inhibit);

    let client = spawn(runtime, "broken", CallValue::Nil, OutputSelection::Implicit);
    let (_, error) = finish(&mut host, client).unwrap_err();
    assert!(error.contains("broken on purpose"), "{error}");

    let client = spawn(
        runtime,
        "resize",
        CallValue::Nil,
        OutputSelection::Named("nowhere".into()),
    );
    let (_, error) = finish(&mut host, client).unwrap_err();
    assert!(error.contains("no output named \"nowhere\""), "{error}");
    assert_eq!(bar_width(&host), Some(480.0));

    // shutting the app's server down removes the socket.
    host.app.commands = None;
    assert!(!socket.exists());
}
