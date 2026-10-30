use std::{
    cell::RefCell,
    io,
    path::Path,
    rc::Rc,
    sync::Arc,
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
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryHandler, RegistryState},
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
    Action, Event, KeyInput, ManagedProcessAction, ManagedProcessDefinition,
    ManagedProcessSnapshot, OutputContext, PointerButton as RuntimePointerButton,
    PointerEvent as RuntimePointerEvent, PointerPhase, Scalar, ScrollInput as RuntimeScrollInput,
    ScrollSource as RuntimeScrollSource, ServiceSnapshot, SurfaceKey, SurfaceTree,
    SystemTrayAction, SystemTrayMenuAction, UiAction,
};
use thing_ui::{
    Interaction, Layout, PaintFrame, ScrollState, ScrollStep, SemanticPlacement, Size,
    TextInputCheckpoint, TextInputEditor,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
    globals::registry_queue_init,
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::WlDataDeviceManager,
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
        wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface,
    },
};
use wayland_protocols::{
    ext::data_control::v1::client::{
        ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
        ext_data_control_source_v1::{self, ExtDataControlSourceV1},
    },
    ext::idle_notify::v1::client::{
        ext_idle_notification_v1::{self, ExtIdleNotificationV1},
        ext_idle_notifier_v1::ExtIdleNotifierV1,
    },
    wp::idle_inhibit::zv1::client::{
        zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
        zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
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

#[cfg(test)]
use crate::output::{fallback_logical_size, valid_dimensions};

#[cfg(test)]
mod idle_sleep_tests;
#[cfg(test)]
mod launch_tests;
#[cfg(test)]
mod wheel_tests;

#[cfg(test)]
delegate_noop!(App: ignore wayland_client::protocol::wl_shm::WlShm);
#[cfg(test)]
delegate_noop!(App: ignore wayland_client::protocol::wl_shm_pool::WlShmPool);
#[cfg(test)]
delegate_noop!(App: ignore wayland_client::protocol::wl_buffer::WlBuffer);

use crate::{
    Renderer, RunOptions, SystemTray,
    applications::{self, ActivationCall, Activator, Applications, Catalog, Plan},
    audio::Audio,
    clipboard::{Clipboard, OfferData as ClipboardOfferData, SourceData as ClipboardSourceData},
    configuration::{
        Configurations, DispatchOutcome, PointerHandlerOutcome, RemovedConfiguration,
        ScrollHandlerOutcome,
    },
    focus::{LayoutSnapshot, ScopedUiTarget, SurfaceLayouts},
    focused_clipboard::{FocusedClipboard, PasteId},
    hover::{Command as HoverCommand, HoverKey, HoverStates, HoverTarget},
    idle_inhibit::{self, Inhibitor, State as IdleInhibitState},
    idle_notify::{
        Desired as IdleWatchDesired, Notification as IdleNotification, State as IdleNotifyState,
        Transition as IdleTransition,
    },
    input_method::{
        ChangeCause as InputMethodChangeCause, CursorRectangle as InputMethodCursorRectangle,
        Done as InputMethodDone, InputMethod, TextState as InputMethodTextState,
    },
    keyboard::{
        RepeatSession, RepeatTimerAction, RootKeyDispatch, TextKeyAction, repeat_action_is_current,
        root_key_input, text_key_action,
    },
    launch::{self, LaunchRequest, Launcher, PendingLaunches},
    managed_process::{
        Definition as ManagedProcessDefinitionInternal, Manager as ProcessManager,
        PreparedScope as PreparedManagedProcessSupervisorScope,
        ScopeActivation as ManagedProcessSupervisorActivation,
        initial_scope_snapshot as initial_managed_process_snapshot,
    },
    media::Media,
    niri::Niri,
    notifications::Server as NotificationServer,
    output::output_context,
    panel::{self, Geometry, ResolvedInputRegion},
    popup::{self, Position as PopupPosition},
    power::Power,
    render::{FrameResult, Render, Window},
    sleep::{SleepInhibition, Transport as SleepTransport},
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
    let registry = RegistryState::new(&globals);
    let mut idle_inhibit = IdleInhibitState::default();
    idle_inhibit.bind_next(&registry, &qh)?;
    services
        .set_service(
            "idle.inhibit_available",
            Scalar::Boolean(idle_inhibit.active().is_some()),
        )
        .map_err(|error| error.to_string())?;
    let mut idle_notify = IdleNotifyState::default();
    // idle watches are optional: without the protocol they stay unarmed.
    if let Err(error) = idle_notify.bind_next(&registry, &qh) {
        eprintln!("thing: {error}");
    }
    services
        .set_service(
            "idle.watch_available",
            Scalar::Boolean(idle_notify.available()),
        )
        .map_err(|error| error.to_string())?;
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
    let (applications_ping, applications_source) =
        calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let applications = Applications::start(applications_ping)
        .map_err(|e| format!("cannot start applications worker: {e}"))?;
    let (activator_ping, activator_source) =
        calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let activator = Activator::start(activator_ping)
        .map_err(|e| format!("cannot start application activation worker: {e}"))?;
    let (power_ping, power_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let power = Power::start(power_ping).map_err(|e| format!("cannot start power worker: {e}"))?;
    let (sleep_ping, sleep_source) = calloop::ping::make_ping().map_err(|e| e.to_string())?;
    // the worker starts with the first started inhibitor.
    let sleep = SleepInhibition::new(SleepTransport::System, sleep_ping);
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
    let (focused_clipboard_ping, focused_clipboard_source) =
        calloop::ping::make_ping().map_err(|e| e.to_string())?;
    let focused_clipboard = FocusedClipboard::bind(&globals, &qh, focused_clipboard_ping)
        .map_err(|error| format!("cannot start text field clipboard support: {error}"))?;
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
        applications,
        application_catalog: Arc::default(),
        activator,
        power,
        sleep,
        niri,
        notifications,
        system_tray,
        clipboard,
        focused_clipboard,
        pending_paste: None,
        input_method,
        launcher,
        managed_processes: None,
        pending_launches: PendingLaunches::new(),
        pending_ui_actions: Vec::new(),
        surfaces: Vec::new(),
        closed_surfaces: Vec::new(),
        registry,
        idle_inhibit,
        idle_notify,
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
        pointer_capture: None,
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
    app.initialize(&qh)?;
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
        .insert_source(applications_source, |_, _, app| app.update_applications())
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(activator_source, |_, _, app| {
            app.start_activation_fallbacks()
        })
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(power_source, |_, _, app| app.update_power())
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(sleep_source, |_, _, app| app.update_sleep())
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
    event_loop
        .handle()
        .insert_source(focused_clipboard_source, |_, _, app| {
            app.update_focused_clipboard()
        })
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
        app.turn(&mut event_loop, &connection, &qh)?;
    }
    Ok(())
}

impl App {
    /// loads the configuration for the outputs known after the first
    /// roundtrip and creates their surfaces.
    fn initialize(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.update_clipboard();
        let outputs: Vec<_> = self
            .outputs
            .outputs()
            .filter_map(|output| {
                self.output_context(&output)
                    .map(|context| (output, context))
            })
            .collect();
        let layer_shell_version = self.layer_shell_version;
        let entries = outputs
            .iter()
            .map(|(output, context)| self.layout_validation_entry(output.clone(), context.clone()))
            .collect();
        let loop_handle = self.loop_handle.clone();
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            entries,
        );
        let mut prepared_configurations = self
            .configurations
            .prepare_reload_for_checked(outputs.clone(), |output, runtime| {
                preflight.validate(output, runtime.surfaces())
            })?;
        let process_definitions = prepared_configurations
            .managed_process_definitions()
            .to_vec();
        let projected_processes = project_managed_processes(
            self.managed_processes.as_ref(),
            &process_definitions,
            ManagedProcessScopeChange::Replace,
        )?;
        prepared_configurations
            .set_managed_processes(projected_processes.clone(), |output, runtime| {
                preflight.validate(output, runtime.surfaces())
            })?;
        let activation = prepare_and_commit_managed_processes(
            &mut self.managed_processes,
            &loop_handle,
            &process_definitions,
            ManagedProcessScopeChange::Replace,
            projected_processes,
            |snapshot| {
                prepared_configurations.set_managed_processes(snapshot, |output, runtime| {
                    preflight.validate(output, runtime.surfaces())
                })
            },
        )?;
        let prepared = preflight.into_prepared();
        self.configurations.commit_reload(prepared_configurations);
        for (output, _) in &outputs {
            self.sync_output_surfaces(output, false, qh)?;
        }
        self.install_prepared_layouts(prepared);
        arm_managed_processes(self.managed_processes.as_ref(), activation);
        self.reconcile_sleep();
        self.reconcile_idle_watches();
        self.loading_outputs = false;
        self.update_watch();
        Ok(())
    }

    /// runs one iteration of the event loop: due work, drawing, and then
    /// waiting for the next event or deadline.
    fn turn(
        &mut self,
        event_loop: &mut EventLoop<'static, App>,
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        self.expire_launches();
        if self
            .reload_at
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            self.reload_at = None;
            self.reload(qh)?;
        }
        self.dispatch_due_timers(Instant::now());
        if self
            .sleep
            .next_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            self.update_sleep();
        }
        self.draw(qh)?;
        self.process_hover(qh);
        connection.flush().map_err(|error| error.to_string())?;
        let timeout = self
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        event_loop
            .dispatch(timeout, self)
            .map_err(|error| error.to_string())?;
        match self.error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Copy)]
enum ManagedProcessScopeChange {
    Replace,
    Reconcile,
}

enum PreparedManagedProcessScope {
    Empty,
    Scope(PreparedManagedProcessSupervisorScope),
}

impl PreparedManagedProcessScope {
    fn runtime(&self) -> ManagedProcessSnapshot {
        match self {
            Self::Empty => ManagedProcessSnapshot::default(),
            Self::Scope(scope) => scope.runtime(),
        }
    }
}

enum ManagedProcessScopeActivation {
    Empty,
    Scope(ManagedProcessSupervisorActivation),
}

const MAX_MANAGED_PROCESS_PREFLIGHT_ATTEMPTS: usize = 8;

fn project_managed_processes(
    manager: Option<&ProcessManager>,
    definitions: &[ManagedProcessDefinition],
    change: ManagedProcessScopeChange,
) -> Result<ManagedProcessSnapshot, String> {
    let definitions = definitions
        .iter()
        .map(ManagedProcessDefinitionInternal::from)
        .collect();
    let snapshot = match (manager, change) {
        (Some(manager), ManagedProcessScopeChange::Replace) => {
            manager.project_replace_scope(definitions)
        }
        (Some(manager), ManagedProcessScopeChange::Reconcile) => {
            manager.project_reconcile_scope(definitions)
        }
        (None, _) => initial_managed_process_snapshot(definitions),
    }
    .map_err(|error| format!("cannot configure managed processes: {error}"))?;
    Ok(snapshot)
}

fn prepare_managed_processes(
    manager: &mut Option<ProcessManager>,
    loop_handle: &LoopHandle<'static, App>,
    definitions: &[ManagedProcessDefinition],
    change: ManagedProcessScopeChange,
) -> Result<PreparedManagedProcessScope, String> {
    if manager.is_none() && definitions.is_empty() {
        return Ok(PreparedManagedProcessScope::Empty);
    }
    let definitions = definitions
        .iter()
        .map(ManagedProcessDefinitionInternal::from)
        .collect();
    if manager.is_none() {
        let (ping, source) = calloop::ping::make_ping().map_err(|error| error.to_string())?;
        let candidate = ProcessManager::start(ping)
            .map_err(|error| format!("cannot start managed process service: {error}"))?;
        loop_handle
            .insert_source(source, |_, _, app| app.update_managed_processes())
            .map_err(|error| format!("cannot monitor managed process service: {error}"))?;
        *manager = Some(candidate);
    }
    let manager = manager
        .as_ref()
        .expect("the managed process manager was initialized above");
    let prepared = match change {
        ManagedProcessScopeChange::Replace => manager.prepare_replace_scope(definitions),
        ManagedProcessScopeChange::Reconcile => manager.prepare_reconcile_scope(definitions),
    }
    .map_err(|error| format!("cannot configure managed processes: {error}"))?;
    Ok(PreparedManagedProcessScope::Scope(prepared))
}

fn commit_managed_processes(
    manager: Option<&ProcessManager>,
    prepared: PreparedManagedProcessScope,
) -> io::Result<ManagedProcessScopeActivation> {
    match prepared {
        PreparedManagedProcessScope::Empty => Ok(ManagedProcessScopeActivation::Empty),
        PreparedManagedProcessScope::Scope(prepared) => manager
            .expect("a prepared managed process scope has a manager")
            .commit_scope(prepared)
            .map(ManagedProcessScopeActivation::Scope),
    }
}

fn prepare_and_commit_managed_processes(
    manager: &mut Option<ProcessManager>,
    loop_handle: &LoopHandle<'static, App>,
    definitions: &[ManagedProcessDefinition],
    change: ManagedProcessScopeChange,
    mut projected: ManagedProcessSnapshot,
    mut apply_snapshot: impl FnMut(ManagedProcessSnapshot) -> Result<(), String>,
) -> Result<ManagedProcessScopeActivation, String> {
    for _ in 0..MAX_MANAGED_PROCESS_PREFLIGHT_ATTEMPTS {
        let prepared = prepare_managed_processes(manager, loop_handle, definitions, change)?;
        let current = prepared.runtime();
        if current != projected {
            apply_snapshot(current.clone())?;
            projected = current;
            continue;
        }
        match commit_managed_processes(manager.as_ref(), prepared) {
            Ok(activation) => return Ok(activation),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(format!("cannot configure managed processes: {error}")),
        }
    }
    Err("cannot configure managed processes: process state kept changing during preflight".into())
}

fn arm_managed_processes(
    manager: Option<&ProcessManager>,
    activation: ManagedProcessScopeActivation,
) {
    if let ManagedProcessScopeActivation::Scope(activation) = activation {
        manager
            .expect("a managed process activation has a manager")
            .arm_scope(activation);
    }
}

fn configure_managed_processes(
    manager: &mut Option<ProcessManager>,
    loop_handle: &LoopHandle<'static, App>,
    definitions: &[ManagedProcessDefinition],
    change: ManagedProcessScopeChange,
) -> Result<(), String> {
    let projected = project_managed_processes(manager.as_ref(), definitions, change)?;
    let activation = prepare_and_commit_managed_processes(
        manager,
        loop_handle,
        definitions,
        change,
        projected,
        |_| Ok(()),
    )?;
    arm_managed_processes(manager.as_ref(), activation);
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

struct PreparedSurfaceLayout {
    key: SurfaceKey,
    root: Node,
    size: (u32, u32),
    scale: Scale,
    layout: Layout,
    scroll: ScrollState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutSurfaceRole {
    Panel,
    Popup,
}

struct LayoutSurfaceState {
    key: SurfaceKey,
    root: NodeId,
    role: LayoutSurfaceRole,
    size: (u32, u32),
    preferred_scale: Scale,
    scroll: ScrollState,
    focused: Option<NodeId>,
}

struct LayoutValidationEntry {
    output: wl_output::WlOutput,
    context: OutputContext,
    surfaces: Vec<LayoutSurfaceState>,
}

struct PreparedOutputLayouts {
    output: wl_output::WlOutput,
    surfaces: Vec<PreparedSurfaceLayout>,
}

// Runtime validates each distinct animation target first and the displayed
// candidate last. This side channel therefore retains the displayed plans, but
// callers install them only after the complete checked operation succeeds.
struct LayoutPreflight<'a> {
    render: RefCell<&'a mut Render>,
    layer_shell_version: u32,
    idle_inhibit_available: bool,
    entries: Vec<LayoutValidationEntry>,
    prepared: RefCell<Vec<PreparedOutputLayouts>>,
}

impl LayoutPreflight<'_> {
    fn validate(
        &self,
        output: &wl_output::WlOutput,
        surfaces: &[SurfaceTree],
    ) -> Result<(), String> {
        validate_surface_capabilities(
            surfaces,
            self.layer_shell_version,
            self.idle_inhibit_available,
        )?;
        let entry = self
            .entries
            .iter()
            .find(|entry| &entry.output == output)
            .ok_or("layout preflight has no output context")?;
        let prepared = prepare_candidate_layouts(
            &mut self.render.borrow_mut(),
            surfaces,
            &entry.context,
            &entry.surfaces,
        )?;
        let mut outputs = self.prepared.borrow_mut();
        if let Some(previous) = outputs
            .iter_mut()
            .find(|previous| previous.output == *output)
        {
            previous.surfaces = prepared;
        } else {
            outputs.push(PreparedOutputLayouts {
                output: output.clone(),
                surfaces: prepared,
            });
        }
        Ok(())
    }

    fn into_prepared(self) -> Vec<PreparedOutputLayouts> {
        self.prepared.into_inner()
    }
}

impl<'a> LayoutPreflight<'a> {
    fn new(
        render: &'a mut Render,
        layer_shell_version: u32,
        idle_inhibit_available: bool,
        entries: Vec<LayoutValidationEntry>,
    ) -> Self {
        Self {
            render: RefCell::new(render),
            layer_shell_version,
            idle_inhibit_available,
            entries,
            prepared: RefCell::new(Vec::new()),
        }
    }
}

#[derive(Clone, Copy)]
struct PopupAnchorLayout {
    node: Option<NodeId>,
    snapshot: LayoutSnapshot,
}

struct InputOrigin {
    seat: wl_seat::WlSeat,
    serial: u32,
    surface: wl_surface::WlSurface,
}

struct PendingPaste {
    id: PasteId,
    surface: wl_surface::WlSurface,
    target: ScopedUiTarget,
    generation: u64,
    revision: u64,
}

#[derive(Clone)]
enum HostUiAction {
    Focus(ScopedUiTarget),
    Reveal(ScopedUiTarget),
}

impl HostUiAction {
    fn target(&self) -> &ScopedUiTarget {
        match self {
            Self::Focus(target) | Self::Reveal(target) => target,
        }
    }

    fn is_focus(&self) -> bool {
        matches!(self, Self::Focus(_))
    }
}

