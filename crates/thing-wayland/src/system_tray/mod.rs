//! bounded StatusNotifierItem discovery, state, and controls.

mod model;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::Duration,
};

use async_io::{Async, Timer};
use calloop::ping::Ping;
use futures_lite::future;
use futures_util::{
    FutureExt, StreamExt,
    future::LocalBoxFuture,
    stream::{self, FuturesUnordered},
};
use thing_runtime::{SystemTrayAction, SystemTrayItem, SystemTraySnapshot};
use zbus::{
    Connection, MatchRule, MessageStream,
    fdo::{DBusProxy, RequestNameFlags, RequestNameReply},
    message::{Flags, Header, Message, Type},
    names::BusName,
    object_server::SignalEmitter,
    zvariant::{OwnedObjectPath, OwnedValue},
};

use model::{Endpoint, Lifetime, Model, QueriedItem, Target};

const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const KDE_WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const KDE_WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const KDE_HOST_PREFIX: &str = "org.kde.StatusNotifierHost";
const FREEDESKTOP_WATCHER_NAME: &str = "org.freedesktop.StatusNotifierWatcher";
const FREEDESKTOP_WATCHER_INTERFACE: &str = "org.freedesktop.StatusNotifierWatcher";
const FREEDESKTOP_HOST_PREFIX: &str = "org.freedesktop.StatusNotifierHost";
const KDE_ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const FREEDESKTOP_ITEM_INTERFACE: &str = "org.freedesktop.StatusNotifierItem";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";
const MAX_ITEMS: usize = 128;
const MAX_HOSTS: usize = 64;
const MAX_COMMANDS: usize = 128;
const MAX_QUERIES: usize = 4;
const MAX_ENDPOINT_BYTES: usize = 4096;
const MAX_SIGNAL_BYTES: usize = 8 * 1024;
const MAX_PROPERTY_BYTES: usize = 512 * 1024;
const MAX_PIXMAPS: usize = 32;
const MAX_PIXMAP_BYTES: usize = 256 * 1024;
const METHOD_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct WatcherProtocol {
    name: &'static str,
    interface: &'static str,
    host_prefix: &'static str,
}

const KDE_PROTOCOL: WatcherProtocol = WatcherProtocol {
    name: KDE_WATCHER_NAME,
    interface: KDE_WATCHER_INTERFACE,
    host_prefix: KDE_HOST_PREFIX,
};
const FREEDESKTOP_PROTOCOL: WatcherProtocol = WatcherProtocol {
    name: FREEDESKTOP_WATCHER_NAME,
    interface: FREEDESKTOP_WATCHER_INTERFACE,
    host_prefix: FREEDESKTOP_HOST_PREFIX,
};
// kde is preferred when both practical protocol variants are present. the
// order also makes two-name acquisition and fallback deterministic.
const WATCHER_PROTOCOLS: [WatcherProtocol; 2] = [KDE_PROTOCOL, FREEDESKTOP_PROTOCOL];

#[derive(Clone, Debug, PartialEq, Eq)]
struct WatcherTarget {
    protocol: WatcherProtocol,
    owner: String,
}

/// how the transport participates in the status notifier protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemTrayMode {
    /// register a visualization host only when another watcher exists.
    Host,
    /// own both recognized watcher names only when both are free, otherwise act as a host.
    OwnWatcher,
}

#[derive(Clone)]
struct Command {
    target: Target,
    action: SystemTrayAction,
}

#[derive(Clone)]
struct QueryRequest {
    lifetime: Lifetime,
}

struct QueryResult {
    request: QueryRequest,
    result: Result<QueriedItem, String>,
}

type ItemQuery = LocalBoxFuture<'static, QueryResult>;

#[derive(Default)]
struct Shared {
    commands: VecDeque<Command>,
    targets: BTreeMap<String, Target>,
    online: bool,
    watcher_owned: bool,
    stop: bool,
    latest: Option<SystemTraySnapshot>,
}

/// a status notifier worker for discovery, metadata, and item actions.
///
/// snapshots retain icon names and report whether bounded pixmaps exist, but do
/// not retain their pixel data. dbusmenu layouts and legacy xembed items are not
/// handled here. action coordinates are `(0, 0)` because wayland clients do not
/// have global screen coordinates.
pub struct SystemTray {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl SystemTray {
    /// connect to the session bus in the requested ownership mode.
    pub fn start(mode: SystemTrayMode, ping: Ping) -> io::Result<Self> {
        Self::start_at(mode, None, ping)
    }

    fn start_at(mode: SystemTrayMode, address: Option<String>, ping: Ping) -> io::Result<Self> {
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("thing-system-tray".into())
            .spawn(move || {
                let mut previous = SystemTraySnapshot::default();
                let mut last_error = String::new();
                let mut next_item_id = 1_u64;
                // once an existing watcher wins the initial non-queueing request,
                // this process remains a host and does not race a later restart.
                let mut ownership_allowed = mode == SystemTrayMode::OwnWatcher;
                loop {
                    if lock(&state).stop {
                        break;
                    }
                    let result = async_io::block_on(session(
                        address.as_deref(),
                        &mut ownership_allowed,
                        &state,
                        &reader,
                        &mut next_item_id,
                        |snapshot| publish(&state, &ping, &mut previous, snapshot),
                    ));
                    {
                        let mut shared = lock(&state);
                        shared.online = false;
                        shared.watcher_owned = false;
                        shared.targets.clear();
                        shared.commands.clear();
                        if shared.stop {
                            break;
                        }
                    }
                    let error = sanitize(
                        result
                            .err()
                            .unwrap_or_else(|| "system tray connection ended".into()),
                        4096,
                    );
                    if error != last_error {
                        eprintln!("thing: system tray unavailable: {error}; retrying in 1 second");
                        last_error = error.clone();
                    }
                    publish(
                        &state,
                        &ping,
                        &mut previous,
                        SystemTraySnapshot {
                            error,
                            ..SystemTraySnapshot::default()
                        },
                    );
                    retry_wait(&reader, Duration::from_secs(1));
                }
            })?;
        Ok(Self {
            shared,
            wake,
            worker: Some(worker),
        })
    }

    /// take the newest complete state generation, if it changed.
    pub fn take_snapshot(&self) -> Option<SystemTraySnapshot> {
        lock(&self.shared).latest.take()
    }

    /// queue an action for one current item lifetime.
    pub fn send(&self, action: SystemTrayAction, item: &str) -> Result<(), &'static str> {
        let mut shared = lock(&self.shared);
        if !shared.online {
            return Err("system tray service is disconnected");
        }
        let target = shared
            .targets
            .get(item)
            .cloned()
            .ok_or("system tray item is no longer available")?;
        if shared.commands.len() >= MAX_COMMANDS {
            return Err("system tray command queue is full");
        }
        shared.commands.push_back(Command { target, action });
        drop(shared);
        wake(&self.wake);
        Ok(())
    }
}

