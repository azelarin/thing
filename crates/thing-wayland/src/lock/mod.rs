mod model;
#[cfg(test)]
mod wire_tests;

use std::{
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use calloop::{
    EventLoop, Interest, Mode, PostAction,
    generic::Generic,
    signals::{Signal, Signals},
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_session_lock,
    output::{OutputHandler, OutputInfo, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
};
#[cfg(test)]
use thing_core::NodeId;
use thing_core::{Kind, Node, Scale};
use thing_runtime::{
    AuthenticationSnapshot, AuthenticationState, OutputContext, Scalar, ServiceSnapshot,
    SurfaceKey, SurfaceTree,
};
use thing_ui::{Interaction, Layout, PaintFrame, ScrollState, Size};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_callback, wl_output, wl_surface},
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use self::model::{Action as LifecycleAction, Lifecycle, Phase};
use crate::{
    Renderer,
    authentication::{
        AuthenticationBroker, Event as AuthenticationEvent, ProcessContext, harden_secret_process,
        preflight_policy,
    },
    configuration::Configurations,
    render::{FrameResult, Render, Window},
    watch::Watch,
};

const RELOAD_DELAY: Duration = Duration::from_millis(80);
const AUTHENTICATION_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// lock the current Wayland session with the configured lock surfaces.
pub fn run(entry: &Path, renderer: Renderer) -> Result<(), String> {
    if renderer == Renderer::Vulkan && !cfg!(feature = "vulkan") {
        return Err(
            "vulkan support is not included in this build; rebuild with --features vulkan".into(),
        );
    }
    let entry = entry
        .canonicalize()
        .map_err(|error| format!("{}: {error}", entry.display()))?;
    preflight_policy().map_err(|error| format!("cannot use the thing PAM policy: {error}"))?;
    let process = ProcessContext::prepare()
        .map_err(|error| format!("cannot prepare authentication helpers: {error}"))?;
    let child_signals = Signals::new(&[Signal::SIGCHLD])
        .map_err(|error| format!("cannot monitor authentication helpers: {error}"))?;
    harden_secret_process().map_err(|error| format!("cannot protect the lock process: {error}"))?;

    let (time, date, next_minute) = crate::clock::read();
    let authentication = AuthenticationSnapshot {
        available: true,
        state: AuthenticationState::Idle,
        prompt: String::new(),
        message: String::new(),
    };
    let mut services = ServiceSnapshot::default();
    services
        .set_service("clock.time", Scalar::String(time))
        .map_err(|error| error.to_string())?;
    services
        .set_service("clock.date", Scalar::String(date))
        .map_err(|error| error.to_string())?;
    services
        .set_authentication(authentication.clone())
        .map_err(|error| error.to_string())?;

    let watch = Rc::new(Watch::new().map_err(|error| error.to_string())?);
    watch
        .set_files(std::slice::from_ref(&entry))
        .map_err(|error| error.to_string())?;
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("cannot connect to Wayland: {error}"))?;
    let (globals, mut queue) =
        registry_queue_init(&connection).map_err(|error| error.to_string())?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(|error| error.to_string())?;
    let session_lock_state = SessionLockState::new(&globals, &qh);
    let fractional = globals.bind(&qh, 1..=1, ()).ok();
    let viewporter = globals.bind(&qh, 1..=1, ()).ok();
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
    let mut event_loop: EventLoop<'static, App> =
        EventLoop::try_new().map_err(|error| error.to_string())?;
    let mut app = App {
        connection: connection.clone(),
        render,
        image_ping,
        configurations: Configurations::new_lock(entry, services),
        watch: watch.clone(),
        reload_at: None,
        authentication: AuthenticationBroker::new(process, 1),
        authentication_snapshot: authentication,
        surfaces: Vec::new(),
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        compositor,
        session_lock_state,
        session_lock: None,
        unlock_sync: None,
        unlock_requested: false,
        fractional,
        viewporter,
        lifecycle: Lifecycle::new(),
        queue: qh.clone(),
        loading_outputs: true,
        render_failed: false,
        exit: false,
        result: Ok(()),
    };
    queue
        .roundtrip(&mut app)
        .map_err(|error| error.to_string())?;
    let advertised_outputs: Vec<_> = app.outputs.outputs().collect();
    if advertised_outputs.is_empty() {
        return Err("cannot lock a session without an advertised output".into());
    }
    let outputs: Vec<_> = advertised_outputs
        .into_iter()
        .map(|output| {
            app.output_context(&output)
                .map(|context| (output, context))
                .ok_or_else(|| {
                    "cannot determine the logical size of every advertised output".to_owned()
                })
        })
        .collect::<Result<_, _>>()?;
    app.configurations
        .reload_for_checked(outputs.clone(), |_, runtime| {
            validate_lock_surfaces(runtime.surfaces())
        })?;
    app.update_watch();

    event_loop
        .handle()
        .insert_source(child_signals, |_, _, app| app.poll_authentication())
        .map_err(|error| error.to_string())?;
    event_loop
        .handle()
        .insert_source(image_source, |_, _, app| app.invalidate_all())
        .map_err(|error| error.to_string())?;
    WaylandSource::new(connection.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|error| error.to_string())?;
    event_loop
        .handle()
        .insert_source(
            Generic::new(watch, Interest::READ, Mode::Level),
            |_, watch, app| {
                if watch.changed()? {
                    app.reload_at = Some(Instant::now() + RELOAD_DELAY);
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| error.to_string())?;
    event_loop
        .handle()
        .insert_source(Timer::from_duration(next_minute), |_, _, app| {
            let (time, date, next) = crate::clock::read();
            if app
                .configurations
                .set_service("clock.time", Scalar::String(time))
                .and_then(|()| {
                    app.configurations
                        .set_service("clock.date", Scalar::String(date))
                })
                .is_ok()
            {
                app.refresh();
            }
            TimeoutAction::ToDuration(next)
        })
        .map_err(|error| error.to_string())?;

    // all fallible host setup completes before the compositor receives the
    // lock request. from this point onward, failures are handled according to
    // whether the compositor has confirmed the locked state.
    let session_lock = app
        .session_lock_state
        .lock(&qh)
        .map_err(|error| format!("the compositor must support ext-session-lock-v1: {error}"))?;
    app.session_lock = Some(session_lock);
    for (output, _) in outputs {
        app.create_surface(output, &qh);
    }
    app.loading_outputs = false;

    while !app.exit {
        app.poll_authentication();
        if app
            .reload_at
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            app.reload_at = None;
            app.reload();
        }
        app.draw(&qh);
        connection.flush().map_err(|error| error.to_string())?;
        let timeout = app
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        event_loop
            .dispatch(timeout, &mut app)
            .map_err(|error| error.to_string())?;
    }
    app.result.clone()
}

struct Surface {
    window: Option<Window>,
    role: SessionLockSurface,
    output: wl_output::WlOutput,
    size: (u32, u32),
    scale: Scale,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    layout: Option<Layout>,
    scroll: ScrollState,
    input: Interaction,
    configured: bool,
    dirty: bool,
    waiting: bool,
    retry_at: Option<Instant>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some(fractional) = self.fractional.take() {
            fractional.destroy();
        }
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
    }
}

