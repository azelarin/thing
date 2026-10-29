use std::{
    collections::{HashMap, HashSet},
    fs::File,
    os::{fd::AsFd, unix::net::UnixStream},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use calloop::EventLoop;
use smithay_client_toolkit::{
    compositor::CompositorState, output::OutputState, registry::RegistryState, seat::SeatState,
    session_lock::SessionLockState,
};
use tempfile::TempDir;
use thing_runtime::{AuthenticationSnapshot, ServiceSnapshot, SurfaceKey};
use thing_ui::Affine;
use wayland_client::{
    Connection, delegate_noop,
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::session_lock::v1::server::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1 as ServerManager,
    ext_session_lock_surface_v1::ExtSessionLockSurfaceV1 as ServerLockSurface,
    ext_session_lock_v1::ExtSessionLockV1 as ServerLock,
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource,
    backend::{GlobalId, ObjectId},
    protocol::{
        wl_buffer::WlBuffer as ServerBuffer, wl_callback::WlCallback as ServerCallback,
        wl_compositor::WlCompositor as ServerCompositor, wl_output::WlOutput as ServerOutput,
        wl_region::WlRegion as ServerRegion, wl_shm::WlShm as ServerShm,
        wl_shm_pool::WlShmPool as ServerShmPool, wl_surface::WlSurface as ServerSurface,
    },
};

use super::{App, LockLayoutPreflight, first_secret_input};
use crate::{
    authentication::{AuthenticationBroker, ProcessContext},
    configuration::Configurations,
    render::Render,
    watch::Watch,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const PROBE_ENV: &str = "THING_LOCK_HOST_WIRE_PROBE";
const LOCK_FIXTURE: &str = r#"
return ui.lock {
    style = { width = 320, height = 240, background = '#000000' },
    children = {
        ui.secret_input {
            autofocus = true,
            style = { width = 200, height = 40, translate_x = 8, rotation = 3 },
        },
    },
}
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Request {
    Lock,
    LockSurface(u32),
    AckConfigure(u32, u32),
    Attach(u32),
    Commit(u32),
    Locked,
    DestroySurface(u32),
    Unlock,
}

#[derive(Clone, Copy)]
struct OutputData(u32);

#[derive(Clone)]
struct LockSurfaceData {
    output: u32,
    surface: ObjectId,
}

#[derive(Clone, Copy)]
struct BufferData {
    width: i32,
    height: i32,
}

struct ServerState {
    records: Arc<Mutex<Vec<Request>>>,
    lock: Option<ServerLock>,
    locked: bool,
    surface_outputs: HashMap<ObjectId, u32>,
    pending_buffers: HashMap<ObjectId, ServerBuffer>,
    acknowledged: HashSet<ObjectId>,
    frame_callbacks: HashMap<ObjectId, Vec<ServerCallback>>,
}

impl ServerState {
    fn record(&self, request: Request) {
        self.records.lock().unwrap().push(request);
    }

    fn confirm_lock_after_first_commit(&mut self) {
        if self.locked {
            return;
        }
        self.locked = true;
        self.record(Request::Locked);
        self.lock.as_ref().unwrap().locked();
    }
}

impl GlobalDispatch<ServerCompositor, ()> for ServerState {
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

impl ServerDispatch<ServerCompositor, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerCompositor,
        request: <ServerCompositor as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            wayland_server::protocol::wl_compositor::Request::CreateSurface { id } => {
                data.init(id, ());
            }
            wayland_server::protocol::wl_compositor::Request::CreateRegion { id } => {
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
        request: <ServerSurface as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_surface::Request as SurfaceRequest;

        match request {
            SurfaceRequest::Attach {
                buffer: Some(buffer),
                ..
            } => {
                state.pending_buffers.insert(surface.id(), buffer);
                if let Some(output) = state.surface_outputs.get(&surface.id()).copied() {
                    state.record(Request::Attach(output));
                }
            }
            SurfaceRequest::Frame { callback } => {
                let callback = data.init(callback, ());
                state
                    .frame_callbacks
                    .entry(surface.id())
                    .or_default()
                    .push(callback);
            }
            SurfaceRequest::Commit => {
                let Some(output) = state.surface_outputs.get(&surface.id()).copied() else {
                    return;
                };
                assert!(state.acknowledged.contains(&surface.id()));
                let buffer = state.pending_buffers.get(&surface.id()).unwrap();
                let size = buffer.data::<BufferData>().unwrap();
                assert_eq!((size.width, size.height), (WIDTH as i32, HEIGHT as i32));
                state.record(Request::Commit(output));
                buffer.release();
                if let Some(callbacks) = state.frame_callbacks.remove(&surface.id()) {
                    for callback in callbacks {
                        callback.done(1);
                    }
                }
                state.confirm_lock_after_first_commit();
            }
            SurfaceRequest::Destroy => {
                state.surface_outputs.remove(&surface.id());
                state.pending_buffers.remove(&surface.id());
                state.acknowledged.remove(&surface.id());
                state.frame_callbacks.remove(&surface.id());
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ServerRegion, ()> for ServerState {
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

impl ServerDispatch<ServerCallback, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerCallback,
        _: <ServerCallback as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ServerOutput, OutputData> for ServerState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerOutput>,
        output: &OutputData,
        data: &mut DataInit<'_, Self>,
    ) {
        let resource = data.init(resource, *output);
        resource.geometry(
            0,
            0,
            300,
            200,
            wayland_server::protocol::wl_output::Subpixel::Unknown,
            "thing".into(),
            format!("private-{}", output.0),
            wayland_server::protocol::wl_output::Transform::Normal,
        );
        resource.mode(
            wayland_server::protocol::wl_output::Mode::Current
                | wayland_server::protocol::wl_output::Mode::Preferred,
            WIDTH as i32,
            HEIGHT as i32,
            60_000,
        );
        resource.scale(1);
        if resource.version() >= 4 {
            resource.name(format!("PRIVATE-{}", output.0));
            resource.description(format!("private output {}", output.0));
        }
        if resource.version() >= 2 {
            resource.done();
        }
    }
}

impl ServerDispatch<ServerOutput, OutputData> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerOutput,
        _: <ServerOutput as Resource>::Request,
        _: &OutputData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
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
        let resource = data.init(resource, ());
        resource.format(wayland_server::protocol::wl_shm::Format::Argb8888);
    }
}

impl ServerDispatch<ServerShm, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerShm,
        request: <ServerShm as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let wayland_server::protocol::wl_shm::Request::CreatePool { id, .. } = request {
            data.init(id, ());
        }
    }
}

impl ServerDispatch<ServerShmPool, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerShmPool,
        request: <ServerShmPool as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let wayland_server::protocol::wl_shm_pool::Request::CreateBuffer {
            id,
            width,
            height,
            ..
        } = request
        {
            data.init(id, BufferData { width, height });
        }
    }
}

