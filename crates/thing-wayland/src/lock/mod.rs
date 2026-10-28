mod model;
#[cfg(test)]
mod wire_tests;

use std::{
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
    generic::Generic,
    signals::{Signal, Signals},
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_session_lock,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{
            KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers, RepeatInfo,
        },
    },
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
use thing_ui::{Interaction, PaintFrame, ScrollState, SecretInputEditor, Size};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_callback, wl_keyboard, wl_output, wl_seat, wl_surface},
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
        AttemptId, AuthenticationBroker, Event as AuthenticationEvent, ProcessContext,
        harden_secret_process, preflight_policy,
    },
    configuration::Configurations,
    focus::{ScopedUiTarget, SurfaceLayouts},
    keyboard::{
        RepeatSession, RepeatTimerAction, TextKeyAction, repeat_action_is_current, text_key_action,
    },
    output::output_context,
    render::{FrameResult, Render, Window},
    watch::Watch,
};
use xkbcommon::xkb;

const RELOAD_DELAY: Duration = Duration::from_millis(80);
const AUTHENTICATION_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Eq, PartialEq)]
enum SecretKeyRoute {
    Apply(TextKeyAction),
    Consume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecretSubmitRoute {
    Start,
    Respond(AttemptId, u64),
}

fn secret_key_route(event: &KeyEvent, modifiers: Modifiers, direct: bool) -> SecretKeyRoute {
    text_key_action(event, modifiers, direct).map_or(SecretKeyRoute::Consume, SecretKeyRoute::Apply)
}

fn secret_submit_route(
    phase: Phase,
    prompt: Option<(AttemptId, u64)>,
    can_start: bool,
) -> Option<SecretSubmitRoute> {
    if phase != Phase::Locked {
        return None;
    }
    prompt
        .map(|(id, generation)| SecretSubmitRoute::Respond(id, generation))
        .or(can_start.then_some(SecretSubmitRoute::Start))
}

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
        pending_prompt: None,
        surfaces: Vec::new(),
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seats: SeatState::new(&globals, &qh),
        seat: None,
        keyboard: None,
        keyboard_surface: None,
        keyboard_keymap: None,
        modifiers: Modifiers::default(),
        repeat: RepeatSession::default(),
        repeat_token: None,
        loop_handle: event_loop.handle(),
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
    layouts: SurfaceLayouts,
    scroll: ScrollState,
    input: Interaction,
    focus_target: Option<ScopedUiTarget>,
    secret_focused: bool,
    secret_input: Option<ActiveSecretInput>,
    autofocus_pending: bool,
    configured: bool,
    dirty: bool,
    waiting: bool,
    retry_at: Option<Instant>,
}

