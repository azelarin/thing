use crate::OutputContext;
use crate::niri::{NiriAction, NiriData, NiriState, parse_id};
use crate::state::{Scalar, Signal, Tracker};
use mlua::{Error, Lua, Result, Table, UserData, UserDataMethods, Value};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    rc::Rc,
    sync::Arc,
    time::Instant,
};
use thing_core::ImageSource;

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
    SystemTray {
        action: SystemTrayAction,
        item: String,
    },
    SystemTrayMenu {
        action: SystemTrayMenuAction,
        id: String,
    },
    DismissNotification {
        key: u64,
    },
    InvokeNotification {
        key: u64,
        action: String,
    },
    ReplyNotification {
        key: u64,
        text: String,
    },
    ForgetNotification {
        key: u64,
    },
    ClearNotificationHistory,
    SetClipboardText(String),
    ClearClipboard,
    ManagedProcess {
        key: String,
        action: ManagedProcessAction,
    },
    Launch {
        argv: Vec<String>,
        cwd: Option<String>,
    },
    LaunchApplication(ApplicationLaunch),
}

/// a request to start an installed application from its desktop entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationLaunch {
    /// the desktop file id.
    pub id: String,
    /// the entry revision the configuration saw. a changed or removed entry is
    /// refused rather than launched in its place.
    pub revision: u64,
    /// a key from the entry's `actions`.
    pub action: Option<String>,
    /// absolute paths or uris to open.
    pub uris: Vec<String>,
    /// arguments placed before the command when the entry runs in a terminal.
    pub terminal: Vec<String>,
}

const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;
const MAX_QUEUED_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;

/// a request for one configuration-managed process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagedProcessAction {
    Start {
        argv: Vec<String>,
        cwd: Option<String>,
    },
    Stop,
    Refresh,
    Cancel,
}

/// the signal sent before a managed process is forcibly stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManagedProcessShutdownSignal {
    #[default]
    Terminate,
    Interrupt,
}

/// one process key declared while a configuration loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedProcessDefinition {
    pub key: String,
    pub shutdown_signal: ManagedProcessShutdownSignal,
    pub command: Option<ManagedProcessCommand>,
}

/// a finite command started and repeated by the managed process supervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedProcessCommand {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub interval_ms: Option<u64>,
}

/// lifecycle state for one managed process incarnation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManagedProcessStatus {
    #[default]
    Idle,
    Starting,
    Running,
    Stopping,
    Exited,
    Failed,
}

impl ManagedProcessStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }

    fn running(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

/// current output and exit state for one managed process key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedProcessState {
    pub key: String,
    pub status: ManagedProcessStatus,
    pub stdout: Arc<str>,
    pub stderr: Arc<str>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub error: Arc<str>,
}

impl ManagedProcessState {
    pub fn idle(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            status: ManagedProcessStatus::Idle,
            stdout: Arc::from(""),
            stderr: Arc::from(""),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: None,
            exit_signal: None,
            error: Arc::from(""),
        }
    }
}

/// one process-wide generation of managed process state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManagedProcessSnapshot {
    pub states: Vec<ManagedProcessState>,
}

/// a media playback control request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaAction {
    PlayPause,
    Next,
    Previous,
    Stop,
    /// move the position by a signed offset. `track` is the track id the
    /// configuration observed, or empty when the player reported none.
    Seek {
        offset_us: i64,
        track: String,
    },
    /// move to an absolute position in the track the configuration observed.
    SetPosition {
        position_us: i64,
        track: String,
    },
}

/// the largest media position, length, or seek offset, about 31 years.
pub const MAX_MEDIA_TIME_US: i64 = 1_000_000_000_000_000;
/// the largest absolute playback rate accepted from a media player.
pub const MAX_MEDIA_RATE: f64 = 1000.0;

/// an action requested from one status notifier item lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemTrayAction {
    Activate,
    SecondaryActivate,
    ContextMenu,
}

/// an action requested from one current DBusMenu branch lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemTrayMenuAction {
    Open,
    Close,
    Activate,
}

pub const MAX_LAUNCH_ARGUMENTS: usize = 256;
pub const MAX_LAUNCH_ARGUMENT_BYTES: usize = 4096;
pub const MAX_LAUNCH_BYTES: usize = 64 * 1024;
const MAX_MANAGED_PROCESS_KEYS: usize = 32;
const MAX_MANAGED_PROCESS_KEY_BYTES: usize = 128;
const MAX_MANAGED_PROCESS_STREAM_BYTES: usize = 64 * 1024;
const MAX_MANAGED_PROCESS_RETAINED_BYTES: usize = 4 * 1024 * 1024;
const MAX_MANAGED_PROCESS_ERROR_BYTES: usize = 4096;
const MIN_MANAGED_PROCESS_INTERVAL_MS: u64 = 1_000;
const MAX_MANAGED_PROCESS_INTERVAL_MS: u64 = 31_536_000_000;
const MAX_TRAY_PIXMAPS: usize = 32;
const MAX_TRAY_PIXMAP_SIDE: u32 = 4096;
const MAX_TRAY_PIXMAP_BYTES: usize = 256 * 1024;
const MAX_TRAY_ITEM_IMAGE_BYTES: usize = 512 * 1024;
const MAX_TRAY_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_TRAY_MENU_CHILDREN: usize = 64;
const MAX_TRAY_MENU_DEPTH: usize = 16;
const MAX_TRAY_MENU_NODES: usize = 256;
const MAX_TRAY_MENU_TOTAL_NODES: usize = 2048;
const MAX_TRAY_MENU_BYTES: usize = 512 * 1024;
const MAX_TRAY_MENU_TOTAL_BYTES: usize = 8 * 1024 * 1024;

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
#[derive(Clone, Debug, PartialEq)]
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
    pub can_seek: bool,
    /// the mpris track id, or empty when the player reports none.
    pub track_id: String,
    pub length_us: Option<i64>,
    /// the playback rate, where 1 is normal speed.
    pub rate: f64,
    pub position: Option<MediaPosition>,
}

/// a reported playback position and the monotonic time it was observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaPosition {
    pub position_us: i64,
    pub observed_at: Instant,
}

impl MediaPlayer {
    /// estimates the position at `now` from the last report, the playback
    /// state, and the rate. the result stays within the known track length.
    pub fn position_at(&self, now: Instant) -> Option<i64> {
        let sample = self.position?;
        let mut position = sample.position_us as f64;
        if self.playing {
            let elapsed = now.saturating_duration_since(sample.observed_at);
            position += elapsed.as_secs_f64() * 1_000_000.0 * self.rate;
        }
        let limit = self.length_us.unwrap_or(MAX_MEDIA_TIME_US) as f64;
        Some(position.clamp(0.0, limit) as i64)
    }
}

/// one status notifier item ownership lifetime and its current properties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemTrayItem {
    pub id: String,
    pub protocol_item_id: String,
    pub category: String,
    pub title: String,
    pub status: String,
    pub icon: Option<ImageSource>,
    pub icon_name: String,
    pub attention_icon_name: String,
    pub overlay_icon_name: String,
    pub tooltip_icon_name: String,
    pub tooltip_title: String,
    pub tooltip_text: String,
    pub item_is_menu: bool,
    pub has_menu: bool,
    pub menu: Option<Arc<SystemTrayMenu>>,
    pub has_icon_pixmap: bool,
    pub has_attention_icon_pixmap: bool,
    pub has_overlay_icon_pixmap: bool,
    pub has_tooltip_icon_pixmap: bool,
}

/// one immutable projection of a DBusMenu root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemTrayMenu {
    pub id: String,
    pub loaded: bool,
    pub loading: bool,
    pub error: String,
    pub children: Vec<SystemTrayMenuItem>,
}

/// one immutable DBusMenu item and its lazily loaded children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemTrayMenuItem {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub access_key: Option<String>,
    pub enabled: bool,
    pub visible: bool,
    pub icon_name: String,
    pub shortcut: Vec<Vec<String>>,
    pub toggle_type: String,
    pub toggle_state: i32,
    pub disposition: String,
    pub has_submenu: bool,
    pub loaded: bool,
    pub loading: bool,
    pub error: String,
    pub children: Vec<SystemTrayMenuItem>,
}

#[derive(Clone)]
pub(crate) struct ImageHandle(pub ImageSource);

impl UserData for ImageHandle {}

/// one UPower device identified by its object path.
#[derive(Clone, Debug, PartialEq)]
pub struct PowerDevice {
    pub id: String,
    pub kind: String,
    pub state: String,
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
    pub native_path: Option<String>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub icon_name: Option<String>,
    pub warning_level: String,
    pub battery_level: Option<String>,
}

/// an action advertised by a desktop notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationButton {
    pub key: String,
    pub label: String,
}

/// the urgency supplied by a notification client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationUrgency {
    Low,
    Normal,
    Critical,
}

impl NotificationUrgency {
    fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::Critical => "critical",
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }
}

/// why a notification in the history closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationCloseReason {
    Expired,
    Dismissed,
    ClosedByApplication,
}

impl NotificationCloseReason {
    fn name(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Dismissed => "dismissed",
            Self::ClosedByApplication => "closed_by_application",
        }
    }
}

/// one notification, identified by an opaque ownership-lifetime key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// pass this opaque key back to notification actions unchanged.
    pub key: String,
    /// the freedesktop protocol id, which may be reused after this record closes.
    pub id: u32,
    pub revision: u64,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<NotificationButton>,
    pub urgency: NotificationUrgency,
    pub raw_expire_timeout_ms: i32,
    pub effective_timeout_ms: Option<u32>,
    /// when the notification was first accepted, in server-relative milliseconds.
    pub created_at_ms: u64,
    /// when the notification was last replaced, in server-relative milliseconds.
    pub updated_at_ms: u64,
    /// a server-relative deadline, or none when the notification does not expire.
    pub expires_at_ms: Option<u64>,
    pub category: String,
    pub desktop_entry: String,
    pub image_path: String,
    pub resident: bool,
    pub transient: bool,
    pub action_icons: bool,
    /// whether the client accepts an inline reply.
    pub can_reply: bool,
    pub reply_placeholder: String,
    /// raw image data supplied with the notification.
    pub image: Option<ImageSource>,
    /// why and when, in server-relative milliseconds, a history record closed.
    pub closed: Option<(NotificationCloseReason, u64)>,
}

/// one confirmed generation of notification-server state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotificationSnapshot {
    pub owned: bool,
    pub error: String,
    pub active: Vec<Notification>,
    /// closed notifications kept for presentation, newest first.
    pub history: Vec<Notification>,
}

/// one confirmed generation of clipboard state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    pub available: bool,
    pub text: Option<String>,
    pub loading: bool,
    pub error: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthenticationState {
    Idle,
    Authenticating,
    Waiting,
    Rejected,
    #[default]
    Unavailable,
}

impl AuthenticationState {
    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Authenticating => "authenticating",
            Self::Waiting => "waiting",
            Self::Rejected => "rejected",
            Self::Unavailable => "unavailable",
        }
    }
}

/// read-only authentication state shared by every lock surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthenticationSnapshot {
    pub available: bool,
    pub state: AuthenticationState,
    pub prompt: String,
    pub message: String,
}

