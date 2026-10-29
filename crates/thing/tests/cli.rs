use std::{fs, process::Command};

use thing_wayland::authentication::HELPER_SENTINEL;

#[test]
fn authentication_helper_entry_point_is_not_an_extensible_public_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .args([HELPER_SENTINEL, "unexpected"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not accept arguments"));
}

#[test]
fn types_writes_the_luau_definition_without_runtime_dependencies() {
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .arg("types")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, include_bytes!("../types/thing.d.luau"));
    assert!(output.stderr.is_empty());

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .args(["types", "unexpected"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not accept arguments"));
}

#[test]
fn check_works_without_display_and_returns_distinct_error_codes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, "return ui.panel {}").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/does/not/exist")
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .arg("check")
        .arg("--notification-server")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .arg("check")
        .arg("--clipboard")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));

    fs::write(&path, "return ui.panel { style = { width = -1 } }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("dimensions"));

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .arg("check")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .arg("run")
        .args(["--renderer", "software"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected opengl or vulkan"));

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .arg("lock")
        .args(["--renderer", "software"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected opengl or vulkan"));

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .arg("lock")
        .arg("--notification-server")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option"));

    #[cfg(not(feature = "vulkan"))]
    {
        for command in ["run", "lock"] {
            let output = Command::new(env!("CARGO_BIN_EXE_thing"))
                .arg(command)
                .args(["--renderer", "vulkan"])
                .arg(&path)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&output.stderr).contains("--features vulkan"));
        }
    }
}

#[test]
fn lock_check_uses_the_lock_loader_without_desktop_services() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lock.luau");
    fs::write(
        &path,
        "return ui.lock { children = { ui.secret_input { autofocus = true } } }",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/does/not/exist")
        .args(["check", "--lock"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("valid (1 file(s))"));
    assert!(output.stderr.is_empty());

    fs::write(&path, "return ui.lock {}").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/does/not/exist")
        .args(["check", "--lock"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("exactly one secret_input"));

    fs::write(&path, "return ui.panel {}").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .args(["check", "--lock"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("ui.lock"));
}

#[test]
fn check_resolves_assets_without_using_the_process_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let configuration = root.path().join("configuration");
    let working = root.path().join("working");
    fs::create_dir(&configuration).unwrap();
    fs::create_dir(&working).unwrap();
    let path = configuration.join("panel.luau");
    let asset = configuration.join("corner.svg");
    fs::write(
        &path,
        format!(
            "assert(ui.asset('./corner.svg') == {:?})\nreturn ui.panel {{}}",
            asset.to_str().unwrap()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .current_dir(working)
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 file(s)"));
}

#[test]
fn system_tray_is_explicit_and_run_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        "return ui.panel { children = { ui.text { text = tostring(services.system_tray.available:get()) } } }",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/missing/session-bus")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .arg("check")
        .args(["--system-tray", "host"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option"));

    for arguments in [
        &["run", "--system-tray", "host"][..],
        &["run", "--system-tray=own-watcher"][..],
        &["run", "--notification-server", "--system-tray", "host"][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_thing"))
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("expected an entry file"));
    }

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .args(["run", "--system-tray"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected a value"));

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .args(["run", "--system-tray", "daemon"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected host or own-watcher"));
}

#[test]
fn check_validates_managed_processes_without_starting_the_host_service() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    let marker = directory.path().join("scheduled-command-ran");
    let touch = ["/usr/bin/touch", "/bin/touch"]
        .into_iter()
        .find(|path| std::path::Path::new(path).is_file())
        .unwrap();
    fs::write(
        &path,
        format!(
            r#"
        local query = services.process('query')
        local scheduled = services.process {{
            key = 'scheduled',
            command = {{ {touch:?}, {marker:?} }},
            interval = 1000,
        }}
        return ui.panel {{ children = {{
            ui.text {{ text = function() return query.status:get() end }},
            ui.button {{ on_click = function() query.start({{ 'never-run-by-check' }}) end }},
            ui.button {{ on_click = function() scheduled.refresh() end }},
        }} }}
        "#,
            marker = marker.to_string_lossy(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/missing/session-bus")
        .arg("check")
        .arg(&path)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(!marker.exists());
}
