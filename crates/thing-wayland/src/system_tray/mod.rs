//! bounded StatusNotifierItem discovery, state, and controls.

mod menu;
mod model;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::Future,
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
use thing_runtime::{SystemTrayAction, SystemTrayItem, SystemTrayMenuAction, SystemTraySnapshot};
use zbus::{
    Connection, MatchRule, MessageStream,
    fdo::{DBusProxy, RequestNameFlags, RequestNameReply},
    message::{Flags, Header, Message, Type},
    names::BusName,
    object_server::SignalEmitter,
    zvariant::{OwnedObjectPath, OwnedValue},
};

use menu::{
    FetchToken, LayoutNode, MenuLayout, MenuModel, MenuProperties, MenuPropertyDelta, MenuTarget,
};
use model::{Endpoint, Lifetime, Model, QueriedIcon, QueriedItem, QueriedPixmap, Target};

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
const DBUSMENU_INTERFACE: &str = "com.canonical.dbusmenu";
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";
const MAX_ITEMS: usize = 128;
const MAX_HOSTS: usize = 64;
const MAX_COMMANDS: usize = 128;
const MAX_QUERIES: usize = 4;
const MAX_MENU_QUERIES: usize = 4;
const MAX_MENU_QUERY_QUEUE: usize = 128;
const MAX_OUTGOING_MENU_EVENTS: usize = menu::MAX_TOTAL_MENU_NODES + MAX_COMMANDS;
const MAX_ENDPOINT_BYTES: usize = 4096;
const MAX_SIGNAL_BYTES: usize = 8 * 1024;
const MAX_PROPERTY_BYTES: usize = 512 * 1024;
const MAX_MENU_BODY_BYTES: usize = 512 * 1024;
const MAX_MENU_PATH_BYTES: usize = 4096;
const MAX_PIXMAPS: usize = 32;
const MAX_PIXMAP_BYTES: usize = 256 * 1024;
const MAX_PIXMAP_SIDE: u32 = 4096;
const MAX_ITEM_IMAGE_BYTES: usize = 512 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const METHOD_TIMEOUT: Duration = Duration::from_secs(2);
const ABOUT_TO_SHOW_TIMEOUT: Duration = Duration::from_millis(500);

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
struct ItemCommand {
    target: Target,
    action: SystemTrayAction,
}

#[derive(Clone)]
struct MenuCommand {
    target: MenuTarget,
    action: SystemTrayMenuAction,
    consumer: String,
}

#[derive(Clone)]
enum Command {
    Item(ItemCommand),
    Menu(MenuCommand),
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

#[derive(Clone)]
struct MenuQueryIntent {
    target: MenuTarget,
    about_to_show: bool,
    fetch_layout: bool,
}

#[derive(Default)]
struct MenuDispatchCursors {
    explicit: Option<String>,
    background: Option<String>,
}

#[derive(Clone)]
struct MenuQueryRequest {
    token: FetchToken,
    about_to_show: bool,
    fetch_layout: bool,
}

struct MenuQueryResult {
    request: MenuQueryRequest,
    result: Result<Option<MenuLayout>, String>,
}

type MenuQuery = LocalBoxFuture<'static, MenuQueryResult>;

#[derive(Clone)]
struct OutgoingMenuEvent {
    target: MenuTarget,
    event: &'static str,
}

#[cfg(test)]
struct MenuTestGate {
    reached: std::sync::mpsc::Sender<()>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl MenuTestGate {
    fn pause(&self) {
        self.reached.send(()).expect("menu test gate disappeared");
        lock(&self.resume)
            .recv_timeout(Duration::from_secs(8))
            .expect("menu test gate timed out");
    }
}

#[derive(Default)]
struct Shared {
    commands: VecDeque<Command>,
    targets: BTreeMap<String, Target>,
    menu_targets: BTreeMap<String, MenuTarget>,
    menu_consumers: BTreeSet<String>,
    menu_known_consumers: BTreeSet<String>,
    releases: BTreeSet<String>,
    online: bool,
    watcher_owned: bool,
    stop: bool,
    latest: Option<SystemTraySnapshot>,
    #[cfg(test)]
    popped_open_gate: Option<Arc<MenuTestGate>>,
    #[cfg(test)]
    popped_activate_gate: Option<Arc<MenuTestGate>>,
    #[cfg(test)]
    open_inserted_gate: Option<Arc<MenuTestGate>>,
    #[cfg(test)]
    opened_sent_gate: Option<Arc<MenuTestGate>>,
}

/// a status notifier worker for discovery, metadata, and item actions.
///
/// snapshots retain bounded normal and attention icons and lazily fetched,
/// bounded dbusmenu layouts. menu presentation remains configuration-owned.
/// pixmap animation, menu icon data, overlays, and legacy xembed items are not
/// handled here. action coordinates are `(0, 0)` because wayland clients do
/// not have global screen coordinates.
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
                let mut next_menu_id = 1_u64;
                let mut connection_epoch = 1_u64;
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
                        SessionIds {
                            next_item: &mut next_item_id,
                            next_menu: &mut next_menu_id,
                            connection_epoch,
                        },
                        |snapshot| publish(&state, &ping, &mut previous, snapshot),
                    ));
                    let Some(next_epoch) = connection_epoch.checked_add(1) else {
                        publish(
                            &state,
                            &ping,
                            &mut previous,
                            SystemTraySnapshot {
                                error: "system tray connection generation space exhausted".into(),
                                ..SystemTraySnapshot::default()
                            },
                        );
                        break;
                    };
                    connection_epoch = next_epoch;
                    {
                        let mut shared = lock(&state);
                        shared.online = false;
                        shared.watcher_owned = false;
                        shared.targets.clear();
                        shared.menu_targets.clear();
                        shared.menu_consumers.clear();
                        shared.menu_known_consumers.clear();
                        shared.releases.clear();
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
        shared
            .commands
            .push_back(Command::Item(ItemCommand { target, action }));
        drop(shared);
        wake(&self.wake);
        Ok(())
    }

    /// queue an action for one current DBusMenu branch and configuration lifetime.
    pub(crate) fn send_menu(
        &self,
        action: SystemTrayMenuAction,
        id: &str,
        consumer: &str,
    ) -> Result<(), &'static str> {
        let mut shared = lock(&self.shared);
        if !shared.online {
            return Err("system tray service is disconnected");
        }
        if consumer.is_empty()
            || consumer.len() > menu::MAX_MENU_CONSUMER_BYTES
            || consumer.contains('\0')
        {
            return Err("invalid system tray menu consumer");
        }
        if shared.releases.contains(consumer) {
            return Err("system tray menu consumer is being released");
        }
        let target = shared
            .menu_targets
            .get(id)
            .cloned()
            .ok_or("system tray menu is no longer available")?;
        if shared.commands.len() >= MAX_COMMANDS {
            return Err("system tray command queue is full");
        }
        if !shared.menu_known_consumers.contains(consumer)
            && shared.menu_known_consumers.len() >= menu::MAX_MENU_CONSUMERS
        {
            return Err("too many system tray menu consumers");
        }
        shared
            .menu_known_consumers
            .insert(consumer.to_owned().into_boxed_str().into_string());
        shared.commands.push_back(Command::Menu(MenuCommand {
            target,
            action,
            consumer: consumer.to_owned(),
        }));
        drop(shared);
        wake(&self.wake);
        Ok(())
    }

    /// release every menu branch opened by one retired configuration lifetime.
    pub(crate) fn release_menu_consumer(&self, consumer: &str) -> Result<(), &'static str> {
        let mut shared = lock(&self.shared);
        if consumer.is_empty()
            || consumer.len() > menu::MAX_MENU_CONSUMER_BYTES
            || consumer.contains('\0')
        {
            return Err("invalid system tray menu consumer");
        }
        shared.commands.retain(
            |command| !matches!(command, Command::Menu(command) if command.consumer == consumer),
        );
        if shared.menu_known_consumers.contains(consumer) {
            if shared.online {
                shared
                    .releases
                    .insert(consumer.to_owned().into_boxed_str().into_string());
            } else {
                shared.menu_known_consumers.remove(consumer);
                shared.releases.remove(consumer);
            }
        }
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

struct SessionIds<'a> {
    next_item: &'a mut u64,
    next_menu: &'a mut u64,
    connection_epoch: u64,
}