impl AuthenticationSnapshot {
    fn values(&self) -> [(&'static str, Scalar); 4] {
        [
            ("authentication.available", Scalar::Boolean(self.available)),
            (
                "authentication.state",
                Scalar::String(self.state.name().into()),
            ),
            ("authentication.prompt", Scalar::String(self.prompt.clone())),
            (
                "authentication.message",
                Scalar::String(self.message.clone()),
            ),
        ]
    }

    fn validate(&self) -> Result<()> {
        if self.available == (self.state == AuthenticationState::Unavailable)
            || (self.state != AuthenticationState::Waiting && !self.prompt.is_empty())
            || !valid_authentication_text(&self.prompt)
            || !valid_authentication_text(&self.message)
        {
            return Err(Error::runtime("inconsistent authentication snapshot"));
        }
        for (name, value) in self.values() {
            validate_service(name, &value)?;
        }
        Ok(())
    }
}

impl ClipboardSnapshot {
    fn values(&self) -> [(&'static str, Scalar); 4] {
        [
            ("clipboard.available", Scalar::Boolean(self.available)),
            (
                "clipboard.text",
                self.text.clone().map_or(Scalar::Nil, Scalar::String),
            ),
            ("clipboard.loading", Scalar::Boolean(self.loading)),
            ("clipboard.error", Scalar::String(self.error.clone())),
        ]
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in self.values() {
            validate_service(name, &value)?;
        }
        if !self.available && (self.text.is_some() || self.loading) {
            return Err(Error::runtime("inconsistent clipboard snapshot"));
        }
        if self.loading && (self.text.is_some() || !self.error.is_empty()) {
            return Err(Error::runtime("inconsistent clipboard snapshot"));
        }
        Ok(())
    }
}

impl NotificationSnapshot {
    fn values(&self) -> [(&'static str, Scalar); 2] {
        [
            ("notifications.owned", Scalar::Boolean(self.owned)),
            ("notifications.error", Scalar::String(self.error.clone())),
        ]
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in self.values() {
            validate_service(name, &value)?;
        }
        if !self.owned && (!self.active.is_empty() || !self.history.is_empty()) {
            return Err(Error::runtime(
                "notification records require notification-server ownership",
            ));
        }
        validate_notifications(&self.active, false)?;
        validate_notifications(&self.history, true)?;
        let mut keys = BTreeSet::new();
        if !self
            .active
            .iter()
            .chain(&self.history)
            .all(|notification| keys.insert(notification.key.as_str()))
        {
            return Err(Error::runtime("notification keys must be unique"));
        }
        Ok(())
    }
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

/// one launchable action from a desktop entry, such as opening a new window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopAction {
    pub key: String,
    pub name: String,
    pub icon: String,
    pub command: Vec<String>,
}

/// which files or urls a launch of an application accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DesktopArguments {
    /// the entry takes no files or urls.
    #[default]
    None,
    /// one local file per process, from `%f`.
    File,
    /// several local files in one process, from `%F`.
    Files,
    /// one url per process, from `%u`.
    Url,
    /// several urls in one process, from `%U` or d-bus activation.
    Urls,
}

impl DesktopArguments {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::File => "file",
            Self::Files => "files",
            Self::Url => "url",
            Self::Urls => "urls",
        }
    }

    fn local_only(self) -> bool {
        matches!(self, Self::File | Self::Files)
    }
}

/// one installed application described by a desktop entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopApplication {
    /// the desktop file id, such as `org.gnome.Nautilus.desktop`.
    pub id: String,
    /// a nonzero number that changes whenever the entry changes and stays the
    /// same across rescans that leave it unchanged.
    pub revision: u64,
    pub name: String,
    pub generic_name: String,
    pub comment: String,
    pub icon: String,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    /// arguments for a launch without files. empty when the entry has no
    /// `Exec`, such as one that only names a window's app id or that starts
    /// through d-bus activation.
    pub command: Vec<String>,
    pub working_directory: Option<String>,
    pub terminal: bool,
    pub no_display: bool,
    pub startup_wm_class: String,
    pub dbus_activatable: bool,
    /// the files or urls that `services.applications.launch` accepts.
    pub accepts: DesktopArguments,
    pub actions: Vec<DesktopAction>,
}

/// one generation of the installed application list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplicationsSnapshot {
    pub available: bool,
    pub error: String,
    pub applications: Vec<DesktopApplication>,
}

impl ApplicationsSnapshot {
    fn values(&self) -> [(&'static str, Scalar); 2] {
        [
            ("applications.available", Scalar::Boolean(self.available)),
            ("applications.error", Scalar::String(self.error.clone())),
        ]
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in self.values() {
            validate_service(name, &value)?;
        }
        validate_applications(&self.applications)
    }
}

/// one confirmed generation of media player state.
#[derive(Clone, Debug, PartialEq)]
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
    pub can_seek: bool,
    pub length_us: Option<i64>,
    pub rate: f64,
    pub error: String,
    pub players: Vec<MediaPlayer>,
}

impl Default for MediaSnapshot {
    fn default() -> Self {
        Self {
            available: false,
            selected_id: String::new(),
            playing: false,
            title: String::new(),
            artist: String::new(),
            art_url: String::new(),
            identity: String::new(),
            desktop_entry: String::new(),
            can_play_pause: false,
            can_next: false,
            can_previous: false,
            can_stop: false,
            can_seek: false,
            length_us: None,
            rate: 1.0,
            error: String::new(),
            players: Vec::new(),
        }
    }
}

