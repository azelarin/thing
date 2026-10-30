use std::{fs, process::Command};

use thing_wayland::authentication::HELPER_SENTINEL;

#[test]
fn inspect_reports_hidden_clipped_and_popup_layout_without_a_display() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
return ui.panel {
    style = { height = 80 },
    children = {
        ui.column { key = 'container', style = { width = 40, height = 20 }, children = {
            ui.row { key = 'outside', style = { width = 80, height = 20, translate_x = 100 } },
        } },
        ui.row { key = 'hidden', visible = false, children = {
            ui.text { key = 'hidden-child', text = 'private widget contents' },
        } },
        ui.button { children = { ui.popup {
            key = 'menu', style = { width = 90, height = 50 },
            children = { ui.text { text = 'popup contents' } },
        } } },
    },
}
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/does/not/exist")
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/does/not/exist")
        .args([
            "inspect",
            "--output-size",
            "640x480",
            "--output-name",
            "DP-1",
            "--scale",
            "1.25",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(
        report.starts_with("output \"DP-1\" size=640x480 scale=1.25\n"),
        "{report}"
    );
    assert!(report.contains("panel default size=640x80\n"), "{report}");
    let outside = report
        .lines()
        .find(|line| line.contains("key=\"outside\""))
        .unwrap();
    assert!(
        outside.contains("visual=[80.00,30.00 80.00x20.00]"),
        "{outside}"
    );
    assert!(
        outside.contains("clip=[0.00,30.00 40.00x20.00]"),
        "{outside}"
    );
    let hidden = report
        .lines()
        .find(|line| line.contains("key=\"hidden-child\""))
        .unwrap();
    assert!(
        hidden.contains("visible=false bounds=none body=omitted"),
        "{hidden}"
    );
    assert!(
        report.contains("key=\"menu\" (requested layout) size=90x50"),
        "{report}"
    );
    assert!(!report.contains("private widget contents") && !report.contains("popup contents"));
}

#[test]
fn inspect_uses_output_context_and_reports_all_panels() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(
        &path,
        r#"
assert(services.output.name:get() == 'DP-2')
return {
    ui.panel { key = 'bar', style = { width = function()
        return services.output.width:get() / services.output.scale:get()
    end, height = 36 } },
    ui.panel { key = 'drawer', visible = false, style = { width = 120, height = 64 } },
}
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .args([
            "inspect",
            "--output-size=800x600",
            "--output-name=DP-2",
            "--scale=2",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("panel \"bar\" size=400x36"), "{report}");
    assert!(report.contains("panel \"drawer\" size=120x64"), "{report}");
    let drawer = report
        .split("panel \"drawer\" size=120x64\n")
        .nth(1)
        .unwrap();
    assert!(
        drawer
            .lines()
            .next()
            .unwrap()
            .contains("visible=false bounds=none"),
        "{drawer}"
    );
}

#[test]
fn inspect_distinguishes_usage_errors_from_configuration_errors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, "return ui.panel {}").unwrap();
    for arguments in [
        vec![],
        vec!["--output-size", "0x480"],
        vec!["--output-size", "640x"],
        vec!["--output-size", "100001x480"],
        vec!["--output-size", "1.5x480"],
        vec!["--output-size", "640x480", "--scale", "NaN"],
        vec!["--output-size", "640x480", "--scale", "0"],
        vec!["--output-size", "640x480", "--scale", "17"],
        vec!["--output-size", "640x480", "--renderer", "opengl"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_thing"))
            .arg("inspect")
            .args(&arguments)
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty());
    }
    fs::write(&path, "error('configuration failed')").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thing"))
        .args(["inspect", "--output-size", "640x480"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("configuration failed"));
    assert!(output.stdout.is_empty());
}

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
