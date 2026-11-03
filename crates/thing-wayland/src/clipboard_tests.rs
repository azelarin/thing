//! a private data-control compositor for clipboard format and image tests.
//! it serves either `ext-data-control-v1` or `wlr-data-control-v1`, and no
//! test touches the desktop clipboard.

use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use thing_core::ImagePixmap;
use wayland_client::{
    Connection, EventQueue, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::{
    client::{ext_data_control_device_v1, ext_data_control_offer_v1, ext_data_control_source_v1},
    server::{
        ext_data_control_device_v1::{
            ExtDataControlDeviceV1 as ExtDevice, Request as ExtDeviceRequest,
        },
        ext_data_control_manager_v1::{
            ExtDataControlManagerV1 as ExtManager, Request as ExtManagerRequest,
        },
        ext_data_control_offer_v1::{
            ExtDataControlOfferV1 as ExtOffer, Request as ExtOfferRequest,
        },
        ext_data_control_source_v1::{
            ExtDataControlSourceV1 as ExtSource, Request as ExtSourceRequest,
        },
    },
};
use wayland_protocols_wlr::data_control::v1::{
    client::{
        zwlr_data_control_device_v1, zwlr_data_control_offer_v1, zwlr_data_control_source_v1,
    },
    server::{
        zwlr_data_control_device_v1::{
            Request as WlrDeviceRequest, ZwlrDataControlDeviceV1 as WlrDevice,
        },
        zwlr_data_control_manager_v1::{
            Request as WlrManagerRequest, ZwlrDataControlManagerV1 as WlrManager,
        },
        zwlr_data_control_offer_v1::{
            Request as WlrOfferRequest, ZwlrDataControlOfferV1 as WlrOffer,
        },
        zwlr_data_control_source_v1::{
            Request as WlrSourceRequest, ZwlrDataControlSourceV1 as WlrSource,
        },
    },
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource, protocol::wl_seat::WlSeat as ServerSeat,
};

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Ext,
    Wlr,
}

const PROTOCOLS: [Protocol; 2] = [Protocol::Ext, Protocol::Wlr];

/// what one advertised mime type delivers when received.
#[derive(Clone)]
enum Payload {
    Bytes(Vec<u8>),
    /// keep the pipe open and never write.
    Stall,
}

/// how the compositor reports our own `set_selection` back to us.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Echo {
    Immediate,
    /// never: the protocol does not promise a client its own echo.
    Absent,
    /// later, on `Command::ReleaseEchoes`.
    Held,
}

enum Command {
    Select(Option<Vec<(String, Payload)>>),
    Primary,
    Finish,
    ReadSource(String, mpsc::Sender<OwnedFd>),
    Echo(Echo),
    ReleaseEchoes,
}

#[derive(Default)]
struct Observed {
    /// the mime types of every source set as the selection, or `None` for a clear.
    selections: Vec<Option<Vec<String>>>,
    primary_requests: usize,
    receives: Vec<String>,
}

#[derive(Default)]
struct Shared {
    commands: VecDeque<Command>,
    observed: Observed,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, PartialEq)]
enum ServerSource {
    Ext(ExtSource),
    Wlr(WlrSource),
}

impl ServerSource {
    fn mimes(&self) -> Vec<String> {
        let mimes = match self {
            Self::Ext(source) => source.data::<Mutex<Vec<String>>>(),
            Self::Wlr(source) => source.data::<Mutex<Vec<String>>>(),
        };
        lock(mimes.unwrap()).clone()
    }

    fn send(&self, mime: String, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(source) => source.send(mime, fd),
            Self::Wlr(source) => source.send(mime, fd),
        }
    }

    fn cancelled(&self) {
        match self {
            Self::Ext(source) => source.cancelled(),
            Self::Wlr(source) => source.cancelled(),
        }
    }
}

enum Backing {
    Payloads(Vec<(String, Payload)>),
    Source(ServerSource),
}

type OfferBacking = Mutex<Option<Backing>>;

#[derive(Clone)]
enum ServerDevice {
    Ext(ExtDevice),
    Wlr(WlrDevice),
}

impl ServerDevice {
    fn select(&self, dh: &DisplayHandle, client: &Client, backing: Option<Backing>) {
        let mimes: Vec<String> = match &backing {
            Some(Backing::Payloads(payloads)) => {
                payloads.iter().map(|(mime, _)| mime.clone()).collect()
            }
            Some(Backing::Source(source)) => source.mimes(),
            None => Vec::new(),
        };
        match self {
            Self::Ext(device) => {
                let Some(backing) = backing else {
                    device.selection(None);
                    return;
                };
                let offer = client
                    .create_resource::<ExtOffer, OfferBacking, ServerState>(
                        dh,
                        device.version(),
                        Mutex::new(Some(backing)),
                    )
                    .unwrap();
                device.data_offer(&offer);
                for mime in mimes {
                    offer.offer(mime);
                }
                device.selection(Some(&offer));
            }
            Self::Wlr(device) => {
                let Some(backing) = backing else {
                    device.selection(None);
                    return;
                };
                let offer = client
                    .create_resource::<WlrOffer, OfferBacking, ServerState>(
                        dh,
                        device.version(),
                        Mutex::new(Some(backing)),
                    )
                    .unwrap();
                device.data_offer(&offer);
                for mime in mimes {
                    offer.offer(mime);
                }
                device.selection(Some(&offer));
            }
        }
    }

