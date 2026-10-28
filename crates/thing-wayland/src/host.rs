use std::{
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use calloop::{
    EventLoop, Interest, Mode, PostAction,
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
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{AxisScroll, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        xdg::{
            XdgShell,
            popup::{Popup as XdgPopup, PopupConfigure, PopupHandler},
            window::{Window as XdgWindow, WindowConfigure, WindowHandler},
        },
    },
};
use thing_core::{Kind, Node, NodeId, PanelInputRegion, Point, Rect, Scale};
use thing_runtime::{Action, Event, OutputContext, Scalar, ServiceSnapshot};
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

use crate::{
    Renderer,
    audio::Audio,
    configuration::Configurations,
    hover::{Command as HoverCommand, HoverKey, HoverState, HoverTarget},
    launch::{self, LaunchRequest, Launcher, PendingLaunches},
    niri::Niri,
    panel::{self, Geometry, ResolvedInputRegion},
    popup::{self, Position as PopupPosition},
    render::{FrameResult, Render, Window},
    watch::Watch,
};

/// run one panel per output. the loop sleeps until input, configuration, or a
/// compositor frame callback arrives; an unchanged panel does not request frames.
pub fn run(entry: &Path, renderer: Renderer) -> Result<(), String> {
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
    let (niri_ping, niri_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let niri = Niri::start(niri_ping).map_err(|e| format!("cannot start niri worker: {e}"))?;
    let mut app = App {
        render,
        image_ping,
        configurations: Configurations::new(entry, services),
        watch: watch.clone(),
        reload_at: None,
        hover: HoverState::default(),
        audio,
        niri,
        launcher,
        pending_launches: PendingLaunches::new(),
        surfaces: Vec::new(),
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        xdg_shell,
        activation,
        fractional: globals.bind(&qh, 1..=1, ()).ok(),
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        pointer: None,
        keyboard: None,
        seat: None,
        keyboard_surface: None,
        queue: qh.clone(),
        shift: false,
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
    app.configurations.reload_for(outputs.clone())?;
    for (output, _) in outputs {
        app.create_panel(output, &qh)?;
    }
    app.loading_outputs = false;
    app.update_watch();
    let mut event_loop: EventLoop<App> = EventLoop::try_new().map_err(|error| error.to_string())?;
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
    Automatic(HoverKey<wl_output::WlOutput>),
    Hover(HoverKey<wl_output::WlOutput>),
}

struct InputOrigin {
    seat: wl_seat::WlSeat,
    serial: u32,
    surface: wl_surface::WlSurface,
}

struct PopupRole {
    shell: XdgPopup,
    mode: PopupMode,
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
    root: NodeId,
    size: (u32, u32),
    panel: Option<PanelState>,
    scale: Scale,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PanelState {
    geometry: Geometry,
    // resolved geometry cannot distinguish auto from an equal explicit zone.
    exclusive_zone: Option<i32>,
    input_region: PanelInputRegion,
}

impl PanelState {
    fn from_root(root: &Node) -> Self {
        Self {
            geometry: panel::geometry(root, None),
            exclusive_zone: root.panel.exclusive_zone,
            input_region: root.panel.input_region,
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
    niri: Niri,
    launcher: Launcher,
    pending_launches: PendingLaunches<XdgActivationTokenV1>,
    watch: Rc<Watch>,
    reload_at: Option<Instant>,
    hover: HoverState<wl_output::WlOutput>,
    surfaces: Vec<Surface>,
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    xdg_shell: XdgShell,
    activation: Option<XdgActivationV1>,
    fractional: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat: Option<wl_seat::WlSeat>,
    keyboard_surface: Option<wl_surface::WlSurface>,
    queue: QueueHandle<App>,
    shift: bool,
    loading_outputs: bool,
    exit: bool,
    error: Option<String>,
}

impl Drop for App {
    fn drop(&mut self) {
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
    fn refresh(&mut self) {
        for output in self.configurations.refresh() {
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
        if let Err(error) = self.configurations.reload_for(outputs) {
            eprintln!("thing: reload failed; keeping the previous panel: {error}");
            return Ok(());
        }
        // release the previous configuration's glyph atlas and text layouts.
        let wake = self.image_ping.clone();
        self.render.reset_painter(move || wake.ping())?;
        self.hover.clear();
        self.surfaces
            .retain(|surface| matches!(surface.role, Role::Panel(_)));
        self.surfaces
            .retain(|surface| self.configurations.contains(&surface.output));
        self.keyboard_surface = None;
        let missing: Vec<_> = self
            .configurations
            .keys()
            .filter(|output| {
                !self.surfaces.iter().any(|surface| {
                    matches!(surface.role, Role::Panel(_)) && &surface.output == *output
                })
            })
            .cloned()
            .collect();
        for output in missing {
            self.create_panel(output, qh)?;
        }
        for surface in &mut self.surfaces {
            let Some(root) = self.configurations.root(&surface.output) else {
                continue;
            };
            surface.root = root.id;
            surface.layout = None;
            surface.scroll = ScrollState::default();
            surface.input = Interaction::default();
            surface.pointer_position = None;
            surface.hover_path.clear();
            surface.press_serial = None;
            surface.pressed_button = None;
            surface.dirty = true;
        }
        let outputs: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| matches!(surface.role, Role::Panel(_)))
            .map(|surface| surface.output.clone())
            .collect();
        for output in outputs {
            self.sync_panel(&output, true)?;
        }
        self.update_watch();
        eprintln!("thing: reloaded {}", self.configurations.entry().display());
        Ok(())
    }

    fn add_panel(
        &mut self,
        output: wl_output::WlOutput,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        if self
            .surfaces
            .iter()
            .any(|surface| matches!(surface.role, Role::Panel(_)) && surface.output == output)
        {
            return Ok(());
        }
        if !self.configurations.contains(&output) {
            let Some(context) = self.output_context(&output) else {
                return Ok(());
            };
            self.configurations.add(output.clone(), context)?;
        }
        self.create_panel(output, qh)?;
        self.update_watch();
        Ok(())
    }

    fn create_panel(
        &mut self,
        output: wl_output::WlOutput,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        let root = self
            .configurations
            .root(&output)
            .ok_or("output configuration is unavailable")?;
        let panel = PanelState::from_root(root);
        let context = self
            .configurations
            .output(&output)
            .ok_or("output context is unavailable")?;
        let scale = context.scale;
        let size = panel_size(panel.geometry, (context.width, context.height));
        let wl = self.compositor.create_surface(qh);
        let layer =
            self.layer_shell
                .create_layer_surface(qh, wl, Layer::Top, Some("thing"), Some(&output));
        apply_panel_geometry(&layer, panel.geometry);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        apply_input_region(
            &self.compositor,
            layer.wl_surface(),
            panel.input_region,
            size,
        )?;
        let mut surface = self.make_surface(Role::Panel(layer), output, root.id, size, scale, qh);
        surface.panel = Some(panel);
        surface.role.surface().commit();
        self.surfaces.push(surface);
        Ok(())
    }

    fn make_surface(
        &self,
        role: Role,
        output: wl_output::WlOutput,
        root: NodeId,
        size: (u32, u32),
        scale: Scale,
        qh: &QueueHandle<Self>,
    ) -> Surface {
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
            output,
            root,
            size,
            panel: None,
            scale,
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

    fn sync_panel(&mut self, output: &wl_output::WlOutput, force: bool) -> Result<(), String> {
        let Some(root) = self.configurations.root(output) else {
            return Ok(());
        };
        let next = PanelState::from_root(root);
        let Some(index) = self.surfaces.iter().position(|surface| {
            &surface.output == output && matches!(surface.role, Role::Panel(_))
        }) else {
            return Ok(());
        };
        let surface = &mut self.surfaces[index];
        let previous = surface.panel;
        if !force && previous == Some(next) {
            return Ok(());
        }
        let Role::Panel(layer) = &surface.role else {
            return Ok(());
        };
        let configured_size = previous
            .filter(|previous| {
                previous.geometry.anchors == next.geometry.anchors
                    && previous.geometry.requested_size == next.geometry.requested_size
            })
            .map(|_| surface.size);
        let geometry = panel::geometry(root, configured_size);
        let size_changed = previous.is_none_or(|previous| {
            previous.geometry.anchors != next.geometry.anchors
                || previous.geometry.requested_size != next.geometry.requested_size
        });
        apply_panel_geometry(layer, geometry);
        apply_input_region(
            &self.compositor,
            layer.wl_surface(),
            next.input_region,
            surface.size,
        )?;
        surface.panel = Some(next);
        if size_changed {
            surface.layout = None;
        }
        surface.dirty = true;
        layer.wl_surface().commit();
        Ok(())
    }

    fn sync_configured_panel(&mut self, index: usize) -> Result<(), String> {
        let surface = &self.surfaces[index];
        let Role::Panel(layer) = &surface.role else {
            return Ok(());
        };
        let Some(root) = self.configurations.root(&surface.output) else {
            return Ok(());
        };
        let geometry = panel::geometry(root, Some(surface.size));
        apply_panel_geometry(layer, geometry);
        apply_input_region(
            &self.compositor,
            layer.wl_surface(),
            root.panel.input_region,
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
            match self.configurations.set_output(output, context) {
                Ok(true) => self.invalidate_output(output),
                Ok(false) => {}
                Err(error) => eprintln!("thing: cannot update output scale: {error}"),
            }
        }
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| &surface.output == output && matches!(surface.role, Role::Panel(_)))
            && let Err(error) = self.sync_configured_panel(index)
        {
            self.error = Some(error);
        }
    }

    fn index(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|surface| surface.role.surface() == wl)
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.render.check_error()?;
        let configurations = &self.configurations;
        let now = Instant::now();
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
                .root(&surface.output)
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
            let layout = self.render.layout(
                root,
                Size {
                    width: surface.size.0 as f32,
                    height: surface.size.1 as f32,
                },
                &mut surface.scroll,
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
                    surface.waiting = true;
                    surface.layout = Some(layout);
                    surface.dirty = redraw;
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
        let mut outputs: Vec<(wl_output::WlOutput, bool)> = Vec::new();
        for surface in presented
            .iter()
            .filter_map(|index| self.surfaces.get(*index))
        {
            let panel = matches!(surface.role, Role::Panel(_));
            if let Some((_, presented)) = outputs
                .iter_mut()
                .find(|(output, _)| output == &surface.output)
            {
                *presented |= panel;
            } else {
                outputs.push((surface.output.clone(), panel));
            }
        }
        for (output, parent_presented) in outputs {
            self.sync_popup_placement(&output, parent_presented, qh);
        }
        self.reconcile_presented_pointers(&presented);
        Ok(())
    }

    fn sync_popup_placement(
        &mut self,
        output: &wl_output::WlOutput,
        parent_presented: bool,
        qh: &QueueHandle<Self>,
    ) {
        if !parent_presented
            && self.surfaces.iter().any(|surface| {
                &surface.output == output && matches!(surface.role, Role::Panel(_)) && surface.dirty
            })
        {
            return;
        }
        let Some((index, anchor, position)) = self.desired_popup_position(output) else {
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
        output: &wl_output::WlOutput,
    ) -> Option<(usize, Option<NodeId>, PopupPosition)> {
        let index = self.surfaces.iter().position(|surface| {
            &surface.output == output && matches!(surface.role, Role::Popup(_))
        })?;
        let popup_surface = &self.surfaces[index];
        // reposition applies only after the popup's first buffer has been mapped.
        popup_surface.layout.as_ref()?;
        let Role::Popup(role) = &popup_surface.role else {
            return None;
        };
        let parent = self
            .surfaces
            .iter()
            .find(|surface| &surface.output == output && matches!(surface.role, Role::Panel(_)))?;
        let root = self.configurations.root(output)?;
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
        Some((
            index,
            anchor,
            popup::resolve(node, trigger, root.panel.edge, parent.size),
        ))
    }

    fn dispatch(
        &mut self,
        output: wl_output::WlOutput,
        id: NodeId,
        event: Event,
        origin: Option<InputOrigin>,
    ) {
        let (changed, actions) = match self.configurations.dispatch(&output, id, event) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("thing: {error}");
                return;
            }
        };
        // a successful handler can remove its own popup. request activation
        // before invalidation destroys the surface that received the input.
        for action in actions {
            let result = match action {
                Action::Niri(action) => self.niri.send(action),
                Action::Launch { argv, cwd } => {
                    self.request_launch(LaunchRequest { argv, cwd }, origin.as_ref());
                    Ok(())
                }
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
        if let Some(index) = self.surfaces.iter().position(|surface| {
            &surface.output == output
                && surface.role.popup_mode().is_some_and(|mode| {
                    !popup_is_current(self.configurations.root(output), surface.root, mode)
                })
        }) {
            self.remove_popup(index, true, false);
        }
        if let Err(error) = self.sync_panel(output, false) {
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
        let Some(node) = self
            .configurations
            .root(&output)
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

    fn remove_popup(&mut self, index: usize, notify: bool, suppress_hover: bool) {
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
            self.keyboard_surface = None;
        }
        for hovered in hovered {
            self.dispatch(output.clone(), hovered, Event::Hover(false), None);
        }
        if notify {
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
        let output = parent_surface.output.clone();
        if !matches!(parent_surface.role, Role::Panel(_)) {
            return;
        }

        if let Some((id, _)) = self
            .configurations
            .root(&output)
            .and_then(|root| find_manual_popup(root, None))
        {
            if self.surfaces.iter().any(|surface| {
                surface.output == output
                    && surface.root == id
                    && matches!(surface.role.popup_mode(), Some(PopupMode::Manual))
            }) {
                return;
            }
            if let Some(index) = self.popup_index() {
                self.remove_popup(index, true, true);
            }
            let Some((id, anchor)) = self
                .configurations
                .root(&output)
                .and_then(|root| find_manual_popup(root, None))
            else {
                return;
            };
            self.hover.reconcile(None, Instant::now());
            self.create_popup(&parent_wl, id, anchor, PopupMode::Manual, Some(serial), qh);
            return;
        }

        let Some(anchor) = activated else {
            return;
        };
        let Some(target) = self
            .configurations
            .root(&output)
            .and_then(|root| automatic_target(root, output.clone(), anchor, false))
        else {
            return;
        };
        if let Some(index) = self.popup_index() {
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
            .root(&output)
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
            .root(&output)
            .and_then(|root| automatic_target(root, output.clone(), anchor, false))
        else {
            return;
        };
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
        let output = parent.output.clone();
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
        let Some(position) = self.configurations.root(&output).and_then(|root| {
            root.find(id)
                .map(|node| popup::resolve(node, anchor, root.panel.edge, parent.size))
        }) else {
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
                        anchor: anchor_id,
                        reposition: popup::RepositionState::new(position),
                    }),
                    output,
                    id,
                    (position.size.0 as u32, position.size.1 as u32),
                    parent_scale,
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

    fn popup_index(&self) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|surface| surface.role.popup_mode().is_some())
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
                    if let Some(index) = self.popup_index() {
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
                        surface.output == current.key.output
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
        key: &HoverKey<wl_output::WlOutput>,
    ) -> Option<HoverTarget<wl_output::WlOutput>> {
        let target = automatic_target(
            self.configurations.root(&key.output)?,
            key.output.clone(),
            key.anchor,
            true,
        )?;
        if &target.key != key {
            return None;
        }
        Some(target)
    }

    fn hover_layout_ready(&self, key: &HoverKey<wl_output::WlOutput>) -> bool {
        self.surfaces
            .iter()
            .find(|surface| surface.output == key.output && matches!(surface.role, Role::Panel(_)))
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
                .root(&surface.output)
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
        if self.surfaces.iter().any(|surface| {
            matches!(
                surface.role.popup_mode(),
                Some(PopupMode::Manual | PopupMode::Automatic(_))
            )
        }) {
            self.hover.reconcile(None, Instant::now());
            return;
        }
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
                    self.configurations.root(&surface.output)?,
                    surface.output.clone(),
                    *anchor,
                    true,
                )
            })
        });
        let keyboard = self.keyboard_hover_target();
        let desired = pointer.or(keyboard);
        let key = desired.as_ref().map(|target| target.key.clone());
        let now = Instant::now();
        self.hover.reconcile(desired, now);
        if let Some(key) = key
            && self.hover_layout_ready(&key)
        {
            self.hover.resume(&key, now);
        }
    }

    fn keyboard_hover_target(&self) -> Option<HoverTarget<wl_output::WlOutput>> {
        self.surfaces.iter().find_map(|surface| {
            if !matches!(surface.role, Role::Panel(_))
                || self.keyboard_surface.as_ref() != Some(surface.role.surface())
            {
                return None;
            }
            let anchor = surface.input.keyboard_focused()?;
            automatic_target(
                self.configurations.root(&surface.output)?,
                surface.output.clone(),
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
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| matches!(surface.role.popup_mode(), Some(PopupMode::Hover(_))))
        {
            self.remove_popup(index, true, false);
        }
        self.hover.clear();
        self.reconcile_hover_intent();
    }

    fn key(&mut self, event: KeyEvent, serial: u32, direct: bool, qh: &QueueHandle<Self>) {
        let Some(index) = self.keyboard_surface.as_ref().and_then(|wl| self.index(wl)) else {
            return;
        };
        if event.keysym == Keysym::Escape {
            if self.surfaces[index].role.popup_mode().is_some() {
                self.remove_popup(index, true, true);
                self.reconcile_hover_intent();
                return;
            }
            if let Some(popup) = self
                .surfaces
                .iter()
                .position(|surface| matches!(surface.role.popup_mode(), Some(PopupMode::Hover(_))))
            {
                self.remove_popup(popup, true, true);
                self.reconcile_hover_intent();
                return;
            }
        }
        let surface = &mut self.surfaces[index];
        if event.keysym == Keysym::Tab || event.keysym == Keysym::ISO_Left_Tab {
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
                let next = if self.shift {
                    current.map_or(order.len() - 1, |n| (n + order.len() - 1) % order.len())
                } else {
                    current.map_or(0, |n| (n + 1) % order.len())
                };
                surface.input.focus_from_keyboard(order[next]);
                surface.dirty = true;
            }
            self.reconcile_hover_intent();
            return;
        }
        let Some(id) = surface.input.focused else {
            return;
        };
        let output = surface.output.clone();
        let Some(node) = self
            .configurations
            .root(&output)
            .and_then(|root| root.find(id))
        else {
            return;
        };
        let event_surface = self.surfaces[index].role.surface().clone();
        let origin = direct
            .then(|| self.input_origin(event_surface.clone(), serial))
            .flatten();
        if node.kind == Kind::Button
            && (event.keysym == Keysym::Return || event.keysym == Keysym::space)
        {
            self.surfaces[index].input.reveal_keyboard_focus();
            self.surfaces[index].dirty = true;
            self.dispatch(output, id, Event::Click, origin);
            if let Some(index) = self.index(&event_surface) {
                self.sync_popup(index, serial, Some(id), qh);
            }
        } else if node.kind == Kind::Slider {
            let slider = node.slider;
            let step = if slider.step > 0.0 {
                slider.step
            } else {
                (slider.max - slider.min) / 100.0
            };
            let value = match event.keysym {
                Keysym::Left | Keysym::Down => slider.value - step,
                Keysym::Right | Keysym::Up => slider.value + step,
                Keysym::Home => slider.min,
                Keysym::End => slider.max,
                _ => return,
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

fn apply_input_region(
    compositor: &CompositorState,
    surface: &wl_surface::WlSurface,
    input: PanelInputRegion,
    size: (u32, u32),
) -> Result<(), String> {
    match panel::input_region(input, size) {
        ResolvedInputRegion::Full => surface.set_input_region(None),
        ResolvedInputRegion::Region(rect) => {
            let region = Region::new(compositor)
                .map_err(|error| format!("cannot create panel input region: {error}"))?;
            if let Some(rect) = rect {
                region.add(rect.x, rect.y, rect.width, rect.height);
            }
            surface.set_input_region(Some(region.wl_region()));
        }
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
    use std::time::{Duration, Instant};

    use super::{
        PanelState, configured_panel_size, earliest_deadline, fallback_logical_size,
        find_automatic_popup, find_manual_popup, interactive_path, panel_size, valid_dimensions,
    };
    use crate::panel;
    use thing_core::{Kind, Node, NodeId, PanelEdge, PanelMargins};

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
    output: wl_output::WlOutput,
    anchor: NodeId,
    hover: bool,
) -> Option<HoverTarget<wl_output::WlOutput>> {
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
            output,
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
        PopupMode::Automatic(key) => automatic_target(root, key.output.clone(), key.anchor, false)
            .is_some_and(|target| target.key.popup == id),
        PopupMode::Hover(key) => automatic_target(root, key.output.clone(), key.anchor, true)
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
        if let Err(error) = self.add_panel(output, qh) {
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
            if let Err(error) = self.add_panel(output, qh) {
                eprintln!("thing: cannot configure output: {error}");
            }
            return;
        }
        match self.configurations.set_output(&output, context) {
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
        self.hover.remove_output(&output);
        while let Some(index) = self.surfaces.iter().rposition(|surface| {
            surface.output == output && matches!(surface.role, Role::Popup(..))
        }) {
            self.remove_popup(index, true, false);
        }
        if let Err(error) = self.render.park() {
            self.error = Some(error);
        }
        self.surfaces.retain(|surface| surface.output != output);
        self.configurations.remove(&output);
        self.hover.remove_output(&output);
        self.update_watch();
        if self
            .keyboard_surface
            .as_ref()
            .is_some_and(|wl| self.index(wl).is_none())
        {
            self.keyboard_surface = None;
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(index) = self.index(layer.wl_surface()) {
            let output = self.surfaces[index].output.clone();
            self.hover.remove_output(&output);
            while let Some(index) = self.surfaces.iter().rposition(|surface| {
                surface.output == output && matches!(surface.role, Role::Popup(..))
            }) {
                self.remove_popup(index, true, false);
            }
            if let Err(error) = self.render.park() {
                self.error = Some(error);
            }
            self.surfaces.retain(|surface| surface.output != output);
            self.configurations.remove(&output);
            self.hover.remove_output(&output);
            self.update_watch();
            if self
                .keyboard_surface
                .as_ref()
                .is_some_and(|wl| self.index(wl).is_none())
            {
                self.keyboard_surface = None;
            }
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
                surface.panel.and_then(|panel| {
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
                surface.layout = None;
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
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
            self.keyboard_surface = None;
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
            self.keyboard_surface = None;
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
    use super::scroll_distance;

    #[test]
    fn scroll_steps_convert_to_finite_logical_distance() {
        assert_eq!(scroll_distance(1.0), Some(40.0));
        assert_eq!(scroll_distance(-0.5), Some(-20.0));
        assert_eq!(scroll_distance(0.0), None);
        assert_eq!(scroll_distance(f64::NAN), None);
        assert_eq!(scroll_distance(f64::INFINITY), None);
        assert_eq!(scroll_distance(f64::MAX), None);
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
        self.keyboard_surface = Some(surface.clone());
        self.reconcile_hover_intent();
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.keyboard_surface = None;
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
        self.key(event, serial, true, qh);
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.key(event, serial, false, qh);
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
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
        self.shift = modifiers.shift;
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
