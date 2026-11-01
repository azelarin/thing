//! pointer cursors through the real panel host. a private compositor sends
//! pointer events and records, in one ordered log, every cursor request the
//! host makes beside markers for the events it sent.

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

use wayland_client::{
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
};
use wayland_protocols::{
    wp::cursor_shape::v1::server::{
        wp_cursor_shape_device_v1::{
            self as server_shape_device, Shape, WpCursorShapeDeviceV1 as ServerShapeDevice,
        },
        wp_cursor_shape_manager_v1::{
            self as server_shape_manager, WpCursorShapeManagerV1 as ServerShapeManager,
        },
    },
    xdg::shell::server::xdg_wm_base::XdgWmBase as ServerWmBase,
};
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

// eighty pixel slots: a button, a text field, a crosshair row, and the bare
// panel, whose own cursor reacts to `clock.date`.
const PANEL: &str = r#"
local slot = { width = 80, height = 80 }
return ui.panel {
    style = { width = 640, height = 80, align = 'start', justify = 'start' },
    cursor = function()
        local date = services.clock.date:get()
        if date == 'grab' then return 'grab' end
        if date == 'bad' then return 'bogus' end
        return nil
    end,
    children = {
        ui.button { style = slot, children = { ui.text { text = 'b' } } },
        ui.text_input { style = slot, value = '', on_change = function() end },
        ui.row { style = slot, cursor = 'crosshair' },
    },
}
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Record {
    ShapeDevice,
    DestroyShapeDevice,
    Shape {
        serial: u32,
        shape: Shape,
    },
    SetCursor {
        serial: u32,
        surface: bool,
    },
    ReleasePointer,
    /// the server sent an event; its serial when it has one.
    Sent(&'static str, u32),
}

enum Command {
    Enter(f64, f64),
    Motion(f64, f64),
    Button(bool),
    Leave,
    Capabilities(server_seat::Capability),
    RemoveSeat,
    AddSeat,
}

#[derive(Default)]
struct Control {
    commands: Vec<Command>,
    records: Vec<Record>,
}

struct ServerState {
    control: Arc<Mutex<Control>>,
    serial: u32,
    seat: Option<GlobalId>,
    seats: Vec<ServerSeat>,
    pointer: Option<ServerPointer>,
    panel: Option<ServerSurface>,
    layer: Option<(ServerLayer, bool)>,
    pending_buffers: HashMap<ObjectId, ServerBuffer>,
    frame_callbacks: HashMap<ObjectId, Vec<ServerCallback>>,
}

impl ServerState {
    fn record(&self, record: Record) {
        self.control.lock().unwrap().records.push(record);
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    fn apply(&mut self, display: &mut DisplayHandle) {
        let commands = std::mem::take(&mut self.control.lock().unwrap().commands);
        for command in commands {
            match command {
                Command::RemoveSeat => {
                    if let Some(seat) = self.seat.take() {
                        display.remove_global::<ServerState>(seat);
                    }
                    self.seats.clear();
                    self.pointer = None;
                }
                Command::AddSeat => {
                    self.seat = Some(display.create_global::<ServerState, ServerSeat, _>(8, ()));
                }
                Command::Capabilities(capabilities) => {
                    for seat in &self.seats {
                        seat.capabilities(capabilities);
                    }
                    if capabilities.is_empty() {
                        self.pointer = None;
                    }
                }
                command => {
                    let (Some(pointer), Some(panel)) = (self.pointer.clone(), self.panel.clone())
                    else {
                        continue;
                    };
                    match command {
                        Command::Enter(x, y) => {
                            let serial = self.next_serial();
                            pointer.enter(serial, &panel, x, y);
                            self.record(Record::Sent("enter", serial));
                        }
                        Command::Motion(x, y) => {
                            pointer.motion(1, x, y);
                            self.record(Record::Sent("motion", 0));
                        }
                        Command::Button(pressed) => {
                            let serial = self.next_serial();
                            let state = if pressed {
                                server_pointer::ButtonState::Pressed
                            } else {
                                server_pointer::ButtonState::Released
                            };
                            pointer.button(serial, 1, 0x110, state);
                            self.record(Record::Sent(
                                if pressed { "press" } else { "release" },
                                serial,
                            ));
                        }
                        Command::Leave => {
                            let serial = self.next_serial();
                            pointer.leave(serial, &panel);
                            self.record(Record::Sent("leave", serial));
                        }
                        _ => unreachable!("handled above"),
                    }
                    pointer.frame();
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
    ServerShapeManager
);

impl ServerDispatch<ServerShapeManager, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerShapeManager,
        request: server_shape_manager::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_shape_manager::Request::GetPointer {
            cursor_shape_device,
            ..
        } = request
        {
            data.init(cursor_shape_device, ());
            state.record(Record::ShapeDevice);
        }
    }
}

impl ServerDispatch<ServerShapeDevice, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerShapeDevice,
        request: server_shape_device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_shape_device::Request::SetShape {
                serial,
                shape: wayland_server::WEnum::Value(shape),
            } => state.record(Record::Shape { serial, shape }),
            server_shape_device::Request::Destroy => state.record(Record::DestroyShapeDevice),
            _ => {}
        }
    }
}

impl ServerDispatch<ServerPointer, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerPointer,
        request: server_pointer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_pointer::Request::SetCursor {
                serial, surface, ..
            } => state.record(Record::SetCursor {
                serial,
                surface: surface.is_some(),
            }),
            server_pointer::Request::Release => state.record(Record::ReleasePointer),
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
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerSeat>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        let seat = data.init(resource, ());
        seat.capabilities(server_seat::Capability::Pointer);
        seat.name("seat0".into());
        state.seats.push(seat);
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
            state.layer = Some((data.init(id, ()), false));
            state.panel = Some(surface);
        }
    }
}

