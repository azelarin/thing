//! pulseaudio-compatible audio control, including pipewire's pulse server.
//!
//! one worker owns the libpulse connection and sends state updates to the host.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use calloop::ping::Ping;
use libpulse_binding::{
    callbacks::ListResult,
    context::{
        Context, FlagSet, State,
        subscribe::{Facility, InterestMaskSet, Operation},
    },
    mainloop::{api::Mainloop as _, events::io::FlagSet as IoFlags, standard::Mainloop},
    proplist::properties,
    time::MicroSeconds,
    volume::{ChannelVolumes, Volume},
};
use thing_runtime::{Action, AudioDevice, AudioSnapshot, AudioSource, AudioStream};

#[derive(Default)]
struct Inbox {
    commands: VecDeque<Action>,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    inbox: Mutex<Inbox>,
    retry: Condvar,
    online: AtomicBool,
    latest: Mutex<Option<AudioSnapshot>>,
}

pub(crate) struct Audio {
    shared: Arc<Shared>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl Audio {
    pub fn start(ping: Ping) -> io::Result<Self> {
        Self::start_with_server(ping, None)
    }

    /// a service without a worker, for host tests that must not reach the
    /// desktop session.
    #[cfg(test)]
    pub(crate) fn inert() -> io::Result<Self> {
        let (wake, _) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        Ok(Self {
            shared: Default::default(),
            wake,
            worker: None,
        })
    }

    fn start_with_server(ping: Ping, server: Option<String>) -> io::Result<Self> {
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Shared::default());
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("thing-audio".into())
            .spawn(move || {
                let mut previous = AudioSnapshot::default();
                let mut last_error = String::new();
                let mut next_stream_id = 1_u64;
                loop {
                    if lock(&state.inbox).stop {
                        break;
                    }
                    let result = session(
                        &state,
                        &reader,
                        server.as_deref(),
                        &mut next_stream_id,
                        |snapshot| {
                            if snapshot != previous {
                                previous = snapshot.clone();
                                *lock(&state.latest) = Some(snapshot);
                                ping.ping();
                            }
                        },
                    );
                    state.online.store(false, Ordering::Release);
                    if lock(&state.inbox).stop {
                        break;
                    }
                    let error = result
                        .err()
                        .unwrap_or_else(|| "audio connection ended".into());
                    if error != last_error {
                        eprintln!("thing: audio unavailable: {error}; retrying in 2 seconds");
                        last_error = error.clone();
                    }
                    let unavailable = AudioSnapshot {
                        error,
                        ..AudioSnapshot::default()
                    };
                    if previous != unavailable {
                        previous = unavailable;
                        *lock(&state.latest) = Some(previous.clone());
                        ping.ping();
                    }
                    let mut inbox = lock(&state.inbox);
                    // requests belong to the connection on which they were issued.
                    // never replay an old volume change after a server restart.
                    inbox.commands.clear();
                    let _wait =
                        state
                            .retry
                            .wait_timeout_while(inbox, Duration::from_secs(2), |inbox| !inbox.stop);
                }
            })?;
        Ok(Self {
            shared,
            wake,
            worker: Some(worker),
        })
    }

    pub fn take_snapshot(&self) -> Option<AudioSnapshot> {
        lock(&self.shared.latest).take()
    }

