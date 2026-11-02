//! drag and drop against a private in-process wayland server.

use super::*;
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};
use thing_runtime::MAX_DRAG_TEXT_BYTES;
use wayland_client::{
    Connection, EventQueue, event_created_child,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_compositor, wl_data_device, wl_data_offer, wl_data_source, wl_registry, wl_seat,
        wl_surface,
    },
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource,
    protocol::{
        wl_compositor::{self as server_compositor, WlCompositor as ServerCompositor},
        wl_data_device::{self as server_data_device, WlDataDevice as ServerDataDevice},
        wl_data_device_manager::{
            self as server_data_device_manager, DndAction as ServerAction,
            WlDataDeviceManager as ServerDataDeviceManager,
        },
        wl_data_offer::{self as server_data_offer, WlDataOffer as ServerDataOffer},
        wl_data_source::{self as server_data_source, WlDataSource as ServerDataSource},
        wl_seat::{self as server_seat, WlSeat as ServerSeat},
        wl_surface::{self as server_surface, WlSurface as ServerSurface},
    },
};

use crate::focused_clipboard::FocusedClipboard;
use crate::text_data::TEXT_MIME_TYPES;

#[derive(Default)]
struct Observed {
    accepts: Vec<(u32, Option<String>)>,
    set_actions: Vec<(u32, u32)>,
    receives: Vec<String>,
    finishes: usize,
    offer_destroys: usize,
    start_serials: Vec<u32>,
    source_mimes: Vec<String>,
    source_actions: Vec<u32>,
    source_destroys: usize,
    /// writers held open by a stalled source.
    stalled: Vec<OwnedFd>,
}

enum Command {
    Enter {
        serial: u32,
        mime_types: Vec<String>,
        actions: u32,
        bytes: Vec<u8>,
        stall: bool,
    },
    Motion,
    OfferAction(u32),
    Leave,
    Drop,
    SourceSend(String, mpsc::Sender<OwnedFd>),
    SourceAction(u32),
    SourceFinished,
    SourceCancelled,
}

struct OfferFixture {
    bytes: Vec<u8>,
    stall: bool,
}

struct ServerState {
    observed: Arc<Mutex<Observed>>,
    device: Option<ServerDataDevice>,
    surface: Option<ServerSurface>,
    offer: Option<ServerDataOffer>,
    source: Option<ServerDataSource>,
}

impl ServerState {
    fn run(&mut self, command: Command, handle: &DisplayHandle) {
        match command {
            Command::Enter {
                serial,
                mime_types,
                actions,
                bytes,
                stall,
            } => {
                let device = self.device.clone().unwrap();
                let client = handle.get_client(device.id()).unwrap();
                let offer = client
                    .create_resource::<ServerDataOffer, _, Self>(
                        handle,
                        device.version(),
                        OfferFixture { bytes, stall },
                    )
                    .unwrap();
                device.data_offer(&offer);
                for mime_type in mime_types {
                    offer.offer(mime_type);
                }
                if offer.version() >= 3 {
                    offer.source_actions(ServerAction::from_bits_truncate(actions));
                }
                device.enter(
                    serial,
                    self.surface.as_ref().unwrap(),
                    4.0,
                    5.0,
                    Some(&offer),
                );
                self.offer = Some(offer);
            }
            Command::Motion => self.device.as_ref().unwrap().motion(1, 6.0, 7.0),
            Command::OfferAction(action) => self
                .offer
                .as_ref()
                .unwrap()
                .action(ServerAction::from_bits_truncate(action)),
            Command::Leave => self.device.as_ref().unwrap().leave(),
            Command::Drop => self.device.as_ref().unwrap().drop(),
            Command::SourceSend(mime_type, reply) => {
                let (reader, writer) = pipe().unwrap();
                self.source
                    .as_ref()
                    .unwrap()
                    .send(mime_type, writer.as_fd());
                drop(writer);
                reply.send(reader).unwrap();
            }
            Command::SourceAction(action) => self
                .source
                .as_ref()
                .unwrap()
                .action(ServerAction::from_bits_truncate(action)),
            Command::SourceFinished => self.source.as_ref().unwrap().dnd_finished(),
            Command::SourceCancelled => self.source.as_ref().unwrap().cancelled(),
        }
    }
}

