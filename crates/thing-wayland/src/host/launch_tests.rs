//! application launches through the real panel host: a pointer click on a
//! private wayland server, the host's activation token request, and the real
//! activation worker and launcher against a private d-bus daemon.

use std::{
    collections::HashMap,
    fs::{self, File},
    os::{fd::AsFd, unix::fs::PermissionsExt, unix::net::UnixStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use tempfile::TempDir;
use wayland_client::{
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
};
use wayland_protocols::xdg::{
    activation::v1::server::{
        xdg_activation_token_v1::{self as server_token, XdgActivationTokenV1 as ServerToken},
        xdg_activation_v1::{self as server_activation, XdgActivationV1 as ServerActivation},
    },
    shell::server::xdg_wm_base::XdgWmBase as ServerWmBase,
};
use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1::{self as server_layer_shell, ZwlrLayerShellV1 as ServerLayerShell},
    zwlr_layer_surface_v1::{self as server_layer_surface, ZwlrLayerSurfaceV1 as ServerLayer},
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource,
    backend::ObjectId,
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
use crate::applications::test_bus::{Application, Behavior, Bus, Received};

const PANEL_WIDTH: u32 = 320;
const PANEL_HEIGHT: u32 = 40;
const PROBE_ENV: &str = "THING_APPLICATION_LAUNCH_PROBE";
const PROBE_TIMEOUT: Duration = Duration::from_secs(40);
const WAIT: Duration = Duration::from_secs(10);
// the quiet time after which a launch that should not happen is taken as absent.
const SETTLE: Duration = Duration::from_millis(400);
const RELEASE_SERIAL: u32 = 7003;

// four 80 pixel buttons, each launching one entry.
const PANEL: &str = r#"
local function launcher(id)
    return ui.button {
        key = id,
        style = { width = 80, height = 40 },
        on_click = function() services.applications.launch(id) end,
    }
end
return ui.panel {
    style = { width = 320, height = 40 },
    children = {
        ui.row {
            children = {
                launcher('org.example.Viewer.desktop'),
                launcher('org.example.Missing.desktop'),
                launcher('org.example.Failing.desktop'),
                launcher('plain.desktop'),
            },
        },
    },
}
"#;

#[derive(Clone, Copy)]
enum Button {
    Viewer,
    Missing,
    Failing,
    Plain,
}

impl Button {
    fn x(self) -> f64 {
        40.0 + 80.0 * self as u8 as f64
    }
}

/// what the compositor saw on one activation token commit.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TokenRequest {
    serial: Option<(u32, ObjectId)>,
    surface: Option<ObjectId>,
}

#[derive(Default)]
struct Control {
    hold_tokens: bool,
    clicks: Vec<Button>,
    release_tokens: bool,
    token_requests: Vec<TokenRequest>,
    seat: Option<ObjectId>,
    panel: Option<ObjectId>,
    released: usize,
}

struct LayerState {
    layer: ServerLayer,
    size: (u32, u32),
    configured: bool,
}

struct ServerState {
    control: Arc<Mutex<Control>>,
    pointer: Option<ServerPointer>,
    layers: HashMap<ObjectId, LayerState>,
    surfaces: HashMap<ObjectId, ServerSurface>,
    pending_buffers: HashMap<ObjectId, ServerBuffer>,
    frame_callbacks: HashMap<ObjectId, Vec<ServerCallback>>,
    held: Vec<ServerToken>,
    next_token: u32,
    next_serial: u32,
}

impl ServerState {
    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn token(&mut self) -> String {
        self.next_token += 1;
        format!("token-{}", self.next_token)
    }

    /// carries out requests from the test thread between client dispatches.
    fn run_commands(&mut self) {
        let (clicks, release) = {
            let mut control = self.control();
            let release = std::mem::take(&mut control.release_tokens);
            (std::mem::take(&mut control.clicks), release)
        };
        for button in clicks {
            let pointer = self.pointer.clone().expect("the host has a pointer");
            let panel = self.control().panel.clone().expect("the panel exists");
            let surface = self.surfaces[&panel].clone();
            self.next_serial += 1;
            pointer.enter(self.next_serial, &surface, button.x(), 20.0);
            pointer.frame();
            self.next_serial += 1;
            pointer.button(
                self.next_serial,
                1,
                0x110,
                server_pointer::ButtonState::Pressed,
            );
            pointer.frame();
            pointer.button(
                RELEASE_SERIAL,
                2,
                0x110,
                server_pointer::ButtonState::Released,
            );
            pointer.frame();
        }
        if release {
            for token in std::mem::take(&mut self.held) {
                let value = self.token();
                token.done(value);
                self.control().released += 1;
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
    ServerActivation
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
                if let Some(layer) = state.layers.get_mut(&id)
                    && !layer.configured
                {
                    layer.configured = true;
                    let (width, height) = layer.size;
                    layer.layer.configure(1, width, height);
                }
                if let Some(buffer) = state.pending_buffers.remove(&id) {
                    buffer.release();
                }
                for callback in state.frame_callbacks.remove(&id).unwrap_or_default() {
                    callback.done(1);
                }
            }
            server_surface::Request::Destroy => {
                state.layers.remove(&id);
                state.surfaces.remove(&id);
                state.pending_buffers.remove(&id);
                state.frame_callbacks.remove(&id);
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
        if output.version() >= 4 {
            output.name("PRIVATE-1".into());
            output.description("private output".into());
        }
        if output.version() >= 2 {
            output.done();
        }
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
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerSeat>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        let seat = data.init(resource, ());
        state.control().seat = Some(seat.id());
        seat.capabilities(server_seat::Capability::Pointer);
        if seat.version() >= 2 {
            seat.name("seat0".into());
        }
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
            let surface_id = surface.id();
            let layer = data.init(id, surface_id.clone());
            state.surfaces.insert(surface_id.clone(), surface);
            state.layers.insert(
                surface_id.clone(),
                LayerState {
                    layer,
                    size: (PANEL_WIDTH, PANEL_HEIGHT),
                    configured: false,
                },
            );
            state.control().panel = Some(surface_id);
        }
    }
}

impl ServerDispatch<ServerLayer, ObjectId> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerLayer,
        request: server_layer_surface::Request,
        surface: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let server_layer_surface::Request::SetSize { width, height } = request
            && let Some(layer) = state.layers.get_mut(surface)
            && width != 0
            && height != 0
        {
            layer.size = (width, height);
        }
    }
}

