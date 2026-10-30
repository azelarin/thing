//! idle watches and sleep inhibitors through the real panel host. a private
//! compositor offers ext-idle-notify, records each notification the host
//! arms, and sends idle and resume events the configurations see through
//! checked dispatch. sleep inhibitors reach a fake logind on a private bus.

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

use thing_core::Kind;
use wayland_client::{
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
};
use wayland_protocols::{
    ext::idle_notify::v1::server::{
        ext_idle_notification_v1::ExtIdleNotificationV1 as ServerNotification,
        ext_idle_notifier_v1::{self as server_notifier, ExtIdleNotifierV1 as ServerNotifier},
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
        wl_pointer::WlPointer as ServerPointer,
        wl_region::WlRegion as ServerRegion,
        wl_seat::{self as server_seat, WlSeat as ServerSeat},
        wl_shm::{self as server_shm, WlShm as ServerShm},
        wl_shm_pool::{self as server_shm_pool, WlShmPool as ServerShmPool},
        wl_surface::{self as server_surface, WlSurface as ServerSurface},
    },
};

use thing_runtime::SleepInhibitorStatus;

use super::*;
use crate::{
    applications::test_bus::Bus,
    sleep::tests::{Behavior as LogindBehavior, Logind},
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 80;
const WAIT: Duration = Duration::from_secs(10);

// `note` appends to a log each output shows in its own text node, followed by
// the protocol availability and the first watch's state. the broken watch
// starts `guard` before failing, so a published action would be visible in
// the host's lease model at once.
const PANEL: &str = r#"
local log = ui.state('')
local function note(text) log:set(log:get() .. text .. ';') end
local guard = services.sleep_inhibitor { key = 'guard', reason = 'must stay stopped' }
local away = services.idle.watch {
    timeout = TIMEOUT,
    on_idle = function(watch) note('idle ' .. tostring(watch.idle:get())) end,
    on_resume = function(watch) note('resume ' .. tostring(watch.idle:get())) end,
}
services.idle.watch {
    timeout = 60000,
    on_idle = function()
        guard.start()
        error('broken watch')
    end,
}
return ui.panel {
    style = { width = 640, height = 80 },
    children = { ui.text {
        style = { width = 0, height = 0 },
        text = function()
            return log:get() .. '|' .. tostring(services.idle.watch_available:get())
                .. '|' .. tostring(away.idle:get())
        end,
    } },
}
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Record {
    Arm {
        notification: u32,
        timeout: u32,
        seat: u32,
    },
    Disarm(u32),
}

enum Command {
    AddNotifier,
    RemoveNotifier,
    AddSeat,
    RemoveSeat,
    RemoveOutput(usize),
    Send { notification: u32, idle: bool },
}

#[derive(Default)]
struct Control {
    commands: Vec<Command>,
    records: Vec<Record>,
}

struct ServerState {
    control: Arc<Mutex<Control>>,
    notifier: Option<GlobalId>,
    seat: Option<GlobalId>,
    outputs: Vec<GlobalId>,
    notifications: HashMap<u32, ServerNotification>,
    layers: Vec<(ServerSurface, ServerLayer, bool)>,
    pending_buffers: HashMap<ObjectId, ServerBuffer>,
    frame_callbacks: HashMap<ObjectId, Vec<ServerCallback>>,
}

impl ServerState {
    fn apply(&mut self, display: &mut DisplayHandle) {
        let commands = std::mem::take(&mut self.control.lock().unwrap().commands);
        for command in commands {
            match command {
                Command::AddNotifier => {
                    self.notifier =
                        Some(display.create_global::<ServerState, ServerNotifier, _>(2, ()));
                }
                Command::RemoveNotifier => {
                    display.remove_global::<ServerState>(self.notifier.take().unwrap());
                }
                Command::AddSeat => {
                    self.seat = Some(display.create_global::<ServerState, ServerSeat, _>(8, ()));
                }
                Command::RemoveSeat => {
                    display.remove_global::<ServerState>(self.seat.take().unwrap());
                }
                Command::RemoveOutput(index) => {
                    display.remove_global::<ServerState>(self.outputs[index].clone());
                }
                Command::Send { notification, idle } => {
                    let notification = &self.notifications[&notification];
                    if idle {
                        notification.idled();
                    } else {
                        notification.resumed();
                    }
                }
            }
        }
    }

    fn record(&self, record: Record) {
        self.control.lock().unwrap().records.push(record);
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
    ServerOutput => usize,
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
    ServerNotifier
);

impl ServerDispatch<ServerNotifier, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerNotifier,
        request: server_notifier::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_notifier::Request::GetIdleNotification { id, timeout, seat } = request {
            let notification = data.init(id, ());
            let id = notification.id().protocol_id();
            state.record(Record::Arm {
                notification: id,
                timeout,
                seat: seat.id().protocol_id(),
            });
            state.notifications.insert(id, notification);
        }
    }
}