macro_rules! bind_global {
    ($interface:ty) => {
        impl GlobalDispatch<$interface, ()> for ServerState {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$interface>,
                _: &(),
                data: &mut DataInit<'_, Self>,
            ) {
                data.init(resource, ());
            }
        }
    };
}

bind_global!(ServerDataDeviceManager);
bind_global!(ServerSeat);
bind_global!(ServerCompositor);

impl ServerDispatch<ServerDataDeviceManager, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerDataDeviceManager,
        request: server_data_device_manager::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_data_device_manager::Request::CreateDataSource { id } => {
                data.init(id, ());
            }
            server_data_device_manager::Request::GetDataDevice { id, .. } => {
                state.device = Some(data.init(id, ()));
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ServerSeat, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerSeat,
        _: server_seat::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl ServerDispatch<ServerCompositor, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerCompositor,
        request: server_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_compositor::Request::CreateSurface { id } = request {
            state.surface = Some(data.init(id, ()));
        }
    }
}

impl ServerDispatch<ServerSurface, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerSurface,
        _: server_surface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl ServerDispatch<ServerDataDevice, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerDataDevice,
        request: server_data_device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let server_data_device::Request::StartDrag {
            source: Some(source),
            serial,
            ..
        } = request
        {
            lock(&state.observed).start_serials.push(serial);
            state.source = Some(source);
        }
    }
}

impl ServerDispatch<ServerDataSource, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerDataSource,
        request: server_data_source::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let mut observed = lock(&state.observed);
        match request {
            server_data_source::Request::Offer { mime_type } => {
                observed.source_mimes.push(mime_type);
            }
            server_data_source::Request::SetActions { dnd_actions } => {
                observed.source_actions.push(server_bits(dnd_actions));
            }
            server_data_source::Request::Destroy => observed.source_destroys += 1,
            _ => {}
        }
    }
}

impl ServerDispatch<ServerDataOffer, OfferFixture> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerDataOffer,
        request: server_data_offer::Request,
        data: &OfferFixture,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let mut observed = lock(&state.observed);
        match request {
            server_data_offer::Request::Accept { serial, mime_type } => {
                observed.accepts.push((serial, mime_type));
            }
            server_data_offer::Request::SetActions {
                dnd_actions,
                preferred_action,
            } => observed
                .set_actions
                .push((server_bits(dnd_actions), server_bits(preferred_action))),
            server_data_offer::Request::Receive { mime_type, fd } => {
                observed.receives.push(mime_type);
                if data.stall {
                    observed.stalled.push(fd);
                } else {
                    let _ = File::from(fd).write_all(&data.bytes);
                }
            }
            server_data_offer::Request::Finish => observed.finishes += 1,
            server_data_offer::Request::Destroy => observed.offer_destroys += 1,
            _ => {}
        }
    }
}

fn server_bits(value: wayland_server::WEnum<ServerAction>) -> u32 {
    match value {
        wayland_server::WEnum::Value(value) => value.bits(),
        wayland_server::WEnum::Unknown(value) => value,
    }
}

#[derive(Default)]
struct Events {
    positions: Vec<Option<DragPosition>>,
    drops: Vec<&'static str>,
    ended: Vec<DragResult>,
}

struct ClientState {
    clipboard: FocusedClipboard,
    drag: DragDrop<u32>,
    events: Events,
}

impl wayland_client::Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
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

wayland_client::delegate_noop!(ClientState: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(ClientState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ClientState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(ClientState: ignore WlDataDeviceManager);

impl wayland_client::Dispatch<WlDataDevice, ()> for ClientState {
    event_created_child!(ClientState, WlDataDevice, [
        0 => (WlDataOffer, OfferData::default())
    ]);

    fn event(
        state: &mut Self,
        device: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                let current = state.clipboard.binding().map(|binding| binding.2);
                let position =
                    state
                        .drag
                        .enter(current.as_ref(), device, serial, surface, x, y, id);
                state.events.positions.push(position);
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                let position = state.drag.motion(device, x, y);
                state.events.positions.push(position);
            }
            wl_data_device::Event::Leave => {
                state.drag.leave(device);
            }
            wl_data_device::Event::Drop => {
                let drop = match state.drag.dropped(device, 7) {
                    DropStart::Ignored => "ignored",
                    DropStart::Reading => "reading",
                    DropStart::Rejected(_) => "rejected",
                };
                state.events.drops.push(drop);
            }
            wl_data_device::Event::Selection { id } => state.clipboard.selection(device, id),
            _ => {}
        }
    }
}

