use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixListener,
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;
use zbus::zvariant::OwnedObjectPath;

use super::*;

const BATTERY_PATH: &str = "/org/freedesktop/UPower/devices/battery_BAT0";
const LINE_PATH: &str = "/org/freedesktop/UPower/devices/line_power_AC";

#[test]
fn private_bus_tracks_devices_properties_service_lifetime_and_reconnect() {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("bus");
    let address = format!("unix:path={}", socket.display());
    let mut bus = Bus::start(&address);
    let service = FakeService::start(&address, "first");
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let power = Power::start_at(Some(address.clone()), ping).unwrap();

    let snapshot = wait_for(&power, |snapshot| {
        snapshot.available && snapshot.devices.len() == 1
    });
    assert!(snapshot.on_battery);
    assert_eq!(snapshot.display.as_ref().unwrap().percentage, Some(0.0));
    assert_eq!(snapshot.display.as_ref().unwrap().energy, Some(0.0));
    assert_eq!(snapshot.display.as_ref().unwrap().energy_rate, Some(0.0));
    assert_eq!(snapshot.display.as_ref().unwrap().time_to_empty, None);
    assert_eq!(snapshot.devices[0].id, BATTERY_PATH);
    assert_eq!(snapshot.devices[0].model.as_deref(), Some("first"));

    let queries = service.battery_queries.load(Ordering::Acquire);
    emit_malformed_properties(&service.connection, BATTERY_PATH);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(service.battery_queries.load(Ordering::Acquire), queries);

    service.root.lock().unwrap().on_battery = false;
    {
        let mut display = service.display.lock().unwrap();
        display.state = 1;
        display.percentage = 40.0;
        display.time_to_full = 1200;
    }
    emit_properties(&service.connection, UPOWER_PATH, UPOWER_INTERFACE);
    emit_properties(&service.connection, DISPLAY_PATH, DEVICE_INTERFACE);
    let snapshot = wait_for(&power, |snapshot| {
        snapshot.available
            && !snapshot.on_battery
            && snapshot.display.as_ref().is_some_and(|display| {
                display.state == "charging" && display.percentage == Some(40.0)
            })
    });
    assert_eq!(snapshot.display.unwrap().time_to_full, Some(1200));

    service.add_line_power();
    let snapshot = wait_for(&power, |snapshot| snapshot.devices.len() == 2);
    let line = snapshot
        .devices
        .iter()
        .find(|device| device.id == LINE_PATH)
        .unwrap();
    assert_eq!(line.online, Some(true));
    assert_eq!(line.percentage, None);
    assert_eq!(line.energy, None);

    service.remove_device(BATTERY_PATH);
    let snapshot = wait_for(&power, |snapshot| {
        snapshot.available && snapshot.devices.len() == 1
    });
    assert_eq!(snapshot.devices[0].id, LINE_PATH);

    assert!(async_io::block_on(service.connection.release_name(UPOWER_NAME)).unwrap());
    let snapshot = wait_for(&power, |snapshot| !snapshot.available);
    assert!(snapshot.error.is_empty());
    async_io::block_on(service.connection.request_name(UPOWER_NAME)).unwrap();
    let snapshot = wait_for(&power, |snapshot| snapshot.available);
    assert!(!snapshot.on_battery);
    assert_eq!(snapshot.devices.len(), 1);

    drop(service);
    bus.stop();
    let snapshot = wait_for(&power, |snapshot| {
        !snapshot.available && !snapshot.error.is_empty()
    });
    assert!(!snapshot.error.is_empty());

    bus = Bus::start(&address);
    let replacement = FakeService::start(&address, "after reconnect");
    let snapshot = wait_for(&power, |snapshot| {
        snapshot.available
            && snapshot
                .devices
                .first()
                .and_then(|device| device.model.as_deref())
                == Some("after reconnect")
    });
    assert!(snapshot.error.is_empty());

    drop(power);
    drop(replacement);
    bus.stop();
}