struct App {
    connection: Connection,
    render: Render,
    image_ping: calloop::ping::Ping,
    configurations: Configurations<wl_output::WlOutput>,
    watch: Rc<Watch>,
    reload_at: Option<Instant>,
    authentication: AuthenticationBroker,
    authentication_snapshot: AuthenticationSnapshot,
    surfaces: Vec<Surface>,
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    session_lock_state: SessionLockState,
    session_lock: Option<SessionLock>,
    unlock_sync: Option<wl_callback::WlCallback>,
    unlock_requested: bool,
    fractional: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    lifecycle: Lifecycle,
    queue: QueueHandle<App>,
    loading_outputs: bool,
    render_failed: bool,
    exit: bool,
    result: Result<(), String>,
}

impl Drop for App {
    fn drop(&mut self) {
        if self.authentication.is_active() {
            self.authentication.cancel(Instant::now());
        }
        self.render.prepare_drop();
        while self.surfaces.pop().is_some() {}
        if matches!(self.lifecycle.phase(), Phase::Locked | Phase::FailedLocked)
            && let Some(lock) = self.session_lock.take()
        {
            std::mem::forget(lock);
        }
    }
}

impl App {
    fn next_deadline(&self) -> Option<Instant> {
        let authentication = self
            .authentication
            .is_active()
            .then(|| Instant::now() + AUTHENTICATION_POLL_INTERVAL);
        self.reload_at
            .into_iter()
            .chain(self.surfaces.iter().filter_map(|surface| surface.retry_at))
            .chain(authentication)
            .min()
    }

    fn update_watch(&self) {
        if let Err(error) = self.watch.set_files(&self.configurations.files()) {
            eprintln!("thing: cannot watch all lock configuration files: {error}");
        }
    }