impl ServerDispatch<ServerBuffer, BufferData> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerBuffer,
        _: <ServerBuffer as Resource>::Request,
        _: &BufferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ServerManager, ()> for ServerState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerManager>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ());
    }
}

impl ServerDispatch<ServerManager, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerManager,
        request: <ServerManager as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        use wayland_protocols::ext::session_lock::v1::server::ext_session_lock_manager_v1::Request as ManagerRequest;

        if let ManagerRequest::Lock { id } = request {
            state.record(Request::Lock);
            state.lock = Some(data.init(id, ()));
        }
    }
}

impl ServerDispatch<ServerLock, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerLock,
        request: <ServerLock as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        use wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::Request as LockRequest;

        match request {
            LockRequest::GetLockSurface {
                id,
                surface,
                output,
            } => {
                let output = output.data::<OutputData>().unwrap().0;
                let surface_id = surface.id();
                state.surface_outputs.insert(surface_id.clone(), output);
                state.record(Request::LockSurface(output));
                let role: ServerLockSurface = data.init(
                    id,
                    LockSurfaceData {
                        output,
                        surface: surface_id,
                    },
                );
                role.configure(100 + output, WIDTH, HEIGHT);
            }
            LockRequest::UnlockAndDestroy => state.record(Request::Unlock),
            LockRequest::Destroy => panic!("a confirmed lock used destroy instead of unlock"),
            _ => {}
        }
    }
}

impl ServerDispatch<ServerLockSurface, LockSurfaceData> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerLockSurface,
        request: <ServerLockSurface as Resource>::Request,
        role: &LockSurfaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wayland_protocols::ext::session_lock::v1::server::ext_session_lock_surface_v1::Request as SurfaceRequest;

        match request {
            SurfaceRequest::AckConfigure { serial } => {
                assert_eq!(serial, 100 + role.output);
                state.acknowledged.insert(role.surface.clone());
                state.record(Request::AckConfigure(role.output, serial));
            }
            SurfaceRequest::Destroy => state.record(Request::DestroySurface(role.output)),
            _ => {}
        }
    }
}

