use crate::OutputContext;
use crate::niri::{NiriAction, NiriData, NiriState, parse_id};
use crate::state::{Scalar, Signal, Tracker};
use mlua::{Error, Lua, Result, Table, UserData, UserDataMethods, Value};
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
    SetStreamVolume {
        id: String,
        volume: f64,
    },
    SetStreamMuted {
        id: String,
        muted: bool,
    },
    SetSourceVolume(f64),
    SetSourceMuted(bool),
    SetDefaultSource(String),
    Niri(NiriAction),
    Media {
        action: MediaAction,
        player: Option<String>,
    },
    Launch {
        argv: Vec<String>,
        cwd: Option<String>,
    },
}

/// a media playback control request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaAction {
    PlayPause,
    Next,
    Previous,
    Stop,
}

const MAX_LAUNCH_ARGUMENTS: usize = 256;
const MAX_LAUNCH_ARGUMENT_BYTES: usize = 4096;
const MAX_LAUNCH_BYTES: usize = 64 * 1024;

/// an audio device identified by its stable server name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    pub name: String,
    pub description: String,
}

/// one output playback stream and its server-provided application metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioStream {
    pub id: String,
    pub application: String,
    pub app_id: String,
    pub name: String,
    pub icon: String,
    pub volume: f64,
    pub muted: bool,
    pub volume_writable: bool,
}

/// one input device, including output monitors exposed by the audio server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSource {
    pub name: String,
    pub description: String,
    pub monitor: bool,
}

/// one confirmed generation of output and input audio state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioSnapshot {
    pub available: bool,
    pub volume: f64,
    pub muted: bool,
    pub device: String,
    pub devices: Vec<AudioDevice>,
    pub streams: Vec<AudioStream>,
    pub source_available: bool,
    pub source_volume: f64,
    pub source_muted: bool,
    pub source_device: String,
    pub sources: Vec<AudioDevice>,
    pub all_sources: Vec<AudioSource>,
    pub error: String,
}

/// one media player ownership lifetime and its current state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPlayer {
    pub id: String,
    pub name: String,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub art_url: String,
    pub identity: String,
    pub desktop_entry: String,
    pub can_play_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_stop: bool,
}

impl AudioSnapshot {
    fn values(&self) -> [(&'static str, Scalar); 9] {
        [
            ("audio.available", Scalar::Boolean(self.available)),
            ("audio.volume", Scalar::Number(self.volume)),
            ("audio.muted", Scalar::Boolean(self.muted)),
            ("audio.device", Scalar::String(self.device.clone())),
            (
                "audio.source_available",
                Scalar::Boolean(self.source_available),
            ),
            ("audio.source_volume", Scalar::Number(self.source_volume)),
            ("audio.source_muted", Scalar::Boolean(self.source_muted)),
            (
                "audio.source_device",
                Scalar::String(self.source_device.clone()),
            ),
            ("audio.error", Scalar::String(self.error.clone())),
        ]
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in self.values() {
            validate_service(name, &value)?;
        }
        validate_devices(&self.devices)?;
        validate_devices(&self.sources)?;
        validate_sources(&self.all_sources)?;
        validate_streams(&self.streams)
    }
}

/// one confirmed generation of media player state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaSnapshot {
    pub available: bool,
    pub selected_id: String,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub art_url: String,
    pub identity: String,
    pub desktop_entry: String,
    pub can_play_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_stop: bool,
    pub error: String,
    pub players: Vec<MediaPlayer>,
}