impl ServerDispatch<ServerNotification, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        notification: &ServerNotification,
        _: <ServerNotification as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let id = notification.id().protocol_id();
        state.notifications.remove(&id);
        state.record(Record::Disarm(id));
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
                if let Some((_, layer, configured)) = state
                    .layers
                    .iter_mut()
                    .find(|(panel, _, _)| panel == surface)
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

impl GlobalDispatch<ServerOutput, usize> for ServerState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerOutput>,
        index: &usize,
        data: &mut DataInit<'_, Self>,
    ) {
        let output = data.init(resource, *index);
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
        output.name(format!("PRIVATE-{}", index + 1));
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
        _: &mut Self,
        _: &Client,
        _: &ServerSeat,
        request: server_seat::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let server_seat::Request::GetPointer { id } = request {
            data.init(id, ());
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
            state.layers.push((surface, data.init(id, ()), false));
        }
    }
}

/// an in-process compositor with one panel per output.
struct PrivateWayland {
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl PrivateWayland {
    fn start(outputs: usize, notifier: bool) -> (Self, Connection) {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        server_socket.set_nonblocking(true).unwrap();
        let mut display = Display::<ServerState>::new().unwrap();
        let mut handle = display.handle();
        let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
        handle.create_global::<ServerState, ServerCompositor, _>(6, ());
        handle.create_global::<ServerState, ServerShm, _>(1, ());
        let outputs = (0..outputs)
            .map(|index| handle.create_global::<ServerState, ServerOutput, _>(4, index))
            .collect();
        let seat = handle.create_global::<ServerState, ServerSeat, _>(8, ());
        handle.create_global::<ServerState, ServerLayerShell, _>(4, ());
        handle.create_global::<ServerState, ServerWmBase, _>(1, ());
        let notifier =
            notifier.then(|| handle.create_global::<ServerState, ServerNotifier, _>(2, ()));
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_control = control.clone();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState {
                control: server_control,
                notifier,
                seat: Some(seat),
                outputs,
                notifications: HashMap::new(),
                layers: Vec::new(),
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
    fn start(outputs: usize, notifier: bool) -> Self {
        Self::start_with(outputs, notifier, |_| source(5000), None)
    }

    /// a host whose configuration is `source`. with a bus, its sleep
    /// inhibitors reach that bus instead of an unreachable address.
    fn start_with(
        outputs: usize,
        notifier: bool,
        source: impl FnOnce(&tempfile::TempDir) -> String,
        bus: Option<&Bus>,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let panel = directory.path().join("panel.luau");
        fs::write(&panel, source(&directory)).unwrap();
        let (wayland, connection) = PrivateWayland::start(outputs, notifier);
        let (globals, mut queue) = registry_queue_init(&connection).unwrap();
        let qh = queue.handle();
        let buffer = shared_buffer(&globals, &qh);
        let event_loop = EventLoop::<'static, App>::try_new().unwrap();
        let (image_ping, _) = calloop::ping::make_ping().unwrap();
        let (clipboard_ping, _) = calloop::ping::make_ping().unwrap();
        let (activator_ping, _) = calloop::ping::make_ping().unwrap();
        let (sleep_ping, sleep_source) = calloop::ping::make_ping().unwrap();
        let watch = Rc::new(Watch::new().unwrap());
        watch.set_files(std::slice::from_ref(&panel)).unwrap();
        let mut idle_notify = crate::idle_notify::State::default();
        let registry = RegistryState::new(&globals);
        idle_notify.bind_next(&registry, &qh).unwrap();
        let mut services = ServiceSnapshot::default();
        services
            .set_service(
                "idle.watch_available",
                Scalar::Boolean(idle_notify.available()),
            )
            .unwrap();
        let mut app = App {
            render: Render::new_test(buffer.buffer.clone(), (WIDTH, HEIGHT)),
            image_ping,
            configurations: Configurations::new(panel, services),
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
            sleep: match bus {
                Some(bus) => crate::sleep::SleepInhibition::new(
                    crate::sleep::Transport::Address(bus.address.clone()),
                    sleep_ping,
                ),
                None => crate::sleep::SleepInhibition::inert(),
            },
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
            registry,
            idle_inhibit: IdleInhibitState::default(),
            idle_notify,
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
        event_loop
            .handle()
            .insert_source(sleep_source, |_, _, app| app.update_sleep())
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
            host.app.seat.is_some()
                && host.app.surfaces.len() == outputs
                && host
                    .app
                    .surfaces
                    .iter()
                    .all(|surface| surface.layouts.presented().is_some())
        });
        host
    }

    fn turn(&mut self) {
        self.app
            .turn(&mut self.event_loop, &self.connection, &self.qh)
            .unwrap();
    }

    /// turns until `done` holds. assertions wait for a positive condition;
    /// a stale event is followed by an ordered event the test waits for.
    fn until(&mut self, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "the host did not reach the expected state"
            );
            self.turn();
        }
    }

