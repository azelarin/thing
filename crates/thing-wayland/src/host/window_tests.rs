//! `ui.window` through the real host. a private compositor serves xdg-shell,
//! records every toplevel, popup, and layer request in order, and sends the
//! configures, closes, scales, and clicks a test asks for. event assertions
//! wait for a positive protocol witness.

use std::{
    collections::HashMap,
    fs::{self, File},
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use thing_runtime::SleepInhibitorStatus;
use wayland_client::{
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
};
use wayland_protocols::xdg::shell::server::{
    xdg_popup::{self as server_popup, XdgPopup as ServerPopup},
    xdg_positioner::XdgPositioner as ServerPositioner,
    xdg_surface::{self as server_xdg_surface, XdgSurface as ServerXdgSurface},
    xdg_toplevel::{self as server_toplevel, XdgToplevel as ServerToplevel},
    xdg_wm_base::{self as server_wm_base, XdgWmBase as ServerWmBase},
};
use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1::{self as server_layer_shell, ZwlrLayerShellV1 as ServerLayerShell},
    zwlr_layer_surface_v1::{self as server_layer, ZwlrLayerSurfaceV1 as ServerLayer},
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource,
    backend::{GlobalId, ObjectId},
    protocol::{
        wl_buffer::WlBuffer as ServerBuffer,
        wl_callback::WlCallback as ServerCallback,
        wl_compositor::{self as server_compositor, WlCompositor as ServerCompositor},
        wl_output::{self as server_output, WlOutput as ServerOutput},
        wl_pointer::{self as server_pointer, WlPointer as ServerPointer},
        wl_region::WlRegion as ServerRegion,
        wl_seat::{self as server_seat, WlSeat as ServerSeat},
        wl_shm::{self as server_shm, WlShm as ServerShm},
        wl_shm_pool::{self as server_shm_pool, WlShmPool as ServerShmPool},
        wl_surface::{self as server_surface, WlSurface as ServerSurface},
    },
};

use super::*;

mod geometry;
mod inspection;
mod links;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const WAIT: Duration = Duration::from_secs(10);
const LEFT_BUTTON: u32 = 0x110;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Record {
    Toplevel {
        toplevel: u32,
        surface: u32,
    },
    Title(u32, String),
    AppId(u32, String),
    MinSize(u32, i32, i32),
    MaxSize(u32, i32, i32),
    Maximized(u32, bool),
    Configure {
        surface: u32,
        serial: u32,
    },
    Ack {
        surface: u32,
        serial: u32,
    },
    Commit {
        surface: u32,
        buffer: bool,
    },
    DestroyToplevel(u32),
    /// a popup and the wl_surface of its xdg parent, when it has one.
    Popup {
        surface: u32,
        parent: Option<u32>,
    },
    DestroyPopup(u32),
    /// the host asked to move a popup, and the server configured it at the
    /// current popup position.
    Reposition {
        surface: u32,
        token: u32,
    },
    LayerPopup,
    Layer(u32),
    DestroyLayer(u32),
}

enum Command {
    /// a compositor-initiated move of a mapped popup.
    ConfigurePopup {
        surface: u32,
        position: (i32, i32),
    },
    /// completes every frame callback held while `hold_callbacks` was set.
    ReleaseCallbacks,
    Configure {
        toplevel: u32,
        size: (i32, i32),
        states: Vec<server_toplevel::State>,
    },
    Close(u32),
    PreferredScale {
        surface: u32,
        scale: i32,
    },
    Click {
        surface: u32,
        x: f64,
        y: f64,
    },
    Button {
        surface: u32,
        x: f64,
        y: f64,
        pressed: bool,
    },
    RemoveOutput,
}

#[derive(Default)]
struct Control {
    commands: Vec<Command>,
    records: Vec<Record>,
    /// where popups are placed in their parent when first configured or
    /// repositioned.
    popup_position: (i32, i32),
    /// keeps frame callbacks pending, as a compositor does for a surface it
    /// is not showing.
    hold_callbacks: bool,
}

const POPUP_SIZE: (i32, i32) = (100, 60);