    pub fn send(&self, action: Action) -> Result<(), &'static str> {
        let mut inbox = lock(&self.shared.inbox);
        if !self.shared.online.load(Ordering::Acquire) {
            return Err("audio server is disconnected");
        }
        enqueue(&mut inbox.commands, action)?;
        drop(inbox);
        wake(&self.wake);
        Ok(())
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        lock(&self.shared.inbox).stop = true;
        self.shared.retry.notify_all();
        wake(&self.wake);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wake(mut stream: &UnixStream) {
    // `WouldBlock` means a wakeup is already pending. `Interrupted` writes retry.
    loop {
        match stream.write(&[1]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            _ => break,
        }
    }
}

fn drain(mut stream: &UnixStream) {
    let mut bytes = [0; 256];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn enqueue(queue: &mut VecDeque<Action>, action: Action) -> Result<(), &'static str> {
    // consecutive drag updates can replace one another. do not move them past
    // a device switch or mute action, where ordering changes their meaning.
    let replaces_previous = match (&action, queue.back()) {
        (Action::SetVolume(_), Some(Action::SetVolume(_)))
        | (Action::SetSourceVolume(_), Some(Action::SetSourceVolume(_))) => true,
        (
            Action::SetStreamVolume { id, .. },
            Some(Action::SetStreamVolume {
                id: previous_id, ..
            }),
        ) => id == previous_id,
        _ => false,
    };
    if replaces_previous {
        queue.pop_back();
    }
    if queue.len() >= 128 {
        return Err("audio command queue is full");
    }
    queue.push_back(action);
    Ok(())
}

#[derive(Clone)]
struct Sink {
    device: AudioDevice,
    volume: ChannelVolumes,
    muted: bool,
}

#[derive(Clone)]
struct Source {
    device: AudioDevice,
    volume: ChannelVolumes,
    muted: bool,
    monitor: bool,
}

#[derive(Clone)]
struct PlaybackStream {
    index: u32,
    channels: ChannelVolumes,
    state: AudioStream,
}

struct QueriedStream {
    index: u32,
    channels: ChannelVolumes,
    application: String,
    app_id: String,
    name: String,
    icon: String,
    muted: bool,
    has_volume: bool,
    volume_writable: bool,
}

struct StreamMetadata {
    application: String,
    app_id: String,
    name: String,
    icon: String,
}

#[derive(Default)]
struct ServerState {
    default_sink: String,
    default_source: String,
    sinks: BTreeMap<String, Sink>,
    sources: BTreeMap<String, Source>,
    streams: BTreeMap<String, PlaybackStream>,
}

#[derive(Default)]
struct Query {
    state: ServerState,
    server_done: bool,
    sinks_done: bool,
    sources_done: bool,
    streams: BTreeMap<u32, QueriedStream>,
    streams_done: bool,
    failed: bool,
}

fn session(
    shared: &Shared,
    reader: &UnixStream,
    server: Option<&str>,
    next_stream_id: &mut u64,
    mut publish: impl FnMut(AudioSnapshot),
) -> Result<(), String> {
    let mut mainloop = Mainloop::new().ok_or("cannot create audio event loop")?;
    let mut context = Context::new(&mainloop, "thing").ok_or("cannot create audio context")?;
    let input = reader.try_clone().map_err(|e| format!("{e}"))?;
    let _wake_event = mainloop
        .new_io_event(
            input.as_raw_fd(),
            IoFlags::INPUT,
            Box::new(move |_, _, _| drain(&input)),
        )
        .ok_or("cannot register audio wakeup")?;
    context
        .connect(server, FlagSet::NOAUTOSPAWN, None)
        .map_err(|e| format!("{e}"))?;
    let result = (|| {
        let dirty = Rc::new(Cell::new(true));
        let failed = Rc::new(Cell::new(false));
        let busy = Rc::new(Cell::new(false));
        let retired_streams = Rc::new(RefCell::new(BTreeSet::new()));
        let removed_during_query = Rc::new(RefCell::new(None::<BTreeSet<u32>>));
        let changed = dirty.clone();
        let retired = retired_streams.clone();
        let removed_during = removed_during_query.clone();
        context.set_subscribe_callback(Some(Box::new(move |facility, operation, index| {
            if facility == Some(Facility::SinkInput) && operation == Some(Operation::Removed) {
                retired.borrow_mut().insert(index);
                if let Some(removed) = removed_during.borrow_mut().as_mut() {
                    removed.insert(index);
                }
            }
            changed.set(true);
        })));
        let mut subscribed = false;
        let mut query: Option<Rc<RefCell<Query>>> = None;
        let mut deadline = Some(Instant::now() + Duration::from_secs(5));
        let mut server_state = ServerState::default();
        let mut stream_ids = BTreeMap::new();
        loop {
            if lock(&shared.inbox).stop {
                return Ok(());
            }
            match context.get_state() {
                State::Failed | State::Terminated => return Err(format!("{}", context.errno())),
                State::Ready => {
                    if !subscribed {
                        let failed = failed.clone();
                        context.subscribe(
                            InterestMaskSet::SINK
                                | InterestMaskSet::SOURCE
                                | InterestMaskSet::SINK_INPUT
                                | InterestMaskSet::SERVER,
                            move |success| failed.set(!success),
                        );
                        subscribed = true;
                        shared.online.store(true, Ordering::Release);
                        deadline = None;
                    }
                    if failed.get() {
                        return Err("server rejected audio subscription".into());
                    }
                    if query.as_ref().is_some_and(|q| {
                        let q = q.borrow();
                        q.failed
                            || (q.server_done && q.sinks_done && q.sources_done && q.streams_done)
                    }) {
                        let q = query.take().expect("completed query exists");
                        let mut q = q.borrow_mut();
                        if q.failed {
                            return Err("could not read audio state".into());
                        }
                        let queried_streams = std::mem::take(&mut q.streams);
                        server_state = std::mem::take(&mut q.state);
                        let retired = std::mem::take(&mut *retired_streams.borrow_mut());
                        let removed_during = removed_during_query
                            .borrow_mut()
                            .take()
                            .expect("active audio query exists");
                        reconcile_streams(
                            &mut server_state,
                            queried_streams,
                            &mut stream_ids,
                            &retired,
                            &removed_during,
                            next_stream_id,
                        );
                        publish(audio_snapshot(&server_state));
                        deadline = None;
                    }
                    if query.is_none() && !busy.get() && dirty.replace(false) {
                        *removed_during_query.borrow_mut() = Some(BTreeSet::new());
                        query = Some(query_audio(&context));
                        deadline = Some(Instant::now() + Duration::from_secs(5));
                    }
                    // refresh after each operation so controls following a
                    // device switch use the server's confirmed default output.
                    if query.is_none() && !busy.get() {
                        let command = lock(&shared.inbox).commands.pop_front();
                        if let Some(command) = command {
                            busy.set(true);
                            if apply(
                                &mut context,
                                &server_state,
                                command,
                                dirty.clone(),
                                busy.clone(),
                            ) {
                                deadline = Some(Instant::now() + Duration::from_secs(5));
                            } else {
                                busy.set(false);
                                continue;
                            }
                        }
                    }
                }
                _ => {}
            }
            if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                return Err("audio server did not respond within 5 seconds".into());
            }
            let timeout = deadline.map(|deadline| {
                MicroSeconds(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_micros()
                        .min(i32::MAX as u128) as u64,
                )
            });
            mainloop.prepare(timeout).map_err(|e| format!("{e}"))?;
            mainloop.poll().map_err(|e| format!("{e}"))?;
            mainloop.dispatch().map_err(|e| format!("{e}"))?;
        }
    })();
    context.disconnect();
    result
}