    fn command(&mut self, command: Command) {
        self.wayland.control.lock().unwrap().commands.push(command);
    }

    fn send(&mut self, notification: u32, idle: bool) {
        self.command(Command::Send { notification, idle });
    }

    fn records(&self) -> Vec<Record> {
        self.wayland.control.lock().unwrap().records.clone()
    }

    /// the notifications armed so far, in order, as (id, timeout).
    fn armed(&self) -> Vec<(u32, u32)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Arm {
                    notification,
                    timeout,
                    ..
                } => Some((notification, timeout)),
                Record::Disarm(_) => None,
            })
            .collect()
    }

    fn seats(&self) -> Vec<u32> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Arm { seat, .. } => Some(seat),
                Record::Disarm(_) => None,
            })
            .collect()
    }

    fn disarmed(&self) -> Vec<u32> {
        let mut disarmed: Vec<_> = self
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Disarm(notification) => Some(notification),
                Record::Arm { .. } => None,
            })
            .collect();
        disarmed.sort_unstable();
        disarmed
    }

    /// the configuration key of output `index`.
    fn output(&self, index: usize) -> wl_output::WlOutput {
        self.app
            .configurations
            .keys()
            .find(|output| {
                self.app
                    .configurations
                    .output(output)
                    .and_then(|context| context.name.as_deref())
                    == Some(&format!("PRIVATE-{}", index + 1))
            })
            .unwrap()
            .clone()
    }

    /// the notification of `watch` on output `index`. the host arms each
    /// output's watches in declaration order, outputs in configuration order.
    fn notification(&self, index: usize, watch: usize) -> u32 {
        let output = self.output(index);
        let position = self
            .app
            .configurations
            .keys()
            .position(|key| *key == output)
            .unwrap();
        self.armed()[position * 2 + watch].0
    }

    /// the text of output `index`'s panel.
    fn log(&self, index: usize) -> String {
        fn text(node: &thing_core::Node) -> Option<&str> {
            if node.kind == Kind::Text {
                return Some(&node.text);
            }
            node.children.iter().find_map(text)
        }
        text(self.app.configurations.root(&self.output(index)).unwrap())
            .unwrap()
            .to_owned()
    }

    fn sleep_status(&self, key: &str) -> Option<SleepInhibitorStatus> {
        self.app.sleep.status(key).map(|(status, _)| status)
    }

    fn write(&self, source: &str) {
        fs::write(self.directory.path().join("panel.luau"), source).unwrap();
    }

    /// reloads synchronously; the new configurations are current on return.
    fn reload(&mut self) {
        let qh = self.qh.clone();
        self.app.reload(&qh).unwrap();
    }
}

