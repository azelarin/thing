use smithay_client_toolkit::registry::RegistryState;
use thing_core::Node;
use thing_runtime::SurfaceTree;
use wayland_client::{
    Dispatch, Proxy, QueueHandle, globals::BindError, protocol::wl_surface::WlSurface,
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

pub(crate) struct Manager {
    registry_name: u32,
    generation: u64,
    proxy: ZwpIdleInhibitManagerV1,
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

pub(crate) struct Inhibitor {
    generation: u64,
    proxy: ZwpIdleInhibitorV1,
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

pub(crate) struct State {
    active: Option<Manager>,
    next_generation: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            active: None,
            next_generation: 1,
        }
    }
}

impl State {
    pub(crate) fn active(&self) -> Option<&Manager> {
        self.active.as_ref()
    }

    pub(crate) fn take_active(&mut self) -> Option<Manager> {
        self.active.take()
    }

    pub(crate) fn retire<'a>(
        &mut self,
        registry_name: u32,
        inhibitors: impl IntoIterator<Item = &'a mut Option<Inhibitor>>,
    ) -> bool {
        if self
            .active
            .as_ref()
            .is_none_or(|manager| manager.registry_name != registry_name)
        {
            return false;
        }
        for inhibitor in inhibitors {
            inhibitor.take();
        }
        self.active.take();
        true
    }

    pub(crate) fn reconcile<D>(
        &self,
        inhibitor: &mut Option<Inhibitor>,
        surface: &WlSurface,
        desired: bool,
        qh: &QueueHandle<D>,
    ) -> Result<(), String>
    where
        D: Dispatch<ZwpIdleInhibitorV1, ()> + 'static,
    {
        if !desired {
            inhibitor.take();
            return Ok(());
        }
        let manager = self
            .active
            .as_ref()
            .ok_or("idle_inhibit requires the compositor's idle-inhibition protocol")?;
        if inhibitor
            .as_ref()
            .is_some_and(|inhibitor| inhibitor.generation == manager.generation)
        {
            return Ok(());
        }
        inhibitor.take();
        *inhibitor = Some(Inhibitor {
            generation: manager.generation,
            proxy: manager.proxy.create_inhibitor(surface, qh, ()),
        });
        Ok(())
    }

    pub(crate) fn bind_next<D>(
        &mut self,
        registry: &RegistryState,
        qh: &QueueHandle<D>,
    ) -> Result<bool, String>
    where
        D: Dispatch<ZwpIdleInhibitManagerV1, ()> + 'static,
    {
        if self.active.is_some() {
            return Ok(false);
        }
        let interface = ZwpIdleInhibitManagerV1::interface().name;
        let mut candidates = registry
            .globals_by_interface(interface)
            .filter(|global| global.version >= 1)
            .map(|global| global.name)
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        let Some(next_generation) = self.next_generation.checked_add(1) else {
            return Err("idle inhibitor manager generation is exhausted".into());
        };
        let mut last_error = None;
        for registry_name in candidates {
            let proxy: Result<ZwpIdleInhibitManagerV1, BindError> =
                registry.bind_specific(qh, registry_name, 1..=1, ());
            match proxy {
                Ok(proxy) => {
                    self.active = Some(Manager {
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
            Err(format!("cannot bind idle inhibitor manager: {error}"))
        } else {
            Ok(false)
        }
    }
}

pub(crate) fn requested(surfaces: &[SurfaceTree]) -> bool {
    fn visit(node: &Node) -> bool {
        node.idle_inhibit || node.children.iter().any(visit)
    }
    surfaces.iter().any(|surface| visit(&surface.root))
}

pub(crate) fn validate(surfaces: &[SurfaceTree], available: bool) -> Result<(), String> {
    if !available && requested(surfaces) {
        return Err("idle_inhibit requires the compositor's idle-inhibition protocol".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