struct PendingUiAction {
    output: wl_output::WlOutput,
    action: HostUiAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MissingSurfacePolicy {
    WaitForSameCycle,
    Expire,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceActionRoute {
    Assign(usize),
    Retain,
    Drop,
}

fn surface_action_route(
    surface: Option<usize>,
    missing: MissingSurfacePolicy,
) -> SurfaceActionRoute {
    match (surface, missing) {
        (Some(index), _) => SurfaceActionRoute::Assign(index),
        (None, MissingSurfacePolicy::WaitForSameCycle) => SurfaceActionRoute::Retain,
        (None, MissingSurfacePolicy::Expire) => SurfaceActionRoute::Drop,
    }
}

fn retain_non_focus_actions<T>(actions: &mut Vec<T>, mut is_focus: impl FnMut(&T) -> bool) {
    actions.retain(|action| !is_focus(action));
}

const MAX_PENDING_UI_ACTIONS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WidgetKeyAction {
    Activate,
    Slider(SliderKeyAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextClipboardAction {
    Copy,
    Cut,
    Paste,
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
    idle_inhibitor: Option<Inhibitor>,
    output: wl_output::WlOutput,
    key: SurfaceKey,
    namespace: String,
    root: NodeId,
    size: (u32, u32),
    panel: Option<PanelState>,
    scale: Scale,
    preferred_scale: Scale,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    layouts: SurfaceLayouts,
    prepared: Option<PreparedSurfaceLayout>,
    scroll: ScrollState,
    input: Interaction,
    focus_target: Option<ScopedUiTarget>,
    pending_focus: Option<ScopedUiTarget>,
    ui_actions: Vec<HostUiAction>,
    autofocus_pending: bool,
    text_input: Option<ActiveTextInput>,
    pointer_position: Option<Point>,
    hover_path: Vec<ScopedUiTarget>,
    press_serial: Option<u32>,
    pressed_button: Option<u32>,
    pressed_target: Option<ScopedUiTarget>,
    range_press: Option<ScopedUiTarget>,
    configured: bool,
    dirty: bool,
    waiting: bool,
    retry_at: Option<Instant>,
}

struct ActiveTextInput {
    id: NodeId,
    generation: u64,
    editor: TextInputEditor,
    cursor_available: bool,
}

#[derive(Clone)]
struct CustomPointerCapture {
    surface: wl_surface::WlSurface,
    output: wl_output::WlOutput,
    target: ScopedUiTarget,
    button: u32,
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

    fn take_prepared_layout(&mut self, root: &Node) -> Option<PreparedSurfaceLayout> {
        self.prepared.take().filter(|prepared| {
            prepared.key == self.key && prepared.size == self.size && prepared.root == *root
        })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.idle_inhibitor.take();
        if let Some(scale) = self.fractional.take() {
            scale.destroy();
        }
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
    }
}

/// work waiting for an activation token.
struct PendingLaunch {
    work: PendingWork,
    /// the entry id and revision an application launch was planned from.
    entry: Option<(String, u64)>,
}

enum PendingWork {
    Spawn(LaunchRequest),
    Activate(Box<ActivationCall>),
}

impl PendingLaunch {
    fn spawn(request: LaunchRequest) -> Self {
        Self {
            work: PendingWork::Spawn(request),
            entry: None,
        }
    }
}

/// one stop on a captured wheel route, bound to its presented identity.
enum ScrollRouteStep {
    Handler(ScopedUiTarget, RuntimeScrollInput),
    Viewport(ScopedUiTarget),
}

pub(crate) struct App {
    render: Render,
    image_ping: calloop::ping::Ping,
    configurations: Configurations<wl_output::WlOutput>,
    audio: Audio,
    media: Media,
    applications: Applications,
    /// launch data for the application list the configurations currently see.
    application_catalog: Arc<Catalog>,
    activator: Activator,
    power: Power,
    sleep: SleepInhibition,
    niri: Niri,
    notifications: Option<NotificationServer>,
    system_tray: Option<SystemTray>,
    clipboard: Option<Clipboard>,
    focused_clipboard: FocusedClipboard,
    pending_paste: Option<PendingPaste>,
    input_method: InputMethod,
    launcher: Launcher,
    managed_processes: Option<ProcessManager>,
    pending_launches: PendingLaunches<XdgActivationTokenV1, PendingLaunch>,
    pending_ui_actions: Vec<PendingUiAction>,
    watch: Rc<Watch>,
    reload_at: Option<Instant>,
    hover: HoverStates<SurfaceOwner>,
    surfaces: Vec<Surface>,
    closed_surfaces: Vec<SurfaceOwner>,
    registry: RegistryState,
    idle_inhibit: IdleInhibitState,
    idle_notify: IdleNotifyState,
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
    pointer_capture: Option<CustomPointerCapture>,
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
        self.idle_inhibit.take_active();
        self.idle_notify.clear();
    }
}

impl App {
    fn layout_validation_entry(
        &self,
        output: wl_output::WlOutput,
        context: OutputContext,
    ) -> LayoutValidationEntry {
        let surfaces = self
            .surfaces
            .iter()
            .filter(|surface| surface.output == output)
            .map(|surface| LayoutSurfaceState {
                key: surface.key.clone(),
                root: surface.root,
                role: match surface.role {
                    Role::Panel(_) => LayoutSurfaceRole::Panel,
                    Role::Popup(_) => LayoutSurfaceRole::Popup,
                },
                size: surface.size,
                preferred_scale: surface.preferred_scale,
                scroll: surface.scroll.clone(),
                focused: surface.input.keyboard_focused(),
            })
            .collect();
        LayoutValidationEntry {
            output,
            context,
            surfaces,
        }
    }

    fn current_layout_validation_entry(
        &self,
        output: &wl_output::WlOutput,
    ) -> Option<LayoutValidationEntry> {
        self.configurations
            .output(output)
            .cloned()
            .map(|context| self.layout_validation_entry(output.clone(), context))
    }

    fn replacement_layout_validation_entry(
        &self,
        output: wl_output::WlOutput,
        context: OutputContext,
    ) -> LayoutValidationEntry {
        let mut entry = self.layout_validation_entry(output, context);
        for surface in &mut entry.surfaces {
            surface.scroll = ScrollState::default();
            surface.focused = None;
        }
        entry
    }

    fn install_prepared_layouts(&mut self, outputs: Vec<PreparedOutputLayouts>) {
        for mut output in outputs {
            for surface in self
                .surfaces
                .iter_mut()
                .filter(|surface| surface.output == output.output)
            {
                let root = self
                    .configurations
                    .surface_root(&surface.output, &surface.key)
                    .and_then(|root| root.find(surface.root));
                let Some(root) = root else {
                    surface.prepared = None;
                    continue;
                };
                let prepared = output.surfaces.iter().position(|prepared| {
                    prepared.key == surface.key
                        && prepared.root.id == surface.root
                        && prepared.size == surface.size
                        && prepared.root == *root
                });
                surface.prepared = prepared.map(|index| output.surfaces.swap_remove(index));
                if let Some(prepared) = &surface.prepared {
                    surface.dirty |= surface.scale != prepared.scale;
                    surface.scale = prepared.scale;
                }
            }
        }
        self.reconcile_idle_inhibitors();
    }

    fn reconcile_idle_inhibitors(&mut self) {
        for surface in &mut self.surfaces {
            let desired = self
                .configurations
                .surface_root(&surface.output, &surface.key)
                .and_then(|root| root.find(surface.root))
                .is_some_and(|root| root.idle_inhibit);
            if let Err(error) = self.idle_inhibit.reconcile(
                &mut surface.idle_inhibitor,
                surface.role.surface(),
                desired,
                &self.queue,
            ) {
                self.error = Some(error);
                return;
            }
        }
    }

    fn publish_idle_availability(&mut self) -> Result<(), String> {
        let available = self.idle_inhibit.active().is_some();
        self.configurations
            .set_service("idle.inhibit_available", Scalar::Boolean(available))?;
        self.refresh();
        // protocol withdrawal cannot roll back with a rejected configuration.
        if !available
            && self.configurations.keys().any(|output| {
                idle_inhibit::requested(self.configurations.surfaces(output).unwrap_or_default())
            })
        {
            return Err("the compositor removed idle-inhibition support while a configuration still requires it".into());
        }
        self.reconcile_idle_inhibitors();
        Ok(())
    }

    fn publish_idle_watch_availability(&mut self) {
        let available = self.idle_notify.available();
        if let Err(error) = self
            .configurations
            .set_service("idle.watch_available", Scalar::Boolean(available))
        {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }

    /// arms the watches of the accepted configurations on the current seat
    /// and delivers the resumes of retired watches that were idle.
    fn reconcile_idle_watches(&mut self) {
        let desired: Vec<_> = self
            .configurations
            .idle_watches()
            .into_iter()
            .flat_map(|(output, consumer, definitions)| {
                definitions
                    .into_iter()
                    .enumerate()
                    .map(move |(index, definition)| IdleWatchDesired {
                        output: output.clone(),
                        consumer: consumer.clone(),
                        index,
                        definition,
                    })
            })
            .collect();
        let resumes = self
            .idle_notify
            .reconcile(&desired, self.seat.as_ref(), &self.queue);
        for resume in resumes {
            self.deliver_idle(resume);
        }
    }

    /// runs an idle transition through the checked event path of the
    /// configuration that armed the watch, if that configuration is current.
    fn deliver_idle(&mut self, transition: IdleTransition) {
        let IdleTransition {
            output,
            consumer,
            index,
            idle,
        } = transition;
        if self.configurations.consumer(&output) != Some(consumer.as_str()) {
            return;
        }
        let Some(entry) = self.current_layout_validation_entry(&output) else {
            return;
        };
        let preflight = LayoutPreflight::new(
            &mut self.render,
            self.layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self.configurations.dispatch_idle_checked(
            &output,
            &consumer,
            index,
            idle,
            |surfaces| preflight.validate(&output, surfaces),
        );
        let prepared = preflight.into_prepared();
        match result {
            Ok(Some(outcome)) => {
                self.finish_dispatch(output, Some(consumer), Ok(outcome), None, prepared);
            }
            Ok(None) => {}
            Err(error) => {
                self.finish_dispatch(output, Some(consumer), Err(error), None, prepared);
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        earliest_deadline(
            self.reload_at,
            self.hover.next_deadline(),
            self.pending_launches.next_deadline(),
            self.surfaces.iter().filter_map(|surface| surface.retry_at),
        )
        .into_iter()
        .chain(self.configurations.next_timer_deadline())
        .chain(self.sleep.next_deadline())
        .min()
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
    fn update_applications(&mut self) {
        let Some(published) = self.applications.take_snapshot() else {
            return;
        };
        // the catalog changes only with the list the configurations accept, so
        // a launch always resolves against the entries a configuration saw.
        match self.configurations.set_applications(published.snapshot) {
            Ok(()) => self.application_catalog = published.catalog,
            Err(error) => eprintln!("thing: {error}"),
        }
        self.refresh();
    }

    fn start_activation_fallbacks(&mut self) {
        for fallback in self.activator.take_fallbacks() {
            for (request, token) in fallback.into_launches() {
                self.spawn_launch(request, token.as_deref());
            }
        }
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
    fn update_sleep(&mut self) {
        let Some(states) = self.sleep.poll(Instant::now()) else {
            return;
        };
        if let Err(error) = self.configurations.set_sleep_inhibitors(states) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }

    /// leases follow the declarations of the accepted configurations; the
    /// new states arrive through the sleep ping.
    fn reconcile_sleep(&mut self) {
        let definitions = self.configurations.sleep_inhibitor_definitions();
        self.sleep.reconcile(&definitions);
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
    fn update_managed_processes(&mut self) {
        let Some(snapshot) = self
            .managed_processes
            .as_ref()
            .and_then(ProcessManager::take_snapshot)
        else {
            return;
        };
        if let Err(error) = self
            .configurations
            .set_managed_processes(snapshot.runtime())
        {
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
    fn update_focused_clipboard(&mut self) {
        let Some(outcome) = self.focused_clipboard.take_paste_outcome() else {
            return;
        };
        let Some(pending) = self.pending_paste.take() else {
            return;
        };
        if outcome.id != pending.id {
            self.pending_paste = Some(pending);
            return;
        }
        let text = match outcome.result {
            Ok(text) => text,
            Err(error) => {
                eprintln!("thing: cannot paste text: {error}");
                return;
            }
        };
        let Some(surface) = self.index(&pending.surface) else {
            return;
        };
        let id = pending.target.target.node();
        let surface_has_keyboard_focus = self.keyboard_surface.as_ref() == Some(&pending.surface);
        let target_is_current = self.surface_target_is_current(surface, &pending.target);
        let Some((node, bounds)) = self
            .text_input_node_and_bounds(surface, id, LayoutSnapshot::Input)
            .filter(|_| surface_has_keyboard_focus && target_is_current)
        else {
            return;
        };
        let focused = self.surfaces[surface].input.focused;
        if focused != Some(id) {
            return;
        }
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let width = (bounds.width - 2.0 * padding_x).max(0.0);
        let active = self.surfaces[surface]
            .text_input
            .as_mut()
            .filter(|active| active.id == id && active.generation == pending.generation)
            .map(|active| {
                active.editor.prepare(&node, width);
                (active.id, active.generation, active.editor.revision())
            });
        if !pending_paste_is_current(
            surface_has_keyboard_focus,
            target_is_current,
            focused,
            (id, pending.generation, pending.revision),
            active,
        ) {
            return;
        }

        self.cancel_input_method_composition(&pending.surface);
        if !self.reset_input_method_generation(surface, id, pending.generation) {
            self.reconcile_input_method(InputMethodChangeCause::Other);
            return;
        }
        let generation = self.surfaces[surface]
            .text_input
            .as_ref()
            .expect("the paste target survived its generation reset")
            .generation;
        self.apply_text_clipboard_edit(surface, id, generation, &text, None);
        self.reconcile_input_method(InputMethodChangeCause::Other);
    }

    fn cancel_pending_text_paste(&mut self) {
        self.pending_paste = None;
        self.focused_clipboard.cancel_pending_paste();
    }

    fn refresh(&mut self) {
        let layer_shell_version = self.layer_shell_version;
        let configured_outputs: Vec<_> = self.configurations.keys().cloned().collect();
        let entries = configured_outputs
            .iter()
            .filter_map(|output| {
                self.configurations
                    .output(output)
                    .cloned()
                    .map(|context| self.layout_validation_entry(output.clone(), context))
            })
            .collect();
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            entries,
        );
        let outputs: Vec<_> = self
            .configurations
            .refresh_checked(|output, surfaces| preflight.validate(output, surfaces));
        let prepared: Vec<_> = preflight
            .into_prepared()
            .into_iter()
            .filter(|prepared| outputs.contains(&prepared.output))
            .collect();
        self.update_watch();
        for output in &outputs {
            self.invalidate_output(output);
        }
        self.install_prepared_layouts(prepared);
        for output in configured_outputs {
            self.reconcile_custom_pointer(&output);
        }
        self.reconcile_published_configuration();
    }

    fn reconcile_published_configuration(&mut self) {
        self.reconcile_idle_inhibitors();
        self.reconcile_focus_targets();
        self.apply_pending_ui_actions_now();
        self.prepare_invalidated_input_layouts_now();
        self.reconcile_layout_focus();
        self.reconcile_input_method(InputMethodChangeCause::Other);
    }

    fn reload(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let outputs: Vec<_> = self
            .outputs
            .outputs()
            .filter_map(|output| {
                self.output_context(&output)
                    .or_else(|| self.configurations.output(&output).cloned())
                    .map(|context| (output, context))
            })
            .collect();
        let layer_shell_version = self.layer_shell_version;
        let entries = outputs
            .iter()
            .map(|(output, context)| {
                self.replacement_layout_validation_entry(output.clone(), context.clone())
            })
            .collect();
        let retired_consumers = self.configurations.consumers();
        let loop_handle = self.loop_handle.clone();
        let replacement = (|| {
            let preflight = LayoutPreflight::new(
                &mut self.render,
                layer_shell_version,
                self.idle_inhibit.active().is_some(),
                entries,
            );
            let mut configurations = self
                .configurations
                .prepare_reload_for_checked(outputs, |output, runtime| {
                    preflight.validate(output, runtime.surfaces())
                })?;
            let process_definitions = configurations.managed_process_definitions().to_vec();
            let projected_processes = project_managed_processes(
                self.managed_processes.as_ref(),
                &process_definitions,
                ManagedProcessScopeChange::Replace,
            )?;
            configurations
                .set_managed_processes(projected_processes.clone(), |output, runtime| {
                    preflight.validate(output, runtime.surfaces())
                })?;
            let activation = prepare_and_commit_managed_processes(
                &mut self.managed_processes,
                &loop_handle,
                &process_definitions,
                ManagedProcessScopeChange::Replace,
                projected_processes,
                |snapshot| {
                    configurations.set_managed_processes(snapshot, |output, runtime| {
                        preflight.validate(output, runtime.surfaces())
                    })
                },
            )?;
            let prepared = preflight.into_prepared();
            Ok::<_, String>((configurations, activation, prepared))
        })();
        let (configurations, activation, prepared) = match replacement {
            Ok(replacement) => replacement,
            Err(error) => {
                eprintln!("thing: reload failed; keeping the previous surfaces: {error}");
                return Ok(());
            }
        };
        self.configurations.commit_reload(configurations);
        self.cancel_pending_text_paste();
        // the replacement runtimes never inherit an in-flight pointer sequence.
        if let Some(capture) = self.pointer_capture.take() {
            self.clear_pressed_for_capture(&capture);
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
        self.pending_ui_actions.clear();
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
        for output in &outputs {
            self.sync_output_surfaces(output, true, qh)?;
        }
        self.install_prepared_layouts(prepared);
        arm_managed_processes(self.managed_processes.as_ref(), activation);
        self.reconcile_sleep();
        self.reconcile_idle_watches();
        if self
            .keyboard_surface
            .as_ref()
            .is_some_and(|wl| self.index(wl).is_none())
        {
            self.clear_keyboard_focus();
        }
        self.apply_pending_ui_actions(MissingSurfacePolicy::Expire)?;
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
            let entry = self.layout_validation_entry(output.clone(), context.clone());
            let loop_handle = self.loop_handle.clone();
            let preflight = LayoutPreflight::new(
                &mut self.render,
                layer_shell_version,
                self.idle_inhibit.active().is_some(),
                vec![entry],
            );
            let Some(mut configuration) =
                self.configurations
                    .prepare_add_checked(output.clone(), context, |runtime| {
                        preflight.validate(&output, runtime.surfaces())
                    })?
            else {
                return Ok(());
            };
            let process_definitions = configuration.managed_process_definitions().to_vec();
            let projected_processes = project_managed_processes(
                self.managed_processes.as_ref(),
                &process_definitions,
                ManagedProcessScopeChange::Reconcile,
            )?;
            configuration.set_managed_processes(projected_processes.clone(), |runtime| {
                preflight.validate(&output, runtime.surfaces())
            })?;
            let activation = prepare_and_commit_managed_processes(
                &mut self.managed_processes,
                &loop_handle,
                &process_definitions,
                ManagedProcessScopeChange::Reconcile,
                projected_processes,
                |snapshot| {
                    configuration.set_managed_processes(snapshot, |runtime| {
                        preflight.validate(&output, runtime.surfaces())
                    })
                },
            )?;
            let prepared = preflight.into_prepared();
            self.configurations.commit_add(configuration);
            self.sync_output_surfaces(&output, false, qh)?;
            self.install_prepared_layouts(prepared);
            arm_managed_processes(self.managed_processes.as_ref(), activation);
            self.reconcile_sleep();
            self.reconcile_idle_watches();
            self.update_watch();
            return Ok(());
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
        )?;
        surface.panel = Some(panel);
        surface.role.surface().commit();
        self.surfaces.push(surface);
        Ok(())
    }

    fn make_surface(
        &self,
        role: Role,
        init: SurfaceInit,
        qh: &QueueHandle<Self>,
    ) -> Result<Surface, String> {
        let desired = self
            .configurations
            .surface_root(&init.owner.output, &init.owner.key)
            .and_then(|root| root.find(init.root))
            .is_some_and(|root| root.idle_inhibit);
        let mut idle_inhibitor = None;
        self.idle_inhibit
            .reconcile(&mut idle_inhibitor, role.surface(), desired, qh)?;
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
        Ok(Surface {
            window: None,
            role,
            idle_inhibitor,
            output: init.owner.output,
            key: init.owner.key,
            namespace: init.namespace,
            root: init.root,
            size: init.size,
            panel: None,
            scale: init.scale,
            preferred_scale: init.scale,
            viewport,
            fractional,
            layouts: SurfaceLayouts::default(),
            prepared: None,
            scroll: ScrollState::default(),
            input: Interaction::default(),
            focus_target: None,
            pending_focus: None,
            ui_actions: Vec::new(),
            autofocus_pending: true,
            text_input: None,
            pointer_position: None,
            hover_path: Vec::new(),
            press_serial: None,
            pressed_button: None,
            pressed_target: None,
            range_press: None,
            configured: false,
            dirty: true,
            waiting: false,
            retry_at: None,
        })
    }

    fn output_context(&self, output: &wl_output::WlOutput) -> Option<OutputContext> {
        let mut context = output_context(self.outputs.info(output)?)?;
        if let Some(surface) = self
            .surfaces
            .iter()
            .find(|surface| &surface.output == output && matches!(surface.role, Role::Panel(_)))
        {
            context.scale = surface.preferred_scale;
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
        // include open popups before panel synchronization commits surface changes.
        self.reconcile_idle_inhibitors();
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
            surface.layouts = SurfaceLayouts::default();
            surface.prepared = None;
            surface.scroll = ScrollState::default();
            surface.input = Interaction::default();
            surface.focus_target = None;
            surface.pending_focus = None;
            surface.ui_actions.clear();
            surface.autofocus_pending = true;
            surface.text_input = None;
            surface.pointer_position = None;
            surface.hover_path.clear();
            surface.press_serial = None;
            surface.pressed_button = None;
            surface.pressed_target = None;
            surface.range_press = None;
        }
        surface.dirty = true;
        layer.wl_surface().commit();
        if reset_input && self.keyboard_surface.as_ref() == Some(&wl) {
            self.clear_key_repeat();
            self.schedule_autofocus(index);
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

    fn prepare_configured_surface_layout(
        &mut self,
        index: usize,
        size: (u32, u32),
    ) -> Result<PreparedSurfaceLayout, String> {
        let surface = self
            .surfaces
            .get(index)
            .ok_or("configured surface is unavailable")?;
        let accepted = surface.scale;
        let preferred = surface.preferred_scale;
        if preferred != accepted
            && let Ok(prepared) = self.prepare_surface_layout_at_scale(index, size, preferred)
        {
            return Ok(prepared);
        }
        self.prepare_surface_layout_at_scale(index, size, accepted)
    }

    fn prepare_surface_layout_at_scale(
        &mut self,
        index: usize,
        size: (u32, u32),
        scale: Scale,
    ) -> Result<PreparedSurfaceLayout, String> {
        let surface = self
            .surfaces
            .get(index)
            .ok_or("configured surface is unavailable")?;
        let root = self
            .configurations
            .surface_root(&surface.output, &surface.key)
            .and_then(|root| root.find(surface.root))
            .cloned()
            .ok_or("configured surface root is unavailable")?;
        prepare_surface_layout(
            &mut self.render,
            surface.key.clone(),
            &root,
            size,
            surface.scroll.clone(),
            surface.input.keyboard_focused(),
            scale,
        )
    }

    fn set_surface_scale(&mut self, index: usize, scale: Scale) {
        let Some(surface) = self.surfaces.get_mut(index) else {
            return;
        };
        surface.preferred_scale = scale;
        let output = surface.output.clone();
        if matches!(surface.role, Role::Panel(_)) {
            self.set_output_scale(&output, scale);
        } else {
            let size = surface.size;
            match self.prepare_surface_layout_at_scale(index, size, scale) {
                Ok(prepared) => {
                    let surface = &mut self.surfaces[index];
                    surface.scale = prepared.scale;
                    surface.prepared = Some(prepared);
                    surface.dirty = true;
                }
                Err(error) => eprintln!("thing: cannot update popup scale: {error}"),
            }
        }
    }

    fn set_output_scale(&mut self, output: &wl_output::WlOutput, scale: Scale) {
        let Some(mut context) = self.configurations.output(output).cloned() else {
            return;
        };
        context.scale = scale;
        let layer_shell_version = self.layer_shell_version;
        let entry = self.layout_validation_entry(output.clone(), context.clone());
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self
            .configurations
            .set_output_checked(output, context, |surfaces| {
                preflight.validate(output, surfaces)
            });
        let prepared = preflight.into_prepared();
        self.update_watch();
        match result {
            Ok(changed) => {
                if changed {
                    self.invalidate_output(output);
                } else {
                    self.reconcile_custom_pointer(output);
                }
                self.install_prepared_layouts(prepared);
                self.reconcile_published_configuration();
            }
            Err(error) => eprintln!("thing: cannot update output scale: {error}"),
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
        let captured = self
            .pointer_capture
            .as_ref()
            .is_some_and(|capture| capture.surface == wl);
        if captured {
            if notify_hover {
                self.cancel_custom_pointer();
            } else {
                self.pointer_capture = None;
            }
        }
        let Some(index) = self.index(&wl) else {
            return;
        };
        let surface_root = self.surfaces[index].root;
        let hovered = std::mem::take(&mut self.surfaces[index].hover_path);
        let pressed = notify_hover
            .then(|| self.surfaces[index].range_press.clone())
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
            if let Some(target) = pressed
                && self.runtime_owns_active_press(&owner.output, &target)
            {
                self.dispatch(
                    owner.output.clone(),
                    target.target.node(),
                    Event::Pressed(false),
                    None,
                );
            }
            for target in hovered {
                if self.scoped_target_identity_is_current_at(&owner.output, surface_root, &target) {
                    self.dispatch(
                        owner.output.clone(),
                        target.target.node(),
                        Event::Hover(false),
                        None,
                    );
                }
            }
        }
    }

    fn index(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|surface| surface.role.surface() == wl)
    }

    fn apply_pending_ui_actions(&mut self, missing: MissingSurfacePolicy) -> Result<(), String> {
        self.assign_pending_ui_actions(missing);
        let keyboard_surface = self.keyboard_surface.clone();
        for index in 0..self.surfaces.len() {
            let keyboard_focused =
                keyboard_surface.as_ref() == Some(self.surfaces[index].role.surface());
            if self.surfaces[index].ui_actions.is_empty()
                && !(keyboard_focused && self.surfaces[index].pending_focus.is_some())
            {
                continue;
            }
            let output = self.surfaces[index].output.clone();
            let surface_root = self.surfaces[index].root;
            let mut actions = std::mem::take(&mut self.surfaces[index].ui_actions);
            if keyboard_focused && let Some(target) = self.surfaces[index].pending_focus.take() {
                actions.insert(0, HostUiAction::Focus(target));
            }

            let mut active = Vec::new();
            for action in actions {
                let target = action.target();
                let current = self.scoped_target_is_current(&output, target)
                    && self
                        .configurations
                        .rendered_surface_root(&output, target.target.node())
                        == Some(surface_root);
                if !current {
                    continue;
                }
                if action.is_focus() && !keyboard_focused {
                    self.surfaces[index].pending_focus = Some(target.clone());
                } else {
                    active.push(action);
                }
            }
            if active.is_empty() {
                continue;
            }

            let Some(root) = self
                .configurations
                .surface_root(&output, &self.surfaces[index].key)
                .and_then(|root| root.find(surface_root))
                .cloned()
            else {
                continue;
            };
            let size = self.surfaces[index].size;
            let prepared = self.surfaces[index].take_prepared_layout(&root);
            let (mut layout, mut scroll) = if let Some(prepared) = prepared {
                (prepared.layout, prepared.scroll)
            } else {
                let mut scroll = self.surfaces[index].scroll.clone();
                let layout = self.render.layout(
                    &root,
                    Size {
                        width: size.0 as f32,
                        height: size.1 as f32,
                    },
                    &mut scroll,
                )?;
                (layout, scroll)
            };
            let mut changed = false;
            let mut focus = None;
            for action in active {
                let target = action.target().clone();
                let id = target.target.node();
                match action {
                    HostUiAction::Focus(_) => {
                        if layout.focus_order().any(|candidate| candidate == id) {
                            changed |= layout.reveal_y(id, &mut scroll);
                            focus = Some(target);
                        }
                    }
                    HostUiAction::Reveal(_) => {
                        changed |= layout.reveal_y(id, &mut scroll);
                    }
                }
            }
            if changed {
                layout = self.render.layout(
                    &root,
                    Size {
                        width: size.0 as f32,
                        height: size.1 as f32,
                    },
                    &mut scroll,
                )?;
            }
            let surface = &mut self.surfaces[index];
            surface.scroll = scroll;
            surface.layouts.stage_input(layout);
            surface.dirty = true;
            if let Some(target) = focus {
                self.focus_from_keyboard(index, target);
            }
        }
        Ok(())
    }

    fn apply_pending_ui_actions_now(&mut self) {
        if let Err(error) = self.apply_pending_ui_actions(MissingSurfacePolicy::WaitForSameCycle) {
            self.error = Some(error);
        }
    }

    fn prepare_invalidated_input_layouts(&mut self) -> Result<(), String> {
        let keyboard_surface = self.keyboard_surface.clone();
        let indices: Vec<_> = self
            .surfaces
            .iter()
            .enumerate()
            .filter(|(_, surface)| {
                surface.layouts.input_needs_refresh()
                    && (surface.input.focused.is_some()
                        || keyboard_surface.as_ref() == Some(surface.role.surface()))
            })
            .map(|(index, _)| index)
            .collect();
        for index in indices {
            let output = self.surfaces[index].output.clone();
            let surface_root = self.surfaces[index].root;
            let Some(root) = self
                .configurations
                .surface_root(&output, &self.surfaces[index].key)
                .and_then(|root| root.find(surface_root))
                .cloned()
            else {
                self.surfaces[index].layouts.clear_input();
                continue;
            };
            let size = self.surfaces[index].size;
            let mut scroll = self.surfaces[index].scroll.clone();
            let mut layout = self.render.layout(
                &root,
                Size {
                    width: size.0 as f32,
                    height: size.1 as f32,
                },
                &mut scroll,
            )?;
            let focused = self.surfaces[index].input.keyboard_focused();
            if focused.is_some_and(|id| layout.reveal_y(id, &mut scroll)) {
                layout = self.render.layout(
                    &root,
                    Size {
                        width: size.0 as f32,
                        height: size.1 as f32,
                    },
                    &mut scroll,
                )?;
            }
            let surface = &mut self.surfaces[index];
            surface.scroll = scroll;
            surface.layouts.stage_input(layout);
            surface.dirty = true;
        }
        Ok(())
    }

    fn prepare_invalidated_input_layouts_now(&mut self) {
        if let Err(error) = self.prepare_invalidated_input_layouts() {
            self.error = Some(error);
        }
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
            let entry = self
                .current_layout_validation_entry(&output)
                .ok_or("animated output has no layout context")?;
            let preflight = LayoutPreflight::new(
                &mut self.render,
                layer_shell_version,
                self.idle_inhibit.active().is_some(),
                vec![entry],
            );
            let changed = self
                .configurations
                .advance_checked(&output, now, |surfaces| {
                    preflight.validate(&output, surfaces)
                });
            let prepared = preflight.into_prepared();
            if changed? {
                for surface in self
                    .surfaces
                    .iter_mut()
                    .filter(|surface| surface.output == output)
                {
                    // a deferred Vulkan frame may retain the previous animation's geometry.
                    surface.layouts.invalidate_input();
                    surface.dirty = true;
                }
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
                self.install_prepared_layouts(prepared);
            }
        }
        self.apply_pending_ui_actions(MissingSurfacePolicy::Expire)?;
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
            let mut scroll = surface.scroll.clone();
            let layout = if let Some(layout) = surface.layouts.take_input() {
                // Input geometry can include focus-driven scroll changes made
                // after preflight, so it supersedes any remaining cached plan.
                surface.prepared = None;
                layout
            } else if let Some(prepared) = surface.take_prepared_layout(root) {
                scroll = prepared.scroll;
                prepared.layout
            } else {
                self.render.layout(
                    root,
                    Size {
                        width: surface.size.0 as f32,
                        height: surface.size.1 as f32,
                    },
                    &mut scroll,
                )?
            };
            let presented_ui = configurations
                .presented_targets(&surface.output, root, &layout)
                .ok_or("rendered output has no configuration consumer")?;
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
            let input = paint_interaction(
                surface.input,
                keyboard_surface.as_ref() == Some(surface.role.surface()),
            );
            let window = surface
                .window
                .as_mut()
                .ok_or("render surface was not initialized")?;
            let result = self.render.draw(
                window,
                PaintFrame {
                    root,
                    layout: &layout,
                    physical,
                    scale: surface.scale.factor(),
                    input,
                    text_input,
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
                    let (consumer, targets) = presented_ui;
                    surface.waiting = true;
                    surface.scroll = scroll;
                    surface
                        .layouts
                        .publish(layout, surface.root, consumer, targets);
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
        let mut presented_outputs = Vec::new();
        for output in presented
            .iter()
            .filter_map(|index| self.surfaces.get(*index))
            .map(|surface| surface.output.clone())
        {
            if !presented_outputs.contains(&output) {
                presented_outputs.push(output);
            }
        }
        self.reconcile_layout_focus();
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
        for output in &presented_outputs {
            self.reconcile_custom_pointer(output);
            self.reconcile_range_presses(output);
            self.reconcile_presented_pointer_state(output);
        }
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
        popup_surface.layouts.presented()?;
        let Role::Popup(role) = &popup_surface.role else {
            return None;
        };
        let parent_index = self.surfaces.iter().position(|surface| {
            surface.output == owner.output
                && surface.key == owner.key
                && matches!(surface.role, Role::Panel(_))
        })?;
        let parent = &self.surfaces[parent_index];
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
            Some(anchor) => {
                self.current_presented_target(parent_index, anchor)?;
                parent
                    .layouts
                    .presented()?
                    .semantic_placement(anchor)
                    .and_then(placement_anchor)?
            }
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
        let Some(entry) = self.current_layout_validation_entry(&output) else {
            return;
        };
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self
            .configurations
            .dispatch_checked(&output, id, event, |surfaces| {
                preflight.validate(&output, surfaces)
            });
        let prepared = preflight.into_prepared();
        self.finish_dispatch(output, consumer, result, origin, prepared);
    }

    fn dispatch_due_timers(&mut self, now: Instant) {
        for (output, token) in self.configurations.due_timers(now) {
            let consumer = self.configurations.consumer(&output).map(str::to_owned);
            let Some(entry) = self.current_layout_validation_entry(&output) else {
                continue;
            };
            let preflight = LayoutPreflight::new(
                &mut self.render,
                self.layer_shell_version,
                self.idle_inhibit.active().is_some(),
                vec![entry],
            );
            let result =
                self.configurations
                    .dispatch_timer_checked(&output, &token, now, |surfaces| {
                        preflight.validate(&output, surfaces)
                    });
            let prepared = preflight.into_prepared();
            match result {
                Ok(Some(outcome)) => {
                    self.finish_dispatch(output, consumer, Ok(outcome), None, prepared);
                }
                Ok(None) => {}
                Err(error) => {
                    self.finish_dispatch(output, consumer, Err(error), None, prepared);
                }
            }
            if self.error.is_some() {
                break;
            }
        }
    }

    /// captures the wheel route under `point` from the presented tree, with
    /// each handler's local coordinates in its own presented transform. it
    /// returns `None` when any stop no longer matches the runtime.
    fn capture_scroll_route(
        &self,
        index: usize,
        point: Point,
        x: f64,
        y: f64,
        position: (f64, f64),
        source: Option<wl_pointer::AxisSource>,
    ) -> Option<Vec<ScrollRouteStep>> {
        let layout = self.surfaces.get(index)?.layouts.presented()?;
        layout
            .scroll_route(point)
            .into_iter()
            .map(|step| match step {
                ScrollStep::Handler(id) => {
                    let target = self.current_presented_scroll_target(index, id)?;
                    let local = layout.surface_to_local(id, point)?;
                    let input =
                        runtime_scroll_input(local, x, y, position, &self.modifiers, source)?;
                    Some(ScrollRouteStep::Handler(target, input))
                }
                ScrollStep::Viewport(id) => self
                    .current_presented_scroll_view_target(index, id)
                    .map(ScrollRouteStep::Viewport),
            })
            .collect()
    }

    /// offers one wheel event to each captured stop in order. a handler that
    /// returns `false` or a viewport at its limit passes it on. callbacks may
    /// change the tree, so every stop is revalidated before it runs, and the
    /// route ends at the first one that no longer applies.
    fn dispatch_scroll_route(
        &mut self,
        output: wl_output::WlOutput,
        event_surface: &wl_surface::WlSurface,
        route: Vec<ScrollRouteStep>,
        y: f64,
    ) {
        for step in route {
            let Some(index) = self.index(event_surface) else {
                return;
            };
            match step {
                ScrollRouteStep::Handler(target, input) => {
                    if !self.surface_scroll_target_is_current(index, &target) {
                        return;
                    }
                    if self.dispatch_scroll(output.clone(), &target, input) != Some(false) {
                        return;
                    }
                }
                ScrollRouteStep::Viewport(target) => {
                    if !self.surface_scroll_view_target_is_current(index, &target) {
                        return;
                    }
                    // viewports scroll vertically; horizontal input passes on.
                    if let Some(delta) = scroll_distance(y)
                        && self.scroll_viewport(index, target.target.node(), delta)
                    {
                        return;
                    }
                }
            }
        }
    }

    /// moves one presented viewport and prepares the surface at the new
    /// offset. it returns whether the viewport could move.
    fn scroll_viewport(&mut self, index: usize, id: NodeId, delta: f32) -> bool {
        let mut scroll = self.surfaces[index].scroll.clone();
        let moved = self.surfaces[index]
            .layouts
            .presented()
            .is_some_and(|layout| layout.scroll_view_y(id, delta, &mut scroll));
        if !moved {
            return false;
        }
        let surface = &self.surfaces[index];
        let key = surface.key.clone();
        let size = surface.size;
        let focused = surface.input.keyboard_focused();
        let Some(root) = self
            .configurations
            .surface_root(&surface.output, &surface.key)
            .and_then(|root| root.find(surface.root))
            .cloned()
        else {
            return true;
        };
        match prepare_surface_layout(
            &mut self.render,
            key,
            &root,
            size,
            scroll,
            focused,
            surface.scale,
        ) {
            Ok(prepared) => {
                let surface = &mut self.surfaces[index];
                surface.scroll = prepared.scroll.clone();
                surface.prepared = Some(prepared);
                surface.layouts.invalidate_input();
                surface.dirty = true;
            }
            Err(error) => eprintln!("thing: cannot apply scroll position: {error}"),
        }
        true
    }

    fn dispatch_scroll(
        &mut self,
        output: wl_output::WlOutput,
        target: &ScopedUiTarget,
        input: RuntimeScrollInput,
    ) -> Option<bool> {
        let layer_shell_version = self.layer_shell_version;
        let consumer = Some(target.consumer.clone());
        let entry = self.current_layout_validation_entry(&output)?;
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self.configurations.dispatch_scroll_checked(
            &output,
            &target.target,
            input,
            |surfaces| preflight.validate(&output, surfaces),
        );
        let prepared = preflight.into_prepared();
        match result {
            Ok(Some(ScrollHandlerOutcome { consumed, dispatch })) => self
                .finish_dispatch(output, consumer, Ok(dispatch), None, prepared)
                .then_some(consumed),
            Ok(None) => None,
            Err(error) => {
                self.finish_dispatch(output, consumer, Err(error), None, prepared);
                None
            }
        }
    }

    fn finish_dispatch(
        &mut self,
        output: wl_output::WlOutput,
        consumer: Option<String>,
        result: Result<DispatchOutcome, String>,
        origin: Option<InputOrigin>,
        prepared: Vec<PreparedOutputLayouts>,
    ) -> bool {
        self.update_watch();
        let outcome = match result {
            Ok(result) => result,
            Err(error) => {
                eprintln!("thing: {error}");
                // hover cleanup can fail after its first valid refresh was
                // published, so reconcile whichever snapshot remains.
                self.invalidate_output(&output);
                self.reconcile_published_configuration();
                return false;
            }
        };
        // a successful handler can remove its own popup. request activation
        // before invalidation destroys the surface that received the input.
        for action in outcome.actions {
            let result = match action {
                Action::Niri(action) => self.niri.send(action),
                Action::Media { action, player } => self
                    .media
                    .send(&action, player.as_deref())
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
                Action::ManagedProcess { key, action } => {
                    send_managed_process_action(self.managed_processes.as_ref(), &key, action)
                }
                Action::SleepInhibitor { key, action } => {
                    self.sleep.act(&key, action);
                    Ok(())
                }
                Action::Launch { argv, cwd } => {
                    self.request_launch(
                        PendingLaunch::spawn(LaunchRequest { argv, cwd }),
                        origin.as_ref(),
                    );
                    Ok(())
                }
                Action::LaunchApplication(launch) => {
                    applications::plan(&self.application_catalog, &launch).map(|plan| {
                        let pending = |work| PendingLaunch {
                            work,
                            entry: Some((launch.id.clone(), launch.revision)),
                        };
                        match plan {
                            // each process gets its own activation token.
                            Plan::Spawn(requests) => {
                                for request in requests {
                                    self.request_launch(
                                        pending(PendingWork::Spawn(request)),
                                        origin.as_ref(),
                                    );
                                }
                            }
                            Plan::Activate(call) => self.request_launch(
                                pending(PendingWork::Activate(Box::new(call))),
                                origin.as_ref(),
                            ),
                        }
                    })
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
                Action::ReplyNotification { key, text } => self
                    .notifications
                    .as_ref()
                    .ok_or("notification server is disabled")
                    .and_then(|server| server.reply(key, text))
                    .map_err(str::to_owned),
                Action::ForgetNotification { key } => self
                    .notifications
                    .as_ref()
                    .ok_or("notification server is disabled")
                    .and_then(|server| server.forget(key))
                    .map_err(str::to_owned),
                Action::ClearNotificationHistory => self
                    .notifications
                    .as_ref()
                    .ok_or("notification server is disabled")
                    .and_then(|server| server.clear_history())
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
        if let Some(consumer) = consumer {
            self.queue_ui_actions(output.clone(), consumer, outcome.ui_actions);
        }
        if outcome.changed {
            self.invalidate_output(&output);
            self.install_prepared_layouts(prepared);
        }
        self.reconcile_custom_pointer(&output);
        self.reconcile_published_configuration();
        true
    }

    fn queue_ui_actions(
        &mut self,
        output: wl_output::WlOutput,
        consumer: String,
        actions: Vec<UiAction>,
    ) {
        for action in actions {
            let action = match action {
                UiAction::Focus(target) => HostUiAction::Focus(ScopedUiTarget {
                    consumer: consumer.clone(),
                    target,
                }),
                UiAction::Reveal(target) => HostUiAction::Reveal(ScopedUiTarget {
                    consumer: consumer.clone(),
                    target,
                }),
            };
            if action.is_focus() {
                self.pending_ui_actions.retain(|pending| {
                    pending.output != output
                        || pending.action.target().consumer != consumer
                        || !pending.action.is_focus()
                });
                for surface in self
                    .surfaces
                    .iter_mut()
                    .filter(|surface| surface.output == output)
                {
                    surface.ui_actions.retain(|pending| {
                        pending.target().consumer != consumer || !pending.is_focus()
                    });
                    if surface
                        .pending_focus
                        .as_ref()
                        .is_some_and(|pending| pending.consumer == consumer)
                    {
                        surface.pending_focus = None;
                    }
                }
            }
            self.pending_ui_actions.push(PendingUiAction {
                output: output.clone(),
                action,
            });
            if self.pending_ui_actions.len() > MAX_PENDING_UI_ACTIONS {
                self.pending_ui_actions.remove(0);
            }
        }
        // bind actions to an existing surface immediately so removing that
        // surface also retires the actions. a popup created by this event is
        // allowed to claim an action later in the same cycle.
        self.assign_pending_ui_actions(MissingSurfacePolicy::WaitForSameCycle);
    }

    fn cancel_queued_focus(&mut self, output: &wl_output::WlOutput) {
        let Some(consumer) = self.configurations.consumer(output).map(str::to_owned) else {
            return;
        };
        self.pending_ui_actions.retain(|pending| {
            &pending.output != output
                || pending.action.target().consumer != consumer
                || !pending.action.is_focus()
        });
        for surface in self
            .surfaces
            .iter_mut()
            .filter(|surface| &surface.output == output)
        {
            surface
                .ui_actions
                .retain(|action| action.target().consumer != consumer || !action.is_focus());
            if surface
                .pending_focus
                .as_ref()
                .is_some_and(|target| target.consumer == consumer)
            {
                surface.pending_focus = None;
            }
        }
    }

    fn cancel_all_queued_focus(&mut self) {
        retain_non_focus_actions(&mut self.pending_ui_actions, |pending| {
            pending.action.is_focus()
        });
        for surface in &mut self.surfaces {
            retain_non_focus_actions(&mut surface.ui_actions, HostUiAction::is_focus);
            surface.pending_focus = None;
        }
    }

    fn assign_pending_ui_actions(&mut self, missing: MissingSurfacePolicy) {
        let mut retained = Vec::new();
        for pending in std::mem::take(&mut self.pending_ui_actions) {
            let target = pending.action.target();
            if self.configurations.consumer(&pending.output) != Some(target.consumer.as_str())
                || !self
                    .configurations
                    .ui_target_is_current(&pending.output, &target.target)
            {
                continue;
            }
            let Some(root) = self
                .configurations
                .rendered_surface_root(&pending.output, target.target.node())
            else {
                continue;
            };
            let surface = self
                .surfaces
                .iter()
                .position(|surface| surface.output == pending.output && surface.root == root);
            match surface_action_route(surface, missing) {
                SurfaceActionRoute::Assign(index) => {
                    let surface = &mut self.surfaces[index];
                    surface.ui_actions.push(pending.action);
                    if surface.ui_actions.len() > MAX_PENDING_UI_ACTIONS {
                        surface.ui_actions.remove(0);
                    }
                    surface.dirty = true;
                }
                SurfaceActionRoute::Retain => retained.push(pending),
                SurfaceActionRoute::Drop => {}
            }
        }
        self.pending_ui_actions = retained;
    }

    fn reconcile_focus_targets(&mut self) {
        let mut cancelled = Vec::new();
        let mut clear_repeat = false;
        for index in 0..self.surfaces.len() {
            let output = self.surfaces[index].output.clone();
            let surface_root = self.surfaces[index].root;
            let focused = self.surfaces[index].input.focused;
            let focus_is_current = focused.is_none()
                || self.surfaces[index]
                    .focus_target
                    .as_ref()
                    .is_some_and(|target| {
                        focused == Some(target.target.node())
                            && self.scoped_target_is_current(&output, target)
                            && self
                                .configurations
                                .rendered_surface_root(&output, target.target.node())
                                == Some(surface_root)
                    });
            if !focus_is_current {
                let surface = &mut self.surfaces[index];
                surface.input.focus_from_pointer(None);
                surface.focus_target = None;
                surface.text_input = None;
                surface.dirty = true;
                cancelled.push(surface.role.surface().clone());
                clear_repeat |= self.keyboard_surface.as_ref() == Some(surface.role.surface());
            }

            let pending_is_current =
                self.surfaces[index]
                    .pending_focus
                    .as_ref()
                    .is_none_or(|target| {
                        self.scoped_target_is_current(&output, target)
                            && self
                                .configurations
                                .rendered_surface_root(&output, target.target.node())
                                == Some(surface_root)
                    });
            if !pending_is_current {
                self.surfaces[index].pending_focus = None;
            }
            let actions = std::mem::take(&mut self.surfaces[index].ui_actions);
            let actions = actions
                .into_iter()
                .filter(|action| {
                    let target = action.target();
                    self.scoped_target_is_current(&output, target)
                        && self
                            .configurations
                            .rendered_surface_root(&output, target.target.node())
                            == Some(surface_root)
                })
                .collect();
            self.surfaces[index].ui_actions = actions;
        }
        if clear_repeat {
            self.clear_key_repeat();
        }
        for surface in cancelled {
            self.cancel_input_method_composition(&surface);
        }
    }

    fn reconcile_layout_focus(&mut self) {
        let invalid: Vec<_> = self
            .surfaces
            .iter()
            .enumerate()
            .filter_map(|(index, surface)| {
                let id = surface.input.focused?;
                surface.layouts.input().and_then(|layout| {
                    (!layout.focus_order().any(|candidate| candidate == id)).then_some(index)
                })
            })
            .collect();
        let mut clear_repeat = false;
        let mut cancelled = Vec::new();
        for index in invalid {
            let surface = &mut self.surfaces[index];
            surface.input.focus_from_pointer(None);
            surface.focus_target = None;
            surface.text_input = None;
            surface.dirty = true;
            cancelled.push(surface.role.surface().clone());
            clear_repeat |= self.keyboard_surface.as_ref() == Some(surface.role.surface());
        }
        if clear_repeat {
            self.clear_key_repeat();
        }
        for surface in cancelled {
            self.cancel_input_method_composition(&surface);
        }
    }

    fn input_origin(&self, surface: wl_surface::WlSurface, serial: u32) -> Option<InputOrigin> {
        Some(InputOrigin {
            seat: self.seat.clone()?,
            serial,
            surface,
        })
    }

    fn request_launch(&mut self, request: PendingLaunch, origin: Option<&InputOrigin>) {
        let (Some(activation), Some(origin)) = (self.activation.clone(), origin) else {
            self.finish_launch(request, None);
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
            self.finish_launch(request, None);
        }
    }

    fn finish_launch(&mut self, request: PendingLaunch, token: Option<&str>) {
        // the catalog may have been replaced while the token was requested. a
        // changed or removed entry no longer describes what was planned.
        if let Some((id, revision)) = &request.entry
            && self.application_catalog.revision(id) != Some(*revision)
        {
            eprintln!(
                "thing: application {id:?} changed while its launch waited for activation; launch it again"
            );
            return;
        }
        match request.work {
            PendingWork::Spawn(request) => self.spawn_launch(request, token),
            PendingWork::Activate(call) => {
                if let Err(error) = self.activator.send(*call, token.map(str::to_owned)) {
                    eprintln!("thing: {error}");
                }
            }
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
            self.finish_launch(request, None);
        }
    }

    fn reap_children(&mut self) {
        for (pid, error) in self.launcher.reap() {
            eprintln!("thing: cannot reap launched process {pid}: {error}");
        }
    }

    fn invalidate_output(&mut self, output: &wl_output::WlOutput) {
        self.reconcile_custom_pointer(output);
        self.reconcile_range_presses(output);
        self.reconcile_presented_pointer_state(output);
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
            surface.layouts.invalidate_input();
            surface.dirty = true;
        }
        self.reconcile_hover_intent();
    }

    fn reconcile_presented_pointer_state(&mut self, output: &wl_output::WlOutput) {
        for index in 0..self.surfaces.len() {
            if &self.surfaces[index].output != output {
                continue;
            }
            let hovered_is_current = self.surfaces[index]
                .input
                .hovered
                .and_then(|id| self.surfaces[index].layouts.presented_target(id))
                .is_some_and(|target| self.surface_target_is_current(index, &target));
            if self.surfaces[index].input.hovered.is_some() && !hovered_is_current {
                self.surfaces[index].input.hovered = None;
                self.surfaces[index].dirty = true;
            }
            let pressed_is_current =
                self.surfaces[index]
                    .pressed_target
                    .clone()
                    .is_none_or(|target| {
                        self.current_presented_press_target(index, target.target.node())
                            .as_ref()
                            == Some(&target)
                    });
            if !pressed_is_current {
                let surface = &mut self.surfaces[index];
                surface.input.pressed = None;
                surface.pressed_target = None;
                surface.pressed_button = None;
                surface.press_serial = None;
                surface.dirty = true;
            }
        }
    }

    fn range_input(&mut self, surface: usize, target: &ScopedUiTarget, point: Point) {
        if !self.surface_target_is_current(surface, target) {
            return;
        }
        let id = target.target.node();
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
        let Some((bounds, local)) = self.surfaces[surface]
            .layouts
            .presented()
            .and_then(|layout| {
                layout
                    .get(id)
                    .zip(layout.surface_to_local(id, point))
                    .map(|(placement, local)| (placement.local_bounds(), local))
            })
        else {
            return;
        };
        let span = match node.kind {
            Kind::Slider => HorizontalRange::unclamped_inset(bounds, 8.0),
            Kind::Range => HorizontalRange::inset(bounds, node.range_inset),
            _ => unreachable!("range kind checked above"),
        };
        let value = node.slider.value_at(f64::from(span.fraction_at(local.x)));
        self.dispatch(output, id, Event::Change(value), None);
    }

    fn reconcile_range_presses(&mut self, output: &wl_output::WlOutput) {
        let stale: Vec<_> = self
            .surfaces
            .iter()
            .enumerate()
            .filter(|(_, surface)| &surface.output == output)
            .filter_map(|(index, surface)| {
                let target = surface.range_press.as_ref()?;
                let id = target.target.node();
                (self.current_presented_press_target(index, id).as_ref() != Some(target)
                    || !self.configurations.active_press_is_available(output, id))
                .then(|| (surface.role.surface().clone(), target.clone()))
            })
            .collect();
        for (wl, target) in stale {
            let id = target.target.node();
            let notify = self.runtime_owns_active_press(output, &target);
            if let Some(index) = self.index(&wl) {
                let surface = &mut self.surfaces[index];
                if surface.input.pressed == Some(id) {
                    surface.input.pressed = None;
                }
                surface.pressed_button = None;
                surface.pressed_target = None;
                surface.press_serial = None;
                surface.range_press = None;
                surface.dirty = true;
            }
            if notify {
                self.dispatch(output.clone(), id, Event::Pressed(false), None);
            }
        }
    }

    fn text_input_node_and_bounds(
        &self,
        surface: usize,
        id: NodeId,
        snapshot: LayoutSnapshot,
    ) -> Option<(Node, Rect)> {
        let surface = self.surfaces.get(surface)?;
        let node = self
            .configurations
            .surface_root(&surface.output, &surface.key)?
            .find(id)?;
        if node.kind != Kind::TextInput {
            return None;
        }
        let bounds = surface
            .layouts
            .snapshot(snapshot)?
            .semantic_placement(id)?
            .local_bounds();
        Some((node.clone(), bounds))
    }

    fn text_input_node_and_placement(
        &self,
        surface: usize,
        id: NodeId,
        snapshot: LayoutSnapshot,
    ) -> Option<(Node, SemanticPlacement)> {
        let surface = self.surfaces.get(surface)?;
        let node = self
            .configurations
            .surface_root(&surface.output, &surface.key)?
            .find(id)?;
        if node.kind != Kind::TextInput {
            return None;
        }
        let placement = *surface.layouts.snapshot(snapshot)?.semantic_placement(id)?;
        Some((node.clone(), placement))
    }

    fn scoped_focus_target(
        &self,
        output: &wl_output::WlOutput,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        Some(ScopedUiTarget {
            consumer: self.configurations.consumer(output)?.to_owned(),
            target: self.configurations.focus_target(output, id)?,
        })
    }

    fn scoped_target_is_current(
        &self,
        output: &wl_output::WlOutput,
        target: &ScopedUiTarget,
    ) -> bool {
        self.configurations.consumer(output) == Some(target.consumer.as_str())
            && self
                .configurations
                .ui_target_is_current(output, &target.target)
    }

    fn surface_target_is_current(&self, surface: usize, target: &ScopedUiTarget) -> bool {
        let Some(surface) = self.surfaces.get(surface) else {
            return false;
        };
        self.configurations.presented_target_is_current(
            &surface.output,
            &target.consumer,
            surface.root,
            &target.target,
        )
    }

    fn surface_pointer_target_is_current(&self, surface: usize, target: &ScopedUiTarget) -> bool {
        let Some(surface) = self.surfaces.get(surface) else {
            return false;
        };
        self.configurations.presented_pointer_target_is_current(
            &surface.output,
            &target.consumer,
            surface.root,
            &target.target,
        )
    }

    fn surface_scroll_target_is_current(&self, surface: usize, target: &ScopedUiTarget) -> bool {
        let Some(surface) = self.surfaces.get(surface) else {
            return false;
        };
        self.configurations.presented_scroll_target_is_current(
            &surface.output,
            &target.consumer,
            surface.root,
            &target.target,
        )
    }

    fn surface_scroll_view_target_is_current(
        &self,
        surface: usize,
        target: &ScopedUiTarget,
    ) -> bool {
        let Some(surface) = self.surfaces.get(surface) else {
            return false;
        };
        self.configurations.presented_scroll_view_target_is_current(
            &surface.output,
            &target.consumer,
            surface.root,
            &target.target,
        )
    }

    fn scoped_target_identity_is_current_at(
        &self,
        output: &wl_output::WlOutput,
        surface_root: NodeId,
        target: &ScopedUiTarget,
    ) -> bool {
        self.configurations.presented_target_identity_is_current(
            output,
            &target.consumer,
            surface_root,
            &target.target,
        )
    }

    fn scoped_target_is_current_at(
        &self,
        output: &wl_output::WlOutput,
        surface_root: NodeId,
        target: &ScopedUiTarget,
    ) -> bool {
        self.configurations.presented_target_is_current(
            output,
            &target.consumer,
            surface_root,
            &target.target,
        )
    }

    fn runtime_owns_active_press(
        &self,
        output: &wl_output::WlOutput,
        target: &ScopedUiTarget,
    ) -> bool {
        self.configurations.consumer(output) == Some(target.consumer.as_str())
            && self
                .configurations
                .has_active_press(output, target.target.node())
    }

    fn current_presented_target(&self, surface: usize, id: NodeId) -> Option<ScopedUiTarget> {
        let target = self.surfaces.get(surface)?.layouts.presented_target(id)?;
        self.surface_target_is_current(surface, &target)
            .then_some(target)
    }

    fn current_presented_pointer_target(
        &self,
        surface: usize,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        let target = self
            .surfaces
            .get(surface)?
            .layouts
            .presented_pointer_target(id)?;
        self.surface_pointer_target_is_current(surface, &target)
            .then_some(target)
    }

    fn current_presented_press_target(&self, surface: usize, id: NodeId) -> Option<ScopedUiTarget> {
        let target = self
            .surfaces
            .get(surface)?
            .layouts
            .presented_pointer_target(id)?;
        self.surface_target_is_current(surface, &target)
            .then_some(target)
    }

    fn current_presented_scroll_target(
        &self,
        surface: usize,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        let target = self.surfaces.get(surface)?.layouts.presented_target(id)?;
        self.surface_scroll_target_is_current(surface, &target)
            .then_some(target)
    }

    fn current_presented_scroll_view_target(
        &self,
        surface: usize,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        let target = self.surfaces.get(surface)?.layouts.presented_target(id)?;
        self.surface_scroll_view_target_is_current(surface, &target)
            .then_some(target)
    }

    fn custom_pointer_event(
        &self,
        surface: usize,
        target: &ScopedUiTarget,
        phase: PointerPhase,
        button: u32,
        position: (f64, f64),
    ) -> Option<RuntimePointerEvent> {
        if !self.surface_pointer_target_is_current(surface, target) {
            return None;
        }
        let layout = self.surfaces[surface].layouts.presented()?;
        let local = layout.surface_to_local(
            target.target.node(),
            Point {
                x: position.0 as f32,
                y: position.1 as f32,
            },
        )?;
        runtime_pointer_event(local, phase, button, position, &self.modifiers)
    }

    fn finish_pointer_dispatch(
        &mut self,
        output: wl_output::WlOutput,
        consumer: Option<String>,
        result: Result<PointerHandlerOutcome, String>,
        origin: Option<InputOrigin>,
        prepared: Vec<PreparedOutputLayouts>,
    ) -> Option<(bool, bool)> {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(capture) = self.pointer_capture.take() {
                    self.clear_pressed_for_capture(&capture);
                }
                self.finish_dispatch(output, consumer, Err(error), origin, prepared);
                return None;
            }
        };
        if !outcome.active
            && let Some(capture) = self.pointer_capture.take()
        {
            self.clear_pressed_for_capture(&capture);
        }
        let result = (outcome.handled, outcome.active);
        self.finish_dispatch(output, consumer, Ok(outcome.dispatch), origin, prepared)
            .then_some(result)
    }

    fn begin_custom_pointer(
        &mut self,
        surface: usize,
        target: ScopedUiTarget,
        event: RuntimePointerEvent,
        origin: Option<InputOrigin>,
    ) -> Option<(bool, bool)> {
        if self.pointer_capture.is_some() {
            return None;
        }
        let output = self.surfaces.get(surface)?.output.clone();
        let wl = self.surfaces[surface].role.surface().clone();
        let consumer = self.configurations.consumer(&output).map(str::to_owned);
        self.pointer_capture = Some(CustomPointerCapture {
            surface: wl,
            output: output.clone(),
            target: target.clone(),
            button: runtime_pointer_code(event.button),
        });
        let layer_shell_version = self.layer_shell_version;
        let entry = self.current_layout_validation_entry(&output)?;
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result =
            self.configurations
                .begin_pointer_checked(&output, &target.target, event, |surfaces| {
                    preflight.validate(&output, surfaces)
                });
        let prepared = preflight.into_prepared();
        self.finish_pointer_dispatch(output, consumer, result, origin, prepared)
    }

    fn dispatch_custom_pointer(
        &mut self,
        event: RuntimePointerEvent,
        origin: Option<InputOrigin>,
    ) -> Option<(bool, bool)> {
        let capture = self.pointer_capture.clone()?;
        if event.button != runtime_pointer_button(capture.button)? {
            return None;
        }
        if self.configurations.consumer(&capture.output) != Some(capture.target.consumer.as_str())
            || !self
                .configurations
                .active_pointer_matches(&capture.output, &capture.target.target)
        {
            self.pointer_capture = None;
            self.clear_pressed_for_capture(&capture);
            return None;
        }
        if event.phase == PointerPhase::Release {
            self.pointer_capture = None;
        }
        let consumer = Some(capture.target.consumer.clone());
        let layer_shell_version = self.layer_shell_version;
        let entry = self.current_layout_validation_entry(&capture.output)?;
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result =
            self.configurations
                .dispatch_pointer_checked(&capture.output, event, |surfaces| {
                    preflight.validate(&capture.output, surfaces)
                });
        let prepared = preflight.into_prepared();
        self.finish_pointer_dispatch(capture.output, consumer, result, origin, prepared)
    }

    fn cancel_custom_pointer(&mut self) {
        let Some(capture) = self.pointer_capture.take() else {
            return;
        };
        self.clear_pressed_for_capture(&capture);
        if self.configurations.consumer(&capture.output) != Some(capture.target.consumer.as_str())
            || !self
                .configurations
                .active_pointer_matches(&capture.output, &capture.target.target)
        {
            return;
        }
        let consumer = Some(capture.target.consumer.clone());
        let layer_shell_version = self.layer_shell_version;
        let Some(entry) = self.current_layout_validation_entry(&capture.output) else {
            return;
        };
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self
            .configurations
            .cancel_pointer_checked(&capture.output, |surfaces| {
                preflight.validate(&capture.output, surfaces)
            });
        let prepared = preflight.into_prepared();
        self.finish_pointer_dispatch(capture.output, consumer, result, None, prepared);
    }

    fn reconcile_custom_pointer(&mut self, output: &wl_output::WlOutput) {
        let Some(capture) = self
            .pointer_capture
            .as_ref()
            .filter(|capture| &capture.output == output)
            .cloned()
        else {
            return;
        };
        if self.configurations.consumer(output) != Some(capture.target.consumer.as_str())
            || !self
                .configurations
                .active_pointer_matches(output, &capture.target.target)
        {
            self.pointer_capture = None;
            self.clear_pressed_for_capture(&capture);
            return;
        }
        let current = self.index(&capture.surface).is_some_and(|index| {
            self.current_presented_pointer_target(index, capture.target.target.node())
                .as_ref()
                == Some(&capture.target)
        });
        if current {
            return;
        }
        self.pointer_capture = None;
        self.clear_pressed_for_capture(&capture);
        let consumer = Some(capture.target.consumer.clone());
        let layer_shell_version = self.layer_shell_version;
        let Some(entry) = self.current_layout_validation_entry(output) else {
            return;
        };
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self
            .configurations
            .cancel_pointer_checked(output, |surfaces| preflight.validate(output, surfaces));
        let prepared = preflight.into_prepared();
        self.finish_pointer_dispatch(output.clone(), consumer, result, None, prepared);
    }

    fn clear_pressed_for_capture(&mut self, capture: &CustomPointerCapture) {
        let Some(index) = self.index(&capture.surface) else {
            return;
        };
        let matches = self.surfaces[index].pressed_button == Some(capture.button)
            && self.surfaces[index]
                .pressed_target
                .as_ref()
                .is_none_or(|target| target == &capture.target);
        if !matches {
            return;
        }
        let surface = &mut self.surfaces[index];
        surface.input.pressed = None;
        surface.pressed_target = None;
        surface.pressed_button = None;
        surface.press_serial = None;
        surface.dirty = true;
    }

    fn same_surface_target(
        &self,
        surface: usize,
        previous: &ScopedUiTarget,
        next: &ScopedUiTarget,
    ) -> bool {
        previous == next
            || (previous.consumer == next.consumer
                && previous.target.node() == next.target.node()
                && self.surface_target_is_current(surface, previous)
                && self.surface_target_is_current(surface, next))
    }

    fn focus_from_pointer(&mut self, surface: usize, target: Option<ScopedUiTarget>) {
        let output = self.surfaces[surface].output.clone();
        self.cancel_queued_focus(&output);
        let target = target.filter(|target| self.surface_target_is_current(surface, target));
        let next = target.as_ref().map(|target| target.target.node());
        let previous = self.surfaces[surface].input.focused;
        if previous != next {
            let wl = self.surfaces[surface].role.surface().clone();
            self.cancel_input_method_composition(&wl);
            self.surfaces[surface].text_input = None;
        }
        let surface = &mut self.surfaces[surface];
        surface.layouts.clear_input();
        surface.focus_target = target;
        surface.pending_focus = None;
        surface.input.focus_from_pointer(next);
        surface.dirty = true;
    }

    fn focus_from_keyboard(&mut self, surface: usize, target: ScopedUiTarget) -> bool {
        let id = target.target.node();
        let focusable = self.surfaces[surface]
            .layouts
            .input()
            .is_some_and(|layout| layout.focus_order().any(|candidate| candidate == id));
        if !focusable {
            return false;
        }
        let previous = self.surfaces[surface].input.focused;
        if previous != Some(id) {
            self.clear_key_repeat();
            let wl = self.surfaces[surface].role.surface().clone();
            self.cancel_input_method_composition(&wl);
            self.surfaces[surface].text_input = None;
        }
        let surface_state = &mut self.surfaces[surface];
        surface_state.focus_target = Some(target);
        surface_state.pending_focus = None;
        surface_state.input.focus_from_keyboard(id);
        surface_state.dirty = true;
        self.focus_text_input(surface, id, None, false);
        true
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
        let snapshot = if point.is_some() {
            LayoutSnapshot::Presented
        } else {
            LayoutSnapshot::Input
        };
        let local_point = point.and_then(|point| {
            self.surfaces[surface]
                .layouts
                .snapshot(snapshot)?
                .surface_to_local(id, point)
        });
        if point.is_some() && local_point.is_none() {
            return false;
        }
        let Some((node, placement)) = self.text_input_node_and_placement(surface, id, snapshot)
        else {
            self.surfaces[surface].text_input = None;
            return false;
        };
        let bounds = placement.local_bounds();
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
            let editor = TextInputEditor::new(&node, width);
            let cursor_available = input_method_cursor_rectangle(
                &node,
                bounds,
                placement.transform,
                placement.is_invertible(),
                &editor,
            )
            .is_some();
            self.surfaces[surface].text_input = Some(ActiveTextInput {
                id,
                generation,
                editor,
                cursor_available,
            });
        }
        let active = self.surfaces[surface]
            .text_input
            .as_mut()
            .expect("the focused text input was initialized");
        active.editor.prepare(&node, width);
        if let Some(point) = local_point {
            let x = point.x - padding_x;
            let content_height = (bounds.height - 2.0 * padding_y).max(0.0);
            let offset_y = ((content_height - active.editor.layout().height()) / 2.0).max(0.0);
            let y = point.y - padding_y - offset_y;
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
        let id = self.surfaces[surface_index]
            .text_input
            .as_ref()
            .map(|active| active.id)?;
        let interactive = active_text_input_is_current(
            self.configurations.surface_root(
                &self.surfaces[surface_index].output,
                &self.surfaces[surface_index].key,
            ),
            self.surfaces[surface_index].layouts.input(),
            self.surfaces[surface_index].input.focused,
            id,
        );
        let Some((node, placement)) = interactive
            .then(|| self.text_input_node_and_placement(surface_index, id, LayoutSnapshot::Input))
            .flatten()
        else {
            self.surfaces[surface_index].text_input = None;
            self.surfaces[surface_index].dirty = true;
            return None;
        };
        let bounds = placement.local_bounds();
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let width = (bounds.width - 2.0 * padding_x).max(0.0);
        let (cursor_rectangle, availability_changed, composition_cleared) = {
            let active = self.surfaces[surface_index]
                .text_input
                .as_mut()
                .expect("the active text input was checked");
            let composing = active.editor.is_composing();
            active.editor.prepare(&node, width);
            let rectangle = input_method_cursor_rectangle(
                &node,
                placement.local_bounds(),
                placement.transform,
                placement.is_invertible(),
                &active.editor,
            );
            let available = rectangle.is_some();
            let availability_changed = active.cursor_available != available;
            if availability_changed {
                active.editor.cancel_composition();
            }
            active.cursor_available = available;
            (
                rectangle,
                availability_changed,
                composing && !active.editor.is_composing(),
            )
        };
        if availability_changed {
            let Some(generation) = self.allocate_text_input_generation() else {
                self.surfaces[surface_index].text_input = None;
                return None;
            };
            self.surfaces[surface_index]
                .text_input
                .as_mut()
                .expect("the active text input was checked")
                .generation = generation;
            self.surfaces[surface_index].dirty = true;
        }
        let generation = self.surfaces[surface_index]
            .text_input
            .as_ref()
            .expect("the active text input was checked")
            .generation;
        let active = self.surfaces[surface_index]
            .text_input
            .as_ref()
            .expect("the active text input was checked");
        let state = InputMethodTextState {
            generation,
            revision: active.editor.revision(),
            surrounding: active.editor.surrounding(),
            cursor_rectangle,
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
                self.surfaces[surface_index].layouts.input(),
                self.surfaces[surface_index].input.focused,
                id,
            )
        {
            self.surfaces[surface_index].text_input = None;
            self.surfaces[surface_index].dirty = true;
            return InputMethodChangeCause::Other;
        }
        let Some((node, bounds)) =
            self.text_input_node_and_bounds(surface_index, id, LayoutSnapshot::Input)
        else {
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
        let Some((node, bounds)) =
            self.text_input_node_and_bounds(surface_index, id, LayoutSnapshot::Input)
        else {
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
        let Some((node, bounds)) =
            self.text_input_node_and_bounds(surface, id, LayoutSnapshot::Input)
        else {
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

    fn text_clipboard_key(
        &mut self,
        surface: usize,
        action: TextClipboardAction,
        serial: u32,
    ) -> bool {
        let Some(id) = self.surfaces[surface].input.focused else {
            return false;
        };
        let Some(target) = self.surfaces[surface]
            .focus_target
            .clone()
            .filter(|target| target.target.node() == id)
        else {
            return false;
        };
        if !self.surface_target_is_current(surface, &target) {
            return false;
        }
        let output = self.surfaces[surface].output.clone();
        let key = self.surfaces[surface].key.clone();
        if self
            .configurations
            .surface_root(&output, &key)
            .and_then(|root| root.find(id))
            .is_none_or(|node| node.kind != Kind::TextInput)
            || !self.focus_text_input(surface, id, None, false)
        {
            return false;
        }

        self.cancel_pending_text_paste();
        let wl = self.surfaces[surface].role.surface().clone();
        self.cancel_input_method_composition(&wl);
        let generation = self.surfaces[surface]
            .text_input
            .as_ref()
            .expect("the focused text input was initialized")
            .generation;
        if !self.reset_input_method_generation(surface, id, generation) {
            self.reconcile_input_method(InputMethodChangeCause::Other);
            return true;
        }
        let surface_state = &mut self.surfaces[surface];
        reveal_text_input_focus(&mut surface_state.input, &mut surface_state.dirty);
        let active = surface_state
            .text_input
            .as_ref()
            .expect("the focused text input survived its generation reset");
        let generation = active.generation;
        let revision = active.editor.revision();
        let selection = active.editor.selected_text().map(str::to_owned);

        match action {
            TextClipboardAction::Copy => {
                if let Some(text) = selection
                    && let Err(error) = self.focused_clipboard.set_text(text, serial, &self.queue)
                {
                    eprintln!("thing: cannot copy text: {error}");
                }
            }
            TextClipboardAction::Cut => {
                if let Some(text) = selection {
                    match self.focused_clipboard.set_text(text, serial, &self.queue) {
                        Ok(()) => {
                            let origin = self.input_origin(wl.clone(), serial);
                            self.apply_text_clipboard_edit(surface, id, generation, "", origin);
                        }
                        Err(error) => eprintln!("thing: cannot cut text: {error}"),
                    }
                }
            }
            TextClipboardAction::Paste => match self.focused_clipboard.start_paste() {
                Ok(paste) => {
                    self.pending_paste = Some(PendingPaste {
                        id: paste,
                        surface: wl,
                        target,
                        generation,
                        revision,
                    });
                }
                Err(error) => eprintln!("thing: cannot paste text: {error}"),
            },
        }
        self.reconcile_input_method(InputMethodChangeCause::Other);
        true
    }

    fn apply_text_clipboard_edit(
        &mut self,
        surface: usize,
        id: NodeId,
        generation: u64,
        text: &str,
        origin: Option<InputOrigin>,
    ) {
        let output = self.surfaces[surface].output.clone();
        let wl = self.surfaces[surface].role.surface().clone();
        let Some(active) = self.surfaces[surface]
            .text_input
            .as_mut()
            .filter(|active| active.id == id && active.generation == generation)
        else {
            return;
        };
        let checkpoint = active.editor.checkpoint();
        let before_revision = active.editor.revision();
        let before = active.editor.value().into_owned();
        active.editor.replace_selection(text);
        let proposed = active.editor.value().into_owned();
        let editor_changed = active.editor.revision() != before_revision;
        self.surfaces[surface].dirty |= editor_changed || proposed != before;
        if proposed == before {
            return;
        }

        self.dispatch(output, id, Event::TextChange(proposed.clone()), origin);
        let Some(surface) = self.index(&wl) else {
            return;
        };
        let Some((node, bounds)) =
            self.text_input_node_and_bounds(surface, id, LayoutSnapshot::Input)
        else {
            self.surfaces[surface].text_input = None;
            return;
        };
        let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
        let width = (bounds.width - 2.0 * padding_x).max(0.0);
        let Some(active) = self.surfaces[surface]
            .text_input
            .as_mut()
            .filter(|active| active.id == id && active.generation == generation)
        else {
            return;
        };
        let rejected =
            reconcile_controlled_text_edit(&mut active.editor, &node, width, &proposed, checkpoint);
        self.surfaces[surface].dirty = true;
        if rejected {
            self.reset_input_method_generation(surface, id, generation);
        }
    }

    fn remove_popup(&mut self, index: usize, notify_runtime: bool, suppress_hover: bool) {
        let wl = self.surfaces[index].role.surface().clone();
        let captured = self
            .pointer_capture
            .as_ref()
            .is_some_and(|capture| capture.surface == wl);
        if captured {
            if notify_runtime {
                self.cancel_custom_pointer();
            } else {
                self.pointer_capture = None;
            }
        }
        let Some(index) = self.index(&wl) else {
            return;
        };
        let id = self.surfaces[index].root;
        let output = self.surfaces[index].output.clone();
        let surface_root = self.surfaces[index].root;
        let close_target = self.surfaces[index].layouts.presented_target(surface_root);
        let mode = self.surfaces[index].role.popup_mode().cloned();
        let hovered = std::mem::take(&mut self.surfaces[index].hover_path);
        let pressed = notify_runtime
            .then(|| self.surfaces[index].range_press.clone())
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
            if let Some(target) = pressed
                && self.runtime_owns_active_press(&output, &target)
            {
                self.dispatch(
                    output.clone(),
                    target.target.node(),
                    Event::Pressed(false),
                    None,
                );
            }
            for target in hovered {
                if self.scoped_target_identity_is_current_at(&output, surface_root, &target) {
                    self.dispatch(
                        output.clone(),
                        target.target.node(),
                        Event::Hover(false),
                        None,
                    );
                }
            }
            if close_target.as_ref().is_none_or(|target| {
                self.scoped_target_identity_is_current_at(&output, surface_root, target)
            }) {
                self.dispatch(output, id, Event::Close, None);
            }
        }
    }

    fn sync_popup(
        &mut self,
        parent: usize,
        serial: u32,
        activated: Option<ScopedUiTarget>,
        snapshot: LayoutSnapshot,
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
            self.create_popup(
                &parent_wl,
                id,
                PopupAnchorLayout {
                    node: anchor,
                    snapshot,
                },
                PopupMode::Manual,
                Some(serial),
                qh,
            );
            return;
        }

        let Some(activated) =
            activated.filter(|target| self.surface_target_is_current(parent, target))
        else {
            return;
        };
        let anchor = activated.target.node();
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
            self.sync_popup(parent, serial, None, snapshot, qh);
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
            PopupAnchorLayout {
                node: Some(target.key.anchor),
                snapshot,
            },
            PopupMode::Automatic(target.key),
            Some(serial),
            qh,
        );
    }

    fn create_popup(
        &mut self,
        parent_wl: &wl_surface::WlSurface,
        id: NodeId,
        anchor: PopupAnchorLayout,
        mode: PopupMode,
        serial: Option<u32>,
        qh: &QueueHandle<Self>,
    ) -> bool {
        let Some(parent_index) = self.index(parent_wl) else {
            return false;
        };
        let parent = &self.surfaces[parent_index];
        let Role::Panel(layer) = &parent.role else {
            return false;
        };
        let owner = parent.owner();
        let namespace = parent.namespace.clone();
        let parent_scale = parent.scale;
        let anchor_id = anchor.node;
        let anchor = match anchor_id {
            Some(id) => {
                let current = match anchor.snapshot {
                    LayoutSnapshot::Presented => {
                        self.current_presented_target(parent_index, id).is_some()
                    }
                    LayoutSnapshot::Input => self
                        .configurations
                        .surface_root(&owner.output, &owner.key)
                        .and_then(|root| root.find(id))
                        .is_some(),
                };
                if !current {
                    return false;
                }
                let Some(anchor) = parent
                    .layouts
                    .snapshot(anchor.snapshot)
                    .and_then(|layout| layout.semantic_placement(id))
                    .and_then(placement_anchor)
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
                let surface = match surface {
                    Ok(surface) => surface,
                    Err(error) => {
                        self.error = Some(error);
                        return false;
                    }
                };
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
                        PopupAnchorLayout {
                            node: Some(key.anchor),
                            snapshot: LayoutSnapshot::Presented,
                        },
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
        let index = self.surfaces.iter().position(|surface| {
            surface.output == key.owner.output
                && surface.key == key.owner.key
                && matches!(surface.role, Role::Panel(_))
        })?;
        self.current_presented_target(index, key.anchor)?;
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
            .enumerate()
            .find(|surface| {
                surface.1.output == key.owner.output
                    && surface.1.key == key.owner.key
                    && matches!(surface.1.role, Role::Panel(_))
            })
            .is_some_and(|(index, surface)| {
                !surface.dirty
                    && surface
                        .layouts
                        .presented()
                        .is_some_and(|layout| layout.get(key.anchor).is_some())
                    && self.current_presented_target(index, key.anchor).is_some()
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
        for index in 0..self.surfaces.len() {
            if selected.is_some_and(|selected| !selected.contains(&index)) {
                continue;
            }
            let surface = &self.surfaces[index];
            let hit = surface.pointer_position.and_then(|point| {
                surface
                    .layouts
                    .presented()
                    .and_then(|layout| layout.hit_test(point))
            });
            let mut path = hit
                .map(|id| surface.layouts.presented_path(id))
                .unwrap_or_default();
            if surface.pointer_position.is_some() && matches!(surface.role, Role::Popup(_)) {
                let root = surface.root;
                if path
                    .first()
                    .is_none_or(|target| target.target.node() != root)
                    && let Some(target) = surface.layouts.presented_target(root)
                {
                    path.insert(0, target);
                }
            }
            let current_hit = hit
                .and_then(|id| surface.layouts.presented_target(id))
                .filter(|target| self.surface_target_is_current(index, target))
                .map(|target| target.target.node());
            let previous = &surface.hover_path;
            for target in previous.iter().filter(|target| {
                !path
                    .iter()
                    .any(|next| self.same_surface_target(index, target, next))
            }) {
                leaving.push((surface.output.clone(), surface.root, target.clone()));
            }
            for target in path.iter().filter(|target| {
                !previous
                    .iter()
                    .any(|old| self.same_surface_target(index, old, target))
            }) {
                entering.push((surface.output.clone(), surface.root, target.clone()));
            }
            let surface = &mut self.surfaces[index];
            if surface.input.hovered != current_hit {
                surface.input.hovered = current_hit;
                surface.dirty = true;
            }
            surface.hover_path = path;
        }
        for (output, root, target) in leaving {
            if self.scoped_target_identity_is_current_at(&output, root, &target) {
                self.dispatch(output, target.target.node(), Event::Hover(false), None);
            }
        }
        for (output, root, target) in entering {
            if self.scoped_target_is_current_at(&output, root, &target) {
                self.dispatch(output, target.target.node(), Event::Hover(true), None);
            }
        }
        self.reconcile_hover_intent();
    }

    fn reconcile_hover_intent(&mut self) {
        let pointer = self
            .surfaces
            .iter()
            .enumerate()
            .find_map(|(index, surface)| {
                surface.pointer_position?;
                if let Some(PopupMode::Hover(key)) = surface.role.popup_mode() {
                    return self.current_hover_target(key);
                }
                if !matches!(surface.role, Role::Panel(_)) {
                    return None;
                }
                surface.hover_path.iter().rev().find_map(|anchor| {
                    if !self.surface_target_is_current(index, anchor) {
                        return None;
                    }
                    automatic_target(
                        self.configurations
                            .surface_root(&surface.output, &surface.key)?,
                        surface.owner(),
                        anchor.target.node(),
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
        self.cancel_custom_pointer();
        let mut leaving = Vec::new();
        let mut pressed = Vec::new();
        for surface in &mut self.surfaces {
            leaving.extend(
                std::mem::take(&mut surface.hover_path)
                    .into_iter()
                    .map(|target| (surface.output.clone(), surface.root, target)),
            );
            surface.pointer_position = None;
            pressed.extend(
                surface
                    .range_press
                    .take()
                    .map(|target| (surface.output.clone(), surface.root, target)),
            );
            surface.input.hovered = None;
            surface.input.pressed = None;
            surface.press_serial = None;
            surface.pressed_button = None;
            surface.pressed_target = None;
            surface.dirty = true;
        }
        for (output, _, target) in pressed {
            if self.runtime_owns_active_press(&output, &target) {
                self.dispatch(output, target.target.node(), Event::Pressed(false), None);
            }
        }
        for (output, root, target) in leaving {
            if self.scoped_target_identity_is_current_at(&output, root, &target) {
                self.dispatch(output, target.target.node(), Event::Hover(false), None);
            }
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

    fn schedule_autofocus(&mut self, index: usize) {
        let should_autofocus = {
            let surface = &mut self.surfaces[index];
            let pending = surface.pending_focus.is_some()
                || surface.ui_actions.iter().any(HostUiAction::is_focus);
            claim_autofocus(
                &mut surface.autofocus_pending,
                surface.input.focused,
                pending,
            )
        };
        if !should_autofocus {
            return;
        }
        let output = self.surfaces[index].output.clone();
        let root = self.surfaces[index].root;
        if let (Some(consumer), Some(target)) = (
            self.configurations.consumer(&output),
            self.configurations.autofocus_target(&output, root),
        ) {
            self.surfaces[index].pending_focus = Some(ScopedUiTarget {
                consumer: consumer.to_owned(),
                target,
            });
            self.surfaces[index].dirty = true;
        }
    }

    fn set_keyboard_surface(&mut self, next: Option<wl_surface::WlSurface>) {
        self.clear_key_repeat();
        let previous = std::mem::replace(&mut self.keyboard_surface, next);
        if keyboard_surface_changed(previous.as_ref(), self.keyboard_surface.as_ref()) {
            self.cancel_pending_text_paste();
        }
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
        if let Some(index) = self
            .keyboard_surface
            .as_ref()
            .and_then(|surface| self.index(surface))
        {
            self.schedule_autofocus(index);
            self.surfaces[index].dirty = true;
        }
        self.apply_pending_ui_actions_now();
        self.prepare_invalidated_input_layouts_now();
        self.reconcile_layout_focus();
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

    fn keyboard_focus_owner(&self) -> Option<(wl_surface::WlSurface, Option<NodeId>)> {
        let surface = self.keyboard_surface.clone()?;
        let focused = self
            .index(&surface)
            .and_then(|index| self.surfaces[index].input.focused);
        Some((surface, focused))
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

    fn dispatch_root_key(
        &mut self,
        surface: usize,
        input: KeyInput,
        origin: Option<InputOrigin>,
    ) -> RootKeyDispatch {
        let output = self.surfaces[surface].output.clone();
        let root = self.surfaces[surface].root;
        let consumer = self.configurations.consumer(&output).map(str::to_owned);
        let layer_shell_version = self.layer_shell_version;
        let Some(entry) = self.current_layout_validation_entry(&output) else {
            return RootKeyDispatch::Missing;
        };
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self
            .configurations
            .dispatch_key_checked(&output, root, input, |surfaces| {
                preflight.validate(&output, surfaces)
            });
        let prepared = preflight.into_prepared();
        let outcome = match result {
            Ok(Some(outcome)) => outcome,
            Ok(None) => return RootKeyDispatch::Missing,
            Err(error) => {
                self.finish_dispatch(output, consumer, Err(error), origin, prepared);
                return RootKeyDispatch::Failed;
            }
        };
        let handled = outcome.handled;
        if !self.finish_dispatch(output, consumer, Ok(outcome.dispatch), origin, prepared) {
            return RootKeyDispatch::Failed;
        }
        if handled {
            RootKeyDispatch::Handled
        } else {
            RootKeyDispatch::Unhandled
        }
    }

    fn key(&mut self, event: KeyEvent, serial: u32, direct: bool, qh: &QueueHandle<Self>) -> bool {
        let Some(index) = self.keyboard_surface.as_ref().and_then(|wl| self.index(wl)) else {
            return false;
        };
        let event_surface = self.surfaces[index].role.surface().clone();
        let origin = direct_input_serial(direct, serial)
            .and_then(|serial| self.input_origin(event_surface.clone(), serial));
        let root_dispatch = self.dispatch_root_key(
            index,
            root_key_input(&event, self.modifiers, !direct),
            origin,
        );
        if direct
            && matches!(
                root_dispatch,
                RootKeyDispatch::Unhandled | RootKeyDispatch::Handled
            )
            && let Some(index) = self.index(&event_surface)
        {
            self.sync_popup(index, serial, None, LayoutSnapshot::Input, qh);
        }
        match root_dispatch {
            RootKeyDispatch::Handled => return true,
            RootKeyDispatch::Failed => return false,
            RootKeyDispatch::Missing | RootKeyDispatch::Unhandled => {}
        }
        let Some(index) = self.index(&event_surface) else {
            return false;
        };
        if let Some(action) = text_clipboard_action(&event, self.modifiers, direct)
            && self.text_clipboard_key(index, action, serial)
        {
            return false;
        }
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
        if direct && (event.keysym == Keysym::Tab || event.keysym == Keysym::ISO_Left_Tab) {
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
                let focused = order[next];
                let output = self.surfaces[index].output.clone();
                self.cancel_queued_focus(&output);
                if let Some(target) = self.scoped_focus_target(&output, focused) {
                    self.surfaces[index].pending_focus = Some(target);
                    self.surfaces[index].dirty = true;
                }
            }
            self.apply_pending_ui_actions_now();
            self.reconcile_hover_intent();
            self.reconcile_input_method(InputMethodChangeCause::Other);
            return false;
        }
        let surface = &mut self.surfaces[index];
        let Some(id) = surface.input.focused else {
            return false;
        };
        if !focused_widget_is_interactive(surface.layouts.input(), id) {
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
                let activated = self.surfaces[index]
                    .focus_target
                    .clone()
                    .filter(|target| target.target.node() == id);
                self.surfaces[index].input.reveal_keyboard_focus();
                self.surfaces[index].dirty = true;
                self.dispatch(output, id, Event::Click, origin);
                if let Some(index) = self.index(&event_surface) {
                    self.sync_popup(index, serial, activated, LayoutSnapshot::Input, qh);
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

fn text_clipboard_action(
    event: &KeyEvent,
    modifiers: Modifiers,
    direct: bool,
) -> Option<TextClipboardAction> {
    if !direct || !modifiers.ctrl || modifiers.alt || modifiers.logo {
        return None;
    }
    match event.keysym {
        Keysym::c | Keysym::C => Some(TextClipboardAction::Copy),
        Keysym::x | Keysym::X => Some(TextClipboardAction::Cut),
        Keysym::v | Keysym::V => Some(TextClipboardAction::Paste),
        _ => None,
    }
}

fn pending_paste_is_current(
    surface_has_keyboard_focus: bool,
    target_is_current: bool,
    focused: Option<NodeId>,
    expected: (NodeId, u64, u64),
    active: Option<(NodeId, u64, u64)>,
) -> bool {
    surface_has_keyboard_focus
        && target_is_current
        && focused == Some(expected.0)
        && active == Some(expected)
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
    transform: thing_ui::Affine,
    invertible: bool,
    editor: &TextInputEditor,
) -> Option<InputMethodCursorRectangle> {
    if !invertible {
        return None;
    }
    let padding_x = node.style.padding_x.unwrap_or(node.style.padding);
    let padding_y = node.style.padding_y.unwrap_or(node.style.padding);
    let content_height = (bounds.height - 2.0 * padding_y).max(0.0);
    let offset_y = ((content_height - editor.layout().height()) / 2.0).max(0.0);
    let area = editor.ime_cursor_area();
    let local = Rect {
        x: padding_x + editor.text_offset_x() + area.x0 as f32,
        y: padding_y + offset_y + area.y0 as f32,
        width: (area.x1 - area.x0) as f32,
        height: (area.y1 - area.y0) as f32,
    };
    if local.width <= 0.0 || local.height <= 0.0 {
        return None;
    }
    let surface = transform.rect_aabb(local).ok()?;
    if surface.width <= 0.0 || surface.height <= 0.0 {
        return None;
    }
    let right = surface.x + surface.width;
    let bottom = surface.y + surface.height;
    if [surface.x, surface.y, right, bottom]
        .into_iter()
        .any(|value| !value.is_finite() || value < i32::MIN as f32 || value > i32::MAX as f32)
    {
        return None;
    }
    let x = floor_i32(surface.x);
    let y = floor_i32(surface.y);
    Some(InputMethodCursorRectangle {
        x,
        y,
        width: ceil_i32(right).saturating_sub(x).max(1),
        height: ceil_i32(bottom).saturating_sub(y).max(1),
    })
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

fn claim_autofocus(available: &mut bool, focused: Option<NodeId>, has_pending_focus: bool) -> bool {
    let claim = *available && focused.is_none() && !has_pending_focus;
    *available = false;
    claim
}

fn paint_interaction(mut input: Interaction, surface_has_keyboard_focus: bool) -> Interaction {
    if !surface_has_keyboard_focus {
        input.hide_keyboard_focus();
    }
    input
}

fn keyboard_focus_transition_affects<T: PartialEq>(
    previous: Option<&T>,
    next: Option<&T>,
    candidate: &T,
) -> bool {
    previous == Some(candidate) || next == Some(candidate)
}

fn keyboard_surface_changed<T: PartialEq>(previous: Option<&T>, next: Option<&T>) -> bool {
    previous != next
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
    layout.is_some_and(|layout| layout.focus_order().any(|candidate| candidate == id))
}

fn placement_anchor(placement: &SemanticPlacement) -> Option<Rect> {
    let anchor = placement.border_aabb;
    (placement.is_invertible()
        && anchor.width.is_finite()
        && anchor.height.is_finite()
        && anchor.width > 0.0
        && anchor.height > 0.0)
        .then_some(anchor)
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

fn prepare_candidate_layouts(
    render: &mut Render,
    surfaces: &[SurfaceTree],
    output: &OutputContext,
    current: &[LayoutSurfaceState],
) -> Result<Vec<PreparedSurfaceLayout>, String> {
    let mut prepared = Vec::new();
    for surface in surfaces {
        let panel = current.iter().find(|candidate| {
            candidate.key == surface.key
                && candidate.role == LayoutSurfaceRole::Panel
                && candidate.root == surface.root.id
        });
        let prospective = panel_size(
            panel::geometry(&surface.root, None),
            (output.width, output.height),
        );
        let (size, scroll, focused) = panel.map_or_else(
            || (prospective, ScrollState::default(), None),
            |panel| (panel.size, panel.scroll.clone(), panel.focused),
        );
        let scale = panel.map_or(output.scale, |panel| panel.preferred_scale);
        prepared.push(prepare_surface_layout(
            render,
            surface.key.clone(),
            &surface.root,
            size,
            scroll,
            focused,
            scale,
        )?);
        if size != prospective {
            prepare_surface_layout(
                render,
                surface.key.clone(),
                &surface.root,
                prospective,
                ScrollState::default(),
                None,
                scale,
            )?;
        }

        let mut popups = Vec::new();
        collect_popup_roots(&surface.root, &mut popups);
        for popup_root in popups {
            let existing = current.iter().find(|candidate| {
                candidate.key == surface.key
                    && candidate.role == LayoutSurfaceRole::Popup
                    && candidate.root == popup_root.id
            });
            let prospective = popup::logical_size(popup_root);
            let (size, scroll, focused) = existing.map_or_else(
                || (prospective, ScrollState::default(), None),
                |popup| (popup.size, popup.scroll.clone(), popup.focused),
            );
            let scale = existing.map_or(output.scale, |popup| popup.preferred_scale);
            prepared.push(prepare_surface_layout(
                render,
                surface.key.clone(),
                popup_root,
                size,
                scroll,
                focused,
                scale,
            )?);
            if size != prospective {
                prepare_surface_layout(
                    render,
                    surface.key.clone(),
                    popup_root,
                    prospective,
                    ScrollState::default(),
                    None,
                    scale,
                )?;
            }
        }
    }
    Ok(prepared)
}

fn prepare_surface_layout(
    render: &mut Render,
    key: SurfaceKey,
    root: &Node,
    size: (u32, u32),
    mut scroll: ScrollState,
    focused: Option<NodeId>,
    scale: Scale,
) -> Result<PreparedSurfaceLayout, String> {
    let logical_size = Size {
        width: size.0 as f32,
        height: size.1 as f32,
    };
    let mut layout = render
        .layout(root, logical_size, &mut scroll)
        .map_err(|error| format!("cannot lay out surface {:?}: {error}", root.id))?;
    if focused.is_some_and(|id| layout.reveal_y(id, &mut scroll)) {
        layout = render
            .layout(root, logical_size, &mut scroll)
            .map_err(|error| format!("cannot reveal focused widget: {error}"))?;
    }
    let physical = (
        scale.physical_size(size.0).unwrap_or(0),
        scale.physical_size(size.1).unwrap_or(0),
    );
    thing_ui::validate_shadows(root, &layout, physical, scale.factor())?;
    Ok(PreparedSurfaceLayout {
        key,
        root: root.clone(),
        size,
        scale,
        layout,
        scroll,
    })
}

fn collect_popup_roots<'a>(node: &'a Node, popups: &mut Vec<&'a Node>) {
    if node.kind == Kind::Popup {
        popups.push(node);
    }
    for child in &node.children {
        collect_popup_roots(child, popups);
    }
}

fn validate_surface_capabilities(
    surfaces: &[SurfaceTree],
    layer_shell_version: u32,
    idle_inhibit_available: bool,
) -> Result<(), String> {
    idle_inhibit::validate(surfaces, idle_inhibit_available)?;
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

fn send_managed_process_action(
    manager: Option<&ProcessManager>,
    key: &str,
    action: ManagedProcessAction,
) -> Result<(), String> {
    let manager = manager.ok_or_else(|| "managed process service is unavailable".to_owned())?;
    match action {
        ManagedProcessAction::Start { argv, cwd } => manager.start_process(key, argv, cwd),
        ManagedProcessAction::Stop => manager.stop_process(key),
        ManagedProcessAction::Refresh => manager.refresh_process(key),
        ManagedProcessAction::Cancel => manager.cancel_process(key),
    }
    .map_err(|error| format!("managed process {key:?}: {error}"))
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

#[cfg(test)]
mod output_tests {
    use std::{
        fs,
        time::{Duration, Instant},
    };

    use super::{
        App, ManagedProcessScopeChange, PanelState, arm_managed_processes,
        configure_managed_processes, configured_panel_size, earliest_deadline,
        fallback_logical_size, find_automatic_popup, find_manual_popup, interactive_path,
        panel_size, prepare_and_commit_managed_processes, project_managed_processes,
        release_system_tray_consumer, send_managed_process_action, send_system_tray_action,
        send_system_tray_menu_action, valid_dimensions, validate_surface_capabilities,
    };
    use crate::{configuration::Configurations, panel};
    use tempfile::tempdir;
    use thing_core::{KeyboardMode, Kind, Node, NodeId, PanelEdge, PanelMargins, Scale};
    use thing_runtime::{
        Event, ManagedProcessAction, ManagedProcessDefinition, ManagedProcessShutdownSignal,
        ManagedProcessStatus, OutputContext, ServiceSnapshot, SurfaceKey, SurfaceTree,
        SystemTrayAction, SystemTrayMenuAction,
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
    fn empty_process_scope_does_not_start_the_optional_worker() {
        let event_loop = calloop::EventLoop::<App>::try_new().unwrap();
        let mut manager = None;

        configure_managed_processes(
            &mut manager,
            &event_loop.handle(),
            &[],
            ManagedProcessScopeChange::Replace,
        )
        .unwrap();

        assert!(manager.is_none());
        assert_eq!(
            send_managed_process_action(None, "query", ManagedProcessAction::Stop),
            Err("managed process service is unavailable".into())
        );
    }

    #[test]
    fn process_preflight_reprojects_a_preserved_state_change() {
        let event_loop = calloop::EventLoop::<App>::try_new().unwrap();
        let mut manager = None;
        let definitions = vec![ManagedProcessDefinition {
            key: "query".into(),
            shutdown_signal: ManagedProcessShutdownSignal::Terminate,
            command: None,
        }];
        configure_managed_processes(
            &mut manager,
            &event_loop.handle(),
            &definitions,
            ManagedProcessScopeChange::Replace,
        )
        .unwrap();
        let projected = project_managed_processes(
            manager.as_ref(),
            &definitions,
            ManagedProcessScopeChange::Reconcile,
        )
        .unwrap();
        assert_eq!(projected.states[0].status, ManagedProcessStatus::Idle);
        manager
            .as_ref()
            .unwrap()
            .start_process(
                "query",
                vec!["/thing/preflight-fixture-does-not-exist".into()],
                None,
            )
            .unwrap();

        let mut applied = Vec::new();
        let activation = prepare_and_commit_managed_processes(
            &mut manager,
            &event_loop.handle(),
            &definitions,
            ManagedProcessScopeChange::Reconcile,
            projected,
            |snapshot| {
                applied.push(snapshot);
                Ok(())
            },
        )
        .unwrap();
        arm_managed_processes(manager.as_ref(), activation);

        assert!(!applied.is_empty());
        assert_ne!(
            applied.last().unwrap().states[0].status,
            ManagedProcessStatus::Idle
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

        assert!(validate_surface_capabilities(&surfaces(root.clone()), 3, false).is_err());
        assert!(validate_surface_capabilities(&surfaces(root.clone()), 4, false).is_ok());
        root.panel.keyboard_mode = KeyboardMode::None;
        assert!(validate_surface_capabilities(&surfaces(root), 1, false).is_ok());
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
        let validate = |surfaces: &[SurfaceTree]| validate_surface_capabilities(surfaces, 3, false);

        let error = configurations
            .dispatch_checked(&1, button, Event::Click, validate)
            .unwrap_err();
        assert!(error.contains("on_demand"), "{error}");
        assert_eq!(
            configurations.root(&1).unwrap().panel.keyboard_mode,
            KeyboardMode::None
        );

        let outcome = configurations
            .dispatch_checked(&1, button, Event::Click, validate)
            .unwrap();
        assert!(!outcome.changed);
        assert!(outcome.actions.is_empty());
        assert!(outcome.ui_actions.is_empty());
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
        if let Some(index) = self.index(wl)
            && self.surfaces[index].fractional.is_none()
        {
            let scale = Scale::from_120ths(scale.clamp(1, 16) as u32 * 120).unwrap_or_default();
            self.set_surface_scale(index, scale);
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
        let entry = self.layout_validation_entry(output.clone(), context.clone());
        let preflight = LayoutPreflight::new(
            &mut self.render,
            layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self
            .configurations
            .set_output_checked(&output, context, |surfaces| {
                preflight.validate(&output, surfaces)
            });
        let prepared = preflight.into_prepared();
        self.update_watch();
        match result {
            Ok(changed) => {
                if changed {
                    self.invalidate_output(&output);
                } else {
                    self.reconcile_custom_pointer(&output);
                }
                self.install_prepared_layouts(prepared);
                self.reconcile_published_configuration();
            }
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
        self.pending_ui_actions
            .retain(|pending| pending.output != output);
        let managed_processes = &mut self.managed_processes;
        let loop_handle = self.loop_handle.clone();
        if let Some(removed) = self
            .configurations
            .remove_with_processes(&output, |definitions| {
                configure_managed_processes(
                    managed_processes,
                    &loop_handle,
                    definitions,
                    ManagedProcessScopeChange::Reconcile,
                )
            })
        {
            let RemovedConfiguration {
                consumer,
                process_reconciliation,
            } = removed;
            if let Err(error) = release_system_tray_consumer(self.system_tray.as_ref(), &consumer) {
                eprintln!("thing: {error}");
            }
            if let Err(error) = process_reconciliation {
                self.error = Some(format!("cannot remove output configuration: {error}"));
            }
            self.reconcile_sleep();
            self.reconcile_idle_watches();
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
            let size = {
                let surface = &self.surfaces[index];
                match resolved_size {
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
                }
            };
            let initial = !self.surfaces[index].configured;
            let prepared = match self.prepare_configured_surface_layout(index, size) {
                Ok(prepared) => prepared,
                Err(error) => {
                    eprintln!("thing: cannot apply panel configuration: {error}");
                    if initial {
                        let owner = self.surfaces[index].owner();
                        self.remove_panel(&owner, false);
                    }
                    return;
                }
            };
            {
                let surface = &mut self.surfaces[index];
                if surface.size != size {
                    surface.layouts.invalidate_input();
                }
                surface.size = size;
                surface.scale = prepared.scale;
                surface.prepared = Some(prepared);
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
            let size = (config.width.max(1) as u32, config.height.max(1) as u32);
            let initial = !self.surfaces[index].configured;
            let prepared = match self.prepare_configured_surface_layout(index, size) {
                Ok(prepared) => prepared,
                Err(error) => {
                    eprintln!("thing: cannot apply popup configuration: {error}");
                    if initial {
                        self.remove_popup(index, false, false);
                    }
                    return;
                }
            };
            let surface = &mut self.surfaces[index];
            if surface.size != size {
                surface.layouts.invalidate_input();
                surface.size = size;
            }
            surface.scale = prepared.scale;
            surface.prepared = Some(prepared);
            surface.configured = true;
            // position-only configurations also need a commit to move the popup.
            surface.dirty = true;
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
        let attached = self.seat.is_none();
        self.seat = Some(seat.clone());
        if attached {
            self.reconcile_idle_watches();
        }
        if let Some(clipboard) = &mut self.clipboard {
            clipboard.attach_seat(&seat, qh);
        }
        self.focused_clipboard.attach_seat(&seat, qh);
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
            self.cancel_all_queued_focus();
            self.clear_keyboard_focus();
            self.input_method.detach_seat();
            self.reconcile_hover_intent();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.as_ref() == Some(&seat) {
            self.seat = None;
            // watches armed on the seat resume while their configuration is current.
            self.reconcile_idle_watches();
            if let Some(clipboard) = &mut self.clipboard {
                clipboard.detach_seat();
            }
            self.focused_clipboard.detach_seat();
            self.pending_paste = None;
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
            self.repeat.settings = None;
            self.keyboard_keymap = None;
            self.cancel_all_queued_focus();
            self.clear_keyboard_focus();
            self.input_method.detach_seat();
            self.clear_pointer_state(false);
            // pointer and popup cleanup handlers may queue new focus requests.
            self.cancel_all_queued_focus();
            for surface in &mut self.surfaces {
                surface.input = Interaction::default();
                surface.focus_target = None;
                surface.pending_focus = None;
                surface.text_input = None;
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
            if self.pointer_capture.as_ref().is_some_and(|capture| {
                capture.surface != event.surface
                    && matches!(
                        event.kind,
                        PointerEventKind::Enter { .. }
                            | PointerEventKind::Motion { .. }
                            | PointerEventKind::Release { .. }
                    )
            }) {
                self.cancel_custom_pointer();
            }
            let Some(index) = self.index(&event.surface) else {
                continue;
            };
            let point = Point {
                x: event.position.0 as f32,
                y: event.position.1 as f32,
            };
            let hit_id = self.surfaces[index]
                .layouts
                .presented()
                .and_then(|layout| layout.hit_test(point));
            let semantic_hit = hit_id.and_then(|id| self.current_presented_target(index, id));
            let pointer_hit_id = self.surfaces[index]
                .layouts
                .presented()
                .and_then(|layout| layout.pointer_event_target(point));
            let custom_hit =
                pointer_hit_id.and_then(|id| self.current_presented_pointer_target(index, id));
            let semantic_id = semantic_hit.as_ref().map(|target| target.target.node());
            let custom_id = custom_hit.as_ref().map(|target| target.target.node());
            let hit = semantic_pointer_target(semantic_id, custom_id).and(semantic_hit);
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.surfaces[index].pointer_position = Some(point);
                    let event_surface = event.surface.clone();
                    if let Some(capture) = self
                        .pointer_capture
                        .as_ref()
                        .filter(|capture| capture.surface == event.surface)
                        .cloned()
                    {
                        if let Some(input) = self.custom_pointer_event(
                            index,
                            &capture.target,
                            PointerPhase::Move,
                            capture.button,
                            event.position,
                        ) {
                            self.dispatch_custom_pointer(input, None);
                        } else {
                            self.reconcile_custom_pointer(&capture.output);
                        }
                    }
                    let Some(index) = self.index(&event_surface) else {
                        continue;
                    };
                    if self.surfaces[index].pressed_button == Some(0x110)
                        && let Some(target) = self.surfaces[index].pressed_target.clone()
                    {
                        let output = self.surfaces[index].output.clone();
                        let id = target.target.node();
                        self.range_input(index, &target, point);
                        self.reconcile_range_presses(&output);
                        if let Some(index) = self.index(&event_surface)
                            && self.surface_target_is_current(index, &target)
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
                        && self.surfaces[index].range_press.is_none()
                        && self.surfaces[index].pressed_button.is_none()
                        && self.pointer_capture.is_none() =>
                {
                    self.clear_key_repeat();
                    let event_surface = self.surfaces[index].role.surface().clone();
                    let surface = &mut self.surfaces[index];
                    surface.pointer_position = Some(point);
                    surface.input.pressed = hit.as_ref().map(|target| target.target.node());
                    surface.pressed_target = hit.clone();
                    surface.pressed_button = Some(button);
                    surface.press_serial = Some(serial);
                    surface.dirty = true;
                    self.focus_from_pointer(index, hit.clone());
                    if let Some(target) = custom_hit
                        && let Some(input) = self.custom_pointer_event(
                            index,
                            &target,
                            PointerPhase::Press,
                            button,
                            event.position,
                        )
                    {
                        let origin = self.input_origin(event_surface.clone(), serial);
                        self.begin_custom_pointer(index, target, input, origin);
                    }
                    let Some(index) = self.index(&event_surface) else {
                        continue;
                    };
                    let pressed = (button == 0x110)
                        .then_some(hit)
                        .flatten()
                        .filter(|target| self.surface_target_is_current(index, target));
                    if let Some(target) = pressed {
                        let id = target.target.node();
                        let output = self.surfaces[index].output.clone();
                        let key = self.surfaces[index].key.clone();
                        let kind = self
                            .configurations
                            .surface_root(&output, &key)
                            .and_then(|root| root.find(id))
                            .map(|node| node.kind);
                        if kind == Some(Kind::Range) {
                            self.surfaces[index].range_press = Some(target.clone());
                            self.dispatch(output.clone(), id, Event::Pressed(true), None);
                            self.reconcile_range_presses(&output);
                        }
                        if (kind == Some(Kind::Slider)
                            || (kind == Some(Kind::Range)
                                && self.configurations.has_active_press(&output, id)))
                            && let Some(index) = self.index(&event_surface)
                        {
                            self.range_input(index, &target, point);
                            self.reconcile_range_presses(&output);
                        }
                        if let Some(index) = self.index(&event_surface)
                            && self.surface_target_is_current(index, &target)
                        {
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
                } if self.surfaces[index].pressed_button == Some(button)
                    || self.pointer_capture.as_ref().is_some_and(|capture| {
                        capture.surface == event.surface && capture.button == button
                    }) =>
                {
                    let custom = self
                        .pointer_capture
                        .as_ref()
                        .filter(|capture| {
                            capture.surface == event.surface && capture.button == button
                        })
                        .cloned();
                    let custom_event = custom.as_ref().and_then(|capture| {
                        self.custom_pointer_event(
                            index,
                            &capture.target,
                            PointerPhase::Release,
                            button,
                            event.position,
                        )
                    });
                    self.surfaces[index].pressed_button = None;
                    self.surfaces[index].pointer_position = Some(point);
                    self.surfaces[index].input.pressed = None;
                    let pressed = self.surfaces[index].pressed_target.take();
                    let range_press = self.surfaces[index].range_press.take();
                    let popup_serial = self.surfaces[index].press_serial.take();
                    let output = self.surfaces[index].output.clone();
                    let event_surface = self.surfaces[index].role.surface().clone();
                    self.surfaces[index].dirty = true;
                    if let Some(target) = range_press {
                        let origin = direct_input_serial(true, release_serial)
                            .and_then(|serial| self.input_origin(event_surface.clone(), serial));
                        if self.runtime_owns_active_press(&output, &target) {
                            self.dispatch(
                                output,
                                target.target.node(),
                                Event::Pressed(false),
                                origin,
                            );
                        }
                        continue;
                    }
                    let handled = if let Some(custom) = custom {
                        let Some(input) = custom_event else {
                            self.reconcile_custom_pointer(&custom.output);
                            continue;
                        };
                        let origin = self.input_origin(event_surface.clone(), release_serial);
                        self.dispatch_custom_pointer(input, origin)
                            .is_none_or(|(handled, _)| handled)
                    } else {
                        false
                    };
                    if handled {
                        continue;
                    }
                    let Some(index) = self.index(&event_surface) else {
                        continue;
                    };
                    if let Some(target) = pressed.filter(|pressed| {
                        hit.as_ref().is_some_and(|hit| {
                            self.same_surface_target(index, pressed, hit)
                                && self.surface_target_is_current(index, pressed)
                        })
                    }) {
                        let id = target.target.node();
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
                            self.sync_popup(
                                index,
                                serial,
                                (button == 0x110).then_some(target),
                                LayoutSnapshot::Presented,
                                qh,
                            );
                        }
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    source,
                    ..
                } => {
                    self.surfaces[index].pointer_position = Some(point);
                    let x = scroll_steps(horizontal);
                    let y = scroll_steps(vertical);
                    if x == 0.0 && y == 0.0 {
                        continue;
                    }
                    let output = self.surfaces[index].output.clone();
                    let event_surface = self.surfaces[index].role.surface().clone();
                    if let Some(route) =
                        self.capture_scroll_route(index, point, x, y, event.position, source)
                    {
                        self.dispatch_scroll_route(output, &event_surface, route, y);
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

fn runtime_scroll_input(
    local: Point,
    x: f64,
    y: f64,
    position: (f64, f64),
    modifiers: &Modifiers,
    source: Option<wl_pointer::AxisSource>,
) -> Option<RuntimeScrollInput> {
    let input = RuntimeScrollInput {
        x,
        y,
        local_x: f64::from(local.x),
        local_y: f64::from(local.y),
        surface_x: position.0,
        surface_y: position.1,
        shift: modifiers.shift,
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        logo: modifiers.logo,
        source: match source {
            Some(wl_pointer::AxisSource::Wheel) => RuntimeScrollSource::Wheel,
            Some(wl_pointer::AxisSource::Finger) => RuntimeScrollSource::Finger,
            Some(wl_pointer::AxisSource::Continuous) => RuntimeScrollSource::Continuous,
            Some(wl_pointer::AxisSource::WheelTilt) => RuntimeScrollSource::WheelTilt,
            _ => RuntimeScrollSource::Unknown,
        },
    };
    [
        input.x,
        input.y,
        input.local_x,
        input.local_y,
        input.surface_x,
        input.surface_y,
    ]
    .into_iter()
    .all(f64::is_finite)
    .then_some(input)
}

fn runtime_pointer_button(button: u32) -> Option<RuntimePointerButton> {
    match button {
        0x110 => Some(RuntimePointerButton::Primary),
        0x111 => Some(RuntimePointerButton::Secondary),
        0x112 => Some(RuntimePointerButton::Middle),
        _ => None,
    }
}

fn runtime_pointer_event(
    local: Point,
    phase: PointerPhase,
    button: u32,
    position: (f64, f64),
    modifiers: &Modifiers,
) -> Option<RuntimePointerEvent> {
    Some(RuntimePointerEvent {
        phase,
        button: runtime_pointer_button(button)?,
        x: f64::from(local.x),
        y: f64::from(local.y),
        surface_x: position.0,
        surface_y: position.1,
        shift: modifiers.shift,
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        logo: modifiers.logo,
    })
}

fn semantic_pointer_target(semantic: Option<NodeId>, custom: Option<NodeId>) -> Option<NodeId> {
    semantic.filter(|semantic| custom.is_none_or(|custom| custom == *semantic))
}

fn runtime_pointer_code(button: RuntimePointerButton) -> u32 {
    match button {
        RuntimePointerButton::Primary => 0x110,
        RuntimePointerButton::Secondary => 0x111,
        RuntimePointerButton::Middle => 0x112,
    }
}

fn scroll_distance(steps: f64) -> Option<f32> {
    const PIXELS_PER_STEP: f64 = 40.0;
    let distance = (steps * PIXELS_PER_STEP) as f32;
    (distance.is_finite() && distance != 0.0).then_some(distance)
}

#[cfg(test)]
mod input_tests {
    use std::{collections::BTreeMap, num::NonZeroU32, time::Duration};

    use smithay_client_toolkit::seat::{
        keyboard::{KeyEvent, Keysym, Modifiers, RepeatInfo},
        pointer::AxisScroll,
    };
    use thing_core::{Kind, Node, NodeId, Point, Rect};
    use thing_ui::{Layout, Measurer, Size, TextInputEditor};
    use wayland_client::protocol::wl_pointer;

    use super::{
        ActiveTextInput, GenerationReset, MissingSurfacePolicy, PointerPhase, RepeatSession,
        RepeatTimerAction, RuntimePointerButton, RuntimeScrollSource, SliderKeyAction,
        SurfaceActionRoute, SurfaceLayouts, TextClipboardAction, TextKeyAction, WidgetKeyAction,
        active_text_input_is_current, allocate_text_input_generation,
        cancel_active_text_input_composition, claim_autofocus, direct_input_serial,
        focused_widget_is_interactive, input_method_cursor_rectangle,
        input_method_surface_has_keyboard_focus, keyboard_focus_transition_affects,
        keyboard_leave_matches, keyboard_surface_changed, paint_interaction,
        pending_paste_is_current, prepare_text_input_submit, reconcile_controlled_text_edit,
        repeat_action_is_current, reset_active_text_input_generation, retain_non_focus_actions,
        reveal_text_input_focus, root_key_input, runtime_pointer_event, runtime_scroll_input,
        scroll_distance, scroll_steps, semantic_pointer_target, surface_action_route,
        text_clipboard_action, text_key_action, widget_key_action,
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
    fn custom_pointer_coordinates_are_local_to_current_presented_bounds() {
        let modifiers = Modifiers {
            shift: true,
            logo: true,
            ..Modifiers::default()
        };
        let event = runtime_pointer_event(
            Point { x: 130.5, y: -15.0 },
            PointerPhase::Move,
            0x110,
            (170.5, 10.0),
            &modifiers,
        )
        .unwrap();
        assert_eq!(event.phase, PointerPhase::Move);
        assert_eq!(event.button, RuntimePointerButton::Primary);
        assert_eq!((event.x, event.y), (130.5, -15.0));
        assert_eq!((event.surface_x, event.surface_y), (170.5, 10.0));
        assert!(event.shift);
        assert!(event.logo);
        assert!(!event.ctrl);
        assert!(!event.alt);
        assert!(
            runtime_pointer_event(
                Point::default(),
                PointerPhase::Press,
                0x113,
                (0.0, 0.0),
                &Modifiers::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn custom_pointer_children_block_parent_button_defaults() {
        let button = NodeId(1);
        let area = NodeId(2);
        assert_eq!(semantic_pointer_target(Some(button), Some(area)), None);
        assert_eq!(
            semantic_pointer_target(Some(button), Some(button)),
            Some(button),
        );
        assert_eq!(semantic_pointer_target(Some(button), None), Some(button));
        assert_eq!(semantic_pointer_target(None, Some(area)), None);
    }

    #[test]
    fn autofocus_is_claimed_once_and_never_overrides_a_choice() {
        let mut available = true;
        assert!(claim_autofocus(&mut available, None, false));
        assert!(!claim_autofocus(&mut available, None, false));

        let mut available = true;
        assert!(!claim_autofocus(&mut available, Some(NodeId(1)), false));
        assert!(!available);

        let mut available = true;
        assert!(!claim_autofocus(&mut available, None, true));
        assert!(!available);
    }

    #[test]
    fn popup_actions_wait_for_same_cycle_creation_and_then_expire() {
        assert_eq!(
            surface_action_route(None, MissingSurfacePolicy::WaitForSameCycle),
            SurfaceActionRoute::Retain
        );
        assert_eq!(
            surface_action_route(Some(4), MissingSurfacePolicy::WaitForSameCycle),
            SurfaceActionRoute::Assign(4)
        );
        assert_eq!(
            surface_action_route(None, MissingSurfacePolicy::Expire),
            SurfaceActionRoute::Drop
        );
    }

    #[test]
    fn keyboard_loss_cancels_focus_requests_but_keeps_reveals() {
        let mut actions = vec![("focus", true), ("reveal", false), ("focus", true)];
        retain_non_focus_actions(&mut actions, |(_, focus)| *focus);
        assert_eq!(actions, [("reveal", false)]);
    }

    #[test]
    fn painting_hides_focus_without_changing_its_input_origin() {
        let mut keyboard = thing_ui::Interaction::default();
        keyboard.focus_from_keyboard(NodeId(1));
        assert_eq!(paint_interaction(keyboard, false).keyboard_focused(), None);
        assert_eq!(keyboard.keyboard_focused(), Some(NodeId(1)));
        assert_eq!(
            paint_interaction(keyboard, true).keyboard_focused(),
            Some(NodeId(1))
        );

        let mut pointer = thing_ui::Interaction::default();
        pointer.focus_from_pointer(Some(NodeId(2)));
        assert_eq!(paint_interaction(pointer, true).keyboard_focused(), None);
    }

    #[test]
    fn input_layouts_do_not_replace_pointer_geometry_before_presentation() {
        let mut layouts = SurfaceLayouts::default();
        layouts.publish(
            slider_layout(false, true),
            NodeId(1),
            "consumer".into(),
            BTreeMap::new(),
        );
        assert!(
            !layouts
                .presented()
                .unwrap()
                .focus_order()
                .any(|id| id == NodeId(3))
        );

        layouts.stage_input(slider_layout(true, true));
        assert_eq!(
            layouts.presented_consumer(),
            Some("consumer"),
            "staging input geometry must preserve presented widget identities",
        );
        assert!(
            !layouts
                .presented()
                .unwrap()
                .focus_order()
                .any(|id| id == NodeId(3))
        );
        assert!(
            layouts
                .input()
                .unwrap()
                .focus_order()
                .any(|id| id == NodeId(3))
        );

        layouts.invalidate_input();
        assert!(layouts.input_needs_refresh());
        assert!(
            !layouts
                .input()
                .unwrap()
                .focus_order()
                .any(|id| id == NodeId(3))
        );

        layouts.stage_input(slider_layout(true, true));
        assert!(!layouts.input_needs_refresh());
        assert!(
            layouts
                .input()
                .unwrap()
                .focus_order()
                .any(|id| id == NodeId(3))
        );
    }

    #[test]
    fn a_focus_change_during_dispatch_cannot_rearm_key_repeat() {
        assert!(repeat_action_is_current(true, &Some((1, 2)), &Some((1, 2))));
        assert!(!repeat_action_is_current(
            true,
            &Some((1, 2)),
            &Some((1, 3))
        ));
        assert!(!repeat_action_is_current(true, &Some((1, 2)), &None));
        assert!(!repeat_action_is_current(
            false,
            &Some((1, 2)),
            &Some((1, 2))
        ));
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
                width: 120.0,
                height: 32.0,
                ..Rect::default()
            },
            thing_ui::Affine::translation(20.0, 10.0),
            true,
            &editor,
        )
        .unwrap();

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
                width: 40.0,
                height: 32.0,
                ..Rect::default()
            },
            thing_ui::Affine::translation(50.0, 12.0),
            true,
            &editor,
        )
        .unwrap();

        assert!((54..=86).contains(&rectangle.x));
        assert!(rectangle.y >= 16);
        assert!(rectangle.width >= 1);
        assert!(rectangle.height >= 1);
    }

    #[test]
    fn input_method_cursor_geometry_rotates_outward_and_singular_geometry_is_unavailable() {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "hello".into();
        node.style.padding = 4.0;
        let editor = TextInputEditor::new(&node, 112.0);
        let mut style = thing_core::Style {
            rotation: 90.0,
            translate_x: 40.0,
            ..thing_core::Style::default()
        };
        style.transform_origin.x = 0.0;
        style.transform_origin.y = 0.0;
        let transform = thing_ui::Affine::widget(&style, 120.0, 32.0).unwrap();
        let rectangle = input_method_cursor_rectangle(
            &node,
            Rect {
                width: 120.0,
                height: 32.0,
                ..Rect::default()
            },
            transform,
            true,
            &editor,
        )
        .unwrap();
        assert!(rectangle.x <= 36);
        assert!(rectangle.width >= 1);
        assert!(rectangle.height >= 1);

        assert!(
            input_method_cursor_rectangle(
                &node,
                Rect {
                    width: 120.0,
                    height: 32.0,
                    ..Rect::default()
                },
                transform,
                false,
                &editor,
            )
            .is_none()
        );
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

        let mut singular = root.clone();
        singular.children[0].style.scale_x = 0.0;
        let singular_layout = Layout::compute(
            &singular,
            Size {
                width: 200.0,
                height: 50.0,
            },
            &mut FixedMeasurer,
        )
        .unwrap();
        assert!(singular_layout.get(NodeId(3)).is_none());
        let semantic = *singular_layout.semantic_placement(NodeId(3)).unwrap();
        assert!(active_text_input_is_current(
            Some(&singular),
            Some(&singular_layout),
            Some(NodeId(3)),
            NodeId(3)
        ));
        let node = singular.find(NodeId(3)).unwrap();
        let editor = TextInputEditor::new(node, semantic.local_bounds().width);
        assert!(
            input_method_cursor_rectangle(
                node,
                semantic.local_bounds(),
                semantic.transform,
                semantic.is_invertible(),
                &editor,
            )
            .is_none()
        );

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
        assert_eq!(
            scroll_steps(AxisScroll {
                value120: 30,
                ..AxisScroll::default()
            }),
            0.25
        );
        assert_eq!(
            scroll_steps(AxisScroll {
                discrete: -1,
                ..AxisScroll::default()
            }),
            -1.0
        );
        assert_eq!(
            scroll_steps(AxisScroll {
                absolute: 7.5,
                ..AxisScroll::default()
            }),
            0.5
        );
        assert_eq!(scroll_distance(1.0), Some(40.0));
        assert_eq!(scroll_distance(-0.5), Some(-20.0));
        assert_eq!(scroll_distance(0.0), None);
        assert_eq!(scroll_distance(f64::NAN), None);
        assert_eq!(scroll_distance(f64::INFINITY), None);
        assert_eq!(scroll_distance(f64::MAX), None);
    }

    #[test]
    fn scroll_metadata_uses_handler_local_coordinates_and_axis_source() {
        let modifiers = Modifiers {
            shift: true,
            ctrl: false,
            alt: true,
            logo: false,
            ..Modifiers::default()
        };
        let input = runtime_scroll_input(
            Point { x: 4.0, y: 5.0 },
            0.25,
            -0.5,
            (14.0, 25.0),
            &modifiers,
            Some(wl_pointer::AxisSource::WheelTilt),
        )
        .unwrap();
        assert_eq!(input.x, 0.25);
        assert_eq!(input.y, -0.5);
        assert_eq!(input.local_x, 4.0);
        assert_eq!(input.local_y, 5.0);
        assert_eq!(input.surface_x, 14.0);
        assert_eq!(input.surface_y, 25.0);
        assert!(input.shift);
        assert!(!input.ctrl);
        assert!(input.alt);
        assert!(!input.logo);
        assert_eq!(input.source, RuntimeScrollSource::WheelTilt);

        assert!(
            runtime_scroll_input(
                Point::default(),
                f64::NAN,
                0.0,
                (0.0, 0.0),
                &Modifiers::default(),
                None,
            )
            .is_none()
        );
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
    fn root_key_records_use_xkb_names_text_modifiers_and_repeat_origin() {
        let mut event = key_event(108, Keysym::Down);
        event.utf8 = Some("é".into());
        let modifiers = Modifiers {
            shift: true,
            ctrl: true,
            alt: false,
            logo: true,
            ..Modifiers::default()
        };

        let input = root_key_input(&event, modifiers, true);

        assert_eq!(input.key, "Down");
        assert_eq!(input.text.as_deref(), Some("é"));
        assert!(input.shift);
        assert!(input.ctrl);
        assert!(!input.alt);
        assert!(input.logo);
        assert!(input.repeated);

        event.utf8 = Some(String::new());
        assert_eq!(root_key_input(&event, modifiers, false).text, None);

        event.keysym = Keysym::new(0xdead_beef);
        assert_eq!(root_key_input(&event, modifiers, false).key, "0xdeadbeef");
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
    fn clipboard_shortcuts_are_direct_ctrl_actions_without_alt_or_logo() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        for (keysym, expected) in [
            (Keysym::c, TextClipboardAction::Copy),
            (Keysym::C, TextClipboardAction::Copy),
            (Keysym::x, TextClipboardAction::Cut),
            (Keysym::X, TextClipboardAction::Cut),
            (Keysym::v, TextClipboardAction::Paste),
            (Keysym::V, TextClipboardAction::Paste),
        ] {
            let event = key_event(30, keysym);
            assert_eq!(text_clipboard_action(&event, ctrl, true), Some(expected));
            assert_eq!(text_clipboard_action(&event, ctrl, false), None);
        }

        let event = key_event(30, Keysym::c);
        assert_eq!(
            text_clipboard_action(
                &event,
                Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Modifiers::default()
                },
                true,
            ),
            Some(TextClipboardAction::Copy)
        );
        assert_eq!(
            text_clipboard_action(&event, Modifiers::default(), true),
            None
        );
        for modifiers in [
            Modifiers {
                ctrl: true,
                alt: true,
                ..Modifiers::default()
            },
            Modifiers {
                ctrl: true,
                logo: true,
                ..Modifiers::default()
            },
        ] {
            assert_eq!(text_clipboard_action(&event, modifiers, true), None);
        }
        assert_eq!(
            text_clipboard_action(&key_event(30, Keysym::a), ctrl, true),
            None
        );
    }

    #[test]
    fn paste_requires_the_original_surface_target_focus_generation_and_revision() {
        let id = NodeId(7);
        let expected = (id, 11, 13);
        assert!(pending_paste_is_current(
            true,
            true,
            Some(id),
            expected,
            Some(expected),
        ));
        assert!(!pending_paste_is_current(
            false,
            true,
            Some(id),
            expected,
            Some(expected),
        ));
        assert!(!pending_paste_is_current(
            true,
            false,
            Some(id),
            expected,
            Some(expected),
        ));
        assert!(!pending_paste_is_current(
            true,
            true,
            Some(NodeId(8)),
            expected,
            Some(expected),
        ));
        assert!(!pending_paste_is_current(
            true,
            true,
            Some(id),
            expected,
            Some((id, 12, 13)),
        ));
        assert!(!pending_paste_is_current(
            true,
            true,
            Some(id),
            expected,
            Some((id, 11, 14)),
        ));
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
            cursor_available: true,
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
            cursor_available: true,
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
    fn keyboard_focus_leave_and_reentry_are_two_paste_cancelling_transitions() {
        assert!(keyboard_surface_changed(Some(&1), None));
        assert!(keyboard_surface_changed(None, Some(&1)));
        assert!(keyboard_surface_changed(Some(&1), Some(&2)));
        assert!(!keyboard_surface_changed(Some(&1), Some(&1)));
        assert!(!keyboard_surface_changed::<u8>(None, None));
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
        assert!(session.host_is_armed());
        assert_eq!(session.key.as_ref().unwrap().surface, 7);
        assert!(!session.accepts_compositor_repeat(106, &7));

        assert_eq!(
            session.update(RepeatInfo::Disable),
            RepeatTimerAction::Disarm
        );
        assert!(!session.host_is_armed());
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
        assert!(!session.host_is_armed());

        assert_eq!(
            session.press(right_key(106), 8, true, true),
            RepeatTimerAction::Arm(Duration::from_millis(600))
        );
        assert_eq!(session.clear(), RepeatTimerAction::Disarm);
        assert!(session.key.is_none());
        assert!(!session.host_is_armed());
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
        assert!(session.host_is_armed());

        assert_eq!(
            session.press(key_event(102, Keysym::Home), 8, true, false),
            RepeatTimerAction::Disarm
        );
        assert!(session.key.is_none());
        assert!(!session.host_is_armed());
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
        let focus_owner = self.keyboard_focus_owner();
        let key_repeats = self.physical_key_repeats(event.raw_code);
        let action_repeats = self.key(event.clone(), serial, true, qh);
        self.finish_key_press(
            event,
            key_repeats,
            repeat_action_is_current(action_repeats, &focus_owner, &self.keyboard_focus_owner()),
        );
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
            app.set_surface_scale(index, scale);
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
        app.finish_launch(completed.request, completed.fresh.then_some(token.as_str()));
    }
}

delegate_noop!(App: ignore WlDataDeviceManager);

impl Dispatch<WlDataDevice, ()> for App {
    event_created_child!(App, WlDataDevice, [
        0 => (WlDataOffer, ClipboardOfferData::default())
    ]);

    fn event(
        app: &mut Self,
        device: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { .. } => {}
            wl_data_device::Event::Enter { serial, id, .. } => {
                app.focused_clipboard.drag_enter(device, serial, id);
            }
            wl_data_device::Event::Leave | wl_data_device::Event::Drop => {
                app.focused_clipboard.drag_finished(device);
            }
            wl_data_device::Event::Selection { id } => {
                app.focused_clipboard.selection(device, id);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, ClipboardOfferData> for App {
    fn event(
        _: &mut Self,
        _: &WlDataOffer,
        event: wl_data_offer::Event,
        data: &ClipboardOfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            data.offer(mime_type);
        }
    }
}

impl Dispatch<WlDataSource, ClipboardSourceData> for App {
    fn event(
        app: &mut Self,
        _: &WlDataSource,
        event: wl_data_source::Event,
        data: &ClipboardSourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                app.focused_clipboard.send(data, &mime_type, fd);
            }
            wl_data_source::Event::Cancelled => {
                app.focused_clipboard.cancelled(data.key());
            }
            _ => {}
        }
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
            zwp_text_input_v3::Event::Leave { surface } if app.input_method.leave(&surface) => {
                app.cancel_input_method_composition(&surface);
                app.reconcile_input_method(InputMethodChangeCause::InputMethod);
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
delegate_noop!(App: ignore ZwpIdleInhibitManagerV1);
delegate_noop!(App: ignore ZwpIdleInhibitorV1);
delegate_noop!(App: ignore ExtIdleNotifierV1);

impl Dispatch<ExtIdleNotificationV1, IdleNotification> for App {
    fn event(
        app: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        notification: &IdleNotification,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idle = match event {
            ext_idle_notification_v1::Event::Idled => true,
            ext_idle_notification_v1::Event::Resumed => false,
            _ => return,
        };
        if let Some(transition) = app.idle_notify.transition(notification.0, idle) {
            app.deliver_idle(transition);
        }
    }
}

impl RegistryHandler<App> for IdleNotifyState {
    fn new_global(
        app: &mut App,
        _: &Connection,
        qh: &QueueHandle<App>,
        _: u32,
        interface: &str,
        _: u32,
    ) {
        if interface != ExtIdleNotifierV1::interface().name {
            return;
        }
        match app.idle_notify.bind_next(&app.registry, qh) {
            Ok(true) => {
                app.publish_idle_watch_availability();
                app.reconcile_idle_watches();
            }
            Ok(false) => {}
            Err(error) => eprintln!("thing: {error}"),
        }
    }

    fn remove_global(app: &mut App, _: &Connection, qh: &QueueHandle<App>, name: u32, _: &str) {
        if !app.idle_notify.retire(name) {
            return;
        }
        if let Err(error) = app.idle_notify.bind_next(&app.registry, qh) {
            eprintln!("thing: {error}");
        }
        // watches of the removed notifier resume and rearm on a replacement.
        app.reconcile_idle_watches();
        app.publish_idle_watch_availability();
    }
}
delegate_noop!(App: ignore xdg_positioner::XdgPositioner);

impl RegistryHandler<App> for IdleInhibitState {
    fn new_global(
        app: &mut App,
        _: &Connection,
        qh: &QueueHandle<App>,
        _: u32,
        interface: &str,
        _: u32,
    ) {
        if interface != ZwpIdleInhibitManagerV1::interface().name {
            return;
        }
        match app.idle_inhibit.bind_next(&app.registry, qh) {
            Ok(true) => {
                if let Err(error) = app.publish_idle_availability() {
                    app.error = Some(error);
                }
            }
            Ok(false) => {}
            Err(error) => eprintln!("thing: {error}"),
        }
    }

    fn remove_global(app: &mut App, _: &Connection, qh: &QueueHandle<App>, name: u32, _: &str) {
        if !app.idle_inhibit.retire(
            name,
            app.surfaces
                .iter_mut()
                .map(|surface| &mut surface.idle_inhibitor),
        ) {
            return;
        }
        if let Err(error) = app.idle_inhibit.bind_next(&app.registry, qh) {
            eprintln!("thing: {error}");
        }
        if app.idle_inhibit.active().is_some() {
            // replacement keeps availability true without reevaluating output policy.
            app.reconcile_idle_inhibitors();
        } else if let Err(error) = app.publish_idle_availability() {
            app.error = Some(error);
        }
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState, IdleInhibitState, IdleNotifyState];
}