fn source(timeout: u32) -> String {
    PANEL.replace("TIMEOUT", &timeout.to_string())
}

fn sorted(mut values: Vec<u32>) -> Vec<u32> {
    values.sort_unstable();
    values
}

#[test]
fn each_output_arms_its_own_watches_and_runs_callbacks_through_checked_events() {
    let mut host = Host::start(2, true);
    // two watches on each output, all on the host's seat, nothing shared.
    host.until(|host| host.armed().len() == 4);
    let timeouts = host.armed().iter().map(|(_, timeout)| *timeout).collect();
    assert_eq!(sorted(timeouts), [5000, 5000, 60000, 60000]);
    let seats = host.seats();
    assert!(seats.iter().all(|seat| *seat == seats[0]));
    assert_eq!(host.log(0), "|true|false");
    assert_eq!(host.log(1), "|true|false");

    let (first, second) = (host.notification(0, 0), host.notification(1, 0));
    host.send(first, true);
    host.until(|host| host.log(0) == "idle true;|true|true");
    assert_eq!(host.log(1), "|true|false");
    // a resume before any idle is stale; the idle after it is the first
    // transition output 2 sees.
    host.send(second, false);
    host.send(second, true);
    host.until(|host| host.log(1) == "idle true;|true|true");
    host.send(first, false);
    host.until(|host| host.log(0) == "idle true;resume false;|true|false");

    // a failing callback publishes none of its actions. the next event is
    // ordered after it, so the broken callback has run once this one has.
    host.send(host.notification(0, 1), true);
    host.send(first, true);
    host.until(|host| host.log(0) == "idle true;resume false;idle true;|true|true");
    assert_eq!(
        host.sleep_status("guard"),
        Some(SleepInhibitorStatus::Stopped)
    );
    assert!(!host.app.sleep.started());

    // removing an output retires its watches without running callbacks.
    let removed = sorted(vec![host.notification(1, 0), host.notification(1, 1)]);
    host.command(Command::RemoveOutput(1));
    host.until(|host| host.disarmed().len() == 2);
    assert_eq!(host.disarmed(), removed);
    assert_eq!(host.app.configurations.keys().count(), 1);
}

#[test]
fn watches_wait_for_the_protocol_and_resume_when_its_global_leaves() {
    let mut host = Host::start(1, false);
    // without a notifier object the host has nothing to send.
    assert!(host.records().is_empty());
    assert_eq!(host.log(0), "|false|false");

    host.command(Command::AddNotifier);
    host.until(|host| host.armed().len() == 2 && host.log(0) == "|true|false");
    let armed = host.armed();
    assert_eq!(
        armed
            .iter()
            .map(|(_, timeout)| *timeout)
            .collect::<Vec<_>>(),
        [5000, 60000]
    );
    host.send(armed[0].0, true);
    host.until(|host| host.log(0) == "idle true;|true|true");

    // the global leaving retires the watches; the idle one resumes because
    // its configuration is still current.
    host.command(Command::RemoveNotifier);
    host.until(|host| {
        host.log(0) == "idle true;resume false;|false|false" && host.disarmed().len() == 2
    });
    assert_eq!(host.disarmed(), sorted(vec![armed[0].0, armed[1].0]));

    // a later global arms them again.
    host.command(Command::AddNotifier);
    host.until(|host| host.armed().len() == 4 && host.log(0).ends_with("|true|false"));
    assert_eq!(host.log(0), "idle true;resume false;|true|false");
}

#[test]
fn a_removed_seat_resumes_its_watches_and_a_new_seat_rearms_them() {
    let mut host = Host::start(1, true);
    host.until(|host| host.armed().len() == 2);
    host.send(host.notification(0, 0), true);
    host.until(|host| host.log(0) == "idle true;|true|true");

    host.command(Command::RemoveSeat);
    host.until(|host| {
        host.log(0) == "idle true;resume false;|true|false" && host.disarmed().len() == 2
    });
    assert!(host.app.seat.is_none());

    host.command(Command::AddSeat);
    host.until(|host| host.armed().len() == 4);
    let seats = host.seats();
    assert_ne!(seats[0], seats[2]);
    host.send(host.armed()[2].0, true);
    host.until(|host| host.log(0) == "idle true;resume false;idle true;|true|true");
}