fn query_audio(context: &Context) -> Rc<RefCell<Query>> {
    let query = Rc::new(RefCell::new(Query::default()));
    let server = query.clone();
    context.introspect().get_server_info(move |info| {
        let mut query = server.borrow_mut();
        query.state.default_sink = info
            .default_sink_name
            .as_deref()
            .unwrap_or_default()
            .to_owned();
        query.state.default_source = info
            .default_source_name
            .as_deref()
            .unwrap_or_default()
            .to_owned();
        query.server_done = true;
    });
    let outputs = query.clone();
    context.introspect().get_sink_info_list(move |result| {
        let mut query = outputs.borrow_mut();
        match result {
            ListResult::Item(info) => {
                if let Some(name) = &info.name {
                    if query.state.sinks.len() >= 512 {
                        query.failed = true;
                        return;
                    }
                    query.state.sinks.insert(
                        name.to_string(),
                        Sink {
                            device: AudioDevice {
                                name: name.to_string(),
                                description: info.description.as_deref().unwrap_or(name).to_owned(),
                            },
                            volume: info.volume,
                            muted: info.mute,
                        },
                    );
                }
            }
            ListResult::End => query.sinks_done = true,
            ListResult::Error => query.failed = true,
        }
    });
    let inputs = query.clone();
    context.introspect().get_source_info_list(move |result| {
        let mut query = inputs.borrow_mut();
        match result {
            ListResult::Item(info) => {
                if let Some(name) = &info.name {
                    if query.state.sources.len() >= 512 {
                        query.failed = true;
                        return;
                    }
                    query.state.sources.insert(
                        name.to_string(),
                        Source {
                            device: AudioDevice {
                                name: name.to_string(),
                                description: info.description.as_deref().unwrap_or(name).to_owned(),
                            },
                            volume: info.volume,
                            muted: info.mute,
                            monitor: info.monitor_of_sink.is_some(),
                        },
                    );
                }
            }
            ListResult::End => query.sources_done = true,
            ListResult::Error => query.failed = true,
        }
    });
    let streams = query.clone();
    context
        .introspect()
        .get_sink_input_info_list(move |result| {
            let mut query = streams.borrow_mut();
            match result {
                ListResult::Item(info) => {
                    if query.streams.len() >= 512 && !query.streams.contains_key(&info.index) {
                        query.failed = true;
                        return;
                    }
                    let metadata =
                        stream_metadata(&info.proplist, info.name.as_deref().unwrap_or_default());
                    query.streams.insert(
                        info.index,
                        QueriedStream {
                            index: info.index,
                            channels: info.volume,
                            application: metadata.application,
                            app_id: metadata.app_id,
                            name: metadata.name,
                            icon: metadata.icon,
                            muted: info.mute,
                            has_volume: info.has_volume,
                            volume_writable: info.has_volume && info.volume_writable,
                        },
                    );
                }
                ListResult::End => query.streams_done = true,
                ListResult::Error => query.failed = true,
            }
        });
    query
}