    fn primary(&self, dh: &DisplayHandle, client: &Client) {
        match self {
            Self::Ext(device) => {
                let offer = client
                    .create_resource::<ExtOffer, OfferBacking, ServerState>(
                        dh,
                        device.version(),
                        Mutex::new(None),
                    )
                    .unwrap();
                device.data_offer(&offer);
                offer.offer("text/plain".into());
                device.primary_selection(Some(&offer));
            }
            Self::Wlr(device) => {
                let offer = client
                    .create_resource::<WlrOffer, OfferBacking, ServerState>(
                        dh,
                        device.version(),
                        Mutex::new(None),
                    )
                    .unwrap();
                device.data_offer(&offer);
                offer.offer("text/plain".into());
                device.primary_selection(Some(&offer));
            }
        }
    }

    fn finished(&self) {
        match self {
            Self::Ext(device) => device.finished(),
            Self::Wlr(device) => device.finished(),
        }
    }
}

struct ServerState {
    shared: Arc<Mutex<Shared>>,
    device: Option<ServerDevice>,
    source: Option<ServerSource>,
    echo: Echo,
    held: Vec<Option<ServerSource>>,
    stalled: Vec<OwnedFd>,
}

impl ServerState {
    /// applies our own `set_selection` like a compositor: the replaced source
    /// is cancelled, and the new selection is echoed to every device unless
    /// the test asked otherwise.
    fn set_selection(&mut self, dh: &DisplayHandle, client: &Client, source: Option<ServerSource>) {
        lock(&self.shared)
            .observed
            .selections
            .push(source.as_ref().map(ServerSource::mimes));
        if let Some(previous) = self.source.take()
            && source.as_ref() != Some(&previous)
        {
            previous.cancelled();
        }
        self.source = source.clone();
        match self.echo {
            Echo::Immediate => self.select(dh, client, source.map(Backing::Source)),
            Echo::Absent => {}
            Echo::Held => self.held.push(source),
        }
    }

    /// another application's selection replaces ours.
    fn foreign(&mut self, dh: &DisplayHandle, client: &Client, backing: Option<Backing>) {
        if let Some(previous) = self.source.take() {
            previous.cancelled();
        }
        self.select(dh, client, backing);
    }

    fn select(&self, dh: &DisplayHandle, client: &Client, backing: Option<Backing>) {
        if let Some(device) = &self.device {
            device.select(dh, client, backing);
        }
    }

    fn receive(&mut self, mime_type: String, fd: OwnedFd, backing: &OfferBacking) {
        lock(&self.shared).observed.receives.push(mime_type.clone());
        match lock(backing).as_ref() {
            Some(Backing::Source(source)) => source.send(mime_type, fd.as_fd()),
            Some(Backing::Payloads(payloads)) => {
                match payloads.iter().find(|(mime, _)| *mime == mime_type) {
                    Some((_, Payload::Bytes(bytes))) => {
                        let bytes = bytes.clone();
                        // a writer thread, so a large payload never stalls the server.
                        thread::spawn(move || {
                            let mut pipe = std::fs::File::from(fd);
                            let _ = pipe.write_all(&bytes);
                        });
                    }
                    Some((_, Payload::Stall)) => self.stalled.push(fd),
                    None => {}
                }
            }
            None => {}
        }
    }
}

impl GlobalDispatch<ExtManager, ()> for ServerState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtManager>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ());
    }
}

impl ServerDispatch<ExtManager, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtManager,
        request: ExtManagerRequest,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            ExtManagerRequest::CreateDataSource { id } => {
                data.init(id, Mutex::new(Vec::<String>::new()));
            }
            ExtManagerRequest::GetDataDevice { id, .. } => {
                state.device = Some(ServerDevice::Ext(data.init(id, ())));
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ExtDevice, ()> for ServerState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &ExtDevice,
        request: ExtDeviceRequest,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ExtDeviceRequest::SetSelection { source } => {
                state.set_selection(dh, client, source.map(ServerSource::Ext));
            }
            ExtDeviceRequest::SetPrimarySelection { .. } => {
                lock(&state.shared).observed.primary_requests += 1;
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ExtSource, Mutex<Vec<String>>> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtSource,
        request: ExtSourceRequest,
        mimes: &Mutex<Vec<String>>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ExtSourceRequest::Offer { mime_type } = request {
            lock(mimes).push(mime_type);
        }
    }
}

impl ServerDispatch<ExtOffer, OfferBacking> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtOffer,
        request: ExtOfferRequest,
        backing: &OfferBacking,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ExtOfferRequest::Receive { mime_type, fd } = request {
            state.receive(mime_type, fd, backing);
        }
    }
}

impl GlobalDispatch<WlrManager, ()> for ServerState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlrManager>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ());
    }
}

impl ServerDispatch<WlrManager, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlrManager,
        request: WlrManagerRequest,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            WlrManagerRequest::CreateDataSource { id } => {
                data.init(id, Mutex::new(Vec::<String>::new()));
            }
            WlrManagerRequest::GetDataDevice { id, .. } => {
                state.device = Some(ServerDevice::Wlr(data.init(id, ())));
            }
            _ => {}
        }
    }
}

