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
    compositor::{CompositorHandler, CompositorState},
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
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        xdg::{
            XdgShell,
            popup::{Popup, PopupConfigure, PopupHandler},
            window::{Window as XdgWindow, WindowConfigure, WindowHandler},
        },
    },
};
use thing_core::{Kind, Node, NodeId, Point, Rect, Scale};
use thing_runtime::{Action, Event, OutputContext, Scalar, ServiceSnapshot};
use thing_ui::{Interaction, Layout, Painter, Size};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
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
    xdg::shell::client::xdg_positioner,
};

use crate::{
    audio::Audio,
    configuration::Configurations,
    egl::{Graphics, Window},
    niri::Niri,
    watch::Watch,
};

/// run one panel per output. the loop sleeps until input, configuration, or a
/// compositor frame callback arrives; an unchanged panel does not request frames.
pub fn run(entry: &Path) -> Result<(), String> {
    let entry = entry
        .canonicalize()
        .map_err(|error| format!("{}: {error}", entry.display()))?;
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
    let graphics = Graphics::new(&connection)?;
    let (image_ping, image_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let wake = image_ping.clone();
    let painter = Painter::new(graphics.renderer()?, move || wake.ping())?;
    let (audio_ping, audio_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let audio = Audio::start(audio_ping).map_err(|e| format!("cannot start audio worker: {e}"))?;
    let (niri_ping, niri_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let niri = Niri::start(niri_ping).map_err(|e| format!("cannot start niri worker: {e}"))?;
    let mut app = App {
        painter: Some(painter),
        image_ping,
        graphics,
        configurations: Configurations::new(entry, services),
        watch: watch.clone(),
        reload_at: None,
        audio,
        niri,
        surfaces: Vec::new(),
        registry: RegistryState::new(&globals),
        seats: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        xdg_shell,
        fractional: globals.bind(&qh, 1..=1, ()).ok(),
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        pointer: None,
        keyboard: None,
        seat: None,
        keyboard_surface: None,
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
        if app
            .reload_at
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            app.reload_at = None;
            app.reload(&qh)?;
        }
        app.draw(&qh)?;
        connection.flush().map_err(|error| error.to_string())?;
        event_loop
            .dispatch(
                app.reload_at
                    .map(|deadline| deadline.saturating_duration_since(Instant::now())),
                &mut app,
            )
            .map_err(|error| error.to_string())?;
        if let Some(error) = app.error.take() {
            return Err(error);
        }
    }
    Ok(())
}

enum Role {
    Panel(LayerSurface),
    Popup(Popup),
}
impl Role {
    fn surface(&self) -> &wl_surface::WlSurface {
        match self {
            Self::Panel(layer) => layer.wl_surface(),
            Self::Popup(popup) => popup.wl_surface(),
        }
    }
}

struct Surface {
    // release egl before the wayland role and its underlying surface.
    window: Option<Window>,
    role: Role,
    output: wl_output::WlOutput,
    root: NodeId,
    size: (u32, u32),
    requested_height: u32,
    scale: Scale,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    layout: Option<Layout>,
    input: Interaction,
    press_serial: Option<u32>,
    pressed_button: Option<u32>,
    configured: bool,
    dirty: bool,
    waiting: bool,
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
    painter: Option<Painter>,
    image_ping: calloop::ping::Ping,
    graphics: Graphics,
    configurations: Configurations<wl_output::WlOutput>,
    audio: Audio,
    niri: Niri,
    watch: Rc<Watch>,
    reload_at: Option<Instant>,
    surfaces: Vec<Surface>,
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    xdg_shell: XdgShell,
    fractional: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat: Option<wl_seat::WlSeat>,
    keyboard_surface: Option<wl_surface::WlSurface>,
    shift: bool,
    loading_outputs: bool,
    exit: bool,
    error: Option<String>,
}

impl Drop for App {
    fn drop(&mut self) {
        // gl-owned resources must be destroyed with a current context, including
        // when all outputs have been unplugged or startup failed partway through.
        let _ = self.graphics.park();
        self.painter.take();
        // popups are appended after their parents and must be released first.
        while self.surfaces.pop().is_some() {}
    }
}

impl App {
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
        let update = (|| -> Result<(), String> {
            self.configurations.set_audio_devices(snapshot.devices)?;
            for (name, value) in [
                ("audio.available", Scalar::Boolean(snapshot.available)),
                ("audio.volume", Scalar::Number(snapshot.volume)),
                ("audio.muted", Scalar::Boolean(snapshot.muted)),
                ("audio.device", Scalar::String(snapshot.device)),
                ("audio.error", Scalar::String(snapshot.error)),
            ] {
                self.configurations.set_service(name, value)?;
            }
            Ok(())
        })();
        if let Err(error) = update {
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
        self.graphics.park()?;
        // release the previous configuration's glyph atlas and text layouts.
        let wake = self.image_ping.clone();
        self.painter = Some(Painter::new(self.graphics.renderer()?, move || {
            wake.ping()
        })?);
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
            surface.input = Interaction::default();
            surface.press_serial = None;
            surface.pressed_button = None;
            surface.dirty = true;
            if let Role::Panel(layer) = &surface.role {
                let height = panel_height(root);
                surface.requested_height = height;
                layer.set_size(0, height);
                layer.set_exclusive_zone(height as i32);
                layer.wl_surface().commit();
            }
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
        let height = panel_height(root);
        let scale = self
            .configurations
            .output(&output)
            .map(|context| context.scale)
            .unwrap_or_default();
        let wl = self.compositor.create_surface(qh);
        let layer =
            self.layer_shell
                .create_layer_surface(qh, wl, Layer::Top, Some("thing"), Some(&output));
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, height);
        layer.set_exclusive_zone(height as i32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        let surface = self.make_surface(
            Role::Panel(layer),
            output,
            root.id,
            (800, height),
            scale,
            qh,
        );
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
            requested_height: size.1,
            scale,
            viewport,
            fractional,
            layout: None,
            input: Interaction::default(),
            press_serial: None,
            pressed_button: None,
            configured: false,
            dirty: true,
            waiting: false,
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

    fn sync_panel_height(&mut self, output: &wl_output::WlOutput) {
        let Some(height) = self.configurations.root(output).map(panel_height) else {
            return;
        };
        let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|surface| &surface.output == output && matches!(surface.role, Role::Panel(_)))
        else {
            return;
        };
        if surface.requested_height == height {
            return;
        }
        let Role::Panel(layer) = &surface.role else {
            return;
        };
        surface.requested_height = height;
        surface.layout = None;
        surface.dirty = true;
        layer.set_size(0, height);
        layer.set_exclusive_zone(height as i32);
        layer.wl_surface().commit();
    }

    fn set_output_scale(&mut self, output: &wl_output::WlOutput, scale: Scale) {
        let Some(mut context) = self.configurations.output(output).cloned() else {
            return;
        };
        if context.scale == scale {
            return;
        }
        context.scale = scale;
        match self.configurations.set_output(output, context) {
            Ok(true) => self.invalidate_output(output),
            Ok(false) => {}
            Err(error) => eprintln!("thing: cannot update output scale: {error}"),
        }
    }

    fn index(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|surface| surface.role.surface() == wl)
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let Some(painter) = self.painter.as_mut() else {
            return Ok(());
        };
        let configurations = &self.configurations;
        for surface in &mut self.surfaces {
            if !surface.configured || !surface.dirty || surface.waiting {
                continue;
            }
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
            if let Some(viewport) = &surface.viewport {
                surface.role.surface().set_buffer_scale(1);
                viewport.set_destination(surface.size.0 as i32, surface.size.1 as i32);
            } else {
                surface
                    .role
                    .surface()
                    .set_buffer_scale(surface.scale.factor().round() as i32);
            }
            if let Some(window) = &surface.window {
                window.resize(physical.0, physical.1)?;
            } else {
                surface.window = Some(self.graphics.window(
                    surface.role.surface(),
                    physical.0,
                    physical.1,
                )?);
            }
            let window = surface
                .window
                .as_ref()
                .ok_or("EGL window was not initialized")?;
            self.graphics.make_current(window)?;
            let layout = painter.layout(
                root,
                Size {
                    width: surface.size.0 as f32,
                    height: surface.size.1 as f32,
                },
            )?;
            painter.paint(
                root,
                &layout,
                physical,
                surface.scale.factor(),
                surface.input,
            )?;
            surface
                .role
                .surface()
                .frame(qh, surface.role.surface().clone());
            surface.waiting = true;
            window.present()?;
            surface.layout = Some(layout);
            surface.dirty = false;
        }
        Ok(())
    }

    fn dispatch(&mut self, output: wl_output::WlOutput, id: NodeId, event: Event) {
        let (changed, actions) = match self.configurations.dispatch(&output, id, event) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("thing: {error}");
                return;
            }
        };
        if changed {
            self.invalidate_output(&output);
        }
        for action in actions {
            let result = match action {
                Action::Niri(action) => self.niri.send(action),
                action => self.audio.send(action).map_err(str::to_owned),
            };
            if let Err(error) = result {
                eprintln!("thing: {error}");
            }
        }
    }

    fn invalidate_output(&mut self, output: &wl_output::WlOutput) {
        let desired = self
            .configurations
            .root(output)
            .and_then(|root| find_popup(root, None))
            .map(|(id, _)| id);
        if let Some(index) = self.surfaces.iter().position(|surface| {
            &surface.output == output
                && matches!(surface.role, Role::Popup(_))
                && Some(surface.root) != desired
        }) {
            self.close_popup(index);
        }
        self.sync_panel_height(output);
        for surface in self
            .surfaces
            .iter_mut()
            .filter(|surface| &surface.output == output)
        {
            surface.dirty = true;
        }
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
        self.dispatch(output, id, Event::Change(value));
    }

    fn close_popup(&mut self, index: usize) {
        let id = self.surfaces[index].root;
        let output = self.surfaces[index].output.clone();
        if let Err(error) = self.graphics.park() {
            self.error = Some(error);
        }
        self.surfaces.remove(index);
        self.dispatch(output, id, Event::Close);
        self.keyboard_surface = None;
    }

    fn sync_popup(&mut self, parent: usize, serial: u32, qh: &QueueHandle<Self>) {
        let Some(parent_surface) = self.surfaces.get(parent) else {
            return;
        };
        let parent_wl = parent_surface.role.surface().clone();
        let output = parent_surface.output.clone();
        let desired = self
            .configurations
            .root(&output)
            .and_then(|root| find_popup(root, None));
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|s| matches!(s.role, Role::Popup(_)))
        {
            let current = &self.surfaces[index];
            if current.output != output
                || desired.as_ref().is_none_or(|(id, _)| *id != current.root)
            {
                self.close_popup(index);
            } else {
                return;
            }
        }
        // closing the previous popup may run a handler that changes visibility.
        let Some((id, anchor_id)) = self
            .configurations
            .root(&output)
            .and_then(|root| find_popup(root, None))
        else {
            return;
        };
        let Some(parent) = self
            .index(&parent_wl)
            .and_then(|index| self.surfaces.get(index))
        else {
            return;
        };
        let Role::Panel(layer) = &parent.role else {
            return;
        };
        let layer = layer.clone();
        let parent_output = parent.output.clone();
        let parent_scale = parent.scale;
        let Some(node) = self
            .configurations
            .root(&output)
            .and_then(|root| root.find(id))
        else {
            return;
        };
        let width = node.style.width.unwrap_or(320.0).round().clamp(1.0, 4096.0) as i32;
        let height = node
            .style
            .height
            .unwrap_or(200.0)
            .round()
            .clamp(1.0, 4096.0) as i32;
        let anchor = anchor_id
            .and_then(|id| parent.layout.as_ref()?.get(id))
            .map(|p| p.bounds)
            .unwrap_or(Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: parent.size.1 as f32,
            });
        let positioner = self.xdg_shell.xdg_wm_base().create_positioner(qh, ());
        positioner.set_size(width, height);
        positioner.set_anchor_rect(
            anchor.x.round() as i32,
            anchor.y.round() as i32,
            anchor.width.max(1.0).round() as i32,
            anchor.height.max(1.0).round() as i32,
        );
        positioner.set_anchor(xdg_positioner::Anchor::BottomRight);
        positioner.set_gravity(xdg_positioner::Gravity::BottomLeft);
        positioner.set_constraint_adjustment(
            xdg_positioner::ConstraintAdjustment::SlideX
                | xdg_positioner::ConstraintAdjustment::FlipX
                | xdg_positioner::ConstraintAdjustment::FlipY,
        );
        let wl = self.compositor.create_surface(qh);
        let popup = Popup::from_surface(None, &positioner, qh, wl, &self.xdg_shell);
        positioner.destroy();
        match popup {
            Ok(popup) => {
                layer.get_popup(popup.xdg_popup());
                if let Some(seat) = &self.seat {
                    popup.xdg_popup().grab(seat, serial);
                }
                let surface = self.make_surface(
                    Role::Popup(popup),
                    parent_output,
                    id,
                    (width as u32, height as u32),
                    parent_scale,
                    qh,
                );
                surface.role.surface().commit();
                self.surfaces.push(surface);
            }
            Err(error) => eprintln!("thing: cannot open popup: {error}"),
        }
    }

    fn key(&mut self, event: KeyEvent, serial: u32, qh: &QueueHandle<Self>) {
        let Some(index) = self.keyboard_surface.as_ref().and_then(|wl| self.index(wl)) else {
            return;
        };
        if event.keysym == Keysym::Escape && matches!(self.surfaces[index].role, Role::Popup(_)) {
            self.close_popup(index);
            return;
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
        if node.kind == Kind::Button
            && (event.keysym == Keysym::Return || event.keysym == Keysym::space)
        {
            self.surfaces[index].input.reveal_keyboard_focus();
            self.surfaces[index].dirty = true;
            let event_surface = self.surfaces[index].role.surface().clone();
            self.dispatch(output, id, Event::Click);
            if let Some(index) = self.index(&event_surface) {
                self.sync_popup(index, serial, qh);
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
            );
        }
    }
}