#[test]
fn stale_query_cannot_restore_a_replaced_device_path() {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("bus");
    let address = format!("unix:path={}", socket.display());
    let mut bus = Bus::start(&address);
    let service = FakeService::start(&address, "first");
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let power = Power::start_at(Some(address), ping).unwrap();
    wait_for(&power, |snapshot| {
        snapshot.available && snapshot.devices.len() == 1
    });

    let queries = service.battery_queries.load(Ordering::Acquire);
    service.battery.lock().unwrap().delay = Duration::from_millis(500);
    emit_properties(&service.connection, BATTERY_PATH, DEVICE_INTERFACE);
    wait_until(|| service.battery_queries.load(Ordering::Acquire) > queries);
    service.replace_battery("replacement");

    let snapshot = wait_for(&power, |snapshot| {
        snapshot
            .devices
            .first()
            .and_then(|device| device.model.as_deref())
            == Some("replacement")
    });
    assert_eq!(snapshot.devices[0].id, BATTERY_PATH);
    thread::sleep(Duration::from_millis(700));
    let latest = power.take_snapshot();
    assert!(latest.as_ref().is_none_or(|snapshot| {
        snapshot
            .devices
            .first()
            .and_then(|device| device.model.as_deref())
            == Some("replacement")
    }));

    drop(power);
    drop(service);
    bus.stop();
}

#[test]
fn transient_query_failure_recovers_without_another_signal() {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("bus");
    let address = format!("unix:path={}", socket.display());
    let mut bus = Bus::start(&address);
    let service = FakeService::start(&address, "initial");
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let power = Power::start_at(Some(address), ping).unwrap();
    wait_for(&power, |snapshot| {
        snapshot.available && snapshot.devices.len() == 1
    });

    let queries = service.battery_queries.load(Ordering::Acquire);
    {
        let mut battery = service.battery.lock().unwrap();
        battery.model = "after retry".into();
        battery.fail_queries = 1;
    }
    emit_properties(&service.connection, BATTERY_PATH, DEVICE_INTERFACE);
    let snapshot = wait_for(&power, |snapshot| {
        snapshot
            .devices
            .first()
            .and_then(|device| device.model.as_deref())
            == Some("after retry")
    });
    assert!(snapshot.error.is_empty());
    assert!(service.battery_queries.load(Ordering::Acquire) >= queries + 2);

    drop(power);
    drop(service);
    bus.stop();
}

#[test]
fn service_can_appear_after_the_worker_connects() {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("bus");
    let address = format!("unix:path={}", socket.display());
    let mut bus = Bus::start(&address);
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let power = Power::start_at(Some(address.clone()), ping).unwrap();

    thread::sleep(Duration::from_millis(100));
    let service = FakeService::start(&address, "late");
    let snapshot = wait_for(&power, |snapshot| {
        snapshot.available
            && snapshot
                .devices
                .first()
                .and_then(|device| device.model.as_deref())
                == Some("late")
    });
    assert!(snapshot.error.is_empty());

    drop(power);
    drop(service);
    bus.stop();
}

#[test]
fn device_tracking_is_bounded() {
    let mut state = OwnerState {
        display_path: DISPLAY_PATH.into(),
        ..OwnerState::default()
    };
    let mut queued = BTreeMap::new();
    for index in 0..MAX_DEVICES + 20 {
        add_path(
            &mut state,
            &mut queued,
            format!("{DEVICE_PATH_PREFIX}battery_{index}"),
        )
        .unwrap();
    }
    assert_eq!(state.generations.len(), MAX_DEVICES);
    assert_eq!(queued.len(), MAX_DEVICES);

    let oversized = format!("{DEVICE_PATH_PREFIX}{}", "a".repeat(MAX_PATH_BYTES));
    assert!(!add_path(&mut state, &mut queued, oversized).unwrap());
}

#[test]
fn exhausted_query_errors_rebootstrap_only_the_current_generation() {
    let mut state = OwnerState {
        display_path: DISPLAY_PATH.into(),
        ..OwnerState::default()
    };
    state.generations.insert(BATTERY_PATH.into(), 2);

    let current = QueryResult {
        request: QueryRequest {
            key: QueryKey::Device(BATTERY_PATH.into()),
            path: BATTERY_PATH.into(),
            generation: 2,
        },
        result: Err("test failure".into()),
    };
    let error = apply_query(current, &mut state).unwrap_err();
    assert!(error.contains("failed after 3 attempts"));

    let replaced = QueryResult {
        request: QueryRequest {
            key: QueryKey::Device(BATTERY_PATH.into()),
            path: BATTERY_PATH.into(),
            generation: 1,
        },
        result: Err("stale failure".into()),
    };
    assert!(!apply_query(replaced, &mut state).unwrap());
}

