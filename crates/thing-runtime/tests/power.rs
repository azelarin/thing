use std::fs;

use thing_runtime::{Event, PowerDevice, PowerSnapshot, Runtime};

fn runtime(source: &str) -> (tempfile::TempDir, Runtime) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    let runtime = Runtime::load(path).unwrap();
    (directory, runtime)
}

fn battery(path: &str) -> PowerDevice {
    PowerDevice {
        id: path.into(),
        kind: "battery".into(),
        state: "empty".into(),
        power_supply: true,
        present: Some(true),
        online: None,
        percentage: Some(0.0),
        time_to_empty: None,
        time_to_full: None,
        energy: Some(0.0),
        energy_empty: Some(0.0),
        energy_full: Some(50.0),
        energy_full_design: Some(55.0),
        energy_rate: Some(0.0),
        capacity: Some(90.0),
        native_path: Some("BAT0".into()),
        vendor: None,
        model: Some("test battery".into()),
        serial: None,
        icon_name: Some("battery-empty-symbolic".into()),
        warning_level: "critical".into(),
        battery_level: None,
    }
}

#[test]
fn power_snapshot_is_reactive_and_preserves_optional_values() {
    let (_directory, mut runtime) = runtime(
        r#"
        return ui.panel { children = { ui.text {
            text = function()
                local display = services.power.display:get() or {}
                local devices = services.power.devices:get()
                local first = devices[1] or { id = '', state = '' }
                return table.concat({
                    tostring(services.power.available:get()),
                    tostring(services.power.on_battery:get()),
                    tostring(display.percentage),
                    tostring(display.energy),
                    tostring(display.energy_rate),
                    tostring(display.time_to_empty),
                    tostring(display.vendor),
                    tostring(#devices),
                    first.id,
                    first.state,
                }, '|')
            end,
        } } }
        "#,
    );
    let display = battery("/org/freedesktop/UPower/devices/DisplayDevice");
    let device = battery("/org/freedesktop/UPower/devices/battery_BAT0");
    runtime
        .set_power(PowerSnapshot {
            available: true,
            on_battery: true,
            display: Some(display),
            devices: vec![device],
            error: String::new(),
        })
        .unwrap();
    assert!(runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true|true|0|0|0|nil|nil|1|/org/freedesktop/UPower/devices/battery_BAT0|empty"
    );

    let invalid = PowerSnapshot {
        available: false,
        on_battery: true,
        ..PowerSnapshot::default()
    };
    assert!(runtime.set_power(invalid).is_err());
    assert!(!runtime.refresh().unwrap());
    assert_eq!(
        runtime.root().unwrap().children[0].text,
        "true|true|0|0|0|nil|nil|1|/org/freedesktop/UPower/devices/battery_BAT0|empty"
    );
}

#[test]
fn power_device_records_and_lists_are_read_only() {
    for mutation in [
        "services.power.display:get().state = 'changed'",
        "services.power.devices:get()[1].state = 'changed'",
        "services.power.devices:get()[1] = {}",
    ] {
        let source = format!(
            r#"
            return ui.panel {{ children = {{ ui.button {{ on_click = function()
                {mutation}
            end }} }} }}
            "#,
        );
        let (_directory, mut runtime) = runtime(&source);
        let display = battery("/org/freedesktop/UPower/devices/DisplayDevice");
        let device = battery("/org/freedesktop/UPower/devices/battery_BAT0");
        runtime
            .set_power(PowerSnapshot {
                available: true,
                display: Some(display),
                devices: vec![device],
                ..PowerSnapshot::default()
            })
            .unwrap();

        assert!(
            runtime
                .dispatch(runtime.root().unwrap().children[0].id, Event::Click)
                .is_err(),
            "accepted {mutation}"
        );
    }
}

#[test]
fn power_service_is_read_only() {
    for source in [
        "services.power.available = true; return ui.panel {}",
        "services.power.display = {}; return ui.panel {}",
        "services.power.devices = {}; return ui.panel {}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        assert!(Runtime::load(path).is_err(), "accepted {source}");
    }
}
