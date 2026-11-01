use super::*;

impl App {
    pub(super) fn dispatch(
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
    pub(super) fn dispatch_due_timers(&mut self, now: Instant) {
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
    pub(super) fn capture_scroll_route(
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
                ScrollStep::Viewport(id) => {
                    let axis = layout.scroll_axis(id)?;
                    let target = self.current_presented_scroll_view_target(index, id)?;
                    let surface = self.surfaces.get(index)?;
                    let current = self
                        .configurations
                        .surface_root(&surface.output, &surface.key)?
                        .find(id)?;
                    scroll_view_axis_matches(layout, current, axis)
                        .then_some(ScrollRouteStep::Viewport(target, axis))
                }
            })
            .collect()
    }
    /// offers one wheel event to each captured stop in order. a handler that
    /// returns `false` or a viewport at its limit passes it on. callbacks may
    /// change the tree, so every stop is revalidated before it runs, and the
    /// route ends at the first one that no longer applies.
    pub(super) fn dispatch_scroll_route(
        &mut self,
        output: wl_output::WlOutput,
        event_surface: &wl_surface::WlSurface,
        route: Vec<ScrollRouteStep>,
        delta: Point,
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
                ScrollRouteStep::Viewport(target, axis) => {
                    if !self.surface_scroll_view_target_is_current(index, &target) {
                        return;
                    }
                    let surface = &self.surfaces[index];
                    let matches = surface
                        .layouts
                        .presented()
                        .zip(
                            self.configurations
                                .surface_root(&surface.output, &surface.key)
                                .and_then(|root| root.find(target.target.node())),
                        )
                        .is_some_and(|(layout, node)| scroll_view_axis_matches(layout, node, axis));
                    if !matches {
                        return;
                    }
                    if self.scroll_viewport(index, target.target.node(), delta) {
                        return;
                    }
                }
            }
        }
    }
    /// moves one presented viewport and prepares the surface at the new
    /// offset. it returns whether the viewport could move.
    pub(super) fn scroll_viewport(&mut self, index: usize, id: NodeId, delta: Point) -> bool {
        let mut scroll = self.surfaces[index].scroll.clone();
        let moved = self.surfaces[index]
            .layouts
            .presented()
            .is_some_and(|layout| layout.scroll_view(id, delta, &mut scroll));
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
    pub(super) fn dispatch_scroll(
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
    pub(super) fn finish_dispatch(
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
                Action::Window { key, request } => self.request_window(&output, &key, request),
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
    pub(super) fn queue_ui_actions(
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
    pub(super) fn cancel_queued_focus(&mut self, output: &wl_output::WlOutput) {
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
    pub(super) fn cancel_all_queued_focus(&mut self) {
        retain_non_focus_actions(&mut self.pending_ui_actions, |pending| {
            pending.action.is_focus()
        });
        for surface in &mut self.surfaces {
            retain_non_focus_actions(&mut surface.ui_actions, HostUiAction::is_focus);
            surface.pending_focus = None;
        }
    }
    pub(super) fn assign_pending_ui_actions(&mut self, missing: MissingSurfacePolicy) {
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
    pub(super) fn reconcile_focus_targets(&mut self) {
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
    pub(super) fn reconcile_layout_focus(&mut self) {
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
}
