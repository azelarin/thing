use thing_runtime::PowerDevice;

const MAX_TEXT_BYTES: usize = 4096;

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Device {
    pub path: String,
    pub kind: u32,
    pub state: u32,
    pub power_supply: bool,
    pub present: Option<bool>,
    pub online: Option<bool>,
    pub percentage: Option<f64>,
    pub time_to_empty: Option<i64>,
    pub time_to_full: Option<i64>,
    pub energy: Option<f64>,
    pub energy_empty: Option<f64>,
    pub energy_full: Option<f64>,
    pub energy_full_design: Option<f64>,
    pub energy_rate: Option<f64>,
    pub capacity: Option<f64>,
    pub native_path: String,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub icon_name: String,
    pub warning_level: u32,
    pub battery_level: Option<u32>,
}

impl Device {
    pub fn project(self) -> PowerDevice {
        let charge_device = matches!(self.kind, 2..=3 | 5..=28);
        let energy_device = matches!(self.kind, 2 | 3);
        PowerDevice {
            id: self.path,
            kind: kind(self.kind).into(),
            state: state(self.state).into(),
            power_supply: self.power_supply,
            present: charge_device.then_some(self.present).flatten(),
            online: (self.kind == 1).then_some(self.online).flatten(),
            percentage: charge_device
                .then(|| self.percentage.and_then(percentage))
                .flatten(),
            // UPower defines zero specifically as unknown for both estimates.
            time_to_empty: energy_device
                .then(|| self.time_to_empty.and_then(positive_time))
                .flatten(),
            time_to_full: energy_device
                .then(|| self.time_to_full.and_then(positive_time))
                .flatten(),
            // unlike the time estimates, zero energy and rate values can describe
            // a real empty or idle device and must not be treated as missing.
            energy: energy_device
                .then(|| self.energy.and_then(nonnegative))
                .flatten(),
            energy_empty: energy_device
                .then(|| self.energy_empty.and_then(nonnegative))
                .flatten(),
            energy_full: energy_device
                .then(|| self.energy_full.and_then(nonnegative))
                .flatten(),
            energy_full_design: energy_device
                .then(|| self.energy_full_design.and_then(nonnegative))
                .flatten(),
            energy_rate: energy_device
                .then(|| self.energy_rate.and_then(finite))
                .flatten(),
            capacity: energy_device
                .then(|| self.capacity.and_then(percentage))
                .flatten(),
            native_path: optional_text(self.native_path),
            vendor: optional_text(self.vendor),
            model: optional_text(self.model),
            serial: optional_text(self.serial),
            icon_name: optional_text(self.icon_name),
            warning_level: warning_level(self.warning_level).into(),
            battery_level: self
                .battery_level
                .and_then(battery_level)
                .map(str::to_owned),
        }
    }
}

fn positive_time(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn nonnegative(value: f64) -> Option<f64> {
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn percentage(value: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}

fn optional_text(mut value: String) -> Option<String> {
    value.retain(|character| character != '\0');
    if value.len() > MAX_TEXT_BYTES {
        let mut boundary = MAX_TEXT_BYTES;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    (!value.is_empty()).then_some(value)
}

fn kind(value: u32) -> &'static str {
    match value {
        1 => "line_power",
        2 => "battery",
        3 => "ups",
        4 => "monitor",
        5 => "mouse",
        6 => "keyboard",
        7 => "pda",
        8 => "phone",
        9 => "media_player",
        10 => "tablet",
        11 => "computer",
        12 => "gaming_input",
        13 => "pen",
        14 => "touchpad",
        15 => "modem",
        16 => "network",
        17 => "headset",
        18 => "speakers",
        19 => "headphones",
        20 => "video",
        21 => "other_audio",
        22 => "remote_control",
        23 => "printer",
        24 => "scanner",
        25 => "camera",
        26 => "wearable",
        27 => "toy",
        28 => "bluetooth_generic",
        _ => "unknown",
    }
}

fn state(value: u32) -> &'static str {
    match value {
        1 => "charging",
        2 => "discharging",
        3 => "empty",
        4 => "fully_charged",
        5 => "pending_charge",
        6 => "pending_discharge",
        _ => "unknown",
    }
}

fn warning_level(value: u32) -> &'static str {
    match value {
        1 => "none",
        2 => "discharging",
        3 => "low",
        4 => "critical",
        5 => "action",
        _ => "unknown",
    }
}

fn battery_level(value: u32) -> Option<&'static str> {
    match value {
        1 => None,
        3 => Some("low"),
        4 => Some("critical"),
        6 => Some("normal"),
        7 => Some("high"),
        8 => Some("full"),
        _ => Some("unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_battery_keeps_meaningful_zero_measurements() {
        let device = Device {
            path: "/org/freedesktop/UPower/devices/battery_BAT0".into(),
            kind: 2,
            state: 3,
            present: Some(true),
            percentage: Some(0.0),
            energy: Some(0.0),
            energy_empty: Some(0.0),
            energy_full: Some(50.0),
            energy_full_design: Some(55.0),
            energy_rate: Some(0.0),
            capacity: Some(0.0),
            ..Device::default()
        }
        .project();

        assert_eq!(device.percentage, Some(0.0));
        assert_eq!(device.energy, Some(0.0));
        assert_eq!(device.energy_empty, Some(0.0));
        assert_eq!(device.energy_rate, Some(0.0));
        assert_eq!(device.capacity, Some(0.0));
        assert_eq!(device.time_to_empty, None);
        assert_eq!(device.time_to_full, None);
    }

    #[test]
    fn non_applicable_metadata_is_absent_instead_of_fabricated() {
        let device = Device {
            path: "/org/freedesktop/UPower/devices/line_power_AC".into(),
            kind: 1,
            online: Some(false),
            percentage: Some(100.0),
            energy: Some(20.0),
            native_path: String::new(),
            ..Device::default()
        }
        .project();

        assert_eq!(device.online, Some(false));
        assert_eq!(device.present, None);
        assert_eq!(device.percentage, None);
        assert_eq!(device.energy, None);
        assert_eq!(device.native_path, None);
    }

    #[test]
    fn optional_text_is_bounded_on_utf8_boundaries() {
        let value = format!("{}éé", "a".repeat(MAX_TEXT_BYTES - 1));
        let value = optional_text(value).unwrap();
        assert_eq!(value.len(), MAX_TEXT_BYTES - 1);
        assert!(!value.contains('\0'));
    }
}