struct PrivateServer {
    connection: Connection,
    globals: GlobalList,
    queue: wayland_client::EventQueue<App>,
    records: Arc<Mutex<Vec<Request>>>,
    display: DisplayHandle,
    first_output: GlobalId,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl PrivateServer {
    fn add_output(&self, id: u32) -> GlobalId {
        self.display
            .create_global::<ServerState, ServerOutput, _>(4, OutputData(id))
    }

    fn remove_output(&self, output: GlobalId) {
        self.display.disable_global::<ServerState>(output);
    }

    fn records(&self) -> Vec<Request> {
        self.records.lock().unwrap().clone()
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        self.display
            .disable_global::<ServerState>(self.first_output.clone());
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

fn private_server() -> PrivateServer {
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    server_socket.set_nonblocking(true).unwrap();
    let mut display = Display::<ServerState>::new().unwrap();
    let mut handle = display.handle();
    let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
    handle.create_global::<ServerState, ServerCompositor, _>(6, ());
    handle.create_global::<ServerState, ServerShm, _>(1, ());
    handle.create_global::<ServerState, ServerManager, _>(1, ());
    let first_output = handle.create_global::<ServerState, ServerOutput, _>(4, OutputData(1));

    let records = Arc::new(Mutex::new(Vec::new()));
    let server_records = records.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server = thread::spawn(move || {
        let mut state = ServerState {
            records: server_records,
            lock: None,
            locked: false,
            surface_outputs: HashMap::new(),
            pending_buffers: HashMap::new(),
            acknowledged: HashSet::new(),
            frame_callbacks: HashMap::new(),
        };
        while !server_stop.load(Ordering::Relaxed) {
            match display
                .backend()
                .dispatch_single_client(&mut state, client.id())
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("private Wayland server failed: {error}"),
            }
            display.backend().flush(Some(client.id())).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    });

    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, queue) = registry_queue_init(&connection).unwrap();
    PrivateServer {
        connection,
        globals,
        queue,
        records,
        display: handle,
        first_output,
        stop,
        server: Some(server),
    }
}

delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);

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

fn shared_buffer(globals: &GlobalList, qh: &wayland_client::QueueHandle<App>) -> SharedBuffer {
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

fn lock_fixture() -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lock.luau");
    std::fs::write(&path, LOCK_FIXTURE).unwrap();
    (directory, path.canonicalize().unwrap())
}

fn app_for_private_server(
    private: &mut PrivateServer,
    entry: std::path::PathBuf,
    buffer: &SharedBuffer,
) -> (App, EventLoop<'static, App>) {
    let qh = private.queue.handle();
    let compositor = CompositorState::bind(&private.globals, &qh).unwrap();
    let session_lock_state = SessionLockState::new(&private.globals, &qh);
    let (image_ping, _) = calloop::ping::make_ping().unwrap();
    let event_loop = EventLoop::try_new().unwrap();
    let watch = std::rc::Rc::new(Watch::new().unwrap());
    watch.set_files(std::slice::from_ref(&entry)).unwrap();
    let authentication = AuthenticationSnapshot::default();
    let mut app = App {
        connection: private.connection.clone(),
        render: Render::new_test(buffer.buffer.clone(), (WIDTH, HEIGHT)),
        image_ping,
        configurations: Configurations::new_lock(entry, ServiceSnapshot::default()),
        watch,
        reload_at: None,
        authentication: AuthenticationBroker::new(ProcessContext::disabled_for_test(), 1),
        authentication_snapshot: authentication,
        pending_prompt: None,
        surfaces: Vec::new(),
        registry: RegistryState::new(&private.globals),
        outputs: OutputState::new(&private.globals, &qh),
        seats: SeatState::new(&private.globals, &qh),
        seat: None,
        keyboard: None,
        keyboard_surface: None,
        keyboard_keymap: None,
        modifiers: Default::default(),
        repeat: Default::default(),
        repeat_token: None,
        loop_handle: event_loop.handle(),
        compositor,
        session_lock_state,
        session_lock: None,
        unlock_sync: None,
        unlock_requested: false,
        fractional: None,
        viewporter: None,
        lifecycle: super::model::Lifecycle::new(),
        queue: qh,
        loading_outputs: true,
        render_failed: false,
        exit: false,
        result: Ok(()),
    };
    private.queue.roundtrip(&mut app).unwrap();
    (app, event_loop)
}

fn drive(private: &mut PrivateServer, app: &mut App, draw: bool) {
    private.queue.roundtrip(app).unwrap();
    if draw {
        let qh = private.queue.handle();
        app.draw(&qh);
        assert!(!app.render_failed, "test renderer failed: {:?}", app.result);
    }
    app.connection.flush().unwrap();
    private.queue.roundtrip(app).unwrap();
}

fn record_position(records: &[Request], request: Request) -> usize {
    records
        .iter()
        .position(|candidate| *candidate == request)
        .unwrap_or_else(|| panic!("missing request {request:?} in {records:?}"))
}

fn run_host_probe() {
    let (_directory, entry) = lock_fixture();
    let mut private = private_server();
    let qh = private.queue.handle();
    let buffer = shared_buffer(&private.globals, &qh);
    let (mut app, _event_loop) = app_for_private_server(&mut private, entry.clone(), &buffer);

    let outputs: Vec<_> = app
        .outputs
        .outputs()
        .map(|output| {
            let context = app.output_context(&output).unwrap();
            (output, context)
        })
        .collect();
    assert_eq!(outputs.len(), 1);
    let entries = outputs
        .iter()
        .map(|(output, context)| app.layout_validation_entry(output.clone(), context, true))
        .collect();
    let preflight = LockLayoutPreflight::new(&mut app.render, entries);
    app.configurations
        .reload_for_checked(outputs.clone(), |output, runtime| {
            preflight.validate(output, runtime.surfaces())
        })
        .unwrap();
    let _ = preflight.into_prepared();
    let lock = app.session_lock_state.lock(&qh).unwrap();
    app.session_lock = Some(lock);
    for (output, _) in &outputs {
        app.create_surface(output.clone(), &qh);
    }
    app.loading_outputs = false;

    drive(&mut private, &mut app, true);
    assert_eq!(app.lifecycle.phase(), super::model::Phase::Locked);
    let root = app
        .configurations
        .surface_root(&outputs[0].0, &SurfaceKey::Default)
        .unwrap();
    let secret = first_secret_input(root).unwrap();
    let target = app.surfaces[0].layouts.presented_target(secret).unwrap();
    let placement = app.surfaces[0]
        .layouts
        .presented()
        .unwrap()
        .get(secret)
        .unwrap();
    assert_ne!(placement.transform, Affine::IDENTITY);
    assert!(app.focus_from_keyboard(0, target.clone()));
    let (_, width) = app.secret_node_and_width(0, &target).unwrap();
    assert_eq!(width, 200.0);
    let records = private.records();
    let lock = record_position(&records, Request::Lock);
    let lock_surface = record_position(&records, Request::LockSurface(1));
    let ack = record_position(&records, Request::AckConfigure(1, 101));
    let attach = record_position(&records, Request::Attach(1));
    let commit = record_position(&records, Request::Commit(1));
    let locked = record_position(&records, Request::Locked);
    assert!(lock < lock_surface && lock_surface < ack && ack < attach);
    assert!(attach < commit && commit < locked);

    let output = &outputs[0].0;
    let consumer = app.configurations.consumer(output).unwrap().to_owned();
    let root = app
        .configurations
        .surface_root(output, &SurfaceKey::Default)
        .unwrap()
        .clone();
    std::fs::write(
        &entry,
        r#"
        local child = ui.secret_input {
            autofocus = true,
            style = { width = 200, height = 40 },
        }
        for _ = 1, 20 do
            child = ui.column {
                style = { scale_x = 100 },
                children = { child },
            }
        end
        return ui.lock {
            style = { width = 320, height = 240 },
            children = { child },
        }
        "#,
    )
    .unwrap();
    app.reload();
    assert_eq!(app.configurations.consumer(output), Some(consumer.as_str()));
    assert_eq!(
        app.configurations
            .surface_root(output, &SurfaceKey::Default),
        Some(&root)
    );
    assert!(!app.render_failed);
    drive(&mut private, &mut app, true);
    std::fs::write(&entry, LOCK_FIXTURE).unwrap();
    app.reload();

    let second_output = private.add_output(2);
    for _ in 0..3 {
        drive(&mut private, &mut app, true);
    }
    assert_eq!(app.surfaces.len(), 2);
    assert!(app.surfaces.iter().all(|surface| surface.configured));
    assert!(
        app.surfaces.iter().all(|surface| app
            .configurations
            .surface_root(&surface.output, &SurfaceKey::Default)
            .is_some()),
        "every lock surface must have an output configuration"
    );
    let records = private.records();
    assert!(records.contains(&Request::LockSurface(2)));
    assert!(records.contains(&Request::AckConfigure(2, 102)));
    assert!(records.contains(&Request::Commit(2)));

    private.remove_output(second_output);
    for _ in 0..2 {
        drive(&mut private, &mut app, false);
    }
    assert_eq!(app.surfaces.len(), 1);
    assert!(private.records().contains(&Request::DestroySurface(2)));

    assert_eq!(
        app.lifecycle.authenticated(),
        super::model::Action::UnlockAndSync
    );
    app.unlock_and_sync();
    assert!(!app.exit);
    app.connection.flush().unwrap();
    private.queue.roundtrip(&mut app).unwrap();
    assert!(app.exit);
    assert_eq!(app.lifecycle.phase(), super::model::Phase::Finished);
    assert!(private.records().contains(&Request::Unlock));
}

#[test]
fn real_lock_host_tracks_private_outputs_and_syncs_confirmed_unlock() {
    if std::env::var_os(PROBE_ENV).is_some() {
        run_host_probe();
        return;
    }

    let test_name =
        "lock::host_wire_tests::real_lock_host_tracks_private_outputs_and_syncs_confirmed_unlock";
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(PROBE_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("private lock host probe timed out: {output:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
}
