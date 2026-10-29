use std::collections::VecDeque;

use thing_ui::{TextInputImeBatch, TextInputPreedit, TextInputSurrounding};
use wayland_client::{
    Dispatch, QueueHandle,
    globals::GlobalList,
    protocol::{wl_seat, wl_surface},
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{ChangeCause as ProtocolChangeCause, ZwpTextInputV3},
};

const MAX_SURROUNDING_BYTES: usize = 4_000;
const MAX_COMMIT_SNAPSHOTS: usize = 64;
const MAX_INPUT_METHOD_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeCause {
    InputMethod,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CursorRectangle {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextState {
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) surrounding: TextInputSurrounding,
    pub(crate) cursor_rectangle: Option<CursorRectangle>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Enable,
    Disable,
    SetSurrounding {
        text: String,
        cursor: i32,
        anchor: i32,
    },
    SetChangeCause(ChangeCause),
    SetCursorRectangle(CursorRectangle),
    Commit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingError {
    InvalidPreeditCursor,
    TextTooLong,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdvertisedSurrounding {
    text: String,
    window_start: usize,
    anchor: usize,
    cursor: usize,
}

impl AdvertisedSurrounding {
    fn new(surrounding: &TextInputSurrounding) -> Option<Self> {
        let value = surrounding.value.as_str();
        if surrounding.anchor > value.len()
            || surrounding.cursor > value.len()
            || !value.is_char_boundary(surrounding.anchor)
            || !value.is_char_boundary(surrounding.cursor)
        {
            return None;
        }
        let selection_start = surrounding.anchor.min(surrounding.cursor);
        let selection_end = surrounding.anchor.max(surrounding.cursor);
        if selection_end - selection_start > MAX_SURROUNDING_BYTES {
            return None;
        }

        let selection_bytes = selection_end - selection_start;
        let mut window_start =
            selection_start.saturating_sub((MAX_SURROUNDING_BYTES - selection_bytes) / 2);
        while !value.is_char_boundary(window_start) {
            window_start += 1;
        }
        let mut window_end = value.len().min(window_start + MAX_SURROUNDING_BYTES);
        while !value.is_char_boundary(window_end) {
            window_end -= 1;
        }

        let remaining = MAX_SURROUNDING_BYTES - (window_end - window_start);
        let mut earlier_start = window_start.saturating_sub(remaining);
        while !value.is_char_boundary(earlier_start) {
            earlier_start += 1;
        }
        window_start = earlier_start;

        let remaining = MAX_SURROUNDING_BYTES - (window_end - window_start);
        window_end = value.len().min(window_end + remaining);
        while !value.is_char_boundary(window_end) {
            window_end -= 1;
        }

        debug_assert!(window_start <= selection_start);
        debug_assert!(window_end >= selection_end);
        Some(Self {
            text: value[window_start..window_end].to_owned(),
            window_start,
            anchor: surrounding.anchor - window_start,
            cursor: surrounding.cursor - window_start,
        })
    }

    fn accepts(
        &self,
        surrounding: &TextInputSurrounding,
        delete_before: usize,
        delete_after: usize,
    ) -> bool {
        let Some(window_end) = self.window_start.checked_add(self.text.len()) else {
            return false;
        };
        if window_end > surrounding.value.len()
            || surrounding.anchor != self.window_start + self.anchor
            || surrounding.cursor != self.window_start + self.cursor
            || surrounding
                .value
                .get(self.window_start..window_end)
                .is_none_or(|text| text != self.text)
        {
            return false;
        }

        let selection_start = self.anchor.min(self.cursor);
        let selection_end = self.anchor.max(self.cursor);
        let Some(delete_start) = selection_start.checked_sub(delete_before) else {
            return false;
        };
        let Some(delete_end) = selection_end.checked_add(delete_after) else {
            return false;
        };
        delete_end <= self.text.len()
            && self.text.is_char_boundary(delete_start)
            && self.text.is_char_boundary(delete_end)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SentState {
    generation: u64,
    revision: u64,
    surrounding: Option<AdvertisedSurrounding>,
    cursor_rectangle: Option<CursorRectangle>,
}

impl SentState {
    fn from_text_state(state: &TextState) -> Self {
        Self {
            generation: state.generation,
            revision: state.revision,
            surrounding: AdvertisedSurrounding::new(&state.surrounding),
            cursor_rectangle: state.cursor_rectangle,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommitSnapshot {
    serial: u32,
    state: Option<SentState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Done {
    pub(crate) serial: u32,
    pub(crate) generation: Option<u64>,
    pub(crate) batch: Result<TextInputImeBatch, PendingError>,
    pub(crate) synchronized: bool,
    snapshot: Option<SentState>,
}

impl Done {
    pub(crate) fn accepts(
        &self,
        generation: u64,
        revision: u64,
        surrounding: &TextInputSurrounding,
    ) -> bool {
        let Ok(batch) = &self.batch else {
            return false;
        };
        let Some(snapshot) = &self.snapshot else {
            return false;
        };
        if snapshot.generation != generation {
            return false;
        }
        if batch.delete_before == 0 && batch.delete_after == 0 {
            return true;
        }
        if snapshot.revision != revision {
            return false;
        }
        match &snapshot.surrounding {
            Some(advertised) => {
                advertised.accepts(surrounding, batch.delete_before, batch.delete_after)
            }
            None => batch.delete_before == 0 && batch.delete_after == 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingBatch {
    preedit: Result<Option<TextInputPreedit>, PendingError>,
    commit: Result<String, PendingError>,
    delete_before: usize,
    delete_after: usize,
}

impl Default for PendingBatch {
    fn default() -> Self {
        Self {
            preedit: Ok(None),
            commit: Ok(String::new()),
            delete_before: 0,
            delete_after: 0,
        }
    }
}

impl PendingBatch {
    fn take(&mut self) -> Result<TextInputImeBatch, PendingError> {
        let pending = std::mem::take(self);
        Ok(TextInputImeBatch {
            preedit: pending.preedit?,
            commit: pending.commit?,
            delete_before: pending.delete_before,
            delete_after: pending.delete_after,
        })
    }
}

pub(crate) struct State {
    commit_count: u32,
    synchronized: bool,
    sent: Option<SentState>,
    deferred_cause: Option<ChangeCause>,
    snapshots: VecDeque<CommitSnapshot>,
    pending: PendingBatch,
}

impl Default for State {
    fn default() -> Self {
        Self {
            commit_count: 0,
            synchronized: true,
            sent: None,
            deferred_cause: None,
            snapshots: VecDeque::new(),
            pending: PendingBatch::default(),
        }
    }
}

impl State {
    pub(crate) fn update(&mut self, state: Option<TextState>, cause: ChangeCause) -> Vec<Request> {
        let desired = state.as_ref().map(SentState::from_text_state);
        if self.lifecycle_changed(desired.as_ref()) {
            self.deferred_cause = None;
            return self.replace_lifecycle(desired, cause);
        }
        if desired == self.sent {
            self.deferred_cause = None;
            return Vec::new();
        }
        if !self.synchronized {
            self.deferred_cause = Some(merge_cause(self.deferred_cause, cause));
            return Vec::new();
        }

        let cause = merge_cause(self.deferred_cause.take(), cause);
        let desired = desired.expect("an unchanged lifecycle has an enabled text input");
        self.send_state(desired, cause)
    }

    fn lifecycle_changed(&self, desired: Option<&SentState>) -> bool {
        match (&self.sent, desired) {
            (None, None) => false,
            (Some(current), Some(desired)) => {
                current.generation != desired.generation
                    || current.surrounding.is_some() != desired.surrounding.is_some()
                    || current.cursor_rectangle.is_some() != desired.cursor_rectangle.is_some()
            }
            _ => true,
        }
    }

    fn replace_lifecycle(
        &mut self,
        desired: Option<SentState>,
        cause: ChangeCause,
    ) -> Vec<Request> {
        let mut requests = Vec::new();
        self.pending = PendingBatch::default();
        self.snapshots.clear();
        if self.sent.is_some() {
            requests.push(Request::Disable);
            self.push_commit(None, &mut requests);
        }
        if let Some(desired) = desired {
            requests.push(Request::Enable);
            append_full_state(&desired, cause, &mut requests);
            self.push_commit(Some(desired.clone()), &mut requests);
            self.sent = Some(desired);
        } else {
            self.sent = None;
        }
        requests
    }

    fn send_state(&mut self, desired: SentState, cause: ChangeCause) -> Vec<Request> {
        let mut requests = Vec::new();
        let current = self
            .sent
            .as_ref()
            .expect("ordinary updates have an enabled text input");
        let surrounding_changed =
            current.revision != desired.revision || current.surrounding != desired.surrounding;
        if surrounding_changed && let Some(surrounding) = &desired.surrounding {
            requests.push(Request::SetSurrounding {
                text: surrounding.text.clone(),
                cursor: surrounding.cursor as i32,
                anchor: surrounding.anchor as i32,
            });
            if cause == ChangeCause::Other {
                requests.push(Request::SetChangeCause(cause));
            }
        }
        if current.cursor_rectangle != desired.cursor_rectangle
            && let Some(rectangle) = desired.cursor_rectangle
        {
            requests.push(Request::SetCursorRectangle(rectangle));
        }
        if requests.is_empty() {
            self.sent = Some(desired);
            return requests;
        }
        self.push_commit(Some(desired.clone()), &mut requests);
        self.sent = Some(desired);
        requests
    }

    fn push_commit(&mut self, state: Option<SentState>, requests: &mut Vec<Request>) {
        self.commit_count = self.commit_count.wrapping_add(1);
        self.snapshots.push_back(CommitSnapshot {
            serial: self.commit_count,
            state,
        });
        if self.snapshots.len() > MAX_COMMIT_SNAPSHOTS {
            self.snapshots.pop_front();
        }
        requests.push(Request::Commit);
    }

    pub(crate) fn preedit_string(
        &mut self,
        text: Option<String>,
        cursor_begin: i32,
        cursor_end: i32,
    ) {
        let text = text.unwrap_or_default();
        if text.len() > MAX_INPUT_METHOD_TEXT_BYTES {
            self.pending.preedit = Err(PendingError::TextTooLong);
            return;
        }
        let cursor = if cursor_begin == -1 && cursor_end == -1 {
            Ok(None)
        } else if cursor_begin >= 0 && cursor_end >= 0 {
            let cursor = (cursor_begin as usize, cursor_end as usize);
            if text.is_char_boundary(cursor.0) && text.is_char_boundary(cursor.1) {
                Ok(Some(cursor))
            } else {
                Err(PendingError::InvalidPreeditCursor)
            }
        } else {
            Err(PendingError::InvalidPreeditCursor)
        };
        self.pending.preedit =
            cursor.map(|cursor| (!text.is_empty()).then_some(TextInputPreedit { text, cursor }));
    }

    pub(crate) fn commit_string(&mut self, text: Option<String>) {
        let text = text.unwrap_or_default();
        self.pending.commit = if text.len() <= MAX_INPUT_METHOD_TEXT_BYTES {
            Ok(text)
        } else {
            Err(PendingError::TextTooLong)
        };
    }

    pub(crate) fn delete_surrounding_text(&mut self, before: u32, after: u32) {
        self.pending.delete_before = before as usize;
        self.pending.delete_after = after as usize;
    }

    pub(crate) fn done(&mut self, serial: u32) -> Done {
        let synchronized = serial == self.commit_count;
        self.synchronized = synchronized;
        let snapshot = self
            .snapshots
            .iter()
            .rev()
            .find(|snapshot| snapshot.serial == serial)
            .and_then(|snapshot| snapshot.state.clone());
        Done {
            serial,
            generation: snapshot.as_ref().map(|state| state.generation),
            batch: self.pending.take(),
            synchronized,
            snapshot,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.sent = None;
        self.deferred_cause = None;
        self.snapshots.clear();
        self.pending = PendingBatch::default();
    }
}

pub(crate) struct InputMethod {
    manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    entered: Option<wl_surface::WlSurface>,
    state: State,
}

impl InputMethod {
    pub(crate) fn bind<D>(globals: &GlobalList, qh: &QueueHandle<D>) -> Self
    where
        D: Dispatch<ZwpTextInputManagerV3, ()> + 'static,
    {
        Self {
            manager: globals.bind(qh, 1..=1, ()).ok(),
            text_input: None,
            entered: None,
            state: State::default(),
        }
    }

    pub(crate) fn attach_seat<D>(&mut self, seat: &wl_seat::WlSeat, qh: &QueueHandle<D>)
    where
        D: Dispatch<ZwpTextInputV3, ()> + 'static,
    {
        if self.text_input.is_some() {
            return;
        }
        let Some(manager) = &self.manager else {
            return;
        };
        self.text_input = Some(manager.get_text_input(seat, qh, ()));
        self.entered = None;
        self.state = State::default();
    }

    pub(crate) fn detach_seat(&mut self) {
        if let Some(text_input) = self.text_input.take() {
            text_input.destroy();
        }
        self.entered = None;
        self.state = State::default();
    }

    pub(crate) fn enter(&mut self, surface: wl_surface::WlSurface) {
        self.entered = Some(surface);
        self.state.reset();
    }

    pub(crate) fn leave(&mut self, surface: &wl_surface::WlSurface) -> bool {
        if self.entered.as_ref() != Some(surface) {
            return false;
        }
        self.entered = None;
        self.state.reset();
        true
    }

    pub(crate) fn entered_surface(&self) -> Option<&wl_surface::WlSurface> {
        self.entered.as_ref()
    }

    pub(crate) fn update(&mut self, state: Option<TextState>, cause: ChangeCause) {
        if self.text_input.is_none() || self.entered.is_none() {
            self.state.reset();
            return;
        }
        let requests = self.state.update(state, cause);
        let Some(text_input) = &self.text_input else {
            return;
        };
        for request in requests {
            match request {
                Request::Enable => text_input.enable(),
                Request::Disable => text_input.disable(),
                Request::SetSurrounding {
                    text,
                    cursor,
                    anchor,
                } => text_input.set_surrounding_text(text, cursor, anchor),
                Request::SetChangeCause(cause) => {
                    let cause = match cause {
                        ChangeCause::InputMethod => ProtocolChangeCause::InputMethod,
                        ChangeCause::Other => ProtocolChangeCause::Other,
                    };
                    text_input.set_text_change_cause(cause);
                }
                Request::SetCursorRectangle(rectangle) => text_input.set_cursor_rectangle(
                    rectangle.x,
                    rectangle.y,
                    rectangle.width,
                    rectangle.height,
                ),
                Request::Commit => text_input.commit(),
            }
        }
    }

    pub(crate) fn preedit_string(
        &mut self,
        text: Option<String>,
        cursor_begin: i32,
        cursor_end: i32,
    ) {
        self.state.preedit_string(text, cursor_begin, cursor_end);
    }

    pub(crate) fn commit_string(&mut self, text: Option<String>) {
        self.state.commit_string(text);
    }

    pub(crate) fn delete_surrounding_text(&mut self, before: u32, after: u32) {
        self.state.delete_surrounding_text(before, after);
    }

    pub(crate) fn done(&mut self, serial: u32) -> Done {
        self.state.done(serial)
    }
}

impl Drop for InputMethod {
    fn drop(&mut self) {
        self.detach_seat();
        if let Some(manager) = self.manager.take() {
            manager.destroy();
        }
    }
}

fn append_full_state(state: &SentState, cause: ChangeCause, requests: &mut Vec<Request>) {
    if let Some(surrounding) = &state.surrounding {
        requests.push(Request::SetSurrounding {
            text: surrounding.text.clone(),
            cursor: surrounding.cursor as i32,
            anchor: surrounding.anchor as i32,
        });
    }
    if cause == ChangeCause::Other {
        requests.push(Request::SetChangeCause(cause));
    }
    if let Some(rectangle) = state.cursor_rectangle {
        requests.push(Request::SetCursorRectangle(rectangle));
    }
}

fn merge_cause(previous: Option<ChangeCause>, next: ChangeCause) -> ChangeCause {
    if previous == Some(ChangeCause::Other) || next == ChangeCause::Other {
        ChangeCause::Other
    } else {
        ChangeCause::InputMethod
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        os::unix::net::UnixStream,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };
    use wayland_client::{
        Connection,
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_compositor, wl_registry},
    };
    use wayland_protocols::wp::text_input::zv3::server::{
        zwp_text_input_manager_v3::ZwpTextInputManagerV3 as ServerTextInputManager,
        zwp_text_input_v3::ZwpTextInputV3 as ServerTextInput,
    };
    use wayland_server::{
        Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
        Resource,
        protocol::{
            wl_compositor::WlCompositor as ServerCompositor, wl_region::WlRegion as ServerRegion,
            wl_seat::WlSeat as ServerSeat, wl_surface::WlSurface as ServerSurface,
        },
    };

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum WireRequest {
        Version(u32),
        Enable,
        Surrounding(String, i32, i32),
        Cause,
        Rectangle(i32, i32, i32, i32),
        Commit,
        Destroy,
    }

    struct WireClient {
        input_method: Option<InputMethod>,
        done: Vec<Done>,
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WireClient {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwpTextInputV3, ()> for WireClient {
        fn event(
            client: &mut Self,
            _: &ZwpTextInputV3,
            event: wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let input_method = client.input_method.as_mut().unwrap();
            match event {
                wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event::Enter {
                    surface,
                } => input_method.enter(surface),
                wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event::PreeditString {
                    text,
                    cursor_begin,
                    cursor_end,
                } => input_method.preedit_string(text, cursor_begin, cursor_end),
                wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event::CommitString {
                    text,
                } => input_method.commit_string(text),
                wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event::DeleteSurroundingText {
                    before_length,
                    after_length,
                } => input_method.delete_surrounding_text(before_length, after_length),
                wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event::Done {
                    serial,
                } => client.done.push(input_method.done(serial)),
                _ => {}
            }
        }
    }

    wayland_client::delegate_noop!(WireClient: ignore wl_compositor::WlCompositor);
    wayland_client::delegate_noop!(WireClient: ignore wl_surface::WlSurface);
    wayland_client::delegate_noop!(WireClient: ignore wl_seat::WlSeat);
    wayland_client::delegate_noop!(WireClient: ignore ZwpTextInputManagerV3);

    struct WireServer {
        records: Arc<Mutex<Vec<WireRequest>>>,
        surface: Option<ServerSurface>,
        commits: u32,
        replied: bool,
    }

    impl GlobalDispatch<ServerCompositor, ()> for WireServer {
        fn bind(
            _: &mut Self,
            _: &DisplayHandle,
            _: &Client,
            resource: New<ServerCompositor>,
            _: &(),
            data: &mut DataInit<'_, Self>,
        ) {
            data.init(resource, ());
        }
    }

    impl ServerDispatch<ServerCompositor, ()> for WireServer {
        fn request(
            state: &mut Self,
            _: &Client,
            _: &ServerCompositor,
            request: <ServerCompositor as Resource>::Request,
            _: &(),
            _: &DisplayHandle,
            data: &mut DataInit<'_, Self>,
        ) {
            match request {
                wayland_server::protocol::wl_compositor::Request::CreateSurface { id } => {
                    state.surface = Some(data.init(id, ()));
                }
                wayland_server::protocol::wl_compositor::Request::CreateRegion { id } => {
                    data.init(id, ());
                }
                _ => {}
            }
        }
    }

    impl ServerDispatch<ServerSurface, ()> for WireServer {
        fn request(
            _: &mut Self,
            _: &Client,
            _: &ServerSurface,
            _: <ServerSurface as Resource>::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl ServerDispatch<ServerRegion, ()> for WireServer {
        fn request(
            _: &mut Self,
            _: &Client,
            _: &ServerRegion,
            _: <ServerRegion as Resource>::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl GlobalDispatch<ServerSeat, ()> for WireServer {
        fn bind(
            _: &mut Self,
            _: &DisplayHandle,
            _: &Client,
            resource: New<ServerSeat>,
            _: &(),
            data: &mut DataInit<'_, Self>,
        ) {
            data.init(resource, ());
        }
    }

    impl ServerDispatch<ServerSeat, ()> for WireServer {
        fn request(
            _: &mut Self,
            _: &Client,
            _: &ServerSeat,
            _: <ServerSeat as Resource>::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl GlobalDispatch<ServerTextInputManager, ()> for WireServer {
        fn bind(
            _: &mut Self,
            _: &DisplayHandle,
            _: &Client,
            resource: New<ServerTextInputManager>,
            _: &(),
            data: &mut DataInit<'_, Self>,
        ) {
            data.init(resource, ());
        }
    }

    impl ServerDispatch<ServerTextInputManager, ()> for WireServer {
        fn request(
            state: &mut Self,
            _: &Client,
            _: &ServerTextInputManager,
            request: <ServerTextInputManager as Resource>::Request,
            _: &(),
            _: &DisplayHandle,
            data: &mut DataInit<'_, Self>,
        ) {
            if let wayland_protocols::wp::text_input::zv3::server::zwp_text_input_manager_v3::Request::GetTextInput {
                id,
                ..
            } = request
            {
                let text_input: ServerTextInput = data.init(id, ());
                state
                    .records
                    .lock()
                    .unwrap()
                    .push(WireRequest::Version(text_input.version()));
                text_input.enter(state.surface.as_ref().unwrap());
            }
        }
    }

    impl ServerDispatch<ServerTextInput, ()> for WireServer {
        fn request(
            state: &mut Self,
            _: &Client,
            text_input: &ServerTextInput,
            request: <ServerTextInput as Resource>::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
            use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::Request;

            let record = match request {
                Request::Enable => Some(WireRequest::Enable),
                Request::SetSurroundingText {
                    text,
                    cursor,
                    anchor,
                } => Some(WireRequest::Surrounding(text, cursor, anchor)),
                Request::SetTextChangeCause { .. } => Some(WireRequest::Cause),
                Request::SetCursorRectangle {
                    x,
                    y,
                    width,
                    height,
                } => Some(WireRequest::Rectangle(x, y, width, height)),
                Request::Commit => {
                    state.commits = state.commits.wrapping_add(1);
                    if !state.replied {
                        state.replied = true;
                        text_input.preedit_string(Some("候補".into()), 6, 0);
                        text_input.commit_string(Some("入力".into()));
                        text_input.done(state.commits);
                    }
                    Some(WireRequest::Commit)
                }
                Request::Destroy => Some(WireRequest::Destroy),
                _ => None,
            };
            if let Some(record) = record {
                state.records.lock().unwrap().push(record);
            }
        }
    }

    struct PrivateServer {
        globals: GlobalList,
        queue: wayland_client::EventQueue<WireClient>,
        compositor: wl_compositor::WlCompositor,
        seat: wl_seat::WlSeat,
        records: Arc<Mutex<Vec<WireRequest>>>,
        stop: Arc<AtomicBool>,
        server: Option<thread::JoinHandle<()>>,
    }

    impl Drop for PrivateServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(server) = self.server.take() {
                server.join().unwrap();
            }
        }
    }

    fn private_server() -> PrivateServer {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        server_socket.set_nonblocking(true).unwrap();
        let mut display = Display::<WireServer>::new().unwrap();
        let mut handle = display.handle();
        let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
        handle.create_global::<WireServer, ServerCompositor, _>(6, ());
        handle.create_global::<WireServer, ServerSeat, _>(9, ());
        handle.create_global::<WireServer, ServerTextInputManager, _>(2, ());
        let records = Arc::new(Mutex::new(Vec::new()));
        let server_records = records.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = WireServer {
                records: server_records,
                surface: None,
                commits: 0,
                replied: false,
            };
            while !server_stop.load(Ordering::Relaxed) {
                match display
                    .backend()
                    .dispatch_single_client(&mut state, client.id())
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("private wayland server failed: {error}"),
                }
                display.backend().flush(Some(client.id())).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });

        let connection = Connection::from_socket(client_socket).unwrap();
        let (globals, queue) = registry_queue_init(&connection).unwrap();
        let qh = queue.handle();
        let compositor = globals.bind(&qh, 1..=6, ()).unwrap();
        let seat = globals.bind(&qh, 1..=9, ()).unwrap();
        PrivateServer {
            globals,
            queue,
            compositor,
            seat,
            records,
            stop,
            server: Some(server),
        }
    }

    #[test]
    fn private_server_observes_v1_state_and_returns_one_atomic_batch() {
        let mut server = private_server();
        let qh = server.queue.handle();
        let surface = server.compositor.create_surface(&qh, ());
        let mut input_method = InputMethod::bind(&server.globals, &qh);
        input_method.attach_seat(&server.seat, &qh);
        let mut client = WireClient {
            input_method: Some(input_method),
            done: Vec::new(),
        };
        server.queue.roundtrip(&mut client).unwrap();
        assert_eq!(
            client.input_method.as_ref().unwrap().entered_surface(),
            Some(&surface)
        );

        let state = text_state(1, 0, "old", 3);
        client
            .input_method
            .as_mut()
            .unwrap()
            .update(Some(state.clone()), ChangeCause::Other);
        server.queue.roundtrip(&mut client).unwrap();

        assert_eq!(
            *server.records.lock().unwrap(),
            vec![
                WireRequest::Version(1),
                WireRequest::Enable,
                WireRequest::Surrounding("old".into(), 3, 3),
                WireRequest::Cause,
                WireRequest::Rectangle(0, 2, 1, 18),
                WireRequest::Commit,
            ]
        );
        assert_eq!(client.done.len(), 1);
        let done = client.done.pop().unwrap();
        assert!(done.accepts(1, 0, &state.surrounding));
        let batch = done.batch.unwrap();
        assert_eq!(batch.commit, "入力");
        assert_eq!(
            batch.preedit,
            Some(TextInputPreedit {
                text: "候補".into(),
                cursor: Some((6, 0)),
            })
        );
        client.input_method.as_mut().unwrap().detach_seat();
        server.queue.roundtrip(&mut client).unwrap();
        assert_eq!(
            server.records.lock().unwrap().last(),
            Some(&WireRequest::Destroy)
        );
        drop(server);
    }

    fn text_state(generation: u64, revision: u64, value: &str, cursor: usize) -> TextState {
        TextState {
            generation,
            revision,
            surrounding: TextInputSurrounding {
                value: value.into(),
                anchor: cursor,
                cursor,
            },
            cursor_rectangle: Some(CursorRectangle {
                x: revision as i32,
                y: 2,
                width: 1,
                height: 18,
            }),
        }
    }

    fn commit_count(requests: &[Request]) -> usize {
        requests
            .iter()
            .filter(|request| **request == Request::Commit)
            .count()
    }

    #[test]
    fn surrounding_window_is_bounded_and_preserves_reversed_selection() {
        let value = format!("{}é{}", "a".repeat(5_000), "z".repeat(5_000));
        let surrounding = TextInputSurrounding {
            value,
            anchor: 5_002,
            cursor: 5_000,
        };
        let advertised = AdvertisedSurrounding::new(&surrounding).unwrap();

        assert!(advertised.text.len() <= MAX_SURROUNDING_BYTES);
        assert!(advertised.text.is_char_boundary(advertised.anchor));
        assert!(advertised.text.is_char_boundary(advertised.cursor));
        assert!(advertised.anchor > advertised.cursor);
        assert!(advertised.accepts(&surrounding, 1, 1));
        assert!(!advertised.accepts(&surrounding, advertised.cursor + 1, 0));
    }

    #[test]
    fn surrounding_window_rejects_an_oversized_selection() {
        let surrounding = TextInputSurrounding {
            value: "x".repeat(MAX_SURROUNDING_BYTES + 1),
            anchor: 0,
            cursor: MAX_SURROUNDING_BYTES + 1,
        };
        assert!(AdvertisedSurrounding::new(&surrounding).is_none());
    }

    #[test]
    fn initial_enable_and_ordinary_updates_commit_full_state() {
        let mut state = State::default();
        let enabled = state.update(Some(text_state(1, 0, "one", 3)), ChangeCause::Other);
        assert_eq!(enabled.first(), Some(&Request::Enable));
        assert_eq!(commit_count(&enabled), 1);
        assert!(enabled.contains(&Request::SetChangeCause(ChangeCause::Other)));

        let second = state.update(Some(text_state(1, 1, "two", 3)), ChangeCause::Other);
        assert_eq!(commit_count(&second), 1);
        assert!(second.contains(&Request::SetChangeCause(ChangeCause::Other)));
        let third = state.update(Some(text_state(1, 2, "three", 5)), ChangeCause::InputMethod);
        assert_eq!(commit_count(&third), 1);
    }

    #[test]
    fn cursor_rectangle_only_updates_do_not_report_a_text_change() {
        let mut state = State::default();
        let initial = text_state(1, 0, "value", 5);
        state.update(Some(initial.clone()), ChangeCause::InputMethod);
        let mut moved = initial;
        moved.cursor_rectangle.as_mut().unwrap().x += 10;

        let requests = state.update(Some(moved), ChangeCause::Other);
        assert_eq!(commit_count(&requests), 1);
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, Request::SetCursorRectangle(_)))
        );
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, Request::SetSurrounding { .. }))
        );
        assert!(!requests.contains(&Request::SetChangeCause(ChangeCause::Other)));
    }

    #[test]
    fn cursor_availability_restarts_the_protocol_lifecycle_without_stale_geometry() {
        let mut state = State::default();
        let initial = text_state(1, 0, "value", 5);
        state.update(Some(initial.clone()), ChangeCause::InputMethod);

        let mut unavailable = initial.clone();
        unavailable.cursor_rectangle = None;
        let requests = state.update(Some(unavailable), ChangeCause::InputMethod);
        assert_eq!(requests.first(), Some(&Request::Disable));
        assert!(requests.contains(&Request::Enable));
        assert_eq!(commit_count(&requests), 2);
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, Request::SetCursorRectangle(_)))
        );

        let requests = state.update(Some(initial.clone()), ChangeCause::InputMethod);
        assert_eq!(requests.first(), Some(&Request::Disable));
        assert!(requests.contains(&Request::Enable));
        assert_eq!(commit_count(&requests), 2);
        assert!(requests.contains(&Request::SetCursorRectangle(
            initial.cursor_rectangle.unwrap()
        )));
    }

    #[test]
    fn mismatch_defers_updates_until_a_matching_done() {
        let mut state = State::default();
        state.update(Some(text_state(1, 0, "zero", 4)), ChangeCause::InputMethod);
        state.update(Some(text_state(1, 1, "one", 3)), ChangeCause::InputMethod);
        state.update(Some(text_state(1, 2, "two", 3)), ChangeCause::InputMethod);

        let mismatch = state.done(2);
        assert!(!mismatch.synchronized);
        assert!(
            state
                .update(Some(text_state(1, 3, "three", 5)), ChangeCause::Other)
                .is_empty()
        );

        let matched = state.done(3);
        assert!(matched.synchronized);
        let flushed = state.update(Some(text_state(1, 3, "three", 5)), ChangeCause::InputMethod);
        assert_eq!(commit_count(&flushed), 1);
        assert!(flushed.contains(&Request::SetChangeCause(ChangeCause::Other)));
        assert!(
            state
                .update(Some(text_state(1, 3, "three", 5)), ChangeCause::InputMethod)
                .is_empty()
        );
    }

    #[test]
    fn a_deferred_change_that_reverts_does_not_leak_its_cause() {
        let mut state = State::default();
        let original = text_state(1, 0, "zero", 4);
        state.update(Some(original.clone()), ChangeCause::InputMethod);
        state.update(Some(text_state(1, 1, "one", 3)), ChangeCause::InputMethod);
        state.done(1);
        state.update(Some(text_state(1, 2, "external", 8)), ChangeCause::Other);
        state.update(Some(text_state(1, 1, "one", 3)), ChangeCause::InputMethod);
        state.done(2);
        assert!(
            state
                .update(Some(text_state(1, 1, "one", 3)), ChangeCause::InputMethod)
                .is_empty()
        );

        let requests = state.update(Some(text_state(1, 3, "ime", 3)), ChangeCause::InputMethod);
        assert!(!requests.contains(&Request::SetChangeCause(ChangeCause::Other)));
    }

    #[test]
    fn field_switch_bypasses_a_deferred_ordinary_update() {
        let mut state = State::default();
        state.update(Some(text_state(1, 0, "old", 3)), ChangeCause::InputMethod);
        state.update(Some(text_state(1, 1, "older", 5)), ChangeCause::InputMethod);
        state.done(1);
        assert!(
            state
                .update(Some(text_state(1, 2, "stale", 5)), ChangeCause::Other)
                .is_empty()
        );

        let switched = state.update(Some(text_state(2, 0, "new", 3)), ChangeCause::InputMethod);
        assert_eq!(switched.first(), Some(&Request::Disable));
        assert!(switched.contains(&Request::Enable));
        assert_eq!(commit_count(&switched), 2);

        let old = state.done(2);
        assert_eq!(old.generation, None);
        let current = state.done(4);
        assert_eq!(current.generation, Some(2));
        assert!(current.synchronized);
    }

    #[test]
    fn pending_fields_reset_after_every_done() {
        let mut state = State::default();
        state.update(Some(text_state(1, 0, "value", 5)), ChangeCause::InputMethod);
        state.preedit_string(Some("候補".into()), 6, 0);
        state.commit_string(Some("committed".into()));
        state.delete_surrounding_text(2, 3);
        let first = state.done(1).batch.unwrap();
        assert_eq!(first.preedit.unwrap().cursor, Some((6, 0)));
        assert_eq!(first.commit, "committed");
        assert_eq!((first.delete_before, first.delete_after), (2, 3));

        assert_eq!(state.done(1).batch.unwrap(), TextInputImeBatch::default());
        state.preedit_string(Some("invalid".into()), -1, 0);
        assert_eq!(state.done(1).batch, Err(PendingError::InvalidPreeditCursor));
        assert_eq!(state.done(1).batch.unwrap(), TextInputImeBatch::default());
    }

    #[test]
    fn lifecycle_changes_discard_old_pending_events() {
        let mut state = State::default();
        state.update(Some(text_state(1, 0, "old", 3)), ChangeCause::InputMethod);
        state.preedit_string(Some("stale".into()), 5, 5);
        state.commit_string(Some("stale".into()));
        state.delete_surrounding_text(1, 1);
        state.update(Some(text_state(2, 0, "new", 3)), ChangeCause::InputMethod);

        assert_eq!(state.done(1).generation, None);
        let current = state.done(3);
        assert_eq!(current.generation, Some(2));
        assert_eq!(current.batch.unwrap(), TextInputImeBatch::default());
    }

    #[test]
    fn preedit_cursor_validation_applies_to_empty_and_utf8_text() {
        let mut state = State::default();
        for (text, begin, end) in [
            (Some(String::new()), -1, 0),
            (Some(String::new()), 4, 4),
            (Some("é".into()), 1, 2),
        ] {
            state.preedit_string(text, begin, end);
            assert_eq!(state.done(0).batch, Err(PendingError::InvalidPreeditCursor));
        }

        state.preedit_string(Some("候補".into()), 6, 0);
        assert_eq!(
            state.done(0).batch.unwrap().preedit.unwrap().cursor,
            Some((6, 0))
        );
        state.preedit_string(Some(String::new()), 0, 0);
        assert_eq!(state.done(0).batch.unwrap(), TextInputImeBatch::default());
    }

    #[test]
    fn pending_input_method_text_is_bounded() {
        let mut state = State::default();
        state.preedit_string(Some("x".repeat(MAX_INPUT_METHOD_TEXT_BYTES + 1)), 0, 0);
        assert_eq!(state.done(0).batch, Err(PendingError::TextTooLong));
        state.commit_string(Some("x".repeat(MAX_INPUT_METHOD_TEXT_BYTES + 1)));
        assert_eq!(state.done(0).batch, Err(PendingError::TextTooLong));
    }

    #[test]
    fn done_validates_exact_generation_revision_and_window() {
        let mut state = State::default();
        let initial = text_state(7, 4, "aéXz", 3);
        state.update(Some(initial.clone()), ChangeCause::InputMethod);
        state.delete_surrounding_text(2, 1);
        let done = state.done(1);
        assert!(done.accepts(7, 4, &initial.surrounding));
        assert!(!done.accepts(8, 4, &initial.surrounding));
        assert!(!done.accepts(7, 5, &initial.surrounding));

        let mut changed = initial.surrounding;
        changed.value.replace_range(0..1, "b");
        assert!(!done.accepts(7, 4, &changed));
    }

    #[test]
    fn mismatched_commit_and_preedit_do_not_require_an_old_revision() {
        let mut state = State::default();
        let initial = text_state(1, 0, "old", 3);
        let current = text_state(1, 1, "current", 7);
        state.update(Some(initial), ChangeCause::InputMethod);
        state.update(Some(current.clone()), ChangeCause::Other);
        state.preedit_string(Some("x".into()), 1, 1);
        state.commit_string(Some("y".into()));
        let done = state.done(1);
        assert!(!done.synchronized);
        assert!(done.accepts(1, 1, &current.surrounding));

        let mut deletes = State::default();
        deletes.update(Some(text_state(1, 0, "old", 3)), ChangeCause::InputMethod);
        deletes.update(Some(current.clone()), ChangeCause::Other);
        deletes.delete_surrounding_text(1, 0);
        assert!(!deletes.done(1).accepts(1, 1, &current.surrounding));
    }

    #[test]
    fn surrounding_capability_changes_with_a_new_enable_generation() {
        let mut state = State::default();
        state.update(Some(text_state(1, 0, "short", 5)), ChangeCause::InputMethod);
        let mut long_selection = text_state(
            1,
            1,
            &"x".repeat(MAX_SURROUNDING_BYTES + 1),
            MAX_SURROUNDING_BYTES + 1,
        );
        long_selection.surrounding.anchor = 0;
        let requests = state.update(Some(long_selection), ChangeCause::Other);
        assert_eq!(requests.first(), Some(&Request::Disable));
        assert!(requests.contains(&Request::Enable));
        assert_eq!(commit_count(&requests), 2);
        assert!(requests.contains(&Request::SetChangeCause(ChangeCause::Other)));
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, Request::SetSurrounding { .. }))
        );
    }

    #[test]
    fn evicted_and_disabled_snapshots_fail_closed() {
        let mut state = State::default();
        state.update(Some(text_state(1, 0, "zero", 4)), ChangeCause::InputMethod);
        for revision in 1..=MAX_COMMIT_SNAPSHOTS as u64 {
            state.update(
                Some(text_state(1, revision, "value", 5)),
                ChangeCause::Other,
            );
        }
        assert!(!state.done(1).accepts(
            1,
            0,
            &TextInputSurrounding {
                value: "zero".into(),
                anchor: 4,
                cursor: 4,
            }
        ));

        let disabled = state.update(None, ChangeCause::Other);
        assert_eq!(commit_count(&disabled), 1);
        let done = state.done(state.commit_count);
        assert_eq!(done.generation, None);
        assert!(!done.accepts(
            1,
            0,
            &TextInputSurrounding {
                value: String::new(),
                anchor: 0,
                cursor: 0,
            }
        ));
    }

    #[test]
    fn reset_clears_pending_and_requires_a_fresh_enable() {
        let mut state = State::default();
        state.update(Some(text_state(1, 0, "old", 3)), ChangeCause::InputMethod);
        state.preedit_string(Some("stale".into()), 5, 5);
        state.reset();
        assert_eq!(state.done(1).generation, None);

        let enabled = state.update(Some(text_state(2, 0, "new", 3)), ChangeCause::InputMethod);
        assert_eq!(enabled.first(), Some(&Request::Enable));
        assert!(!enabled.contains(&Request::Disable));
        let current = state.done(2);
        assert_eq!(current.generation, Some(2));
        assert_eq!(current.batch.unwrap(), TextInputImeBatch::default());
    }

    #[test]
    fn commit_serials_wrap_without_aliasing_the_latest_snapshot() {
        let mut state = State {
            commit_count: u32::MAX - 1,
            ..State::default()
        };
        state.update(Some(text_state(1, 0, "max", 3)), ChangeCause::InputMethod);
        state.update(Some(text_state(1, 1, "zero", 4)), ChangeCause::InputMethod);

        let older = state.done(u32::MAX);
        assert!(!older.synchronized);
        assert_eq!(older.generation, Some(1));
        assert!(older.accepts(
            1,
            0,
            &TextInputSurrounding {
                value: "max".into(),
                anchor: 3,
                cursor: 3,
            }
        ));
        let latest = state.done(0);
        assert!(latest.synchronized);
        assert_eq!(latest.generation, Some(1));
    }
}