impl wayland_client::Dispatch<WlDataOffer, OfferData> for ClientState {
    fn event(
        _: &mut Self,
        _: &WlDataOffer,
        event: wl_data_offer::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => data.offer(mime_type),
            wl_data_offer::Event::SourceActions { source_actions } => {
                data.set_source_actions(action_bits(source_actions));
            }
            wl_data_offer::Event::Action { dnd_action } => {
                data.set_action(action_bits(dnd_action));
            }
            _ => {}
        }
    }
}

impl wayland_client::Dispatch<WlDataSource, DragSourceData> for ClientState {
    fn event(
        state: &mut Self,
        _: &WlDataSource,
        event: wl_data_source::Event,
        data: &DragSourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                state.drag.send(data.key(), &mime_type, fd);
            }
            wl_data_source::Event::Action { dnd_action } => {
                state
                    .drag
                    .source_action(data.key(), action_bits(dnd_action));
            }
            wl_data_source::Event::DndFinished => {
                if let Some((_, result)) = state.drag.end_source(data.key(), true) {
                    state.events.ended.push(result);
                }
            }
            wl_data_source::Event::Cancelled => {
                if let Some((_, result)) = state.drag.end_source(data.key(), false) {
                    state.events.ended.push(result);
                }
            }
            _ => {}
        }
    }
}

struct Fixture {
    globals: GlobalList,
    queue: EventQueue<ClientState>,
    state: ClientState,
    surface: wl_surface::WlSurface,
    observed: Arc<Mutex<Observed>>,
    commands: mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

impl Fixture {
    fn new(version: u32) -> Self {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        server_socket.set_nonblocking(true).unwrap();
        let observed = Arc::new(Mutex::new(Observed::default()));
        let mut display = Display::<ServerState>::new().unwrap();
        let mut handle = display.handle();
        let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
        handle.create_global::<ServerState, ServerDataDeviceManager, _>(version, ());
        handle.create_global::<ServerState, ServerSeat, _>(9, ());
        handle.create_global::<ServerState, ServerCompositor, _>(4, ());
        let stop = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::channel();
        let server_stop = stop.clone();
        let server_observed = observed.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState {
                observed: server_observed,
                device: None,
                surface: None,
                offer: None,
                source: None,
            };
            while !server_stop.load(Ordering::Relaxed) {
                while let Ok(command) = receiver.try_recv() {
                    let handle = display.handle();
                    state.run(command, &handle);
                }
                match display
                    .backend()
                    .dispatch_single_client(&mut state, client.id())
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("private wayland server failed: {error}"),
                }
                let _ = display.backend().flush(Some(client.id()));
                thread::sleep(Duration::from_millis(1));
            }
        });
        let connection = Connection::from_socket(client_socket).unwrap();
        let (globals, mut queue) = registry_queue_init::<ClientState>(&connection).unwrap();
        let qh = queue.handle();
        let (ping, _) = calloop::ping::make_ping().unwrap();
        let (drag_ping, _) = calloop::ping::make_ping().unwrap();
        let mut state = ClientState {
            clipboard: FocusedClipboard::bind(&globals, &qh, ping).unwrap(),
            drag: DragDrop::new(drag_ping).unwrap(),
            events: Events::default(),
        };
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).unwrap();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).unwrap();
        let surface = compositor.create_surface(&qh, ());
        state.clipboard.attach_seat(&seat, &qh);
        queue.roundtrip(&mut state).unwrap();
        Self {
            globals,
            queue,
            state,
            surface,
            observed,
            commands,
            stop,
            server: Some(server),
        }
    }

    /// sends a command and waits until the server and client exchanged it.
    fn command(&mut self, command: Command) {
        self.commands.send(command).unwrap();
        self.sync();
    }

    fn sync(&mut self) {
        // the server applies queued commands before it dispatches requests.
        // the pause lets it pick up a command that raced the first
        // roundtrip, and the last roundtrip flushes the client's answers and
        // waits until the server handled them.
        self.queue.roundtrip(&mut self.state).unwrap();
        thread::sleep(Duration::from_millis(5));
        self.queue.roundtrip(&mut self.state).unwrap();
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    fn enter(&mut self, mime_types: &[&str], actions: u32, bytes: &[u8], stall: bool) -> u64 {
        self.command(Command::Enter {
            serial: 11,
            mime_types: mime_types.iter().map(|mime| (*mime).to_owned()).collect(),
            actions,
            bytes: bytes.to_vec(),
            stall,
        });
        self.state
            .events
            .positions
            .last()
            .cloned()
            .flatten()
            .expect("the drag entered")
            .session
    }

    fn deliveries(&mut self, count: usize) -> Vec<DropDelivery<u32>> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut deliveries = Vec::new();
        while deliveries.len() < count {
            deliveries.extend(self.state.drag.take_deliveries());
            assert!(Instant::now() < deadline, "drop was not delivered");
            thread::sleep(Duration::from_millis(2));
        }
        deliveries
    }

    fn start(&mut self, data: DragData, actions: DragActions) -> Result<(), &'static str> {
        let (manager, _, device) = self.state.clipboard.binding().unwrap();
        let qh = self.queue.handle();
        let result =
            self.state
                .drag
                .start(&manager, &device, &self.surface, 42, &data, actions, 3, &qh);
        self.sync();
        result
    }

    fn observed(&self) -> MutexGuard<'_, Observed> {
        lock(&self.observed)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const COPY: u32 = 1;
