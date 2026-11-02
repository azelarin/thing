//! host glue for external drag and drop. protocol objects, transfers, and
//! their limits live in `crate::drag_drop`; this module maps drag positions
//! to presented drop targets and runs their callbacks.
//!
//! drag focus is separate from pointer and keyboard focus: a drag never
//! changes hover, pressed, or focused widgets.

use thing_core::DropType;
use thing_runtime::{
    DragAction, DragOverInput, DragOverPhase, DragRequest, DragResult, DropInput, DropPosition,
    UiTarget,
};

use super::*;
use crate::drag_drop::{DragDrop, DragPosition, DragSourceData, DropStart};

#[cfg(test)]
mod tests;

/// the configuration generation and surface a drop or an outgoing drag
/// belongs to.
#[derive(Clone)]
pub(super) struct DragOwner {
    output: wl_output::WlOutput,
    consumer: String,
    surface: wl_surface::WlSurface,
    /// the drop target, for a dropped offer.
    target: Option<UiTarget>,
    /// the runtime's drag id, for an outgoing drag.
    request: u64,
    position: DropPosition,
}

/// the drop target under the current incoming drag.
struct DropFocus {
    session: u64,
    surface: wl_surface::WlSurface,
    output: wl_output::WlOutput,
    target: ScopedUiTarget,
    position: DropPosition,
}

pub(super) struct HostDrag {
    manager: DragDrop<DragOwner>,
    focus: Option<DropFocus>,
    /// the press that last started a drag. one press starts one drag at most.
    used_serial: Option<(wl_surface::WlSurface, u32)>,
}

impl HostDrag {
    pub(super) fn new(ping: calloop::ping::Ping) -> io::Result<Self> {
        Ok(Self {
            manager: DragDrop::new(ping)?,
            focus: None,
            used_serial: None,
        })
    }
}

impl App {
    pub(super) fn drag_enter(
        &mut self,
        device: &WlDataDevice,
        serial: u32,
        surface: wl_surface::WlSurface,
        x: f64,
        y: f64,
        offer: Option<WlDataOffer>,
    ) {
        self.leave_drop_focus(DragOverPhase::Leave);
        let current = self.focused_clipboard.binding().map(|binding| binding.2);
        if let Some(position) =
            self.drag
                .manager
                .enter(current.as_ref(), device, serial, surface, x, y, offer)
        {
            self.update_drop_focus(position);
        }
    }

    pub(super) fn drag_motion(&mut self, device: &WlDataDevice, x: f64, y: f64) {
        if let Some(position) = self.drag.manager.motion(device, x, y) {
            self.update_drop_focus(position);
        }
    }

    pub(super) fn drag_leave(&mut self, device: &WlDataDevice) {
        if self.drag.manager.leave(device).is_some() {
            self.leave_drop_focus(DragOverPhase::Leave);
        }
    }

    pub(super) fn drag_dropped(&mut self, device: &WlDataDevice) {
        let session = self
            .drag
            .manager
            .position()
            .map(|position| position.session);
        let focus = self
            .drag
            .focus
            .take_if(|focus| Some(focus.session) == session);
        let Some(focus) = focus else {
            // nothing under the drag accepted it: destroy the offer unanswered.
            self.drag.manager.leave(device);
            return;
        };
        let owner = DragOwner {
            output: focus.output.clone(),
            consumer: focus.target.consumer.clone(),
            surface: focus.surface.clone(),
            target: Some(focus.target.target.clone()),
            request: 0,
            position: focus.position,
        };
        match self.drag.manager.dropped(device, owner) {
            DropStart::Reading | DropStart::Ignored => {}
            DropStart::Rejected(owner) => self.notify_drop_target(&owner, DragOverPhase::Leave),
        }
    }

    /// delivers finished drop reads and answers their offers.
    pub(super) fn update_drag_drop(&mut self) {
        for delivery in self.drag.manager.take_deliveries() {
            let owner = delivery.owner;
            match delivery.result {
                Ok(data) => {
                    let accepted = owner.target.clone().is_some_and(|target| {
                        self.dispatch_drop(
                            owner.output.clone(),
                            ScopedUiTarget {
                                consumer: owner.consumer.clone(),
                                target,
                            },
                            DropInput {
                                position: owner.position,
                                action: delivery.action,
                                data,
                            },
                        ) == Some(true)
                    });
                    self.drag.manager.finish_drop(delivery.session, accepted);
                }
                Err(error) => {
                    eprintln!("thing: dropped data was not delivered: {error}");
                    self.drag.manager.finish_drop(delivery.session, false);
                    self.notify_drop_target(&owner, DragOverPhase::Cancel);
                }
            }
        }
    }

