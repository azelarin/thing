//! external drag and drop over `wl_data_device`.
//!
//! the manager shares the focused clipboard's data device, so each seat has
//! one device and drag offers are never duplicated. it owns the protocol
//! objects of at most one incoming drag, one dropped offer being read, and
//! one outgoing drag, and moves their bytes through a private transfer
//! worker so a stalled peer never blocks the event loop.
//!
//! drag and drop needs `wl_data_device_manager` version 3, which added
//! actions and `finish`. with an older manager, incoming offers are
//! destroyed unanswered and outgoing drags end as cancelled.

mod model;

use std::{
    collections::BTreeMap,
    io,
    os::fd::{AsFd, OwnedFd},
    sync::Arc,
    time::Duration,
};

use calloop::ping::Ping;
use thing_core::DropType;
use thing_runtime::{DragAction, DragActions, DragData, DragResult};
use wayland_client::{
    Dispatch, Proxy, QueueHandle,
    protocol::{
        wl_data_device::WlDataDevice,
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_offer::WlDataOffer,
        wl_data_source::WlDataSource,
        wl_surface::WlSurface,
    },
};

use crate::{
    data_transfer::{Completion, Worker},
    text_data::{OfferData, pipe},
};

pub(crate) use model::bits as action_bits;

/// the first `wl_data_device_manager` version with drag actions and finish.
pub(crate) const DRAG_AND_DROP_VERSION: u32 = 3;
/// how long a dropped offer or a requested write may take.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);
/// concurrent writes for outgoing drags. a destination asks once per drop,
/// so a few cover retries without letting a peer queue unbounded work.
const MAX_ACTIVE_WRITES: usize = 4;
/// the most bytes the worker may buffer for dropped data at once.
const READ_BUDGET: usize = 2 * 1024 * 1024;

/// user data for an outgoing drag source.
pub(crate) struct DragSourceData {
    key: u64,
}

impl DragSourceData {
    pub(crate) fn key(&self) -> u64 {
        self.key
    }
}

/// where a drag is over one of this client's surfaces.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DragPosition {
    pub(crate) session: u64,
    pub(crate) surface: WlSurface,
    pub(crate) x: f64,
    pub(crate) y: f64,
    /// the kinds of data the source can supply.
    pub(crate) types: Vec<DropType>,
    /// the actions the source allows.
    pub(crate) actions: DragActions,
}

/// what happened to a drop.
pub(crate) enum DropStart<T> {
    /// no current drag matched the event.
    Ignored,
    /// the dropped data is being read; a [`DropDelivery`] follows.
    Reading,
    /// nothing was accepted, or the read could not be admitted. the offer
    /// was destroyed without `finish`, so the source sees a cancellation.
    Rejected(T),
}

/// dropped data that finished reading, or why it could not be read.
pub(crate) struct DropDelivery<T> {
    pub(crate) session: u64,
    pub(crate) owner: T,
    pub(crate) action: DragAction,
    pub(crate) result: Result<DragData, String>,
}

struct Incoming {
    session: u64,
    device: WlDataDevice,
    surface: WlSurface,
    serial: u32,
    offer: WlDataOffer,
    x: f64,
    y: f64,
    /// the mime type and action last sent with `accept` and `set_actions`.
    sent: Option<Option<(String, DragAction)>>,
}

enum ReadState {
    Reading {
        transfer: u64,
    },
    /// the data was delivered and waits for the target's answer.
    Delivered,
}

struct Reading<T> {
    session: u64,
    offer: WlDataOffer,
    kind: DropType,
    action: DragAction,
    owner: T,
    state: ReadState,
}

struct Outgoing<T> {
    key: u64,
    source: WlDataSource,
    kind: DropType,
    bytes: Arc<[u8]>,
    owner: T,
    action: Option<DragAction>,
}

pub(crate) struct DragDrop<T> {
    incoming: Option<Incoming>,
    reading: Option<Reading<T>>,
    outgoing: Option<Outgoing<T>>,
    /// readings retired before their data arrived; their owners still need a
    /// cancellation callback.
    retired: Vec<DropDelivery<T>>,
    /// active writes by transfer id, with the source key they serve.
    writes: BTreeMap<u64, u64>,
    next_id: u64,
    worker: Worker,
}

impl<T: Clone> DragDrop<T> {
    pub(crate) fn new(ping: Ping) -> io::Result<Self> {
        Ok(Self {
            incoming: None,
            reading: None,
            outgoing: None,
            retired: Vec::new(),
            writes: BTreeMap::new(),
            next_id: 1,
            worker: Worker::with_read_budget(ping, READ_BUDGET)?,
        })
    }