impl ServerDispatch<WlrDevice, ()> for ServerState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &WlrDevice,
        request: WlrDeviceRequest,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            WlrDeviceRequest::SetSelection { source } => {
                state.set_selection(dh, client, source.map(ServerSource::Wlr));
            }
            WlrDeviceRequest::SetPrimarySelection { .. } => {
                lock(&state.shared).observed.primary_requests += 1;
            }
            _ => {}
        }
    }
}

impl ServerDispatch<WlrSource, Mutex<Vec<String>>> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlrSource,
        request: WlrSourceRequest,
        mimes: &Mutex<Vec<String>>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let WlrSourceRequest::Offer { mime_type } = request {
            lock(mimes).push(mime_type);
        }
    }
}

impl ServerDispatch<WlrOffer, OfferBacking> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlrOffer,
        request: WlrOfferRequest,
        backing: &OfferBacking,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let WlrOfferRequest::Receive { mime_type, fd } = request {
            state.receive(mime_type, fd, backing);
        }
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

impl ServerDispatch<ServerSeat, ()> for ServerState {
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

struct ClientState {
    clipboard: Option<Clipboard>,
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

wayland_client::delegate_noop!(ClientState: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(ClientState: ignore ExtDataControlManagerV1);
wayland_client::delegate_noop!(ClientState: ignore ZwlrDataControlManagerV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for ClientState {
    event_created_child!(ClientState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, OfferData::default())
    ]);

    fn event(
        state: &mut Self,
        device: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let clipboard = state.clipboard.as_mut().unwrap();
        match event {
            ext_data_control_device_v1::Event::Selection { id } => clipboard.ext_offer(id),
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                clipboard.ext_primary_offer(id);
            }
            ext_data_control_device_v1::Event::Finished => clipboard.ext_finished(device),
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, OfferData> for ClientState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.offer(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, SourceData> for ClientState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let clipboard = state.clipboard.as_mut().unwrap();
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                clipboard.send(data, &mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => clipboard.cancelled(data.key()),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClientState {
    event_created_child!(ClientState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, OfferData::default())
    ]);

    fn event(
        state: &mut Self,
        device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let clipboard = state.clipboard.as_mut().unwrap();
        match event {
            zwlr_data_control_device_v1::Event::Selection { id } => clipboard.wlr_offer(id),
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                clipboard.wlr_primary_offer(id);
            }
            zwlr_data_control_device_v1::Event::Finished => clipboard.wlr_finished(device),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, OfferData> for ClientState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.offer(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, SourceData> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let clipboard = state.clipboard.as_mut().unwrap();
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                clipboard.send(data, &mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => clipboard.cancelled(data.key()),
            _ => {}
        }
    }
}

struct Compositor {
    protocol: Protocol,
    state: ClientState,
    queue: EventQueue<ClientState>,
    seat: wl_seat::WlSeat,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Drop for Compositor {
    fn drop(&mut self) {
        // the clipboard goes first, as when the host exits.
        self.state.clipboard = None;
        let _ = self.queue.flush();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

impl Compositor {
    fn start(protocol: Protocol) -> Self {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        server_socket.set_nonblocking(true).unwrap();
        let mut display = Display::<ServerState>::new().unwrap();
        let mut handle = display.handle();
        let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
        match protocol {
            Protocol::Ext => {
                handle.create_global::<ServerState, ExtManager, _>(1, ());
            }
            Protocol::Wlr => {
                handle.create_global::<ServerState, WlrManager, _>(2, ());
            }
        }
        handle.create_global::<ServerState, ServerSeat, _>(7, ());
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let shared = shared.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut state = ServerState {
                    shared: shared.clone(),
                    device: None,
                    source: None,
                    echo: Echo::Immediate,
                    held: Vec::new(),
                    stalled: Vec::new(),
                };
                let dh = display.handle();
                while !stop.load(Ordering::Relaxed) {
                    let commands: Vec<_> = lock(&shared).commands.drain(..).collect();
                    for command in commands {
                        match command {
                            Command::Select(payloads) => {
                                state.foreign(&dh, &client, payloads.map(Backing::Payloads));
                            }
                            Command::Primary => {
                                if let Some(device) = &state.device {
                                    device.primary(&dh, &client);
                                }
                            }
                            Command::Finish => {
                                if let Some(device) = &state.device {
                                    device.finished();
                                }
                            }
                            Command::ReadSource(mime, reply) => {
                                if let Some(source) = &state.source {
                                    let (reader, writer) = pipe().unwrap();
                                    source.send(mime, writer.as_fd());
                                    drop(writer);
                                    let _ = reply.send(reader);
                                }
                            }
                            Command::Echo(echo) => state.echo = echo,
                            Command::ReleaseEchoes => {
                                for source in std::mem::take(&mut state.held) {
                                    state.select(&dh, &client, source.map(Backing::Source));
                                }
                            }
                        }
                    }
                    match display
                        .backend()
                        .dispatch_single_client(&mut state, client.id())
                    {
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => break,
                    }
                    let _ = display.backend().flush(Some(client.id()));
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };
        let connection = Connection::from_socket(client_socket).unwrap();
        let (globals, mut queue) = registry_queue_init(&connection).unwrap();
        let qh = queue.handle();
        let (ping, _) = calloop::ping::make_ping().unwrap();
        let clipboard = Clipboard::bind(&globals, &qh, ping).unwrap();
        match (protocol, &clipboard.backend) {
            (Protocol::Ext, Backend::Ext { .. }) | (Protocol::Wlr, Backend::Wlr { .. }) => {}
            _ => panic!("the clipboard did not bind {protocol:?} data control"),
        }
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
        let mut state = ClientState {
            clipboard: Some(clipboard),
        };
        state.clipboard.as_mut().unwrap().attach_seat(&seat, &qh);
        queue.roundtrip(&mut state).unwrap();
        Self {
            protocol,
            state,
            queue,
            seat,
            shared,
            stop,
            server: Some(server),
        }
    }

    fn clipboard(&mut self) -> &mut Clipboard {
        self.state.clipboard.as_mut().unwrap()
    }

    fn set_image(&mut self, image: &ImageSource) -> Result<(), String> {
        let qh = self.queue.handle();
        self.clipboard().set_image(image, &qh)
    }

    fn command(&mut self, command: Command) {
        lock(&self.shared).commands.push_back(command);
        // the server applies commands before dispatching, so one roundtrip
        // after the queued command delivers its events.
        self.queue.roundtrip(&mut self.state).unwrap();
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    fn select(&mut self, payloads: &[(&str, Payload)]) {
        self.command(Command::Select(Some(
            payloads
                .iter()
                .map(|(mime, payload)| ((*mime).to_owned(), payload.clone()))
                .collect(),
        )));
    }

    /// pumps the protocol and the clipboard until `done` holds for the latest
    /// snapshot. each step waits on a server roundtrip; the deadline only
    /// turns a hang into a failure.
    fn until(&mut self, mut done: impl FnMut(&ClipboardSnapshot) -> bool) -> ClipboardSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.queue.roundtrip(&mut self.state).unwrap();
            let qh = self.queue.handle();
            self.clipboard().update(&qh);
            let current = self.clipboard().state.current.clone();
            if done(&current) {
                return current;
            }
            assert!(
                Instant::now() < deadline,
                "{:?}: clipboard never reached the state: {current:?}",
                self.protocol
            );
        }
    }

    /// pumps the protocol, without applying codec results, until the codec
    /// has finished its queued work and holds a result.
    fn until_codec_result(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.clipboard().codec.has_results() {
            self.queue.roundtrip(&mut self.state).unwrap();
            assert!(
                Instant::now() < deadline,
                "{:?}: the codec never finished",
                self.protocol
            );
        }
    }

    /// waits for every pending encode to be applied or discarded.
    fn settle_encode(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.clipboard().encode.is_some() || self.clipboard().codec.has_results() {
            self.until(|_| true);
            assert!(
                Instant::now() < deadline,
                "{:?}: the encode never settled",
                self.protocol
            );
        }
        self.until(|_| true);
    }

    fn observed<T>(&self, read: impl FnOnce(&Observed) -> T) -> T {
        read(&lock(&self.shared).observed)
    }

    fn published_images(&self) -> usize {
        self.observed(|observed| {
            observed
                .selections
                .iter()
                .flatten()
                .filter(|mimes| mimes.iter().any(|mime| mime == PNG_MIME_TYPE))
                .count()
        })
    }

    /// asks the compositor to read our current source as `mime`.
    fn read_source(&mut self, mime: &str) -> Vec<u8> {
        let (reply, reader) = mpsc::channel();
        self.command(Command::ReadSource(mime.into(), reply));
        let reader = reader.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut pipe = std::fs::File::from(reader);
        // keep serving the client while the worker writes into the pipe.
        let (done, bytes) = mpsc::channel();
        thread::spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            let _ = done.send(data);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(data) = bytes.try_recv() {
                return data;
            }
            self.queue.roundtrip(&mut self.state).unwrap();
            let qh = self.queue.handle();
            self.clipboard().update(&qh);
            assert!(Instant::now() < deadline, "the source was never read");
        }
    }
}

/// deterministic rgba noise, which compresses poorly.
fn noise(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..width * height * 4)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

fn png(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
    let rgba: Vec<u8> = (0..width * height * 4)
        .map(|value| (value * 37 % 251) as u8)
        .collect();
    (
        thing_ui::png::encode_png(width, height, &rgba).unwrap(),
        rgba,
    )
}

fn handle(id: &str, width: u32, height: u32, rgba: Vec<u8>) -> ImageSource {
    ImageSource::new(
        id.into(),
        vec![
            ImageCandidate::new(None, vec![ImagePixmap::new(width, height, rgba).unwrap()])
                .unwrap(),
        ],
    )
    .unwrap()
}

fn status(snapshot: &ClipboardSnapshot) -> ClipboardImageStatus {
    snapshot.image_status
}

fn text_formats() -> Vec<(&'static str, Payload)> {
    TEXT_MIME_TYPES
        .iter()
        .map(|mime| (*mime, Payload::Bytes(b"theirs".to_vec())))
        .collect()
}

#[test]
fn selections_report_formats_and_load_png_images_on_request() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        let (png, rgba) = png(3, 2);
        assert!(
            png.contains(&0),
            "png payloads are binary, not nul-terminated text"
        );
        compositor.select(&[
            (
                "text/plain;charset=utf-8",
                Payload::Bytes(b"caption".to_vec()),
            ),
            ("image/png", Payload::Bytes(png)),
            ("image/jpeg", Payload::Bytes(vec![0xff, 0xd8])),
        ]);
        let snapshot = compositor.until(|snapshot| snapshot.text.is_some());
        assert_eq!(snapshot.text.as_deref(), Some("caption"));
        assert_eq!(
            snapshot.formats,
            ["text/plain;charset=utf-8", "image/png", "image/jpeg"]
        );
        assert_eq!(status(&snapshot), ClipboardImageStatus::Available);
        assert!(snapshot.image.is_none());
        // nothing reads the image until asked.
        assert_eq!(
            compositor.observed(|observed| observed.receives.clone()),
            ["text/plain;charset=utf-8"]
        );

        compositor.clipboard().read_image().unwrap();
        assert_eq!(
            compositor.clipboard().state.current.image_status,
            ClipboardImageStatus::Loading
        );
        let snapshot = compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Ready);
        let image = snapshot.image.unwrap();
        let pixmap = &image.candidates()[0].pixmaps()[0];
        assert_eq!((pixmap.width(), pixmap.height()), (3, 2));
        assert_eq!(pixmap.rgba(), rgba);
        // a second request for a ready image does not transfer again.
        compositor.clipboard().read_image().unwrap();
        compositor.until(|_| true);
        assert_eq!(compositor.observed(|observed| observed.receives.len()), 2);

        // the decoded handle owns its pixels after the selection moves on.
        compositor.select(&[("text/plain", Payload::Bytes(b"next".to_vec()))]);
        let snapshot = compositor.until(|snapshot| snapshot.text.as_deref() == Some("next"));
        assert_eq!(status(&snapshot), ClipboardImageStatus::None);
        assert!(snapshot.image.is_none());
        assert_eq!(image.candidates()[0].pixmaps()[0].rgba(), rgba);
    }
}

#[test]
fn images_larger_than_the_text_read_budget_load() {
    // about 5 MiB of incompressible png: more than the 4 MiB that text reads
    // may buffer, and within the 16 MiB png limit.
    let rgba = noise(1200, 1100, 7);
    let png = thing_ui::png::encode_png(1200, 1100, &rgba).unwrap();
    assert!(png.len() > 5 * 1024 * 1024, "{}", png.len());
    let mut compositor = Compositor::start(Protocol::Ext);
    compositor.select(&[("image/png", Payload::Bytes(png))]);
    compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Available);
    compositor.clipboard().read_image().unwrap();
    let snapshot = compositor.until(|snapshot| {
        matches!(
            status(snapshot),
            ClipboardImageStatus::Ready | ClipboardImageStatus::Failed
        )
    });
    assert_eq!(
        status(&snapshot),
        ClipboardImageStatus::Ready,
        "{}",
        snapshot.image_error
    );
    assert_eq!(
        snapshot.image.unwrap().candidates()[0].pixmaps()[0].rgba(),
        rgba
    );
}

#[test]
fn unsupported_empty_malformed_and_oversized_images_are_distinct() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        compositor.select(&[("image/jpeg", Payload::Bytes(vec![0xff, 0xd8]))]);
        let snapshot =
            compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Unsupported);
        assert_eq!(snapshot.formats, ["image/jpeg"]);
        assert!(
            compositor
                .clipboard()
                .read_image()
                .unwrap_err()
                .contains("not offered as png")
        );