impl MediaSnapshot {
    fn values(&self) -> [(&'static str, Scalar); 13] {
        [
            ("media.available", Scalar::Boolean(self.available)),
            (
                "media.selected_id",
                Scalar::String(self.selected_id.clone()),
            ),
            ("media.playing", Scalar::Boolean(self.playing)),
            ("media.title", Scalar::String(self.title.clone())),
            ("media.artist", Scalar::String(self.artist.clone())),
            ("media.art_url", Scalar::String(self.art_url.clone())),
            ("media.identity", Scalar::String(self.identity.clone())),
            (
                "media.desktop_entry",
                Scalar::String(self.desktop_entry.clone()),
            ),
            ("media.can_play_pause", Scalar::Boolean(self.can_play_pause)),
            ("media.can_next", Scalar::Boolean(self.can_next)),
            ("media.can_previous", Scalar::Boolean(self.can_previous)),
            ("media.can_stop", Scalar::Boolean(self.can_stop)),
            ("media.error", Scalar::String(self.error.clone())),
        ]
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in self.values() {
            validate_service(name, &value)?;
        }
        validate_media_players(&self.players)?;
        let selected = self
            .players
            .iter()
            .find(|player| player.id == self.selected_id);
        let has_players = !self.players.is_empty();
        let selected_matches = selected.map_or_else(
            || {
                self.selected_id.is_empty()
                    && !self.playing
                    && self.title.is_empty()
                    && self.artist.is_empty()
                    && self.art_url.is_empty()
                    && self.identity.is_empty()
                    && self.desktop_entry.is_empty()
                    && !self.can_play_pause
                    && !self.can_next
                    && !self.can_previous
                    && !self.can_stop
            },
            |player| {
                player.playing == self.playing
                    && player.title == self.title
                    && player.artist == self.artist
                    && player.art_url == self.art_url
                    && player.identity == self.identity
                    && player.desktop_entry == self.desktop_entry
                    && player.can_play_pause == self.can_play_pause
                    && player.can_next == self.can_next
                    && player.can_previous == self.can_previous
                    && player.can_stop == self.can_stop
            },
        );
        if self.available != selected.is_some()
            || self.available != has_players
            || !selected_matches
        {
            return Err(Error::runtime("inconsistent media player snapshot"));
        }
        Ok(())
    }
}

/// desktop service values used when loading a runtime.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServiceSnapshot {
    pub(crate) values: BTreeMap<String, Scalar>,
    pub(crate) devices: Vec<AudioDevice>,
    pub(crate) sources: Vec<AudioDevice>,
    pub(crate) all_sources: Vec<AudioSource>,
    pub(crate) streams: Vec<AudioStream>,
    pub(crate) media_players: Vec<MediaPlayer>,
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

    pub fn set_audio_sources(&mut self, sources: Vec<AudioDevice>) -> Result<()> {
        validate_devices(&sources)?;
        self.sources = sources;
        Ok(())
    }

    pub fn set_all_audio_sources(&mut self, sources: Vec<AudioSource>) -> Result<()> {
        validate_sources(&sources)?;
        self.all_sources = sources;
        Ok(())
    }

    pub fn set_audio(&mut self, audio: AudioSnapshot) -> Result<()> {
        audio.validate()?;
        for (name, value) in audio.values() {
            self.values.insert(name.into(), value);
        }
        self.devices = audio.devices;
        self.sources = audio.sources;
        self.all_sources = audio.all_sources;
        self.streams = audio.streams;
        Ok(())
    }

