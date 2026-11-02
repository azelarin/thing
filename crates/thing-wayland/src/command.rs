//! the local command socket: the host's listener and `thing call` clients.
//!
//! each `thing run` instance owns `$XDG_RUNTIME_DIR/thing/<instance>.sock`. a
//! client connects, writes one json request line, and reads one json reply.
//! the listener never blocks the ui: clients are nonblocking, bounded in
//! number, bytes, and time, and each connection carries exactly one request
//! and its reply.
//!
//! ownership: an exclusive `flock` on `<instance>.lock` names the live owner.
//! only the lock holder replaces a stale socket, and on shutdown it unlinks
//! the socket only while the path is still the socket it bound.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use calloop::{
    Interest, LoopHandle, Mode, PostAction, RegistrationToken,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use serde_json::{Map, Number, Value as Json};
use socket2::{Domain, SockAddr, Socket, Type};
use thing_runtime::{
    CallValue, MAX_CALL_DEPTH, MAX_CALL_STRING_BYTES, MAX_CALL_VALUES, validate_command_name,
};

pub use crate::inspection::InspectQuery;
use crate::inspection::{InspectReply, InspectRequest};

/// the instance `thing run` and `thing call` use without `--instance`.
pub const DEFAULT_INSTANCE: &str = "default";
/// the wire protocol version in every request.
pub const PROTOCOL_VERSION: u64 = 1;
/// the largest request line, including its newline.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// the largest reply. a larger result is replaced by an error.
pub const MAX_REPLY_BYTES: usize = 256 * 1024;
/// connections served at once. later clients get a busy reply.
pub const MAX_CLIENTS: usize = 8;
/// how long one connection may take to send its request and read the reply.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// an instance name: 1 to 32 ascii letters, digits, `-`, or `_`.
pub fn validate_instance_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "instance name {name:?} must be 1 to 32 letters, digits, '-', or '_'"
        ));
    }
    Ok(())
}

/// the user's runtime directory from `XDG_RUNTIME_DIR`.
pub fn runtime_directory() -> Result<PathBuf, String> {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .ok_or("XDG_RUNTIME_DIR is not set, so there is no private place for command sockets")?;
    Ok(PathBuf::from(directory))
}

/// the socket an instance listens on below `runtime_directory`.
pub fn socket_path(runtime_directory: &Path, instance: &str) -> PathBuf {
    runtime_directory
        .join("thing")
        .join(format!("{instance}.sock"))
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// checks that `path` is a real directory only `uid` can enter or change.
fn check_private_directory(path: &Path, uid: u32) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be owned by this user and closed to others (mode 0700)",
            path.display()
        ));
    }
    Ok(())
}

fn command_directory(runtime_directory: &Path, uid: u32, create: bool) -> Result<PathBuf, String> {
    if !runtime_directory.is_absolute() {
        return Err("XDG_RUNTIME_DIR must be an absolute path".into());
    }
    check_private_directory(runtime_directory, uid)?;
    let directory = runtime_directory.join("thing");
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!("cannot create {}: {error}", directory.display()));
            }
        }
    }
    check_private_directory(&directory, uid)?;
    Ok(directory)
}

fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the descriptor is a live socket borrowed from `stream`, and
    // `credentials` and `length` describe a writable ucred of exactly that size.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::other("peer credentials were truncated"));
    }
    Ok(credentials.uid)
}

/// the claim on one instance name: its lock and the socket it bound.
pub(crate) struct Endpoint {
    socket: PathBuf,
    identity: (u64, u64),
    // dropped after `drop` runs, so the unlink happens while still owning the name.
    _lock: File,
}