#[test]
fn rejected_reloads_keep_watches_and_accepted_reloads_replace_them() {
    let mut host = Host::start(1, true);
    host.until(|host| host.armed().len() == 2);
    let armed = host.armed();
    host.send(armed[0].0, true);
    host.until(|host| host.log(0) == "idle true;|true|true");

    // a rejected reload changes nothing: the old watch still delivers to the
    // current configuration.
    host.write("services.idle.watch { timeout = 10 }\nreturn ui.panel {}");
    host.reload();
    host.send(armed[0].0, false);
    host.send(armed[0].0, true);
    host.until(|host| host.log(0) == "idle true;resume false;idle true;|true|true");
    assert!(host.disarmed().is_empty());

    // an accepted reload retires the old watches without a resume for the
    // replaced configuration and arms the new declarations.
    host.write(&source(7000));
    host.reload();
    assert_eq!(host.log(0), "|true|false");
    host.until(|host| host.disarmed().len() == 2 && host.armed().len() == 4);
    assert_eq!(host.disarmed(), sorted(vec![armed[0].0, armed[1].0]));
    let rearmed = host.armed();
    assert_eq!(rearmed[2].1, 7000);
    // an event already queued for the first, retired watch is stale.
    assert_eq!(host.app.idle_notify.transition(1, false), None);
    host.send(rearmed[2].0, true);
    host.until(|host| host.log(0) == "idle true;|true|true");
}

const SLEEP_PANEL: &str = r#"
local inhibitor = services.sleep_inhibitor { key = 'presentation', reason = 'REASON' }
services.idle.watch {
    timeout = 5000,
    on_idle = function() inhibitor.start() end,
    on_resume = function() inhibitor.stop() end,
}
return ui.panel {
    style = { width = 640, height = 80 },
    children = { ui.text {
        style = { width = 0, height = 0 },
        text = function() return inhibitor.state:get() .. '|' .. inhibitor.error:get() end,
    } },
}
"#;

#[test]
fn sleep_inhibitors_are_shared_across_outputs_and_follow_accepted_configurations() {
    let bus = Bus::start(&[]);
    let logind = Logind::start(&bus, LogindBehavior::Grant);
    let panel = SLEEP_PANEL.replace("REASON", "slides are showing");
    let mut host = Host::start_with(2, true, |_| panel.clone(), Some(&bus));
    host.until(|host| host.armed().len() == 2);
    assert_eq!(host.log(0), "stopped|");
    assert_eq!(host.log(1), "stopped|");
    // declarations alone request nothing and never reach the bus.
    assert!(!host.app.sleep.started());

    // one output's event starts the shared lease; both outputs see it.
    let first = host.armed()[0].0;
    host.send(first, true);
    host.until(|host| host.log(0) == "active|" && host.log(1) == "active|");
    assert_eq!(
        logind.arguments(0),
        ["sleep", "thing", "slides are showing", "block"].map(String::from)
    );
    assert!(logind.open(0));

    // an unchanged reload keeps the lease: a new request would already show
    // as requested when the reload returns. new runtimes see the shared state.
    host.reload();
    assert_eq!(
        host.sleep_status("presentation"),
        Some(SleepInhibitorStatus::Active)
    );
    assert_eq!(host.log(0), "active|");

    // a rejected reload keeps it as well.
    host.write(
        "services.sleep_inhibitor { key = 'presentation', reason = '' }\nreturn ui.panel {}",
    );
    host.reload();
    assert_eq!(
        host.sleep_status("presentation"),
        Some(SleepInhibitorStatus::Active)
    );

    // the lease stays while any accepted configuration declares its key.
    host.command(Command::RemoveOutput(1));
    host.until(|host| host.app.configurations.keys().count() == 1);
    assert_eq!(
        host.sleep_status("presentation"),
        Some(SleepInhibitorStatus::Active)
    );
    assert!(logind.open(0));

    // the last declaration leaving releases it.
    host.write("return ui.panel { children = { ui.text { text = 'none' } } }");
    host.reload();
    assert_eq!(host.sleep_status("presentation"), None);
    host.until(|_| !logind.open(0));
    assert_eq!(logind.count(), 1);
}