    pub(super) fn drag_source_event(
        &mut self,
        data: &DragSourceData,
        event: wl_data_source::Event,
    ) {
        let key = data.key();
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                self.drag.manager.send(key, &mime_type, fd);
            }
            wl_data_source::Event::Action { dnd_action } => {
                self.drag
                    .manager
                    .source_action(key, crate::drag_drop::action_bits(dnd_action));
            }
            wl_data_source::Event::DndFinished => self.end_outgoing_drag(key, true),
            wl_data_source::Event::Cancelled => self.end_outgoing_drag(key, false),
            _ => {}
        }
    }

    /// starts the drag a pointer callback requested, or reports it
    /// cancelled. the request is honored only while the callback's primary
    /// press still holds its implicit grab and serial.
    pub(super) fn start_requested_drag(
        &mut self,
        output: wl_output::WlOutput,
        consumer: String,
        request: DragRequest,
        capture: Option<CustomPointerCapture>,
    ) {
        let id = request.id;
        match self.start_drag_with_capture(&output, &consumer, request, capture) {
            Ok(()) => {
                // the compositor now owns the pointer; end the captured
                // sequence with a cancel so the handler can reset itself.
                self.cancel_custom_pointer();
            }
            Err(error) => {
                eprintln!("thing: cannot start drag: {error}");
                self.finish_outgoing(output, consumer, id, DragResult::Cancelled);
            }
        }
    }

    fn start_drag_with_capture(
        &mut self,
        output: &wl_output::WlOutput,
        consumer: &str,
        request: DragRequest,
        capture: Option<CustomPointerCapture>,
    ) -> Result<(), &'static str> {
        let capture = capture
            .filter(|capture| {
                &capture.output == output
                    && capture.target.consumer == consumer
                    && capture.button == 0x110
            })
            .ok_or("start_drag needs the active primary press")?;
        let index = self
            .index(&capture.surface)
            .ok_or("the pressed surface is gone")?;
        let surface = &self.surfaces[index];
        if surface.pressed_button != Some(0x110) {
            return Err("the primary press has ended");
        }
        let serial = surface
            .press_serial
            .ok_or("the press serial was already used")?;
        if self.drag.used_serial.as_ref() == Some(&(capture.surface.clone(), serial)) {
            return Err("this press already started a drag");
        }
        let (manager, seat, device) = self
            .focused_clipboard
            .binding()
            .ok_or("drag and drop is unavailable")?;
        if self.seat.as_ref() != Some(&seat) {
            return Err("the press belongs to another seat");
        }
        let owner = DragOwner {
            output: output.clone(),
            consumer: consumer.to_owned(),
            surface: capture.surface.clone(),
            target: None,
            request: request.id,
            position: DropPosition::default(),
        };
        self.drag.manager.start(
            &manager,
            &device,
            &capture.surface,
            serial,
            &request.data,
            request.actions,
            owner,
            &self.queue,
        )?;
        self.drag.used_serial = Some((capture.surface, serial));
        Ok(())
    }

    fn end_outgoing_drag(&mut self, key: u64, finished: bool) {
        if let Some((owner, result)) = self.drag.manager.end_source(key, finished) {
            self.finish_outgoing(owner.output, owner.consumer, owner.request, result);
        }
    }

    /// runs `on_end` once in the generation that started the drag.
    fn finish_outgoing(
        &mut self,
        output: wl_output::WlOutput,
        consumer: String,
        id: u64,
        result: DragResult,
    ) {
        let Some(entry) = self.current_layout_validation_entry(&output) else {
            return;
        };
        let preflight = LayoutPreflight::new(
            &mut self.render,
            self.layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let outcome =
            self.configurations
                .finish_drag_checked(&output, &consumer, id, result, |surfaces| {
                    preflight.validate(&output, surfaces)
                });
        let prepared = preflight.into_prepared();
        match outcome {
            Ok(Some(dispatch)) => {
                self.finish_dispatch(output, Some(consumer), Ok(dispatch), None, prepared);
            }
            Ok(None) => {}
            Err(error) => {
                self.finish_dispatch(output, Some(consumer), Err(error), None, prepared);
            }
        }
    }

    /// retires drag state tied to a surface that is being destroyed.
    pub(super) fn retire_drags_for_surface(&mut self, surface: &wl_surface::WlSurface) {
        if self.drag.manager.retire_surface(surface).is_some()
            || self
                .drag
                .focus
                .as_ref()
                .is_some_and(|focus| &focus.surface == surface)
        {
            self.drag.focus = None;
        }
        self.drag
            .manager
            .retire_drop(|owner| &owner.surface == surface);
        if let Some(owner) = self
            .drag
            .manager
            .cancel_outgoing(|owner| &owner.surface == surface)
        {
            self.finish_outgoing(
                owner.output,
                owner.consumer,
                owner.request,
                DragResult::Cancelled,
            );
        }
        if self
            .drag
            .used_serial
            .as_ref()
            .is_some_and(|(used, _)| used == surface)
        {
            self.drag.used_serial = None;
        }
    }

    /// retires drag state whose configuration generation was replaced or
    /// removed. a replaced runtime cannot receive callbacks, so none run.
    pub(super) fn reconcile_drags(&mut self) {
        let configurations = &self.configurations;
        let stale = |output: &wl_output::WlOutput, consumer: &str| {
            configurations.consumer(output) != Some(consumer)
        };
        if self.drag.focus.as_ref().is_some_and(|focus| {
            !configurations.drop_target_is_current(
                &focus.output,
                &focus.target.consumer,
                &focus.target.target,
            )
        }) {
            let session = self.drag.focus.take().map(|focus| focus.session);
            if let Some(session) = session {
                self.drag.manager.negotiate(session, None);
            }
        }
        self.drag
            .manager
            .retire_drop(|owner| stale(&owner.output, &owner.consumer));
        self.drag
            .manager
            .cancel_outgoing(|owner| stale(&owner.output, &owner.consumer));
    }

    /// the seat's data device is going away.
    pub(super) fn detach_drags(&mut self) {
        let (_, outgoing) = self.drag.manager.detach();
        self.leave_drop_focus(DragOverPhase::Leave);
        self.drag.used_serial = None;
        if let Some(owner) = outgoing {
            self.finish_outgoing(
                owner.output,
                owner.consumer,
                owner.request,
                DragResult::Cancelled,
            );
        }
        self.update_drag_drop();
    }

    fn update_drop_focus(&mut self, position: DragPosition) {
        let session = position.session;
        let point = Point {
            x: position.x as f32,
            y: position.y as f32,
        };
        let hit = self.index(&position.surface).and_then(|index| {
            let id = self.surfaces[index]
                .layouts
                .presented()?
                .drop_target(point)?;
            let target = self.current_presented_drop_target(index, id)?;
            let local = self.surfaces[index]
                .layouts
                .presented()?
                .surface_to_local(id, point)?;
            Some((
                self.surfaces[index].output.clone(),
                target,
                DropPosition {
                    x: local.x.into(),
                    y: local.y.into(),
                    surface_x: position.x,
                    surface_y: position.y,
                },
            ))
        });
        let same = self.drag.focus.as_ref().is_some_and(|focus| {
            focus.session == session
                && focus.surface == position.surface
                && hit.as_ref().is_some_and(|(output, target, _)| {
                    &focus.output == output && &focus.target == target
                })
        });
        if !same {
            self.leave_drop_focus(DragOverPhase::Leave);
        }
        let Some((output, target, drop_position)) = hit else {
            self.drag.manager.negotiate(session, None);
            return;
        };
        let input = DragOverInput {
            phase: if same {
                DragOverPhase::Move
            } else {
                DragOverPhase::Enter
            },
            position: drop_position,
            offered: position.types.clone(),
            actions: position.actions,
        };
        let accepted = self.dispatch_drag_over(output.clone(), &target, input);
        // the callback may have ended the session or removed the target.
        let current = self
            .drag
            .manager
            .position()
            .is_some_and(|current| current.session == session);
        if !current {
            return;
        }
        let accepted = accepted.filter(|_| {
            self.configurations
                .drop_target_is_current(&output, &target.consumer, &target.target)
        });
        match accepted {
            Some(accepted) => {
                self.drag.focus = Some(DropFocus {
                    session,
                    surface: position.surface,
                    output,
                    target,
                    position: drop_position,
                });
                self.drag.manager.negotiate(session, accepted);
            }
            None => {
                self.drag.focus = None;
                self.drag.manager.negotiate(session, None);
            }
        }
    }

    fn leave_drop_focus(&mut self, phase: DragOverPhase) {
        let Some(focus) = self.drag.focus.take() else {
            return;
        };
        let owner = DragOwner {
            output: focus.output,
            consumer: focus.target.consumer,
            surface: focus.surface,
            target: Some(focus.target.target),
            request: 0,
            position: focus.position,
        };
        self.notify_drop_target(&owner, phase);
    }

    /// runs a leave or cancel callback if the target is still current.
    fn notify_drop_target(&mut self, owner: &DragOwner, phase: DragOverPhase) {
        let Some(target) = owner.target.clone() else {
            return;
        };
        let target = ScopedUiTarget {
            consumer: owner.consumer.clone(),
            target,
        };
        self.dispatch_drag_over(
            owner.output.clone(),
            &target,
            DragOverInput {
                phase,
                position: owner.position,
                offered: Vec::new(),
                actions: Default::default(),
            },
        );
    }

    fn current_presented_drop_target(&self, index: usize, id: NodeId) -> Option<ScopedUiTarget> {
        let surface = self.surfaces.get(index)?;
        if !surface.layouts.presented()?.drop_target_is_eligible(id) {
            return None;
        }
        let target = surface.layouts.presented_target(id)?;
        (self.configurations.drop_target_is_current(
            &surface.output,
            &target.consumer,
            &target.target,
        ) && self
            .configurations
            .rendered_surface_root(&surface.output, id)
            == Some(surface.root))
        .then_some(target)
    }

    /// runs `on_drag_over`. returns the accepted type and action, or `None`
    /// when the target is stale or the callback failed.
    fn dispatch_drag_over(
        &mut self,
        output: wl_output::WlOutput,
        target: &ScopedUiTarget,
        input: DragOverInput,
    ) -> Option<Option<(DropType, DragAction)>> {
        let consumer = Some(target.consumer.clone());
        let entry = self.current_layout_validation_entry(&output)?;
        let preflight = LayoutPreflight::new(
            &mut self.render,
            self.layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self.configurations.dispatch_drag_over_checked(
            &output,
            &target.consumer,
            &target.target,
            input,
            |surfaces| preflight.validate(&output, surfaces),
        );
        let prepared = preflight.into_prepared();
        match result {
            Ok(Some((outcome, dispatch))) => self
                .finish_dispatch(output, consumer, Ok(dispatch), None, prepared)
                .then_some(outcome.accepted),
            Ok(None) => None,
            Err(error) => {
                self.finish_dispatch(output, consumer, Err(error), None, prepared);
                None
            }
        }
    }

    /// runs `on_drop`. returns whether the target accepted the data.
    fn dispatch_drop(
        &mut self,
        output: wl_output::WlOutput,
        target: ScopedUiTarget,
        input: DropInput,
    ) -> Option<bool> {
        let consumer = Some(target.consumer.clone());
        let entry = self.current_layout_validation_entry(&output)?;
        let preflight = LayoutPreflight::new(
            &mut self.render,
            self.layer_shell_version,
            self.idle_inhibit.active().is_some(),
            vec![entry],
        );
        let result = self.configurations.dispatch_drop_checked(
            &output,
            &target.consumer,
            &target.target,
            input,
            |surfaces| preflight.validate(&output, surfaces),
        );
        let prepared = preflight.into_prepared();
        match result {
            Ok(Some((accepted, dispatch))) => self
                .finish_dispatch(output, consumer, Ok(dispatch), None, prepared)
                .then_some(accepted),
            Ok(None) => None,
            Err(error) => {
                self.finish_dispatch(output, consumer, Err(error), None, prepared);
                None
            }
        }
    }
}