struct ServerState {
    control: Arc<Mutex<Control>>,
    serial: u32,
    output: Option<GlobalId>,
    pointer: Option<ServerPointer>,
    surfaces: HashMap<u32, ServerSurface>,
    toplevels: HashMap<u32, (ServerToplevel, ServerXdgSurface)>,
    /// popups and layers configure on their first commit.
    popups: HashMap<u32, (ServerPopup, ServerXdgSurface, bool)>,
    layers: HashMap<u32, (ServerLayer, bool)>,
    pending_buffers: HashMap<ObjectId, ServerBuffer>,
    frame_callbacks: HashMap<ObjectId, Vec<ServerCallback>>,
    held_callbacks: Vec<ServerCallback>,
}

impl ServerState {
    fn record(&self, record: Record) {
        self.control.lock().unwrap().records.push(record);
    }

    fn configure_popup(&mut self, surface: u32, position: (i32, i32)) {
        let Some((popup, xdg, _)) = self.popups.get(&surface).cloned() else {
            return;
        };
        popup.configure(position.0, position.1, POPUP_SIZE.0, POPUP_SIZE.1);
        let serial = self.next_serial();
        xdg.configure(serial);
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    fn apply(&mut self, display: &mut DisplayHandle) {
        let commands = std::mem::take(&mut self.control.lock().unwrap().commands);
        for command in commands {
            match command {
                Command::ConfigurePopup { surface, position } => {
                    self.configure_popup(surface, position);
                }
                Command::ReleaseCallbacks => {
                    for callback in std::mem::take(&mut self.held_callbacks) {
                        callback.done(1);
                    }
                }
                Command::Configure {
                    toplevel,
                    size,
                    states,
                } => {
                    let Some((shell, xdg)) = self.toplevels.get(&toplevel).cloned() else {
                        continue;
                    };
                    let states = states
                        .into_iter()
                        .flat_map(|state| (state as u32).to_ne_bytes())
                        .collect();
                    shell.configure(size.0, size.1, states);
                    let serial = self.next_serial();
                    xdg.configure(serial);
                    let surface = xdg.data::<u32>().copied().unwrap_or_default();
                    self.record(Record::Configure { surface, serial });
                }
                Command::Close(toplevel) => {
                    if let Some((shell, _)) = self.toplevels.get(&toplevel) {
                        shell.close();
                    }
                }
                Command::PreferredScale { surface, scale } => {
                    if let Some(surface) = self.surfaces.get(&surface) {
                        surface.preferred_buffer_scale(scale);
                    }
                }
                Command::Click { surface, x, y } => {
                    let (Some(pointer), Some(surface)) =
                        (self.pointer.clone(), self.surfaces.get(&surface).cloned())
                    else {
                        continue;
                    };
                    let serial = self.next_serial();
                    pointer.enter(serial, &surface, x, y);
                    pointer.frame();
                    for state in [
                        server_pointer::ButtonState::Pressed,
                        server_pointer::ButtonState::Released,
                    ] {
                        let serial = self.next_serial();
                        pointer.button(serial, 0, LEFT_BUTTON, state);
                        pointer.frame();
                    }
                    let serial = self.next_serial();
                    pointer.leave(serial, &surface);
                    pointer.frame();
                }
                Command::Button {
                    surface,
                    x,
                    y,
                    pressed,
                } => {
                    let (Some(pointer), Some(surface)) =
                        (self.pointer.clone(), self.surfaces.get(&surface).cloned())
                    else {
                        continue;
                    };
                    if pressed {
                        let serial = self.next_serial();
                        pointer.enter(serial, &surface, x, y);
                    } else {
                        pointer.motion(0, x, y);
                    }
                    let serial = self.next_serial();
                    pointer.button(
                        serial,
                        0,
                        LEFT_BUTTON,
                        if pressed {
                            server_pointer::ButtonState::Pressed
                        } else {
                            server_pointer::ButtonState::Released
                        },
                    );
                    pointer.frame();
                }
                Command::RemoveOutput => {
                    if let Some(output) = self.output.take() {
                        display.remove_global::<ServerState>(output);
                    }
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
    ServerPositioner => (),
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

plain_globals!(ServerCompositor, ServerLayerShell, ServerWmBase);

impl ServerDispatch<ServerWmBase, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerWmBase,
        request: server_wm_base::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_wm_base::Request::CreatePositioner { id } => {
                data.init(id, ());
            }
            server_wm_base::Request::GetXdgSurface { id, surface } => {
                data.init(id, surface.id().protocol_id());
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ServerXdgSurface, u32> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        xdg: &ServerXdgSurface,
        request: server_xdg_surface::Request,
        surface: &u32,
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_xdg_surface::Request::GetToplevel { id } => {
                let toplevel = data.init(id, *surface);
                let id = toplevel.id().protocol_id();
                state.record(Record::Toplevel {
                    toplevel: id,
                    surface: *surface,
                });
                state.toplevels.insert(id, (toplevel, xdg.clone()));
            }
            server_xdg_surface::Request::GetPopup { id, parent, .. } => {
                let popup = data.init(id, *surface);
                let parent = parent.and_then(|parent| parent.data::<u32>().copied());
                state.record(Record::Popup {
                    surface: *surface,
                    parent,
                });
                state.popups.insert(*surface, (popup, xdg.clone(), false));
            }
            server_xdg_surface::Request::AckConfigure { serial } => state.record(Record::Ack {
                surface: *surface,
                serial,
            }),
            _ => {}
        }
    }
}

impl ServerDispatch<ServerToplevel, u32> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        toplevel: &ServerToplevel,
        request: server_toplevel::Request,
        _: &u32,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let id = toplevel.id().protocol_id();
        match request {
            server_toplevel::Request::SetTitle { title } => state.record(Record::Title(id, title)),
            server_toplevel::Request::SetAppId { app_id } => {
                state.record(Record::AppId(id, app_id))
            }
            server_toplevel::Request::SetMinSize { width, height } => {
                state.record(Record::MinSize(id, width, height))
            }
            server_toplevel::Request::SetMaxSize { width, height } => {
                state.record(Record::MaxSize(id, width, height))
            }
            server_toplevel::Request::SetMaximized => state.record(Record::Maximized(id, true)),
            server_toplevel::Request::UnsetMaximized => state.record(Record::Maximized(id, false)),
            server_toplevel::Request::Destroy => {
                state.toplevels.remove(&id);
                state.record(Record::DestroyToplevel(id));
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ServerPopup, u32> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerPopup,
        request: server_popup::Request,
        surface: &u32,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_popup::Request::Destroy => {
                state.popups.remove(surface);
                state.record(Record::DestroyPopup(*surface));
            }
            server_popup::Request::Reposition { token, .. } => {
                if let Some((popup, _, _)) = state.popups.get(surface) {
                    popup.repositioned(token);
                }
                let position = state.control.lock().unwrap().popup_position;
                state.configure_popup(*surface, position);
                state.record(Record::Reposition {
                    surface: *surface,
                    token,
                });
            }
            _ => {}
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
            let surface = surface.id().protocol_id();
            let layer = data.init(id, surface);
            state.record(Record::Layer(surface));
            state.layers.insert(surface, (layer, false));
        }
    }
}

impl ServerDispatch<ServerLayer, u32> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerLayer,
        request: server_layer::Request,
        surface: &u32,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_layer::Request::GetPopup { .. } => state.record(Record::LayerPopup),
            server_layer::Request::Destroy => {
                state.layers.remove(surface);
                state.record(Record::DestroyLayer(*surface));
            }
            _ => {}
        }
    }
}

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
        let object = surface.id();
        let id = object.protocol_id();
        state.surfaces.entry(id).or_insert_with(|| surface.clone());
        match request {
            server_surface::Request::Attach {
                buffer: Some(buffer),
                ..
            } => {
                state.pending_buffers.insert(object, buffer);
            }
            server_surface::Request::Frame { callback } => {
                let callback = data.init(callback, ());
                state
                    .frame_callbacks
                    .entry(object)
                    .or_default()
                    .push(callback);
            }
            server_surface::Request::Commit => {
                let buffer = state.pending_buffers.remove(&object);
                state.record(Record::Commit {
                    surface: id,
                    buffer: buffer.is_some(),
                });
                if let Some(buffer) = buffer {
                    buffer.release();
                }
                if let Some((layer, configured)) = state.layers.get_mut(&id)
                    && !*configured
                {
                    *configured = true;
                    let layer = layer.clone();
                    let serial = state.next_serial();
                    layer.configure(serial, WIDTH, 40);
                }
                if let Some((_, _, configured)) = state.popups.get_mut(&id)
                    && !*configured
                {
                    *configured = true;
                    let position = state.control.lock().unwrap().popup_position;
                    state.configure_popup(id, position);
                }
                let callbacks = state.frame_callbacks.remove(&object).unwrap_or_default();
                if state.control.lock().unwrap().hold_callbacks {
                    state.held_callbacks.extend(callbacks);
                } else {
                    for callback in callbacks {
                        callback.done(1);
                    }
                }
            }
            server_surface::Request::Destroy => {
                state.surfaces.remove(&id);
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
            1920,
            1080,
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

/// an in-process xdg-shell compositor with one output and one pointer.
struct PrivateWayland {
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl PrivateWayland {
    fn start(layer_shell: bool) -> (Self, Connection) {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        server_socket.set_nonblocking(true).unwrap();
        let mut display = Display::<ServerState>::new().unwrap();
        let mut handle = display.handle();
        let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
        handle.create_global::<ServerState, ServerCompositor, _>(6, ());
        handle.create_global::<ServerState, ServerShm, _>(1, ());
        let output = handle.create_global::<ServerState, ServerOutput, _>(4, ());
        handle.create_global::<ServerState, ServerSeat, _>(8, ());
        handle.create_global::<ServerState, ServerWmBase, _>(6, ());
        if layer_shell {
            handle.create_global::<ServerState, ServerLayerShell, _>(4, ());
        }
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_control = control.clone();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState {
                control: server_control,
                serial: 0,
                output: Some(output),
                pointer: None,
                surfaces: HashMap::new(),
                toplevels: HashMap::new(),
                popups: HashMap::new(),
                layers: HashMap::new(),
                pending_buffers: HashMap::new(),
                frame_callbacks: HashMap::new(),
                held_callbacks: Vec::new(),
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

/// a window host with inert desktop services. fields drop in order, so the
/// host disconnects before its compositor stops.
struct Host {
    app: App,
    event_loop: EventLoop<'static, App>,
    connection: Connection,
    qh: QueueHandle<App>,
    _buffer: SharedBuffer,
    wayland: PrivateWayland,
    directory: tempfile::TempDir,
}

impl Host {
    fn start(source: &str, layer_shell: bool) -> Self {
        Self::try_start(source, layer_shell).unwrap()
    }

    fn try_start(source: &str, layer_shell: bool) -> Result<Self, String> {
        let directory = tempfile::tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(&entry, source).unwrap();
        let (wayland, connection) = PrivateWayland::start(layer_shell);
        let (globals, mut queue) = registry_queue_init(&connection).unwrap();
        let qh = queue.handle();
        let buffer = shared_buffer(&globals, &qh);
        let event_loop = EventLoop::<'static, App>::try_new().unwrap();
        let (image_ping, _) = calloop::ping::make_ping().unwrap();
        let (clipboard_ping, _) = calloop::ping::make_ping().unwrap();
        let (activator_ping, _) = calloop::ping::make_ping().unwrap();
        let watch = Rc::new(Watch::new().unwrap());
        watch.set_files(std::slice::from_ref(&entry)).unwrap();
        let layer = LayerShell::bind(&globals, &qh).ok();
        let mut app = App {
            render: Render::new_test_any_size(buffer.buffer.clone()),
            image_ping,
            configurations: Configurations::new(entry, ServiceSnapshot::default()),
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
            drag: super::drag::HostDrag::new(calloop::ping::make_ping().unwrap().0).unwrap(),
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
            layer_shell_version: if layer.is_some() { 4 } else { 0 },
            layer_shell: layer,
            xdg_shell: XdgShell::bind(&globals, &qh).unwrap(),
            activation: None,
            fractional: None,
            viewporter: None,
            pointer: None,
            shm: globals.bind(&qh, 1..=1, ()).ok(),
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
        app.initialize(&qh)?;
        WaylandSource::new(connection.clone(), queue)
            .insert(event_loop.handle())
            .unwrap();
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
        host.until(|host| host.app.seat.is_some() && !host.toplevels().is_empty());
        Ok(host)
    }

    fn turn(&mut self) {
        self.app
            .turn(&mut self.event_loop, &self.connection, &self.qh)
            .unwrap();
    }

    fn until(&mut self, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "the host did not reach the expected state: {:#?}",
                self.records()
            );
            self.turn();
        }
    }

    fn command(&mut self, command: Command) {
        self.wayland.control.lock().unwrap().commands.push(command);
    }

    fn configure(&mut self, toplevel: u32, size: (i32, i32), states: Vec<server_toplevel::State>) {
        self.command(Command::Configure {
            toplevel,
            size,
            states,
        });
    }

    fn records(&self) -> Vec<Record> {
        self.wayland.control.lock().unwrap().records.clone()
    }

    /// toplevels in creation order, as (toplevel, wl_surface).
    fn toplevels(&self) -> Vec<(u32, u32)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Toplevel { toplevel, surface } => Some((toplevel, surface)),
                _ => None,
            })
            .collect()
    }

    fn title(&self, toplevel: u32) -> Option<String> {
        self.records()
            .into_iter()
            .rev()
            .find_map(|record| match record {
                Record::Title(id, title) if id == toplevel => Some(title),
                _ => None,
            })
    }

    fn has(&self, record: &Record) -> bool {
        self.records().contains(record)
    }

    /// commits of `surface` that attached a buffer, so each is one frame.
    fn frames(&self, surface: u32) -> usize {
        self.records()
            .iter()
            .filter(|record| {
                **record
                    == Record::Commit {
                        surface,
                        buffer: true,
                    }
            })
            .count()
    }

    /// popup surfaces in creation order.
    fn popups(&self) -> Vec<u32> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Popup { surface, .. } => Some(surface),
                _ => None,
            })
            .collect()
    }

    fn set_popup_position(&self, position: (i32, i32)) {
        self.wayland.control.lock().unwrap().popup_position = position;
    }

    fn hold_callbacks(&self, hold: bool) {
        self.wayland.control.lock().unwrap().hold_callbacks = hold;
    }

    fn position(&self, record: &Record) -> usize {
        self.records()
            .iter()
            .position(|candidate| candidate == record)
            .unwrap_or_else(|| panic!("missing {record:?}"))
    }

    fn set_service(&mut self, name: &str, value: &str) {
        self.app
            .configurations
            .set_service(name, Scalar::String(value.into()))
            .unwrap();
        self.app.refresh();
    }

    fn owner(&self) -> SurfaceOwner {
        SurfaceOwner {
            output: self.app.configurations.keys().next().unwrap().clone(),
            key: SurfaceKey::Named("settings".into()),
        }
    }

    fn write(&self, source: &str) {
        fs::write(self.directory.path().join("panel.luau"), source).unwrap();
    }

    fn reload(&mut self) {
        let qh = self.qh.clone();
        self.app.reload(&qh).unwrap();
    }
}

