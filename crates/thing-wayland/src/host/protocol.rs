use super::*;

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
            .filter(|surface| surface.output == output && surface.role.is_root())
            .map(Surface::owner)
            .collect();
        for owner in owners {
            self.remove_root(&owner, false);
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
            self.reconcile_file_watches();
        }
        self.reconcile_notification_expiry();
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
            self.remove_root(&owner, true);
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
                        self.remove_root(&owner, false);
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

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &XdgWindow) {
        // a close for a window that was already replaced finds no surface.
        let Some(index) = self.index(window.wl_surface()) else {
            return;
        };
        let owner = self.surfaces[index].owner();
        let root = self.surfaces[index].root;
        let handled = self
            .configurations
            .runtime(&owner.output)
            .is_some_and(|runtime| runtime.handles_close(root));
        if handled {
            // the configuration decides; the window stays unless the callback
            // hides it, and a failing callback publishes none of its actions.
            self.dispatch(owner.output.clone(), root, Event::Close, None);
            return;
        }
        // without a handler a close hides the window until its declaration is
        // hidden and shown again, or the configuration reloads.
        if !self.closed_surfaces.contains(&owner) {
            self.closed_surfaces.push(owner.clone());
        }
        self.remove_root(&owner, true);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        window: &XdgWindow,
        configure: WindowConfigure,
        _: u32,
    ) {
        // sctk has acknowledged the serial; the next commit answers it.
        let Some(index) = self.index(window.wl_surface()) else {
            return;
        };
        let Some(root) = self
            .configurations
            .surface_root(&self.surfaces[index].output, &self.surfaces[index].key)
        else {
            return;
        };
        let surface = &self.surfaces[index];
        let current = surface.configured.then_some(surface.size);
        let size = toplevel::configured_size(
            configure.new_size,
            current,
            &root.window,
            configure.suggested_bounds,
        );
        match self.prepare_configured_surface_layout(index, size) {
            Ok(prepared) => {
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
            Err(error) => {
                // the last accepted layout keeps drawing at its own size.
                eprintln!("thing: cannot apply window configuration: {error}");
                let surface = &mut self.surfaces[index];
                if !surface.configured {
                    let owner = surface.owner();
                    self.remove_root(&owner, false);
                    return;
                }
                surface.dirty = true;
            }
        }
        let surface = &mut self.surfaces[index];
        let state = toplevel::state(&configure, surface.size);
        let owner = surface.owner();
        if let Role::Window(role) = &mut surface.role {
            role.state = state;
        }
        self.queue_window_state(owner, state);
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
            if let Role::Popup(role) = &mut surface.role {
                role.position = Some(config.position);
            }
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
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => app.drag_enter(device, serial, surface, x, y, id),
            wl_data_device::Event::Motion { x, y, .. } => app.drag_motion(device, x, y),
            wl_data_device::Event::Leave => app.drag_leave(device),
            wl_data_device::Event::Drop => app.drag_dropped(device),
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
        match event {
            wl_data_offer::Event::Offer { mime_type } => data.offer(mime_type),
            wl_data_offer::Event::SourceActions { source_actions } => {
                data.set_source_actions(crate::drag_drop::action_bits(source_actions));
            }
            wl_data_offer::Event::Action { dnd_action } => {
                data.set_action(crate::drag_drop::action_bits(dnd_action));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataSource, crate::drag_drop::DragSourceData> for App {
    fn event(
        app: &mut Self,
        _: &WlDataSource,
        event: wl_data_source::Event,
        data: &crate::drag_drop::DragSourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        app.drag_source_event(data, event);
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

impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, crate::clipboard::ImageBarrier>
    for App
{
    fn event(
        app: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        event: wayland_client::protocol::wl_callback::Event,
        data: &crate::clipboard::ImageBarrier,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(
            event,
            wayland_client::protocol::wl_callback::Event::Done { .. }
        ) && let Some(clipboard) = app.clipboard.as_mut()
        {
            clipboard.image_barrier_done(data.0);
        }
    }
}
