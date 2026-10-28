use std::{
    collections::BTreeMap,
    io,
    os::fd::{AsFd, OwnedFd},
    sync::Arc,
    time::Duration,
};

use calloop::ping::Ping;
use wayland_client::{
    Dispatch, Proxy, QueueHandle,
    globals::GlobalList,
    protocol::{
        wl_data_device::WlDataDevice, wl_data_device_manager::WlDataDeviceManager,
        wl_data_offer::WlDataOffer, wl_data_source::WlDataSource, wl_seat::WlSeat,
    },
};

use crate::{
    data_transfer::{Completion, TransferError, Worker},
    text_data::{OfferData, SourceData, TEXT_MIME_TYPES, pipe, supports_mime_type},
};

const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_OWNED_BYTES: usize = 4 * 1024 * 1024;
const MAX_SOURCES: usize = 16;
const MAX_ACTIVE_WRITES: usize = 16;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PasteId(u64);

#[derive(Debug)]
pub(crate) struct PasteOutcome {
    pub(crate) id: PasteId,
    pub(crate) result: Result<String, PasteError>,
}

#[derive(Debug)]
pub(crate) enum PasteError {
    Transfer(TransferError),
    InvalidUtf8,
    ContainsNul,
}

impl std::fmt::Display for PasteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transfer(error) => write!(formatter, "{error}"),
            Self::InvalidUtf8 => formatter.write_str("clipboard text is not valid UTF-8"),
            Self::ContainsNul => formatter.write_str("clipboard text contains a NUL byte"),
        }
    }
}

impl std::error::Error for PasteError {}

struct ActivePaste {
    transfer: u64,
    paste: PasteId,
}

struct RetainedSource {
    proxy: Option<WlDataSource>,
    bytes: usize,
    active_writes: usize,
    cancelled: bool,
}

pub(crate) struct FocusedClipboard {
    manager: Option<WlDataDeviceManager>,
    seat: Option<WlSeat>,
    device: Option<WlDataDevice>,
    selection: Option<WlDataOffer>,
    drag_offer: Option<WlDataOffer>,
    sources: BTreeMap<u64, RetainedSource>,
    active_writes: BTreeMap<u64, u64>,
    owned_bytes: usize,
    active_paste: Option<ActivePaste>,
    paste_result: Option<PasteOutcome>,
    next_transfer: u64,
    next_source: u64,
    transfer: Worker,
}

impl FocusedClipboard {
    pub(crate) fn bind<D>(globals: &GlobalList, qh: &QueueHandle<D>, ping: Ping) -> io::Result<Self>
    where
        D: Dispatch<WlDataDeviceManager, ()> + 'static,
    {
        Ok(Self {
            manager: globals.bind(qh, 1..=3, ()).ok(),
            seat: None,
            device: None,
            selection: None,
            drag_offer: None,
            sources: BTreeMap::new(),
            active_writes: BTreeMap::new(),
            owned_bytes: 0,
            active_paste: None,
            paste_result: None,
            next_transfer: 1,
            next_source: 1,
            transfer: Worker::start(ping)?,
        })
    }

    #[cfg(test)]
    pub(crate) fn available(&self) -> bool {
        self.manager.is_some() && self.device.is_some()
    }

    pub(crate) fn attach_seat<D>(&mut self, seat: &WlSeat, qh: &QueueHandle<D>)
    where
        D: Dispatch<WlDataDevice, ()> + 'static,
    {
        if self.seat.as_ref() == Some(seat) && self.device.is_some() {
            return;
        }
        self.detach_seat();
        let Some(manager) = &self.manager else {
            return;
        };
        self.seat = Some(seat.clone());
        self.device = Some(manager.get_data_device(seat, qh, ()));
    }

    pub(crate) fn detach_seat(&mut self) {
        self.process_completions();
        self.cancel_paste();
        self.paste_result = None;
        self.destroy_offer();
        self.destroy_drag_offer();
        self.retire_sources();
        if let Some(device) = self.device.take()
            && device.version() >= 2
        {
            device.release();
        }
        self.seat = None;
    }