    fn allocate(&mut self) -> Option<u64> {
        let id = self.next_id;
        self.next_id = id.checked_add(1)?;
        Some(id)
    }

    /// a drag entered `surface`. `current` is the seat's live data device;
    /// offers from any other device or an older protocol are destroyed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enter(
        &mut self,
        current: Option<&WlDataDevice>,
        device: &WlDataDevice,
        serial: u32,
        surface: WlSurface,
        x: f64,
        y: f64,
        offer: Option<WlDataOffer>,
    ) -> Option<DragPosition> {
        self.retire_incoming();
        let offer = offer?;
        if current != Some(device) || device.version() < DRAG_AND_DROP_VERSION {
            offer.destroy();
            return None;
        }
        let Some(session) = self.allocate() else {
            offer.destroy();
            return None;
        };
        self.incoming = Some(Incoming {
            session,
            device: device.clone(),
            surface,
            serial,
            offer,
            x,
            y,
            sent: None,
        });
        self.position()
    }

    pub(crate) fn motion(&mut self, device: &WlDataDevice, x: f64, y: f64) -> Option<DragPosition> {
        let incoming = self
            .incoming
            .as_mut()
            .filter(|incoming| &incoming.device == device)?;
        incoming.x = x;
        incoming.y = y;
        self.position()
    }

    /// the current drag over one of this client's surfaces.
    pub(crate) fn position(&self) -> Option<DragPosition> {
        let incoming = self.incoming.as_ref()?;
        let data = incoming.offer.data::<OfferData>()?;
        Some(DragPosition {
            session: incoming.session,
            surface: incoming.surface.clone(),
            x: incoming.x,
            y: incoming.y,
            types: model::offered_types(&data.mime_types()),
            actions: model::actions_from_mask(data.source_actions()),
        })
    }

    /// the drag left, or its source ended it. returns the ended session.
    pub(crate) fn leave(&mut self, device: &WlDataDevice) -> Option<u64> {
        if self
            .incoming
            .as_ref()
            .is_none_or(|incoming| &incoming.device != device)
        {
            return None;
        }
        self.retire_incoming()
    }

    /// tells the source which type and action the target under the drag
    /// accepts. an unchanged answer is not repeated.
    pub(crate) fn negotiate(&mut self, session: u64, accepted: Option<(DropType, DragAction)>) {
        let Some(incoming) = self
            .incoming
            .as_mut()
            .filter(|incoming| incoming.session == session)
        else {
            return;
        };
        let Some(data) = incoming.offer.data::<OfferData>() else {
            return;
        };
        let actions = model::actions_from_mask(data.source_actions());
        let answer = accepted.and_then(|(kind, action)| {
            let mime_type = model::mime_type_for(kind, &data.mime_types(), data.selected())?;
            actions.contains(action).then_some((mime_type, action))
        });
        if incoming.sent.as_ref() == Some(&answer) {
            return;
        }
        match &answer {
            Some((mime_type, action)) => {
                let action = model::action_mask(*action);
                incoming
                    .offer
                    .accept(incoming.serial, Some(mime_type.clone()));
                incoming.offer.set_actions(action, action);
            }
            None => {
                incoming.offer.accept(incoming.serial, None);
                incoming
                    .offer
                    .set_actions(DndAction::empty(), DndAction::empty());
            }
        }
        incoming.sent = Some(answer);
    }

    /// the pointer released the drag over the current session. the accepted
    /// type is read only when the compositor selected the accepted action.
    pub(crate) fn dropped(&mut self, device: &WlDataDevice, owner: T) -> DropStart<T> {
        let Some(incoming) = self.incoming.take_if(|incoming| &incoming.device == device) else {
            return DropStart::Ignored;
        };
        let selected = incoming
            .offer
            .data::<OfferData>()
            .and_then(|data| model::selected_action(data.action()));
        let Some(Some((mime_type, action))) = incoming.sent.clone() else {
            incoming.offer.destroy();
            return DropStart::Rejected(owner);
        };
        if selected != Some(action) {
            incoming.offer.destroy();
            return DropStart::Rejected(owner);
        }
        let kind = if model::serves(DropType::Files, &mime_type) {
            DropType::Files
        } else {
            DropType::Text
        };
        // a new drop replaces a read that is still running, so one stalled
        // source cannot hold more than one transfer.
        self.retire_reading();
        let Some(transfer) = self.allocate() else {
            incoming.offer.destroy();
            return DropStart::Rejected(owner);
        };
        let started = pipe().and_then(|(reader, writer)| {
            incoming.offer.receive(mime_type, writer.as_fd());
            drop(writer);
            self.worker
                .read(transfer, reader, model::read_limit(kind), TRANSFER_TIMEOUT)
        });
        if let Err(error) = started {
            eprintln!("thing: cannot read dropped data: {error}");
            incoming.offer.destroy();
            return DropStart::Rejected(owner);
        }
        self.reading = Some(Reading {
            session: incoming.session,
            offer: incoming.offer,
            kind,
            action,
            owner,
            state: ReadState::Reading { transfer },
        });
        DropStart::Reading
    }

    /// finished reads and retired drops. stale reads are discarded.
    pub(crate) fn take_deliveries(&mut self) -> Vec<DropDelivery<T>> {
        self.process_completions();
        std::mem::take(&mut self.retired)
    }

    /// answers a delivered drop. an accepted drop finishes the offer, which
    /// lets a move source remove its data; a rejected one is destroyed, so
    /// the source sees a cancellation. each offer is answered once.
    pub(crate) fn finish_drop(&mut self, session: u64, accepted: bool) {
        let Some(reading) = self.reading.take_if(|reading| {
            reading.session == session && matches!(reading.state, ReadState::Delivered)
        }) else {
            return;
        };
        if accepted {
            reading.offer.finish();
        }
        reading.offer.destroy();
    }

    /// starts an outgoing drag from `origin` with the implicit grab of the
    /// press `serial`. `manager` and `device` belong to the current seat.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start<D>(
        &mut self,
        manager: &WlDataDeviceManager,
        device: &WlDataDevice,
        origin: &WlSurface,
        serial: u32,
        data: &DragData,
        actions: DragActions,
        owner: T,
        qh: &QueueHandle<D>,
    ) -> Result<(), &'static str>
    where
        D: Dispatch<WlDataSource, DragSourceData> + 'static,
    {
        if manager.version() < DRAG_AND_DROP_VERSION || device.version() < DRAG_AND_DROP_VERSION {
            return Err("the compositor does not support drag and drop actions");
        }
        if self.outgoing.is_some() {
            return Err("a drag is already active");
        }
        if actions.is_empty() {
            return Err("a drag needs at least one action");
        }
        let key = self.allocate().ok_or("drag identity space is exhausted")?;
        let source = manager.create_data_source(qh, DragSourceData { key });
        for mime_type in model::source_mime_types(data) {
            source.offer((*mime_type).to_owned());
        }
        source.set_actions(model::mask(actions));
        // icon surfaces are not supported yet; the compositor shows its own
        // pointer while the drag is active.
        device.start_drag(Some(&source), origin, None, serial);
        self.outgoing = Some(Outgoing {
            key,
            source,
            kind: data.kind(),
            bytes: Arc::from(model::encode(data)),
            owner,
            action: None,
        });
        Ok(())
    }

    /// a destination asked for the outgoing drag's data.
    pub(crate) fn send(&mut self, key: u64, mime_type: &str, fd: OwnedFd) {
        self.process_completions();
        let Some(outgoing) = self
            .outgoing
            .as_ref()
            .filter(|outgoing| outgoing.key == key)
        else {
            return;
        };
        if !model::serves(outgoing.kind, mime_type) {
            return;
        }
        if self.writes.len() >= MAX_ACTIVE_WRITES {
            eprintln!("thing: drag transfer queue is full");
            return;
        }
        let bytes = outgoing.bytes.clone();
        let Some(transfer) = self.allocate() else {
            return;
        };
        match self.worker.write(transfer, fd, bytes, TRANSFER_TIMEOUT) {
            Ok(()) => {
                self.writes.insert(transfer, key);
            }
            Err(error) => eprintln!("thing: cannot serve dragged data: {error}"),
        }
    }

    /// the destination's chosen action for the outgoing drag.
    pub(crate) fn source_action(&mut self, key: u64, mask: u32) {
        if let Some(outgoing) = self
            .outgoing
            .as_mut()
            .filter(|outgoing| outgoing.key == key)
        {
            outgoing.action = model::selected_action(mask);
        }
    }

    /// the outgoing drag ended. `finished` is true for `dnd_finished`, when
    /// the destination has read and accepted the data; only then does a
    /// move report success. the source is destroyed; writes already started
    /// keep their own copy of the bytes until they end.
    pub(crate) fn end_source(&mut self, key: u64, finished: bool) -> Option<(T, DragResult)> {
        let outgoing = self.outgoing.take_if(|outgoing| outgoing.key == key)?;
        outgoing.source.destroy();
        let result = match (finished, outgoing.action) {
            (true, Some(DragAction::Move)) => DragResult::Move,
            (true, Some(DragAction::Copy)) => DragResult::Copy,
            _ => DragResult::Cancelled,
        };
        Some((outgoing.owner, result))
    }

    /// cancels the outgoing drag if `retire` matches its owner.
    pub(crate) fn cancel_outgoing(&mut self, retire: impl Fn(&T) -> bool) -> Option<T> {
        let outgoing = self.outgoing.take_if(|outgoing| retire(&outgoing.owner))?;
        outgoing.source.destroy();
        Some(outgoing.owner)
    }

    /// ends the incoming drag over `surface`, which is being destroyed.
    pub(crate) fn retire_surface(&mut self, surface: &WlSurface) -> Option<u64> {
        if self
            .incoming
            .as_ref()
            .is_none_or(|incoming| &incoming.surface != surface)
        {
            return None;
        }
        self.retire_incoming()
    }

    /// retires a dropped offer whose owner matches, before or after its
    /// data arrived. a reading drop is reported through
    /// [`Self::take_deliveries`] as cancelled.
    pub(crate) fn retire_drop(&mut self, retire: impl Fn(&T) -> bool) {
        if self
            .reading
            .as_ref()
            .is_some_and(|reading| retire(&reading.owner))
        {
            self.retire_reading();
        }
    }

    /// the seat's device is going away: every protocol object it created is
    /// destroyed. returns the session that was over a surface and the
    /// outgoing drag's owner. a reading drop is reported through
    /// [`Self::take_deliveries`] as cancelled.
    pub(crate) fn detach(&mut self) -> (Option<u64>, Option<T>) {
        let incoming = self.retire_incoming();
        self.retire_reading();
        let outgoing = self.cancel_outgoing(|_| true);
        (incoming, outgoing)
    }

    #[cfg(test)]
    pub(crate) fn active_writes(&self) -> usize {
        self.writes.len()
    }

    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.incoming.is_none() && self.reading.is_none() && self.outgoing.is_none()
    }

    fn retire_incoming(&mut self) -> Option<u64> {
        let incoming = self.incoming.take()?;
        incoming.offer.destroy();
        Some(incoming.session)
    }

    fn retire_reading(&mut self) {
        let Some(reading) = self.reading.take() else {
            return;
        };
        if let ReadState::Reading { transfer } = reading.state {
            if let Err(error) = self.worker.cancel(transfer) {
                eprintln!("thing: cannot cancel a stale drop transfer: {error}");
            }
            self.retired.push(DropDelivery {
                session: reading.session,
                owner: reading.owner,
                action: reading.action,
                result: Err("the drop was replaced or its target went away".into()),
            });
        }
        reading.offer.destroy();
    }

    fn process_completions(&mut self) {
        for completion in self.worker.take_completions() {
            match completion {
                Completion::Read { id, result } => {
                    let Some(reading) = self.reading.as_mut().filter(|reading| {
                        matches!(reading.state, ReadState::Reading { transfer } if transfer == id)
                    }) else {
                        continue;
                    };
                    let result = result.map_err(|error| error.to_string()).and_then(|bytes| {
                        model::decode(reading.kind, bytes).map_err(|error| error.to_string())
                    });
                    let failed = result.is_err();
                    self.retired.push(DropDelivery {
                        session: reading.session,
                        owner: reading.owner.clone(),
                        action: reading.action,
                        result,
                    });
                    reading.state = ReadState::Delivered;
                    if failed {
                        // nothing was delivered, so the offer cannot finish.
                        let session = reading.session;
                        self.finish_drop(session, false);
                    }
                }
                Completion::Write { id, result } => {
                    if self.writes.remove(&id).is_some()
                        && let Err(error) = result
                    {
                        eprintln!("thing: drag transfer {id} failed: {error}");
                    }
                }
            }
        }
    }
}

impl<T> Drop for DragDrop<T> {
    fn drop(&mut self) {
        if let Some(incoming) = self.incoming.take() {
            incoming.offer.destroy();
        }
        if let Some(reading) = self.reading.take() {
            reading.offer.destroy();
        }
        if let Some(outgoing) = self.outgoing.take() {
            outgoing.source.destroy();
        }
    }
}

#[cfg(test)]
mod tests;