const MOVE: u32 = 2;
const BOTH: DragActions = DragActions {
    copy: true,
    moves: true,
};

#[test]
fn incoming_text_is_negotiated_read_and_finished_once() {
    let mut fixture = Fixture::new(3);
    let session = fixture.enter(
        &["text/uri-list", "text/plain", "text/plain;charset=utf-8"],
        COPY | MOVE,
        b"hello",
        false,
    );
    let position = fixture.state.events.positions[0].clone().unwrap();
    assert_eq!(position.types, [DropType::Files, DropType::Text]);
    assert_eq!(position.actions, BOTH);
    assert_eq!((position.x, position.y), (4.0, 5.0));

    fixture
        .state
        .drag
        .negotiate(session, Some((DropType::Text, DragAction::Move)));
    // an unchanged answer is not repeated after motion.
    fixture.command(Command::Motion);
    assert_eq!(fixture.state.events.positions[1].as_ref().unwrap().x, 6.0);
    fixture
        .state
        .drag
        .negotiate(session, Some((DropType::Text, DragAction::Move)));
    fixture.command(Command::OfferAction(MOVE));
    assert_eq!(
        fixture.observed().accepts,
        [(11, Some("text/plain;charset=utf-8".to_owned()))]
    );
    assert_eq!(fixture.observed().set_actions, [(MOVE, MOVE)]);

    fixture.command(Command::Drop);
    assert_eq!(fixture.state.events.drops, ["reading"]);
    let delivery = fixture.deliveries(1).pop().unwrap();
    assert_eq!(delivery.session, session);
    assert_eq!(delivery.action, DragAction::Move);
    assert_eq!(delivery.owner, 7);
    assert_eq!(delivery.result.unwrap(), DragData::Text("hello".into()));
    // the offer is still alive until the target answers.
    assert_eq!(fixture.observed().finishes, 0);
    fixture.state.drag.finish_drop(session, true);
    fixture.state.drag.finish_drop(session, true);
    // a leave after the drop does not touch the dropped offer.
    fixture.command(Command::Leave);
    let observed = fixture.observed();
    assert_eq!(observed.receives, ["text/plain;charset=utf-8"]);
    assert_eq!(observed.finishes, 1);
    assert_eq!(observed.offer_destroys, 1);
    drop(observed);
    assert!(fixture.state.drag.is_idle());
}