#[test]
fn shutdown_interrupts_a_stalled_private_query() {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("bus");
    let address = format!("unix:path={}", socket.display());
    let mut bus = Bus::start(&address);
    let service = FakeService::start(&address, "stalled");
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let power = Power::start_at(Some(address), ping).unwrap();
    wait_for(&power, |snapshot| snapshot.available);

    let queries = service.battery_queries.load(Ordering::Acquire);
    service.battery.lock().unwrap().delay = Duration::from_secs(10);
    emit_properties(&service.connection, BATTERY_PATH, DEVICE_INTERFACE);
    wait_until(|| service.battery_queries.load(Ordering::Acquire) > queries);
    let started = Instant::now();
    drop(power);
    assert!(started.elapsed() < METHOD_TIMEOUT + Duration::from_secs(1));

    drop(service);
    bus.stop();
}

#[test]
fn shutdown_interrupts_private_bus_authentication() {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("bus");
    let listener = UnixListener::bind(&socket).unwrap();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let stream = listener.accept().unwrap().0;
        accepted_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        drop(stream);
    });
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let power = Power::start_at(Some(format!("unix:path={}", socket.display())), ping).unwrap();
    accepted_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let (dropped_tx, dropped_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(power);
        dropped_tx.send(()).unwrap();
    });
    let bounded = dropped_rx.recv_timeout(Duration::from_secs(3)).is_ok();
    release_tx.send(()).unwrap();
    server.join().unwrap();
    dropper.join().unwrap();
    assert!(bounded);
}

#[test]
fn shutdown_interrupts_a_continuous_lifecycle_signal_flood() {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("bus");
    let address = format!("unix:path={}", socket.display());
    let mut bus = Bus::start(&address);
    let service = FakeService::start(&address, "flooded");
    let (ping, _source) = calloop::ping::make_ping().unwrap();
    let power = Power::start_at(Some(address), ping).unwrap();
    wait_for(&power, |snapshot| snapshot.available);

    let emitting = Arc::new(AtomicBool::new(true));
    let emitted = Arc::new(AtomicUsize::new(0));
    let connection = service.connection.clone();
    let keep_emitting = emitting.clone();
    let emission_count = emitted.clone();
    let emitter = thread::spawn(move || {
        while keep_emitting.load(Ordering::Acquire) {
            emit_device_signal(&connection, "DeviceAdded", BATTERY_PATH);
            emission_count.fetch_add(1, Ordering::Release);
        }
    });
    wait_until(|| emitted.load(Ordering::Acquire) > EVENT_QUEUE_CAPACITY * 2);

    let (dropped_tx, dropped_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(power);
        dropped_tx.send(()).unwrap();
    });
    let bounded = dropped_rx.recv_timeout(Duration::from_secs(3)).is_ok();
    emitting.store(false, Ordering::Release);
    emitter.join().unwrap();
    dropper.join().unwrap();
    assert!(bounded);

    drop(service);
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
            .expect("dbus-daemon must be installed for power tests");
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

struct FakeRootState {
    on_battery: bool,
    devices: Vec<String>,
}

struct FakeRoot {
    state: Arc<Mutex<FakeRootState>>,
}

#[zbus::interface(name = "org.freedesktop.UPower")]
impl FakeRoot {
    fn enumerate_devices(&self) -> Vec<OwnedObjectPath> {
        self.state
            .lock()
            .unwrap()
            .devices
            .iter()
            .map(|path| OwnedObjectPath::try_from(path.as_str()).unwrap())
            .collect()
    }

    fn get_display_device(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(DISPLAY_PATH).unwrap()
    }

    #[zbus(property)]
    fn on_battery(&self) -> bool {
        self.state.lock().unwrap().on_battery
    }
}

struct FakeDeviceState {
    kind: u32,
    state: u32,
    power_supply: bool,
    present: bool,
    online: bool,
    percentage: f64,
    time_to_empty: i64,
    time_to_full: i64,
    energy: f64,
    energy_empty: f64,
    energy_full: f64,
    energy_full_design: f64,
    energy_rate: f64,
    capacity: f64,
    native_path: String,
    vendor: String,
    model: String,
    serial: String,
    icon_name: String,
    warning_level: u32,
    battery_level: u32,
    delay: Duration,
    fail_queries: usize,
}

