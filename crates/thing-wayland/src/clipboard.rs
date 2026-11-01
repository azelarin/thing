//! wayland data-control integration for the regular clipboard.
//!
//! text is read eagerly when a selection arrives. images are read only when
//! a configuration asks: `read_image()` transfers the selection's
//! `image/png` and decodes it on the codec thread, and `set_image()` encodes
//! a pixmap there before offering it. every asynchronous result carries the
//! generation or request it was started for, so a stale transfer or decode
//! cannot restore a superseded offer.
//!
//! data control cannot say who set a selection: an offer carries only mime
//! types, and another client may advertise exactly the types of our own
//! source. a compositor also need not echo our own selection back, or may
//! echo it late. so a pending image is never matched against echoes. instead,
//! `set_image` sends a `wl_display.sync` after its request. selection events
//! delivered before that sync's `done` were sent before the compositor reached
//! the request, so the image is the newer value and may replace them. any
//! selection event delivered after `done` is a change the compositor made
//! after the request, whoever made it, and it cancels the pending image. a
//! late echo of our own earlier selection therefore cancels it too.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    os::fd::{AsFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use calloop::ping::Ping;
use thing_core::{ImageCandidate, ImageSource};
use thing_runtime::{ClipboardImageStatus, ClipboardSnapshot};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::GlobalList,
    protocol::{wl_callback::WlCallback, wl_seat::WlSeat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};

use crate::{
    data_transfer::{Completion, Worker},
    image_codec::Codec,
    text_data::{PNG_MIME_TYPE, TEXT_MIME_TYPES, pipe},
};

pub(crate) use crate::text_data::{OfferData, SourceData};

const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_OWNED_BYTES: usize = 4 * 1024 * 1024;
const MAX_OWNED_IMAGE_BYTES: usize = 2 * thing_ui::png::MAX_PNG_BYTES;
const MAX_SOURCES: usize = 16;
const MAX_ACTIVE_WRITES: usize = 16;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2);
const IMAGE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
/// what text reads may buffer, as for other text transfers.
const TEXT_READ_BUDGET: usize = 4 * 1024 * 1024;
/// one image read runs at a time, next to the text reads.
const READ_BUDGET: usize = thing_ui::png::MAX_PNG_BYTES + TEXT_READ_BUDGET;
const MAX_IMAGE_BARRIERS: usize = 64;

// the runtime refuses handles by the same limits the encoder applies.
const _: () = assert!(
    thing_runtime::MAX_CLIPBOARD_IMAGE_SIDE == thing_ui::png::MAX_PNG_DIMENSION
        && thing_runtime::MAX_CLIPBOARD_IMAGE_PIXELS == thing_ui::png::MAX_PNG_PIXELS
);

/// identities for decoded images, unique in the process, because renderer
/// caches key in-memory images by identity.
static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);

/// the user data of the `wl_display.sync` sent after a `set_image`. its
/// `done` marks the point where the compositor has processed the request.
pub(crate) struct ImageBarrier(pub(crate) u64);

/// the wayland dispatch a clipboard's proxies need.
pub(crate) trait ClipboardDispatch:
    Dispatch<ExtDataControlManagerV1, ()>
    + Dispatch<ExtDataControlDeviceV1, ()>
    + Dispatch<ExtDataControlSourceV1, SourceData>
    + Dispatch<ZwlrDataControlManagerV1, ()>
    + Dispatch<ZwlrDataControlDeviceV1, ()>
    + Dispatch<ZwlrDataControlSourceV1, SourceData>
    + Dispatch<WlCallback, ImageBarrier>
    + 'static
{
}

impl<T> ClipboardDispatch for T where
    T: Dispatch<ExtDataControlManagerV1, ()>
        + Dispatch<ExtDataControlDeviceV1, ()>
        + Dispatch<ExtDataControlSourceV1, SourceData>
        + Dispatch<ZwlrDataControlManagerV1, ()>
        + Dispatch<ZwlrDataControlDeviceV1, ()>
        + Dispatch<ZwlrDataControlSourceV1, SourceData>
        + Dispatch<WlCallback, ImageBarrier>
        + 'static
{
}

enum Backend {
    Ext {
        manager: ExtDataControlManagerV1,
        device: Option<ExtDataControlDeviceV1>,
    },
    Wlr {
        manager: ZwlrDataControlManagerV1,
        device: Option<ZwlrDataControlDeviceV1>,
    },
    Unsupported,
}

enum SelectionOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl SelectionOffer {
    fn destroy(self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }

