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
    delegate_registry, delegate_seat, delegate_xdg_popup, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryHandler, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{
            KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers, RepeatInfo,
        },
        pointer::{AxisScroll, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure, SurfaceKind,
        },
        xdg::{
            XdgShell, XdgSurface,
            popup::{Popup as XdgPopup, PopupConfigure, PopupHandler},
            window::{Window as XdgWindow, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
};
use thing_core::{
    Cursor, HorizontalRange, KeyboardMode, Kind, Node, NodeId, PanelInputRegion, Point, Rect,
    Scale, ScrollAxis, SurfaceLayer,
};
use thing_runtime::{
    Action, Event, KeyInput, ManagedProcessAction, ManagedProcessDefinition,
    ManagedProcessSnapshot, OutputContext, PointerButton as RuntimePointerButton,
    PointerEvent as RuntimePointerEvent, PointerPhase, Scalar, ScrollInput as RuntimeScrollInput,
    ScrollSource as RuntimeScrollSource, ServiceSnapshot, SurfaceKey, SurfaceTree,
    SystemTrayAction, SystemTrayMenuAction, UiAction, WindowRequest, WindowState,
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

mod dispatch;
mod input;
mod inspection;
mod launching;
mod protocol;
mod services;
mod surfaces;

#[cfg(test)]
mod command_tests;
#[cfg(test)]
mod cursor_tests;
#[cfg(test)]
mod idle_sleep_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod launch_tests;
#[cfg(test)]
mod output_tests;
#[cfg(test)]
mod wheel_tests;
#[cfg(test)]
mod window_tests;

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
    command::{self, CommandHost, CommandReply, CommandRequest, CommandServer, OutputResult},
    configuration::{
        Configurations, DispatchOutcome, PointerHandlerOutcome, RemovedConfiguration,
        ScrollHandlerOutcome,
    },
    cursor::{CursorState, SeatPointer},
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
    panel::{self, Geometry, ResolvedInputRegion, logical_size as panel_size},
    popup::{self, Position as PopupPosition},
    power::Power,
    render::{FrameResult, Render, Window},
    sleep::{SleepInhibition, Transport as SleepTransport},
    toplevel::{self, WindowRole},
    watch::Watch,
};

/// run configured surfaces on each output. the loop sleeps until input,
/// configuration, or a compositor frame callback arrives; unchanged surfaces do
/// not request frames.
pub fn run(entry: &Path, renderer: Renderer) -> Result<(), String> {
    run_with_options(entry, RunOptions::default().with_renderer(renderer))
}

/// run configured surfaces with process-wide service and renderer options.
/// the configuration's commands listen as the default instance when that name
/// is free, and are unavailable otherwise.
pub fn run_with_options(entry: &Path, options: RunOptions) -> Result<(), String> {
    run_instance(entry, options, command::DEFAULT_INSTANCE, false)
}

/// run configured surfaces as a named instance. `thing call --instance`
/// reaches this instance's commands; starting fails when another live
/// instance already owns the name.
pub fn run_with_instance(entry: &Path, options: RunOptions, instance: &str) -> Result<(), String> {
    command::validate_instance_name(instance)?;
    run_instance(entry, options, instance, true)
}

fn run_instance(
    entry: &Path,
    options: RunOptions,
    instance: &str,
    required: bool,
) -> Result<(), String> {
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
    // a configuration made only of windows runs without layer shell; one that
    // declares a panel is rejected during validation instead.
    let layer_shell = LayerShell::bind(&globals, &qh).ok();
    let xdg_shell = XdgShell::bind(&globals, &qh).map_err(|error| error.to_string())?;
    let activation: Option<XdgActivationV1> = globals.bind(&qh, 1..=1, ()).ok();
    // the cursor theme fallback draws into shared memory.
    let shm: Option<wayland_client::protocol::wl_shm::WlShm> = globals.bind(&qh, 1..=1, ()).ok();
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
        pending_window_states: Vec::new(),
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
        shm,
        cursor: CursorState::default(),
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
        commands: None,
        inspector: options.inspector.then(inspection::Inspector::default),
    };
    queue
        .roundtrip(&mut app)
        .map_err(|error| error.to_string())?;
    app.initialize(&qh)?;
    match command::runtime_directory()
        .and_then(|directory| CommandServer::start(&event_loop.handle(), &directory, instance))
    {
        Ok(server) => app.commands = Some(server),
        Err(error) if required => return Err(format!("cannot serve commands: {error}")),
        Err(error) => eprintln!("thing: commands are unavailable: {error}"),
    }
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
        self.reconcile_notification_expiry();
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
        self.publish_window_states();
        if self
            .sleep
            .next_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            self.update_sleep();
        }
        // closing a popup or window during the last turn forgot its geometry.
        // refresh before an animation frame could evaluate those dependents
        // without updating window properties.
        if self.configurations.take_geometry_refresh() {
            self.refresh();
        }
        // an expired or retired highlight marks its surface dirty, so this
        // turn's draw removes it.
        self.reconcile_highlight(Instant::now());
        self.draw(qh)?;
        // presented geometry may feed property functions. their surfaces draw
        // once a frame callback or another event wakes the loop again.
        if self.configurations.take_geometry_refresh() {
            self.refresh();
        }
        // drawing installs newly accepted layouts, which can change what is
        // under a pointer that has not moved.
        self.reconcile_cursor(connection);
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
    Root,
    Popup,
}