impl ServerDispatch<ServerActivation, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerActivation,
        request: server_activation::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_activation::Request::GetActivationToken { id } = request {
            data.init(
                id,
                Mutex::new(TokenRequest {
                    serial: None,
                    surface: None,
                }),
            );
        }
    }
}

impl ServerDispatch<ServerToken, Mutex<TokenRequest>> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        token: &ServerToken,
        request: server_token::Request,
        pending: &Mutex<TokenRequest>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let mut pending = pending.lock().unwrap();
        match request {
            server_token::Request::SetSerial { serial, seat } => {
                pending.serial = Some((serial, seat.id()));
            }
            server_token::Request::SetSurface { surface } => pending.surface = Some(surface.id()),
            server_token::Request::Commit => {
                let request = pending.clone();
                drop(pending);
                let hold = {
                    let mut control = state.control();
                    control.token_requests.push(request);
                    control.hold_tokens
                };
                if hold {
                    state.held.push(token.clone());
                } else {
                    let value = state.token();
                    token.done(value);
                }
            }
            _ => {}
        }
    }
}

/// an in-process compositor with just the globals the panel host needs.
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
        handle.create_global::<ServerState, ServerSeat, _>(7, ());
        handle.create_global::<ServerState, ServerLayerShell, _>(4, ());
        handle.create_global::<ServerState, ServerWmBase, _>(1, ());
        handle.create_global::<ServerState, ServerActivation, _>(1, ());
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_control = control.clone();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState {
                control: server_control,
                pointer: None,
                layers: HashMap::new(),
                surfaces: HashMap::new(),
                pending_buffers: HashMap::new(),
                frame_callbacks: HashMap::new(),
                held: Vec::new(),
                next_token: 0,
                next_serial: 7000,
            };
            while !server_stop.load(Ordering::Relaxed) {
                match display
                    .backend()
                    .dispatch_single_client(&mut state, client.id())
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    // the host closed its connection, as it does when a probe fails.
                    Err(_) => break,
                }
                state.run_commands();
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

    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    let size = PANEL_WIDTH * PANEL_HEIGHT * 4;
    file.set_len(u64::from(size)).unwrap();
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        PANEL_WIDTH as i32,
        PANEL_HEIGHT as i32,
        (PANEL_WIDTH * 4) as i32,
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