    fn receive(&self, mime_type: String, writer: &OwnedFd) {
        match self {
            Self::Ext(offer) => offer.receive(mime_type, writer.as_fd()),
            Self::Wlr(offer) => offer.receive(mime_type, writer.as_fd()),
        }
    }
}

enum OwnedSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

struct RetainedSource {
    proxy: Option<OwnedSource>,
    bytes: usize,
    image: bool,
    active_writes: usize,
    cancelled: bool,
}

impl OwnedSource {
    fn destroy(self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }
}

/// the selection's image half of the published snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ImagePart {
    formats: Vec<String>,
    status: ClipboardImageStatus,
    image: Option<ImageSource>,
    error: String,
}

impl ImagePart {
    fn for_formats(formats: Vec<String>) -> Self {
        let status = if png_type(&formats).is_some() {
            ClipboardImageStatus::Available
        } else if formats
            .iter()
            .any(|format| format.trim().to_ascii_lowercase().starts_with("image/"))
        {
            ClipboardImageStatus::Unsupported
        } else {
            ClipboardImageStatus::None
        };
        Self {
            formats,
            status,
            ..Self::default()
        }
    }

    fn failed(&mut self, error: String) {
        self.status = ClipboardImageStatus::Failed;
        self.image = None;
        self.error = error;
    }
}

fn png_type(formats: &[String]) -> Option<&String> {
    formats
        .iter()
        .find(|format| format.trim().eq_ignore_ascii_case(PNG_MIME_TYPE))
}

struct State {
    /// the text half, as the read paths last set it.
    text: ClipboardSnapshot,
    image: ImagePart,
    current: ClipboardSnapshot,
    latest: Option<ClipboardSnapshot>,
    active_read: Option<u64>,
    next_transfer: u64,
    next_source: u64,
}

impl State {
    fn new(snapshot: ClipboardSnapshot) -> Self {
        Self {
            latest: Some(snapshot.clone()),
            text: snapshot.clone(),
            image: ImagePart::default(),
            current: snapshot,
            active_read: None,
            next_transfer: 1,
            next_source: 1,
        }
    }

    /// publishes a new text half with the current image half.
    fn publish(&mut self, ping: &Ping, snapshot: ClipboardSnapshot) {
        self.text = snapshot;
        self.republish(ping);
    }

    fn set_image(&mut self, ping: &Ping, image: ImagePart) {
        self.image = image;
        self.republish(ping);
    }

    fn republish(&mut self, ping: &Ping) {
        let mut snapshot = self.text.clone();
        if snapshot.available {
            snapshot.formats = self.image.formats.clone();
            snapshot.image_status = self.image.status;
            snapshot.image = self.image.image.clone();
            snapshot.image_error = self.image.error.clone();
        } else {
            self.image = ImagePart::default();
        }
        if self.current != snapshot {
            self.current = snapshot.clone();
            self.latest = Some(snapshot);
            ping.ping();
        }
    }

    fn allocate_transfer(&mut self) -> Result<u64, &'static str> {
        allocate(
            &mut self.next_transfer,
            "clipboard transfer id is exhausted",
        )
    }

    fn allocate_source(&mut self) -> Result<u64, &'static str> {
        allocate(&mut self.next_source, "clipboard source id is exhausted")
    }

    fn begin_read(&mut self, id: u64, ping: &Ping) {
        self.active_read = Some(id);
        self.publish(
            ping,
            ClipboardSnapshot {
                available: true,
                loading: true,
                ..ClipboardSnapshot::default()
            },
        );
    }

    fn no_text(&mut self, ping: &Ping) {
        self.active_read = None;
        self.publish(
            ping,
            ClipboardSnapshot {
                available: true,
                ..ClipboardSnapshot::default()
            },
        );
    }

    fn complete_read(
        &mut self,
        id: u64,
        result: Result<Vec<u8>, crate::data_transfer::TransferError>,
        ping: &Ping,
    ) {
        if self.active_read != Some(id) {
            return;
        }
        self.active_read = None;
        let snapshot = match result {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) if !text.contains('\0') => ClipboardSnapshot {
                    available: true,
                    text: Some(text),
                    ..ClipboardSnapshot::default()
                },
                Ok(_) => ClipboardSnapshot {
                    available: true,
                    error: "clipboard text contains a NUL byte".into(),
                    ..ClipboardSnapshot::default()
                },
                Err(_) => ClipboardSnapshot {
                    available: true,
                    error: "clipboard text is not valid UTF-8".into(),
                    ..ClipboardSnapshot::default()
                },
            },
            Err(error) => ClipboardSnapshot {
                available: true,
                error: format!("cannot read clipboard text: {error}"),
                ..ClipboardSnapshot::default()
            },
        };
        self.publish(ping, snapshot);
    }
}