impl Endpoint {
    pub(crate) fn bind(
        runtime_directory: &Path,
        instance: &str,
    ) -> Result<(Self, UnixListener), String> {
        validate_instance_name(instance)?;
        let uid = current_uid();
        let directory = command_directory(runtime_directory, uid, true)?;
        let lock_path = directory.join(format!("{instance}.lock"));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
            .map_err(|error| format!("cannot open {}: {error}", lock_path.display()))?;
        let metadata = lock
            .metadata()
            .map_err(|error| format!("cannot inspect {}: {error}", lock_path.display()))?;
        if !metadata.file_type().is_file() || metadata.uid() != uid {
            return Err(format!(
                "{} is not a lock file owned by this user",
                lock_path.display()
            ));
        }
        // SAFETY: the descriptor belongs to `lock`, which outlives the call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return Err(if error.kind() == ErrorKind::WouldBlock {
                format!(
                    "instance {instance:?} is already running; pass --instance to start another"
                )
            } else {
                format!("cannot lock {}: {error}", lock_path.display())
            });
        }
        let socket = socket_path(runtime_directory, instance);
        // holding the lock means no live instance owns this path, so a socket
        // left here is stale. anything else is not ours to remove.
        match fs::symlink_metadata(&socket) {
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
                fs::remove_file(&socket).map_err(|error| {
                    format!("cannot remove stale socket {}: {error}", socket.display())
                })?;
            }
            Ok(_) => {
                return Err(format!(
                    "refusing to replace {}: it is not a socket owned by this user",
                    socket.display()
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot inspect {}: {error}", socket.display())),
        }
        let listener = UnixListener::bind(&socket)
            .map_err(|error| format!("cannot listen on {}: {error}", socket.display()))?;
        let bound = (|| {
            fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let metadata = fs::symlink_metadata(&socket)?;
            Ok::<_, io::Error>((metadata.dev(), metadata.ino()))
        })();
        let identity = match bound {
            Ok(identity) => identity,
            Err(error) => {
                let _ = fs::remove_file(&socket);
                return Err(format!("cannot prepare {}: {error}", socket.display()));
            }
        };
        Ok((
            Self {
                socket,
                identity,
                _lock: lock,
            },
            listener,
        ))
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.socket)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

/// which outputs a request names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputSelection {
    /// the only output; an error when the instance runs on several.
    Implicit,
    /// the output with this name.
    Named(String),
    /// every output, in the order they were added.
    All,
}

/// one parsed request.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommandRequest {
    pub(crate) command: String,
    pub(crate) arguments: CallValue,
    pub(crate) selection: OutputSelection,
}

/// what one output's handler returned.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OutputResult {
    pub(crate) output: Option<String>,
    pub(crate) result: Result<CallValue, String>,
}

/// the reply to one request: a selection error, or each output's result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CommandReply {
    Failed(String),
    Ran(Vec<OutputResult>),
}

/// one request line: a declared command, or a live layout inspection.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HostRequest {
    Command(CommandRequest),
    Inspect(InspectRequest),
}

/// what the event loop's data does with a parsed request.
pub(crate) trait CommandHost: 'static {
    fn run_command(&mut self, request: CommandRequest) -> CommandReply;

    /// inspection reads presented frames and never runs configuration code.
    fn inspect(&mut self, _request: InspectRequest) -> InspectReply {
        InspectReply::Failed(INSPECTOR_DISABLED.into())
    }
}

/// the reply when an instance was started without `--inspector`.
pub(crate) const INSPECTOR_DISABLED: &str =
    "live inspection is disabled; start the instance with 'thing run --inspector'";

fn json_to_call(value: Json, depth: usize, count: &mut usize) -> Result<CallValue, String> {
    *count += 1;
    if *count > MAX_CALL_VALUES {
        return Err(format!(
            "arguments may contain at most {MAX_CALL_VALUES} items"
        ));
    }
    let nested = |depth: usize| {
        if depth >= MAX_CALL_DEPTH {
            Err(format!(
                "arguments may nest at most {MAX_CALL_DEPTH} levels"
            ))
        } else {
            Ok(())
        }
    };
    let text = |text: String| {
        if text.len() > MAX_CALL_STRING_BYTES {
            Err(format!(
                "argument strings may be at most {MAX_CALL_STRING_BYTES} bytes"
            ))
        } else {
            Ok(text)
        }
    };
    Ok(match value {
        Json::Null => CallValue::Nil,
        Json::Bool(value) => CallValue::Boolean(value),
        Json::Number(number) => CallValue::Number(
            number
                .as_f64()
                .filter(|number| number.is_finite())
                .ok_or("argument numbers must be finite")?,
        ),
        Json::String(value) => CallValue::String(text(value)?),
        Json::Array(items) => {
            nested(depth)?;
            CallValue::List(
                items
                    .into_iter()
                    .map(|item| json_to_call(item, depth + 1, count))
                    .collect::<Result<_, _>>()?,
            )
        }
        Json::Object(entries) => {
            nested(depth)?;
            let mut map = BTreeMap::new();
            for (key, item) in entries {
                *count += 1;
                let key = text(key)?;
                map.insert(key, json_to_call(item, depth + 1, count)?);
            }
            CallValue::Map(map)
        }
    })
}