    fn output_context(&self, output: &wl_output::WlOutput) -> Option<OutputContext> {
        let mut context = output_context(self.outputs.info(output)?)?;
        if let Some(surface) = self
            .surfaces
            .iter()
            .find(|surface| &surface.output == output)
        {
            context.scale = surface.scale;
        }
        Some(context)
    }

    fn create_surface(&mut self, output: wl_output::WlOutput, qh: &QueueHandle<Self>) {
        if self.surfaces.iter().any(|surface| surface.output == output) {
            return;
        }
        let Some(lock) = self.session_lock.as_ref() else {
            return;
        };
        let wl = self.compositor.create_surface(qh);
        let role = lock.create_lock_surface(wl, &output, qh);
        let viewport = self
            .viewporter
            .as_ref()
            .map(|manager| manager.get_viewport(role.wl_surface(), qh, ()));
        let fractional = if viewport.is_some() {
            self.fractional.as_ref().map(|manager| {
                manager.get_fractional_scale(role.wl_surface(), qh, role.wl_surface().clone())
            })
        } else {
            None
        };
        let scale = self
            .configurations
            .output(&output)
            .map_or_else(Scale::default, |context| context.scale);
        self.surfaces.push(Surface {
            window: None,
            role,
            output,
            size: (1, 1),
            scale,
            viewport,
            fractional,
            layout: None,
            scroll: ScrollState::default(),
            input: Interaction::default(),
            configured: false,
            dirty: true,
            waiting: false,
            retry_at: None,
        });
    }

