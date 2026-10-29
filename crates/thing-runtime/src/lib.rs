//! embedded luau configurations and dependency-tracked widget properties.
//!
//! a [`Runtime`] owns a loaded configuration and its state. it exposes validated
//! surface trees to the renderer.

mod modules;
mod niri;
mod properties;
mod reconcile;
mod services;
mod state;
mod timer;
mod transition;
pub use niri::{NiriAction, NiriState, NiriWindow, Workspace};
pub use services::{
    Action, ApplicationsSnapshot, AudioDevice, AudioSnapshot, AudioSource, AudioStream,
    AuthenticationSnapshot, AuthenticationState, ClipboardSnapshot, DesktopAction,
    DesktopApplication, MAX_LAUNCH_ARGUMENT_BYTES, MAX_LAUNCH_ARGUMENTS, MAX_LAUNCH_BYTES,
    MAX_MEDIA_RATE, MAX_MEDIA_TIME_US, ManagedProcessAction, ManagedProcessCommand,
    ManagedProcessDefinition, ManagedProcessShutdownSignal, ManagedProcessSnapshot,
    ManagedProcessState, ManagedProcessStatus, MediaAction, MediaPlayer, MediaPosition,
    MediaSnapshot, Notification, NotificationButton, NotificationSnapshot, NotificationUrgency,
    PowerDevice, PowerSnapshot, ServiceSnapshot, SystemTrayAction, SystemTrayItem, SystemTrayMenu,
    SystemTrayMenuAction, SystemTrayMenuItem, SystemTraySnapshot,
};
pub use timer::TimerToken;

use mlua::{Error, ErrorContext, Function, Lua, Result, Table, Value, VmState};
pub use state::Scalar;
use state::{Signal, Tracker};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
use thing_core::{Kind, Node, NodeId, Scale};

/// stable identity for one root surface in a loaded runtime.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SurfaceKey {
    Default,
    Named(String),
}

/// one configured surface and its compositor namespace.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceTree {
    pub key: SurfaceKey,
    pub namespace: String,
    pub root: Node,
}

/// the wayland output for one runtime, measured in logical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputContext {
    pub name: Option<String>,
    pub width: u32,
    pub height: u32,
    pub scale: Scale,
}

impl OutputContext {
    fn validate(&self) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self
                .name
                .as_ref()
                .is_some_and(|name| name.len() > 4096 || name.contains('\0'))
        {
            return Err(Error::runtime("invalid output context"));
        }
        Ok(())
    }
}

/// limits apply to each load, event callback, and batch of reactive updates.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory_bytes: usize,
    pub execution_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            execution_time: Duration::from_millis(250),
        }
    }
}

/// input events are separate from pure property functions.
#[derive(Clone, Debug)]
pub enum Event {
    Click,
    MiddleClick,
    SecondaryClick,
    /// scroll steps; positive values move right or down.
    Scroll {
        x: f64,
        y: f64,
    },
    Change(f64),
    Pressed(bool),
    TextChange(String),
    Submit(String),
    Hover(bool),
    Close,
}

/// one keyboard press delivered to a panel or popup handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub key: String,
    pub text: Option<String>,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
    pub repeated: bool,
}

/// result of a root keyboard handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyDispatch {
    pub handled: bool,
    pub changed: bool,
}

/// one phase of a captured custom pointer sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
    Press,
    Move,
    Release,
    Cancel,
}

/// a compositor pointer button exposed to a custom pointer handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Middle,
    Secondary,
}

/// logical coordinates and modifiers for a custom pointer callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerEvent {
    pub phase: PointerPhase,
    pub button: PointerButton,
    pub x: f64,
    pub y: f64,
    pub surface_x: f64,
    pub surface_y: f64,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

/// the observable result of dispatching one custom pointer callback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerDispatchOutcome {
    pub changed: bool,
    pub handled: bool,
    pub active: bool,
}

/// physical source reported for one compositor scroll frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollSource {
    Wheel,
    Finger,
    Continuous,
    WheelTilt,
    Unknown,
}

/// normalized scroll deltas, pointer position, and current modifiers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollInput {
    pub x: f64,
    pub y: f64,
    pub local_x: f64,
    pub local_y: f64,
    pub surface_x: f64,
    pub surface_y: f64,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
    pub source: ScrollSource,
}

/// the result of one custom scroll handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollDispatchOutcome {
    pub changed: bool,
    pub consumed: bool,
}

const MAX_UI_ACTIONS_PER_HANDLER: usize = 1024;

/// one widget identity captured by a local ui action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiTarget {
    node: NodeId,
    source: u64,
    lineage: Rc<[UiIdentity]>,
}

impl UiTarget {
    pub fn node(&self) -> NodeId {
        self.node
    }

    pub fn interactive_path_from(&self, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut within_surface = false;
        self.lineage.iter().filter_map(move |identity| {
            within_surface |= identity.node == root;
            (within_surface && identity.kind.is_interactive()).then_some(identity.node)
        })
    }
}

/// host-local actions produced by a successful event handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    Focus(UiTarget),
    Reveal(UiTarget),
}