fn stream_metadata(
    proplist: &libpulse_binding::proplist::Proplist,
    stream_name: &str,
) -> StreamMetadata {
    let stream_name = bounded_text(stream_name.to_owned());
    let process = property(proplist, properties::APPLICATION_PROCESS_BINARY);
    StreamMetadata {
        application: first_nonempty([
            property(proplist, properties::APPLICATION_NAME),
            process.clone(),
            stream_name.clone(),
        ]),
        app_id: first_nonempty([property(proplist, properties::APPLICATION_ID), process]),
        name: first_nonempty([property(proplist, properties::MEDIA_NAME), stream_name]),
        icon: property(proplist, properties::APPLICATION_ICON_NAME),
    }
}

fn property(proplist: &libpulse_binding::proplist::Proplist, name: &str) -> String {
    bounded_text(proplist.get_str(name).unwrap_or_default())
}

fn bounded_text(mut value: String) -> String {
    const MAX_BYTES: usize = 4096;
    if value.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}

fn first_nonempty<const N: usize>(values: [String; N]) -> String {
    values
        .into_iter()
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn reconcile_streams(
    state: &mut ServerState,
    mut queried: BTreeMap<u32, QueriedStream>,
    ids: &mut BTreeMap<u32, String>,
    retired: &BTreeSet<u32>,
    removed_during_query: &BTreeSet<u32>,
    next_id: &mut u64,
) {
    for index in removed_during_query {
        queried.remove(index);
    }
    ids.retain(|index, _| queried.contains_key(index) && !retired.contains(index));
    for index in queried.keys() {
        if !ids.contains_key(index) {
            let id = next_stream_id(next_id, ids.values());
            ids.insert(*index, id);
        }
    }
    state.streams = queried
        .into_iter()
        .map(|(index, stream)| {
            let id = ids[&index].clone();
            let volume = if stream.has_volume {
                volume(&stream.channels)
            } else {
                0.0
            };
            (
                id.clone(),
                PlaybackStream {
                    index: stream.index,
                    channels: stream.channels,
                    state: AudioStream {
                        id,
                        application: stream.application,
                        app_id: stream.app_id,
                        name: stream.name,
                        icon: stream.icon,
                        volume,
                        muted: stream.muted,
                        volume_writable: stream.volume_writable,
                    },
                },
            )
        })
        .collect();
}

fn next_stream_id<'a>(next: &mut u64, active: impl Iterator<Item = &'a String>) -> String {
    let active = active.cloned().collect::<BTreeSet<_>>();
    loop {
        let value = *next;
        *next = next.wrapping_add(1);
        if *next == 0 {
            *next = 1;
        }
        if value != 0 {
            let id = format!("stream-{value:016x}");
            if !active.contains(&id) {
                return id;
            }
        }
    }
}

