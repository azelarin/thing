use std::{
    path::{Path, PathBuf},
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
    output::{OutputHandler, OutputState},
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
use thing_runtime::{Action, Event, Runtime, Scalar};
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
    let mut runtime = Runtime::load(&entry).map_err(|error| error.to_string())?;
    let (time, date, next_minute) = crate::clock::read();
    runtime
        .set_service("clock.time", Scalar::String(time))
        .map_err(|e| e.to_string())?;
    runtime
        .set_service("clock.date", Scalar::String(date))
        .map_err(|e| e.to_string())?;
    runtime.refresh().map_err(|e| e.to_string())?;
    let watch = Rc::new(Watch::new().map_err(|e| e.to_string())?);
    watch
        .set_files(&runtime.files())
        .map_err(|e| e.to_string())?;
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("cannot connect to Wayland: {error}"))?;
    let (globals, queue) = registry_queue_init(&connection).map_err(|error| error.to_string())?;
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
        runtime,
        entry,
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
        exit: false,
        error: None,
    };
    let outputs: Vec<_> = app.outputs.outputs().collect();
    for output in outputs {
        app.add_panel(output, &qh);
    }
    let mut event_loop: EventLoop<App> = EventLoop::try_new().map_err(|error| error.to_string())?;
    event_loop
        .handle()
        .insert_source(image_source, |_, _, app| app.invalidate())
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
            if let Err(error) = app.runtime.set_service("clock.time", Scalar::String(time)) {
                eprintln!("thing: {error}");
            }
            if let Err(error) = app.runtime.set_service("clock.date", Scalar::String(date)) {
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
            app.reload()?;
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
    runtime: Runtime,
    audio: Audio,
    niri: Niri,
    entry: PathBuf,
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
            .runtime
            .set_niri(snapshot.state)
            .and_then(|()| {
                self.runtime
                    .set_service("niri.available", Scalar::Boolean(snapshot.available))
            })
            .and_then(|()| {
                self.runtime
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
            self.runtime
                .set_audio_devices(snapshot.devices)
                .map_err(|e| e.to_string())?;
            for (name, value) in [
                ("audio.available", Scalar::Boolean(snapshot.available)),
                ("audio.volume", Scalar::Number(snapshot.volume)),
                ("audio.muted", Scalar::Boolean(snapshot.muted)),
                ("audio.device", Scalar::String(snapshot.device)),
                ("audio.error", Scalar::String(snapshot.error)),
            ] {
                self.runtime
                    .set_service(name, value)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })();
        if let Err(error) = update {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    fn refresh(&mut self) {
        match self.runtime.refresh() {
            Ok(true) => self.invalidate(),
            Ok(false) => {}
            Err(error) => eprintln!("thing: {error}"),
        }
    }

    fn reload(&mut self) -> Result<(), String> {
        if let Err(error) = self.runtime.reload(&self.entry) {
            eprintln!("thing: reload failed; keeping the previous panel: {error}");
            return Ok(());
        }
        if let Err(error) = self.watch.set_files(&self.runtime.files()) {
            eprintln!("thing: cannot watch all configuration files: {error}");
        }
        self.graphics.park()?;
        // release the previous configuration's glyph atlas and text layouts.
        let wake = self.image_ping.clone();
        self.painter = Some(Painter::new(self.graphics.renderer()?, move || {
            wake.ping()
        })?);
        self.surfaces
            .retain(|surface| matches!(surface.role, Role::Panel(_)));
        self.keyboard_surface = None;
        let height = self
            .runtime
            .root()
            .style
            .height
            .unwrap_or(36.0)
            .round()
            .clamp(1.0, 4096.0) as u32;
        for surface in &mut self.surfaces {
            surface.root = self.runtime.root().id;
            surface.layout = None;
            surface.input = Interaction::default();
            surface.press_serial = None;
            surface.pressed_button = None;
            surface.dirty = true;
            if let Role::Panel(layer) = &surface.role {
                layer.set_size(0, height);
                layer.set_exclusive_zone(height as i32);
                layer.wl_surface().commit();
            }
        }
        eprintln!("thing: reloaded {}", self.entry.display());
        Ok(())
    }

    fn add_panel(&mut self, output: wl_output::WlOutput, qh: &QueueHandle<Self>) {
        if self
            .surfaces
            .iter()
            .any(|surface| matches!(surface.role, Role::Panel(_)) && surface.output == output)
        {
            return;
        }
        let height = self
            .runtime
            .root()
            .style
            .height
            .unwrap_or(36.0)
            .round()
            .clamp(1.0, 4096.0) as u32;
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
            self.runtime.root().id,
            (800, height),
            qh,
        );
        surface.role.surface().commit();
        self.surfaces.push(surface);
    }

    fn make_surface(
        &self,
        role: Role,
        output: wl_output::WlOutput,
        root: NodeId,
        size: (u32, u32),
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
            scale: Scale::default(),
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

    fn index(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|surface| surface.role.surface() == wl)
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let Some(painter) = self.painter.as_mut() else {
            return Ok(());
        };
        for surface in &mut self.surfaces {
            if !surface.configured || !surface.dirty || surface.waiting {
                continue;
            }
            let Some(root) = self.runtime.root().find(surface.root) else {
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

    fn dispatch(&mut self, id: NodeId, event: Event) {
        match self.runtime.dispatch(id, event) {
            Ok(true) => self.invalidate(),
            Ok(false) => {}
            Err(error) => eprintln!("thing: {error}"),
        }
        for action in self.runtime.take_actions() {
            let result = match action {
                Action::Niri(action) => self.niri.send(action),
                action => self.audio.send(action).map_err(str::to_owned),
            };
            if let Err(error) = result {
                eprintln!("thing: {error}");
            }
        }
    }

    fn invalidate(&mut self) {
        let desired = find_popup(self.runtime.root(), None).map(|(id, _)| id);
        if let Some(index) = self.surfaces.iter().position(|surface| {
            matches!(surface.role, Role::Popup(_)) && Some(surface.root) != desired
        }) {
            self.close_popup(index);
        }
        for surface in &mut self.surfaces {
            surface.dirty = true;
        }
    }

    fn slider(&mut self, surface: usize, id: NodeId, x: f32) {
        let Some(node) = self.runtime.root().find(id) else {
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
        self.dispatch(id, Event::Change(node.slider.value_at(fraction as f64)));
    }

    fn close_popup(&mut self, index: usize) {
        let id = self.surfaces[index].root;
        if let Err(error) = self.graphics.park() {
            self.error = Some(error);
        }
        self.surfaces.remove(index);
        self.dispatch(id, Event::Close);
        self.keyboard_surface = None;
    }

    fn sync_popup(&mut self, parent: usize, serial: u32, qh: &QueueHandle<Self>) {
        let desired = find_popup(self.runtime.root(), None);
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|s| matches!(s.role, Role::Popup(_)))
        {
            if desired
                .as_ref()
                .is_none_or(|(id, _)| *id != self.surfaces[index].root)
            {
                self.close_popup(index);
            } else {
                return;
            }
        }
        // closing the previous popup may run a handler that changes visibility.
        let Some((id, anchor_id)) = find_popup(self.runtime.root(), None) else {
            return;
        };
        let Some(parent) = self.surfaces.get(parent) else {
            return;
        };
        let Role::Panel(layer) = &parent.role else {
            return;
        };
        let Some(node) = self.runtime.root().find(id) else {
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
                    parent.output.clone(),
                    id,
                    (width as u32, height as u32),
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
                surface.input.focused = Some(order[next]);
                surface.dirty = true;
            }
            return;
        }
        let Some(id) = surface.input.focused else {
            return;
        };
        let Some(node) = self.runtime.root().find(id) else {
            return;
        };
        if node.kind == Kind::Button
            && (event.keysym == Keysym::Return || event.keysym == Keysym::space)
        {
            self.dispatch(id, Event::Click);
            self.sync_popup(index, serial, qh);
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
            self.dispatch(id, Event::Change(value.clamp(slider.min, slider.max)));
        }
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
                surface.scale =
                    Scale::from_120ths(scale.clamp(1, 16) as u32 * 120).unwrap_or_default();
                surface.dirty = true;
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
        self.add_panel(output, qh);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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
                    surface.input.focused = hit;
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
                    self.surfaces[index].dirty = true;
                    if let Some(id) = pressed.filter(|id| Some(*id) == hit) {
                        self.dispatch(
                            id,
                            match button {
                                0x111 => Event::SecondaryClick,
                                0x112 => Event::MiddleClick,
                                _ => Event::Click,
                            },
                        );
                        if let Some(serial) = serial {
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
                        self.dispatch(id, Event::Scroll { x, y });
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
            app.surfaces[index].scale = scale;
            app.surfaces[index].dirty = true;
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
