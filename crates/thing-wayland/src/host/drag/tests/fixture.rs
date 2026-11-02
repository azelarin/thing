//! a private compositor for drag and drop through the real panel host. it
//! serves one panel, one pointer, and a version 3 data device, and acts as
//! the other side of every drag: it offers data to the panel, answers the
//! panel's reads, and selects actions the way a compositor does.

use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use thing_core::Kind;
use wayland_client::{
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
};
use wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase as ServerWmBase;
use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1::{self as server_layer_shell, ZwlrLayerShellV1 as ServerLayerShell},
    zwlr_layer_surface_v1::ZwlrLayerSurfaceV1 as ServerLayer,
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource,
    backend::{GlobalId, ObjectId},
    protocol::{
        wl_buffer::WlBuffer as ServerBuffer,
        wl_callback::WlCallback as ServerCallback,
        wl_compositor::{self as server_compositor, WlCompositor as ServerCompositor},
        wl_data_device::{self as server_data_device, WlDataDevice as ServerDataDevice},
        wl_data_device_manager::{
            self as server_data_device_manager, DndAction as ServerAction,
            WlDataDeviceManager as ServerDataDeviceManager,
        },
        wl_data_offer::{self as server_data_offer, WlDataOffer as ServerDataOffer},
        wl_data_source::{self as server_data_source, WlDataSource as ServerDataSource},
        wl_output::{self as server_output, WlOutput as ServerOutput},
        wl_pointer::{self as server_pointer, WlPointer as ServerPointer},
        wl_region::WlRegion as ServerRegion,
        wl_seat::{self as server_seat, WlSeat as ServerSeat},
        wl_shm::{self as server_shm, WlShm as ServerShm},
        wl_shm_pool::{self as server_shm_pool, WlShmPool as ServerShmPool},
        wl_surface::{self as server_surface, WlSurface as ServerSurface},
    },
};

use super::super::super::*;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 80;
pub(super) const COPY: u32 = 1;
pub(super) const MOVE: u32 = 2;
const WAIT: Duration = Duration::from_secs(10);
/// long enough for the private server and the host to exchange one command.
const SETTLE: Duration = Duration::from_millis(120);

pub(super) enum Command {
    /// a drag from another client enters the panel.
    DragEnter {
        x: f64,
        y: f64,
        mime_types: Vec<&'static str>,
        actions: u32,
        bytes: Vec<u8>,
        /// hold the write end of each read instead of answering it.
        stall: bool,
    },
    DragMotion(f64, f64),
    Drop,
    /// a primary press at a panel point.
    Press(f64, f64),
    Motion(f64, f64),
    Release,
    SourceAction(u32),
    SourceFinished,
    RemoveSeat,
    ClosePanel,
}

/// what the panel asked of the compositor.
#[derive(Default)]
pub(super) struct Observed {
    pub(super) accepts: Vec<Option<String>>,
    pub(super) receives: Vec<String>,
    pub(super) finishes: usize,
    pub(super) offer_destroys: usize,
    /// write ends of reads the compositor has not answered yet.
    pub(super) stalled: Vec<OwnedFd>,
    pub(super) presses: Vec<u32>,
    /// the serial of each `start_drag`, and whether it named the panel.
    pub(super) drags: Vec<(u32, bool)>,
    pub(super) source_mimes: Vec<String>,
    pub(super) source_actions: Vec<u32>,
    pub(super) source_destroys: usize,
}

#[derive(Default)]
struct Control {
    commands: Vec<Command>,
    applied: usize,
    observed: Observed,
}

struct OfferFixture {
    bytes: Vec<u8>,
    stall: bool,
    source_actions: u32,
}