impl UiAction {
    pub fn target(&self) -> &UiTarget {
        match self {
            Self::Focus(target) | Self::Reveal(target) => target,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UiIdentity {
    node: NodeId,
    source: u64,
    key: Option<Rc<str>>,
    kind: Kind,
}

fn ui_target_identity_matches(current: &UiTarget, previous: &UiTarget) -> bool {
    current.lineage.len() == previous.lineage.len()
        && current
            .lineage
            .iter()
            .zip(previous.lineage.iter())
            .all(|(current, previous)| {
                current.node == previous.node
                    && current.kind == previous.kind
                    && (current.source == previous.source
                        || previous
                            .key
                            .as_ref()
                            .is_some_and(|key| current.key.as_ref() == Some(key)))
            })
}

#[derive(Default)]
struct UiContext {
    action_start: Option<usize>,
    targets: BTreeMap<u64, UiTarget>,
    actions: Vec<UiAction>,
}

impl UiContext {
    fn queue(&mut self, source: u64, focus: bool) -> Result<()> {
        let Some(action_start) = self.action_start else {
            return Err(Error::runtime(
                "ui actions are available only inside event handlers",
            ));
        };
        if self.actions.len().saturating_sub(action_start) >= MAX_UI_ACTIONS_PER_HANDLER {
            return Err(Error::runtime("too many ui actions in one handler"));
        }
        let target =
            self.targets.get(&source).cloned().ok_or_else(|| {
                Error::runtime("ui action target is not in the current widget tree")
            })?;
        if focus
            && !target
                .lineage
                .last()
                .is_some_and(|identity| identity.kind.is_interactive())
        {
            return Err(Error::runtime("ui.focus target must be interactive"));
        }
        self.actions.push(if focus {
            UiAction::Focus(target)
        } else {
            UiAction::Reveal(target)
        });
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ActionCheckpoint {
    services: usize,
    ui: usize,
    timers: usize,
}

struct Binding {
    node: NodeId,
    property: String,
    function: Function,
    transition: Option<transition::State>,
}

type Handlers = BTreeMap<(NodeId, &'static str), Function>;

struct ActivePointer {
    target: UiTarget,
    handler: Function,
    last_event: PointerEvent,
    handled: bool,
}

#[derive(Clone, Copy)]
enum ConfigurationMode {
    Shell,
    Lock,
}

struct Children {
    nodes: Vec<Node>,
    bindings: BTreeMap<usize, Binding>,
    handlers: Handlers,
    sources: BTreeMap<NodeId, u64>,
}

/// a loaded configuration, its state, and the current surface trees.
pub struct Runtime {
    lua: Lua,
    surfaces: Vec<SurfaceTree>,
    tracker: Rc<RefCell<Tracker>>,
    bindings: BTreeMap<usize, Binding>,
    next_binding: usize,
    next_node: u64,
    sources: BTreeMap<NodeId, u64>,
    deferred: BTreeSet<usize>,
    handlers: Handlers,
    active_presses: BTreeMap<NodeId, Option<Function>>,
    active_pointer: Option<ActivePointer>,
    ended_hovers: BTreeSet<NodeId>,
    deadline: Rc<Cell<Instant>>,
    limits: Limits,
    modules: modules::Modules,
    services: services::Services,
    ui: Rc<RefCell<UiContext>>,
    timers: Rc<RefCell<timer::Timers>>,
    timer_event_depth: usize,
    output: Option<OutputContext>,
    mode: ConfigurationMode,
}

impl Runtime {
    /// load an entry file and its relative imports without connecting to wayland.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_with_limits(path, Limits::default())
    }

    pub fn load_with_limits(path: impl AsRef<Path>, limits: Limits) -> Result<Self> {
        Self::load_with_services(
            path.as_ref(),
            limits,
            ServiceSnapshot::default(),
            None,
            ConfigurationMode::Shell,
        )
    }

    /// load one session-lock surface without connecting to wayland.
    pub fn load_lock(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_with_services(
            path.as_ref(),
            Limits::default(),
            ServiceSnapshot::default(),
            None,
            ConfigurationMode::Lock,
        )
    }

    /// load an entry file for one output with current desktop service values.
    pub fn load_for_output(
        path: impl AsRef<Path>,
        output: OutputContext,
        services: &ServiceSnapshot,
    ) -> Result<Self> {
        output.validate()?;
        Self::load_with_services(
            path.as_ref(),
            Limits::default(),
            services.clone(),
            Some(output),
            ConfigurationMode::Shell,
        )
    }

    /// load one session-lock surface for an output with current service values.
    pub fn load_lock_for_output(
        path: impl AsRef<Path>,
        output: OutputContext,
        services: &ServiceSnapshot,
    ) -> Result<Self> {
        output.validate()?;
        Self::load_with_services(
            path.as_ref(),
            Limits::default(),
            services.clone(),
            Some(output),
            ConfigurationMode::Lock,
        )
    }

    fn load_with_services(
        path: &Path,
        limits: Limits,
        snapshot: ServiceSnapshot,
        output: Option<OutputContext>,
        mode: ConfigurationMode,
    ) -> Result<Self> {
        if limits.memory_bytes == 0 || limits.execution_time.is_zero() {
            return Err(Error::runtime("runtime limits must be positive"));
        }
        let lua = Lua::new();
        lua.set_memory_limit(limits.memory_bytes)?;
        let deadline = Rc::new(Cell::new(Instant::now() + limits.execution_time));
        let interrupt_deadline = deadline.clone();
        lua.set_interrupt(move |_| {
            if Instant::now() >= interrupt_deadline.get() {
                Err(Error::runtime("script exceeded its execution time limit"))
            } else {
                Ok(VmState::Continue)
            }
        });
        let tracker = Rc::new(RefCell::new(Tracker::default()));
        let services = services::Services::install(&lua, tracker.clone())?;
        for (name, value) in snapshot.values {
            services.set(&name, value)?;
        }
        services.set_devices(snapshot.devices)?;
        services.set_sources(snapshot.sources)?;
        services.set_all_sources(snapshot.all_sources)?;
        services.set_streams(snapshot.streams)?;
        services.set_media_players(snapshot.media_players)?;
        services.set_application_list(snapshot.applications)?;
        services.set_power_devices(snapshot.power_display, snapshot.power_devices)?;
        services.set_notification_records(snapshot.notifications)?;
        services.set_system_tray_items(snapshot.system_tray_items)?;
        services.set_managed_processes(ManagedProcessSnapshot {
            states: snapshot.managed_processes,
        })?;
        services.set_niri(snapshot.niri)?;
        if let Some(output) = &output {
            services.set_output(output)?;
        }
        let timers = Rc::new(RefCell::new(timer::Timers::default()));
        let ui = install_ui(&lua, tracker.clone(), &timers)?;
        lua.sandbox(true)?;
        let modules = modules::Modules::default();
        let exported = modules.load(&lua, path)?;
        if matches!(mode, ConfigurationMode::Lock)
            && services
                .managed_process_definitions()
                .iter()
                .any(|definition| definition.command.is_some())
        {
            return Err(Error::runtime(
                "scheduled processes are unavailable in lock configurations",
            ));
        }
        services.freeze_managed_process_declarations();
        timers.borrow_mut().freeze();
        let mut runtime = Self {
            lua,
            surfaces: Vec::new(),
            tracker,
            bindings: BTreeMap::new(),
            next_binding: 0,
            next_node: 1,
            sources: BTreeMap::new(),
            deferred: BTreeSet::new(),
            handlers: BTreeMap::new(),
            active_presses: BTreeMap::new(),
            active_pointer: None,
            ended_hovers: BTreeSet::new(),
            deadline,
            limits,
            modules,
            services,
            ui,
            timers,
            timer_event_depth: 0,
            output,
            mode,
        };
        runtime.surfaces = runtime.parse_surfaces(exported)?;
        let _ = runtime.sync_ui_targets();
        Ok(runtime)
    }

    /// arm autostart timers after the host accepts this runtime.
    pub fn activate_timers(&mut self, now: Instant) {
        self.timers.borrow_mut().activate(now);
    }

    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timers.borrow().next_deadline()
    }

    /// snapshot pending callbacks; rescheduling invalidates an older token.
    pub fn due_timers(&self, now: Instant) -> Vec<TimerToken> {
        self.timers.borrow().due(now)
    }

    /// run one due callback and validate its candidate surfaces before publication.
    pub fn dispatch_timer_checked(
        &mut self,
        token: &TimerToken,
        now: Instant,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<Option<bool>> {
        self.with_timer_event(Some(now), |runtime| {
            let handler = runtime.timers.borrow_mut().claim(token, now);
            let Some(handler) = handler else {
                return Ok(None);
            };
            let checkpoint = runtime.action_checkpoint();
            runtime.begin_call();
            runtime.allow_actions(true);
            if matches!(runtime.mode, ConfigurationMode::Lock) {
                runtime.services.allow_actions(false);
                runtime.ui.borrow_mut().action_start = None;
            }
            let handle = timer::Timers::handle(&runtime.timers, token);
            let result = handler.call::<()>(handle);
            runtime.allow_actions(false);
            let annotate = |error: Error| {
                let info = handler.info();
                error.context(format!(
                    "timer callback at {}:{}",
                    info.short_src.as_deref().unwrap_or("<unknown>"),
                    info.line_defined.unwrap_or(0)
                ))
            };
            if let Err(error) = result {
                runtime.truncate_actions(checkpoint);
                return Err(annotate(error));
            }
            let result = (|| {
                let changed = runtime.refresh_at_checked(now, &validate)?;
                let cleaned = runtime.cancel_unavailable_presses_checked(now, &validate)?;
                let pointer = runtime.reconcile_pointer_checked(&validate)?;
                Ok(changed || cleaned || pointer.changed)
            })();
            match result {
                Ok(changed) => {
                    runtime.timers.borrow_mut().repeat(token);
                    Ok(Some(changed))
                }
                Err(error) => {
                    runtime.truncate_actions(checkpoint);
                    runtime
                        .timers
                        .borrow_mut()
                        .repeat_after_rejection(token, now.max(Instant::now()));
                    Err(annotate(error))
                }
            }
        })
    }

    fn with_timer_event<T>(
        &mut self,
        now: Option<Instant>,
        callback: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let checkpoint = self.timers.borrow().checkpoint();
        self.timer_event_depth += 1;
        let result = callback(self);
        self.timer_event_depth -= 1;
        if result.is_err() {
            self.timers.borrow_mut().truncate(checkpoint);
        } else if self.timer_event_depth == 0 {
            self.timers
                .borrow_mut()
                .commit(now.map_or_else(Instant::now, |now| now.max(Instant::now())));
        }
        result
    }

    /// the first root, retained for callers that support one surface only.
    pub fn root(&self) -> Option<&Node> {
        self.surfaces.first().map(|surface| &surface.root)
    }

    pub fn surfaces(&self) -> &[SurfaceTree] {
        &self.surfaces
    }

    pub fn surface(&self, key: &SurfaceKey) -> Option<&SurfaceTree> {
        self.surfaces.iter().find(|surface| &surface.key == key)
    }

    /// dependency files referenced by the configuration, including its entry file.
    pub fn files(&self) -> Vec<PathBuf> {
        self.modules.files()
    }

    pub fn memory_used(&self) -> usize {
        self.lua.used_memory()
    }

    /// sets a service value. call [`Self::refresh`] after updating all fields.
    pub fn set_service(&mut self, name: &str, value: Scalar) -> Result<()> {
        self.services.set(name, value)
    }

    /// replaces the audio output list and marks its subscribers for update.
    pub fn set_audio_devices(&mut self, devices: Vec<AudioDevice>) -> Result<()> {
        self.services.set_devices(devices)
    }

    /// replaces the audio input list and marks its subscribers for update.
    pub fn set_audio_sources(&mut self, sources: Vec<AudioDevice>) -> Result<()> {
        self.services.set_sources(sources)
    }

    /// replaces the complete audio input list and marks its subscribers for update.
    pub fn set_all_audio_sources(&mut self, sources: Vec<AudioSource>) -> Result<()> {
        self.services.set_all_sources(sources)
    }

    /// replaces output and input audio state in one update. call [`Self::refresh`] afterward.
    pub fn set_audio(&mut self, audio: AudioSnapshot) -> Result<()> {
        self.services.set_audio(audio)
    }

    /// replaces the installed application list. call [`Self::refresh`] afterward.
    pub fn set_applications(&mut self, applications: ApplicationsSnapshot) -> Result<()> {
        self.services.set_applications(applications)
    }

    /// replaces selected media player state in one update. call [`Self::refresh`] afterward.
    pub fn set_media(&mut self, media: MediaSnapshot) -> Result<()> {
        self.services.set_media(media)
    }

    /// replaces read-only power state in one update. call [`Self::refresh`] afterward.
    pub fn set_power(&mut self, power: PowerSnapshot) -> Result<()> {
        self.services.set_power(power)
    }

    /// replaces notification-server state in one update. call [`Self::refresh`] afterward.
    pub fn set_notifications(&mut self, notifications: NotificationSnapshot) -> Result<()> {
        self.services.set_notifications(notifications)
    }

    /// replaces status notifier watcher and item state. call [`Self::refresh`] afterward.
    pub fn set_system_tray(&mut self, system_tray: SystemTraySnapshot) -> Result<()> {
        self.services.set_system_tray(system_tray)
    }

    /// replaces managed process state in one update. call [`Self::refresh`] afterward.
    pub fn set_managed_processes(&mut self, processes: ManagedProcessSnapshot) -> Result<()> {
        self.services.set_managed_processes(processes)
    }

    /// the managed process keys declared while this configuration loaded.
    pub fn managed_process_definitions(&self) -> Vec<ManagedProcessDefinition> {
        self.services.managed_process_definitions()
    }

    /// replaces clipboard state in one update. call [`Self::refresh`] afterward.
    pub fn set_clipboard(&mut self, clipboard: ClipboardSnapshot) -> Result<()> {
        self.services.set_clipboard(clipboard)
    }

    /// replaces read-only lock authentication state. call [`Self::refresh`] afterward.
    pub fn set_authentication(&mut self, authentication: AuthenticationSnapshot) -> Result<()> {
        self.services.set_authentication(authentication)
    }

    /// replaces workspace and window lists. call [`Self::refresh`] to update widgets.
    pub fn set_niri(&mut self, state: NiriState) -> Result<()> {
        self.services.set_niri(state)
    }

    /// replace all output properties together. call [`Self::refresh`] afterward.
    pub fn set_output(&mut self, output: OutputContext) -> Result<()> {
        self.services.set_output(&output)?;
        self.output = Some(output);
        Ok(())
    }

    /// takes the service actions queued by event handlers.
    pub fn take_actions(&mut self) -> Vec<Action> {
        self.services.take_actions()
    }

    /// takes local ui actions queued by the last successful event handler.
    pub fn take_ui_actions(&mut self) -> Vec<UiAction> {
        let actions = std::mem::take(&mut self.ui.borrow_mut().actions);
        let last_focus = actions
            .iter()
            .rposition(|action| matches!(action, UiAction::Focus(_)));
        actions
            .into_iter()
            .enumerate()
            .filter_map(|(index, action)| {
                if matches!(action, UiAction::Focus(_)) && Some(index) != last_focus {
                    return None;
                }
                self.ui_action_is_current(&action).then_some(action)
            })
            .collect()
    }

    pub fn ui_target(&self, id: NodeId) -> Option<UiTarget> {
        self.is_available(id)
            .then(|| {
                self.ui
                    .borrow()
                    .targets
                    .values()
                    .find(|target| target.node == id)
                    .cloned()
            })
            .flatten()
    }

    /// clones the current identities for a set of nodes in one tree scan.
    pub fn ui_targets(&self, ids: &BTreeSet<NodeId>) -> BTreeMap<NodeId, UiTarget> {
        self.ui
            .borrow()
            .targets
            .values()
            .filter(|target| ids.contains(&target.node))
            .map(|target| (target.node, target.clone()))
            .collect()
    }

    pub fn focus_target(&self, id: NodeId) -> Option<UiTarget> {
        self.find(id)
            .is_some_and(|node| node.kind.is_interactive())
            .then(|| self.ui_target(id))
            .flatten()
    }

    pub fn ui_target_is_current(&self, target: &UiTarget) -> bool {
        self.is_available(target.node) && self.ui_target_identity_is_current(target)
    }

    pub fn ui_target_identity_is_current(&self, target: &UiTarget) -> bool {
        let context = self.ui.borrow();
        let Some(current) = context
            .targets
            .values()
            .find(|current| current.node == target.node)
        else {
            return false;
        };
        ui_target_identity_matches(current, target)
    }

    pub fn ui_action_is_current(&self, action: &UiAction) -> bool {
        let target = action.target();
        self.ui_target_is_current(target)
            && (!matches!(action, UiAction::Focus(_))
                || self
                    .find(target.node)
                    .is_some_and(|node| node.kind.is_interactive()))
    }

    pub fn autofocus_target(&self, surface_root: NodeId) -> Option<UiTarget> {
        fn find(node: &Node) -> Option<NodeId> {
            if node.autofocus {
                return Some(node.id);
            }
            node.children
                .iter()
                .filter(|child| child.kind != Kind::Popup)
                .find_map(find)
        }

        let root = self.find(surface_root)?;
        find(root).and_then(|id| self.focus_target(id))
    }

    pub fn rendered_surface_root(&self, id: NodeId) -> Option<NodeId> {
        fn find(node: &Node, id: NodeId, surface_root: NodeId) -> Option<NodeId> {
            if node.id == id {
                return Some(surface_root);
            }
            node.children.iter().find_map(|child| {
                find(
                    child,
                    id,
                    if child.kind == Kind::Popup {
                        child.id
                    } else {
                        surface_root
                    },
                )
            })
        }

        self.surfaces
            .iter()
            .find_map(|surface| find(&surface.root, id, surface.root.id))
    }

    /// whether an available widget has an explicit scroll handler.
    pub fn has_scroll_handler(&self, id: NodeId) -> bool {
        self.is_available(id) && self.handlers.contains_key(&(id, "on_scroll"))
    }

    /// capture an available button or pointer area that has a scroll handler.
    pub fn scroll_target(&self, id: NodeId) -> Option<UiTarget> {
        let target = self.ui_target(id)?;
        self.scroll_target_is_current(&target).then_some(target)
    }

    /// whether a captured source and its keyed lineage still identify a scroll handler.
    pub fn scroll_target_is_current(&self, target: &UiTarget) -> bool {
        self.ui_target_is_current(target)
            && self.find(target.node).is_some_and(|node| {
                node.scroll_events && matches!(node.kind, Kind::Button | Kind::PointerArea)
            })
            && self.handlers.contains_key(&(target.node, "on_scroll"))
    }

    /// whether a captured source and its keyed lineage still identify a scroll viewport.
    pub fn scroll_view_target_is_current(&self, target: &UiTarget) -> bool {
        self.ui_target_is_current(target)
            && self
                .find(target.node)
                .is_some_and(|node| node.kind == Kind::Scroll)
    }

    /// capture an available button or pointer area that has a custom handler.
    pub fn pointer_target(&self, id: NodeId) -> Option<UiTarget> {
        let target = self.ui_target(id)?;
        self.pointer_target_is_current(&target).then_some(target)
    }

    /// whether the captured source and every keyed ancestor still identify a pointer target.
    pub fn pointer_target_is_current(&self, target: &UiTarget) -> bool {
        self.ui_target_is_current(target)
            && self.find(target.node).is_some_and(|node| {
                node.pointer_events && matches!(node.kind, Kind::Button | Kind::PointerArea)
            })
            && self.handlers.contains_key(&(target.node, "on_pointer"))
    }

    pub fn active_pointer_target(&self) -> Option<&UiTarget> {
        self.active_pointer
            .as_ref()
            .map(|sequence| &sequence.target)
    }

    pub fn active_pointer_is_current(&self) -> bool {
        self.active_pointer
            .as_ref()
            .is_some_and(|sequence| self.pointer_target_is_current(&sequence.target))
    }

    /// cancel a sequence whose live widget identity or availability changed.
    pub fn reconcile_pointer_checked(
        &mut self,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PointerDispatchOutcome> {
        self.with_timer_event(None, |runtime| {
            runtime.reconcile_pointer_checked_inner(validate)
        })
    }

    fn reconcile_pointer_checked_inner(
        &mut self,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PointerDispatchOutcome> {
        let Some(sequence) = &self.active_pointer else {
            return Ok(PointerDispatchOutcome::default());
        };
        if self.pointer_target_is_current(&sequence.target) {
            return Ok(PointerDispatchOutcome {
                changed: false,
                handled: sequence.handled,
                active: true,
            });
        }
        self.cancel_pointer_checked(validate)
    }

    /// start an implicitly captured custom pointer sequence.
    pub fn begin_pointer_checked(
        &mut self,
        target: &UiTarget,
        event: PointerEvent,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PointerDispatchOutcome> {
        self.with_timer_event(None, |runtime| {
            runtime.begin_pointer_checked_inner(target, event, validate)
        })
    }

    fn begin_pointer_checked_inner(
        &mut self,
        target: &UiTarget,
        event: PointerEvent,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PointerDispatchOutcome> {
        validate_pointer_event(event)?;
        if event.phase != PointerPhase::Press {
            return Err(Error::runtime("a pointer sequence must begin with press"));
        }
        if let Some(sequence) = &self.active_pointer {
            return Ok(PointerDispatchOutcome {
                changed: false,
                handled: sequence.handled,
                active: true,
            });
        }
        if !self.pointer_target_is_current(target) {
            return Ok(PointerDispatchOutcome::default());
        }
        let handler = self.handlers[&(target.node, "on_pointer")].clone();
        let checkpoint = self.action_checkpoint();
        self.active_pointer = Some(ActivePointer {
            target: target.clone(),
            handler: handler.clone(),
            last_event: event,
            handled: false,
        });
        let (changed, handled) = match self.call_pointer_checked(&handler, event, &validate) {
            Ok(result) => result,
            Err(error) => {
                self.active_pointer = None;
                return Err(error);
            }
        };
        if let Some(sequence) = &mut self.active_pointer {
            sequence.handled = handled;
        }
        let mut outcome = PointerDispatchOutcome {
            changed,
            handled,
            active: true,
        };
        if !self.active_pointer_is_current() {
            match self.reconcile_pointer_checked(&validate) {
                Ok(cancelled) => {
                    outcome.changed |= cancelled.changed;
                    outcome.handled |= cancelled.handled;
                    outcome.active = false;
                }
                Err(error) => {
                    self.truncate_actions(checkpoint);
                    return Err(error);
                }
            }
        }
        Ok(outcome)
    }

    /// deliver motion or release to the retained handler for a captured sequence.
    pub fn dispatch_pointer_checked(
        &mut self,
        event: PointerEvent,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PointerDispatchOutcome> {
        self.with_timer_event(None, |runtime| {
            runtime.dispatch_pointer_checked_inner(event, validate)
        })
    }

    fn dispatch_pointer_checked_inner(
        &mut self,
        event: PointerEvent,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PointerDispatchOutcome> {
        validate_pointer_event(event)?;
        if !matches!(event.phase, PointerPhase::Move | PointerPhase::Release) {
            return Err(Error::runtime(
                "a captured pointer sequence accepts move or release",
            ));
        }
        let Some(mut sequence) = self.active_pointer.take() else {
            return Ok(PointerDispatchOutcome::default());
        };
        if event.button != sequence.last_event.button {
            self.active_pointer = Some(sequence);
            return Err(Error::runtime(
                "pointer event button does not match the captured sequence",
            ));
        }
        if !self.pointer_target_is_current(&sequence.target) {
            self.active_pointer = Some(sequence);
            return self.reconcile_pointer_checked(&validate);
        }

        sequence.last_event = event;
        let checkpoint = self.action_checkpoint();
        let (changed, handled) = self.call_pointer_checked(&sequence.handler, event, &validate)?;
        sequence.handled |= handled;
        let mut outcome = PointerDispatchOutcome {
            changed,
            handled: sequence.handled,
            active: event.phase == PointerPhase::Move,
        };
        if event.phase == PointerPhase::Release {
            return Ok(outcome);
        }

        self.active_pointer = Some(sequence);
        if !self.active_pointer_is_current() {
            match self.reconcile_pointer_checked(&validate) {
                Ok(cancelled) => {
                    outcome.changed |= cancelled.changed;
                    outcome.handled |= cancelled.handled;
                    outcome.active = false;
                }
                Err(error) => {
                    self.truncate_actions(checkpoint);
                    return Err(error);
                }
            }
        }
        Ok(outcome)
    }

    /// end a captured sequence with one synthetic cancellation callback.
    pub fn cancel_pointer_checked(
        &mut self,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PointerDispatchOutcome> {
        self.with_timer_event(None, |runtime| {
            runtime.cancel_pointer_checked_inner(validate)
        })
    }

    fn cancel_pointer_checked_inner(
        &mut self,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<PointerDispatchOutcome> {
        let Some(sequence) = self.active_pointer.take() else {
            return Ok(PointerDispatchOutcome::default());
        };
        let event = PointerEvent {
            phase: PointerPhase::Cancel,
            ..sequence.last_event
        };
        let (changed, handled) = self.call_pointer_checked(&sequence.handler, event, &validate)?;
        Ok(PointerDispatchOutcome {
            changed,
            handled: sequence.handled || handled,
            active: false,
        })
    }

    fn call_pointer_checked(
        &mut self,
        handler: &Function,
        event: PointerEvent,
        validate: &impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<(bool, bool)> {
        let argument = self.lua.create_table()?;
        argument.set("phase", pointer_phase_name(event.phase))?;
        argument.set("button", pointer_button_name(event.button))?;
        argument.set("x", event.x)?;
        argument.set("y", event.y)?;
        argument.set("surface_x", event.surface_x)?;
        argument.set("surface_y", event.surface_y)?;
        argument.set("shift", event.shift)?;
        argument.set("ctrl", event.ctrl)?;
        argument.set("alt", event.alt)?;
        argument.set("logo", event.logo)?;
        argument.set_readonly(true);

        let checkpoint = self.action_checkpoint();
        self.begin_call();
        self.allow_actions(true);
        let result = handler.call::<Option<bool>>(argument);
        self.allow_actions(false);
        let result = result.and_then(|handled| {
            self.refresh_at_checked(Instant::now(), validate)
                .map(|changed| (changed, handled.unwrap_or(false)))
        });
        if result.is_err() {
            self.truncate_actions(checkpoint);
        }
        result
    }

    /// run a custom scroll handler captured from the presented widget tree.
    pub fn dispatch_scroll_checked(
        &mut self,
        target: &UiTarget,
        input: ScrollInput,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<Option<ScrollDispatchOutcome>> {
        self.with_timer_event(None, |runtime| {
            runtime.dispatch_scroll_checked_inner(target, input, validate)
        })
    }

    fn dispatch_scroll_checked_inner(
        &mut self,
        target: &UiTarget,
        input: ScrollInput,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<Option<ScrollDispatchOutcome>> {
        validate_scroll_input(input)?;
        if !self.scroll_target_is_current(target) {
            return Ok(None);
        }
        let handler = self.handlers[&(target.node, "on_scroll")].clone();
        self.call_scroll_checked(&handler, input, &validate)
            .map(Some)
    }

    fn call_scroll_checked(
        &mut self,
        handler: &Function,
        input: ScrollInput,
        validate: &impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<ScrollDispatchOutcome> {
        let event = self.lua.create_table()?;
        event.set("local_x", input.local_x)?;
        event.set("local_y", input.local_y)?;
        event.set("surface_x", input.surface_x)?;
        event.set("surface_y", input.surface_y)?;
        event.set("shift", input.shift)?;
        event.set("ctrl", input.ctrl)?;
        event.set("alt", input.alt)?;
        event.set("logo", input.logo)?;
        event.set("source", scroll_source_name(input.source))?;
        event.set_readonly(true);

        let checkpoint = self.action_checkpoint();
        self.begin_call();
        self.allow_actions(true);
        let result =
            handler
                .call::<Value>((input.x, input.y, event))
                .and_then(|value| match value {
                    Value::Nil => Ok(true),
                    Value::Boolean(consumed) => Ok(consumed),
                    _ => Err(Error::runtime("on_scroll must return a boolean or nil")),
                });
        self.allow_actions(false);
        let now = Instant::now();
        let result = result.and_then(|consumed| {
            self.refresh_at_checked(now, validate)
                .map(|changed| ScrollDispatchOutcome { changed, consumed })
        });
        if result.is_err() {
            self.truncate_actions(checkpoint);
        }
        let mut outcome = result?;
        match self.cancel_unavailable_presses_checked(now, validate) {
            Ok(cleaned) => outcome.changed |= cleaned,
            Err(error) => {
                self.truncate_actions(checkpoint);
                return Err(error);
            }
        }
        match self.reconcile_pointer_checked(validate) {
            Ok(pointer) => outcome.changed |= pointer.changed,
            Err(error) => {
                self.truncate_actions(checkpoint);
                return Err(error);
            }
        }
        Ok(outcome)
    }

    /// run an input handler. disabled or hidden widgets ignore input.
    pub fn dispatch(&mut self, id: NodeId, event: Event) -> Result<bool> {
        self.dispatch_checked(id, event, |_| Ok(()))
    }

    /// run an input handler and validate its candidate surface trees before publishing them.
    pub fn dispatch_checked(
        &mut self,
        id: NodeId,
        event: Event,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        self.with_timer_event(None, |runtime| {
            runtime.dispatch_checked_inner(id, event, validate)
        })
    }

    fn dispatch_checked_inner(
        &mut self,
        id: NodeId,
        event: Event,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        self.dispatch_inner_checked(id, event, None, &validate)
    }

    /// run a surface-root keyboard handler.
    pub fn dispatch_key(&mut self, id: NodeId, input: KeyInput) -> Result<Option<KeyDispatch>> {
        self.dispatch_key_checked(id, input, |_| Ok(()))
    }

    /// run a surface-root keyboard handler and validate its candidate trees.
    pub fn dispatch_key_checked(
        &mut self,
        id: NodeId,
        input: KeyInput,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<Option<KeyDispatch>> {
        self.with_timer_event(None, |runtime| {
            runtime.dispatch_key_checked_inner(id, input, validate)
        })
    }

    fn dispatch_key_checked_inner(
        &mut self,
        id: NodeId,
        input: KeyInput,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<Option<KeyDispatch>> {
        if !self.is_available(id) {
            return Ok(None);
        }
        let Some(handler) = self.handlers.get(&(id, "on_key")).cloned() else {
            return Ok(None);
        };
        self.begin_call();
        let event = self.lua.create_table()?;
        event.set("key", input.key.as_str())?;
        if let Some(text) = input.text.as_deref() {
            event.set("text", text)?;
        }
        event.set("shift", input.shift)?;
        event.set("ctrl", input.ctrl)?;
        event.set("alt", input.alt)?;
        event.set("logo", input.logo)?;
        event.set("repeated", input.repeated)?;
        event.set_readonly(true);
        let checkpoint = self.action_checkpoint();
        self.allow_actions(true);
        let result = handler.call::<Value>(event).and_then(|value| {
            let Value::Boolean(handled) = value else {
                return Err(Error::runtime("on_key must return a boolean"));
            };
            Ok(handled)
        });
        self.allow_actions(false);
        let now = Instant::now();
        let result = result.and_then(|handled| {
            self.refresh_at_checked(now, &validate)
                .map(|changed| KeyDispatch { handled, changed })
        });
        if result.is_err() {
            self.truncate_actions(checkpoint);
        }
        let mut result = result?;
        match self.cancel_unavailable_presses_checked(now, &validate) {
            Ok(cleaned) => result.changed |= cleaned,
            Err(error) => {
                self.truncate_actions(checkpoint);
                return Err(error);
            }
        }
        match self.reconcile_pointer_checked(&validate) {
            Ok(pointer) => result.changed |= pointer.changed,
            Err(error) => {
                self.truncate_actions(checkpoint);
                return Err(error);
            }
        }
        Ok(Some(result))
    }

    #[cfg(test)]
    fn dispatch_inner(&mut self, id: NodeId, event: Event, now: Option<Instant>) -> Result<bool> {
        self.dispatch_inner_checked(id, event, now, &|_| Ok(()))
    }

    fn dispatch_inner_checked(
        &mut self,
        id: NodeId,
        event: Event,
        now: Option<Instant>,
        validate: &impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        if let Event::Pressed(pressed) = event {
            let now = now.unwrap_or_else(Instant::now);
            return if pressed {
                self.begin_press_checked(id, now, validate)
            } else {
                self.end_press_checked(id, now, validate)
            };
        }
        if let Event::Scroll { x, y } = event {
            let Some(target) = self.scroll_target(id) else {
                return Ok(false);
            };
            let input = ScrollInput {
                x,
                y,
                local_x: 0.0,
                local_y: 0.0,
                surface_x: 0.0,
                surface_y: 0.0,
                shift: false,
                ctrl: false,
                alt: false,
                logo: false,
                source: ScrollSource::Unknown,
            };
            return self
                .dispatch_scroll_checked(&target, input, validate)
                .map(|outcome| outcome.is_some_and(|outcome| outcome.changed));
        }
        if matches!(&event, Event::Hover(false)) && self.ended_hovers.remove(&id) {
            return Ok(false);
        }
        if matches!(&event, Event::Hover(true)) && self.ended_hovers.contains(&id) {
            return Ok(false);
        }
        if !matches!(&event, Event::Close | Event::Hover(false)) && !self.is_available(id) {
            return Ok(false);
        }
        let hover_started = matches!(&event, Event::Hover(true));
        let name = match &event {
            Event::Click => "on_click",
            Event::MiddleClick => "on_middle_click",
            Event::SecondaryClick => "on_secondary_click",
            Event::Scroll { .. } => unreachable!("scroll events are handled above"),
            Event::Close => "on_close",
            Event::Hover(_) => "on_hover",
            Event::Change(value) if value.is_finite() => "on_change",
            Event::Change(_) => return Err(Error::runtime("input value must be finite")),
            Event::Pressed(_) => unreachable!("press events are handled above"),
            Event::TextChange(value) if valid_text_input_event(value) => "on_change",
            Event::TextChange(_) => return Err(Error::runtime("invalid text input value")),
            Event::Submit(value) if valid_text_input_event(value) => "on_submit",
            Event::Submit(_) => return Err(Error::runtime("invalid text input value")),
        };
        let Some(handler) = self.handlers.get(&(id, name)).cloned() else {
            return Ok(false);
        };
        self.begin_call();
        let checkpoint = self.action_checkpoint();
        self.allow_actions(true);
        let result = match event {
            Event::Click | Event::MiddleClick | Event::SecondaryClick | Event::Close => {
                handler.call::<()>(())
            }
            Event::Scroll { .. } => unreachable!("scroll events are handled above"),
            Event::Change(value) => handler.call::<()>(value),
            Event::TextChange(value) | Event::Submit(value) => handler.call::<()>(value),
            Event::Hover(inside) => handler.call::<()>(inside),
            Event::Pressed(_) => unreachable!("press events are handled above"),
        };
        self.allow_actions(false);
        let now = now.unwrap_or_else(Instant::now);
        let result = result.and_then(|()| self.refresh_at_checked(now, validate));
        let result = match result {
            Ok(changed)
                if hover_started
                    && (self.find(id).is_none()
                        || !self.handlers.contains_key(&(id, "on_hover"))) =>
            {
                self.ended_hovers.insert(id);
                self.begin_call();
                self.allow_actions(true);
                let cleanup = handler.call::<()>(false);
                self.allow_actions(false);
                cleanup
                    .and_then(|()| self.refresh_at_checked(now, validate))
                    .map(|cleanup_changed| changed || cleanup_changed)
            }
            result => result,
        };
        if result.is_err() {
            self.truncate_actions(checkpoint);
        }
        let changed = result?;
        let changed = match self.cancel_unavailable_presses_checked(now, validate) {
            Ok(cleaned) => changed || cleaned,
            Err(error) => {
                self.truncate_actions(checkpoint);
                return Err(error);
            }
        };
        match self.reconcile_pointer_checked(validate) {
            Ok(pointer) => Ok(changed || pointer.changed),
            Err(error) => {
                self.truncate_actions(checkpoint);
                Err(error)
            }
        }
    }

    fn begin_press_checked(
        &mut self,
        id: NodeId,
        now: Instant,
        validate: &impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        if self.active_presses.contains_key(&id) {
            return Ok(false);
        }
        if !self.is_available(id) || self.find(id).is_none_or(|node| node.kind != Kind::Range) {
            return Ok(false);
        }
        let handler = self.handlers.get(&(id, "on_pressed")).cloned();
        let checkpoint = self.action_checkpoint();
        let result = if let Some(handler) = &handler {
            self.begin_call();
            self.allow_actions(true);
            let result = handler.call::<()>(true);
            self.allow_actions(false);
            result.and_then(|()| self.refresh_at_checked(now, validate))
        } else {
            Ok(false)
        };
        let changed = match result {
            Ok(changed) => changed,
            Err(error) => {
                if let Some(handler) = handler {
                    self.begin_call();
                    self.allow_actions(true);
                    let cleanup = handler.call::<()>(false);
                    self.allow_actions(false);
                    let cleanup = cleanup.and_then(|()| self.refresh_at_checked(now, validate));
                    self.truncate_actions(checkpoint);
                    return match cleanup {
                        Ok(_) => Err(error),
                        Err(cleanup) => Err(Error::runtime(format!(
                            "{error}; press cleanup also failed: {cleanup}"
                        ))),
                    };
                }
                self.truncate_actions(checkpoint);
                return Err(error);
            }
        };
        self.active_presses.insert(id, handler);
        match self.cancel_unavailable_presses_checked(now, validate) {
            Ok(cleaned) => Ok(changed || cleaned),
            Err(error) => {
                self.truncate_actions(checkpoint);
                Err(error)
            }
        }
    }

    fn end_press_checked(
        &mut self,
        id: NodeId,
        now: Instant,
        validate: &impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        let Some(handler) = self.active_presses.remove(&id) else {
            return Ok(false);
        };
        let Some(handler) = handler else {
            return Ok(false);
        };
        let checkpoint = self.action_checkpoint();
        self.begin_call();
        self.allow_actions(true);
        let result = handler.call::<()>(false);
        self.allow_actions(false);
        let result = result.and_then(|()| self.refresh_at_checked(now, validate));
        if result.is_err() {
            self.truncate_actions(checkpoint);
        }
        result
    }

    fn cancel_unavailable_presses_checked(
        &mut self,
        now: Instant,
        validate: &impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        let mut changed = false;
        while let Some(id) = self
            .active_presses
            .keys()
            .copied()
            .find(|id| !self.is_available(*id))
        {
            changed |= self.end_press_checked(id, now, validate)?;
        }
        Ok(changed)
    }

    pub fn has_active_press(&self, id: NodeId) -> bool {
        self.active_presses.contains_key(&id)
    }

    pub fn active_press_is_available(&self, id: NodeId) -> bool {
        self.has_active_press(id) && self.is_available(id)
    }

    /// reevaluate only dirty properties and publish them together after validation.
    /// an error preserves the previous snapshot. a later state change can retry.
    pub fn refresh(&mut self) -> Result<bool> {
        self.refresh_checked(|_| Ok(()))
    }

    /// reevaluate dirty properties and validate candidate surfaces before publishing them.
    pub fn refresh_checked(
        &mut self,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        self.refresh_at_checked(Instant::now(), &validate)
    }

    fn refresh_at_checked(
        &mut self,
        now: Instant,
        validate: &impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        let mut dirty = std::mem::take(&mut self.tracker.borrow_mut().dirty);
        if dirty.is_empty() {
            return Ok(false);
        }
        dirty.append(&mut self.deferred);
        // retry a failed batch when another dependency changes. retaining the
        // whole batch prevents unrelated, valid changes from being lost.
        self.deferred = dirty.clone();
        self.begin_call();
        let mut candidate = self.surfaces.clone();
        let checkpoint = self.next_binding;
        let mut retired = BTreeSet::new();
        let mut removed = BTreeSet::new();
        let mut handlers = Handlers::new();
        let mut sources = self.sources.clone();
        let mut staged_transitions = BTreeMap::new();
        let mut ui_structure_changed = false;
        let result = (|| {
            for index in dirty {
                if retired.contains(&index) {
                    continue;
                }
                let value = self.evaluate_binding(index)?;
                let binding = &self.bindings[&index];
                let id = binding.node;
                let property = binding.property.clone();
                if property == "children" {
                    ui_structure_changed = true;
                    let (surface, depth) = candidate
                        .iter()
                        .enumerate()
                        .find_map(|(index, surface)| {
                            reconcile::depth(&surface.root, id).map(|depth| (index, depth))
                        })
                        .ok_or_else(|| Error::runtime("list refers to a missing widget"))?;
                    let node = candidate[surface]
                        .root
                        .find_mut(id)
                        .expect("depth located the widget");
                    let mut old_ids = BTreeSet::new();
                    for child in &node.children {
                        reconcile::ids(child, &mut old_ids);
                    }
                    let previous_transitions: BTreeMap<_, _> = self
                        .bindings
                        .iter()
                        .filter(|(_, binding)| old_ids.contains(&binding.node))
                        .filter_map(|(index, binding)| {
                            staged_transitions
                                .get(index)
                                .or(binding.transition.as_ref())
                                .map(|state| {
                                    ((binding.node, binding.property.clone()), state.clone())
                                })
                        })
                        .collect();
                    sources.retain(|id, _| !old_ids.contains(id));
                    let mut seen = sources.values().copied().collect();
                    let mut children = self.detached_children(value, depth + 1, &mut seen)?;
                    let mapping = reconcile::preserve(&node.children, &mut children.nodes);
                    for binding in children.bindings.values_mut() {
                        binding.node = mapping.get(&binding.node).copied().unwrap_or(binding.node);
                        let Some(initial) = binding.transition.as_ref() else {
                            continue;
                        };
                        let Some(previous) =
                            previous_transitions.get(&(binding.node, binding.property.clone()))
                        else {
                            continue;
                        };
                        binding.transition =
                            Some(transition::State::inherit(previous, initial, now));
                    }
                    let transition_values: Vec<_> = children
                        .bindings
                        .values()
                        .filter_map(|binding| {
                            binding.transition.as_ref().map(|state| {
                                (binding.node, binding.property.clone(), state.current())
                            })
                        })
                        .collect();
                    let apply_transitions = (|| {
                        for (id, property, value) in transition_values {
                            let child = children
                                .nodes
                                .iter_mut()
                                .find_map(|child| child.find_mut(id))
                                .ok_or_else(|| {
                                    Error::runtime("animation refers to a missing list widget")
                                })?;
                            properties::apply_animation_frame(child, &property, value)?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = apply_transitions {
                        for index in children.bindings.keys() {
                            self.tracker.borrow_mut().forget(*index);
                        }
                        return Err(error);
                    }
                    handlers.extend(children.handlers.into_iter().map(
                        |((id, event), function)| {
                            ((mapping.get(&id).copied().unwrap_or(id), event), function)
                        },
                    ));
                    sources.extend(
                        children
                            .sources
                            .into_iter()
                            .map(|(id, source)| (mapping.get(&id).copied().unwrap_or(id), source)),
                    );
                    retired.extend(self.bindings.iter().filter_map(|(index, binding)| {
                        old_ids.contains(&binding.node).then_some(*index)
                    }));
                    removed.extend(old_ids);
                    self.bindings.extend(children.bindings);
                    node.children = children.nodes;
                } else {
                    let node = candidate
                        .iter_mut()
                        .find_map(|surface| surface.root.find_mut(id))
                        .ok_or_else(|| {
                            Error::runtime("reactive binding refers to a missing widget")
                        })?;
                    if let Some(previous) = self.bindings[&index].transition.as_ref() {
                        let target = transition::target(value, &property)?;
                        let mut state = previous.clone();
                        state.retarget(target, now);
                        properties::apply_animation_frame(node, &property, state.current())?;
                        staged_transitions.insert(index, state);
                    } else {
                        properties::apply(node, &property, value)?;
                    }
                }
            }
            for surface in &candidate {
                surface.root.validate().map_err(Error::external)?;
            }
            let mut target = candidate.clone();
            for (index, binding) in &self.bindings {
                if retired.contains(index) {
                    continue;
                }
                let Some(state) = staged_transitions
                    .get(index)
                    .or(binding.transition.as_ref())
                else {
                    continue;
                };
                let node = target
                    .iter_mut()
                    .find_map(|surface| surface.root.find_mut(binding.node))
                    .ok_or_else(|| Error::runtime("animation refers to a missing widget"))?;
                properties::apply_transition(node, &binding.property, state.target())?;
            }
            for surface in &target {
                surface.root.validate().map_err(Error::external)?;
            }
            // target geometry must be representable before animation starts,
            // but the displayed candidate is validated last so a successful
            // host can retain and reuse that exact layout plan.
            if target != candidate {
                validate(&target).map_err(Error::runtime)?;
            }
            validate(&candidate).map_err(Error::runtime)?;
            Ok(())
        })();
        if let Err(error) = result {
            // only bindings made for the rejected candidate are discarded.
            // existing subscriptions and handlers continue serving the old ui.
            for index in self.bindings.split_off(&checkpoint).keys() {
                self.tracker.borrow_mut().forget(*index);
            }
            return Err(error);
        }
        for index in retired {
            self.bindings.remove(&index);
            self.tracker.borrow_mut().forget(index);
        }
        for (index, transition) in staged_transitions {
            self.bindings
                .get_mut(&index)
                .expect("a staged transition belongs to a live binding")
                .transition = Some(transition);
        }
        self.handlers.retain(|(id, _), _| !removed.contains(id));
        self.handlers.extend(handlers);
        self.sources = sources;
        let changed = candidate != self.surfaces;
        self.surfaces = candidate;
        let ui_identity_changed = ui_structure_changed && self.sync_ui_targets();
        self.deferred.clear();
        Ok(changed || ui_identity_changed || self.has_active_transitions())
    }

    /// advance active style animations to a monotonic point in time.
    pub fn advance(&mut self, now: Instant) -> Result<bool> {
        self.advance_checked(now, |_| Ok(()))
    }

    /// advance animations and validate the interpolated surfaces before publishing them.
    pub fn advance_checked(
        &mut self,
        now: Instant,
        validate: impl Fn(&[SurfaceTree]) -> std::result::Result<(), String>,
    ) -> Result<bool> {
        let mut candidate = self.surfaces.clone();
        let mut staged = BTreeMap::new();
        for (index, binding) in &self.bindings {
            let Some(previous) = binding
                .transition
                .as_ref()
                .filter(|state| state.is_active())
            else {
                continue;
            };
            let mut state = previous.clone();
            state.advance(now);
            let node = candidate
                .iter_mut()
                .find_map(|surface| surface.root.find_mut(binding.node))
                .ok_or_else(|| Error::runtime("animation refers to a missing widget"))?;
            properties::apply_animation_frame(node, &binding.property, state.current())?;
            staged.insert(*index, state);
        }
        if staged.is_empty() {
            return Ok(false);
        }
        for surface in &candidate {
            surface.root.validate().map_err(Error::external)?;
        }
        validate(&candidate).map_err(Error::runtime)?;
        for (index, transition) in staged {
            self.bindings
                .get_mut(&index)
                .expect("an active animation belongs to a live binding")
                .transition = Some(transition);
        }
        let changed = candidate != self.surfaces;
        self.surfaces = candidate;
        Ok(changed)
    }

    /// whether a widget has an active style animation.
    pub fn is_animating(&self, id: NodeId) -> bool {
        self.bindings.values().any(|binding| {
            binding.node == id
                && binding
                    .transition
                    .as_ref()
                    .is_some_and(transition::State::is_active)
        })
    }

    /// whether any widget in a set has an active style animation.
    pub fn is_any_animating(&self, ids: impl IntoIterator<Item = NodeId>) -> bool {
        let active: BTreeSet<_> = self
            .bindings
            .values()
            .filter_map(|binding| {
                binding
                    .transition
                    .as_ref()
                    .is_some_and(transition::State::is_active)
                    .then_some(binding.node)
            })
            .collect();
        ids.into_iter().any(|id| active.contains(&id))
    }

    fn has_active_transitions(&self) -> bool {
        self.bindings.values().any(|binding| {
            binding
                .transition
                .as_ref()
                .is_some_and(transition::State::is_active)
        })
    }

    /// load a replacement without changing the current runtime.
    pub fn load_replacement(&self, path: impl AsRef<Path>) -> Result<Self> {
        let mut services = self.services.snapshot();
        services.set_managed_processes(ManagedProcessSnapshot::default())?;
        Self::load_with_services(
            path.as_ref(),
            self.limits,
            services,
            self.output.clone(),
            self.mode,
        )
    }

    /// reload the configuration, preserving it if loading or validation fails.
    pub fn reload(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let mut replacement = self.load_replacement(path)?;
        if self.timers.borrow().is_activated() {
            replacement.activate_timers(Instant::now());
        }
        *self = replacement;
        Ok(())
    }

    fn begin_call(&self) {
        self.deadline
            .set(Instant::now() + self.limits.execution_time);
    }

    fn action_checkpoint(&self) -> ActionCheckpoint {
        ActionCheckpoint {
            services: self.services.action_count(),
            ui: self.ui.borrow().actions.len(),
            timers: self.timers.borrow().checkpoint(),
        }
    }

    fn allow_actions(&self, allowed: bool) {
        self.services.allow_actions(allowed);
        self.timers.borrow_mut().allow_controls(allowed);
        let mut ui = self.ui.borrow_mut();
        ui.action_start = allowed.then_some(ui.actions.len());
    }

    fn truncate_actions(&self, checkpoint: ActionCheckpoint) {
        self.services.truncate_actions(checkpoint.services);
        self.ui.borrow_mut().actions.truncate(checkpoint.ui);
        self.timers.borrow_mut().truncate(checkpoint.timers);
    }

    fn sync_ui_targets(&self) -> bool {
        fn collect(
            node: &Node,
            sources: &BTreeMap<NodeId, u64>,
            lineage: &mut Vec<UiIdentity>,
            targets: &mut BTreeMap<u64, UiTarget>,
        ) {
            let Some(source) = sources.get(&node.id).copied() else {
                return;
            };
            lineage.push(UiIdentity {
                node: node.id,
                source,
                key: node.key.as_deref().map(Rc::from),
                kind: node.kind,
            });
            targets.insert(
                source,
                UiTarget {
                    node: node.id,
                    source,
                    lineage: Rc::from(lineage.as_slice()),
                },
            );
            for child in &node.children {
                collect(child, sources, lineage, targets);
            }
            lineage.pop();
        }

        let mut targets = BTreeMap::new();
        for surface in &self.surfaces {
            collect(&surface.root, &self.sources, &mut Vec::new(), &mut targets);
        }
        let mut context = self.ui.borrow_mut();
        let identity_changed = context.targets.values().any(|previous| {
            self.sources
                .get(&previous.node)
                .and_then(|source| targets.get(source))
                .is_none_or(|current| !ui_target_identity_matches(current, previous))
        });
        context.targets = targets;
        identity_changed
    }

    fn evaluate_binding(&self, index: usize) -> Result<Value> {
        self.tracker.borrow_mut().begin(index);
        let result = self.bindings[&index].function.call::<Value>(());
        self.tracker.borrow_mut().finish(index);
        result
    }

    fn parse_surfaces(&mut self, exported: Value) -> Result<Vec<SurfaceTree>> {
        let Value::Table(table) = exported else {
            return Err(Error::runtime(match self.mode {
                ConfigurationMode::Shell => {
                    "the entry file must return ui.panel { ... } or a dense panel array"
                }
                ConfigurationMode::Lock => "the lock entry file must return ui.lock { ... }",
            }));
        };
        let single = !table.raw_get::<Value>("_kind")?.is_nil();
        if matches!(self.mode, ConfigurationMode::Lock) && !single {
            return Err(Error::runtime(
                "the lock entry file must return exactly one ui.lock { ... }",
            ));
        }
        let roots = if single {
            vec![table]
        } else {
            dense_root_tables(table)?
        };
        let mut seen_widgets = BTreeSet::new();
        let mut seen_keys = BTreeSet::new();
        let mut surfaces = Vec::with_capacity(roots.len());
        for table in roots {
            if matches!(self.mode, ConfigurationMode::Lock)
                && (!table.raw_get::<Value>("key")?.is_nil()
                    || !table.raw_get::<Value>("namespace")?.is_nil())
            {
                return Err(Error::runtime(
                    "lock surfaces do not accept key or namespace",
                ));
            }
            let key = if matches!(self.mode, ConfigurationMode::Lock) {
                SurfaceKey::Default
            } else {
                surface_key(&table, !single)?
            };
            if !seen_keys.insert(key.clone()) {
                return Err(Error::runtime(format!("duplicate surface key {key:?}")));
            }
            let namespace = if matches!(self.mode, ConfigurationMode::Lock) {
                "thing-lock".into()
            } else {
                surface_namespace(&table, &key)?
            };
            let root = self.parse_node(Value::Table(table), 0, &mut seen_widgets)?;
            let expected = match self.mode {
                ConfigurationMode::Shell => Kind::Panel,
                ConfigurationMode::Lock => Kind::Lock,
            };
            if root.kind != expected {
                return Err(Error::runtime(match self.mode {
                    ConfigurationMode::Shell => "every surface root must be ui.panel { ... }",
                    ConfigurationMode::Lock => "the lock surface root must be ui.lock { ... }",
                }));
            }
            root.validate().map_err(Error::external)?;
            surfaces.push(SurfaceTree {
                key,
                namespace,
                root,
            });
        }
        Ok(surfaces)
    }

    fn parse_node(&mut self, value: Value, depth: usize, seen: &mut BTreeSet<u64>) -> Result<Node> {
        if depth > 64 || seen.len() >= 10_000 {
            return Err(Error::runtime(
                "widget tree exceeds 64 levels or 10,000 nodes",
            ));
        }
        let Value::Table(table) = value else {
            return Err(Error::runtime("expected a widget returned by ui.*"));
        };
        let source: u64 = table.raw_get("_id")?;
        if !seen.insert(source) {
            return Err(Error::runtime(
                "a widget is reused or the tree contains a cycle",
            ));
        }
        let kind = kind(&table.raw_get::<String>("_kind")?)?;
        if kind == Kind::TextInput {
            if table.raw_get::<Value>("value")?.is_nil() {
                return Err(Error::runtime("text_input requires value"));
            }
            if table.raw_get::<Value>("on_change")?.is_nil() {
                return Err(Error::runtime("text_input requires on_change"));
            }
        }
        if kind == Kind::PointerArea
            && table.raw_get::<Value>("on_pointer")?.is_nil()
            && table.raw_get::<Value>("on_scroll")?.is_nil()
        {
            return Err(Error::runtime(
                "pointer_area requires on_pointer or on_scroll",
            ));
        }
        if depth == 0
            && kind == Kind::Panel
            && !table.raw_get::<Value>("edge")?.is_nil()
            && !table.raw_get::<Value>("anchors")?.is_nil()
        {
            return Err(Error::runtime(
                "a root panel cannot contain both edge and anchors",
            ));
        }
        let id = NodeId(self.next_node);
        self.next_node = self
            .next_node
            .checked_add(1)
            .ok_or_else(|| Error::runtime("too many widget identities"))?;
        self.sources.insert(id, source);
        let mut node = Node::new(id, kind);
        for pair in table.clone().pairs::<String, Value>() {
            let (key, value) = pair?;
            match key.as_str() {
                "_id" | "_kind" | "children" => {}
                "key" | "namespace" if depth == 0 => {}
                "edge" | "anchors" | "layer" | "keyboard_mode" | "margins" | "exclusive_zone"
                | "input_region"
                    if depth > 0 =>
                {
                    return Err(Error::runtime(format!(
                        "{key} is only supported on the root panel"
                    )));
                }
                "key" => node.key = Some(properties::string(value, "key")?),
                "autofocus" => {
                    let Value::Boolean(value) = value else {
                        return Err(Error::runtime("autofocus must be a static boolean"));
                    };
                    node.autofocus = value;
                }
                "on_click" | "on_middle_click" | "on_secondary_click" | "on_scroll"
                | "on_change" | "on_pressed" | "on_submit" | "on_hover" | "on_close" | "on_key"
                | "on_pointer" => {
                    if (matches!(
                        key.as_str(),
                        "on_click" | "on_middle_click" | "on_secondary_click"
                    ) && kind != Kind::Button)
                        || (key == "on_scroll" && !matches!(kind, Kind::Button | Kind::PointerArea))
                        || (key == "on_change"
                            && !matches!(kind, Kind::Slider | Kind::Range | Kind::TextInput))
                        || (key == "on_pressed" && kind != Kind::Range)
                        || (key == "on_submit" && kind != Kind::TextInput)
                        || (key == "on_hover" && kind != Kind::Popup && !kind.is_interactive())
                        || (key == "on_close" && kind != Kind::Popup)
                        || (key == "on_key"
                            && !matches!(kind, Kind::Panel | Kind::Popup | Kind::Lock))
                        || (key == "on_pointer"
                            && !matches!(kind, Kind::Button | Kind::PointerArea))
                    {
                        return Err(Error::runtime(format!(
                            "{key} is not supported on {kind:?}"
                        )));
                    }
                    let Value::Function(function) = value else {
                        return Err(Error::runtime(format!("{key} must be a function")));
                    };
                    let name = match key.as_str() {
                        "on_click" => "on_click",
                        "on_middle_click" => "on_middle_click",
                        "on_secondary_click" => "on_secondary_click",
                        "on_scroll" => "on_scroll",
                        "on_change" => "on_change",
                        "on_pressed" => "on_pressed",
                        "on_submit" => "on_submit",
                        "on_hover" => "on_hover",
                        "on_close" => "on_close",
                        "on_key" => "on_key",
                        "on_pointer" => "on_pointer",
                        _ => unreachable!("event name was accepted above"),
                    };
                    if key == "on_pointer" {
                        node.pointer_events = true;
                    } else if key == "on_scroll" {
                        node.scroll_events = true;
                    }
                    self.handlers.insert((node.id, name), function);
                }
                "style" => {
                    let Value::Table(style) = value else {
                        return Err(Error::runtime("style must be a table"));
                    };
                    for pair in style.pairs::<String, Value>() {
                        let (property, value) = pair?;
                        self.property(&mut node, format!("style.{property}"), value)?;
                    }
                }
                _ => self.property(&mut node, key, value)?,
            }
        }
        let children = table.raw_get::<Value>("children")?;
        if !matches!(children, Value::Nil) {
            let children = if let Value::Function(function) = children {
                let index = self.bind(node.id, "children".into(), function)?;
                self.evaluate_binding(index)?
            } else {
                children
            };
            node.children = self.parse_children(children, depth + 1, seen)?;
        }
        Ok(node)
    }

    fn property(&mut self, node: &mut Node, property: String, value: Value) -> Result<()> {
        let descriptor = match &value {
            Value::UserData(value) => value
                .borrow::<transition::Descriptor>()
                .ok()
                .map(|descriptor| descriptor.parts()),
            _ => None,
        };
        let value = if let Some((function, spec)) = descriptor {
            if !spec.supports(&property) {
                return Err(Error::runtime(format!(
                    "{} are not supported for {property}",
                    spec.name()
                )));
            }
            let index = self.bind(node.id, property.clone(), function)?;
            let value = self.evaluate_binding(index)?;
            let target = transition::target(value, &property)?;
            self.bindings
                .get_mut(&index)
                .expect("the transition binding was just inserted")
                .transition = Some(transition::State::new(spec, target));
            properties::apply_transition(node, &property, target)?;
            return Ok(());
        } else if let Value::Function(function) = value {
            let index = self.bind(node.id, property.clone(), function)?;
            self.evaluate_binding(index)?
        } else {
            value
        };
        properties::apply(node, &property, value)
    }

    fn bind(&mut self, node: NodeId, property: String, function: Function) -> Result<usize> {
        let index = self.next_binding;
        self.next_binding = index
            .checked_add(1)
            .ok_or_else(|| Error::runtime("too many bindings"))?;
        self.bindings.insert(
            index,
            Binding {
                node,
                property,
                function,
                transition: None,
            },
        );
        Ok(index)
    }

    fn parse_children(
        &mut self,
        value: Value,
        depth: usize,
        seen: &mut BTreeSet<u64>,
    ) -> Result<Vec<Node>> {
        let Value::Table(children) = value else {
            return Err(Error::runtime("children must be a dense array of widgets"));
        };
        let count = children.clone().pairs::<Value, Value>().count();
        if count != children.raw_len() {
            return Err(Error::runtime("children must be a dense array of widgets"));
        }
        (1..=count)
            .map(|index| self.parse_node(children.raw_get(index)?, depth, seen))
            .collect()
    }

    fn detached_children(
        &mut self,
        value: Value,
        depth: usize,
        seen: &mut BTreeSet<u64>,
    ) -> Result<Children> {
        // stage callback registrations so invalid rows cannot replace a working handler.
        let bindings = std::mem::take(&mut self.bindings);
        let handlers = std::mem::take(&mut self.handlers);
        let sources = std::mem::take(&mut self.sources);
        let result = self.parse_children(value, depth, seen);
        let bindings = std::mem::replace(&mut self.bindings, bindings);
        let handlers = std::mem::replace(&mut self.handlers, handlers);
        let sources = std::mem::replace(&mut self.sources, sources);
        match result {
            Ok(nodes) => Ok(Children {
                nodes,
                bindings,
                handlers,
                sources,
            }),
            Err(error) => {
                for index in bindings.keys() {
                    self.tracker.borrow_mut().forget(*index);
                }
                Err(error)
            }
        }
    }

    fn find(&self, id: NodeId) -> Option<&Node> {
        self.surfaces
            .iter()
            .find_map(|surface| surface.root.find(id))
    }

    fn is_available(&self, id: NodeId) -> bool {
        self.surfaces
            .iter()
            .any(|surface| is_available(&surface.root, id))
    }
}

fn dense_root_tables(table: Table) -> Result<Vec<Table>> {
    let length = table.raw_len();
    if length > 64 {
        return Err(Error::runtime("an entry may return at most 64 surfaces"));
    }
    let entries = table
        .clone()
        .pairs::<Value, Value>()
        .collect::<Result<Vec<_>>>()?;
    if entries.len() != length
        || !entries.iter().all(|(key, _)| {
            matches!(key, Value::Integer(index) if *index >= 1 && (*index as usize) <= length)
        })
    {
        return Err(Error::runtime(
            "the entry surface list must be a dense array",
        ));
    }
    (1..=length)
        .map(|index| match table.raw_get(index)? {
            Value::Table(root) => Ok(root),
            _ => Err(Error::runtime(
                "every surface root must be ui.panel { ... }",
            )),
        })
        .collect()
}

fn surface_key(table: &Table, required: bool) -> Result<SurfaceKey> {
    match static_root_string(table, "key")? {
        Some(key) => Ok(SurfaceKey::Named(key)),
        None if required => Err(Error::runtime(
            "every panel in a surface list needs a nonempty static key",
        )),
        None => Ok(SurfaceKey::Default),
    }
}

fn surface_namespace(table: &Table, key: &SurfaceKey) -> Result<String> {
    if let Some(namespace) = static_root_string(table, "namespace")? {
        return Ok(namespace);
    }
    Ok(match key {
        SurfaceKey::Default => "thing".into(),
        SurfaceKey::Named(key) => format!("thing-{key}"),
    })
}

fn static_root_string(table: &Table, name: &str) -> Result<Option<String>> {
    match table.raw_get::<Value>(name)? {
        Value::Nil => Ok(None),
        Value::String(value) => {
            let value = value.to_str()?.to_owned();
            validate_surface_string(&value, name)?;
            Ok(Some(value))
        }
        Value::Function(_) => Err(Error::runtime(format!(
            "surface {name} must be a static string"
        ))),
        _ => Err(Error::runtime(format!("surface {name} must be a string"))),
    }
}

fn validate_surface_string(value: &str, name: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || value.contains('\0') {
        return Err(Error::runtime(format!(
            "surface {name} must be 1 to 256 bytes and contain no NUL"
        )));
    }
    Ok(())
}

fn valid_text_input_event(value: &str) -> bool {
    value.len() <= 1024 * 1024
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
}

fn validate_pointer_event(event: PointerEvent) -> Result<()> {
    if [event.x, event.y, event.surface_x, event.surface_y]
        .into_iter()
        .all(f64::is_finite)
    {
        Ok(())
    } else {
        Err(Error::runtime("pointer coordinates must be finite"))
    }
}

fn validate_scroll_input(input: ScrollInput) -> Result<()> {
    if [
        input.x,
        input.y,
        input.local_x,
        input.local_y,
        input.surface_x,
        input.surface_y,
    ]
    .into_iter()
    .all(f64::is_finite)
    {
        Ok(())
    } else {
        Err(Error::runtime(
            "scroll deltas and coordinates must be finite",
        ))
    }
}

fn scroll_source_name(source: ScrollSource) -> &'static str {
    match source {
        ScrollSource::Wheel => "wheel",
        ScrollSource::Finger => "finger",
        ScrollSource::Continuous => "continuous",
        ScrollSource::WheelTilt => "wheel_tilt",
        ScrollSource::Unknown => "unknown",
    }
}

fn pointer_phase_name(phase: PointerPhase) -> &'static str {
    match phase {
        PointerPhase::Press => "press",
        PointerPhase::Move => "move",
        PointerPhase::Release => "release",
        PointerPhase::Cancel => "cancel",
    }
}

fn pointer_button_name(button: PointerButton) -> &'static str {
    match button {
        PointerButton::Primary => "primary",
        PointerButton::Middle => "middle",
        PointerButton::Secondary => "secondary",
    }
}

fn is_available(node: &Node, id: NodeId) -> bool {
    node.visible
        && node.enabled
        && (node.id == id || node.children.iter().any(|n| is_available(n, id)))
}

fn kind(name: &str) -> Result<Kind> {
    match name {
        "panel" => Ok(Kind::Panel),
        "lock" => Ok(Kind::Lock),
        "popup" => Ok(Kind::Popup),
        "row" => Ok(Kind::Row),
        "centerbox" => Ok(Kind::CenterBox),
        "column" => Ok(Kind::Column),
        "stack" => Ok(Kind::Stack),
        "scroll" => Ok(Kind::Scroll),
        "text" => Ok(Kind::Text),
        "text_input" => Ok(Kind::TextInput),
        "secret_input" => Ok(Kind::SecretInput),
        "image" => Ok(Kind::Image),
        "path" => Ok(Kind::Path),
        "button" => Ok(Kind::Button),
        "pointer_area" => Ok(Kind::PointerArea),
        "slider" => Ok(Kind::Slider),
        "range" => Ok(Kind::Range),
        _ => Err(Error::runtime(format!("unknown widget {name:?}"))),
    }
}

fn install_ui(
    lua: &Lua,
    tracker: Rc<RefCell<Tracker>>,
    timers: &Rc<RefCell<timer::Timers>>,
) -> Result<Rc<RefCell<UiContext>>> {
    let ui = lua.create_table()?;
    timer::Timers::install(timers, lua, &ui)?;
    let context = Rc::new(RefCell::new(UiContext::default()));
    let next_id = Rc::new(Cell::new(1_u64));
    for name in [
        "panel",
        "lock",
        "popup",
        "row",
        "centerbox",
        "column",
        "stack",
        "scroll",
        "text",
        "text_input",
        "secret_input",
        "image",
        "path",
        "button",
        "pointer_area",
        "slider",
        "range",
    ] {
        let next_id = next_id.clone();
        ui.set(
            name,
            lua.create_function(move |lua, props: Table| {
                let result = lua.create_table()?;
                for pair in props.pairs::<Value, Value>() {
                    let (key, value) = pair?;
                    result.raw_set(key, value)?;
                }
                let id = next_id.get();
                next_id.set(
                    id.checked_add(1)
                        .ok_or_else(|| Error::runtime("too many widgets"))?,
                );
                result.raw_set("_id", id)?;
                result.raw_set("_kind", name)?;
                Ok(result)
            })?,
        )?;
    }
    ui.set(
        "state",
        lua.create_function(move |_, value: Value| {
            let value = Scalar::from_lua(value)?;
            let id = tracker.borrow_mut().insert(value)?;
            Ok(Signal {
                id,
                tracker: tracker.clone(),
                readonly: false,
            })
        })?,
    )?;
    ui.set(
        "transition",
        lua.create_function(|_, (function, options): (Function, Table)| {
            transition::Descriptor::transition(function, options)
        })?,
    )?;
    ui.set(
        "spring",
        lua.create_function(|_, (function, options): (Function, Table)| {
            transition::Descriptor::spring(function, options)
        })?,
    )?;
    for (name, focus) in [("focus", true), ("reveal", false)] {
        let context = context.clone();
        ui.set(
            name,
            lua.create_function(move |_, widget: Table| {
                let source = widget
                    .raw_get::<u64>("_id")
                    .map_err(|_| Error::runtime("ui action target must be a widget"))?;
                context.borrow_mut().queue(source, focus)
            })?,
        )?;
    }
    ui.set_readonly(true);
    lua.globals().set("ui", ui)?;
    Ok(context)
}

#[cfg(test)]
mod ui_action_tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn load(source: &str) -> (TempDir, Runtime) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(path, source).unwrap();
        let runtime = Runtime::load(directory.path().join("panel.luau")).unwrap();
        (directory, runtime)
    }

    #[test]
    fn ui_action_limit_applies_to_each_handler() {
        let source = 1;
        let identity = UiIdentity {
            node: NodeId(1),
            source,
            key: None,
            kind: Kind::Button,
        };
        let mut context = UiContext::default();
        context.targets.insert(
            source,
            UiTarget {
                node: identity.node,
                source,
                lineage: vec![identity].into(),
            },
        );

        context.action_start = Some(0);
        for _ in 0..MAX_UI_ACTIONS_PER_HANDLER {
            context.queue(source, false).unwrap();
        }
        assert!(context.queue(source, false).is_err());

        context.action_start = Some(context.actions.len());
        assert!(context.queue(source, false).is_ok());
    }

    #[test]
    fn ui_actions_require_handlers_and_focusable_targets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(
            &path,
            "local target = ui.button {}; ui.focus(target); return ui.panel {}",
        )
        .unwrap();
        assert!(
            Runtime::load(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("only inside event handlers")
        );

        let (_directory, mut runtime) = load(
            r#"
            local target = ui.text {}
            return ui.panel { children = {
                ui.button { on_click = function() ui.focus(target) end },
                target,
            } }
            "#,
        );
        let trigger = runtime.root().unwrap().children[0].id;
        assert!(
            runtime
                .dispatch(trigger, Event::Click)
                .unwrap_err()
                .to_string()
                .contains("must be interactive")
        );
        assert!(runtime.take_ui_actions().is_empty());
    }

    #[test]
    fn successful_handlers_publish_reveal_and_only_the_last_focus() {
        let (_directory, mut runtime) = load(
            r#"
            local first = ui.button { key = 'first' }
            local second = ui.text_input {
                key = 'second',
                value = '',
                on_change = function() end,
            }
            return ui.panel { children = {
                ui.button { on_click = function()
                    ui.focus(first)
                    ui.reveal(first)
                    ui.focus(second)
                end },
                first,
                second,
            } }
            "#,
        );
        let root = runtime.root().unwrap();
        let trigger = root.children[0].id;
        let first = root.children[1].id;
        let second = root.children[2].id;

        assert!(!runtime.dispatch(trigger, Event::Click).unwrap());
        let actions = runtime.take_ui_actions();
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], UiAction::Reveal(target) if target.node() == first));
        assert!(matches!(&actions[1], UiAction::Focus(target) if target.node() == second));
    }

    #[test]
    fn rejected_refreshes_discard_local_actions() {
        let (_directory, mut runtime) = load(
            r#"
            local invalid = ui.state(false)
            local target = ui.button {}
            return ui.panel { children = {
                ui.button { on_click = function()
                    ui.focus(target)
                    invalid:set(true)
                end },
                target,
                ui.text { text = function()
                    return invalid:get() and 42 or 'valid'
                end },
            } }
            "#,
        );
        let trigger = runtime.root().unwrap().children[0].id;
        assert!(runtime.dispatch(trigger, Event::Click).is_err());
        assert!(runtime.take_ui_actions().is_empty());
        assert_eq!(runtime.root().unwrap().children[2].text, "valid");
    }

    #[test]
    fn property_only_refreshes_check_target_availability_live() {
        let (_directory, mut runtime) = load(
            r#"
            local enabled = ui.state(true)
            local target = ui.button {
                enabled = function() return enabled:get() end,
            }
            return ui.panel { children = {
                ui.button { on_click = function()
                    ui.focus(target)
                    enabled:set(false)
                end },
                target,
            } }
            "#,
        );
        let trigger = runtime.root().unwrap().children[0].id;

        assert!(runtime.dispatch(trigger, Event::Click).unwrap());
        assert!(runtime.take_ui_actions().is_empty());
    }

    fn replacement_fixture(key: &str) -> (TempDir, Runtime, NodeId) {
        let source = format!(
            r#"
            local replace = ui.state(false)
            local target = nil
            local list = ui.column {{ children = function()
                local changed = replace:get()
                target = ui.button {{ {key} }}
                return {{ target }}
            end }}
            return ui.panel {{ children = {{
                ui.button {{ on_click = function()
                    ui.focus(target)
                    replace:set(true)
                end }},
                list,
            }} }}
            "#,
        );
        let (directory, runtime) = load(&source);
        let target = runtime.root().unwrap().children[1].children[0].id;
        (directory, runtime, target)
    }

    #[test]
    fn stale_unkeyed_handles_do_not_target_positional_replacements() {
        let (_directory, mut runtime, target) = replacement_fixture("");
        let trigger = runtime.root().unwrap().children[0].id;
        assert!(runtime.dispatch(trigger, Event::Click).unwrap());
        assert_eq!(runtime.root().unwrap().children[1].children[0].id, target);
        assert!(runtime.take_ui_actions().is_empty());
    }

    #[test]
    fn keyed_replacements_preserve_queued_target_identity() {
        let (_directory, mut runtime, target) = replacement_fixture("key = 'row',");
        let trigger = runtime.root().unwrap().children[0].id;
        assert!(!runtime.dispatch(trigger, Event::Click).unwrap());
        let actions = runtime.take_ui_actions();
        assert!(matches!(actions.as_slice(), [UiAction::Focus(action)] if action.node() == target));
    }

    fn ancestor_replacement_fixture(key: &str) -> (TempDir, Runtime, NodeId) {
        let source = format!(
            r#"
            local replace = ui.state(false)
            local target = ui.button {{ key = 'target' }}
            local list = ui.column {{ children = function()
                local changed = replace:get()
                return {{ ui.row {{ {key} children = {{ target }} }} }}
            end }}
            return ui.panel {{ children = {{
                ui.button {{ on_click = function()
                    ui.focus(target)
                    replace:set(true)
                end }},
                list,
            }} }}
            "#,
        );
        let (directory, runtime) = load(&source);
        let target = runtime.root().unwrap().children[1].children[0].children[0].id;
        (directory, runtime, target)
    }

    #[test]
    fn replaced_ancestors_require_their_own_key_continuity() {
        let (_directory, mut runtime, target) = ancestor_replacement_fixture("");
        let trigger = runtime.root().unwrap().children[0].id;
        assert!(runtime.dispatch(trigger, Event::Click).unwrap());
        assert_eq!(
            runtime.root().unwrap().children[1].children[0].children[0].id,
            target
        );
        assert!(runtime.take_ui_actions().is_empty());

        let (_directory, mut runtime, target) = ancestor_replacement_fixture("key = 'branch',");
        let trigger = runtime.root().unwrap().children[0].id;
        assert!(!runtime.dispatch(trigger, Event::Click).unwrap());
        assert!(matches!(
            runtime.take_ui_actions().as_slice(),
            [UiAction::Focus(action)] if action.node() == target
        ));
    }

    #[test]
    fn removed_targets_discard_queued_actions() {
        let (_directory, mut runtime) = load(
            r#"
            local shown = ui.state(true)
            local target = nil
            local list = ui.column { children = function()
                if not shown:get() then return {} end
                target = ui.button { key = 'row' }
                return { target }
            end }
            return ui.panel { children = {
                ui.button { on_click = function()
                    ui.focus(target)
                    shown:set(false)
                end },
                list,
            } }
            "#,
        );
        let trigger = runtime.root().unwrap().children[0].id;
        assert!(runtime.dispatch(trigger, Event::Click).unwrap());
        assert!(runtime.take_ui_actions().is_empty());
    }

    #[test]
    fn autofocus_and_surface_roots_stop_at_popup_boundaries() {
        let (_directory, runtime) = load(
            r#"
            local panel_target = ui.button { autofocus = true }
            local popup_target = ui.text_input {
                autofocus = true,
                value = '',
                on_change = function() end,
            }
            local popup = ui.popup { children = { popup_target } }
            return ui.panel { children = { panel_target, popup } }
            "#,
        );
        let root = runtime.root().unwrap();
        let panel_target = root.children[0].id;
        let popup = root.children[1].id;
        let popup_target = root.children[1].children[0].id;

        assert_eq!(
            runtime.autofocus_target(root.id).unwrap().node(),
            panel_target
        );
        assert_eq!(
            runtime.autofocus_target(popup).unwrap().node(),
            popup_target
        );
        assert_eq!(runtime.rendered_surface_root(panel_target), Some(root.id));
        assert_eq!(runtime.rendered_surface_root(popup_target), Some(popup));
    }

    #[test]
    fn unavailable_targets_keep_their_identity_for_input_cleanup() {
        let (_directory, mut runtime) = load(
            r#"
            local shown = ui.state(true)
            return ui.panel { children = {
                ui.button {
                    key = 'hide',
                    on_click = function() shown:set(false) end,
                },
                ui.range {
                    key = 'target',
                    visible = function() return shown:get() end,
                    on_pressed = function() end,
                },
            } }
            "#,
        );
        let hide = runtime.root().unwrap().children[0].id;
        let target = runtime.root().unwrap().children[1].id;
        let target = runtime.ui_target(target).unwrap();

        runtime.dispatch(hide, Event::Click).unwrap();

        assert!(!runtime.ui_target_is_current(&target));
        assert!(runtime.ui_target_identity_is_current(&target));
    }

    #[test]
    fn interactive_paths_stop_at_the_rendered_surface_root() {
        let (_directory, runtime) = load(
            r#"
            return ui.panel { children = { ui.button {
                key = 'anchor',
                children = { ui.popup { children = { ui.button {
                    key = 'popup-target',
                } } } },
            } } }
            "#,
        );
        let root = runtime.root().unwrap();
        let anchor = root.children[0].id;
        let popup = root.children[0].children[0].id;
        let target = root.children[0].children[0].children[0].id;
        let target = runtime.ui_target(target).unwrap();

        assert_eq!(
            target.interactive_path_from(root.id).collect::<Vec<_>>(),
            [anchor, target.node()],
        );
        assert_eq!(
            target.interactive_path_from(popup).collect::<Vec<_>>(),
            [target.node()],
        );
    }

    #[test]
    fn autofocus_is_static() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(
            &path,
            "return ui.panel { children = { ui.button { autofocus = function() return true end } } }",
        )
        .unwrap();
        assert!(
            Runtime::load(path)
                .err()
                .unwrap()
                .to_string()
                .contains("static boolean")
        );
    }
}

#[cfg(test)]
mod managed_process_tests {
    use std::{fs, sync::Arc};

    use super::*;

    fn process(status: ManagedProcessStatus, stdout: &str) -> ManagedProcessState {
        ManagedProcessState {
            key: "query".into(),
            status,
            stdout: Arc::from(stdout),
            stderr: Arc::from(""),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: None,
            exit_signal: None,
            error: Arc::from(""),
        }
    }

    #[test]
    fn process_declarations_freeze_after_load_and_replacements_start_idle() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(
            &path,
            r#"
            local query = services.process('query')
            return ui.panel { children = {
                ui.text { text = function()
                    return query.status:get() .. ':' .. query.stdout:get()
                end },
                ui.button { on_click = function()
                    services.process('late')
                end },
            } }
            "#,
        )
        .unwrap();
        let mut services = ServiceSnapshot::default();
        services
            .set_managed_processes(ManagedProcessSnapshot {
                states: vec![process(ManagedProcessStatus::Running, "ready")],
            })
            .unwrap();
        let mut runtime = Runtime::load_for_output(
            &path,
            OutputContext {
                name: Some("test".into()),
                width: 100,
                height: 40,
                scale: Scale::default(),
            },
            &services,
        )
        .unwrap();

        assert_eq!(
            runtime.managed_process_definitions(),
            vec![ManagedProcessDefinition {
                key: "query".into(),
                shutdown_signal: ManagedProcessShutdownSignal::Terminate,
                command: None,
            }]
        );
        assert_eq!(runtime.root().unwrap().children[0].text, "running:ready");
        let button = runtime.root().unwrap().children[1].id;
        assert!(runtime.dispatch(button, Event::Click).is_err());

        let replacement = runtime.load_replacement(&path).unwrap();
        assert_eq!(replacement.root().unwrap().children[0].text, "idle:");
    }

    #[test]
    fn process_snapshot_updates_reactive_properties_together() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(
            &path,
            r#"
            local query = services.process('query')
            return ui.panel { children = { ui.text { text = function()
                return query.status:get() .. ':' .. query.stdout:get()
            end } } }
            "#,
        )
        .unwrap();
        let mut runtime = Runtime::load(&path).unwrap();
        runtime
            .set_managed_processes(ManagedProcessSnapshot {
                states: vec![process(ManagedProcessStatus::Exited, "complete")],
            })
            .unwrap();

        assert!(runtime.refresh().unwrap());
        assert_eq!(runtime.root().unwrap().children[0].text, "exited:complete");
    }
}

#[cfg(test)]
mod transition_tests {
    use std::{cell::Cell, fs, time::Duration};

    use tempfile::TempDir;
    use thing_core::Color;

    use super::*;

    fn load(source: &str) -> (TempDir, PathBuf, Runtime) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path, source).unwrap();
        let runtime = Runtime::load(&path).unwrap();
        (directory, path, runtime)
    }

    fn child_with_key<'a>(node: &'a Node, key: &str) -> &'a Node {
        fn find<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
            if node.key.as_deref() == Some(key) {
                return Some(node);
            }
            node.children.iter().find_map(|child| find(child, key))
        }
        find(node, key).unwrap()
    }

    fn root(runtime: &Runtime) -> &Node {
        runtime.root().unwrap()
    }

    fn surface<'a>(runtime: &'a Runtime, key: &str) -> &'a Node {
        &runtime
            .surface(&SurfaceKey::Named(key.into()))
            .unwrap()
            .root
    }

    fn close(left: f32, right: f32) {
        assert!((left - right).abs() < 0.001, "{left} != {right}");
    }

    #[test]
    fn numeric_transitions_advance_and_retarget_without_jumping() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local expanded = ui.state(false)
            return ui.panel { children = { ui.button {
                on_click = function() expanded:set(not expanded:get()) end,
                style = {
                    width = ui.transition(function()
                        return expanded:get() and 200 or 100
                    end, { duration = 100, easing = 'linear' }),
                    opacity = ui.transition(function()
                        return expanded:get() and 1 or 0.5
                    end, { duration = 100, easing = 'linear' }),
                },
            } } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        close(root(&runtime).children[0].style.width.unwrap(), 100.0);
        close(root(&runtime).children[0].style.opacity, 0.5);

        let start = Instant::now();
        assert!(
            runtime
                .dispatch_inner(button, Event::Click, Some(start))
                .unwrap()
        );
        close(root(&runtime).children[0].style.width.unwrap(), 100.0);
        assert!(runtime.is_animating(button));

        assert!(runtime.advance(start + Duration::from_millis(40)).unwrap());
        close(root(&runtime).children[0].style.width.unwrap(), 140.0);
        close(root(&runtime).children[0].style.opacity, 0.7);

        assert!(
            runtime
                .dispatch_inner(
                    button,
                    Event::Click,
                    Some(start + Duration::from_millis(40)),
                )
                .unwrap()
        );
        close(root(&runtime).children[0].style.width.unwrap(), 140.0);

        runtime.advance(start + Duration::from_millis(90)).unwrap();
        close(root(&runtime).children[0].style.width.unwrap(), 120.0);
        close(root(&runtime).children[0].style.opacity, 0.6);
        runtime.advance(start + Duration::from_millis(140)).unwrap();
        close(root(&runtime).children[0].style.width.unwrap(), 100.0);
        close(root(&runtime).children[0].style.opacity, 0.5);
        assert!(!runtime.is_animating(button));
    }

    #[test]
    fn spring_overshoot_is_clamped_without_ending_internal_motion() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local moving = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() moving:set(true) end },
                ui.row { style = {
                    width = ui.spring(function()
                        return moving:get() and 0 or 100
                    end, { stiffness = 100, damping = 0.1, epsilon = 0.001 }),
                    opacity = ui.spring(function()
                        return moving:get() and 1 or 0
                    end, { stiffness = 100, damping = 0.1, epsilon = 0.001 }),
                } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let animated = root(&runtime).children[1].id;
        let start = Instant::now();

        runtime
            .dispatch_inner(button, Event::Click, Some(start))
            .unwrap();
        assert!(runtime.advance(start + Duration::from_millis(300)).unwrap());
        close(root(&runtime).children[1].style.width.unwrap(), 0.0);
        close(root(&runtime).children[1].style.opacity, 1.0);
        assert!(runtime.is_animating(animated));

        assert!(runtime.advance(start + Duration::from_millis(600)).unwrap());
        assert!(root(&runtime).children[1].style.width.unwrap() > 0.0);
        assert!(root(&runtime).children[1].style.opacity < 1.0);

        runtime.advance(start + Duration::from_secs(60)).unwrap();
        close(root(&runtime).children[1].style.width.unwrap(), 0.0);
        close(root(&runtime).children[1].style.opacity, 1.0);
        assert!(!runtime.is_animating(animated));
    }

    #[test]
    fn very_stiff_springs_do_not_snap_after_a_finite_step() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local moved = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() moved:set(true) end },
                ui.row { style = { width = ui.spring(function()
                    return moved:get() and 100 or 0
                end, { stiffness = 1e308, damping = 1e-154 }) } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let animated = root(&runtime).children[1].id;
        let start = Instant::now();

        runtime
            .dispatch_inner(button, Event::Click, Some(start))
            .unwrap();
        runtime.advance(start + Duration::from_millis(1)).unwrap();

        assert!(runtime.is_animating(animated));
        assert!(root(&runtime).children[1].style.width.is_some());
    }

    #[test]
    fn extremely_overdamped_springs_do_not_snap_after_a_finite_step() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local moved = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() moved:set(true) end },
                ui.row { style = { width = ui.spring(function()
                    return moved:get() and 100 or 0
                end, { stiffness = 1, damping = 5e306 }) } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let animated = root(&runtime).children[1].id;
        let start = Instant::now();

        runtime
            .dispatch_inner(button, Event::Click, Some(start))
            .unwrap();
        runtime.advance(start + Duration::from_millis(1)).unwrap();

        assert!(runtime.is_animating(animated));
        close(root(&runtime).children[1].style.width.unwrap(), 0.0);
    }

    #[test]
    fn invalid_spring_targets_are_rejected_instead_of_clamped() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local invalid = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() invalid:set(true) end },
                ui.row { style = { opacity = ui.spring(function()
                    return invalid:get() and 2 or 0.5
                end, { stiffness = 170, damping = 0.8 }) } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;

        assert!(runtime.dispatch(button, Event::Click).is_err());
        close(root(&runtime).children[1].style.opacity, 0.5);
        assert!(!runtime.is_animating(root(&runtime).children[1].id));
    }

