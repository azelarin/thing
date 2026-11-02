use super::*;

impl App {
    pub(super) fn reconcile_idle_inhibitors(&mut self) {
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
    pub(super) fn publish_idle_availability(&mut self) -> Result<(), String> {
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
    pub(super) fn publish_idle_watch_availability(&mut self) {
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
    pub(super) fn reconcile_idle_watches(&mut self) {
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
    pub(super) fn deliver_idle(&mut self, transition: IdleTransition) {
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
    pub(super) fn update_niri(&mut self) {
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
    pub(super) fn update_audio(&mut self) {
        let Some(snapshot) = self.audio.take_snapshot() else {
            return;
        };
        if let Err(error) = self.configurations.set_audio(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    pub(super) fn update_media(&mut self) {
        let Some(snapshot) = self.media.take_snapshot() else {
            return;
        };
        if let Err(error) = self.configurations.set_media(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    pub(super) fn update_applications(&mut self) {
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
    pub(super) fn update_power(&mut self) {
        let Some(snapshot) = self.power.take_snapshot() else {
            return;
        };
        if let Err(error) = self.configurations.set_power(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    pub(super) fn update_sleep(&mut self) {
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
    pub(super) fn reconcile_sleep(&mut self) {
        let definitions = self.configurations.sleep_inhibitor_definitions();
        self.sleep.reconcile(&definitions);
    }
    pub(super) fn update_notifications(&mut self) {
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
    pub(super) fn update_system_tray(&mut self) {
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
    pub(super) fn update_managed_processes(&mut self) {
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
    pub(super) fn update_clipboard(&mut self) {
        let queue = self.queue.clone();
        let Some(snapshot) = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.update(&queue))
        else {
            return;
        };
        if let Err(error) = self.configurations.set_clipboard(snapshot) {
            eprintln!("thing: {error}");
        }
        self.refresh();
    }
    pub(super) fn update_focused_clipboard(&mut self) {
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
    pub(super) fn cancel_pending_text_paste(&mut self) {
        self.pending_paste = None;
        self.focused_clipboard.cancel_pending_paste();
    }
    /// publishes the shared values one accepted event queued on `output` to
    /// every output, and installs the layouts of the outputs that changed.
    pub(super) fn publish_shared_states(
        &mut self,
        output: &wl_output::WlOutput,
        consumer: Option<&str>,
        updates: Vec<(String, std::sync::Arc<thing_runtime::CallValue>)>,
    ) {
        let Some(consumer) = consumer else {
            return;
        };
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
        let publication = self.configurations.publish_shared_states_checked(
            output,
            consumer,
            updates,
            |output, surfaces| preflight.validate(output, surfaces),
        );
        let prepared: Vec<_> = preflight
            .into_prepared()
            .into_iter()
            .filter(|prepared| publication.refreshed.contains(&prepared.output))
            .collect();
        if let Err(error) = &publication.result {
            eprintln!("thing: shared state was not published: {error}");
        }
        self.update_watch();
        for output in &publication.refreshed {
            self.invalidate_output(output);
        }
        self.install_prepared_layouts(prepared);
        for output in configured_outputs {
            self.reconcile_custom_pointer(&output);
        }
    }
    pub(super) fn reconcile_notification_expiry(&self) {
        if let Some(server) = &self.notifications {
            server.set_expiry_holds(self.configurations.notification_expiry_holds());
        }
    }
}
