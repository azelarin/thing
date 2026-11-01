//! private-socket tests: real runtimes behind the real listener and client.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener, net::UnixStream},
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::Duration,
};

use calloop::EventLoop;
use tempfile::TempDir;
use thing_core::Scale;
use thing_runtime::{CallValue, OutputContext, ServiceSnapshot};

use super::*;
use crate::configuration::{Configurations, DispatchOutcome};

const SOURCE: &str = r#"
    local count = ui.state(0)
    local label = ui.state('ready')
    local later = ui.timer { interval = 1000, on_timeout = function() end }
    ui.command('increment', function(arguments)
        count:set(count:get() + (if arguments and arguments.by then arguments.by else 1))
        return { output = services.output.name:get(), count = count:get() }
    end)
    ui.command('fail', function()
        services.media.next()
        later:start()
        error('the launcher is not ready')
    end)
    ui.command('reject', function() label:set('rejected') return true end)
    ui.command('ready', function() label:set('ready') end)
    ui.command('large', function()
        local items = {}
        for index = 1, 15 do items[index] = string.rep('x', 16000) end
        return items
    end)
    ui.command('huge', function()
        local items = {}
        for index = 1, 20 do items[index] = string.rep('x', 16000) end
        return items
    end)
    return ui.panel { children = {
        ui.text { text = function() return `{count:get()} {label:get()}` end },
    } }
"#;

struct Host {
    configurations: Configurations<u32>,
    outcomes: Vec<DispatchOutcome>,
}

impl CommandHost for Host {
    // the same selection and checked call the app uses, with a validator
    // standing in for layout preflight.
    fn run_command(&mut self, request: CommandRequest) -> CommandReply {
        let outputs = match self
            .configurations
            .command_targets(&request.selection, &request.command)
        {
            Ok(outputs) => outputs,
            Err(error) => return CommandReply::Failed(error),
        };
        let mut results = Vec::new();
        for key in outputs {
            let output = self
                .configurations
                .output(&key)
                .and_then(|context| context.name.clone());
            let result = self
                .configurations
                .call_command_checked(&key, &request.command, &request.arguments, |surfaces| {
                    if surfaces[0].root.children[0].text.ends_with("rejected") {
                        Err("layout rejected the candidate".into())
                    } else {
                        Ok(())
                    }
                })
                .and_then(|result| result.ok_or_else(|| "the command is gone".to_owned()))
                .map(|(value, outcome)| {
                    self.outcomes.push(outcome);
                    value
                });
            results.push(OutputResult { output, result });
        }
        CommandReply::Ran(results)
    }
}

fn output(name: &str) -> OutputContext {
    OutputContext {
        name: Some(name.into()),
        width: 1920,
        height: 1080,
        scale: Scale::default(),
    }
}

// fields drop in order: the server closes before its loop and directory.
struct Fixture {
    server: Option<CommandServer<Host>>,
    event_loop: EventLoop<'static, Host>,
    host: Host,
    entry: PathBuf,
    // the runtime directory: private to this test, mode 0700.
    directory: TempDir,
}

impl Fixture {
    fn new(outputs: &[&str]) -> Self {
        Self::with_timeout(outputs, Duration::from_secs(5))
    }

    fn with_timeout(outputs: &[&str], timeout: Duration) -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(&entry, SOURCE).unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        for (key, name) in outputs.iter().enumerate() {
            configurations.add(key as u32, output(name)).unwrap();
        }
        let event_loop = EventLoop::try_new().unwrap();
        let server = CommandServer::start_with_timeout(
            &event_loop.handle(),
            directory.path(),
            "test",
            timeout,
        )
        .unwrap();
        Self {
            directory,
            entry,
            event_loop,
            host: Host {
                configurations,
                outcomes: Vec::new(),
            },
            server: Some(server),
        }
    }

    fn socket(&self) -> PathBuf {
        socket_path(self.directory.path(), "test")
    }

    /// serves the loop until the client thread finishes. the thread's exit is
    /// the witness; the timeout only bounds each idle wait.
    fn serve<T: Send + 'static>(&mut self, client: impl FnOnce() -> T + Send + 'static) -> T {
        let thread = thread::spawn(client);
        self.serve_until(|_| thread.is_finished());
        thread.join().unwrap()
    }

    fn serve_until(&mut self, mut done: impl FnMut(&Self) -> bool) {
        while !done(self) {
            self.event_loop
                .dispatch(Some(Duration::from_millis(10)), &mut self.host)
                .unwrap();
        }
    }

    fn call(
        &mut self,
        command: &str,
        arguments: CallValue,
        selection: OutputSelection,
    ) -> Result<String, (u8, String)> {
        let directory = self.directory.path().to_owned();
        let options = CallOptions {
            instance: "test".into(),
            selection,
            command: command.into(),
            arguments,
            timeout: Duration::from_secs(10),
        };
        self.serve(move || call_at(&directory, &options))
    }

    fn text(&self, key: u32) -> String {
        self.host.configurations.root(&key).unwrap().children[0]
            .text
            .clone()
    }
}

