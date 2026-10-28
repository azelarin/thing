use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use zbus::Connection;

type Pixmap = (i32, i32, Vec<u8>);

const ITEM_NAME: &str = "org.example.ThingTrayIconNative";
const ACTIVE_PATH: &str = "/ActiveItem";
const ATTENTION_PATH: &str = "/AttentionItem";
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const DISPLAY_TIME: Duration = Duration::from_secs(8);

#[test]
fn tray_icon_fixture_configuration_is_valid() {
    let directory = tempfile::tempdir().unwrap();
    let configuration = directory.path().join("panel.luau");
    fs::write(&configuration, CONFIGURATION).unwrap();
    thing_runtime::Runtime::load(configuration).unwrap();
}

#[test]
#[ignore = "requires an explicit Wayland graphics run"]
fn renders_status_notifier_pixmap_fallbacks() {
    let directory = tempfile::tempdir().unwrap();
    let bus = Bus::start(directory.path());
    let item = fixture_item(&bus.address);
    let registration = item.clone();
    let register = thread::spawn(move || register_items(&registration));
    let configuration = directory.path().join("panel.luau");
    fs::write(&configuration, CONFIGURATION).unwrap();

    let mut thing = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/does/not/exist")
        .env("PULSE_SERVER", "unix:/does/not/exist")
        .env_remove("NIRI_SOCKET")
        .arg("run")
        .args(["--system-tray", "own-watcher"])
        .arg(&configuration)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    if let Err(error) = register.join().unwrap() {
        let _ = thing.kill();
        let _ = thing.wait();
        panic!("{error}");
    }
    assert!(
        thing.try_wait().unwrap().is_none(),
        "thing exited before drawing"
    );
    eprintln!(
        "fixture ready for {} seconds: active is #f02030/#20dc50 left-to-right; attention is #2060f0/#f0c820 top-to-bottom",
        DISPLAY_TIME.as_secs()
    );
    let deadline = Instant::now() + DISPLAY_TIME;
    while Instant::now() < deadline {
        if let Some(status) = thing.try_wait().unwrap() {
            panic!("thing exited before the fixture interval ended: {status}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let status = stop(&mut thing);
    assert!(
        status.success() || status.signal() == Some(libc::SIGINT),
        "thing returned an unexpected fixture status: {status}"
    );
}

fn fixture_item(address: &str) -> Connection {
    async_io::block_on(async {
        zbus::connection::Builder::address(address)?
            .name(ITEM_NAME)?
            .serve_at(ACTIVE_PATH, FixtureItem::active())?
            .serve_at(ATTENTION_PATH, FixtureItem::attention())?
            .build()
            .await
    })
    .unwrap()
}

fn register_items(connection: &Connection) -> Result<(), String> {
    for path in [ACTIVE_PATH, ATTENTION_PATH] {
        let registration = format!("{ITEM_NAME}{path}");
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let result = async_io::block_on(connection.call_method(
                Some(WATCHER_NAME),
                WATCHER_PATH,
                Some(WATCHER_INTERFACE),
                "RegisterStatusNotifierItem",
                &(registration.as_str(),),
            ));
            if result.is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out registering {registration}"));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}

fn stop(child: &mut Child) -> ExitStatus {
    if let Some(status) = child.try_wait().unwrap() {
        return status;
    }
    // SAFETY: child.id() is the live process returned by Command::spawn, and
    // SIGINT carries no pointer or memory-safety preconditions.
    let result = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(result, 0, "could not interrupt the fixture process");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let status = child.wait().unwrap();
    panic!("fixture process ignored SIGINT and was killed: {status}");
}

struct Bus {
    address: String,
    child: Option<Child>,
}

impl Bus {
    fn start(directory: &Path) -> Self {
        let requested = format!("unix:path={}", directory.join("session-bus").display());
        let mut child = Command::new("dbus-daemon")
            .args([
                "--session",
                "--nofork",
                "--nopidfile",
                "--print-address=1",
                &format!("--address={requested}"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("dbus-daemon is required for the native tray fixture");
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut address = String::new();
        output.read_line(&mut address).unwrap();
        let address = address.trim().to_owned();
        assert!(address.starts_with(&requested), "unexpected bus address");
        Self {
            address,
            child: Some(child),
        }
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct FixtureItem {
    id: &'static str,
    title: &'static str,
    status: &'static str,
}

impl FixtureItem {
    fn active() -> Self {
        Self {
            id: "fixture-active",
            title: "normal pixmap",
            status: "Active",
        }
    }

    fn attention() -> Self {
        Self {
            id: "fixture-attention",
            title: "attention pixmap",
            status: "NeedsAttention",
        }
    }
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl FixtureItem {
    #[zbus(property)]
    fn id(&self) -> &str {
        self.id
    }

    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        self.title
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        self.status
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "thing-native-missing-normal-5d8198b3"
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        "thing-native-missing-attention-5d8198b3"
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        vec![(16, 16, normal_argb())]
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<Pixmap> {
        vec![(16, 16, attention_argb())]
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
}

fn normal_argb() -> Vec<u8> {
    argb(|x, _| {
        if x < 8 {
            [255, 240, 32, 48]
        } else {
            [255, 32, 220, 80]
        }
    })
}

fn attention_argb() -> Vec<u8> {
    argb(|_, y| {
        if y < 8 {
            [255, 32, 96, 240]
        } else {
            [255, 240, 200, 32]
        }
    })
}

fn argb(mut pixel: impl FnMut(usize, usize) -> [u8; 4]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16 {
        for x in 0..16 {
            bytes.extend_from_slice(&pixel(x, y));
        }
    }
    bytes
}

const CONFIGURATION: &str = r#"
local tray = services.system_tray

return ui.panel {
    layer = 'overlay',
    exclusive_zone = 0,
    style = {
        height = 112,
        padding = 12,
        gap = 8,
        background = '#101820',
    },
    children = {
        ui.text {
            text = 'private-bus tray pixmap fallback',
            style = { foreground = '#f4f7fb' },
        },
        ui.row {
            style = { gap = 16 },
            children = function()
                local children = {}
                for _, item in tray.items:get() do
                    table.insert(children, ui.row {
                        key = item.id,
                        style = {
                            width = 220,
                            height = 56,
                            padding = 8,
                            gap = 12,
                            radius = 8,
                            background = '#263241',
                        },
                        children = {
                            ui.image {
                                source = item.icon,
                                fit = 'fill',
                                style = { width = 40, height = 40 },
                            },
                            ui.text {
                                text = item.title .. '\n' .. item.status,
                                style = { foreground = '#dce6f2' },
                            },
                        },
                    })
                end
                return children
            end,
        },
    },
}
"#;
