use std::{fs, process::Command};

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

    #[cfg(not(feature = "vulkan"))]
    {
        let output = Command::new(env!("CARGO_BIN_EXE_thing"))
            .arg("run")
            .args(["--renderer", "vulkan"])
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("--features vulkan"));
    }
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