struct LayoutSurfaceState {
    key: SurfaceKey,
    root: NodeId,
    role: LayoutSurfaceRole,
    size: (u32, u32),
    /// the scale the surface draws at and the one the compositor prefers.
    scale: Scale,
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
    /// move the keyboard's link selection inside a text widget.
    Link(i32),
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
            Self::Slider(SliderKeyAction::Decrease | SliderKeyAction::Increase) | Self::Link(_)
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
    /// where the compositor last configured the popup in its parent's
    /// surface coordinates. it applies from the next presented frame.
    position: Option<(i32, i32)>,
}

impl PopupRole {
    /// the popup's origin in its parent surface, in logical pixels.
    fn origin(&self) -> Option<(f64, f64)> {
        self.position.map(|(x, y)| (f64::from(x), f64::from(y)))
    }
}

enum Role {
    Panel(LayerSurface),
    Window(Box<WindowRole>),
    Popup(PopupRole),
}
impl Role {
    fn surface(&self) -> &wl_surface::WlSurface {
        match self {
            Self::Panel(layer) => layer.wl_surface(),
            Self::Window(window) => window.shell.wl_surface(),
            Self::Popup(role) => role.shell.wl_surface(),
        }
    }

    fn popup_mode(&self) -> Option<&PopupMode> {
        match self {
            Self::Panel(_) | Self::Window(_) => None,
            Self::Popup(role) => Some(&role.mode),
        }
    }

    /// panels and windows are the roots of a configuration's surfaces;
    /// popups belong to one of them.
    fn is_root(&self) -> bool {
        !matches!(self, Self::Popup(_))
    }