#[test]
fn rejected_and_unselected_drops_are_destroyed_without_finish() {
    let mut fixture = Fixture::new(3);
    let session = fixture.enter(&["text/plain"], COPY, b"x", false);
    // move is not offered, so the answer is a rejection.
    fixture
        .state
        .drag
        .negotiate(session, Some((DropType::Text, DragAction::Move)));
    // files are not offered either.
    fixture
        .state
        .drag
        .negotiate(session, Some((DropType::Files, DragAction::Copy)));
    fixture.command(Command::Drop);
    assert_eq!(fixture.state.events.drops, ["rejected"]);
    assert_eq!(fixture.observed().accepts, [(11, None)]);
    assert_eq!(fixture.observed().set_actions, [(0, 0)]);

    // an accepted type without the compositor's selected action is rejected.
    let session = fixture.enter(&["text/plain"], COPY, b"x", false);
    fixture
        .state
        .drag
        .negotiate(session, Some((DropType::Text, DragAction::Copy)));
    fixture.command(Command::Drop);
    fixture.command(Command::Drop);
    assert_eq!(
        fixture.state.events.drops,
        ["rejected", "rejected", "ignored"]
    );
    let observed = fixture.observed();
    assert!(observed.receives.is_empty());
    assert_eq!(observed.finishes, 0);
    assert_eq!(observed.offer_destroys, 2);
}

#[test]
fn invalid_files_are_reported_and_never_finished() {
    let mut fixture = Fixture::new(3);
    for (bytes, error) in [
        (
            &b"file:///a\r\nhttps://example.com/b\r\n"[..],
            "not a file uri",
        ),
        (&b"file://remote/a\r\n"[..], "remote host"),
        (&b"file:///a/../b\r\n"[..], "normalized absolute path"),
        (&b"\xff"[..], "UTF-8"),
    ] {
        let session = fixture.enter(&["text/uri-list"], COPY, bytes, false);
        fixture
            .state
            .drag
            .negotiate(session, Some((DropType::Files, DragAction::Copy)));
        fixture.command(Command::OfferAction(COPY));
        fixture.command(Command::Drop);
        let delivery = fixture.deliveries(1).pop().unwrap();
        let message = delivery.result.unwrap_err();
        assert!(message.contains(error), "{message}");
        fixture.state.drag.finish_drop(session, true);
    }
    // oversized text fails at the transfer limit.
    let session = fixture.enter(
        &["text/plain"],
        COPY,
        &vec![b'x'; MAX_DRAG_TEXT_BYTES + 1],
        false,
    );
    fixture
        .state
        .drag
        .negotiate(session, Some((DropType::Text, DragAction::Copy)));
    fixture.command(Command::OfferAction(COPY));
    fixture.command(Command::Drop);
    assert!(fixture.deliveries(1).pop().unwrap().result.is_err());
    fixture.sync();
    let observed = fixture.observed();
    assert_eq!(observed.finishes, 0);
    assert_eq!(observed.offer_destroys, 5);
}

#[test]
fn a_new_drop_retires_a_stalled_read_and_discards_its_result() {
    let mut fixture = Fixture::new(3);
    let stalled = fixture.enter(&["text/plain"], COPY, b"", true);
    fixture
        .state
        .drag
        .negotiate(stalled, Some((DropType::Text, DragAction::Copy)));
    fixture.command(Command::OfferAction(COPY));
    fixture.command(Command::Drop);

    let current = fixture.enter(&["text/plain"], COPY, b"fresh", false);
    fixture
        .state
        .drag
        .negotiate(current, Some((DropType::Text, DragAction::Copy)));
    fixture.command(Command::OfferAction(COPY));
    fixture.command(Command::Drop);
    let deliveries = fixture.deliveries(2);
    assert_eq!(deliveries[0].session, stalled);
    assert!(deliveries[0].result.is_err());
    assert_eq!(deliveries[1].session, current);
    assert_eq!(
        deliveries[1].result.as_ref().unwrap(),
        &DragData::Text("fresh".into())
    );
    // the stalled peer finally writes; its stale result is discarded.
    let stalled_writer = fixture.observed().stalled.pop().unwrap();
    let _ = File::from(stalled_writer).write_all(b"late");
    thread::sleep(Duration::from_millis(20));
    assert!(fixture.state.drag.take_deliveries().is_empty());
    // the stale session can no longer be finished.
    fixture.state.drag.finish_drop(stalled, true);
    fixture.state.drag.finish_drop(current, false);
    fixture.sync();
    let observed = fixture.observed();
    assert_eq!(observed.finishes, 0);
    assert_eq!(observed.offer_destroys, 2);
}