async fn session(
    address: Option<&str>,
    ownership_allowed: &mut bool,
    shared: &Mutex<Shared>,
    wake_stream: &UnixStream,
    ids: SessionIds<'_>,
    mut publish: impl FnMut(SystemTraySnapshot),
) -> Result<(), String> {
    let SessionIds {
        next_item: next_item_id,
        next_menu: next_menu_id,
        connection_epoch,
    } = ids;
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
    let mut menu_events = menu_stream(&connection).await?;
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

    let host_names = host_names(&unique_name);
    let mut owned_host_names = BTreeSet::new();
    let mut active_host = None;
    if watcher_owned {
        acquire_host_names(&connection, &host_names).await?;
        owned_host_names.extend(WATCHER_PROTOCOLS);
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
    let mut menus = MenuModel::default();
    let mut current_watcher = select_watcher(&watcher_owners);
    let mut attached_watcher = None;
    let mut queued = BTreeMap::new();
    let mut running = BTreeSet::new();
    let mut queries = FuturesUnordered::<ItemQuery>::new();
    let mut queued_menus = BTreeMap::new();
    let mut menu_queue_cursor = None;
    let mut menu_dispatch_cursors = MenuDispatchCursors::default();
    let mut running_menus = BTreeSet::new();
    let mut menu_queries = FuturesUnordered::<MenuQuery>::new();
    let mut outgoing_menu_events = VecDeque::<OutgoingMenuEvent>::new();

    loop {
        if stopping(shared) {
            return Ok(());
        }
        let pending_release = { lock(shared).releases.first().cloned() };
        if let Some(consumer) = pending_release {
            queue_closed_menu_events(
                &mut queued_menus,
                &mut outgoing_menu_events,
                menus.release_consumer(&consumer),
            )?;
            consume_menu_release(shared, &consumer);
            publish(update_shared(
                shared,
                &model,
                &menus,
                current_watcher.is_some(),
                watcher_owned,
                String::new(),
            ));
            continue;
        }
        if let Some(event) = outgoing_menu_events.front().cloned() {
            if !send_menu_event_interruptible(
                &connection,
                &event.target,
                event.event,
                shared,
                &wake_source,
            )
            .await?
            {
                return Ok(());
            }
            outgoing_menu_events.pop_front();
            continue;
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
            menus.clear();
            queue_retired_menu_events(&mut menus, &mut queued_menus, &mut outgoing_menu_events)?;
            queued.clear();
            running.clear();
            queries = FuturesUnordered::new();
            queued_menus.clear();
            menu_queue_cursor = None;
            menu_dispatch_cursors = MenuDispatchCursors::default();
            running_menus.clear();
            menu_queries = FuturesUnordered::new();
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
            reconcile_menus(&mut menus, &model, connection_epoch, next_menu_id)?;
            publish(update_shared(
                shared,
                &model,
                &menus,
                current_watcher.is_some(),
                watcher_owned,
                String::new(),
            ));
        }
        if !lock(shared).releases.is_empty() {
            continue;
        }
        start_queries(&connection, &mut queued, &mut running, &mut queries);
        refill_menu_queue(&menus, &mut queued_menus, &mut menu_queue_cursor);
        if !lock(shared).releases.is_empty() {
            continue;
        }
        if start_menu_queries(
            &connection,
            &mut menus,
            &mut queued_menus,
            &mut running_menus,
            &mut menu_queries,
            &mut menu_dispatch_cursors,
        )? {
            publish(update_shared(
                shared,
                &model,
                &menus,
                current_watcher.is_some(),
                watcher_owned,
                String::new(),
            ));
        }

        let input = future::race(
            async { Input::Name(names.next().await) },
            future::race(
                async { Input::Watcher(watcher_events.next().await) },
                future::race(
                    async { Input::Item(item_events.next().await) },
                    future::race(
                        async { Input::Menu(menu_events.next().await) },
                        future::race(
                            async {
                                let Some(result) = queries.next().await else {
                                    return future::pending().await;
                                };
                                Input::Query(Box::new(result))
                            },
                            future::race(
                                async {
                                    let Some(result) = menu_queries.next().await else {
                                        return future::pending().await;
                                    };
                                    Input::MenuQuery(Box::new(result))
                                },
                                future::race(
                                    async {
                                        let command = pop_command(shared);
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
                        reconcile_menus(&mut menus, &model, connection_epoch, next_menu_id)?;
                        retain_current_menu_queries(&menus, &mut queued_menus);
                        queue_retired_menu_events(
                            &mut menus,
                            &mut queued_menus,
                            &mut outgoing_menu_events,
                        )?;
                        publish(update_shared(
                            shared,
                            &model,
                            &menus,
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
                            reconcile_menus(&mut menus, &model, connection_epoch, next_menu_id)?;
                            retain_current_menu_queries(&menus, &mut queued_menus);
                            queue_retired_menu_events(
                                &mut menus,
                                &mut queued_menus,
                                &mut outgoing_menu_events,
                            )?;
                            publish(update_shared(
                                shared,
                                &model,
                                &menus,
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
            Input::Menu(Some(Ok(message))) => {
                let Some(owner) = message.header().sender().map(ToString::to_string) else {
                    continue;
                };
                let Some(path) = message.header().path().map(ToString::to_string) else {
                    continue;
                };
                let registrations = menus.registrations_for_signal(&owner, &path);
                if registrations.is_empty() {
                    continue;
                }
                let member = message.header().member().map(ToString::to_string);
                let mut targets = Vec::new();
                let mut changed = false;
                if message.body().len() > MAX_MENU_BODY_BYTES {
                    for registration in &registrations {
                        targets.extend(menus.mark_whole_menu_dirty(registration));
                    }
                } else {
                    match member.as_deref() {
                        Some("LayoutUpdated") => {
                            if let Ok((_revision, parent)) =
                                message.body().deserialize::<(u32, i32)>()
                            {
                                for registration in &registrations {
                                    if parent == 0 {
                                        targets.extend(menus.mark_whole_menu_dirty(registration));
                                    } else if let Some(target) =
                                        menus.invalidate_branch(registration, parent)
                                    {
                                        targets.push(target);
                                    }
                                }
                            } else {
                                for registration in &registrations {
                                    targets.extend(menus.mark_whole_menu_dirty(registration));
                                }
                            }
                        }
                        Some("ItemsPropertiesUpdated") => {
                            match parse_menu_property_updates(&message) {
                                Ok(updates) => {
                                    for registration in &registrations {
                                        match menus.apply_property_updates(registration, &updates) {
                                            Ok(updated) => changed |= updated,
                                            Err(_) => targets
                                                .extend(menus.mark_whole_menu_dirty(registration)),
                                        }
                                    }
                                }
                                Err(_) => {
                                    for registration in &registrations {
                                        targets.extend(menus.mark_whole_menu_dirty(registration));
                                    }
                                }
                            }
                        }
                        _ => continue,
                    }
                }
                for target in targets {
                    changed |= queue_menu_query(&menus, &mut queued_menus, target, false, true);
                }
                queue_retired_menu_events(
                    &mut menus,
                    &mut queued_menus,
                    &mut outgoing_menu_events,
                )?;
                if changed {
                    publish(update_shared(
                        shared,
                        &model,
                        &menus,
                        current_watcher.is_some(),
                        watcher_owned,
                        String::new(),
                    ));
                }
            }
            Input::Menu(Some(Err(error))) => return Err(error.to_string()),
            Input::Menu(None) => return Err("system tray menu subscription ended".into()),
            Input::Query(result) => {
                running.remove(&result.request.lifetime.id);
                if let Ok(queried) = result.result
                    && model.update(&result.request.lifetime, queried, MAX_TOTAL_IMAGE_BYTES)?
                {
                    reconcile_menus(&mut menus, &model, connection_epoch, next_menu_id)?;
                    retain_current_menu_queries(&menus, &mut queued_menus);
                    queue_retired_menu_events(
                        &mut menus,
                        &mut queued_menus,
                        &mut outgoing_menu_events,
                    )?;
                    publish(update_shared(
                        shared,
                        &model,
                        &menus,
                        current_watcher.is_some(),
                        watcher_owned,
                        String::new(),
                    ));
                }
            }
            Input::MenuQuery(result) => {
                running_menus.remove(&result.request.token.target.id);
                match result.result {
                    Ok(Some(layout)) => {
                        if let Err(error) =
                            menus.apply_layout(&result.request.token, layout, next_menu_id)
                        {
                            menus.finish_error(&result.request.token, error);
                        }
                    }
                    Ok(None) => {
                        menus.finish_without_layout(&result.request.token);
                    }
                    Err(error) => {
                        menus.finish_error(&result.request.token, error);
                    }
                }
                queue_retired_menu_events(
                    &mut menus,
                    &mut queued_menus,
                    &mut outgoing_menu_events,
                )?;
                publish(update_shared(
                    shared,
                    &model,
                    &menus,
                    current_watcher.is_some(),
                    watcher_owned,
                    String::new(),
                ));
            }
            Input::Command(command) => match command {
                Command::Item(command) => {
                    if model.contains_target(&command.target)
                        && !send_action_interruptible(&connection, command, shared, &wake_source)
                            .await?
                    {
                        return Ok(());
                    }
                }
                Command::Menu(command) => {
                    if menu_consumer_released(shared, &command.consumer) {
                        continue;
                    }
                    match command.action {
                        SystemTrayMenuAction::Open => {
                            if menus.contains_target(&command.target)
                                && !menu_consumer_released(shared, &command.consumer)
                                && let Ok(decision) = menus.open(&command.target, &command.consumer)
                            {
                                publish(update_shared(
                                    shared,
                                    &model,
                                    &menus,
                                    current_watcher.is_some(),
                                    watcher_owned,
                                    String::new(),
                                ));
                                #[cfg(test)]
                                {
                                    let gate = { lock(shared).open_inserted_gate.take() };
                                    if let Some(gate) = gate {
                                        gate.pause();
                                    }
                                }
                                if menu_consumer_released(shared, &command.consumer) {
                                    let mut closed = menus.release_consumer(&command.consumer);
                                    if decision.first_lease {
                                        closed.retain(|target| target != &command.target);
                                    }
                                    queue_closed_menu_events(
                                        &mut queued_menus,
                                        &mut outgoing_menu_events,
                                        closed,
                                    )?;
                                    publish(update_shared(
                                        shared,
                                        &model,
                                        &menus,
                                        current_watcher.is_some(),
                                        watcher_owned,
                                        String::new(),
                                    ));
                                    continue;
                                }
                                if decision.first_lease {
                                    if !send_menu_event_interruptible(
                                        &connection,
                                        &command.target,
                                        "opened",
                                        shared,
                                        &wake_source,
                                    )
                                    .await?
                                    {
                                        return Ok(());
                                    }
                                    #[cfg(test)]
                                    {
                                        let gate = { lock(shared).opened_sent_gate.take() };
                                        if let Some(gate) = gate {
                                            gate.pause();
                                        }
                                    }
                                    if menu_consumer_released(shared, &command.consumer) {
                                        queue_closed_menu_events(
                                            &mut queued_menus,
                                            &mut outgoing_menu_events,
                                            menus.release_consumer(&command.consumer),
                                        )?;
                                        publish(update_shared(
                                            shared,
                                            &model,
                                            &menus,
                                            current_watcher.is_some(),
                                            watcher_owned,
                                            String::new(),
                                        ));
                                        continue;
                                    }
                                    queue_menu_query(
                                        &menus,
                                        &mut queued_menus,
                                        command.target,
                                        true,
                                        decision.needs_layout,
                                    );
                                }
                            }
                        }
                        SystemTrayMenuAction::Close => {
                            if menus.contains_target(&command.target)
                                && let Ok(decision) =
                                    menus.close(&command.target, &command.consumer)
                            {
                                queue_closed_menu_events(
                                    &mut queued_menus,
                                    &mut outgoing_menu_events,
                                    decision.closed,
                                )?;
                                publish(update_shared(
                                    shared,
                                    &model,
                                    &menus,
                                    current_watcher.is_some(),
                                    watcher_owned,
                                    String::new(),
                                ));
                            }
                        }
                        SystemTrayMenuAction::Activate => {
                            if menus.can_activate(&command.target)
                                && !send_menu_event_interruptible(
                                    &connection,
                                    &command.target,
                                    "clicked",
                                    shared,
                                    &wake_source,
                                )
                                .await?
                            {
                                return Ok(());
                            }
                        }
                    }
                }
            },
        }
    }
}

enum Input {
    Name(Option<zbus::Result<Message>>),
    Watcher(Option<zbus::Result<Message>>),
    Item(Option<zbus::Result<Message>>),
    Menu(Option<zbus::Result<Message>>),
    Query(Box<QueryResult>),
    MenuQuery(Box<MenuQueryResult>),
    Command(Command),
    Wake(io::Result<()>),
}

fn update_shared(
    shared: &Mutex<Shared>,
    model: &Model,
    menus: &MenuModel,
    available: bool,
    watcher_owned: bool,
    error: String,
) -> SystemTraySnapshot {
    let projection = model.projection_with(available, watcher_owned && available, error, |item| {
        menus.projection(item)
    });
    let mut shared = lock(shared);
    shared.online = available;
    shared.watcher_owned = watcher_owned && available;
    shared.targets = projection.targets;
    shared.menu_targets = menus.targets();
    shared.menu_consumers = menus.consumers();
    projection.snapshot
}

fn queue_query(queued: &mut BTreeMap<String, QueryRequest>, lifetime: Lifetime) {
    if queued.contains_key(&lifetime.id) || queued.len() < MAX_ITEMS {
        queued.insert(lifetime.id.clone(), QueryRequest { lifetime });
    }
}

fn queue_outgoing_menu_event(
    queued: &mut VecDeque<OutgoingMenuEvent>,
    target: MenuTarget,
    event: &'static str,
) -> Result<(), String> {
    if queued.len() >= MAX_OUTGOING_MENU_EVENTS {
        return Err("system tray menu event queue exceeded its internal bound".into());
    }
    queued.push_back(OutgoingMenuEvent { target, event });
    Ok(())
}

fn queue_retired_menu_events(
    menus: &mut MenuModel,
    queries: &mut BTreeMap<String, MenuQueryIntent>,
    events: &mut VecDeque<OutgoingMenuEvent>,
) -> Result<(), String> {
    queue_closed_menu_events(queries, events, menus.take_retired())
}

fn queue_closed_menu_events(
    queries: &mut BTreeMap<String, MenuQueryIntent>,
    events: &mut VecDeque<OutgoingMenuEvent>,
    targets: impl IntoIterator<Item = MenuTarget>,
) -> Result<(), String> {
    for target in targets {
        queries.remove(&target.id);
        queue_outgoing_menu_event(events, target, "closed")?;
    }
    Ok(())
}

fn retain_current_menu_queries(menus: &MenuModel, queries: &mut BTreeMap<String, MenuQueryIntent>) {
    queries.retain(|_, request| menus.can_query(&request.target));
}

fn menu_consumer_released(shared: &Mutex<Shared>, consumer: &str) -> bool {
    lock(shared).releases.contains(consumer)
}

fn consume_menu_release(shared: &Mutex<Shared>, consumer: &str) {
    let mut shared = lock(shared);
    shared.commands.retain(
        |command| !matches!(command, Command::Menu(command) if command.consumer == consumer),
    );
    shared.releases.remove(consumer);
    shared.menu_known_consumers.remove(consumer);
}

fn pop_command(shared: &Mutex<Shared>) -> Option<Command> {
    let command = lock(shared).commands.pop_front();
    #[cfg(test)]
    {
        let gate = match &command {
            Some(Command::Menu(MenuCommand {
                action: SystemTrayMenuAction::Open,
                ..
            })) => lock(shared).popped_open_gate.take(),
            Some(Command::Menu(MenuCommand {
                action: SystemTrayMenuAction::Activate,
                ..
            })) => lock(shared).popped_activate_gate.take(),
            _ => None,
        };
        if let Some(gate) = gate {
            gate.pause();
        }
    }
    command
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

fn reconcile_menus(
    menus: &mut MenuModel,
    model: &Model,
    connection_epoch: u64,
    next_menu_id: &mut u64,
) -> Result<bool, String> {
    menus.reconcile(model.menu_registrations(), connection_epoch, next_menu_id)
}

fn queue_menu_query(
    menus: &MenuModel,
    queued: &mut BTreeMap<String, MenuQueryIntent>,
    target: MenuTarget,
    about_to_show: bool,
    fetch_layout: bool,
) -> bool {
    if !menus.can_query(&target) {
        return false;
    }
    if let Some(existing) = queued.get_mut(&target.id) {
        existing.about_to_show |= about_to_show;
        existing.fetch_layout |= fetch_layout;
        return true;
    }
    if queued.len() >= MAX_MENU_QUERY_QUEUE {
        if about_to_show
            && let Some(id) = queued
                .iter()
                .rev()
                .find(|(_, request)| !request.about_to_show)
                .map(|(id, _)| id.clone())
        {
            queued.remove(&id);
        } else {
            return false;
        }
    }
    queued.insert(
        target.id.clone(),
        MenuQueryIntent {
            target,
            about_to_show,
            fetch_layout,
        },
    );
    true
}

fn refill_menu_queue(
    menus: &MenuModel,
    queued: &mut BTreeMap<String, MenuQueryIntent>,
    cursor: &mut Option<String>,
) {
    let mut targets = menus.pending_leased_targets();
    targets.sort_by(|left, right| left.id.cmp(&right.id));
    if let Some(cursor) = cursor {
        let split = targets.partition_point(|target| target.id <= *cursor);
        targets.rotate_left(split);
    }
    for target in targets {
        if queued.len() >= MAX_MENU_QUERY_QUEUE {
            break;
        }
        *cursor = Some(target.id.clone());
        let about_to_show = menus.pending_about_to_show(&target);
        let fetch_layout = menus.needs_layout(&target);
        queue_menu_query(menus, queued, target, about_to_show, fetch_layout);
    }
}

fn start_menu_queries(
    connection: &Connection,
    menus: &mut MenuModel,
    queued: &mut BTreeMap<String, MenuQueryIntent>,
    running: &mut BTreeSet<String>,
    queries: &mut FuturesUnordered<MenuQuery>,
    cursors: &mut MenuDispatchCursors,
) -> Result<bool, String> {
    let mut changed = false;
    while running.len() < MAX_MENU_QUERIES {
        let Some(id) = next_menu_query_id(queued, running, cursors) else {
            break;
        };
        let intent = queued.remove(&id).expect("queued menu query disappeared");
        if !menus.can_query(&intent.target) {
            continue;
        }
        let about_to_show = intent.about_to_show || menus.pending_about_to_show(&intent.target);
        let fetch_layout = intent.fetch_layout || menus.needs_layout(&intent.target);
        let Some(token) = menus.prepare_fetch(&intent.target, about_to_show)? else {
            continue;
        };
        let request = MenuQueryRequest {
            token,
            about_to_show,
            fetch_layout,
        };
        running.insert(id);
        changed = true;
        let connection = connection.clone();
        let query_request = request.clone();
        queries.push(
            async move {
                let result = query_menu(&connection, &query_request).await;
                MenuQueryResult { request, result }
            }
            .boxed_local(),
        );
    }
    Ok(changed)
}

fn next_menu_query_id(
    queued: &BTreeMap<String, MenuQueryIntent>,
    running: &BTreeSet<String>,
    cursors: &mut MenuDispatchCursors,
) -> Option<String> {
    let explicit = queued
        .iter()
        .any(|(id, request)| request.about_to_show && !running.contains(id));
    let cursor = if explicit {
        &mut cursors.explicit
    } else {
        &mut cursors.background
    };
    let eligible = |id: &&String, request: &&MenuQueryIntent| {
        request.about_to_show == explicit && !running.contains(*id)
    };
    let next = queued
        .iter()
        .find(|(id, request)| {
            eligible(id, request) && cursor.as_ref().is_some_and(|cursor| *id > cursor)
        })
        .or_else(|| queued.iter().find(|(id, request)| eligible(id, request)))
        .map(|(id, _)| id.clone())?;
    *cursor = Some(next.clone());
    Some(next)
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
                let raw_icon_pixmaps =
                    take::<Vec<Pixmap>>(&mut values, "IconPixmap").unwrap_or_default();
                let raw_attention_pixmaps =
                    take::<Vec<Pixmap>>(&mut values, "AttentionIconPixmap").unwrap_or_default();
                let overlay_pixmaps =
                    take::<Vec<Pixmap>>(&mut values, "OverlayIconPixmap").unwrap_or_default();
                let menu = take::<OwnedObjectPath>(&mut values, "Menu")
                    .map(|path| path.to_string())
                    .unwrap_or_default();
                let menu_path = (!menu.is_empty()
                    && menu != "/NO_DBUSMENU"
                    && menu.len() <= MAX_MENU_PATH_BYTES)
                    .then(|| menu.into_boxed_str().into_string());
                let icon_name = take_string(&mut values, "IconName", 4096);
                let attention_icon_name = take_string(&mut values, "AttentionIconName", 4096);
                let mut icon = QueriedIcon {
                    name: icon_name.clone(),
                    pixmaps: normalize_pixmaps(raw_icon_pixmaps),
                };
                let mut attention_icon = QueriedIcon {
                    name: attention_icon_name.clone(),
                    pixmaps: normalize_pixmaps(raw_attention_pixmaps),
                };
                enforce_item_image_budget(&mut icon, &mut attention_icon);
                return Ok(QueriedItem {
                    item: SystemTrayItem {
                        id: lifetime.id.clone(),
                        protocol_item_id: take_string(&mut values, "Id", 4096),
                        category: take_string(&mut values, "Category", 4096),
                        title: take_string(&mut values, "Title", 4096),
                        status: take_string(&mut values, "Status", 4096),
                        icon: None,
                        icon_name,
                        attention_icon_name,
                        overlay_icon_name: take_string(&mut values, "OverlayIconName", 4096),
                        tooltip_icon_name: sanitize(tooltip.0, 4096),
                        tooltip_title: sanitize(tooltip.2, 4096),
                        tooltip_text: sanitize(tooltip.3, 16 * 1024),
                        item_is_menu: take::<bool>(&mut values, "ItemIsMenu").unwrap_or(false),
                        has_menu: menu_path.is_some(),
                        menu: None,
                        has_icon_pixmap: !icon.pixmaps.is_empty(),
                        has_attention_icon_pixmap: !attention_icon.pixmaps.is_empty(),
                        has_overlay_icon_pixmap: valid_pixmaps(&overlay_pixmaps),
                        has_tooltip_icon_pixmap: valid_pixmaps(&tooltip.1),
                    },
                    menu_path,
                    icon,
                    attention_icon,
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
        && pixmaps.iter().all(valid_pixmap)
        && pixmaps
            .iter()
            .try_fold(0_usize, |total, (_, _, bytes)| {
                total.checked_add(bytes.len())
            })
            .is_some_and(|total| total <= MAX_ITEM_IMAGE_BYTES)
}

fn valid_pixmap((width, height, bytes): &Pixmap) -> bool {
    let dimensions = u32::try_from(*width).ok().zip(u32::try_from(*height).ok());
    let Some((width, height)) = dimensions else {
        return false;
    };
    if width == 0 || height == 0 || width > MAX_PIXMAP_SIDE || height > MAX_PIXMAP_SIDE {
        return false;
    }
    let expected = usize::try_from(width)
        .ok()
        .zip(usize::try_from(height).ok())
        .and_then(|(width, height)| width.checked_mul(height))
        .and_then(|pixels| pixels.checked_mul(4));
    expected.is_some_and(|expected| expected == bytes.len() && expected <= MAX_PIXMAP_BYTES)
}

fn normalize_pixmaps(pixmaps: Vec<Pixmap>) -> Vec<QueriedPixmap> {
    if !valid_pixmaps(&pixmaps) {
        return Vec::new();
    }
    let mut sizes = BTreeSet::new();
    let mut normalized = Vec::new();
    for (width, height, argb) in pixmaps {
        let width = width as u32;
        let height = height as u32;
        if !sizes.insert((width, height)) {
            continue;
        }
        let mut rgba = Vec::with_capacity(argb.len());
        for pixel in argb.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
        }
        normalized.push(QueriedPixmap {
            width,
            height,
            rgba,
        });
    }
    normalized
}

fn enforce_item_image_budget(icon: &mut QueriedIcon, attention_icon: &mut QueriedIcon) {
    if icon
        .retained_bytes()
        .checked_add(attention_icon.retained_bytes())
        .is_none_or(|bytes| bytes > MAX_ITEM_IMAGE_BYTES)
    {
        icon.pixmaps.clear();
        attention_icon.pixmaps.clear();
    }
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
    value.into_boxed_str().into_string()
}

type RawMenuLayout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

async fn query_menu(
    connection: &Connection,
    request: &MenuQueryRequest,
) -> Result<Option<MenuLayout>, String> {
    enum Progress {
        About(Result<bool, String>),
        Layout(Result<MenuLayout, String>),
        Timeout,
    }

    let timeout = Timer::after(METHOD_TIMEOUT);
    futures_lite::pin!(timeout);
    if request.about_to_show && request.fetch_layout {
        let about = about_to_show(connection, &request.token.target);
        let speculative = get_menu_layout(connection, &request.token.target);
        futures_lite::pin!(about);
        futures_lite::pin!(speculative);
        let mut speculative_result = None::<Result<MenuLayout, String>>;
        loop {
            let progress = if speculative_result.is_some() {
                future::race(async { Progress::About(about.as_mut().await) }, async {
                    timeout.as_mut().await;
                    Progress::Timeout
                })
                .await
            } else {
                future::race(
                    async { Progress::About(about.as_mut().await) },
                    future::race(
                        async { Progress::Layout(speculative.as_mut().await) },
                        async {
                            timeout.as_mut().await;
                            Progress::Timeout
                        },
                    ),
                )
                .await
            };
            match progress {
                Progress::About(Ok(true)) => {
                    return future::race(
                        async {
                            get_menu_layout(connection, &request.token.target)
                                .await
                                .map(Some)
                        },
                        async {
                            timeout.as_mut().await;
                            Err("system tray menu request timed out".into())
                        },
                    )
                    .await;
                }
                Progress::About(Ok(false) | Err(_)) => {
                    if let Some(result) = speculative_result {
                        return result.map(Some);
                    }
                    return future::race(async { speculative.as_mut().await.map(Some) }, async {
                        timeout.as_mut().await;
                        Err("system tray menu request timed out".into())
                    })
                    .await;
                }
                Progress::Layout(result) => speculative_result = Some(result),
                Progress::Timeout => {
                    return speculative_result.map_or_else(
                        || Err("system tray menu request timed out".into()),
                        |result| result.map(Some),
                    );
                }
            }
        }
    }
    if request.about_to_show {
        let about_timeout = Timer::after(ABOUT_TO_SHOW_TIMEOUT);
        let about = future::race(
            async { Some(about_to_show(connection, &request.token.target).await) },
            async {
                about_timeout.await;
                None
            },
        )
        .await;
        return match about {
            Some(Ok(true)) => {
                future::race(
                    async {
                        get_menu_layout(connection, &request.token.target)
                            .await
                            .map(Some)
                    },
                    async {
                        timeout.as_mut().await;
                        Err("system tray menu request timed out".into())
                    },
                )
                .await
            }
            Some(Ok(false)) => Ok(None),
            Some(Err(error)) => Err(error),
            None => Err("system tray menu visibility request timed out".into()),
        };
    }
    future::race(
        async {
            get_menu_layout(connection, &request.token.target)
                .await
                .map(Some)
        },
        async {
            timeout.as_mut().await;
            Err("system tray menu request timed out".into())
        },
    )
    .await
}

async fn about_to_show(connection: &Connection, target: &MenuTarget) -> Result<bool, String> {
    let message =
        call_menu_method(connection, target, "AboutToShow", &(target.protocol_id,)).await?;
    message
        .body()
        .deserialize::<(bool,)>()
        .map(|(update,)| update)
        .map_err(|error| error.to_string())
}

async fn get_menu_layout(
    connection: &Connection,
    target: &MenuTarget,
) -> Result<MenuLayout, String> {
    const PROPERTIES: [&str; 10] = [
        "type",
        "label",
        "enabled",
        "visible",
        "icon-name",
        "shortcut",
        "toggle-type",
        "toggle-state",
        "children-display",
        "disposition",
    ];
    let message = call_menu_method(
        connection,
        target,
        "GetLayout",
        &(target.protocol_id, 1_i32, PROPERTIES.as_slice()),
    )
    .await?;
    let (revision, raw) = message
        .body()
        .deserialize::<(u32, RawMenuLayout)>()
        .map_err(|error| error.to_string())?;
    Ok(MenuLayout {
        revision,
        parent: parse_menu_layout(raw)?,
    })
}

async fn call_menu_method<B>(
    connection: &Connection,
    target: &MenuTarget,
    method: &str,
    body: &B,
) -> Result<Message, String>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    let mut replies = MessageStream::from(connection);
    let request = Message::method_call(target.registration.path.as_str(), method)
        .and_then(|builder| builder.destination(target.registration.owner.as_str()))
        .and_then(|builder| builder.interface(DBUSMENU_INTERFACE))
        .and_then(|builder| builder.with_flags(Flags::NoAutoStart))
        .and_then(|builder| builder.build(body))
        .map_err(|error| error.to_string())?;
    let serial = request.primary_header().serial_num();
    connection
        .send(&request)
        .await
        .map_err(|error| error.to_string())?;
    loop {
        let reply = replies
            .next()
            .await
            .ok_or_else(|| "system tray menu reply stream ended".to_owned())?
            .map_err(|error| error.to_string())?;
        if reply.header().reply_serial() != Some(serial)
            || reply.header().sender().map(|sender| sender.as_str())
                != Some(target.registration.owner.as_str())
        {
            continue;
        }
        if reply.body().len() > MAX_MENU_BODY_BYTES {
            return Err("system tray menu reply exceeds 512 KiB".into());
        }
        return match reply.message_type() {
            Type::MethodReturn => Ok(reply),
            Type::Error => Err(format!(
                "system tray menu method failed: {}",
                reply
                    .header()
                    .error_name()
                    .map_or("unknown D-Bus error", |name| name.as_str())
            )),
            _ => Err("system tray menu returned an invalid reply".into()),
        };
    }
}

fn parse_menu_layout(raw: RawMenuLayout) -> Result<LayoutNode, String> {
    let (protocol_id, values, children) = raw;
    if children.len() > menu::MAX_MENU_CHILDREN {
        return Err("system tray menu has too many direct children".into());
    }
    let mut parsed = Vec::with_capacity(children.len());
    for child in children {
        let (protocol_id, values, grandchildren): RawMenuLayout = child
            .try_into()
            .map_err(|error: zbus::zvariant::Error| error.to_string())?;
        if !grandchildren.is_empty() {
            return Err("system tray menu returned more than one requested level".into());
        }
        parsed.push(LayoutNode {
            protocol_id,
            properties: parse_menu_properties(values)?,
            children: Vec::new(),
        });
    }
    Ok(LayoutNode {
        protocol_id,
        properties: parse_menu_properties(values)?,
        children: parsed,
    })
}

fn parse_menu_properties(
    mut values: HashMap<String, OwnedValue>,
) -> Result<MenuProperties, String> {
    let mut properties = MenuProperties::protocol_defaults();
    properties.kind = menu_property(&mut values, "type")?.unwrap_or(properties.kind);
    properties.raw_label = menu_property(&mut values, "label")?.unwrap_or_default();
    properties.enabled = menu_property(&mut values, "enabled")?.unwrap_or(true);
    properties.visible = menu_property(&mut values, "visible")?.unwrap_or(true);
    properties.icon_name = menu_property(&mut values, "icon-name")?.unwrap_or_default();
    properties.shortcut = menu_property(&mut values, "shortcut")?.unwrap_or_default();
    properties.toggle_type = menu_property(&mut values, "toggle-type")?.unwrap_or_default();
    properties.toggle_state = menu_property(&mut values, "toggle-state")?.unwrap_or(-1);
    let children_display: String =
        menu_property(&mut values, "children-display")?.unwrap_or_default();
    properties.has_submenu = children_display == "submenu";
    properties.disposition =
        menu_property(&mut values, "disposition")?.unwrap_or(properties.disposition);
    Ok(properties.compact())
}

fn menu_property<T>(
    values: &mut HashMap<String, OwnedValue>,
    key: &str,
) -> Result<Option<T>, String>
where
    T: TryFrom<OwnedValue>,
{
    values
        .remove(key)
        .map(|value| {
            value
                .try_into()
                .map_err(|_| format!("system tray menu property {key} has the wrong type"))
        })
        .transpose()
}

fn parse_menu_property_updates(
    message: &Message,
) -> Result<BTreeMap<i32, MenuPropertyDelta>, String> {
    let (updated, removed) = message
        .body()
        .deserialize::<(
            Vec<(i32, HashMap<String, OwnedValue>)>,
            Vec<(i32, Vec<String>)>,
        )>()
        .map_err(|error| error.to_string())?;
    parse_menu_property_update_values(updated, removed)
}

fn parse_menu_property_update_values(
    updated: Vec<(i32, HashMap<String, OwnedValue>)>,
    removed: Vec<(i32, Vec<String>)>,
) -> Result<BTreeMap<i32, MenuPropertyDelta>, String> {
    let mut result = BTreeMap::new();
    for (id, mut values) in updated {
        if result.contains_key(&id) {
            return Err("system tray menu property update contains a duplicate item".into());
        }
        let mut delta = MenuPropertyDelta {
            kind: menu_property(&mut values, "type")?,
            raw_label: menu_property(&mut values, "label")?,
            enabled: menu_property(&mut values, "enabled")?,
            visible: menu_property(&mut values, "visible")?,
            icon_name: menu_property(&mut values, "icon-name")?,
            shortcut: menu_property(&mut values, "shortcut")?,
            toggle_type: menu_property(&mut values, "toggle-type")?,
            toggle_state: menu_property(&mut values, "toggle-state")?,
            ..MenuPropertyDelta::default()
        };
        let children_display: Option<String> = menu_property(&mut values, "children-display")?;
        delta.has_submenu = children_display.map(|value| value == "submenu");
        delta.disposition = menu_property(&mut values, "disposition")?;
        // icon-data is intentionally not cloned or retained in the first menu API.
        values.remove("icon-data");
        result.insert(id, delta.compact());
    }
    for (id, properties) in removed {
        let delta = result.entry(id).or_insert_with(MenuPropertyDelta::default);
        for property in properties {
            if matches!(
                property.as_str(),
                "type"
                    | "label"
                    | "enabled"
                    | "visible"
                    | "icon-name"
                    | "shortcut"
                    | "toggle-type"
                    | "toggle-state"
                    | "children-display"
                    | "disposition"
            ) {
                delta.removed.insert(property);
            }
        }
    }
    for delta in result.values_mut() {
        *delta = std::mem::take(delta).compact();
    }
    Ok(result)
}

async fn send_menu_event(
    connection: &Connection,
    target: &MenuTarget,
    event: &str,
) -> Result<(), String> {
    let data =
        OwnedValue::try_from(zbus::zvariant::Value::from("")).map_err(|error| error.to_string())?;
    let message = Message::method_call(target.registration.path.as_str(), "Event")
        .and_then(|builder| builder.destination(target.registration.owner.as_str()))
        .and_then(|builder| builder.interface(DBUSMENU_INTERFACE))
        .and_then(|builder| builder.with_flags(Flags::NoReplyExpected))
        .and_then(|builder| builder.with_flags(Flags::NoAutoStart))
        .and_then(|builder| builder.build(&(target.protocol_id, event, data, 0_u32)))
        .map_err(|error| error.to_string())?;
    connection
        .send(&message)
        .await
        .map_err(|error| error.to_string())
}

async fn send_menu_event_interruptible(
    connection: &Connection,
    target: &MenuTarget,
    event: &str,
    shared: &Mutex<Shared>,
    wake_source: &Async<UnixStream>,
) -> Result<bool, String> {
    wait_for_menu_send(
        async {
            let _ = send_menu_event(connection, target, event).await;
        },
        shared,
        wake_source,
    )
    .await
}

async fn wait_for_menu_send<F>(
    send: F,
    shared: &Mutex<Shared>,
    wake_source: &Async<UnixStream>,
) -> Result<bool, String>
where
    F: Future<Output = ()>,
{
    enum Progress {
        Sent,
        Wake(io::Result<()>),
        Timeout,
    }

    let timeout = Timer::after(METHOD_TIMEOUT);
    futures_lite::pin!(send);
    futures_lite::pin!(timeout);
    loop {
        let progress = future::race(
            async {
                send.as_mut().await;
                Progress::Sent
            },
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
            Progress::Sent | Progress::Timeout => return Ok(true),
            Progress::Wake(result) => {
                result.map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
                if stopping(shared) {
                    return Ok(false);
                }
            }
        }
    }
}

async fn send_action(connection: &Connection, command: ItemCommand) -> Result<(), String> {
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
    command: ItemCommand,
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

async fn menu_stream(connection: &Connection) -> Result<MessageStream, String> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(DBUSMENU_INTERFACE)
        .map_err(|error| error.to_string())?
        .build();
    MessageStream::for_match_rule(rule, connection, Some(256))
        .await
        .map_err(|error| error.to_string())
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
        match request_name_without_queue(connection, protocol.name).await {
            Ok(true) => acquired.push(protocol.name.to_owned()),
            Ok(false) => {
                release_names(connection, &acquired).await?;
                return Ok(WatcherAcquisition::Declined);
            }
            Err(error) => {
                if let Err(rollback) = release_names(connection, &acquired).await {
                    return Err(format!("{error}; {rollback}"));
                }
                return Err(error);
            }
        }
    }
    Ok(WatcherAcquisition::Owned)
}

fn host_names(unique_name: &str) -> BTreeMap<WatcherProtocol, String> {
    WATCHER_PROTOCOLS
        .into_iter()
        .map(|protocol| (protocol, host_name(protocol, unique_name)))
        .collect()
}

async fn acquire_host_names(
    connection: &Connection,
    names: &BTreeMap<WatcherProtocol, String>,
) -> Result<(), String> {
    let mut acquired = Vec::new();
    for protocol in WATCHER_PROTOCOLS {
        let name = &names[&protocol];
        match request_name_without_queue(connection, name).await {
            Ok(true) => acquired.push(name.clone()),
            Ok(false) => {
                release_names(connection, &acquired).await?;
                return Err("cannot acquire a unique status notifier host name".into());
            }
            Err(error) => {
                if let Err(rollback) = release_names(connection, &acquired).await {
                    return Err(format!("{error}; {rollback}"));
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

async fn release_names(connection: &Connection, names: &[String]) -> Result<(), String> {
    for name in names.iter().rev() {
        connection
            .release_name(name.as_str())
            .await
            .map_err(|error| format!("failed to release partially acquired name: {error}"))?;
    }
    Ok(())
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

    fn menu_test_target() -> MenuTarget {
        let mut menus = MenuModel::default();
        let mut next_id = 1;
        menus
            .reconcile(
                vec![(
                    Lifetime {
                        id: "tray1".into(),
                        serial: 1,
                        endpoint: Endpoint {
                            service: ITEM_NAME.into(),
                            path: ITEM_PATH.into(),
                        },
                        owner: ":1.10".into(),
                    },
                    "/Menu".into(),
                )],
                1,
                &mut next_id,
            )
            .unwrap();
        menus.targets().into_values().next().unwrap()
    }

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
        assert!(!valid_pixmaps(&[(4097, 1, vec![0; 4097 * 4])]));
        assert!(!valid_pixmaps(&vec![(1, 1, vec![0; 4]); MAX_PIXMAPS + 1]));
        assert!(!valid_pixmaps(&vec![(128, 128, vec![0; 128 * 128 * 4]); 9]));
    }

    #[test]
    fn pixmaps_are_deduplicated_and_converted_from_network_argb_to_rgba() {
        let pixmaps = normalize_pixmaps(vec![
            (1, 1, vec![128, 10, 20, 30]),
            (1, 1, vec![255, 40, 50, 60]),
            (2, 1, vec![64, 1, 2, 3, 255, 4, 5, 6]),
        ]);
        assert_eq!(pixmaps.len(), 2);
        assert_eq!(pixmaps[0].rgba, vec![10, 20, 30, 128]);
        assert_eq!(pixmaps[1].rgba, vec![1, 2, 3, 64, 4, 5, 6, 255]);
    }

    #[test]
    fn normal_and_attention_pixmaps_share_one_item_budget() {
        let large_pixmap = || QueriedPixmap {
            width: 256,
            height: 256,
            rgba: vec![0; 256 * 256 * 4],
        };
        let mut icon = QueriedIcon {
            name: "normal".into(),
            pixmaps: vec![large_pixmap()],
        };
        let mut attention = QueriedIcon {
            name: "attention".into(),
            pixmaps: vec![
                large_pixmap(),
                QueriedPixmap {
                    width: 1,
                    height: 1,
                    rgba: vec![0; 4],
                },
            ],
        };
        enforce_item_image_budget(&mut icon, &mut attention);
        assert!(icon.pixmaps.is_empty());
        assert!(attention.pixmaps.is_empty());
        assert_eq!(icon.name, "normal");
        assert_eq!(attention.name, "attention");
    }

    #[test]
    fn retained_error_text_is_bounded_on_a_utf8_boundary() {
        let error = sanitize(format!("{}\0", "é".repeat(4096)), 4096);
        assert_eq!(error.len(), 4096);
        assert!(!error.contains('\0'));
    }

    #[test]
    fn menu_property_updates_reject_duplicate_items_and_removal_wins() {
        let update = || HashMap::from([("label".into(), fake_menu_string("updated"))]);
        assert!(
            parse_menu_property_update_values(vec![(1, update()), (1, update())], Vec::new(),)
                .is_err()
        );

        let parsed =
            parse_menu_property_update_values(vec![(1, update())], vec![(1, vec!["label".into()])])
                .unwrap();
        assert_eq!(parsed[&1].raw_label.as_deref(), Some("updated"));
        assert!(parsed[&1].removed.contains("label"));
    }

    #[test]
    fn stopping_interrupts_a_stalled_menu_event_send() {
        let (writer, reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader.set_nonblocking(true).unwrap();
        let reader = Async::new(reader).unwrap();
        let shared = Mutex::new(Shared::default());

        let before = Instant::now();
        let (result, ()) = async_io::block_on(future::zip(
            wait_for_menu_send(future::pending(), &shared, &reader),
            async {
                Timer::after(Duration::from_millis(10)).await;
                lock(&shared).stop = true;
                wake(&writer);
            },
        ));
        assert!(!result.unwrap());
        assert!(before.elapsed() < Duration::from_millis(500));
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
    fn overflowed_dirty_menu_is_scheduled_when_queue_space_returns() {
        let mut menus = MenuModel::default();
        let mut next_id = 1;
        menus
            .reconcile(
                vec![(
                    Lifetime {
                        id: "tray1".into(),
                        serial: 1,
                        endpoint: Endpoint {
                            service: ITEM_NAME.into(),
                            path: ITEM_PATH.into(),
                        },
                        owner: ":1.10".into(),
                    },
                    "/Menu".into(),
                )],
                1,
                &mut next_id,
            )
            .unwrap();
        let target = menus.targets().into_values().next().unwrap();
        menus.open(&target, "view").unwrap();
        let mut queued = (0..MAX_MENU_QUERY_QUEUE)
            .map(|index| {
                (
                    format!("occupied-{index}"),
                    MenuQueryIntent {
                        target: target.clone(),
                        about_to_show: true,
                        fetch_layout: true,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut cursor = None;

        refill_menu_queue(&menus, &mut queued, &mut cursor);
        assert!(!queued.contains_key(&target.id));
        assert!(menus.pending_about_to_show(&target));
        queued.pop_first();
        refill_menu_queue(&menus, &mut queued, &mut cursor);
        assert!(queued.contains_key(&target.id));
        assert!(menus.pending_about_to_show(&target));
    }

    #[test]
    fn explicit_open_is_scheduled_before_lower_id_background_refreshes() {
        let target = menu_test_target();
        let mut queued = (0..MAX_MENU_QUERY_QUEUE - 1)
            .map(|index| {
                (
                    format!("background-{index:03}"),
                    MenuQueryIntent {
                        target: target.clone(),
                        about_to_show: false,
                        fetch_layout: true,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        queued.insert(
            "zz-explicit".into(),
            MenuQueryIntent {
                target,
                about_to_show: true,
                fetch_layout: true,
            },
        );
        let mut cursors = MenuDispatchCursors::default();

        assert_eq!(
            next_menu_query_id(&queued, &BTreeSet::new(), &mut cursors).as_deref(),
            Some("zz-explicit")
        );
    }

    #[test]
    fn background_query_dispatch_makes_progress_past_requeued_low_ids() {
        let target = menu_test_target();
        let request = || MenuQueryIntent {
            target: target.clone(),
            about_to_show: false,
            fetch_layout: true,
        };
        let mut queued = (0..4)
            .map(|index| (format!("low-{index}"), request()))
            .chain([("zz-waiting".into(), request())])
            .collect::<BTreeMap<_, _>>();
        let mut cursors = MenuDispatchCursors::default();

        for expected in ["low-0", "low-1", "low-2", "low-3"] {
            let selected = next_menu_query_id(&queued, &BTreeSet::new(), &mut cursors).unwrap();
            assert_eq!(selected, expected);
            let request = queued.remove(&selected).unwrap();
            queued.insert(selected, request);
        }
        assert_eq!(
            next_menu_query_id(&queued, &BTreeSet::new(), &mut cursors).as_deref(),
            Some("zz-waiting")
        );
    }

    #[test]
    fn consumer_release_does_not_depend_on_command_queue_capacity() {
        let mut menus = MenuModel::default();
        let mut next_id = 1;
        menus
            .reconcile(
                vec![(
                    Lifetime {
                        id: "tray1".into(),
                        serial: 1,
                        endpoint: Endpoint {
                            service: ITEM_NAME.into(),
                            path: ITEM_PATH.into(),
                        },
                        owner: ":1.10".into(),
                    },
                    "/Menu".into(),
                )],
                1,
                &mut next_id,
            )
            .unwrap();
        let target = menus.targets().into_values().next().unwrap();
        let shared = Arc::new(Mutex::new(Shared {
            online: true,
            menu_consumers: BTreeSet::from(["view".into()]),
            menu_known_consumers: BTreeSet::from(["view".into()]),
            ..Shared::default()
        }));
        lock(&shared).commands.extend((0..MAX_COMMANDS).map(|_| {
            Command::Menu(MenuCommand {
                target: target.clone(),
                action: SystemTrayMenuAction::Activate,
                consumer: "other".into(),
            })
        }));
        let (writer, _reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let tray = SystemTray {
            shared: shared.clone(),
            wake: writer,
            worker: None,
        };

        tray.release_menu_consumer("view").unwrap();
        let shared = lock(&shared);
        assert_eq!(shared.commands.len(), MAX_COMMANDS);
        assert!(shared.releases.contains("view"));
        drop(shared);
    }

    #[test]
    fn pending_consumer_release_rejects_and_purges_late_commands() {
        let target = menu_test_target();
        let shared = Arc::new(Mutex::new(Shared {
            online: true,
            menu_targets: BTreeMap::from([(target.id.clone(), target.clone())]),
            menu_known_consumers: BTreeSet::from(["retired-view".into()]),
            releases: BTreeSet::from(["retired-view".into()]),
            ..Shared::default()
        }));
        let (writer, _reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let tray = SystemTray {
            shared: shared.clone(),
            wake: writer,
            worker: None,
        };

        assert_eq!(
            tray.send_menu(SystemTrayMenuAction::Open, &target.id, "retired-view"),
            Err("system tray menu consumer is being released")
        );
        lock(&shared).commands.extend([
            Command::Menu(MenuCommand {
                target: target.clone(),
                action: SystemTrayMenuAction::Open,
                consumer: "retired-view".into(),
            }),
            Command::Menu(MenuCommand {
                target,
                action: SystemTrayMenuAction::Activate,
                consumer: "current-view".into(),
            }),
        ]);

        consume_menu_release(&shared, "retired-view");

        let shared = lock(&shared);
        assert!(!shared.menu_known_consumers.contains("retired-view"));
        assert!(!shared.releases.contains("retired-view"));
        assert_eq!(shared.commands.len(), 1);
        assert!(matches!(
            &shared.commands[0],
            Command::Menu(command) if command.consumer == "current-view"
        ));
    }

    #[test]
    fn release_while_the_watcher_is_unavailable_forgets_the_consumer() {
        let target = menu_test_target();
        let shared = Arc::new(Mutex::new(Shared {
            online: false,
            menu_known_consumers: BTreeSet::from(["retired-view".into()]),
            releases: BTreeSet::from(["retired-view".into()]),
            commands: VecDeque::from([
                Command::Menu(MenuCommand {
                    target: target.clone(),
                    action: SystemTrayMenuAction::Open,
                    consumer: "retired-view".into(),
                }),
                Command::Menu(MenuCommand {
                    target,
                    action: SystemTrayMenuAction::Activate,
                    consumer: "current-view".into(),
                }),
            ]),
            ..Shared::default()
        }));
        let (writer, _reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let tray = SystemTray {
            shared: shared.clone(),
            wake: writer,
            worker: None,
        };

        tray.release_menu_consumer("retired-view").unwrap();

        let shared = lock(&shared);
        assert!(!shared.menu_known_consumers.contains("retired-view"));
        assert!(!shared.releases.contains("retired-view"));
        assert_eq!(shared.commands.len(), 1);
        assert!(matches!(
            &shared.commands[0],
            Command::Menu(command) if command.consumer == "current-view"
        ));
    }

    #[test]
    fn closing_the_last_lease_cancels_queued_open_work() {
        let mut menus = MenuModel::default();
        let mut next_id = 1;
        menus
            .reconcile(
                vec![(
                    Lifetime {
                        id: "tray1".into(),
                        serial: 1,
                        endpoint: Endpoint {
                            service: ITEM_NAME.into(),
                            path: ITEM_PATH.into(),
                        },
                        owner: ":1.10".into(),
                    },
                    "/Menu".into(),
                )],
                1,
                &mut next_id,
            )
            .unwrap();
        let target = menus.targets().into_values().next().unwrap();
        let decision = menus.open(&target, "view").unwrap();
        let mut queries = BTreeMap::new();
        assert!(queue_menu_query(
            &menus,
            &mut queries,
            target.clone(),
            true,
            decision.needs_layout,
        ));
        assert!(queries.contains_key(&target.id));

        let closed = menus.close(&target, "view").unwrap().closed;
        let mut events = VecDeque::new();
        queue_closed_menu_events(&mut queries, &mut events, closed).unwrap();
        assert!(queries.is_empty());
        assert!(!menus.can_query(&target));
        assert_eq!(events.len(), 1);
        assert_eq!(events.front().unwrap().target.id, target.id);
        assert_eq!(events.front().unwrap().event, "closed");
    }

    #[test]
    fn registration_change_purges_queued_work_and_closes_old_lifetime() {
        let item_lifetime = |serial| Lifetime {
            id: "tray1".into(),
            serial,
            endpoint: Endpoint {
                service: ITEM_NAME.into(),
                path: ITEM_PATH.into(),
            },
            owner: ":1.10".into(),
        };
        let mut menus = MenuModel::default();
        let mut next_id = 1;
        menus
            .reconcile(vec![(item_lifetime(1), "/Menu".into())], 1, &mut next_id)
            .unwrap();
        let old = menus.targets().into_values().next().unwrap();
        let decision = menus.open(&old, "view").unwrap();
        let mut queries = BTreeMap::new();
        queue_menu_query(
            &menus,
            &mut queries,
            old.clone(),
            true,
            decision.needs_layout,
        );

        menus
            .reconcile(vec![(item_lifetime(1), "/Other".into())], 1, &mut next_id)
            .unwrap();
        retain_current_menu_queries(&menus, &mut queries);
        let mut events = VecDeque::new();
        queue_retired_menu_events(&mut menus, &mut queries, &mut events).unwrap();

        assert!(queries.is_empty());
        assert_eq!(events.len(), 1);
        assert_eq!(events.front().unwrap().target, old);
        assert_ne!(menus.targets().into_values().next().unwrap().id, old.id);
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
        let observer = async_io::block_on(
            zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build(),
        )
        .unwrap();
        let dbus = async_io::block_on(DBusProxy::new(&observer)).unwrap();
        assert_eq!(bus_name_count(&dbus, KDE_HOST_PREFIX), 1);
        assert_eq!(bus_name_count(&dbus, FREEDESKTOP_HOST_PREFIX), 1);

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
        let icon = snapshot.items[0].icon.as_ref().unwrap();
        let first_icon_id = icon.id().to_owned();
        assert_eq!(icon.candidates().len(), 2);
        assert_eq!(icon.candidates()[0].icon(), Some("thing-item-attention"));
        assert!(icon.candidates()[0].pixmaps().is_empty());
        assert_eq!(icon.candidates()[1].icon(), Some("thing-item"));
        assert_eq!(icon.candidates()[1].pixmaps()[0].rgba(), &[1, 2, 3, 255]);

        let menu_id = snapshot.items[0].menu.as_ref().unwrap().id.clone();
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "output-a:1")
            .unwrap();
        let menu_snapshot = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .is_some_and(|menu| menu.loaded && menu.children.len() == 2)
        });
        let menu = menu_snapshot.items[0].menu.as_ref().unwrap();
        assert_eq!(menu.children[0].label, "Recent");
        assert_eq!(menu.children[0].access_key.as_deref(), Some("R"));
        assert!(menu.children[0].has_submenu);
        let submenu_id = menu.children[0].id.clone();
        let quit_id = menu.children[1].id.clone();
        assert_eq!(
            tray.send_menu(SystemTrayMenuAction::Open, &quit_id, ""),
            Err("invalid system tray menu consumer")
        );
        tray.send_menu(SystemTrayMenuAction::Open, &quit_id, "misuse")
            .unwrap();
        state.lock().unwrap().title = "after invalid menu open".into();
        emit_item_signal(&item_connection, "NewTitle", &());
        let after_misuse = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .is_some_and(|item| item.title == "after invalid menu open")
        });
        assert_eq!(after_misuse.items[0].id, first_id);
        assert_eq!(after_misuse.items[0].menu.as_ref().unwrap().id, menu_id);
        tray.send_menu(SystemTrayMenuAction::Open, &submenu_id, "output-a:1")
            .unwrap();
        let submenu_snapshot = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .and_then(|menu| menu.children.first())
                .is_some_and(|submenu| submenu.loaded && submenu.children.len() == 1)
        });
        assert_eq!(
            submenu_snapshot.items[0].menu.as_ref().unwrap().children[0].children[0].label,
            "Document _one"
        );
        let mut updated = HashMap::new();
        updated.insert("label".into(), fake_menu_string("_Exit now"));
        updated.insert("enabled".into(), OwnedValue::from(false));
        emit_menu_properties(
            &item_connection,
            vec![(2, updated)],
            vec![(2, vec!["enabled".into()])],
        );
        let property_snapshot = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .and_then(|menu| menu.children.get(1))
                .is_some_and(|item| item.label == "Exit now")
        });
        let updated_quit = &property_snapshot.items[0].menu.as_ref().unwrap().children[1];
        assert_eq!(updated_quit.id, quit_id);
        assert!(updated_quit.enabled);
        tray.send_menu(SystemTrayMenuAction::Activate, &quit_id, "output-a:1")
            .unwrap();
        tray.send_menu(SystemTrayMenuAction::Close, &menu_id, "output-a:1")
            .unwrap();
        wait_until(|| calls.menu_clicked.load(Ordering::Acquire) == 1);
        wait_until(|| calls.menu_opened.load(Ordering::Acquire) == 2);
        wait_until(|| calls.menu_closed.load(Ordering::Acquire) == 2);
        assert_eq!(calls.menu_about_to_show.load(Ordering::Acquire), 2);

        calls.menu_layout_version.store(0, Ordering::Release);
        calls.menu_about_updates.store(true, Ordering::Release);
        emit_menu_layout(&item_connection, 2, 0);
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "output-b:1")
            .unwrap();
        let reopened = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .and_then(|menu| menu.children.get(1))
                .is_some_and(|item| item.label == "Exit")
        });
        assert_eq!(
            reopened.items[0].menu.as_ref().unwrap().children[1].id,
            quit_id
        );
        assert!(calls.menu_layout_calls.load(Ordering::Acquire) >= 4);
        tray.release_menu_consumer("output-b:1").unwrap();
        wait_until(|| calls.menu_closed.load(Ordering::Acquire) == 3);

        calls
            .menu_error_bytes
            .store(MAX_MENU_BODY_BYTES + 1024, Ordering::Release);
        emit_menu_layout(&item_connection, 3, 0);
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "error-view")
            .unwrap();
        let bounded_error = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .is_some_and(|menu| menu.error == "system tray menu reply exceeds 512 KiB")
        });
        assert_eq!(bounded_error.items[0].id, first_id);
        assert_eq!(bounded_error.items[0].menu.as_ref().unwrap().id, menu_id);
        assert_eq!(
            bounded_error.items[0].menu.as_ref().unwrap().children[1].id,
            quit_id
        );
        calls.menu_error_bytes.store(0, Ordering::Release);
        tray.release_menu_consumer("error-view").unwrap();
        wait_until(|| calls.menu_closed.load(Ordering::Acquire) == 4);

        state.lock().unwrap().icon_red = 9;
        emit_item_signal(&item_connection, "NewIcon", &());
        let icon_updated = wait_for(&tray, |snapshot| {
            snapshot.items.first().is_some_and(|item| {
                item.icon
                    .as_ref()
                    .is_some_and(|icon| icon.candidates()[1].pixmaps()[0].rgba() == [9, 2, 3, 255])
            })
        });
        let updated_icon_id = icon_updated.items[0].icon.as_ref().unwrap().id().to_owned();
        assert_ne!(updated_icon_id, first_icon_id);

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
        assert_eq!(
            updated.items[0].icon.as_ref().unwrap().id(),
            updated_icon_id
        );

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
        assert_ne!(
            replacement.items[0].icon.as_ref().unwrap().id(),
            updated_icon_id
        );
        let replacement_menu = replacement.items[0].menu.as_ref().unwrap().id.clone();
        tray.send_menu(
            SystemTrayMenuAction::Open,
            &replacement_menu,
            "replacement-view",
        )
        .unwrap();
        wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .is_some_and(|menu| menu.loaded)
        });
        assert!(async_io::block_on(item_connection.release_name(ITEM_NAME)).unwrap());
        wait_for(&tray, |snapshot| {
            snapshot.available && snapshot.items.is_empty()
        });
        wait_until(|| calls.menu_closed.load(Ordering::Acquire) == 5);

        drop(tray);
        drop(observer);
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
        assert_eq!(bus_name_count(&dbus, KDE_HOST_PREFIX), 0);
        assert_eq!(bus_name_count(&dbus, FREEDESKTOP_HOST_PREFIX), 1);

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
    fn dropping_the_worker_interrupts_a_stalled_menu_query() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);
        let calls = Arc::new(FakeCalls::default());
        calls.menu_stall.store(true, Ordering::Release);
        let item_connection = fake_item(
            &address,
            Arc::new(Mutex::new(FakeItemState::default())),
            calls.clone(),
        );
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let menu_id = snapshot.items[0].menu.as_ref().unwrap().id.clone();
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "view")
            .unwrap();
        wait_until(|| calls.menu_started.load(Ordering::Acquire));

        let before = Instant::now();
        drop(tray);
        assert!(before.elapsed() < Duration::from_millis(500));

        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn stalled_about_to_show_does_not_discard_a_completed_speculative_layout() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);
        let calls = Arc::new(FakeCalls::default());
        calls.menu_about_stall.store(true, Ordering::Release);
        let item_connection = fake_item(
            &address,
            Arc::new(Mutex::new(FakeItemState::default())),
            calls.clone(),
        );
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let menu_id = snapshot.items[0].menu.as_ref().unwrap().id.clone();

        let before = Instant::now();
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "view")
            .unwrap();
        wait_until(|| calls.menu_about_started.load(Ordering::Acquire));
        let loaded = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .is_some_and(|menu| menu.loaded && menu.children.len() == 2)
        });
        assert!(before.elapsed() >= METHOD_TIMEOUT);
        assert!(loaded.items[0].menu.as_ref().unwrap().error.is_empty());

        drop(tray);
        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn clean_branch_about_to_show_leaves_time_for_the_required_layout() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);
        let calls = Arc::new(FakeCalls::default());
        let item_connection = fake_item(
            &address,
            Arc::new(Mutex::new(FakeItemState::default())),
            calls.clone(),
        );
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let menu_id = snapshot.items[0].menu.as_ref().unwrap().id.clone();
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "first-view")
            .unwrap();
        wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .is_some_and(|menu| menu.loaded)
        });
        tray.release_menu_consumer("first-view").unwrap();
        wait_until(|| calls.menu_closed.load(Ordering::Acquire) == 1);

        calls.menu_layout_calls.store(0, Ordering::Release);
        calls.menu_about_to_show.store(0, Ordering::Release);
        calls.menu_about_updates.store(true, Ordering::Release);
        calls.menu_about_delay_ms.store(200, Ordering::Release);
        calls.menu_layout_delay_ms.store(1000, Ordering::Release);
        let before = Instant::now();
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "second-view")
            .unwrap();
        let refreshed = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .and_then(|menu| menu.children.get(1))
                .is_some_and(|item| item.label == "Exit")
        });
        assert!(before.elapsed() >= Duration::from_millis(1100));
        assert!(before.elapsed() < METHOD_TIMEOUT);
        assert_eq!(calls.menu_about_to_show.load(Ordering::Acquire), 1);
        assert_eq!(calls.menu_layout_calls.load(Ordering::Acquire), 1);
        assert!(refreshed.items[0].menu.as_ref().unwrap().error.is_empty());

        drop(tray);
        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn release_after_an_open_is_popped_prevents_the_lease_and_query() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);
        let calls = Arc::new(FakeCalls::default());
        let item_connection = fake_item(
            &address,
            Arc::new(Mutex::new(FakeItemState::default())),
            calls.clone(),
        );
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let menu_id = snapshot.items[0].menu.as_ref().unwrap().id.clone();
        let (gate, reached, resume) = menu_test_gate();
        lock(&tray.shared).popped_open_gate = Some(gate);

        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "retired-view")
            .unwrap();
        reached.recv_timeout(Duration::from_secs(8)).unwrap();
        tray.release_menu_consumer("retired-view").unwrap();
        resume.send(()).unwrap();
        wait_until(|| {
            let shared = lock(&tray.shared);
            !shared.releases.contains("retired-view")
                && !shared.menu_known_consumers.contains("retired-view")
                && !shared.menu_consumers.contains("retired-view")
        });
        thread::sleep(Duration::from_millis(50));
        assert_eq!(calls.menu_opened.load(Ordering::Acquire), 0);
        assert_eq!(calls.menu_about_to_show.load(Ordering::Acquire), 0);
        assert_eq!(calls.menu_layout_calls.load(Ordering::Acquire), 0);

        drop(tray);
        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn release_after_open_insertion_closes_only_previously_announced_branches() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);
        let calls = Arc::new(FakeCalls::default());
        let item_connection = fake_item(
            &address,
            Arc::new(Mutex::new(FakeItemState::default())),
            calls.clone(),
        );
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let menu_id = snapshot.items[0].menu.as_ref().unwrap().id.clone();
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "retired-view")
            .unwrap();
        let loaded = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .is_some_and(|menu| menu.loaded && menu.children.len() == 2)
        });
        wait_until(|| calls.menu_opened.load(Ordering::Acquire) == 1);
        let submenu_id = loaded.items[0].menu.as_ref().unwrap().children[0]
            .id
            .clone();
        let (gate, reached, resume) = menu_test_gate();
        lock(&tray.shared).open_inserted_gate = Some(gate);

        tray.send_menu(SystemTrayMenuAction::Open, &submenu_id, "retired-view")
            .unwrap();
        reached.recv_timeout(Duration::from_secs(8)).unwrap();
        tray.release_menu_consumer("retired-view").unwrap();
        resume.send(()).unwrap();
        wait_until(|| calls.menu_closed.load(Ordering::Acquire) == 1);
        wait_until(|| {
            let shared = lock(&tray.shared);
            !shared.releases.contains("retired-view")
                && !shared.menu_known_consumers.contains("retired-view")
                && !shared.menu_consumers.contains("retired-view")
        });
        assert_eq!(calls.menu_opened.load(Ordering::Acquire), 1);
        assert_eq!(calls.menu_about_to_show.load(Ordering::Acquire), 1);
        assert_eq!(calls.menu_layout_calls.load(Ordering::Acquire), 1);

        drop(tray);
        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn release_after_an_activation_is_popped_prevents_the_click() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);
        let state = Arc::new(Mutex::new(FakeItemState::default()));
        let calls = Arc::new(FakeCalls::default());
        let item_connection = fake_item(&address, state.clone(), calls.clone());
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let menu_id = snapshot.items[0].menu.as_ref().unwrap().id.clone();
        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "retired-view")
            .unwrap();
        let loaded = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .and_then(|item| item.menu.as_ref())
                .is_some_and(|menu| menu.loaded && menu.children.len() == 2)
        });
        let item_id = loaded.items[0].id.clone();
        let quit_id = loaded.items[0].menu.as_ref().unwrap().children[1]
            .id
            .clone();
        let (gate, reached, resume) = menu_test_gate();
        lock(&tray.shared).popped_activate_gate = Some(gate);

        tray.send_menu(SystemTrayMenuAction::Activate, &quit_id, "retired-view")
            .unwrap();
        reached.recv_timeout(Duration::from_secs(8)).unwrap();
        tray.release_menu_consumer("retired-view").unwrap();
        resume.send(()).unwrap();
        wait_until(|| calls.menu_closed.load(Ordering::Acquire) == 1);
        assert_eq!(calls.menu_clicked.load(Ordering::Acquire), 0);

        state.lock().unwrap().title = "still connected".into();
        emit_item_signal(&item_connection, "NewTitle", &());
        let refreshed = wait_for(&tray, |snapshot| {
            snapshot
                .items
                .first()
                .is_some_and(|item| item.title == "still connected")
        });
        assert_eq!(refreshed.items[0].id, item_id);
        assert_eq!(refreshed.items[0].menu.as_ref().unwrap().id, menu_id);

        drop(tray);
        drop(item_connection);
        bus.stop();
    }

    #[test]
    fn release_after_opened_is_sent_prevents_about_to_show() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut bus = Bus::start(&address);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let tray =
            SystemTray::start_at(SystemTrayMode::OwnWatcher, Some(address.clone()), ping).unwrap();
        wait_for(&tray, |snapshot| snapshot.available);
        let calls = Arc::new(FakeCalls::default());
        let item_connection = fake_item(
            &address,
            Arc::new(Mutex::new(FakeItemState::default())),
            calls.clone(),
        );
        register_item(&item_connection, KDE_PROTOCOL, ITEM_NAME);
        let snapshot = wait_for(&tray, |snapshot| snapshot.items.len() == 1);
        let menu_id = snapshot.items[0].menu.as_ref().unwrap().id.clone();
        let (gate, reached, resume) = menu_test_gate();
        lock(&tray.shared).opened_sent_gate = Some(gate);

        tray.send_menu(SystemTrayMenuAction::Open, &menu_id, "retired-view")
            .unwrap();
        reached.recv_timeout(Duration::from_secs(8)).unwrap();
        tray.release_menu_consumer("retired-view").unwrap();
        resume.send(()).unwrap();
        wait_until(|| calls.menu_closed.load(Ordering::Acquire) == 1);
        assert_eq!(calls.menu_opened.load(Ordering::Acquire), 1);
        assert_eq!(calls.menu_about_to_show.load(Ordering::Acquire), 0);
        assert_eq!(calls.menu_layout_calls.load(Ordering::Acquire), 0);

        drop(tray);
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
        let observer = async_io::block_on(
            zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build(),
        )
        .unwrap();
        let dbus = async_io::block_on(DBusProxy::new(&observer)).unwrap();
        assert_eq!(bus_name_count(&dbus, KDE_HOST_PREFIX), 0);
        assert_eq!(bus_name_count(&dbus, FREEDESKTOP_HOST_PREFIX), 0);

        let registry = Arc::new(Mutex::new(WatcherRegistry::default()));
        let watcher = external_watcher(&address, KDE_PROTOCOL, registry.clone());
        let snapshot = wait_for(&tray, |snapshot| snapshot.available);
        assert!(!snapshot.watcher_owned);
        wait_until(|| !lock(&registry).hosts.is_empty());
        assert_eq!(bus_name_count(&dbus, KDE_HOST_PREFIX), 1);
        assert_eq!(bus_name_count(&dbus, FREEDESKTOP_HOST_PREFIX), 0);

        drop(watcher);
        drop(tray);
        drop(observer);
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
        icon_red: u8,
    }

    impl Default for FakeItemState {
        fn default() -> Self {
            Self {
                title: "first title".into(),
                icon_red: 1,
            }
        }
    }

    #[derive(Default)]
    struct FakeCalls {
        activate: AtomicUsize,
        secondary: AtomicUsize,
        context: AtomicUsize,
        menu_opened: AtomicUsize,
        menu_closed: AtomicUsize,
        menu_clicked: AtomicUsize,
        menu_about_to_show: AtomicUsize,
        menu_layout_version: AtomicUsize,
        menu_layout_calls: AtomicUsize,
        menu_layout_delay_ms: AtomicUsize,
        menu_error_bytes: AtomicUsize,
        menu_about_updates: AtomicBool,
        menu_about_delay_ms: AtomicUsize,
        menu_about_stall: AtomicBool,
        menu_about_started: AtomicBool,
        menu_stall: AtomicBool,
        menu_started: AtomicBool,
        coordinates: Mutex<Vec<(i32, i32)>>,
    }

    struct FakeItem {
        state: Arc<Mutex<FakeItemState>>,
        calls: Arc<FakeCalls>,
    }

    struct FakeFreedesktopItem {
        calls: Arc<FakeCalls>,
    }

    struct FakeMenu {
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

    #[zbus::interface(name = "com.canonical.dbusmenu")]
    impl FakeMenu {
        async fn get_layout(
            &self,
            parent: i32,
            _depth: i32,
            _properties: Vec<String>,
        ) -> zbus::fdo::Result<(u32, RawMenuLayout)> {
            self.calls.menu_layout_calls.fetch_add(1, Ordering::Release);
            if self.calls.menu_stall.load(Ordering::Acquire) {
                self.calls.menu_started.store(true, Ordering::Release);
                future::pending().await
            }
            let delay = self.calls.menu_layout_delay_ms.load(Ordering::Acquire);
            if delay != 0 {
                Timer::after(Duration::from_millis(delay as u64)).await;
            }
            let error_bytes = self.calls.menu_error_bytes.load(Ordering::Acquire);
            if error_bytes != 0 {
                return Err(zbus::fdo::Error::Failed("x".repeat(error_bytes)));
            }
            let version = self.calls.menu_layout_version.load(Ordering::Acquire);
            let children = match parent {
                0 => vec![
                    fake_menu_item(1, "_Recent", true),
                    fake_menu_item(2, if version == 0 { "_Quit" } else { "_Exit" }, false),
                ],
                1 => vec![fake_menu_item(3, "Document __one", false)],
                _ => Vec::new(),
            };
            Ok((
                1,
                (
                    parent,
                    HashMap::new(),
                    children.into_iter().map(fake_menu_value).collect(),
                ),
            ))
        }

        async fn about_to_show(&self, _id: i32) -> bool {
            self.calls
                .menu_about_to_show
                .fetch_add(1, Ordering::Release);
            if self.calls.menu_about_stall.load(Ordering::Acquire) {
                self.calls.menu_about_started.store(true, Ordering::Release);
                future::pending().await
            }
            let delay = self.calls.menu_about_delay_ms.load(Ordering::Acquire);
            if delay != 0 {
                Timer::after(Duration::from_millis(delay as u64)).await;
            }
            if self.calls.menu_about_updates.swap(false, Ordering::AcqRel) {
                if delay == 0 {
                    Timer::after(Duration::from_millis(20)).await;
                }
                self.calls.menu_layout_version.store(1, Ordering::Release);
                true
            } else {
                false
            }
        }

        fn event(&self, _id: i32, event: String, _data: OwnedValue, _timestamp: u32) {
            match event.as_str() {
                "opened" => &self.calls.menu_opened,
                "closed" => &self.calls.menu_closed,
                "clicked" => &self.calls.menu_clicked,
                _ => return,
            }
            .fetch_add(1, Ordering::Release);
        }

        #[zbus(signal)]
        async fn items_properties_updated(
            emitter: &SignalEmitter<'_>,
            updated: Vec<(i32, HashMap<String, OwnedValue>)>,
            removed: Vec<(i32, Vec<String>)>,
        ) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn layout_updated(
            emitter: &SignalEmitter<'_>,
            revision: u32,
            parent: i32,
        ) -> zbus::Result<()>;
    }

    fn fake_menu_item(id: i32, label: &str, submenu: bool) -> RawMenuLayout {
        let mut properties = HashMap::new();
        properties.insert("label".into(), fake_menu_string(label));
        if submenu {
            properties.insert("children-display".into(), fake_menu_string("submenu"));
        }
        (id, properties, Vec::new())
    }

    fn fake_menu_string(value: &str) -> OwnedValue {
        OwnedValue::try_from(zbus::zvariant::Value::from(value)).unwrap()
    }

    fn fake_menu_value(layout: RawMenuLayout) -> OwnedValue {
        let structure = zbus::zvariant::Structure::from(layout);
        OwnedValue::try_from(zbus::zvariant::Value::from(structure)).unwrap()
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
            vec![(1, 1, vec![255, lock(&self.state).icon_red, 2, 3])]
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
                .serve_at(
                    ITEM_PATH,
                    FakeItem {
                        state,
                        calls: calls.clone(),
                    },
                )?
                .serve_at("/Menu", FakeMenu { calls })?
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

    fn emit_menu_properties(
        connection: &Connection,
        updated: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed: Vec<(i32, Vec<String>)>,
    ) {
        async_io::block_on(async {
            let emitter = SignalEmitter::new(connection, "/Menu")?;
            FakeMenu::items_properties_updated(&emitter, updated, removed).await
        })
        .unwrap();
    }

    fn emit_menu_layout(connection: &Connection, revision: u32, parent: i32) {
        async_io::block_on(async {
            let emitter = SignalEmitter::new(connection, "/Menu")?;
            FakeMenu::layout_updated(&emitter, revision, parent).await
        })
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

    fn bus_name_count(dbus: &DBusProxy<'_>, prefix: &str) -> usize {
        async_io::block_on(dbus.list_names())
            .unwrap()
            .into_iter()
            .filter(|name| name.as_str().starts_with(prefix))
            .count()
    }

    fn menu_test_gate() -> (
        Arc<MenuTestGate>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached_sender, reached_receiver) = std::sync::mpsc::channel();
        let (resume_sender, resume_receiver) = std::sync::mpsc::channel();
        (
            Arc::new(MenuTestGate {
                reached: reached_sender,
                resume: Mutex::new(resume_receiver),
            }),
            reached_receiver,
            resume_sender,
        )
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