fn audio_snapshot(state: &ServerState) -> AudioSnapshot {
    let sink = state.sinks.get(&state.default_sink);
    let source = state.sources.get(&state.default_source);
    AudioSnapshot {
        available: sink.is_some(),
        volume: sink.map_or(0.0, |sink| volume(&sink.volume)),
        muted: sink.is_some_and(|sink| sink.muted),
        device: state.default_sink.clone(),
        devices: state
            .sinks
            .values()
            .map(|sink| sink.device.clone())
            .collect(),
        streams: state
            .streams
            .values()
            .map(|stream| stream.state.clone())
            .collect(),
        source_available: source.is_some(),
        source_volume: source.map_or(0.0, |source| volume(&source.volume)),
        source_muted: source.is_some_and(|source| source.muted),
        source_device: state.default_source.clone(),
        // output monitors are useful for recording desktop audio, but make a
        // microphone picker noisy. retain one only while it is the default.
        sources: state
            .sources
            .values()
            .filter(|source| !source.monitor || source.device.name == state.default_source)
            .map(|source| source.device.clone())
            .collect(),
        all_sources: state
            .sources
            .values()
            .map(|source| AudioSource {
                name: source.device.name.clone(),
                description: source.device.description.clone(),
                monitor: source.monitor,
            })
            .collect(),
        error: String::new(),
    }
}

fn volume(channels: &ChannelVolumes) -> f64 {
    f64::from(channels.max().0) / f64::from(Volume::NORMAL.0)
}

fn scaled_volume(channels: &ChannelVolumes, value: f64) -> Option<ChannelVolumes> {
    let mut channels = *channels;
    channels.scale(Volume((value * f64::from(Volume::NORMAL.0)).round() as u32))?;
    Some(channels)
}