    #[test]
    fn rejected_spring_frames_do_not_advance_internal_state() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local expanded = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() expanded:set(true) end },
                ui.row { style = { width = ui.spring(function()
                    return expanded:get() and 200 or 100
                end, { stiffness = 100, damping = 0.8 }) } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let animated = root(&runtime).children[1].id;
        let start = Instant::now();
        runtime
            .dispatch_inner(button, Event::Click, Some(start))
            .unwrap();

        let reject_motion = |surfaces: &[SurfaceTree]| {
            (surfaces[0].root.children[1].style.width == Some(100.0))
                .then_some(())
                .ok_or_else(|| "animated width is unsupported".into())
        };
        assert!(
            runtime
                .advance_checked(start + Duration::from_millis(100), reject_motion)
                .is_err()
        );
        close(root(&runtime).children[1].style.width.unwrap(), 100.0);
        assert!(runtime.is_animating(animated));

        assert!(runtime.advance(start + Duration::from_millis(100)).unwrap());
        let width = root(&runtime).children[1].style.width.unwrap();
        assert!(width > 100.0 && width < 200.0);
    }

    #[test]
    fn keyed_children_keep_spring_velocity_when_retargeted() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local width = ui.state(100)
            local revision = ui.state(0)
            local function spring_width()
                return ui.spring(function() return width:get() end, {
                    stiffness = 100,
                    damping = 0.5,
                    epsilon = 0.001,
                })
            end
            return ui.panel { children = {
                ui.button {
                    key = 'start',
                    on_click = function() width:set(200) end,
                },
                ui.button {
                    key = 'reverse',
                    on_click = function()
                        width:set(100)
                        revision:set(revision:get() + 1)
                    end,
                },
                ui.row { key = 'control', style = { width = spring_width() } },
                ui.column { children = function()
                    local _ = revision:get()
                    return { ui.row {
                        key = 'stable',
                        style = { width = spring_width() },
                    } }
                end },
            } }
            "#,
        );
        let start_button = child_with_key(root(&runtime), "start").id;
        let reverse_button = child_with_key(root(&runtime), "reverse").id;
        let stable = child_with_key(root(&runtime), "stable").id;
        let start = Instant::now();

        runtime
            .dispatch_inner(start_button, Event::Click, Some(start))
            .unwrap();
        runtime.advance(start + Duration::from_millis(100)).unwrap();
        runtime
            .dispatch_inner(
                reverse_button,
                Event::Click,
                Some(start + Duration::from_millis(100)),
            )
            .unwrap();
        assert_eq!(child_with_key(root(&runtime), "stable").id, stable);

        runtime.advance(start + Duration::from_millis(150)).unwrap();
        close(
            child_with_key(root(&runtime), "control")
                .style
                .width
                .unwrap(),
            child_with_key(root(&runtime), "stable")
                .style
                .width
                .unwrap(),
        );
    }

    #[test]
    fn color_transitions_advance_and_retarget_without_jumping() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local color = ui.state('#000000')
            return ui.panel { children = {
                ui.button {
                    key = 'white',
                    on_click = function() color:set('#ffffff') end,
                },
                ui.button {
                    key = 'black',
                    on_click = function() color:set('#000000') end,
                },
                ui.row {
                    key = 'animated',
                    style = { background = ui.transition(function()
                        return color:get()
                    end, { duration = 100, easing = 'linear' }) },
                },
            } }
            "#,
        );
        let white = child_with_key(root(&runtime), "white").id;
        let black = child_with_key(root(&runtime), "black").id;
        let animated = child_with_key(root(&runtime), "animated").id;
        let start = Instant::now();

        runtime
            .dispatch_inner(white, Event::Click, Some(start))
            .unwrap();
        runtime.advance(start + Duration::from_millis(40)).unwrap();
        assert_eq!(
            child_with_key(root(&runtime), "animated").style.background,
            Color([170, 170, 170, 255])
        );

        runtime
            .dispatch_inner(black, Event::Click, Some(start + Duration::from_millis(40)))
            .unwrap();
        assert_eq!(
            child_with_key(root(&runtime), "animated").style.background,
            Color([170, 170, 170, 255])
        );
        runtime.advance(start + Duration::from_millis(90)).unwrap();
        assert_eq!(
            child_with_key(root(&runtime), "animated").style.background,
            Color([124, 124, 124, 255])
        );
        runtime.advance(start + Duration::from_millis(140)).unwrap();
        assert_eq!(
            child_with_key(root(&runtime), "animated").style.background,
            Color([0, 0, 0, 255])
        );
        assert!(!runtime.is_animating(animated));
    }

    #[test]
    fn invalid_color_targets_preserve_the_previous_snapshot_and_recover() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local color = ui.state('#112233')
            return ui.panel { children = {
                ui.button {
                    key = 'invalid',
                    on_click = function() color:set('red') end,
                },
                ui.button {
                    key = 'recover',
                    on_click = function() color:set('#44556680') end,
                },
                ui.row {
                    key = 'animated',
                    style = { background = ui.transition(function()
                        return color:get()
                    end, { duration = 0 }) },
                },
            } }
            "#,
        );
        let invalid = child_with_key(root(&runtime), "invalid").id;
        let recover = child_with_key(root(&runtime), "recover").id;
        let animated = child_with_key(root(&runtime), "animated").id;

        assert!(runtime.dispatch(invalid, Event::Click).is_err());
        assert_eq!(
            child_with_key(root(&runtime), "animated").style.background,
            Color([0x11, 0x22, 0x33, 0xff])
        );
        assert!(!runtime.is_animating(animated));

        assert!(runtime.dispatch(recover, Event::Click).unwrap());
        assert_eq!(
            child_with_key(root(&runtime), "animated").style.background,
            Color([0x44, 0x55, 0x66, 0x80])
        );
        assert!(!runtime.is_animating(animated));
    }

    #[test]
    fn shared_state_advances_transitions_across_surface_roots_together() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local expanded = ui.state(false)
            local function width()
                return expanded:get() and 200 or 100
            end
            return {
                ui.panel { key = 'first', children = {
                    ui.button { on_click = function() expanded:set(true) end },
                    ui.row { style = { width = ui.transition(width, {
                        duration = 100,
                        easing = 'linear',
                    }) } },
                } },
                ui.panel { key = 'second', children = {
                    ui.row { style = { width = ui.transition(width, {
                        duration = 100,
                        easing = 'linear',
                    }) } },
                } },
            }
            "#,
        );
        let button = surface(&runtime, "first").children[0].id;
        let first = surface(&runtime, "first").children[1].id;
        let second = surface(&runtime, "second").children[0].id;
        let start = Instant::now();

        assert!(
            runtime
                .dispatch_inner(button, Event::Click, Some(start))
                .unwrap()
        );
        assert!(runtime.is_animating(first));
        assert!(runtime.is_animating(second));
        assert!(runtime.advance(start + Duration::from_millis(40)).unwrap());
        close(
            surface(&runtime, "first").children[1].style.width.unwrap(),
            140.0,
        );
        close(
            surface(&runtime, "second").children[0].style.width.unwrap(),
            140.0,
        );
    }

    #[test]
    fn invalid_transition_target_preserves_every_surface_root() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local invalid = ui.state(false)
            return {
                ui.panel { key = 'first', children = {
                    ui.button { on_click = function() invalid:set(true) end },
                    ui.row { style = { width = ui.transition(function()
                        return invalid:get() and 200 or 100
                    end, { duration = 100 }) } },
                } },
                ui.panel { key = 'second', children = {
                    ui.row { style = { opacity = ui.transition(function()
                        return invalid:get() and 2 or 0.5
                    end, { duration = 100 }) } },
                } },
            }
            "#,
        );
        let button = surface(&runtime, "first").children[0].id;
        let first = surface(&runtime, "first").children[1].id;
        let second = surface(&runtime, "second").children[0].id;

        assert!(runtime.dispatch(button, Event::Click).is_err());
        close(
            surface(&runtime, "first").children[1].style.width.unwrap(),
            100.0,
        );
        close(surface(&runtime, "second").children[0].style.opacity, 0.5);
        assert!(!runtime.is_animating(first));
        assert!(!runtime.is_animating(second));
    }

    #[test]
    fn checked_refresh_rejects_a_transition_target_before_publication() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local expanded = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() expanded:set(true) end },
                ui.row { style = { width = ui.transition(function()
                    return expanded:get() and 200 or 100
                end, { duration = 100, easing = 'linear' }) } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let animated = root(&runtime).children[1].id;
        let start = Instant::now();
        let reject_target = |surfaces: &[SurfaceTree]| {
            let width = surfaces[0].root.children[1].style.width.unwrap();
            if (width - 200.0).abs() < 0.001 {
                Err("target width is unsupported".into())
            } else {
                Ok(())
            }
        };

        assert!(
            runtime
                .dispatch_inner_checked(button, Event::Click, Some(start), &reject_target)
                .is_err()
        );
        close(root(&runtime).children[1].style.width.unwrap(), 100.0);
        assert!(!runtime.is_animating(animated));
    }

    #[test]
    fn checked_refresh_validates_the_displayed_frame_last_before_cache_commit() {
        let source = r#"
            local expanded = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() expanded:set(true) end },
                ui.row { style = { width = ui.transition(function()
                    return expanded:get() and 200 or 100
                end, { duration = 100, easing = 'linear' }) } },
            } }
        "#;
        let (_directory, _path, mut runtime) = load(source);
        let button = root(&runtime).children[0].id;
        let last_plan = Cell::new(77.0_f32);
        let candidate_plan = Cell::new(None);
        let reject_displayed = |surfaces: &[SurfaceTree]| {
            let width = surfaces[0].root.children[1].style.width.unwrap();
            candidate_plan.set(Some(width));
            if (width - 100.0).abs() < 0.001 {
                Err("displayed frame is unsupported".into())
            } else {
                Ok(())
            }
        };
        let failed = runtime.dispatch_inner_checked(
            button,
            Event::Click,
            Some(Instant::now()),
            &reject_displayed,
        );
        if failed.is_ok() {
            last_plan.set(candidate_plan.get().unwrap());
        }
        assert!(failed.is_err());
        close(last_plan.get(), 77.0);
        close(root(&runtime).children[1].style.width.unwrap(), 100.0);

        let (_directory, _path, mut runtime) = load(source);
        let button = root(&runtime).children[0].id;
        let validations = std::cell::RefCell::new(Vec::new());
        let result = runtime.dispatch_inner_checked(
            button,
            Event::Click,
            Some(Instant::now()),
            &|surfaces: &[SurfaceTree]| {
                let width = surfaces[0].root.children[1].style.width.unwrap();
                validations.borrow_mut().push(width);
                candidate_plan.set(Some(width));
                Ok(())
            },
        );
        if result.is_ok() {
            last_plan.set(candidate_plan.get().unwrap());
        }
        assert!(result.unwrap());
        assert_eq!(&*validations.borrow(), &[200.0, 100.0]);
        close(last_plan.get(), 100.0);
        close(root(&runtime).children[1].style.width.unwrap(), 100.0);
    }

    #[test]
    fn checked_advance_rejects_an_intermediate_frame_without_advancing_state() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local expanded = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() expanded:set(true) end },
                ui.row { style = { width = ui.transition(function()
                    return expanded:get() and 200 or 100
                end, { duration = 100, easing = 'linear' }) } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let animated = root(&runtime).children[1].id;
        let start = Instant::now();
        let endpoints_only = |surfaces: &[SurfaceTree]| {
            let width = surfaces[0].root.children[1].style.width.unwrap();
            if width > 100.001 && width < 199.999 {
                Err("intermediate width is unsupported".into())
            } else {
                Ok(())
            }
        };

        assert!(
            runtime
                .dispatch_inner_checked(button, Event::Click, Some(start), &endpoints_only)
                .unwrap()
        );
        assert!(runtime.is_animating(animated));
        assert!(
            runtime
                .advance_checked(start + Duration::from_millis(50), endpoints_only)
                .is_err()
        );
        close(root(&runtime).children[1].style.width.unwrap(), 100.0);
        assert!(runtime.is_animating(animated));

        assert!(
            runtime
                .advance_checked(start + Duration::from_millis(100), endpoints_only)
                .unwrap()
        );
        close(root(&runtime).children[1].style.width.unwrap(), 200.0);
        assert!(!runtime.is_animating(animated));
    }

    #[test]
    fn keyed_children_keep_their_transition_segment() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local expanded = ui.state(false)
            local revision = ui.state(0)
            return ui.panel { children = {
                ui.button {
                    key = 'toggle',
                    on_click = function()
                        expanded:set(true)
                        revision:set(revision:get() + 1)
                    end,
                },
                ui.button {
                    key = 'rebuild',
                    on_click = function() revision:set(revision:get() + 1) end,
                },
                ui.column { children = function()
                    local _ = revision:get()
                    return { ui.row {
                        key = 'stable',
                        style = { width = ui.transition(function()
                            return expanded:get() and 200 or 100
                        end, { duration = 100, easing = 'linear' }) },
                    } }
                end },
            } }
            "#,
        );
        let toggle = child_with_key(root(&runtime), "toggle").id;
        let rebuild = child_with_key(root(&runtime), "rebuild").id;
        let stable = child_with_key(root(&runtime), "stable").id;
        let start = Instant::now();

        runtime
            .dispatch_inner(toggle, Event::Click, Some(start))
            .unwrap();
        assert_eq!(child_with_key(root(&runtime), "stable").id, stable);
        runtime.advance(start + Duration::from_millis(40)).unwrap();
        close(
            child_with_key(root(&runtime), "stable")
                .style
                .width
                .unwrap(),
            140.0,
        );

        runtime
            .dispatch_inner(
                rebuild,
                Event::Click,
                Some(start + Duration::from_millis(40)),
            )
            .unwrap();
        close(
            child_with_key(root(&runtime), "stable")
                .style
                .width
                .unwrap(),
            140.0,
        );
        runtime.advance(start + Duration::from_millis(100)).unwrap();
        close(
            child_with_key(root(&runtime), "stable")
                .style
                .width
                .unwrap(),
            200.0,
        );
        assert!(!runtime.is_animating(stable));
    }

    #[test]
    fn removed_children_drop_their_animation_state() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local expanded = ui.state(false)
            local present = ui.state(true)
            return ui.panel { children = {
                ui.button {
                    key = 'start',
                    on_click = function() expanded:set(true) end,
                },
                ui.button {
                    key = 'remove',
                    on_click = function() present:set(false) end,
                },
                ui.column { children = function()
                    if not present:get() then return {} end
                    return {
                        ui.row {
                            key = 'animated',
                            style = { width = ui.transition(function()
                                return expanded:get() and 200 or 100
                            end, { duration = 100, easing = 'linear' }) },
                        },
                        ui.row {
                            key = 'spring',
                            style = { width = ui.spring(function()
                                return expanded:get() and 200 or 100
                            end, { stiffness = 100, damping = 0.8 }) },
                        },
                    }
                end },
            } }
            "#,
        );
        let start_button = child_with_key(root(&runtime), "start").id;
        let remove_button = child_with_key(root(&runtime), "remove").id;
        let animated = child_with_key(root(&runtime), "animated").id;
        let spring = child_with_key(root(&runtime), "spring").id;
        let start = Instant::now();

        runtime
            .dispatch_inner(start_button, Event::Click, Some(start))
            .unwrap();
        runtime.advance(start + Duration::from_millis(40)).unwrap();
        assert!(runtime.is_animating(animated));
        assert!(runtime.is_animating(spring));
        runtime
            .dispatch_inner(
                remove_button,
                Event::Click,
                Some(start + Duration::from_millis(40)),
            )
            .unwrap();
        assert!(root(&runtime).find(animated).is_none());
        assert!(root(&runtime).find(spring).is_none());
        assert!(!runtime.is_animating(animated));
        assert!(!runtime.is_animating(spring));
        assert!(!runtime.advance(start + Duration::from_millis(50)).unwrap());
    }

    #[test]
    fn invalid_targets_preserve_the_previous_snapshot() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local opacity = ui.state(0.5)
            return ui.panel { children = {
                ui.button {
                    key = 'start',
                    on_click = function() opacity:set(1) end,
                },
                ui.button {
                    key = 'invalid',
                    on_click = function() opacity:set(2) end,
                },
                ui.row {
                key = 'animated',
                style = { opacity = ui.transition(function()
                    return opacity:get()
                end, { duration = 100, easing = 'linear' }) },
            } } }
            "#,
        );
        let start_button = child_with_key(root(&runtime), "start").id;
        let invalid_button = child_with_key(root(&runtime), "invalid").id;
        let animated = child_with_key(root(&runtime), "animated").id;
        let start = Instant::now();
        runtime
            .dispatch_inner(start_button, Event::Click, Some(start))
            .unwrap();
        runtime.advance(start + Duration::from_millis(40)).unwrap();
        close(
            child_with_key(root(&runtime), "animated").style.opacity,
            0.7,
        );

        assert!(
            runtime
                .dispatch_inner(
                    invalid_button,
                    Event::Click,
                    Some(start + Duration::from_millis(40)),
                )
                .is_err()
        );
        close(
            child_with_key(root(&runtime), "animated").style.opacity,
            0.7,
        );
        assert!(runtime.is_animating(animated));
        runtime.advance(start + Duration::from_millis(50)).unwrap();
        close(
            child_with_key(root(&runtime), "animated").style.opacity,
            0.75,
        );
    }

    #[test]
    fn failed_and_successful_reloads_have_explicit_animation_semantics() {
        let (_directory, path, mut runtime) = load(
            r#"
            local expanded = ui.state(false)
            return ui.panel { children = { ui.button {
                on_click = function() expanded:set(true) end,
                style = { width = ui.transition(function()
                    return expanded:get() and 200 or 100
                end, { duration = 100, easing = 'linear' }),
                height = ui.spring(function()
                    return expanded:get() and 200 or 100
                end, { stiffness = 100, damping = 0.8 }) },
            } } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let start = Instant::now();
        runtime
            .dispatch_inner(button, Event::Click, Some(start))
            .unwrap();

        fs::write(&path, "return ui.text { text = 'invalid root' }").unwrap();
        assert!(runtime.reload(&path).is_err());
        runtime.advance(start + Duration::from_millis(50)).unwrap();
        close(root(&runtime).children[0].style.width.unwrap(), 150.0);
        assert!(root(&runtime).children[0].style.height.unwrap() > 100.0);
        assert!(runtime.is_animating(button));

        fs::write(
            &path,
            r#"return ui.panel { style = { width = ui.transition(function()
                return 40
            end, { duration = 100 }), height = ui.spring(function()
                return 30
            end, { stiffness = 100, damping = 0.8 }) } }"#,
        )
        .unwrap();
        runtime.reload(&path).unwrap();
        close(root(&runtime).style.width.unwrap(), 40.0);
        close(root(&runtime).style.height.unwrap(), 30.0);
        assert!(!runtime.is_animating(root(&runtime).id));
    }

    #[test]
    fn transition_options_and_targets_are_strict() {
        for source in [
            "return ui.panel { style = { width = ui.transition(function() return 1 end, {}) } }",
            "return ui.panel { style = { width = ui.transition(function() return 1 end, { duration = -1 }) } }",
            "return ui.panel { style = { width = ui.transition(function() return 1 end, { duration = 10.5 }) } }",
            "return ui.panel { style = { width = ui.transition(function() return 1 end, { duration = 10, easing = 'bounce' }) } }",
            "return ui.panel { style = { width = ui.transition(function() return nil end, { duration = 10 }) } }",
            "return ui.panel { style = { background = ui.transition(function() return 1 end, { duration = 10 }) } }",
            "return ui.panel { style = { background = ui.transition(function() return 'red' end, { duration = 10 }) } }",
            "return ui.panel { style = { font_size = ui.transition(function() return 14 end, { duration = 10 }) } }",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("panel.luau");
            fs::write(&path, source).unwrap();
            assert!(Runtime::load(path).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn spring_options_and_targets_are_strict() {
        for source in [
            "return ui.panel { style = { width = ui.spring(function() return 1 end, {}) } }",
            "return ui.panel { style = { width = ui.spring(function() return 1 end, { damping = 1 }) } }",
            "return ui.panel { style = { width = ui.spring(function() return 1 end, { stiffness = 1 }) } }",
            "return ui.panel { style = { width = ui.spring(function() return 1 end, { stiffness = 0, damping = 1 }) } }",
            "return ui.panel { style = { width = ui.spring(function() return 1 end, { stiffness = 1, damping = 0 }) } }",
            "return ui.panel { style = { width = ui.spring(function() return 1 end, { stiffness = 1, damping = 1, epsilon = 0 }) } }",
            "return ui.panel { style = { width = ui.spring(function() return 1 end, { stiffness = 1e-308, damping = 1e308 }) } }",
            "return ui.panel { style = { width = ui.spring(function() return 1 end, { stiffness = 1e32, damping = 1e292 }) } }",
            "return ui.panel { style = { width = ui.spring(function() return 1 end, { stiffness = 1, damping = 1, unknown = 1 }) } }",
            "return ui.panel { style = { width = ui.spring(function() return 'wide' end, { stiffness = 1, damping = 1 }) } }",
            "return ui.panel { style = { background = ui.spring(function() return 1 end, { stiffness = 1, damping = 1 }) } }",
            "return ui.panel { style = { font_size = ui.spring(function() return 14 end, { stiffness = 1, damping = 1 }) } }",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("panel.luau");
            fs::write(&path, source).unwrap();
            assert!(Runtime::load(path).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn every_numeric_style_spring_is_accepted() {
        let (_directory, _path, runtime) = load(
            r#"
            local function fixed(value)
                return ui.spring(function() return value end, {
                    stiffness = 170,
                    damping = 0.8,
                })
            end
            return ui.panel { children = { ui.row { style = {
                width = fixed(100), height = fixed(40), padding = fixed(1),
                padding_x = fixed(2), padding_y = fixed(3), gap = fixed(4),
                grow = fixed(0), radius = fixed(5), border_width = fixed(1),
                opacity = fixed(0.8),
            } } } }
            "#,
        );
        assert!(!runtime.has_active_transitions());
    }

    #[test]
    fn every_numeric_style_transition_is_accepted() {
        let (_directory, _path, runtime) = load(
            r#"
            local function fixed(value)
                return ui.transition(function() return value end, { duration = 0 })
            end
            return ui.panel { children = { ui.row { style = {
                width = fixed(100), height = fixed(40), padding = fixed(1),
                padding_x = fixed(2), padding_y = fixed(3), gap = fixed(4),
                grow = fixed(0), radius = fixed(5), border_width = fixed(1),
                opacity = fixed(0.8),
            } } } }
            "#,
        );
        assert!(!runtime.has_active_transitions());
    }

    #[test]
    fn every_color_style_transition_is_accepted() {
        let (_directory, _path, runtime) = load(
            r#"
            local function fixed(value)
                return ui.transition(function() return value end, { duration = 0 })
            end
            return ui.panel { children = { ui.row { style = {
                background = fixed('#01020304'),
                foreground = fixed('#11121314'),
                border_color = fixed('#21222324'),
                hover_overlay = fixed('#31323334'),
                pressed_overlay = fixed('#41424344'),
                focus_ring_color = fixed('#51525354'),
            } } } }
            "#,
        );
        let style = &root(&runtime).children[0].style;
        assert_eq!(style.background, Color([1, 2, 3, 4]));
        assert_eq!(style.foreground, Color([0x11, 0x12, 0x13, 0x14]));
        assert_eq!(style.border_color, Color([0x21, 0x22, 0x23, 0x24]));
        assert_eq!(style.hover_overlay, Color([0x31, 0x32, 0x33, 0x34]));
        assert_eq!(style.pressed_overlay, Color([0x41, 0x42, 0x43, 0x44]));
        assert_eq!(style.focus_ring_color, Color([0x51, 0x52, 0x53, 0x54]));
        assert!(!runtime.has_active_transitions());
    }

    #[test]
    fn transform_transitions_cross_zero_and_retarget_from_the_displayed_frame() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local flipped = ui.state(false)
            local function moving(before, after)
                return ui.transition(function()
                    return if flipped:get() then after else before
                end, { duration = 100, easing = 'linear' })
            end
            return ui.panel { children = {
                ui.button { on_click = function() flipped:set(not flipped:get()) end },
                ui.row { style = {
                    translate_x = moving(0, 40), translate_y = moving(10, -10),
                    scale_x = moving(1, -1), scale_y = moving(-1, 1),
                    rotation = moving(350, 370),
                } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let animated = root(&runtime).children[1].id;
        let start = Instant::now();
        runtime
            .dispatch_inner(button, Event::Click, Some(start))
            .unwrap();
        runtime.advance(start + Duration::from_millis(50)).unwrap();
        let style = &root(&runtime).children[1].style;
        close(style.translate_x, 20.0);
        close(style.translate_y, 0.0);
        close(style.scale_x, 0.0);
        close(style.scale_y, 0.0);
        close(style.rotation, 360.0);

        runtime
            .dispatch_inner(
                button,
                Event::Click,
                Some(start + Duration::from_millis(50)),
            )
            .unwrap();
        runtime.advance(start + Duration::from_millis(75)).unwrap();
        let style = &root(&runtime).children[1].style;
        close(style.translate_x, 15.0);
        close(style.translate_y, 2.5);
        close(style.scale_x, 0.25);
        close(style.scale_y, -0.25);
        close(style.rotation, 357.5);

        runtime.advance(start + Duration::from_millis(150)).unwrap();
        let style = &root(&runtime).children[1].style;
        close(style.translate_x, 0.0);
        close(style.translate_y, 10.0);
        close(style.scale_x, 1.0);
        close(style.scale_y, -1.0);
        close(style.rotation, 350.0);
        assert!(!runtime.is_animating(animated));
    }

    #[test]
    fn transform_springs_bound_published_frames_without_losing_momentum() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local moving = ui.state(false)
            local function spring(before, after)
                return ui.spring(function()
                    return if moving:get() then after else before
                end, { stiffness = 100, damping = 0.1, epsilon = 0.001 })
            end
            return ui.panel { children = {
                ui.button { on_click = function() moving:set(true) end },
                ui.row { style = {
                    translate_x = spring(0, 100000), translate_y = spring(0, -100000),
                    scale_x = spring(1, 100), scale_y = spring(-1, -100),
                    rotation = spring(0, 1000000),
                } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let animated = root(&runtime).children[1].id;
        let start = Instant::now();
        runtime
            .dispatch_inner(button, Event::Click, Some(start))
            .unwrap();
        runtime.advance(start + Duration::from_millis(300)).unwrap();
        let style = &root(&runtime).children[1].style;
        close(style.translate_x, 100_000.0);
        close(style.translate_y, -100_000.0);
        close(style.scale_x, 100.0);
        close(style.scale_y, -100.0);
        close(style.rotation, 1_000_000.0);
        assert!(runtime.is_animating(animated));

        runtime.advance(start + Duration::from_millis(600)).unwrap();
        let style = &root(&runtime).children[1].style;
        assert!(style.translate_x < 100_000.0 && style.translate_y > -100_000.0);
        assert!(style.scale_x < 100.0 && style.scale_y > -100.0);
        assert!(style.rotation < 1_000_000.0);
        assert!(runtime.is_animating(animated));
    }

    #[test]
    fn transform_animation_rejects_invalid_targets_before_retargeting() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local invalid = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() invalid:set(true) end },
                ui.row { style = {
                    scale_x = ui.spring(function()
                        return if invalid:get() then 101 else 1
                    end, { stiffness = 100, damping = 0.8 }),
                } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        assert!(runtime.dispatch(button, Event::Click).is_err());
        close(root(&runtime).children[1].style.scale_x, 1.0);
        assert!(!runtime.has_active_transitions());
    }

    #[test]
    fn reactive_transform_properties_publish_together_or_preserve_the_previous_style() {
        let (_directory, _path, mut runtime) = load(
            r#"
            local invalid = ui.state(false)
            return ui.panel { children = {
                ui.button { on_click = function() invalid:set(true) end },
                ui.row { style = {
                    translate_x = function() return invalid:get() and 20 or 4 end,
                    scale_x = function() return invalid:get() and 101 or -1 end,
                    rotation = function() return invalid:get() and 30 or 15 end,
                    transform_origin = function()
                        return { x = invalid:get() and 2 or 0, y = 1, offset_x = 3 }
                    end,
                } },
            } }
            "#,
        );
        let button = root(&runtime).children[0].id;
        let style = &root(&runtime).children[1].style;
        close(style.translate_x, 4.0);
        close(style.scale_x, -1.0);
        close(style.rotation, 15.0);
        close(style.transform_origin.x, 0.0);
        close(style.transform_origin.y, 1.0);
        close(style.transform_origin.offset_x, 3.0);

        assert!(runtime.dispatch(button, Event::Click).is_err());
        let style = &root(&runtime).children[1].style;
        close(style.translate_x, 4.0);
        close(style.scale_x, -1.0);
        close(style.rotation, 15.0);
        close(style.transform_origin.x, 0.0);
    }
}

#[cfg(test)]
mod timer_tests;
