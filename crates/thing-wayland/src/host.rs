use std::{
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_xdg_popup, delegate_xdg_shell,
    output::{OutputHandler, OutputInfo, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{
            KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers, RepeatInfo,
        },
        pointer::{AxisScroll, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure, SurfaceKind,
        },
        xdg::{
            XdgShell,
            popup::{Popup as XdgPopup, PopupConfigure, PopupHandler},
            window::{Window as XdgWindow, WindowConfigure, WindowHandler},
        },
    },
};
use thing_core::{
    KeyboardMode, Kind, Node, NodeId, PanelInputRegion, Point, Rect, Scale, SurfaceLayer,
};
use thing_runtime::{
    Action, Event, OutputContext, Scalar, ServiceSnapshot, SurfaceKey, SurfaceTree,
    SystemTrayAction,
};
use thing_ui::{Interaction, Layout, PaintFrame, ScrollState, Size};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
};
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::{
            wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
            wp_fractional_scale_v1::{self, WpFractionalScaleV1},
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::{
        activation::v1::client::{
            xdg_activation_token_v1::{self, XdgActivationTokenV1},
            xdg_activation_v1::XdgActivationV1,
        },
        shell::client::xdg_positioner,
    },
};
use xkbcommon::xkb;

use crate::{
    Renderer, RunOptions, SystemTray,
    audio::Audio,
    configuration::Configurations,
    hover::{Command as HoverCommand, HoverKey, HoverStates, HoverTarget},
    launch::{self, LaunchRequest, Launcher, PendingLaunches},
    media::Media,
    niri::Niri,
    notifications::Server as NotificationServer,
    panel::{self, Geometry, ResolvedInputRegion},
    popup::{self, Position as PopupPosition},
    power::Power,
    render::{FrameResult, Render, Window},
    watch::Watch,
};

/// run configured surfaces on each output. the loop sleeps until input,
/// configuration, or a compositor frame callback arrives; unchanged surfaces do
/// not request frames.
pub fn run(entry: &Path, renderer: Renderer) -> Result<(), String> {
    run_with_options(entry, RunOptions::default().with_renderer(renderer))
}