fn apply(
    context: &mut Context,
    state: &ServerState,
    action: Action,
    dirty: Rc<Cell<bool>>,
    busy: Rc<Cell<bool>>,
) -> bool {
    let completed = move |success: bool| {
        busy.set(false);
        dirty.set(true);
        if !success {
            eprintln!("thing: audio server rejected a control request");
        }
    };
    match action {
        Action::SetDefaultSink(name) if state.sinks.contains_key(&name) => {
            context.set_default_sink(&name, completed);
            return true;
        }
        Action::SetVolume(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
            if let Some(sink) = state.sinks.get(&state.default_sink)
                && let Some(volume) = scaled_volume(&sink.volume, value)
            {
                context.introspect().set_sink_volume_by_name(
                    &state.default_sink,
                    &volume,
                    Some(Box::new(completed)),
                );
                return true;
            }
        }
        Action::SetMuted(muted) if state.sinks.contains_key(&state.default_sink) => {
            context.introspect().set_sink_mute_by_name(
                &state.default_sink,
                muted,
                Some(Box::new(completed)),
            );
            return true;
        }
        Action::SetStreamVolume { id, volume }
            if volume.is_finite() && (0.0..=1.0).contains(&volume) =>
        {
            if let Some(stream) = state
                .streams
                .get(&id)
                .filter(|stream| stream.state.volume_writable)
                && let Some(channels) = scaled_volume(&stream.channels, volume)
            {
                // pulse controls a stream by its numeric index. the opaque
                // id rejects retired snapshots, but the protocol cannot
                // make this request conditional on the stream generation.
                context.introspect().set_sink_input_volume(
                    stream.index,
                    &channels,
                    Some(Box::new(completed)),
                );
                return true;
            }
        }
        Action::SetStreamMuted { id, muted } => {
            if let Some(stream) = state.streams.get(&id) {
                context.introspect().set_sink_input_mute(
                    stream.index,
                    muted,
                    Some(Box::new(completed)),
                );
                return true;
            }
        }
        Action::SetDefaultSource(name) if state.sources.contains_key(&name) => {
            context.set_default_source(&name, completed);
            return true;
        }
        Action::SetSourceVolume(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
            if let Some(source) = state.sources.get(&state.default_source)
                && let Some(volume) = scaled_volume(&source.volume, value)
            {
                context.introspect().set_source_volume_by_name(
                    &state.default_source,
                    &volume,
                    Some(Box::new(completed)),
                );
                return true;
            }
        }
        Action::SetSourceMuted(muted) if state.sources.contains_key(&state.default_source) => {
            context.introspect().set_source_mute_by_name(
                &state.default_source,
                muted,
                Some(Box::new(completed)),
            );
            return true;
        }
        _ => eprintln!("thing: audio request refers to unavailable state or an invalid volume"),
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queried_stream(index: u32, application: &str) -> QueriedStream {
        let mut channels = ChannelVolumes::default();
        channels.set(1, Volume::NORMAL);
        QueriedStream {
            index,
            channels,
            application: application.into(),
            app_id: application.to_lowercase(),
            name: "Playback".into(),
            icon: application.to_lowercase(),
            muted: false,
            has_volume: true,
            volume_writable: true,
        }
    }

    #[test]
    fn stream_metadata_uses_declared_values_then_documented_fallbacks() {
        let mut proplist = libpulse_binding::proplist::Proplist::new().unwrap();
        proplist
            .set_str(properties::APPLICATION_PROCESS_BINARY, "firefox")
            .unwrap();
        let metadata = stream_metadata(&proplist, "AudioStream");
        assert_eq!(metadata.application, "firefox");
        assert_eq!(metadata.app_id, "firefox");
        assert_eq!(metadata.name, "AudioStream");
        assert!(metadata.icon.is_empty());

        proplist
            .set_str(properties::APPLICATION_NAME, "Firefox")
            .unwrap();
        proplist
            .set_str(properties::APPLICATION_ID, "org.mozilla.firefox")
            .unwrap();
        proplist
            .set_str(properties::APPLICATION_ICON_NAME, "firefox")
            .unwrap();
        proplist.set_str(properties::MEDIA_NAME, "Music").unwrap();
        let metadata = stream_metadata(&proplist, "AudioStream");
        assert_eq!(metadata.application, "Firefox");
        assert_eq!(metadata.app_id, "org.mozilla.firefox");
        assert_eq!(metadata.name, "Music");
        assert_eq!(metadata.icon, "firefox");
    }

    #[test]
    fn coalesces_drag_updates_without_crossing_a_device_switch() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, Action::SetVolume(0.1)).unwrap();
        enqueue(&mut queue, Action::SetVolume(0.2)).unwrap();
        enqueue(&mut queue, Action::SetSourceVolume(0.4)).unwrap();
        enqueue(&mut queue, Action::SetSourceVolume(0.5)).unwrap();
        enqueue(&mut queue, Action::SetDefaultSource("webcam".into())).unwrap();
        enqueue(&mut queue, Action::SetSourceVolume(0.6)).unwrap();
        enqueue(&mut queue, Action::SetSourceVolume(0.7)).unwrap();
        enqueue(&mut queue, Action::SetDefaultSink("headphones".into())).unwrap();
        enqueue(&mut queue, Action::SetVolume(0.3)).unwrap();
        enqueue(
            &mut queue,
            Action::SetStreamVolume {
                id: "a".into(),
                volume: 0.1,
            },
        )
        .unwrap();
        enqueue(
            &mut queue,
            Action::SetStreamVolume {
                id: "a".into(),
                volume: 0.2,
            },
        )
        .unwrap();
        enqueue(
            &mut queue,
            Action::SetStreamVolume {
                id: "b".into(),
                volume: 0.4,
            },
        )
        .unwrap();
        assert_eq!(
            queue.into_iter().collect::<Vec<_>>(),
            vec![
                Action::SetVolume(0.2),
                Action::SetSourceVolume(0.5),
                Action::SetDefaultSource("webcam".into()),
                Action::SetSourceVolume(0.7),
                Action::SetDefaultSink("headphones".into()),
                Action::SetVolume(0.3),
                Action::SetStreamVolume {
                    id: "a".into(),
                    volume: 0.2,
                },
                Action::SetStreamVolume {
                    id: "b".into(),
                    volume: 0.4,
                },
            ]
        );
    }

    #[test]
    fn stream_ids_survive_refresh_and_change_after_retirement_or_reconnect() {
        let mut next_id = 1;
        let mut ids = BTreeMap::new();
        let mut state = ServerState::default();
        reconcile_streams(
            &mut state,
            BTreeMap::from([(7, queried_stream(7, "Firefox"))]),
            &mut ids,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut next_id,
        );
        let first = state.streams.keys().next().unwrap().clone();
        let snapshot = audio_snapshot(&state);
        assert_eq!(snapshot.streams.len(), 1);
        assert_eq!(snapshot.streams[0].id, first);
        assert_eq!(snapshot.streams[0].application, "Firefox");
        assert_eq!(snapshot.streams[0].volume, 1.0);
        assert!(snapshot.streams[0].volume_writable);

        reconcile_streams(
            &mut state,
            BTreeMap::from([(7, queried_stream(7, "Firefox changed"))]),
            &mut ids,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut next_id,
        );
        assert_eq!(state.streams.keys().next().unwrap(), &first);

        reconcile_streams(
            &mut state,
            BTreeMap::from([(7, queried_stream(7, "New app"))]),
            &mut ids,
            &BTreeSet::from([7]),
            &BTreeSet::new(),
            &mut next_id,
        );
        let replacement = state.streams.keys().next().unwrap().clone();
        assert_ne!(replacement, first);

        let mut reconnect_ids = BTreeMap::new();
        reconcile_streams(
            &mut state,
            BTreeMap::from([(7, queried_stream(7, "New app"))]),
            &mut reconnect_ids,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut next_id,
        );
        assert_ne!(state.streams.keys().next().unwrap(), &replacement);
    }

    #[test]
    fn stream_removed_during_query_is_omitted_until_the_followup_query() {
        let mut next_id = 1;
        let mut ids = BTreeMap::new();
        let mut state = ServerState::default();
        reconcile_streams(
            &mut state,
            BTreeMap::from([(7, queried_stream(7, "Old app"))]),
            &mut ids,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut next_id,
        );
        let old = state.streams.keys().next().unwrap().clone();

        reconcile_streams(
            &mut state,
            BTreeMap::from([(7, queried_stream(7, "Stale query result"))]),
            &mut ids,
            &BTreeSet::from([7]),
            &BTreeSet::from([7]),
            &mut next_id,
        );
        assert!(state.streams.is_empty());
        assert!(ids.is_empty());

        reconcile_streams(
            &mut state,
            BTreeMap::from([(7, queried_stream(7, "Replacement"))]),
            &mut ids,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut next_id,
        );
        assert_ne!(state.streams.keys().next().unwrap(), &old);
        assert_eq!(
            state.streams.values().next().unwrap().state.application,
            "Replacement"
        );
    }

    #[test]
    fn snapshot_hides_nondefault_monitors_and_preserves_server_volume() {
        let mut channels = ChannelVolumes::default();
        channels.set(1, Volume(Volume::NORMAL.0 * 3 / 2));
        let source = |name: &str, monitor| Source {
            device: AudioDevice {
                name: name.into(),
                description: name.into(),
            },
            volume: channels,
            muted: false,
            monitor,
        };
        let sources = BTreeMap::from([
            ("microphone".into(), source("microphone", false)),
            ("monitor-a".into(), source("monitor-a", true)),
            ("monitor-b".into(), source("monitor-b", true)),
        ]);

        let snapshot = audio_snapshot(&ServerState {
            default_sink: "missing".into(),
            default_source: "monitor-a".into(),
            sources: sources.clone(),
            ..ServerState::default()
        });
        assert!(!snapshot.available);
        assert_eq!(snapshot.device, "missing");
        assert!(snapshot.source_available);
        assert_eq!(snapshot.source_volume, 1.5);
        assert_eq!(
            snapshot
                .sources
                .iter()
                .map(|source| source.name.as_str())
                .collect::<Vec<_>>(),
            vec!["microphone", "monitor-a"]
        );
        assert_eq!(
            snapshot
                .all_sources
                .iter()
                .map(|source| (source.name.as_str(), source.monitor))
                .collect::<Vec<_>>(),
            vec![
                ("microphone", false),
                ("monitor-a", true),
                ("monitor-b", true),
            ]
        );

        let snapshot = audio_snapshot(&ServerState {
            default_source: "missing".into(),
            sources: sources.clone(),
            ..ServerState::default()
        });
        assert!(!snapshot.source_available);
        assert_eq!(snapshot.source_volume, 0.0);
        assert_eq!(snapshot.source_device, "missing");
        assert_eq!(snapshot.sources.len(), 1);
        assert_eq!(snapshot.all_sources.len(), 3);
    }

    #[test]
    fn command_queue_is_bounded() {
        let mut queue = VecDeque::new();
        for _ in 0..128 {
            enqueue(&mut queue, Action::SetMuted(true)).unwrap();
        }
        assert!(enqueue(&mut queue, Action::SetMuted(false)).is_err());
        assert_eq!(queue.len(), 128);
    }

    #[test]
    fn volume_scaling_preserves_channel_balance() {
        let mut volume = ChannelVolumes::default();
        volume.set(2, Volume::NORMAL);
        volume.get_mut()[0] = Volume(Volume::NORMAL.0 / 2);
        let volume = scaled_volume(&volume, 0.5).unwrap();
        assert_eq!(volume.get()[0].0, Volume::NORMAL.0 / 4);
        assert_eq!(volume.get()[1].0, Volume::NORMAL.0 / 2);
    }

    #[test]
    fn disconnected_worker_reports_failure_and_shutdown_interrupts_retry() {
        let directory = tempfile::tempdir().unwrap();
        let server = format!("unix:{}", directory.path().join("missing-server").display());
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let audio = Audio::start_with_server(ping, Some(server)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(snapshot) = audio.take_snapshot() {
                assert!(!snapshot.available);
                assert!(!snapshot.source_available);
                assert!(!snapshot.error.is_empty());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not report connection failure"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(audio.send(Action::SetMuted(true)).is_err());
        let started = Instant::now();
        drop(audio);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown waited for reconnect timeout"
        );
    }

    #[test]
    #[ignore = "requires a running pulseaudio-compatible server; reads state only"]
    fn reads_live_audio_state_without_sending_actions() {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let audio = Audio::start(ping).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(snapshot) = audio.take_snapshot() {
                assert!(snapshot.error.is_empty(), "{}", snapshot.error);
                if snapshot.available {
                    assert!(
                        snapshot
                            .devices
                            .iter()
                            .any(|device| device.name == snapshot.device)
                    );
                }
                if snapshot.source_available {
                    assert!(
                        snapshot
                            .sources
                            .iter()
                            .any(|source| source.name == snapshot.source_device)
                    );
                }
                let ids = snapshot
                    .streams
                    .iter()
                    .map(|stream| stream.id.as_str())
                    .collect::<BTreeSet<_>>();
                assert_eq!(ids.len(), snapshot.streams.len());
                assert!(snapshot.streams.iter().all(|stream| {
                    !stream.id.is_empty() && stream.volume.is_finite() && stream.volume >= 0.0
                }));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not report live audio state"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