/// a running panel host with private services. fields drop in order, so the
/// host disconnects before its compositor and bus stop.
struct Host {
    app: App,
    event_loop: EventLoop<'static, App>,
    connection: Connection,
    qh: QueueHandle<App>,
    _buffer: SharedBuffer,
    wayland: PrivateWayland,
    bus: Bus,
    applications: PathBuf,
    output: PathBuf,
    work: PathBuf,
    _directory: TempDir,
}

impl Host {
    /// starts the host. `token_wait` replaces the 250 ms limit on waiting for
    /// an activation token where a scenario needs a longer one.
    fn start(token_wait: Option<Duration>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let applications = root.join("applications");
        let work = root.join("work dir");
        fs::create_dir(&applications).unwrap();
        fs::create_dir(&work).unwrap();
        let output = root.join("helper output");
        let helper = root.join("helper");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\n{{ printf 'run cwd=<%s> token=<%s>' \"$PWD\" \"${{XDG_ACTIVATION_TOKEN-unset}}\"; \
                 for arg do printf ' arg=<%s>' \"$arg\"; done; printf '\\n'; }} >> '{}'\n",
                output.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let entry = |id: &str, extra: &str| {
            fs::write(
                applications.join(id),
                format!(
                    "[Desktop Entry]\nType=Application\nName={id}\nPath={}\n\
                     Exec=\"{}\" \"{id}\" \"--literal=\\\\$HOME;\\\\`x\\\\`\"\n{extra}",
                    work.display(),
                    helper.display()
                ),
            )
            .unwrap();
        };
        for id in [
            "org.example.Viewer.desktop",
            "org.example.Missing.desktop",
            "org.example.Failing.desktop",
        ] {
            entry(id, "DBusActivatable=true\n");
        }
        entry("plain.desktop", "");
        let panel = root.join("panel.luau");
        fs::write(&panel, PANEL).unwrap();

        let bus = Bus::start(&[]);
        let (wayland, connection) = PrivateWayland::start();
        let (globals, mut queue) = registry_queue_init(&connection).unwrap();
        let qh = queue.handle();
        let buffer = shared_buffer(&globals, &qh);
        let event_loop = EventLoop::<'static, App>::try_new().unwrap();
        let (image_ping, _) = calloop::ping::make_ping().unwrap();
        let (applications_ping, applications_source) = calloop::ping::make_ping().unwrap();
        let (activator_ping, activator_source) = calloop::ping::make_ping().unwrap();
        let (clipboard_ping, _) = calloop::ping::make_ping().unwrap();
        let watch = Rc::new(Watch::new().unwrap());
        watch.set_files(std::slice::from_ref(&panel)).unwrap();
        let mut app = App {
            render: Render::new_test(buffer.buffer.clone(), (PANEL_WIDTH, PANEL_HEIGHT)),
            image_ping,
            configurations: Configurations::new(panel, ServiceSnapshot::default()),
            audio: Audio::inert().unwrap(),
            media: Media::inert().unwrap(),
            applications: Applications::start_in(vec![applications.clone()], applications_ping)
                .unwrap(),
            application_catalog: Arc::default(),
            activator: Activator::start_on(&bus.address, activator_ping).unwrap(),
            power: Power::inert().unwrap(),
            niri: Niri::inert().unwrap(),
            notifications: None,
            system_tray: None,
            clipboard: None,
            focused_clipboard: FocusedClipboard::bind(&globals, &qh, clipboard_ping).unwrap(),
            pending_paste: None,
            input_method: InputMethod::bind(&globals, &qh),
            launcher: Launcher::for_tests(),
            managed_processes: None,
            pending_launches: match token_wait {
                Some(wait) => PendingLaunches::with_limits(128, wait),
                None => PendingLaunches::new(),
            },
            pending_ui_actions: Vec::new(),
            watch,
            reload_at: None,
            hover: HoverStates::default(),
            surfaces: Vec::new(),
            closed_surfaces: Vec::new(),
            registry: RegistryState::new(&globals),
            idle_inhibit: IdleInhibitState::default(),
            seats: SeatState::new(&globals, &qh),
            outputs: OutputState::new(&globals, &qh),
            compositor: CompositorState::bind(&globals, &qh).unwrap(),
            layer_shell: LayerShell::bind(&globals, &qh).unwrap(),
            layer_shell_version: 4,
            xdg_shell: XdgShell::bind(&globals, &qh).unwrap(),
            activation: globals.bind(&qh, 1..=1, ()).ok(),
            fractional: None,
            viewporter: None,
            pointer: None,
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
        };
        assert!(app.activation.is_some());
        queue.roundtrip(&mut app).unwrap();
        app.initialize(&qh).unwrap();
        // the same callbacks `run_with_options` installs for these services.
        event_loop
            .handle()
            .insert_source(applications_source, |_, _, app| app.update_applications())
            .unwrap();
        event_loop
            .handle()
            .insert_source(activator_source, |_, _, app| {
                app.start_activation_fallbacks()
            })
            .unwrap();
        WaylandSource::new(connection.clone(), queue)
            .insert(event_loop.handle())
            .unwrap();
        // an idle host waits for events without a deadline. a short heartbeat
        // returns control to the test so its own deadlines apply.
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
            bus,
            applications,
            output,
            work,
            _directory: directory,
        };
        host.wait_until("the panel and its application list", |host| {
            host.app.pointer.is_some()
                && host
                    .app
                    .application_catalog
                    .revision("plain.desktop")
                    .is_some()
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

    /// turns the host loop until `condition` holds, failing at the deadline.
    fn wait_until(&mut self, what: &str, mut condition: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !condition(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.turn();
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// keeps the host running for a while, for launches that must not happen.
    fn settle(&mut self) {
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            self.turn();
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn click(&mut self, button: Button) {
        let before = self.wayland.control().token_requests.len();
        self.wayland.control().clicks.push(button);
        self.wait_until("the activation token request", |host| {
            host.wayland.control().token_requests.len() > before
        });
    }

    fn runs(&self) -> Vec<String> {
        fs::read_to_string(&self.output)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn expected_run(&self, id: &str, token: &str) -> String {
        format!(
            "run cwd=<{}> token=<{token}> arg=<{id}> arg=<--literal=$HOME;`x`>",
            self.work.display()
        )
    }

    fn write_entry(&self, id: &str, exec: &str) {
        fs::write(
            self.applications.join(id),
            format!("[Desktop Entry]\nType=Application\nName={id}\nExec={exec}\n"),
        )
        .unwrap();
    }

    fn assert_token_origin(&self) {
        let control = self.wayland.control();
        let request = control.token_requests.last().unwrap();
        assert_eq!(
            request.serial,
            Some((RELEASE_SERIAL, control.seat.clone().unwrap()))
        );
        assert_eq!(request.surface, control.panel);
    }
}

fn activation_data(token: &str) -> Vec<(String, String)> {
    vec![
        ("activation-token".into(), token.into()),
        ("desktop-startup-id".into(), token.into()),
    ]
}

fn token_reaches_the_application_once() {
    let mut host = Host::start(None);
    let viewer = Application::start(&host.bus, "org.example.Viewer", Some(Behavior::Reply));
    host.click(Button::Viewer);
    host.assert_token_origin();
    host.wait_until("the activation call", |_| !viewer.received().is_empty());
    host.settle();
    assert_eq!(
        viewer.received(),
        [Received::Activate(activation_data("token-1"))]
    );
    assert_eq!(host.wayland.control().token_requests.len(), 1);
    assert!(host.runs().is_empty(), "{:?}", host.runs());
}

fn only_definite_non_delivery_falls_back() {
    let mut host = Host::start(None);
    let failing = Application::start(&host.bus, "org.example.Failing", Some(Behavior::Fail));
    host.click(Button::Missing);
    host.assert_token_origin();
    host.wait_until("the fallback launch", |host| !host.runs().is_empty());
    host.click(Button::Failing);
    host.wait_until("the refused call", |host| {
        host.app.activator.finished() == 2 && failing.received().len() == 1
    });
    host.settle();
    host.app.reap_children();
    assert_eq!(
        host.runs(),
        [host.expected_run("org.example.Missing.desktop", "token-1")]
    );
}

fn an_expired_token_wait_launches_once_without_a_token() {
    let mut host = Host::start(None);
    host.wayland.control().hold_tokens = true;
    host.click(Button::Plain);
    host.assert_token_origin();
    // the host is busy past the 250 ms deadline while the token arrives, so
    // expiry runs first and the late event finds no pending launch.
    thread::sleep(Duration::from_millis(300));
    host.wayland.control().release_tokens = true;
    let deadline = Instant::now() + WAIT;
    while host.wayland.control().released == 0 {
        assert!(Instant::now() < deadline, "the token was never sent");
        thread::sleep(Duration::from_millis(2));
    }
    host.wait_until("the expired launch", |host| !host.runs().is_empty());
    host.settle();
    assert_eq!(host.runs(), [host.expected_run("plain.desktop", "unset")]);
}

fn catalog_changes_decide_waiting_launches() {
    // the token wait outlasts the scanner's 250 ms quiet period.
    let mut host = Host::start(Some(Duration::from_secs(8)));
    host.wayland.control().hold_tokens = true;
    let viewer = Application::start(&host.bus, "org.example.Viewer", Some(Behavior::Reply));
    let revision = |host: &Host, id: &str| host.app.application_catalog.revision(id);

    // a rescan that leaves the entry alone keeps the waiting launch valid.
    let plain = revision(&host, "plain.desktop");
    host.click(Button::Plain);
    host.write_entry("unrelated.desktop", "unrelated");
    host.wait_until("the unrelated entry", |host| {
        revision(host, "unrelated.desktop").is_some()
    });
    assert_eq!(revision(&host, "plain.desktop"), plain);
    host.wayland.control().release_tokens = true;
    host.wait_until("the preserved launch", |host| !host.runs().is_empty());
    assert_eq!(host.runs(), [host.expected_run("plain.desktop", "token-1")]);

    // a replaced entry refuses the launch that waited for it.
    host.click(Button::Plain);
    host.write_entry("plain.desktop", "replacement");
    host.wait_until("the replaced entry", |host| {
        revision(host, "plain.desktop") != plain
    });
    host.wayland.control().release_tokens = true;
    host.settle();

    // so does a removed one, before any d-bus call.
    host.click(Button::Viewer);
    fs::remove_file(host.applications.join("org.example.Viewer.desktop")).unwrap();
    host.wait_until("the removed entry", |host| {
        revision(host, "org.example.Viewer.desktop").is_none()
    });
    host.wayland.control().release_tokens = true;
    host.settle();
    host.app.reap_children();
    assert_eq!(host.runs(), [host.expected_run("plain.desktop", "token-1")]);
    assert!(viewer.received().is_empty());
    assert_eq!(host.wayland.control().released, 3);
}

/// runs `probe` in a child test process, which bounds hangs and keeps signal
/// masks and launched children out of the parallel test runner. it returns the
/// child's standard error for message checks.
fn isolated(name: &str, probe: fn()) -> Option<String> {
    if std::env::var(PROBE_ENV).as_deref() == Ok(name) {
        probe();
        return None;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("host::launch_tests::{name}"),
            "--nocapture",
        ])
        .env(PROBE_ENV, name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "{name} timed out:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{name} failed:\n{stderr}");
    Some(stderr)
}

#[test]
fn a_click_delivers_its_activation_token_to_the_application_once() {
    isolated(
        "a_click_delivers_its_activation_token_to_the_application_once",
        token_reaches_the_application_once,
    );
}

#[test]
fn only_a_definite_dbus_failure_runs_the_exec_fallback() {
    if let Some(stderr) = isolated(
        "only_a_definite_dbus_failure_runs_the_exec_fallback",
        only_definite_non_delivery_falls_back,
    ) {
        assert!(
            stderr.contains(
                "activating \"org.example.Failing.desktop\" did not complete \
                 (org.freedesktop.DBus.Error.Failed: refused)"
            ),
            "{stderr}"
        );
    }
}

#[test]
fn an_expired_token_wait_still_launches_exactly_once() {
    isolated(
        "an_expired_token_wait_still_launches_exactly_once",
        an_expired_token_wait_launches_once_without_a_token,
    );
}

#[test]
fn catalog_publication_refuses_replaced_and_removed_waiting_launches() {
    if let Some(stderr) = isolated(
        "catalog_publication_refuses_replaced_and_removed_waiting_launches",
        catalog_changes_decide_waiting_launches,
    ) {
        for id in ["plain.desktop", "org.example.Viewer.desktop"] {
            assert!(
                stderr.contains(&format!(
                    "application {id:?} changed while its launch waited for activation"
                )),
                "{stderr}"
            );
        }
    }
}
