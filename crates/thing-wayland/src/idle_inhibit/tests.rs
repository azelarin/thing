use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use smithay_client_toolkit::{
    delegate_registry,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use wayland_client::{
    Connection, Proxy, delegate_noop,
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_compositor::WlCompositor, wl_surface::WlSurface},
};
use wayland_protocols::wp::idle_inhibit::zv1::server::{
    zwp_idle_inhibit_manager_v1::{self, ZwpIdleInhibitManagerV1 as ServerManager},
    zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1 as ServerInhibitor,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
    backend::GlobalId,
    protocol::{
        wl_compositor::{self, WlCompositor as ServerCompositor},
        wl_surface::{self, WlSurface as ServerSurface},
    },
};

use super::{State, ZwpIdleInhibitManagerV1, ZwpIdleInhibitorV1};

struct ClientState {
    registry: RegistryState,
}

impl ProvidesRegistryState for ClientState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![];
}

delegate_registry!(ClientState);
delegate_noop!(ClientState: ignore WlCompositor);
delegate_noop!(ClientState: ignore WlSurface);
delegate_noop!(ClientState: ignore ZwpIdleInhibitManagerV1);
delegate_noop!(ClientState: ignore ZwpIdleInhibitorV1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Bind(u32),
    Create { manager: u32, surface: u32 },
    DestroyInhibitor(u32),
    DestroyManager(u32),
    Commit(u32),
    DestroySurface(u32),
}

struct ServerState(Arc<Mutex<Vec<Request>>>);

impl ServerState {
    fn record(&self, request: Request) {
        self.0.lock().unwrap().push(request);
    }
}

impl GlobalDispatch<ServerManager, u32> for ServerState {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ServerManager>,
        tag: &u32,
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, *tag);
        state.record(Request::Bind(*tag));
    }
}

impl Dispatch<ServerManager, u32> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerManager,
        request: zwp_idle_inhibit_manager_v1::Request,
        tag: &u32,
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_idle_inhibit_manager_v1::Request::CreateInhibitor { id, surface } => {
                let surface = surface.id().protocol_id();
                data.init(id, surface);
                state.record(Request::Create {
                    manager: *tag,
                    surface,
                });
            }
            zwp_idle_inhibit_manager_v1::Request::Destroy => {
                state.record(Request::DestroyManager(*tag))
            }
            _ => {}
        }
    }
}

impl Dispatch<ServerInhibitor, u32> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ServerInhibitor,
        _: <ServerInhibitor as Resource>::Request,
        surface: &u32,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        state.record(Request::DestroyInhibitor(*surface));
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

impl Dispatch<ServerCompositor, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let wl_compositor::Request::CreateSurface { id } = request {
            data.init(id, ());
        }
    }
}

impl Dispatch<ServerSurface, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &ServerSurface,
        request: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let id = surface.id().protocol_id();
        match request {
            wl_surface::Request::Commit => state.record(Request::Commit(id)),
            wl_surface::Request::Destroy => state.record(Request::DestroySurface(id)),
            _ => {}
        }
    }
}