fn panel_height(root: &Node) -> u32 {
    root.style.height.unwrap_or(36.0).round().clamp(1.0, 4096.0) as u32
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
    use super::{fallback_logical_size, valid_dimensions};

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
    fn rejects_unknown_or_invalid_output_dimensions() {
        assert_eq!(valid_dimensions((0, 1080)), None);
        assert_eq!(valid_dimensions((-1, 1080)), None);
        assert_eq!(fallback_logical_size((1, 1), 2, false), None);
    }
}

fn find_popup(node: &Node, anchor: Option<NodeId>) -> Option<(NodeId, Option<NodeId>)> {
    if !node.visible {
        return None;
    }
    if node.kind == Kind::Popup {
        return Some((node.id, anchor));
    }
    let anchor = if node.kind == Kind::Button {
        Some(node.id)
    } else {
        anchor
    };
    node.children
        .iter()
        .find_map(|child| find_popup(child, anchor))
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
        while let Some(index) = self
            .surfaces
            .iter()
            .rposition(|surface| surface.output == output && matches!(surface.role, Role::Popup(_)))
        {
            self.close_popup(index);
        }
        if let Err(error) = self.graphics.park() {
            self.error = Some(error);
        }
        self.surfaces.retain(|surface| surface.output != output);
        self.configurations.remove(&output);
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
            while let Some(index) = self.surfaces.iter().rposition(|surface| {
                surface.output == output && matches!(surface.role, Role::Popup(_))
            }) {
                self.close_popup(index);
            }
            if let Err(error) = self.graphics.park() {
                self.error = Some(error);
            }
            self.surfaces.retain(|surface| surface.output != output);
            self.configurations.remove(&output);
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
            let surface = &mut self.surfaces[index];
            if config.new_size.0 != 0 {
                surface.size.0 = config.new_size.0;
            }
            if config.new_size.1 != 0 {
                surface.size.1 = config.new_size.1;
            }
            surface.configured = true;
            surface.dirty = true;
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
        popup: &Popup,
        config: PopupConfigure,
    ) {
        if let Some(index) = self.index(popup.wl_surface()) {
            let surface = &mut self.surfaces[index];
            surface.size = (config.width.max(1) as u32, config.height.max(1) as u32);
            surface.configured = true;
            surface.dirty = true;
        }
    }
    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if let Some(index) = self.index(popup.wl_surface()) {
            self.close_popup(index);
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
        }
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
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
            for surface in &mut self.surfaces {
                surface.input = Interaction::default();
                surface.press_serial = None;
                surface.pressed_button = None;
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
                    if surface.input.hovered != hit {
                        surface.input.hovered = hit;
                        surface.dirty = true;
                    }
                    if surface.pressed_button == Some(0x110)
                        && let Some(id) = surface.input.pressed
                    {
                        self.slider(index, id, point.x);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.surfaces[index].input.hovered = None;
                    self.surfaces[index].dirty = true;
                }
                PointerEventKind::Press { button, serial, .. }
                    if matches!(button, 0x110..=0x112) =>
                {
                    let surface = &mut self.surfaces[index];
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
                PointerEventKind::Release { button, .. }
                    if self.surfaces[index].pressed_button == Some(button) =>
                {
                    self.surfaces[index].pressed_button = None;
                    let pressed = self.surfaces[index].input.pressed.take();
                    let serial = self.surfaces[index].press_serial.take();
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
                        );
                        if let Some((index, serial)) = self.index(&event_surface).zip(serial) {
                            self.sync_popup(index, serial, qh);
                        }
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    let x = scroll_steps(horizontal);
                    let y = scroll_steps(vertical);
                    if (x != 0.0 || y != 0.0)
                        && let Some(id) = hit
                    {
                        let output = self.surfaces[index].output.clone();
                        self.dispatch(output, id, Event::Scroll { x, y });
                    }
                }
                _ => {}
            }
        }
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
    }
    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.key(event, serial, qh);
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.key(event, serial, qh);
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
delegate_noop!(App: ignore xdg_positioner::XdgPositioner);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