        compositor.command(Command::Select(None));
        let snapshot = compositor.until(|snapshot| snapshot.formats.is_empty());
        assert_eq!(status(&snapshot), ClipboardImageStatus::None);
        assert!(
            compositor
                .clipboard()
                .read_image()
                .unwrap_err()
                .contains("no image")
        );

        compositor.select(&[(
            "image/png",
            Payload::Bytes(b"\x89PNG\r\n\x1a\nnot really".to_vec()),
        )]);
        compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Available);
        compositor.clipboard().read_image().unwrap();
        let snapshot =
            compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Failed);
        assert!(
            snapshot.image_error.contains("cannot decode"),
            "{}",
            snapshot.image_error
        );
        assert!(snapshot.image.is_none());

        let oversized = vec![0_u8; thing_ui::png::MAX_PNG_BYTES + 1];
        compositor.select(&[("image/png", Payload::Bytes(oversized))]);
        compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Available);
        compositor.clipboard().read_image().unwrap();
        let snapshot =
            compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Failed);
        assert!(
            snapshot.image_error.contains("exceeds its size limit"),
            "{}",
            snapshot.image_error
        );
        // a failed read may be retried.
        compositor.clipboard().read_image().unwrap();
        assert_eq!(
            compositor.clipboard().state.current.image_status,
            ClipboardImageStatus::Loading
        );
    }
}