/// run configured surfaces with process-wide service and renderer options.
pub fn run_with_options(entry: &Path, options: RunOptions) -> Result<(), String> {
    let renderer = options.renderer;
    if renderer == Renderer::Vulkan && !cfg!(feature = "vulkan") {
        return Err(
            "vulkan support is not included in this build; rebuild with --features vulkan".into(),
        );
    }
    let entry = entry
        .canonicalize()
        .map_err(|error| format!("{}: {error}", entry.display()))?;
    // signalfd must block SIGCHLD before any worker exists. workers inherit the
    // mask; launched children restore its previous state immediately before exec.
    let (launcher, child_signals) =
        launch::prepare().map_err(|error| format!("cannot monitor launched processes: {error}"))?;
    let (time, date, next_minute) = crate::clock::read();
    let mut services = ServiceSnapshot::default();
    services
        .set_service("clock.time", Scalar::String(time))
        .map_err(|e| e.to_string())?;
    services
        .set_service("clock.date", Scalar::String(date))
        .map_err(|e| e.to_string())?;
    let watch = Rc::new(Watch::new().map_err(|e| e.to_string())?);
    watch
        .set_files(std::slice::from_ref(&entry))
        .map_err(|e| e.to_string())?;
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("cannot connect to Wayland: {error}"))?;
    let (globals, mut queue) =
        registry_queue_init(&connection).map_err(|error| error.to_string())?;
    let qh = queue.handle();
    let layer_shell_version = globals.contents().with_list(|globals| {
        globals
            .iter()
            .find(|global| global.interface == "zwlr_layer_shell_v1")
            .map_or(0, |global| global.version.min(4))
    });
    let compositor = CompositorState::bind(&globals, &qh).map_err(|error| error.to_string())?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|error| format!("the compositor must support wlr-layer-shell: {error}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh).map_err(|error| error.to_string())?;
    let activation: Option<XdgActivationV1> = globals.bind(&qh, 1..=1, ()).ok();
    let (image_ping, image_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let wake = image_ping.clone();
    let gpu_wake = image_ping.clone();
    let probe = (renderer == Renderer::Vulkan).then(|| compositor.create_surface(&qh));
    let render = Render::new(
        renderer,
        &connection,
        probe.as_ref(),
        move || wake.ping(),
        move || gpu_wake.ping(),
    );
    if let Some(probe) = probe {
        probe.destroy();
    }
    let render = render?;
    let (audio_ping, audio_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let audio = Audio::start(audio_ping).map_err(|e| format!("cannot start audio worker: {e}"))?;
    let (media_ping, media_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let media = Media::start(media_ping).map_err(|e| format!("cannot start media worker: {e}"))?;
    let (power_ping, power_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let power = Power::start(power_ping).map_err(|e| format!("cannot start power worker: {e}"))?;
    let (niri_ping, niri_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let niri = Niri::start(niri_ping).map_err(|e| format!("cannot start niri worker: {e}"))?;
    let (notifications, notification_source) = if options.notification_server {
        let (ping, source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
        let server = NotificationServer::start(ping)
            .map_err(|e| format!("cannot start notification server: {e}"))?;
        (Some(server), Some(source))
    } else {
        (None, None)
    };
    let mut event_loop: EventLoop<'static, App> =
        EventLoop::try_new().map_err(|error| error.to_string())?;
    let (system_tray, system_tray_source) = match options.system_tray {
        Some(mode) => {
            let (ping, source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
            let tray = SystemTray::start(mode, ping)
                .map_err(|error| format!("cannot start system tray worker: {error}"))?;
            (Some(tray), Some(source))
        }
        None => (None, None),
    };
    let loop_handle = event_loop.handle();
    let mut app = App {
        render,
        image_ping,
        configurations: Configurations::new(entry, services),
        watch: watch.clone(),
        reload_at: None,
        hover: HoverStates::default(),
        audio,
        media,
        power,
        niri,
        notifications,
        system_tray,
        launcher,
        pending_launches: PendingLaunches::new(),
        surfaces: Vec::new(),
        closed_surfaces: Vec::new(),
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        layer_shell_version,
        xdg_shell,
        activation,
        fractional: globals.bind(&qh, 1..=1, ()).ok(),
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        pointer: None,
        keyboard: None,
        seat: None,
        keyboard_surface: None,
        queue: qh.clone(),
        loop_handle,
        modifiers: Modifiers::default(),
        keyboard_keymap: None,
        repeat: RepeatSession::default(),
        repeat_token: None,
        loading_outputs: true,
        exit: false,
        error: None,
    };
    queue
        .roundtrip(&mut app)
        .map_err(|error| error.to_string())?;
    let outputs: Vec<_> = app
        .outputs
        .outputs()
        .filter_map(|output| app.output_context(&output).map(|context| (output, context)))
        .collect();
    let layer_shell_version = app.layer_shell_version;
    app.configurations
        .reload_for_checked(outputs.clone(), |_, runtime| {
            validate_surface_capabilities(runtime.surfaces(), layer_shell_version)
        })?;
    for (output, _) in outputs {
        app.sync_output_surfaces(&output, false, &qh)?;
    }
    app.loading_outputs = false;
    app.update_watch();
    event_loop
        .handle()
        .insert_source(child_signals, |_, _, app| app.reap_children())
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(image_source, |_, _, app| app.invalidate_all())
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(niri_source, |_, _, app| app.update_niri())
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(audio_source, |_, _, app| app.update_audio())
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(media_source, |_, _, app| app.update_media())
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(power_source, |_, _, app| app.update_power())
        .map_err(|e| e.to_string())?;
    if let Some(source) = notification_source {
        event_loop
            .handle()
            .insert_source(source, |_, _, app| app.update_notifications())
            .map_err(|e| e.to_string())?;
    }
    if let Some(source) = system_tray_source {
        event_loop
            .handle()
            .insert_source(source, |_, _, app| app.update_system_tray())
            .map_err(|e| e.to_string())?;
    }
    WaylandSource::new(connection.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|error| error.to_string())?;
    event_loop
        .handle()
        .insert_source(
            Generic::new(watch, Interest::READ, Mode::Level),
            |_, watch, app| {
                if watch.changed()? {
                    app.reload_at = Some(Instant::now() + Duration::from_millis(80));
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(Timer::from_duration(next_minute), |_, _, app| {
            let (time, date, next) = crate::clock::read();
            if let Err(error) = app
                .configurations
                .set_service("clock.time", Scalar::String(time))
            {
                eprintln!("thing: {error}");
            }
            if let Err(error) = app
                .configurations
                .set_service("clock.date", Scalar::String(date))
            {
                eprintln!("thing: {error}");
            }
            app.refresh();
            TimeoutAction::ToDuration(next)
        })
        .map_err(|e| e.to_string())?;
    while !app.exit {
        app.expire_launches();
        if app
            .reload_at
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            app.reload_at = None;
            app.reload(&qh)?;
        }
        app.draw(&qh)?;
        app.process_hover(&qh);
        connection.flush().map_err(|error| error.to_string())?;
        let timeout = app
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        event_loop
            .dispatch(timeout, &mut app)
            .map_err(|error| error.to_string())?;
        if let Some(error) = app.error.take() {
            return Err(error);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PopupMode {
    Manual,
    Automatic(HoverKey<SurfaceOwner>),
    Hover(HoverKey<SurfaceOwner>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SurfaceOwner {
    output: wl_output::WlOutput,
    key: SurfaceKey,
}

struct SurfaceInit {
    owner: SurfaceOwner,
    namespace: String,
    root: NodeId,
    size: (u32, u32),
    scale: Scale,
}

struct InputOrigin {
    seat: wl_seat::WlSeat,
    serial: u32,
    surface: wl_surface::WlSurface,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WidgetKeyAction {
    Activate,
    Slider(SliderKeyAction),
}

impl WidgetKeyAction {
    fn repeats(self) -> bool {
        matches!(
            self,
            Self::Slider(SliderKeyAction::Decrease | SliderKeyAction::Increase)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SliderKeyAction {
    Decrease,
    Increase,
    Minimum,
    Maximum,
}

#[derive(Clone, Copy)]
struct RepeatSettings {
    delay: Duration,
    interval: Duration,
}

impl RepeatSettings {
    fn from_info(info: RepeatInfo) -> Option<Self> {
        match info {
            RepeatInfo::Repeat { rate, delay } => Some(Self {
                delay: Duration::from_millis(u64::from(delay)),
                interval: Duration::from_micros((1_000_000 / u64::from(rate.get())).max(1)),
            }),
            RepeatInfo::Disable => None,
        }
    }
}

struct RepeatingKey<S> {
    event: KeyEvent,
    surface: S,
    first: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeatTimerAction {
    None,
    Arm(Duration),
    Disarm,
}

struct RepeatSession<S> {
    settings: Option<RepeatSettings>,
    key: Option<RepeatingKey<S>>,
    host_armed: bool,
}

impl<S> Default for RepeatSession<S> {
    fn default() -> Self {
        Self {
            settings: None,
            key: None,
            host_armed: false,
        }
    }
}

impl<S: PartialEq> RepeatSession<S> {
    fn press(
        &mut self,
        event: KeyEvent,
        surface: S,
        key_repeats: bool,
        action_repeats: bool,
    ) -> RepeatTimerAction {
        if !key_repeats {
            return RepeatTimerAction::None;
        }
        if !action_repeats {
            return self.clear();
        }
        self.key = Some(RepeatingKey {
            event,
            surface,
            first: true,
        });
        match self.settings {
            Some(settings) => {
                self.host_armed = true;
                RepeatTimerAction::Arm(settings.delay)
            }
            None if self.host_armed => {
                self.host_armed = false;
                RepeatTimerAction::Disarm
            }
            None => RepeatTimerAction::None,
        }
    }

    fn update(&mut self, info: RepeatInfo) -> RepeatTimerAction {
        self.settings = RepeatSettings::from_info(info);
        match (self.settings, self.key.as_mut()) {
            (Some(settings), Some(key)) => {
                key.first = true;
                self.host_armed = true;
                RepeatTimerAction::Arm(settings.delay)
            }
            _ if self.host_armed => {
                self.host_armed = false;
                RepeatTimerAction::Disarm
            }
            _ => RepeatTimerAction::None,
        }
    }

    fn release(&mut self, raw_code: u32) -> RepeatTimerAction {
        if self
            .key
            .as_ref()
            .is_some_and(|key| key.event.raw_code == raw_code)
        {
            self.clear()
        } else {
            RepeatTimerAction::None
        }
    }

    fn clear(&mut self) -> RepeatTimerAction {
        self.key = None;
        if self.host_armed {
            self.host_armed = false;
            RepeatTimerAction::Disarm
        } else {
            RepeatTimerAction::None
        }
    }

    fn accepts_compositor_repeat(&self, raw_code: u32, surface: &S) -> bool {
        self.settings.is_none()
            && self
                .key
                .as_ref()
                .is_some_and(|key| key.event.raw_code == raw_code && &key.surface == surface)
    }
}

struct PopupRole {
    shell: XdgPopup,
    mode: PopupMode,
    grabbed: bool,
    anchor: Option<NodeId>,
    reposition: popup::RepositionState,
}

enum Role {
    Panel(LayerSurface),
    Popup(PopupRole),
}
impl Role {
    fn surface(&self) -> &wl_surface::WlSurface {
        match self {
            Self::Panel(layer) => layer.wl_surface(),
            Self::Popup(role) => role.shell.wl_surface(),
        }
    }

    fn popup_mode(&self) -> Option<&PopupMode> {
        match self {
            Self::Panel(_) => None,
            Self::Popup(role) => Some(&role.mode),
        }
    }
}

struct Surface {
    // release graphics state before the Wayland role and its underlying surface.
    window: Option<Window>,
    role: Role,
    output: wl_output::WlOutput,
    key: SurfaceKey,
    namespace: String,
    root: NodeId,
    size: (u32, u32),
    panel: Option<PanelState>,
    scale: Scale,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    // geometry from the last successfully presented frame.
    layout: Option<Layout>,
    scroll: ScrollState,
    input: Interaction,
    pointer_position: Option<Point>,
    hover_path: Vec<NodeId>,
    press_serial: Option<u32>,
    pressed_button: Option<u32>,
    configured: bool,
    dirty: bool,
    waiting: bool,
    retry_at: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelState {
    geometry: Geometry,
    // resolved geometry cannot distinguish auto from an equal explicit zone.
    exclusive_zone: Option<i32>,
    input_region: PanelInputRegion,
    layer: SurfaceLayer,
    keyboard_mode: KeyboardMode,
}

impl PanelState {
    fn from_root(root: &Node) -> Self {
        Self {
            geometry: panel::geometry(root, None),
            exclusive_zone: root.panel.exclusive_zone,
            input_region: root.panel.input_region.clone(),
            layer: root.panel.layer,
            keyboard_mode: root.panel.keyboard_mode,
        }
    }
}

impl Surface {
    fn owner(&self) -> SurfaceOwner {
        SurfaceOwner {
            output: self.output.clone(),
            key: self.key.clone(),
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some(scale) = self.fractional.take() {
            scale.destroy();
        }
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
    }
}

struct App {
    render: Render,
    image_ping: calloop::ping::Ping,
    configurations: Configurations<wl_output::WlOutput>,
    audio: Audio,
    media: Media,
    power: Power,
    niri: Niri,
    notifications: Option<NotificationServer>,
    system_tray: Option<SystemTray>,
    launcher: Launcher,
    pending_launches: PendingLaunches<XdgActivationTokenV1>,
    watch: Rc<Watch>,
    reload_at: Option<Instant>,
    hover: HoverStates<SurfaceOwner>,
    surfaces: Vec<Surface>,
    closed_surfaces: Vec<SurfaceOwner>,
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    layer_shell_version: u32,
    xdg_shell: XdgShell,
    activation: Option<XdgActivationV1>,
    fractional: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat: Option<wl_seat::WlSeat>,
    keyboard_surface: Option<wl_surface::WlSurface>,
    queue: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    modifiers: Modifiers,
    keyboard_keymap: Option<xkb::Keymap>,
    repeat: RepeatSession<wl_surface::WlSurface>,
    repeat_token: Option<RegistrationToken>,
    loading_outputs: bool,
    exit: bool,
    error: Option<String>,
}

impl Drop for App {
    fn drop(&mut self) {
        self.clear_key_repeat();
        for (_, token) in self.pending_launches.drain() {
            token.destroy();
        }
        if let Some(activation) = self.activation.take() {
            activation.destroy();
        }
        self.render.prepare_drop();
        // popups are appended after their parents and must be released first.
        while self.surfaces.pop().is_some() {}
    }
}

impl App {
    fn next_deadline(&self) -> Option<Instant> {
        earliest_deadline(
            self.reload_at,
            self.hover.next_deadline(),
            self.pending_launches.next_deadline(),
            self.surfaces.iter().filter_map(|surface| surface.retry_at),
        )
    }

    fn update_niri(&mut self) {
        let Some(snapshot) = self.niri.take_snapshot() else {
            return;
        };
        let update = self
            .configurations
            .set_niri(snapshot.state)
            .and_then(|()| {
                self.configurations
                    .set_service("niri.available", Scalar::Boolean(snapshot.available))
            })
            .and_then(|()| {
                self.configurations
                    .set_service("niri.error", Scalar::String(snapshot.error))
            });
        if let Err(error) = update {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    fn update_audio(&mut self) {
        let Some(snapshot) = self.audio.take_snapshot() else {
            return;
        };
        if let Err(error) = self.configurations.set_audio(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    fn update_media(&mut self) {
        let Some(snapshot) = self.media.take_snapshot() else {
            return;
        };
        if let Err(error) = self.configurations.set_media(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    fn update_power(&mut self) {
        let Some(snapshot) = self.power.take_snapshot() else {
            return;
        };
        if let Err(error) = self.configurations.set_power(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    fn update_notifications(&mut self) {
        let Some(snapshot) = self
            .notifications
            .as_ref()
            .and_then(NotificationServer::take_snapshot)
        else {
            return;
        };
        if let Err(error) = self.configurations.set_notifications(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    fn update_system_tray(&mut self) {
        let Some(snapshot) = self
            .system_tray
            .as_ref()
            .and_then(SystemTray::take_snapshot)
        else {
            return;
        };
        if let Err(error) = self.configurations.set_system_tray(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    fn refresh(&mut self) {
        let layer_shell_version = self.layer_shell_version;
        let outputs = self.configurations.refresh_checked(|_, surfaces| {
            validate_surface_capabilities(surfaces, layer_shell_version)
        });
        self.update_watch();
        for output in outputs {
            self.invalidate_output(&output);
        }
    }

    fn reload(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let outputs = self
            .outputs
            .outputs()
            .filter_map(|output| {
                self.output_context(&output)
                    .or_else(|| self.configurations.output(&output).cloned())
                    .map(|context| (output, context))
            })
            .collect();
        let layer_shell_version = self.layer_shell_version;
        if let Err(error) = self
            .configurations
            .reload_for_checked(outputs, |_, runtime| {
                validate_surface_capabilities(runtime.surfaces(), layer_shell_version)
            })
        {
            eprintln!("thing: reload failed; keeping the previous surfaces: {error}");
            return Ok(());
        }
        // release the previous configuration's glyph atlas and text layouts.
        let wake = self.image_ping.clone();
        self.render.reset_painter(move || wake.ping())?;
        self.hover.clear();
        self.clear_key_repeat();
        while let Some(index) = self
            .surfaces
            .iter()
            .rposition(|surface| matches!(surface.role, Role::Popup(_)))
        {
            self.remove_popup(index, false, false);
        }
        self.closed_surfaces.clear();
        let removed: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| {
                matches!(surface.role, Role::Panel(_))
                    && !self.configurations.contains(&surface.output)
            })
            .map(Surface::owner)
            .collect();
        for owner in removed {
            self.remove_panel(&owner, false);
        }
        let outputs: Vec<_> = self.configurations.keys().cloned().collect();
        for output in outputs {
            self.sync_output_surfaces(&output, true, qh)?;
        }
        if self
            .keyboard_surface
            .as_ref()
            .is_some_and(|wl| self.index(wl).is_none())
        {
            self.clear_keyboard_focus();
        }
        self.update_watch();
        eprintln!("thing: reloaded {}", self.configurations.entry().display());
        Ok(())
    }

    fn add_output(
        &mut self,
        output: wl_output::WlOutput,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        if !self.configurations.contains(&output) {
            let Some(context) = self.output_context(&output) else {
                return Ok(());
            };
            let layer_shell_version = self.layer_shell_version;
            self.configurations
                .add_checked(output.clone(), context, |runtime| {
                    validate_surface_capabilities(runtime.surfaces(), layer_shell_version)
                })?;
        }
        self.sync_output_surfaces(&output, false, qh)?;
        self.update_watch();
        Ok(())
    }

    fn create_panel(&mut self, owner: SurfaceOwner, qh: &QueueHandle<Self>) -> Result<(), String> {
        let (root, namespace, panel, context) = {
            let surface = self
                .configurations
                .surface(&owner.output, &owner.key)
                .ok_or("surface configuration is unavailable")?;
            let context = self
                .configurations
                .output(&owner.output)
                .ok_or("output context is unavailable")?;
            (
                surface.root.id,
                surface.namespace.clone(),
                PanelState::from_root(&surface.root),
                context.clone(),
            )
        };
        let size = panel_size(panel.geometry, (context.width, context.height));
        let wl = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            wl,
            wayland_layer(panel.layer),
            Some(&namespace),
            Some(&owner.output),
        );
        apply_panel_geometry(&layer, panel.geometry);
        apply_keyboard_mode(&layer, panel.keyboard_mode)?;
        apply_input_region(
            &self.compositor,
            layer.wl_surface(),
            &panel.input_region,
            size,
        )?;
        let mut surface = self.make_surface(
            Role::Panel(layer),
            SurfaceInit {
                owner,
                namespace,
                root,
                size,
                scale: context.scale,
            },
            qh,
        );
        surface.panel = Some(panel);
        surface.role.surface().commit();
        self.surfaces.push(surface);
        Ok(())
    }

    fn make_surface(&self, role: Role, init: SurfaceInit, qh: &QueueHandle<Self>) -> Surface {
        let viewport = self
            .viewporter
            .as_ref()
            .map(|manager| manager.get_viewport(role.surface(), qh, ()));
        let fractional = if viewport.is_some() {
            self.fractional.as_ref().map(|manager| {
                manager.get_fractional_scale(role.surface(), qh, role.surface().clone())
            })
        } else {
            None
        };
        Surface {
            window: None,
            role,
            output: init.owner.output,
            key: init.owner.key,
            namespace: init.namespace,
            root: init.root,
            size: init.size,
            panel: None,
            scale: init.scale,
            viewport,
            fractional,
            layout: None,
            scroll: ScrollState::default(),
            input: Interaction::default(),
            pointer_position: None,
            hover_path: Vec::new(),
            press_serial: None,
            pressed_button: None,
            configured: false,
            dirty: true,
            waiting: false,
            retry_at: None,
        }
    }

    fn output_context(&self, output: &wl_output::WlOutput) -> Option<OutputContext> {
        let mut context = output_context(self.outputs.info(output)?)?;
        if let Some(surface) = self
            .surfaces
            .iter()
            .find(|surface| &surface.output == output && matches!(surface.role, Role::Panel(_)))
        {
            context.scale = surface.scale;
        }
        Some(context)
    }

    fn update_watch(&self) {
        if let Err(error) = self.watch.set_files(&self.configurations.files()) {
            eprintln!("thing: cannot watch all configuration files: {error}");
        }
    }

    fn invalidate_all(&mut self) {
        for surface in &mut self.surfaces {
            surface.dirty = true;
        }
    }

    fn sync_output_surfaces(
        &mut self,
        output: &wl_output::WlOutput,
        force: bool,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        let desired: Vec<_> = self
            .configurations
            .surfaces(output)
            .unwrap_or_default()
            .iter()
            .map(|surface| {
                (
                    surface.key.clone(),
                    surface.namespace.clone(),
                    surface.root.visible,
                    surface.root.panel.layer,
                )
            })
            .collect();

        self.closed_surfaces.retain(|owner| {
            &owner.output != output
                || desired
                    .iter()
                    .any(|(key, _, visible, _)| key == &owner.key && *visible)
        });

        let stale: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| &surface.output == output && matches!(surface.role, Role::Panel(_)))
            .filter(|surface| {
                let Some((_, namespace, visible, layer)) =
                    desired.iter().find(|(key, _, _, _)| key == &surface.key)
                else {
                    return true;
                };
                !visible
                    || namespace != &surface.namespace
                    || (surface
                        .panel
                        .as_ref()
                        .is_some_and(|panel| panel.layer != *layer)
                        && panel_role_version(&surface.role) < 2)
            })
            .map(Surface::owner)
            .collect();
        for owner in stale {
            self.remove_panel(&owner, !force);
        }

        for (key, _, visible, _) in &desired {
            let owner = SurfaceOwner {
                output: output.clone(),
                key: key.clone(),
            };
            if *visible
                && !self.closed_surfaces.contains(&owner)
                && self.panel_index(&owner).is_none()
            {
                self.create_panel(owner, qh)?;
            }
        }

        let owners: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| &surface.output == output && matches!(surface.role, Role::Panel(_)))
            .map(Surface::owner)
            .collect();
        for owner in owners {
            self.sync_panel(&owner, force)?;
        }
        Ok(())
    }

    fn sync_panel(&mut self, owner: &SurfaceOwner, force: bool) -> Result<(), String> {
        let Some(index) = self.surfaces.iter().position(|surface| {
            surface.output == owner.output
                && surface.key == owner.key
                && matches!(surface.role, Role::Panel(_))
        }) else {
            return Ok(());
        };
        let previous = self.surfaces[index].panel.clone();
        let size = self.surfaces[index].size;
        let Some(root) = self.configurations.surface_root(&owner.output, &owner.key) else {
            return Ok(());
        };
        let next = PanelState::from_root(root);
        if !force && previous.as_ref() == Some(&next) {
            return Ok(());
        }
        let configured_size = previous
            .as_ref()
            .filter(|previous| {
                previous.geometry.anchors == next.geometry.anchors
                    && previous.geometry.requested_size == next.geometry.requested_size
            })
            .map(|_| size);
        let geometry = panel::geometry(root, configured_size);
        let root_id = root.id;
        let surface = &mut self.surfaces[index];
        let root_changed = surface.root != root_id;
        let reset_input = force || root_changed;
        let wl = surface.role.surface().clone();
        let Role::Panel(layer) = &surface.role else {
            return Ok(());
        };
        if previous
            .as_ref()
            .is_some_and(|previous| previous.layer != next.layer)
        {
            layer.set_layer(wayland_layer(next.layer));
        }
        apply_panel_geometry(layer, geometry);
        apply_keyboard_mode(layer, next.keyboard_mode)?;
        apply_input_region(
            &self.compositor,
            layer.wl_surface(),
            &next.input_region,
            surface.size,
        )?;
        surface.root = root_id;
        surface.panel = Some(next);
        if reset_input {
            surface.layout = None;
            surface.scroll = ScrollState::default();
            surface.input = Interaction::default();
            surface.pointer_position = None;
            surface.hover_path.clear();
            surface.press_serial = None;
            surface.pressed_button = None;
        }
        surface.dirty = true;
        layer.wl_surface().commit();
        if reset_input && self.keyboard_surface.as_ref() == Some(&wl) {
            self.clear_key_repeat();
        }
        Ok(())
    }

    fn sync_configured_panel(&mut self, index: usize) -> Result<(), String> {
        let surface = &self.surfaces[index];
        let Role::Panel(layer) = &surface.role else {
            return Ok(());
        };
        let Some(root) = self
            .configurations
            .surface_root(&surface.output, &surface.key)
        else {
            return Ok(());
        };
        let geometry = panel::geometry(root, Some(surface.size));
        apply_panel_geometry(layer, geometry);
        apply_input_region(
            &self.compositor,
            layer.wl_surface(),
            &root.panel.input_region,
            surface.size,
        )?;
        Ok(())
    }

    fn set_output_scale(&mut self, output: &wl_output::WlOutput, scale: Scale) {
        let Some(mut context) = self.configurations.output(output).cloned() else {
            return;
        };
        if context.scale != scale {
            context.scale = scale;
            let layer_shell_version = self.layer_shell_version;
            let result = self
                .configurations
                .set_output_checked(output, context, |surfaces| {
                    validate_surface_capabilities(surfaces, layer_shell_version)
                });
            self.update_watch();
            match result {
                Ok(true) => self.invalidate_output(output),
                Ok(false) => {}
                Err(error) => eprintln!("thing: cannot update output scale: {error}"),
            }
        }
        let indices: Vec<_> = self
            .surfaces
            .iter()
            .enumerate()
            .filter(|(_, surface)| {
                &surface.output == output && matches!(surface.role, Role::Panel(_))
            })
            .map(|(index, _)| index)
            .collect();
        for index in indices {
            if let Err(error) = self.sync_configured_panel(index) {
                self.error = Some(error);
                break;
            }
        }
    }

    fn panel_index(&self, owner: &SurfaceOwner) -> Option<usize> {
        self.surfaces.iter().position(|surface| {
            surface.output == owner.output
                && surface.key == owner.key
                && matches!(surface.role, Role::Panel(_))
        })
    }

    fn remove_panel(&mut self, owner: &SurfaceOwner, notify_hover: bool) {
        while let Some(index) = self.popup_index(owner) {
            self.remove_popup(index, notify_hover, false);
        }
        let Some(index) = self.panel_index(owner) else {
            return;
        };
        let wl = self.surfaces[index].role.surface().clone();
        let hovered = std::mem::take(&mut self.surfaces[index].hover_path);
        if let Err(error) = self.render.park() {
            self.error = Some(error);
        }
        self.surfaces.remove(index);
        self.hover.remove_owner(owner);
        if self.keyboard_surface.as_ref() == Some(&wl) {
            self.clear_keyboard_focus();
        }
        if notify_hover {
            for id in hovered {
                self.dispatch(owner.output.clone(), id, Event::Hover(false), None);
            }
        }
    }

    fn index(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|surface| surface.role.surface() == wl)
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.render.check_error()?;
        let now = Instant::now();
        let mut ready_outputs = Vec::new();
        for surface in &self.surfaces {
            if surface.configured
                && surface.dirty
                && !surface.waiting
                && surface.retry_at.is_none_or(|deadline| deadline <= now)
                && !ready_outputs.contains(&surface.output)
            {
                ready_outputs.push(surface.output.clone());
            }
        }
        for output in ready_outputs {
            let layer_shell_version = self.layer_shell_version;
            if self
                .configurations
                .advance_checked(&output, now, |surfaces| {
                    validate_surface_capabilities(surfaces, layer_shell_version)
                })?
            {
                // animated panel dimensions must reach layer shell before this
                // frame is laid out. pointer paths still update after present.
                let owners: Vec<_> = self
                    .surfaces
                    .iter()
                    .filter(|surface| {
                        surface.output == output && matches!(surface.role, Role::Panel(_))
                    })
                    .map(Surface::owner)
                    .collect();
                for owner in owners {
                    self.sync_panel(&owner, false)?;
                }
            }
        }
        let configurations = &self.configurations;
        let mut presented = Vec::new();
        for (index, surface) in self.surfaces.iter_mut().enumerate() {
            if !surface.configured || !surface.dirty || surface.waiting {
                continue;
            }
            if surface.retry_at.is_some_and(|deadline| deadline > now) {
                continue;
            }
            surface.retry_at = None;
            let Some(root) = configurations
                .surface_root(&surface.output, &surface.key)
                .and_then(|root| root.find(surface.root))
            else {
                continue;
            };
            let physical = (
                surface
                    .scale
                    .physical_size(surface.size.0)
                    .ok_or("invalid surface width")?,
                surface
                    .scale
                    .physical_size(surface.size.1)
                    .ok_or("invalid surface height")?,
            );
            if surface.window.is_none() {
                surface.window = Some(self.render.create_window(
                    surface.role.surface(),
                    physical.0,
                    physical.1,
                )?);
            }
            let window = surface
                .window
                .as_mut()
                .ok_or("render surface was not initialized")?;
            let mut scroll = surface.scroll.clone();
            let layout = self.render.layout(
                root,
                Size {
                    width: surface.size.0 as f32,
                    height: surface.size.1 as f32,
                },
                &mut scroll,
            )?;
            let wl = surface.role.surface().clone();
            let viewport = surface.viewport.clone();
            let logical = surface.size;
            let buffer_scale = surface.scale.factor().round() as i32;
            let result = self.render.draw(
                window,
                PaintFrame {
                    root,
                    layout: &layout,
                    physical,
                    scale: surface.scale.factor(),
                    input: surface.input,
                },
                || {
                    if let Some(viewport) = &viewport {
                        wl.set_buffer_scale(1);
                        viewport.set_destination(logical.0 as i32, logical.1 as i32);
                    } else {
                        wl.set_buffer_scale(buffer_scale);
                    }
                    wl.frame(qh, wl.clone());
                },
            )?;
            match result {
                FrameResult::Presented { redraw } => {
                    let animating = configurations.layout_is_animating(&surface.output, &layout);
                    surface.waiting = true;
                    surface.scroll = scroll;
                    surface.layout = Some(layout);
                    surface.dirty = redraw || animating;
                    presented.push(index);
                }
                #[cfg(feature = "vulkan")]
                FrameResult::RetryAfter(delay) => {
                    surface.waiting = false;
                    surface.dirty = true;
                    surface.retry_at = Some(Instant::now() + delay);
                }
            }
        }
        let mut owners: Vec<(SurfaceOwner, bool)> = Vec::new();
        for surface in presented
            .iter()
            .filter_map(|index| self.surfaces.get(*index))
        {
            let panel = matches!(surface.role, Role::Panel(_));
            let owner = surface.owner();
            if let Some((_, presented)) =
                owners.iter_mut().find(|(candidate, _)| candidate == &owner)
            {
                *presented |= panel;
            } else {
                owners.push((owner, panel));
            }
        }
        for (owner, parent_presented) in owners {
            self.sync_popup_placement(&owner, parent_presented, qh);
        }
        self.reconcile_presented_pointers(&presented);
        Ok(())
    }

    fn sync_popup_placement(
        &mut self,
        owner: &SurfaceOwner,
        parent_presented: bool,
        qh: &QueueHandle<Self>,
    ) {
        if !parent_presented
            && self.surfaces.iter().any(|surface| {
                surface.output == owner.output
                    && surface.key == owner.key
                    && matches!(surface.role, Role::Panel(_))
                    && surface.dirty
            })
        {
            return;
        }
        let Some((index, anchor, position)) = self.desired_popup_position(owner) else {
            return;
        };
        let Role::Popup(role) = &self.surfaces[index].role else {
            return;
        };
        if role.reposition.current() == position {
            if role.anchor != anchor
                && let Role::Popup(role) = &mut self.surfaces[index].role
            {
                role.anchor = anchor;
            }
            return;
        }
        let supported = role.shell.xdg_popup().version() >= 3;
        let token = if let Role::Popup(role) = &mut self.surfaces[index].role {
            role.reposition.request(position, supported)
        } else {
            None
        };
        let Some(token) = token else {
            return;
        };

        let positioner = self.xdg_shell.xdg_wm_base().create_positioner(qh, ());
        popup::configure(&positioner, position, true);
        let Role::Popup(role) = &mut self.surfaces[index].role else {
            positioner.destroy();
            return;
        };
        role.shell.reposition(&positioner, token);
        role.anchor = anchor;
        positioner.destroy();
    }

    fn desired_popup_position(
        &self,
        owner: &SurfaceOwner,
    ) -> Option<(usize, Option<NodeId>, PopupPosition)> {
        let index = self.surfaces.iter().position(|surface| {
            surface.output == owner.output
                && surface.key == owner.key
                && matches!(surface.role, Role::Popup(_))
        })?;
        let popup_surface = &self.surfaces[index];
        // reposition applies only after the popup's first buffer has been mapped.
        popup_surface.layout.as_ref()?;
        let Role::Popup(role) = &popup_surface.role else {
            return None;
        };
        let parent = self.surfaces.iter().find(|surface| {
            surface.output == owner.output
                && surface.key == owner.key
                && matches!(surface.role, Role::Panel(_))
        })?;
        let root = self
            .configurations
            .surface_root(&owner.output, &owner.key)?;
        let anchor = match &role.mode {
            PopupMode::Manual => {
                let (popup, anchor) = find_manual_popup(root, None)?;
                (popup == popup_surface.root).then_some(anchor)?
            }
            PopupMode::Automatic(key) | PopupMode::Hover(key) => Some(key.anchor),
        };
        let trigger = match anchor {
            Some(anchor) => parent
                .layout
                .as_ref()?
                .get(anchor)
                .map(|placement| placement.bounds)?,
            None => Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: parent.size.1 as f32,
            },
        };
        let node = root.find(popup_surface.root)?;
        let edge = root
            .panel
            .anchors
            .and_then(|anchors| anchors.exclusive_edge())
            .or(root.panel.anchors.is_none().then_some(root.panel.edge));
        Some((
            index,
            anchor,
            popup::resolve_for_surface(node, trigger, edge, parent.size)?,
        ))
    }

    fn dispatch(
        &mut self,
        output: wl_output::WlOutput,
        id: NodeId,
        event: Event,
        origin: Option<InputOrigin>,
    ) {
        let layer_shell_version = self.layer_shell_version;
        let result = self
            .configurations
            .dispatch_checked(&output, id, event, |surfaces| {
                validate_surface_capabilities(surfaces, layer_shell_version)
            });
        self.update_watch();
        let (changed, actions) = match result {
            Ok(result) => result,
            Err(error) => {
                eprintln!("thing: {error}");
                // hover cleanup can fail after its first valid refresh was
                // published, so reconcile whichever snapshot remains.
                self.invalidate_output(&output);
                return;
            }
        };
        // a successful handler can remove its own popup. request activation
        // before invalidation destroys the surface that received the input.
        for action in actions {
            let result = match action {
                Action::Niri(action) => self.niri.send(action),
                Action::Media { action, player } => self
                    .media
                    .send(action, player.as_deref())
                    .map_err(str::to_owned),
                Action::SystemTray { action, item } => {
                    send_system_tray_action(self.system_tray.as_ref(), action, &item)
                }
                Action::Launch { argv, cwd } => {
                    self.request_launch(LaunchRequest { argv, cwd }, origin.as_ref());
                    Ok(())
                }
                Action::DismissNotification { key } => self
                    .notifications
                    .as_ref()
                    .ok_or("notification server is disabled")
                    .and_then(|server| server.dismiss(key))
                    .map_err(str::to_owned),
                Action::InvokeNotification { key, action } => self
                    .notifications
                    .as_ref()
                    .ok_or("notification server is disabled")
                    .and_then(|server| server.invoke(key, action))
                    .map_err(str::to_owned),
                action @ (Action::SetVolume(_)
                | Action::SetMuted(_)
                | Action::SetDefaultSink(_)
                | Action::SetStreamVolume { .. }
                | Action::SetStreamMuted { .. }
                | Action::SetSourceVolume(_)
                | Action::SetSourceMuted(_)
                | Action::SetDefaultSource(_)) => self.audio.send(action).map_err(str::to_owned),
            };
            if let Err(error) = result {
                eprintln!("thing: {error}");
            }
        }
        if changed {
            self.invalidate_output(&output);
        }
    }

    fn input_origin(&self, surface: wl_surface::WlSurface, serial: u32) -> Option<InputOrigin> {
        Some(InputOrigin {
            seat: self.seat.clone()?,
            serial,
            surface,
        })
    }

    fn request_launch(&mut self, request: LaunchRequest, origin: Option<&InputOrigin>) {
        let (Some(activation), Some(origin)) = (self.activation.clone(), origin) else {
            self.spawn_launch(request, None);
            return;
        };
        let queue = self.queue.clone();
        let now = Instant::now();
        if let Err(request) = self.pending_launches.push(now, request, |id| {
            let token = activation.get_activation_token(&queue, id);
            token.set_serial(origin.serial, &origin.seat);
            token.set_surface(&origin.surface);
            token.commit();
            token
        }) {
            self.spawn_launch(request, None);
        }
    }

    fn spawn_launch(&mut self, request: LaunchRequest, token: Option<&str>) {
        let program = request.argv.first().map_or("<missing>", String::as_str);
        if let Err(error) = self
            .launcher
            .spawn(&request.argv, request.cwd.as_deref(), token)
        {
            if let Some(cwd) = &request.cwd {
                eprintln!("thing: cannot launch {program:?} in {cwd:?}: {error}");
            } else {
                eprintln!("thing: cannot launch {program:?}: {error}");
            }
        }
    }

    fn expire_launches(&mut self) {
        for (request, token) in self.pending_launches.expire(Instant::now()) {
            token.destroy();
            self.spawn_launch(request, None);
        }
    }

    fn reap_children(&mut self) {
        for (pid, error) in self.launcher.reap() {
            eprintln!("thing: cannot reap launched process {pid}: {error}");
        }
    }

    fn invalidate_output(&mut self, output: &wl_output::WlOutput) {
        while let Some(index) = self.surfaces.iter().position(|surface| {
            if &surface.output != output {
                return false;
            }
            surface.role.popup_mode().is_some_and(|mode| {
                !popup_is_current(
                    self.configurations.surface_root(output, &surface.key),
                    surface.root,
                    mode,
                )
            })
        }) {
            self.remove_popup(index, true, false);
        }
        let qh = self.queue.clone();
        if let Err(error) = self.sync_output_surfaces(output, false, &qh) {
            self.error = Some(error);
        }
        for surface in self
            .surfaces
            .iter_mut()
            .filter(|surface| &surface.output == output)
        {
            surface.dirty = true;
        }
        self.reconcile_hover_intent();
    }

    fn slider(&mut self, surface: usize, id: NodeId, x: f32) {
        let output = self.surfaces[surface].output.clone();
        let key = self.surfaces[surface].key.clone();
        let Some(node) = self
            .configurations
            .surface_root(&output, &key)
            .and_then(|root| root.find(id))
        else {
            return;
        };
        if node.kind != Kind::Slider || !node.enabled {
            return;
        }
        let Some(bounds) = self.surfaces[surface]
            .layout
            .as_ref()
            .and_then(|layout| layout.get(id))
            .map(|p| p.bounds)
        else {
            return;
        };
        let fraction = (x - bounds.x - 8.0) / (bounds.width - 16.0).max(1.0);
        let value = node.slider.value_at(fraction as f64);
        self.dispatch(output, id, Event::Change(value), None);
    }

    fn remove_popup(&mut self, index: usize, notify_runtime: bool, suppress_hover: bool) {
        let id = self.surfaces[index].root;
        let output = self.surfaces[index].output.clone();
        let wl = self.surfaces[index].role.surface().clone();
        let mode = self.surfaces[index].role.popup_mode().cloned();
        let hovered = std::mem::take(&mut self.surfaces[index].hover_path);
        if let Err(error) = self.render.park() {
            self.error = Some(error);
        }
        self.surfaces.remove(index);
        if let Some(PopupMode::Hover(key) | PopupMode::Automatic(key)) = mode {
            self.hover.close(&key, suppress_hover);
        }
        if self.keyboard_surface.as_ref() == Some(&wl) {
            self.clear_keyboard_focus();
        }
        if notify_runtime {
            for hovered in hovered {
                self.dispatch(output.clone(), hovered, Event::Hover(false), None);
            }
            self.dispatch(output, id, Event::Close, None);
        }
    }

    fn sync_popup(
        &mut self,
        parent: usize,
        serial: u32,
        activated: Option<NodeId>,
        qh: &QueueHandle<Self>,
    ) {
        let Some(parent_surface) = self.surfaces.get(parent) else {
            return;
        };
        let parent_wl = parent_surface.role.surface().clone();
        let owner = parent_surface.owner();
        if !matches!(parent_surface.role, Role::Panel(_)) {
            return;
        }

        if let Some((id, _)) = self
            .configurations
            .surface_root(&owner.output, &owner.key)
            .and_then(|root| find_manual_popup(root, None))
        {
            if self.surfaces.iter().any(|surface| {
                surface.output == owner.output
                    && surface.key == owner.key
                    && surface.root == id
                    && matches!(surface.role.popup_mode(), Some(PopupMode::Manual))
            }) {
                return;
            }
            if let Some(index) = self.popup_index(&owner) {
                self.remove_popup(index, true, true);
            }
            self.close_other_grabbed_popups(&owner);
            let Some((id, anchor)) = self
                .configurations
                .surface_root(&owner.output, &owner.key)
                .and_then(|root| find_manual_popup(root, None))
            else {
                return;
            };
            self.hover.reconcile(owner.clone(), None, Instant::now());
            self.create_popup(&parent_wl, id, anchor, PopupMode::Manual, Some(serial), qh);
            return;
        }

        let Some(anchor) = activated else {
            return;
        };
        let Some(target) = self
            .configurations
            .surface_root(&owner.output, &owner.key)
            .and_then(|root| automatic_target(root, owner.clone(), anchor, false))
        else {
            return;
        };
        if let Some(index) = self.popup_index(&owner) {
            let mode = self.surfaces[index].role.popup_mode().cloned();
            if mode.as_ref() == Some(&PopupMode::Automatic(target.key.clone())) {
                self.remove_popup(index, true, true);
                return;
            }
            let upgrading = mode.as_ref() == Some(&PopupMode::Hover(target.key.clone()));
            self.remove_popup(index, !upgrading, true);
        }
        if self
            .configurations
            .surface_root(&owner.output, &owner.key)
            .and_then(|root| find_manual_popup(root, None))
            .is_some()
        {
            let Some(parent) = self.index(&parent_wl) else {
                return;
            };
            self.sync_popup(parent, serial, None, qh);
            return;
        }
        let Some(target) = self
            .configurations
            .surface_root(&owner.output, &owner.key)
            .and_then(|root| automatic_target(root, owner.clone(), anchor, false))
        else {
            return;
        };
        self.close_other_grabbed_popups(&owner);
        self.hover.close(&target.key, true);
        self.create_popup(
            &parent_wl,
            target.key.popup,
            Some(target.key.anchor),
            PopupMode::Automatic(target.key),
            Some(serial),
            qh,
        );
    }

    fn create_popup(
        &mut self,
        parent_wl: &wl_surface::WlSurface,
        id: NodeId,
        anchor_id: Option<NodeId>,
        mode: PopupMode,
        serial: Option<u32>,
        qh: &QueueHandle<Self>,
    ) -> bool {
        let Some(parent) = self
            .index(parent_wl)
            .and_then(|index| self.surfaces.get(index))
        else {
            return false;
        };
        let Role::Panel(layer) = &parent.role else {
            return false;
        };
        let owner = parent.owner();
        let namespace = parent.namespace.clone();
        let parent_scale = parent.scale;
        let anchor = match anchor_id {
            Some(id) => {
                let Some(anchor) = parent
                    .layout
                    .as_ref()
                    .and_then(|layout| layout.get(id))
                    .map(|placement| placement.bounds)
                else {
                    return false;
                };
                anchor
            }
            None => Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: parent.size.1 as f32,
            },
        };
        let Some(position) = self
            .configurations
            .surface_root(&owner.output, &owner.key)
            .and_then(|root| {
                let edge = root
                    .panel
                    .anchors
                    .and_then(|anchors| anchors.exclusive_edge())
                    .or(root.panel.anchors.is_none().then_some(root.panel.edge));
                root.find(id)
                    .and_then(|node| popup::resolve_for_surface(node, anchor, edge, parent.size))
            })
        else {
            return false;
        };
        let layer = layer.clone();
        let seat = match serial {
            Some(_) => {
                let Some(seat) = self.seat.clone() else {
                    return false;
                };
                Some(seat)
            }
            None => None,
        };
        let positioner = self.xdg_shell.xdg_wm_base().create_positioner(qh, ());
        popup::configure(&positioner, position, true);
        let wl = self.compositor.create_surface(qh);
        let popup = XdgPopup::from_surface(None, &positioner, qh, wl, &self.xdg_shell);
        positioner.destroy();
        match popup {
            Ok(shell) => {
                layer.get_popup(shell.xdg_popup());
                if let Some((seat, serial)) = seat.as_ref().zip(serial) {
                    shell.xdg_popup().grab(seat, serial);
                }
                let surface = self.make_surface(
                    Role::Popup(PopupRole {
                        shell,
                        mode,
                        grabbed: serial.is_some(),
                        anchor: anchor_id,
                        reposition: popup::RepositionState::new(position),
                    }),
                    SurfaceInit {
                        owner,
                        namespace,
                        root: id,
                        size: (position.size.0 as u32, position.size.1 as u32),
                        scale: parent_scale,
                    },
                    qh,
                );
                surface.role.surface().commit();
                self.surfaces.push(surface);
                true
            }
            Err(error) => {
                eprintln!("thing: cannot open popup: {error}");
                false
            }
        }
    }

    fn popup_index(&self, owner: &SurfaceOwner) -> Option<usize> {
        self.surfaces.iter().position(|surface| {
            surface.output == owner.output
                && surface.key == owner.key
                && surface.role.popup_mode().is_some()
        })
    }

    fn close_other_grabbed_popups(&mut self, owner: &SurfaceOwner) {
        while let Some(index) = self.surfaces.iter().position(|surface| {
            surface.owner() != *owner && matches!(&surface.role, Role::Popup(role) if role.grabbed)
        }) {
            self.remove_popup(index, true, true);
        }
    }

    fn process_hover(&mut self, qh: &QueueHandle<Self>) {
        let now = Instant::now();
        for _ in 0..4 {
            let Some(command) = self.hover.take_due(now) else {
                break;
            };
            match command {
                HoverCommand::Close(key) => {
                    if let Some(index) = self.surfaces.iter().position(|surface| {
                        surface.role.popup_mode() == Some(&PopupMode::Hover(key.clone()))
                    }) {
                        self.remove_popup(index, true, false);
                    }
                }
                HoverCommand::Open(target) => {
                    if !self.hover.wants(&target.key) {
                        continue;
                    }
                    let Some(current) = self.current_hover_target(&target.key) else {
                        self.hover.close(&target.key, true);
                        continue;
                    };
                    if !self.hover_layout_ready(&current.key) {
                        self.hover.defer(current);
                        break;
                    }
                    if let Some(index) = self.popup_index(&current.key.owner) {
                        if !matches!(
                            self.surfaces[index].role.popup_mode(),
                            Some(PopupMode::Hover(_))
                        ) {
                            continue;
                        }
                        self.remove_popup(index, true, false);
                        self.reconcile_hover_intent();
                        if !self.hover.wants(&current.key)
                            || self.current_hover_target(&current.key).is_none()
                        {
                            continue;
                        }
                        if !self.hover_layout_ready(&current.key) {
                            self.hover.defer(current);
                            break;
                        }
                    }
                    let Some(parent) = self.surfaces.iter().find(|surface| {
                        surface.output == current.key.owner.output
                            && surface.key == current.key.owner.key
                            && matches!(surface.role, Role::Panel(_))
                    }) else {
                        self.hover.close(&current.key, true);
                        continue;
                    };
                    let parent_wl = parent.role.surface().clone();
                    let key = current.key.clone();
                    if self.create_popup(
                        &parent_wl,
                        key.popup,
                        Some(key.anchor),
                        PopupMode::Hover(key.clone()),
                        None,
                        qh,
                    ) {
                        if let Some(previous) = self.hover.opened(current)
                            && previous != key
                        {
                            self.hover.close(&previous, false);
                        }
                    } else {
                        self.hover.close(&key, true);
                    }
                }
            }
        }
    }

    fn current_hover_target(
        &self,
        key: &HoverKey<SurfaceOwner>,
    ) -> Option<HoverTarget<SurfaceOwner>> {
        let target = automatic_target(
            self.configurations
                .surface_root(&key.owner.output, &key.owner.key)?,
            key.owner.clone(),
            key.anchor,
            true,
        )?;
        if &target.key != key {
            return None;
        }
        Some(target)
    }

    fn hover_layout_ready(&self, key: &HoverKey<SurfaceOwner>) -> bool {
        self.surfaces
            .iter()
            .find(|surface| {
                surface.output == key.owner.output
                    && surface.key == key.owner.key
                    && matches!(surface.role, Role::Panel(_))
            })
            .is_some_and(|surface| {
                !surface.dirty
                    && surface
                        .layout
                        .as_ref()
                        .is_some_and(|layout| layout.get(key.anchor).is_some())
            })
    }

    fn reconcile_pointer(&mut self) {
        self.reconcile_pointers(None);
    }

    fn reconcile_presented_pointers(&mut self, presented: &[usize]) {
        if !presented.is_empty() {
            self.reconcile_pointers(Some(presented));
        }
    }

    fn reconcile_pointers(&mut self, selected: Option<&[usize]>) {
        let mut leaving = Vec::new();
        let mut entering = Vec::new();
        for (index, surface) in self.surfaces.iter_mut().enumerate() {
            if selected.is_some_and(|selected| !selected.contains(&index)) {
                continue;
            }
            let Some(root) = self
                .configurations
                .surface_root(&surface.output, &surface.key)
                .and_then(|root| root.find(surface.root))
            else {
                continue;
            };
            let hit = surface.pointer_position.and_then(|point| {
                surface
                    .layout
                    .as_ref()
                    .and_then(|layout| layout.hit_test(point))
            });
            let mut path = hit
                .and_then(|id| interactive_path(root, id))
                .unwrap_or_default();
            if surface.pointer_position.is_some()
                && root.kind == Kind::Popup
                && root.visible
                && root.enabled
            {
                path.insert(0, root.id);
            }
            let hit = hit.filter(|id| path.contains(id));
            if surface.input.hovered != hit {
                surface.input.hovered = hit;
                surface.dirty = true;
            }
            for id in surface.hover_path.iter().filter(|id| !path.contains(id)) {
                leaving.push((surface.output.clone(), *id));
            }
            for id in path.iter().filter(|id| !surface.hover_path.contains(id)) {
                entering.push((surface.output.clone(), *id));
            }
            surface.hover_path = path;
        }
        for (output, id) in leaving {
            self.dispatch(output, id, Event::Hover(false), None);
        }
        for (output, id) in entering {
            self.dispatch(output, id, Event::Hover(true), None);
        }
        self.reconcile_hover_intent();
    }

    fn reconcile_hover_intent(&mut self) {
        let pointer = self.surfaces.iter().find_map(|surface| {
            surface.pointer_position?;
            if let Some(PopupMode::Hover(key)) = surface.role.popup_mode() {
                return self.current_hover_target(key);
            }
            if !matches!(surface.role, Role::Panel(_)) {
                return None;
            }
            surface.hover_path.iter().rev().find_map(|anchor| {
                automatic_target(
                    self.configurations
                        .surface_root(&surface.output, &surface.key)?,
                    surface.owner(),
                    *anchor,
                    true,
                )
            })
        });
        let keyboard = self.keyboard_hover_target();
        let mut owners: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| matches!(surface.role, Role::Panel(_)))
            .map(Surface::owner)
            .collect();
        for owner in self.hover.owners() {
            if !owners.contains(owner) {
                owners.push(owner.clone());
            }
        }
        let now = Instant::now();
        for owner in owners {
            let blocked = self.surfaces.iter().any(|surface| {
                surface.owner() == owner
                    && matches!(
                        surface.role.popup_mode(),
                        Some(PopupMode::Manual | PopupMode::Automatic(_))
                    )
            });
            let desired = (!blocked).then(|| {
                pointer
                    .as_ref()
                    .filter(|target| target.key.owner == owner)
                    .cloned()
                    .or_else(|| {
                        keyboard
                            .as_ref()
                            .filter(|target| target.key.owner == owner)
                            .cloned()
                    })
            });
            let desired = desired.flatten();
            let ready = desired
                .as_ref()
                .map(|target| target.key.clone())
                .filter(|key| self.hover_layout_ready(key));
            self.hover.reconcile(owner, desired, now);
            if let Some(key) = ready {
                self.hover.resume(&key, now);
            }
        }
    }

    fn keyboard_hover_target(&self) -> Option<HoverTarget<SurfaceOwner>> {
        self.surfaces.iter().find_map(|surface| {
            if !matches!(surface.role, Role::Panel(_))
                || self.keyboard_surface.as_ref() != Some(surface.role.surface())
            {
                return None;
            }
            let anchor = surface.input.keyboard_focused()?;
            automatic_target(
                self.configurations
                    .surface_root(&surface.output, &surface.key)?,
                surface.owner(),
                anchor,
                true,
            )
        })
    }

    fn clear_pointer_state(&mut self, preserve_keyboard_hover: bool) {
        let mut leaving = Vec::new();
        for surface in &mut self.surfaces {
            leaving.extend(
                std::mem::take(&mut surface.hover_path)
                    .into_iter()
                    .map(|id| (surface.output.clone(), id)),
            );
            surface.pointer_position = None;
            surface.input.hovered = None;
            surface.input.pressed = None;
            surface.press_serial = None;
            surface.pressed_button = None;
            surface.dirty = true;
        }
        for (output, id) in leaving {
            self.dispatch(output, id, Event::Hover(false), None);
        }
        if preserve_keyboard_hover && self.keyboard_hover_target().is_some() {
            self.reconcile_hover_intent();
            return;
        }
        self.hover.clear();
        while let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| matches!(surface.role.popup_mode(), Some(PopupMode::Hover(_))))
        {
            self.remove_popup(index, true, false);
        }
        self.hover.clear();
        self.reconcile_hover_intent();
    }

    fn clear_keyboard_focus(&mut self) {
        self.clear_key_repeat();
        self.keyboard_surface = None;
        self.modifiers = Modifiers::default();
    }

    fn clear_key_repeat(&mut self) {
        let action = self.repeat.clear();
        self.apply_repeat_timer(action);
    }

    fn apply_repeat_timer(&mut self, action: RepeatTimerAction) {
        match action {
            RepeatTimerAction::None => {}
            RepeatTimerAction::Disarm => {
                if let Some(token) = self.repeat_token.take() {
                    self.loop_handle.remove(token);
                }
            }
            RepeatTimerAction::Arm(delay) => {
                if let Some(token) = self.repeat_token.take() {
                    self.loop_handle.remove(token);
                }
                let timer = Timer::from_duration(delay);
                match self
                    .loop_handle
                    .insert_source(timer, |_, _, app| app.dispatch_key_repeat())
                {
                    Ok(token) => self.repeat_token = Some(token),
                    Err(error) => {
                        self.error = Some(format!("cannot start keyboard repeat: {error}"));
                    }
                }
            }
        }
    }

    fn finish_key_press(&mut self, event: KeyEvent, key_repeats: bool, action_repeats: bool) {
        let action = match self.keyboard_surface.clone() {
            Some(surface) => self
                .repeat
                .press(event, surface, key_repeats, action_repeats),
            None if key_repeats => self.repeat.clear(),
            None => RepeatTimerAction::None,
        };
        self.apply_repeat_timer(action);
    }

    fn physical_key_repeats(&self, raw_code: u32) -> bool {
        let Some(keycode) = raw_code.checked_add(8).map(xkb::Keycode::new) else {
            return false;
        };
        self.keyboard_keymap
            .as_ref()
            .is_some_and(|keymap| keymap.key_repeats(keycode))
    }

    fn dispatch_key_repeat(&mut self) -> TimeoutAction {
        let Some(settings) = self.repeat.settings else {
            self.repeat_token = None;
            return TimeoutAction::Drop;
        };
        let Some(repeating) = self.repeat.key.as_mut() else {
            self.repeat_token = None;
            return TimeoutAction::Drop;
        };
        if !repeating.surface.is_alive()
            || self.keyboard_surface.as_ref() != Some(&repeating.surface)
        {
            self.clear_key_repeat();
            return TimeoutAction::Drop;
        }

        let elapsed = if repeating.first {
            repeating.first = false;
            settings.delay
        } else {
            settings.interval
        };
        repeating.event.time = repeating
            .event
            .time
            .wrapping_add(elapsed.as_millis().min(u128::from(u32::MAX)) as u32);
        let event = repeating.event.clone();
        let raw_code = event.raw_code;
        let surface = repeating.surface.clone();
        let qh = self.queue.clone();
        let repeats = self.key(event, 0, false, &qh);
        let session_is_current = self.repeat.key.as_ref().is_some_and(|repeating| {
            repeating.event.raw_code == raw_code && repeating.surface == surface
        });
        if !repeats || !session_is_current {
            self.clear_key_repeat();
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToDuration(settings.interval)
        }
    }

    fn key(&mut self, event: KeyEvent, serial: u32, direct: bool, qh: &QueueHandle<Self>) -> bool {
        let Some(index) = self.keyboard_surface.as_ref().and_then(|wl| self.index(wl)) else {
            return false;
        };
        if direct && event.keysym == Keysym::Escape {
            let owner = self.surfaces[index].owner();
            if self.surfaces[index].role.popup_mode().is_some() {
                self.remove_popup(index, true, true);
                self.reconcile_hover_intent();
                return false;
            }
            if let Some(popup) = self.surfaces.iter().position(|surface| {
                surface.owner() == owner
                    && matches!(surface.role.popup_mode(), Some(PopupMode::Hover(_)))
            }) {
                self.remove_popup(popup, true, true);
                self.reconcile_hover_intent();
                return false;
            }
        }
        let surface = &mut self.surfaces[index];
        if direct && (event.keysym == Keysym::Tab || event.keysym == Keysym::ISO_Left_Tab) {
            let order: Vec<_> = surface
                .layout
                .as_ref()
                .map(|layout| layout.focus_order().collect())
                .unwrap_or_default();
            if !order.is_empty() {
                let current = surface
                    .input
                    .focused
                    .and_then(|id| order.iter().position(|candidate| *candidate == id));
                let next = if self.modifiers.shift {
                    current.map_or(order.len() - 1, |n| (n + order.len() - 1) % order.len())
                } else {
                    current.map_or(0, |n| (n + 1) % order.len())
                };
                surface.input.focus_from_keyboard(order[next]);
                surface.dirty = true;
            }
            self.reconcile_hover_intent();
            return false;
        }
        let Some(id) = surface.input.focused else {
            return false;
        };
        if !focused_widget_is_interactive(surface.layout.as_ref(), id) {
            return false;
        }
        let output = surface.output.clone();
        let key = surface.key.clone();
        let Some(node) = self
            .configurations
            .surface_root(&output, &key)
            .and_then(|root| root.find(id))
        else {
            return false;
        };
        let event_surface = self.surfaces[index].role.surface().clone();
        let origin = keyboard_input_serial(direct, serial)
            .and_then(|serial| self.input_origin(event_surface.clone(), serial));
        let Some(action) = widget_key_action(node.kind, event.keysym, direct) else {
            return false;
        };
        match action {
            WidgetKeyAction::Activate => {
                self.surfaces[index].input.reveal_keyboard_focus();
                self.surfaces[index].dirty = true;
                self.dispatch(output, id, Event::Click, origin);
                if let Some(index) = self.index(&event_surface) {
                    self.sync_popup(index, serial, Some(id), qh);
                }
            }
            WidgetKeyAction::Slider(action) => {
                let slider = node.slider;
                let step = if slider.step > 0.0 {
                    slider.step
                } else {
                    (slider.max - slider.min) / 100.0
                };
                let value = match action {
                    SliderKeyAction::Decrease => slider.value - step,
                    SliderKeyAction::Increase => slider.value + step,
                    SliderKeyAction::Minimum => slider.min,
                    SliderKeyAction::Maximum => slider.max,
                };
                self.surfaces[index].input.reveal_keyboard_focus();
                self.surfaces[index].dirty = true;
                self.dispatch(
                    output,
                    id,
                    Event::Change(value.clamp(slider.min, slider.max)),
                    origin,
                );
            }
        }
        action.repeats()
    }
}

fn widget_key_action(kind: Kind, keysym: Keysym, direct: bool) -> Option<WidgetKeyAction> {
    match (kind, keysym) {
        (Kind::Button, Keysym::Return | Keysym::space) if direct => Some(WidgetKeyAction::Activate),
        (Kind::Slider, Keysym::Left | Keysym::Down) => {
            Some(WidgetKeyAction::Slider(SliderKeyAction::Decrease))
        }
        (Kind::Slider, Keysym::Right | Keysym::Up) => {
            Some(WidgetKeyAction::Slider(SliderKeyAction::Increase))
        }
        (Kind::Slider, Keysym::Home) if direct => {
            Some(WidgetKeyAction::Slider(SliderKeyAction::Minimum))
        }
        (Kind::Slider, Keysym::End) if direct => {
            Some(WidgetKeyAction::Slider(SliderKeyAction::Maximum))
        }
        _ => None,
    }
}

fn keyboard_input_serial(direct: bool, serial: u32) -> Option<u32> {
    direct.then_some(serial)
}

fn focused_widget_is_interactive(layout: Option<&Layout>, id: NodeId) -> bool {
    layout
        .and_then(|layout| layout.get(id))
        .is_some_and(|placement| placement.interactive)
}

fn panel_size(geometry: Geometry, output: (u32, u32)) -> (u32, u32) {
    let requested = geometry.requested_size;
    (
        if requested.0 == 0 {
            fill_dimension(output.0, geometry.margins.left, geometry.margins.right)
        } else {
            requested.0
        },
        if requested.1 == 0 {
            fill_dimension(output.1, geometry.margins.top, geometry.margins.bottom)
        } else {
            requested.1
        },
    )
}

fn fill_dimension(output: u32, start_margin: i32, end_margin: i32) -> u32 {
    (i64::from(output) - i64::from(start_margin) - i64::from(end_margin))
        .clamp(1, i64::from(u32::MAX)) as u32
}

fn configured_panel_size(
    configured: (u32, u32),
    geometry: Geometry,
    output: (u32, u32),
) -> (u32, u32) {
    let fallback = panel_size(geometry, output);
    (
        if configured.0 == 0 {
            fallback.0
        } else {
            configured.0
        },
        if configured.1 == 0 {
            fallback.1
        } else {
            configured.1
        },
    )
}

fn apply_panel_geometry(layer: &LayerSurface, geometry: Geometry) {
    layer.set_anchor(geometry.anchors);
    layer.set_size(geometry.requested_size.0, geometry.requested_size.1);
    layer.set_margin(
        geometry.margins.top,
        geometry.margins.right,
        geometry.margins.bottom,
        geometry.margins.left,
    );
    layer.set_exclusive_zone(geometry.exclusive_zone);
}

fn wayland_layer(layer: SurfaceLayer) -> Layer {
    match layer {
        SurfaceLayer::Background => Layer::Background,
        SurfaceLayer::Bottom => Layer::Bottom,
        SurfaceLayer::Top => Layer::Top,
        SurfaceLayer::Overlay => Layer::Overlay,
    }
}

fn validate_surface_capabilities(
    surfaces: &[SurfaceTree],
    layer_shell_version: u32,
) -> Result<(), String> {
    if layer_shell_version < 4
        && surfaces
            .iter()
            .any(|surface| surface.root.panel.keyboard_mode == KeyboardMode::OnDemand)
    {
        return Err("keyboard_mode 'on_demand' needs wlr-layer-shell version 4".into());
    }
    Ok(())
}

fn apply_keyboard_mode(layer: &LayerSurface, mode: KeyboardMode) -> Result<(), String> {
    if mode == KeyboardMode::OnDemand && layer_version(layer) < 4 {
        return Err("keyboard_mode 'on_demand' needs wlr-layer-shell version 4".into());
    }
    layer.set_keyboard_interactivity(match mode {
        KeyboardMode::None => KeyboardInteractivity::None,
        KeyboardMode::OnDemand => KeyboardInteractivity::OnDemand,
        KeyboardMode::Exclusive => KeyboardInteractivity::Exclusive,
    });
    Ok(())
}

fn panel_role_version(role: &Role) -> u32 {
    match role {
        Role::Panel(layer) => layer_version(layer),
        Role::Popup(_) => 0,
    }
}

fn layer_version(layer: &LayerSurface) -> u32 {
    match layer.kind() {
        SurfaceKind::Wlr(layer) => layer.version(),
        _ => 0,
    }
}

fn apply_input_region(
    compositor: &CompositorState,
    surface: &wl_surface::WlSurface,
    input: &PanelInputRegion,
    size: (u32, u32),
) -> Result<(), String> {
    match panel::input_region(input, size) {
        ResolvedInputRegion::Full => surface.set_input_region(None),
        ResolvedInputRegion::Region(rectangles) => {
            let region = Region::new(compositor)
                .map_err(|error| format!("cannot create panel input region: {error}"))?;
            for rect in rectangles {
                region.add(rect.x, rect.y, rect.width, rect.height);
            }
            surface.set_input_region(Some(region.wl_region()));
        }
    }
    Ok(())
}

fn send_system_tray_action(
    system_tray: Option<&SystemTray>,
    action: SystemTrayAction,
    item: &str,
) -> Result<(), String> {
    system_tray
        .ok_or_else(|| "system tray capability is disabled".to_owned())?
        .send(action, item)
        .map_err(str::to_owned)
}

fn earliest_deadline(
    reload: Option<Instant>,
    hover: Option<Instant>,
    launch: Option<Instant>,
    retries: impl IntoIterator<Item = Instant>,
) -> Option<Instant> {
    reload
        .into_iter()
        .chain(hover)
        .chain(launch)
        .chain(retries)
        .min()
}

fn output_context(info: OutputInfo) -> Option<OutputContext> {
    let integer_scale = info.scale_factor.clamp(1, 16) as u32;
    let scale = Scale::from_120ths(integer_scale * 120).unwrap_or_default();
    let logical_size = info.logical_size.and_then(valid_dimensions).or_else(|| {
        let dimensions = info
            .modes
            .iter()
            .find(|mode| mode.current)
            .map(|mode| mode.dimensions)?;
        let rotated = matches!(
            info.transform,
            wl_output::Transform::_90
                | wl_output::Transform::_270
                | wl_output::Transform::Flipped90
                | wl_output::Transform::Flipped270
        );
        fallback_logical_size(dimensions, integer_scale, rotated)
    })?;
    Some(OutputContext {
        name: info.name,
        width: logical_size.0,
        height: logical_size.1,
        scale,
    })
}

fn valid_dimensions((width, height): (i32, i32)) -> Option<(u32, u32)> {
    Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

fn fallback_logical_size(dimensions: (i32, i32), scale: u32, rotated: bool) -> Option<(u32, u32)> {
    let (mut width, mut height) = valid_dimensions(dimensions)?;
    if rotated {
        std::mem::swap(&mut width, &mut height);
    }
    Some((width / scale, height / scale)).filter(|(width, height)| *width > 0 && *height > 0)
}

#[cfg(test)]
mod output_tests {
    use std::{
        fs,
        time::{Duration, Instant},
    };

    use super::{
        PanelState, configured_panel_size, earliest_deadline, fallback_logical_size,
        find_automatic_popup, find_manual_popup, interactive_path, panel_size,
        send_system_tray_action, valid_dimensions, validate_surface_capabilities,
    };
    use crate::{configuration::Configurations, panel};
    use tempfile::tempdir;
    use thing_core::{KeyboardMode, Kind, Node, NodeId, PanelEdge, PanelMargins, Scale};
    use thing_runtime::{
        Event, OutputContext, ServiceSnapshot, SurfaceKey, SurfaceTree, SystemTrayAction,
    };

    #[test]
    fn disabled_system_tray_rejects_actions_without_starting_a_worker() {
        assert_eq!(
            send_system_tray_action(None, SystemTrayAction::Activate, "tray1"),
            Err("system tray capability is disabled".into())
        );
    }

    #[test]
    fn renderer_retry_can_wake_before_hover_and_configuration_deadlines() {
        let now = Instant::now();
        let retry = now + Duration::from_millis(16);
        let hover = now + Duration::from_millis(50);
        let reload = now + Duration::from_millis(80);

        assert_eq!(
            earliest_deadline(Some(reload), Some(hover), None, [retry]),
            Some(retry)
        );
        assert_eq!(
            earliest_deadline(Some(reload), Some(hover), None, []),
            Some(hover)
        );
        assert_eq!(
            earliest_deadline(Some(reload), None, None, []),
            Some(reload)
        );
        assert_eq!(earliest_deadline(None, None, None, [retry]), Some(retry));
        let launch = now + Duration::from_millis(8);
        assert_eq!(
            earliest_deadline(Some(reload), Some(hover), Some(launch), [retry]),
            Some(launch)
        );
    }

    #[test]
    fn mode_fallback_applies_scale_and_rotation() {
        assert_eq!(
            fallback_logical_size((3840, 2160), 2, false),
            Some((1920, 1080))
        );
        assert_eq!(
            fallback_logical_size((3840, 2160), 2, true),
            Some((1080, 1920))
        );
    }

    #[test]
    fn panel_fill_axis_uses_the_output_size_and_margins() {
        let mut top = Node::new(NodeId(1), Kind::Panel);
        top.style.height = Some(52.0);
        top.panel.margins = PanelMargins {
            right: 6,
            left: 4,
            ..PanelMargins::default()
        };
        assert_eq!(
            panel_size(panel::geometry(&top, None), (1920, 1080)),
            (1910, 52)
        );

        top.style.width = Some(480.0);
        assert_eq!(
            panel_size(panel::geometry(&top, None), (1920, 1080)),
            (480, 52)
        );
    }

    #[test]
    fn zero_configure_dimensions_follow_the_current_panel_request() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        root.panel.edge = PanelEdge::Left;
        root.panel.margins = PanelMargins {
            top: 5,
            bottom: 7,
            ..PanelMargins::default()
        };
        root.style.width = Some(48.0);
        let side = panel::geometry(&root, None);
        assert_eq!(
            configured_panel_size((48, 0), side, (1920, 1080)),
            (48, 1068)
        );

        root.panel.edge = PanelEdge::Top;
        root.style.width = None;
        root.style.height = Some(80.0);
        let resized_top = panel::geometry(&root, None);
        assert_eq!(
            configured_panel_size((0, 0), resized_top, (1920, 1080)),
            (1920, 80)
        );
    }

    #[test]
    fn explicit_exclusivity_differs_from_an_equal_automatic_value() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        root.style.height = Some(52.0);
        let automatic = PanelState::from_root(&root);
        assert_eq!(panel::geometry(&root, Some((1920, 50))).exclusive_zone, 50);

        root.panel.exclusive_zone = Some(52);
        let explicit = PanelState::from_root(&root);
        assert_eq!(automatic.geometry, explicit.geometry);
        assert_ne!(automatic, explicit);
        assert_eq!(panel::geometry(&root, Some((1920, 50))).exclusive_zone, 52);

        root.panel.exclusive_zone = None;
        let automatic_again = PanelState::from_root(&root);
        assert_ne!(explicit, automatic_again);
        assert_eq!(panel::geometry(&root, Some((1920, 50))).exclusive_zone, 50);
    }

    #[test]
    fn rejects_unknown_or_invalid_output_dimensions() {
        assert_eq!(valid_dimensions((0, 1080)), None);
        assert_eq!(valid_dimensions((-1, 1080)), None);
        assert_eq!(fallback_logical_size((1, 1), 2, false), None);
    }

    #[test]
    fn preflights_keyboard_modes_against_layer_shell_version() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let surfaces = |root| {
            vec![SurfaceTree {
                key: SurfaceKey::Default,
                namespace: "thing".into(),
                root,
            }]
        };

        assert!(validate_surface_capabilities(&surfaces(root.clone()), 3).is_err());
        assert!(validate_surface_capabilities(&surfaces(root.clone()), 4).is_ok());
        root.panel.keyboard_mode = KeyboardMode::None;
        assert!(validate_surface_capabilities(&surfaces(root), 1).is_ok());
    }

    #[test]
    fn reactive_capability_rejection_preserves_the_surface_and_can_recover() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local demand = ui.state(false)
            return ui.panel {
                keyboard_mode = function()
                    return if demand:get() then 'on_demand' else 'none'
                end,
                children = { ui.button {
                    on_click = function() demand:set(not demand:get()) end,
                } },
            }
            "#,
        )
        .unwrap();
        let output = OutputContext {
            name: Some("DP-1".into()),
            width: 1920,
            height: 1080,
            scale: Scale::default(),
        };
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output).unwrap();
        let button = configurations.root(&1).unwrap().children[0].id;
        let validate = |surfaces: &[SurfaceTree]| validate_surface_capabilities(surfaces, 3);

        let error = configurations
            .dispatch_checked(&1, button, Event::Click, validate)
            .unwrap_err();
        assert!(error.contains("on_demand"), "{error}");
        assert_eq!(
            configurations.root(&1).unwrap().panel.keyboard_mode,
            KeyboardMode::None
        );

        let (changed, actions) = configurations
            .dispatch_checked(&1, button, Event::Click, validate)
            .unwrap();
        assert!(!changed);
        assert!(actions.is_empty());
        assert_eq!(
            configurations.root(&1).unwrap().panel.keyboard_mode,
            KeyboardMode::None
        );
    }

    #[test]
    fn popup_lookup_keeps_manual_and_automatic_activation_separate() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut button = Node::new(NodeId(2), Kind::Button);
        let mut automatic = Node::new(NodeId(3), Kind::Popup);
        automatic.popup.open_on_hover = true;
        button.children.push(automatic);
        button.children.push(Node::new(NodeId(4), Kind::Popup));
        root.children.push(button);

        assert_eq!(
            find_manual_popup(&root, None),
            Some((NodeId(4), Some(NodeId(2))))
        );
        assert_eq!(
            find_automatic_popup(&root, NodeId(2), None).map(|node| node.id),
            Some(NodeId(3))
        );
    }

    #[test]
    fn interactive_path_keeps_ancestors_hovered_across_descendants() {
        let mut root = Node::new(NodeId(1), Kind::Panel);
        let mut outer = Node::new(NodeId(2), Kind::Button);
        outer.children.push(Node::new(NodeId(3), Kind::Button));
        root.children.push(outer);

        assert_eq!(
            interactive_path(&root, NodeId(3)),
            Some(vec![NodeId(2), NodeId(3)])
        );
    }
}

fn find_manual_popup(node: &Node, anchor: Option<NodeId>) -> Option<(NodeId, Option<NodeId>)> {
    if !node.visible {
        return None;
    }
    if node.kind == Kind::Popup {
        return (!node.popup.is_automatic()).then_some((node.id, anchor));
    }
    let anchor = if node.kind == Kind::Button {
        Some(node.id)
    } else {
        anchor
    };
    node.children
        .iter()
        .find_map(|child| find_manual_popup(child, anchor))
}

fn find_automatic_popup(
    node: &Node,
    wanted_anchor: NodeId,
    anchor: Option<NodeId>,
) -> Option<&Node> {
    if !node.visible || !node.enabled {
        return None;
    }
    if node.kind == Kind::Popup {
        return (node.popup.is_automatic() && anchor == Some(wanted_anchor)).then_some(node);
    }
    let anchor = if node.kind == Kind::Button {
        Some(node.id)
    } else {
        anchor
    };
    node.children
        .iter()
        .find_map(|child| find_automatic_popup(child, wanted_anchor, anchor))
}

fn automatic_target(
    root: &Node,
    owner: SurfaceOwner,
    anchor: NodeId,
    hover: bool,
) -> Option<HoverTarget<SurfaceOwner>> {
    let popup = find_automatic_popup(root, anchor, None)?;
    if if hover {
        !popup.popup.open_on_hover
    } else {
        !popup.popup.open_on_click
    } {
        return None;
    }
    Some(HoverTarget {
        key: HoverKey {
            owner,
            anchor,
            popup: popup.id,
        },
        open_delay: Duration::from_millis(u64::from(popup.popup.open_delay_ms)),
        close_delay: Duration::from_millis(u64::from(popup.popup.close_delay_ms)),
    })
}

fn popup_is_current(root: Option<&Node>, id: NodeId, mode: &PopupMode) -> bool {
    let Some(root) = root else {
        return false;
    };
    match mode {
        PopupMode::Manual => find_manual_popup(root, None).is_some_and(|(popup, _)| popup == id),
        PopupMode::Automatic(key) => automatic_target(root, key.owner.clone(), key.anchor, false)
            .is_some_and(|target| target.key.popup == id),
        PopupMode::Hover(key) => automatic_target(root, key.owner.clone(), key.anchor, true)
            .is_some_and(|target| target.key.popup == id),
    }
}

fn interactive_path(root: &Node, target: NodeId) -> Option<Vec<NodeId>> {
    fn visit(node: &Node, target: NodeId, path: &mut Vec<NodeId>) -> bool {
        if !node.visible || !node.enabled {
            return false;
        }
        let interactive = node.kind.is_interactive();
        if interactive {
            path.push(node.id);
        }
        if node.id == target
            || node
                .children
                .iter()
                .filter(|child| child.kind != Kind::Popup)
                .any(|child| visit(child, target, path))
        {
            return true;
        }
        if interactive {
            path.pop();
        }
        false
    }

    let mut path = Vec::new();
    visit(root, target, &mut path).then_some(path)
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        wl: &wl_surface::WlSurface,
        scale: i32,
    ) {
        if let Some(index) = self.index(wl) {
            let surface = &mut self.surfaces[index];
            if surface.fractional.is_none() {
                let scale = Scale::from_120ths(scale.clamp(1, 16) as u32 * 120).unwrap_or_default();
                surface.scale = scale;
                surface.dirty = true;
                let panel = matches!(surface.role, Role::Panel(_));
                let output = surface.output.clone();
                if panel {
                    self.set_output_scale(&output, scale);
                }
            }
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, wl: &wl_surface::WlSurface, _: u32) {
        if let Some(index) = self.index(wl) {
            self.surfaces[index].waiting = false;
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.loading_outputs {
            return;
        }
        if let Err(error) = self.add_output(output, qh) {
            eprintln!("thing: cannot configure new output: {error}");
        }
    }
    fn update_output(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.loading_outputs {
            return;
        }
        let Some(context) = self.output_context(&output) else {
            return;
        };
        if !self.configurations.contains(&output) {
            if let Err(error) = self.add_output(output, qh) {
                eprintln!("thing: cannot configure output: {error}");
            }
            return;
        }
        let layer_shell_version = self.layer_shell_version;
        let result = self
            .configurations
            .set_output_checked(&output, context, |surfaces| {
                validate_surface_capabilities(surfaces, layer_shell_version)
            });
        self.update_watch();
        match result {
            Ok(true) => self.invalidate_output(&output),
            Ok(false) => {}
            Err(error) => eprintln!("thing: cannot update output: {error}"),
        }
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let owners: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| surface.output == output && matches!(surface.role, Role::Panel(_)))
            .map(Surface::owner)
            .collect();
        for owner in owners {
            self.remove_panel(&owner, false);
        }
        while let Some(index) = self
            .surfaces
            .iter()
            .rposition(|surface| surface.output == output)
        {
            self.remove_popup(index, false, false);
        }
        self.closed_surfaces.retain(|owner| owner.output != output);
        self.configurations.remove(&output);
        self.update_watch();
        if self
            .keyboard_surface
            .as_ref()
            .is_some_and(|wl| self.index(wl).is_none())
        {
            self.clear_keyboard_focus();
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(index) = self.index(layer.wl_surface()) {
            let owner = self.surfaces[index].owner();
            if !self.closed_surfaces.contains(&owner) {
                self.closed_surfaces.push(owner.clone());
            }
            self.remove_panel(&owner, true);
        }
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        config: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(index) = self.index(layer.wl_surface()) {
            let resolved_size = {
                let surface = &self.surfaces[index];
                surface.panel.as_ref().and_then(|panel| {
                    self.configurations.output(&surface.output).map(|output| {
                        configured_panel_size(
                            config.new_size,
                            panel.geometry,
                            (output.width, output.height),
                        )
                    })
                })
            };
            {
                let surface = &mut self.surfaces[index];
                surface.size = match resolved_size {
                    Some(size) => size,
                    None => (
                        if config.new_size.0 == 0 {
                            surface.size.0
                        } else {
                            config.new_size.0
                        },
                        if config.new_size.1 == 0 {
                            surface.size.1
                        } else {
                            config.new_size.1
                        },
                    ),
                };
                surface.configured = true;
                surface.dirty = true;
            }
            if let Err(error) = self.sync_configured_panel(index) {
                self.error = Some(error);
            }
        }
    }
}

// `XdgShell`'s dispatch adapter also exposes toplevel events. this host creates
// only layers and popups, so it has no toplevel windows to configure or close.
impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &XdgWindow) {}
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &XdgWindow,
        _: WindowConfigure,
        _: u32,
    ) {
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &XdgPopup,
        config: PopupConfigure,
    ) {
        if let Some(index) = self.index(popup.wl_surface()) {
            let surface = &mut self.surfaces[index];
            let size = (config.width.max(1) as u32, config.height.max(1) as u32);
            let initial = !surface.configured;
            if surface.size != size {
                surface.size = size;
                surface.dirty = true;
            }
            surface.configured = true;
            surface.dirty |= initial;
        }
    }
    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &XdgPopup) {
        if let Some(index) = self.index(popup.wl_surface()) {
            self.remove_popup(index, true, true);
            self.reconcile_hover_intent();
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if self.seat.as_ref().is_some_and(|active| active != &seat) {
            return;
        }
        self.seat = Some(seat.clone());
        match capability {
            Capability::Pointer if self.pointer.is_none() => {
                match self.seats.get_pointer(qh, &seat) {
                    Ok(pointer) => self.pointer = Some(pointer),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            Capability::Keyboard if self.keyboard.is_none() => {
                self.clear_key_repeat();
                self.repeat.settings = None;
                self.keyboard_keymap = None;
                match self.seats.get_keyboard(qh, &seat, None) {
                    Ok(keyboard) => self.keyboard = Some(keyboard),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            _ => {}
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
            self.clear_pointer_state(true);
        }
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
            self.repeat.settings = None;
            self.keyboard_keymap = None;
            self.clear_keyboard_focus();
            self.reconcile_hover_intent();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.as_ref() == Some(&seat) {
            self.seat = None;
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
            self.repeat.settings = None;
            self.keyboard_keymap = None;
            self.clear_keyboard_focus();
            self.clear_pointer_state(false);
            for surface in &mut self.surfaces {
                surface.input = Interaction::default();
                surface.dirty = true;
            }
        }
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(index) = self.index(&event.surface) else {
                continue;
            };
            let point = Point {
                x: event.position.0 as f32,
                y: event.position.1 as f32,
            };
            let hit = self.surfaces[index]
                .layout
                .as_ref()
                .and_then(|layout| layout.hit_test(point));
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    let surface = &mut self.surfaces[index];
                    surface.pointer_position = Some(point);
                    if surface.pressed_button == Some(0x110)
                        && let Some(id) = surface.input.pressed
                    {
                        self.slider(index, id, point.x);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.surfaces[index].pointer_position = None;
                    self.surfaces[index].dirty = true;
                }
                PointerEventKind::Press { button, serial, .. }
                    if matches!(button, 0x110..=0x112) =>
                {
                    self.clear_key_repeat();
                    let surface = &mut self.surfaces[index];
                    surface.pointer_position = Some(point);
                    surface.input.pressed = hit;
                    surface.pressed_button = Some(button);
                    surface.press_serial = Some(serial);
                    surface.input.focus_from_pointer(hit);
                    surface.dirty = true;
                    if button == 0x110
                        && let Some(id) = hit
                    {
                        self.slider(index, id, point.x);
                    }
                }
                PointerEventKind::Release {
                    button,
                    serial: release_serial,
                    ..
                } if self.surfaces[index].pressed_button == Some(button) => {
                    self.surfaces[index].pressed_button = None;
                    self.surfaces[index].pointer_position = Some(point);
                    let pressed = self.surfaces[index].input.pressed.take();
                    let popup_serial = self.surfaces[index].press_serial.take();
                    let output = self.surfaces[index].output.clone();
                    let event_surface = self.surfaces[index].role.surface().clone();
                    self.surfaces[index].dirty = true;
                    if let Some(id) = pressed.filter(|id| Some(*id) == hit) {
                        self.dispatch(
                            output,
                            id,
                            match button {
                                0x111 => Event::SecondaryClick,
                                0x112 => Event::MiddleClick,
                                _ => Event::Click,
                            },
                            self.input_origin(event_surface.clone(), release_serial),
                        );
                        if let Some((index, serial)) = self.index(&event_surface).zip(popup_serial)
                        {
                            self.sync_popup(index, serial, (button == 0x110).then_some(id), qh);
                        }
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    self.surfaces[index].pointer_position = Some(point);
                    let x = scroll_steps(horizontal);
                    let y = scroll_steps(vertical);
                    if x == 0.0 && y == 0.0 {
                        continue;
                    }
                    let output = self.surfaces[index].output.clone();
                    let handler = self.surfaces[index]
                        .layout
                        .as_ref()
                        .and_then(|layout| layout.scroll_event_target(point))
                        .filter(|id| self.configurations.has_scroll_handler(&output, *id));
                    if let Some(id) = handler {
                        self.dispatch(output, id, Event::Scroll { x, y }, None);
                    } else if let Some(delta) = scroll_distance(y) {
                        let surface = &mut self.surfaces[index];
                        let changed = surface.layout.as_ref().is_some_and(|layout| {
                            layout.scroll_y(point, delta, &mut surface.scroll)
                        });
                        if changed {
                            surface.dirty = true;
                        }
                    }
                }
                _ => {}
            }
        }
        self.reconcile_pointer();
    }
}

fn scroll_steps(axis: AxisScroll) -> f64 {
    if axis.value120 != 0 {
        f64::from(axis.value120) / 120.0
    } else if axis.discrete != 0 {
        f64::from(axis.discrete)
    } else {
        axis.absolute / 15.0
    }
}

fn scroll_distance(steps: f64) -> Option<f32> {
    const PIXELS_PER_STEP: f64 = 40.0;
    let distance = (steps * PIXELS_PER_STEP) as f32;
    (distance.is_finite() && distance != 0.0).then_some(distance)
}

#[cfg(test)]
mod input_tests {
    use std::{num::NonZeroU32, time::Duration};

    use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, RepeatInfo};
    use thing_core::{Kind, Node, NodeId};
    use thing_ui::{Layout, Measurer, Size};

    use super::{
        RepeatSession, RepeatTimerAction, SliderKeyAction, WidgetKeyAction,
        focused_widget_is_interactive, keyboard_input_serial, scroll_distance, widget_key_action,
    };

    struct FixedMeasurer;

    impl Measurer for FixedMeasurer {
        fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
            Size {
                width: 20.0,
                height: 20.0,
            }
        }
    }

    fn slider_layout(parent_enabled: bool, slider_visible: bool) -> Layout {
        let mut panel = Node::new(NodeId(1), Kind::Panel);
        panel.style.width = Some(200.0);
        panel.style.height = Some(50.0);
        let mut row = Node::new(NodeId(2), Kind::Row);
        row.enabled = parent_enabled;
        let mut slider = Node::new(NodeId(3), Kind::Slider);
        slider.visible = slider_visible;
        slider.style.width = Some(100.0);
        slider.style.height = Some(30.0);
        row.children.push(slider);
        panel.children.push(row);
        Layout::compute(
            &panel,
            Size {
                width: 200.0,
                height: 50.0,
            },
            &mut FixedMeasurer,
        )
        .unwrap()
    }

    fn key_event(raw_code: u32, keysym: Keysym) -> KeyEvent {
        KeyEvent {
            time: 10,
            raw_code,
            keysym,
            utf8: None,
        }
    }

    fn right_key(raw_code: u32) -> KeyEvent {
        key_event(raw_code, Keysym::Right)
    }

    fn enabled_repeat() -> RepeatInfo {
        RepeatInfo::Repeat {
            rate: NonZeroU32::new(25).unwrap(),
            delay: 600,
        }
    }

    #[test]
    fn scroll_steps_convert_to_finite_logical_distance() {
        assert_eq!(scroll_distance(1.0), Some(40.0));
        assert_eq!(scroll_distance(-0.5), Some(-20.0));
        assert_eq!(scroll_distance(0.0), None);
        assert_eq!(scroll_distance(f64::NAN), None);
        assert_eq!(scroll_distance(f64::INFINITY), None);
        assert_eq!(scroll_distance(f64::MAX), None);
    }

    #[test]
    fn button_activation_never_repeats() {
        assert_eq!(
            widget_key_action(Kind::Button, Keysym::Return, true),
            Some(WidgetKeyAction::Activate)
        );
        assert_eq!(
            widget_key_action(Kind::Button, Keysym::space, true),
            Some(WidgetKeyAction::Activate)
        );
        assert_eq!(widget_key_action(Kind::Button, Keysym::Return, false), None);
        assert_eq!(widget_key_action(Kind::Button, Keysym::space, false), None);
        assert!(!WidgetKeyAction::Activate.repeats());
    }

    #[test]
    fn slider_arrows_repeat_but_endpoints_do_not() {
        assert_eq!(
            widget_key_action(Kind::Slider, Keysym::Left, false),
            Some(WidgetKeyAction::Slider(SliderKeyAction::Decrease))
        );
        assert_eq!(
            widget_key_action(Kind::Slider, Keysym::Right, false),
            Some(WidgetKeyAction::Slider(SliderKeyAction::Increase))
        );
        assert_eq!(
            widget_key_action(Kind::Slider, Keysym::Home, true),
            Some(WidgetKeyAction::Slider(SliderKeyAction::Minimum))
        );
        assert_eq!(
            widget_key_action(Kind::Slider, Keysym::End, true),
            Some(WidgetKeyAction::Slider(SliderKeyAction::Maximum))
        );
        assert_eq!(widget_key_action(Kind::Slider, Keysym::Home, false), None);
        assert_eq!(widget_key_action(Kind::Slider, Keysym::End, false), None);
        assert!(WidgetKeyAction::Slider(SliderKeyAction::Decrease).repeats());
        assert!(WidgetKeyAction::Slider(SliderKeyAction::Increase).repeats());
        assert!(!WidgetKeyAction::Slider(SliderKeyAction::Minimum).repeats());
        assert!(!WidgetKeyAction::Slider(SliderKeyAction::Maximum).repeats());
    }

    #[test]
    fn repeated_keys_cannot_reuse_an_activation_serial() {
        assert_eq!(keyboard_input_serial(true, 42), Some(42));
        assert_eq!(keyboard_input_serial(false, 42), None);
    }

    #[test]
    fn keyboard_actions_require_an_interactive_focused_placement() {
        let enabled = slider_layout(true, true);
        assert!(focused_widget_is_interactive(Some(&enabled), NodeId(3)));

        let disabled = slider_layout(false, true);
        assert!(!focused_widget_is_interactive(Some(&disabled), NodeId(3)));

        let hidden = slider_layout(true, false);
        assert!(!focused_widget_is_interactive(Some(&hidden), NodeId(3)));
        assert!(!focused_widget_is_interactive(None, NodeId(3)));
    }

    #[test]
    fn repeat_session_transfers_ownership_without_losing_the_held_key() {
        let mut session = RepeatSession::<u8>::default();
        assert_eq!(session.update(RepeatInfo::Disable), RepeatTimerAction::None);
        assert_eq!(
            session.press(right_key(106), 7, true, true),
            RepeatTimerAction::None
        );
        assert!(session.accepts_compositor_repeat(106, &7));
        assert_eq!(session.key.as_ref().unwrap().event.raw_code, 106);

        assert_eq!(
            session.update(enabled_repeat()),
            RepeatTimerAction::Arm(Duration::from_millis(600))
        );
        assert!(session.host_armed);
        assert_eq!(session.key.as_ref().unwrap().surface, 7);
        assert!(!session.accepts_compositor_repeat(106, &7));

        assert_eq!(
            session.update(RepeatInfo::Disable),
            RepeatTimerAction::Disarm
        );
        assert!(!session.host_armed);
        assert_eq!(session.key.as_ref().unwrap().event.raw_code, 106);
        assert!(session.accepts_compositor_repeat(106, &7));
    }

    #[test]
    fn release_and_focus_loss_clear_the_repeat_session() {
        let mut session = RepeatSession::<u8>::default();
        assert_eq!(session.update(enabled_repeat()), RepeatTimerAction::None);
        assert_eq!(
            session.press(right_key(106), 7, true, true),
            RepeatTimerAction::Arm(Duration::from_millis(600))
        );
        assert_eq!(session.release(105), RepeatTimerAction::None);
        assert!(session.key.is_some());
        assert_eq!(session.release(106), RepeatTimerAction::Disarm);
        assert!(session.key.is_none());
        assert!(!session.host_armed);

        assert_eq!(
            session.press(right_key(106), 8, true, true),
            RepeatTimerAction::Arm(Duration::from_millis(600))
        );
        assert_eq!(session.clear(), RepeatTimerAction::Disarm);
        assert!(session.key.is_none());
        assert!(!session.host_armed);
    }

    #[test]
    fn physical_repeatability_controls_session_replacement() {
        let mut session = RepeatSession::<u8>::default();
        assert_eq!(session.update(enabled_repeat()), RepeatTimerAction::None);
        assert_eq!(
            session.press(right_key(106), 7, true, true),
            RepeatTimerAction::Arm(Duration::from_millis(600))
        );

        assert_eq!(
            session.press(key_event(42, Keysym::Shift_L), 8, false, false),
            RepeatTimerAction::None
        );
        let held = session.key.as_ref().unwrap();
        assert_eq!(held.event.raw_code, 106);
        assert_eq!(held.surface, 7);
        assert!(session.host_armed);

        assert_eq!(
            session.press(key_event(102, Keysym::Home), 8, true, false),
            RepeatTimerAction::Disarm
        );
        assert!(session.key.is_none());
        assert!(!session.host_armed);
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.clear_keyboard_focus();
        self.keyboard_surface = Some(surface.clone());
        self.reconcile_hover_intent();
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.keyboard_surface.as_ref() == Some(surface) {
            self.clear_keyboard_focus();
        }
        self.reconcile_hover_intent();
    }
    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        let key_repeats = self.physical_key_repeats(event.raw_code);
        let action_repeats = self.key(event.clone(), serial, true, qh);
        self.finish_key_press(event, key_repeats, action_repeats);
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // wl_keyboard v10 lets the compositor take over repeat generation by
        // disabling the client clock and sending repeated key states instead.
        if self.keyboard_surface.as_ref().is_some_and(|surface| {
            self.repeat
                .accepts_compositor_repeat(event.raw_code, surface)
        }) && !self.key(event, 0, false, qh)
        {
            self.clear_key_repeat();
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let action = self.repeat.release(event.raw_code);
        self.apply_repeat_timer(action);
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        info: RepeatInfo,
    ) {
        let action = self.repeat.update(info);
        self.apply_repeat_timer(action);
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        self.clear_key_repeat();
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        self.keyboard_keymap = xkb::Keymap::new_from_string(
            &context,
            keymap.as_string(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::COMPILE_NO_FLAGS,
        );
        if self.keyboard_keymap.is_none() {
            self.error = Some("cannot retain the keyboard keymap".into());
        }
    }
}

impl Dispatch<WpFractionalScaleV1, wl_surface::WlSurface> for App {
    fn event(
        app: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        wl: &wl_surface::WlSurface,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(index) = app.index(wl)
            && let Some(scale) = Scale::from_120ths(scale)
        {
            let surface = &mut app.surfaces[index];
            surface.scale = scale;
            surface.dirty = true;
            let panel = matches!(surface.role, Role::Panel(_));
            let output = surface.output.clone();
            if panel {
                app.set_output_scale(&output, scale);
            }
        }
    }
}

impl Dispatch<XdgActivationTokenV1, u64> for App {
    fn event(
        app: &mut Self,
        _: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let xdg_activation_token_v1::Event::Done { token } = event else {
            return;
        };
        let Some(completed) = app.pending_launches.complete(*id, Instant::now()) else {
            return;
        };
        completed.token.destroy();
        app.spawn_launch(completed.request, completed.fresh.then_some(token.as_str()));
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_layer!(App);
delegate_xdg_shell!(App);
delegate_xdg_popup!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_registry!(App);
delegate_noop!(App: ignore WpFractionalScaleManagerV1);
delegate_noop!(App: ignore WpViewporter);
delegate_noop!(App: ignore WpViewport);
delegate_noop!(App: ignore XdgActivationV1);
delegate_noop!(App: ignore xdg_positioner::XdgPositioner);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
