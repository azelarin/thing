//! external drag and drop: payload validation, local file uris, and the
//! `on_drag_over`, `on_drop`, and `event:start_drag` callbacks.

use std::{cell::RefCell, rc::Rc, time::Instant};

use mlua::{Error, Function, Lua, Result, Table, Value};
use thing_core::{DropType, Kind};

use crate::{ConfigurationMode, Runtime, SurfaceTree, UiTarget};

/// the most utf-8 bytes one dragged or dropped text may hold.
pub const MAX_DRAG_TEXT_BYTES: usize = 1024 * 1024;
/// the most files one drag may carry.
pub const MAX_DRAG_FILES: usize = 256;
/// the most bytes one encoded file uri may hold.
pub const MAX_FILE_URI_BYTES: usize = 4096;
/// the most bytes an encoded `text/uri-list` may hold.
pub const MAX_URI_LIST_BYTES: usize = 1024 * 1024;

/// a wayland drag and drop action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragAction {
    Copy,
    Move,
}

impl DragAction {
    pub fn name(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
        }
    }
}

/// the set of actions a drag source allows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DragActions {
    pub copy: bool,
    pub moves: bool,
}

impl DragActions {
    pub fn contains(self, action: DragAction) -> bool {
        match action {
            DragAction::Copy => self.copy,
            DragAction::Move => self.moves,
        }
    }

    pub fn is_empty(self) -> bool {
        !self.copy && !self.moves
    }

    fn names(self) -> Vec<&'static str> {
        let mut names = Vec::with_capacity(2);
        if self.copy {
            names.push("copy");
        }
        if self.moves {
            names.push("move");
        }
        names
    }
}

/// validated data carried by a drag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragData {
    Text(String),
    /// normalized absolute local paths.
    Files(Vec<String>),
}

impl DragData {
    pub fn kind(&self) -> DropType {
        match self {
            Self::Text(_) => DropType::Text,
            Self::Files(_) => DropType::Files,
        }
    }
}

/// an outgoing drag that a pointer handler asked the host to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragRequest {
    pub id: u64,
    pub data: DragData,
    pub actions: DragActions,
}

/// how an outgoing drag ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragResult {
    /// the destination finished a copy.
    Copy,
    /// the destination finished a move; the source may now remove its data.
    Move,
    /// the drag was cancelled, rejected, or could not start.
    Cancelled,
}

impl DragResult {
    fn name(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
            Self::Cancelled => "cancelled",
        }
    }
}

/// the phase of an external drag over a drop target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOverPhase {
    Enter,
    Move,
    /// the drag moved away or its source ended it before a drop.
    Leave,
    /// a drop on this target ended without data: the transfer failed, timed
    /// out, or carried invalid data.
    Cancel,
}

/// logical coordinates of a drag, local to the target and to its surface.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DropPosition {
    pub x: f64,
    pub y: f64,
    pub surface_x: f64,
    pub surface_y: f64,
}

/// what an external drag over a drop target offers.
#[derive(Clone, Debug, PartialEq)]
pub struct DragOverInput {
    pub phase: DragOverPhase,
    pub position: DropPosition,
    /// the offered types the source can supply, before target preference.
    pub offered: Vec<DropType>,
    pub actions: DragActions,
}

/// the outcome of one `on_drag_over` callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragOverOutcome {
    pub changed: bool,
    /// the type and action the target accepts, if any.
    pub accepted: Option<(DropType, DragAction)>,
}

/// a completed and validated drop.
#[derive(Clone, Debug, PartialEq)]
pub struct DropInput {
    pub position: DropPosition,
    pub action: DragAction,
    pub data: DragData,
}

/// the outcome of one `on_drop` callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropOutcome {
    pub changed: bool,
    pub accepted: bool,
}

/// why dropped bytes were rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropDataError {
    InvalidUtf8,
    ContainsNul,
    TooLarge,
    Empty,
    TooManyFiles,
    UnsupportedUri,
    RemoteUri,
    InvalidPath,
}