#[test]
fn replaced_selections_retire_stalled_and_stale_image_work() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        compositor.select(&[("image/png", Payload::Stall)]);
        compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Available);
        compositor.clipboard().read_image().unwrap();
        compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Loading);

        // the stalled transfer never blocks a new selection.
        let (next, _) = png(2, 2);
        compositor.select(&[
            ("text/plain", Payload::Bytes(b"mixed".to_vec())),
            ("image/png", Payload::Bytes(next)),
        ]);
        let snapshot = compositor.until(|snapshot| snapshot.text.as_deref() == Some("mixed"));
        assert_eq!(status(&snapshot), ClipboardImageStatus::Available);
        assert!(compositor.clipboard().image_read.is_none());
        assert!(snapshot.image.is_none());

        // a decode for an older selection is ignored when it lands late.
        compositor.clipboard().read_image().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !compositor.clipboard().codec.has_results() {
            compositor.queue.roundtrip(&mut compositor.state).unwrap();
            compositor.clipboard().process_transfers_only();
            assert!(Instant::now() < deadline, "the image was never decoded");
        }
        compositor.select(&[("text/plain", Payload::Bytes(b"newer".to_vec()))]);
        let snapshot = compositor.until(|snapshot| snapshot.text.as_deref() == Some("newer"));
        assert_eq!(status(&snapshot), ClipboardImageStatus::None);
        assert!(snapshot.image.is_none());
        compositor.until(|_| true);
        assert_eq!(
            compositor.clipboard().state.current.image_status,
            ClipboardImageStatus::None
        );
        // the text half kept working through every replacement.
        assert_eq!(
            compositor.clipboard().state.current.text.as_deref(),
            Some("newer")
        );
    }
}