struct ServerState {
    control: Arc<Mutex<Control>>,
    serial: u32,
    seat: Option<GlobalId>,
    pointer: Option<ServerPointer>,
    entered: bool,
    device: Option<ServerDataDevice>,
    source: Option<ServerDataSource>,
    panel: Option<ServerSurface>,
    layer: Option<(ServerLayer, bool)>,
    pending_buffers: HashMap<ObjectId, ServerBuffer>,
    frame_callbacks: HashMap<ObjectId, Vec<ServerCallback>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ServerState {
    fn observed(&self) -> MutexGuard<'_, Control> {
        lock(&self.control)
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    fn point_pointer(&mut self, x: f64, y: f64) {
        let (Some(pointer), Some(panel)) = (self.pointer.clone(), self.panel.clone()) else {
            return;
        };
        if self.entered {
            pointer.motion(1, x, y);
        } else {
            let serial = self.next_serial();
            pointer.enter(serial, &panel, x, y);
            self.entered = true;
        }
        pointer.frame();
    }

    fn apply(&mut self, display: &mut DisplayHandle) {
        let commands = std::mem::take(&mut lock(&self.control).commands);
        for command in commands {
            self.run(command, display);
            lock(&self.control).applied += 1;
        }
    }

    fn run(&mut self, command: Command, display: &mut DisplayHandle) {
        match command {
            Command::DragEnter {
                x,
                y,
                mime_types,
                actions,
                bytes,
                stall,
            } => {
                let (Some(device), Some(panel)) = (self.device.clone(), self.panel.clone()) else {
                    return;
                };
                let client = display.get_client(device.id()).unwrap();
                let offer = client
                    .create_resource::<ServerDataOffer, _, Self>(
                        display,
                        device.version(),
                        OfferFixture {
                            bytes,
                            stall,
                            source_actions: actions,
                        },
                    )
                    .unwrap();
                device.data_offer(&offer);
                for mime_type in mime_types {
                    offer.offer(mime_type.into());
                }
                offer.source_actions(ServerAction::from_bits_truncate(actions));
                let serial = self.next_serial();
                device.enter(serial, &panel, x, y, Some(&offer));
            }
            Command::DragMotion(x, y) => {
                if let Some(device) = &self.device {
                    device.motion(2, x, y);
                }
            }
            Command::Drop => {
                if let Some(device) = &self.device {
                    device.drop();
                }
            }
            Command::Press(x, y) => {
                self.point_pointer(x, y);
                let Some(pointer) = self.pointer.clone() else {
                    return;
                };
                let serial = self.next_serial();
                pointer.button(serial, 3, 0x110, server_pointer::ButtonState::Pressed);
                pointer.frame();
                self.observed().observed.presses.push(serial);
            }
            Command::Motion(x, y) => self.point_pointer(x, y),
            Command::Release => {
                let Some(pointer) = self.pointer.clone() else {
                    return;
                };
                let serial = self.next_serial();
                pointer.button(serial, 4, 0x110, server_pointer::ButtonState::Released);
                pointer.frame();
            }
            Command::SourceAction(action) => {
                if let Some(source) = &self.source {
                    source.action(ServerAction::from_bits_truncate(action));
                }
            }
            Command::SourceFinished => {
                if let Some(source) = &self.source {
                    source.dnd_drop_performed();
                    source.dnd_finished();
                }
            }
            Command::RemoveSeat => {
                if let Some(seat) = self.seat.take() {
                    display.remove_global::<ServerState>(seat);
                }
                self.pointer = None;
                self.device = None;
                self.entered = false;
            }
            Command::ClosePanel => {
                if let Some((layer, _)) = &self.layer {
                    layer.closed();
                }
            }
        }
    }
}

macro_rules! ignore_requests {
    ($($resource:ty => $data:ty),* $(,)?) => {$(
        impl ServerDispatch<$resource, $data> for ServerState {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$resource,
                _: <$resource as Resource>::Request,
                _: &$data,
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    )*};
}

ignore_requests! {
    ServerRegion => (),
    ServerCallback => (),
    ServerOutput => (),
    ServerBuffer => (),
    ServerPointer => (),
    ServerWmBase => (),
    ServerLayer => (),
}

macro_rules! plain_globals {
    ($($resource:ty),* $(,)?) => {$(
        impl GlobalDispatch<$resource, ()> for ServerState {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$resource>,
                _: &(),
                data: &mut DataInit<'_, Self>,
            ) {
                data.init(resource, ());
            }
        }
    )*};
}

plain_globals!(
    ServerCompositor,
    ServerLayerShell,
    ServerWmBase,
    ServerDataDeviceManager
);

impl ServerDispatch<ServerCompositor, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerCompositor,
        request: server_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_compositor::Request::CreateSurface { id } => {
                data.init(id, ());
            }
            server_compositor::Request::CreateRegion { id } => {
                data.init(id, ());
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ServerSurface, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &ServerSurface,
        request: server_surface::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        let id = surface.id();
        match request {
            server_surface::Request::Attach {
                buffer: Some(buffer),
                ..
            } => {
                state.pending_buffers.insert(id, buffer);
            }
            server_surface::Request::Frame { callback } => {
                let callback = data.init(callback, ());
                state.frame_callbacks.entry(id).or_default().push(callback);
            }
            server_surface::Request::Commit => {
                if state.panel.as_ref() == Some(surface)
                    && let Some((layer, configured)) = &mut state.layer
                    && !*configured
                {
                    *configured = true;
                    layer.configure(1, WIDTH, HEIGHT);
                }
                if let Some(buffer) = state.pending_buffers.remove(&id) {
                    buffer.release();
                }
                for callback in state.frame_callbacks.remove(&id).unwrap_or_default() {
                    callback.done(1);
                }
            }
            _ => {}
        }
    }
}