impl std::fmt::Display for DropDataError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUtf8 => "dropped data is not valid UTF-8",
            Self::ContainsNul => "dropped data contains a NUL byte",
            Self::TooLarge => "dropped data exceeds its size limit",
            Self::Empty => "dropped uri list names no files",
            Self::TooManyFiles => "dropped uri list names too many files",
            Self::UnsupportedUri => "dropped uri is not a file uri",
            Self::RemoteUri => "dropped file uri names a remote host",
            Self::InvalidPath => "dropped file uri is not a normalized absolute path",
        })
    }
}

impl std::error::Error for DropDataError {}

/// decodes dropped `text/plain` bytes.
pub fn decode_drop_text(bytes: Vec<u8>) -> std::result::Result<String, DropDataError> {
    if bytes.len() > MAX_DRAG_TEXT_BYTES {
        return Err(DropDataError::TooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| DropDataError::InvalidUtf8)?;
    if text.contains('\0') {
        return Err(DropDataError::ContainsNul);
    }
    Ok(text)
}

/// decodes a dropped `text/uri-list` into normalized local paths. comment
/// lines are skipped; any remote or non-file uri rejects the whole list.
pub fn decode_uri_list(bytes: &[u8]) -> std::result::Result<Vec<String>, DropDataError> {
    if bytes.len() > MAX_URI_LIST_BYTES {
        return Err(DropDataError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| DropDataError::InvalidUtf8)?;
    if text.contains('\0') {
        return Err(DropDataError::ContainsNul);
    }
    let mut paths = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if paths.len() == MAX_DRAG_FILES {
            return Err(DropDataError::TooManyFiles);
        }
        paths.push(file_uri_path(line)?);
    }
    if paths.is_empty() {
        return Err(DropDataError::Empty);
    }
    Ok(paths)
}

/// encodes local paths as a `text/uri-list` with CRLF line ends.
pub fn encode_uri_list(paths: &[String]) -> String {
    let mut list = String::new();
    for path in paths {
        list.push_str(&path_file_uri(path));
        list.push_str("\r\n");
    }
    list
}

/// the local path named by a `file:` uri. only an empty host or `localhost`
/// is local; queries, fragments, and other schemes are rejected.
pub fn file_uri_path(uri: &str) -> std::result::Result<String, DropDataError> {
    if uri.len() > MAX_FILE_URI_BYTES {
        return Err(DropDataError::TooLarge);
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return Err(DropDataError::UnsupportedUri);
    };
    if !scheme.eq_ignore_ascii_case("file") {
        return Err(DropDataError::UnsupportedUri);
    }
    if rest.contains(['?', '#']) {
        return Err(DropDataError::UnsupportedUri);
    }
    let encoded = if let Some(authority) = rest.strip_prefix("//") {
        let (host, path) = authority
            .find('/')
            .map_or((authority, ""), |index| authority.split_at(index));
        if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
            return Err(DropDataError::RemoteUri);
        }
        path
    } else {
        rest
    };
    let decoded = percent_decode(encoded).ok_or(DropDataError::InvalidPath)?;
    let path = String::from_utf8(decoded).map_err(|_| DropDataError::InvalidUtf8)?;
    normalize_path(&path).ok_or(DropDataError::InvalidPath)
}

/// the `file:///` uri for a normalized absolute path.
pub fn path_file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// a path or local file uri from a configuration, as a normalized path.
fn local_path(value: &str) -> std::result::Result<String, String> {
    let path = if value.starts_with('/') {
        normalize_path(value)
            .ok_or_else(|| format!("{value:?} is not a normalized absolute path"))?
    } else {
        file_uri_path(value).map_err(|error| format!("{value:?}: {error}"))?
    };
    if path_file_uri(&path).len() > MAX_FILE_URI_BYTES {
        return Err(format!("{value:?} is too long"));
    }
    Ok(path)
}

/// collapses repeated and `.` components. `..`, relative paths, NUL, and
/// control characters are rejected because they cannot be resolved without
/// the filesystem.
fn normalize_path(path: &str) -> Option<String> {
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return None;
    }
    let mut normalized = String::with_capacity(path.len());
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => return None,
            component => {
                normalized.push('/');
                normalized.push_str(component);
            }
        }
    }
    if normalized.is_empty() {
        normalized.push('/');
    }
    Some(normalized)
}