/// an image copy waiting for the compositor's ordering marker and its png.
#[derive(Clone, Copy, PartialEq, Eq)]
struct EncodeRequest {
    request: u64,
    ready: bool,
}

pub(crate) struct Clipboard {
    backend: Backend,
    selection: Option<SelectionOffer>,
    sources: BTreeMap<u64, RetainedSource>,
    active_writes: BTreeMap<u64, u64>,
    owned_bytes: usize,
    owned_image_bytes: usize,
    state: State,
    transfer: Worker,
    codec: Codec,
    ping: Ping,
    /// advances with every selection event, seat change, and teardown.
    selection_generation: u64,
    image_read: Option<u64>,
    /// advances with every `set_text`, `set_image`, and `clear`.
    publish_request: u64,
    encode: Option<EncodeRequest>,
    barriers: BTreeSet<u64>,
}

impl Clipboard {
    pub(crate) fn bind<D: ClipboardDispatch>(
        globals: &GlobalList,
        qh: &QueueHandle<D>,
        ping: Ping,
    ) -> io::Result<Self> {
        let backend = bind_backend(globals, qh);
        let snapshot = match backend {
            Backend::Unsupported => ClipboardSnapshot {
                error: "the compositor does not support a data-control clipboard protocol".into(),
                ..ClipboardSnapshot::default()
            },
            _ => ClipboardSnapshot::default(),
        };
        Self::with_backend(backend, snapshot, ping)
    }

    fn with_backend(backend: Backend, snapshot: ClipboardSnapshot, ping: Ping) -> io::Result<Self> {
        Ok(Self {
            backend,
            selection: None,
            sources: BTreeMap::new(),
            active_writes: BTreeMap::new(),
            owned_bytes: 0,
            owned_image_bytes: 0,
            state: State::new(snapshot),
            transfer: Worker::with_read_budget(ping.clone(), READ_BUDGET)?,
            codec: Codec::start(ping.clone())?,
            ping,
            selection_generation: 0,
            image_read: None,
            publish_request: 0,
            encode: None,
            barriers: BTreeSet::new(),
        })
    }

    pub(crate) fn attach_seat<D: ClipboardDispatch>(&mut self, seat: &WlSeat, qh: &QueueHandle<D>) {
        self.process_completions();
        let attached = match &mut self.backend {
            Backend::Ext { manager, device } if device.is_none() => {
                *device = Some(manager.get_data_device(seat, qh, ()));
                true
            }
            Backend::Wlr { manager, device } if device.is_none() => {
                *device = Some(manager.get_data_device(seat, qh, ()));
                true
            }
            _ => false,
        };
        if attached {
            self.state.no_text(&self.ping);
        }
    }

    pub(crate) fn detach_seat(&mut self) {
        self.cancel_read();
        self.cancel_image_work();
        self.cancel_encode();
        if let Some(selection) = self.selection.take() {
            selection.destroy();
        }
        self.retire_sources();
        match &mut self.backend {
            Backend::Ext { device, .. } => {
                if let Some(device) = device.take() {
                    device.destroy();
                }
            }
            Backend::Wlr { device, .. } => {
                if let Some(device) = device.take() {
                    device.destroy();
                }
            }
            Backend::Unsupported => {}
        }
        self.state.publish(&self.ping, ClipboardSnapshot::default());
    }

    pub(crate) fn ext_offer(&mut self, offer: Option<ExtDataControlOfferV1>) {
        let offer = offer.map(|offer| {
            let data = offer.data::<OfferData>();
            let formats = data.map(OfferData::mime_types).unwrap_or_default();
            let text = data.and_then(OfferData::selected);
            (SelectionOffer::Ext(offer), formats, text)
        });
        self.selection_changed(offer);
    }

    pub(crate) fn wlr_offer(&mut self, offer: Option<ZwlrDataControlOfferV1>) {
        let offer = offer.map(|offer| {
            let data = offer.data::<OfferData>();
            let formats = data.map(OfferData::mime_types).unwrap_or_default();
            let text = data.and_then(OfferData::selected);
            (SelectionOffer::Wlr(offer), formats, text)
        });
        self.selection_changed(offer);
    }