impl Drop for SystemTray {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        wake(&self.wake);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn publish(
    shared: &Mutex<Shared>,
    ping: &Ping,
    previous: &mut SystemTraySnapshot,
    snapshot: SystemTraySnapshot,
) {
    if *previous != snapshot {
        *previous = snapshot.clone();
        lock(shared).latest = Some(snapshot);
        ping.ping();
    }
}

async fn session(
    address: Option<&str>,
    ownership_allowed: &mut bool,
    shared: &Mutex<Shared>,
    wake_stream: &UnixStream,
    next_item_id: &mut u64,
    mut publish: impl FnMut(SystemTraySnapshot),
) -> Result<(), String> {
    let reader = wake_stream.try_clone().map_err(|error| error.to_string())?;
    let wake_source = Async::new(reader).map_err(|error| error.to_string())?;
    let watcher_registry = Arc::new(Mutex::new(WatcherRegistry::default()));
    let kde_watcher = KdeWatcherObject {
        registry: watcher_registry.clone(),
    };
    let freedesktop_watcher = FreedesktopWatcherObject {
        registry: watcher_registry.clone(),
    };
    let builder = match address {
        Some(address) => zbus::connection::Builder::address(address),
        None => zbus::connection::Builder::session(),
    }
    .map_err(|error| error.to_string())?
    .method_timeout(METHOD_TIMEOUT)
    .serve_at(WATCHER_PATH, kde_watcher)
    .map_err(|error| error.to_string())?
    .serve_at(WATCHER_PATH, freedesktop_watcher)
    .map_err(|error| error.to_string())?;
    let connection = future::race(
        async {
            builder
                .build()
                .await
                .map(Some)
                .map_err(|error| error.to_string())
        },
        future::race(
            async {
                wake_source
                    .readable()
                    .await
                    .map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
                Ok::<Option<Connection>, String>(None)
            },
            async {
                Timer::after(METHOD_TIMEOUT).await;
                Err::<Option<Connection>, String>("system tray connection timed out".into())
            },
        ),
    )
    .await?;
    let Some(connection) = connection else {
        return if stopping(shared) {
            Ok(())
        } else {
            Err("system tray connection interrupted".into())
        };
    };
    if stopping(shared) {
        return Ok(());
    }

    let mut names = name_stream(&connection).await?;
    let mut watcher_events = watcher_stream(&connection).await?;
    let mut item_events = item_stream(&connection).await?;
    let dbus = DBusProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    let unique_name = connection
        .unique_name()
        .ok_or_else(|| "session bus did not assign a unique name".to_owned())?
        .to_string();
    let mut watcher_owners = query_watcher_owners(&dbus).await?;
    let mut watcher_owned = false;
    if *ownership_allowed && !watcher_owners.is_empty() {
        *ownership_allowed = false;
    }
    if *ownership_allowed {
        match acquire_watcher_names(&connection).await? {
            WatcherAcquisition::Owned => {
                watcher_owned = true;
                watcher_owners = WATCHER_PROTOCOLS
                    .into_iter()
                    .map(|protocol| (protocol, unique_name.clone()))
                    .collect();
            }
            WatcherAcquisition::Declined => {
                *ownership_allowed = false;
                watcher_owners = query_watcher_owners(&dbus).await?;
            }
        }
    }

    let host_names = request_host_names(&connection, &unique_name).await?;
    let mut owned_host_names = WATCHER_PROTOCOLS.into_iter().collect::<BTreeSet<_>>();
    let mut active_host = None;
    if watcher_owned {
        let mut registry = lock(&watcher_registry);
        for host_name in host_names.values() {
            registry.hosts.insert(
                host_name.clone(),
                RegisteredOwner {
                    owner: unique_name.clone(),
                },
            );
        }
    }

    let mut model = Model::default();
    let mut current_watcher = select_watcher(&watcher_owners);
    let mut attached_watcher = None;
    let mut queued = BTreeMap::new();
    let mut running = BTreeSet::new();
    let mut queries = FuturesUnordered::<ItemQuery>::new();

    loop {
        if stopping(shared) {
            return Ok(());
        }
        if current_watcher != attached_watcher {
            if !watcher_owned {
                let next_host = current_watcher.as_ref().map(|watcher| watcher.protocol);
                if active_host != next_host {
                    if let Some(protocol) = active_host {
                        connection
                            .release_name(host_names[&protocol].as_str())
                            .await
                            .map_err(|error| error.to_string())?;
                        owned_host_names.remove(&protocol);
                    }
                    if let Some(protocol) = next_host
                        && !owned_host_names.contains(&protocol)
                    {
                        if !request_name_without_queue(&connection, &host_names[&protocol]).await? {
                            return Err("cannot reacquire a status notifier host name".into());
                        }
                        owned_host_names.insert(protocol);
                    }
                    active_host = next_host;
                }
            }
            model.clear();
            queued.clear();
            running.clear();
            queries = FuturesUnordered::new();
            attached_watcher = current_watcher.clone();
            if let Some(watcher) = current_watcher.as_ref() {
                if !watcher_owned {
                    register_host(
                        &connection,
                        &watcher.owner,
                        watcher.protocol.interface,
                        &host_names[&watcher.protocol],
                    )
                    .await?;
                }
                let registrations =
                    watcher_items(&connection, &watcher.owner, watcher.protocol.interface).await?;
                for registration in registrations.into_iter().take(MAX_ITEMS) {
                    if let Some((endpoint, owner)) =
                        resolve_registration(&dbus, &registration, None).await?
                        && let Some(lifetime) =
                            model.register(endpoint, owner, next_item_id, MAX_ITEMS)?
                    {
                        queue_query(&mut queued, lifetime);
                    }
                }
            }
            publish(update_shared(
                shared,
                &model,
                current_watcher.is_some(),
                watcher_owned,
                String::new(),
            ));
        }
        start_queries(&connection, &mut queued, &mut running, &mut queries);

        let input = future::race(
            async { Input::Name(names.next().await) },
            future::race(
                async { Input::Watcher(watcher_events.next().await) },
                future::race(
                    async { Input::Item(item_events.next().await) },
                    future::race(
                        async {
                            let Some(result) = queries.next().await else {
                                return future::pending().await;
                            };
                            Input::Query(Box::new(result))
                        },
                        future::race(
                            async {
                                let command = { lock(shared).commands.pop_front() };
                                let Some(command) = command else {
                                    return future::pending().await;
                                };
                                Input::Command(command)
                            },
                            async { Input::Wake(wake_source.readable().await) },
                        ),
                    ),
                ),
            ),
        )
        .await;
        match input {
            Input::Wake(result) => {
                result.map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
            }
            Input::Name(Some(Ok(message))) => {
                let Some((name, old_owner, new_owner)) = name_event(&message) else {
                    continue;
                };
                if let Some(protocol) = watcher_protocol(&name) {
                    if watcher_owners.get(&protocol).map(String::as_str) == Some(old_owner.as_str())
                    {
                        watcher_owners.remove(&protocol);
                    }
                    if !new_owner.is_empty() {
                        watcher_owners.insert(protocol, new_owner.clone());
                    }
                    current_watcher = select_watcher(&watcher_owners);
                }
                if !old_owner.is_empty() {
                    let model_changed = model.remove_name(&name, &old_owner);
                    let (removed, host_removed) = if watcher_owned {
                        remove_registered_name(&watcher_registry, &name, &old_owner)
                    } else {
                        (Vec::new(), false)
                    };
                    for endpoint in removed {
                        emit_item_unregistered(&connection, &endpoint.key()).await?;
                    }
                    if host_removed {
                        emit_host_unregistered(&connection).await?;
                    }
                    if model_changed {
                        publish(update_shared(
                            shared,
                            &model,
                            current_watcher.is_some(),
                            watcher_owned,
                            String::new(),
                        ));
                    }
                }
            }
            Input::Name(Some(Err(error))) => return Err(error.to_string()),
            Input::Name(None) => return Err("system tray name subscription ended".into()),
            Input::Watcher(Some(Ok(message))) => {
                let Some(watcher) = current_watcher.as_ref() else {
                    continue;
                };
                if message.header().sender().map(ToString::to_string) != Some(watcher.owner.clone())
                    || message.header().interface().map(ToString::to_string)
                        != Some(watcher.protocol.interface.into())
                    || message.body().len() > MAX_SIGNAL_BYTES
                {
                    continue;
                }
                let Some(member) = message.header().member().map(ToString::to_string) else {
                    continue;
                };
                let Ok((registration,)) = message.body().deserialize::<(String,)>() else {
                    continue;
                };
                match member.as_str() {
                    "StatusNotifierItemRegistered" => {
                        if let Some((endpoint, owner)) =
                            resolve_registration(&dbus, &registration, None).await?
                            && let Some(lifetime) =
                                model.register(endpoint, owner, next_item_id, MAX_ITEMS)?
                        {
                            queue_query(&mut queued, lifetime);
                        }
                    }
                    "StatusNotifierItemUnregistered" => {
                        if let Ok(endpoint) = parse_endpoint(&registration, None)
                            && model.remove(&endpoint, None)
                        {
                            publish(update_shared(
                                shared,
                                &model,
                                current_watcher.is_some(),
                                watcher_owned,
                                String::new(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Input::Watcher(Some(Err(error))) => return Err(error.to_string()),
            Input::Watcher(None) => {
                return Err("status notifier watcher subscription ended".into());
            }
            Input::Item(Some(Ok(message))) => {
                queue_item_refreshes(&message, &model, &mut queued);
            }
            Input::Item(Some(Err(error))) => return Err(error.to_string()),
            Input::Item(None) => return Err("status notifier item subscription ended".into()),
            Input::Query(result) => {
                running.remove(&result.request.lifetime.id);
                if let Ok(queried) = result.result
                    && model.update(&result.request.lifetime, queried)
                {
                    publish(update_shared(
                        shared,
                        &model,
                        current_watcher.is_some(),
                        watcher_owned,
                        String::new(),
                    ));
                }
            }
            Input::Command(command) => {
                if model.contains_target(&command.target)
                    && !send_action_interruptible(&connection, command, shared, &wake_source)
                        .await?
                {
                    return Ok(());
                }
            }
        }
    }
}

enum Input {
    Name(Option<zbus::Result<Message>>),
    Watcher(Option<zbus::Result<Message>>),
    Item(Option<zbus::Result<Message>>),
    Query(Box<QueryResult>),
    Command(Command),
    Wake(io::Result<()>),
}

fn update_shared(
    shared: &Mutex<Shared>,
    model: &Model,
    available: bool,
    watcher_owned: bool,
    error: String,
) -> SystemTraySnapshot {
    let projection = model.projection(available, watcher_owned && available, error);
    let mut shared = lock(shared);
    shared.online = available;
    shared.watcher_owned = watcher_owned && available;
    shared.targets = projection.targets;
    projection.snapshot
}

fn queue_query(queued: &mut BTreeMap<String, QueryRequest>, lifetime: Lifetime) {
    if queued.contains_key(&lifetime.id) || queued.len() < MAX_ITEMS {
        queued.insert(lifetime.id.clone(), QueryRequest { lifetime });
    }
}

fn start_queries(
    connection: &Connection,
    queued: &mut BTreeMap<String, QueryRequest>,
    running: &mut BTreeSet<String>,
    queries: &mut FuturesUnordered<ItemQuery>,
) {
    while running.len() < MAX_QUERIES {
        let Some(id) = queued.keys().find(|id| !running.contains(*id)).cloned() else {
            break;
        };
        let request = queued.remove(&id).expect("queued tray query disappeared");
        running.insert(id);
        let connection = connection.clone();
        let query_request = request.clone();
        queries.push(
            async move {
                let result = query_item(&connection, &query_request.lifetime).await;
                QueryResult { request, result }
            }
            .boxed_local(),
        );
    }
}

fn queue_item_refreshes(
    message: &Message,
    model: &Model,
    queued: &mut BTreeMap<String, QueryRequest>,
) {
    if message.body().len() > MAX_PROPERTY_BYTES {
        return;
    }
    let Some(owner) = message.header().sender().map(ToString::to_string) else {
        return;
    };
    let Some(path) = message.header().path().map(ToString::to_string) else {
        return;
    };
    let interface = message.header().interface().map(ToString::to_string);
    let property_interface = (interface.as_deref() == Some(PROPERTIES_INTERFACE))
        .then(|| {
            message
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .ok()
                .map(|(interface, _, _)| interface)
        })
        .flatten();
    let relevant = matches!(
        interface.as_deref(),
        Some(KDE_ITEM_INTERFACE | FREEDESKTOP_ITEM_INTERFACE)
    ) || matches!(
        property_interface.as_deref(),
        Some(KDE_ITEM_INTERFACE | FREEDESKTOP_ITEM_INTERFACE)
    );
    if !relevant {
        return;
    }
    for lifetime in model
        .lifetimes_for_owner(&owner)
        .into_iter()
        .filter(|lifetime| lifetime.endpoint.path == path)
    {
        queue_query(queued, lifetime);
    }
}

async fn query_item(connection: &Connection, lifetime: &Lifetime) -> Result<QueriedItem, String> {
    let mut last_error = None;
    for interface in [KDE_ITEM_INTERFACE, FREEDESKTOP_ITEM_INTERFACE] {
        match get_all(
            connection,
            &lifetime.owner,
            &lifetime.endpoint.path,
            interface,
        )
        .await
        {
            Ok(mut values) => {
                let tooltip = take::<Tooltip>(&mut values, "ToolTip").unwrap_or_default();
                let icon_pixmaps =
                    take::<Vec<Pixmap>>(&mut values, "IconPixmap").unwrap_or_default();
                let attention_pixmaps =
                    take::<Vec<Pixmap>>(&mut values, "AttentionIconPixmap").unwrap_or_default();
                let overlay_pixmaps =
                    take::<Vec<Pixmap>>(&mut values, "OverlayIconPixmap").unwrap_or_default();
                let menu = take::<OwnedObjectPath>(&mut values, "Menu")
                    .map(|path| path.to_string())
                    .unwrap_or_default();
                return Ok(QueriedItem {
                    item: SystemTrayItem {
                        id: lifetime.id.clone(),
                        protocol_item_id: take_string(&mut values, "Id", 4096),
                        category: take_string(&mut values, "Category", 4096),
                        title: take_string(&mut values, "Title", 4096),
                        status: take_string(&mut values, "Status", 4096),
                        icon_name: take_string(&mut values, "IconName", 4096),
                        attention_icon_name: take_string(&mut values, "AttentionIconName", 4096),
                        overlay_icon_name: take_string(&mut values, "OverlayIconName", 4096),
                        tooltip_icon_name: sanitize(tooltip.0, 4096),
                        tooltip_title: sanitize(tooltip.2, 4096),
                        tooltip_text: sanitize(tooltip.3, 16 * 1024),
                        item_is_menu: take::<bool>(&mut values, "ItemIsMenu").unwrap_or(false),
                        has_menu: !menu.is_empty() && menu != "/" && menu != "/NO_DBUSMENU",
                        has_icon_pixmap: valid_pixmaps(&icon_pixmaps),
                        has_attention_icon_pixmap: valid_pixmaps(&attention_pixmaps),
                        has_overlay_icon_pixmap: valid_pixmaps(&overlay_pixmaps),
                        has_tooltip_icon_pixmap: valid_pixmaps(&tooltip.1),
                    },
                    interface: interface.into(),
                });
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "status notifier item has no supported interface".into()))
}

type Pixmap = (i32, i32, Vec<u8>);
type Tooltip = (String, Vec<Pixmap>, String, String);

fn valid_pixmaps(pixmaps: &[Pixmap]) -> bool {
    !pixmaps.is_empty()
        && pixmaps.len() <= MAX_PIXMAPS
        && pixmaps.iter().all(|(width, height, bytes)| {
            if *width <= 0 || *height <= 0 {
                return false;
            }
            let expected = usize::try_from(*width)
                .ok()
                .zip(usize::try_from(*height).ok())
                .and_then(|(width, height)| width.checked_mul(height))
                .and_then(|pixels| pixels.checked_mul(4));
            expected.is_some_and(|expected| expected == bytes.len() && expected <= MAX_PIXMAP_BYTES)
        })
}

async fn get_all(
    connection: &Connection,
    owner: &str,
    path: &str,
    interface: &str,
) -> Result<HashMap<String, OwnedValue>, String> {
    let message = connection
        .call_method(
            Some(owner),
            path,
            Some(PROPERTIES_INTERFACE),
            "GetAll",
            &(interface,),
        )
        .await
        .map_err(|error| error.to_string())?;
    let body = message.body();
    if body.len() > MAX_PROPERTY_BYTES {
        return Err("status notifier item properties exceed 512 KiB".into());
    }
    body.deserialize().map_err(|error| error.to_string())
}

fn take<T>(values: &mut HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    values.remove(key).and_then(|value| value.try_into().ok())
}

fn take_string(values: &mut HashMap<String, OwnedValue>, key: &str, max: usize) -> String {
    sanitize(take::<String>(values, key).unwrap_or_default(), max)
}

fn sanitize(mut value: String, max: usize) -> String {
    value.retain(|character| character != '\0');
    if value.len() > max {
        let mut boundary = max;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    value
}

async fn send_action(connection: &Connection, command: Command) -> Result<(), String> {
    let method = match command.action {
        SystemTrayAction::Activate => "Activate",
        SystemTrayAction::SecondaryActivate => "SecondaryActivate",
        SystemTrayAction::ContextMenu => "ContextMenu",
    };
    let message = Message::method_call(command.target.endpoint.path.as_str(), method)
        .and_then(|builder| builder.destination(command.target.owner.as_str()))
        .and_then(|builder| builder.interface(command.target.interface.as_str()))
        .and_then(|builder| builder.with_flags(Flags::NoReplyExpected))
        .and_then(|builder| builder.with_flags(Flags::NoAutoStart))
        .and_then(|builder| builder.build(&(0_i32, 0_i32)))
        .map_err(|error| error.to_string())?;
    connection
        .send(&message)
        .await
        .map_err(|error| error.to_string())
}

async fn send_action_interruptible(
    connection: &Connection,
    command: Command,
    shared: &Mutex<Shared>,
    wake_source: &Async<UnixStream>,
) -> Result<bool, String> {
    enum Progress {
        Sent(Result<(), String>),
        Wake(io::Result<()>),
        Timeout,
    }

    let send = send_action(connection, command);
    let timeout = Timer::after(METHOD_TIMEOUT);
    futures_lite::pin!(send);
    futures_lite::pin!(timeout);
    loop {
        let progress = future::race(
            async { Progress::Sent(send.as_mut().await) },
            future::race(
                async { Progress::Wake(wake_source.readable().await) },
                async {
                    timeout.as_mut().await;
                    Progress::Timeout
                },
            ),
        )
        .await;
        match progress {
            Progress::Sent(result) => return result.map(|()| true),
            Progress::Wake(result) => {
                result.map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
                if stopping(shared) {
                    return Ok(false);
                }
            }
            Progress::Timeout => return Err("system tray command timed out".into()),
        }
    }
}

async fn name_stream(connection: &Connection) -> Result<MessageStream, String> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(|error| error.to_string())?
        .interface("org.freedesktop.DBus")
        .map_err(|error| error.to_string())?
        .member("NameOwnerChanged")
        .map_err(|error| error.to_string())?
        .build();
    MessageStream::for_match_rule(rule, connection, Some(256))
        .await
        .map_err(|error| error.to_string())
}

async fn watcher_stream(
    connection: &Connection,
) -> Result<stream::SelectAll<MessageStream>, String> {
    let mut streams = Vec::new();
    for protocol in WATCHER_PROTOCOLS {
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .path(WATCHER_PATH)
            .map_err(|error| error.to_string())?
            .interface(protocol.interface)
            .map_err(|error| error.to_string())?
            .build();
        streams.push(
            MessageStream::for_match_rule(rule, connection, Some(256))
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(stream::select_all(streams))
}

async fn item_stream(connection: &Connection) -> Result<stream::SelectAll<MessageStream>, String> {
    let mut streams = Vec::new();
    for interface in [KDE_ITEM_INTERFACE, FREEDESKTOP_ITEM_INTERFACE] {
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(interface)
            .map_err(|error| error.to_string())?
            .build();
        streams.push(
            MessageStream::for_match_rule(rule, connection, Some(256))
                .await
                .map_err(|error| error.to_string())?,
        );
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(PROPERTIES_INTERFACE)
            .map_err(|error| error.to_string())?
            .member("PropertiesChanged")
            .map_err(|error| error.to_string())?
            .add_arg(interface)
            .map_err(|error| error.to_string())?
            .build();
        streams.push(
            MessageStream::for_match_rule(rule, connection, Some(256))
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(stream::select_all(streams))
}

fn name_event(message: &Message) -> Option<(String, String, String)> {
    if message.body().len() > MAX_SIGNAL_BYTES {
        return None;
    }
    message
        .body()
        .deserialize::<(String, String, String)>()
        .ok()
}

fn host_name(protocol: WatcherProtocol, unique_name: &str) -> String {
    let suffix = unique_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{}-{}-{suffix}", protocol.host_prefix, std::process::id())
}

fn watcher_protocol(name: &str) -> Option<WatcherProtocol> {
    WATCHER_PROTOCOLS
        .into_iter()
        .find(|protocol| protocol.name == name)
}

fn select_watcher(owners: &BTreeMap<WatcherProtocol, String>) -> Option<WatcherTarget> {
    WATCHER_PROTOCOLS.into_iter().find_map(|protocol| {
        owners.get(&protocol).map(|owner| WatcherTarget {
            protocol,
            owner: owner.clone(),
        })
    })
}

async fn query_watcher_owners(
    dbus: &DBusProxy<'_>,
) -> Result<BTreeMap<WatcherProtocol, String>, String> {
    let mut owners = BTreeMap::new();
    for protocol in WATCHER_PROTOCOLS {
        if let Some(owner) = get_name_owner(dbus, protocol.name).await? {
            owners.insert(protocol, owner);
        }
    }
    Ok(owners)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatcherAcquisition {
    Owned,
    Declined,
}

async fn acquire_watcher_names(connection: &Connection) -> Result<WatcherAcquisition, String> {
    let mut acquired = Vec::new();
    for protocol in WATCHER_PROTOCOLS {
        match request_name_without_queue(connection, protocol.name).await? {
            true => acquired.push(protocol.name),
            false => {
                for name in acquired.into_iter().rev() {
                    connection
                        .release_name(name)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                return Ok(WatcherAcquisition::Declined);
            }
        }
    }
    Ok(WatcherAcquisition::Owned)
}

async fn request_host_names(
    connection: &Connection,
    unique_name: &str,
) -> Result<BTreeMap<WatcherProtocol, String>, String> {
    let mut names = BTreeMap::new();
    for protocol in WATCHER_PROTOCOLS {
        let name = host_name(protocol, unique_name);
        if !request_name_without_queue(connection, &name).await? {
            return Err("cannot acquire a unique status notifier host name".into());
        }
        names.insert(protocol, name);
    }
    Ok(names)
}

async fn request_name_without_queue(connection: &Connection, name: &str) -> Result<bool, String> {
    match connection
        .request_name_with_flags(name, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Ok(true),
        Err(zbus::Error::NameTaken) | Ok(RequestNameReply::Exists) => Ok(false),
        Ok(RequestNameReply::InQueue) => {
            Err("status notifier name ownership was unexpectedly queued".into())
        }
        Err(error) => Err(error.to_string()),
    }
}

async fn get_name_owner(dbus: &DBusProxy<'_>, name: &str) -> Result<Option<String>, String> {
    let name = BusName::try_from(name).map_err(|error| error.to_string())?;
    match dbus.get_name_owner(name).await {
        Ok(owner) => Ok(Some(owner.to_string())),
        Err(zbus::fdo::Error::NameHasNoOwner(_)) | Err(zbus::fdo::Error::ServiceUnknown(_)) => {
            Ok(None)
        }
        Err(error) => Err(error.to_string()),
    }
}

async fn resolve_registration(
    dbus: &DBusProxy<'_>,
    registration: &str,
    sender: Option<&str>,
) -> Result<Option<(Endpoint, String)>, String> {
    let Ok(endpoint) = parse_endpoint(registration, sender) else {
        return Ok(None);
    };
    let owner = get_name_owner(dbus, &endpoint.service).await?;
    Ok(owner.map(|owner| (endpoint, owner)))
}

fn parse_endpoint(registration: &str, sender: Option<&str>) -> Result<Endpoint, String> {
    if registration.len() > MAX_ENDPOINT_BYTES {
        return Err("status notifier item endpoint exceeds 4096 bytes".into());
    }
    let (service, path) = if registration.starts_with('/') {
        (
            sender.ok_or_else(|| "item path registration has no sender".to_owned())?,
            registration,
        )
    } else if let Some(index) = registration.find('/') {
        (&registration[..index], &registration[index..])
    } else {
        (registration, DEFAULT_ITEM_PATH)
    };
    if service
        .len()
        .checked_add(path.len())
        .is_none_or(|length| length > MAX_ENDPOINT_BYTES)
    {
        return Err("status notifier item endpoint exceeds 4096 bytes".into());
    }
    BusName::try_from(service).map_err(|error| error.to_string())?;
    OwnedObjectPath::try_from(path).map_err(|error| error.to_string())?;
    Ok(Endpoint {
        service: service.into(),
        path: path.into(),
    })
}

async fn register_host(
    connection: &Connection,
    watcher_owner: &str,
    watcher_interface: &str,
    host_name: &str,
) -> Result<(), String> {
    connection
        .call_method(
            Some(watcher_owner),
            WATCHER_PATH,
            Some(watcher_interface),
            "RegisterStatusNotifierHost",
            &(host_name,),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn watcher_items(
    connection: &Connection,
    owner: &str,
    watcher_interface: &str,
) -> Result<Vec<String>, String> {
    let values = get_all(connection, owner, WATCHER_PATH, watcher_interface).await?;
    let items = values
        .into_iter()
        .find_map(|(name, value)| {
            (name == "RegisteredStatusNotifierItems")
                .then(|| Vec::<String>::try_from(value).ok())
                .flatten()
        })
        .unwrap_or_default();
    Ok(items.into_iter().take(MAX_ITEMS).collect())
}

#[derive(Clone)]
struct RegisteredOwner {
    owner: String,
}

#[derive(Default)]
struct WatcherRegistry {
    items: BTreeMap<Endpoint, RegisteredOwner>,
    hosts: BTreeMap<String, RegisteredOwner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistryChange {
    Unchanged,
    Inserted,
    Replaced,
}

impl WatcherRegistry {
    fn register_item(
        &mut self,
        endpoint: Endpoint,
        owner: String,
    ) -> Result<RegistryChange, zbus::fdo::Error> {
        if self
            .items
            .get(&endpoint)
            .is_some_and(|registered| registered.owner == owner)
        {
            return Ok(RegistryChange::Unchanged);
        }
        if !self.items.contains_key(&endpoint) && self.items.len() >= MAX_ITEMS {
            return Err(zbus::fdo::Error::LimitsExceeded(
                "too many status notifier items".into(),
            ));
        }
        let replaced = self
            .items
            .insert(endpoint, RegisteredOwner { owner })
            .is_some();
        Ok(if replaced {
            RegistryChange::Replaced
        } else {
            RegistryChange::Inserted
        })
    }

    fn register_host(
        &mut self,
        service: String,
        owner: String,
    ) -> Result<RegistryChange, zbus::fdo::Error> {
        if self
            .hosts
            .get(&service)
            .is_some_and(|registered| registered.owner == owner)
        {
            return Ok(RegistryChange::Unchanged);
        }
        if !self.hosts.contains_key(&service) && self.hosts.len() >= MAX_HOSTS {
            return Err(zbus::fdo::Error::LimitsExceeded(
                "too many status notifier hosts".into(),
            ));
        }
        let replaced = self
            .hosts
            .insert(service, RegisteredOwner { owner })
            .is_some();
        Ok(if replaced {
            RegistryChange::Replaced
        } else {
            RegistryChange::Inserted
        })
    }
}

async fn register_watcher_item(
    registry: &Mutex<WatcherRegistry>,
    service_or_path: String,
    header: Header<'_>,
    connection: &Connection,
) -> zbus::fdo::Result<()> {
    let sender = header
        .sender()
        .ok_or_else(|| zbus::fdo::Error::InvalidArgs("missing method sender".into()))?
        .to_string();
    let endpoint =
        parse_endpoint(&service_or_path, Some(&sender)).map_err(zbus::fdo::Error::InvalidArgs)?;
    let dbus = DBusProxy::new(connection).await?;
    let owner = get_name_owner(&dbus, &endpoint.service)
        .await
        .map_err(zbus::fdo::Error::Failed)?
        .ok_or_else(|| zbus::fdo::Error::InvalidArgs("item service has no owner".into()))?;
    let key = endpoint.key();
    let change = lock(registry).register_item(endpoint, owner)?;
    if change == RegistryChange::Replaced {
        emit_item_unregistered(connection, &key)
            .await
            .map_err(zbus::fdo::Error::Failed)?;
    }
    if change != RegistryChange::Unchanged {
        emit_item_registered(connection, &key)
            .await
            .map_err(zbus::fdo::Error::Failed)?;
    }
    Ok(())
}

async fn register_watcher_host(
    registry: &Mutex<WatcherRegistry>,
    service: String,
    connection: &Connection,
) -> zbus::fdo::Result<()> {
    if service.len() > MAX_ENDPOINT_BYTES {
        return Err(zbus::fdo::Error::InvalidArgs(
            "status notifier host name exceeds 4096 bytes".into(),
        ));
    }
    BusName::try_from(service.as_str())
        .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
    let dbus = DBusProxy::new(connection).await?;
    let owner = get_name_owner(&dbus, &service)
        .await
        .map_err(zbus::fdo::Error::Failed)?
        .ok_or_else(|| zbus::fdo::Error::InvalidArgs("host service has no owner".into()))?;
    let change = lock(registry).register_host(service, owner)?;
    if change == RegistryChange::Replaced {
        emit_host_unregistered(connection)
            .await
            .map_err(zbus::fdo::Error::Failed)?;
    }
    if change != RegistryChange::Unchanged {
        emit_host_registered(connection)
            .await
            .map_err(zbus::fdo::Error::Failed)?;
    }
    Ok(())
}

macro_rules! watcher_object {
    ($type:ident, $interface:literal) => {
        pub(super) struct $type {
            pub(super) registry: Arc<Mutex<WatcherRegistry>>,
        }

        // these declarations keep signals in introspection; emission is mirrored
        // through the generated associated functions below.
        #[zbus::interface(name = $interface)]
        impl $type {
            async fn register_status_notifier_item(
                &self,
                service_or_path: String,
                #[zbus(header)] header: Header<'_>,
                #[zbus(connection)] connection: &Connection,
            ) -> zbus::fdo::Result<()> {
                register_watcher_item(&self.registry, service_or_path, header, connection).await
            }

            async fn register_status_notifier_host(
                &self,
                service: String,
                #[zbus(connection)] connection: &Connection,
            ) -> zbus::fdo::Result<()> {
                register_watcher_host(&self.registry, service, connection).await
            }

            #[zbus(property)]
            fn registered_status_notifier_items(&self) -> Vec<String> {
                lock(&self.registry)
                    .items
                    .keys()
                    .map(Endpoint::key)
                    .collect()
            }

            #[zbus(property)]
            fn is_status_notifier_host_registered(&self) -> bool {
                !lock(&self.registry).hosts.is_empty()
            }

            #[zbus(property)]
            fn protocol_version(&self) -> i32 {
                0
            }

            #[zbus(signal)]
            pub(super) async fn status_notifier_item_registered(
                emitter: &SignalEmitter<'_>,
                service: &str,
            ) -> zbus::Result<()>;

            #[zbus(signal)]
            pub(super) async fn status_notifier_item_unregistered(
                emitter: &SignalEmitter<'_>,
                service: &str,
            ) -> zbus::Result<()>;

            #[zbus(signal)]
            pub(super) async fn status_notifier_host_registered(
                emitter: &SignalEmitter<'_>,
            ) -> zbus::Result<()>;

            #[zbus(signal)]
            pub(super) async fn status_notifier_host_unregistered(
                emitter: &SignalEmitter<'_>,
            ) -> zbus::Result<()>;
        }
    };
}

// zbus generates signal traits for introspection in addition to the associated
// emitters used below, so the trait methods themselves have no Rust caller.
#[allow(dead_code)]
mod watcher_interfaces {
    use super::*;

    watcher_object!(KdeWatcherObject, "org.kde.StatusNotifierWatcher");
    watcher_object!(
        FreedesktopWatcherObject,
        "org.freedesktop.StatusNotifierWatcher"
    );
}

use watcher_interfaces::{FreedesktopWatcherObject, KdeWatcherObject};

async fn emit_item_registered(connection: &Connection, service: &str) -> Result<(), String> {
    let emitter =
        SignalEmitter::new(connection, WATCHER_PATH).map_err(|error| error.to_string())?;
    KdeWatcherObject::status_notifier_item_registered(&emitter, service)
        .await
        .map_err(|error| error.to_string())?;
    FreedesktopWatcherObject::status_notifier_item_registered(&emitter, service)
        .await
        .map_err(|error| error.to_string())
}

async fn emit_item_unregistered(connection: &Connection, service: &str) -> Result<(), String> {
    let emitter =
        SignalEmitter::new(connection, WATCHER_PATH).map_err(|error| error.to_string())?;
    KdeWatcherObject::status_notifier_item_unregistered(&emitter, service)
        .await
        .map_err(|error| error.to_string())?;
    FreedesktopWatcherObject::status_notifier_item_unregistered(&emitter, service)
        .await
        .map_err(|error| error.to_string())
}

async fn emit_host_registered(connection: &Connection) -> Result<(), String> {
    let emitter =
        SignalEmitter::new(connection, WATCHER_PATH).map_err(|error| error.to_string())?;
    KdeWatcherObject::status_notifier_host_registered(&emitter)
        .await
        .map_err(|error| error.to_string())?;
    FreedesktopWatcherObject::status_notifier_host_registered(&emitter)
        .await
        .map_err(|error| error.to_string())
}

async fn emit_host_unregistered(connection: &Connection) -> Result<(), String> {
    let emitter =
        SignalEmitter::new(connection, WATCHER_PATH).map_err(|error| error.to_string())?;
    KdeWatcherObject::status_notifier_host_unregistered(&emitter)
        .await
        .map_err(|error| error.to_string())?;
    FreedesktopWatcherObject::status_notifier_host_unregistered(&emitter)
        .await
        .map_err(|error| error.to_string())
}

fn remove_registered_name(
    registry: &Mutex<WatcherRegistry>,
    name: &str,
    owner: &str,
) -> (Vec<Endpoint>, bool) {
    let mut registry = lock(registry);
    let items = registry
        .items
        .iter()
        .filter(|(endpoint, registered)| {
            registered.owner == owner && (name == owner || endpoint.service == name)
        })
        .map(|(endpoint, _)| endpoint.clone())
        .collect::<Vec<_>>();
    registry.items.retain(|endpoint, registered| {
        registered.owner != owner || (name != owner && endpoint.service != name)
    });
    let previous_host_count = registry.hosts.len();
    registry.hosts.retain(|service, registered| {
        registered.owner != owner || (name != owner && service != name)
    });
    (items, registry.hosts.len() != previous_host_count)
}

fn wake(mut stream: &UnixStream) {
    loop {
        match stream.write(&[1]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            _ => break,
        }
    }
}

fn drain(mut stream: &UnixStream) {
    let mut bytes = [0; 256];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn retry_wait(stream: &UnixStream, timeout: Duration) {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
    loop {
        // SAFETY: the descriptor points to one initialized pollfd for this call.
        let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if result >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
    drain(stream);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stopping(shared: &Mutex<Shared>) -> bool {
    lock(shared).stop
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command as ProcessCommand, Stdio},
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Instant,
    };

    use tempfile::TempDir;

    use super::*;

    const ITEM_NAME: &str = "org.example.ThingTrayItem";
    const FREEDESKTOP_ITEM_NAME: &str = "org.example.ThingFreedesktopTrayItem";
    const STALLED_ITEM_NAME: &str = "org.example.ThingStalledTrayItem";
    const ITEM_PATH: &str = "/StatusNotifierItem";

    #[test]
    fn endpoint_parser_supports_service_path_and_caller_path_forms() {
        assert_eq!(
            parse_endpoint(ITEM_NAME, None).unwrap(),
            Endpoint {
                service: ITEM_NAME.into(),
                path: ITEM_PATH.into(),
            }
        );
        assert_eq!(
            parse_endpoint(&format!("{ITEM_NAME}/custom/item"), None).unwrap(),
            Endpoint {
                service: ITEM_NAME.into(),
                path: "/custom/item".into(),
            }
        );
        assert_eq!(
            parse_endpoint("/custom/item", Some(":1.42")).unwrap(),
            Endpoint {
                service: ":1.42".into(),
                path: "/custom/item".into(),
            }
        );
        assert!(parse_endpoint("/custom/item", None).is_err());
        assert!(parse_endpoint("not a bus name", None).is_err());
        assert!(parse_endpoint("org.example.Item/not a path", None).is_err());
        let oversized_path = format!("/{}", "a".repeat(MAX_ENDPOINT_BYTES));
        assert!(parse_endpoint(&oversized_path, Some(":1.42")).is_err());
    }

    #[test]
    fn pixmap_presence_requires_bounded_complete_argb_frames() {
        assert!(valid_pixmaps(&[(1, 1, vec![0, 1, 2, 3])]));
        assert!(!valid_pixmaps(&[(0, 0, Vec::new())]));
        assert!(!valid_pixmaps(&[(2, 2, vec![0; 15])]));
        assert!(!valid_pixmaps(&[(257, 257, vec![0; 257 * 257 * 4])]));
        assert!(!valid_pixmaps(&vec![(1, 1, vec![0; 4]); MAX_PIXMAPS + 1]));
    }

    #[test]
    fn retained_error_text_is_bounded_on_a_utf8_boundary() {
        let error = sanitize(format!("{}\0", "é".repeat(4096)), 4096);
        assert_eq!(error.len(), 4096);
        assert!(!error.contains('\0'));
    }

    #[test]
    fn host_owner_replacement_survives_late_cleanup_for_the_previous_owner() {
        let registry = Mutex::new(WatcherRegistry::default());
        assert_eq!(
            lock(&registry)
                .register_host("org.example.Host".into(), ":1.10".into())
                .unwrap(),
            RegistryChange::Inserted
        );
        assert_eq!(
            lock(&registry)
                .register_host("org.example.Host".into(), ":1.11".into())
                .unwrap(),
            RegistryChange::Replaced
        );

        let (items, host_removed) = remove_registered_name(&registry, "org.example.Host", ":1.10");
        assert!(items.is_empty());
        assert!(!host_removed);
        assert_eq!(lock(&registry).hosts["org.example.Host"].owner, ":1.11");
    }

    #[test]
    fn private_bus_covers_owned_watcher_lifetimes_updates_and_actions() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();

        let empty = wait_for(&tray, |snapshot| snapshot.available);
        assert!(empty.watcher_owned);
        assert!(empty.items.is_empty());

        let state = Arc::new(Mutex::new(FakeItemState::default()));
        let calls = Arc::new(FakeCalls::default());
        let item_connection = fake_item(&address, state.clone(), calls.clone());
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);

        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let first_id = snapshot.items[0].id.clone();
        assert_eq!(snapshot.items[0].protocol_item_id, "thing-item");
        assert_eq!(snapshot.items[0].category, "Communications");
        assert_eq!(snapshot.items[0].title, "first title");
        assert_eq!(snapshot.items[0].status, "NeedsAttention");
        assert_eq!(snapshot.items[0].icon_name, "thing-item");
        assert_eq!(
            snapshot.items[0].attention_icon_name,
            "thing-item-attention"
        );
        assert_eq!(snapshot.items[0].overlay_icon_name, "thing-item-overlay");
        assert_eq!(snapshot.items[0].tooltip_icon_name, "dialog-information");
        assert_eq!(snapshot.items[0].tooltip_title, "tooltip title");
        assert_eq!(snapshot.items[0].tooltip_text, "tooltip text");
        assert!(!snapshot.items[0].item_is_menu);
        assert!(snapshot.items[0].has_menu);
        assert!(snapshot.items[0].has_icon_pixmap);
        assert!(!snapshot.items[0].has_attention_icon_pixmap);
        assert!(snapshot.items[0].has_overlay_icon_pixmap);
        assert!(snapshot.items[0].has_tooltip_icon_pixmap);

        tray.send(SystemTrayAction::Activate, &first_id).unwrap();
        tray.send(SystemTrayAction::SecondaryActivate, &first_id)
            .unwrap();
        tray.send(SystemTrayAction::ContextMenu, &first_id).unwrap();
        wait_until(|| {
            calls.activate.load(Ordering::Acquire) == 1
                && calls.secondary.load(Ordering::Acquire) == 1
                && calls.context.load(Ordering::Acquire) == 1
        });
        assert_eq!(*lock(&calls.coordinates), vec![(0, 0); 3]);

        state.lock().unwrap().title = "updated title".into();
        emit_item_signal(&item_connection, "NewTitle", &());
        let updated = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .is_some_and(|item| item.title == "updated title")
        });
        assert_eq!(updated.items[0].id, first_id);

        let unrelated_name = "org.example.ThingUnrelatedName";
        async_io::block_on(
            item_connection
                .request_name_with_flags(unrelated_name, RequestNameFlags::DoNotQueue.into()),
        )
        .unwrap();
        assert!(async_io::block_on(item_connection.release_name(unrelated_name)).unwrap());
        state.lock().unwrap().title = "after unrelated release".into();
        emit_item_signal(&item_connection, "NewTitle", &());
        let updated = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .is_some_and(|item| item.title == "after unrelated release")
        });
        assert_eq!(updated.items[0].id, first_id);

        assert!(async_io::block_on(item_connection.release_name(ITEM_NAME)).unwrap());
        wait_for(&tray, |snapshot| {
            snapshot.available && snapshot.items.is_empty()
        });
        assert_eq!(
            tray.send(SystemTrayAction::Activate, &first_id),
            Err("system tray item is no longer available")
        );

        async_io::block_on(
            item_connection.request_name_with_flags(ITEM_NAME, RequestNameFlags::DoNotQueue.into()),
        )
        .unwrap();
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        let replacement = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        assert_ne!(replacement.items[0].id, first_id);

        drop(tray);
        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn private_bus_supports_the_freedesktop_item_interface() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);

        let calls = Arc::new(FakeCalls::default());
        let item_connection = freedesktop_item(&address, calls.clone());
        register_item(
            &item_connection,
            FREEDESKTOP_PROTOCOL,
            FREEDESKTOP_ITEM_NAME,
        );
        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let item = &snapshot.items[0];
        assert_eq!(item.protocol_item_id, "freedesktop-item");
        assert_eq!(item.title, "freedesktop title");

        tray.send(SystemTrayAction::Activate, &item.id).unwrap();
        wait_until(|| calls.activate.load(Ordering::Acquire) == 1);

        drop(tray);
        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn own_watcher_mode_never_replaces_an_existing_watcher() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let registry = Arc::new(Mutex::new(WatcherRegistry::default()));
        let external = external_watcher(&address, KDE_PROTOCOL, registry.clone());
        let external_owner = external.unique_name().unwrap().to_string();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();

        let snapshot = wait_for(&tray, |snapshot| snapshot.available);
        assert!(!snapshot.watcher_owned);
        wait_until(|| !lock(&registry).hosts.is_empty());
        let observer = async_io::block_on(
            zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build(),
        )
        .unwrap();
        let dbus = async_io::block_on(DBusProxy::new(&observer)).unwrap();
        assert_eq!(
            async_io::block_on(get_name_owner(&dbus, KDE_WATCHER_NAME)).unwrap(),
            Some(external_owner)
        );

        drop(external);
        wait_for(&tray, |snapshot| !snapshot.available);
        thread::sleep(Duration::from_millis(150));
        assert_eq!(
            async_io::block_on(get_name_owner(&dbus, KDE_WATCHER_NAME)).unwrap(),
            None
        );

        let replacement = external_watcher(
            &address,
            KDE_PROTOCOL,
            Arc::new(Mutex::new(WatcherRegistry::default())),
        );
        let replacement_owner = replacement.unique_name().unwrap().to_string();
        let snapshot = wait_for(&tray, |snapshot| snapshot.available);
        assert!(!snapshot.watcher_owned);
        assert_eq!(
            async_io::block_on(get_name_owner(&dbus, KDE_WATCHER_NAME)).unwrap(),
            Some(replacement_owner)
        );

        drop(tray);
        drop(observer);
        drop(replacement);
        bus.stop();
    }

    #[test]
    fn own_watcher_mode_follows_a_freedesktop_watcher_without_splitting_names() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let registry = Arc::new(Mutex::new(WatcherRegistry::default()));
        let watcher = external_watcher(&address, FREEDESKTOP_PROTOCOL, registry.clone());
        let expected_owner = watcher.unique_name().unwrap().to_string();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();

        let snapshot = wait_for(&tray, |snapshot| snapshot.available);
        assert!(!snapshot.watcher_owned);
        wait_until(|| {
            lock(&registry)
                .hosts
                .keys()
                .any(|name| name.starts_with(FREEDESKTOP_HOST_PREFIX))
        });
        let freedesktop_host_name = lock(&registry).hosts.keys().next().unwrap().clone();
        let observer = async_io::block_on(
            zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build(),
        )
        .unwrap();
        let dbus = async_io::block_on(DBusProxy::new(&observer)).unwrap();
        assert_eq!(
            async_io::block_on(get_name_owner(&dbus, FREEDESKTOP_WATCHER_NAME)).unwrap(),
            Some(expected_owner)
        );
        assert_eq!(
            async_io::block_on(get_name_owner(&dbus, KDE_WATCHER_NAME)).unwrap(),
            None
        );

        let kde_registry = Arc::new(Mutex::new(WatcherRegistry::default()));
        let kde_watcher = external_watcher(&address, KDE_PROTOCOL, kde_registry.clone());
        wait_until(|| !lock(&kde_registry).hosts.is_empty());
        wait_until(|| {
            async_io::block_on(get_name_owner(&dbus, &freedesktop_host_name))
                .unwrap()
                .is_none()
        });

        drop(tray);
        drop(observer);
        drop(kde_watcher);
        drop(watcher);
        bus.stop();
    }

    #[test]
    fn host_uses_one_watcher_and_retires_targets_when_falling_back_to_the_other() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let kde_registry = Arc::new(Mutex::new(WatcherRegistry::default()));
        let freedesktop_registry = Arc::new(Mutex::new(WatcherRegistry::default()));
        let kde = external_watcher(&address, KDE_PROTOCOL, kde_registry.clone());
        let freedesktop =
            external_watcher(&address, FREEDESKTOP_PROTOCOL, freedesktop_registry.clone());
        let calls = Arc::new(FakeCalls::default());
        let item_connection = fake_item(
            &address,
            Arc::new(Mutex::new(FakeItemState::default())),
            calls.clone(),
        );
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        register_item(&item_connection, FREEDESKTOP_PROTOCOL, ITEM_NAME);

        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray = SystemTray::start_at(SystemTrayMode::Host, Some(address.clone()), ping).unwrap();
        let first = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let first_id = first.items[0].id.clone();
        wait_until(|| !lock(&kde_registry).hosts.is_empty());
        assert!(lock(&freedesktop_registry).hosts.is_empty());
        tray.send(SystemTrayAction::Activate, &first_id).unwrap();
        wait_until(|| calls.activate.load(Ordering::Acquire) == 1);

        drop(kde);
        let second = wait_for(&tray, |snapshot| {
            snapshot.items.len() == 1 && snapshot.items[0].id != first_id
        });
        let second_id = second.items[0].id.clone();
        wait_until(|| !lock(&freedesktop_registry).hosts.is_empty());
        assert_eq!(
            tray.send(SystemTrayAction::Activate, &first_id),
            Err("system tray item is no longer available")
        );
        tray.send(SystemTrayAction::Activate, &second_id).unwrap();
        wait_until(|| calls.activate.load(Ordering::Acquire) == 2);

        drop(tray);
        drop(item_connection);
        drop(freedesktop);
        bus.stop();
    }

    #[test]
    fn second_watcher_name_race_releases_partial_ownership() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let external = external_watcher(
            &address,
            FREEDESKTOP_PROTOCOL,
            Arc::new(Mutex::new(WatcherRegistry::default())),
        );
        let expected_owner = external.unique_name().unwrap().to_string();
        let contender = async_io::block_on(
            zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build(),
        )
        .unwrap();

        assert_eq!(
            async_io::block_on(acquire_watcher_names(&contender)).unwrap(),
            WatcherAcquisition::Declined
        );
        let dbus = async_io::block_on(DBusProxy::new(&contender)).unwrap();
        assert_eq!(
            async_io::block_on(get_name_owner(&dbus, KDE_WATCHER_NAME)).unwrap(),
            None
        );
        assert_eq!(
            async_io::block_on(get_name_owner(&dbus, FREEDESKTOP_WATCHER_NAME)).unwrap(),
            Some(expected_owner)
        );

        drop(contender);
        drop(external);
        bus.stop();
    }

    #[test]
    fn malformed_external_registrations_are_isolated_and_bounded() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let state = Arc::new(Mutex::new(FakeItemState::default()));
        let item_connection = fake_item(&address, state.clone(), Arc::new(FakeCalls::default()));
        let watcher = listed_watcher(&address, vec!["not a bus name".into(), ITEM_NAME.into()]);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray = SystemTray::start_at(SystemTrayMode::Host, Some(address.clone()), ping).unwrap();
        let first = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        assert_eq!(first.items[0].protocol_item_id, "thing-item");

        emit_watcher_registration(&watcher, KDE_PROTOCOL, "still not a bus name");
        emit_watcher_registration(
            &watcher,
            KDE_PROTOCOL,
            &format!("org.example.{}", "a".repeat(MAX_SIGNAL_BYTES)),
        );
        state.lock().unwrap().title = "still connected".into();
        emit_item_signal(&item_connection, "NewTitle", &());
        let updated = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .is_some_and(|item| item.title == "still connected")
        });
        assert!(updated.available);

        drop(tray);
        drop(watcher);
        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn dropping_the_worker_interrupts_a_stalled_item_query() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);
        let started = Arc::new(AtomicBool::new(false));
        let item_connection = stalled_item(&address, started.clone());
        register_item(&item_connection, KDE_PROTOCOL, STALLED_ITEM_NAME);
        wait_until(|| started.load(Ordering::Acquire));

        let before = Instant::now();
        drop(tray);
        assert!(before.elapsed() < Duration::from_millis(500));

        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn host_mode_waits_for_a_watcher_without_claiming_its_name() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray = SystemTray::start_at(SystemTrayMode::Host, Some(address.clone()), ping).unwrap();
        thread::sleep(Duration::from_millis(150));
        assert!(!lock(&tray.shared).online);

        let registry = Arc::new(Mutex::new(WatcherRegistry::default()));
        let watcher = external_watcher(&address, KDE_PROTOCOL, registry.clone());
        let snapshot = wait_for(&tray, |snapshot| snapshot.available);
        assert!(!snapshot.watcher_owned);
        wait_until(|| !lock(&registry).hosts.is_empty());

        drop(watcher);
        drop(tray);
        bus.stop();
    }

    struct Bus {
        child: Option<Child>,
    }

    impl Bus {
        fn start(address: &str) -> Self {
            let mut child = ProcessCommand::new("dbus-daemon")
                .args([
                    "--session",
                    "--nofork",
                    "--nopidfile",
                    "--print-address=1",
                    &format!("--address={address}"),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("dbus-daemon must be installed for system tray tests");
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let mut ready = String::new();
            output.read_line(&mut ready).unwrap();
            assert!(ready.trim().starts_with(address));
            Self { child: Some(child) }
        }

        fn stop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            self.stop();
        }
    }

    struct FakeItemState {
        title: String,
    }

    impl Default for FakeItemState {
        fn default() -> Self {
            Self {
                title: "first title".into(),
            }
        }
    }

    #[derive(Default)]
    struct FakeCalls {
        activate: AtomicUsize,
        secondary: AtomicUsize,
        context: AtomicUsize,
        coordinates: Mutex<Vec<(i32, i32)>>,
    }

    struct FakeItem {
        state: Arc<Mutex<FakeItemState>>,
        calls: Arc<FakeCalls>,
    }

    struct FakeFreedesktopItem {
        calls: Arc<FakeCalls>,
    }

    struct StalledItem {
        started: Arc<AtomicBool>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl StalledItem {
        #[zbus(property)]
        async fn title(&self) -> String {
            self.started.store(true, Ordering::Release);
            future::pending().await
        }
    }

    struct ListedWatcher {
        registrations: Vec<String>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl ListedWatcher {
        fn register_status_notifier_host(&self, _service: String) {}

        #[zbus(property)]
        fn registered_status_notifier_items(&self) -> Vec<String> {
            self.registrations.clone()
        }

        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn protocol_version(&self) -> i32 {
            0
        }
    }

    #[zbus::interface(name = "org.freedesktop.StatusNotifierItem")]
    impl FakeFreedesktopItem {
        fn activate(&self, x: i32, y: i32) {
            self.calls.activate.fetch_add(1, Ordering::Release);
            lock(&self.calls.coordinates).push((x, y));
        }

        #[zbus(property)]
        fn id(&self) -> &str {
            "freedesktop-item"
        }

        #[zbus(property)]
        fn category(&self) -> &str {
            "SystemServices"
        }

        #[zbus(property)]
        fn title(&self) -> &str {
            "freedesktop title"
        }

        #[zbus(property)]
        fn status(&self) -> &str {
            "Active"
        }

        #[zbus(property)]
        fn icon_name(&self) -> &str {
            "freedesktop-item"
        }
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl FakeItem {
        fn activate(&self, x: i32, y: i32) {
            self.calls.activate.fetch_add(1, Ordering::Release);
            lock(&self.calls.coordinates).push((x, y));
        }

        fn secondary_activate(&self, x: i32, y: i32) {
            self.calls.secondary.fetch_add(1, Ordering::Release);
            lock(&self.calls.coordinates).push((x, y));
        }

        fn context_menu(&self, x: i32, y: i32) {
            self.calls.context.fetch_add(1, Ordering::Release);
            lock(&self.calls.coordinates).push((x, y));
        }

        #[zbus(property)]
        fn id(&self) -> &str {
            "thing-item"
        }

        #[zbus(property)]
        fn category(&self) -> &str {
            "Communications"
        }

        #[zbus(property)]
        fn title(&self) -> String {
            lock(&self.state).title.clone()
        }

        #[zbus(property)]
        fn status(&self) -> &str {
            "NeedsAttention"
        }

        #[zbus(property)]
        fn icon_name(&self) -> &str {
            "thing-item"
        }

        #[zbus(property)]
        fn attention_icon_name(&self) -> &str {
            "thing-item-attention"
        }

        #[zbus(property)]
        fn overlay_icon_name(&self) -> &str {
            "thing-item-overlay"
        }

        #[zbus(property)]
        fn icon_pixmap(&self) -> Vec<Pixmap> {
            vec![(1, 1, vec![255, 1, 2, 3])]
        }

        #[zbus(property)]
        fn attention_icon_pixmap(&self) -> Vec<Pixmap> {
            vec![(2, 2, vec![0; 15])]
        }

        #[zbus(property)]
        fn overlay_icon_pixmap(&self) -> Vec<Pixmap> {
            vec![(1, 1, vec![255, 4, 5, 6])]
        }

        #[zbus(property)]
        fn tool_tip(&self) -> Tooltip {
            (
                "dialog-information".into(),
                vec![(1, 1, vec![255, 7, 8, 9])],
                "tooltip title".into(),
                "tooltip text".into(),
            )
        }

        #[zbus(property)]
        fn item_is_menu(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from("/Menu").unwrap()
        }
    }

    fn external_watcher(
        address: &str,
        protocol: WatcherProtocol,
        registry: Arc<Mutex<WatcherRegistry>>,
    ) -> Connection {
        async_io::block_on(async {
            let builder = zbus::connection::Builder::address(address)?.name(protocol.name)?;
            let builder = if protocol == KDE_PROTOCOL {
                builder.serve_at(WATCHER_PATH, KdeWatcherObject { registry })?
            } else {
                builder.serve_at(WATCHER_PATH, FreedesktopWatcherObject { registry })?
            };
            builder.build().await
        })
        .unwrap()
    }

    fn listed_watcher(address: &str, registrations: Vec<String>) -> Connection {
        async_io::block_on(async {
            zbus::connection::Builder::address(address)?
                .name(KDE_WATCHER_NAME)?
                .serve_at(WATCHER_PATH, ListedWatcher { registrations })?
                .build()
                .await
        })
        .unwrap()
    }

    fn fake_item(
        address: &str,
        state: Arc<Mutex<FakeItemState>>,
        calls: Arc<FakeCalls>,
    ) -> Connection {
        async_io::block_on(async {
            zbus::connection::Builder::address(address)?
                .name(ITEM_NAME)?
                .serve_at(ITEM_PATH, FakeItem { state, calls })?
                .build()
                .await
        })
        .unwrap()
    }

    fn freedesktop_item(address: &str, calls: Arc<FakeCalls>) -> Connection {
        async_io::block_on(async {
            zbus::connection::Builder::address(address)?
                .name(FREEDESKTOP_ITEM_NAME)?
                .serve_at(ITEM_PATH, FakeFreedesktopItem { calls })?
                .build()
                .await
        })
        .unwrap()
    }

    fn stalled_item(address: &str, started: Arc<AtomicBool>) -> Connection {
        async_io::block_on(async {
            zbus::connection::Builder::address(address)?
                .name(STALLED_ITEM_NAME)?
                .serve_at(ITEM_PATH, StalledItem { started })?
                .build()
                .await
        })
        .unwrap()
    }

    fn register_item(connection: &Connection, protocol: WatcherProtocol, service: &str) {
        async_io::block_on(connection.call_method(
            Some(protocol.name),
            WATCHER_PATH,
            Some(protocol.interface),
            "RegisterStatusNotifierItem",
            &(service,),
        ))
        .unwrap();
    }

    fn emit_item_signal<B>(connection: &Connection, member: &str, body: &B)
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        async_io::block_on(connection.emit_signal(
            None::<&str>,
            ITEM_PATH,
            KDE_ITEM_INTERFACE,
            member,
            body,
        ))
        .unwrap();
    }

    fn emit_watcher_registration(
        connection: &Connection,
        protocol: WatcherProtocol,
        registration: &str,
    ) {
        async_io::block_on(connection.emit_signal(
            None::<&str>,
            WATCHER_PATH,
            protocol.interface,
            "StatusNotifierItemRegistered",
            &(registration,),
        ))
        .unwrap();
    }

    fn wait_for(
        tray: &SystemTray,
        predicate: impl Fn(&SystemTraySnapshot) -> bool,
    ) -> SystemTraySnapshot {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut observed = Vec::new();
        while Instant::now() < deadline {
            if let Some(snapshot) = tray.take_snapshot() {
                observed.push(snapshot.clone());
                if predicate(&snapshot) {
                    return snapshot;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for system tray snapshot; observed {observed:?}");
    }

    fn wait_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for system tray fixture");
    }
}
