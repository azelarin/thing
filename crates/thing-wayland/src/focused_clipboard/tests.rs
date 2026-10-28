use super::*;
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_registry, wl_seat,
    },
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource,
    protocol::{
        wl_data_device::WlDataDevice as ServerDataDevice,
        wl_data_device_manager::{
            self as server_data_device_manager, WlDataDeviceManager as ServerDataDeviceManager,
        },
        wl_data_offer::{self as server_data_offer, WlDataOffer as ServerDataOffer},
        wl_data_source::{self as server_data_source, WlDataSource as ServerDataSource},
        wl_seat::{self as server_seat, WlSeat as ServerSeat},
    },
};

#[derive(Default)]
struct Observed {
    receive_mimes: Vec<String>,
    offer_destroys: usize,
    source_destroys: usize,
    device_releases: usize,
    selection_serials: Vec<u32>,
    selection_mimes: Vec<Vec<String>>,
    source_reader: Option<OwnedFd>,
}

#[derive(Clone)]
struct SelectionFixture {
    mime_types: Vec<String>,
    bytes: Arc<[u8]>,
}

struct ServerState {
    selection: Option<SelectionFixture>,
    request_source: bool,
    observed: Arc<Mutex<Observed>>,
}

#[derive(Default)]
struct ServerSourceData(Mutex<Vec<String>>);

struct ServerOfferData {
    bytes: Arc<[u8]>,
}

impl GlobalDispatch<ServerDataDeviceManager, ()> for ServerState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerDataDeviceManager>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ());
    }
}