    pub fn set_media(&mut self, media: MediaSnapshot) -> Result<()> {
        media.validate()?;
        for (name, value) in media.values() {
            self.values.insert(name.into(), value);
        }
        self.media_players = media.players;
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

struct MediaPlayerList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    players: Rc<RefCell<Vec<MediaPlayer>>>,
}

impl UserData for MediaPlayerList {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let list = lua.create_table()?;
            for (index, player) in this.players.borrow().iter().enumerate() {
                let item = lua.create_table()?;
                item.set("id", player.id.as_str())?;
                item.set("name", player.name.as_str())?;
                item.set("playing", player.playing)?;
                item.set("title", player.title.as_str())?;
                item.set("artist", player.artist.as_str())?;
                item.set("art_url", player.art_url.as_str())?;
                item.set("identity", player.identity.as_str())?;
                item.set("desktop_entry", player.desktop_entry.as_str())?;
                item.set("can_play_pause", player.can_play_pause)?;
                item.set("can_next", player.can_next)?;
                item.set("can_previous", player.can_previous)?;
                item.set("can_stop", player.can_stop)?;
                item.set_readonly(true);
                list.raw_set(index + 1, item)?;
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
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

struct StreamList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    streams: Rc<RefCell<Vec<AudioStream>>>,
}

impl UserData for StreamList {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let list = lua.create_table()?;
            for (index, stream) in this.streams.borrow().iter().enumerate() {
                let item = lua.create_table()?;
                item.set("id", stream.id.as_str())?;
                item.set("application", stream.application.as_str())?;
                item.set("app_id", stream.app_id.as_str())?;
                item.set("name", stream.name.as_str())?;
                item.set("icon", stream.icon.as_str())?;
                item.set("volume", stream.volume)?;
                item.set("muted", stream.muted)?;
                item.set("volume_writable", stream.volume_writable)?;
                item.set_readonly(true);
                list.raw_set(index + 1, item)?;
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
}

struct SourceList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    sources: Rc<RefCell<Vec<AudioSource>>>,
}

impl UserData for SourceList {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let list = lua.create_table()?;
            for (index, source) in this.sources.borrow().iter().enumerate() {
                let item = lua.create_table()?;
                item.set("name", source.name.as_str())?;
                item.set("description", source.description.as_str())?;
                item.set("monitor", source.monitor)?;
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
    sources: Rc<RefCell<Vec<AudioDevice>>>,
    sources_revision: usize,
    all_sources: Rc<RefCell<Vec<AudioSource>>>,
    all_sources_revision: usize,
    streams: Rc<RefCell<Vec<AudioStream>>>,
    streams_revision: usize,
    media_players: Rc<RefCell<Vec<MediaPlayer>>>,
    media_players_revision: usize,
    niri: NiriData,
}

impl Services {
    pub fn install(lua: &Lua, tracker: Rc<RefCell<Tracker>>) -> Result<Self> {
        let devices_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let sources_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let all_sources_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let streams_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let media_players_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let niri = lua.create_table()?;
        let niri_data = NiriData::install(&niri, tracker.clone())?;
        let mut result = Self {
            tracker,
            signals: BTreeMap::new(),
            actions: Rc::default(),
            allowed: Rc::default(),
            devices: Rc::default(),
            devices_revision,
            sources: Rc::default(),
            sources_revision,
            all_sources: Rc::default(),
            all_sources_revision,
            streams: Rc::default(),
            streams_revision,
            media_players: Rc::default(),
            media_players_revision,
            niri: niri_data,
        };
        let services = lua.create_table()?;
        let audio = lua.create_table()?;
        let clock = lua.create_table()?;
        let media = lua.create_table()?;
        let output = lua.create_table()?;
        audio.set(
            "devices",
            DeviceList {
                tracker: result.tracker.clone(),
                revision: devices_revision,
                devices: result.devices.clone(),
            },
        )?;
        audio.set(
            "sources",
            DeviceList {
                tracker: result.tracker.clone(),
                revision: sources_revision,
                devices: result.sources.clone(),
            },
        )?;
        audio.set(
            "all_sources",
            SourceList {
                tracker: result.tracker.clone(),
                revision: all_sources_revision,
                sources: result.all_sources.clone(),
            },
        )?;
        audio.set(
            "streams",
            StreamList {
                tracker: result.tracker.clone(),
                revision: streams_revision,
                streams: result.streams.clone(),
            },
        )?;
        media.set(
            "players",
            MediaPlayerList {
                tracker: result.tracker.clone(),
                revision: media_players_revision,
                players: result.media_players.clone(),
            },
        )?;
        for (name, value) in [
            ("audio.available", Scalar::Boolean(false)),
            ("audio.volume", Scalar::Number(0.0)),
            ("audio.muted", Scalar::Boolean(false)),
            ("audio.device", Scalar::String(String::new())),
            ("audio.source_available", Scalar::Boolean(false)),
            ("audio.source_volume", Scalar::Number(0.0)),
            ("audio.source_muted", Scalar::Boolean(false)),
            ("audio.source_device", Scalar::String(String::new())),
            ("audio.error", Scalar::String(String::new())),
            ("clock.time", Scalar::String("--:--".into())),
            ("clock.date", Scalar::String(String::new())),
            ("media.available", Scalar::Boolean(false)),
            ("media.selected_id", Scalar::String(String::new())),
            ("media.playing", Scalar::Boolean(false)),
            ("media.title", Scalar::String(String::new())),
            ("media.artist", Scalar::String(String::new())),
            ("media.art_url", Scalar::String(String::new())),
            ("media.identity", Scalar::String(String::new())),
            ("media.desktop_entry", Scalar::String(String::new())),
            ("media.can_play_pause", Scalar::Boolean(false)),
            ("media.can_next", Scalar::Boolean(false)),
            ("media.can_previous", Scalar::Boolean(false)),
            ("media.can_stop", Scalar::Boolean(false)),
            ("media.error", Scalar::String(String::new())),
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
                "media" => media.set(property, signal)?,
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
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        audio.set(
            "set_stream_volume",
            lua.create_function(move |_, (id, volume): (String, f64)| {
                validate_stream_id(&id)?;
                if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
                    return Err(Error::runtime("stream volume must be between 0 and 1"));
                }
                enqueue(&actions, &allowed, Action::SetStreamVolume { id, volume })
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        audio.set(
            "set_stream_muted",
            lua.create_function(move |_, (id, value): (String, Value)| {
                validate_stream_id(&id)?;
                let Value::Boolean(muted) = value else {
                    return Err(Error::runtime("stream mute must be a boolean"));
                };
                enqueue(&actions, &allowed, Action::SetStreamMuted { id, muted })
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        audio.set(
            "set_source_volume",
            lua.create_function(move |_, value: f64| {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Err(Error::runtime("source volume must be between 0 and 1"));
                }
                enqueue(&actions, &allowed, Action::SetSourceVolume(value))
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        audio.set(
            "set_source_muted",
            lua.create_function(move |_, value: Value| {
                let Value::Boolean(value) = value else {
                    return Err(Error::runtime("source mute must be a boolean"));
                };
                enqueue(&actions, &allowed, Action::SetSourceMuted(value))
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        audio.set(
            "set_default_source",
            lua.create_function(move |_, name: String| {
                if name.is_empty() || name.contains('\0') {
                    return Err(Error::runtime(
                        "source name must be nonempty and contain no NUL",
                    ));
                }
                enqueue(&actions, &allowed, Action::SetDefaultSource(name))
            })?,
        )?;
        audio.set_readonly(true);
        for (name, action) in [
            ("play_pause", MediaAction::PlayPause),
            ("next", MediaAction::Next),
            ("previous", MediaAction::Previous),
            ("stop", MediaAction::Stop),
        ] {
            let actions = result.actions.clone();
            let allowed = result.allowed.clone();
            media.set(
                name,
                lua.create_function(move |_, player: Value| {
                    let player = media_player_id(player)?;
                    enqueue(&actions, &allowed, Action::Media { action, player })
                })?,
            )?;
        }
        media.set_readonly(true);
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
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        services.set(
            "launch",
            lua.create_function(move |_, (arguments, cwd): (Table, Value)| {
                let argv = launch_arguments(arguments)?;
                let cwd = launch_working_directory(cwd)?;
                enqueue(&actions, &allowed, Action::Launch { argv, cwd })
            })?,
        )?;
        services.set("audio", audio)?;
        services.set("clock", clock)?;
        services.set("media", media)?;
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

    pub fn set_sources(&self, sources: Vec<AudioDevice>) -> Result<()> {
        validate_devices(&sources)?;
        if *self.sources.borrow() != sources {
            *self.sources.borrow_mut() = sources;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.sources_revision);
            tracker.write(
                self.sources_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            )?;
        }
        Ok(())
    }

    pub fn set_all_sources(&self, sources: Vec<AudioSource>) -> Result<()> {
        validate_sources(&sources)?;
        if *self.all_sources.borrow() != sources {
            *self.all_sources.borrow_mut() = sources;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.all_sources_revision);
            tracker.write(
                self.all_sources_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            )?;
        }
        Ok(())
    }

    pub fn set_streams(&self, streams: Vec<AudioStream>) -> Result<()> {
        validate_streams(&streams)?;
        if *self.streams.borrow() != streams {
            *self.streams.borrow_mut() = streams;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.streams_revision);
            tracker.write(
                self.streams_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            )?;
        }
        Ok(())
    }

    pub fn set_audio(&self, audio: AudioSnapshot) -> Result<()> {
        audio.validate()?;
        let devices_changed = *self.devices.borrow() != audio.devices;
        let sources_changed = *self.sources.borrow() != audio.sources;
        let all_sources_changed = *self.all_sources.borrow() != audio.all_sources;
        let streams_changed = *self.streams.borrow() != audio.streams;
        let mut devices = self.devices.borrow_mut();
        let mut sources = self.sources.borrow_mut();
        let mut all_sources = self.all_sources.borrow_mut();
        let mut streams = self.streams.borrow_mut();
        let mut tracker = self.tracker.borrow_mut();
        let mut updates = audio
            .values()
            .into_iter()
            .map(|(name, value)| (self.signals[name], value))
            .collect::<Vec<_>>();
        if devices_changed {
            let revision = tracker.read(self.devices_revision);
            updates.push((
                self.devices_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        if sources_changed {
            let revision = tracker.read(self.sources_revision);
            updates.push((
                self.sources_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        if all_sources_changed {
            let revision = tracker.read(self.all_sources_revision);
            updates.push((
                self.all_sources_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        if streams_changed {
            let revision = tracker.read(self.streams_revision);
            updates.push((
                self.streams_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        tracker.write_batch(updates)?;
        if devices_changed {
            *devices = audio.devices;
        }
        if sources_changed {
            *sources = audio.sources;
        }
        if all_sources_changed {
            *all_sources = audio.all_sources;
        }
        if streams_changed {
            *streams = audio.streams;
        }
        Ok(())
    }

    pub fn set_media(&self, media: MediaSnapshot) -> Result<()> {
        media.validate()?;
        let players_changed = *self.media_players.borrow() != media.players;
        let mut players = self.media_players.borrow_mut();
        let mut tracker = self.tracker.borrow_mut();
        let mut updates = media
            .values()
            .into_iter()
            .map(|(name, value)| (self.signals[name], value))
            .collect::<Vec<_>>();
        if players_changed {
            let revision = tracker.read(self.media_players_revision);
            updates.push((
                self.media_players_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        tracker.write_batch(updates)?;
        if players_changed {
            *players = media.players;
        }
        Ok(())
    }

    pub fn set_media_players(&self, media_players: Vec<MediaPlayer>) -> Result<()> {
        validate_media_players(&media_players)?;
        if *self.media_players.borrow() != media_players {
            *self.media_players.borrow_mut() = media_players;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.media_players_revision);
            tracker.write(
                self.media_players_revision,
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
            sources: self.sources.borrow().clone(),
            all_sources: self.all_sources.borrow().clone(),
            streams: self.streams.borrow().clone(),
            media_players: self.media_players.borrow().clone(),
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

fn launch_arguments(table: Table) -> Result<Vec<String>> {
    let length = table.raw_len();
    if length == 0 {
        return Err(Error::runtime("launch requires at least one argument"));
    }
    if length > MAX_LAUNCH_ARGUMENTS {
        return Err(Error::runtime(format!(
            "launch accepts at most {MAX_LAUNCH_ARGUMENTS} arguments"
        )));
    }

    let mut arguments = Vec::with_capacity(length);
    let mut bytes = 0_usize;
    for index in 1..=length {
        let Value::String(value) = table.raw_get::<Value>(index)? else {
            return Err(Error::runtime(
                "launch arguments must be a dense array of strings",
            ));
        };
        let value = value
            .to_str()
            .map_err(|_| Error::runtime("launch arguments must be valid utf-8"))?;
        if value.len() > MAX_LAUNCH_ARGUMENT_BYTES {
            return Err(Error::runtime(format!(
                "launch arguments must not exceed {MAX_LAUNCH_ARGUMENT_BYTES} bytes each"
            )));
        }
        if value.contains('\0') {
            return Err(Error::runtime("launch arguments must contain no NUL bytes"));
        }
        bytes = bytes
            .checked_add(value.len())
            .ok_or_else(|| Error::runtime("launch arguments are too large"))?;
        if bytes > MAX_LAUNCH_BYTES {
            return Err(Error::runtime(format!(
                "launch arguments must not exceed {MAX_LAUNCH_BYTES} bytes in total"
            )));
        }
        arguments.push(value.to_owned());
    }
    if arguments[0].is_empty() {
        return Err(Error::runtime("launch program must be nonempty"));
    }
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::Integer(index) = key else {
            return Err(Error::runtime(
                "launch arguments must be a dense array of strings",
            ));
        };
        if !(1..=length as i64).contains(&index) {
            return Err(Error::runtime(
                "launch arguments must be a dense array of strings",
            ));
        }
    }
    Ok(arguments)
}

fn launch_working_directory(value: Value) -> Result<Option<String>> {
    let value = match value {
        Value::Nil => return Ok(None),
        Value::String(value) => value,
        _ => {
            return Err(Error::runtime(
                "launch working directory must be a string or nil",
            ));
        }
    };
    let value = value
        .to_str()
        .map_err(|_| Error::runtime("launch working directory must be valid utf-8"))?;
    if value.is_empty() || value.len() > MAX_LAUNCH_ARGUMENT_BYTES || value.contains('\0') {
        return Err(Error::runtime(
            "launch working directory must be nonempty, contain no NUL, and not exceed 4096 bytes",
        ));
    }
    Ok(Some(value.to_owned()))
}

fn media_player_id(value: Value) -> Result<Option<String>> {
    let value = match value {
        Value::Nil => return Ok(None),
        Value::String(value) => value,
        _ => return Err(Error::runtime("media player id must be a string or nil")),
    };
    let value = value
        .to_str()
        .map_err(|_| Error::runtime("media player id must be valid utf-8"))?;
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        return Err(Error::runtime(
            "media player id must be nonempty, contain no NUL, and not exceed 4096 bytes",
        ));
    }
    Ok(Some(value.to_owned()))
}

fn validate_service(name: &str, value: &Scalar) -> Result<()> {
    let valid = match (name, value) {
        (
            "audio.available"
            | "audio.muted"
            | "audio.source_available"
            | "audio.source_muted"
            | "media.available"
            | "media.playing"
            | "media.can_play_pause"
            | "media.can_next"
            | "media.can_previous"
            | "media.can_stop"
            | "niri.available",
            Scalar::Boolean(_),
        ) => true,
        ("audio.volume" | "audio.source_volume", Scalar::Number(value)) => {
            value.is_finite() && *value >= 0.0
        }
        (
            "audio.device"
            | "audio.source_device"
            | "audio.error"
            | "clock.time"
            | "clock.date"
            | "media.selected_id"
            | "media.title"
            | "media.artist"
            | "media.identity"
            | "media.desktop_entry"
            | "media.error"
            | "niri.error",
            Scalar::String(value),
        ) => value.len() <= 4096 && !value.contains('\0'),
        ("media.art_url", Scalar::String(value)) => {
            value.len() <= 16 * 1024 && !value.contains('\0')
        }
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

fn validate_stream_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 || id.contains('\0') {
        Err(Error::runtime("invalid audio stream id"))
    } else {
        Ok(())
    }
}

fn validate_streams(streams: &[AudioStream]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let invalid = streams.len() > 512
        || streams.iter().any(|stream| {
            validate_stream_id(&stream.id).is_err()
                || !stream.volume.is_finite()
                || stream.volume < 0.0
                || [
                    &stream.application,
                    &stream.app_id,
                    &stream.name,
                    &stream.icon,
                ]
                .into_iter()
                .any(|value| value.len() > 4096 || value.contains('\0'))
                || !ids.insert(stream.id.as_str())
        });
    if invalid {
        Err(Error::runtime("invalid audio stream list"))
    } else {
        Ok(())
    }
}

fn validate_sources(sources: &[AudioSource]) -> Result<()> {
    let mut names = BTreeSet::new();
    let invalid = sources.len() > 512
        || sources.iter().any(|source| {
            source.name.is_empty()
                || source.name.len() > 4096
                || source.name.contains('\0')
                || source.description.len() > 4096
                || source.description.contains('\0')
                || !names.insert(source.name.as_str())
        });
    if invalid {
        Err(Error::runtime("invalid audio source list"))
    } else {
        Ok(())
    }
}

fn validate_media_players(players: &[MediaPlayer]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let invalid = players.len() > 64
        || players.iter().any(|player| {
            player.id.is_empty()
                || player.id.len() > 4096
                || player.id.contains('\0')
                || !ids.insert(player.id.as_str())
                || player.name.is_empty()
                || invalid_media_text(&player.name, 4096)
                || !names.insert(player.name.as_str())
                || invalid_media_text(&player.title, 4096)
                || invalid_media_text(&player.artist, 4096)
                || invalid_media_text(&player.art_url, 16 * 1024)
                || invalid_media_text(&player.identity, 4096)
                || invalid_media_text(&player.desktop_entry, 4096)
        });
    if invalid {
        Err(Error::runtime("invalid media player list"))
    } else {
        Ok(())
    }
}

fn invalid_media_text(value: &str, max: usize) -> bool {
    value.len() > max || value.contains('\0')
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

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str) -> AudioDevice {
        AudioDevice {
            name: name.into(),
            description: name.into(),
        }
    }

    fn stream(id: &str) -> AudioStream {
        AudioStream {
            id: id.into(),
            application: "app".into(),
            app_id: "app".into(),
            name: "playback".into(),
            icon: "app".into(),
            volume: 0.5,
            muted: false,
            volume_writable: true,
        }
    }

    fn source(name: &str, monitor: bool) -> AudioSource {
        AudioSource {
            name: name.into(),
            description: name.into(),
            monitor,
        }
    }

    #[test]
    fn audio_lists_have_independent_revisions() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker.clone()).unwrap();

        tracker.borrow_mut().begin(1);
        lua.load("return services.audio.devices:get()")
            .eval::<Value>()
            .unwrap();
        tracker.borrow_mut().finish(1);
        tracker.borrow_mut().begin(2);
        lua.load("return services.audio.sources:get()")
            .eval::<Value>()
            .unwrap();
        tracker.borrow_mut().finish(2);
        tracker.borrow_mut().begin(3);
        lua.load("return services.audio.all_sources:get()")
            .eval::<Value>()
            .unwrap();
        tracker.borrow_mut().finish(3);
        tracker.borrow_mut().begin(4);
        lua.load("return services.audio.streams:get()")
            .eval::<Value>()
            .unwrap();
        tracker.borrow_mut().finish(4);

        services.set_sources(vec![device("microphone")]).unwrap();
        assert_eq!(tracker.borrow().dirty, BTreeSet::from([2]));
        tracker.borrow_mut().dirty.clear();
        services.set_devices(vec![device("speakers")]).unwrap();
        assert_eq!(tracker.borrow().dirty, BTreeSet::from([1]));
        tracker.borrow_mut().dirty.clear();
        services
            .set_all_sources(vec![source("speakers.monitor", true)])
            .unwrap();
        assert_eq!(tracker.borrow().dirty, BTreeSet::from([3]));
        tracker.borrow_mut().dirty.clear();
        services.set_streams(vec![stream("stream-1")]).unwrap();
        assert_eq!(tracker.borrow().dirty, BTreeSet::from([4]));
    }

    #[test]
    fn invalid_audio_snapshot_does_not_change_any_audio_state() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker).unwrap();
        services
            .set_audio(AudioSnapshot {
                available: true,
                volume: 0.4,
                device: "speakers".into(),
                devices: vec![device("speakers")],
                source_available: true,
                source_volume: 0.6,
                source_device: "microphone".into(),
                sources: vec![device("microphone")],
                all_sources: vec![
                    source("microphone", false),
                    source("speakers.monitor", true),
                ],
                streams: vec![stream("stream-1")],
                ..AudioSnapshot::default()
            })
            .unwrap();
        let before = services.snapshot();

        let invalid = AudioSnapshot {
            available: false,
            devices: vec![device("headphones")],
            sources: vec![device("webcam")],
            all_sources: vec![source("duplicate", false), source("duplicate", true)],
            streams: vec![stream("stream-2")],
            ..AudioSnapshot::default()
        };
        assert!(services.set_audio(invalid).is_err());
        assert_eq!(services.snapshot(), before);

        let invalid = AudioSnapshot {
            available: false,
            devices: vec![device("headphones")],
            sources: vec![device("webcam")],
            all_sources: vec![source("webcam", false)],
            streams: vec![AudioStream {
                volume: f64::NAN,
                ..stream("stream-2")
            }],
            ..AudioSnapshot::default()
        };
        assert!(services.set_audio(invalid).is_err());
        assert_eq!(services.snapshot(), before);
    }
}