    /// replaces the selection. reads and decodes for the previous one are
    /// cancelled or ignored, and its offer is destroyed now.
    fn selection_changed(&mut self, offer: Option<(SelectionOffer, Vec<String>, Option<String>)>) {
        self.cancel_read();
        self.cancel_image_work();
        if let Some(previous) = self.selection.take() {
            previous.destroy();
        }
        if self.encode.is_some_and(|request| request.ready) {
            self.cancel_encode();
        }
        let Some((offer, formats, text)) = offer else {
            self.state.image = ImagePart::default();
            self.state.no_text(&self.ping);
            return;
        };
        self.state.image = ImagePart::for_formats(formats);
        self.selection = Some(offer);
        match text {
            Some(mime_type) => self.receive_text(mime_type),
            None => self.state.no_text(&self.ping),
        }
    }

    pub(crate) fn ext_primary_offer(&self, offer: Option<ExtDataControlOfferV1>) {
        if let Some(offer) = offer {
            offer.destroy();
        }
    }

    pub(crate) fn wlr_primary_offer(&self, offer: Option<ZwlrDataControlOfferV1>) {
        if let Some(offer) = offer {
            offer.destroy();
        }
    }

    pub(crate) fn ext_finished(&mut self, device: &ExtDataControlDeviceV1) {
        let matches = matches!(
            &self.backend,
            Backend::Ext {
                device: Some(current),
                ..
            } if current == device
        );
        if matches {
            self.device_finished();
        }
    }

    pub(crate) fn wlr_finished(&mut self, device: &ZwlrDataControlDeviceV1) {
        let matches = matches!(
            &self.backend,
            Backend::Wlr {
                device: Some(current),
                ..
            } if current == device
        );
        if matches {
            self.device_finished();
        }
    }

    fn device_finished(&mut self) {
        self.detach_seat();
        self.state.publish(
            &self.ping,
            ClipboardSnapshot {
                error: "clipboard control is no longer available".into(),
                ..ClipboardSnapshot::default()
            },
        );
    }

