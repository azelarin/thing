//! idle watches armed through ext-idle-notify-v1.
//!
//! each accepted configuration arms its own watches on its own output; there
//! is no primary output and no deduplication across outputs. a watch belongs
//! to the configuration consumer that declared it, so replacing or removing
//! that configuration retires it, and events for a retired watch are ignored.

use smithay_client_toolkit::registry::RegistryState;
use thing_runtime::IdleWatchDefinition;
use wayland_client::{
    Dispatch, Proxy, QueueHandle,
    globals::BindError,
    protocol::{wl_output::WlOutput, wl_seat::WlSeat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::ExtIdleNotificationV1, ext_idle_notifier_v1::ExtIdleNotifierV1,
};

/// identifies one armed notification, so events for a destroyed one are
/// recognized as stale.
pub(crate) struct Notification(pub(crate) u64);

struct Notifier {
    registry_name: u32,
    generation: u64,
    proxy: ExtIdleNotifierV1,
}

impl Drop for Notifier {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

struct Watch {
    id: u64,
    output: WlOutput,
    consumer: String,
    index: usize,
    timeout_ms: u32,
    seat: WlSeat,
    generation: u64,
    idle: bool,
    proxy: ExtIdleNotificationV1,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

/// a watch one configuration wants armed on its output.
pub(crate) struct Desired {
    pub(crate) output: WlOutput,
    pub(crate) consumer: String,
    pub(crate) index: usize,
    pub(crate) definition: IdleWatchDefinition,
}

/// a transition the host reports to the configuration that owns a watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Transition {
    pub(crate) output: WlOutput,
    pub(crate) consumer: String,
    pub(crate) index: usize,
    pub(crate) idle: bool,
}

pub(crate) struct State {
    active: Option<Notifier>,
    next_generation: u64,
    next_id: u64,
    watches: Vec<Watch>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            active: None,
            next_generation: 1,
            next_id: 1,
            watches: Vec::new(),
        }
    }
}

impl State {
    pub(crate) fn available(&self) -> bool {
        self.active.is_some()
    }

    /// arms the desired watches and retires the rest. a retired watch that
    /// was idle returns a resume, which the host delivers only while the
    /// watch's configuration is still current.
    pub(crate) fn reconcile<D>(
        &mut self,
        desired: &[Desired],
        seat: Option<&WlSeat>,
        qh: &QueueHandle<D>,
    ) -> Vec<Transition>
    where
        D: Dispatch<ExtIdleNotificationV1, Notification> + 'static,
    {
        let generation = self.active.as_ref().map(|notifier| notifier.generation);
        let mut resumes = Vec::new();
        self.watches.retain(|watch| {
            let keep = Some(watch.generation) == generation
                && Some(&watch.seat) == seat
                && desired.iter().any(|desired| watch.matches(desired));
            if !keep && watch.idle {
                resumes.push(watch.transition(false));
            }
            keep
        });
        let (Some(notifier), Some(seat)) = (&self.active, seat) else {
            return resumes;
        };
        for desired in desired {
            if self.watches.iter().any(|watch| watch.matches(desired)) {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            let proxy = notifier.proxy.get_idle_notification(
                desired.definition.timeout_ms,
                seat,
                qh,
                Notification(id),
            );
            self.watches.push(Watch {
                id,
                output: desired.output.clone(),
                consumer: desired.consumer.clone(),
                index: desired.index,
                timeout_ms: desired.definition.timeout_ms,
                seat: seat.clone(),
                generation: notifier.generation,
                idle: false,
                proxy,
            });
        }
        resumes
    }

    /// records an event from notification `id`. events for retired watches
    /// and repeated states are stale and return nothing.
    pub(crate) fn transition(&mut self, id: u64, idle: bool) -> Option<Transition> {
        let watch = self.watches.iter_mut().find(|watch| watch.id == id)?;
        if watch.idle == idle {
            return None;
        }
        watch.idle = idle;
        Some(watch.transition(idle))
    }

    /// forgets the notifier when its global is removed. the caller reconciles
    /// afterward, which retires the watches it armed.
    pub(crate) fn retire(&mut self, registry_name: u32) -> bool {
        if self
            .active
            .as_ref()
            .is_none_or(|notifier| notifier.registry_name != registry_name)
        {
            return false;
        }
        self.active.take();
        true
    }

    /// releases every watch and the notifier, without resumes, at shutdown.
    pub(crate) fn clear(&mut self) {
        self.watches.clear();
        self.active.take();
    }

    pub(crate) fn bind_next<D>(
        &mut self,
        registry: &RegistryState,
        qh: &QueueHandle<D>,
    ) -> Result<bool, String>
    where
        D: Dispatch<ExtIdleNotifierV1, ()> + 'static,
    {
        if self.active.is_some() {
            return Ok(false);
        }
        let interface = ExtIdleNotifierV1::interface().name;
        let mut candidates = registry
            .globals_by_interface(interface)
            .map(|global| global.name)
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        let Some(next_generation) = self.next_generation.checked_add(1) else {
            return Err("idle notifier generation is exhausted".into());
        };
        let mut last_error = None;
        for registry_name in candidates {
            // version 1 is enough: get_idle_notification respects idle
            // inhibitors in every version.
            let proxy: Result<ExtIdleNotifierV1, BindError> =
                registry.bind_specific(qh, registry_name, 1..=1, ());
            match proxy {
                Ok(proxy) => {
                    self.active = Some(Notifier {
                        registry_name,
                        generation: self.next_generation,
                        proxy,
                    });
                    self.next_generation = next_generation;
                    return Ok(true);
                }
                Err(error) => last_error = Some(error),
            }
        }
        if let Some(error) = last_error {
            Err(format!("cannot bind idle notifier: {error}"))
        } else {
            Ok(false)
        }
    }
}

impl Watch {
    fn matches(&self, desired: &Desired) -> bool {
        self.output == desired.output
            && self.consumer == desired.consumer
            && self.index == desired.index
            && self.timeout_ms == desired.definition.timeout_ms
    }

    fn transition(&self, idle: bool) -> Transition {
        Transition {
            output: self.output.clone(),
            consumer: self.consumer.clone(),
            index: self.index,
            idle,
        }
    }
}