impl GlobalDispatch<ServerSeat, ()> for ServerState {
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

impl ServerDispatch<ServerDataDeviceManager, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerDataDeviceManager,
        request: server_data_device_manager::Request,
        _: &(),
        client: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_data_device_manager::Request::CreateDataSource { id } => {
                data.init(id, ServerSourceData::default());
            }
            server_data_device_manager::Request::GetDataDevice { id, .. } => {
                let device = data.init(id, ());
                if let Some(selection) = &state.selection {
                    let offer = client
                        .get_client(device.id())
                        .unwrap()
                        .create_resource::<ServerDataOffer, _, Self>(
                            client,
                            device.version(),
                            ServerOfferData {
                                bytes: selection.bytes.clone(),
                            },
                        )
                        .unwrap();
                    device.data_offer(&offer);
                    for mime_type in &selection.mime_types {
                        offer.offer(mime_type.clone());
                    }
                    device.selection(Some(&offer));
                }
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

impl ServerDispatch<ServerDataDevice, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerDataDevice,
        request: wayland_server::protocol::wl_data_device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wayland_server::protocol::wl_data_device::Request::SetSelection {
                source: Some(source),
                serial,
            } => {
                let mime_types = lock(&source.data::<ServerSourceData>().unwrap().0).clone();
                let mut observed = lock(&state.observed);
                observed.selection_serials.push(serial);
                observed.selection_mimes.push(mime_types);
                if state.request_source {
                    let (reader, writer) = pipe().unwrap();
                    source.send(TEXT_MIME_TYPES[0].into(), writer.as_fd());
                    source.cancelled();
                    observed.source_reader = Some(reader);
                }
            }
            wayland_server::protocol::wl_data_device::Request::Release => {
                lock(&state.observed).device_releases += 1;
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ServerDataSource, ServerSourceData> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerDataSource,
        request: server_data_source::Request,
        data: &ServerSourceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_data_source::Request::Offer { mime_type } => {
                lock(&data.0).push(mime_type);
            }
            server_data_source::Request::Destroy => {
                lock(&state.observed).source_destroys += 1;
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ServerDataOffer, ServerOfferData> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerDataOffer,
        request: server_data_offer::Request,
        data: &ServerOfferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_data_offer::Request::Receive { mime_type, fd } => {
                lock(&state.observed).receive_mimes.push(mime_type);
                let mut file = File::from(fd);
                let _ = file.write_all(&data.bytes);
            }
            server_data_offer::Request::Destroy => {
                lock(&state.observed).offer_destroys += 1;
            }
            _ => {}
        }
    }
}

struct ClientState {
    clipboard: Option<FocusedClipboard>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
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

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for ClientState {
    fn event(
        _: &mut Self,
        _: &wl_data_device_manager::WlDataDeviceManager,
        _: wl_data_device_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(ClientState: ignore wl_seat::WlSeat);

impl Dispatch<wl_data_device::WlDataDevice, ()> for ClientState {
    event_created_child!(ClientState, wl_data_device::WlDataDevice, [
        0 => (wl_data_offer::WlDataOffer, OfferData::default())
    ]);

    fn event(
        state: &mut Self,
        device: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let clipboard = state.clipboard.as_mut().unwrap();
        match event {
            wl_data_device::Event::DataOffer { .. } => {}
            wl_data_device::Event::Enter { serial, id, .. } => {
                clipboard.drag_enter(device, serial, id);
            }
            wl_data_device::Event::Leave | wl_data_device::Event::Drop => {
                clipboard.drag_finished(device);
            }
            wl_data_device::Event::Selection { id } => clipboard.selection(device, id),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, OfferData> for ClientState {
    fn event(
        _: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            data.offer(mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, SourceData> for ClientState {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let clipboard = state.clipboard.as_mut().unwrap();
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                clipboard.send(data, &mime_type, fd);
            }
            wl_data_source::Event::Cancelled => clipboard.cancelled(data.key()),
            _ => {}
        }
    }
}

struct PrivateServer {
    globals: wayland_client::globals::GlobalList,
    queue: wayland_client::EventQueue<ClientState>,
    observed: Arc<Mutex<Observed>>,
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

fn private_server(selection: Option<SelectionFixture>, request_source: bool) -> PrivateServer {
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    server_socket.set_nonblocking(true).unwrap();
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut display = Display::<ServerState>::new().unwrap();
    let mut handle = display.handle();
    let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
    handle.create_global::<ServerState, ServerDataDeviceManager, _>(3, ());
    handle.create_global::<ServerState, ServerSeat, _>(9, ());
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server_observed = observed.clone();
    let server = thread::spawn(move || {
        let mut state = ServerState {
            selection,
            request_source,
            observed: server_observed,
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
    PrivateServer {
        globals,
        queue,
        observed,
        stop,
        server: Some(server),
    }
}

fn attach(server: &mut PrivateServer) -> (ClientState, wl_seat::WlSeat) {
    let qh = server.queue.handle();
    let (ping, _) = calloop::ping::make_ping().unwrap();
    let clipboard = FocusedClipboard::bind(&server.globals, &qh, ping).unwrap();
    let seat = server.globals.bind(&qh, 1..=9, ()).unwrap();
    let mut state = ClientState {
        clipboard: Some(clipboard),
    };
    state.clipboard.as_mut().unwrap().attach_seat(&seat, &qh);
    server.queue.roundtrip(&mut state).unwrap();
    (state, seat)
}

fn wait_for_paste(clipboard: &mut FocusedClipboard) -> PasteOutcome {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(outcome) = clipboard.take_paste_outcome() {
            return outcome;
        }
        assert!(Instant::now() < deadline, "paste did not complete");
        thread::sleep(Duration::from_millis(2));
    }
}

fn paste_error(bytes: Arc<[u8]>) -> PasteError {
    let selection = SelectionFixture {
        mime_types: vec![TEXT_MIME_TYPES[0].into()],
        bytes,
    };
    let mut server = private_server(Some(selection), false);
    let (mut state, _) = attach(&mut server);
    state.clipboard.as_mut().unwrap().start_paste().unwrap();
    server.queue.roundtrip(&mut state).unwrap();
    wait_for_paste(state.clipboard.as_mut().unwrap())
        .result
        .unwrap_err()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn paste_is_explicit_and_prefers_utf8_text() {
    let selection = SelectionFixture {
        mime_types: vec![
            "image/png".into(),
            "text/plain".into(),
            "UTF8_STRING".into(),
            "text/plain; charset=UTF-8".into(),
        ],
        bytes: Arc::from(&b"h\xc3\xa9llo"[..]),
    };
    let mut server = private_server(Some(selection), false);
    let (mut state, _) = attach(&mut server);
    assert!(state.clipboard.as_ref().unwrap().available());
    assert!(lock(&server.observed).receive_mimes.is_empty());

    let paste = state.clipboard.as_mut().unwrap().start_paste().unwrap();
    server.queue.roundtrip(&mut state).unwrap();
    let outcome = wait_for_paste(state.clipboard.as_mut().unwrap());
    assert_eq!(outcome.id, paste);
    assert_eq!(outcome.result.unwrap(), "h\u{e9}llo");
    assert_eq!(
        lock(&server.observed).receive_mimes,
        ["text/plain; charset=UTF-8"]
    );
}

#[test]
fn a_new_paste_retires_the_previous_transfer() {
    let selection = SelectionFixture {
        mime_types: vec![TEXT_MIME_TYPES[0].into()],
        bytes: Arc::from(&b"current"[..]),
    };
    let mut server = private_server(Some(selection), false);
    let (mut state, _) = attach(&mut server);
    let clipboard = state.clipboard.as_mut().unwrap();
    let previous = clipboard.start_paste().unwrap();
    let current = clipboard.start_paste().unwrap();
    assert_ne!(previous, current);
    server.queue.roundtrip(&mut state).unwrap();

    let outcome = wait_for_paste(state.clipboard.as_mut().unwrap());
    assert_eq!(outcome.id, current);
    assert_eq!(outcome.result.unwrap(), "current");
    thread::sleep(Duration::from_millis(5));
    assert!(
        state
            .clipboard
            .as_mut()
            .unwrap()
            .take_paste_outcome()
            .is_none()
    );
    assert_eq!(lock(&server.observed).receive_mimes.len(), 2);
}

#[test]
fn source_bytes_survive_protocol_cancellation() {
    let mut server = private_server(None, true);
    let (mut state, _) = attach(&mut server);
    let qh = server.queue.handle();
    let text = "x".repeat(MAX_TEXT_BYTES);
    state
        .clipboard
        .as_mut()
        .unwrap()
        .set_text(text.clone(), 77, &qh)
        .unwrap();
    server.queue.roundtrip(&mut state).unwrap();

    let reader = lock(&server.observed).source_reader.take().unwrap();
    let mut received = String::new();
    File::from(reader).read_to_string(&mut received).unwrap();
    assert_eq!(received, text);
    let clipboard = state.clipboard.as_mut().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !clipboard.sources.is_empty() {
        clipboard.process_completions();
        assert!(Instant::now() < deadline, "source write did not complete");
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(clipboard.owned_bytes, 0);
    server.queue.roundtrip(&mut state).unwrap();

    let observed = lock(&server.observed);
    assert_eq!(observed.selection_serials, [77]);
    assert_eq!(observed.selection_mimes, [TEXT_MIME_TYPES]);
    assert_eq!(observed.source_destroys, 1);
}

#[test]
fn detach_releases_device_and_destroys_selection() {
    let selection = SelectionFixture {
        mime_types: vec![TEXT_MIME_TYPES[0].into()],
        bytes: Arc::from(&b"text"[..]),
    };
    let mut server = private_server(Some(selection), false);
    let (mut state, _) = attach(&mut server);
    state.clipboard.as_mut().unwrap().detach_seat();
    assert!(!state.clipboard.as_ref().unwrap().available());
    assert_eq!(
        state
            .clipboard
            .as_mut()
            .unwrap()
            .start_paste()
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    server.queue.roundtrip(&mut state).unwrap();
    let observed = lock(&server.observed);
    assert_eq!(observed.offer_destroys, 1);
    assert_eq!(observed.device_releases, 1);
}

#[test]
fn source_limits_fail_before_protocol_mutation() {
    let mut server = private_server(None, false);
    let (mut state, _) = attach(&mut server);
    let qh = server.queue.handle();
    let error = state
        .clipboard
        .as_mut()
        .unwrap()
        .set_text("x".repeat(MAX_TEXT_BYTES + 1), 9, &qh)
        .unwrap_err();
    assert_eq!(
        error,
        "clipboard text must contain no NUL and must not exceed 1 MiB"
    );
    server.queue.roundtrip(&mut state).unwrap();
    assert!(lock(&server.observed).selection_serials.is_empty());
}

#[test]
fn paste_rejects_non_utf8_nul_and_oversized_payloads() {
    assert!(matches!(
        paste_error(Arc::from(&b"\xff"[..])),
        PasteError::InvalidUtf8
    ));
    assert!(matches!(
        paste_error(Arc::from(&b"a\0b"[..])),
        PasteError::ContainsNul
    ));
    assert!(matches!(
        paste_error(Arc::from(vec![b'x'; MAX_TEXT_BYTES + 1])),
        PasteError::Transfer(TransferError::TooLarge)
    ));
}

#[test]
fn source_storage_is_bounded_while_the_compositor_retains_sources() {
    let mut server = private_server(None, false);
    let (mut state, _) = attach(&mut server);
    let qh = server.queue.handle();
    let clipboard = state.clipboard.as_mut().unwrap();
    for serial in 1..=4 {
        clipboard
            .set_text("x".repeat(MAX_TEXT_BYTES), serial, &qh)
            .unwrap();
    }
    assert_eq!(clipboard.owned_bytes, MAX_OWNED_BYTES);
    assert_eq!(
        clipboard
            .set_text("another byte".into(), 5, &qh)
            .unwrap_err(),
        "clipboard source storage is full"
    );
    server.queue.roundtrip(&mut state).unwrap();
    assert_eq!(lock(&server.observed).selection_serials, [1, 2, 3, 4]);
}
