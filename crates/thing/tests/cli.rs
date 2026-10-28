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