struct PrivateWayland {
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl PrivateWayland {
    fn start(shape_protocol: bool) -> (Self, Connection) {
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
        if shape_protocol {
            handle.create_global::<ServerState, ServerShapeManager, _>(1, ());
        }
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_control = control.clone();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState {
                control: server_control,
                serial: 0,
                seat: Some(seat),
                seats: Vec::new(),
                pointer: None,
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
    fn start(shape_protocol: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let panel = directory.path().join("panel.luau");
        fs::write(&panel, PANEL).unwrap();
        let (wayland, connection) = PrivateWayland::start(shape_protocol);
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
        };
        queue.roundtrip(&mut app).unwrap();
        app.initialize(&qh).unwrap();
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
        host.until(|host| {
            host.app.pointer.is_some()
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

    fn records(&self) -> Vec<Record> {
        self.wayland.control.lock().unwrap().records.clone()
    }

    fn shapes(&self) -> Vec<Shape> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Shape { shape, .. } => Some(shape),
                _ => None,
            })
            .collect()
    }

    fn sent(&self, event: &str) -> u32 {
        self.records()
            .into_iter()
            .rev()
            .find_map(|record| match record {
                Record::Sent(name, serial) if name == event => Some(serial),
                _ => None,
            })
            .unwrap()
    }

    /// waits until the cursor requests end with `shapes`.
    fn shows(&mut self, shapes: &[Shape]) {
        self.until(|host| host.shapes().ends_with(shapes));
    }

    fn set_date(&mut self, value: &str) {
        self.app
            .configurations
            .set_service("clock.date", Scalar::String(value.into()))
            .unwrap();
        self.app.refresh();
    }

    fn reload(&mut self, source: &str) {
        fs::write(self.directory.path().join("panel.luau"), source).unwrap();
        let qh = self.qh.clone();
        self.app.reload(&qh).unwrap();
    }
}

#[test]
fn shapes_follow_the_widget_under_the_pointer_and_every_enter_sends_again() {
    let mut host = Host::start(true);
    host.until(|host| host.records().contains(&Record::ShapeDevice));
    // text inside a button shows the button's cursor.
    host.command(Command::Enter(40.0, 40.0));
    host.shows(&[Shape::Pointer]);
    let enter = host.sent("enter");
    assert!(host.records().contains(&Record::Shape {
        serial: enter,
        shape: Shape::Pointer
    }));

    host.command(Command::Motion(120.0, 40.0));
    host.shows(&[Shape::Pointer, Shape::Text]);
    // moving within the field sends nothing; the crosshair after it is the
    // ordered witness.
    host.command(Command::Motion(130.0, 40.0));
    host.command(Command::Motion(200.0, 40.0));
    host.shows(&[Shape::Pointer, Shape::Text, Shape::Crosshair]);
    // the bare panel shows the default.
    host.command(Command::Motion(300.0, 40.0));
    host.shows(&[Shape::Text, Shape::Crosshair, Shape::Default]);

    // after leaving, the same shape is sent again with the new enter serial.
    host.command(Command::Leave);
    host.command(Command::Enter(300.0, 40.0));
    host.until(|host| {
        let latest = host.sent("enter");
        latest != enter
            && host.records().contains(&Record::Shape {
                serial: latest,
                shape: Shape::Default,
            })
    });
    // the shape protocol replaces wl_pointer.set_cursor entirely.
    assert!(
        !host
            .records()
            .iter()
            .any(|record| matches!(record, Record::SetCursor { .. }))
    );
}

#[test]
fn a_press_holds_its_cursor_until_release() {
    let mut host = Host::start(true);
    host.command(Command::Enter(40.0, 40.0));
    host.shows(&[Shape::Pointer]);
    host.command(Command::Button(true));
    host.command(Command::Motion(120.0, 40.0));
    // the host has handled the motion over the field while the button is
    // still down; the cursor decision runs in the same pointer frame, and a
    // roundtrip lets the server record anything it sent.
    host.until(|host| {
        let surface = &host.app.surfaces[0];
        surface.pressed_button.is_some()
            && surface
                .pointer_position
                .is_some_and(|point| point.x == 120.0)
    });
    host.connection.roundtrip().unwrap();
    assert_eq!(host.shapes(), [Shape::Pointer]);
    // releasing shows what is under the pointer now.
    host.command(Command::Button(false));
    host.shows(&[Shape::Pointer, Shape::Text]);
}

#[test]
fn reactive_and_reloaded_cursors_publish_only_accepted_state() {
    let mut host = Host::start(true);
    host.command(Command::Enter(300.0, 40.0));
    host.shows(&[Shape::Default]);

    // an unknown name rejects the update, so nothing is shown from it.
    host.set_date("bad");
    host.set_date("grab");
    host.shows(&[Shape::Default, Shape::Grab]);
    assert_eq!(host.shapes(), [Shape::Default, Shape::Grab]);

    // a rejected reload keeps the accepted configuration's cursor.
    host.reload(
        &PANEL
            .replace("return nil", "return 'zoom_out'")
            .replace("'crosshair'", "'sideways'"),
    );
    host.set_date("");
    host.shows(&[Shape::Default, Shape::Grab, Shape::Default]);
    // an accepted one changes it.
    host.reload(&PANEL.replace("return nil", "return 'zoom_out'"));
    host.command(Command::Motion(301.0, 40.0));
    host.shows(&[Shape::Grab, Shape::Default, Shape::ZoomOut]);
}

#[test]
fn seat_teardown_releases_the_cursor_and_a_new_pointer_starts_fresh() {
    let mut host = Host::start(true);
    host.command(Command::Enter(40.0, 40.0));
    host.shows(&[Shape::Pointer]);

    // losing the pointer capability releases the device and the pointer.
    host.command(Command::Capabilities(server_seat::Capability::empty()));
    host.until(|host| {
        host.records().contains(&Record::DestroyShapeDevice)
            && host.records().contains(&Record::ReleasePointer)
    });
    assert!(host.app.pointer.is_none());
    host.command(Command::Capabilities(server_seat::Capability::Pointer));
    host.until(|host| host.app.pointer.is_some());
    host.command(Command::Enter(120.0, 40.0));
    host.shows(&[Shape::Pointer, Shape::Text]);

    // a replaced seat starts over with a new device and enter serial.
    host.command(Command::RemoveSeat);
    host.until(|host| host.app.pointer.is_none());
    host.command(Command::AddSeat);
    host.until(|host| host.app.pointer.is_some());
    host.command(Command::Enter(120.0, 40.0));
    host.until(|host| {
        let enter = host.sent("enter");
        host.records().contains(&Record::Shape {
            serial: enter,
            shape: Shape::Text,
        }) && host.shapes().len() == 3
    });
    let devices = host
        .records()
        .iter()
        .filter(|record| **record == Record::ShapeDevice)
        .count();
    assert_eq!(devices, 3);
}

#[test]
fn without_the_shape_protocol_the_host_falls_back_to_the_theme() {
    let mut host = Host::start(false);
    host.command(Command::Enter(40.0, 40.0));
    host.until(|host| {
        host.app
            .surfaces
            .first()
            .is_some_and(|surface| surface.pointer_position.is_some())
    });
    // a roundtrip lets the server record every request the enter caused.
    host.connection.roundtrip().unwrap();
    let records = host.records();
    assert!(!records.contains(&Record::ShapeDevice));
    // whether a theme is installed depends on the machine; a cursor that is
    // sent always carries an image, and the panel keeps working either way.
    assert!(
        records
            .iter()
            .all(|record| !matches!(record, Record::SetCursor { surface: false, .. }))
    );
    host.command(Command::Motion(120.0, 40.0));
    host.until(|host| {
        host.app.surfaces[0]
            .pointer_position
            .is_some_and(|point| point.x == 120.0)
    });
}
