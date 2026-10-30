//! wheel routing through the real panel host: axis events from a private
//! wayland server reach `pointer_frame`, which captures the route from the
//! presented tree and offers the event to each stop.

use std::{
    collections::HashMap,
    fs::{self, File},
    os::{fd::AsFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
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

const WIDTH: u32 = 640;
const HEIGHT: u32 = 80;
const WAIT: Duration = Duration::from_secs(10);
// long enough for the private server and host to finish one wheel event.
const SETTLE: Duration = Duration::from_millis(150);

// eight 80 pixel regions. `note` records each handler call with its deltas and
// local coordinates.
const PANEL: &str = r#"
local log = ui.state('')
local replaced = ui.state(false)
local inhibit = ui.state(false)
local swapped = ui.state(false)
local markers = ...
local function note(text) log:set(log:get() .. text .. ';') end
local function passes(name)
    return function(x, y, event)
        note(`{name} {x} {y} {event.local_x} {event.local_y} {event.source}`)
        return false
    end
end
local function touch(path) services.launch({ '/bin/sh', '-c', 'touch "$0"', path }) end
local slot = { width = 80, height = 80 }
return ui.panel {
    style = { width = 640, height = 80 },
    -- the private compositor offers no idle inhibitor, so enabling this fails
    -- the host's surface validation.
    idle_inhibit = function() return inhibit:get() end,
    on_scroll = function(x, y, event)
        note(`panel {x} {y} {event.local_x} {event.local_y}`)
        return true
    end,
    children = { ui.row { children = function()
        return {
            ui.column { style = slot, on_scroll = passes('parent'), children = {
                ui.button {
                    style = { width = 80, height = 40 },
                    on_scroll = function(_, _, event)
                        touch(markers.consumed)
                        note(`child {event.local_x} {event.local_y}`)
                    end,
                },
            } },
            ui.column { style = slot, on_scroll = passes('background') },
            ui.column {
                style = slot,
                on_scroll = function() note('outer') return true end,
                children = { ui.scroll { key = 'viewport', style = slot, children = {
                    ui.column { style = { width = 80, height = 160 } },
                } } },
            },
            if replaced:get()
                then ui.stack { key = 'replacement', style = slot, on_scroll = passes('replacement') }
                else ui.stack { key = 'original', style = slot, on_scroll = passes('original'), children = {
                    ui.pointer_area {
                        style = slot,
                        on_scroll = function()
                            replaced:set(true)
                            note('replace')
                            return false
                        end,
                    },
                } },
            ui.stack { style = slot, children = {
                ui.pointer_area { style = slot, on_scroll = passes('covered') },
                ui.row { style = slot },
            } },
            ui.column { style = slot, on_scroll = passes('invalid parent'), children = {
                ui.pointer_area {
                    style = slot,
                    on_scroll = function()
                        touch(markers.invalid)
                        return 'invalid'
                    end,
                },
            } },
            ui.column { style = slot, on_scroll = passes('rejected parent'), children = {
                ui.pointer_area {
                    style = slot,
                    on_scroll = function()
                        touch(markers.rejected)
                        inhibit:set(true)
                        return false
                    end,
                },
            } },
            ui.row { style = slot, children = function()
                local half = { width = 40, height = 80 }
                local function side(key)
                    return ui.column { key = key, style = half, on_scroll = passes(key), children = {
                        ui.pointer_area {
                            style = half,
                            on_scroll = function()
                                swapped:set(not swapped:get())
                                note('swap')
                                return false
                            end,
                        },
                    } }
                end
                if swapped:get() then
                    return { side('right'), side('left') }
                end
                return { side('left'), side('right') }
            end },
            ui.text { style = { width = 0, height = 0 }, text = function() return log:get() end },
        }
    end } },
}
"#;

#[derive(Default)]
struct Control {
    axes: Vec<(f64, f64, i32, i32)>,
}

struct ServerState {
    control: Arc<Mutex<Control>>,
    pointer: Option<ServerPointer>,
    entered: bool,
    panel: Option<ServerSurface>,
    layer: Option<(ServerLayer, bool)>,
    pending_buffers: HashMap<ObjectId, ServerBuffer>,
    frame_callbacks: HashMap<ObjectId, Vec<ServerCallback>>,
    serial: u32,
}

impl ServerState {
    fn send_axes(&mut self) {
        let axes = std::mem::take(&mut self.control.lock().unwrap().axes);
        for (x, y, horizontal, vertical) in axes {
            let pointer = self.pointer.clone().expect("the host has a pointer");
            if !self.entered {
                self.serial += 1;
                let panel = self.panel.clone().expect("the panel exists");
                pointer.enter(self.serial, &panel, x, y);
                self.entered = true;
            } else {
                pointer.motion(1, x, y);
            }
            pointer.frame();
            pointer.axis_source(server_pointer::AxisSource::Wheel);
            for (axis, value120) in [
                (server_pointer::Axis::HorizontalScroll, horizontal),
                (server_pointer::Axis::VerticalScroll, vertical),
            ] {
                if value120 != 0 {
                    pointer.axis_value120(axis, value120);
                    pointer.axis(2, axis, f64::from(value120) / 8.0);
                }
            }
            pointer.frame();
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

plain_globals!(ServerCompositor, ServerLayerShell, ServerWmBase);

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

/// an in-process compositor serving one panel and one pointer.
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
        // version 8 carries exact fractional wheel steps as `axis_value120`.
        handle.create_global::<ServerState, ServerSeat, _>(8, ());
        handle.create_global::<ServerState, ServerLayerShell, _>(4, ());
        handle.create_global::<ServerState, ServerWmBase, _>(1, ());
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_control = control.clone();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState {
                control: server_control,
                pointer: None,
                entered: false,
                panel: None,
                layer: None,
                pending_buffers: HashMap::new(),
                frame_callbacks: HashMap::new(),
                serial: 0,
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
                state.send_axes();
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

/// a panel host with inert desktop services. fields drop in order, so the
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
    fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let marker = |name: &str| directory.path().join(name).display().to_string();
        let panel = directory.path().join("panel.luau");
        fs::write(
            &panel,
            PANEL.replace(
                "local markers = ...",
                &format!(
                    "local markers = {{ consumed = {:?}, invalid = {:?}, rejected = {:?} }}",
                    marker("consumed"),
                    marker("invalid"),
                    marker("rejected"),
                ),
            ),
        )
        .unwrap();
        let (wayland, connection) = PrivateWayland::start();
        let (globals, mut queue) = registry_queue_init(&connection).unwrap();
        let qh = queue.handle();
        let buffer = shared_buffer(&globals, &qh);
        let event_loop = EventLoop::<'static, App>::try_new().unwrap();
        let (image_ping, _) = calloop::ping::make_ping().unwrap();
        let (clipboard_ping, _) = calloop::ping::make_ping().unwrap();
        let (activator_ping, _) = calloop::ping::make_ping().unwrap();
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
            // even an accidental activation must stay off the session bus.
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
            input_method: InputMethod::bind(&globals, &qh),
            launcher: Launcher::for_tests(),
            managed_processes: None,
            pending_launches: PendingLaunches::new(),
            pending_ui_actions: Vec::new(),
            watch,
            reload_at: None,
            hover: HoverStates::default(),
            surfaces: Vec::new(),
            closed_surfaces: Vec::new(),
            registry: RegistryState::new(&globals),
            idle_inhibit: IdleInhibitState::default(),
            idle_notify: crate::idle_notify::State::default(),
            seats: SeatState::new(&globals, &qh),
            outputs: OutputState::new(&globals, &qh),
            compositor: CompositorState::bind(&globals, &qh).unwrap(),
            layer_shell: LayerShell::bind(&globals, &qh).unwrap(),
            layer_shell_version: 4,
            xdg_shell: XdgShell::bind(&globals, &qh).unwrap(),
            activation: None,
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
        queue.roundtrip(&mut app).unwrap();
        app.initialize(&qh).unwrap();
        WaylandSource::new(connection.clone(), queue)
            .insert(event_loop.handle())
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
        let deadline = Instant::now() + WAIT;
        while host.app.pointer.is_none()
            || host
                .app
                .surfaces
                .first()
                .is_none_or(|surface| surface.layouts.presented().is_none())
        {
            assert!(Instant::now() < deadline, "the panel was never presented");
            host.turn();
        }
        host
    }

    fn turn(&mut self) {
        self.app
            .turn(&mut self.event_loop, &self.connection, &self.qh)
            .unwrap();
    }

    /// sends one wheel frame at a surface point. deltas are in 120ths of a
    /// wheel step, as `axis_value120` carries them.
    fn wheel(&mut self, x: f64, y: f64, horizontal: i32, vertical: i32) {
        self.wayland
            .control
            .lock()
            .unwrap()
            .axes
            .push((x, y, horizontal, vertical));
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            self.turn();
        }
    }

    /// returns and clears the handler log.
    fn log(&self) -> String {
        fn text(node: &thing_core::Node) -> Option<&str> {
            if node.kind == Kind::Text {
                return Some(&node.text);
            }
            node.children.iter().find_map(text)
        }
        let output = &self.app.surfaces[0].output;
        text(self.app.configurations.root(output).unwrap())
            .unwrap()
            .to_owned()
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn viewport_offset(&self) -> Option<f32> {
        let surface = &self.app.surfaces[0];
        let root = self
            .app
            .configurations
            .surface_root(&surface.output, &surface.key)?;
        fn find(node: &thing_core::Node) -> Option<NodeId> {
            if node.kind == Kind::Scroll {
                return Some(node.id);
            }
            node.children.iter().find_map(find)
        }
        let id = find(root)?;
        surface
            .layouts
            .presented()?
            .semantic_placement(id)?
            .scroll_offset
    }
}

fn wait_for_file(path: &Path) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn wheel_events_follow_one_presented_route_from_child_to_ancestors() {
    let mut host = Host::start();
    let mut seen = String::new();
    let mut expect = |host: &Host, added: &str| {
        seen.push_str(added);
        assert_eq!(host.log(), seen);
    };

    // a consuming child stops the route before its parent and the panel.
    host.wheel(10.0, 20.0, 0, 120);
    expect(&host, "child 10 20;");
    assert!(wait_for_file(&host.marker("consumed")));

    // a container background gets horizontal and fractional input in its own
    // coordinates, passes it on, and the panel receives it once in its own.
    host.wheel(100.0, 30.0, -60, 30);
    expect(
        &host,
        "background -0.5 0.25 20 30 wheel;panel -0.5 0.25 100 30;",
    );

    // the inner viewport moves before its ancestor's handler and passes the
    // event outward only at its limits.
    host.wheel(200.0, 40.0, 0, -120);
    expect(&host, "outer;");
    for offset in [40.0, 80.0] {
        host.wheel(200.0, 40.0, 0, 120);
        expect(&host, "");
        assert_eq!(host.viewport_offset(), Some(offset));
    }
    host.wheel(200.0, 40.0, 0, 120);
    expect(&host, "outer;");

    // a handler that replaces its ancestor ends the route; the replacement
    // receives only later events.
    host.wheel(280.0, 40.0, 0, 120);
    expect(&host, "replace;");
    host.wheel(280.0, 40.0, 0, 120);
    expect(&host, "replacement 0 1 40 40 wheel;panel 0 1 280 40;");

    // an overlapped background handler is not on the foreground's route.
    host.wheel(360.0, 40.0, 0, 120);
    expect(&host, "panel 0 1 360 40;");

    // reordering keyed siblings does not hand the event to the widget that
    // moved into the original's place.
    host.wheel(580.0, 40.0, 0, 120);
    expect(&host, "swap;left 0 1 20 40 wheel;panel 0 1 580 40;");

    // an invalid result stops the route and discards the callback's launch.
    host.wheel(440.0, 40.0, 0, 120);
    expect(&host, "");

    // so does a callback whose layout fails validation.
    host.wheel(520.0, 40.0, 0, 120);
    expect(&host, "");
    host.app.reap_children();
    assert!(!host.marker("invalid").exists());
    assert!(!host.marker("rejected").exists());
}
