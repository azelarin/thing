use crate::OutputContext;
use crate::niri::{NiriAction, NiriData, NiriState, parse_id};
use crate::state::{Scalar, Signal, Tracker};
use mlua::{Error, Lua, Result, UserData, UserDataMethods, Value};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

/// a request to a desktop service.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    SetVolume(f64),
    SetMuted(bool),
    SetDefaultSink(String),
    Niri(NiriAction),
}

/// an output device identified by its stable server name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    pub name: String,
    pub description: String,
}

/// desktop service values used when loading a runtime.
#[derive(Clone, Debug, Default)]
pub struct ServiceSnapshot {
    pub(crate) values: BTreeMap<String, Scalar>,
    pub(crate) devices: Vec<AudioDevice>,
    pub(crate) niri: NiriState,
}

impl ServiceSnapshot {
    pub fn set_service(&mut self, name: &str, value: Scalar) -> Result<()> {
        validate_service(name, &value)?;
        self.values.insert(name.to_owned(), value);
        Ok(())
    }

    pub fn set_audio_devices(&mut self, devices: Vec<AudioDevice>) -> Result<()> {
        validate_devices(&devices)?;
        self.devices = devices;
        Ok(())
    }

    pub fn set_niri(&mut self, state: NiriState) -> Result<()> {
        state.validate()?;
        self.niri = state;
        Ok(())
    }
}

struct DeviceList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    devices: Rc<RefCell<Vec<AudioDevice>>>,
}

