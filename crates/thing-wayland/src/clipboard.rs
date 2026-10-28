//! wayland data-control integration for the regular clipboard.

use std::{
    collections::BTreeMap,
    io,
    os::fd::{AsFd, FromRawFd, OwnedFd},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use calloop::ping::Ping;
use thing_runtime::ClipboardSnapshot;
use wayland_client::{
    Dispatch, Proxy, QueueHandle, globals::GlobalList, protocol::wl_seat::WlSeat,
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

use crate::data_transfer::{Completion, Worker};

const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_OWNED_BYTES: usize = 4 * 1024 * 1024;
const MAX_SOURCES: usize = 16;
const MAX_ACTIVE_WRITES: usize = 16;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2);
const MIME_UTF8: &str = "text/plain;charset=utf-8";
const MIME_TEXT: &str = "text/plain";
const MIME_X11_UTF8: &str = "UTF8_STRING";

pub(crate) struct OfferData(Mutex<Option<MimeChoice>>);

impl Default for OfferData {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl OfferData {
    pub(crate) fn offer(&self, mime_type: String) {
        let Some(priority) = mime_priority(&mime_type) else {
            return;
        };
        let mut selected = lock(&self.0);
        if selected
            .as_ref()
            .is_none_or(|current| priority < current.priority)
        {
            *selected = Some(MimeChoice {
                priority,
                mime_type,
            });
        }
    }

    fn selected(&self) -> Option<String> {
        lock(&self.0)
            .as_ref()
            .map(|choice| choice.mime_type.clone())
    }
}

struct MimeChoice {
    priority: u8,
    mime_type: String,
}

pub(crate) struct SourceData {
    key: u64,
    bytes: Arc<[u8]>,
}

impl SourceData {
    fn new(key: u64, bytes: Arc<[u8]>) -> Self {
        Self { key, bytes }
    }

    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    pub(crate) fn bytes(&self) -> Arc<[u8]> {
        self.bytes.clone()
    }
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
}

enum OwnedSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

struct RetainedSource {
    proxy: Option<OwnedSource>,
    bytes: usize,
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

struct State {
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
            current: snapshot,
            active_read: None,
            next_transfer: 1,
            next_source: 1,
        }
    }

    fn publish(&mut self, ping: &Ping, snapshot: ClipboardSnapshot) {
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

pub(crate) struct Clipboard {
    backend: Backend,
    selection: Option<SelectionOffer>,
    sources: BTreeMap<u64, RetainedSource>,
    active_writes: BTreeMap<u64, u64>,
    owned_bytes: usize,
    state: State,
    transfer: Worker,
    ping: Ping,
}

impl Clipboard {
    pub(crate) fn bind(
        globals: &GlobalList,
        qh: &QueueHandle<crate::host::App>,
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
        Ok(Self {
            backend,
            selection: None,
            sources: BTreeMap::new(),
            active_writes: BTreeMap::new(),
            owned_bytes: 0,
            state: State::new(snapshot),
            transfer: Worker::start(ping.clone())?,
            ping,
        })
    }

    pub(crate) fn attach_seat(&mut self, seat: &WlSeat, qh: &QueueHandle<crate::host::App>) {
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
        self.cancel_read();
        if let Some(previous) = self.selection.take() {
            previous.destroy();
        }
        let Some(offer) = offer else {
            self.state.no_text(&self.ping);
            return;
        };
        let mime_type = offer.data::<OfferData>().and_then(OfferData::selected);
        self.selection = Some(SelectionOffer::Ext(offer.clone()));
        match mime_type {
            Some(mime_type) => self.receive_ext(&offer, mime_type),
            None => self.state.no_text(&self.ping),
        }
    }

    pub(crate) fn wlr_offer(&mut self, offer: Option<ZwlrDataControlOfferV1>) {
        self.cancel_read();
        if let Some(previous) = self.selection.take() {
            previous.destroy();
        }
        let Some(offer) = offer else {
            self.state.no_text(&self.ping);
            return;
        };
        let mime_type = offer.data::<OfferData>().and_then(OfferData::selected);
        self.selection = Some(SelectionOffer::Wlr(offer.clone()));
        match mime_type {
            Some(mime_type) => self.receive_wlr(&offer, mime_type),
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
            self.detach_seat();
            self.state.publish(
                &self.ping,
                ClipboardSnapshot {
                    error: "clipboard control is no longer available".into(),
                    ..ClipboardSnapshot::default()
                },
            );
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
            self.detach_seat();
            self.state.publish(
                &self.ping,
                ClipboardSnapshot {
                    error: "clipboard control is no longer available".into(),
                    ..ClipboardSnapshot::default()
                },
            );
        }
    }

    pub(crate) fn set_text(
        &mut self,
        text: String,
        qh: &QueueHandle<crate::host::App>,
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
        let source = match &self.backend {
            Backend::Ext {
                manager,
                device: Some(device),
            } => {
                let source = manager.create_data_source(qh, SourceData::new(key, bytes.clone()));
                offer_ext_text(&source);
                device.set_selection(Some(&source));
                OwnedSource::Ext(source)
            }
            Backend::Wlr {
                manager,
                device: Some(device),
            } => {
                let source = manager.create_data_source(qh, SourceData::new(key, bytes.clone()));
                offer_wlr_text(&source);
                device.set_selection(Some(&source));
                OwnedSource::Wlr(source)
            }
            _ => return Err("clipboard service is unavailable"),
        };
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

    pub(crate) fn clear(&self) -> Result<(), &'static str> {
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
        Ok(())
    }

    pub(crate) fn send(&mut self, data: &SourceData, mime_type: &str, fd: OwnedFd) {
        if mime_priority(mime_type).is_none() {
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
        match self.transfer.write(id, fd, data.bytes(), TRANSFER_TIMEOUT) {
            Ok(()) => {
                self.active_writes.insert(id, data.key());
                self.sources
                    .get_mut(&data.key())
                    .expect("validated clipboard source must remain present")
                    .active_writes += 1;
            }
            Err(error) => eprintln!("thing: cannot serve clipboard text: {error}"),
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

    pub(crate) fn take_snapshot(&mut self) -> Option<ClipboardSnapshot> {
        self.process_completions();
        self.state.latest.take()
    }

    fn process_completions(&mut self) {
        for completion in self.transfer.take_completions() {
            match completion {
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

    fn receive_ext(&mut self, offer: &ExtDataControlOfferV1, mime_type: String) {
        match pipe() {
            Ok((reader, writer)) => {
                offer.receive(mime_type, writer.as_fd());
                drop(writer);
                self.start_read(reader);
            }
            Err(error) => self.read_start_failed(error),
        }
    }

    fn receive_wlr(&mut self, offer: &ZwlrDataControlOfferV1, mime_type: String) {
        match pipe() {
            Ok((reader, writer)) => {
                offer.receive(mime_type, writer.as_fd());
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

    fn destroy_sources(&mut self) {
        for (_, mut source) in std::mem::take(&mut self.sources) {
            if let Some(proxy) = source.proxy.take() {
                proxy.destroy();
            }
        }
        self.active_writes.clear();
        self.owned_bytes = 0;
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

fn offer_ext_text(source: &ExtDataControlSourceV1) {
    source.offer(MIME_UTF8.into());
    source.offer(MIME_TEXT.into());
    source.offer(MIME_X11_UTF8.into());
}

fn offer_wlr_text(source: &ZwlrDataControlSourceV1) {
    source.offer(MIME_UTF8.into());
    source.offer(MIME_TEXT.into());
    source.offer(MIME_X11_UTF8.into());
}

fn mime_priority(mime_type: &str) -> Option<u8> {
    let normalized = mime_type
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    match normalized.as_str() {
        "text/plain;charset=utf-8" | "text/plain;charset=utf8" => Some(0),
        "utf8_string" => Some(1),
        "text/plain" => Some(2),
        _ => None,
    }
}

fn allocate(next: &mut u64, exhausted: &'static str) -> Result<u64, &'static str> {
    let current = *next;
    *next = next.checked_add(1).ok_or(exhausted)?;
    Ok(current)
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0_i32; 2];
    // SAFETY: `fds` points to storage for two descriptors. both returned
    // descriptors are wrapped in owning types immediately on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two new owned descriptors.
    let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: pipe2 returned two new owned descriptors.
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        os::{fd::AsRawFd, unix::net::UnixStream},
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
    fn text_mime_preference_is_stable() {
        let offer = OfferData::default();
        offer.offer("image/png".into());
        offer.offer("text/plain".into());
        offer.offer("UTF8_STRING".into());
        offer.offer("text/plain; charset=UTF-8".into());
        assert_eq!(
            offer.selected().as_deref(),
            Some("text/plain; charset=UTF-8")
        );
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
    fn clipboard_sender_receives_a_blocking_pipe() {
        let (_, writer) = pipe().unwrap();
        // SAFETY: the writer owns a valid descriptor for the duration of the call.
        let flags = unsafe { libc::fcntl(writer.as_fd().as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
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
        let mut clipboard = Clipboard {
            backend: Backend::Unsupported,
            selection: None,
            sources: BTreeMap::from([(
                7,
                RetainedSource {
                    proxy: None,
                    bytes: MAX_TEXT_BYTES,
                    active_writes: 1,
                    cancelled: false,
                },
            )]),
            active_writes: BTreeMap::from([(11, 7)]),
            owned_bytes: MAX_TEXT_BYTES,
            state: State::new(ClipboardSnapshot::default()),
            transfer: Worker::start(ping.clone()).unwrap(),
            ping,
        };

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