#[test]
fn decoded_images_have_identities_unique_to_their_pixels() {
    let mut ids = Vec::new();
    for (protocol, seed) in [(Protocol::Ext, 1), (Protocol::Wlr, 2)] {
        let mut compositor = Compositor::start(protocol);
        let rgba = noise(2, 2, seed);
        let png = thing_ui::png::encode_png(2, 2, &rgba).unwrap();
        compositor.select(&[("image/png", Payload::Bytes(png))]);
        compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Available);
        compositor.clipboard().read_image().unwrap();
        let snapshot = compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Ready);
        let image = snapshot.image.unwrap();
        assert_eq!(image.candidates()[0].pixmaps()[0].rgba(), rgba);
        ids.push(image.id().to_owned());
    }
    // renderer caches key in-memory images by identity, so two clipboards in
    // one process must never reuse an identity for different pixels.
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn images_are_published_as_png_and_newer_values_win() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        let rgba: Vec<u8> = (0..2 * 2 * 4).map(|value| value as u8 * 13).collect();
        let image = handle("test-image", 2, 2, rgba.clone());
        compositor.set_image(&image).unwrap();
        compositor.until(|snapshot| snapshot.formats == ["image/png"]);
        assert_eq!(
            compositor.observed(|observed| observed.selections.clone()),
            [Some(vec!["image/png".to_owned()])]
        );
        let served = compositor.read_source("image/png");
        let decoded = thing_ui::png::decode_png(&served).unwrap();
        assert_eq!((decoded.width, decoded.height, decoded.rgba), (2, 2, rgba));
        // a png source never answers a text request.
        assert!(compositor.read_source("text/plain").is_empty());

        // set_text after set_image supersedes the image before it is encoded.
        compositor.set_image(&image).unwrap();
        let qh = compositor.queue.handle();
        compositor
            .clipboard()
            .set_text("words".into(), &qh)
            .unwrap();
        compositor.until(|snapshot| snapshot.text.as_deref() == Some("words"));
        compositor.settle_encode();
        let selections = compositor.observed(|observed| observed.selections.clone());
        assert_eq!(selections.len(), 2, "{selections:?}");
        assert_eq!(
            selections[1].as_ref().unwrap()[0],
            "text/plain;charset=utf-8"
        );

        // set_image after set_text in the same handler wins: the text echo
        // predates the image request.
        let qh = compositor.queue.handle();
        compositor
            .clipboard()
            .set_text("first".into(), &qh)
            .unwrap();
        compositor.set_image(&image).unwrap();
        compositor.settle_encode();
        assert_eq!(compositor.published_images(), 2);
        assert_eq!(compositor.clipboard().state.current.formats, ["image/png"]);

        // so does another application's selection while the image encodes.
        compositor.set_image(&image).unwrap();
        compositor.queue.roundtrip(&mut compositor.state).unwrap();
        compositor.select(&[("text/plain", Payload::Bytes(b"theirs".to_vec()))]);
        compositor.settle_encode();
        let snapshot = compositor.until(|snapshot| snapshot.text.as_deref() == Some("theirs"));
        assert_eq!(snapshot.formats, ["text/plain"]);
        assert_eq!(compositor.published_images(), 2);
    }
}

#[test]
fn unsupported_handles_are_rejected_without_queuing_work() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        let icon = ImageSource::new(
            "icon".into(),
            vec![ImageCandidate::new(Some("folder".into()), Vec::new()).unwrap()],
        )
        .unwrap();
        let wide = handle(
            "wide",
            thing_ui::png::MAX_PNG_DIMENSION + 1,
            1,
            vec![0; (thing_ui::png::MAX_PNG_DIMENSION as usize + 1) * 4],
        );
        for (image, message) in [(&icon, "no pixels"), (&wide, "limited")] {
            let error = compositor.set_image(image).unwrap_err();
            assert!(error.contains(message), "{error}");
            assert!(compositor.clipboard().encode.is_none());
        }
        compositor.until(|_| true);
        assert!(!compositor.clipboard().codec.has_results());
        assert_eq!(compositor.observed(|observed| observed.selections.len()), 0);
    }
}