#[test]
fn a_burst_of_replaced_drops_keeps_the_delivery_queue_bounded() {
    // every drop below replaces a stalled read, so each one queues a
    // cancellation. nothing drains the queue during the burst.
    const BOUND: usize = MAX_PENDING_DELIVERIES;
    let mut fixture = Fixture::new(3);
    let mut sessions = Vec::new();
    for _ in 0..BOUND * 4 {
        let session = fixture.enter(&["text/plain"], COPY, b"", true);
        fixture
            .state
            .drag
            .negotiate(session, Some((DropType::Text, DragAction::Copy)));
        fixture.command(Command::OfferAction(COPY));
        fixture.command(Command::Drop);
        sessions.push(session);
    }
    // once the queue is full, new drops are refused before the running read
    // is replaced, so its owner keeps it.
    let drops = &fixture.state.events.drops;
    assert!(drops[..=BOUND].iter().all(|drop| *drop == "reading"));
    assert!(drops[BOUND + 1..].iter().all(|drop| *drop == "rejected"));
    let deliveries = fixture.state.drag.take_deliveries();
    assert_eq!(
        deliveries
            .iter()
            .map(|delivery| delivery.session)
            .collect::<Vec<_>>(),
        sessions[..BOUND]
    );
    assert!(deliveries.iter().all(|delivery| delivery.result.is_err()));
    fixture.sync();
    {
        let observed = fixture.observed();
        assert_eq!(observed.offer_destroys, BOUND * 4 - 1);
        assert_eq!(observed.finishes, 0);
    }

    // a drained queue admits drops again; the kept read is retired once.
    let session = fixture.enter(&["text/plain"], COPY, b"late", false);
    fixture
        .state
        .drag
        .negotiate(session, Some((DropType::Text, DragAction::Copy)));
    fixture.command(Command::OfferAction(COPY));
    fixture.command(Command::Drop);
    let deliveries = fixture.deliveries(2);
    assert_eq!(deliveries[0].session, sessions[BOUND]);
    assert!(deliveries[0].result.is_err());
    assert_eq!(
        deliveries[1].result.as_ref().unwrap(),
        &DragData::Text("late".into())
    );
    fixture.state.drag.finish_drop(session, true);
    fixture.sync();
    let observed = fixture.observed();
    assert_eq!(observed.finishes, 1);
    assert_eq!(observed.offer_destroys, BOUND * 4 + 1);
}

#[test]
fn leave_surface_and_seat_retirement_destroy_each_offer_once() {
    let mut fixture = Fixture::new(3);
    fixture.enter(&["text/plain"], COPY, b"x", false);
    fixture.command(Command::Leave);
    fixture.command(Command::Leave);

    fixture.enter(&["text/plain"], COPY, b"x", false);
    let surface = fixture.surface.clone();
    assert!(fixture.state.drag.retire_surface(&surface).is_some());
    // events for the retired session are ignored.
    fixture.command(Command::Motion);
    assert!(fixture.state.events.positions.last().unwrap().is_none());

    let session = fixture.enter(&["text/plain"], COPY, b"x", true);
    fixture
        .state
        .drag
        .negotiate(session, Some((DropType::Text, DragAction::Copy)));
    fixture.command(Command::OfferAction(COPY));
    fixture.command(Command::Drop);
    let (incoming, outgoing) = fixture.state.drag.detach();
    assert!(incoming.is_none() && outgoing.is_none());
    let delivery = fixture.state.drag.take_deliveries().pop().unwrap();
    assert!(delivery.result.is_err());
    assert!(fixture.state.drag.is_idle());
    fixture.sync();
    assert_eq!(fixture.observed().offer_destroys, 3);
    assert_eq!(fixture.observed().finishes, 0);
}