    fn remove_surface(&mut self, output: &wl_output::WlOutput) {
        let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| &surface.output == output)
        else {
            return;
        };
        if let Err(error) = self.render.park() {
            self.fail(error);
        }
        self.surfaces.remove(index);
    }

    fn reload(&mut self) {
        let outputs = self
            .outputs
            .outputs()
            .filter_map(|output| {
                self.output_context(&output)
                    .or_else(|| self.configurations.output(&output).cloned())
                    .map(|context| (output, context))
            })
            .collect();
        if let Err(error) = self
            .configurations
            .reload_for_checked(outputs, |_, runtime| {
                validate_lock_surfaces(runtime.surfaces())
            })
        {
            eprintln!("thing: lock reload failed; keeping the previous surfaces: {error}");
            return;
        }
        for surface in &mut self.surfaces {
            surface.layout = None;
            surface.scroll = ScrollState::default();
            surface.input = Interaction::default();
            surface.dirty = true;
        }
        let wake = self.image_ping.clone();
        if let Err(error) = self.render.reset_painter(move || wake.ping()) {
            eprintln!("thing: cannot refresh lock renderer caches: {error}");
        }
        self.update_watch();
        eprintln!("thing: reloaded {}", self.configurations.entry().display());
    }

    fn refresh(&mut self) {
        let changed = self
            .configurations
            .refresh_checked(|_, surfaces| validate_lock_surfaces(surfaces));
        for output in changed {
            if let Some(surface) = self
                .surfaces
                .iter_mut()
                .find(|surface| surface.output == output)
            {
                surface.dirty = true;
            }
        }
    }

    fn set_authentication(&mut self, snapshot: AuthenticationSnapshot) {
        if snapshot == self.authentication_snapshot {
            return;
        }
        if let Err(error) = self.configurations.set_authentication(snapshot.clone()) {
            self.fail(error);
            return;
        }
        self.authentication_snapshot = snapshot;
        self.refresh();
    }

    fn poll_authentication(&mut self) {
        for event in self.authentication.poll(Instant::now()) {
            match event {
                AuthenticationEvent::Prompt { text, .. } => {
                    self.set_authentication(AuthenticationSnapshot {
                        available: true,
                        state: AuthenticationState::Waiting,
                        prompt: text,
                        message: String::new(),
                    });
                }
                AuthenticationEvent::Info { text, .. }
                | AuthenticationEvent::Error { text, .. } => {
                    let mut snapshot = self.authentication_snapshot.clone();
                    snapshot.message = text;
                    self.set_authentication(snapshot);
                }
                AuthenticationEvent::Authenticated { .. } => {
                    if self.lifecycle.authenticated() == LifecycleAction::UnlockAndSync {
                        self.unlock_and_sync();
                    }
                }
                AuthenticationEvent::Rejected { .. } => {
                    self.set_authentication(AuthenticationSnapshot {
                        available: true,
                        state: AuthenticationState::Rejected,
                        prompt: String::new(),
                        message: "authentication failed".into(),
                    });
                }
                AuthenticationEvent::Unavailable { .. } => {
                    self.set_authentication(AuthenticationSnapshot {
                        available: true,
                        state: AuthenticationState::Rejected,
                        prompt: String::new(),
                        message: "authentication helper failed".into(),
                    });
                }
            }
        }
        if self.unlock_requested && !self.authentication.is_active() {
            self.unlock_and_sync();
        }
    }

    fn invalidate_all(&mut self) {
        for surface in &mut self.surfaces {
            surface.dirty = true;
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if self.render_failed {
            return;
        }
        if let Err(error) = self.draw_inner(qh) {
            self.fail(error);
        }
    }

    fn draw_inner(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.render.check_error()?;
        let now = Instant::now();
        let ready_outputs: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| {
                surface.configured
                    && surface.dirty
                    && !surface.waiting
                    && surface.retry_at.is_none_or(|deadline| deadline <= now)
            })
            .map(|surface| surface.output.clone())
            .collect();
        for output in ready_outputs {
            if self
                .configurations
                .advance_checked(&output, now, validate_lock_surfaces)?
                && let Some(surface) = self
                    .surfaces
                    .iter_mut()
                    .find(|surface| surface.output == output)
            {
                surface.dirty = true;
            }
        }
        let configurations = &self.configurations;
        for surface in &mut self.surfaces {
            if !surface.configured || !surface.dirty || surface.waiting {
                continue;
            }
            if surface.retry_at.is_some_and(|deadline| deadline > now) {
                continue;
            }
            surface.retry_at = None;
            let Some(root) = configurations.surface_root(&surface.output, &SurfaceKey::Default)
            else {
                continue;
            };
            let physical = (
                surface
                    .scale
                    .physical_size(surface.size.0)
                    .ok_or("invalid lock surface width")?,
                surface
                    .scale
                    .physical_size(surface.size.1)
                    .ok_or("invalid lock surface height")?,
            );
            if surface.window.is_none() {
                surface.window = Some(self.render.create_window(
                    surface.role.wl_surface(),
                    physical.0,
                    physical.1,
                )?);
            }
            let window = surface
                .window
                .as_mut()
                .ok_or("lock render surface was not initialized")?;
            let mut scroll = surface.scroll.clone();
            let layout = self.render.layout(
                root,
                Size {
                    width: surface.size.0 as f32,
                    height: surface.size.1 as f32,
                },
                &mut scroll,
            )?;
            let wl = surface.role.wl_surface().clone();
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
                    text_input: None,
                    secret_input: None,
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
                }
                #[cfg(feature = "vulkan")]
                FrameResult::RetryAfter(delay) => {
                    surface.waiting = false;
                    surface.dirty = true;
                    surface.retry_at = Some(Instant::now() + delay);
                }
            }
        }
        Ok(())
    }

    fn fail(&mut self, error: String) {
        eprintln!("thing: lock host failed: {error}");
        match self.lifecycle.failed() {
            LifecycleAction::DestroyAndExit => {
                self.result = Err(error);
                self.session_lock = None;
                self.exit = true;
            }
            LifecycleAction::None => {
                self.render_failed = true;
                let unavailable = AuthenticationSnapshot::default();
                let _ = self.configurations.set_authentication(unavailable.clone());
                self.authentication_snapshot = unavailable;
                if self.authentication.is_active() {
                    self.authentication.cancel(Instant::now());
                }
            }
            LifecycleAction::UnlockAndSync => unreachable!(),
        }
    }

    fn unlock_and_sync(&mut self) {
        self.unlock_requested = true;
        if self.authentication.is_active() {
            self.authentication.cancel(Instant::now());
            return;
        }
        if self.unlock_sync.is_some() {
            return;
        }
        self.render.prepare_drop();
        if let Some(lock) = self.session_lock.take() {
            lock.unlock();
        }
        self.surfaces.clear();
        self.unlock_sync = Some(self.connection.display().sync(&self.queue, ()));
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.lifecycle.locked();
        self.invalidate_all();
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        match self.lifecycle.compositor_finished() {
            LifecycleAction::DestroyAndExit => {
                self.session_lock = None;
                self.result = Err("the compositor declined the session lock".into());
                self.exit = true;
            }
            LifecycleAction::UnlockAndSync => self.unlock_and_sync(),
            LifecycleAction::None => {}
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        role: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        if configure.new_size.0 == 0 || configure.new_size.1 == 0 {
            self.fail("the compositor configured an empty lock surface".into());
            return;
        }
        let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|surface| surface.role.wl_surface() == role.wl_surface())
        else {
            return;
        };
        surface.size = configure.new_size;
        surface.configured = true;
        surface.dirty = true;
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|candidate| candidate.role.wl_surface() == surface)
        {
            surface.waiting = false;
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
        if !self.loading_outputs {
            self.create_surface(output, qh);
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
        self.create_surface(output.clone(), qh);
        let Some(context) = self.output_context(&output) else {
            return;
        };
        let result = if self.configurations.contains(&output) {
            self.configurations
                .set_output_checked(&output, context, validate_lock_surfaces)
        } else {
            self.configurations
                .add_checked(output.clone(), context, |runtime| {
                    validate_lock_surfaces(runtime.surfaces())
                })
                .map(|()| true)
        };
        match result {
            Ok(true) => {
                if let Some(surface) = self
                    .surfaces
                    .iter_mut()
                    .find(|surface| surface.output == output)
                {
                    surface.dirty = true;
                }
                self.update_watch();
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("thing: cannot configure lock output: {error}");
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.remove_surface(&output);
        self.configurations.remove(&output);
        self.update_watch();
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
            && let Some(scale) = Scale::from_120ths(scale)
            && let Some(surface) = app
                .surfaces
                .iter_mut()
                .find(|surface| surface.role.wl_surface() == wl)
        {
            surface.scale = scale;
            surface.dirty = true;
            let output = surface.output.clone();
            if let Some(mut context) = app.configurations.output(&output).cloned() {
                context.scale = scale;
                if let Err(error) =
                    app.configurations
                        .set_output_checked(&output, context, validate_lock_surfaces)
                {
                    eprintln!("thing: cannot update lock output scale: {error}");
                }
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        app: &mut Self,
        callback: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if app.unlock_sync.as_ref() == Some(callback)
            && matches!(event, wl_callback::Event::Done { .. })
            && app.lifecycle.sync_done()
        {
            app.unlock_sync = None;
            app.exit = true;
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_session_lock!(App);
delegate_registry!(App);
delegate_noop!(App: ignore WpFractionalScaleManagerV1);
delegate_noop!(App: ignore WpViewporter);
delegate_noop!(App: ignore WpViewport);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState];
}

fn validate_lock_surfaces(surfaces: &[SurfaceTree]) -> Result<(), String> {
    let [surface] = surfaces else {
        return Err("a lock configuration must provide exactly one surface".into());
    };
    if surface.key != SurfaceKey::Default || surface.root.kind != Kind::Lock {
        return Err("a lock configuration must provide one ui.lock root".into());
    }
    if count_kind(&surface.root, Kind::SecretInput) != 1 {
        return Err("a lock configuration must contain exactly one secret_input".into());
    }
    Ok(())
}

fn count_kind(node: &Node, kind: Kind) -> usize {
    usize::from(node.kind == kind)
        + node
            .children
            .iter()
            .map(|child| count_kind(child, kind))
            .sum::<usize>()
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
    let (width, height) = valid_dimensions(dimensions)?;
    let (width, height) = if rotated {
        (height, width)
    } else {
        (width, height)
    };
    Some((width / scale, height / scale)).filter(|(width, height)| *width > 0 && *height > 0)
}

#[cfg(test)]
fn first_secret_input(node: &Node) -> Option<NodeId> {
    if node.kind == Kind::SecretInput {
        Some(node.id)
    } else {
        node.children.iter().find_map(first_secret_input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_validation_requires_one_host_owned_secret_field() {
        let mut root = Node::new(NodeId(1), Kind::Lock);
        assert!(
            validate_lock_surfaces(&[SurfaceTree {
                key: SurfaceKey::Default,
                namespace: "thing-lock".into(),
                root: root.clone(),
            }])
            .is_err()
        );

        root.children.push(Node::new(NodeId(2), Kind::SecretInput));
        let surfaces = [SurfaceTree {
            key: SurfaceKey::Default,
            namespace: "thing-lock".into(),
            root,
        }];
        validate_lock_surfaces(&surfaces).unwrap();
        assert_eq!(first_secret_input(&surfaces[0].root), Some(NodeId(2)));

        let second = SurfaceTree {
            key: SurfaceKey::Default,
            namespace: "thing-lock-extra".into(),
            root: surfaces[0].root.clone(),
        };
        assert!(validate_lock_surfaces(&[surfaces[0].clone(), second]).is_err());

        let mut panel = surfaces[0].clone();
        panel.root.kind = Kind::Panel;
        assert!(validate_lock_surfaces(&[panel]).is_err());

        let mut duplicate = surfaces[0].clone();
        duplicate
            .root
            .children
            .push(Node::new(NodeId(3), Kind::SecretInput));
        assert!(validate_lock_surfaces(&[duplicate]).is_err());
    }
}
