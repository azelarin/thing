use std::{
    io,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use smithay_client_toolkit::{
    delegate_session_lock,
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{wl_callback, wl_compositor, wl_output, wl_registry, wl_surface},
};
use wayland_protocols::ext::session_lock::v1::server::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1 as ServerManager,
    ext_session_lock_surface_v1::ExtSessionLockSurfaceV1 as ServerLockSurface,
    ext_session_lock_v1::ExtSessionLockV1 as ServerLock,
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource,
    protocol::{
        wl_compositor::WlCompositor as ServerCompositor, wl_output::WlOutput as ServerOutput,
        wl_region::WlRegion as ServerRegion, wl_surface::WlSurface as ServerSurface,
    },
};

use super::model::{Action, Lifecycle, Phase};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Request {
    Lock,
    LockSurface,
    AckConfigure(u32),
    Commit,
    Destroy,
    Unlock,
}

#[derive(Clone, Copy)]
enum Disposition {
    Accept,
    Reject,
}

struct WireClient {
    lifecycle: Lifecycle,
    lock: Option<SessionLock>,
    surface: Option<SessionLockSurface>,
    configured: Option<(u32, u32)>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WireClient {
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

impl Dispatch<wl_callback::WlCallback, ()> for WireClient {
    fn event(
        client: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            assert!(client.lifecycle.sync_done());
        }
    }
}

impl SessionLockHandler for WireClient {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.lifecycle.locked();
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, lock: SessionLock) {
        match self.lifecycle.compositor_finished() {
            Action::DestroyAndExit => self.lock = None,
            Action::UnlockAndSync => {
                self.lock = None;
                lock.unlock();
            }
            Action::None => {}
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        self.configured = Some(configure.new_size);
    }
}

delegate_session_lock!(WireClient);
delegate_noop!(WireClient: ignore wl_compositor::WlCompositor);
delegate_noop!(WireClient: ignore wl_surface::WlSurface);
delegate_noop!(WireClient: ignore wl_output::WlOutput);

struct WireServer {
    records: Arc<Mutex<Vec<Request>>>,
    lock: Option<ServerLock>,
    disposition: Disposition,
}

impl GlobalDispatch<ServerCompositor, ()> for WireServer {
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

impl ServerDispatch<ServerCompositor, ()> for WireServer {
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

impl ServerDispatch<ServerSurface, ()> for WireServer {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerSurface,
        request: <ServerSurface as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if matches!(
            request,
            wayland_server::protocol::wl_surface::Request::Commit
        ) {
            state.records.lock().unwrap().push(Request::Commit);
        }
    }
}

impl ServerDispatch<ServerRegion, ()> for WireServer {
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

impl GlobalDispatch<ServerOutput, ()> for WireServer {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerOutput>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ());
    }
}

impl ServerDispatch<ServerOutput, ()> for WireServer {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerOutput,
        _: <ServerOutput as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ServerManager, ()> for WireServer {
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

impl ServerDispatch<ServerManager, ()> for WireServer {
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
            state.records.lock().unwrap().push(Request::Lock);
            let lock = data.init(id, ());
            if matches!(state.disposition, Disposition::Reject) {
                lock.finished();
            }
            state.lock = Some(lock);
        }
    }
}

impl ServerDispatch<ServerLock, ()> for WireServer {
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
            LockRequest::GetLockSurface { id, .. } => {
                state.records.lock().unwrap().push(Request::LockSurface);
                let surface: ServerLockSurface = data.init(id, ());
                surface.configure(19, 1920, 1080);
            }
            LockRequest::UnlockAndDestroy => {
                state.records.lock().unwrap().push(Request::Unlock);
            }
            LockRequest::Destroy => {
                state.records.lock().unwrap().push(Request::Destroy);
            }
            _ => {}
        }
    }
}

impl ServerDispatch<ServerLockSurface, ()> for WireServer {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerLockSurface,
        request: <ServerLockSurface as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wayland_protocols::ext::session_lock::v1::server::ext_session_lock_surface_v1::Request as SurfaceRequest;