/// a foreign selection that advertises exactly the formats of one of our
/// unconfirmed echoes must still cancel a pending image.
#[test]
fn foreign_selections_with_our_formats_cancel_pending_images() {
    for protocol in PROTOCOLS {
        let image = handle("mine", 64, 64, noise(64, 64, 3));

        // the compositor never echoes our own selection back.
        let mut compositor = Compositor::start(protocol);
        compositor.command(Command::Echo(Echo::Absent));
        let qh = compositor.queue.handle();
        compositor.clipboard().set_text("mine".into(), &qh).unwrap();
        compositor.set_image(&image).unwrap();
        compositor.queue.roundtrip(&mut compositor.state).unwrap();
        compositor.select(&text_formats());
        compositor.settle_encode();
        compositor.until(|snapshot| snapshot.text.as_deref() == Some("theirs"));
        assert_eq!(
            compositor.published_images(),
            0,
            "{protocol:?}: absent echo"
        );
        assert_eq!(
            compositor.clipboard().state.current.text.as_deref(),
            Some("theirs")
        );

        // our echo arrives only after the other application's selection.
        let mut compositor = Compositor::start(protocol);
        compositor.command(Command::Echo(Echo::Held));
        let qh = compositor.queue.handle();
        compositor.clipboard().set_text("mine".into(), &qh).unwrap();
        compositor.set_image(&image).unwrap();
        compositor.queue.roundtrip(&mut compositor.state).unwrap();
        compositor.select(&text_formats());
        compositor.settle_encode();
        compositor.command(Command::ReleaseEchoes);
        compositor.settle_encode();
        assert_eq!(
            compositor.published_images(),
            0,
            "{protocol:?}: delayed echo"
        );

        // an earlier image of ours is still unconfirmed when another
        // application offers png too.
        let mut compositor = Compositor::start(protocol);
        compositor.set_image(&image).unwrap();
        compositor.settle_encode();
        assert_eq!(compositor.published_images(), 1);
        compositor.command(Command::Echo(Echo::Absent));
        compositor.set_image(&image).unwrap();
        compositor.queue.roundtrip(&mut compositor.state).unwrap();
        let (theirs, _) = png(1, 1);
        compositor.select(&[("image/png", Payload::Bytes(theirs))]);
        compositor.settle_encode();
        assert_eq!(compositor.published_images(), 1, "{protocol:?}: png echo");
    }
}

/// with no way to tell a late echo from another application, a pending image
/// is abandoned when any selection arrives after the compositor has seen the
/// request.
#[test]
fn late_echoes_conservatively_cancel_pending_images() {
    for protocol in PROTOCOLS {
        let image = handle("mine", 64, 64, noise(64, 64, 4));
        let mut compositor = Compositor::start(protocol);
        compositor.command(Command::Echo(Echo::Held));
        let qh = compositor.queue.handle();
        compositor.clipboard().set_text("mine".into(), &qh).unwrap();
        compositor.set_image(&image).unwrap();
        compositor.queue.roundtrip(&mut compositor.state).unwrap();
        compositor.command(Command::ReleaseEchoes);
        compositor.settle_encode();
        assert_eq!(compositor.published_images(), 0, "{protocol:?}");
        assert_eq!(
            compositor.observed(|observed| observed.selections.len()),
            1,
            "{protocol:?}: only the text was offered"
        );
    }
}

#[test]
fn delayed_drains_keep_only_the_latest_image_request() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        let first = handle("first", 32, 32, noise(32, 32, 5));
        let latest_rgba = noise(3, 3, 6);
        let latest = handle("latest", 3, 3, latest_rgba.clone());

        // the first image finishes while the ui is not draining results.
        compositor.set_image(&first).unwrap();
        compositor.until_codec_result();
        // conversion can finish before the first protocol dispatch. put the
        // foreign selection after the request's compositor ordering marker.
        compositor.queue.roundtrip(&mut compositor.state).unwrap();
        assert!(compositor.clipboard().codec.result_count() <= 1);
        // another application's selection retires it and its buffer.
        compositor.select(&[("text/plain", Payload::Bytes(b"theirs".to_vec()))]);
        assert_eq!(compositor.clipboard().codec.result_count(), 0);

        // newer requests replace older ones while results wait.
        compositor.set_image(&first).unwrap();
        compositor.until_codec_result();
        compositor.set_image(&latest).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(compositor.clipboard().codec.result_count() <= 1);
            compositor.queue.roundtrip(&mut compositor.state).unwrap();
            if compositor.clipboard().codec.result_count() == 1
                && compositor.clipboard().codec.pending_encode().is_none()
            {
                break;
            }
            assert!(Instant::now() < deadline, "{protocol:?}: never encoded");
        }
        compositor.settle_encode();
        assert_eq!(compositor.published_images(), 1, "{protocol:?}");
        let served = compositor.read_source("image/png");
        let decoded = thing_ui::png::decode_png(&served).unwrap();
        assert_eq!(decoded.rgba, latest_rgba);
    }
}

