//! desired expiry holds owned by one configuration runtime.

use std::collections::BTreeSet;

use mlua::{Error, Result};

#[derive(Default)]
pub(crate) struct ExpiryHolds {
    active: BTreeSet<u64>,
    pending: Vec<(u64, bool)>,
}

impl ExpiryHolds {
    pub(crate) fn stage(&mut self, key: u64, paused: bool) -> Result<()> {
        if self.pending.len() >= 128 {
            return Err(Error::runtime(
                "event exceeds 128 notification expiry controls",
            ));
        }
        self.pending.push((key, paused));
        Ok(())
    }

    pub(crate) fn checkpoint(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn truncate(&mut self, checkpoint: usize) {
        self.pending.truncate(checkpoint);
    }

    pub(crate) fn commit(&mut self) {
        for (key, paused) in self.pending.drain(..) {
            if paused {
                self.active.insert(key);
            } else {
                self.active.remove(&key);
            }
        }
    }

    pub(crate) fn retain(&mut self, keys: &BTreeSet<u64>) {
        self.active.retain(|key| keys.contains(key));
        self.pending.retain(|(key, _)| keys.contains(key));
    }

    pub(crate) fn keys(&self) -> &BTreeSet<u64> {
        &self.active
    }
}
