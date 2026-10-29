//! a private d-bus daemon and fake `org.freedesktop.Application` services
//! shared by the activation and host tests.

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, MutexGuard},
};

use futures_lite::future;
use tempfile::TempDir;
use zbus::{fdo, zvariant::OwnedValue};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Received {
    Activate(Vec<(String, String)>),
    Open(Vec<String>, Vec<(String, String)>),
    ActivateAction(String, usize, Vec<(String, String)>),
}

#[derive(Clone, Copy)]
pub(crate) enum Behavior {
    Reply,
    Fail,
    Hang,
}

struct FakeApplication {
    received: Arc<Mutex<Vec<Received>>>,
    behavior: Behavior,
}

impl FakeApplication {
    async fn respond(&self, received: Received) -> fdo::Result<()> {
        lock(&self.received).push(received);
        match self.behavior {
            Behavior::Reply => Ok(()),
            Behavior::Fail => Err(fdo::Error::Failed("refused".into())),
            Behavior::Hang => future::pending().await,
        }
    }
}

fn strings(platform_data: HashMap<String, OwnedValue>) -> Vec<(String, String)> {
    let mut values: Vec<_> = platform_data
        .into_iter()
        .map(|(key, value)| (key, <&str>::try_from(&value).unwrap().to_owned()))
        .collect();
    values.sort();
    values
}

// the argument types fix the signatures the specification requires:
// `a{sv}`, `asa{sv}`, and `sava{sv}`.
#[zbus::interface(name = "org.freedesktop.Application")]
impl FakeApplication {
    async fn activate(&self, platform_data: HashMap<String, OwnedValue>) -> fdo::Result<()> {
        self.respond(Received::Activate(strings(platform_data)))
            .await
    }

    async fn open(
        &self,
        uris: Vec<String>,
        platform_data: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.respond(Received::Open(uris, strings(platform_data)))
            .await
    }

    async fn activate_action(
        &self,
        action_name: String,
        parameter: Vec<OwnedValue>,
        platform_data: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.respond(Received::ActivateAction(
            action_name,
            parameter.len(),
            strings(platform_data),
        ))
        .await
    }
}

pub(crate) struct Application {
    pub(crate) connection: zbus::Connection,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Application {
    /// owns `name` on the bus. without a behavior, it serves the interface
    /// at another path, so calls to the expected path are unknown.
    pub(crate) fn start(bus: &Bus, name: &str, behavior: Option<Behavior>) -> Self {
        let received = Arc::<Mutex<Vec<Received>>>::default();
        let object_path = match behavior {
            Some(_) => path(name),
            None => "/elsewhere".into(),
        };
        let builder = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(name.to_owned())
            .unwrap()
            .serve_at(
                object_path,
                FakeApplication {
                    received: received.clone(),
                    behavior: behavior.unwrap_or(Behavior::Reply),
                },
            )
            .unwrap();
        let connection = async_io::block_on(builder.build()).unwrap();
        Self {
            connection,
            received,
        }
    }

    pub(crate) fn received(&self) -> Vec<Received> {
        lock(&self.received).clone()
    }
}

pub(crate) struct Bus {
    pub(crate) address: String,
    config: PathBuf,
    socket: PathBuf,
    child: Option<Child>,
    _directory: TempDir,
}

impl Bus {
    /// a private bus whose activatable services are `(name, script)` pairs.
    pub(crate) fn start(services: &[(&str, &str)]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let service_directory = directory.path().join("services");
        fs::create_dir(&service_directory).unwrap();
        for (name, script) in services {
            let program = directory.path().join(format!("{name}.sh"));
            fs::write(&program, script).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(
                service_directory.join(format!("{name}.service")),
                format!("[D-BUS Service]\nName={name}\nExec={}\n", program.display()),
            )
            .unwrap();
        }
        let socket = directory.path().join("bus");
        let config = directory.path().join("bus.conf");
        fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <servicedir>{}</servicedir>
  <policy context="default">
<allow send_destination="*" eavesdrop="true"/>
<allow eavesdrop="true"/>
<allow own="*"/>
  </policy>
</busconfig>
"#,
                socket.display(),
                service_directory.display()
            ),
        )
        .unwrap();
        let mut bus = Self {
            address: format!("unix:path={}", socket.display()),
            config,
            socket,
            child: None,
            _directory: directory,
        };
        bus.spawn();
        bus
    }

    pub(crate) fn spawn(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let mut command = Command::new("dbus-daemon");
        command
            .arg(format!("--config-file={}", self.config.display()))
            .args(["--nofork", "--nopidfile", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // a test process killed at its deadline cannot run `Drop`, so the bus
        // also ends with the thread that started it.
        // SAFETY: prctl is async-signal-safe and touches only the child.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .expect("dbus-daemon must be installed for activation tests");
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert!(ready.starts_with(&self.address), "{ready}");
        self.child = Some(child);
    }

    pub(crate) fn stop(&mut self) {
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

pub(crate) fn path(name: &str) -> String {
    format!("/{}", name.replace('.', "/").replace('-', "_"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