#[test]
fn text_sources_still_serve_only_text_and_clear_is_explicit() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        let qh = compositor.queue.handle();
        compositor
            .clipboard()
            .set_text("hello".into(), &qh)
            .unwrap();
        compositor.until(|snapshot| snapshot.text.as_deref() == Some("hello"));
        assert_eq!(compositor.read_source("text/plain"), b"hello");
        assert!(compositor.read_source("image/png").is_empty());
        compositor.clipboard().clear().unwrap();
        let snapshot = compositor.until(|snapshot| snapshot.text.is_none() && !snapshot.loading);
        assert!(snapshot.formats.is_empty());
        assert_eq!(
            compositor.observed(|observed| observed.selections.last().cloned()),
            Some(None)
        );
    }
}

#[test]
fn primary_selection_is_ignored_and_never_set() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        compositor.select(&[("text/plain", Payload::Bytes(b"regular".to_vec()))]);
        compositor.until(|snapshot| snapshot.text.as_deref() == Some("regular"));
        compositor.command(Command::Primary);
        let snapshot = compositor.until(|_| true);
        assert_eq!(snapshot.text.as_deref(), Some("regular"));
        assert_eq!(snapshot.formats, ["text/plain"]);
        let qh = compositor.queue.handle();
        compositor.clipboard().set_text("mine".into(), &qh).unwrap();
        compositor.until(|snapshot| snapshot.text.as_deref() == Some("mine"));
        assert_eq!(compositor.observed(|observed| observed.primary_requests), 0);
    }
}

#[test]
fn seat_teardown_device_loss_and_shutdown_end_image_work() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        compositor.select(&[("image/png", Payload::Stall)]);
        compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Available);
        compositor.clipboard().read_image().unwrap();
        let pending = handle("pending", 64, 64, noise(64, 64, 8));
        compositor.set_image(&pending).unwrap();
        compositor.clipboard().detach_seat();
        let snapshot = compositor.until(|snapshot| !snapshot.available);
        assert!(snapshot.formats.is_empty() && snapshot.image.is_none());
        assert!(compositor.clipboard().image_read.is_none());
        assert!(
            compositor
                .clipboard()
                .read_image()
                .unwrap_err()
                .contains("unavailable")
        );

        // an image requested for the removed seat never reaches the new one.
        let qh = compositor.queue.handle();
        let seat = compositor.seat.clone();
        compositor.clipboard().attach_seat(&seat, &qh);
        compositor.queue.roundtrip(&mut compositor.state).unwrap();
        compositor.settle_encode();
        assert_eq!(compositor.published_images(), 0, "{protocol:?}");

        compositor.select(&[("image/png", Payload::Stall)]);
        compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Available);
        compositor.command(Command::Finish);
        let snapshot = compositor.until(|snapshot| !snapshot.available);
        assert!(snapshot.error.contains("no longer available"));

        // dropping the clipboard with a stalled read, a write the compositor
        // never drains, and a busy codec returns at once.
        let mut compositor = Compositor::start(protocol);
        let qh = compositor.queue.handle();
        compositor
            .clipboard()
            .set_text("x".repeat(1024 * 1024), &qh)
            .unwrap();
        compositor.until(|snapshot| snapshot.text.is_some());
        let (reply, reader) = mpsc::channel();
        compositor.command(Command::ReadSource("text/plain".into(), reply));
        let unread = reader.recv_timeout(Duration::from_secs(10)).unwrap();
        compositor.until(|_| true);
        compositor.select(&[("image/png", Payload::Stall)]);
        compositor.until(|snapshot| status(snapshot) == ClipboardImageStatus::Available);
        compositor.clipboard().read_image().unwrap();
        // a large pixmap keeps the codec busy encoding while the clipboard drops.
        let large = handle("large", 2048, 2048, noise(2048, 2048, 9));
        compositor.set_image(&large).unwrap();
        let started = Instant::now();
        compositor.state.clipboard = None;
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(unread);
    }
}

impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ImageBarrier> for ClientState {
    fn event(
        state: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        event: wayland_client::protocol::wl_callback::Event,
        data: &ImageBarrier,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(
            event,
            wayland_client::protocol::wl_callback::Event::Done { .. }
        ) && let Some(clipboard) = state.clipboard.as_mut()
        {
            clipboard.image_barrier_done(data.0);
        }
    }
}

#[test]
fn pending_image_barriers_are_bounded_and_resume_after_the_compositor_replies() {
    for protocol in PROTOCOLS {
        let mut compositor = Compositor::start(protocol);
        let image = handle("copy", 2, 2, noise(2, 2, 7));
        for _ in 0..MAX_IMAGE_BARRIERS {
            compositor.set_image(&image).unwrap();
            assert!(compositor.clipboard().codec.result_count() <= 1);
        }
        assert!(
            compositor
                .set_image(&image)
                .unwrap_err()
                .contains("awaiting the compositor")
        );
        compositor.settle_encode();
        assert!(compositor.clipboard().barriers.is_empty());
        assert_eq!(compositor.published_images(), 1);
        compositor.set_image(&image).unwrap();
        compositor.settle_encode();
        assert_eq!(compositor.published_images(), 2);
    }
}
