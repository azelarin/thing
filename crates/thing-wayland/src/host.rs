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
    HorizontalRange, KeyboardMode, Kind, Node, NodeId, PanelInputRegion, Point, Rect, Scale,
    SurfaceLayer,
};
use thing_runtime::{
    Action, Event, OutputContext, Scalar, ServiceSnapshot, SurfaceKey, SurfaceTree,
    SystemTrayAction, SystemTrayMenuAction,
};
use thing_ui::{
    Interaction, Layout, PaintFrame, ScrollState, Size, TextInputCheckpoint, TextInputEditor,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
};
use wayland_protocols::{
    ext::data_control::v1::client::{
        ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
        ext_data_control_source_v1::{self, ExtDataControlSourceV1},
    },
    wp::text_input::zv3::client::{
        zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        zwp_text_input_v3::{self, ZwpTextInputV3},
    },
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
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use xkbcommon::xkb;

use crate::{
    Renderer, RunOptions, SystemTray,
    audio::Audio,
    clipboard::{Clipboard, OfferData as ClipboardOfferData, SourceData as ClipboardSourceData},
    configuration::Configurations,
    hover::{Command as HoverCommand, HoverKey, HoverStates, HoverTarget},
    input_method::{
        ChangeCause as InputMethodChangeCause, CursorRectangle as InputMethodCursorRectangle,
        Done as InputMethodDone, InputMethod, TextState as InputMethodTextState,
    },
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
    let (clipboard, clipboard_source) = if options.clipboard {
        let (ping, source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
        let clipboard = Clipboard::bind(&globals, &qh, ping)
            .map_err(|error| format!("cannot start clipboard service: {error}"))?;
        (Some(clipboard), Some(source))
    } else {
        (None, None)
    };
    let input_method = InputMethod::bind(&globals, &qh);
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
        clipboard,
        input_method,
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
        next_text_input_generation: 1,
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
    app.update_clipboard();
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
    if let Some(source) = clipboard_source {
        event_loop
            .handle()
            .insert_source(source, |_, _, app| app.update_clipboard())
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum TextKeyAction {
    Insert(String),
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveToStart,
    MoveToEnd,
    SelectAll,
    Submit,
}

impl TextKeyAction {
    fn repeats(&self) -> bool {
        matches!(
            self,
            Self::Insert(_) | Self::Backspace | Self::Delete | Self::MoveLeft | Self::MoveRight
        )
    }

    fn changes_value(&self) -> bool {
        matches!(self, Self::Insert(_) | Self::Backspace | Self::Delete)
    }
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
    text_input: Option<ActiveTextInput>,
    pointer_position: Option<Point>,
    hover_path: Vec<NodeId>,
    press_serial: Option<u32>,
    pressed_button: Option<u32>,
    range_press: Option<NodeId>,
    configured: bool,
    dirty: bool,
    waiting: bool,
    retry_at: Option<Instant>,
}

struct ActiveTextInput {
    id: NodeId,
    generation: u64,
    editor: TextInputEditor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GenerationReset {
    Reset,
    Stale,
    Exhausted,
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

pub(crate) struct App {
    render: Render,
    image_ping: calloop::ping::Ping,
    configurations: Configurations<wl_output::WlOutput>,
    audio: Audio,
    media: Media,
    power: Power,
    niri: Niri,
    notifications: Option<NotificationServer>,
    system_tray: Option<SystemTray>,
    clipboard: Option<Clipboard>,
    input_method: InputMethod,
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
    next_text_input_generation: u64,
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
    fn update_clipboard(&mut self) {
        let Some(snapshot) = self.clipboard.as_mut().and_then(Clipboard::take_snapshot) else {
            return;
        };
        if let Err(error) = self.configurations.set_clipboard(snapshot) {
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
        self.reconcile_input_method(InputMethodChangeCause::Other);
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
        let retired_consumers = self.configurations.consumers();
        if let Err(error) = self
            .configurations
            .reload_for_checked(outputs, |_, runtime| {
                validate_surface_capabilities(runtime.surfaces(), layer_shell_version)
            })
        {
            eprintln!("thing: reload failed; keeping the previous surfaces: {error}");
            return Ok(());
        }
        for consumer in retired_consumers {
            if let Err(error) = release_system_tray_consumer(self.system_tray.as_ref(), &consumer) {
                eprintln!("thing: {error}");
            }
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
        self.reconcile_input_method(InputMethodChangeCause::Other);
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
            text_input: None,
            pointer_position: None,
            hover_path: Vec::new(),
            press_serial: None,
            pressed_button: None,
            range_press: None,
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
            surface.text_input = None;
            surface.pointer_position = None;
            surface.hover_path.clear();
            surface.press_serial = None;
            surface.pressed_button = None;
            surface.range_press = None;
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
        let pressed = notify_hover
            .then(|| self.surfaces[index].range_press)
            .flatten();
        if let Err(error) = self.render.park() {
            self.error = Some(error);
        }
        self.surfaces.remove(index);
        self.hover.remove_owner(owner);
        if self.keyboard_surface.as_ref() == Some(&wl) {
            self.clear_keyboard_focus();
        }
        if notify_hover {
            if let Some(id) = pressed {
                self.dispatch(owner.output.clone(), id, Event::Pressed(false), None);
            }
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
        let keyboard_surface = self.keyboard_surface.clone();
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
            let text_input = (keyboard_surface.as_ref() == Some(surface.role.surface())
                && surface.text_input.as_ref().is_some_and(|active| {
                    Some(active.id) == surface.input.focused
                        && layout
                            .get(active.id)
                            .is_some_and(|placement| placement.interactive)
                }))
            .then(|| {
                &mut surface
                    .text_input
                    .as_mut()
                    .expect("the active text input was checked")
                    .editor
            });
            let result = self.render.draw(
                window,
                PaintFrame {
                    root,
                    layout: &layout,
                    physical,
                    scale: surface.scale.factor(),
                    input: surface.input,
                    text_input,
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
        self.reconcile_input_method(InputMethodChangeCause::Other);
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
        let consumer = self.configurations.consumer(&output).map(str::to_owned);
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
                Action::SystemTrayMenu { action, id } => send_system_tray_menu_action(
                    self.system_tray.as_ref(),
                    action,
                    &id,
                    consumer.as_deref(),
                ),
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
                Action::SetClipboardText(text) => self
                    .clipboard
                    .as_mut()
                    .ok_or("clipboard service is disabled")
                    .and_then(|clipboard| clipboard.set_text(text, &self.queue))
                    .map_err(str::to_owned),
                Action::ClearClipboard => self
                    .clipboard
                    .as_ref()
                    .ok_or("clipboard service is disabled")
                    .and_then(Clipboard::clear)
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
        self.reconcile_range_presses(output);
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

    fn range_input(&mut self, surface: usize, id: NodeId, x: f32) {
        let output = self.surfaces[surface].output.clone();
        let key = self.surfaces[surface].key.clone();
        let Some(node) = self
            .configurations
            .surface_root(&output, &key)
            .and_then(|root| root.find(id))
        else {
            return;
        };
        if !matches!(node.kind, Kind::Slider | Kind::Range) || !node.enabled {
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
        let span = match node.kind {
            Kind::Slider => HorizontalRange::unclamped_inset(bounds, 8.0),
            Kind::Range => HorizontalRange::inset(bounds, node.range_inset),
            _ => unreachable!("range kind checked above"),
        };
        let value = node.slider.value_at(f64::from(span.fraction_at(x)));
        self.dispatch(output, id, Event::Change(value), None);
    }

    fn reconcile_range_presses(&mut self, output: &wl_output::WlOutput) {
        let stale: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| &surface.output == output)
            .filter_map(|surface| {
                let id = surface.range_press?;
                (!self.configurations.active_press_is_available(output, id))
                    .then(|| (surface.role.surface().clone(), id))
            })
            .collect();
        for (wl, id) in stale {
            if let Some(index) = self.index(&wl) {
                let surface = &mut self.surfaces[index];
                if surface.input.pressed == Some(id) {
                    surface.input.pressed = None;
                }
                surface.pressed_button = None;
                surface.press_serial = None;
                surface.range_press = None;
                surface.dirty = true;
            }
            if self.configurations.has_active_press(output, id) {
                self.dispatch(output.clone(), id, Event::Pressed(false), None);
            }
        }
    }

    fn text_input_node_and_bounds(&self, surface: usize, id: NodeId) -> Option<(Node, Rect)> {
        let surface = self.surfaces.get(surface)?;
        let node = self
            .configurations
            .surface_root(&surface.output, &surface.key)?
            .find(id)?;
        if node.kind != Kind::TextInput {
            return None;
        }
        let bounds = surface.layout.as_ref()?.get(id)?.bounds;
        Some((node.clone(), bounds))
    }

    fn allocate_text_input_generation(&mut self) -> Option<u64> {
        let Some(generation) = allocate_text_input_generation(&mut self.next_text_input_generation)
        else {
            self.error = Some("text input generation counter is exhausted".into());
            return None;
        };
        Some(generation)
    }

    fn focus_text_input(
        &mut self,
        surface: usize,
        id: NodeId,
        point: Option<Point>,
        extend: bool,
    ) -> bool {
        let Some((node, bounds)) = self.text_input_node_and_bounds(surface, id) else {
            self.surfaces[surface].text_input = None;
            return false;
        };
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
        let width = (bounds.width - 2.0 * padding_x).max(0.0);
        let was_active = self.surfaces[surface]
            .text_input
            .as_ref()
            .is_some_and(|active| active.id == id);
        if !was_active {
            let Some(generation) = self.allocate_text_input_generation() else {
                self.surfaces[surface].text_input = None;
                return false;
            };
            self.surfaces[surface].text_input = Some(ActiveTextInput {
                id,
                generation,
                editor: TextInputEditor::new(&node, width),
            });
        }
        let active = self.surfaces[surface]
            .text_input
            .as_mut()
            .expect("the focused text input was initialized");
        active.editor.prepare(&node, width);
        if let Some(point) = point {
            let x = point.x - bounds.x - padding_x;
            let content_height = (bounds.height - 2.0 * padding_y).max(0.0);
            let offset_y = ((content_height - active.editor.layout().height()) / 2.0).max(0.0);
            let y = point.y - bounds.y - padding_y - offset_y;
            if extend && was_active {
                active.editor.extend_selection_to_point(x, y);
            } else {
                active.editor.move_to_point(x, y);
            }
        }
        true
    }

    fn input_method_text_state(&mut self) -> Option<InputMethodTextState> {
        let entered = self.input_method.entered_surface()?.clone();
        if !input_method_surface_has_keyboard_focus(&entered, self.keyboard_surface.as_ref()) {
            return None;
        }
        let surface_index = self.index(&entered)?;
        let (id, generation) = self.surfaces[surface_index]
            .text_input
            .as_ref()
            .map(|active| (active.id, active.generation))?;
        let interactive = active_text_input_is_current(
            self.configurations.surface_root(
                &self.surfaces[surface_index].output,
                &self.surfaces[surface_index].key,
            ),
            self.surfaces[surface_index].layout.as_ref(),
            self.surfaces[surface_index].input.focused,
            id,
        );
        let Some((node, bounds)) = interactive
            .then(|| self.text_input_node_and_bounds(surface_index, id))
            .flatten()
        else {
            self.surfaces[surface_index].text_input = None;
            self.surfaces[surface_index].dirty = true;
            return None;
        };
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let width = (bounds.width - 2.0 * padding_x).max(0.0);
        let (state, composition_cleared) = {
            let active = self.surfaces[surface_index]
                .text_input
                .as_mut()
                .expect("the active text input was checked");
            let composing = active.editor.is_composing();
            active.editor.prepare(&node, width);
            (
                InputMethodTextState {
                    generation,
                    revision: active.editor.revision(),
                    surrounding: active.editor.surrounding(),
                    cursor_rectangle: input_method_cursor_rectangle(&node, bounds, &active.editor),
                },
                composing && !active.editor.is_composing(),
            )
        };
        if composition_cleared {
            self.surfaces[surface_index].dirty = true;
        }
        Some(state)
    }

    fn reconcile_input_method(&mut self, cause: InputMethodChangeCause) {
        let state = self.input_method_text_state();
        self.input_method.update(state, cause);
    }

    fn cancel_input_method_composition(&mut self, surface: &wl_surface::WlSurface) {
        let Some(index) = self.index(surface) else {
            return;
        };
        if cancel_active_text_input_composition(&mut self.surfaces[index].text_input) {
            self.surfaces[index].dirty = true;
        }
    }

    fn reset_input_method_generation(
        &mut self,
        surface: usize,
        id: NodeId,
        generation: u64,
    ) -> bool {
        let result = reset_active_text_input_generation(
            &mut self.surfaces[surface].text_input,
            id,
            generation,
            &mut self.next_text_input_generation,
        );
        match result {
            GenerationReset::Reset => {
                self.surfaces[surface].dirty = true;
                true
            }
            GenerationReset::Stale => false,
            GenerationReset::Exhausted => {
                self.surfaces[surface].dirty = true;
                self.error = Some("text input generation counter is exhausted".into());
                false
            }
        }
    }

    fn apply_input_method_done(&mut self, done: InputMethodDone) {
        let cause = self.apply_input_method_done_inner(done);
        // done may resume deferred protocol state. always publish the editor's
        // post-batch state, including when the batch was rejected.
        self.reconcile_input_method(cause);
    }

    fn apply_input_method_done_inner(&mut self, done: InputMethodDone) -> InputMethodChangeCause {
        let Some(generation) = done.generation else {
            return InputMethodChangeCause::InputMethod;
        };
        let Some(entered) = self.input_method.entered_surface().cloned() else {
            return InputMethodChangeCause::InputMethod;
        };
        let Some(surface_index) = self.index(&entered) else {
            return InputMethodChangeCause::InputMethod;
        };
        let Some((id, active_generation)) = self.surfaces[surface_index]
            .text_input
            .as_ref()
            .map(|active| (active.id, active.generation))
        else {
            return InputMethodChangeCause::InputMethod;
        };
        if active_generation != generation {
            return InputMethodChangeCause::InputMethod;
        }
        if !input_method_surface_has_keyboard_focus(&entered, self.keyboard_surface.as_ref())
            || !active_text_input_is_current(
                self.configurations.surface_root(
                    &self.surfaces[surface_index].output,
                    &self.surfaces[surface_index].key,
                ),
                self.surfaces[surface_index].layout.as_ref(),
                self.surfaces[surface_index].input.focused,
                id,
            )
        {
            self.surfaces[surface_index].text_input = None;
            self.surfaces[surface_index].dirty = true;
            return InputMethodChangeCause::Other;
        }
        let Some((node, bounds)) = self.text_input_node_and_bounds(surface_index, id) else {
            return InputMethodChangeCause::Other;
        };
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let width = (bounds.width - 2.0 * padding_x).max(0.0);
        let (surrounding, revision, checkpoint, composition_cleared) = {
            let active = self.surfaces[surface_index]
                .text_input
                .as_mut()
                .expect("the active input method target was checked");
            let composing = active.editor.is_composing();
            active.editor.prepare(&node, width);
            (
                active.editor.surrounding(),
                active.editor.revision(),
                active.editor.checkpoint(),
                composing && !active.editor.is_composing(),
            )
        };
        if composition_cleared {
            self.surfaces[surface_index].dirty = true;
        }
        if !done.accepts(generation, revision, &surrounding) {
            self.cancel_input_method_composition(&entered);
            self.reset_input_method_generation(surface_index, id, generation);
            return InputMethodChangeCause::Other;
        }
        let Ok(batch) = done.batch else {
            self.cancel_input_method_composition(&entered);
            self.reset_input_method_generation(surface_index, id, generation);
            return InputMethodChangeCause::Other;
        };

        let output = self.surfaces[surface_index].output.clone();
        let outcome = {
            let active = self.surfaces[surface_index]
                .text_input
                .as_mut()
                .expect("the active input method target was checked");
            active.editor.apply_ime_batch(batch)
        };
        let Ok(outcome) = outcome else {
            self.cancel_input_method_composition(&entered);
            self.reset_input_method_generation(surface_index, id, generation);
            return InputMethodChangeCause::Other;
        };
        self.surfaces[surface_index].dirty = true;
        if !outcome.changed {
            return InputMethodChangeCause::InputMethod;
        }

        let proposed = outcome.surrounding.value;
        self.dispatch(output, id, Event::TextChange(proposed.clone()), None);
        let Some(surface_index) = self.index(&entered) else {
            return InputMethodChangeCause::Other;
        };
        let Some((node, bounds)) = self.text_input_node_and_bounds(surface_index, id) else {
            self.surfaces[surface_index].text_input = None;
            return InputMethodChangeCause::Other;
        };
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let width = (bounds.width - 2.0 * padding_x).max(0.0);
        let Some(active) = self.surfaces[surface_index]
            .text_input
            .as_mut()
            .filter(|active| active.id == id && active.generation == generation)
        else {
            return InputMethodChangeCause::Other;
        };
        let rejected =
            reconcile_controlled_text_edit(&mut active.editor, &node, width, &proposed, checkpoint);
        self.surfaces[surface_index].dirty = true;
        if rejected {
            self.reset_input_method_generation(surface_index, id, generation);
            InputMethodChangeCause::Other
        } else {
            InputMethodChangeCause::InputMethod
        }
    }

    fn text_input_key(
        &mut self,
        surface: usize,
        id: NodeId,
        action: TextKeyAction,
        origin: Option<InputOrigin>,
    ) -> bool {
        if !self.focus_text_input(surface, id, None, false) {
            return false;
        }
        let output = self.surfaces[surface].output.clone();
        let wl = self.surfaces[surface].role.surface().clone();
        let select = self.modifiers.shift;
        let word = self.modifiers.ctrl && !self.modifiers.alt;
        let repeats = action.repeats();
        let surface_state = &mut self.surfaces[surface];
        reveal_text_input_focus(&mut surface_state.input, &mut surface_state.dirty);
        let generation = self.surfaces[surface]
            .text_input
            .as_ref()
            .expect("the focused text input was initialized")
            .generation;

        if action == TextKeyAction::Submit {
            let (value, cancelled) = prepare_text_input_submit(
                self.surfaces[surface]
                    .text_input
                    .as_mut()
                    .expect("the focused text input was initialized"),
            );
            if cancelled {
                self.reset_input_method_generation(surface, id, generation);
            }
            self.dispatch(output, id, Event::Submit(value), origin);
            return false;
        }

        let active = self.surfaces[surface]
            .text_input
            .as_mut()
            .expect("the focused text input was initialized");
        let checkpoint = action.changes_value().then(|| active.editor.checkpoint());
        let changed = match action {
            TextKeyAction::Insert(text) => active.editor.replace_selection(&text),
            TextKeyAction::Backspace => active.editor.backspace(word),
            TextKeyAction::Delete => active.editor.delete(word),
            TextKeyAction::MoveLeft => {
                active.editor.move_left(select, word);
                false
            }
            TextKeyAction::MoveRight => {
                active.editor.move_right(select, word);
                false
            }
            TextKeyAction::MoveToStart => {
                active.editor.move_to_start(select);
                false
            }
            TextKeyAction::MoveToEnd => {
                active.editor.move_to_end(select);
                false
            }
            TextKeyAction::SelectAll => {
                active.editor.select_all();
                false
            }
            TextKeyAction::Submit => unreachable!(),
        };
        let proposed = changed.then(|| active.editor.value().into_owned());

        let Some(proposed) = proposed else {
            return repeats;
        };
        let checkpoint = checkpoint.expect("value-changing edits create a rollback checkpoint");
        self.dispatch(output, id, Event::TextChange(proposed.clone()), origin);

        let Some(surface) = self.index(&wl) else {
            return repeats;
        };
        let Some((node, bounds)) = self.text_input_node_and_bounds(surface, id) else {
            self.surfaces[surface].text_input = None;
            return repeats;
        };
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let width = (bounds.width - 2.0 * padding_x).max(0.0);
        let Some(active) = self.surfaces[surface]
            .text_input
            .as_mut()
            .filter(|active| active.id == id && active.generation == generation)
        else {
            return repeats;
        };
        let rejected =
            reconcile_controlled_text_edit(&mut active.editor, &node, width, &proposed, checkpoint);
        if rejected {
            self.reset_input_method_generation(surface, id, generation);
        }
        repeats
    }

    fn remove_popup(&mut self, index: usize, notify_runtime: bool, suppress_hover: bool) {
        let id = self.surfaces[index].root;
        let output = self.surfaces[index].output.clone();
        let wl = self.surfaces[index].role.surface().clone();
        let mode = self.surfaces[index].role.popup_mode().cloned();
        let hovered = std::mem::take(&mut self.surfaces[index].hover_path);
        let pressed = notify_runtime
            .then(|| self.surfaces[index].range_press)
            .flatten();
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
            if let Some(id) = pressed {
                self.dispatch(output.clone(), id, Event::Pressed(false), None);
            }
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
        let mut pressed = Vec::new();
        for surface in &mut self.surfaces {
            leaving.extend(
                std::mem::take(&mut surface.hover_path)
                    .into_iter()
                    .map(|id| (surface.output.clone(), id)),
            );
            surface.pointer_position = None;
            pressed.extend(
                surface
                    .range_press
                    .take()
                    .map(|id| (surface.output.clone(), id)),
            );
            surface.input.hovered = None;
            surface.input.pressed = None;
            surface.press_serial = None;
            surface.pressed_button = None;
            surface.dirty = true;
        }
        for (output, id) in pressed {
            self.dispatch(output, id, Event::Pressed(false), None);
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
        self.set_keyboard_surface(None);
    }

    fn set_keyboard_surface(&mut self, next: Option<wl_surface::WlSurface>) {
        self.clear_key_repeat();
        let previous = std::mem::replace(&mut self.keyboard_surface, next);
        if let Some(previous) = previous.as_ref()
            && self.keyboard_surface.as_ref() != Some(previous)
        {
            self.cancel_input_method_composition(previous);
        }
        self.modifiers = Modifiers::default();
        for surface in &mut self.surfaces {
            if keyboard_focus_transition_affects(
                previous.as_ref(),
                self.keyboard_surface.as_ref(),
                surface.role.surface(),
            ) {
                surface.dirty = true;
            }
        }
        self.reconcile_input_method(InputMethodChangeCause::Other);
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
                let focused = order[next];
                surface.input.focus_from_keyboard(focused);
                surface.dirty = true;
                self.focus_text_input(index, focused, None, false);
            }
            self.reconcile_hover_intent();
            self.reconcile_input_method(InputMethodChangeCause::Other);
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
        let origin = direct_input_serial(direct, serial)
            .and_then(|serial| self.input_origin(event_surface.clone(), serial));
        if node.kind == Kind::TextInput {
            let Some(action) = text_key_action(&event, self.modifiers, direct) else {
                return false;
            };
            let repeats = self.text_input_key(index, id, action, origin);
            self.reconcile_input_method(InputMethodChangeCause::Other);
            return repeats;
        }
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
        (Kind::Slider | Kind::Range, Keysym::Left | Keysym::Down) => {
            Some(WidgetKeyAction::Slider(SliderKeyAction::Decrease))
        }
        (Kind::Slider | Kind::Range, Keysym::Right | Keysym::Up) => {
            Some(WidgetKeyAction::Slider(SliderKeyAction::Increase))
        }
        (Kind::Slider | Kind::Range, Keysym::Home) if direct => {
            Some(WidgetKeyAction::Slider(SliderKeyAction::Minimum))
        }
        (Kind::Slider | Kind::Range, Keysym::End) if direct => {
            Some(WidgetKeyAction::Slider(SliderKeyAction::Maximum))
        }
        _ => None,
    }
}

fn text_key_action(event: &KeyEvent, modifiers: Modifiers, direct: bool) -> Option<TextKeyAction> {
    if modifiers.logo || (modifiers.alt && !modifiers.ctrl) {
        return None;
    }
    match event.keysym {
        Keysym::BackSpace => return Some(TextKeyAction::Backspace),
        Keysym::Delete => return Some(TextKeyAction::Delete),
        Keysym::Left => return Some(TextKeyAction::MoveLeft),
        Keysym::Right => return Some(TextKeyAction::MoveRight),
        Keysym::Home if direct => return Some(TextKeyAction::MoveToStart),
        Keysym::End if direct => return Some(TextKeyAction::MoveToEnd),
        Keysym::Return | Keysym::KP_Enter if direct => return Some(TextKeyAction::Submit),
        Keysym::a | Keysym::A if direct && modifiers.ctrl && !modifiers.alt => {
            return Some(TextKeyAction::SelectAll);
        }
        _ => {}
    }
    if modifiers.ctrl && !modifiers.alt {
        return None;
    }
    let text = event.utf8.as_ref()?;
    (!text.is_empty()
        && !text.chars().any(|character| {
            character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
        }))
    .then(|| TextKeyAction::Insert(text.clone()))
}

fn reconcile_controlled_text_edit(
    editor: &mut TextInputEditor,
    node: &Node,
    width: f32,
    proposed: &str,
    checkpoint: TextInputCheckpoint,
) -> bool {
    let rejected = node.text != proposed;
    if rejected {
        editor.restore(checkpoint);
        editor.cancel_composition();
    }
    editor.prepare(node, width);
    rejected
}

fn allocate_text_input_generation(next: &mut u64) -> Option<u64> {
    let generation = *next;
    if generation == 0 {
        return None;
    }
    *next = generation.checked_add(1).unwrap_or(0);
    Some(generation)
}

fn reset_active_text_input_generation(
    active: &mut Option<ActiveTextInput>,
    expected_id: NodeId,
    expected_generation: u64,
    next: &mut u64,
) -> GenerationReset {
    if !active
        .as_ref()
        .is_some_and(|active| active.id == expected_id && active.generation == expected_generation)
    {
        return GenerationReset::Stale;
    }
    let Some(generation) = allocate_text_input_generation(next) else {
        *active = None;
        return GenerationReset::Exhausted;
    };
    active
        .as_mut()
        .expect("the active text input was checked")
        .generation = generation;
    GenerationReset::Reset
}

fn prepare_text_input_submit(active: &mut ActiveTextInput) -> (String, bool) {
    let cancelled = active.editor.cancel_composition();
    (active.editor.value().into_owned(), cancelled)
}

fn cancel_active_text_input_composition(active: &mut Option<ActiveTextInput>) -> bool {
    active
        .as_mut()
        .is_some_and(|active| active.editor.cancel_composition())
}

fn input_method_cursor_rectangle(
    node: &Node,
    bounds: Rect,
    editor: &TextInputEditor,
) -> InputMethodCursorRectangle {
    let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
    let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
    let content_height = (bounds.height - 2.0 * padding_y).max(0.0);
    let offset_y = ((content_height - editor.layout().height()) / 2.0).max(0.0);
    let area = editor.ime_cursor_area();
    let x0 = bounds.x + padding_x + editor.text_offset_x() + area.x0 as f32;
    let y0 = bounds.y + padding_y + offset_y + area.y0 as f32;
    let x1 = bounds.x + padding_x + editor.text_offset_x() + area.x1 as f32;
    let y1 = bounds.y + padding_y + offset_y + area.y1 as f32;
    let x = floor_i32(x0);
    let y = floor_i32(y0);
    InputMethodCursorRectangle {
        x,
        y,
        width: ceil_i32(x1).saturating_sub(x).max(1),
        height: ceil_i32(y1).saturating_sub(y).max(1),
    }
}

fn floor_i32(value: f32) -> i32 {
    if value.is_finite() {
        value.floor().clamp(i32::MIN as f32, i32::MAX as f32) as i32
    } else {
        0
    }
}

fn ceil_i32(value: f32) -> i32 {
    if value.is_finite() {
        value.ceil().clamp(i32::MIN as f32, i32::MAX as f32) as i32
    } else {
        0
    }
}

fn direct_input_serial(direct: bool, serial: u32) -> Option<u32> {
    direct.then_some(serial)
}

fn keyboard_focus_transition_affects<T: PartialEq>(
    previous: Option<&T>,
    next: Option<&T>,
    candidate: &T,
) -> bool {
    previous == Some(candidate) || next == Some(candidate)
}

fn keyboard_leave_matches<T: PartialEq>(current: Option<&T>, leaving: &T) -> bool {
    current == Some(leaving)
}

fn input_method_surface_has_keyboard_focus<T: PartialEq>(
    entered: &T,
    keyboard_surface: Option<&T>,
) -> bool {
    keyboard_surface == Some(entered)
}

fn reveal_text_input_focus(input: &mut Interaction, dirty: &mut bool) {
    input.reveal_keyboard_focus();
    *dirty = true;
}

fn focused_widget_is_interactive(layout: Option<&Layout>, id: NodeId) -> bool {
    layout
        .and_then(|layout| layout.get(id))
        .is_some_and(|placement| placement.interactive)
}

fn active_text_input_is_current(
    root: Option<&Node>,
    layout: Option<&Layout>,
    focused: Option<NodeId>,
    id: NodeId,
) -> bool {
    focused == Some(id)
        && root.is_some_and(|root| {
            root.find(id)
                .is_some_and(|node| node.kind == Kind::TextInput)
                && interactive_path(root, id).is_some()
        })
        && focused_widget_is_interactive(layout, id)
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

fn send_system_tray_menu_action(
    system_tray: Option<&SystemTray>,
    action: SystemTrayMenuAction,
    id: &str,
    consumer: Option<&str>,
) -> Result<(), String> {
    let system_tray = system_tray.ok_or_else(|| "system tray capability is disabled".to_owned())?;
    let consumer = consumer.ok_or_else(|| "configuration is no longer available".to_owned())?;
    system_tray
        .send_menu(action, id, consumer)
        .map_err(str::to_owned)
}

fn release_system_tray_consumer(
    system_tray: Option<&SystemTray>,
    consumer: &str,
) -> Result<(), String> {
    if let Some(system_tray) = system_tray {
        system_tray
            .release_menu_consumer(consumer)
            .map_err(str::to_owned)?;
    }
    Ok(())
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
        release_system_tray_consumer, send_system_tray_action, send_system_tray_menu_action,
        valid_dimensions, validate_surface_capabilities,
    };
    use crate::{configuration::Configurations, panel};
    use tempfile::tempdir;
    use thing_core::{KeyboardMode, Kind, Node, NodeId, PanelEdge, PanelMargins, Scale};
    use thing_runtime::{
        Event, OutputContext, ServiceSnapshot, SurfaceKey, SurfaceTree, SystemTrayAction,
        SystemTrayMenuAction,
    };

    #[test]
    fn disabled_system_tray_rejects_actions_without_starting_a_worker() {
        assert_eq!(
            send_system_tray_action(None, SystemTrayAction::Activate, "tray1"),
            Err("system tray capability is disabled".into())
        );
        assert_eq!(
            send_system_tray_menu_action(
                None,
                SystemTrayMenuAction::Open,
                "menu1",
                Some("configuration1"),
            ),
            Err("system tray capability is disabled".into())
        );
        assert_eq!(release_system_tray_consumer(None, "configuration1"), Ok(()));
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
        if let Some(consumer) = self.configurations.remove(&output)
            && let Err(error) = release_system_tray_consumer(self.system_tray.as_ref(), &consumer)
        {
            eprintln!("thing: {error}");
        }
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
        if let Some(clipboard) = &mut self.clipboard {
            clipboard.attach_seat(&seat, qh);
        }
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
                    Ok(keyboard) => {
                        self.keyboard = Some(keyboard);
                        self.input_method.attach_seat(&seat, qh);
                    }
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
            self.input_method.detach_seat();
            self.reconcile_hover_intent();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.as_ref() == Some(&seat) {
            self.seat = None;
            if let Some(clipboard) = &mut self.clipboard {
                clipboard.detach_seat();
            }
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
            self.repeat.settings = None;
            self.keyboard_keymap = None;
            self.clear_keyboard_focus();
            self.input_method.detach_seat();
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
                        let output = surface.output.clone();
                        let event_surface = surface.role.surface().clone();
                        self.range_input(index, id, point.x);
                        self.reconcile_range_presses(&output);
                        if let Some(index) = self.index(&event_surface)
                            && self.surfaces[index]
                                .text_input
                                .as_ref()
                                .is_some_and(|active| active.id == id)
                        {
                            self.focus_text_input(index, id, Some(point), true);
                            self.surfaces[index].dirty = true;
                        }
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.surfaces[index].pointer_position = None;
                    self.surfaces[index].dirty = true;
                }
                PointerEventKind::Press { button, serial, .. }
                    if matches!(button, 0x110..=0x112)
                        && self.surfaces[index].range_press.is_none() =>
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
                        let output = surface.output.clone();
                        let key = surface.key.clone();
                        let event_surface = surface.role.surface().clone();
                        let kind = self
                            .configurations
                            .surface_root(&output, &key)
                            .and_then(|root| root.find(id))
                            .map(|node| node.kind);
                        if kind == Some(Kind::Range) {
                            self.surfaces[index].range_press = Some(id);
                            self.dispatch(output.clone(), id, Event::Pressed(true), None);
                            self.reconcile_range_presses(&output);
                        }
                        if (kind == Some(Kind::Slider)
                            || (kind == Some(Kind::Range)
                                && self.configurations.has_active_press(&output, id)))
                            && let Some(index) = self.index(&event_surface)
                        {
                            self.range_input(index, id, point.x);
                            self.reconcile_range_presses(&output);
                        }
                        if let Some(index) = self.index(&event_surface) {
                            self.focus_text_input(index, id, Some(point), self.modifiers.shift);
                        }
                    } else if button == 0x110 {
                        self.surfaces[index].text_input = None;
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
                    let range_press = self.surfaces[index].range_press.take();
                    let popup_serial = self.surfaces[index].press_serial.take();
                    let output = self.surfaces[index].output.clone();
                    let event_surface = self.surfaces[index].role.surface().clone();
                    self.surfaces[index].dirty = true;
                    if let Some(id) = range_press {
                        let origin = direct_input_serial(true, release_serial)
                            .and_then(|serial| self.input_origin(event_surface, serial));
                        self.dispatch(output, id, Event::Pressed(false), origin);
                        continue;
                    }
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
        self.reconcile_input_method(InputMethodChangeCause::Other);
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

    use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers, RepeatInfo};
    use thing_core::{Kind, Node, NodeId, Rect};
    use thing_ui::{Layout, Measurer, Size, TextInputEditor};

    use super::{
        ActiveTextInput, GenerationReset, RepeatSession, RepeatTimerAction, SliderKeyAction,
        TextKeyAction, WidgetKeyAction, active_text_input_is_current,
        allocate_text_input_generation, cancel_active_text_input_composition, direct_input_serial,
        focused_widget_is_interactive, input_method_cursor_rectangle,
        input_method_surface_has_keyboard_focus, keyboard_focus_transition_affects,
        keyboard_leave_matches, prepare_text_input_submit, reconcile_controlled_text_edit,
        reset_active_text_input_generation, reveal_text_input_focus, scroll_distance,
        text_key_action, widget_key_action,
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

    #[test]
    fn text_input_generations_never_wrap_or_repeat() {
        let mut next = u64::MAX;
        assert_eq!(allocate_text_input_generation(&mut next), Some(u64::MAX));
        assert_eq!(next, 0);
        assert_eq!(allocate_text_input_generation(&mut next), None);
        assert_eq!(next, 0);
    }

    #[test]
    fn input_method_cursor_rectangle_is_surface_local_and_nonempty() {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "hello".into();
        node.style.padding = 4.0;
        let editor = TextInputEditor::new(&node, 112.0);
        let rectangle = input_method_cursor_rectangle(
            &node,
            Rect {
                x: 20.0,
                y: 10.0,
                width: 120.0,
                height: 32.0,
            },
            &editor,
        );

        assert!(rectangle.x >= 24);
        assert!(rectangle.y >= 14);
        assert!(rectangle.width >= 1);
        assert!(rectangle.height >= 1);
    }

    #[test]
    fn input_method_cursor_rectangle_tracks_scrolled_rtl_preedit() {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "abc אבג ".repeat(20);
        node.style.padding = 4.0;
        let mut editor = TextInputEditor::new(&node, 32.0);
        editor
            .apply_ime_batch(thing_ui::TextInputImeBatch {
                preedit: Some(thing_ui::TextInputPreedit {
                    text: "候補".into(),
                    cursor: Some((6, 0)),
                }),
                ..thing_ui::TextInputImeBatch::default()
            })
            .unwrap();
        let rectangle = input_method_cursor_rectangle(
            &node,
            Rect {
                x: 50.0,
                y: 12.0,
                width: 40.0,
                height: 32.0,
            },
            &editor,
        );

        assert!((54..=86).contains(&rectangle.x));
        assert!(rectangle.y >= 16);
        assert!(rectangle.width >= 1);
        assert!(rectangle.height >= 1);
    }

    #[test]
    fn refreshed_tree_invalidates_stale_text_input_targets() {
        assert!(input_method_surface_has_keyboard_focus(&1, Some(&1)));
        assert!(!input_method_surface_has_keyboard_focus(&1, Some(&2)));
        assert!(!input_method_surface_has_keyboard_focus::<u8>(&1, None));

        let mut root = Node::new(NodeId(1), Kind::Panel);
        root.style.width = Some(200.0);
        root.style.height = Some(50.0);
        let mut row = Node::new(NodeId(2), Kind::Row);
        row.children.push(Node::new(NodeId(3), Kind::TextInput));
        root.children.push(row);
        let layout = Layout::compute(
            &root,
            Size {
                width: 200.0,
                height: 50.0,
            },
            &mut FixedMeasurer,
        )
        .unwrap();
        assert!(active_text_input_is_current(
            Some(&root),
            Some(&layout),
            Some(NodeId(3)),
            NodeId(3)
        ));

        let mut hidden = root.clone();
        hidden.children[0].visible = false;
        assert!(!active_text_input_is_current(
            Some(&hidden),
            Some(&layout),
            Some(NodeId(3)),
            NodeId(3)
        ));
        let mut disabled = root.clone();
        disabled.children[0].enabled = false;
        assert!(!active_text_input_is_current(
            Some(&disabled),
            Some(&layout),
            Some(NodeId(3)),
            NodeId(3)
        ));
        let mut transformed = root.clone();
        transformed.children[0].children[0].kind = Kind::Text;
        assert!(!active_text_input_is_current(
            Some(&transformed),
            Some(&layout),
            Some(NodeId(3)),
            NodeId(3)
        ));
        root.children[0].children.clear();
        assert!(!active_text_input_is_current(
            Some(&root),
            Some(&layout),
            Some(NodeId(3)),
            NodeId(3)
        ));
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

        assert_eq!(
            widget_key_action(Kind::Range, Keysym::Left, false),
            Some(WidgetKeyAction::Slider(SliderKeyAction::Decrease))
        );
        assert_eq!(
            widget_key_action(Kind::Range, Keysym::Home, true),
            Some(WidgetKeyAction::Slider(SliderKeyAction::Minimum))
        );
    }

    #[test]
    fn only_direct_input_can_supply_an_activation_serial() {
        assert_eq!(direct_input_serial(true, 42), Some(42));
        assert_eq!(direct_input_serial(false, 42), None);
    }

    #[test]
    fn text_actions_separate_editing_shortcuts_from_direct_text() {
        let mut event = key_event(30, Keysym::e);
        event.utf8 = Some("é".into());
        let insert = text_key_action(&event, Modifiers::default(), true).unwrap();
        assert_eq!(insert, TextKeyAction::Insert("é".into()));
        assert!(insert.repeats());
        assert!(insert.changes_value());

        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        let select = text_key_action(&key_event(30, Keysym::a), ctrl, true).unwrap();
        assert_eq!(select, TextKeyAction::SelectAll);
        assert!(!select.repeats());
        assert!(!select.changes_value());
        assert_eq!(text_key_action(&event, ctrl, true), None);

        let alt_gr = Modifiers {
            ctrl: true,
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            text_key_action(&event, alt_gr, true),
            Some(TextKeyAction::Insert("é".into()))
        );
        assert_eq!(
            text_key_action(&key_event(28, Keysym::Return), Modifiers::default(), true),
            Some(TextKeyAction::Submit)
        );
        assert_eq!(
            text_key_action(&key_event(28, Keysym::Return), Modifiers::default(), false),
            None
        );
        for action in [
            TextKeyAction::MoveLeft,
            TextKeyAction::MoveRight,
            TextKeyAction::MoveToStart,
            TextKeyAction::MoveToEnd,
        ] {
            assert!(!action.changes_value());
        }

        let mut separator = key_event(30, Keysym::e);
        separator.utf8 = Some("\u{2028}".into());
        assert_eq!(
            text_key_action(&separator, Modifiers::default(), true),
            None
        );
    }

    #[test]
    fn controlled_edits_keep_only_the_runtime_value() {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "old".into();
        let mut editor = TextInputEditor::new(&node, 100.0);
        let checkpoint = editor.checkpoint();
        editor.select_all();
        assert!(editor.replace_selection("proposed"));
        assert!(reconcile_controlled_text_edit(
            &mut editor,
            &node,
            100.0,
            "proposed",
            checkpoint
        ));
        assert_eq!(editor.value(), "old");

        let checkpoint = editor.checkpoint();
        editor.select_all();
        assert!(editor.replace_selection("accepted"));
        node.text = "accepted".into();
        assert!(!reconcile_controlled_text_edit(
            &mut editor,
            &node,
            100.0,
            "accepted",
            checkpoint
        ));
        assert_eq!(editor.value(), "accepted");
    }

    #[test]
    fn rejected_controlled_edits_do_not_restore_old_composition() {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "old".into();
        let mut editor = TextInputEditor::new(&node, 100.0);
        editor
            .apply_ime_batch(thing_ui::TextInputImeBatch {
                preedit: Some(thing_ui::TextInputPreedit {
                    text: "x".into(),
                    cursor: Some((1, 1)),
                }),
                ..thing_ui::TextInputImeBatch::default()
            })
            .unwrap();
        let checkpoint = editor.checkpoint();
        assert!(editor.replace_selection("proposed"));

        assert!(reconcile_controlled_text_edit(
            &mut editor,
            &node,
            100.0,
            "proposed",
            checkpoint
        ));

        assert_eq!(editor.value(), "old");
        assert!(!editor.is_composing());
    }

    #[test]
    fn accepted_controlled_result_keeps_generation_but_rejected_and_invalid_reset() {
        let id = NodeId(1);
        let mut node = Node::new(id, Kind::TextInput);
        node.text = "old".into();
        let mut active = Some(ActiveTextInput {
            id,
            generation: 1,
            editor: TextInputEditor::new(&node, 100.0),
        });
        let mut next = 2;

        let editor = &mut active.as_mut().unwrap().editor;
        let checkpoint = editor.checkpoint();
        editor.select_all();
        editor.replace_selection("accepted");
        node.text = "accepted".into();
        let rejected = reconcile_controlled_text_edit(editor, &node, 100.0, "accepted", checkpoint);
        assert!(!rejected);
        assert_eq!(active.as_ref().unwrap().generation, 1);

        let editor = &mut active.as_mut().unwrap().editor;
        let checkpoint = editor.checkpoint();
        editor.select_all();
        editor.replace_selection("rejected");
        assert!(reconcile_controlled_text_edit(
            editor, &node, 100.0, "rejected", checkpoint,
        ));
        assert_eq!(
            reset_active_text_input_generation(&mut active, id, 1, &mut next),
            GenerationReset::Reset
        );
        assert_eq!(active.as_ref().unwrap().generation, 2);

        let editor = &mut active.as_mut().unwrap().editor;
        let checkpoint = editor.checkpoint();
        editor.select_all();
        editor.replace_selection("proposal");
        node.text = "transformed".into();
        assert!(reconcile_controlled_text_edit(
            editor, &node, 100.0, "proposal", checkpoint,
        ));
        assert_eq!(
            reset_active_text_input_generation(&mut active, id, 2, &mut next),
            GenerationReset::Reset
        );
        assert_eq!(active.as_ref().unwrap().generation, 3);

        let invalid_batch_generation = 3;
        assert_eq!(
            reset_active_text_input_generation(
                &mut active,
                id,
                invalid_batch_generation,
                &mut next,
            ),
            GenerationReset::Reset
        );
        assert_eq!(active.as_ref().unwrap().generation, 4);
        let unchanged_next = next;
        let stale_done_generation = invalid_batch_generation;
        assert_eq!(
            reset_active_text_input_generation(&mut active, id, stale_done_generation, &mut next,),
            GenerationReset::Stale
        );
        assert_eq!(active.as_ref().unwrap().generation, 4);
        assert_eq!(next, unchanged_next);
        next = 0;
        assert_eq!(
            reset_active_text_input_generation(&mut active, id, 4, &mut next),
            GenerationReset::Exhausted
        );
        assert!(active.is_none());
    }

    #[test]
    fn submit_and_repeated_enter_clear_preedit_without_submitting_it() {
        let id = NodeId(1);
        let mut node = Node::new(id, Kind::TextInput);
        node.text = "old".into();
        let mut active = Some(ActiveTextInput {
            id,
            generation: 1,
            editor: TextInputEditor::new(&node, 100.0),
        });
        active
            .as_mut()
            .unwrap()
            .editor
            .apply_ime_batch(thing_ui::TextInputImeBatch {
                preedit: Some(thing_ui::TextInputPreedit {
                    text: "compose".into(),
                    cursor: Some((7, 7)),
                }),
                ..thing_ui::TextInputImeBatch::default()
            })
            .unwrap();

        let (value, cancelled) = prepare_text_input_submit(active.as_mut().unwrap());
        assert!(cancelled);
        assert_eq!(value, "old");
        assert!(!active.as_ref().unwrap().editor.is_composing());
        let mut next = 2;
        assert_eq!(
            reset_active_text_input_generation(&mut active, id, 1, &mut next),
            GenerationReset::Reset
        );

        active
            .as_mut()
            .unwrap()
            .editor
            .apply_ime_batch(thing_ui::TextInputImeBatch {
                preedit: Some(thing_ui::TextInputPreedit {
                    text: "again".into(),
                    cursor: None,
                }),
                ..thing_ui::TextInputImeBatch::default()
            })
            .unwrap();
        assert!(cancel_active_text_input_composition(&mut active));
        assert!(!cancel_active_text_input_composition(&mut active));
        assert_eq!(active.as_ref().unwrap().generation, 2);
    }

    #[test]
    fn every_text_key_action_reveals_keyboard_focus() {
        let mut input = thing_ui::Interaction::default();
        input.focus_from_pointer(Some(NodeId(1)));
        assert_eq!(input.keyboard_focused(), None);
        let mut dirty = false;

        reveal_text_input_focus(&mut input, &mut dirty);

        assert_eq!(input.keyboard_focused(), Some(NodeId(1)));
        assert!(dirty);
    }

    #[test]
    fn keyboard_focus_transitions_dirty_only_old_and_new_surfaces() {
        assert!(keyboard_focus_transition_affects(Some(&1), Some(&2), &1));
        assert!(keyboard_focus_transition_affects(Some(&1), Some(&2), &2));
        assert!(!keyboard_focus_transition_affects(Some(&1), Some(&2), &3));

        assert!(keyboard_focus_transition_affects(Some(&1), Some(&1), &1));
        assert!(keyboard_focus_transition_affects(Some(&1), None, &1));
        assert!(!keyboard_focus_transition_affects(Some(&1), None, &2));

        assert!(keyboard_leave_matches(Some(&1), &1));
        assert!(!keyboard_leave_matches(Some(&1), &2));
        assert!(!keyboard_leave_matches::<u8>(None, &1));
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
        self.set_keyboard_surface(Some(surface.clone()));
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
        if keyboard_leave_matches(self.keyboard_surface.as_ref(), surface) {
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

impl Dispatch<ExtDataControlDeviceV1, ()> for App {
    event_created_child!(App, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ClipboardOfferData::default())
    ]);

    fn event(
        app: &mut Self,
        device: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(clipboard) = &mut app.clipboard else {
            return;
        };
        match event {
            ext_data_control_device_v1::Event::DataOffer { .. } => {}
            ext_data_control_device_v1::Event::Selection { id } => clipboard.ext_offer(id),
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                clipboard.ext_primary_offer(id);
            }
            ext_data_control_device_v1::Event::Finished => clipboard.ext_finished(device),
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, ClipboardOfferData> for App {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        data: &ClipboardOfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.offer(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ClipboardSourceData> for App {
    fn event(
        app: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &ClipboardSourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(clipboard) = &mut app.clipboard else {
            source.destroy();
            return;
        };
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                clipboard.send(data, &mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => clipboard.cancelled(data.key()),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for App {
    event_created_child!(App, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ClipboardOfferData::default())
    ]);

    fn event(
        app: &mut Self,
        device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(clipboard) = &mut app.clipboard else {
            return;
        };
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { .. } => {}
            zwlr_data_control_device_v1::Event::Selection { id } => clipboard.wlr_offer(id),
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                clipboard.wlr_primary_offer(id);
            }
            zwlr_data_control_device_v1::Event::Finished => clipboard.wlr_finished(device),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ClipboardOfferData> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        data: &ClipboardOfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.offer(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ClipboardSourceData> for App {
    fn event(
        app: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        data: &ClipboardSourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(clipboard) = &mut app.clipboard else {
            source.destroy();
            return;
        };
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                clipboard.send(data, &mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => clipboard.cancelled(data.key()),
            _ => {}
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                if let Some(previous) = app.input_method.entered_surface().cloned() {
                    app.cancel_input_method_composition(&previous);
                }
                app.input_method.enter(surface);
                app.reconcile_input_method(InputMethodChangeCause::InputMethod);
            }
            zwp_text_input_v3::Event::Leave { surface } => {
                if app.input_method.leave(&surface) {
                    app.cancel_input_method_composition(&surface);
                    app.reconcile_input_method(InputMethodChangeCause::InputMethod);
                }
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => app
                .input_method
                .preedit_string(text, cursor_begin, cursor_end),
            zwp_text_input_v3::Event::CommitString { text } => {
                app.input_method.commit_string(text);
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => app
                .input_method
                .delete_surrounding_text(before_length, after_length),
            zwp_text_input_v3::Event::Done { serial } => {
                let done = app.input_method.done(serial);
                app.apply_input_method_done(done);
            }
            _ => {}
        }
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
delegate_noop!(App: ignore ExtDataControlManagerV1);
delegate_noop!(App: ignore ZwlrDataControlManagerV1);
delegate_noop!(App: ignore ZwpTextInputManagerV3);
delegate_noop!(App: ignore xdg_positioner::XdgPositioner);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