impl MediaSnapshot {
    fn values(&self) -> [(&'static str, Scalar); 16] {
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
            ("media.can_seek", Scalar::Boolean(self.can_seek)),
            ("media.length_ms", media_milliseconds(self.length_us)),
            ("media.rate", Scalar::Number(self.rate)),
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
                    && !self.can_seek
                    && self.length_us.is_none()
                    && self.rate == 1.0
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
                    && player.can_seek == self.can_seek
                    && player.length_us == self.length_us
                    && player.rate == self.rate
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

/// one confirmed generation of status notifier watcher and item state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemTraySnapshot {
    pub available: bool,
    pub watcher_owned: bool,
    pub error: String,
    pub items: Vec<SystemTrayItem>,
}

impl SystemTraySnapshot {
    fn values(&self) -> [(&'static str, Scalar); 3] {
        [
            ("system_tray.available", Scalar::Boolean(self.available)),
            (
                "system_tray.watcher_owned",
                Scalar::Boolean(self.watcher_owned),
            ),
            ("system_tray.error", Scalar::String(self.error.clone())),
        ]
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in self.values() {
            validate_service(name, &value)?;
        }
        validate_system_tray_items(&self.items)?;
        if !self.available && (!self.items.is_empty() || self.watcher_owned) {
            return Err(Error::runtime("inconsistent system tray snapshot"));
        }
        Ok(())
    }
}

/// one confirmed generation of UPower state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PowerSnapshot {
    pub available: bool,
    pub on_battery: bool,
    pub display: Option<PowerDevice>,
    pub devices: Vec<PowerDevice>,
    pub error: String,
}

impl PowerSnapshot {
    fn values(&self) -> [(&'static str, Scalar); 3] {
        [
            ("power.available", Scalar::Boolean(self.available)),
            ("power.on_battery", Scalar::Boolean(self.on_battery)),
            ("power.error", Scalar::String(self.error.clone())),
        ]
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in self.values() {
            validate_service(name, &value)?;
        }
        validate_power_devices(&self.devices)?;
        if let Some(display) = &self.display {
            validate_power_device(display)?;
        }
        if self.available {
            if !self.error.is_empty() {
                return Err(Error::runtime("inconsistent power snapshot"));
            }
        } else if self.on_battery || self.display.is_some() || !self.devices.is_empty() {
            return Err(Error::runtime("inconsistent power snapshot"));
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
    pub(crate) applications: Vec<DesktopApplication>,
    pub(crate) power_display: Option<PowerDevice>,
    pub(crate) power_devices: Vec<PowerDevice>,
    pub(crate) notifications: Vec<Notification>,
    pub(crate) notification_history: Vec<Notification>,
    pub(crate) system_tray_items: Vec<SystemTrayItem>,
    pub(crate) managed_processes: Vec<ManagedProcessState>,
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

    pub fn set_applications(&mut self, applications: ApplicationsSnapshot) -> Result<()> {
        applications.validate()?;
        for (name, value) in applications.values() {
            self.values.insert(name.into(), value);
        }
        self.applications = applications.applications;
        Ok(())
    }

    pub fn set_power(&mut self, power: PowerSnapshot) -> Result<()> {
        power.validate()?;
        for (name, value) in power.values() {
            self.values.insert(name.into(), value);
        }
        self.power_display = power.display;
        self.power_devices = power.devices;
        Ok(())
    }

    pub fn set_notifications(&mut self, notifications: NotificationSnapshot) -> Result<()> {
        notifications.validate()?;
        for (name, value) in notifications.values() {
            self.values.insert(name.into(), value);
        }
        self.notifications = notifications.active;
        self.notification_history = notifications.history;
        Ok(())
    }

    pub fn set_system_tray(&mut self, system_tray: SystemTraySnapshot) -> Result<()> {
        system_tray.validate()?;
        for (name, value) in system_tray.values() {
            self.values.insert(name.into(), value);
        }
        self.system_tray_items = system_tray.items;
        Ok(())
    }

    pub fn set_clipboard(&mut self, clipboard: ClipboardSnapshot) -> Result<()> {
        clipboard.validate()?;
        for (name, value) in clipboard.values() {
            self.values.insert(name.into(), value);
        }
        Ok(())
    }

    pub fn set_managed_processes(&mut self, snapshot: ManagedProcessSnapshot) -> Result<()> {
        validate_managed_process_states(&snapshot.states)?;
        self.managed_processes = snapshot.states;
        Ok(())
    }

    /// returns the current process-wide managed process state.
    pub fn managed_processes(&self) -> ManagedProcessSnapshot {
        ManagedProcessSnapshot {
            states: self.managed_processes.clone(),
        }
    }

    pub fn set_authentication(&mut self, authentication: AuthenticationSnapshot) -> Result<()> {
        authentication.validate()?;
        for (name, value) in authentication.values() {
            self.values.insert(name.into(), value);
        }
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

struct ApplicationList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    applications: Rc<RefCell<Vec<DesktopApplication>>>,
}

fn readonly_strings(lua: &Lua, values: &[String]) -> Result<Table> {
    let list = lua.create_table()?;
    for (index, value) in values.iter().enumerate() {
        list.raw_set(index + 1, value.as_str())?;
    }
    list.set_readonly(true);
    Ok(list)
}

impl UserData for ApplicationList {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let list = lua.create_table()?;
            for (index, application) in this.applications.borrow().iter().enumerate() {
                let actions = lua.create_table()?;
                for (action_index, action) in application.actions.iter().enumerate() {
                    let item = lua.create_table()?;
                    item.set("key", action.key.as_str())?;
                    item.set("name", action.name.as_str())?;
                    item.set("icon", action.icon.as_str())?;
                    item.set("command", readonly_strings(lua, &action.command)?)?;
                    item.set_readonly(true);
                    actions.raw_set(action_index + 1, item)?;
                }
                actions.set_readonly(true);

                let item = lua.create_table()?;
                item.set("id", application.id.as_str())?;
                item.set("name", application.name.as_str())?;
                item.set("generic_name", application.generic_name.as_str())?;
                item.set("comment", application.comment.as_str())?;
                item.set("icon", application.icon.as_str())?;
                item.set("keywords", readonly_strings(lua, &application.keywords)?)?;
                item.set(
                    "categories",
                    readonly_strings(lua, &application.categories)?,
                )?;
                item.set("command", readonly_strings(lua, &application.command)?)?;
                item.set(
                    "working_directory",
                    application.working_directory.as_deref(),
                )?;
                item.set("terminal", application.terminal)?;
                item.set("no_display", application.no_display)?;
                item.set("startup_wm_class", application.startup_wm_class.as_str())?;
                item.set("dbus_activatable", application.dbus_activatable)?;
                item.set("accepts", application.accepts.name())?;
                item.set("actions", actions)?;
                item.set_readonly(true);
                list.raw_set(index + 1, item)?;
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
}

struct MediaPlayerList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    players: Rc<RefCell<Vec<MediaPlayer>>>,
}

struct SystemTrayItemList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    items: Rc<RefCell<Vec<SystemTrayItem>>>,
}

struct NotificationList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    notifications: Rc<RefCell<Vec<Notification>>>,
}

impl UserData for NotificationList {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let list = lua.create_table()?;
            for (index, notification) in this.notifications.borrow().iter().enumerate() {
                let actions = lua.create_table()?;
                for (action_index, action) in notification.actions.iter().enumerate() {
                    let item = lua.create_table()?;
                    item.set("key", action.key.as_str())?;
                    item.set("label", action.label.as_str())?;
                    item.set_readonly(true);
                    actions.raw_set(action_index + 1, item)?;
                }
                actions.set_readonly(true);

                let item = lua.create_table()?;
                item.set("key", notification.key.as_str())?;
                item.set("id", notification.id)?;
                item.set("revision", notification.revision as f64)?;
                item.set("app_name", notification.app_name.as_str())?;
                item.set("app_icon", notification.app_icon.as_str())?;
                item.set("summary", notification.summary.as_str())?;
                item.set("body", notification.body.as_str())?;
                item.set("actions", actions)?;
                item.set("urgency", notification.urgency.name())?;
                item.set("urgency_code", notification.urgency.code())?;
                item.set("raw_expire_timeout_ms", notification.raw_expire_timeout_ms)?;
                item.set("effective_timeout_ms", notification.effective_timeout_ms)?;
                item.set("created_at_ms", notification.created_at_ms as f64)?;
                item.set("updated_at_ms", notification.updated_at_ms as f64)?;
                item.set(
                    "expires_at_ms",
                    notification.expires_at_ms.map(|v| v as f64),
                )?;
                item.set("category", notification.category.as_str())?;
                item.set("desktop_entry", notification.desktop_entry.as_str())?;
                item.set("image_path", notification.image_path.as_str())?;
                item.set("resident", notification.resident)?;
                item.set("transient", notification.transient)?;
                item.set("action_icons", notification.action_icons)?;
                item.set("can_reply", notification.can_reply)?;
                item.set("reply_placeholder", notification.reply_placeholder.as_str())?;
                if let Some(image) = &notification.image {
                    item.set("image", lua.create_userdata(ImageHandle(image.clone()))?)?;
                }
                if let Some((reason, at_ms)) = notification.closed {
                    item.set("closed_reason", reason.name())?;
                    item.set("closed_at_ms", at_ms as f64)?;
                }
                item.set_readonly(true);
                list.raw_set(index + 1, item)?;
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
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
                item.set("can_seek", player.can_seek)?;
                item.set("length_ms", player.length_us.map(|us| us as f64 / 1000.0))?;
                item.set("rate", player.rate)?;
                item.set_readonly(true);
                list.raw_set(index + 1, item)?;
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
}

impl UserData for SystemTrayItemList {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let list = lua.create_table()?;
            for (index, tray_item) in this.items.borrow().iter().enumerate() {
                let item = lua.create_table()?;
                item.set("id", tray_item.id.as_str())?;
                item.set("protocol_item_id", tray_item.protocol_item_id.as_str())?;
                item.set("category", tray_item.category.as_str())?;
                item.set("title", tray_item.title.as_str())?;
                item.set("status", tray_item.status.as_str())?;
                if let Some(icon) = &tray_item.icon {
                    item.set("icon", lua.create_userdata(ImageHandle(icon.clone()))?)?;
                }
                item.set("icon_name", tray_item.icon_name.as_str())?;
                item.set(
                    "attention_icon_name",
                    tray_item.attention_icon_name.as_str(),
                )?;
                item.set("overlay_icon_name", tray_item.overlay_icon_name.as_str())?;
                item.set("tooltip_icon_name", tray_item.tooltip_icon_name.as_str())?;
                item.set("tooltip_title", tray_item.tooltip_title.as_str())?;
                item.set("tooltip_text", tray_item.tooltip_text.as_str())?;
                item.set("item_is_menu", tray_item.item_is_menu)?;
                item.set("has_menu", tray_item.has_menu)?;
                if let Some(menu) = &tray_item.menu {
                    item.set("menu", system_tray_menu_table(lua, menu)?)?;
                }
                item.set("has_icon_pixmap", tray_item.has_icon_pixmap)?;
                item.set(
                    "has_attention_icon_pixmap",
                    tray_item.has_attention_icon_pixmap,
                )?;
                item.set("has_overlay_icon_pixmap", tray_item.has_overlay_icon_pixmap)?;
                item.set("has_tooltip_icon_pixmap", tray_item.has_tooltip_icon_pixmap)?;
                item.set_readonly(true);
                list.raw_set(index + 1, item)?;
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
}

fn system_tray_menu_table(lua: &Lua, menu: &SystemTrayMenu) -> Result<Table> {
    let value = lua.create_table()?;
    value.set("id", menu.id.as_str())?;
    value.set("loaded", menu.loaded)?;
    value.set("loading", menu.loading)?;
    value.set("error", menu.error.as_str())?;
    value.set("children", system_tray_menu_children(lua, &menu.children)?)?;
    value.set_readonly(true);
    Ok(value)
}

fn system_tray_menu_children(lua: &Lua, children: &[SystemTrayMenuItem]) -> Result<Table> {
    let list = lua.create_table()?;
    for (index, child) in children.iter().enumerate() {
        let item = lua.create_table()?;
        item.set("id", child.id.as_str())?;
        item.set("kind", child.kind.as_str())?;
        item.set("label", child.label.as_str())?;
        item.set("access_key", child.access_key.as_deref())?;
        item.set("enabled", child.enabled)?;
        item.set("visible", child.visible)?;
        item.set("icon_name", child.icon_name.as_str())?;
        item.set("shortcut", system_tray_shortcuts(lua, &child.shortcut)?)?;
        item.set("toggle_type", child.toggle_type.as_str())?;
        item.set("toggle_state", child.toggle_state)?;
        item.set("disposition", child.disposition.as_str())?;
        item.set("has_submenu", child.has_submenu)?;
        item.set("loaded", child.loaded)?;
        item.set("loading", child.loading)?;
        item.set("error", child.error.as_str())?;
        item.set("children", system_tray_menu_children(lua, &child.children)?)?;
        item.set_readonly(true);
        list.raw_set(index + 1, item)?;
    }
    list.set_readonly(true);
    Ok(list)
}

fn system_tray_shortcuts(lua: &Lua, shortcuts: &[Vec<String>]) -> Result<Table> {
    let list = lua.create_table()?;
    for (index, shortcut) in shortcuts.iter().enumerate() {
        let chord = lua.create_table()?;
        for (part_index, part) in shortcut.iter().enumerate() {
            chord.raw_set(part_index + 1, part.as_str())?;
        }
        chord.set_readonly(true);
        list.raw_set(index + 1, chord)?;
    }
    list.set_readonly(true);
    Ok(list)
}

struct PowerDeviceList {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    devices: Rc<RefCell<Vec<PowerDevice>>>,
}

impl UserData for PowerDeviceList {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let list = lua.create_table()?;
            for (index, device) in this.devices.borrow().iter().enumerate() {
                list.raw_set(index + 1, power_device_table(lua, device)?)?;
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
}

struct PowerDisplay {
    tracker: Rc<RefCell<Tracker>>,
    revision: usize,
    display: Rc<RefCell<Option<PowerDevice>>>,
}

impl UserData for PowerDisplay {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            this.display
                .borrow()
                .as_ref()
                .map(|device| power_device_table(lua, device))
                .transpose()
        });
    }
}

fn power_device_table(lua: &Lua, device: &PowerDevice) -> Result<Table> {
    let item = lua.create_table()?;
    item.set("id", device.id.as_str())?;
    item.set("kind", device.kind.as_str())?;
    item.set("state", device.state.as_str())?;
    item.set("power_supply", device.power_supply)?;
    item.set("present", device.present)?;
    item.set("online", device.online)?;
    item.set("percentage", device.percentage)?;
    item.set("time_to_empty", device.time_to_empty)?;
    item.set("time_to_full", device.time_to_full)?;
    item.set("energy", device.energy)?;
    item.set("energy_empty", device.energy_empty)?;
    item.set("energy_full", device.energy_full)?;
    item.set("energy_full_design", device.energy_full_design)?;
    item.set("energy_rate", device.energy_rate)?;
    item.set("capacity", device.capacity)?;
    item.set("native_path", device.native_path.as_deref())?;
    item.set("vendor", device.vendor.as_deref())?;
    item.set("model", device.model.as_deref())?;
    item.set("serial", device.serial.as_deref())?;
    item.set("icon_name", device.icon_name.as_deref())?;
    item.set("warning_level", device.warning_level.as_str())?;
    item.set("battery_level", device.battery_level.as_deref())?;
    item.set_readonly(true);
    Ok(item)
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

#[derive(Clone)]
struct ManagedProcessSignals {
    definition: ManagedProcessDefinition,
    status: usize,
    running: usize,
    stdout: usize,
    stderr: usize,
    stdout_truncated: usize,
    stderr_truncated: usize,
    exit_code: usize,
    exit_signal: usize,
    error: usize,
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
    applications: Rc<RefCell<Vec<DesktopApplication>>>,
    applications_revision: usize,
    power_display: Rc<RefCell<Option<PowerDevice>>>,
    power_display_revision: usize,
    power_devices: Rc<RefCell<Vec<PowerDevice>>>,
    power_devices_revision: usize,
    notifications: Rc<RefCell<Vec<Notification>>>,
    notifications_revision: usize,
    notification_history: Rc<RefCell<Vec<Notification>>>,
    notification_history_revision: usize,
    system_tray_items: Rc<RefCell<Vec<SystemTrayItem>>>,
    system_tray_items_revision: usize,
    managed_processes: Rc<RefCell<BTreeMap<String, ManagedProcessState>>>,
    managed_process_signals: Rc<RefCell<BTreeMap<String, ManagedProcessSignals>>>,
    managed_process_declarations_open: Rc<Cell<bool>>,
    application_launches_allowed: Rc<Cell<bool>>,
    niri: NiriData,
}

impl Services {
    pub fn install(lua: &Lua, tracker: Rc<RefCell<Tracker>>) -> Result<Self> {
        let devices_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let sources_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let all_sources_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let streams_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let media_players_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let applications_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let power_display_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let power_devices_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let notifications_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let notification_history_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let system_tray_items_revision = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
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
            applications: Rc::default(),
            applications_revision,
            power_display: Rc::default(),
            power_display_revision,
            power_devices: Rc::default(),
            power_devices_revision,
            notifications: Rc::default(),
            notifications_revision,
            notification_history: Rc::default(),
            notification_history_revision,
            system_tray_items: Rc::default(),
            system_tray_items_revision,
            managed_processes: Rc::default(),
            managed_process_signals: Rc::default(),
            managed_process_declarations_open: Rc::new(Cell::new(true)),
            application_launches_allowed: Rc::new(Cell::new(true)),
            niri: niri_data,
        };
        let services = lua.create_table()?;
        let applications = lua.create_table()?;
        let audio = lua.create_table()?;
        let authentication = lua.create_table()?;
        let clipboard = lua.create_table()?;
        let clock = lua.create_table()?;
        let idle = lua.create_table()?;
        let media = lua.create_table()?;
        let notifications = lua.create_table()?;
        let output = lua.create_table()?;
        let power = lua.create_table()?;
        let system_tray = lua.create_table()?;
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
        applications.set(
            "entries",
            ApplicationList {
                tracker: result.tracker.clone(),
                revision: applications_revision,
                applications: result.applications.clone(),
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
        power.set(
            "display",
            PowerDisplay {
                tracker: result.tracker.clone(),
                revision: power_display_revision,
                display: result.power_display.clone(),
            },
        )?;
        power.set(
            "devices",
            PowerDeviceList {
                tracker: result.tracker.clone(),
                revision: power_devices_revision,
                devices: result.power_devices.clone(),
            },
        )?;
        notifications.set(
            "active",
            NotificationList {
                tracker: result.tracker.clone(),
                revision: notifications_revision,
                notifications: result.notifications.clone(),
            },
        )?;
        notifications.set(
            "history",
            NotificationList {
                tracker: result.tracker.clone(),
                revision: notification_history_revision,
                notifications: result.notification_history.clone(),
            },
        )?;
        system_tray.set(
            "items",
            SystemTrayItemList {
                tracker: result.tracker.clone(),
                revision: system_tray_items_revision,
                items: result.system_tray_items.clone(),
            },
        )?;
        for (name, value) in [
            ("applications.available", Scalar::Boolean(false)),
            ("applications.error", Scalar::String(String::new())),
            ("audio.available", Scalar::Boolean(false)),
            ("audio.volume", Scalar::Number(0.0)),
            ("audio.muted", Scalar::Boolean(false)),
            ("audio.device", Scalar::String(String::new())),
            ("audio.source_available", Scalar::Boolean(false)),
            ("audio.source_volume", Scalar::Number(0.0)),
            ("audio.source_muted", Scalar::Boolean(false)),
            ("audio.source_device", Scalar::String(String::new())),
            ("audio.error", Scalar::String(String::new())),
            ("authentication.available", Scalar::Boolean(false)),
            ("authentication.state", Scalar::String("unavailable".into())),
            ("authentication.prompt", Scalar::String(String::new())),
            ("authentication.message", Scalar::String(String::new())),
            ("clipboard.available", Scalar::Boolean(false)),
            ("clipboard.text", Scalar::Nil),
            ("clipboard.loading", Scalar::Boolean(false)),
            ("clipboard.error", Scalar::String(String::new())),
            ("clock.time", Scalar::String("--:--".into())),
            ("clock.date", Scalar::String(String::new())),
            ("idle.inhibit_available", Scalar::Boolean(false)),
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
            ("media.can_seek", Scalar::Boolean(false)),
            ("media.length_ms", Scalar::Nil),
            ("media.rate", Scalar::Number(1.0)),
            ("media.error", Scalar::String(String::new())),
            ("power.available", Scalar::Boolean(false)),
            ("power.on_battery", Scalar::Boolean(false)),
            ("power.error", Scalar::String(String::new())),
            ("notifications.owned", Scalar::Boolean(false)),
            ("notifications.error", Scalar::String(String::new())),
            ("system_tray.available", Scalar::Boolean(false)),
            ("system_tray.watcher_owned", Scalar::Boolean(false)),
            ("system_tray.error", Scalar::String(String::new())),
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
                "applications" => applications.set(property, signal)?,
                "audio" => audio.set(property, signal)?,
                "authentication" => authentication.set(property, signal)?,
                "clipboard" => clipboard.set(property, signal)?,
                "clock" => clock.set(property, signal)?,
                "idle" => idle.set(property, signal)?,
                "media" => media.set(property, signal)?,
                "notifications" => notifications.set(property, signal)?,
                "niri" => niri.set(property, signal)?,
                "output" => output.set(property, signal)?,
                "power" => power.set(property, signal)?,
                "system_tray" => system_tray.set(property, signal)?,
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
        authentication.set_readonly(true);
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        clipboard.set(
            "set_text",
            lua.create_function(move |_, text: String| {
                validate_clipboard_text(&text)?;
                enqueue_clipboard(&actions, &allowed, Action::SetClipboardText(text))
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        clipboard.set(
            "clear",
            lua.create_function(move |_, ()| enqueue(&actions, &allowed, Action::ClearClipboard))?,
        )?;
        clipboard.set_readonly(true);
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
                    let action = action.clone();
                    enqueue(&actions, &allowed, Action::Media { action, player })
                })?,
            )?;
        }
        let selected = result.signals["media.selected_id"];
        for (name, signed) in [("seek", true), ("set_position", false)] {
            let actions = result.actions.clone();
            let allowed = result.allowed.clone();
            let tracker = result.tracker.clone();
            let players = result.media_players.clone();
            media.set(
                name,
                lua.create_function(move |_, (milliseconds, player): (f64, Value)| {
                    let amount = media_microseconds(
                        milliseconds,
                        signed,
                        if signed {
                            "seek offset"
                        } else {
                            "media position"
                        },
                    )?;
                    let player = media_player_id(player)?;
                    // bind the request to the track this configuration observed, so
                    // the service can reject it after the player changes tracks.
                    let id = match &player {
                        Some(id) => id.clone(),
                        None => scalar_string(tracker.borrow_mut().read(selected)),
                    };
                    let track = players
                        .borrow()
                        .iter()
                        .find(|candidate| candidate.id == id)
                        .map(|candidate| candidate.track_id.clone())
                        .unwrap_or_default();
                    let action = if signed {
                        MediaAction::Seek {
                            offset_us: amount,
                            track,
                        }
                    } else {
                        MediaAction::SetPosition {
                            position_us: amount,
                            track,
                        }
                    };
                    enqueue(
                        &actions,
                        &allowed,
                        Action::Media {
                            action,
                            player: Some(id),
                        },
                    )
                })?,
            )?;
        }
        let tracker = result.tracker.clone();
        let players = result.media_players.clone();
        let revision = result.media_players_revision;
        media.set(
            "position",
            lua.create_function(move |_, player: Value| {
                let player = media_player_id(player)?;
                let mut tracker = tracker.borrow_mut();
                tracker.read(revision);
                let id = match player {
                    Some(id) => id,
                    None => scalar_string(tracker.read(selected)),
                };
                drop(tracker);
                Ok(players
                    .borrow()
                    .iter()
                    .find(|candidate| candidate.id == id)
                    .and_then(|candidate| candidate.position_at(Instant::now()))
                    .map(|us| us as f64 / 1000.0))
            })?,
        )?;
        media.set_readonly(true);
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        let launches_allowed = result.application_launches_allowed.clone();
        let list = result.applications.clone();
        applications.set(
            "launch",
            lua.create_function(move |_, (id, options): (Value, Value)| {
                if !allowed.get() {
                    return Err(Error::runtime(
                        "service actions are allowed only in event handlers",
                    ));
                }
                if !launches_allowed.get() {
                    return Err(Error::runtime(
                        "applications cannot be launched from lock configurations",
                    ));
                }
                let launch = application_launch(&list.borrow(), id, options)?;
                enqueue(&actions, &allowed, Action::LaunchApplication(launch))
            })?,
        )?;
        applications.set_readonly(true);
        for (name, action) in [
            ("activate", SystemTrayAction::Activate),
            ("secondary_activate", SystemTrayAction::SecondaryActivate),
            ("context_menu", SystemTrayAction::ContextMenu),
        ] {
            let actions = result.actions.clone();
            let allowed = result.allowed.clone();
            system_tray.set(
                name,
                lua.create_function(move |_, item: Value| {
                    let item = system_tray_item_id(item)?;
                    enqueue(&actions, &allowed, Action::SystemTray { action, item })
                })?,
            )?;
        }
        for (name, action) in [
            ("open_menu", SystemTrayMenuAction::Open),
            ("close_menu", SystemTrayMenuAction::Close),
            ("activate_menu_item", SystemTrayMenuAction::Activate),
        ] {
            let actions = result.actions.clone();
            let allowed = result.allowed.clone();
            system_tray.set(
                name,
                lua.create_function(move |_, id: Value| {
                    let id = system_tray_menu_id(id)?;
                    enqueue(&actions, &allowed, Action::SystemTrayMenu { action, id })
                })?,
            )?;
        }
        system_tray.set_readonly(true);
        power.set_readonly(true);
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        notifications.set(
            "dismiss",
            lua.create_function(move |_, key: Value| {
                let key = notification_key(key)?;
                enqueue(&actions, &allowed, Action::DismissNotification { key })
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        notifications.set(
            "invoke",
            lua.create_function(move |_, (key, action): (Value, String)| {
                let key = notification_key(key)?;
                validate_notification_action_key(&action)?;
                enqueue(
                    &actions,
                    &allowed,
                    Action::InvokeNotification { key, action },
                )
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        notifications.set(
            "reply",
            lua.create_function(move |_, (key, text): (Value, String)| {
                let key = notification_key(key)?;
                if text.is_empty() || invalid_notification_text(&text) {
                    return Err(Error::runtime(
                        "a notification reply must be nonempty, contain no NUL, and not exceed 64 kib",
                    ));
                }
                enqueue(&actions, &allowed, Action::ReplyNotification { key, text })
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        notifications.set(
            "forget",
            lua.create_function(move |_, key: Value| {
                let key = notification_key(key)?;
                enqueue(&actions, &allowed, Action::ForgetNotification { key })
            })?,
        )?;
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        notifications.set(
            "clear_history",
            lua.create_function(move |_, ()| {
                enqueue(&actions, &allowed, Action::ClearNotificationHistory)
            })?,
        )?;
        notifications.set_readonly(true);
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
        idle.set_readonly(true);
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
        let tracker = result.tracker.clone();
        let actions = result.actions.clone();
        let allowed = result.allowed.clone();
        let states = result.managed_processes.clone();
        let processes = result.managed_process_signals.clone();
        let declarations_open = result.managed_process_declarations_open.clone();
        services.set(
            "process",
            lua.create_function(move |lua, value: Value| {
                if !declarations_open.get() {
                    return Err(Error::runtime(
                        "managed processes must be declared while the configuration loads",
                    ));
                }
                let definition = managed_process_definition(value)?;
                let existing = processes.borrow().get(&definition.key).cloned();
                let signals = if let Some(signals) = existing {
                    if signals.definition != definition {
                        return Err(Error::runtime(format!(
                            "managed process {:?} has conflicting declarations",
                            definition.key
                        )));
                    }
                    signals
                } else {
                    if processes.borrow().len() >= MAX_MANAGED_PROCESS_KEYS {
                        return Err(Error::runtime(format!(
                            "a configuration may declare at most {MAX_MANAGED_PROCESS_KEYS} managed processes"
                        )));
                    }
                    let state = states
                        .borrow()
                        .get(&definition.key)
                        .cloned()
                        .unwrap_or_else(|| ManagedProcessState::idle(definition.key.clone()));
                    let signals = create_managed_process_signals(
                        &tracker,
                        definition.clone(),
                        &state,
                    )?;
                    processes
                        .borrow_mut()
                        .insert(definition.key.clone(), signals.clone());
                    signals
                };
                managed_process_handle(lua, signals, &tracker, &actions, &allowed)
            })?,
        )?;
        services.set("applications", applications)?;
        services.set("audio", audio)?;
        services.set("authentication", authentication)?;
        services.set("clipboard", clipboard)?;
        services.set("clock", clock)?;
        services.set("idle", idle)?;
        services.set("media", media)?;
        services.set("notifications", notifications)?;
        services.set("niri", niri)?;
        services.set("output", output)?;
        services.set("power", power)?;
        services.set("system_tray", system_tray)?;
        services.set_readonly(true);
        lua.globals().set("services", services)?;
        Ok(result)
    }
    pub fn allow_actions(&self, allowed: bool) {
        self.allowed.set(allowed);
    }

    /// lock configurations may not start applications.
    pub fn forbid_application_launches(&self) {
        self.application_launches_allowed.set(false);
    }

    pub fn freeze_managed_process_declarations(&self) {
        self.managed_process_declarations_open.set(false);
    }

    pub fn managed_process_definitions(&self) -> Vec<ManagedProcessDefinition> {
        self.managed_process_signals
            .borrow()
            .values()
            .map(|signals| signals.definition.clone())
            .collect()
    }

    pub fn set_managed_processes(&self, snapshot: ManagedProcessSnapshot) -> Result<()> {
        validate_managed_process_states(&snapshot.states)?;
        let states = snapshot
            .states
            .into_iter()
            .map(|state| (state.key.clone(), state))
            .collect::<BTreeMap<_, _>>();
        let mut updates = Vec::new();
        for signals in self.managed_process_signals.borrow().values() {
            let state = states
                .get(&signals.definition.key)
                .cloned()
                .unwrap_or_else(|| ManagedProcessState::idle(signals.definition.key.clone()));
            updates.extend(managed_process_updates(signals, &state));
        }
        self.tracker.borrow_mut().write_batch(updates)?;
        *self.managed_processes.borrow_mut() = states;
        Ok(())
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

    pub fn set_applications(&self, applications: ApplicationsSnapshot) -> Result<()> {
        applications.validate()?;
        let changed = *self.applications.borrow() != applications.applications;
        let mut list = self.applications.borrow_mut();
        let mut tracker = self.tracker.borrow_mut();
        let mut updates = applications
            .values()
            .into_iter()
            .map(|(name, value)| (self.signals[name], value))
            .collect::<Vec<_>>();
        if changed {
            let revision = tracker.read(self.applications_revision);
            updates.push((
                self.applications_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        tracker.write_batch(updates)?;
        if changed {
            *list = applications.applications;
        }
        Ok(())
    }

    pub fn set_application_list(&self, applications: Vec<DesktopApplication>) -> Result<()> {
        validate_applications(&applications)?;
        if *self.applications.borrow() != applications {
            *self.applications.borrow_mut() = applications;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.applications_revision);
            tracker.write(
                self.applications_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            )?;
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

    pub fn set_power(&self, power: PowerSnapshot) -> Result<()> {
        power.validate()?;
        let display_changed = *self.power_display.borrow() != power.display;
        let devices_changed = *self.power_devices.borrow() != power.devices;
        let mut display = self.power_display.borrow_mut();
        let mut devices = self.power_devices.borrow_mut();
        let mut tracker = self.tracker.borrow_mut();
        let mut updates = power
            .values()
            .into_iter()
            .map(|(name, value)| (self.signals[name], value))
            .collect::<Vec<_>>();
        if display_changed {
            let revision = tracker.read(self.power_display_revision);
            updates.push((
                self.power_display_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        if devices_changed {
            let revision = tracker.read(self.power_devices_revision);
            updates.push((
                self.power_devices_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        tracker.write_batch(updates)?;
        if display_changed {
            *display = power.display;
        }
        if devices_changed {
            *devices = power.devices;
        }
        Ok(())
    }

    pub fn set_notifications(&self, notifications: NotificationSnapshot) -> Result<()> {
        notifications.validate()?;
        let records_changed = *self.notifications.borrow() != notifications.active;
        let history_changed = *self.notification_history.borrow() != notifications.history;
        let mut records = self.notifications.borrow_mut();
        let mut history = self.notification_history.borrow_mut();
        let mut tracker = self.tracker.borrow_mut();
        let mut updates = notifications
            .values()
            .into_iter()
            .map(|(name, value)| (self.signals[name], value))
            .collect::<Vec<_>>();
        if records_changed {
            let revision = tracker.read(self.notifications_revision);
            updates.push((
                self.notifications_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        if history_changed {
            let revision = tracker.read(self.notification_history_revision);
            updates.push((
                self.notification_history_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        tracker.write_batch(updates)?;
        if records_changed {
            *records = notifications.active;
        }
        if history_changed {
            *history = notifications.history;
        }
        Ok(())
    }

    pub fn set_system_tray(&self, system_tray: SystemTraySnapshot) -> Result<()> {
        system_tray.validate()?;
        let items_changed = *self.system_tray_items.borrow() != system_tray.items;
        let mut items = self.system_tray_items.borrow_mut();
        let mut tracker = self.tracker.borrow_mut();
        let mut updates = system_tray
            .values()
            .into_iter()
            .map(|(name, value)| (self.signals[name], value))
            .collect::<Vec<_>>();
        if items_changed {
            let revision = tracker.read(self.system_tray_items_revision);
            updates.push((
                self.system_tray_items_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        tracker.write_batch(updates)?;
        if items_changed {
            *items = system_tray.items;
        }
        Ok(())
    }

    pub fn set_clipboard(&self, clipboard: ClipboardSnapshot) -> Result<()> {
        clipboard.validate()?;
        let updates = clipboard
            .values()
            .into_iter()
            .map(|(name, value)| (self.signals[name], value));
        self.tracker.borrow_mut().write_batch(updates)
    }

    pub fn set_authentication(&self, authentication: AuthenticationSnapshot) -> Result<()> {
        authentication.validate()?;
        let updates = authentication
            .values()
            .into_iter()
            .map(|(name, value)| (self.signals[name], value));
        self.tracker.borrow_mut().write_batch(updates)
    }

    pub fn set_power_devices(
        &self,
        display: Option<PowerDevice>,
        devices: Vec<PowerDevice>,
    ) -> Result<()> {
        validate_power_devices(&devices)?;
        if let Some(display) = &display {
            validate_power_device(display)?;
        }
        let display_changed = *self.power_display.borrow() != display;
        let devices_changed = *self.power_devices.borrow() != devices;
        let mut tracker = self.tracker.borrow_mut();
        let mut updates = Vec::new();
        if display_changed {
            let revision = tracker.read(self.power_display_revision);
            updates.push((
                self.power_display_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        if devices_changed {
            let revision = tracker.read(self.power_devices_revision);
            updates.push((
                self.power_devices_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            ));
        }
        tracker.write_batch(updates)?;
        if display_changed {
            *self.power_display.borrow_mut() = display;
        }
        if devices_changed {
            *self.power_devices.borrow_mut() = devices;
        }
        Ok(())
    }

    pub fn set_notification_history(&self, history: Vec<Notification>) -> Result<()> {
        validate_notifications(&history, true)?;
        if *self.notification_history.borrow() != history {
            *self.notification_history.borrow_mut() = history;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.notification_history_revision);
            tracker.write(
                self.notification_history_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            )?;
        }
        Ok(())
    }

    pub fn set_notification_records(&self, notifications: Vec<Notification>) -> Result<()> {
        validate_notifications(&notifications, false)?;
        if *self.notifications.borrow() != notifications {
            *self.notifications.borrow_mut() = notifications;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.notifications_revision);
            tracker.write(
                self.notifications_revision,
                Scalar::Boolean(revision != Scalar::Boolean(true)),
            )?;
        }
        Ok(())
    }

    pub fn set_system_tray_items(&self, items: Vec<SystemTrayItem>) -> Result<()> {
        validate_system_tray_items(&items)?;
        if *self.system_tray_items.borrow() != items {
            *self.system_tray_items.borrow_mut() = items;
            let mut tracker = self.tracker.borrow_mut();
            let revision = tracker.read(self.system_tray_items_revision);
            tracker.write(
                self.system_tray_items_revision,
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
            applications: self.applications.borrow().clone(),
            power_display: self.power_display.borrow().clone(),
            power_devices: self.power_devices.borrow().clone(),
            notifications: self.notifications.borrow().clone(),
            notification_history: self.notification_history.borrow().clone(),
            system_tray_items: self.system_tray_items.borrow().clone(),
            managed_processes: self.managed_processes.borrow().values().cloned().collect(),
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

fn managed_process_state_values(state: &ManagedProcessState) -> [Scalar; 9] {
    [
        Scalar::String(state.status.as_str().into()),
        Scalar::Boolean(state.status.running()),
        Scalar::String(state.stdout.to_string()),
        Scalar::String(state.stderr.to_string()),
        Scalar::Boolean(state.stdout_truncated),
        Scalar::Boolean(state.stderr_truncated),
        state
            .exit_code
            .map_or(Scalar::Nil, |value| Scalar::Number(value.into())),
        state
            .exit_signal
            .map_or(Scalar::Nil, |value| Scalar::Number(value.into())),
        Scalar::String(state.error.to_string()),
    ]
}

fn managed_process_updates(
    signals: &ManagedProcessSignals,
    state: &ManagedProcessState,
) -> [(usize, Scalar); 9] {
    let [
        status,
        running,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        exit_code,
        exit_signal,
        error,
    ] = managed_process_state_values(state);
    [
        (signals.status, status),
        (signals.running, running),
        (signals.stdout, stdout),
        (signals.stderr, stderr),
        (signals.stdout_truncated, stdout_truncated),
        (signals.stderr_truncated, stderr_truncated),
        (signals.exit_code, exit_code),
        (signals.exit_signal, exit_signal),
        (signals.error, error),
    ]
}

fn validate_managed_process_states(states: &[ManagedProcessState]) -> Result<()> {
    if states.len() > MAX_MANAGED_PROCESS_KEYS {
        return Err(Error::runtime("too many managed process states"));
    }
    let mut keys = BTreeSet::new();
    let mut retained = 0_usize;
    for state in states {
        validate_managed_process_key(&state.key)?;
        if !keys.insert(state.key.as_str()) {
            return Err(Error::runtime("duplicate managed process state"));
        }
        if state.stdout.len() > MAX_MANAGED_PROCESS_STREAM_BYTES
            || state.stderr.len() > MAX_MANAGED_PROCESS_STREAM_BYTES
            || state.error.len() > MAX_MANAGED_PROCESS_ERROR_BYTES
            || state.stdout.contains('\0')
            || state.stderr.contains('\0')
            || state.error.contains('\0')
            || state.exit_code.is_some() && state.exit_signal.is_some()
            || state
                .exit_signal
                .is_some_and(|signal| signal <= 0 || signal > 128)
        {
            return Err(Error::runtime("invalid managed process state"));
        }
        retained = retained
            .checked_add(state.stdout.len())
            .and_then(|value| value.checked_add(state.stderr.len()))
            .ok_or_else(|| Error::runtime("managed process state is too large"))?;
    }
    if retained > MAX_MANAGED_PROCESS_RETAINED_BYTES {
        return Err(Error::runtime(
            "managed process state exceeds the 4 MiB retained output limit",
        ));
    }
    Ok(())
}

fn validate_managed_process_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_MANAGED_PROCESS_KEY_BYTES || key.contains('\0') {
        Err(Error::runtime(format!(
            "managed process keys must be 1 to {MAX_MANAGED_PROCESS_KEY_BYTES} bytes without NUL"
        )))
    } else {
        Ok(())
    }
}

fn managed_process_definition(value: Value) -> Result<ManagedProcessDefinition> {
    let definition = match value {
        Value::String(key) => ManagedProcessDefinition {
            key: key.to_str()?.to_owned(),
            shutdown_signal: ManagedProcessShutdownSignal::Terminate,
            command: None,
        },
        Value::Table(table) => {
            for pair in table.clone().pairs::<Value, Value>() {
                let (key, _) = pair?;
                let Value::String(key) = key else {
                    return Err(Error::runtime(
                        "managed process options must use string keys",
                    ));
                };
                let key = key.to_str()?;
                if !matches!(
                    key.as_ref(),
                    "key" | "shutdown_signal" | "command" | "working_directory" | "interval"
                ) {
                    return Err(Error::runtime(format!(
                        "unknown managed process option {key:?}"
                    )));
                }
            }
            let key: String = table.get("key")?;
            let shutdown_signal = match table.get::<Value>("shutdown_signal")? {
                Value::Nil => ManagedProcessShutdownSignal::Terminate,
                Value::String(value) if value.to_str()?.as_ref() == "terminate" => {
                    ManagedProcessShutdownSignal::Terminate
                }
                Value::String(value) if value.to_str()?.as_ref() == "interrupt" => {
                    ManagedProcessShutdownSignal::Interrupt
                }
                _ => {
                    return Err(Error::runtime(
                        "managed process shutdown_signal must be 'terminate' or 'interrupt'",
                    ));
                }
            };
            let command = match table.get::<Value>("command")? {
                Value::Nil => {
                    if !matches!(table.get::<Value>("working_directory")?, Value::Nil)
                        || !matches!(table.get::<Value>("interval")?, Value::Nil)
                    {
                        return Err(Error::runtime(
                            "managed process working_directory and interval require command",
                        ));
                    }
                    None
                }
                Value::Table(arguments) => Some(ManagedProcessCommand {
                    argv: launch_arguments(arguments)?,
                    cwd: launch_working_directory(table.get("working_directory")?)?,
                    interval_ms: managed_process_interval(table.get("interval")?)?,
                }),
                _ => {
                    return Err(Error::runtime(
                        "managed process command must be a dense array of strings",
                    ));
                }
            };
            ManagedProcessDefinition {
                key,
                shutdown_signal,
                command,
            }
        }
        _ => {
            return Err(Error::runtime(
                "services.process expects a key string or options table",
            ));
        }
    };
    validate_managed_process_key(&definition.key)?;
    Ok(definition)
}

fn managed_process_interval(value: Value) -> Result<Option<u64>> {
    let number = match value {
        Value::Nil => return Ok(None),
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => {
            return Err(Error::runtime(format!(
                "managed process interval must be a whole number between {MIN_MANAGED_PROCESS_INTERVAL_MS} and {MAX_MANAGED_PROCESS_INTERVAL_MS}"
            )));
        }
    };
    if !number.is_finite()
        || number.fract() != 0.0
        || !(MIN_MANAGED_PROCESS_INTERVAL_MS as f64..=MAX_MANAGED_PROCESS_INTERVAL_MS as f64)
            .contains(&number)
    {
        return Err(Error::runtime(format!(
            "managed process interval must be a whole number between {MIN_MANAGED_PROCESS_INTERVAL_MS} and {MAX_MANAGED_PROCESS_INTERVAL_MS}"
        )));
    }
    Ok(Some(number as u64))
}

fn create_managed_process_signals(
    tracker: &Rc<RefCell<Tracker>>,
    definition: ManagedProcessDefinition,
    state: &ManagedProcessState,
) -> Result<ManagedProcessSignals> {
    let values = managed_process_state_values(state);
    let mut tracker = tracker.borrow_mut();
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        ids.push(tracker.insert(value)?);
    }
    let [
        status,
        running,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        exit_code,
        exit_signal,
        error,
    ] = ids.as_slice()
    else {
        unreachable!("managed process signal count is fixed");
    };
    Ok(ManagedProcessSignals {
        definition,
        status: *status,
        running: *running,
        stdout: *stdout,
        stderr: *stderr,
        stdout_truncated: *stdout_truncated,
        stderr_truncated: *stderr_truncated,
        exit_code: *exit_code,
        exit_signal: *exit_signal,
        error: *error,
    })
}

fn managed_process_handle(
    lua: &Lua,
    signals: ManagedProcessSignals,
    tracker: &Rc<RefCell<Tracker>>,
    actions: &Rc<RefCell<Vec<Action>>>,
    allowed: &Rc<Cell<bool>>,
) -> Result<Table> {
    let handle = lua.create_table()?;
    for (name, id) in [
        ("status", signals.status),
        ("running", signals.running),
        ("stdout", signals.stdout),
        ("stderr", signals.stderr),
        ("stdout_truncated", signals.stdout_truncated),
        ("stderr_truncated", signals.stderr_truncated),
        ("exit_code", signals.exit_code),
        ("exit_signal", signals.exit_signal),
        ("error", signals.error),
    ] {
        handle.set(
            name,
            Signal {
                id,
                tracker: tracker.clone(),
                readonly: true,
            },
        )?;
    }
    if signals.definition.command.is_some() {
        for (name, action) in [
            ("refresh", ManagedProcessAction::Refresh),
            ("cancel", ManagedProcessAction::Cancel),
        ] {
            let key = signals.definition.key.clone();
            let actions = actions.clone();
            let allowed = allowed.clone();
            handle.set(
                name,
                lua.create_function(move |_, ()| {
                    enqueue(
                        &actions,
                        &allowed,
                        Action::ManagedProcess {
                            key: key.clone(),
                            action: action.clone(),
                        },
                    )
                })?,
            )?;
        }
    } else {
        let key = signals.definition.key.clone();
        let start_actions = actions.clone();
        let start_allowed = allowed.clone();
        handle.set(
            "start",
            lua.create_function(move |_, (arguments, cwd): (Table, Value)| {
                let argv = launch_arguments(arguments)?;
                let cwd = launch_working_directory(cwd)?;
                enqueue(
                    &start_actions,
                    &start_allowed,
                    Action::ManagedProcess {
                        key: key.clone(),
                        action: ManagedProcessAction::Start { argv, cwd },
                    },
                )
            })?,
        )?;
        let key = signals.definition.key;
        let stop_actions = actions.clone();
        let stop_allowed = allowed.clone();
        handle.set(
            "stop",
            lua.create_function(move |_, ()| {
                enqueue(
                    &stop_actions,
                    &stop_allowed,
                    Action::ManagedProcess {
                        key: key.clone(),
                        action: ManagedProcessAction::Stop,
                    },
                )
            })?,
        )?;
    }
    handle.set_readonly(true);
    Ok(handle)
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

/// validates a launch against the list the configuration currently sees, and
/// binds it to the observed entry revision.
fn application_launch(
    applications: &[DesktopApplication],
    id: Value,
    options: Value,
) -> Result<ApplicationLaunch> {
    let Value::String(id) = id else {
        return Err(Error::runtime("application id must be a string"));
    };
    let id = id
        .to_str()
        .map_err(|_| Error::runtime("application id must be valid utf-8"))?;
    let application = applications
        .iter()
        .find(|application| application.id == *id)
        .ok_or_else(|| Error::runtime(format!("unknown application {:?}", &*id)))?;
    let mut launch = ApplicationLaunch {
        id: application.id.clone(),
        revision: application.revision,
        action: None,
        uris: Vec::new(),
        terminal: Vec::new(),
    };
    let options = match options {
        Value::Nil => return Ok(launch),
        Value::Table(options) => options,
        _ => {
            return Err(Error::runtime(
                "application launch options must be a table or nil",
            ));
        }
    };
    for pair in options.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let known = matches!(&key, Value::String(key)
            if matches!(key.as_bytes().as_ref(), b"action" | b"uris" | b"terminal"));
        if !known {
            return Err(Error::runtime(
                "application launch options accept only action, uris, and terminal",
            ));
        }
    }
    match options.raw_get::<Value>("action")? {
        Value::Nil => {}
        Value::String(key) => {
            let key = key
                .to_str()
                .map_err(|_| Error::runtime("application action must be valid utf-8"))?;
            if !application.actions.iter().any(|action| action.key == *key) {
                return Err(Error::runtime(format!(
                    "application {:?} has no action {:?}",
                    application.id, &*key
                )));
            }
            launch.action = Some(key.to_owned());
        }
        _ => return Err(Error::runtime("application action must be a string")),
    }
    match options.raw_get::<Value>("uris")? {
        Value::Nil => {}
        Value::Table(uris) => {
            launch.uris = launch_arguments(uris)
                .ok()
                .filter(|uris| uris.iter().all(|uri| !uri.is_empty()))
                .ok_or_else(|| Error::runtime(launch_uri_error()))?;
        }
        _ => return Err(Error::runtime(launch_uri_error())),
    }
    match options.raw_get::<Value>("terminal")? {
        Value::Nil => {}
        Value::Table(terminal) => launch.terminal = launch_arguments(terminal)?,
        _ => {
            return Err(Error::runtime(
                "application terminal must be an array of strings",
            ));
        }
    }
    if launch.uris.is_empty() {
        return Ok(launch);
    }
    // the d-bus method for actions takes no uris, so neither backend accepts them.
    if launch.action.is_some() {
        return Err(Error::runtime("application actions do not accept uris"));
    }
    if application.accepts == DesktopArguments::None {
        return Err(Error::runtime(format!(
            "application {:?} does not accept files or urls",
            application.id
        )));
    }
    for uri in &launch.uris {
        if !uri.starts_with('/') && uri_scheme(uri).is_none() {
            return Err(Error::runtime(
                "application uris must be absolute paths or uris with a scheme",
            ));
        }
        if application.accepts.local_only() && !is_local_file(uri) {
            return Err(Error::runtime(format!(
                "application {:?} opens only local files; remote urls are not downloaded",
                application.id
            )));
        }
    }
    Ok(launch)
}

fn launch_uri_error() -> String {
    format!(
        "application uris must be a nonempty dense array of at most {MAX_LAUNCH_ARGUMENTS} \
         nonempty strings within the launch size limits"
    )
}

/// returns the scheme of an absolute uri, such as `https` in `https://...`.
pub fn uri_scheme(uri: &str) -> Option<&str> {
    let (scheme, _) = uri.split_once(':')?;
    let mut characters = scheme.chars();
    (characters.next()?.is_ascii_alphabetic()
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        }))
    .then_some(scheme)
}

/// whether a launch uri names a file on this machine: an absolute path, or a
/// `file:` uri without a host other than `localhost`.
pub fn is_local_file(uri: &str) -> bool {
    if uri.starts_with('/') {
        return true;
    }
    if !uri_scheme(uri).is_some_and(|scheme| scheme.eq_ignore_ascii_case("file")) {
        return false;
    }
    let rest = &uri[5..];
    match rest.strip_prefix("//") {
        Some(authority) => authority
            .split_once('/')
            .is_some_and(|(host, _)| host.is_empty() || host.eq_ignore_ascii_case("localhost")),
        None => rest.starts_with('/'),
    }
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

fn system_tray_item_id(value: Value) -> Result<String> {
    let Value::String(value) = value else {
        return Err(Error::runtime("system tray item id must be a string"));
    };
    let value = value
        .to_str()
        .map_err(|_| Error::runtime("system tray item id must be valid utf-8"))?;
    if value.is_empty() || value.len() > 128 || value.contains('\0') {
        return Err(Error::runtime(
            "system tray item id must be nonempty, contain no NUL, and not exceed 128 bytes",
        ));
    }
    Ok(value.to_owned())
}

fn system_tray_menu_id(value: Value) -> Result<String> {
    let Value::String(value) = value else {
        return Err(Error::runtime("system tray menu id must be a string"));
    };
    let value = value
        .to_str()
        .map_err(|_| Error::runtime("system tray menu id must be valid utf-8"))?;
    if value.is_empty() || value.len() > 128 || value.contains('\0') {
        return Err(Error::runtime(
            "system tray menu id must be nonempty, contain no NUL, and not exceed 128 bytes",
        ));
    }
    Ok(value.to_owned())
}

fn notification_key(value: Value) -> Result<u64> {
    let Value::String(value) = value else {
        return Err(Error::runtime("notification key must be a string"));
    };
    let value = value
        .to_str()
        .map_err(|_| Error::runtime("notification key must be valid utf-8"))?;
    let key = value
        .parse::<u64>()
        .map_err(|_| Error::runtime("invalid notification key"))?;
    if key == 0 || value != key.to_string() {
        return Err(Error::runtime("invalid notification key"));
    }
    Ok(key)
}

fn validate_notification_action_key(value: &str) -> Result<()> {
    if value.is_empty() || invalid_notification_text(value) {
        Err(Error::runtime("invalid notification action key"))
    } else {
        Ok(())
    }
}

fn validate_service(name: &str, value: &Scalar) -> Result<()> {
    let valid = match (name, value) {
        (
            "audio.available"
            | "audio.muted"
            | "audio.source_available"
            | "audio.source_muted"
            | "media.available"
            | "applications.available"
            | "media.playing"
            | "media.can_play_pause"
            | "media.can_next"
            | "media.can_previous"
            | "media.can_stop"
            | "media.can_seek"
            | "power.available"
            | "power.on_battery"
            | "notifications.owned"
            | "system_tray.available"
            | "system_tray.watcher_owned"
            | "clipboard.available"
            | "clipboard.loading"
            | "idle.inhibit_available"
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
            | "applications.error"
            | "power.error"
            | "notifications.error"
            | "system_tray.error"
            | "clipboard.error"
            | "niri.error",
            Scalar::String(value),
        ) => value.len() <= 4096 && !value.contains('\0'),
        (
            "authentication.state" | "authentication.prompt" | "authentication.message",
            Scalar::String(value),
        ) => valid_authentication_text(value),
        ("authentication.available", Scalar::Boolean(_)) => true,
        ("media.art_url", Scalar::String(value)) => {
            value.len() <= 16 * 1024 && !value.contains('\0')
        }
        ("media.length_ms", Scalar::Nil) => true,
        ("media.length_ms", Scalar::Number(value)) => {
            value.is_finite() && (0.0..=MAX_MEDIA_TIME_US as f64 / 1000.0).contains(value)
        }
        ("media.rate", Scalar::Number(value)) => valid_media_rate(*value),
        ("clipboard.text", Scalar::Nil) => true,
        ("clipboard.text", Scalar::String(value)) => validate_clipboard_text(value).is_ok(),
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

fn valid_authentication_text(value: &str) -> bool {
    value.len() <= 1024
        && !value.chars().any(|character| {
            character == '\0'
                || character.is_control()
                || matches!(character, '\u{2028}' | '\u{2029}')
        })
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

const MAX_APPLICATIONS: usize = 4096;

fn validate_applications(applications: &[DesktopApplication]) -> Result<()> {
    let text = |value: &str| value.len() <= 4096 && !value.contains('\0');
    let list = |values: &[String], max: usize| {
        values.len() <= max && values.iter().all(|value| text(value))
    };
    let command = |values: &[String]| {
        values.len() <= MAX_LAUNCH_ARGUMENTS
            && values
                .iter()
                .all(|value| value.len() <= MAX_LAUNCH_ARGUMENT_BYTES && !value.contains('\0'))
            && values.iter().map(String::len).sum::<usize>() <= MAX_LAUNCH_BYTES
            && values.first().is_none_or(|program| !program.is_empty())
    };
    let mut ids = BTreeSet::new();
    let valid = applications.len() <= MAX_APPLICATIONS
        && applications.iter().all(|application| {
            !application.id.is_empty()
                && application.revision != 0
                && text(&application.id)
                && ids.insert(application.id.as_str())
                && !application.name.is_empty()
                && text(&application.name)
                && text(&application.generic_name)
                && text(&application.comment)
                && text(&application.icon)
                && text(&application.startup_wm_class)
                && application
                    .working_directory
                    .as_deref()
                    .is_none_or(|path| !path.is_empty() && text(path))
                && list(&application.keywords, 64)
                && list(&application.categories, 64)
                && command(&application.command)
                && application.actions.len() <= 32
                && application.actions.iter().all(|action| {
                    !action.key.is_empty()
                        && text(&action.key)
                        && !action.name.is_empty()
                        && text(&action.name)
                        && text(&action.icon)
                        && command(&action.command)
                })
        });
    if valid {
        Ok(())
    } else {
        Err(Error::runtime("invalid application list"))
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
                || invalid_media_text(&player.track_id, 4096)
                || player
                    .length_us
                    .is_some_and(|length| !(0..=MAX_MEDIA_TIME_US).contains(&length))
                || !valid_media_rate(player.rate)
                || player.position.is_some_and(|position| {
                    !(0..=MAX_MEDIA_TIME_US).contains(&position.position_us)
                })
        });
    if invalid {
        Err(Error::runtime("invalid media player list"))
    } else {
        Ok(())
    }
}

fn valid_media_rate(rate: f64) -> bool {
    rate.is_finite() && rate != 0.0 && rate.abs() <= MAX_MEDIA_RATE
}

fn scalar_string(value: Scalar) -> String {
    match value {
        Scalar::String(value) => value,
        _ => String::new(),
    }
}

fn media_milliseconds(microseconds: Option<i64>) -> Scalar {
    microseconds.map_or(Scalar::Nil, |us| Scalar::Number(us as f64 / 1000.0))
}

/// converts a luau millisecond count to whole microseconds within the media limit.
fn media_microseconds(milliseconds: f64, signed: bool, name: &str) -> Result<i64> {
    let limit = MAX_MEDIA_TIME_US as f64 / 1000.0;
    let lower = if signed { -limit } else { 0.0 };
    if !milliseconds.is_finite() || !(lower..=limit).contains(&milliseconds) {
        return Err(Error::runtime(format!(
            "{name} must be a finite number of milliseconds within about 31 years{}",
            if signed { "" } else { " and not negative" }
        )));
    }
    Ok((milliseconds * 1000.0).round() as i64)
}

fn validate_system_tray_items(items: &[SystemTrayItem]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut image_bytes = 0_usize;
    let mut menu_ids = BTreeSet::new();
    let mut menu_nodes = 0_usize;
    let mut menu_bytes = 0_usize;
    let invalid = items.len() > 128
        || items.iter().any(|item| {
            let source_bytes = item.icon.as_ref().and_then(validate_system_tray_image);
            let valid_source = item.icon.is_none() || source_bytes.is_some();
            image_bytes = image_bytes.saturating_add(source_bytes.unwrap_or(0));
            let menu_size = item
                .menu
                .as_ref()
                .and_then(|menu| validate_system_tray_menu(menu, &mut menu_ids));
            let valid_menu = item.has_menu == item.menu.is_some()
                && (item.menu.is_none() || menu_size.is_some());
            if let Some((nodes, bytes)) = menu_size {
                menu_nodes = menu_nodes.saturating_add(nodes);
                menu_bytes = menu_bytes.saturating_add(bytes);
            }
            item.id.is_empty()
                || item.id.len() > 128
                || item.id.contains('\0')
                || !ids.insert(item.id.as_str())
                || !valid_source
                || !valid_menu
                || image_bytes > MAX_TRAY_IMAGE_BYTES
                || menu_nodes > MAX_TRAY_MENU_TOTAL_NODES
                || menu_bytes > MAX_TRAY_MENU_TOTAL_BYTES
                || invalid_system_tray_text(&item.protocol_item_id, 4096)
                || invalid_system_tray_text(&item.category, 4096)
                || invalid_system_tray_text(&item.title, 4096)
                || invalid_system_tray_text(&item.status, 4096)
                || invalid_system_tray_text(&item.icon_name, 4096)
                || invalid_system_tray_text(&item.attention_icon_name, 4096)
                || invalid_system_tray_text(&item.overlay_icon_name, 4096)
                || invalid_system_tray_text(&item.tooltip_icon_name, 4096)
                || invalid_system_tray_text(&item.tooltip_title, 4096)
                || invalid_system_tray_text(&item.tooltip_text, 16 * 1024)
        });
    if invalid {
        Err(Error::runtime("invalid system tray item list"))
    } else {
        Ok(())
    }
}

fn validate_system_tray_menu(
    menu: &SystemTrayMenu,
    ids: &mut BTreeSet<String>,
) -> Option<(usize, usize)> {
    if menu.id.is_empty()
        || menu.id.len() > 128
        || menu.id.contains('\0')
        || !ids.insert(menu.id.clone())
        || invalid_system_tray_text(&menu.error, 4096)
        || !menu.loaded && !menu.children.is_empty()
        || menu.children.len() > MAX_TRAY_MENU_CHILDREN
    {
        return None;
    }
    let mut nodes = 1_usize;
    let mut bytes = size_of::<SystemTrayMenu>()
        .checked_add(menu.id.capacity())?
        .checked_add(menu.error.capacity())?
        .checked_add(
            menu.children
                .capacity()
                .checked_mul(size_of::<SystemTrayMenuItem>())?,
        )?;
    for child in &menu.children {
        let (child_nodes, child_bytes) = validate_system_tray_menu_item(child, ids, 1)?;
        nodes = nodes.checked_add(child_nodes)?;
        bytes = bytes.checked_add(child_bytes)?;
    }
    (nodes <= MAX_TRAY_MENU_NODES && bytes <= MAX_TRAY_MENU_BYTES).then_some((nodes, bytes))
}

fn validate_system_tray_menu_item(
    item: &SystemTrayMenuItem,
    ids: &mut BTreeSet<String>,
    depth: usize,
) -> Option<(usize, usize)> {
    if depth > MAX_TRAY_MENU_DEPTH
        || item.id.is_empty()
        || item.id.len() > 128
        || item.id.contains('\0')
        || !ids.insert(item.id.clone())
        || invalid_system_tray_text(&item.kind, 4096)
        || invalid_system_tray_text(&item.label, 4096)
        || item
            .access_key
            .as_ref()
            .is_some_and(|key| key.chars().count() != 1 || invalid_system_tray_text(key, 8))
        || invalid_system_tray_text(&item.icon_name, 4096)
        || invalid_system_tray_text(&item.toggle_type, 4096)
        || invalid_system_tray_text(&item.disposition, 4096)
        || invalid_system_tray_text(&item.error, 4096)
        || item.shortcut.len() > 8
        || item.shortcut.iter().any(|chord| {
            chord.len() > 8 || chord.iter().any(|part| invalid_system_tray_text(part, 64))
        })
        || item.children.len() > MAX_TRAY_MENU_CHILDREN
        || !item.loaded && !item.children.is_empty()
        || !item.has_submenu && (item.loaded || item.loading || !item.children.is_empty())
    {
        return None;
    }
    let shortcut_bytes = item.shortcut.iter().try_fold(
        item.shortcut
            .capacity()
            .checked_mul(size_of::<Vec<String>>())?,
        |total, chord| {
            chord.iter().try_fold(
                total.checked_add(chord.capacity().checked_mul(size_of::<String>())?)?,
                |total, part| total.checked_add(part.capacity()),
            )
        },
    )?;
    let mut nodes = 1_usize;
    let mut bytes = item
        .id
        .capacity()
        .checked_add(item.kind.capacity())?
        .checked_add(item.label.capacity())?
        .checked_add(item.access_key.as_ref().map_or(0, String::capacity))?
        .checked_add(item.icon_name.capacity())?
        .checked_add(item.toggle_type.capacity())?
        .checked_add(item.disposition.capacity())?
        .checked_add(item.error.capacity())?
        .checked_add(shortcut_bytes)?
        .checked_add(
            item.children
                .capacity()
                .checked_mul(size_of::<SystemTrayMenuItem>())?,
        )?;
    for child in &item.children {
        let (child_nodes, child_bytes) = validate_system_tray_menu_item(child, ids, depth + 1)?;
        nodes = nodes.checked_add(child_nodes)?;
        bytes = bytes.checked_add(child_bytes)?;
    }
    Some((nodes, bytes))
}

fn validate_system_tray_image(source: &ImageSource) -> Option<usize> {
    if source.id().len() > 256 || source.candidates().is_empty() || source.candidates().len() > 2 {
        return None;
    }
    let mut retained = 0_usize;
    for candidate in source.candidates() {
        if candidate
            .icon()
            .is_some_and(|icon| invalid_system_tray_text(icon, 4096))
            || candidate.pixmaps().len() > MAX_TRAY_PIXMAPS
        {
            return None;
        }
        for pixmap in candidate.pixmaps() {
            if pixmap.width() == 0
                || pixmap.height() == 0
                || pixmap.width() > MAX_TRAY_PIXMAP_SIDE
                || pixmap.height() > MAX_TRAY_PIXMAP_SIDE
                || pixmap.rgba().len() > MAX_TRAY_PIXMAP_BYTES
            {
                return None;
            }
            let expected = usize::try_from(pixmap.width())
                .ok()
                .zip(usize::try_from(pixmap.height()).ok())
                .and_then(|(width, height)| width.checked_mul(height))
                .and_then(|pixels| pixels.checked_mul(4));
            if expected != Some(pixmap.rgba().len()) {
                return None;
            }
            retained = retained.checked_add(pixmap.rgba().len())?;
        }
    }
    (retained <= MAX_TRAY_ITEM_IMAGE_BYTES).then_some(retained)
}

fn validate_power_devices(devices: &[PowerDevice]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let invalid = devices.len() > 128
        || devices
            .iter()
            .any(|device| validate_power_device(device).is_err() || !ids.insert(&device.id));
    if invalid {
        Err(Error::runtime("invalid power device list"))
    } else {
        Ok(())
    }
}

fn validate_power_device(device: &PowerDevice) -> Result<()> {
    let optional_text_valid = |value: &Option<String>| {
        value
            .as_ref()
            .is_none_or(|value| !value.is_empty() && valid_power_text(value, 4096))
    };
    let optional_number_valid = |value: Option<f64>, range: Option<(f64, f64)>| {
        value.is_none_or(|value| {
            value.is_finite()
                && range.is_none_or(|(minimum, maximum)| (minimum..=maximum).contains(&value))
        })
    };
    let valid = device.id.starts_with('/')
        && valid_power_text(&device.id, 4096)
        && valid_power_enum(&device.kind)
        && valid_power_enum(&device.state)
        && valid_power_enum(&device.warning_level)
        && device
            .battery_level
            .as_ref()
            .is_none_or(|value| valid_power_enum(value))
        && device.time_to_empty.is_none_or(|value| value > 0)
        && device.time_to_full.is_none_or(|value| value > 0)
        && optional_number_valid(device.percentage, Some((0.0, 100.0)))
        && optional_number_valid(device.energy, Some((0.0, f64::MAX)))
        && optional_number_valid(device.energy_empty, Some((0.0, f64::MAX)))
        && optional_number_valid(device.energy_full, Some((0.0, f64::MAX)))
        && optional_number_valid(device.energy_full_design, Some((0.0, f64::MAX)))
        && optional_number_valid(device.energy_rate, None)
        && optional_number_valid(device.capacity, Some((0.0, 100.0)))
        && optional_text_valid(&device.native_path)
        && optional_text_valid(&device.vendor)
        && optional_text_valid(&device.model)
        && optional_text_valid(&device.serial)
        && optional_text_valid(&device.icon_name);
    if valid {
        Ok(())
    } else {
        Err(Error::runtime("invalid power device"))
    }
}

fn valid_power_enum(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
}

fn valid_power_text(value: &str, max: usize) -> bool {
    value.len() <= max && !value.contains('\0')
}

fn invalid_media_text(value: &str, max: usize) -> bool {
    value.len() > max || value.contains('\0')
}

fn invalid_system_tray_text(value: &str, max: usize) -> bool {
    value.len() > max || value.contains('\0')
}

const MAX_SAFE_LUA_INTEGER: u64 = (1_u64 << 53) - 1;
/// maximum number of closed notifications kept during one bus connection.
pub const MAX_NOTIFICATION_HISTORY: usize = 128;
const MAX_NOTIFICATION_RETAINED_BYTES: usize = 512 * 1024;
const MAX_ACTIVE_NOTIFICATION_RETAINED_BYTES: usize = 8 * 1024 * 1024;

/// validates active records, or closed history records when `closed` is true.
/// protocol ids may repeat in the history, because a closed id can be reused.
fn validate_notifications(notifications: &[Notification], closed: bool) -> Result<()> {
    let mut keys = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut retained_bytes = 0_usize;
    let limit = if closed {
        MAX_NOTIFICATION_HISTORY
    } else {
        1_024
    };
    let invalid = notifications.len() > limit
        || notifications.iter().any(|notification| {
            let valid_key = notification
                .key
                .parse::<u64>()
                .is_ok_and(|key| key != 0 && key.to_string() == notification.key);
            let record_bytes = notification_retained_bytes(notification);
            let within_budget = record_bytes.is_some_and(|record_bytes| {
                retained_bytes = retained_bytes.saturating_add(record_bytes);
                record_bytes <= MAX_NOTIFICATION_RETAINED_BYTES
                    && retained_bytes <= MAX_ACTIVE_NOTIFICATION_RETAINED_BYTES
            });
            !within_budget
                || notification.id == 0
                || notification.revision == 0
                || notification.revision > MAX_SAFE_LUA_INTEGER
                || !valid_key
                || !keys.insert(notification.key.as_str())
                || (!closed && !ids.insert(notification.id))
                || notification.closed.is_some() != closed
                || notification
                    .closed
                    .is_some_and(|(_, at_ms)| at_ms > MAX_SAFE_LUA_INTEGER)
                || notification.actions.len() > 128
                || notification.raw_expire_timeout_ms < -1
                || notification.updated_at_ms < notification.created_at_ms
                || notification.created_at_ms > MAX_SAFE_LUA_INTEGER
                || notification.updated_at_ms > MAX_SAFE_LUA_INTEGER
                || notification
                    .expires_at_ms
                    .is_some_and(|deadline| deadline > MAX_SAFE_LUA_INTEGER)
                || match (
                    notification.effective_timeout_ms,
                    notification.expires_at_ms,
                ) {
                    (None, None) => false,
                    (Some(timeout), Some(deadline)) => {
                        timeout == 0
                            || deadline
                                != notification
                                    .updated_at_ms
                                    .saturating_add(u64::from(timeout))
                    }
                    _ => true,
                }
                || [
                    &notification.app_name,
                    &notification.app_icon,
                    &notification.summary,
                    &notification.body,
                    &notification.category,
                    &notification.desktop_entry,
                    &notification.image_path,
                    &notification.reply_placeholder,
                ]
                .into_iter()
                .any(|value| invalid_notification_text(value))
                || {
                    let mut action_keys = BTreeSet::new();
                    notification.actions.iter().any(|action| {
                        validate_notification_action_key(&action.key).is_err()
                            || invalid_notification_text(&action.label)
                            || !action_keys.insert(action.key.as_str())
                    })
                }
        });
    if invalid {
        Err(Error::runtime("invalid notification list"))
    } else {
        Ok(())
    }
}

fn notification_retained_bytes(notification: &Notification) -> Option<usize> {
    let text = [
        &notification.key,
        &notification.app_name,
        &notification.app_icon,
        &notification.summary,
        &notification.body,
        &notification.category,
        &notification.desktop_entry,
        &notification.image_path,
        &notification.reply_placeholder,
    ];
    let image = notification.image.as_ref().map_or(0, |image| {
        image
            .candidates()
            .iter()
            .flat_map(|candidate| candidate.pixmaps())
            .map(|pixmap| pixmap.rgba().len())
            .sum()
    });
    let mut retained = size_of::<Notification>().checked_add(image)?.checked_add(
        notification
            .actions
            .capacity()
            .checked_mul(size_of::<NotificationButton>())?,
    )?;
    for value in text {
        retained = retained.checked_add(value.capacity())?;
    }
    for action in &notification.actions {
        retained = retained.checked_add(action.key.capacity())?;
        retained = retained.checked_add(action.label.capacity())?;
    }
    Some(retained)
}

fn invalid_notification_text(value: &str) -> bool {
    value.len() > 64 * 1024 || value.contains('\0')
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

fn validate_clipboard_text(value: &str) -> Result<()> {
    if value.len() > MAX_CLIPBOARD_TEXT_BYTES || value.contains('\0') {
        Err(Error::runtime(
            "clipboard text must contain no NUL and must not exceed 1 MiB",
        ))
    } else {
        Ok(())
    }
}

fn enqueue_clipboard(
    actions: &RefCell<Vec<Action>>,
    allowed: &Cell<bool>,
    action: Action,
) -> Result<()> {
    if !allowed.get() {
        return Err(Error::runtime(
            "service actions are allowed only in event handlers",
        ));
    }
    let mut actions = actions.borrow_mut();
    if actions.len() >= 128 {
        return Err(Error::runtime("too many service actions in one event"));
    }
    let queued = actions
        .iter()
        .filter_map(|action| match action {
            Action::SetClipboardText(text) => Some(text.len()),
            _ => None,
        })
        .try_fold(0_usize, usize::checked_add)
        .and_then(|bytes| match &action {
            Action::SetClipboardText(text) => bytes.checked_add(text.len()),
            _ => Some(bytes),
        });
    if queued.is_none_or(|bytes| bytes > MAX_QUEUED_CLIPBOARD_BYTES) {
        return Err(Error::runtime(
            "clipboard actions exceed the 4 MiB per-event limit",
        ));
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

    fn process_state(key: &str, status: ManagedProcessStatus) -> ManagedProcessState {
        ManagedProcessState {
            key: key.into(),
            status,
            stdout: Arc::from("ready\n"),
            stderr: Arc::from("warning\n"),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: None,
            exit_signal: None,
            error: Arc::from(""),
        }
    }

    #[test]
    fn managed_process_declarations_and_actions_are_explicit() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker).unwrap();
        lua.load(
            r#"
            query = services.process('query')
            recorder = services.process {
                key = 'recorder',
                shutdown_signal = 'interrupt',
            }
            scheduled = services.process {
                key = 'scheduled',
                command = { 'program', '--once' },
                working_directory = '/tmp',
                interval = 3000,
            }
            "#,
        )
        .exec()
        .unwrap();
        assert_eq!(
            services.managed_process_definitions(),
            vec![
                ManagedProcessDefinition {
                    key: "query".into(),
                    shutdown_signal: ManagedProcessShutdownSignal::Terminate,
                    command: None,
                },
                ManagedProcessDefinition {
                    key: "recorder".into(),
                    shutdown_signal: ManagedProcessShutdownSignal::Interrupt,
                    command: None,
                },
                ManagedProcessDefinition {
                    key: "scheduled".into(),
                    shutdown_signal: ManagedProcessShutdownSignal::Terminate,
                    command: Some(ManagedProcessCommand {
                        argv: vec!["program".into(), "--once".into()],
                        cwd: Some("/tmp".into()),
                        interval_ms: Some(3000),
                    }),
                },
            ]
        );
        assert!(lua.load("query.start({'program'})").exec().is_err());
        assert!(lua.load("scheduled.refresh()").exec().is_err());
        assert!(
            lua.load("return scheduled.start")
                .eval::<Value>()
                .unwrap()
                .is_nil()
        );
        assert!(
            lua.load("return query.refresh")
                .eval::<Value>()
                .unwrap()
                .is_nil()
        );

        services.allow_actions(true);
        lua.load(
            "query.start({'program', '--flag'}, '/tmp'); recorder.stop(); scheduled.refresh(); scheduled.cancel()",
        )
            .exec()
            .unwrap();
        assert_eq!(
            services.take_actions(),
            vec![
                Action::ManagedProcess {
                    key: "query".into(),
                    action: ManagedProcessAction::Start {
                        argv: vec!["program".into(), "--flag".into()],
                        cwd: Some("/tmp".into()),
                    },
                },
                Action::ManagedProcess {
                    key: "recorder".into(),
                    action: ManagedProcessAction::Stop,
                },
                Action::ManagedProcess {
                    key: "scheduled".into(),
                    action: ManagedProcessAction::Refresh,
                },
                Action::ManagedProcess {
                    key: "scheduled".into(),
                    action: ManagedProcessAction::Cancel,
                },
            ]
        );

        services.freeze_managed_process_declarations();
        assert!(lua.load("services.process('late')").exec().is_err());
    }

    #[test]
    fn managed_process_state_is_shared_reactive_and_transactional() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker.clone()).unwrap();
        services
            .set_managed_processes(ManagedProcessSnapshot {
                states: vec![process_state("query", ManagedProcessStatus::Running)],
            })
            .unwrap();
        lua.load("query = services.process('query')")
            .exec()
            .unwrap();
        assert_eq!(
            lua.load("return query.status:get(), query.running:get(), query.stdout:get()")
                .eval::<(String, bool, String)>()
                .unwrap(),
            ("running".into(), true, "ready\n".into())
        );

        tracker.borrow_mut().begin(42);
        lua.load("return query.stdout:get()")
            .eval::<String>()
            .unwrap();
        tracker.borrow_mut().finish(42);
        let mut exited = process_state("query", ManagedProcessStatus::Exited);
        exited.stdout = Arc::from("complete\n");
        exited.exit_code = Some(0);
        services
            .set_managed_processes(ManagedProcessSnapshot {
                states: vec![exited.clone()],
            })
            .unwrap();
        assert_eq!(tracker.borrow().dirty, BTreeSet::from([42]));
        assert_eq!(
            lua.load("return query.status:get(), query.running:get(), query.stdout:get(), query.exit_code:get()")
                .eval::<(String, bool, String, i32)>()
                .unwrap(),
            ("exited".into(), false, "complete\n".into(), 0)
        );
        assert_eq!(services.snapshot().managed_processes, vec![exited]);

        let before = services.snapshot();
        let mut invalid = process_state("query", ManagedProcessStatus::Running);
        invalid.stdout = Arc::from("x".repeat(MAX_MANAGED_PROCESS_STREAM_BYTES + 1));
        assert!(
            services
                .set_managed_processes(ManagedProcessSnapshot {
                    states: vec![invalid],
                })
                .is_err()
        );
        assert_eq!(services.snapshot(), before);
    }

    #[test]
    fn managed_process_declarations_reject_conflicts_and_unknown_options() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker).unwrap();
        lua.load("first = services.process('same'); second = services.process('same')")
            .exec()
            .unwrap();
        assert_eq!(services.managed_process_definitions().len(), 1);
        assert!(
            lua.load("services.process { key = 'same', shutdown_signal = 'interrupt' }")
                .exec()
                .is_err()
        );
        assert!(
            lua.load("services.process { key = 'other', restart = true }")
                .exec()
                .is_err()
        );
        for source in [
            "services.process { key = 'missing-command', interval = 1000 }",
            "services.process { key = 'sparse', command = { [2] = 'program' } }",
            "services.process { key = 'short', command = { 'program' }, interval = 999 }",
            "services.process { key = 'fractional', command = { 'program' }, interval = 1000.5 }",
            "services.process { key = 'long', command = { 'program' }, interval = 31536000001 }",
        ] {
            assert!(lua.load(source).exec().is_err(), "accepted {source}");
        }
        lua.load(
            "services.process { key = 'scheduled', command = { 'program' }, interval = 1000 }",
        )
        .exec()
        .unwrap();
        assert!(
            lua.load(
                "services.process { key = 'scheduled', command = { 'other' }, interval = 1000 }"
            )
            .exec()
            .is_err()
        );
        assert!(lua.load("services.process('scheduled')").exec().is_err());
        assert!(lua.load("services.process('')").exec().is_err());
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
    fn idle_inhibitor_availability_is_a_read_only_signal() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker.clone()).unwrap();

        tracker.borrow_mut().begin(1);
        assert!(
            !lua.load("return services.idle.inhibit_available:get()")
                .eval::<bool>()
                .unwrap()
        );
        tracker.borrow_mut().finish(1);
        services
            .set("idle.inhibit_available", Scalar::Boolean(true))
            .unwrap();

        assert_eq!(tracker.borrow().dirty, BTreeSet::from([1]));
        assert!(
            lua.load("return services.idle.inhibit_available:get()")
                .eval::<bool>()
                .unwrap()
        );
        assert!(
            lua.load("services.idle.inhibit_available = false")
                .exec()
                .is_err()
        );
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

    #[test]
    fn authentication_snapshot_is_read_only_and_atomic() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker).unwrap();

        assert_eq!(
            lua.load("return services.authentication.state:get()")
                .eval::<String>()
                .unwrap(),
            "unavailable"
        );
        services
            .set_authentication(AuthenticationSnapshot {
                available: true,
                state: AuthenticationState::Waiting,
                prompt: "verification code".into(),
                message: "additional authentication is required".into(),
            })
            .unwrap();
        assert_eq!(
            lua.load(
                "return services.authentication.available:get(), services.authentication.state:get(), services.authentication.prompt:get()",
            )
            .eval::<(bool, String, String)>()
            .unwrap(),
            (true, "waiting".into(), "verification code".into())
        );

        let before = services.snapshot();
        assert!(
            services
                .set_authentication(AuthenticationSnapshot {
                    available: true,
                    state: AuthenticationState::Idle,
                    prompt: "unexpected".into(),
                    message: String::new(),
                })
                .is_err()
        );
        assert_eq!(services.snapshot(), before);
        assert!(
            lua.load("services.authentication.state = 'idle'")
                .exec()
                .is_err()
        );
    }

    #[test]
    fn clipboard_snapshot_updates_atomically() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker.clone()).unwrap();

        tracker.borrow_mut().begin(1);
        lua.load("return services.clipboard.text:get()")
            .eval::<Value>()
            .unwrap();
        tracker.borrow_mut().finish(1);
        services
            .set_clipboard(ClipboardSnapshot {
                available: true,
                text: Some(String::new()),
                ..ClipboardSnapshot::default()
            })
            .unwrap();
        assert_eq!(tracker.borrow().dirty, BTreeSet::from([1]));

        let before = services.snapshot();
        assert!(
            services
                .set_clipboard(ClipboardSnapshot {
                    available: false,
                    text: Some("stale".into()),
                    ..ClipboardSnapshot::default()
                })
                .is_err()
        );
        assert_eq!(services.snapshot(), before);
    }

    #[test]
    fn clipboard_actions_are_explicit_and_bounded() {
        let lua = Lua::new();
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = Services::install(&lua, tracker).unwrap();
        assert!(
            lua.load("services.clipboard.set_text('outside')")
                .exec()
                .is_err()
        );

        services.allow_actions(true);
        lua.load("services.clipboard.set_text('hello'); services.clipboard.clear()")
            .exec()
            .unwrap();
        assert_eq!(
            services.take_actions(),
            vec![
                Action::SetClipboardText("hello".into()),
                Action::ClearClipboard,
            ]
        );

        let chunk = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES);
        lua.globals().set("chunk", chunk).unwrap();
        for _ in 0..4 {
            lua.load("services.clipboard.set_text(chunk)")
                .exec()
                .unwrap();
        }
        assert!(
            lua.load("services.clipboard.set_text(chunk)")
                .exec()
                .is_err()
        );
    }
}