/// parses a json argument, applying the same limits as the host.
pub fn parse_arguments(text: &str) -> Result<CallValue, String> {
    let value: Json = serde_json::from_str(text)
        .map_err(|error| format!("arguments are not valid json: {error}"))?;
    json_to_call(value, 0, &mut 0)
}

fn call_to_json(value: &CallValue) -> Json {
    match value {
        CallValue::Nil => Json::Null,
        CallValue::Boolean(value) => Json::Bool(*value),
        CallValue::Number(value) => {
            if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
                Json::Number(Number::from(*value as i64))
            } else {
                Number::from_f64(*value).map_or(Json::Null, Json::Number)
            }
        }
        CallValue::String(value) => Json::String(value.clone()),
        CallValue::List(items) => Json::Array(items.iter().map(call_to_json).collect()),
        CallValue::Map(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, item)| (key.clone(), call_to_json(item)))
                .collect(),
        ),
    }
}

pub(crate) fn encode_request(
    command: &str,
    arguments: &CallValue,
    selection: &OutputSelection,
) -> Vec<u8> {
    let mut object = Map::new();
    object.insert("version".into(), Json::from(PROTOCOL_VERSION));
    object.insert("command".into(), Json::String(command.into()));
    object.insert("arguments".into(), call_to_json(arguments));
    match selection {
        OutputSelection::Implicit => {}
        OutputSelection::Named(name) => {
            object.insert("output".into(), Json::String(name.clone()));
        }
        OutputSelection::All => {
            object.insert("all_outputs".into(), Json::Bool(true));
        }
    }
    let mut bytes = serde_json::to_vec(&Json::Object(object)).expect("json values serialize");
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
pub(crate) fn parse_request(line: &[u8]) -> Result<CommandRequest, String> {
    match parse_host_request(line)? {
        HostRequest::Command(request) => Ok(request),
        HostRequest::Inspect(_) => Err("the request is an inspection".into()),
    }
}

pub(crate) fn parse_host_request(line: &[u8]) -> Result<HostRequest, String> {
    let text = std::str::from_utf8(line).map_err(|_| "the request is not utf-8")?;
    let Json::Object(mut object) =
        serde_json::from_str(text).map_err(|error| format!("malformed request: {error}"))?
    else {
        return Err("the request must be a json object".into());
    };
    let inspect = object.contains_key("inspect");
    if inspect && (object.contains_key("command") || object.contains_key("arguments")) {
        return Err("a request names either a command or an inspection, not both".into());
    }
    if let Some(key) = object.keys().find(|key| {
        !matches!(
            key.as_str(),
            "version" | "output" | "all_outputs" | "command" | "arguments" | "inspect"
        )
    }) {
        return Err(format!("unknown request field {key:?}"));
    }
    if object.remove("version") != Some(Json::from(PROTOCOL_VERSION)) {
        return Err(format!(
            "unsupported request version; this instance speaks version {PROTOCOL_VERSION}"
        ));
    }
    if let Some(query) = object.remove("inspect") {
        let selection = parse_selection(&mut object)?;
        let query = crate::inspection::parse_query(query, &selection)?;
        return Ok(HostRequest::Inspect(InspectRequest { selection, query }));
    }
    let Some(Json::String(command)) = object.remove("command") else {
        return Err("the request needs a command name".into());
    };
    validate_command_name(&command)?;
    let arguments = json_to_call(object.remove("arguments").unwrap_or(Json::Null), 0, &mut 0)?;
    let selection = parse_selection(&mut object)?;
    Ok(HostRequest::Command(CommandRequest {
        command,
        arguments,
        selection,
    }))
}

fn parse_selection(object: &mut Map<String, Json>) -> Result<OutputSelection, String> {
    let all = match object.remove("all_outputs") {
        None | Some(Json::Bool(false)) => false,
        Some(Json::Bool(true)) => true,
        Some(_) => return Err("all_outputs must be a boolean".into()),
    };
    let selection = match (object.remove("output"), all) {
        (None | Some(Json::Null), false) => OutputSelection::Implicit,
        (None | Some(Json::Null), true) => OutputSelection::All,
        (Some(Json::String(name)), false) => OutputSelection::Named(name),
        (Some(Json::String(_)), true) => {
            return Err("choose either one output or all outputs".into());
        }
        (Some(_), _) => return Err("output must be a string".into()),
    };
    Ok(selection)
}

pub(crate) fn encode_reply(reply: &CommandReply) -> Vec<u8> {
    let json = match reply {
        CommandReply::Failed(error) => serde_json::json!({ "ok": false, "error": error }),
        CommandReply::Ran(results) => {
            let ok = results.iter().all(|result| result.result.is_ok());
            let results: Vec<_> = results
                .iter()
                .map(|result| {
                    let mut object = Map::new();
                    object.insert(
                        "output".into(),
                        result.output.clone().map_or(Json::Null, Json::String),
                    );
                    match &result.result {
                        Ok(value) => object.insert("value".into(), call_to_json(value)),
                        Err(error) => object.insert("error".into(), Json::String(error.clone())),
                    };
                    Json::Object(object)
                })
                .collect();
            serde_json::json!({ "ok": ok, "results": results })
        }
    };
    let mut bytes = serde_json::to_vec(&json).expect("json values serialize");
    if bytes.len() >= MAX_REPLY_BYTES {
        bytes = serde_json::to_vec(&serde_json::json!({
            "ok": false,
            "error": format!("the command's reply exceeds {} KiB", MAX_REPLY_BYTES / 1024),
        }))
        .expect("json values serialize");
    }
    bytes.push(b'\n');
    bytes
}

#[derive(Default)]
struct Clients {
    next: u64,
    live: BTreeMap<u64, ClientTokens>,
}

struct ClientTokens {
    source: RegistrationToken,
    timer: Option<RegistrationToken>,
}

type Shared = Rc<RefCell<Clients>>;

/// the host's listener and its connections on one event loop.
pub(crate) struct CommandServer<D: 'static> {
    handle: LoopHandle<'static, D>,
    listener: Option<RegistrationToken>,
    clients: Shared,
    // held for its drop, which unlinks the socket after the sources close.
    _endpoint: Endpoint,
}