/// a settings window whose title reports its configured state, so every
/// configure the host accepts is visible as a set_title request.
const SETTINGS: &str = r#"
local clicks = ui.state(0)
local settings = ui.window_state('settings')
local guard = services.sleep_inhibitor { key = 'guard', reason = 'must stay stopped' }
local function window(extra)
    local properties = {
        key = 'settings',
        title = function()
            return string.format('settings %d %dx%d %s', clicks:get(), settings.width:get(),
                settings.height:get(), tostring(settings.maximized:get()))
        end,
        app_id = 'org.example.settings',
        size = { width = 400, height = 300 },
        min_size = { width = 200, height = 150 },
        max_size = { width = 1200, height = 900 },
        visible = function() return services.clock.time:get() ~= 'hide' end,
        style = { background = '#202020' },
        children = { ui.button {
            style = { width = 100, height = 40 },
            on_click = function()
                clicks:set(clicks:get() + 1)
                settings.set_maximized(true)
            end,
            children = {
                ui.text { text = 'more' },
                ui.popup {
                    visible = function() return clicks:get() % 2 == 1 end,
                    style = { width = 100, height = 60 },
                    children = { ui.text { text = 'popup' } },
                },
            },
        } },
    }
    for name, value in pairs(extra or {}) do properties[name] = value end
    return ui.window(properties)