        if let SurfaceRequest::AckConfigure { serial } = request {
            state
                .records
                .lock()
                .unwrap()
                .push(Request::AckConfigure(serial));
            if matches!(state.disposition, Disposition::Accept) {
                state.lock.as_ref().unwrap().locked();
            }
        }
    }
}

struct PrivateServer {
    connection: Connection,
    globals: GlobalList,
    queue: wayland_client::EventQueue<WireClient>,
    records: Arc<Mutex<Vec<Request>>>,
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

fn private_server(disposition: Disposition) -> PrivateServer {
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    server_socket.set_nonblocking(true).unwrap();
    let mut display = Display::<WireServer>::new().unwrap();
    let mut handle = display.handle();
    let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
    handle.create_global::<WireServer, ServerCompositor, _>(6, ());
    handle.create_global::<WireServer, ServerOutput, _>(4, ());
    handle.create_global::<WireServer, ServerManager, _>(1, ());

    let records = Arc::new(Mutex::new(Vec::new()));
    let server_records = records.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server = thread::spawn(move || {
        let mut state = WireServer {
            records: server_records,
            lock: None,
            disposition,
        };
        while !server_stop.load(Ordering::Relaxed) {
            match display
                .backend()
                .dispatch_single_client(&mut state, client.id())
            {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
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
        stop,
        server: Some(server),
    }
}

#[test]
fn lock_roles_wait_for_configure_and_unlock_only_after_confirmation() {
    let mut private = private_server(Disposition::Accept);
    let qh = private.queue.handle();
    let compositor: wl_compositor::WlCompositor = private.globals.bind(&qh, 1..=6, ()).unwrap();
    let output: wl_output::WlOutput = private.globals.bind(&qh, 1..=4, ()).unwrap();
    let lock_state = SessionLockState::new(&private.globals, &qh);
    let lock = lock_state.lock(&qh).unwrap();
    let surface = compositor.create_surface(&qh, ());
    let role = lock.create_lock_surface(surface, &output, &qh);
    let mut client = WireClient {
        lifecycle: Lifecycle::new(),
        lock: Some(lock),
        surface: Some(role),
        configured: None,
    };

    for _ in 0..3 {
        private.queue.roundtrip(&mut client).unwrap();
    }

    assert_eq!(client.configured, Some((1920, 1080)));
    assert_eq!(client.lifecycle.phase(), Phase::Locked);
    let records = private.records.lock().unwrap().clone();
    assert!(records.starts_with(&[
        Request::Lock,
        Request::LockSurface,
        Request::AckConfigure(19),
    ]));
    assert!(!records.contains(&Request::Commit));

    assert_eq!(client.lifecycle.authenticated(), Action::UnlockAndSync);
    client.lock.take().unwrap().unlock();
    let _sync = private.connection.display().sync(&qh, ());
    for _ in 0..2 {
        private.queue.roundtrip(&mut client).unwrap();
    }
    assert!(private.records.lock().unwrap().contains(&Request::Unlock));
    assert_eq!(client.lifecycle.phase(), Phase::Finished);

    drop(client.surface.take());
}

#[test]
fn a_declined_lock_is_destroyed_without_an_unlock_request() {
    let mut private = private_server(Disposition::Reject);
    let qh = private.queue.handle();
    let compositor: wl_compositor::WlCompositor = private.globals.bind(&qh, 1..=6, ()).unwrap();
    let output: wl_output::WlOutput = private.globals.bind(&qh, 1..=4, ()).unwrap();
    let lock_state = SessionLockState::new(&private.globals, &qh);
    let lock = lock_state.lock(&qh).unwrap();
    let surface = compositor.create_surface(&qh, ());
    let role = lock.create_lock_surface(surface, &output, &qh);
    let mut client = WireClient {
        lifecycle: Lifecycle::new(),
        lock: Some(lock),
        surface: Some(role),
        configured: None,
    };

    for _ in 0..3 {
        private.queue.roundtrip(&mut client).unwrap();
    }

    let records = private.records.lock().unwrap().clone();
    assert_eq!(client.lifecycle.phase(), Phase::Finished);
    assert!(records.contains(&Request::Destroy));
    assert!(!records.contains(&Request::Unlock));
    assert!(!records.contains(&Request::Commit));
}
