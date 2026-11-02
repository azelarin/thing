//! shared state ownership across output configurations.
//!
//! the accepted values live in the configurations' service snapshot. a
//! publication prepares every output before committing any tree. a rejected
//! candidate leaves the accepted values, trees, and handlers in place.

use std::sync::Arc;

use thing_runtime::{CallValue, SharedStateDefinition, SharedStates};

use super::{Configurations, Runtime, SurfaceTree};

/// the result of one shared state publication.
#[derive(Debug)]
pub(crate) struct SharedStatePublication<K> {
    /// outputs whose runtime committed a new frame. their prepared layouts
    /// are ready to install.
    pub(crate) refreshed: Vec<K>,
    /// whether the values changed, or why they were not published.
    pub(crate) result: Result<bool, String>,
}

/// a key may be declared by several outputs only with the same initial value,
/// so every output agrees on when the shared value resets.
pub(super) fn collect_shared_state_definitions<'a>(
    runtimes: impl IntoIterator<Item = &'a Runtime>,
) -> Result<Vec<SharedStateDefinition>, String> {
    let mut definitions = std::collections::BTreeMap::<String, SharedStateDefinition>::new();
    for definition in runtimes
        .into_iter()
        .flat_map(Runtime::shared_state_definitions)
    {
        if let Some(existing) = definitions.get(&definition.key)
            && existing != &definition
        {
            return Err(format!(
                "shared state {:?} has conflicting declarations across outputs",
                definition.key
            ));
        }
        definitions.insert(definition.key.clone(), definition);
    }
    Ok(definitions.into_values().collect())
}

/// the accepted shared states after an output joins: unchanged declarations
/// keep their values, and values that no current output declares are retained
/// for an output that returns.
pub(super) fn project_added<'a>(
    current: &SharedStates,
    runtimes: impl IntoIterator<Item = &'a Runtime>,
) -> Result<SharedStates, String> {
    current.project_retaining(&collect_shared_state_definitions(runtimes)?)
}

/// the accepted shared states for a complete replacement. keys the
/// replacement no longer declares are dropped.
pub(super) fn project_replaced<'a>(
    current: &SharedStates,
    runtimes: impl IntoIterator<Item = &'a Runtime>,
) -> Result<SharedStates, String> {
    current.project(&collect_shared_state_definitions(runtimes)?)
}

impl<K: Clone + Eq> Configurations<K> {
    #[cfg(test)]
    pub(crate) fn shared_states(&self) -> &SharedStates {
        self.services.shared_states()
    }

    #[cfg(test)]
    pub(crate) fn publish_shared_states(
        &mut self,
        source: &K,
        consumer: &str,
        updates: Vec<(String, Arc<CallValue>)>,
    ) -> SharedStatePublication<K> {
        self.publish_shared_states_checked(source, consumer, updates, |_, _| Ok(()))
    }

    /// publishes the values that the configuration `consumer` on output
    /// `source` queued in one event. a retired configuration publishes
    /// nothing. equivalent values change nothing and refresh no output.
    pub(crate) fn publish_shared_states_checked(
        &mut self,
        source: &K,
        consumer: &str,
        updates: Vec<(String, Arc<CallValue>)>,
        validate: impl Fn(&K, &[SurfaceTree]) -> Result<(), String>,
    ) -> SharedStatePublication<K> {
        let unchanged = |result| SharedStatePublication {
            refreshed: Vec::new(),
            result,
        };
        if self.consumer(source) != Some(consumer) {
            return unchanged(Err(
                "a retired configuration cannot publish shared state".into()
            ));
        }
        let previous = self.services.shared_states().clone();
        let next = match previous.with_values(&updates) {
            Ok(Some(next)) => next,
            Ok(None) => return unchanged(Ok(false)),
            Err(error) => return unchanged(Err(error)),
        };
        let mut candidates = Vec::new();
        for instance in &mut self.instances {
            let key = &instance.key;
            match instance
                .runtime
                .prepare_shared_states(&next, |surfaces| validate(key, surfaces))
            {
                Ok(candidate) => candidates.push((key.clone(), candidate)),
                Err(error) => {
                    // dropping the earlier guards restores their values and
                    // subscriptions; no tree or handler was published yet.
                    return unchanged(Err(error.to_string()));
                }
            }
        }
        let refreshed = candidates
            .into_iter()
            .filter_map(|(key, candidate)| candidate.commit().then_some(key))
            .collect();
        self.services.set_shared_states(next);
        SharedStatePublication {
            refreshed,
            result: Ok(true),
        }
    }
}