impl GlobalDispatch<ServerOutput, ()> for ServerState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerOutput>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        let output = data.init(resource, ());
        output.geometry(
            0,
            0,
            300,
            200,
            server_output::Subpixel::Unknown,
            "thing".into(),
            "private".into(),
            server_output::Transform::Normal,
        );
        output.mode(
            server_output::Mode::Current | server_output::Mode::Preferred,
            640,
            480,
            60_000,
        );
        output.scale(1);
        output.name("PRIVATE-1".into());
        output.done();
    }
}

impl GlobalDispatch<ServerShm, ()> for ServerState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerShm>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ()).format(server_shm::Format::Argb8888);
    }
}

impl ServerDispatch<ServerShm, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerShm,
        request: server_shm::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_shm::Request::CreatePool { id, .. } = request {
            data.init(id, ());
        }
    }
}

impl ServerDispatch<ServerShmPool, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerShmPool,
        request: server_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_shm_pool::Request::CreateBuffer { id, .. } = request {
            data.init(id, ());
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
        let seat = data.init(resource, ());
        seat.capabilities(server_seat::Capability::Pointer);
        seat.name("seat0".into());
    }
}

impl ServerDispatch<ServerSeat, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerSeat,
        request: server_seat::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_seat::Request::GetPointer { id } = request {
            state.pointer = Some(data.init(id, ()));
        }
    }
}

