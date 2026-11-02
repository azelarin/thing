//! one file-read worker shared by all outputs of a shell instance.
//!
//! pending definitions and snapshots replace older versions instead of forming
//! an event queue. callbacks belong to the host's configuration generations.

mod worker;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
};

use calloop::ping::Ping;
use thing_runtime::{
    FileWatchDefinition, FileWatchSnapshot, MAX_FILE_WATCHES, MAX_WATCHED_FILE_PATHS,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Subscription<T> {
    pub(crate) output: T,
    pub(crate) consumer: String,
    pub(crate) definition: FileWatchDefinition,
}

pub(crate) struct Delivery<T> {
    pub(crate) output: T,
    pub(crate) consumer: String,
    pub(crate) key: String,
    pub(crate) snapshot: FileWatchSnapshot,
}

#[derive(Clone)]
struct Versioned {
    revision: u64,
    snapshot: FileWatchSnapshot,
}

struct Request {
    generation: u64,
    paths: BTreeSet<String>,
}

#[derive(Default)]
struct Shared {
    generation: u64,
    request: Option<Request>,
    snapshots: BTreeMap<String, Versioned>,
    revision: u64,
    stopped: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct FileWatches<T> {
    subscriptions: Vec<Subscription<T>>,
    seen: BTreeMap<(String, String), u64>,
    next: usize,
    shared: Arc<Mutex<Shared>>,
    worker: Option<worker::Worker>,
    ping: Ping,
}

impl<T: Clone + Eq> FileWatches<T> {
    pub(crate) fn new(ping: Ping) -> Self {
        Self {
            subscriptions: Vec::new(),
            seen: BTreeMap::new(),
            next: 0,
            shared: Arc::default(),
            worker: None,
            ping,
        }
    }

    pub(crate) fn reconcile(&mut self, subscriptions: Vec<Subscription<T>>) -> Result<(), String> {
        validate(subscriptions.iter().map(|item| &item.definition))?;
        if self.subscriptions == subscriptions {
            return Ok(());
        }
        let paths = subscriptions
            .iter()
            .map(|item| item.definition.path.clone())
            .collect();
        {
            let mut shared = lock(&self.shared);
            shared.generation = shared
                .generation
                .checked_add(1)
                .ok_or("file watch identity space is exhausted")?;
            shared.snapshots.clear();
            shared.request = Some(Request {
                generation: shared.generation,
                paths,
            });
        }
        self.subscriptions = subscriptions;
        self.seen.clear();
        self.next = 0;
        if self.worker.is_none() && !self.subscriptions.is_empty() {
            match worker::Worker::new(self.shared.clone(), self.ping.clone()) {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    let generation = lock(&self.shared).generation;
                    for subscription in &self.subscriptions {
                        publish(
                            &self.shared,
                            &self.ping,
                            generation,
                            &subscription.definition.path,
                            failed(&error),
                        );
                    }
                }
            }
        }
        if let Some(worker) = &mut self.worker {
            worker.wake();
        }
        Ok(())
    }

    pub(crate) fn take_deliveries(&mut self) -> Vec<Delivery<T>> {
        let shared = lock(&self.shared);
        let count = self.subscriptions.len();
        for offset in 0..count {
            let index = (self.next + offset) % count;
            let subscription = &self.subscriptions[index];
            let Some(version) = shared.snapshots.get(&subscription.definition.path) else {
                continue;
            };
            let identity = (
                subscription.consumer.clone(),
                subscription.definition.key.clone(),
            );
            if self.seen.get(&identity) == Some(&version.revision) {
                continue;
            }
            // one callback per loop turn keeps a burst from multiplying the
            // luau execution limit. rotate so a busy file cannot starve others.
            self.seen.insert(identity, version.revision);
            self.next = (index + 1) % count;
            let delivery = Delivery {
                output: subscription.output.clone(),
                consumer: subscription.consumer.clone(),
                key: subscription.definition.key.clone(),
                snapshot: version.snapshot.clone(),
            };
            let more = self.subscriptions.iter().any(|item| {
                shared
                    .snapshots
                    .get(&item.definition.path)
                    .is_some_and(|snapshot| {
                        self.seen
                            .get(&(item.consumer.clone(), item.definition.key.clone()))
                            != Some(&snapshot.revision)
                    })
            });
            drop(shared);
            if more {
                self.ping.ping();
            }
            return vec![delivery];
        }
        Vec::new()
    }
}

pub(crate) fn validate<'a>(
    definitions: impl IntoIterator<Item = &'a FileWatchDefinition>,
) -> Result<(), String> {
    let mut count = 0;
    let mut paths = BTreeSet::new();
    for definition in definitions {
        definition.validate()?;
        count += 1;
        paths.insert(&definition.path);
        if count > MAX_FILE_WATCHES || paths.len() > MAX_WATCHED_FILE_PATHS {
            return Err("an instance may declare at most 64 file watches across 32 paths".into());
        }
    }
    Ok(())
}

fn failed(error: &impl std::fmt::Display) -> FileWatchSnapshot {
    let mut text = error.to_string().replace('\0', "�");
    if text.len() > thing_runtime::MAX_FILE_WATCH_ERROR_BYTES {
        let mut end = thing_runtime::MAX_FILE_WATCH_ERROR_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    FileWatchSnapshot::Failed(Arc::from(text))
}

fn publish(
    shared: &Mutex<Shared>,
    ping: &Ping,
    generation: u64,
    path: &str,
    snapshot: FileWatchSnapshot,
) {
    let mut shared = lock(shared);
    if shared.stopped
        || shared.generation != generation
        || shared
            .snapshots
            .get(path)
            .is_some_and(|current| current.snapshot == snapshot)
    {
        return;
    }
    let Some(revision) = shared.revision.checked_add(1) else {
        shared.stopped = true;
        eprintln!("thing: file watch revision space is exhausted");
        return;
    };
    shared.revision = revision;
    shared
        .snapshots
        .insert(path.into(), Versioned { revision, snapshot });
    drop(shared);
    ping.ping();
}

fn cancelled(shared: &Mutex<Shared>, generation: u64) -> bool {
    let shared = lock(shared);
    shared.stopped || shared.generation != generation
}

impl<T> Drop for FileWatches<T> {
    fn drop(&mut self) {
        lock(&self.shared).stopped = true;
        if let Some(worker) = &mut self.worker {
            worker.wake();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