impl<D: CommandHost> CommandServer<D> {
    pub(crate) fn start(
        handle: &LoopHandle<'static, D>,
        runtime_directory: &Path,
        instance: &str,
    ) -> Result<Self, String> {
        Self::start_with_timeout(handle, runtime_directory, instance, CLIENT_TIMEOUT)
    }

    pub(crate) fn start_with_timeout(
        handle: &LoopHandle<'static, D>,
        runtime_directory: &Path,
        instance: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        let (endpoint, listener) = Endpoint::bind(runtime_directory, instance)?;
        let clients = Shared::default();
        let uid = current_uid();
        let listener = handle
            .insert_source(Generic::new(listener, Interest::READ, Mode::Level), {
                let handle = handle.clone();
                let clients = clients.clone();
                move |_, listener, _| {
                    // a bounded batch keeps one flood from starving the ui.
                    for _ in 0..MAX_CLIENTS * 2 {
                        match listener.accept() {
                            Ok((stream, _)) => accept(&handle, &clients, stream, uid, timeout),
                            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                            Err(error) => {
                                eprintln!("thing: command socket: {error}");
                                break;
                            }
                        }
                    }
                    Ok(PostAction::Continue)
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            handle: handle.clone(),
            listener: Some(listener),
            clients,
            _endpoint: endpoint,
        })
    }

    #[cfg(test)]
    pub(crate) fn clients(&self) -> usize {
        self.clients.borrow().live.len()
    }
}

impl<D: 'static> Drop for CommandServer<D> {
    fn drop(&mut self) {
        // close every connection before the endpoint unlinks the socket.
        if let Some(listener) = self.listener.take() {
            self.handle.remove(listener);
        }
        for (_, client) in std::mem::take(&mut self.clients.borrow_mut().live) {
            self.handle.remove(client.source);
            if let Some(timer) = client.timer {
                self.handle.remove(timer);
            }
        }
    }
}