struct ActiveSecretInput {
    target: ScopedUiTarget,
    editor: SecretInputEditor,
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
    pending_prompt: Option<(AttemptId, u64)>,
    surfaces: Vec<Surface>,
    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_surface: Option<wl_surface::WlSurface>,
    keyboard_keymap: Option<xkb::Keymap>,
    modifiers: Modifiers,
    repeat: RepeatSession<wl_surface::WlSurface>,
    repeat_token: Option<RegistrationToken>,
    loop_handle: LoopHandle<'static, App>,
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
            layouts: SurfaceLayouts::default(),
            scroll: ScrollState::default(),
            input: Interaction::default(),
            focus_target: None,
            secret_focused: false,
            secret_input: None,
            autofocus_pending: true,
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
        let wl = self.surfaces[index].role.wl_surface().clone();
        if self.keyboard_surface.as_ref() == Some(&wl) {
            self.set_keyboard_surface(None);
        }
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
        self.clear_key_repeat();
        for surface in &mut self.surfaces {
            surface.layouts = SurfaceLayouts::default();
            surface.scroll = ScrollState::default();
            surface.input = Interaction::default();
            surface.focus_target = None;
            surface.secret_focused = false;
            surface.secret_input = None;
            surface.autofocus_pending = true;
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
                AuthenticationEvent::Prompt {
                    id,
                    generation,
                    text,
                } => {
                    self.pending_prompt = Some((id, generation));
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
                    self.pending_prompt = None;
                    self.clear_secret_editors();
                    if self.lifecycle.authenticated() == LifecycleAction::UnlockAndSync {
                        self.unlock_and_sync();
                    }
                }
                AuthenticationEvent::Rejected { .. } => {
                    self.pending_prompt = None;
                    self.clear_secret_editors();
                    self.set_authentication(AuthenticationSnapshot {
                        available: true,
                        state: AuthenticationState::Rejected,
                        prompt: String::new(),
                        message: "authentication failed".into(),
                    });
                }
                AuthenticationEvent::Unavailable { .. } => {
                    self.pending_prompt = None;
                    self.clear_secret_editors();
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

    fn index(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|surface| surface.role.wl_surface() == wl)
    }

    fn surface_target_is_current(&self, index: usize, target: &ScopedUiTarget) -> bool {
        let Some(surface) = self.surfaces.get(index) else {
            return false;
        };
        let Some(root) = self
            .configurations
            .surface_root(&surface.output, &SurfaceKey::Default)
        else {
            return false;
        };
        self.configurations.presented_target_is_current(
            &surface.output,
            &target.consumer,
            root.id,
            &target.target,
        )
    }

    fn secret_node_and_width(&self, index: usize, target: &ScopedUiTarget) -> Option<(Node, f32)> {
        if !self.surface_target_is_current(index, target) {
            return None;
        }
        let surface = self.surfaces.get(index)?;
        let id = target.target.node();
        let placement = surface.layouts.input()?.get(id)?;
        if !placement.interactive {
            return None;
        }
        let node = self
            .configurations
            .surface_root(&surface.output, &SurfaceKey::Default)?
            .find(id)?;
        if node.kind != Kind::SecretInput {
            return None;
        }
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        Some((
            node.clone(),
            (placement.bounds.width - 2.0 * padding_x).max(0.0),
        ))
    }

    fn clear_surface_focus(&mut self, index: usize) {
        let Some(surface) = self.surfaces.get(index) else {
            return;
        };
        let owns_keyboard = self.keyboard_surface.as_ref() == Some(surface.role.wl_surface());
        if owns_keyboard {
            self.clear_key_repeat();
        }
        let surface = &mut self.surfaces[index];
        surface.focus_target = None;
        surface.secret_focused = false;
        surface.secret_input = None;
        surface.input.focus_from_pointer(None);
        surface.dirty = true;
    }

    fn focus_from_keyboard(&mut self, index: usize, target: ScopedUiTarget) -> bool {
        if !self.surface_target_is_current(index, &target) {
            return false;
        }
        let id = target.target.node();
        let focusable = self.surfaces[index]
            .layouts
            .input()
            .is_some_and(|layout| layout.focus_order().any(|candidate| candidate == id));
        if !focusable {
            return false;
        }
        let secret = self.secret_node_and_width(index, &target);
        let changed = self.surfaces[index].focus_target.as_ref() != Some(&target);
        if changed {
            self.clear_key_repeat();
            self.surfaces[index].secret_input = None;
        }
        let surface = &mut self.surfaces[index];
        surface.focus_target = Some(target.clone());
        surface.secret_focused = secret.is_some();
        surface.input.focus_from_keyboard(id);
        if let Some((node, width)) = secret {
            match surface.secret_input.as_mut() {
                Some(active) if active.target == target => active.editor.prepare(&node, width),
                _ => {
                    surface.secret_input = Some(ActiveSecretInput {
                        target,
                        editor: SecretInputEditor::new(&node, width),
                    });
                }
            }
        } else {
            surface.secret_input = None;
        }
        surface.dirty = true;
        true
    }

    fn reconcile_surface_focus(&mut self, index: usize) {
        if self.surfaces.get(index).is_none() {
            return;
        }
        let current = self.surfaces[index].focus_target.clone();
        if let Some(target) = current {
            let id = target.target.node();
            let focusable = self.surface_target_is_current(index, &target)
                && self.surfaces[index]
                    .layouts
                    .input()
                    .is_some_and(|layout| layout.focus_order().any(|candidate| candidate == id));
            if focusable {
                self.focus_from_keyboard(index, target);
                return;
            }
            self.clear_surface_focus(index);
        }

        if self.keyboard_surface.as_ref() != Some(self.surfaces[index].role.wl_surface())
            || self.surfaces[index].layouts.presented().is_none()
            || !self.surfaces[index].autofocus_pending
        {
            return;
        }
        self.surfaces[index].autofocus_pending = false;
        let output = self.surfaces[index].output.clone();
        let Some(root) = self
            .configurations
            .surface_root(&output, &SurfaceKey::Default)
        else {
            return;
        };
        let (Some(consumer), Some(target)) = (
            self.configurations.consumer(&output),
            self.configurations.autofocus_target(&output, root.id),
        ) else {
            return;
        };
        self.focus_from_keyboard(
            index,
            ScopedUiTarget {
                consumer: consumer.to_owned(),
                target,
            },
        );
    }

    fn reconcile_layout_focus(&mut self) {
        for index in 0..self.surfaces.len() {
            self.reconcile_surface_focus(index);
        }
    }

    fn clear_secret_editors(&mut self) {
        for surface in &mut self.surfaces {
            surface.secret_input = None;
            surface.dirty = true;
        }
    }

    fn set_keyboard_surface(&mut self, next: Option<wl_surface::WlSurface>) {
        self.clear_key_repeat();
        let previous = std::mem::replace(&mut self.keyboard_surface, next);
        self.modifiers = Modifiers::default();
        if let Some(previous) = previous.as_ref()
            && self.keyboard_surface.as_ref() != Some(previous)
            && let Some(index) = self.index(previous)
        {
            self.clear_surface_focus(index);
        }
        if let Some(index) = self
            .keyboard_surface
            .as_ref()
            .and_then(|surface| self.index(surface))
        {
            self.surfaces[index].autofocus_pending = true;
            self.surfaces[index].dirty = true;
            self.reconcile_surface_focus(index);
        }
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
                match self
                    .loop_handle
                    .insert_source(Timer::from_duration(delay), |_, _, app| {
                        app.dispatch_key_repeat()
                    }) {
                    Ok(token) => self.repeat_token = Some(token),
                    Err(error) => self.fail(format!("cannot start keyboard repeat: {error}")),
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

    fn keyboard_focus_owner(&self) -> Option<(wl_surface::WlSurface, Option<ScopedUiTarget>)> {
        let wl = self.keyboard_surface.clone()?;
        let target = self
            .index(&wl)
            .and_then(|index| self.surfaces[index].focus_target.clone());
        Some((wl, target))
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
        let repeats = self.key(event, false);
        let current = self.repeat.key.as_ref().is_some_and(|repeating| {
            repeating.event.raw_code == raw_code && repeating.surface == surface
        });
        if !repeats || !current {
            self.clear_key_repeat();
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToDuration(settings.interval)
        }
    }

    fn key(&mut self, event: KeyEvent, direct: bool) -> bool {
        let Some(index) = self
            .keyboard_surface
            .as_ref()
            .and_then(|surface| self.index(surface))
        else {
            return false;
        };
        self.reconcile_surface_focus(index);
        if let Some(repeats) = self.secret_input_key(index, &event, direct) {
            return repeats;
        }
        if direct && matches!(event.keysym, Keysym::Tab | Keysym::ISO_Left_Tab) {
            let order: Vec<_> = self.surfaces[index]
                .layouts
                .input()
                .map(|layout| layout.focus_order().collect())
                .unwrap_or_default();
            if !order.is_empty() {
                let current = self.surfaces[index]
                    .input
                    .focused
                    .and_then(|id| order.iter().position(|candidate| *candidate == id));
                let next = if self.modifiers.shift {
                    current.map_or(order.len() - 1, |n| (n + order.len() - 1) % order.len())
                } else {
                    current.map_or(0, |n| (n + 1) % order.len())
                };
                if let Some(target) = self.surfaces[index].layouts.presented_target(order[next]) {
                    self.focus_from_keyboard(index, target);
                }
            }
        }
        false
    }

    fn secret_input_key(&mut self, index: usize, event: &KeyEvent, direct: bool) -> Option<bool> {
        let target = self.surfaces.get(index)?.focus_target.clone()?;
        let was_secret = self.surfaces[index].secret_focused;
        let Some((node, width)) = self.secret_node_and_width(index, &target) else {
            if was_secret {
                self.clear_surface_focus(index);
                return Some(false);
            }
            return None;
        };
        if self.surfaces[index]
            .secret_input
            .as_ref()
            .is_none_or(|active| active.target != target)
        {
            self.surfaces[index].secret_input = Some(ActiveSecretInput {
                target,
                editor: SecretInputEditor::new(&node, width),
            });
        }
        self.surfaces[index].input.reveal_keyboard_focus();
        self.surfaces[index].dirty = true;
        let SecretKeyRoute::Apply(action) = secret_key_route(event, self.modifiers, direct) else {
            return Some(false);
        };
        let repeats = action.repeats();
        if action == TextKeyAction::Submit {
            self.submit_secret(index, &node, width);
            return Some(false);
        }
        let select = self.modifiers.shift;
        let word = self.modifiers.ctrl && !self.modifiers.alt;
        let editor = &mut self.surfaces[index]
            .secret_input
            .as_mut()
            .expect("the secret editor was initialized")
            .editor;
        match action {
            TextKeyAction::Insert(text) => {
                editor.replace_selection(&text, &node, width);
            }
            TextKeyAction::Backspace => {
                editor.backspace(word, &node, width);
            }
            TextKeyAction::Delete => {
                editor.delete(word, &node, width);
            }
            TextKeyAction::MoveLeft => editor.move_left(select, word, &node, width),
            TextKeyAction::MoveRight => editor.move_right(select, word, &node, width),
            TextKeyAction::MoveToStart => editor.move_to_start(select, &node, width),
            TextKeyAction::MoveToEnd => editor.move_to_end(select, &node, width),
            TextKeyAction::SelectAll => editor.select_all(&node, width),
            TextKeyAction::Submit => unreachable!(),
        }
        Some(repeats)
    }

    fn submit_secret(&mut self, index: usize, node: &Node, width: f32) {
        let now = Instant::now();
        let Some(route) = secret_submit_route(
            self.lifecycle.phase(),
            self.pending_prompt,
            self.authentication.can_start(now),
        ) else {
            return;
        };
        let Some(active) = self.surfaces[index].secret_input.as_mut() else {
            return;
        };
        let secret = active.editor.take(node, width);
        self.clear_secret_editors();
        let result = match route {
            SecretSubmitRoute::Respond(id, generation) => {
                self.pending_prompt = None;
                self.authentication
                    .respond(id, generation, secret, now)
                    .map(|()| id)
                    .map_err(|secret| {
                        drop(secret);
                        "authentication prompt is no longer current".to_owned()
                    })
            }
            SecretSubmitRoute::Start => self.authentication.start(secret, now).map_err(|error| {
                let message = error.error().to_string();
                drop(error.into_secret());
                message
            }),
        };
        match result {
            Ok(_) => self.set_authentication(AuthenticationSnapshot {
                available: true,
                state: AuthenticationState::Authenticating,
                prompt: String::new(),
                message: String::new(),
            }),
            Err(error) => {
                eprintln!("thing: cannot start authentication helper: {error}");
                self.set_authentication(AuthenticationSnapshot {
                    available: true,
                    state: AuthenticationState::Rejected,
                    prompt: String::new(),
                    message: "authentication helper failed".into(),
                });
            }
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
        self.reconcile_layout_focus();
        let configurations = &self.configurations;
        let keyboard_surface = self.keyboard_surface.clone();
        let mut presented = Vec::new();
        let mut repeat_invalidated = false;
        for (index, surface) in self.surfaces.iter_mut().enumerate() {
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
            if let Some(id) = surface.input.focused
                && !layout.focus_order().any(|candidate| candidate == id)
            {
                repeat_invalidated |= keyboard_surface.as_ref() == Some(surface.role.wl_surface());
                surface.focus_target = None;
                surface.secret_focused = false;
                surface.secret_input = None;
                surface.input.focus_from_pointer(None);
            }
            let presented_ui = configurations
                .presented_targets(&surface.output, root, &layout)
                .ok_or("rendered lock output has no configuration consumer")?;
            let wl = surface.role.wl_surface().clone();
            let viewport = surface.viewport.clone();
            let logical = surface.size;
            let buffer_scale = surface.scale.factor().round() as i32;
            let secret_input = (keyboard_surface.as_ref() == Some(surface.role.wl_surface())
                && surface.secret_input.as_ref().is_some_and(|active| {
                    Some(active.target.target.node()) == surface.input.focused
                        && layout
                            .get(active.target.target.node())
                            .is_some_and(|placement| placement.interactive)
                }))
            .then(|| {
                let active = surface
                    .secret_input
                    .as_mut()
                    .expect("the active secret input was checked");
                let id = active.target.target.node();
                let placement = layout
                    .get(id)
                    .expect("the active secret input placement was checked");
                let node = root
                    .find(id)
                    .expect("the active secret input belongs to the rendered tree");
                let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
                let width = (placement.bounds.width - 2.0 * padding_x).max(0.0);
                active.editor.prepare(node, width);
                &mut active.editor
            });
            let mut input = surface.input;
            if keyboard_surface.as_ref() != Some(surface.role.wl_surface()) {
                input.hide_keyboard_focus();
            }
            let result = self.render.draw(
                window,
                PaintFrame {
                    root,
                    layout: &layout,
                    physical,
                    scale: surface.scale.factor(),
                    input,
                    text_input: None,
                    secret_input,
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
                    let (consumer, targets) = presented_ui;
                    surface.layouts.publish(layout, root.id, consumer, targets);
                    surface.dirty = redraw || animating;
                    presented.push(index);
                }
                #[cfg(feature = "vulkan")]
                FrameResult::RetryAfter(delay) => {
                    surface.waiting = false;
                    surface.dirty = true;
                    surface.layouts.stage_input(layout);
                    surface.retry_at = Some(Instant::now() + delay);
                }
            }
        }
        if repeat_invalidated {
            self.clear_key_repeat();
        }
        for index in presented {
            self.reconcile_surface_focus(index);
        }
        Ok(())
    }

    fn fail(&mut self, error: String) {
        eprintln!("thing: lock host failed: {error}");
        self.pending_prompt = None;
        self.set_keyboard_surface(None);
        self.clear_secret_editors();
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
        self.pending_prompt = None;
        self.set_keyboard_surface(None);
        self.clear_secret_editors();
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
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.clear_key_repeat();
            self.repeat.settings = None;
            self.keyboard_keymap = None;
            match self.seats.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                Err(error) => self.fail(format!("cannot acquire lock keyboard: {error}")),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if self.seat.as_ref() != Some(&seat) || capability != Capability::Keyboard {
            return;
        }
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.release();
        }
        self.repeat.settings = None;
        self.keyboard_keymap = None;
        self.set_keyboard_surface(None);
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        self.seat = None;
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.release();
        }
        self.repeat.settings = None;
        self.keyboard_keymap = None;
        self.set_keyboard_surface(None);
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
            self.set_keyboard_surface(None);
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let owner = self.keyboard_focus_owner();
        let key_repeats = self.physical_key_repeats(event.raw_code);
        let action_repeats = self.key(event.clone(), true);
        self.finish_key_press(
            event,
            key_repeats,
            repeat_action_is_current(action_repeats, &owner, &self.keyboard_focus_owner()),
        );
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.keyboard_surface.as_ref().is_some_and(|surface| {
            self.repeat
                .accepts_compositor_repeat(event.raw_code, surface)
        }) && !self.key(event, false)
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
            self.fail("cannot retain the lock keyboard keymap".into());
        }
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
delegate_keyboard!(App);
delegate_output!(App);
delegate_session_lock!(App);
delegate_seat!(App);
delegate_registry!(App);
delegate_noop!(App: ignore WpFractionalScaleManagerV1);
delegate_noop!(App: ignore WpViewporter);
delegate_noop!(App: ignore WpViewport);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState, SeatState];
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

    fn key(keysym: Keysym, utf8: Option<&str>) -> KeyEvent {
        KeyEvent {
            time: 1,
            raw_code: 2,
            keysym,
            utf8: utf8.map(str::to_owned),
        }
    }

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

    #[test]
    fn secret_focus_consumes_every_key_before_public_routing() {
        assert_eq!(
            secret_key_route(&key(Keysym::p, Some("p")), Modifiers::default(), true),
            SecretKeyRoute::Apply(TextKeyAction::Insert("p".into()))
        );
        assert_eq!(
            secret_key_route(&key(Keysym::Return, None), Modifiers::default(), true),
            SecretKeyRoute::Apply(TextKeyAction::Submit)
        );
        assert_eq!(
            secret_key_route(&key(Keysym::Return, None), Modifiers::default(), false),
            SecretKeyRoute::Consume
        );
        assert_eq!(
            secret_key_route(
                &key(Keysym::c, Some("c")),
                Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
                true,
            ),
            SecretKeyRoute::Consume
        );
        assert_eq!(
            secret_key_route(&key(Keysym::Tab, None), Modifiers::default(), true),
            SecretKeyRoute::Consume
        );
    }

    #[test]
    fn secret_submission_is_locked_generation_and_prompt_scoped() {
        let id = AttemptId {
            lock: 4,
            attempt: 7,
        };
        assert_eq!(secret_submit_route(Phase::Requesting, None, true), None);
        assert_eq!(
            secret_submit_route(Phase::Locked, None, true),
            Some(SecretSubmitRoute::Start)
        );
        assert_eq!(secret_submit_route(Phase::Locked, None, false), None);
        assert_eq!(
            secret_submit_route(Phase::Locked, Some((id, 3)), false),
            Some(SecretSubmitRoute::Respond(id, 3))
        );
        assert_eq!(secret_submit_route(Phase::FailedLocked, None, true), None);
    }
}