    /// whether the root's declared kind still matches this role.
    fn matches_kind(&self, kind: Kind) -> bool {
        match self {
            Self::Panel(_) => kind == Kind::Panel,
            Self::Window(_) => kind == Kind::Window,
            Self::Popup(_) => false,
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
    /// the logical size and scroll offsets of the last presented frame, so
    /// geometry can tell a resize or scroll from feedback.
    presented_input: Option<((u32, u32), ScrollState)>,
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
    pressed_link: Option<(u16, String)>,
    range_press: Option<ScopedUiTarget>,
    configured: bool,
    dirty: bool,
    waiting: bool,
    retry_at: Option<Instant>,
    /// the last presented frame and the widget metadata it was painted
    /// from, kept only while live inspection is enabled.
    inspection: Option<crate::inspection::FrameSnapshot>,
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
    Viewport(ScopedUiTarget, ScrollAxis),
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
    layer_shell: Option<LayerShell>,
    /// window states waiting to be published, newest last for each owner.
    pending_window_states: Vec<(SurfaceOwner, WindowState)>,
    layer_shell_version: u32,
    xdg_shell: XdgShell,
    activation: Option<XdgActivationV1>,
    fractional: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    pointer: Option<SeatPointer>,
    shm: Option<wayland_client::protocol::wl_shm::WlShm>,
    cursor: CursorState,
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
    /// the command socket. dropping the app closes it and unlinks the socket.
    commands: Option<CommandServer<App>>,
    /// live inspection, present only with `thing run --inspector`.
    inspector: Option<inspection::Inspector>,
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
        .chain(
            self.inspector
                .as_ref()
                .and_then(inspection::Inspector::deadline),
        )
        .min()
    }

    fn request_window(
        &mut self,
        output: &wl_output::WlOutput,
        key: &str,
        request: WindowRequest,
    ) -> Result<(), String> {
        let owner = SurfaceOwner {
            output: output.clone(),
            key: SurfaceKey::Named(key.to_owned()),
        };
        let window = self
            .root_index(&owner)
            .and_then(|index| match &self.surfaces[index].role {
                Role::Window(window) => Some(window),
                _ => None,
            })
            .ok_or_else(|| format!("window {key:?} is not open"))?;
        // the compositor answers with a configure that updates the state.
        match request {
            WindowRequest::Maximized(true) => window.shell.set_maximized(),
            WindowRequest::Maximized(false) => window.shell.unset_maximized(),
            WindowRequest::Fullscreen(true) => window.shell.set_fullscreen(None),
            WindowRequest::Fullscreen(false) => window.shell.unset_fullscreen(),
        }
        Ok(())
    }

    /// window states change while surfaces are being synchronized, so they
    /// are published from the event loop rather than inside a refresh.
    fn queue_window_state(&mut self, owner: SurfaceOwner, state: WindowState) {
        if !matches!(owner.key, SurfaceKey::Named(_)) {
            return;
        }
        self.pending_window_states
            .retain(|(pending, _)| pending != &owner);
        self.pending_window_states.push((owner, state));
    }

    fn publish_window_states(&mut self) {
        if self.pending_window_states.is_empty() {
            return;
        }
        for (owner, state) in std::mem::take(&mut self.pending_window_states) {
            let SurfaceKey::Named(key) = &owner.key else {
                continue;
            };
            if let Err(error) = self
                .configurations
                .set_window_state(&owner.output, key, state)
            {
                eprintln!("thing: {error}");
            }
        }
        self.refresh();
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
        self.reconcile_notification_expiry();
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
        // a successful reload retires every widget identity; a rejected one
        // returned above and keeps the highlight.
        self.clear_highlight();
        self.reconcile_notification_expiry();
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
                surface.role.is_root() && !self.configurations.contains(&surface.output)
            })
            .map(Surface::owner)
            .collect();
        for owner in removed {
            self.remove_root(&owner, false);
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

/// a text widget's visible links, once each, in layout order.
fn visible_links(layout: &Layout, id: NodeId) -> Vec<u16> {
    let mut links = Vec::new();
    for (link, _) in layout.link_regions(id) {
        if !links.contains(link) {
            links.push(*link);
        }
    }
    links
}

/// the link `step` places from `current`, stopping at either end.
fn step_link(links: &[u16], current: Option<u16>, step: i32) -> Option<u16> {
    let position = current.and_then(|current| links.iter().position(|link| *link == current));
    let next = match position {
        None if step < 0 => links.len().checked_sub(1)?,
        None => 0,
        Some(position) => position
            .saturating_add_signed(step as isize)
            .min(links.len().saturating_sub(1)),
    };
    links.get(next).copied()
}

fn widget_key_action(kind: Kind, keysym: Keysym, direct: bool) -> Option<WidgetKeyAction> {
    match (kind, keysym) {
        (Kind::Button | Kind::Text, Keysym::Return | Keysym::space) if direct => {
            Some(WidgetKeyAction::Activate)
        }
        (Kind::Text, Keysym::Left | Keysym::Up) => Some(WidgetKeyAction::Link(-1)),
        (Kind::Text, Keysym::Right | Keysym::Down) => Some(WidgetKeyAction::Link(1)),
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
                && candidate.role == LayoutSurfaceRole::Root
                && candidate.root == surface.root.id
        });
        let prospective = if surface.root.kind == Kind::Window {
            toplevel::initial_size(&surface.root.window, None)
        } else {
            panel_size(
                panel::geometry(&surface.root, None),
                (output.width, output.height),
            )
        };
        let (size, scroll, focused) = panel.map_or_else(
            || (prospective, ScrollState::default(), None),
            |panel| (panel.size, panel.scroll.clone(), panel.focused),
        );
        let (layout, scale) = prepare_candidate_layout(
            render,
            surface.key.clone(),
            &surface.root,
            (size, scroll, focused),
            panel.map_or((output.scale, None), |panel| {
                (panel.preferred_scale, Some(panel.scale))
            }),
        )?;
        prepared.push(layout);
        // a window shown again starts at its initial size and its owner
        // output's scale; a panel keeps the scale it already draws at.
        let prospective_scale = if surface.root.kind == Kind::Window {
            output.scale
        } else {
            scale
        };
        if size != prospective || scale != prospective_scale {
            prepare_surface_layout(
                render,
                surface.key.clone(),
                &surface.root,
                prospective,
                ScrollState::default(),
                None,
                prospective_scale,
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
            let (layout, scale) = prepare_candidate_layout(
                render,
                surface.key.clone(),
                popup_root,
                (size, scroll, focused),
                existing.map_or((output.scale, None), |popup| {
                    (popup.preferred_scale, Some(popup.scale))
                }),
            )?;
            prepared.push(layout);
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

/// lays out a candidate at the compositor's preferred scale. when that scale
/// is rejected, a surface that already draws at another scale keeps it, as
/// configure handling does, so one oversized preferred scale cannot block
/// every later update.
fn prepare_candidate_layout(
    render: &mut Render,
    key: SurfaceKey,
    root: &Node,
    (size, scroll, focused): ((u32, u32), ScrollState, Option<NodeId>),
    (preferred, accepted): (Scale, Option<Scale>),
) -> Result<(PreparedSurfaceLayout, Scale), String> {
    match prepare_surface_layout(
        render,
        key.clone(),
        root,
        size,
        scroll.clone(),
        focused,
        preferred,
    ) {
        Ok(layout) => Ok((layout, preferred)),
        Err(error) => match accepted.filter(|accepted| *accepted != preferred) {
            Some(accepted) => {
                prepare_surface_layout(render, key, root, size, scroll, focused, accepted)
                    .map(|layout| (layout, accepted))
            }
            None => Err(error),
        },
    }
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
    if focused.is_some_and(|id| layout.reveal(id, &mut scroll)) {
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
    let mut panels = surfaces
        .iter()
        .filter(|surface| surface.root.kind == Kind::Panel);
    if layer_shell_version == 0 && panels.clone().next().is_some() {
        return Err(LAYER_SHELL_REQUIRED.into());
    }
    if layer_shell_version < 4
        && panels.any(|surface| surface.root.panel.keyboard_mode == KeyboardMode::OnDemand)
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
        Role::Window(_) | Role::Popup(_) => 0,
    }
}

const LAYER_SHELL_REQUIRED: &str =
    "ui.panel needs a compositor with wlr-layer-shell; ui.window works without it";

enum PopupParent {
    Layer(LayerSurface),
    Window(XdgWindow),
}

/// the output edge automatic popups open away from. windows have no edge, so
/// their popups open below the trigger as a top panel's do.
fn popup_edge(root: &Node) -> Option<thing_core::PanelEdge> {
    if root.kind == Kind::Window {
        return Some(thing_core::PanelEdge::Top);
    }
    root.panel
        .anchors
        .and_then(|anchors| anchors.exclusive_edge())
        .or(root.panel.anchors.is_none().then_some(root.panel.edge))
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
        let interactive = node.is_interactive();
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

fn scroll_view_axis_matches(layout: &Layout, node: &Node, axis: ScrollAxis) -> bool {
    node.kind == Kind::Scroll
        && node.scroll_axis == axis
        && layout.scroll_axis(node.id) == Some(axis)
}

fn viewport_scroll_delta(x: f64, y: f64, shift: bool) -> Point {
    let (x, y) = if shift && x == 0.0 { (y, 0.0) } else { (x, y) };
    Point {
        x: scroll_distance(x).unwrap_or(0.0),
        y: scroll_distance(y).unwrap_or(0.0),
    }
}

fn scroll_distance(steps: f64) -> Option<f32> {
    const PIXELS_PER_STEP: f64 = 40.0;
    let distance = (steps * PIXELS_PER_STEP) as f32;
    (distance.is_finite() && distance != 0.0).then_some(distance)
}

delegate_noop!(App: ignore WlDataDeviceManager);

delegate_compositor!(App);
delegate_output!(App);
delegate_layer!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
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

delegate_noop!(App: ignore xdg_positioner::XdgPositioner);