fn accept<D: CommandHost>(
    handle: &LoopHandle<'static, D>,
    clients: &Shared,
    stream: UnixStream,
    uid: u32,
    timeout: Duration,
) {
    if stream.set_nonblocking(true).is_err() || peer_uid(&stream).ok() != Some(uid) {
        return;
    }
    let id = {
        let mut clients = clients.borrow_mut();
        if clients.live.len() >= MAX_CLIENTS {
            let busy = encode_reply(&CommandReply::Failed(
                "the instance is busy with other command clients".into(),
            ));
            let _ = (&stream).write(&busy);
            return;
        }
        clients.next += 1;
        clients.next
    };
    let source = handle.insert_source(
        Generic::new(stream, Interest::READ, Mode::Level),
        reader(id, handle.clone(), clients.clone()),
    );
    let Ok(source) = source else {
        return;
    };
    let timer = handle
        .insert_source(Timer::from_duration(timeout), {
            let handle = handle.clone();
            let clients = clients.clone();
            move |_, _, _| {
                // a stalled client loses its connection; nothing else waits.
                if let Some(client) = clients.borrow_mut().live.remove(&id) {
                    handle.remove(client.source);
                }
                TimeoutAction::Drop
            }
        })
        .ok();
    clients
        .borrow_mut()
        .live
        .insert(id, ClientTokens { source, timer });
}

fn finish<D>(handle: &LoopHandle<'static, D>, clients: &Shared, id: u64) {
    if let Some(client) = clients.borrow_mut().live.remove(&id)
        && let Some(timer) = client.timer
    {
        handle.remove(timer);
    }
}

type ClientCallback<D> = Box<
    dyn FnMut(
        calloop::Readiness,
        &mut calloop::generic::NoIoDrop<UnixStream>,
        &mut D,
    ) -> io::Result<PostAction>,
>;

fn reader<D: CommandHost>(
    id: u64,
    handle: LoopHandle<'static, D>,
    clients: Shared,
) -> ClientCallback<D> {
    let mut input = Vec::new();
    Box::new(move |_, stream, data| {
        let mut ended = false;
        let mut chunk = [0_u8; 4096];
        while !input.contains(&b'\n') && input.len() <= MAX_REQUEST_BYTES {
            match (&**stream).read(&mut chunk) {
                Ok(0) => {
                    ended = true;
                    break;
                }
                Ok(read) => input.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    finish(&handle, &clients, id);
                    return Ok(PostAction::Remove);
                }
            }
        }
        let line = match input.iter().position(|byte| *byte == b'\n') {
            Some(end) if end < MAX_REQUEST_BYTES => Some(&input[..end]),
            None if ended && !input.is_empty() && input.len() < MAX_REQUEST_BYTES => {
                Some(&input[..])
            }
            _ => None,
        };
        let reply = match line {
            Some(line) => match parse_host_request(line) {
                Ok(HostRequest::Command(request)) => encode_reply(&data.run_command(request)),
                Ok(HostRequest::Inspect(request)) => {
                    crate::inspection::encode_reply(&data.inspect(request))
                }
                Err(error) => encode_reply(&CommandReply::Failed(error)),
            },
            None if input.len() >= MAX_REQUEST_BYTES => encode_reply(&CommandReply::Failed(
                format!("the request exceeds {} KiB", MAX_REQUEST_BYTES / 1024),
            )),
            None if ended => {
                finish(&handle, &clients, id);
                return Ok(PostAction::Remove);
            }
            None => return Ok(PostAction::Continue),
        };
        let written = write_some(stream, &reply);
        match written {
            Some(written) if written < reply.len() => {
                // the client has not read yet. keep the rest without blocking.
                let Ok(stream) = stream.try_clone() else {
                    finish(&handle, &clients, id);
                    return Ok(PostAction::Remove);
                };
                let pending = reply[written..].to_vec();
                match handle.insert_source(
                    Generic::new(stream, Interest::WRITE, Mode::Level),
                    writer(id, handle.clone(), clients.clone(), pending),
                ) {
                    Ok(source) => {
                        if let Some(client) = clients.borrow_mut().live.get_mut(&id) {
                            client.source = source;
                        } else {
                            handle.remove(source);
                        }
                    }
                    Err(_) => finish(&handle, &clients, id),
                }
            }
            _ => finish(&handle, &clients, id),
        }
        Ok(PostAction::Remove)
    })
}

