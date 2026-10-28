//! pulseaudio-compatible audio control, including pipewire's pulse server.
//!
//! one worker owns the libpulse connection and sends state updates to the host.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
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
    context::{Context, FlagSet, State, subscribe::InterestMaskSet},
    mainloop::{api::Mainloop as _, events::io::FlagSet as IoFlags, standard::Mainloop},
    time::MicroSeconds,
    volume::{ChannelVolumes, Volume},
};
use thing_runtime::{Action, AudioDevice, AudioSnapshot};

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
                loop {
                    if lock(&state.inbox).stop {
                        break;
                    }
                    let result = session(&state, &reader, server.as_deref(), |snapshot| {
                        if snapshot != previous {
                            previous = snapshot.clone();
                            *lock(&state.latest) = Some(snapshot);
                            ping.ping();
                        }
                    });
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
    if matches!(
        (&action, queue.back()),
        (Action::SetVolume(_), Some(Action::SetVolume(_)))
            | (Action::SetSourceVolume(_), Some(Action::SetSourceVolume(_)))
    ) {
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

#[derive(Default)]
struct ServerState {
    default_sink: String,
    default_source: String,
    sinks: BTreeMap<String, Sink>,
    sources: BTreeMap<String, Source>,
}

#[derive(Default)]
struct Query {
    state: ServerState,
    server_done: bool,
    sinks_done: bool,
    sources_done: bool,
    failed: bool,
}

fn session(
    shared: &Shared,
    reader: &UnixStream,
    server: Option<&str>,
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
        let changed = dirty.clone();
        context.set_subscribe_callback(Some(Box::new(move |_, _, _| changed.set(true))));
        let mut subscribed = false;
        let mut query: Option<Rc<RefCell<Query>>> = None;
        let mut deadline = Some(Instant::now() + Duration::from_secs(5));
        let mut server_state = ServerState::default();
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
                        q.failed || (q.server_done && q.sinks_done && q.sources_done)
                    }) {
                        let q = query.take().expect("completed query exists");
                        let mut q = q.borrow_mut();
                        if q.failed {
                            return Err("could not read audio devices".into());
                        }
                        server_state = std::mem::take(&mut q.state);
                        publish(audio_snapshot(&server_state));
                        deadline = None;
                    }
                    if query.is_none() && !busy.get() && dirty.replace(false) {
                        query = Some(query_devices(&context));
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

fn query_devices(context: &Context) -> Rc<RefCell<Query>> {
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
    query
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
        error: String::new(),
    }
}

fn volume(channels: &ChannelVolumes) -> f64 {
    f64::from(channels.max().0) / f64::from(Volume::NORMAL.0)
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
            if let Some(sink) = state.sinks.get(&state.default_sink) {
                let mut volume = sink.volume;
                if volume
                    .scale(Volume((value * f64::from(Volume::NORMAL.0)).round() as u32))
                    .is_some()
                {
                    context.introspect().set_sink_volume_by_name(
                        &state.default_sink,
                        &volume,
                        Some(Box::new(completed)),
                    );
                    return true;
                }
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
        Action::SetDefaultSource(name) if state.sources.contains_key(&name) => {
            context.set_default_source(&name, completed);
            return true;
        }
        Action::SetSourceVolume(value) if value.is_finite() && (0.0..=1.0).contains(&value) => {
            if let Some(source) = state.sources.get(&state.default_source) {
                let mut volume = source.volume;
                if volume
                    .scale(Volume((value * f64::from(Volume::NORMAL.0)).round() as u32))
                    .is_some()
                {
                    context.introspect().set_source_volume_by_name(
                        &state.default_source,
                        &volume,
                        Some(Box::new(completed)),
                    );
                    return true;
                }
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
        _ => eprintln!("thing: audio request refers to an unavailable device or invalid volume"),
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            queue.into_iter().collect::<Vec<_>>(),
            vec![
                Action::SetVolume(0.2),
                Action::SetSourceVolume(0.5),
                Action::SetDefaultSource("webcam".into()),
                Action::SetSourceVolume(0.7),
                Action::SetDefaultSink("headphones".into()),
                Action::SetVolume(0.3)
            ]
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

        let snapshot = audio_snapshot(&ServerState {
            default_source: "missing".into(),
            sources: sources.clone(),
            ..ServerState::default()
        });
        assert!(!snapshot.source_available);
        assert_eq!(snapshot.source_volume, 0.0);
        assert_eq!(snapshot.source_device, "missing");
        assert_eq!(snapshot.sources.len(), 1);
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
        volume.scale(Volume(Volume::NORMAL.0 / 2)).unwrap();
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