    pub(crate) fn selection(&mut self, device: &WlDataDevice, offer: Option<WlDataOffer>) {
        if self.device.as_ref() != Some(device) {
            if let Some(offer) = offer {
                offer.destroy();
            }
            return;
        }
        self.destroy_offer();
        if let Some(offer) = &offer
            && self.drag_offer.as_ref() == Some(offer)
        {
            self.drag_offer = None;
        }
        self.selection = offer;
    }

    pub(crate) fn drag_enter(
        &mut self,
        device: &WlDataDevice,
        serial: u32,
        offer: Option<WlDataOffer>,
    ) {
        if self.device.as_ref() != Some(device) {
            if let Some(offer) = offer {
                offer.destroy();
            }
            return;
        }
        self.destroy_drag_offer();
        if let Some(offer) = offer {
            offer.accept(serial, None);
            self.drag_offer = Some(offer);
        }
    }

    pub(crate) fn drag_finished(&mut self, device: &WlDataDevice) {
        if self.device.as_ref() == Some(device) {
            self.destroy_drag_offer();
        }
    }

    pub(crate) fn start_paste(&mut self) -> io::Result<PasteId> {
        self.process_completions();
        self.cancel_paste();
        self.paste_result = None;
        let offer = self
            .selection
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "clipboard has no selection"))?;
        let mime_type = offer
            .data::<OfferData>()
            .and_then(OfferData::selected)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "clipboard has no text offer")
            })?;
        let transfer = allocate(
            &mut self.next_transfer,
            "focused clipboard transfer id is exhausted",
        )
        .map_err(io::Error::other)?;
        let (reader, writer) = pipe()?;
        offer.receive(mime_type, writer.as_fd());
        drop(writer);
        self.transfer
            .read(transfer, reader, MAX_TEXT_BYTES, TRANSFER_TIMEOUT)?;
        let paste = PasteId(transfer);
        self.active_paste = Some(ActivePaste { transfer, paste });
        Ok(paste)
    }

    pub(crate) fn take_paste_outcome(&mut self) -> Option<PasteOutcome> {
        self.process_completions();
        self.paste_result.take()
    }

    pub(crate) fn cancel_pending_paste(&mut self) {
        self.process_completions();
        self.cancel_paste();
        self.paste_result = None;
    }

    pub(crate) fn set_text<D>(
        &mut self,
        text: String,
        serial: u32,
        qh: &QueueHandle<D>,
    ) -> Result<(), &'static str>
    where
        D: Dispatch<WlDataSource, SourceData> + 'static,
    {
        self.process_completions();
        if text.len() > MAX_TEXT_BYTES || text.contains('\0') {
            return Err("clipboard text must contain no NUL and must not exceed 1 MiB");
        }
        let retained = self
            .owned_bytes
            .checked_add(text.len())
            .ok_or("clipboard source storage is full")?;
        if self.sources.len() >= MAX_SOURCES || retained > MAX_OWNED_BYTES {
            return Err("clipboard source storage is full");
        }
        let manager = self
            .manager
            .as_ref()
            .ok_or("standard clipboard is unavailable")?;
        let device = self
            .device
            .as_ref()
            .ok_or("standard clipboard is unavailable")?;
        let key = allocate(
            &mut self.next_source,
            "focused clipboard source id is exhausted",
        )?;
        let bytes: Arc<[u8]> = Arc::from(text.into_bytes());
        let source = manager.create_data_source(qh, SourceData::new(key, bytes.clone()));
        for mime_type in TEXT_MIME_TYPES {
            source.offer(mime_type.into());
        }
        device.set_selection(Some(&source), serial);
        self.owned_bytes = retained;
        self.sources.insert(
            key,
            RetainedSource {
                proxy: Some(source),
                bytes: bytes.len(),
                active_writes: 0,
                cancelled: false,
            },
        );
        Ok(())
    }

    pub(crate) fn send(&mut self, data: &SourceData, mime_type: &str, fd: OwnedFd) {
        if !supports_mime_type(mime_type) {
            return;
        }
        self.process_completions();
        if self.active_writes.len() >= MAX_ACTIVE_WRITES {
            eprintln!("thing: focused clipboard transfer queue is full");
            return;
        }
        let Some(source) = self.sources.get(&data.key()) else {
            return;
        };
        if source.cancelled {
            return;
        }
        let transfer = match allocate(
            &mut self.next_transfer,
            "focused clipboard transfer id is exhausted",
        ) {
            Ok(transfer) => transfer,
            Err(error) => {
                eprintln!("thing: {error}");
                return;
            }
        };
        if let Err(error) = self
            .transfer
            .write(transfer, fd, data.bytes(), TRANSFER_TIMEOUT)
        {
            eprintln!("thing: cannot serve focused clipboard text: {error}");
            return;
        }
        self.active_writes.insert(transfer, data.key());
        self.sources
            .get_mut(&data.key())
            .expect("validated focused clipboard source must remain present")
            .active_writes += 1;
    }

    pub(crate) fn cancelled(&mut self, key: u64) {
        self.process_completions();
        let remove = if let Some(source) = self.sources.get_mut(&key) {
            if let Some(proxy) = source.proxy.take() {
                proxy.destroy();
            }
            source.cancelled = true;
            source.active_writes == 0
        } else {
            false
        };
        if remove {
            self.remove_source(key);
        }
    }

    fn process_completions(&mut self) {
        for completion in self.transfer.take_completions() {
            match completion {
                Completion::Read { id, result } => {
                    let Some(active) = self.active_paste.take() else {
                        continue;
                    };
                    if active.transfer != id {
                        self.active_paste = Some(active);
                        continue;
                    }
                    self.paste_result = Some(PasteOutcome {
                        id: active.paste,
                        result: decode(result),
                    });
                }
                Completion::Write {
                    id,
                    result: Err(error),
                } => {
                    self.finish_write(id);
                    eprintln!("thing: focused clipboard transfer {id} failed: {error}");
                }
                Completion::Write { id, result: Ok(()) } => self.finish_write(id),
            }
        }
    }

    fn cancel_paste(&mut self) {
        if let Some(active) = self.active_paste.take()
            && let Err(error) = self.transfer.cancel(active.transfer)
        {
            eprintln!("thing: cannot cancel stale focused clipboard paste: {error}");
        }
    }

    fn finish_write(&mut self, transfer: u64) {
        let Some(key) = self.active_writes.remove(&transfer) else {
            return;
        };
        let remove = if let Some(source) = self.sources.get_mut(&key) {
            source.active_writes = source.active_writes.saturating_sub(1);
            source.cancelled && source.active_writes == 0
        } else {
            false
        };
        if remove {
            self.remove_source(key);
        }
    }

    fn remove_source(&mut self, key: u64) {
        if let Some(source) = self.sources.remove(&key) {
            self.owned_bytes = self.owned_bytes.saturating_sub(source.bytes);
        }
    }

    fn retire_sources(&mut self) {
        let mut removable = Vec::new();
        for (key, source) in &mut self.sources {
            if let Some(proxy) = source.proxy.take() {
                proxy.destroy();
            }
            source.cancelled = true;
            if source.active_writes == 0 {
                removable.push(*key);
            }
        }
        for key in removable {
            self.remove_source(key);
        }
    }

    fn destroy_offer(&mut self) {
        if let Some(offer) = self.selection.take() {
            offer.destroy();
        }
    }

    fn destroy_drag_offer(&mut self) {
        if let Some(offer) = self.drag_offer.take() {
            offer.destroy();
        }
    }
}

impl Drop for FocusedClipboard {
    fn drop(&mut self) {
        self.detach_seat();
    }
}

fn decode(result: Result<Vec<u8>, TransferError>) -> Result<String, PasteError> {
    let bytes = result.map_err(PasteError::Transfer)?;
    let text = String::from_utf8(bytes).map_err(|_| PasteError::InvalidUtf8)?;
    if text.contains('\0') {
        return Err(PasteError::ContainsNul);
    }
    Ok(text)
}

fn allocate(next: &mut u64, exhausted: &'static str) -> Result<u64, &'static str> {
    let current = *next;
    *next = next.checked_add(1).ok_or(exhausted)?;
    Ok(current)
}

#[cfg(test)]
mod tests;