fn writer<D: CommandHost>(
    id: u64,
    handle: LoopHandle<'static, D>,
    clients: Shared,
    mut pending: Vec<u8>,
) -> ClientCallback<D> {
    Box::new(move |_, stream, _| match write_some(stream, &pending) {
        Some(written) if written < pending.len() => {
            pending.drain(..written);
            Ok(PostAction::Continue)
        }
        _ => {
            finish(&handle, &clients, id);
            Ok(PostAction::Remove)
        }
    })
}

/// writes without blocking. `None` means the connection failed.
fn write_some(stream: &UnixStream, bytes: &[u8]) -> Option<usize> {
    let mut written = 0;
    while written < bytes.len() {
        match (&*stream).write(&bytes[written..]) {
            Ok(0) => return None,
            Ok(count) => written += count,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(written)
}

/// one `thing call` invocation.
#[derive(Clone, Debug)]
pub struct CallOptions {
    pub instance: String,
    pub selection: OutputSelection,
    pub command: String,
    pub arguments: CallValue,
    /// how long to wait to connect, send, and read the reply.
    pub timeout: Duration,
}

/// calls a command in a running instance, returning the json to print.
/// errors carry an exit status: 2 for usage problems, 1 otherwise.
pub fn call(options: &CallOptions) -> Result<String, (u8, String)> {
    let directory = runtime_directory().map_err(|error| (1, error))?;
    call_at(&directory, options)
}

pub(crate) fn call_at(
    runtime_directory: &Path,
    options: &CallOptions,
) -> Result<String, (u8, String)> {
    validate_instance_name(&options.instance).map_err(|error| (2, error))?;
    validate_command_name(&options.command).map_err(|error| (2, error))?;
    options.arguments.validate().map_err(|error| (2, error))?;
    let request = encode_request(&options.command, &options.arguments, &options.selection);
    exchange(
        runtime_directory,
        &options.instance,
        &request,
        &options.selection,
        options.timeout,
    )
}

/// one `thing inspect --live` invocation.
#[derive(Clone, Debug)]
pub struct InspectOptions {
    pub instance: String,
    pub selection: OutputSelection,
    pub query: InspectQuery,
    /// how long to wait to connect, send, and read the reply.
    pub timeout: Duration,
}

/// inspects the presented layout of a running instance, returning the json
/// to print. errors carry an exit status like `call`.
pub fn inspect(options: &InspectOptions) -> Result<String, (u8, String)> {
    let directory = runtime_directory().map_err(|error| (1, error))?;
    inspect_at(&directory, options)
}

pub(crate) fn inspect_at(
    runtime_directory: &Path,
    options: &InspectOptions,
) -> Result<String, (u8, String)> {
    validate_instance_name(&options.instance).map_err(|error| (2, error))?;
    options
        .query
        .validate(&options.selection)
        .map_err(|error| (2, error))?;
    let mut object = Map::new();
    object.insert("version".into(), Json::from(PROTOCOL_VERSION));
    object.insert(
        "inspect".into(),
        crate::inspection::encode_query(&options.query),
    );
    match &options.selection {
        OutputSelection::Implicit => {}
        OutputSelection::Named(name) => {
            object.insert("output".into(), Json::String(name.clone()));
        }
        OutputSelection::All => {
            object.insert("all_outputs".into(), Json::Bool(true));
        }
    }
    let mut request = serde_json::to_vec(&Json::Object(object)).expect("json values serialize");
    request.push(b'\n');
    exchange(
        runtime_directory,
        &options.instance,
        &request,
        &options.selection,
        options.timeout,
    )
}

/// sends one request line and formats its reply, within one deadline for
/// connecting, writing, and reading.
fn exchange(
    runtime_directory: &Path,
    instance: &str,
    request: &[u8],
    selection: &OutputSelection,
    timeout: Duration,
) -> Result<String, (u8, String)> {
    if request.len() > MAX_REQUEST_BYTES {
        return Err((
            2,
            format!("the request exceeds {} KiB", MAX_REQUEST_BYTES / 1024),
        ));
    }
    let deadline = Instant::now()
        .checked_add(timeout.max(Duration::from_millis(1)))
        .ok_or_else(|| (2, "the call timeout is too large".to_owned()))?;
    let uid = current_uid();
    let failed = |message: String| (1, message);
    let path = socket_path(runtime_directory, instance);
    match command_directory(runtime_directory, uid, false) {
        Ok(_) => {}
        Err(_) if !path.exists() => {
            return Err(failed(format!(
                "no thing instance named {instance:?} is running; start one with 'thing run --instance {instance} <entry.luau>'"
            )));
        }
        Err(error) => return Err(failed(error)),
    }
    let connect = || -> io::Result<UnixStream> {
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket.connect_timeout(&SockAddr::unix(&path)?, remaining(deadline)?)?;
        Ok(socket.into())
    };
    let stream = connect().map_err(|error| {
        failed(match error.kind() {
            ErrorKind::NotFound => format!(
                "no thing instance named {instance:?} is running; start one with 'thing run --instance {instance} <entry.luau>'"
            ),
            ErrorKind::ConnectionRefused => {
                format!("thing instance {instance:?} is not running (stale socket {})", path.display())
            }
            ErrorKind::WouldBlock | ErrorKind::TimedOut => {
                format!("instance {instance:?} did not reply in time")
            }
            _ => format!("cannot connect to {}: {error}", path.display()),
        })
    })?;
    if peer_uid(&stream).ok() != Some(uid) {
        return Err(failed(format!(
            "{} is not served by this user; refusing to send the command",
            path.display()
        )));
    }
    let timed_out = |error: io::Error| {
        failed(
            if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
                format!("instance {instance:?} did not reply in time")
            } else {
                format!("lost the connection to instance {instance:?}: {error}")
            },
        )
    };
    // a busy instance may reply and close before reading the request, so a
    // failed write still reads whatever reply arrived.
    let sent = send_request(&stream, request, deadline);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reply = Vec::new();
    let received = receive_reply(&stream, &mut reply, deadline);
    if reply.last() != Some(&b'\n') {
        if let Err(error) = sent.and(received.map(drop)) {
            return Err(timed_out(error));
        }
        return Err(failed(format!(
            "instance {instance:?} closed the connection without replying"
        )));
    }
    format_reply(&reply, selection).map_err(failed)
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(ErrorKind::TimedOut, "the call deadline has passed"))
}