impl ServerDispatch<ServerLayerShell, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerLayerShell,
        request: server_layer_shell::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_layer_shell::Request::GetLayerSurface { id, surface, .. } = request {
            assert!(state.layer.is_none(), "the fixture serves one panel");
            state.layer = Some((data.init(id, ()), false));
            state.panel = Some(surface);
        }
    }
}

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
            origin,
            serial,
            ..
        } = request
        {
            let panel = state.panel.as_ref() == Some(&origin);
            state.observed().observed.drags.push((serial, panel));
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
        let mut control = state.observed();
        let observed = &mut control.observed;
        match request {
            server_data_source::Request::Offer { mime_type } => {
                observed.source_mimes.push(mime_type);
            }
            server_data_source::Request::SetActions { dnd_actions } => {
                observed.source_actions.push(bits(dnd_actions));
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
        offer: &ServerDataOffer,
        request: server_data_offer::Request,
        data: &OfferFixture,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let mut control = state.observed();
        let observed = &mut control.observed;
        match request {
            server_data_offer::Request::Accept { mime_type, .. } => {
                observed.accepts.push(mime_type);
            }
            server_data_offer::Request::SetActions {
                dnd_actions,
                preferred_action,
            } => {
                // like a compositor, select the preferred action when both
                // sides allow it, otherwise any common one.
                let common = bits(dnd_actions) & data.source_actions;
                let preferred = bits(preferred_action) & common;
                let selected = if preferred != 0 {
                    preferred
                } else {
                    common & common.wrapping_neg()
                };
                offer.action(ServerAction::from_bits_truncate(selected));
            }
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

fn bits(value: wayland_server::WEnum<ServerAction>) -> u32 {
    match value {
        wayland_server::WEnum::Value(value) => value.bits(),
        wayland_server::WEnum::Unknown(value) => value,
    }
}

struct PrivateWayland {
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl PrivateWayland {
    fn start() -> (Self, Connection) {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        server_socket.set_nonblocking(true).unwrap();
        let mut display = Display::<ServerState>::new().unwrap();
        let mut handle = display.handle();
        let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
        handle.create_global::<ServerState, ServerCompositor, _>(6, ());
        handle.create_global::<ServerState, ServerShm, _>(1, ());
        handle.create_global::<ServerState, ServerOutput, _>(4, ());
        let seat = handle.create_global::<ServerState, ServerSeat, _>(8, ());
        handle.create_global::<ServerState, ServerLayerShell, _>(4, ());
        handle.create_global::<ServerState, ServerWmBase, _>(1, ());
        handle.create_global::<ServerState, ServerDataDeviceManager, _>(3, ());
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_control = control.clone();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState {
                control: server_control,
                serial: 0,
                seat: Some(seat),
                pointer: None,
                entered: false,
                device: None,
                source: None,
                panel: None,
                layer: None,
                pending_buffers: HashMap::new(),
                frame_callbacks: HashMap::new(),
            };
            while !server_stop.load(Ordering::Relaxed) {
                match display
                    .backend()
                    .dispatch_single_client(&mut state, client.id())
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    // the host disconnected, as it does when a test fails.
                    Err(_) => break,
                }
                let mut handle = display.handle();
                state.apply(&mut handle);
                let _ = display.backend().flush(Some(client.id()));
                thread::sleep(Duration::from_millis(1));
            }
        });
        let wayland = Self {
            control,
            stop,
            server: Some(server),
        };
        (wayland, Connection::from_socket(client_socket).unwrap())
    }
}

impl Drop for PrivateWayland {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

struct SharedBuffer {
    _file: File,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
}

impl Drop for SharedBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

fn shared_buffer(globals: &GlobalList, qh: &QueueHandle<App>) -> SharedBuffer {
    let shm: wl_shm::WlShm = globals.bind(qh, 1..=1, ()).unwrap();
    let file = tempfile::tempfile().unwrap();
    let size = WIDTH * HEIGHT * 4;
    file.set_len(u64::from(size)).unwrap();
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        WIDTH as i32,
        HEIGHT as i32,
        (WIDTH * 4) as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    SharedBuffer {
        _file: file,
        pool,
        buffer,
    }
}

/// a panel host with inert desktop services whose drag worker wakes the loop
/// as it does in `run_with_options`. fields drop in order, so the host
/// disconnects before its compositor stops.
pub(super) struct Host {
    pub(super) app: App,
    event_loop: EventLoop<'static, App>,
    connection: Connection,
    qh: QueueHandle<App>,
    _buffer: SharedBuffer,
    wayland: PrivateWayland,
    directory: tempfile::TempDir,
}

impl Host {
    pub(super) fn start(source: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let panel = directory.path().join("panel.luau");
        fs::write(&panel, source).unwrap();
        let (wayland, connection) = PrivateWayland::start();
        let (globals, mut queue) = registry_queue_init(&connection).unwrap();
        let qh = queue.handle();
        let buffer = shared_buffer(&globals, &qh);
        let event_loop = EventLoop::<'static, App>::try_new().unwrap();
        let (image_ping, _) = calloop::ping::make_ping().unwrap();
        let (clipboard_ping, _) = calloop::ping::make_ping().unwrap();
        let (activator_ping, _) = calloop::ping::make_ping().unwrap();
        let (drag_ping, drag_source) = calloop::ping::make_ping().unwrap();
        let watch = Rc::new(Watch::new().unwrap());
        watch.set_files(std::slice::from_ref(&panel)).unwrap();
        let mut app = App {
            render: Render::new_test(buffer.buffer.clone(), (WIDTH, HEIGHT)),
            image_ping,
            configurations: Configurations::new(panel, ServiceSnapshot::default()),
            audio: Audio::inert().unwrap(),
            media: Media::inert().unwrap(),
            applications: Applications::inert().unwrap(),
            application_catalog: Arc::default(),
            activator: Activator::start_on(
                &format!(
                    "unix:path={}",
                    directory.path().join("unused-bus").display()
                ),
                activator_ping,
            )
            .unwrap(),
            power: Power::inert().unwrap(),
            sleep: crate::sleep::SleepInhibition::inert(),
            niri: Niri::inert().unwrap(),
            notifications: None,
            system_tray: None,
            clipboard: None,
            focused_clipboard: FocusedClipboard::bind(&globals, &qh, clipboard_ping).unwrap(),
            pending_paste: None,
            drag: super::super::HostDrag::new(drag_ping).unwrap(),
            input_method: InputMethod::bind(&globals, &qh),
            launcher: Launcher::for_tests(),
            managed_processes: None,
            file_watches: None,
            pending_launches: PendingLaunches::new(),
            pending_ui_actions: Vec::new(),
            watch,
            reload_at: None,
            hover: HoverStates::default(),
            surfaces: Vec::new(),
            closed_surfaces: Vec::new(),
            pending_window_states: Vec::new(),
            registry: RegistryState::new(&globals),
            idle_inhibit: IdleInhibitState::default(),
            idle_notify: crate::idle_notify::State::default(),
            seats: SeatState::new(&globals, &qh),
            outputs: OutputState::new(&globals, &qh),
            compositor: CompositorState::bind(&globals, &qh).unwrap(),
            layer_shell: Some(LayerShell::bind(&globals, &qh).unwrap()),
            layer_shell_version: 4,
            xdg_shell: XdgShell::bind(&globals, &qh).unwrap(),
            activation: None,
            fractional: None,
            viewporter: None,
            pointer: None,
            shm: None,
            cursor: crate::cursor::CursorState::default(),
            pointer_capture: None,
            keyboard: None,
            seat: None,
            keyboard_surface: None,
            next_text_input_generation: 1,
            queue: qh.clone(),
            loop_handle: event_loop.handle(),
            modifiers: Modifiers::default(),
            keyboard_keymap: None,
            repeat: RepeatSession::default(),
            repeat_token: None,
            loading_outputs: true,
            exit: false,
            error: None,
            commands: None,
            inspector: None,
        };
        queue.roundtrip(&mut app).unwrap();
        app.initialize(&qh).unwrap();
        WaylandSource::new(connection.clone(), queue)
            .insert(event_loop.handle())
            .unwrap();
        // the same callback `run_with_options` installs for drag transfers.
        event_loop
            .handle()
            .insert_source(drag_source, |_, _, app| app.update_drag_drop())
            .unwrap();
        // an idle host waits without a deadline. a short heartbeat returns
        // control to the test so its own deadlines apply.
        event_loop
            .handle()
            .insert_source(
                Timer::from_duration(Duration::from_millis(10)),
                |_, _, _| TimeoutAction::ToDuration(Duration::from_millis(10)),
            )
            .unwrap();
        let mut host = Self {
            app,
            event_loop,
            connection,
            qh,
            _buffer: buffer,
            wayland,
            directory,
        };
        host.until("the presented panel and its data device", |host| {
            host.app.pointer.is_some()
                && host.app.focused_clipboard.binding().is_some()
                && host
                    .app
                    .surfaces
                    .first()
                    .is_some_and(|surface| surface.layouts.presented().is_some())
        });
        host
    }

    fn turn(&mut self) {
        self.app
            .turn(&mut self.event_loop, &self.connection, &self.qh)
            .unwrap();
    }

    /// turns the host until `done` holds, failing at the deadline.
    pub(super) fn until(&mut self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; log: {}",
                self.log()
            );
            self.turn();
        }
    }

    /// keeps the host running for a while, for effects that must not happen.
    pub(super) fn settle(&mut self) {
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            self.turn();
        }
    }

    /// has the compositor carry out `command`, then lets the host answer.
    pub(super) fn command(&mut self, command: Command) {
        let target = {
            let mut control = lock(&self.wayland.control);
            control.commands.push(command);
            control.applied + control.commands.len()
        };
        self.until("the compositor command", |host| {
            lock(&host.wayland.control).applied >= target
        });
        self.settle();
    }

    pub(super) fn observed(&self) -> impl std::ops::DerefMut<Target = Observed> + '_ {
        struct Guard<'a>(MutexGuard<'a, Control>);
        impl std::ops::Deref for Guard<'_> {
            type Target = Observed;
            fn deref(&self) -> &Observed {
                &self.0.observed
            }
        }
        impl std::ops::DerefMut for Guard<'_> {
            fn deref_mut(&mut self) -> &mut Observed {
                &mut self.0.observed
            }
        }
        Guard(lock(&self.wayland.control))
    }

    /// the panel's handler log: the first text widget in the current tree.
    pub(super) fn log(&self) -> String {
        fn text(node: &thing_core::Node) -> Option<&str> {
            if node.kind == Kind::Text {
                return Some(&node.text);
            }
            node.children.iter().find_map(text)
        }
        self.app
            .surfaces
            .first()
            .and_then(|surface| self.app.configurations.root(&surface.output))
            .or_else(|| {
                self.app
                    .outputs
                    .outputs()
                    .find_map(|output| self.app.configurations.root(&output))
            })
            .and_then(text)
            .unwrap_or_default()
            .to_owned()
    }

    /// whether the shared state `key` holds the string `expected`.
    pub(super) fn shared_is(&self, key: &str, expected: &str) -> bool {
        self.app
            .configurations
            .shared_states()
            .get(key)
            .is_some_and(|entry| {
                entry.value.as_ref() == &thing_runtime::CallValue::String(expected.into())
            })
    }

    /// replaces the panel source and reloads it, as the file watch does.
    pub(super) fn reload(&mut self, source: &str) {
        fs::write(self.directory.path().join("panel.luau"), source).unwrap();
        let qh = self.qh.clone();
        self.app.reload(&qh).unwrap();
    }
}