end
return { window(EXTRA) }
"#;

fn settings(extra: &str) -> String {
    SETTINGS.replace("EXTRA", extra)
}

#[test]
fn a_window_handshakes_before_drawing_and_follows_configures() {
    let mut host = Host::start(&settings("nil"), true);
    let (toplevel, surface) = host.toplevels()[0];
    // properties and an empty initial commit come before any configure.
    assert_eq!(
        host.records()[1..5],
        [
            Record::Title(toplevel, "settings 0 0x0 false".into()),
            Record::AppId(toplevel, "org.example.settings".into()),
            Record::MinSize(toplevel, 200, 150),
            Record::MaxSize(toplevel, 1200, 900),
        ]
    );
    host.until(|host| {
        host.has(&Record::Commit {
            surface,
            buffer: false,
        })
    });
    assert!(!host.has(&Record::Commit {
        surface,
        buffer: true
    }));

    // a zero suggestion leaves the declared size; the buffer follows the ack.
    host.configure(toplevel, (0, 0), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 400x300 false"));
    let ack = host.position(&Record::Ack { surface, serial: 1 });
    assert!(host.records()[ack..].contains(&Record::Commit {
        surface,
        buffer: true
    }));

    // two configures before the host turns are both acknowledged; the
    // newer size is the one the configuration sees.
    host.configure(toplevel, (700, 500), vec![]);
    host.configure(toplevel, (900, 700), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 900x700 false"));
    assert!(host.has(&Record::Ack { surface, serial: 2 }));
    assert!(host.has(&Record::Ack { surface, serial: 3 }));

    // a maximized configure is the compositor's decision, even past
    // max_size; a zero axis keeps the current size within the limits.
    host.configure(
        toplevel,
        (1920, 1080),
        vec![server_toplevel::State::Maximized],
    );
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 1920x1080 true"));
    host.configure(toplevel, (0, 0), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 1200x900 false"));

    // a click routes to the button inside the window, and its popup is an
    // xdg child of the window rather than a layer popup.
    host.command(Command::Click {
        surface,
        // like a panel, the window root is a row that centers its button.
        x: 10.0,
        y: 450.0,
    });
    host.until(|host| {
        host.records().iter().any(|record| {
            matches!(record, Record::Popup { parent: Some(parent), .. } if *parent == surface)
        })
    });
    assert!(!host.has(&Record::LayerPopup));
    assert!(host.title(toplevel).unwrap().starts_with("settings 1 "));
    // the handler's request reaches the compositor as an event-only action.
    assert!(host.has(&Record::Maximized(toplevel, true)));
}

#[test]
fn a_close_without_a_handler_hides_until_the_declaration_shows_it_again() {
    let mut host = Host::start(&settings("nil"), true);
    let (toplevel, _) = host.toplevels()[0];
    host.configure(toplevel, (0, 0), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 400x300 false"));

    host.command(Command::Close(toplevel));
    host.until(|host| host.has(&Record::DestroyToplevel(toplevel)));
    // an unrelated refresh does not reopen it; surface synchronization runs
    // inside the refresh, so the check is immediate.
    host.set_service("clock.date", "tomorrow");
    assert!(host.app.root_index(&host.owner()).is_none());
    assert_eq!(host.toplevels().len(), 1);

    // hiding and showing the declaration opens a fresh window.
    host.set_service("clock.time", "hide");
    host.set_service("clock.time", "show");
    host.until(|host| host.toplevels().len() == 2);
    // the server may reuse the protocol id; the host sees a new toplevel
    // object, so nothing from the destroyed one can reach it.
    let (replacement, _) = host.toplevels()[1];
    host.configure(replacement, (0, 0), vec![]);
    host.until(|host| host.title(replacement).as_deref() == Some("settings 0 400x300 false"));
    let destroyed = host
        .records()
        .iter()
        .filter(|record| matches!(record, Record::DestroyToplevel(_)))
        .count();
    assert_eq!(destroyed, 1);
}

#[test]
fn on_close_decides_and_a_failing_callback_publishes_nothing() {
    let source = settings(
        r#"{
        on_close = function()
            if services.clock.date:get() == 'refuse' then
                guard.start()
                error('not now')
            end
            if services.clock.date:get() == 'keep' then return end
            clicks:set(100)
        end,
        visible = function() return clicks:get() < 100 end,
    }"#,
    );
    let mut host = Host::start(&source, true);
    let (toplevel, _) = host.toplevels()[0];
    host.configure(toplevel, (0, 0), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 400x300 false"));

    // a failing callback keeps the window and discards its action. the
    // configure after it is the ordered witness that the close was handled.
    host.set_service("clock.date", "refuse");
    host.command(Command::Close(toplevel));
    host.configure(toplevel, (500, 400), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 500x400 false"));
    assert!(!host.has(&Record::DestroyToplevel(toplevel)));
    assert_eq!(
        host.app.sleep.status("guard").map(|(status, _)| status),
        Some(SleepInhibitorStatus::Stopped)
    );

    // a callback that keeps the window open is respected.
    host.set_service("clock.date", "keep");
    host.command(Command::Close(toplevel));
    host.configure(toplevel, (600, 400), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 600x400 false"));
    assert!(!host.has(&Record::DestroyToplevel(toplevel)));

    // one that hides it closes the window.
    host.set_service("clock.date", "close");
    host.command(Command::Close(toplevel));
    host.until(|host| host.has(&Record::DestroyToplevel(toplevel)));
}

#[test]
fn windows_and_panels_are_independent_and_popups_leave_before_their_window() {
    let source = SETTINGS.replace(
        "return { window(EXTRA) }",
        "return { ui.panel { key = 'bar', style = { height = 40 } }, window(nil) }",
    );
    let mut host = Host::start(&source, true);
    host.until(|host| {
        host.records()
            .iter()
            .any(|record| matches!(record, Record::Layer(_)))
    });
    let layer = host
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::Layer(surface) => Some(surface),
            _ => None,
        })
        .unwrap();
    let (toplevel, surface) = host.toplevels()[0];
    host.configure(toplevel, (0, 0), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 400x300 false"));
    host.command(Command::Click {
        surface,
        // like a panel, the window root is a row that centers its button.
        x: 10.0,
        y: 150.0,
    });
    host.until(|host| {
        host.records()
            .iter()
            .any(|record| matches!(record, Record::Popup { parent: Some(parent), .. } if *parent == surface))
    });
    let popup = host
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::Popup { surface, .. } => Some(surface),
            _ => None,
        })
        .unwrap();
    host.until(|host| {
        host.has(&Record::Commit {
            surface: popup,
            buffer: true,
        })
    });

    // closing the window releases its popup first and leaves the panel.
    host.command(Command::Close(toplevel));
    host.until(|host| host.has(&Record::DestroyToplevel(toplevel)));
    assert!(
        host.position(&Record::DestroyPopup(popup))
            < host.position(&Record::DestroyToplevel(toplevel))
    );
    assert!(!host.has(&Record::DestroyLayer(layer)));
    assert!(
        host.app
            .surfaces
            .iter()
            .any(|surface| matches!(surface.role, Role::Panel(_)))
    );
}

#[test]
fn window_only_configurations_need_no_layer_shell() {
    let mut host = Host::start(&settings("nil"), false);
    let (toplevel, surface) = host.toplevels()[0];
    host.configure(toplevel, (0, 0), vec![]);
    host.until(|host| {
        host.has(&Record::Commit {
            surface,
            buffer: true,
        })
    });
    drop(host);

    // a panel still asks for layer shell, with the existing kind of error.
    let error = Host::try_start("return ui.panel { style = { height = 40 } }", false)
        .err()
        .unwrap();
    assert!(error.contains("wlr-layer-shell"), "{error}");
}

#[test]
fn rejected_reloads_keep_the_window_and_accepted_ones_update_it_in_place() {
    let mut host = Host::start(&settings("nil"), true);
    let (toplevel, _) = host.toplevels()[0];
    host.configure(toplevel, (0, 0), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 400x300 false"));

    host.write(&settings("{ size = { width = 0, height = 300 } }"));
    host.reload();
    host.configure(toplevel, (640, 480), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 640x480 false"));
    assert!(!host.has(&Record::DestroyToplevel(toplevel)));

    // an accepted reload keeps the toplevel; the replacement runtime only
    // changes what it says.
    host.write(&settings("{ title = 'replaced' }"));
    host.reload();
    host.until(|host| host.title(toplevel).as_deref() == Some("replaced"));
    assert_eq!(host.toplevels().len(), 1);
    assert!(!host.has(&Record::DestroyToplevel(toplevel)));
}

#[test]
fn reactive_app_ids_can_be_cleared() {
    let mut host = Host::start(
        &settings(
            r#"{
        app_id = function()
            return if services.clock.time:get() == 'clear' then '' else 'org.example.settings'
        end,
        min_size = function()
            return if services.clock.time:get() == 'clear' then { width = 200, height = 150 } else nil
        end,
    }"#,
        ),
        true,
    );
    let (toplevel, _) = host.toplevels()[0];
    host.set_service("clock.time", "clear");
    // size requests follow app-id updates on the same connection.
    host.until(|host| host.has(&Record::MinSize(toplevel, 200, 150)));
    assert!(host.has(&Record::AppId(toplevel, String::new())));
}

#[test]
fn reload_preserves_configure_state_when_node_ids_repeat() {
    let mut host = Host::start(&settings("nil"), true);
    let (toplevel, _) = host.toplevels()[0];
    host.configure(toplevel, (640, 480), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 640x480 false"));
    let owner = host.owner();
    let before = host
        .app
        .configurations
        .surface_root(&owner.output, &owner.key)
        .unwrap()
        .id;
    let index = host.app.root_index(&owner).unwrap();
    let focused = host
        .app
        .configurations
        .surface_root(&owner.output, &owner.key)
        .unwrap()
        .children[0]
        .id;
    host.app.surfaces[index]
        .input
        .focus_from_pointer(Some(focused));
    host.write(&settings("nil").replace("settings %d", "reloaded %d"));
    host.reload();
    host.turn();
    let root = host
        .app
        .configurations
        .surface_root(&owner.output, &owner.key)
        .unwrap();
    assert_eq!(root.id, before);
    assert_eq!(root.window.title, "reloaded 0 640x480 false");
    assert_eq!(host.app.surfaces[index].input.focused, None);
    assert_eq!(host.toplevels().len(), 1);
    assert!(!host.has(&Record::DestroyToplevel(toplevel)));
}

#[test]
fn scale_follows_the_compositor_and_the_owner_output_retires_the_window() {
    // a full-size blurred shadow fits the effect budget at scale 1 but not at
    // scale 2, so the preferred scale waits until the window shrinks.
    let source = settings(
        r#"{
        size = { width = 3000, height = 3000 },
        max_size = { width = 4000, height = 4000 },
        style = { box_shadow = { color = '#000000', blur = 32 } },
    }"#,
    );
    let mut host = Host::start(&source, true);
    let (toplevel, surface) = host.toplevels()[0];
    host.configure(toplevel, (0, 0), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 3000x3000 false"));
    let index = |host: &Host| host.app.root_index(&host.owner()).unwrap();
    let factor = |host: &Host| host.app.surfaces[index(host)].scale.factor();

    host.command(Command::PreferredScale { surface, scale: 2 });
    host.configure(toplevel, (3000, 2900), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 3000x2900 false"));
    assert_eq!(factor(&host), 1.0);

    host.configure(toplevel, (800, 600), vec![]);
    host.until(|host| host.title(toplevel).as_deref() == Some("settings 0 800x600 false"));
    assert_eq!(factor(&host), 2.0);

    // removing the owner output removes its runtime and the window with it.
    host.command(Command::RemoveOutput);
    host.until(|host| host.has(&Record::DestroyToplevel(toplevel)));
    assert!(host.app.surfaces.is_empty());
}