#[test]
fn outgoing_drags_serve_data_and_report_move_only_after_finish() {
    let mut fixture = Fixture::new(3);
    fixture
        .start(DragData::Text("payload".into()), BOTH)
        .unwrap();
    assert_eq!(
        fixture.start(DragData::Text("other".into()), BOTH),
        Err("a drag is already active")
    );
    {
        let observed = fixture.observed();
        assert_eq!(observed.start_serials, [42]);
        assert_eq!(observed.source_mimes, TEXT_MIME_TYPES);
        assert_eq!(observed.source_actions, [COPY | MOVE]);
    }

    let (reply, replies) = mpsc::channel();
    fixture.command(Command::SourceSend("text/plain".into(), reply.clone()));
    let mut text = String::new();
    File::from(replies.recv().unwrap())
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "payload");
    // an unsupported request is closed without data.
    fixture.command(Command::SourceSend("image/png".into(), reply));
    let mut bytes = Vec::new();
    File::from(replies.recv().unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.is_empty());

    fixture.command(Command::SourceAction(MOVE));
    fixture.command(Command::SourceFinished);
    fixture.command(Command::SourceCancelled);
    assert_eq!(fixture.state.events.ended, [DragResult::Move]);
    assert_eq!(fixture.observed().source_destroys, 1);

    // a move that the destination never finished is a cancellation.
    fixture
        .start(DragData::Files(vec!["/tmp/a b".into()]), BOTH)
        .unwrap();
    assert_eq!(
        fixture.observed().source_mimes.last().unwrap(),
        "text/uri-list"
    );
    let (reply, replies) = mpsc::channel();
    fixture.command(Command::SourceSend("text/uri-list".into(), reply));
    let mut list = String::new();
    File::from(replies.recv().unwrap())
        .read_to_string(&mut list)
        .unwrap();
    assert_eq!(list, "file:///tmp/a%20b\r\n");
    fixture.command(Command::SourceAction(MOVE));
    fixture.command(Command::SourceCancelled);
    assert_eq!(
        fixture.state.events.ended,
        [DragResult::Move, DragResult::Cancelled]
    );
    assert!(fixture.state.drag.cancel_outgoing(|_| true).is_none());
}

#[test]
fn a_copy_only_drag_never_reports_move() {
    let mut fixture = Fixture::new(3);
    let copy_only = DragActions {
        copy: true,
        moves: false,
    };
    fixture
        .start(DragData::Text("payload".into()), copy_only)
        .unwrap();
    assert_eq!(fixture.observed().source_actions, [COPY]);
    // a compositor that selects an action the drag did not offer, even after
    // a valid one, leaves no selected action.
    fixture.command(Command::SourceAction(COPY));
    fixture.command(Command::SourceAction(MOVE));
    fixture.command(Command::SourceFinished);
    assert_eq!(fixture.state.events.ended, [DragResult::Cancelled]);

    fixture
        .start(DragData::Text("payload".into()), copy_only)
        .unwrap();
    fixture.command(Command::SourceAction(MOVE));
    fixture.command(Command::SourceAction(COPY));
    fixture.command(Command::SourceFinished);
    assert_eq!(
        fixture.state.events.ended,
        [DragResult::Cancelled, DragResult::Copy]
    );
    assert_eq!(fixture.observed().source_destroys, 2);
}

#[test]
fn outgoing_writes_are_bounded_and_cancellation_destroys_the_source() {
    let mut fixture = Fixture::new(3);
    fixture
        .start(DragData::Text("x".repeat(MAX_DRAG_TEXT_BYTES)), BOTH)
        .unwrap();
    let (reply, replies) = mpsc::channel();
    let mut readers = Vec::new();
    for _ in 0..MAX_ACTIVE_WRITES + 2 {
        fixture.command(Command::SourceSend("text/plain".into(), reply.clone()));
        readers.push(replies.recv().unwrap());
    }
    // the readers are never drained, so each admitted write stays active.
    assert_eq!(fixture.state.drag.active_writes(), MAX_ACTIVE_WRITES);
    assert_eq!(
        fixture.state.drag.cancel_outgoing(|owner| *owner == 3),
        Some(3)
    );
    fixture.sync();
    assert_eq!(fixture.observed().source_destroys, 1);
    // events for the destroyed source are ignored.
    assert!(fixture.state.drag.end_source(1, true).is_none());
    drop(readers);
}

#[test]
fn older_managers_never_negotiate_or_start_drags() {
    let mut fixture = Fixture::new(2);
    fixture.command(Command::Enter {
        serial: 11,
        mime_types: vec!["text/plain".into()],
        actions: COPY,
        bytes: b"x".to_vec(),
        stall: false,
    });
    assert!(fixture.state.events.positions[0].is_none());
    assert_eq!(
        fixture.start(DragData::Text("x".into()), BOTH),
        Err("the compositor does not support drag and drop actions")
    );
    let observed = fixture.observed();
    assert_eq!(observed.offer_destroys, 1);
    assert!(observed.accepts.is_empty() && observed.start_serials.is_empty());
    drop(observed);
    let _ = &fixture.globals;
}