impl UserData for DeviceList {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let list = lua.create_table()?;
            for (index, device) in this.devices.borrow().iter().enumerate() {
                let item = lua.create_table()?;
                item.set("name", device.name.as_str())?;
                item.set("description", device.description.as_str())?;
                item.set_readonly(true);
                list.raw_set(index + 1, item)?;
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
}

pub(crate) struct Services {
    tracker: Rc<RefCell<Tracker>>,
    signals: BTreeMap<String, usize>,
    actions: Rc<RefCell<Vec<Action>>>,
    allowed: Rc<Cell<bool>>,
    devices: Rc<RefCell<Vec<AudioDevice>>>,
    devices_revision: usize,
    niri: NiriData,
}

impl Services {
    pub fn install(lua: &Lua, tracker: Rc<RefCell<Tracker>>) -> Result<Self> {
        let devices_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let niri = lua.create_table()?;
        let niri_data = NiriData::install(&niri, tracker.clone())?;
        let mut result = Self {
            tracker,
            signals: BTreeMap::new(),
            actions: Rc::default(),
            allowed: Rc::default(),
            devices: Rc::default(),
            devices_revision,
            niri: niri_data,
        };
        let services = lua.create_table()?;
        let audio = lua.create_table()?;
        let clock = lua.create_table()?;
        let output = lua.create_table()?;
        audio.set(
            "devices",
            DeviceList {
                tracker: result.tracker.clone(),
                revision: devices_revision,
                devices: result.devices.clone(),
            },
        )?;
        for (name, value) in [
            ("audio.available", Scalar::Boolean(false)),
            ("audio.volume", Scalar::Number(0.0)),
            ("audio.muted", Scalar::Boolean(false)),
            ("audio.device", Scalar::String(String::new())),
            ("audio.error", Scalar::String(String::new())),
            ("clock.time", Scalar::String("--:--".into())),
            ("clock.date", Scalar::String(String::new())),
            ("niri.available", Scalar::Boolean(false)),
            ("niri.error", Scalar::String(String::new())),
            ("output.available", Scalar::Boolean(false)),
            ("output.name", Scalar::String(String::new())),
            ("output.width", Scalar::Number(0.0)),
            ("output.height", Scalar::Number(0.0)),
            ("output.scale", Scalar::Number(1.0)),
        ] {
            let id = result.tracker.borrow_mut().insert(value)?;
            result.signals.insert(name.into(), id);
            let signal = Signal {
                id,
                tracker: result.tracker.clone(),
                readonly: true,
            };
            let (service, property) = name
                .split_once('.')
                .ok_or_else(|| Error::runtime("invalid service name"))?;
            match service {
                "audio" => audio.set(property, signal)?,
                "clock" => clock.set(property, signal)?,
                "niri" => niri.set(property, signal)?,
                "output" => output.set(property, signal)?,
                _ => return Err(Error::runtime("invalid service name")),
            }
        }
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        audio.set(
            "set_volume",
            lua.create_function(move |_, value: f64| {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Err(Error::runtime("volume must be between 0 and 1"));
                }
                enqueue(&actions, &allowed, Action::SetVolume(value))
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        audio.set(
            "set_muted",
            lua.create_function(move |_, value: Value| {
                let Value::Boolean(value) = value else {
                    return Err(Error::runtime("mute must be a boolean"));
                };
                enqueue(&actions, &allowed, Action::SetMuted(value))
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        audio.set(
            "set_default_sink",
            lua.create_function(move |_, name: String| {
                if name.is_empty() || name.contains('\0') {
                    return Err(Error::runtime(
                        "device name must be nonempty and contain no NUL",
                    ));
                }
                enqueue(&actions, &allowed, Action::SetDefaultSink(name))
            })?,
        )?;
        audio.set_readonly(true);
        for (name, action) in [
            (
                "focus_workspace",
                NiriAction::FocusWorkspace as fn(u64) -> NiriAction,
            ),
            ("focus_window", NiriAction::FocusWindow),
            ("close_window", NiriAction::CloseWindow),
        ] {
            let actions = result.actions.clone();
            let allowed = result.allowed.clone();
            niri.set(
                name,
                lua.create_function(move |_, value: Value| {
                    enqueue(&actions, &allowed, Action::Niri(action(parse_id(value)?)))
                })?,
            )?;
        }
        niri.set_readonly(true);
        clock.set_readonly(true);
        output.set_readonly(true);
        services.set("audio", audio)?;
        services.set("clock", clock)?;
        services.set("niri", niri)?;
        services.set("output", output)?;
        services.set_readonly(true);
        lua.globals().set("services", services)?;
        Ok(result)
    }
    pub fn allow_actions(&self, allowed: bool) {
        self.allowed.set(allowed);
    }
    pub fn take_actions(&self) -> Vec<Action> {
        std::mem::take(&mut self.actions.borrow_mut())
    }
    pub fn action_count(&self) -> usize {
        self.actions.borrow().len()
    }
    pub fn truncate_actions(&self, count: usize) {
        self.actions.borrow_mut().truncate(count);
    }
    pub fn set(&self, name: &str, value: Scalar) -> Result<()> {
        if name.starts_with("output.") {
            return Err(Error::runtime("output properties must be updated together"));
        }
        let id = self
            .signals
            .get(name)
            .ok_or_else(|| Error::runtime(format!("unknown service property {name}")))?;
        validate_service(name, &value)?;
        self.tracker.borrow_mut().write(*id, value)
    }
    pub fn set_devices(&self, devices: Vec<AudioDevice>) -> Result<()> {
        validate_devices(&devices)?;
        if *self.devices.borrow() != devices {
            *self.devices.borrow_mut() = devices;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.devices_revision);
            tracker.write(
                self.devices_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            )?;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> ServiceSnapshot {
        let mut tracker = self.tracker.borrow_mut();
        let values = self
            .signals
            .iter()
            .filter(|(name, _)| !name.starts_with("output."))
            .map(|(name, id)| (name.clone(), tracker.read(*id)))
            .collect();
        ServiceSnapshot {
            values,
            devices: self.devices.borrow().clone(),
            niri: self.niri.snapshot(),
        }
    }

    pub fn set_output(&self, output: &OutputContext) -> Result<()> {
        output.validate()?;
        let name = output.name.clone().unwrap_or_default();
        let values = [
            ("output.available", Scalar::Boolean(true)),
            ("output.name", Scalar::String(name)),
            ("output.width", Scalar::Number(f64::from(output.width))),
            ("output.height", Scalar::Number(f64::from(output.height))),
            (
                "output.scale",
                Scalar::Number(f64::from(output.scale.factor())),
            ),
        ];
        let updates = values
            .into_iter()
            .map(|(name, value)| (self.signals[name], value));
        self.tracker.borrow_mut().write_batch(updates)
    }

    pub fn set_niri(&self, state: NiriState) -> Result<()> {
        self.niri.set(state)
    }
}

fn validate_service(name: &str, value: &Scalar) -> Result<()> {
    let valid = match (name, value) {
        ("audio.available" | "audio.muted" | "niri.available", Scalar::Boolean(_)) => true,
        ("audio.volume", Scalar::Number(value)) => value.is_finite() && *value >= 0.0,
        (
            "audio.device" | "audio.error" | "clock.time" | "clock.date" | "niri.error",
            Scalar::String(value),
        ) => value.len() <= 4096 && !value.contains('\0'),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Error::runtime(format!(
            "invalid value for service property {name}"
        )))
    }
}

fn validate_devices(devices: &[AudioDevice]) -> Result<()> {
    let mut names = BTreeSet::new();
    let invalid = devices.len() > 512
        || devices.iter().any(|device| {
            device.name.is_empty()
                || device.name.len() > 4096
                || device.name.contains('\0')
                || device.description.len() > 4096
                || device.description.contains('\0')
                || !names.insert(device.name.as_str())
        });
    if invalid {
        Err(Error::runtime("invalid audio device list"))
    } else {
        Ok(())
    }
}

fn enqueue(actions: &RefCell<Vec<Action>>, allowed: &Cell<bool>, action: Action) -> Result<()> {
    if !allowed.get() {
        return Err(Error::runtime(
            "service actions are allowed only in event handlers",
        ));
    }
    let mut actions = actions.borrow_mut();
    if actions.len() >= 128 {
        return Err(Error::runtime("too many service actions in one event"));
    }
    actions.push(action);
    Ok(())
}