fn percent_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let digits = bytes.get(index + 1..index + 3)?;
            let digits = std::str::from_utf8(digits).ok()?;
            decoded.push(u8::from_str_radix(digits, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(decoded)
}

struct PendingStart {
    data: DragData,
    actions: DragActions,
    on_end: Option<Function>,
}

#[derive(Default)]
struct StartSlot {
    /// the address of the pointer event table whose callback is running. only
    /// that table may start a drag, so a stored event or function cannot start
    /// one from a later callback, timer, or keyboard handler.
    event: Option<usize>,
    active: bool,
    request: Option<PendingStart>,
}

/// outgoing drag state owned by one runtime.
pub(crate) struct Drags {
    slot: Rc<RefCell<StartSlot>>,
    start: Function,
    active: Option<(u64, Option<Function>)>,
    pending: Option<DragRequest>,
    next_id: u64,
}

impl Drags {
    pub(crate) fn install(lua: &Lua, mode: ConfigurationMode) -> Result<Self> {
        let slot = Rc::new(RefCell::new(StartSlot::default()));
        let state = slot.clone();
        let lock = matches!(mode, ConfigurationMode::Lock);
        let start = lua.create_function(move |_, (event, options): (Value, Value)| {
            if lock {
                return Err(Error::runtime(
                    "drag and drop is unavailable in lock configurations",
                ));
            }
            let mut slot = state.borrow_mut();
            let current = match &event {
                Value::Table(event) => slot.event == Some(event.to_pointer() as usize),
                _ => false,
            };
            if !current {
                return Err(Error::runtime(
                    "start_drag must be called as event:start_drag while handling a primary press or move",
                ));
            }
            if slot.active || slot.request.is_some() {
                return Err(Error::runtime("a drag is already active"));
            }
            let Value::Table(options) = options else {
                return Err(Error::runtime("start_drag expects an options table"));
            };
            slot.request = Some(parse_start(&options)?);
            Ok(())
        })?;
        Ok(Self {
            slot,
            start,
            active: None,
            pending: None,
            next_id: 1,
        })
    }
}

fn parse_start(options: &Table) -> Result<PendingStart> {
    let mut text = None;
    let mut files = None;
    let mut actions = None;
    let mut on_end = None;
    for pair in options.pairs::<String, Value>() {
        let (key, value) = pair?;
        match (key.as_str(), value) {
            ("text", Value::String(value)) => {
                let bytes = value.as_bytes();
                if bytes.len() > MAX_DRAG_TEXT_BYTES || bytes.contains(&0) {
                    return Err(Error::runtime(
                        "drag text must contain no NUL and must not exceed 1 MiB",
                    ));
                }
                let value = std::str::from_utf8(&bytes)
                    .map_err(|_| Error::runtime("drag text must be valid UTF-8"))?
                    .to_owned();
                text = Some(value);
            }
            ("files", Value::Table(list)) => {
                let length = list.raw_len();
                if length == 0 || length > MAX_DRAG_FILES {
                    return Err(Error::runtime(format!(
                        "drag files must name 1 to {MAX_DRAG_FILES} files"
                    )));
                }
                let mut paths = Vec::with_capacity(length);
                let mut encoded = 0_usize;
                for index in 1..=length {
                    let Value::String(value) = list.raw_get::<Value>(index)? else {
                        return Err(Error::runtime("drag files must be strings"));
                    };
                    let value = value.to_str()?;
                    let path = local_path(&value).map_err(Error::runtime)?;
                    encoded = encoded.saturating_add(path_file_uri(&path).len() + 2);
                    paths.push(path);
                }
                if encoded > MAX_URI_LIST_BYTES {
                    return Err(Error::runtime("drag files exceed 1 MiB as a uri list"));
                }
                if list.pairs::<Value, Value>().count() != length {
                    return Err(Error::runtime("drag files must be a dense list"));
                }
                files = Some(paths);
            }
            ("actions", Value::Table(list)) => {
                let mut parsed = DragActions::default();
                for value in list.sequence_values::<String>() {
                    match value?.as_str() {
                        "copy" => parsed.copy = true,
                        "move" => parsed.moves = true,
                        other => {
                            return Err(Error::runtime(format!("unknown drag action {other:?}")));
                        }
                    }
                }
                if parsed.is_empty() {
                    return Err(Error::runtime("drag actions must not be empty"));
                }
                actions = Some(parsed);
            }
            ("on_end", Value::Function(function)) => on_end = Some(function),
            ("text" | "files" | "actions" | "on_end", _) => {
                return Err(Error::runtime(format!(
                    "start_drag {key} has the wrong type"
                )));
            }
            (other, _) => {
                return Err(Error::runtime(format!(
                    "unknown start_drag option {other:?}"
                )));
            }
        }
    }
    let data = match (text, files) {
        (Some(text), None) => DragData::Text(text),
        (None, Some(files)) => DragData::Files(files),
        _ => {
            return Err(Error::runtime(
                "start_drag requires exactly one of text or files",
            ));
        }
    };
    Ok(PendingStart {
        data,
        actions: actions.unwrap_or(DragActions {
            copy: true,
            moves: false,
        }),
        on_end,
    })
}

type Validate<'a> = &'a dyn Fn(&[SurfaceTree]) -> std::result::Result<(), String>;

impl Runtime {
    /// the shared `start_drag` function placed on pointer events.
    pub(crate) fn drag_start_function(&self) -> Function {
        self.drags.start.clone()
    }

    /// allow `event:start_drag` for exactly this event table while its
    /// callback runs, or forbid it with `None`.
    pub(crate) fn allow_drag_start(&self, event: Option<&Table>) {
        let mut slot = self.drags.slot.borrow_mut();
        slot.event = event.map(|event| event.to_pointer() as usize);
        slot.active = self.drags.active.is_some();
        if event.is_none() {
            slot.request = None;
        }
    }

    /// the id of the outgoing drag that has not ended yet.
    pub fn active_drag(&self) -> Option<u64> {
        self.drags.active.as_ref().map(|(id, _)| *id)
    }

    /// keep the drag a successful pointer callback started.
    pub(crate) fn commit_drag_start(&mut self) -> Result<()> {
        let Some(start) = self.drags.slot.borrow_mut().request.take() else {
            return Ok(());
        };
        let id = self.drags.next_id;
        self.drags.next_id = id
            .checked_add(1)
            .ok_or_else(|| Error::runtime("drag identity space is exhausted"))?;
        self.drags.active = Some((id, start.on_end));
        self.drags.pending = Some(DragRequest {
            id,
            data: start.data,
            actions: start.actions,
        });
        Ok(())
    }

    /// the outgoing drag a pointer callback requested, if any.
    pub fn take_drag_request(&mut self) -> Option<DragRequest> {
        self.drags.pending.take()
    }

    /// report how an outgoing drag ended and run its `on_end` callback once.
    /// unknown or already finished drags are ignored.
    pub fn finish_drag_checked(
        &mut self,
        id: u64,
        result: DragResult,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        if self
            .drags
            .active
            .as_ref()
            .is_none_or(|(active, _)| *active != id)
        {
            return Ok(false);
        }
        if self
            .drags
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            self.drags.pending = None;
        }
        let Some((_, on_end)) = self.drags.active.take() else {
            return Ok(false);
        };
        let Some(on_end) = on_end else {
            return Ok(false);
        };
        self.with_event_controls(None, |runtime| {
            let checkpoint = runtime.action_checkpoint();
            runtime.begin_call();
            runtime.allow_actions(true);
            let result = on_end.call::<()>(result.name());
            runtime.allow_actions(false);
            let result =
                result.and_then(|()| runtime.refresh_at_checked(Instant::now(), &validate));
            if result.is_err() {
                runtime.truncate_actions(checkpoint);
            }
            result
        })
    }

    /// capture an available pointer area that handles drops.
    pub fn drop_target(&self, id: thing_core::NodeId) -> Option<UiTarget> {
        let target = self.ui_target(id)?;
        self.drop_target_is_current(&target).then_some(target)
    }

    /// whether a captured identity still names an available drop target.
    pub fn drop_target_is_current(&self, target: &UiTarget) -> bool {
        self.ui_target_is_current(target)
            && self
                .find(target.node)
                .is_some_and(|node| node.drop_events && node.kind == Kind::PointerArea)
            && self.handlers.contains_key(&(target.node, "on_drag_over"))
            && self.handlers.contains_key(&(target.node, "on_drop"))
    }

    /// the offered types a drop target accepts, most preferred first.
    pub fn drop_types(&self, target: &UiTarget, offered: &[DropType]) -> Vec<DropType> {
        let Some(node) = self.find(target.node) else {
            return Vec::new();
        };
        let preference: &[DropType] = if node.drop_types.is_empty() {
            &[DropType::Files, DropType::Text]
        } else {
            &node.drop_types
        };
        preference
            .iter()
            .copied()
            .filter(|kind| offered.contains(kind))
            .collect()
    }

    /// run `on_drag_over` for an enter, move, or leave. returns `None` when
    /// the target is no longer current.
    pub fn dispatch_drag_over_checked(
        &mut self,
        target: &UiTarget,
        input: DragOverInput,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<Option<DragOverOutcome>> {
        self.with_event_controls(None, |runtime| {
            runtime.dispatch_drag_over_inner(target, input, &validate)
        })
    }

    fn dispatch_drag_over_inner(
        &mut self,
        target: &UiTarget,
        input: DragOverInput,
        validate: Validate<'_>,
    ) -> Result<Option<DragOverOutcome>> {
        validate_position(input.position)?;
        if !self.drop_target_is_current(target) {
            return Ok(None);
        }
        let types = self.drop_types(target, &input.offered);
        let handler = self.handlers[&(target.node, "on_drag_over")].clone();
        let event = self.lua.create_table()?;
        event.set(
            "phase",
            match input.phase {
                DragOverPhase::Enter => "enter",
                DragOverPhase::Move => "move",
                DragOverPhase::Leave => "leave",
                DragOverPhase::Cancel => "cancel",
            },
        )?;
        set_position(&event, input.position)?;
        let names = self
            .lua
            .create_sequence_from(types.iter().map(|kind| kind.name()))?;
        names.set_readonly(true);
        event.set("types", names)?;
        let actions = self.lua.create_sequence_from(input.actions.names())?;
        actions.set_readonly(true);
        event.set("actions", actions)?;
        event.set_readonly(true);

        // the return value of a leave or cancel callback has no effect.
        let leave = matches!(input.phase, DragOverPhase::Leave | DragOverPhase::Cancel);
        let offered = input.actions;
        let first = types.first().copied();
        let checkpoint = self.action_checkpoint();
        self.begin_call();
        self.allow_actions(true);
        let result = handler.call::<Value>(event).and_then(|value| {
            if leave {
                return Ok(None);
            }
            let action = match value {
                Value::Nil | Value::Boolean(false) => return Ok(None),
                Value::String(name) => match &*name.to_str()? {
                    "copy" => DragAction::Copy,
                    "move" => DragAction::Move,
                    _ => {
                        return Err(Error::runtime(
                            "on_drag_over must return 'copy', 'move', or nil",
                        ));
                    }
                },
                _ => {
                    return Err(Error::runtime(
                        "on_drag_over must return 'copy', 'move', or nil",
                    ));
                }
            };
            if !offered.contains(action) {
                return Err(Error::runtime(format!(
                    "on_drag_over returned {:?}, which the drag source does not offer",
                    action.name()
                )));
            }
            Ok(first.map(|kind| (kind, action)))
        });
        self.allow_actions(false);
        let result = result.and_then(|accepted| {
            self.refresh_at_checked(Instant::now(), &validate)
                .map(|changed| DragOverOutcome { changed, accepted })
        });
        if result.is_err() {
            self.truncate_actions(checkpoint);
        }
        result.map(Some)
    }

    /// run `on_drop` with validated data. returns `None` when the target is
    /// no longer current; `accepted` is true only when the handler returned
    /// `true`.
    pub fn dispatch_drop_checked(
        &mut self,
        target: &UiTarget,
        input: DropInput,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<Option<DropOutcome>> {
        self.with_event_controls(None, |runtime| {
            runtime.dispatch_drop_inner(target, input, &validate)
        })
    }

    fn dispatch_drop_inner(
        &mut self,
        target: &UiTarget,
        input: DropInput,
        validate: Validate<'_>,
    ) -> Result<Option<DropOutcome>> {
        validate_position(input.position)?;
        if !self.drop_target_is_current(target) {
            return Ok(None);
        }
        let handler = self.handlers[&(target.node, "on_drop")].clone();
        let event = self.lua.create_table()?;
        set_position(&event, input.position)?;
        event.set("action", input.action.name())?;
        event.set("type", input.data.kind().name())?;
        match input.data {
            DragData::Text(text) => event.set("text", text)?,
            DragData::Files(files) => {
                let files = self.lua.create_sequence_from(files)?;
                files.set_readonly(true);
                event.set("files", files)?;
            }
        }
        event.set_readonly(true);
        let checkpoint = self.action_checkpoint();
        self.begin_call();
        self.allow_actions(true);
        let result = handler.call::<Option<bool>>(event);
        self.allow_actions(false);
        let result = result.and_then(|accepted| {
            self.refresh_at_checked(Instant::now(), &validate)
                .map(|changed| DropOutcome {
                    changed,
                    accepted: accepted.unwrap_or(false),
                })
        });
        if result.is_err() {
            self.truncate_actions(checkpoint);
        }
        result.map(Some)
    }
}

fn set_position(event: &Table, position: DropPosition) -> Result<()> {
    event.set("x", position.x)?;
    event.set("y", position.y)?;
    event.set("surface_x", position.surface_x)?;
    event.set("surface_y", position.surface_y)
}

fn validate_position(position: DropPosition) -> Result<()> {
    if [
        position.x,
        position.y,
        position.surface_x,
        position.surface_y,
    ]
    .iter()
    .all(|value| value.is_finite())
    {
        Ok(())
    } else {
        Err(Error::runtime("drag coordinates must be finite"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_are_local_normalized_and_round_trip() {
        assert_eq!(file_uri_path("file:///tmp/a%20b").unwrap(), "/tmp/a b");
        assert_eq!(
            file_uri_path("FILE://localhost//tmp/./x").unwrap(),
            "/tmp/x"
        );
        assert_eq!(file_uri_path("file:/tmp/x").unwrap(), "/tmp/x");
        assert_eq!(
            file_uri_path("file://example.com/tmp/x"),
            Err(DropDataError::RemoteUri)
        );
        for bad in [
            "https://example.com/x",
            "smb://host/share",
            "file:///a?b",
            "x",
        ] {
            assert_eq!(
                file_uri_path(bad),
                Err(DropDataError::UnsupportedUri),
                "{bad}"
            );
        }
        for bad in [
            "file:///a/../b",
            "file:///a%00b",
            "file:///a%0Ab",
            "file://relative",
            "file:///%zz",
        ] {
            assert!(file_uri_path(bad).is_err(), "{bad}");
        }
        assert_eq!(
            file_uri_path("file:///%FF"),
            Err(DropDataError::InvalidUtf8)
        );
        let path = "/tmp/ä b%#?".to_owned();
        assert_eq!(file_uri_path(&path_file_uri(&path)).unwrap(), path);
    }

    #[test]
    fn uri_lists_are_bounded_and_reject_any_remote_entry() {
        assert_eq!(
            decode_uri_list(b"# comment\r\nfile:///a\r\n\r\nfile:///b%20c\n").unwrap(),
            ["/a", "/b c"]
        );
        assert_eq!(
            decode_uri_list(b"file:///a\r\nhttp://example.com/b\r\n"),
            Err(DropDataError::UnsupportedUri)
        );
        assert_eq!(decode_uri_list(b"# only\r\n"), Err(DropDataError::Empty));
        assert_eq!(decode_uri_list(b"\xff"), Err(DropDataError::InvalidUtf8));
        let many = "file:///x\r\n".repeat(MAX_DRAG_FILES + 1);
        assert_eq!(
            decode_uri_list(many.as_bytes()),
            Err(DropDataError::TooManyFiles)
        );
        assert_eq!(
            encode_uri_list(&["/a b".into(), "/c".into()]),
            "file:///a%20b\r\nfile:///c\r\n"
        );
    }

    #[test]
    fn dropped_text_must_be_utf8_without_nul() {
        assert_eq!(decode_drop_text(b"h\xc3\xa9".to_vec()).unwrap(), "h\u{e9}");
        assert_eq!(
            decode_drop_text(b"\xff".to_vec()),
            Err(DropDataError::InvalidUtf8)
        );
        assert_eq!(
            decode_drop_text(b"a\0".to_vec()),
            Err(DropDataError::ContainsNul)
        );
        assert_eq!(
            decode_drop_text(vec![b'x'; MAX_DRAG_TEXT_BYTES + 1]),
            Err(DropDataError::TooLarge)
        );
    }
}
