use super::*;

impl App {
    pub(super) fn layout_validation_entry(
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
                    Role::Panel(_) | Role::Window(_) => LayoutSurfaceRole::Root,
                    Role::Popup(_) => LayoutSurfaceRole::Popup,
                },
                size: surface.size,
                scale: surface.scale,
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
    pub(super) fn current_layout_validation_entry(
        &self,
        output: &wl_output::WlOutput,
    ) -> Option<LayoutValidationEntry> {
        self.configurations
            .output(output)
            .cloned()
            .map(|context| self.layout_validation_entry(output.clone(), context))
    }
    pub(super) fn replacement_layout_validation_entry(
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
    pub(super) fn install_prepared_layouts(&mut self, outputs: Vec<PreparedOutputLayouts>) {
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
    pub(super) fn add_output(
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
    pub(super) fn create_panel(
        &mut self,
        owner: SurfaceOwner,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
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
        let layer_shell = self.layer_shell.as_ref().ok_or(LAYER_SHELL_REQUIRED)?;
        let wl = self.compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(
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
    pub(super) fn create_window(
        &mut self,
        owner: SurfaceOwner,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        let (root, namespace, properties, scale) = {
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
                surface.root.window.clone(),
                context.scale,
            )
        };
        let wl = self.compositor.create_surface(qh);
        // server decorations are requested; thing draws no frame of its own.
        let shell = self
            .xdg_shell
            .create_window(wl, WindowDecorations::RequestServer, qh);
        let role = WindowRole::new(shell, &properties);
        let surface = self.make_surface(
            Role::Window(Box::new(role)),
            SurfaceInit {
                owner,
                namespace,
                root,
                size: toplevel::initial_size(&properties, None),
                // the owner's output is only a first guess; the compositor's
                // preferred scale follows the window wherever it is shown.
                scale,
            },
            qh,
        )?;
        // the initial commit has no buffer; drawing waits for the configure.
        surface.role.surface().commit();
        self.surfaces.push(surface);
        Ok(())
    }
    /// sends changed window properties for an open window.
    pub(super) fn sync_window(&mut self, owner: &SurfaceOwner, force: bool) {
        let Some(index) = self.root_index(owner) else {
            return;
        };
        let Some(root) = self.configurations.surface_root(&owner.output, &owner.key) else {
            return;
        };
        let properties = root.window.clone();
        let root_id = root.id;
        let surface = &mut self.surfaces[index];
        let Role::Window(window) = &mut surface.role else {
            return;
        };
        let limits = window.apply(&properties);
        let state = window.state;
        let wl = surface.role.surface().clone();
        // a reload binds the open window to the replacement runtime's root;
        // input and focus from the previous runtime do not carry over.
        let replaced = force || surface.root != root_id;
        if replaced {
            surface.root = root_id;
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
        if limits || replaced {
            surface.dirty = true;
        }
        if replaced && self.keyboard_surface.as_ref() == Some(&wl) {
            self.clear_key_repeat();
            self.schedule_autofocus(index);
        }
        if replaced {
            self.queue_window_state(owner.clone(), state);
        }
    }
    pub(super) fn make_surface(
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
    pub(super) fn output_context(&self, output: &wl_output::WlOutput) -> Option<OutputContext> {
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
    pub(super) fn update_watch(&self) {
        if let Err(error) = self.watch.set_files(&self.configurations.files()) {
            eprintln!("thing: cannot watch all configuration files: {error}");
        }
    }
    pub(super) fn invalidate_all(&mut self) {
        for surface in &mut self.surfaces {
            surface.dirty = true;
        }
    }
    pub(super) fn sync_output_surfaces(
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
                    surface.root.kind,
                )
            })
            .collect();

        // a root the compositor closed stays closed until it is hidden.
        self.closed_surfaces.retain(|owner| {
            &owner.output != output
                || desired
                    .iter()
                    .any(|(key, _, visible, _, _)| key == &owner.key && *visible)
        });

        let stale: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| &surface.output == output && surface.role.is_root())
            .filter(|surface| {
                let Some((_, namespace, visible, layer, kind)) =
                    desired.iter().find(|(key, _, _, _, _)| key == &surface.key)
                else {
                    return true;
                };
                !visible
                    || !surface.role.matches_kind(*kind)
                    || (*kind == Kind::Panel && namespace != &surface.namespace)
                    || (surface
                        .panel
                        .as_ref()
                        .is_some_and(|panel| panel.layer != *layer)
                        && panel_role_version(&surface.role) < 2)
            })
            .map(Surface::owner)
            .collect();
        for owner in stale {
            self.remove_root(&owner, !force);
        }

        for (key, _, visible, _, kind) in &desired {
            let owner = SurfaceOwner {
                output: output.clone(),
                key: key.clone(),
            };
            if *visible
                && !self.closed_surfaces.contains(&owner)
                && self.root_index(&owner).is_none()
            {
                match kind {
                    Kind::Window => self.create_window(owner, qh)?,
                    _ => self.create_panel(owner, qh)?,
                }
            }
        }

        let owners: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| &surface.output == output && surface.role.is_root())
            .map(|surface| (surface.owner(), matches!(surface.role, Role::Window(_))))
            .collect();
        for (owner, window) in owners {
            if window {
                self.sync_window(&owner, force);
            } else {
                self.sync_panel(&owner, force)?;
            }
        }
        Ok(())
    }
    pub(super) fn sync_panel(&mut self, owner: &SurfaceOwner, force: bool) -> Result<(), String> {
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
    pub(super) fn sync_configured_panel(&mut self, index: usize) -> Result<(), String> {
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
    pub(super) fn prepare_configured_surface_layout(
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
    pub(super) fn prepare_surface_layout_at_scale(
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
    pub(super) fn set_surface_scale(&mut self, index: usize, scale: Scale) {
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
                Err(error) => eprintln!("thing: cannot update surface scale: {error}"),
            }
        }
    }
    pub(super) fn set_output_scale(&mut self, output: &wl_output::WlOutput, scale: Scale) {
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
    pub(super) fn root_index(&self, owner: &SurfaceOwner) -> Option<usize> {
        self.surfaces.iter().position(|surface| {
            surface.output == owner.output && surface.key == owner.key && surface.role.is_root()
        })
    }
    /// removes a panel or window with its popups and cancels its input.
    pub(super) fn remove_root(&mut self, owner: &SurfaceOwner, notify_hover: bool) {
        while let Some(index) = self.popup_index(owner) {
            self.remove_popup(index, notify_hover, false);
        }
        let Some(index) = self.root_index(owner) else {
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
        if matches!(self.surfaces[index].role, Role::Window(_)) {
            self.queue_window_state(owner.clone(), WindowState::default());
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
    pub(super) fn index(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|surface| surface.role.surface() == wl)
    }
    pub(super) fn apply_pending_ui_actions(
        &mut self,
        missing: MissingSurfacePolicy,
    ) -> Result<(), String> {
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
                            changed |= layout.reveal(id, &mut scroll);
                            focus = Some(target);
                        }
                    }
                    HostUiAction::Reveal(_) => {
                        changed |= layout.reveal(id, &mut scroll);
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
    pub(super) fn apply_pending_ui_actions_now(&mut self) {
        if let Err(error) = self.apply_pending_ui_actions(MissingSurfacePolicy::WaitForSameCycle) {
            self.error = Some(error);
        }
    }
    pub(super) fn prepare_invalidated_input_layouts(&mut self) -> Result<(), String> {
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
            if focused.is_some_and(|id| layout.reveal(id, &mut scroll)) {
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
    pub(super) fn prepare_invalidated_input_layouts_now(&mut self) {
        if let Err(error) = self.prepare_invalidated_input_layouts() {
            self.error = Some(error);
        }
    }
    pub(super) fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
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
            let changed = match changed {
                Ok(changed) => changed,
                Err(error) => {
                    eprintln!("thing: animation frame rejected: {error}");
                    continue;
                }
            };
            if changed {
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
        self.render.retain_images(
            self.surfaces
                .iter()
                .filter_map(|surface| configurations.surface_root(&surface.output, &surface.key)),
        );
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
                    let layout = Rc::new(layout);
                    let bounds = configurations.record_presented_bounds(
                        &surface.output,
                        &consumer,
                        surface.root,
                        layout.clone(),
                    );
                    surface.waiting = true;
                    surface.scroll = scroll;
                    surface
                        .layouts
                        .publish_shared(layout, surface.root, consumer, targets);
                    surface.layouts.keep_bounds(bounds);
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
            let panel = surface.role.is_root();
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
    pub(super) fn sync_popup_placement(
        &mut self,
        owner: &SurfaceOwner,
        parent_presented: bool,
        qh: &QueueHandle<Self>,
    ) {
        if !parent_presented
            && self.surfaces.iter().any(|surface| {
                surface.output == owner.output
                    && surface.key == owner.key
                    && surface.role.is_root()
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
    pub(super) fn desired_popup_position(
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
            surface.output == owner.output && surface.key == owner.key && surface.role.is_root()
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
        let edge = popup_edge(root);
        Some((
            index,
            anchor,
            popup::resolve_for_surface(node, trigger, edge, parent.size)?,
        ))
    }
}
