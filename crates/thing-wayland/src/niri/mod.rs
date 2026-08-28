//! niri event subscription and action dispatch.
//!
//! one worker owns two nonblocking sockets: an event subscription and ordered
//! actions. the mailbox holds the latest state until the host reads it.

mod model;
#[cfg(test)]
mod tests;
mod transport;

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::Instant,
};

use calloop::ping::Ping;
use thing_runtime::{NiriAction, NiriState};

use model::Model;
use transport::{Connection, poll};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub available: bool,
    pub error: String,
    pub state: NiriState,
}

#[derive(Default)]
struct Shared {
    commands: VecDeque<NiriAction>,
    online: bool,
    stop: bool,
    latest: Option<Snapshot>,
}

pub(crate) struct Niri {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl Niri {
    pub fn start(ping: Ping) -> io::Result<Self> {
        Self::start_at(std::env::var_os("NIRI_SOCKET").map(PathBuf::from), ping)
    }

    fn start_at(path: Option<PathBuf>, ping: Ping) -> io::Result<Self> {
        let (wake, reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let worker = if let Some(path) = path {
            let shared = shared.clone();
            Some(
                thread::Builder::new()
                    .name("thing-niri".into())
                    .spawn(move || {
                        let mut previous = Snapshot::default();
                        loop {
                            if lock(&shared).stop {
                                break;
                            }
                            let result = session(&path, &shared, &reader, |snapshot| {
                                if previous != snapshot {
                                    previous = snapshot.clone();
                                    lock(&shared).latest = Some(snapshot);
                                    ping.ping();
                                }
                            });
                            {
                                let mut state = lock(&shared);
                                state.online = false;
                                state.commands.clear();
                                if state.stop {
                                    break;
                                }
                            }
                            let snapshot = Snapshot {
                                error: result.err().map(|e| e.to_string()).unwrap_or_default(),
                                ..Snapshot::default()
                            };
                            if snapshot != previous {
                                previous = snapshot.clone();
                                lock(&shared).latest = Some(snapshot);
                                ping.ping();
                            }
                            // shutdown wakes the retry wait; there are no periodic wakeups
                            // once the event connection is healthy and idle.
                            let mut fds = [wake_descriptor(&reader)];
                            let _ = poll(&mut fds, 1000);
                            drain(&reader);
                        }
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            shared,
            wake,
            worker,
        })
    }

    pub fn take_snapshot(&self) -> Option<Snapshot> {
        lock(&self.shared).latest.take()
    }

    pub fn send(&self, action: NiriAction) -> Result<(), String> {
        let mut shared = lock(&self.shared);
        if !shared.online {
            return Err("niri is unavailable".into());
        }
        if shared.commands.len() >= 128 {
            return Err("niri command queue is full".into());
        }
        shared.commands.push_back(action);
        drop(shared);
        wake(&self.wake);
        Ok(())
    }
}

impl Drop for Niri {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        wake(&self.wake);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn session(
    path: &std::path::Path,
    shared: &Mutex<Shared>,
    wake: &UnixStream,
    mut publish: impl FnMut(Snapshot),
) -> io::Result<()> {
    let mut events = Connection::connect(path)?;
    events.send(&serde_json::json!("EventStream"))?;
    let mut commands: Option<Connection> = None;
    let mut model = Model::default();
    let mut subscribed = false;
    let mut error = String::new();
    loop {
        if lock(shared).stop {
            return Ok(());
        }
        if subscribed && commands.as_ref().is_none_or(|c| c.deadline.is_none()) {
            let next = lock(shared).commands.pop_front();
            if let Some(action) = next {
                let connection = match commands.as_mut() {
                    Some(connection) => connection,
                    None => commands.insert(Connection::connect(path)?),
                };
                connection.send(&model::request(action))?;
            }
        }
        let deadline = events
            .deadline
            .into_iter()
            .chain(commands.as_ref().and_then(|c| c.deadline))
            .min();
        let timeout = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "niri request timed out",
                    ));
                }
                remaining.as_millis().clamp(1, i32::MAX as u128) as i32
            }
            None => -1,
        };
        let mut descriptors = vec![wake_descriptor(wake), events.descriptor()];
        if let Some(connection) = &commands {
            descriptors.push(connection.descriptor());
        }
        poll(&mut descriptors, timeout)?;
        drain(wake);
        if lock(shared).stop {
            return Ok(());
        }
        let mut changed = false;
        for line in events.ready(descriptors[1].revents)? {
            if !subscribed {
                model::reply(&line)?;
                events.deadline = None;
                subscribed = true;
                lock(shared).online = true;
                changed = true;
            } else {
                changed |= model.apply(&line)?;
            }
        }
        if let Some(connection) = &mut commands {
            for line in connection.ready(descriptors[2].revents)? {
                if connection.deadline.take().is_none() {
                    return Err(io::Error::other("unsolicited niri command reply"));
                }
                error = model::reply(&line)
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default();
                changed = true;
            }
        }
        if changed {
            publish(Snapshot {
                available: subscribed,
                error: error.clone(),
                state: model.snapshot(),
            });
        }
    }
}

fn wake_descriptor(stream: &UnixStream) -> libc::pollfd {
    libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }
}

fn wake(mut stream: &UnixStream) {
    let _ = stream.write(&[1]);
}
fn drain(mut stream: &UnixStream) {
    let mut bytes = [0; 256];
    while matches!(stream.read(&mut bytes), Ok(count) if count > 0) {}
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