fn send_request(mut stream: &UnixStream, mut request: &[u8], deadline: Instant) -> io::Result<()> {
    while !request.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(request) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "cannot send the request",
                ));
            }
            Ok(written) => request = &request[written..],
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn receive_reply(
    mut stream: &UnixStream,
    reply: &mut Vec<u8>,
    deadline: Instant,
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    while reply.len() <= MAX_REPLY_BYTES {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let capacity = buffer.len().min(MAX_REPLY_BYTES + 1 - reply.len());
        match stream.read(&mut buffer[..capacity]) {
            Ok(0) => break,
            Ok(read) => {
                reply.extend_from_slice(&buffer[..read]);
                if buffer[..read].contains(&b'\n') {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn format_reply(reply: &[u8], selection: &OutputSelection) -> Result<String, String> {
    if reply.len() > MAX_REPLY_BYTES || reply.last() != Some(&b'\n') {
        return Err("the instance sent an incomplete or oversized reply".into());
    }
    let reply: Json =
        serde_json::from_slice(reply).map_err(|error| format!("malformed reply: {error}"))?;
    if let Some(Json::String(error)) = reply.get("error") {
        return Err(error.clone());
    }
    let Some(Json::Array(results)) = reply.get("results") else {
        return Err("malformed reply: no results".into());
    };
    let name = |result: &Json| match result.get("output") {
        Some(Json::String(name)) => name.clone(),
        _ => "an unnamed output".into(),
    };
    let errors: Vec<_> = results
        .iter()
        .filter_map(|result| match result.get("error") {
            Some(Json::String(error)) => Some(format!("{}: {error}", name(result))),
            _ => None,
        })
        .collect();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let value = |result: &Json| result.get("value").cloned().unwrap_or(Json::Null);
    let output = match (selection, results.as_slice()) {
        (OutputSelection::All, _) => Json::Array(
            results
                .iter()
                .map(|result| {
                    serde_json::json!({
                        "output": result.get("output").cloned().unwrap_or(Json::Null),
                        "value": value(result),
                    })
                })
                .collect(),
        ),
        (_, [result]) => value(result),
        _ => return Err("malformed reply: expected one result".into()),
    };
    Ok(serde_json::to_string(&output).expect("json values serialize"))
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
