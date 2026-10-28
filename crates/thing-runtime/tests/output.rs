use std::fs;

use tempfile::TempDir;
use thing_core::Scale;
use thing_runtime::{
    AudioDevice, Event, NiriState, NiriWindow, OutputContext, Runtime, Scalar, ServiceSnapshot,
};

fn fixture(source: &str) -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    (directory, path)
}

fn output(name: &str, width: u32, height: u32, scale: u32) -> OutputContext {
    OutputContext {
        name: Some(name.into()),
        width,
        height,
        scale: Scale::from_120ths(scale).unwrap(),
    }
}

#[test]
fn output_context_is_available_during_load_and_updates_reactively() {
    let (_directory, path) = fixture(
        r#"
        local output = services.output
        local initial = ui.state(output.name:get())
        return ui.panel { children = {
            ui.text { text = initial:get() },
            ui.text { text = function() return output.name:get() end },
            ui.text { text = function() return tostring(output.width:get()) end },
            ui.text { text = function() return tostring(output.height:get()) end },
            ui.text { text = function() return tostring(output.scale:get()) end },
            ui.text { text = function() return tostring(output.available:get()) end },
        } }
        "#,
    );
    let mut runtime = Runtime::load_for_output(
        path,
        output("DP-1", 1920, 1080, 150),
        &ServiceSnapshot::default(),
    )
    .unwrap();
    let text = |index: usize| runtime.root().children[index].text.as_str();
    assert_eq!(text(0), "DP-1");
    assert_eq!(text(1), "DP-1");
    assert_eq!(text(2), "1920");
    assert_eq!(text(3), "1080");
    assert_eq!(text(4), "1.25");
    assert_eq!(text(5), "true");

    runtime
        .set_output(output("HDMI-A-1", 2560, 1440, 240))
        .unwrap();
    assert!(runtime.refresh().unwrap());
    let text = |index: usize| runtime.root().children[index].text.as_str();
    assert_eq!(text(0), "DP-1");
    assert_eq!(text(1), "HDMI-A-1");
    assert_eq!(text(2), "2560");
    assert_eq!(text(3), "1440");
    assert_eq!(text(4), "2");
}

#[test]
fn display_free_load_uses_safe_output_defaults() {
    let (_directory, path) = fixture(
        r#"
        local output = services.output
        return ui.panel { children = { ui.text { text = table.concat({
            tostring(output.available:get()), output.name:get(),
            tostring(output.width:get()), tostring(output.height:get()),
            tostring(output.scale:get()),
        }, '|') } } }
        "#,
    );
    let mut runtime = Runtime::load(path).unwrap();
    assert_eq!(runtime.root().children[0].text, "false||0|0|1");
    assert!(
        runtime
            .set_service("output.width", Scalar::Number(1920.0))
            .is_err()
    );
    assert!(!runtime.refresh().unwrap());
}

#[test]
fn invalid_output_update_preserves_the_current_context() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = { ui.text { text = function()
            return services.output.name:get()
        end } } }
        "#,
    );
    let mut runtime = Runtime::load_for_output(
        path,
        output("DP-1", 1920, 1080, 120),
        &ServiceSnapshot::default(),
    )
    .unwrap();
    let invalid = OutputContext {
        name: Some("HDMI-A-1".into()),
        width: 0,
        height: 1440,
        scale: Scale::default(),
    };
    assert!(runtime.set_output(invalid).is_err());
    assert!(!runtime.refresh().unwrap());
    assert_eq!(runtime.root().children[0].text, "DP-1");

    let invalid = OutputContext {
        name: Some("bad\0name".into()),
        width: 1920,
        height: 1080,
        scale: Scale::default(),
    };
    assert!(
        Runtime::load_for_output(&runtime.files()[0], invalid, &ServiceSnapshot::default())
            .is_err()
    );
}

