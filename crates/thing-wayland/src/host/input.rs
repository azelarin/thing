use super::*;

impl App {
    fn presented_link_is_current(&self, index: usize, id: NodeId, link: u16) -> bool {
        let Some(surface) = self.surfaces.get(index) else {
            return false;
        };
        let Some(target) = surface
            .layouts
            .presented()
            .and_then(|layout| layout.link_target(id, link))
        else {
            return false;
        };
        self.configurations
            .surface_root(&surface.output, &surface.key)
            .and_then(|root| root.find(id))
            .and_then(|node| node.links.get(usize::from(link)))
            .is_some_and(|current| current == target)
    }

    /// shows the cursor of the presented layout under the pointer. only
    /// presented, accepted layouts decide, so a rejected candidate or reload
    /// never shows its cursor.
    pub(super) fn reconcile_cursor(&mut self, connection: &Connection) {
        let Some(pointer) = &self.pointer else {
            return;
        };
        let Some(surface) = self
            .surfaces
            .iter()
            .find(|surface| surface.pointer_position.is_some())
        else {
            self.cursor.reset();
            return;
        };
        let point = surface
            .pointer_position
            .expect("the surface was found by its pointer");
        let wl = surface.role.surface().clone();
        let owner = self
            .pointer_capture
            .as_ref()
            .filter(|capture| capture.surface == wl)
            .map(|capture| capture.target.target.node())
            .or_else(|| {
                surface
                    .pressed_target
                    .as_ref()
                    .map(|target| target.target.node())
            });
        let cursor = surface
            .layouts
            .presented()
            .map_or(Cursor::Default, |layout| {
                owner
                    .and_then(|id| layout.cursor_for(id))
                    .unwrap_or_else(|| layout.cursor_at(point))
            });
        let held = owner.is_none()
            && (surface.pressed_button.is_some()
                || self
                    .pointer_capture
                    .as_ref()
                    .is_some_and(|capture| capture.surface == wl));
        self.cursor.show(connection, pointer, &wl, cursor, held);
    }
    pub(super) fn input_origin(
        &self,
        surface: wl_surface::WlSurface,
        serial: u32,
    ) -> Option<InputOrigin> {
        Some(InputOrigin {
            seat: self.seat.clone()?,
            serial,
            surface,
        })
    }
    pub(super) fn reconcile_presented_pointer_state(&mut self, output: &wl_output::WlOutput) {
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
                surface.pressed_link = None;
                surface.pressed_button = None;
                surface.press_serial = None;
                surface.dirty = true;
            }
        }
    }
    pub(super) fn range_input(&mut self, surface: usize, target: &ScopedUiTarget, point: Point) {
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
    pub(super) fn reconcile_range_presses(&mut self, output: &wl_output::WlOutput) {
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
                surface.pressed_link = None;
                surface.press_serial = None;
                surface.range_press = None;
                surface.dirty = true;
            }
            if notify {
                self.dispatch(output.clone(), id, Event::Pressed(false), None);
            }
        }
    }
    pub(super) fn text_input_node_and_bounds(
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
    pub(super) fn text_input_node_and_placement(
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
    pub(super) fn scoped_focus_target(
        &self,
        output: &wl_output::WlOutput,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        Some(ScopedUiTarget {
            consumer: self.configurations.consumer(output)?.to_owned(),
            target: self.configurations.focus_target(output, id)?,
        })
    }
    pub(super) fn scoped_target_is_current(
        &self,
        output: &wl_output::WlOutput,
        target: &ScopedUiTarget,
    ) -> bool {
        self.configurations.consumer(output) == Some(target.consumer.as_str())
            && self
                .configurations
                .ui_target_is_current(output, &target.target)
    }
    pub(super) fn surface_target_is_current(
        &self,
        surface: usize,
        target: &ScopedUiTarget,
    ) -> bool {
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
    pub(super) fn surface_pointer_target_is_current(
        &self,
        surface: usize,
        target: &ScopedUiTarget,
    ) -> bool {
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
    pub(super) fn surface_scroll_target_is_current(
        &self,
        surface: usize,
        target: &ScopedUiTarget,
    ) -> bool {
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
    pub(super) fn surface_scroll_view_target_is_current(
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
    pub(super) fn scoped_target_identity_is_current_at(
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
    pub(super) fn scoped_target_is_current_at(
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
    pub(super) fn runtime_owns_active_press(
        &self,
        output: &wl_output::WlOutput,
        target: &ScopedUiTarget,
    ) -> bool {
        self.configurations.consumer(output) == Some(target.consumer.as_str())
            && self
                .configurations
                .has_active_press(output, target.target.node())
    }
    pub(super) fn current_presented_target(
        &self,
        surface: usize,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        let target = self.surfaces.get(surface)?.layouts.presented_target(id)?;
        self.surface_target_is_current(surface, &target)
            .then_some(target)
    }
    pub(super) fn current_presented_pointer_target(
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
    pub(super) fn current_presented_press_target(
        &self,
        surface: usize,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        let target = self
            .surfaces
            .get(surface)?
            .layouts
            .presented_pointer_target(id)?;
        self.surface_target_is_current(surface, &target)
            .then_some(target)
    }
    pub(super) fn current_presented_scroll_target(
        &self,
        surface: usize,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        let target = self.surfaces.get(surface)?.layouts.presented_target(id)?;
        self.surface_scroll_target_is_current(surface, &target)
            .then_some(target)
    }
    pub(super) fn current_presented_scroll_view_target(
        &self,
        surface: usize,
        id: NodeId,
    ) -> Option<ScopedUiTarget> {
        let target = self.surfaces.get(surface)?.layouts.presented_target(id)?;
        self.surface_scroll_view_target_is_current(surface, &target)
            .then_some(target)
    }
    pub(super) fn custom_pointer_event(
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
    pub(super) fn finish_pointer_dispatch(
        &mut self,
        output: wl_output::WlOutput,
        consumer: Option<String>,
        result: Result<PointerHandlerOutcome, String>,
        origin: Option<InputOrigin>,
        prepared: Vec<PreparedOutputLayouts>,
    ) -> Option<(bool, bool)> {
        // a drag may start only from the capture that is still active after
        // the callback that requested it.
        let drag = consumer.clone().and_then(|consumer| {
            let request = self.configurations.take_drag_request(&output, &consumer)?;
            let active = result.as_ref().is_ok_and(|outcome| outcome.active);
            Some((
                consumer,
                request,
                self.pointer_capture.clone().filter(|_| active),
            ))
        });
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(capture) = self.pointer_capture.take() {
                    self.clear_pressed_for_capture(&capture);
                }
                self.finish_dispatch(output.clone(), consumer, Err(error), origin, prepared);
                if let Some((consumer, request, _)) = drag {
                    self.start_requested_drag(output, consumer, request, None);
                }
                return None;
            }
        };
        if !outcome.active
            && let Some(capture) = self.pointer_capture.take()
        {
            self.clear_pressed_for_capture(&capture);
        }
        let result = (outcome.handled, outcome.active);
        let finished = self.finish_dispatch(
            output.clone(),
            consumer,
            Ok(outcome.dispatch),
            origin,
            prepared,
        );
        if let Some((consumer, request, capture)) = drag {
            self.start_requested_drag(output, consumer, request, capture);
        }
        finished.then_some(result)
    }
    pub(super) fn begin_custom_pointer(
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
    pub(super) fn dispatch_custom_pointer(
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
    pub(super) fn cancel_custom_pointer(&mut self) {
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
    pub(super) fn reconcile_custom_pointer(&mut self, output: &wl_output::WlOutput) {
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
    pub(super) fn clear_pressed_for_capture(&mut self, capture: &CustomPointerCapture) {
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
        surface.pressed_link = None;
        surface.pressed_button = None;
        surface.press_serial = None;
        surface.dirty = true;
    }
    pub(super) fn same_surface_target(
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
    pub(super) fn focus_from_pointer(&mut self, surface: usize, target: Option<ScopedUiTarget>) {
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
    pub(super) fn focus_from_keyboard(&mut self, surface: usize, target: ScopedUiTarget) -> bool {
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
    pub(super) fn allocate_text_input_generation(&mut self) -> Option<u64> {
        let Some(generation) = allocate_text_input_generation(&mut self.next_text_input_generation)
        else {
            self.error = Some("text input generation counter is exhausted".into());
            return None;
        };
        Some(generation)
    }
    pub(super) fn focus_text_input(
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
    pub(super) fn input_method_text_state(&mut self) -> Option<InputMethodTextState> {
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
    pub(super) fn reconcile_input_method(&mut self, cause: InputMethodChangeCause) {
        let state = self.input_method_text_state();
        self.input_method.update(state, cause);
    }
    pub(super) fn cancel_input_method_composition(&mut self, surface: &wl_surface::WlSurface) {
        let Some(index) = self.index(surface) else {
            return;
        };
        if cancel_active_text_input_composition(&mut self.surfaces[index].text_input) {
            self.surfaces[index].dirty = true;
        }
    }
    pub(super) fn reset_input_method_generation(
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
    pub(super) fn apply_input_method_done(&mut self, done: InputMethodDone) {
        let cause = self.apply_input_method_done_inner(done);
        // done may resume deferred protocol state. always publish the editor's
        // post-batch state, including when the batch was rejected.
        self.reconcile_input_method(cause);
    }
    pub(super) fn apply_input_method_done_inner(
        &mut self,
        done: InputMethodDone,
    ) -> InputMethodChangeCause {
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
    pub(super) fn text_input_key(
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
    pub(super) fn text_clipboard_key(
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
    pub(super) fn apply_text_clipboard_edit(
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
    pub(super) fn remove_popup(
        &mut self,
        index: usize,
        notify_runtime: bool,
        suppress_hover: bool,
    ) {
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
        self.retire_drags_for_surface(&wl);
        self.configurations
            .forget_presented_geometry(&output, surface_root);
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
    pub(super) fn sync_popup(
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
        if !parent_surface.role.is_root() {
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
    pub(super) fn create_popup(
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
        // a panel adopts its popup through layer shell; a window is the
        // popup's xdg parent and its coordinate space.
        let parent_role = match &parent.role {
            Role::Panel(layer) => PopupParent::Layer(layer.clone()),
            Role::Window(window) => PopupParent::Window(window.shell.clone()),
            Role::Popup(_) => return false,
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
                let edge = popup_edge(root);
                root.find(id)
                    .and_then(|node| popup::resolve_for_surface(node, anchor, edge, parent.size))
            })
        else {
            return false;
        };
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
        let xdg_parent = match &parent_role {
            PopupParent::Layer(_) => None,
            PopupParent::Window(window) => Some(window.xdg_surface().clone()),
        };
        let popup =
            XdgPopup::from_surface(xdg_parent.as_ref(), &positioner, qh, wl, &self.xdg_shell);
        positioner.destroy();
        match popup {
            Ok(shell) => {
                if let PopupParent::Layer(layer) = &parent_role {
                    layer.get_popup(shell.xdg_popup());
                }
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
                        position: None,
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
    pub(super) fn popup_index(&self, owner: &SurfaceOwner) -> Option<usize> {
        self.surfaces.iter().position(|surface| {
            surface.output == owner.output
                && surface.key == owner.key
                && surface.role.popup_mode().is_some()
        })
    }
    pub(super) fn close_other_grabbed_popups(&mut self, owner: &SurfaceOwner) {
        while let Some(index) = self.surfaces.iter().position(|surface| {
            surface.owner() != *owner && matches!(&surface.role, Role::Popup(role) if role.grabbed)
        }) {
            self.remove_popup(index, true, true);
        }
    }
    pub(super) fn process_hover(&mut self, qh: &QueueHandle<Self>) {
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
                            && surface.role.is_root()
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
    pub(super) fn current_hover_target(
        &self,
        key: &HoverKey<SurfaceOwner>,
    ) -> Option<HoverTarget<SurfaceOwner>> {
        let index = self.surfaces.iter().position(|surface| {
            surface.output == key.owner.output
                && surface.key == key.owner.key
                && surface.role.is_root()
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
    pub(super) fn hover_layout_ready(&self, key: &HoverKey<SurfaceOwner>) -> bool {
        self.surfaces
            .iter()
            .enumerate()
            .find(|surface| {
                surface.1.output == key.owner.output
                    && surface.1.key == key.owner.key
                    && surface.1.role.is_root()
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
    pub(super) fn reconcile_pointer(&mut self) {
        self.reconcile_pointers(None);
    }
    pub(super) fn reconcile_presented_pointers(&mut self, presented: &[usize]) {
        if !presented.is_empty() {
            self.reconcile_pointers(Some(presented));
        }
    }
    pub(super) fn reconcile_pointers(&mut self, selected: Option<&[usize]>) {
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
    pub(super) fn reconcile_hover_intent(&mut self) {
        let pointer = self
            .surfaces
            .iter()
            .enumerate()
            .find_map(|(index, surface)| {
                surface.pointer_position?;
                if let Some(PopupMode::Hover(key)) = surface.role.popup_mode() {
                    return self.current_hover_target(key);
                }
                if !surface.role.is_root() {
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
            .filter(|surface| surface.role.is_root())
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
    pub(super) fn keyboard_hover_target(&self) -> Option<HoverTarget<SurfaceOwner>> {
        self.surfaces.iter().find_map(|surface| {
            if !surface.role.is_root()
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
    pub(super) fn clear_pointer_state(&mut self, preserve_keyboard_hover: bool) {
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
            surface.pressed_link = None;
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
    pub(super) fn clear_keyboard_focus(&mut self) {
        self.set_keyboard_surface(None);
    }
    pub(super) fn schedule_autofocus(&mut self, index: usize) {
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
    pub(super) fn set_keyboard_surface(&mut self, next: Option<wl_surface::WlSurface>) {
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
    pub(super) fn clear_key_repeat(&mut self) {
        let action = self.repeat.clear();
        self.apply_repeat_timer(action);
    }
    pub(super) fn apply_repeat_timer(&mut self, action: RepeatTimerAction) {
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
    pub(super) fn finish_key_press(
        &mut self,
        event: KeyEvent,
        key_repeats: bool,
        action_repeats: bool,
    ) {
        let action = match self.keyboard_surface.clone() {
            Some(surface) => self
                .repeat
                .press(event, surface, key_repeats, action_repeats),
            None if key_repeats => self.repeat.clear(),
            None => RepeatTimerAction::None,
        };
        self.apply_repeat_timer(action);
    }
    pub(super) fn keyboard_focus_owner(&self) -> Option<(wl_surface::WlSurface, Option<NodeId>)> {
        let surface = self.keyboard_surface.clone()?;
        let focused = self
            .index(&surface)
            .and_then(|index| self.surfaces[index].input.focused);
        Some((surface, focused))
    }
    pub(super) fn physical_key_repeats(&self, raw_code: u32) -> bool {
        let Some(keycode) = raw_code.checked_add(8).map(xkb::Keycode::new) else {
            return false;
        };
        self.keyboard_keymap
            .as_ref()
            .is_some_and(|keymap| keymap.key_repeats(keycode))
    }
    pub(super) fn dispatch_key_repeat(&mut self) -> TimeoutAction {
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
    pub(super) fn dispatch_root_key(
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
    pub(super) fn key(
        &mut self,
        event: KeyEvent,
        serial: u32,
        direct: bool,
        qh: &QueueHandle<Self>,
    ) -> bool {
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
            WidgetKeyAction::Link(step) => {
                let surface = &mut self.surfaces[index];
                let links = surface
                    .layouts
                    .presented()
                    .map(|layout| visible_links(layout, id))
                    .unwrap_or_default();
                let current = surface
                    .layouts
                    .presented()
                    .and_then(|layout| layout.selected_link(id, surface.input.focused_link));
                surface.input.focused_link = step_link(&links, current, step);
                surface.input.reveal_keyboard_focus();
                surface.layouts.invalidate_input();
                surface.prepared = None;
                surface.dirty = true;
                self.prepare_invalidated_input_layouts_now();
            }
            WidgetKeyAction::Activate if node.kind == Kind::Text => {
                let surface = &mut self.surfaces[index];
                let links = surface
                    .layouts
                    .presented()
                    .map(|layout| visible_links(layout, id))
                    .unwrap_or_default();
                // the selected link if it is still visible, else the first.
                let link = surface
                    .input
                    .focused_link
                    .filter(|link| links.contains(link))
                    .or_else(|| links.first().copied());
                surface.input.focused_link = link;
                surface.input.reveal_keyboard_focus();
                surface.dirty = true;
                if let Some(link) =
                    link.filter(|link| self.presented_link_is_current(index, id, *link))
                {
                    self.dispatch(output, id, Event::Link(link), origin);
                }
            }
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
                self.cursor.reset();
                let pointer = match &self.shm {
                    Some(shm) => self
                        .seats
                        .get_pointer_with_theme::<_, smithay_client_toolkit::compositor::SurfaceData>(
                            qh,
                            &seat,
                            shm,
                            self.compositor.create_surface(qh),
                            ThemeSpec::System,
                        )
                        .map(SeatPointer::Themed),
                    None => self.seats.get_pointer(qh, &seat).map(SeatPointer::Plain),
                };
                match pointer {
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
            self.cursor.reset();
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
            // drag objects belong to the seat's data device, so they go first.
            self.detach_drags();
            self.focused_clipboard.detach_seat();
            self.pending_paste = None;
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
            self.cursor.reset();
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
        connection: &Connection,
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
                    let pressed_link = hit.as_ref().and_then(|target| {
                        let id = target.target.node();
                        let layout = self.surfaces[index].layouts.presented()?;
                        let link = layout.link_at(id, point)?;
                        Some((link, layout.link_target(id, link)?.to_owned()))
                    });
                    let surface = &mut self.surfaces[index];
                    surface.pointer_position = Some(point);
                    surface.input.pressed = hit.as_ref().map(|target| target.target.node());
                    surface.pressed_target = hit.clone();
                    surface.pressed_link = pressed_link;
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
                    let pressed_link = self.surfaces[index].pressed_link.take();
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
                        let is_text = self
                            .configurations
                            .surface_root(&output, &self.surfaces[index].key)
                            .and_then(|root| root.find(id))
                            .is_some_and(|node| node.kind == Kind::Text);
                        let event = if is_text {
                            // only the link under the release point activates,
                            // as the presented frame shaped it.
                            let link = self.surfaces[index]
                                .layouts
                                .presented()
                                .and_then(|layout| layout.link_at(id, point));
                            self.surfaces[index].input.focused_link = link;
                            link.filter(|link| {
                                button == 0x110
                                    && self.presented_link_is_current(index, id, *link)
                                    && pressed_link.as_ref().is_some_and(|(pressed, target)| {
                                        pressed == link
                                            && self.surfaces[index]
                                                .layouts
                                                .presented()
                                                .and_then(|layout| layout.link_target(id, *link))
                                                == Some(target.as_str())
                                    })
                            })
                            .map(Event::Link)
                        } else {
                            Some(match button {
                                0x111 => Event::SecondaryClick,
                                0x112 => Event::MiddleClick,
                                _ => Event::Click,
                            })
                        };
                        if let Some(event) = event {
                            self.dispatch(
                                output,
                                id,
                                event,
                                self.input_origin(event_surface.clone(), release_serial),
                            );
                        }
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
                        let delta = viewport_scroll_delta(x, y, self.modifiers.shift);
                        self.dispatch_scroll_route(output, &event_surface, route, delta);
                    }
                }
                _ => {}
            }
        }
        self.reconcile_pointer();
        self.reconcile_input_method(InputMethodChangeCause::Other);
        self.reconcile_cursor(connection);
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