impl FakeDeviceState {
    fn battery(model: &str) -> Self {
        Self {
            kind: 2,
            state: 3,
            power_supply: true,
            present: true,
            online: false,
            percentage: 0.0,
            time_to_empty: 0,
            time_to_full: 0,
            energy: 0.0,
            energy_empty: 0.0,
            energy_full: 50.0,
            energy_full_design: 55.0,
            energy_rate: 0.0,
            capacity: 90.0,
            native_path: "BAT0".into(),
            vendor: String::new(),
            model: model.into(),
            serial: String::new(),
            icon_name: "battery-empty-symbolic".into(),
            warning_level: 4,
            battery_level: 1,
            delay: Duration::ZERO,
            fail_queries: 0,
        }
    }

    fn line_power() -> Self {
        Self {
            kind: 1,
            state: 0,
            power_supply: true,
            present: false,
            online: true,
            percentage: 100.0,
            time_to_empty: 0,
            time_to_full: 0,
            energy: 20.0,
            energy_empty: 0.0,
            energy_full: 20.0,
            energy_full_design: 20.0,
            energy_rate: 0.0,
            capacity: 100.0,
            native_path: "AC".into(),
            vendor: String::new(),
            model: String::new(),
            serial: String::new(),
            icon_name: "ac-adapter-symbolic".into(),
            warning_level: 1,
            battery_level: 1,
            delay: Duration::ZERO,
            fail_queries: 0,
        }
    }
}

struct FakeDevice {
    state: Arc<Mutex<FakeDeviceState>>,
    queries: Arc<AtomicUsize>,
}

#[zbus::interface(name = "org.freedesktop.UPower.Device")]
impl FakeDevice {
    #[zbus(property)]
    fn native_path(&self) -> String {
        self.state.lock().unwrap().native_path.clone()
    }

    #[zbus(property)]
    fn vendor(&self) -> String {
        self.state.lock().unwrap().vendor.clone()
    }

    #[zbus(property)]
    fn model(&self) -> String {
        self.state.lock().unwrap().model.clone()
    }

    #[zbus(property)]
    fn serial(&self) -> String {
        self.state.lock().unwrap().serial.clone()
    }

    #[zbus(property, name = "Type")]
    fn kind(&self) -> zbus::fdo::Result<u32> {
        let mut state = self.state.lock().unwrap();
        if state.fail_queries > 0 {
            state.fail_queries -= 1;
            Err(zbus::fdo::Error::Failed("transient test failure".into()))
        } else {
            Ok(state.kind)
        }
    }

    #[zbus(property)]
    fn power_supply(&self) -> bool {
        self.state.lock().unwrap().power_supply
    }

    #[zbus(property)]
    fn online(&self) -> bool {
        self.state.lock().unwrap().online
    }

    #[zbus(property)]
    fn energy(&self) -> f64 {
        self.state.lock().unwrap().energy
    }

    #[zbus(property)]
    fn energy_empty(&self) -> f64 {
        self.state.lock().unwrap().energy_empty
    }

    #[zbus(property)]
    fn energy_full(&self) -> f64 {
        self.state.lock().unwrap().energy_full
    }

    #[zbus(property)]
    fn energy_full_design(&self) -> f64 {
        self.state.lock().unwrap().energy_full_design
    }

    #[zbus(property)]
    fn energy_rate(&self) -> f64 {
        self.state.lock().unwrap().energy_rate
    }

    #[zbus(property)]
    fn time_to_empty(&self) -> i64 {
        self.state.lock().unwrap().time_to_empty
    }

    #[zbus(property)]
    fn time_to_full(&self) -> i64 {
        self.state.lock().unwrap().time_to_full
    }

    #[zbus(property)]
    async fn percentage(&self) -> zbus::fdo::Result<f64> {
        self.queries.fetch_add(1, Ordering::Release);
        let delay = self.state.lock().unwrap().delay;
        if !delay.is_zero() {
            Timer::after(delay).await;
        }
        Ok(self.state.lock().unwrap().percentage)
    }

    #[zbus(property)]
    fn is_present(&self) -> bool {
        self.state.lock().unwrap().present
    }

    #[zbus(property)]
    fn state(&self) -> u32 {
        self.state.lock().unwrap().state
    }

    #[zbus(property)]
    fn capacity(&self) -> f64 {
        self.state.lock().unwrap().capacity
    }

    #[zbus(property)]
    fn warning_level(&self) -> u32 {
        self.state.lock().unwrap().warning_level
    }

    #[zbus(property)]
    fn battery_level(&self) -> u32 {
        self.state.lock().unwrap().battery_level
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.state.lock().unwrap().icon_name.clone()
    }
}