#[test]
fn runtimes_keep_output_context_and_local_state_isolated() {
    let (_directory, path) = fixture(
        r#"
        local count = ui.state(0)
        return ui.panel { children = {
            ui.text { text = function()
                return `{services.output.name:get()}:{count:get()}`
            end },
            ui.button { on_click = function() count:set(count:get() + 1) end },
        } }
        "#,
    );
    let snapshot = ServiceSnapshot::default();
    let mut left =
        Runtime::load_for_output(&path, output("DP-1", 1920, 1080, 120), &snapshot).unwrap();
    let right = Runtime::load_for_output(&path, output("DP-2", 1280, 720, 120), &snapshot).unwrap();

    left.dispatch(left.root().children[1].id, Event::Click)
        .unwrap();
    assert_eq!(left.root().children[0].text, "DP-1:1");
    assert_eq!(right.root().children[0].text, "DP-2:0");
}

#[test]
fn replacement_load_preserves_output_and_services_without_mutating_the_old_runtime() {
    let (_directory, path) = fixture(
        r#"
        local count = ui.state(0)
        return ui.panel { children = {
            ui.text { text = function() return tostring(count:get()) end },
            ui.text { text = services.output.name:get() },
            ui.text { text = services.clock.time:get() },
            ui.button { on_click = function() count:set(count:get() + 1) end },
        } }
        "#,
    );
    let mut snapshot = ServiceSnapshot::default();
    snapshot
        .set_service("clock.time", Scalar::String("12:34".into()))
        .unwrap();
    let mut runtime =
        Runtime::load_for_output(&path, output("DP-1", 1920, 1080, 120), &snapshot).unwrap();
    runtime
        .dispatch(runtime.root().children[3].id, Event::Click)
        .unwrap();

    let replacement = runtime.load_replacement(&path).unwrap();
    assert_eq!(runtime.root().children[0].text, "1");
    assert_eq!(replacement.root().children[0].text, "0");
    assert_eq!(replacement.root().children[1].text, "DP-1");
    assert_eq!(replacement.root().children[2].text, "12:34");
}

#[test]
fn service_snapshot_rejects_invalid_replacements_atomically() {
    let (_directory, path) = fixture(
        r#"
        return ui.panel { children = {
            ui.text { text = services.clock.time:get() },
            ui.text { text = function()
                local devices = services.audio.devices:get()
                return devices[1].description
            end },
            ui.text { text = function()
                local windows = services.niri.windows:get()
                return windows[1].title
            end },
        } }
        "#,
    );
    let mut snapshot = ServiceSnapshot::default();
    snapshot
        .set_service("clock.time", Scalar::String("12:34".into()))
        .unwrap();
    assert!(
        snapshot
            .set_service("clock.time", Scalar::Number(1234.0))
            .is_err()
    );
    snapshot
        .set_audio_devices(vec![AudioDevice {
            name: "speakers".into(),
            description: "Speakers".into(),
        }])
        .unwrap();
    assert!(
        snapshot
            .set_audio_devices(vec![
                AudioDevice {
                    name: "duplicate".into(),
                    description: "First".into(),
                },
                AudioDevice {
                    name: "duplicate".into(),
                    description: "Second".into(),
                },
            ])
            .is_err()
    );
    snapshot
        .set_niri(NiriState {
            windows: vec![NiriWindow {
                id: 1,
                workspace_id: None,
                title: Some("Terminal".into()),
                app_id: None,
                focused: false,
                urgent: false,
                floating: false,
                column: None,
                row: None,
                width: 800.0,
                height: 600.0,
            }],
            ..NiriState::default()
        })
        .unwrap();
    let duplicate = NiriWindow {
        id: 2,
        workspace_id: None,
        title: Some("Invalid".into()),
        app_id: None,
        focused: false,
        urgent: false,
        floating: false,
        column: None,
        row: None,
        width: 800.0,
        height: 600.0,
    };
    assert!(
        snapshot
            .set_niri(NiriState {
                windows: vec![duplicate.clone(), duplicate],
                ..NiriState::default()
            })
            .is_err()
    );

    let runtime =
        Runtime::load_for_output(path, output("DP-1", 1920, 1080, 120), &snapshot).unwrap();
    assert_eq!(runtime.root().children[0].text, "12:34");
    assert_eq!(runtime.root().children[1].text, "Speakers");
    assert_eq!(runtime.root().children[2].text, "Terminal");
}