fn by(amount: f64) -> CallValue {
    CallValue::Map(BTreeMap::from([(
        "by".to_owned(),
        CallValue::Number(amount),
    )]))
}

/// a raw client: sends `bytes` and returns everything the server replies.
fn raw(socket: PathBuf, bytes: Vec<u8>) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.write_all(&bytes).unwrap();
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    })
}

#[test]
fn a_single_output_instance_runs_commands_by_name() {
    let mut fixture = Fixture::new(&["DP-1"]);
    let printed = fixture
        .call("increment", by(2.0), OutputSelection::Implicit)
        .unwrap();
    assert_eq!(printed, r#"{"count":2,"output":"DP-1"}"#);
    assert_eq!(fixture.text(0), "2 ready");
    let printed = fixture
        .call(
            "increment",
            CallValue::Nil,
            OutputSelection::Named("DP-1".into()),
        )
        .unwrap();
    assert_eq!(printed, r#"{"count":3,"output":"DP-1"}"#);

    let (code, error) = fixture
        .call(
            "increment",
            CallValue::Nil,
            OutputSelection::Named("HDMI-A-1".into()),
        )
        .unwrap_err();
    assert_eq!(code, 1);
    assert!(
        error.contains("no output named \"HDMI-A-1\"; this instance runs on DP-1"),
        "{error}"
    );
    let (_, error) = fixture
        .call("missing", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert!(error.contains("has no command \"missing\""), "{error}");
    assert!(
        error.contains("fail, huge, increment, large, ready, reject"),
        "{error}"
    );
    assert_eq!(fixture.text(0), "3 ready");
}

#[test]
fn several_outputs_need_an_explicit_selection() {
    let mut fixture = Fixture::new(&["DP-1", "DP-2"]);
    let (_, error) = fixture
        .call("increment", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert!(
        error.contains("runs on 2 outputs (DP-1, DP-2); pass --output <name> or --all-outputs"),
        "{error}"
    );
    assert_eq!(
        (fixture.text(0), fixture.text(1)),
        ("0 ready".into(), "0 ready".into())
    );

    let printed = fixture
        .call(
            "increment",
            CallValue::Nil,
            OutputSelection::Named("DP-2".into()),
        )
        .unwrap();
    assert_eq!(printed, r#"{"count":1,"output":"DP-2"}"#);
    assert_eq!(
        (fixture.text(0), fixture.text(1)),
        ("0 ready".into(), "1 ready".into())
    );

    let printed = fixture
        .call("increment", by(5.0), OutputSelection::All)
        .unwrap();
    assert_eq!(
        printed,
        r#"[{"output":"DP-1","value":{"count":5,"output":"DP-1"}},{"output":"DP-2","value":{"count":6,"output":"DP-2"}}]"#
    );
}

#[test]
fn failed_handlers_and_rejected_candidates_publish_nothing() {
    let mut fixture = Fixture::new(&["DP-1"]);
    let (code, error) = fixture
        .call("fail", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert_eq!(code, 1);
    assert!(error.starts_with("DP-1: "), "{error}");
    assert!(error.contains("the launcher is not ready"), "{error}");
    // the failed call queued a media action; it never reaches the host.
    assert!(fixture.host.outcomes.is_empty());

    let (_, error) = fixture
        .call("reject", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert!(error.contains("layout rejected the candidate"), "{error}");
    assert_eq!(fixture.text(0), "0 ready");
    // the handler's state write is not rolled back, so the next candidate is
    // still rejected until a call repairs the state.
    let (_, error) = fixture
        .call("increment", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert!(error.contains("layout rejected the candidate"), "{error}");
    assert_eq!(fixture.text(0), "0 ready");
    fixture
        .call("ready", CallValue::Nil, OutputSelection::Implicit)
        .unwrap();
    assert_eq!(fixture.text(0), "1 ready");

    // a successful call carries only its own actions.
    assert_eq!(fixture.host.outcomes.len(), 1);
    assert!(fixture.host.outcomes[0].actions.is_empty());
}

#[test]
fn malformed_oversized_and_unsupported_requests_get_errors() {
    let mut fixture = Fixture::new(&["DP-1"]);
    let mut deep = String::new();
    for _ in 0..20 {
        deep.push('[');
    }
    for _ in 0..20 {
        deep.push(']');
    }
    let cases = [
        (b"not json\n".to_vec(), "malformed request"),
        (b"[1, 2]\n".to_vec(), "must be a json object"),
        (
            br#"{"version":2,"command":"increment"}"#.to_vec(),
            "unsupported request version",
        ),
        (
            br#"{"version":1,"command":"increment","extra":1}"#.to_vec(),
            "unknown request field",
        ),
        (
            br#"{"version":1,"command":"has space"}"#.to_vec(),
            "must be 1 to 64",
        ),
        (
            br#"{"version":1,"command":"increment","output":"DP-1","all_outputs":true}"#.to_vec(),
            "either one output or all outputs",
        ),
        (
            format!(r#"{{"version":1,"command":"increment","arguments":{deep}}}"#).into_bytes(),
            "nest at most 16 levels",
        ),
        (
            vec![b' '; MAX_REQUEST_BYTES + 1],
            "the request exceeds 64 KiB",
        ),
        (b"\xff\xfe\n".to_vec(), "not utf-8"),
    ];
    for (request, message) in cases {
        let client = raw(fixture.socket(), request);
        fixture.serve_until(|_| client.is_finished());
        let reply = client.join().unwrap();
        assert!(reply.contains(message), "{message}: {reply}");
        assert!(
            reply.starts_with(r#"{"error":"#) && reply.ends_with("}\n"),
            "{reply}"
        );
    }
    let (_, error) = fixture
        .call("huge", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert!(error.contains("exceeds 256 KiB"), "{error}");
    // the client checks names and limits before connecting.
    let (code, error) = fixture
        .call("bad name", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert_eq!(code, 2, "{error}");
    let (code, _) = fixture
        .call(
            "increment",
            CallValue::List(vec![CallValue::Nil; 2000]),
            OutputSelection::Implicit,
        )
        .unwrap_err();
    assert_eq!(code, 2);
    assert_eq!(fixture.text(0), "0 ready");
}

#[test]
fn stalled_and_silent_clients_never_block_other_calls() {
    let mut fixture = Fixture::with_timeout(&["DP-1"], Duration::from_millis(300));
    // half a request, then nothing.
    let partial = {
        let socket = fixture.socket();
        thread::spawn(move || {
            let mut stream = UnixStream::connect(socket).unwrap();
            stream.write_all(br#"{"version":1,"comm"#).unwrap();
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).unwrap();
            rest
        })
    };
    // a complete request whose large reply is never read.
    let silent = UnixStream::connect(fixture.socket()).unwrap();
    (&silent)
        .write_all(&encode_request(
            "large",
            &CallValue::Nil,
            &OutputSelection::Implicit,
        ))
        .unwrap();
    fixture.serve_until(|fixture| fixture.server.as_ref().unwrap().clients() == 2);

    // both are pending, and a third client is served at once.
    let printed = fixture
        .call("increment", CallValue::Nil, OutputSelection::Implicit)
        .unwrap();
    assert_eq!(printed, r#"{"count":1,"output":"DP-1"}"#);

    // the stalled clients are dropped after their deadline.
    fixture.serve_until(|fixture| fixture.server.as_ref().unwrap().clients() == 0);
    fixture.serve_until(|_| partial.is_finished());
    assert!(partial.join().unwrap().is_empty());
    let mut received = Vec::new();
    (&silent).read_to_end(&mut received).unwrap();
    // whatever the kernel buffered arrives, then the connection ends.
    assert!(received.len() <= 15 * 16_000 + 1024);

    // a client that does read gets the whole large reply.
    let printed = fixture
        .call("large", CallValue::Nil, OutputSelection::Implicit)
        .unwrap();
    assert_eq!(printed.len(), 15 * 16_002 + 16);
}

#[test]
fn a_busy_instance_turns_away_extra_clients() {
    let mut fixture = Fixture::new(&["DP-1"]);
    let idle: Vec<_> = (0..MAX_CLIENTS)
        .map(|_| UnixStream::connect(fixture.socket()).unwrap())
        .collect();
    fixture.serve_until(|fixture| fixture.server.as_ref().unwrap().clients() == MAX_CLIENTS);
    let (_, error) = fixture
        .call("increment", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert!(error.contains("busy"), "{error}");
    drop(idle);
    fixture.serve_until(|fixture| fixture.server.as_ref().unwrap().clients() == 0);
    assert!(
        fixture
            .call("increment", CallValue::Nil, OutputSelection::Implicit)
            .is_ok()
    );
}

#[test]
fn reloads_and_output_removal_change_what_calls_reach() {
    let mut fixture = Fixture::new(&["DP-1", "DP-2"]);
    let outputs = vec![(0, output("DP-1")), (1, output("DP-2"))];
    // a rejected reload keeps the running commands.
    fs::write(
        &fixture.entry,
        "ui.command('next', function() end) return ui.panel {",
    )
    .unwrap();
    assert!(
        fixture
            .host
            .configurations
            .reload_for(outputs.clone())
            .is_err()
    );
    assert!(
        fixture
            .call(
                "increment",
                CallValue::Nil,
                OutputSelection::Named("DP-1".into())
            )
            .is_ok()
    );
    // an accepted reload replaces them; the old closures are gone.
    fs::write(
        &fixture.entry,
        "ui.command('next', function() return 'reloaded' end) return ui.panel {}",
    )
    .unwrap();
    fixture.host.configurations.reload_for(outputs).unwrap();
    let (_, error) = fixture
        .call(
            "increment",
            CallValue::Nil,
            OutputSelection::Named("DP-1".into()),
        )
        .unwrap_err();
    assert!(error.contains("it declares next"), "{error}");
    assert_eq!(
        fixture
            .call(
                "next",
                CallValue::Nil,
                OutputSelection::Named("DP-2".into())
            )
            .unwrap(),
        r#""reloaded""#
    );

    fixture.host.configurations.remove(&1);
    let (_, error) = fixture
        .call(
            "next",
            CallValue::Nil,
            OutputSelection::Named("DP-2".into()),
        )
        .unwrap_err();
    assert!(
        error.contains("no output named \"DP-2\"; this instance runs on DP-1"),
        "{error}"
    );
    // with one output left, an unnamed call is unambiguous again.
    assert_eq!(
        fixture
            .call("next", CallValue::Nil, OutputSelection::Implicit)
            .unwrap(),
        r#""reloaded""#
    );
    fixture.host.configurations.remove(&0);
    let (_, error) = fixture
        .call("next", CallValue::Nil, OutputSelection::Implicit)
        .unwrap_err();
    assert!(error.contains("no outputs yet"), "{error}");
}

#[test]
fn shutdown_closes_clients_and_removes_only_its_own_socket() {
    let mut fixture = Fixture::new(&["DP-1"]);
    let socket = fixture.socket();
    let pending = UnixStream::connect(&socket).unwrap();
    fixture.serve_until(|fixture| fixture.server.as_ref().unwrap().clients() == 1);
    fixture.server = None;
    let mut rest = Vec::new();
    (&pending).read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    assert!(!socket.exists());

    let directory = fixture.directory.path().to_owned();
    let options = CallOptions {
        instance: "test".into(),
        selection: OutputSelection::Implicit,
        command: "increment".into(),
        arguments: CallValue::Nil,
        timeout: Duration::from_secs(1),
    };
    let (code, error) = call_at(&directory, &options).unwrap_err();
    assert_eq!(code, 1);
    assert!(
        error.contains("no thing instance named \"test\" is running"),
        "{error}"
    );
}

fn bind(directory: &Path, instance: &str) -> Result<(Endpoint, UnixListener), String> {
    Endpoint::bind(directory, instance)
}

#[test]
fn client_timeout_covers_the_entire_reply_instead_of_each_read() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let (endpoint, listener) = bind(directory.path(), "slow").unwrap();
    let peer = thread::spawn(move || {
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1))
                }
                Err(error) => panic!("{error}"),
            }
        };
        let mut request = Vec::new();
        stream.read_to_end(&mut request).unwrap();
        // each chunk arrives inside the idle timeout, but the full reply takes
        // much longer than the caller's deadline.
        for byte in b"{\"results\":[{\"output\":\"DP-1\",\"value\":true}]}\n" {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let started = std::time::Instant::now();
    let result = call_at(
        directory.path(),
        &CallOptions {
            instance: "slow".into(),
            command: "echo".into(),
            arguments: CallValue::Nil,
            selection: OutputSelection::Implicit,
            timeout: Duration::from_millis(150),
        },
    );
    let elapsed = started.elapsed();
    peer.join().unwrap();
    drop(endpoint);
    assert!(
        result.is_err(),
        "a trickled reply escaped the call deadline"
    );
    assert!(
        elapsed < Duration::from_millis(600),
        "call took {elapsed:?}"
    );
}

#[test]
fn endpoints_own_their_instance_name() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = socket_path(root, "panel");

    let first = bind(root, "panel").unwrap();
    let error = bind(root, "panel").err().unwrap();
    assert!(
        error.contains("instance \"panel\" is already running"),
        "{error}"
    );
    // other names are independent.
    let other = bind(root, "dock").unwrap();
    drop(first);
    assert!(!socket.exists());
    assert!(socket_path(root, "dock").exists());
    drop(other);

    // a stale socket from an instance that died without cleaning up. later
    // steps use fresh names: a child another test thread forks may hold an
    // inherited lock descriptor until it execs.
    let socket = socket_path(root, "stale");
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());
    let (endpoint, _listener) = bind(root, "stale").unwrap();
    let options = CallOptions {
        instance: "stale".into(),
        selection: OutputSelection::Implicit,
        command: "increment".into(),
        arguments: CallValue::Nil,
        timeout: Duration::from_secs(1),
    };
    // the path belongs to someone else now: drop must leave it alone.
    fs::remove_file(&socket).unwrap();
    let replacement = UnixListener::bind(&socket).unwrap();
    drop(endpoint);
    assert!(socket.exists());
    drop(replacement);
    let (code, error) = call_at(root, &options).unwrap_err();
    assert_eq!(code, 1);
    assert!(error.contains("stale socket"), "{error}");
    fs::remove_file(&socket).unwrap();

    // only sockets are replaced.
    let socket = socket_path(root, "file");
    fs::write(&socket, "not a socket").unwrap();
    let error = bind(root, "file").err().unwrap();
    assert!(error.contains("refusing to replace"), "{error}");
    fs::remove_file(&socket).unwrap();

    // directories others can reach are refused, by both sides.
    fs::set_permissions(root.join("thing"), fs::Permissions::from_mode(0o755)).unwrap();
    let error = bind(root, "panel").err().unwrap();
    assert!(error.contains("closed to others"), "{error}");
    let (_, error) = call_at(root, &options).unwrap_err();
    assert!(
        error.contains("closed to others") || error.contains("no thing instance"),
        "{error}"
    );
    fs::set_permissions(root.join("thing"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(root, fs::Permissions::from_mode(0o711)).unwrap();
    let error = bind(root, "panel").err().unwrap();
    assert!(error.contains("closed to others"), "{error}");
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(bind(Path::new("relative"), "panel").is_err());
    assert!(
        bind(root, "../escape")
            .err()
            .unwrap()
            .contains("instance name")
    );
}

#[test]
fn json_arguments_and_results_round_trip_within_limits() {
    let value =
        parse_arguments(r#"{"tab":"apps","sizes":[1,2.5,-3],"open":true,"none":null}"#).unwrap();
    assert_eq!(
        value,
        CallValue::Map(BTreeMap::from([
            ("none".to_owned(), CallValue::Nil),
            ("open".to_owned(), CallValue::Boolean(true)),
            (
                "sizes".to_owned(),
                CallValue::List(vec![
                    CallValue::Number(1.0),
                    CallValue::Number(2.5),
                    CallValue::Number(-3.0),
                ])
            ),
            ("tab".to_owned(), CallValue::String("apps".into())),
        ]))
    );
    assert_eq!(
        serde_json::to_string(&call_to_json(&value)).unwrap(),
        r#"{"none":null,"open":true,"sizes":[1,2.5,-3],"tab":"apps"}"#
    );
    assert!(parse_arguments("{").is_err());
    assert!(parse_arguments(&format!("\"{}\"", "x".repeat(MAX_CALL_STRING_BYTES + 1))).is_err());
    let request = encode_request("increment", &value, &OutputSelection::Named("DP-1".into()));
    assert_eq!(
        parse_request(&request[..request.len() - 1]).unwrap(),
        CommandRequest {
            command: "increment".into(),
            arguments: value,
            selection: OutputSelection::Named("DP-1".into()),
        }
    );
}