struct FakeService {
    connection: Connection,
    root: Arc<Mutex<FakeRootState>>,
    display: Arc<Mutex<FakeDeviceState>>,
    battery: Arc<Mutex<FakeDeviceState>>,
    battery_queries: Arc<AtomicUsize>,
}

impl FakeService {
    fn start(address: &str, model: &str) -> Self {
        let root = Arc::new(Mutex::new(FakeRootState {
            on_battery: true,
            devices: vec![BATTERY_PATH.into()],
        }));
        let display = Arc::new(Mutex::new(FakeDeviceState::battery("display")));
        let battery = Arc::new(Mutex::new(FakeDeviceState::battery(model)));
        let display_queries = Arc::new(AtomicUsize::new(0));
        let battery_queries = Arc::new(AtomicUsize::new(0));
        let deadline = Instant::now() + Duration::from_secs(5);
        let connection = loop {
            let result = async_io::block_on(async {
                zbus::connection::Builder::address(address)?
                    .name(UPOWER_NAME)?
                    .serve_at(
                        UPOWER_PATH,
                        FakeRoot {
                            state: root.clone(),
                        },
                    )?
                    .serve_at(
                        DISPLAY_PATH,
                        FakeDevice {
                            state: display.clone(),
                            queries: display_queries.clone(),
                        },
                    )?
                    .serve_at(
                        BATTERY_PATH,
                        FakeDevice {
                            state: battery.clone(),
                            queries: battery_queries.clone(),
                        },
                    )?
                    .build()
                    .await
            });
            match result {
                Ok(connection) => break connection,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("cannot start fake UPower service: {error}"),
            }
        };
        Self {
            connection,
            root,
            display,
            battery,
            battery_queries,
        }
    }

    fn add_line_power(&self) {
        let state = Arc::new(Mutex::new(FakeDeviceState::line_power()));
        async_io::block_on(self.connection.object_server().at(
            LINE_PATH,
            FakeDevice {
                state,
                queries: Arc::new(AtomicUsize::new(0)),
            },
        ))
        .unwrap();
        self.root.lock().unwrap().devices.push(LINE_PATH.into());
        emit_device_signal(&self.connection, "DeviceAdded", LINE_PATH);
    }

    fn remove_device(&self, path: &str) {
        self.root
            .lock()
            .unwrap()
            .devices
            .retain(|device| device != path);
        emit_device_signal(&self.connection, "DeviceRemoved", path);
        async_io::block_on(
            self.connection
                .object_server()
                .remove::<FakeDevice, _>(path),
        )
        .unwrap();
    }

    fn replace_battery(&self, model: &str) {
        self.remove_device(BATTERY_PATH);
        let state = Arc::new(Mutex::new(FakeDeviceState::battery(model)));
        async_io::block_on(self.connection.object_server().at(
            BATTERY_PATH,
            FakeDevice {
                state,
                queries: Arc::new(AtomicUsize::new(0)),
            },
        ))
        .unwrap();
        self.root.lock().unwrap().devices.push(BATTERY_PATH.into());
        emit_device_signal(&self.connection, "DeviceAdded", BATTERY_PATH);
    }
}

fn emit_device_signal(connection: &Connection, member: &str, path: &str) {
    let path = OwnedObjectPath::try_from(path).unwrap();
    async_io::block_on(connection.emit_signal(
        None::<&str>,
        UPOWER_PATH,
        UPOWER_INTERFACE,
        member,
        &(path,),
    ))
    .unwrap();
}

fn emit_properties(connection: &Connection, path: &str, interface: &str) {
    async_io::block_on(connection.emit_signal(
        None::<&str>,
        path,
        PROPERTIES_INTERFACE,
        "PropertiesChanged",
        &(
            interface,
            HashMap::<String, OwnedValue>::new(),
            vec!["Percentage"],
        ),
    ))
    .unwrap();
}

fn emit_malformed_properties(connection: &Connection, path: &str) {
    async_io::block_on(connection.emit_signal(
        None::<&str>,
        path,
        PROPERTIES_INTERFACE,
        "PropertiesChanged",
        &(7_u32,),
    ))
    .unwrap();
}

fn wait_for(power: &Power, predicate: impl Fn(&PowerSnapshot) -> bool) -> PowerSnapshot {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut observed = Vec::new();
    while Instant::now() < deadline {
        if let Some(snapshot) = power.take_snapshot() {
            observed.push(snapshot.clone());
            if predicate(&snapshot) {
                return snapshot;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for power state; observed {observed:?}");
}

fn wait_until(predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for fake power query");
}
