//! read-only UPower device discovery and state.

mod model;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::Duration,
};

use async_io::{Async, Timer};
use calloop::ping::Ping;
use futures_lite::future;
use futures_util::{FutureExt, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
use thing_runtime::{PowerDevice, PowerSnapshot};
use zbus::{
    Connection, MatchRule, MessageStream,
    fdo::DBusProxy,
    message::{Message, Type},
    names::BusName,
    zvariant::{OwnedObjectPath, OwnedValue},
};

use model::Device;

const UPOWER_NAME: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_INTERFACE: &str = "org.freedesktop.UPower";
const DEVICE_INTERFACE: &str = "org.freedesktop.UPower.Device";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const DISPLAY_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE_PATH_PREFIX: &str = "/org/freedesktop/UPower/devices/";
const MAX_DEVICES: usize = 128;
const MAX_QUERIES: usize = 4;
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const OWNER_QUEUE_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 256;
const QUERY_ATTEMPTS: usize = 3;
const METHOD_TIMEOUT: Duration = Duration::from_secs(2);
const QUERY_RETRY_DELAY: Duration = Duration::from_millis(100);
const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Shared {
    stop: bool,
    latest: Option<PowerSnapshot>,
}

pub(crate) struct Power {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl Power {
    pub fn start(ping: Ping) -> io::Result<Self> {
        Self::start_at(None, ping)
    }

    /// a service without a worker, for host tests that must not reach the
    /// desktop session.
    #[cfg(test)]
    pub(crate) fn inert() -> io::Result<Self> {
        let (wake, _) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        Ok(Self {
            shared: Default::default(),
            wake,
            worker: None,
        })
    }

    fn start_at(address: Option<String>, ping: Ping) -> io::Result<Self> {
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("thing-power".into())
            .spawn(move || {
                let mut previous = PowerSnapshot::default();
                let mut last_error = String::new();
                loop {
                    if stopping(&state) {
                        break;
                    }
                    let result = async_io::block_on(bus_session(
                        address.as_deref(),
                        &state,
                        &reader,
                        |snapshot| publish(&state, &ping, &mut previous, snapshot),
                    ));
                    if stopping(&state) {
                        break;
                    }
                    let error = sanitize(
                        result
                            .err()
                            .unwrap_or_else(|| "power connection ended".into()),
                        4096,
                    );
                    if error != last_error {
                        eprintln!("thing: power unavailable: {error}; retrying in 1 second");
                        last_error = error.clone();
                    }
                    publish(
                        &state,
                        &ping,
                        &mut previous,
                        PowerSnapshot {
                            error,
                            ..PowerSnapshot::default()
                        },
                    );
                    retry_wait(&reader, RETRY_DELAY);
                }
            })?;
        Ok(Self {
            shared,
            wake,
            worker: Some(worker),
        })
    }

    pub fn take_snapshot(&self) -> Option<PowerSnapshot> {
        lock(&self.shared).latest.take()
    }
}

impl Drop for Power {
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
    previous: &mut PowerSnapshot,
    snapshot: PowerSnapshot,
) {
    if *previous != snapshot {
        *previous = snapshot.clone();
        lock(shared).latest = Some(snapshot);
        ping.ping();
    }
}

async fn bus_session(
    address: Option<&str>,
    shared: &Mutex<Shared>,
    wake_stream: &UnixStream,
    mut publish: impl FnMut(PowerSnapshot),
) -> Result<(), String> {
    let reader = wake_stream.try_clone().map_err(|error| error.to_string())?;
    let wake_source = Async::new(reader).map_err(|error| error.to_string())?;
    let builder = match address {
        Some(address) => zbus::connection::Builder::address(address),
        None => zbus::connection::Builder::system(),
    }
    .map_err(|error| error.to_string())?;
    let connection = future::race(
        async {
            builder
                .method_timeout(METHOD_TIMEOUT)
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
                Err::<Option<Connection>, String>("power connection timed out".into())
            },
        ),
    )
    .await?;
    let Some(connection) = connection else {
        return if stopping(shared) {
            Ok(())
        } else {
            Err("power connection interrupted".into())
        };
    };
    if stopping(shared) {
        return Ok(());
    }

    let name_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(|error| error.to_string())?
        .interface("org.freedesktop.DBus")
        .map_err(|error| error.to_string())?
        .member("NameOwnerChanged")
        .map_err(|error| error.to_string())?
        .add_arg(UPOWER_NAME)
        .map_err(|error| error.to_string())?
        .build();
    let mut names =
        MessageStream::for_match_rule(name_rule, &connection, Some(OWNER_QUEUE_CAPACITY))
            .await
            .map_err(|error| error.to_string())?;
    let dbus = DBusProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    let name = BusName::try_from(UPOWER_NAME).map_err(|error| error.to_string())?;
    let mut owner = match dbus.get_name_owner(name).await {
        Ok(owner) => Some(owner.to_string()),
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => None,
        Err(error) => return Err(error.to_string()),
    };
    publish(PowerSnapshot::default());

    loop {
        if stopping(shared) {
            return Ok(());
        }
        if let Some(change) = drain_owner_events(&mut names)? {
            owner = change;
            publish(PowerSnapshot::default());
        }
        let Some(current) = owner.clone() else {
            let input = future::race(async { WaitingInput::Owner(names.next().await) }, async {
                WaitingInput::Wake(wake_source.readable().await)
            })
            .await;
            match input {
                WaitingInput::Owner(Some(Ok(message))) => {
                    if let Some(change) = owner_change(message)? {
                        owner = change;
                        publish(PowerSnapshot::default());
                    }
                }
                WaitingInput::Owner(Some(Err(error))) => return Err(error.to_string()),
                WaitingInput::Owner(None) => {
                    return Err("power owner subscription ended".into());
                }
                WaitingInput::Wake(result) => {
                    result.map_err(|error| error.to_string())?;
                    drain(wake_source.get_ref());
                    if stopping(shared) {
                        return Ok(());
                    }
                }
            }
            continue;
        };
        match owner_session(
            &connection,
            &current,
            &mut names,
            shared,
            &wake_source,
            &mut publish,
        )
        .await?
        {
            OwnerOutcome::Changed(next) => {
                owner = next;
                publish(PowerSnapshot::default());
            }
            OwnerOutcome::Stopped => return Ok(()),
        }
    }
}

enum WaitingInput {
    Owner(Option<zbus::Result<Message>>),
    Wake(io::Result<()>),
}

enum OwnerOutcome {
    Changed(Option<String>),
    Stopped,
}

async fn owner_session(
    connection: &Connection,
    owner: &str,
    names: &mut MessageStream,
    shared: &Mutex<Shared>,
    wake_source: &Async<UnixStream>,
    publish: &mut impl FnMut(PowerSnapshot),
) -> Result<OwnerOutcome, String> {
    let lifecycle_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(owner)
        .map_err(|error| error.to_string())?
        .path(UPOWER_PATH)
        .map_err(|error| error.to_string())?
        .interface(UPOWER_INTERFACE)
        .map_err(|error| error.to_string())?
        .build();
    let mut lifecycle =
        MessageStream::for_match_rule(lifecycle_rule, connection, Some(EVENT_QUEUE_CAPACITY))
            .await
            .map_err(|error| error.to_string())?;
    let property_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(owner)
        .map_err(|error| error.to_string())?
        .path_namespace(UPOWER_PATH)
        .map_err(|error| error.to_string())?
        .interface(PROPERTIES_INTERFACE)
        .map_err(|error| error.to_string())?
        .member("PropertiesChanged")
        .map_err(|error| error.to_string())?
        .build();
    let mut properties =
        MessageStream::for_match_rule(property_rule, connection, Some(EVENT_QUEUE_CAPACITY))
            .await
            .map_err(|error| error.to_string())?;
    if let Some(change) = drain_owner_events(names)? {
        return Ok(OwnerOutcome::Changed(change));
    }

    let startup = future::race(
        async { StartupInput::Ready(Box::new(bootstrap(connection, owner).await)) },
        future::race(async { StartupInput::Owner(names.next().await) }, async {
            StartupInput::Wake(wake_source.readable().await)
        }),
    )
    .await;
    let initial = match startup {
        StartupInput::Ready(result) => (*result)?,
        StartupInput::Owner(Some(Ok(message))) => {
            return Ok(OwnerOutcome::Changed(
                owner_change(message)?.unwrap_or_else(|| Some(owner.into())),
            ));
        }
        StartupInput::Owner(Some(Err(error))) => return Err(error.to_string()),
        StartupInput::Owner(None) => return Err("power owner subscription ended".into()),
        StartupInput::Wake(result) => {
            result.map_err(|error| error.to_string())?;
            drain(wake_source.get_ref());
            return if stopping(shared) {
                Ok(OwnerOutcome::Stopped)
            } else {
                Ok(OwnerOutcome::Changed(Some(owner.into())))
            };
        }
    };
    if let Some(change) = drain_owner_events(names)? {
        return Ok(OwnerOutcome::Changed(change));
    }

    let mut state = OwnerState {
        on_battery: initial.on_battery,
        display_path: initial.display_path,
        display: Some(initial.display),
        ..OwnerState::default()
    };
    let mut queued = BTreeMap::new();
    for path in initial.paths {
        add_path(&mut state, &mut queued, path)?;
    }
    publish(state.snapshot());
    let mut running = BTreeSet::new();
    let mut queries = FuturesUnordered::<DeviceQuery>::new();

    loop {
        if stopping(shared) {
            return Ok(OwnerOutcome::Stopped);
        }
        start_queries(connection, owner, &mut queued, &mut running, &mut queries);
        let input = future::race(
            async { OwnerInput::Owner(names.next().await) },
            future::race(
                async { OwnerInput::Lifecycle(lifecycle.next().await) },
                future::race(
                    async { OwnerInput::Property(properties.next().await) },
                    future::race(
                        async {
                            let Some(result) = queries.next().await else {
                                return future::pending().await;
                            };
                            OwnerInput::Query(Box::new(result))
                        },
                        async { OwnerInput::Wake(wake_source.readable().await) },
                    ),
                ),
            ),
        )
        .await;
        match input {
            OwnerInput::Owner(Some(Ok(message))) => {
                if let Some(change) = owner_change(message)? {
                    return Ok(OwnerOutcome::Changed(change));
                }
            }
            OwnerInput::Owner(Some(Err(error))) => return Err(error.to_string()),
            OwnerInput::Owner(None) => return Err("power owner subscription ended".into()),
            OwnerInput::Lifecycle(Some(Ok(message))) => {
                if apply_lifecycle(message, &mut state, &mut queued)? {
                    publish(state.snapshot());
                }
            }
            OwnerInput::Lifecycle(Some(Err(error))) => return Err(error.to_string()),
            OwnerInput::Lifecycle(None) => {
                return Err("power device subscription ended".into());
            }
            OwnerInput::Property(Some(Ok(message))) => {
                queue_property_refresh(message, &state, &mut queued)?;
            }
            OwnerInput::Property(Some(Err(error))) => return Err(error.to_string()),
            OwnerInput::Property(None) => {
                return Err("power property subscription ended".into());
            }
            OwnerInput::Query(result) => {
                running.remove(&result.request.key);
                if let Some(change) = drain_owner_events(names)? {
                    return Ok(OwnerOutcome::Changed(change));
                }
                if drain_lifecycle_events(&mut lifecycle, &mut state, &mut queued)? {
                    publish(state.snapshot());
                }
                if apply_query(*result, &mut state)? {
                    publish(state.snapshot());
                }
            }
            OwnerInput::Wake(result) => {
                result.map_err(|error| error.to_string())?;
                drain(wake_source.get_ref());
                if stopping(shared) {
                    return Ok(OwnerOutcome::Stopped);
                }
            }
        }
    }
}

enum StartupInput {
    Ready(Box<Result<Bootstrap, String>>),
    Owner(Option<zbus::Result<Message>>),
    Wake(io::Result<()>),
}

enum OwnerInput {
    Owner(Option<zbus::Result<Message>>),
    Lifecycle(Option<zbus::Result<Message>>),
    Property(Option<zbus::Result<Message>>),
    Query(Box<QueryResult>),
    Wake(io::Result<()>),
}

struct Bootstrap {
    on_battery: bool,
    display_path: String,
    display: PowerDevice,
    paths: Vec<String>,
}

async fn bootstrap(connection: &Connection, owner: &str) -> Result<Bootstrap, String> {
    let root = get_all(connection, owner, UPOWER_PATH, UPOWER_INTERFACE);
    let devices = call_method::<Vec<OwnedObjectPath>>(
        connection,
        owner,
        UPOWER_PATH,
        UPOWER_INTERFACE,
        "EnumerateDevices",
    );
    let display = call_method::<OwnedObjectPath>(
        connection,
        owner,
        UPOWER_PATH,
        UPOWER_INTERFACE,
        "GetDisplayDevice",
    );
    let (root, (devices, display)) = future::zip(root, future::zip(devices, display)).await;
    let mut root = root.map_err(|error| error.to_string())?;
    let on_battery = take::<bool>(&mut root, "OnBattery")
        .ok_or_else(|| "UPower did not provide OnBattery".to_owned())?;
    let mut paths = devices
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|path| path.to_string())
        .filter(|path| valid_device_path(path) && path != DISPLAY_PATH)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths.truncate(MAX_DEVICES);
    let display_path = display.map_err(|error| error.to_string())?.to_string();
    if display_path != DISPLAY_PATH {
        return Err("UPower returned an invalid display device path".into());
    }
    let display = query_device(connection, owner, &display_path).await?;
    Ok(Bootstrap {
        on_battery,
        display_path,
        display,
        paths,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum QueryKey {
    Root,
    Display,
    Device(String),
}

#[derive(Clone)]
struct QueryRequest {
    key: QueryKey,
    path: String,
    generation: u64,
}

enum QueryValue {
    OnBattery(bool),
    Device(Box<PowerDevice>),
}

struct QueryResult {
    request: QueryRequest,
    result: Result<QueryValue, String>,
}

type DeviceQuery = LocalBoxFuture<'static, QueryResult>;

#[derive(Default)]
struct OwnerState {
    on_battery: bool,
    display_path: String,
    display: Option<PowerDevice>,
    devices: BTreeMap<String, PowerDevice>,
    generations: BTreeMap<String, u64>,
    next_generation: u64,
}

impl OwnerState {
    fn snapshot(&self) -> PowerSnapshot {
        PowerSnapshot {
            available: true,
            on_battery: self.on_battery,
            display: self.display.clone(),
            devices: self.devices.values().cloned().collect(),
            error: String::new(),
        }
    }
}

fn add_path(
    state: &mut OwnerState,
    queued: &mut BTreeMap<QueryKey, QueryRequest>,
    path: String,
) -> Result<bool, String> {
    if !valid_device_path(&path) || path == state.display_path {
        return Ok(false);
    }
    if !state.generations.contains_key(&path) && state.generations.len() >= MAX_DEVICES {
        return Ok(false);
    }
    state.next_generation = state
        .next_generation
        .checked_add(1)
        .ok_or_else(|| "power device generation space exhausted".to_owned())?;
    let generation = state.next_generation;
    state.generations.insert(path.clone(), generation);
    let key = QueryKey::Device(path.clone());
    queued.insert(
        key.clone(),
        QueryRequest {
            key,
            path: path.clone(),
            generation,
        },
    );
    Ok(state.devices.remove(&path).is_some())
}

fn start_queries(
    connection: &Connection,
    owner: &str,
    queued: &mut BTreeMap<QueryKey, QueryRequest>,
    running: &mut BTreeSet<QueryKey>,
    queries: &mut FuturesUnordered<DeviceQuery>,
) {
    while running.len() < MAX_QUERIES {
        let Some(key) = queued.keys().find(|key| !running.contains(*key)).cloned() else {
            break;
        };
        let request = queued.remove(&key).expect("queued power query disappeared");
        running.insert(key);
        let connection = connection.clone();
        let owner = owner.to_owned();
        queries.push(
            async move {
                let mut attempts = 0;
                let result = loop {
                    attempts += 1;
                    let result = match &request.key {
                        QueryKey::Root => query_on_battery(&connection, &owner)
                            .await
                            .map(QueryValue::OnBattery),
                        QueryKey::Display | QueryKey::Device(_) => {
                            query_device(&connection, &owner, &request.path)
                                .await
                                .map(Box::new)
                                .map(QueryValue::Device)
                        }
                    };
                    if result.is_ok() || attempts == QUERY_ATTEMPTS {
                        break result;
                    }
                    Timer::after(QUERY_RETRY_DELAY).await;
                };
                QueryResult { request, result }
            }
            .boxed_local(),
        );
    }
}

fn apply_query(result: QueryResult, state: &mut OwnerState) -> Result<bool, String> {
    match &result.request.key {
        QueryKey::Display if result.request.path != state.display_path => return Ok(false),
        QueryKey::Device(path)
            if state.generations.get(path) != Some(&result.request.generation) =>
        {
            return Ok(false);
        }
        _ => {}
    }
    let value = result.result.map_err(|error| {
        format!(
            "power query for {} failed after {QUERY_ATTEMPTS} attempts: {error}",
            result.request.path
        )
    })?;
    match (result.request.key, value) {
        (QueryKey::Root, QueryValue::OnBattery(value)) => {
            if state.on_battery == value {
                Ok(false)
            } else {
                state.on_battery = value;
                Ok(true)
            }
        }
        (QueryKey::Display, QueryValue::Device(device)) => {
            if state.display.as_ref() == Some(&device) {
                Ok(false)
            } else {
                state.display = Some(*device);
                Ok(true)
            }
        }
        (QueryKey::Device(path), QueryValue::Device(device)) => {
            if state.devices.get(&path) == Some(&device) {
                Ok(false)
            } else {
                state.devices.insert(path, *device);
                Ok(true)
            }
        }
        _ => Ok(false),
    }
}

fn apply_lifecycle(
    message: Message,
    state: &mut OwnerState,
    queued: &mut BTreeMap<QueryKey, QueryRequest>,
) -> Result<bool, String> {
    if message.body().len() > MAX_MESSAGE_BYTES {
        return Ok(false);
    }
    let member = message
        .header()
        .member()
        .map(ToString::to_string)
        .unwrap_or_default();
    let Ok(path) = message.body().deserialize::<OwnedObjectPath>() else {
        return Ok(false);
    };
    let path = path.to_string();
    match member.as_str() {
        "DeviceAdded" => add_path(state, queued, path),
        "DeviceRemoved" => {
            state.generations.remove(&path);
            queued.remove(&QueryKey::Device(path.clone()));
            Ok(state.devices.remove(&path).is_some())
        }
        _ => Ok(false),
    }
}

fn drain_lifecycle_events(
    lifecycle: &mut MessageStream,
    state: &mut OwnerState,
    queued: &mut BTreeMap<QueryKey, QueryRequest>,
) -> Result<bool, String> {
    let mut changed = false;
    for _ in 0..EVENT_QUEUE_CAPACITY {
        match lifecycle.next().now_or_never() {
            None => return Ok(changed),
            Some(Some(Ok(message))) => changed |= apply_lifecycle(message, state, queued)?,
            Some(Some(Err(error))) => return Err(error.to_string()),
            Some(None) => return Err("power device subscription ended".into()),
        }
    }
    Ok(changed)
}

fn queue_property_refresh(
    message: Message,
    state: &OwnerState,
    queued: &mut BTreeMap<QueryKey, QueryRequest>,
) -> Result<(), String> {
    if message.body().len() > MAX_MESSAGE_BYTES {
        return Ok(());
    }
    let Some(path) = message.header().path().map(ToString::to_string) else {
        return Ok(());
    };
    type PropertyChange = (String, HashMap<String, OwnedValue>, Vec<String>);
    let Ok((interface, _, _)) = message.body().deserialize::<PropertyChange>() else {
        return Ok(());
    };
    if interface
        != if path == UPOWER_PATH {
            UPOWER_INTERFACE
        } else {
            DEVICE_INTERFACE
        }
    {
        return Ok(());
    }
    let request = if path == UPOWER_PATH {
        QueryRequest {
            key: QueryKey::Root,
            path,
            generation: 0,
        }
    } else if path == state.display_path {
        QueryRequest {
            key: QueryKey::Display,
            path,
            generation: 0,
        }
    } else {
        let Some(generation) = state.generations.get(&path).copied() else {
            return Ok(());
        };
        QueryRequest {
            key: QueryKey::Device(path.clone()),
            path,
            generation,
        }
    };
    queued.insert(request.key.clone(), request);
    Ok(())
}

async fn query_on_battery(connection: &Connection, owner: &str) -> Result<bool, String> {
    let mut values = get_all(connection, owner, UPOWER_PATH, UPOWER_INTERFACE)
        .await
        .map_err(|error| error.to_string())?;
    take::<bool>(&mut values, "OnBattery")
        .ok_or_else(|| "UPower did not provide OnBattery".to_owned())
}

async fn query_device(
    connection: &Connection,
    owner: &str,
    path: &str,
) -> Result<PowerDevice, String> {
    let mut values = get_all(connection, owner, path, DEVICE_INTERFACE)
        .await
        .map_err(|error| error.to_string())?;
    let kind = take::<u32>(&mut values, "Type")
        .ok_or_else(|| format!("UPower device {path} did not provide Type"))?;
    Ok(Device {
        path: path.into(),
        kind,
        state: take::<u32>(&mut values, "State").unwrap_or_default(),
        power_supply: take::<bool>(&mut values, "PowerSupply").unwrap_or(false),
        present: take(&mut values, "IsPresent"),
        online: take(&mut values, "Online"),
        percentage: take(&mut values, "Percentage"),
        time_to_empty: take(&mut values, "TimeToEmpty"),
        time_to_full: take(&mut values, "TimeToFull"),
        energy: take(&mut values, "Energy"),
        energy_empty: take(&mut values, "EnergyEmpty"),
        energy_full: take(&mut values, "EnergyFull"),
        energy_full_design: take(&mut values, "EnergyFullDesign"),
        energy_rate: take(&mut values, "EnergyRate"),
        capacity: take(&mut values, "Capacity"),
        native_path: take(&mut values, "NativePath").unwrap_or_default(),
        vendor: take(&mut values, "Vendor").unwrap_or_default(),
        model: take(&mut values, "Model").unwrap_or_default(),
        serial: take(&mut values, "Serial").unwrap_or_default(),
        icon_name: take(&mut values, "IconName").unwrap_or_default(),
        warning_level: take::<u32>(&mut values, "WarningLevel").unwrap_or_default(),
        battery_level: take(&mut values, "BatteryLevel"),
    }
    .project())
}

async fn get_all(
    connection: &Connection,
    owner: &str,
    path: &str,
    interface: &str,
) -> zbus::Result<HashMap<String, OwnedValue>> {
    let message = connection
        .call_method(
            Some(owner),
            path,
            Some(PROPERTIES_INTERFACE),
            "GetAll",
            &(interface,),
        )
        .await?;
    let body = message.body();
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(zbus::Error::ExcessData);
    }
    body.deserialize()
}

async fn call_method<T>(
    connection: &Connection,
    owner: &str,
    path: &str,
    interface: &str,
    method: &str,
) -> zbus::Result<T>
where
    T: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type,
{
    let message = connection
        .call_method(Some(owner), path, Some(interface), method, &())
        .await?;
    let body = message.body();
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(zbus::Error::ExcessData);
    }
    body.deserialize()
}

fn take<T>(values: &mut HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    values.remove(key).and_then(|value| value.try_into().ok())
}

fn owner_change(message: Message) -> Result<Option<Option<String>>, String> {
    if message.body().len() > MAX_MESSAGE_BYTES {
        return Ok(None);
    }
    let Ok((name, _, owner)) = message.body().deserialize::<(String, String, String)>() else {
        return Ok(None);
    };
    if name != UPOWER_NAME {
        return Ok(None);
    }
    Ok(Some((!owner.is_empty()).then_some(owner)))
}

fn drain_owner_events(names: &mut MessageStream) -> Result<Option<Option<String>>, String> {
    let mut latest = None;
    for _ in 0..OWNER_QUEUE_CAPACITY {
        match names.next().now_or_never() {
            None => return Ok(latest),
            Some(Some(Ok(message))) => {
                if let Some(change) = owner_change(message)? {
                    latest = Some(change);
                }
            }
            Some(Some(Err(error))) => return Err(error.to_string()),
            Some(None) => return Err("power owner subscription ended".into()),
        }
    }
    Ok(latest)
}

fn valid_device_path(path: &str) -> bool {
    path.len() <= MAX_PATH_BYTES
        && path
            .strip_prefix(DEVICE_PATH_PREFIX)
            .is_some_and(|suffix| !suffix.is_empty())
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