    pub(crate) fn set_text<D: ClipboardDispatch>(
        &mut self,
        text: String,
        qh: &QueueHandle<D>,
    ) -> Result<(), &'static str> {
        self.process_completions();
        if !self.state.current.available {
            return Err("clipboard service is unavailable");
        }
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
        let key = self.state.allocate_source()?;
        let bytes: Arc<[u8]> = Arc::from(text.into_bytes());
        self.offer_source(SourceData::new(key, bytes), &TEXT_MIME_TYPES, qh)?;
        self.owned_bytes = retained;
        Ok(())
    }

    /// starts encoding `image` as a png. it reaches the clipboard on a later
    /// `update` unless a newer `set_text`, `set_image`, `clear`, or another
    /// application's selection comes first.
    pub(crate) fn set_image<D: ClipboardDispatch>(
        &mut self,
        image: &ImageSource,
        qh: &QueueHandle<D>,
    ) -> Result<(), String> {
        self.process_completions();
        if !self.state.current.available {
            return Err("clipboard service is unavailable".into());
        }
        let pixmap = thing_runtime::clipboard_image_pixmap(image)?;
        if self.barriers.len() >= MAX_IMAGE_BARRIERS {
            return Err("too many clipboard image requests are awaiting the compositor".into());
        }
        let backend = match &self.backend {
            Backend::Ext { manager, .. } => manager.backend(),
            Backend::Wlr { manager, .. } => manager.backend(),
            Backend::Unsupported => return Err("clipboard service is unavailable".into()),
        }
        .upgrade()
        .ok_or("the clipboard connection has ended")?;
        self.cancel_encode();
        self.publish_request += 1;
        self.encode = Some(EncodeRequest {
            request: self.publish_request,
            ready: false,
        });
        self.barriers.insert(self.publish_request);
        Connection::from_backend(backend)
            .display()
            .sync(qh, ImageBarrier(self.publish_request));
        self.codec.encode(self.publish_request, pixmap.clone());
        Ok(())
    }

    pub(crate) fn image_barrier_done(&mut self, request: u64) {
        self.barriers.remove(&request);
        if let Some(current) = self
            .encode
            .as_mut()
            .filter(|current| current.request == request)
        {
            current.ready = true;
            self.ping.ping();
        }
    }

    fn cancel_encode(&mut self) {
        self.encode = None;
        self.codec.cancel_encode();
    }

    pub(crate) fn clear(&mut self) -> Result<(), &'static str> {
        match &self.backend {
            Backend::Ext {
                device: Some(device),
                ..
            } => device.set_selection(None),
            Backend::Wlr {
                device: Some(device),
                ..
            } => device.set_selection(None),
            _ => return Err("clipboard service is unavailable"),
        }
        self.publish_request += 1;
        self.cancel_encode();
        Ok(())
    }

    /// starts transferring and decoding the selection's png.
    pub(crate) fn read_image(&mut self) -> Result<(), &'static str> {
        self.process_completions();
        if !self.state.current.available {
            return Err("clipboard service is unavailable");
        }
        match self.state.image.status {
            ClipboardImageStatus::Loading | ClipboardImageStatus::Ready => return Ok(()),
            ClipboardImageStatus::None => return Err("the clipboard has no image"),
            ClipboardImageStatus::Unsupported => {
                return Err("the clipboard image is not offered as png");
            }
            ClipboardImageStatus::Available | ClipboardImageStatus::Failed => {}
        }
        let (Some(selection), Some(mime_type)) = (
            self.selection.as_ref(),
            png_type(&self.state.image.formats).cloned(),
        ) else {
            return Err("the clipboard has no image");
        };
        let started = pipe().and_then(|(reader, writer)| {
            selection.receive(mime_type, &writer);
            drop(writer);
            let id = self.state.allocate_transfer().map_err(io::Error::other)?;
            self.transfer.read(
                id,
                reader,
                thing_ui::png::MAX_PNG_BYTES,
                IMAGE_TRANSFER_TIMEOUT,
            )?;
            Ok(id)
        });
        let mut image = ImagePart {
            formats: self.state.image.formats.clone(),
            ..ImagePart::default()
        };
        match started {
            Ok(id) => {
                self.image_read = Some(id);
                image.status = ClipboardImageStatus::Loading;
            }
            Err(error) => image.failed(format!("cannot read the clipboard image: {error}")),
        }
        self.state.set_image(&self.ping, image);
        Ok(())
    }

    pub(crate) fn send(&mut self, data: &SourceData, mime_type: &str, fd: OwnedFd) {
        if !data.serves(mime_type) {
            return;
        }
        self.process_completions();
        if self.active_writes.len() >= MAX_ACTIVE_WRITES {
            eprintln!("thing: clipboard transfer queue is full");
            return;
        }
        let Some(source) = self.sources.get(&data.key()) else {
            return;
        };
        if source.cancelled {
            return;
        }
        let id = match self.state.allocate_transfer() {
            Ok(id) => id,
            Err(error) => {
                eprintln!("thing: {error}");
                return;
            }
        };
        let timeout = if data.is_png() {
            IMAGE_TRANSFER_TIMEOUT
        } else {
            TRANSFER_TIMEOUT
        };
        match self.transfer.write(id, fd, data.bytes(), timeout) {
            Ok(()) => {
                self.active_writes.insert(id, data.key());
                self.sources
                    .get_mut(&data.key())
                    .expect("validated clipboard source must remain present")
                    .active_writes += 1;
            }
            Err(error) => eprintln!("thing: cannot serve clipboard data: {error}"),
        }
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

    /// applies finished work, publishes an encoded image whose request is
    /// still current, and returns a changed snapshot.
    pub(crate) fn update<D: ClipboardDispatch>(
        &mut self,
        qh: &QueueHandle<D>,
    ) -> Option<ClipboardSnapshot> {
        self.process_completions();
        if self.encode.is_some_and(|request| request.ready)
            && let Some(encoded) = self.codec.take_encoded()
        {
            if self
                .encode
                .is_none_or(|current| encoded.request != current.request)
            {
                return self.state.latest.take();
            }
            self.encode = None;
            let result = encoded
                .result
                .and_then(|png| self.publish_png(png, qh).map_err(str::to_owned));
            if let Err(error) = result {
                eprintln!("thing: cannot copy the image: {error}");
            }
        }
        self.state.latest.take()
    }

    fn publish_png<D: ClipboardDispatch>(
        &mut self,
        png: Vec<u8>,
        qh: &QueueHandle<D>,
    ) -> Result<(), &'static str> {
        if !self.state.current.available {
            return Err("clipboard service is unavailable");
        }
        let retained = self
            .owned_image_bytes
            .checked_add(png.len())
            .ok_or("clipboard image storage is full")?;
        if self.sources.len() >= MAX_SOURCES || retained > MAX_OWNED_IMAGE_BYTES {
            return Err("clipboard image storage is full");
        }
        let key = self.state.allocate_source()?;
        let bytes: Arc<[u8]> = Arc::from(png);
        self.offer_source(SourceData::png(key, bytes), &[PNG_MIME_TYPE], qh)?;
        self.owned_image_bytes = retained;
        Ok(())
    }

    fn offer_source<D: ClipboardDispatch>(
        &mut self,
        data: SourceData,
        mime_types: &[&str],
        qh: &QueueHandle<D>,
    ) -> Result<(), &'static str> {
        let key = data.key();
        let bytes = data.bytes().len();
        let image = data.is_png();
        let source = match &self.backend {
            Backend::Ext {
                manager,
                device: Some(device),
            } => {
                let source = manager.create_data_source(qh, data);
                for mime_type in mime_types {
                    source.offer((*mime_type).into());
                }
                device.set_selection(Some(&source));
                OwnedSource::Ext(source)
            }
            Backend::Wlr {
                manager,
                device: Some(device),
            } => {
                let source = manager.create_data_source(qh, data);
                for mime_type in mime_types {
                    source.offer((*mime_type).into());
                }
                device.set_selection(Some(&source));
                OwnedSource::Wlr(source)
            }
            _ => return Err("clipboard service is unavailable"),
        };
        // a newer value supersedes any image still encoding.
        self.publish_request += 1;
        self.cancel_encode();
        self.sources.insert(
            key,
            RetainedSource {
                proxy: Some(source),
                bytes,
                image,
                active_writes: 0,
                cancelled: false,
            },
        );
        Ok(())
    }

    fn process_completions(&mut self) {
        self.process_transfers();
        self.apply_decoded();
    }

    /// applies finished transfers without applying decoded images.
    #[cfg(test)]
    pub(crate) fn process_transfers_only(&mut self) {
        self.process_transfers();
    }

    fn process_transfers(&mut self) {
        for completion in self.transfer.take_completions() {
            match completion {
                Completion::Read { id, result } if self.image_read == Some(id) => {
                    self.image_read = None;
                    match result {
                        Ok(bytes) => self.codec.decode(self.selection_generation, bytes),
                        Err(error) => {
                            let mut image = self.state.image.clone();
                            image.failed(format!("cannot read the clipboard image: {error}"));
                            self.state.set_image(&self.ping, image);
                        }
                    }
                }
                Completion::Read { id, result } => {
                    self.state.complete_read(id, result, &self.ping);
                }
                Completion::Write {
                    id,
                    result: Err(error),
                } => {
                    self.finish_write(id);
                    eprintln!("thing: clipboard transfer {id} failed: {error}");
                }
                Completion::Write { id, result: Ok(()) } => self.finish_write(id),
            }
        }
    }

    fn apply_decoded(&mut self) {
        if let Some(decoded) = self.codec.take_decoded() {
            if decoded.generation != self.selection_generation
                || self.state.image.status != ClipboardImageStatus::Loading
            {
                return;
            }
            let mut image = self.state.image.clone();
            let source = decoded.result.and_then(|pixmap| {
                ImageCandidate::new(None, vec![pixmap]).and_then(|candidate| {
                    ImageSource::new(
                        format!(
                            "clipboard-image:{}",
                            NEXT_IMAGE.fetch_add(1, Ordering::Relaxed)
                        ),
                        vec![candidate],
                    )
                })
            });
            match source {
                Ok(source) => {
                    image.status = ClipboardImageStatus::Ready;
                    image.image = Some(source);
                    image.error.clear();
                }
                Err(error) => image.failed(format!("cannot decode the clipboard image: {error}")),
            }
            self.state.set_image(&self.ping, image);
        }
    }

    fn finish_write(&mut self, id: u64) {
        let Some(key) = self.active_writes.remove(&id) else {
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
            if source.image {
                self.owned_image_bytes = self.owned_image_bytes.saturating_sub(source.bytes);
            } else {
                self.owned_bytes = self.owned_bytes.saturating_sub(source.bytes);
            }
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

    fn receive_text(&mut self, mime_type: String) {
        let Some(selection) = &self.selection else {
            return;
        };
        match pipe() {
            Ok((reader, writer)) => {
                selection.receive(mime_type, &writer);
                drop(writer);
                self.start_read(reader);
            }
            Err(error) => self.read_start_failed(error),
        }
    }

    fn start_read(&mut self, reader: OwnedFd) {
        let id = match self.state.allocate_transfer() {
            Ok(id) => id,
            Err(error) => {
                self.read_start_failed(io::Error::other(error));
                return;
            }
        };
        match self
            .transfer
            .read(id, reader, MAX_TEXT_BYTES, TRANSFER_TIMEOUT)
        {
            Ok(()) => self.state.begin_read(id, &self.ping),
            Err(error) => self.read_start_failed(error),
        }
    }

    fn read_start_failed(&mut self, error: io::Error) {
        self.state.active_read = None;
        self.state.publish(
            &self.ping,
            ClipboardSnapshot {
                available: true,
                error: format!("cannot read clipboard text: {error}"),
                ..ClipboardSnapshot::default()
            },
        );
    }

    fn cancel_read(&mut self) {
        if let Some(id) = self.state.active_read.take()
            && let Err(error) = self.transfer.cancel(id)
        {
            eprintln!("thing: cannot cancel stale clipboard read: {error}");
        }
    }

    /// abandons the image transfer and any decode for the old selection.
    fn cancel_image_work(&mut self) {
        self.selection_generation += 1;
        self.codec.cancel_decode();
        if let Some(id) = self.image_read.take()
            && let Err(error) = self.transfer.cancel(id)
        {
            eprintln!("thing: cannot cancel stale clipboard image read: {error}");
        }
    }

    fn destroy_sources(&mut self) {
        for (_, mut source) in std::mem::take(&mut self.sources) {
            if let Some(proxy) = source.proxy.take() {
                proxy.destroy();
            }
        }
        self.active_writes.clear();
        self.owned_bytes = 0;
        self.owned_image_bytes = 0;
    }
}

fn bind_backend<D>(globals: &GlobalList, qh: &QueueHandle<D>) -> Backend
where
    D: Dispatch<ExtDataControlManagerV1, ()> + Dispatch<ZwlrDataControlManagerV1, ()> + 'static,
{
    if let Ok(manager) = globals.bind(qh, 1..=1, ()) {
        Backend::Ext {
            manager,
            device: None,
        }
    } else if let Ok(manager) = globals.bind(qh, 1..=2, ()) {
        Backend::Wlr {
            manager,
            device: None,
        }
    } else {
        Backend::Unsupported
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        if let Some(selection) = self.selection.take() {
            selection.destroy();
        }
        self.destroy_sources();
        match &mut self.backend {
            Backend::Ext { manager, device } => {
                if let Some(device) = device.take() {
                    device.destroy();
                }
                manager.destroy();
            }
            Backend::Wlr { manager, device } => {
                if let Some(device) = device.take() {
                    device.destroy();
                }
                manager.destroy();
            }
            Backend::Unsupported => {}
        }
    }
}

fn allocate(next: &mut u64, exhausted: &'static str) -> Result<u64, &'static str> {
    let current = *next;
    *next = next.checked_add(1).ok_or(exhausted)?;
    Ok(current)
}

#[cfg(test)]
#[path = "clipboard_tests.rs"]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        os::unix::net::UnixStream,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };
    use wayland_client::{
        Connection,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };
    use wayland_protocols::ext::data_control::v1::server::ext_data_control_manager_v1::ExtDataControlManagerV1 as ServerExtManager;
    use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1 as ServerWlrManager;
    use wayland_server::{
        Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    };

    struct ClientState;

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    wayland_client::delegate_noop!(ClientState: ignore ExtDataControlManagerV1);
    wayland_client::delegate_noop!(ClientState: ignore ZwlrDataControlManagerV1);

    #[derive(Default)]
    struct ServerState;

    impl GlobalDispatch<ServerExtManager, ()> for ServerState {
        fn bind(
            _: &mut Self,
            _: &DisplayHandle,
            _: &Client,
            resource: New<ServerExtManager>,
            _: &(),
            data: &mut DataInit<'_, Self>,
        ) {
            data.init(resource, ());
        }
    }

    impl ServerDispatch<ServerExtManager, ()> for ServerState {
        fn request(
            _: &mut Self,
            _: &Client,
            _: &ServerExtManager,
            _: <ServerExtManager as wayland_server::Resource>::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl GlobalDispatch<ServerWlrManager, ()> for ServerState {
        fn bind(
            _: &mut Self,
            _: &DisplayHandle,
            _: &Client,
            resource: New<ServerWlrManager>,
            _: &(),
            data: &mut DataInit<'_, Self>,
        ) {
            data.init(resource, ());
        }
    }

    impl ServerDispatch<ServerWlrManager, ()> for ServerState {
        fn request(
            _: &mut Self,
            _: &Client,
            _: &ServerWlrManager,
            _: <ServerWlrManager as wayland_server::Resource>::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
        }
    }

    struct PrivateRegistry {
        globals: GlobalList,
        queue: wayland_client::EventQueue<ClientState>,
        stop: Arc<AtomicBool>,
        server: Option<thread::JoinHandle<()>>,
    }

    impl Drop for PrivateRegistry {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(server) = self.server.take() {
                server.join().unwrap();
            }
        }
    }

    fn private_registry(ext: bool, wlr: bool) -> PrivateRegistry {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        server_socket.set_nonblocking(true).unwrap();
        let mut display = Display::<ServerState>::new().unwrap();
        let mut handle = display.handle();
        let client = handle.insert_client(server_socket, Arc::new(())).unwrap();
        if ext {
            handle.create_global::<ServerState, ServerExtManager, _>(1, ());
        }
        if wlr {
            handle.create_global::<ServerState, ServerWlrManager, _>(2, ());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut state = ServerState;
            while !server_stop.load(Ordering::Relaxed) {
                match display
                    .backend()
                    .dispatch_single_client(&mut state, client.id())
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("private wayland server failed: {error}"),
                }
                display.backend().flush(Some(client.id())).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        let connection = Connection::from_socket(client_socket).unwrap();
        let (globals, queue) = registry_queue_init(&connection).unwrap();
        PrivateRegistry {
            globals,
            queue,
            stop,
            server: Some(server),
        }
    }

    #[test]
    fn stale_read_completion_cannot_replace_current_text() {
        let (ping, _) = calloop::ping::make_ping().unwrap();
        let mut state = State::new(ClipboardSnapshot {
            available: true,
            ..ClipboardSnapshot::default()
        });
        state.begin_read(1, &ping);
        state.begin_read(2, &ping);
        state.complete_read(1, Ok(b"stale".to_vec()), &ping);
        assert!(state.current.loading);
        state.complete_read(2, Ok(b"current".to_vec()), &ping);
        assert_eq!(state.current.text.as_deref(), Some("current"));
    }

    #[test]
    fn source_and_transfer_ids_never_wrap() {
        let mut next = u64::MAX;
        assert!(allocate(&mut next, "exhausted").is_err());
        assert_eq!(next, u64::MAX);
    }

    #[test]
    fn source_bytes_survive_repeated_async_sends() {
        let (ping, _) = calloop::ping::make_ping().unwrap();
        let worker = Worker::start(ping).unwrap();
        let data = SourceData::new(1, Arc::from(&b"clipboard"[..]));
        let mut readers = Vec::new();
        for id in 1..=2 {
            let (reader, writer) = UnixStream::pair().unwrap();
            worker
                .write(id, writer.into(), data.bytes(), Duration::from_secs(1))
                .unwrap();
            readers.push(reader);
        }
        drop(data);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut completed = 0;
        while completed < 2 {
            for completion in worker.take_completions() {
                let Completion::Write { result, .. } = completion else {
                    panic!("expected a write completion");
                };
                result.unwrap();
                completed += 1;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "clipboard writes did not complete"
            );
            thread::sleep(Duration::from_millis(2));
        }
        for mut reader in readers {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"clipboard");
        }
    }

    #[test]
    fn cancelled_sources_stay_accounted_until_writes_finish() {
        let (ping, _) = calloop::ping::make_ping().unwrap();
        let mut clipboard =
            Clipboard::with_backend(Backend::Unsupported, ClipboardSnapshot::default(), ping)
                .unwrap();
        clipboard.sources = BTreeMap::from([(
            7,
            RetainedSource {
                proxy: None,
                bytes: MAX_TEXT_BYTES,
                image: false,
                active_writes: 1,
                cancelled: false,
            },
        )]);
        clipboard.active_writes = BTreeMap::from([(11, 7)]);
        clipboard.owned_bytes = MAX_TEXT_BYTES;

        clipboard.cancelled(7);
        assert_eq!(clipboard.owned_bytes, MAX_TEXT_BYTES);
        assert!(clipboard.sources[&7].cancelled);
        clipboard.finish_write(11);
        assert_eq!(clipboard.owned_bytes, 0);
        assert!(clipboard.sources.is_empty());
    }

    #[test]
    fn private_registry_prefers_ext_and_falls_back_to_wlr() {
        let mut registry = private_registry(true, true);
        assert!(matches!(
            bind_backend(&registry.globals, &registry.queue.handle()),
            Backend::Ext { .. }
        ));
        registry.queue.roundtrip(&mut ClientState).unwrap();

        let mut registry = private_registry(false, true);
        assert!(matches!(
            bind_backend(&registry.globals, &registry.queue.handle()),
            Backend::Wlr { .. }
        ));
        registry.queue.roundtrip(&mut ClientState).unwrap();

        let registry = private_registry(false, false);
        assert!(matches!(
            bind_backend(&registry.globals, &registry.queue.handle()),
            Backend::Unsupported
        ));
    }
}