struct PrivateServer {
    globals: GlobalList,
    queue: wayland_client::EventQueue<ClientState>,
    client: ClientState,
    display: DisplayHandle,
    records: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl PrivateServer {
    fn new() -> Self {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        server_socket.set_nonblocking(true).unwrap();
        let mut display = Display::<ServerState>::new().unwrap();
        let mut handle = display.handle();
        let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
        handle.create_global::<ServerState, ServerCompositor, _>(1, ());
        let records = Arc::new(Mutex::new(Vec::new()));
        let server_records = records.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState(server_records);
            while !server_stop.load(Ordering::Relaxed) {
                match display
                    .backend()
                    .dispatch_single_client(&mut state, client.id())
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("private wayland server failed: {error}"),
                }
                display.backend().flush(Some(client.id())).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        let connection = Connection::from_socket(client_socket).unwrap();
        let (globals, queue) = registry_queue_init(&connection).unwrap();
        let client = ClientState {
            registry: RegistryState::new(&globals),
        };
        Self {
            globals,
            queue,
            client,
            display: handle,
            records,
            stop,
            server: Some(server),
        }
    }

    fn add_manager(&mut self, tag: u32) -> GlobalId {
        let global = self
            .display
            .create_global::<ServerState, ServerManager, _>(1, tag);
        self.sync();
        global
    }

    fn remove_manager(&mut self, global: GlobalId) {
        self.display.disable_global::<ServerState>(global);
        self.sync();
    }

    fn surface(&self) -> WlSurface {
        let qh = self.queue.handle();
        let compositor: WlCompositor = self.globals.bind(&qh, 1..=1, ()).unwrap();
        compositor.create_surface(&qh, ())
    }

    fn sync(&mut self) {
        self.queue.roundtrip(&mut self.client).unwrap();
    }
    fn records(&self) -> Vec<Request> {
        self.records.lock().unwrap().clone()
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

#[test]
fn absent_manager_rejects_true_and_a_later_global_can_be_bound() {
    let mut server = PrivateServer::new();
    let qh = server.queue.handle();
    let surface = server.surface();
    let mut state = State::default();
    let mut inhibitor = None;
    assert!(!state.bind_next(&server.client.registry, &qh).unwrap());
    state
        .reconcile(&mut inhibitor, &surface, false, &qh)
        .unwrap();
    assert!(
        state
            .reconcile(&mut inhibitor, &surface, true, &qh)
            .is_err()
    );
    server.add_manager(1);
    assert!(state.bind_next(&server.client.registry, &qh).unwrap());
    assert!(!state.bind_next(&server.client.registry, &qh).unwrap());
    state
        .reconcile(&mut inhibitor, &surface, true, &qh)
        .unwrap();
    let id = inhibitor.as_ref().unwrap().proxy.id();
    state
        .reconcile(&mut inhibitor, &surface, true, &qh)
        .unwrap();
    assert_eq!(inhibitor.as_ref().unwrap().proxy.id(), id);
    surface.commit();
    server.sync();
    let surface_id = surface.id().protocol_id();
    assert_eq!(
        server.records(),
        vec![
            Request::Bind(1),
            Request::Create {
                manager: 1,
                surface: surface_id
            },
            Request::Commit(surface_id)
        ]
    );
    state
        .reconcile(&mut inhibitor, &surface, false, &qh)
        .unwrap();
    surface.destroy();
    drop(state);
    server.sync();
    assert_eq!(
        &server.records()[3..],
        &[
            Request::DestroyInhibitor(surface_id),
            Request::DestroySurface(surface_id),
            Request::DestroyManager(1)
        ]
    );
}

#[test]
fn active_removal_destroys_children_before_rebinding_a_fresh_generation() {
    let mut server = PrivateServer::new();
    let first = server.add_manager(1);
    let second = server.add_manager(2);
    let qh = server.queue.handle();
    let mut state = State::default();
    state.bind_next(&server.client.registry, &qh).unwrap();
    let name = state.active().unwrap().registry_name;
    let generation = state.active().unwrap().generation;
    let panel = server.surface();
    let popup = server.surface();
    let mut children = [None, None];
    state
        .reconcile(&mut children[0], &panel, true, &qh)
        .unwrap();
    state
        .reconcile(&mut children[1], &popup, true, &qh)
        .unwrap();
    assert!(!state.retire(name + 1000, &mut children));
    assert!(children.iter().all(Option::is_some));
    server.sync();
    let previous = server.records().len();
    server.remove_manager(first);
    assert!(state.retire(name, &mut children));
    assert!(children.iter().all(Option::is_none));
    assert!(state.bind_next(&server.client.registry, &qh).unwrap());
    assert!(state.active().unwrap().generation > generation);
    assert!(!state.retire(name, &mut children));
    state
        .reconcile(&mut children[0], &panel, true, &qh)
        .unwrap();
    state
        .reconcile(&mut children[1], &popup, true, &qh)
        .unwrap();
    server.sync();
    let panel_id = panel.id().protocol_id();
    let popup_id = popup.id().protocol_id();
    assert_eq!(
        &server.records()[previous..],
        &[
            Request::DestroyInhibitor(panel_id),
            Request::DestroyInhibitor(popup_id),
            Request::DestroyManager(1),
            Request::Bind(2),
            Request::Create {
                manager: 2,
                surface: panel_id
            },
            Request::Create {
                manager: 2,
                surface: popup_id
            },
        ]
    );
    let name = state.active().unwrap().registry_name;
    server.remove_manager(second);
    assert!(state.retire(name, &mut children));
    assert!(!state.bind_next(&server.client.registry, &qh).unwrap());
    assert!(state.active().is_none());
    assert!(
        state
            .reconcile(&mut children[0], &panel, true, &qh)
            .is_err()
    );
    panel.commit();
    popup.commit();
    server.sync();
    assert_eq!(
        &server.records()[previous + 6..],
        &[
            Request::DestroyInhibitor(panel_id),
            Request::DestroyInhibitor(popup_id),
            Request::DestroyManager(2),
            Request::Commit(panel_id),
            Request::Commit(popup_id),
        ]
    );
    panel.destroy();
    popup.destroy();
    server.sync();
}

fn configurations(
    source: &str,
    available: bool,
) -> (tempfile::TempDir, crate::configuration::Configurations<u32>) {
    use thing_core::Scale;
    use thing_runtime::{OutputContext, Scalar, ServiceSnapshot};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    std::fs::write(&path, source).unwrap();
    let mut services = ServiceSnapshot::default();
    services
        .set_service("idle.inhibit_available", Scalar::Boolean(available))
        .unwrap();
    let mut configurations = crate::configuration::Configurations::new(path, services);
    for key in [1, 2] {
        let prepared = configurations
            .prepare_add_checked(
                key,
                OutputContext {
                    name: Some(format!("DP-{key}")),
                    width: 1920,
                    height: 1080,
                    scale: Scale::default(),
                },
                |runtime| super::validate(runtime.surfaces(), available),
            )
            .unwrap()
            .unwrap();
        configurations.commit_add(prepared);
    }
    (directory, configurations)
}

#[test]
fn capability_checks_include_hidden_panels_and_closed_popups() {
    use thing_core::{Kind, Node, NodeId};
    use thing_runtime::{SurfaceKey, SurfaceTree};
    let mut root = Node::new(NodeId(1), Kind::Panel);
    root.visible = false;
    root.idle_inhibit = true;
    let mut surface = SurfaceTree {
        key: SurfaceKey::Default,
        namespace: "test".into(),
        root,
    };
    assert!(super::validate(std::slice::from_ref(&surface), false).is_err());
    surface.root.idle_inhibit = false;
    let mut popup = Node::new(NodeId(2), Kind::Popup);
    popup.visible = false;
    popup.idle_inhibit = true;
    surface.root.children.push(popup);
    assert!(super::validate(std::slice::from_ref(&surface), false).is_err());
    assert!(super::validate(&[surface], true).is_ok());
}

#[test]
fn availability_refreshes_each_output_independently_and_recovers() {
    use thing_runtime::Scalar;
    let (_directory, mut configurations) = configurations(
        r#"
        return ui.panel {
            idle_inhibit = function()
                local available = services.idle.inhibit_available:get()
                if available and services.output.name:get() == 'DP-2' and services.clock.time:get() ~= 'retry' then
                    error('output is not ready')
                end
                return available
            end,
        }
    "#,
        false,
    );
    configurations
        .set_service("idle.inhibit_available", Scalar::Boolean(true))
        .unwrap();
    assert_eq!(
        configurations.refresh_checked(|_, surfaces| super::validate(surfaces, true)),
        vec![1]
    );
    assert!(super::requested(configurations.surfaces(&1).unwrap()));
    assert!(!super::requested(configurations.surfaces(&2).unwrap()));
    configurations
        .set_service("clock.time", Scalar::String("retry".into()))
        .unwrap();
    assert_eq!(
        configurations.refresh_checked(|_, surfaces| super::validate(surfaces, true)),
        vec![2]
    );
    configurations
        .set_service("idle.inhibit_available", Scalar::Boolean(false))
        .unwrap();
    assert_eq!(
        configurations.refresh_checked(|_, surfaces| super::validate(surfaces, false)),
        vec![1, 2]
    );
    assert!(
        configurations
            .keys()
            .all(|output| !super::requested(configurations.surfaces(output).unwrap()))
    );
}

#[test]
fn withdrawal_leaves_static_or_rejected_requests_detectable() {
    use thing_runtime::Scalar;
    for source in [
        "return ui.panel { idle_inhibit = true }",
        r#"return ui.panel { idle_inhibit = function()
            if not services.idle.inhibit_available:get() then error('unavailable') end
            return true
        end }"#,
    ] {
        let (_directory, mut configurations) = configurations(source, true);
        configurations
            .set_service("idle.inhibit_available", Scalar::Boolean(false))
            .unwrap();
        assert!(
            configurations
                .refresh_checked(|_, surfaces| super::validate(surfaces, false))
                .is_empty()
        );
        assert!(
            configurations
                .keys()
                .all(|output| super::requested(configurations.surfaces(output).unwrap()))
        );
    }
}

#[test]
fn rejected_refresh_and_reload_preserve_live_panel_and_popup_inhibitors() {
    use thing_runtime::Scalar;
    let mut server = PrivateServer::new();
    server.add_manager(1);
    let qh = server.queue.handle();
    let mut state = State::default();
    state.bind_next(&server.client.registry, &qh).unwrap();
    let (directory, mut configurations) = configurations(
        r#"
        return ui.panel {
            idle_inhibit = true,
            style = { width = function()
                if services.clock.time:get() == 'bad' then error('invalid update') end
                return 400
            end },
            children = { ui.popup { idle_inhibit = true } },
        }
    "#,
        true,
    );
    let panel = server.surface();
    let popup = server.surface();
    let mut children = [None, None];
    state
        .reconcile(&mut children[0], &panel, true, &qh)
        .unwrap();
    state
        .reconcile(&mut children[1], &popup, true, &qh)
        .unwrap();
    let ids = children
        .each_ref()
        .map(|child| child.as_ref().unwrap().proxy.id());
    server.sync();
    let requests = server.records();
    configurations
        .set_service("clock.time", Scalar::String("bad".into()))
        .unwrap();
    assert!(
        configurations
            .refresh_checked(|_, surfaces| super::validate(surfaces, true))
            .is_empty()
    );
    std::fs::write(
        directory.path().join("panel.luau"),
        "return ui.panel { idle_inhibit = 'yes' }",
    )
    .unwrap();
    let outputs = configurations
        .keys()
        .map(|key| (*key, configurations.output(key).unwrap().clone()))
        .collect();
    assert!(
        configurations
            .prepare_reload_for_checked(outputs, |_, runtime| super::validate(
                runtime.surfaces(),
                true
            ))
            .is_err()
    );
    let root = configurations
        .surface_root(&1, &thing_runtime::SurfaceKey::Default)
        .unwrap();
    state
        .reconcile(&mut children[0], &panel, root.idle_inhibit, &qh)
        .unwrap();
    state
        .reconcile(&mut children[1], &popup, root.children[0].idle_inhibit, &qh)
        .unwrap();
    assert_eq!(
        children
            .each_ref()
            .map(|child| child.as_ref().unwrap().proxy.id()),
        ids
    );
    server.sync();
    assert_eq!(server.records(), requests);
    drop(children);
    popup.destroy();
    panel.destroy();
    drop(state);
    server.sync();
}
