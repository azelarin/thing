use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    rc::Rc,
    time::Instant,
};

use thing_core::{Node, NodeId, Rect};
use thing_runtime::{
    Action, ApplicationsSnapshot, AudioSnapshot, AuthenticationSnapshot, BoundsProvider,
    BoundsRect, ClipboardSnapshot, Event, IdleWatchDefinition, KeyInput, ManagedProcessDefinition,
    ManagedProcessSnapshot, ManagedProcessState, MediaSnapshot, NiriState, NotificationSnapshot,
    OutputContext, PointerDispatchOutcome as RuntimePointerOutcome, PointerEvent, PowerSnapshot,
    PresentedBounds, Runtime, Scalar, ScrollInput, ServiceSnapshot, SleepInhibitorDefinition,
    SleepInhibitorState, SurfaceKey, SurfaceTree, SystemTraySnapshot, TimerToken, UiAction,
    UiTarget, WidgetBounds,
};
use thing_ui::Layout;

struct LayoutBounds(Rc<Layout>);

impl BoundsProvider for LayoutBounds {
    fn get(&self, node: NodeId) -> Option<WidgetBounds> {
        let placement = self.0.semantic_placement(node)?;
        let rect = |value: Rect| BoundsRect {
            x: value.x.into(),
            y: value.y.into(),
            width: value.width.into(),
            height: value.height.into(),
        };
        Some(WidgetBounds {
            node,
            layout: rect(placement.bounds),
            visual: rect(placement.border_aabb),
        })
    }
}

struct Instance<K> {
    key: K,
    consumer: String,
    output: OutputContext,
    runtime: Runtime,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ConfigurationMode {
    #[default]
    Shell,
    Lock,
}

impl ConfigurationMode {
    fn load(
        self,
        entry: &PathBuf,
        output: OutputContext,
        services: &ServiceSnapshot,
    ) -> Result<Runtime, String> {
        let runtime = match self {
            Self::Shell => Runtime::load_for_output(entry, output, services),
            Self::Lock => Runtime::load_lock_for_output(entry, output, services),
        };
        runtime.map_err(|error| error.to_string())
    }
}

pub(crate) struct PreparedConfigurations<K> {
    services: ServiceSnapshot,
    instances: Vec<Instance<K>>,
    next_consumer: u64,
    process_definitions: Vec<ManagedProcessDefinition>,
}

pub(crate) struct PreparedOutput<K> {
    services: ServiceSnapshot,
    instance: Instance<K>,
    next_consumer: u64,
    process_definitions: Vec<ManagedProcessDefinition>,
}

/// loaded configurations and desktop state shared by every output.
pub(crate) struct Configurations<K> {
    entry: PathBuf,
    mode: ConfigurationMode,
    services: ServiceSnapshot,
    instances: Vec<Instance<K>>,
    next_consumer: u64,
}

pub(crate) struct RemovedConfiguration {
    pub(crate) consumer: String,
    pub(crate) process_reconciliation: Result<(), String>,
}

#[derive(Debug)]
pub(crate) struct DispatchOutcome {
    pub(crate) changed: bool,
    pub(crate) actions: Vec<Action>,
    pub(crate) ui_actions: Vec<UiAction>,
}

#[derive(Debug)]
pub(crate) struct KeyHandlerOutcome {
    pub(crate) handled: bool,
    pub(crate) dispatch: DispatchOutcome,
}

#[derive(Debug)]
pub(crate) struct PointerHandlerOutcome {
    pub(crate) handled: bool,
    pub(crate) active: bool,
    pub(crate) dispatch: DispatchOutcome,
}

#[derive(Debug)]
pub(crate) struct ScrollHandlerOutcome {
    pub(crate) consumed: bool,
    pub(crate) dispatch: DispatchOutcome,
}

impl PointerHandlerOutcome {
    fn inactive() -> Self {
        Self {
            handled: false,
            active: false,
            dispatch: DispatchOutcome {
                changed: false,
                actions: Vec::new(),
                ui_actions: Vec::new(),
            },
        }
    }

    fn collect(runtime: &mut Runtime, outcome: RuntimePointerOutcome) -> Self {
        Self {
            handled: outcome.handled,
            active: outcome.active,
            dispatch: DispatchOutcome {
                changed: outcome.changed,
                actions: runtime.take_actions(),
                ui_actions: runtime.take_ui_actions(),
            },
        }
    }
}

impl<K> PreparedConfigurations<K> {
    pub(crate) fn managed_process_definitions(&self) -> &[ManagedProcessDefinition] {
        &self.process_definitions
    }

    pub(crate) fn set_managed_processes(
        &mut self,
        processes: ManagedProcessSnapshot,
        mut validate: impl FnMut(&K, &Runtime) -> Result<(), String>,
    ) -> Result<(), String> {
        self.services
            .set_managed_processes(processes.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_managed_processes(processes.clone())
                .map_err(|error| error.to_string())?;
            instance
                .runtime
                .refresh()
                .map_err(|error| error.to_string())?;
            validate(&instance.key, &instance.runtime)?;
        }
        Ok(())
    }
}

impl<K> PreparedOutput<K> {
    pub(crate) fn managed_process_definitions(&self) -> &[ManagedProcessDefinition] {
        &self.process_definitions
    }

    pub(crate) fn set_managed_processes(
        &mut self,
        processes: ManagedProcessSnapshot,
        validate: impl Fn(&Runtime) -> Result<(), String>,
    ) -> Result<(), String> {
        self.services
            .set_managed_processes(processes.clone())
            .map_err(|error| error.to_string())?;
        self.instance
            .runtime
            .set_managed_processes(processes)
            .map_err(|error| error.to_string())?;
        self.instance
            .runtime
            .refresh()
            .map_err(|error| error.to_string())?;
        validate(&self.instance.runtime)
    }
}

impl<K: Clone + Eq> Configurations<K> {
    pub(crate) fn new(entry: PathBuf, services: ServiceSnapshot) -> Self {
        Self::with_mode(entry, services, ConfigurationMode::Shell)
    }

    pub(crate) fn new_lock(entry: PathBuf, services: ServiceSnapshot) -> Self {
        Self::with_mode(entry, services, ConfigurationMode::Lock)
    }

    fn with_mode(entry: PathBuf, services: ServiceSnapshot, mode: ConfigurationMode) -> Self {
        Self {
            entry,
            mode,
            services,
            instances: Vec::new(),
            next_consumer: 1,
        }
    }

    pub(crate) fn contains(&self, key: &K) -> bool {
        self.instances.iter().any(|instance| &instance.key == key)
    }

    pub(crate) fn entry(&self) -> &PathBuf {
        &self.entry
    }

    pub(crate) fn prepare_add_checked(
        &self,
        key: K,
        output: OutputContext,
        validate: impl Fn(&Runtime) -> Result<(), String>,
    ) -> Result<Option<PreparedOutput<K>>, String> {
        if self.contains(&key) {
            return Ok(None);
        }
        let runtime = self
            .mode
            .load(&self.entry, output.clone(), &self.services)?;
        validate(&runtime)?;
        let definitions = collect_managed_process_definitions(
            self.instances
                .iter()
                .map(|instance| &instance.runtime)
                .chain(std::iter::once(&runtime)),
        )?;
        collect_sleep_inhibitor_definitions(
            self.instances
                .iter()
                .map(|instance| &instance.runtime)
                .chain(std::iter::once(&runtime)),
        )?;
        let mut services = self.services.clone();
        let snapshot = reconciled_managed_processes(services.managed_processes(), &definitions);
        services
            .set_managed_processes(snapshot)
            .map_err(|error| error.to_string())?;
        let mut next_consumer = self.next_consumer;
        let consumer = allocate_consumer(&mut next_consumer)?;
        Ok(Some(PreparedOutput {
            services,
            instance: Instance {
                key,
                consumer,
                output,
                runtime,
            },
            next_consumer,
            process_definitions: definitions,
        }))
    }

    pub(crate) fn commit_add(&mut self, mut prepared: PreparedOutput<K>) {
        prepared.instance.runtime.activate_timers(Instant::now());
        self.services = prepared.services;
        self.next_consumer = prepared.next_consumer;
        self.instances.push(prepared.instance);
    }

    #[cfg(test)]
    pub(crate) fn add(&mut self, key: K, output: OutputContext) -> Result<(), String> {
        self.add_checked(key, output, |_| Ok(()))
    }

    pub(crate) fn add_checked(
        &mut self,
        key: K,
        output: OutputContext,
        validate: impl FnOnce(&Runtime) -> Result<(), String>,
    ) -> Result<(), String> {
        self.add_checked_with_processes(key, output, validate, |_| Ok(()))
    }

    pub(crate) fn add_checked_with_processes(
        &mut self,
        key: K,
        output: OutputContext,
        validate: impl FnOnce(&Runtime) -> Result<(), String>,
        reconcile: impl FnOnce(&[ManagedProcessDefinition]) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.contains(&key) {
            return Ok(());
        }
        let mut runtime = self
            .mode
            .load(&self.entry, output.clone(), &self.services)?;
        validate(&runtime)?;
        let definitions = collect_managed_process_definitions(
            self.instances
                .iter()
                .map(|instance| &instance.runtime)
                .chain(std::iter::once(&runtime)),
        )?;
        collect_sleep_inhibitor_definitions(
            self.instances
                .iter()
                .map(|instance| &instance.runtime)
                .chain(std::iter::once(&runtime)),
        )?;
        let snapshot =
            reconciled_managed_processes(self.services.managed_processes(), &definitions);
        runtime
            .set_managed_processes(snapshot.clone())
            .map_err(|error| error.to_string())?;
        let mut services = self.services.clone();
        services
            .set_managed_processes(snapshot)
            .map_err(|error| error.to_string())?;
        let mut next_consumer = self.next_consumer;
        let consumer = allocate_consumer(&mut next_consumer)?;
        reconcile(&definitions)?;
        self.services = services;
        self.next_consumer = next_consumer;
        runtime.activate_timers(Instant::now());
        self.instances.push(Instance {
            key,
            consumer,
            output,
            runtime,
        });
        Ok(())
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<String> {
        let removed = self.remove_with_processes(key, |_| Ok(()))?;
        removed
            .process_reconciliation
            .expect("loaded managed process definitions must remain valid");
        Some(removed.consumer)
    }

    pub(crate) fn remove_with_processes(
        &mut self,
        key: &K,
        reconcile: impl FnOnce(&[ManagedProcessDefinition]) -> Result<(), String>,
    ) -> Option<RemovedConfiguration> {
        let index = self
            .instances
            .iter()
            .position(|instance| &instance.key == key)?;
        let consumer = self.instances.remove(index).consumer;
        let definitions = collect_managed_process_definitions(
            self.instances.iter().map(|instance| &instance.runtime),
        )
        .expect("loaded managed process definitions must remain compatible");
        let snapshot =
            reconciled_managed_processes(self.services.managed_processes(), &definitions);
        let mut services = self.services.clone();
        services
            .set_managed_processes(snapshot)
            .expect("loaded managed process state must remain valid");
        self.services = services;
        Some(RemovedConfiguration {
            consumer,
            process_reconciliation: reconcile(&definitions),
        })
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.instances.iter().map(|instance| &instance.key)
    }

    pub(crate) fn runtime(&self, key: &K) -> Option<&Runtime> {
        self.instances
            .iter()
            .find(|instance| &instance.key == key)
            .map(|instance| &instance.runtime)
    }

    pub(crate) fn has_active_press(&self, key: &K, id: NodeId) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.has_active_press(id))
    }

    pub(crate) fn focus_target(&self, key: &K, id: NodeId) -> Option<UiTarget> {
        self.runtime(key)?.focus_target(id)
    }

    pub(crate) fn autofocus_target(&self, key: &K, root: NodeId) -> Option<UiTarget> {
        self.runtime(key)?.autofocus_target(root)
    }

    pub(crate) fn ui_target_is_current(&self, key: &K, target: &UiTarget) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.ui_target_is_current(target))
    }

    pub(crate) fn ui_target_identity_is_current(&self, key: &K, target: &UiTarget) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.ui_target_identity_is_current(target))
    }

    pub(crate) fn pointer_target_is_current(&self, key: &K, target: &UiTarget) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.pointer_target_is_current(target))
    }

    pub(crate) fn scroll_target_is_current(&self, key: &K, target: &UiTarget) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.scroll_target_is_current(target))
    }

    pub(crate) fn scroll_view_target_is_current(&self, key: &K, target: &UiTarget) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.scroll_view_target_is_current(target))
    }

    pub(crate) fn active_pointer_matches(&self, key: &K, target: &UiTarget) -> bool {
        self.runtime(key).and_then(Runtime::active_pointer_target) == Some(target)
    }

    pub(crate) fn presented_target_is_current(
        &self,
        key: &K,
        consumer: &str,
        surface_root: NodeId,
        target: &UiTarget,
    ) -> bool {
        self.consumer(key) == Some(consumer)
            && self.ui_target_is_current(key, target)
            && self.rendered_surface_root(key, target.node()) == Some(surface_root)
    }

    pub(crate) fn presented_target_identity_is_current(
        &self,
        key: &K,
        consumer: &str,
        surface_root: NodeId,
        target: &UiTarget,
    ) -> bool {
        self.consumer(key) == Some(consumer)
            && self.ui_target_identity_is_current(key, target)
            && self.rendered_surface_root(key, target.node()) == Some(surface_root)
    }

    pub(crate) fn presented_pointer_target_is_current(
        &self,
        key: &K,
        consumer: &str,
        surface_root: NodeId,
        target: &UiTarget,
    ) -> bool {
        self.consumer(key) == Some(consumer)
            && self.pointer_target_is_current(key, target)
            && self.rendered_surface_root(key, target.node()) == Some(surface_root)
    }

    pub(crate) fn presented_scroll_target_is_current(
        &self,
        key: &K,
        consumer: &str,
        surface_root: NodeId,
        target: &UiTarget,
    ) -> bool {
        self.consumer(key) == Some(consumer)
            && self.scroll_target_is_current(key, target)
            && self.rendered_surface_root(key, target.node()) == Some(surface_root)
    }

    pub(crate) fn presented_scroll_view_target_is_current(
        &self,
        key: &K,
        consumer: &str,
        surface_root: NodeId,
        target: &UiTarget,
    ) -> bool {
        self.consumer(key) == Some(consumer)
            && self.scroll_view_target_is_current(key, target)
            && self.rendered_surface_root(key, target.node()) == Some(surface_root)
    }

    pub(crate) fn rendered_surface_root(&self, key: &K, id: NodeId) -> Option<NodeId> {
        self.runtime(key)?.rendered_surface_root(id)
    }

    pub(crate) fn active_press_is_available(&self, key: &K, id: NodeId) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.active_press_is_available(id))
    }

    pub(crate) fn layout_is_animating(&self, key: &K, layout: &Layout) -> bool {
        self.runtime(key).is_some_and(|runtime| {
            runtime.is_any_animating(layout.placements().iter().map(|placement| placement.id))
        })
    }

    pub(crate) fn presented_targets(
        &self,
        key: &K,
        root: &Node,
        layout: &Layout,
    ) -> Option<(String, BTreeMap<NodeId, UiTarget>)> {
        let consumer = self.consumer(key)?.to_owned();
        let mut ids: BTreeSet<_> = layout.pointer_targets().collect();
        if layout
            .get(root.id)
            .is_some_and(|placement| placement.enabled)
        {
            ids.insert(root.id);
        }
        let targets = self.runtime(key)?.ui_targets(&ids);
        Some((consumer, targets))
    }

    pub(crate) fn output(&self, key: &K) -> Option<&OutputContext> {
        self.instances
            .iter()
            .find(|instance| &instance.key == key)
            .map(|instance| &instance.output)
    }

    pub(crate) fn record_presented_bounds(
        &self,
        key: &K,
        consumer: &str,
        root: NodeId,
        layout: Rc<Layout>,
    ) -> Option<Rc<PresentedBounds>> {
        if self.consumer(key)? != consumer {
            return None;
        }
        match self
            .runtime(key)?
            .record_presented_bounds(root, Rc::new(LayoutBounds(layout)))
        {
            Ok(bounds) => Some(bounds),
            Err(error) => {
                eprintln!("thing: cannot retain presented bounds: {error}");
                None
            }
        }
    }

    pub(crate) fn consumer(&self, key: &K) -> Option<&str> {
        self.instances
            .iter()
            .find(|instance| &instance.key == key)
            .map(|instance| instance.consumer.as_str())
    }

    pub(crate) fn consumers(&self) -> Vec<String> {
        self.instances
            .iter()
            .map(|instance| instance.consumer.clone())
            .collect()
    }

    fn runtime_mut(&mut self, key: &K) -> Option<&mut Runtime> {
        self.instances
            .iter_mut()
            .find(|instance| &instance.key == key)
            .map(|instance| &mut instance.runtime)
    }

    #[cfg(test)]
    pub(crate) fn advance(&mut self, key: &K, now: Instant) -> Result<bool, String> {
        self.advance_checked(key, now, |_| Ok(()))
    }

    pub(crate) fn advance_checked(
        &mut self,
        key: &K,
        now: Instant,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<bool, String> {
        self.runtime_mut(key)
            .map(|runtime| runtime.advance_checked(now, validate))
            .transpose()
            .map(|changed| changed.unwrap_or(false))
            .map_err(|error| error.to_string())
    }

    #[cfg(test)]
    pub(crate) fn set_output(&mut self, key: &K, output: OutputContext) -> Result<bool, String> {
        self.set_output_checked(key, output, |_| Ok(()))
    }

    pub(crate) fn set_output_checked(
        &mut self,
        key: &K,
        output: OutputContext,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<bool, String> {
        let Some(instance) = self
            .instances
            .iter_mut()
            .find(|instance| &instance.key == key)
        else {
            return Ok(false);
        };
        if instance.output == output {
            validate(instance.runtime.surfaces())?;
            return Ok(false);
        }
        instance
            .runtime
            .set_output(output.clone())
            .map_err(|error| error.to_string())?;
        instance.output = output;
        let changed = instance
            .runtime
            .refresh_checked(&validate)
            .map_err(|error| error.to_string())?;
        // output changes can affect rendering even when no reactive property
        // reads them, so an unchanged tree still needs candidate validation.
        if !changed {
            validate(instance.runtime.surfaces())?;
        }
        Ok(changed)
    }

    pub(crate) fn set_service(&mut self, name: &str, value: Scalar) -> Result<(), String> {
        self.services
            .set_service(name, value.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_service(name, value.clone())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn set_audio(&mut self, audio: AudioSnapshot) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_audio(audio.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_audio(audio.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn set_media(&mut self, media: MediaSnapshot) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_media(media.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_media(media.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn set_applications(
        &mut self,
        applications: ApplicationsSnapshot,
    ) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_applications(applications.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_applications(applications.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn set_power(&mut self, power: PowerSnapshot) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_power(power.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_power(power.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn set_system_tray(
        &mut self,
        system_tray: SystemTraySnapshot,
    ) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_system_tray(system_tray.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_system_tray(system_tray.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn notification_expiry_holds(&self) -> BTreeSet<u64> {
        self.instances
            .iter()
            .flat_map(|instance| instance.runtime.notification_expiry_holds())
            .collect()
    }

    pub(crate) fn set_notifications(
        &mut self,
        notifications: NotificationSnapshot,
    ) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_notifications(notifications.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_notifications(notifications.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn set_managed_processes(
        &mut self,
        processes: ManagedProcessSnapshot,
    ) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_managed_processes(processes.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_managed_processes(processes.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    /// the sleep inhibitor keys every accepted configuration declares.
    pub(crate) fn sleep_inhibitor_definitions(&self) -> Vec<SleepInhibitorDefinition> {
        collect_sleep_inhibitor_definitions(self.instances.iter().map(|instance| &instance.runtime))
            .expect("accepted sleep inhibitor declarations must remain compatible")
    }

    pub(crate) fn set_sleep_inhibitors(
        &mut self,
        states: Vec<SleepInhibitorState>,
    ) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_sleep_inhibitors(states.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_sleep_inhibitors(states.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn set_clipboard(&mut self, clipboard: ClipboardSnapshot) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_clipboard(clipboard.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_clipboard(clipboard.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn set_authentication(
        &mut self,
        authentication: AuthenticationSnapshot,
    ) -> Result<(), String> {
        let mut services = self.services.clone();
        services
            .set_authentication(authentication.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_authentication(authentication.clone())
                .map_err(|error| error.to_string())?;
        }
        self.services = services;
        Ok(())
    }

    pub(crate) fn set_niri(&mut self, state: NiriState) -> Result<(), String> {
        self.services
            .set_niri(state.clone())
            .map_err(|error| error.to_string())?;
        for instance in &mut self.instances {
            instance
                .runtime
                .set_niri(state.clone())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn refresh(&mut self) -> Vec<K> {
        self.refresh_checked(|_, _| Ok(()))
    }

    pub(crate) fn refresh_checked(
        &mut self,
        validate: impl Fn(&K, &[SurfaceTree]) -> Result<(), String>,
    ) -> Vec<K> {
        self.instances
            .iter_mut()
            .filter_map(|instance| {
                let key = &instance.key;
                match instance
                    .runtime
                    .refresh_checked(|surfaces| validate(key, surfaces))
                {
                    Ok(true) => Some(instance.key.clone()),
                    Ok(false) => None,
                    Err(error) => {
                        eprintln!("thing: {error}");
                        None
                    }
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn dispatch(
        &mut self,
        key: &K,
        id: NodeId,
        event: Event,
    ) -> Result<DispatchOutcome, String> {
        self.dispatch_checked(key, id, event, |_| Ok(()))
    }

    pub(crate) fn dispatch_checked(
        &mut self,
        key: &K,
        id: NodeId,
        event: Event,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<DispatchOutcome, String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok(DispatchOutcome {
                changed: false,
                actions: Vec::new(),
                ui_actions: Vec::new(),
            });
        };
        let changed = runtime
            .dispatch_checked(id, event, validate)
            .map_err(|error| error.to_string())?;
        Ok(DispatchOutcome {
            changed,
            actions: runtime.take_actions(),
            ui_actions: runtime.take_ui_actions(),
        })
    }

    pub(crate) fn next_timer_deadline(&self) -> Option<Instant> {
        self.instances
            .iter()
            .filter_map(|instance| instance.runtime.next_timer_deadline())
            .min()
    }

    pub(crate) fn due_timers(&self, now: Instant) -> Vec<(K, TimerToken)> {
        let mut due = self
            .instances
            .iter()
            .flat_map(|instance| {
                instance
                    .runtime
                    .due_timers(now)
                    .into_iter()
                    .map(|token| (instance.key.clone(), token))
            })
            .collect::<Vec<_>>();
        due.sort_by_key(|(_, token)| token.deadline());
        // leave input and rendering a turn even when many timers become due together.
        due.truncate(8);
        due
    }

    pub(crate) fn dispatch_timer_checked(
        &mut self,
        key: &K,
        token: &TimerToken,
        now: Instant,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<Option<DispatchOutcome>, String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok(None);
        };
        let Some(changed) = runtime
            .dispatch_timer_checked(token, now, validate)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        Ok(Some(DispatchOutcome {
            changed,
            actions: runtime.take_actions(),
            ui_actions: runtime.take_ui_actions(),
        }))
    }

    /// each output's consumer with the idle watches its configuration
    /// declared, in declaration order.
    pub(crate) fn idle_watches(&self) -> Vec<(K, String, Vec<IdleWatchDefinition>)> {
        self.instances
            .iter()
            .map(|instance| {
                (
                    instance.key.clone(),
                    instance.consumer.clone(),
                    instance.runtime.idle_watches(),
                )
            })
            .collect()
    }

    /// reports an idle transition to the configuration that armed the watch.
    /// a transition for a replaced configuration or a repeated state does
    /// nothing.
    pub(crate) fn dispatch_idle_checked(
        &mut self,
        key: &K,
        consumer: &str,
        index: usize,
        idle: bool,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<Option<DispatchOutcome>, String> {
        let Some(instance) = self
            .instances
            .iter_mut()
            .find(|instance| &instance.key == key && instance.consumer == consumer)
        else {
            return Ok(None);
        };
        let runtime = &mut instance.runtime;
        let Some(changed) = runtime
            .dispatch_idle_checked(index, idle, validate)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        Ok(Some(DispatchOutcome {
            changed,
            actions: runtime.take_actions(),
            ui_actions: runtime.take_ui_actions(),
        }))
    }

    pub(crate) fn dispatch_key_checked(
        &mut self,
        key: &K,
        id: NodeId,
        input: KeyInput,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<Option<KeyHandlerOutcome>, String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok(None);
        };
        let Some(result) = runtime
            .dispatch_key_checked(id, input, validate)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        Ok(Some(KeyHandlerOutcome {
            handled: result.handled,
            dispatch: DispatchOutcome {
                changed: result.changed,
                actions: runtime.take_actions(),
                ui_actions: runtime.take_ui_actions(),
            },
        }))
    }

    pub(crate) fn begin_pointer_checked(
        &mut self,
        key: &K,
        target: &UiTarget,
        event: PointerEvent,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<PointerHandlerOutcome, String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok(PointerHandlerOutcome::inactive());
        };
        let outcome = runtime
            .begin_pointer_checked(target, event, validate)
            .map_err(|error| error.to_string())?;
        Ok(PointerHandlerOutcome::collect(runtime, outcome))
    }

    pub(crate) fn dispatch_pointer_checked(
        &mut self,
        key: &K,
        event: PointerEvent,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<PointerHandlerOutcome, String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok(PointerHandlerOutcome::inactive());
        };
        let outcome = runtime
            .dispatch_pointer_checked(event, validate)
            .map_err(|error| error.to_string())?;
        Ok(PointerHandlerOutcome::collect(runtime, outcome))
    }

    #[cfg(test)]
    pub(crate) fn reconcile_pointer_checked(
        &mut self,
        key: &K,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<PointerHandlerOutcome, String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok(PointerHandlerOutcome::inactive());
        };
        let outcome = runtime
            .reconcile_pointer_checked(validate)
            .map_err(|error| error.to_string())?;
        Ok(PointerHandlerOutcome::collect(runtime, outcome))
    }

    pub(crate) fn cancel_pointer_checked(
        &mut self,
        key: &K,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<PointerHandlerOutcome, String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok(PointerHandlerOutcome::inactive());
        };
        let outcome = runtime
            .cancel_pointer_checked(validate)
            .map_err(|error| error.to_string())?;
        Ok(PointerHandlerOutcome::collect(runtime, outcome))
    }

    pub(crate) fn dispatch_scroll_checked(
        &mut self,
        key: &K,
        target: &UiTarget,
        input: ScrollInput,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<Option<ScrollHandlerOutcome>, String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok(None);
        };
        let Some(outcome) = runtime
            .dispatch_scroll_checked(target, input, validate)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        Ok(Some(ScrollHandlerOutcome {
            consumed: outcome.consumed,
            dispatch: DispatchOutcome {
                changed: outcome.changed,
                actions: runtime.take_actions(),
                ui_actions: runtime.take_ui_actions(),
            },
        }))
    }

    pub(crate) fn prepare_reload_for_checked(
        &self,
        outputs: Vec<(K, OutputContext)>,
        validate: impl Fn(&K, &Runtime) -> Result<(), String>,
    ) -> Result<PreparedConfigurations<K>, String> {
        let mut candidates = Vec::with_capacity(outputs.len());
        let mut next_consumer = self.next_consumer;
        let mut services = self.services.clone();
        services
            .set_managed_processes(ManagedProcessSnapshot::default())
            .map_err(|error| error.to_string())?;
        for (key, output) in outputs {
            if candidates
                .iter()
                .any(|instance: &Instance<K>| instance.key == key)
            {
                return Err("duplicate output while reloading configuration".into());
            }
            let runtime = self.mode.load(&self.entry, output.clone(), &services)?;
            validate(&key, &runtime)?;
            let consumer = allocate_consumer(&mut next_consumer)?;
            candidates.push(Instance {
                key,
                consumer,
                output,
                runtime,
            });
        }
        let definitions = collect_managed_process_definitions(
            candidates.iter().map(|instance| &instance.runtime),
        )?;
        collect_sleep_inhibitor_definitions(candidates.iter().map(|instance| &instance.runtime))?;
        Ok(PreparedConfigurations {
            services,
            instances: candidates,
            next_consumer,
            process_definitions: definitions,
        })
    }

    pub(crate) fn commit_reload(&mut self, mut prepared: PreparedConfigurations<K>) {
        let now = Instant::now();
        for instance in &mut prepared.instances {
            instance.runtime.activate_timers(now);
        }
        self.services = prepared.services;
        self.next_consumer = prepared.next_consumer;
        self.instances = prepared.instances;
    }

    #[cfg(test)]
    fn reload(&mut self) -> Result<(), String> {
        let outputs = self
            .instances
            .iter()
            .map(|instance| (instance.key.clone(), instance.output.clone()))
            .collect();
        self.reload_for(outputs)
    }

    #[cfg(test)]
    pub(crate) fn reload_for(&mut self, outputs: Vec<(K, OutputContext)>) -> Result<(), String> {
        self.reload_for_checked(outputs, |_, _| Ok(()))
    }

    pub(crate) fn reload_for_checked(
        &mut self,
        outputs: Vec<(K, OutputContext)>,
        validate: impl Fn(&K, &Runtime) -> Result<(), String>,
    ) -> Result<(), String> {
        self.reload_for_checked_with_processes(outputs, validate, |_| Ok(()))
    }

    pub(crate) fn reload_for_checked_with_processes(
        &mut self,
        outputs: Vec<(K, OutputContext)>,
        validate: impl Fn(&K, &Runtime) -> Result<(), String>,
        replace: impl FnOnce(&[ManagedProcessDefinition]) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut candidates = Vec::with_capacity(outputs.len());
        let mut next_consumer = self.next_consumer;
        let mut services = self.services.clone();
        services
            .set_managed_processes(ManagedProcessSnapshot::default())
            .map_err(|error| error.to_string())?;
        for (key, output) in outputs {
            if candidates
                .iter()
                .any(|instance: &Instance<K>| instance.key == key)
            {
                return Err("duplicate output while reloading configuration".into());
            }
            let runtime = self.mode.load(&self.entry, output.clone(), &services)?;
            validate(&key, &runtime)?;
            let consumer = allocate_consumer(&mut next_consumer)?;
            candidates.push(Instance {
                key,
                consumer,
                output,
                runtime,
            });
        }
        let definitions = collect_managed_process_definitions(
            candidates.iter().map(|instance| &instance.runtime),
        )?;
        collect_sleep_inhibitor_definitions(candidates.iter().map(|instance| &instance.runtime))?;
        let snapshot = ManagedProcessSnapshot {
            states: definitions
                .iter()
                .map(|definition| ManagedProcessState::idle(definition.key.clone()))
                .collect(),
        };
        services
            .set_managed_processes(snapshot.clone())
            .map_err(|error| error.to_string())?;
        for candidate in &mut candidates {
            candidate
                .runtime
                .set_managed_processes(snapshot.clone())
                .map_err(|error| error.to_string())?;
        }
        replace(&definitions)?;
        self.services = services;
        self.next_consumer = next_consumer;
        let now = Instant::now();
        for instance in &mut candidates {
            instance.runtime.activate_timers(now);
        }
        self.instances = candidates;
        Ok(())
    }

    pub(crate) fn files(&self) -> Vec<PathBuf> {
        let mut files = BTreeSet::from([self.entry.clone()]);
        for instance in &self.instances {
            files.extend(instance.runtime.files());
        }
        files.into_iter().collect()
    }

    #[cfg(test)]
    pub(crate) fn root(&self, key: &K) -> Option<&Node> {
        self.runtime(key).and_then(Runtime::root)
    }

    pub(crate) fn surfaces(&self, key: &K) -> Option<&[SurfaceTree]> {
        self.runtime(key).map(Runtime::surfaces)
    }

    pub(crate) fn surface(&self, key: &K, surface: &SurfaceKey) -> Option<&SurfaceTree> {
        self.runtime(key)?.surface(surface)
    }

    pub(crate) fn surface_root(&self, key: &K, surface: &SurfaceKey) -> Option<&Node> {
        self.surface(key, surface).map(|surface| &surface.root)
    }
}

fn allocate_consumer(next: &mut u64) -> Result<String, String> {
    let value = *next;
    *next = next
        .checked_add(1)
        .ok_or_else(|| "configuration generation space exhausted".to_owned())?;
    Ok(format!("configuration{value}"))
}

fn collect_managed_process_definitions<'a>(
    runtimes: impl IntoIterator<Item = &'a Runtime>,
) -> Result<Vec<ManagedProcessDefinition>, String> {
    let mut definitions = BTreeMap::<String, ManagedProcessDefinition>::new();
    for definition in runtimes
        .into_iter()
        .flat_map(Runtime::managed_process_definitions)
    {
        if let Some(existing) = definitions.get(&definition.key)
            && existing != &definition
        {
            return Err(format!(
                "managed process {:?} has conflicting declarations across outputs",
                definition.key
            ));
        }
        definitions.insert(definition.key.clone(), definition);
    }
    Ok(definitions.into_values().collect())
}

/// a key may be declared by several outputs only with the same reason, so
/// one lease means one thing whichever output started it.
fn collect_sleep_inhibitor_definitions<'a>(
    runtimes: impl IntoIterator<Item = &'a Runtime>,
) -> Result<Vec<SleepInhibitorDefinition>, String> {
    let mut definitions = BTreeMap::<String, SleepInhibitorDefinition>::new();
    for definition in runtimes
        .into_iter()
        .flat_map(Runtime::sleep_inhibitor_definitions)
    {
        if let Some(existing) = definitions.get(&definition.key)
            && existing != &definition
        {
            return Err(format!(
                "sleep inhibitor {:?} has conflicting declarations across outputs",
                definition.key
            ));
        }
        definitions.insert(definition.key.clone(), definition);
    }
    Ok(definitions.into_values().collect())
}

fn reconciled_managed_processes(
    current: ManagedProcessSnapshot,
    definitions: &[ManagedProcessDefinition],
) -> ManagedProcessSnapshot {
    let mut current = current
        .states
        .into_iter()
        .map(|state| (state.key.clone(), state))
        .collect::<BTreeMap<_, _>>();
    ManagedProcessSnapshot {
        states: definitions
            .iter()
            .map(|definition| {
                current
                    .remove(&definition.key)
                    .unwrap_or_else(|| ManagedProcessState::idle(definition.key.clone()))
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, time::Duration};

    use tempfile::tempdir;
    use thing_core::{KeyboardMode, Kind, Point, Scale};
    use thing_runtime::{
        AudioDevice, AudioSource, AudioStream, MediaPlayer, Notification, NotificationButton,
        NotificationUrgency, PointerButton, PointerPhase, PowerDevice, SystemTrayItem,
        SystemTrayMenu, SystemTrayMenuItem, SystemTraySnapshot,
    };
    use thing_ui::{Layout, Measurer, Size};

    use super::*;

    fn output(name: &str, width: u32) -> OutputContext {
        OutputContext {
            name: Some(name.into()),
            width,
            height: 1080,
            scale: Scale::default(),
        }
    }

    #[test]
    fn timers_belong_to_accepted_output_runtimes_and_reject_retired_tokens() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        let source = r#"
            local count = ui.state(0)
            ui.timer { interval = 1000, autostart = true,
                on_timeout = function() count:set(count:get() + 1) end }
            return ui.panel { children = { ui.text { text = function()
                return services.output.name:get() .. ':' .. tostring(count:get())
            end } } }
        "#;
        fs::write(&path, source).unwrap();
        let mut configurations = Configurations::new(path.clone(), ServiceSnapshot::default());
        let outputs = vec![(1, output("DP-1", 1920)), (2, output("DP-2", 1920))];
        let prepared = configurations
            .prepare_reload_for_checked(outputs.clone(), |_, runtime| {
                assert!(runtime.next_timer_deadline().is_none());
                Ok(())
            })
            .unwrap();
        assert!(configurations.next_timer_deadline().is_none());
        configurations.commit_reload(prepared);
        let now = Instant::now() + Duration::from_secs(5);
        let tokens = configurations.due_timers(now);
        assert_eq!(tokens.len(), 2);
        assert!(
            configurations
                .dispatch_timer_checked(&2, &tokens[0].1, now, |_| Ok(()))
                .unwrap()
                .is_none()
        );
        configurations
            .dispatch_timer_checked(&tokens[0].0, &tokens[0].1, now, |_| Ok(()))
            .unwrap();
        assert_eq!(configurations.root(&1).unwrap().children[0].text, "DP-1:1");
        assert_eq!(configurations.root(&2).unwrap().children[0].text, "DP-2:0");
        let deadline = configurations.next_timer_deadline();
        fs::write(&path, "return ui.panel {").unwrap();
        assert!(
            configurations
                .prepare_reload_for_checked(outputs.clone(), |_, _| Ok(()))
                .is_err()
        );
        assert_eq!(configurations.next_timer_deadline(), deadline);
        fs::write(&path, source).unwrap();
        let prepared = configurations
            .prepare_reload_for_checked(outputs, |_, _| Ok(()))
            .unwrap();
        configurations.commit_reload(prepared);
        assert!(
            configurations
                .dispatch_timer_checked(&tokens[1].0, &tokens[1].1, now, |_| Ok(()))
                .unwrap()
                .is_none()
        );
        let due = configurations.due_timers(now);
        configurations.remove(&2).unwrap();
        let removed = due.iter().find(|(key, _)| *key == 2).unwrap();
        assert!(
            configurations
                .dispatch_timer_checked(&removed.0, &removed.1, now, |_| Ok(()))
                .unwrap()
                .is_none()
        );
        assert!(
            configurations
                .due_timers(now)
                .iter()
                .all(|(key, _)| *key == 1)
        );
    }

    #[test]
    fn autostart_timer_actions_run_once_per_output_and_resize_keeps_deadlines() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(
            &path,
            r#"
            ui.timer { interval = 1000, autostart = true,
                on_timeout = function() services.launch({ 'must-not-run' }) end }
            return ui.panel {}
        "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(path, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 1920)).unwrap();
        let deadline = configurations.next_timer_deadline();
        configurations.set_output(&1, output("DP-1", 1280)).unwrap();
        assert_eq!(configurations.next_timer_deadline(), deadline);
        let now = Instant::now() + Duration::from_secs(5);
        let due = configurations.due_timers(now);
        assert_eq!(due.len(), 2);
        for (output, token) in due {
            let outcome = configurations
                .dispatch_timer_checked(&output, &token, now, |_| Ok(()))
                .unwrap()
                .unwrap();
            assert_eq!(outcome.actions.len(), 1);
            assert!(
                matches!(&outcome.actions[0], Action::Launch { argv, .. } if argv == &["must-not-run"])
            );
        }
        assert!(configurations.next_timer_deadline().is_none());
    }

    #[test]
    fn due_timer_batches_are_bounded_and_do_not_replay_completed_callbacks() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(
            &path,
            r#"
            for index = 1, 12 do
                ui.timer { interval = index * 1000, autostart = true, on_timeout = function() end }
            end
            return ui.panel {}
        "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(path, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let now = Instant::now() + Duration::from_secs(60);
        let first = configurations.due_timers(now);
        assert_eq!(first.len(), 8);
        assert!(
            first
                .windows(2)
                .all(|pair| pair[0].1.deadline() <= pair[1].1.deadline())
        );
        for (key, token) in first {
            configurations
                .dispatch_timer_checked(&key, &token, now, |_| Ok(()))
                .unwrap();
        }
        assert_eq!(configurations.due_timers(now).len(), 4);
    }

    fn system_tray_item(id: &str) -> SystemTrayItem {
        SystemTrayItem {
            id: id.into(),
            protocol_item_id: "mail".into(),
            category: "Communications".into(),
            title: "Mail".into(),
            status: "Active".into(),
            icon: None,
            icon_name: "mail-unread".into(),
            attention_icon_name: String::new(),
            overlay_icon_name: String::new(),
            tooltip_icon_name: String::new(),
            tooltip_title: String::new(),
            tooltip_text: String::new(),
            item_is_menu: false,
            has_menu: true,
            menu: Some(Arc::new(SystemTrayMenu {
                id: format!("{id}-menu"),
                loaded: true,
                loading: false,
                error: String::new(),
                children: vec![SystemTrayMenuItem {
                    id: format!("{id}-compose"),
                    kind: "standard".into(),
                    label: "Compose".into(),
                    access_key: None,
                    enabled: true,
                    visible: true,
                    icon_name: String::new(),
                    shortcut: Vec::new(),
                    toggle_type: String::new(),
                    toggle_state: -1,
                    disposition: "normal".into(),
                    has_submenu: false,
                    loaded: false,
                    loading: false,
                    error: String::new(),
                    children: Vec::new(),
                }],
            })),
            has_icon_pixmap: false,
            has_attention_icon_pixmap: false,
            has_overlay_icon_pixmap: false,
            has_tooltip_icon_pixmap: false,
        }
    }

    fn managed_process_state(
        key: &str,
        status: thing_runtime::ManagedProcessStatus,
        stdout: &str,
    ) -> ManagedProcessState {
        ManagedProcessState {
            key: key.into(),
            status,
            stdout: Arc::from(stdout),
            stderr: Arc::from(""),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: None,
            exit_signal: None,
            error: Arc::from(""),
        }
    }

    fn text(configurations: &Configurations<u8>, key: u8) -> &str {
        fn find(node: &Node) -> Option<&str> {
            if node.kind == Kind::Text {
                return Some(&node.text);
            }
            node.children.iter().find_map(find)
        }
        find(configurations.root(&key).unwrap()).unwrap()
    }

    #[test]
    fn lock_configurations_keep_the_lock_loader_across_reload() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("lock.luau");
        fs::write(
            &entry,
            "return ui.lock { children = { ui.text { text = 'old' } } }",
        )
        .unwrap();
        let mut configurations =
            Configurations::new_lock(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        assert_eq!(configurations.root(&1).unwrap().kind, Kind::Lock);
        assert_eq!(text(&configurations, 1), "old");

        fs::write(
            &entry,
            "return ui.lock { children = { ui.text { text = 'new' } } }",
        )
        .unwrap();
        configurations.reload().unwrap();
        assert_eq!(configurations.root(&1).unwrap().kind, Kind::Lock);
        assert_eq!(text(&configurations, 1), "new");
    }

    #[test]
    fn prepared_lock_candidates_keep_the_lock_loader() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("lock.luau");
        fs::write(
            &entry,
            "return ui.lock { children = { ui.text { text = 'old' } } }",
        )
        .unwrap();
        let mut configurations =
            Configurations::new_lock(entry.clone(), ServiceSnapshot::default());
        let candidate = configurations
            .prepare_add_checked(1, output("DP-1", 1920), |_| Ok(()))
            .unwrap()
            .unwrap();
        configurations.commit_add(candidate);
        assert_eq!(configurations.root(&1).unwrap().kind, Kind::Lock);

        fs::write(
            &entry,
            "return ui.lock { children = { ui.text { text = 'new' } } }",
        )
        .unwrap();
        let replacement = configurations
            .prepare_reload_for_checked(vec![(1, output("DP-1", 1920))], |_, _| Ok(()))
            .unwrap();
        configurations.commit_reload(replacement);
        assert_eq!(configurations.root(&1).unwrap().kind, Kind::Lock);
        assert_eq!(text(&configurations, 1), "new");
    }

    fn power_device(path: &str) -> PowerDevice {
        PowerDevice {
            id: path.into(),
            kind: "battery".into(),
            state: "discharging".into(),
            power_supply: true,
            present: Some(true),
            online: None,
            percentage: Some(42.0),
            time_to_empty: Some(3600),
            time_to_full: None,
            energy: Some(21.0),
            energy_empty: Some(0.0),
            energy_full: Some(50.0),
            energy_full_design: Some(55.0),
            energy_rate: Some(8.0),
            capacity: Some(90.0),
            native_path: Some("BAT0".into()),
            vendor: None,
            model: Some("battery".into()),
            serial: None,
            icon_name: Some("battery-good-symbolic".into()),
            warning_level: "none".into(),
            battery_level: None,
        }
    }

    struct FixedText;

    impl Measurer for FixedText {
        fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
            Size {
                width: 30.0,
                height: 12.0,
            }
        }
    }

    #[test]
    fn bounds_follow_presented_layout_and_surface_ownership() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local result = ui.state('')
            local target = ui.row { key = 'target', enabled = false, style = {
                width = 80, height = 20, translate_x = 325, scale_x = 2,
                transform_origin = { x = 0, y = 0 },
            } }
            return ui.panel { style = { width = 300, height = 80, padding = 10 }, children = {
                target,
                ui.button { key = 'read', on_click = function()
                    local bounds = ui.bounds(target)
                    result:set(if bounds then string.format('%g %g %g %g', bounds.layout.x,
                        bounds.layout.width, bounds.visual.x, bounds.visual.width) else 'none')
                end },
                ui.text { text = function() return result:get() end },
            } }
        "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        let root = configurations.root(&1).unwrap().id;
        let button = configurations.root(&1).unwrap().children[1].id;
        let second_button = configurations.root(&2).unwrap().children[1].id;
        let consumer = configurations.consumer(&1).unwrap().to_owned();
        let layout = Rc::new(
            Layout::compute(
                configurations.root(&1).unwrap(),
                Size {
                    width: 300.0,
                    height: 80.0,
                },
                &mut FixedText,
            )
            .unwrap(),
        );
        let target = configurations.root(&1).unwrap().children[0].id;
        let painted = layout.get(target).unwrap();
        assert!(painted.border_aabb.intersection(painted.clip).is_none());
        assert!(layout.semantic_placement(target).is_some());
        assert!(
            configurations
                .record_presented_bounds(&1, "retired", root, layout.clone())
                .is_none()
        );
        let lease = configurations
            .record_presented_bounds(&1, &consumer, root, layout.clone())
            .unwrap();
        let mut surface = crate::focus::SurfaceLayouts::default();
        surface.publish_shared(layout, root, consumer, BTreeMap::new());
        surface.keep_bounds(Some(lease));
        configurations.dispatch(&1, button, Event::Click).unwrap();
        assert_eq!(
            configurations.root(&1).unwrap().children[2].text,
            "10 80 335 160"
        );
        configurations
            .dispatch(&2, second_button, Event::Click)
            .unwrap();
        assert_eq!(configurations.root(&2).unwrap().children[2].text, "none");

        let mut input_root = configurations.root(&1).unwrap().clone();
        input_root.children[0].style.translate_x = 75.0;
        let input_layout = Layout::compute(
            &input_root,
            Size {
                width: 300.0,
                height: 80.0,
            },
            &mut FixedText,
        )
        .unwrap();
        surface.stage_input(input_layout);
        configurations.dispatch(&1, button, Event::Click).unwrap();
        assert_eq!(
            configurations.root(&1).unwrap().children[2].text,
            "10 80 335 160"
        );
        drop(surface);
        configurations.dispatch(&1, button, Event::Click).unwrap();
        assert_eq!(configurations.root(&1).unwrap().children[2].text, "none");
    }

    #[test]
    fn output_instances_have_independent_context_and_state() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local count = ui.state(0)
            return ui.panel { children = { ui.button {
                on_click = function() count:set(count:get() + 1) end,
                children = { ui.text { text = function()
                    return `{services.output.name:get()}:{services.output.width:get()}:{count:get()}`
                end } },
            } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        let consumers = configurations.consumers();
        assert_eq!(consumers.len(), 2);
        assert_ne!(consumers[0], consumers[1]);
        assert_eq!(text(&configurations, 1), "DP-1:1920:0");
        assert_eq!(text(&configurations, 2), "DP-2:2560:0");

        let button = configurations.root(&1).unwrap().children[0].id;
        assert!(
            configurations
                .dispatch(&1, button, Event::Click)
                .unwrap()
                .changed
        );
        assert_eq!(text(&configurations, 1), "DP-1:1920:1");
        assert_eq!(text(&configurations, 2), "DP-2:2560:0");
    }

    #[test]
    fn local_ui_actions_keep_runtime_and_reload_identity_separate() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        let source = r#"
            local target = ui.button { key = 'target', autofocus = true }
            return ui.panel { children = {
                ui.button { on_click = function()
                    ui.reveal(target)
                    ui.focus(target)
                end },
                target,
            } }
            "#;
        fs::write(&entry, source).unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let root = configurations.root(&1).unwrap();
        let root_id = root.id;
        let trigger = root.children[0].id;
        let target_id = root.children[1].id;
        let consumer = configurations.consumer(&1).unwrap().to_owned();

        let outcome = configurations.dispatch(&1, trigger, Event::Click).unwrap();
        assert!(!outcome.changed);
        assert!(outcome.actions.is_empty());
        assert!(matches!(
            outcome.ui_actions.as_slice(),
            [UiAction::Reveal(reveal), UiAction::Focus(focus)]
                if reveal.node() == target_id && focus.node() == target_id
        ));
        let target = outcome.ui_actions[1].target().clone();
        assert!(configurations.ui_target_is_current(&1, &target));
        assert_eq!(
            configurations.rendered_surface_root(&1, target_id),
            Some(root_id)
        );
        assert_eq!(
            configurations.autofocus_target(&1, root_id).unwrap().node(),
            target_id
        );

        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = 42 } } }",
        )
        .unwrap();
        assert!(
            configurations
                .reload_for(vec![(1, output("DP-1", 1920))])
                .is_err()
        );
        assert_eq!(configurations.consumer(&1), Some(consumer.as_str()));
        assert!(configurations.ui_target_is_current(&1, &target));

        fs::write(&entry, source).unwrap();
        configurations
            .reload_for(vec![(1, output("DP-1", 1920))])
            .unwrap();
        assert_ne!(configurations.consumer(&1), Some(consumer.as_str()));
    }

    #[test]
    fn output_refresh_invalidates_an_unkeyed_focus_target() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local target = nil
            local list = ui.column { children = function()
                local width = services.output.width:get()
                target = ui.button {}
                return { target }
            end }
            return ui.panel { children = { list } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let target_id = configurations.root(&1).unwrap().children[0].children[0].id;
        let target = configurations.focus_target(&1, target_id).unwrap();

        assert!(configurations.set_output(&1, output("DP-1", 2560)).unwrap());
        assert_eq!(
            configurations.root(&1).unwrap().children[0].children[0].id,
            target_id
        );
        assert!(!configurations.ui_target_is_current(&1, &target));
    }

    #[test]
    fn transitions_are_paced_only_for_their_output_and_visible_layout() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local expanded = ui.state(false)
            return ui.panel { children = { ui.button {
                on_click = function() expanded:set(true) end,
                style = { width = ui.transition(function()
                    return expanded:get() and 200 or 100
                end, { duration = 100, easing = 'linear' }) },
            } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        let first = configurations.root(&1).unwrap().children[0].id;
        let second = configurations.root(&2).unwrap().children[0].id;

        assert!(
            configurations
                .dispatch(&1, first, Event::Click)
                .unwrap()
                .changed
        );
        assert!(configurations.runtime(&1).unwrap().is_animating(first));
        assert!(!configurations.runtime(&2).unwrap().is_animating(second));
        let layout = Layout::compute(
            configurations.root(&1).unwrap(),
            Size {
                width: 300.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert!(configurations.layout_is_animating(&1, &layout));

        configurations
            .advance(&1, Instant::now() + Duration::from_secs(1))
            .unwrap();
        let layout = Layout::compute(
            configurations.root(&1).unwrap(),
            Size {
                width: 300.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert!(!configurations.layout_is_animating(&1, &layout));
        assert_eq!(
            configurations.root(&1).unwrap().children[0].style.width,
            Some(200.0)
        );
        assert_eq!(
            configurations.root(&2).unwrap().children[0].style.width,
            Some(100.0)
        );
    }

    #[test]
    fn hidden_transitions_do_not_keep_a_surface_drawing() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local expanded = ui.state(false)
            local visible = ui.state(true)
            return ui.panel { children = {
                ui.button { on_click = function()
                    expanded:set(true)
                    visible:set(false)
                end },
                ui.row {
                    visible = function() return visible:get() end,
                    style = { width = ui.transition(function()
                        return expanded:get() and 200 or 100
                    end, { duration = 1000 }) },
                },
            } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let button = configurations.root(&1).unwrap().children[0].id;
        let animated = configurations.root(&1).unwrap().children[1].id;

        configurations.dispatch(&1, button, Event::Click).unwrap();
        assert!(configurations.runtime(&1).unwrap().is_animating(animated));
        let layout = Layout::compute(
            configurations.root(&1).unwrap(),
            Size {
                width: 300.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert!(!configurations.layout_is_animating(&1, &layout));
    }

    #[test]
    fn colliding_node_ids_dispatch_to_the_owning_output() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local open = ui.state(false)
            return ui.panel { children = { ui.button {
                on_click = function() open:set(true) end,
                children = { ui.popup { visible = function() return open:get() end } },
            } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        let first = configurations.root(&1).unwrap().children[0].id;
        let second = configurations.root(&2).unwrap().children[0].id;
        assert_eq!(first, second);

        configurations.dispatch(&1, first, Event::Click).unwrap();
        assert!(configurations.root(&1).unwrap().children[0].children[0].visible);
        assert!(!configurations.root(&2).unwrap().children[0].children[0].visible);
    }

    #[test]
    fn presented_hits_follow_keyed_identity_but_not_reused_node_ids() {
        for keyed in [false, true] {
            let directory = tempdir().unwrap();
            let entry = directory.path().join("panel.luau");
            let source = r#"
                local replaced = ui.state(false)
                local fired = ui.state('none')
                local list = ui.row { children = function()
                    local replacement = replaced:get()
                    return { ui.button {
                        __KEY__
                        style = {
                            width = replacement and 20 or 100,
                            height = 30,
                        },
                        on_click = function()
                            fired:set(replacement and 'new' or 'old')
                        end,
                    } }
                end }
                return ui.panel { children = {
                    ui.button {
                        key = 'replace',
                        on_click = function() replaced:set(true) end,
                    },
                    list,
                    ui.text { text = function() return fired:get() end },
                } }
                "#
            .replace("__KEY__", if keyed { "key = 'target'," } else { "" });
            fs::write(&entry, source).unwrap();
            let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
            configurations.add(1, output("DP-1", 1920)).unwrap();

            let root = configurations.root(&1).unwrap();
            let surface_root = root.id;
            let replace = root.children[0].id;
            let target = root.children[1].children[0].id;
            let presented = Layout::compute(
                root,
                Size {
                    width: 300.0,
                    height: 80.0,
                },
                &mut FixedText,
            )
            .unwrap();
            let old = presented.get(target).unwrap().bounds;
            let point = Point {
                x: old.x + old.width - 1.0,
                y: old.y + old.height / 2.0,
            };
            assert_eq!(presented.hit_test(point), Some(target));
            let (consumer, presented_targets) = configurations
                .presented_targets(&1, root, &presented)
                .unwrap();
            let presented_target = presented_targets.get(&target).unwrap().clone();

            assert!(
                configurations
                    .dispatch(&1, replace, Event::Click)
                    .unwrap()
                    .changed
            );
            assert_eq!(
                configurations.root(&1).unwrap().children[1].children[0].id,
                target,
                "both replacements reuse the presented node id",
            );
            let current = Layout::compute(
                configurations.root(&1).unwrap(),
                Size {
                    width: 300.0,
                    height: 80.0,
                },
                &mut FixedText,
            )
            .unwrap();
            assert_ne!(current.hit_test(point), Some(target));
            let current = configurations.presented_target_is_current(
                &1,
                &consumer,
                surface_root,
                &presented_target,
            );
            assert_eq!(current, keyed);

            if current {
                configurations
                    .dispatch(&1, presented_target.node(), Event::Click)
                    .unwrap();
            }
            assert_eq!(text(&configurations, 1), if keyed { "new" } else { "none" });
        }
    }

    #[test]
    fn scroll_fallback_rejects_a_replaced_presented_ancestor() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local revision = ui.state(0)
            local area = ui.pointer_area {
                key = 'area',
                style = { width = 100, height = 40 },
                on_scroll = function()
                    revision:set(revision:get() + 1)
                    return false
                end,
                children = function()
                    revision:get()
                    return { ui.scroll {
                        style = { width = 100, height = 40 },
                        children = {
                            ui.row { style = { width = 100, height = 80 } },
                        },
                    } }
                end,
            }
            return ui.panel { children = { area } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();

        let root = configurations.root(&1).unwrap();
        let surface_root = root.id;
        let area = root.children[0].id;
        let scroll = root.children[0].children[0].id;
        let layout = Layout::compute(
            root,
            Size {
                width: 100.0,
                height: 40.0,
            },
            &mut FixedText,
        )
        .unwrap();
        let point = Point { x: 10.0, y: 10.0 };
        assert_eq!(
            layout.scroll_route(point),
            [
                thing_ui::ScrollStep::Viewport(scroll),
                thing_ui::ScrollStep::Handler(area)
            ]
        );
        let (consumer, targets) = configurations.presented_targets(&1, root, &layout).unwrap();
        let area_target = targets.get(&area).unwrap().clone();
        let scroll_target = targets.get(&scroll).unwrap().clone();

        let outcome = configurations
            .dispatch_scroll_checked(
                &1,
                &area_target,
                ScrollInput {
                    x: 0.0,
                    y: 0.25,
                    local_x: 10.0,
                    local_y: 10.0,
                    surface_x: 10.0,
                    surface_y: 10.0,
                    shift: false,
                    ctrl: false,
                    alt: false,
                    logo: false,
                    source: thing_runtime::ScrollSource::Wheel,
                },
                |_| Ok(()),
            )
            .unwrap()
            .unwrap();
        assert!(!outcome.consumed);
        assert!(
            outcome.dispatch.changed,
            "identity-only replacement needs a fresh presented target map",
        );
        assert_eq!(
            configurations.root(&1).unwrap().children[0].children[0].id,
            scroll,
            "the replacement deliberately reuses the presented node id",
        );
        assert!(configurations.presented_scroll_target_is_current(
            &1,
            &consumer,
            surface_root,
            &area_target,
        ));
        assert!(!configurations.presented_scroll_view_target_is_current(
            &1,
            &consumer,
            surface_root,
            &scroll_target,
        ));
    }

    #[test]
    fn source_only_replacements_request_a_presented_identity_update() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local fired = ui.state('none')
            local list = ui.row { children = function()
                local available = services.audio.available:get()
                return { ui.button {
                    style = { width = 100, height = 30 },
                    on_scroll = function()
                        fired:set(available and 'new' or 'old')
                    end,
                } }
            end }
            return ui.panel { children = {
                list,
                ui.text { text = function() return fired:get() end },
            } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();

        let root = configurations.root(&1).unwrap();
        let surface_root = root.id;
        let target = root.children[0].children[0].id;
        let presented = Layout::compute(
            root,
            Size {
                width: 300.0,
                height: 80.0,
            },
            &mut FixedText,
        )
        .unwrap();
        let (consumer, presented_targets) = configurations
            .presented_targets(&1, root, &presented)
            .unwrap();
        let presented_target = presented_targets.get(&target).unwrap().clone();

        configurations
            .set_audio(AudioSnapshot {
                available: true,
                ..AudioSnapshot::default()
            })
            .unwrap();
        let refreshed = configurations.refresh();

        assert_eq!(
            configurations.root(&1).unwrap().children[0].children[0].id,
            target,
            "the structurally identical replacement reuses its node id",
        );
        let current = Layout::compute(
            configurations.root(&1).unwrap(),
            Size {
                width: 300.0,
                height: 80.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(
            presented.placements(),
            current.placements(),
            "the replacement has identical presented geometry",
        );
        assert!(!configurations.presented_target_is_current(
            &1,
            &consumer,
            surface_root,
            &presented_target,
        ));
        assert_eq!(
            refreshed,
            [1],
            "the host needs a frame to publish the replacement input identity",
        );
        let (_, current_targets) = configurations
            .presented_targets(&1, configurations.root(&1).unwrap(), &current)
            .unwrap();
        let current_target = current_targets.get(&target).unwrap();
        assert!(configurations.presented_target_is_current(
            &1,
            &consumer,
            surface_root,
            current_target,
        ));

        configurations
            .dispatch(&1, current_target.node(), Event::Scroll { x: 0.0, y: 1.0 })
            .unwrap();
        assert_eq!(text(&configurations, 1), "new");
    }

    #[test]
    fn presented_pointer_areas_cancel_after_output_refresh_hides_the_target() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local phase = ui.state('none')
            return ui.panel { children = {
                ui.pointer_area {
                    key = 'drag-region',
                    visible = function() return services.output.width:get() < 2000 end,
                    style = { width = 100, height = 40 },
                    on_pointer = function(event)
                        phase:set(event.phase)
                        return true
                    end,
                },
                ui.text { text = function() return phase:get() end },
            } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let root = configurations.root(&1).unwrap();
        let area = root.children[0].id;
        let layout = Layout::compute(
            root,
            Size {
                width: 300.0,
                height: 80.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert_eq!(
            layout.pointer_event_target(Point { x: 50.0, y: 20.0 }),
            Some(area),
        );
        let (_, targets) = configurations.presented_targets(&1, root, &layout).unwrap();
        let target = targets
            .get(&area)
            .expect("a presented pointer area needs an identity snapshot")
            .clone();
        let press = PointerEvent {
            phase: PointerPhase::Press,
            button: PointerButton::Primary,
            x: 50.0,
            y: 20.0,
            surface_x: 50.0,
            surface_y: 20.0,
            shift: false,
            ctrl: false,
            alt: false,
            logo: false,
        };
        let pressed = configurations
            .begin_pointer_checked(&1, &target, press, |_| Ok(()))
            .unwrap();
        assert!(pressed.active);
        assert!(pressed.handled);
        assert_eq!(text(&configurations, 1), "press");

        assert!(configurations.set_output(&1, output("DP-1", 2560)).unwrap());
        let cancelled = configurations
            .reconcile_pointer_checked(&1, |_| Ok(()))
            .unwrap();
        assert!(!cancelled.active);
        assert!(cancelled.handled);
        assert!(cancelled.dispatch.changed);
        assert_eq!(text(&configurations, 1), "cancel");
    }

    #[test]
    fn singular_presented_geometry_requires_host_capture_cancellation() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local collapsed = ui.state(false)
            local phase = ui.state('none')
            return ui.panel { children = {
                ui.pointer_area {
                    key = 'drag-region',
                    style = {
                        width = 100,
                        height = 40,
                        scale_x = function() return collapsed:get() and 0 or 1 end,
                    },
                    on_pointer = function(event)
                        phase:set(event.phase)
                        if event.phase == 'press' then collapsed:set(true) end
                        return true
                    end,
                },
                ui.text { text = function() return phase:get() end },
            } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let root = configurations.root(&1).unwrap();
        let surface_root = root.id;
        let area = root.children[0].id;
        let layout = Layout::compute(
            root,
            Size {
                width: 300.0,
                height: 80.0,
            },
            &mut FixedText,
        )
        .unwrap();
        let (_, targets) = configurations.presented_targets(&1, root, &layout).unwrap();
        let target = targets.get(&area).unwrap().clone();
        let pressed = configurations
            .begin_pointer_checked(
                &1,
                &target,
                PointerEvent {
                    phase: PointerPhase::Press,
                    button: PointerButton::Primary,
                    x: 50.0,
                    y: 20.0,
                    surface_x: 50.0,
                    surface_y: 20.0,
                    shift: false,
                    ctrl: false,
                    alt: false,
                    logo: false,
                },
                |_| Ok(()),
            )
            .unwrap();
        assert!(pressed.active);
        assert_eq!(text(&configurations, 1), "press");

        let root = configurations.root(&1).unwrap();
        let singular = Layout::compute(
            root,
            Size {
                width: 300.0,
                height: 80.0,
            },
            &mut FixedText,
        )
        .unwrap();
        assert!(!singular.pointer_targets().any(|id| id == area));
        let (_, targets) = configurations
            .presented_targets(&1, root, &singular)
            .unwrap();
        assert!(targets.contains_key(&surface_root));
        assert!(!singular.pointer_target_is_eligible(surface_root));
        assert!(!targets.contains_key(&area));
        assert!(configurations.active_pointer_matches(&1, &target));

        let cancelled = configurations
            .cancel_pointer_checked(&1, |_| Ok(()))
            .unwrap();
        assert!(!cancelled.active);
        assert!(cancelled.handled);
        assert!(cancelled.dispatch.changed);
        assert_eq!(text(&configurations, 1), "cancel");
    }

    #[test]
    fn output_changes_update_reactive_properties() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = function() return tostring(services.output.width:get()) end } } }",
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();

        assert!(configurations.set_output(&1, output("DP-1", 2560)).unwrap());
        assert_eq!(text(&configurations, 1), "2560");
    }

    #[test]
    fn checked_output_update_rejects_unsupported_candidate_and_preserves_previous_surface() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            return ui.panel {
                keyboard_mode = function()
                    return if services.output.width:get() > 2000 then 'on_demand' else 'none'
                end,
            }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let reject_on_demand = |surfaces: &[SurfaceTree]| {
            if surfaces
                .iter()
                .any(|surface| surface.root.panel.keyboard_mode == KeyboardMode::OnDemand)
            {
                Err("unsupported keyboard mode".into())
            } else {
                Ok(())
            }
        };

        assert!(
            configurations
                .set_output_checked(&1, output("DP-1", 2560), reject_on_demand)
                .is_err()
        );
        assert_eq!(
            configurations.root(&1).unwrap().panel.keyboard_mode,
            KeyboardMode::None
        );
        assert_eq!(configurations.output(&1).unwrap().width, 2560);

        assert!(
            !configurations
                .set_output_checked(&1, output("DP-1", 1920), reject_on_demand)
                .unwrap()
        );
        assert_eq!(
            configurations.root(&1).unwrap().panel.keyboard_mode,
            KeyboardMode::None
        );
    }

    #[test]
    fn checked_service_refresh_rejects_unsupported_candidate_and_preserves_previous_surface() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            return ui.panel {
                keyboard_mode = function()
                    return if services.clock.time:get() == 'unsupported'
                        then 'on_demand' else 'none'
                end,
            }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let reject_on_demand = |_: &u8, surfaces: &[SurfaceTree]| {
            if surfaces
                .iter()
                .any(|surface| surface.root.panel.keyboard_mode == KeyboardMode::OnDemand)
            {
                Err("unsupported keyboard mode".into())
            } else {
                Ok(())
            }
        };

        configurations
            .set_service("clock.time", Scalar::String("unsupported".into()))
            .unwrap();
        assert!(configurations.refresh_checked(reject_on_demand).is_empty());
        assert_eq!(
            configurations.root(&1).unwrap().panel.keyboard_mode,
            KeyboardMode::None
        );

        configurations
            .set_service("clock.time", Scalar::String("supported".into()))
            .unwrap();
        assert!(configurations.refresh_checked(reject_on_demand).is_empty());
        assert_eq!(
            configurations.root(&1).unwrap().panel.keyboard_mode,
            KeyboardMode::None
        );
    }

    #[test]
    fn failed_reload_preserves_every_output() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = services.output.name:get() } } }",
        )
        .unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        let consumers = configurations.consumers();
        fs::write(
            &entry,
            "if services.output.name:get() == 'DP-2' then error('bad output') end\nreturn ui.panel { children = { ui.text { text = 'new' } } }",
        )
        .unwrap();

        assert!(configurations.reload().is_err());
        assert_eq!(configurations.consumers(), consumers);
        assert_eq!(text(&configurations, 1), "DP-1");
        assert_eq!(text(&configurations, 2), "DP-2");
    }

    #[test]
    fn failed_host_preflight_preserves_every_output() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = 'old' } } }",
        )
        .unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        let previous_consumers = configurations.consumers();
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = 'new' } } }",
        )
        .unwrap();

        let result = configurations.reload_for_checked(
            vec![(1, output("DP-1", 1920)), (2, output("DP-2", 2560))],
            |key, _| {
                if *key == 2 {
                    Err("unsupported surface capability".into())
                } else {
                    Ok(())
                }
            },
        );

        assert!(result.is_err());
        assert_eq!(configurations.consumers(), previous_consumers);
        assert_eq!(text(&configurations, 1), "old");
        assert_eq!(text(&configurations, 2), "old");
    }

    #[test]
    fn successful_reload_replaces_every_output_together() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = 'old:' .. services.output.name:get() } } }",
        )
        .unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        let previous_consumers = configurations.consumers();
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = 'new:' .. services.output.name:get() } } }",
        )
        .unwrap();

        configurations.reload().unwrap();
        let current_consumers = configurations.consumers();
        assert_eq!(current_consumers.len(), 2);
        assert_ne!(current_consumers[0], current_consumers[1]);
        assert!(
            current_consumers
                .iter()
                .all(|consumer| !previous_consumers.contains(consumer))
        );
        assert_eq!(text(&configurations, 1), "new:DP-1");
        assert_eq!(text(&configurations, 2), "new:DP-2");
    }

    #[test]
    fn new_outputs_start_with_the_latest_service_snapshot() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = services.clock.time:get() } } }",
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations
            .set_service("clock.time", Scalar::String("12:34".into()))
            .unwrap();
        configurations.add(1, output("DP-1", 1920)).unwrap();
        assert_eq!(text(&configurations, 1), "12:34");
    }

    #[test]
    fn readded_outputs_start_with_the_latest_service_snapshot() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = services.clock.time:get() } } }",
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations
            .set_service("clock.time", Scalar::String("12:34".into()))
            .unwrap();
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let first_consumer = configurations.remove(&1).unwrap();
        configurations
            .set_service("clock.time", Scalar::String("12:35".into()))
            .unwrap();

        configurations.add(1, output("DP-1", 1920)).unwrap();
        assert_ne!(configurations.consumer(&1), Some(first_consumer.as_str()));
        assert_eq!(text(&configurations, 1), "12:35");
    }

    #[test]
    fn audio_updates_reach_every_output_as_one_validated_snapshot() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            return ui.panel { children = { ui.text { text = function()
                local sources = services.audio.sources:get()
                local description = if #sources == 0 then 'none' else sources[1].description
                local streams = services.audio.streams:get()
                local application = if #streams == 0 then 'none' else streams[1].application
                local all_sources = services.audio.all_sources:get()
                local monitor = if #all_sources == 0 then 'none' else tostring(all_sources[#all_sources].monitor)
                return `{services.audio.volume:get()}:{services.audio.source_volume:get()}:{description}:{#all_sources}:{monitor}:{application}`
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations
            .set_audio(AudioSnapshot {
                available: true,
                volume: 0.4,
                source_available: true,
                source_volume: 0.7,
                sources: vec![AudioDevice {
                    name: "microphone".into(),
                    description: "Microphone".into(),
                }],
                all_sources: vec![
                    AudioSource {
                        name: "microphone".into(),
                        description: "Microphone".into(),
                        monitor: false,
                    },
                    AudioSource {
                        name: "speakers.monitor".into(),
                        description: "Monitor of Speakers".into(),
                        monitor: true,
                    },
                ],
                streams: vec![AudioStream {
                    id: "stream-1".into(),
                    application: "Firefox".into(),
                    app_id: "firefox".into(),
                    name: "Music".into(),
                    icon: "firefox".into(),
                    volume: 0.5,
                    muted: false,
                    volume_writable: true,
                }],
                ..AudioSnapshot::default()
            })
            .unwrap();
        assert_eq!(configurations.refresh(), vec![1]);
        assert_eq!(
            text(&configurations, 1),
            "0.4:0.7:Microphone:2:true:Firefox"
        );

        configurations.add(2, output("DP-2", 2560)).unwrap();
        assert_eq!(
            text(&configurations, 2),
            "0.4:0.7:Microphone:2:true:Firefox"
        );
        assert!(
            configurations
                .set_audio(AudioSnapshot {
                    volume: 0.9,
                    source_volume: 0.2,
                    sources: vec![AudioDevice {
                        name: "webcam".into(),
                        description: "Webcam".into(),
                    }],
                    all_sources: vec![
                        AudioSource {
                            name: "duplicate".into(),
                            description: "First".into(),
                            monitor: false,
                        },
                        AudioSource {
                            name: "duplicate".into(),
                            description: "Second".into(),
                            monitor: true,
                        },
                    ],
                    streams: vec![AudioStream {
                        id: "stream-2".into(),
                        application: "Music".into(),
                        app_id: "music".into(),
                        name: "Playback".into(),
                        icon: "music".into(),
                        volume: 0.8,
                        muted: false,
                        volume_writable: true,
                    }],
                    ..AudioSnapshot::default()
                })
                .is_err()
        );
        assert!(configurations.refresh().is_empty());
        for key in [1, 2] {
            assert_eq!(
                text(&configurations, key),
                "0.4:0.7:Microphone:2:true:Firefox"
            );
        }
        configurations.add(3, output("DP-3", 1920)).unwrap();
        assert_eq!(
            text(&configurations, 3),
            "0.4:0.7:Microphone:2:true:Firefox"
        );
    }

    #[test]
    fn media_updates_reach_existing_and_later_outputs_together() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            return ui.panel { children = { ui.text { text = function()
                local players = services.media.players:get()
                local id = if #players == 0 then '' else players[1].id
                return `{services.media.title:get()}:{services.media.artist:get()}:{services.media.playing:get()}:{#players}:{id}`
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let player = MediaPlayer {
            id: "p1".into(),
            name: "org.mpris.MediaPlayer2.test".into(),
            playing: true,
            title: "Track".into(),
            artist: "Artist".into(),
            art_url: String::new(),
            identity: "Test player".into(),
            desktop_entry: "test".into(),
            can_play_pause: true,
            can_next: true,
            can_previous: true,
            can_stop: true,
            can_seek: false,
            track_id: String::new(),
            length_us: None,
            rate: 1.0,
            position: None,
        };
        configurations
            .set_media(MediaSnapshot {
                available: true,
                selected_id: player.id.clone(),
                playing: player.playing,
                title: player.title.clone(),
                artist: player.artist.clone(),
                art_url: player.art_url.clone(),
                identity: player.identity.clone(),
                desktop_entry: player.desktop_entry.clone(),
                can_play_pause: player.can_play_pause,
                can_next: player.can_next,
                can_previous: player.can_previous,
                can_stop: player.can_stop,
                can_seek: player.can_seek,
                length_us: player.length_us,
                rate: player.rate,
                players: vec![player],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(configurations.refresh(), vec![1]);
        assert_eq!(text(&configurations, 1), "Track:Artist:true:1:p1");

        configurations.add(2, output("DP-2", 2560)).unwrap();
        assert_eq!(text(&configurations, 2), "Track:Artist:true:1:p1");
        assert!(
            configurations
                .set_media(MediaSnapshot {
                    title: "bad\0title".into(),
                    ..MediaSnapshot::default()
                })
                .is_err()
        );
        assert!(configurations.refresh().is_empty());
        assert_eq!(text(&configurations, 1), "Track:Artist:true:1:p1");
        assert_eq!(text(&configurations, 2), "Track:Artist:true:1:p1");
    }

    #[test]
    fn power_updates_reach_existing_and_later_outputs_together() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            return ui.panel { children = { ui.text { text = function()
                local display = services.power.display:get() or { percentage = 0 }
                local devices = services.power.devices:get()
                local first = devices[1] or { state = 'none' }
                return `{services.power.available:get()}:{services.power.on_battery:get()}:{display.percentage}:{#devices}:{first.state}`
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations
            .set_power(PowerSnapshot {
                available: true,
                on_battery: true,
                display: Some(power_device(
                    "/org/freedesktop/UPower/devices/DisplayDevice",
                )),
                devices: vec![power_device("/org/freedesktop/UPower/devices/battery_BAT0")],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(configurations.refresh(), vec![1]);
        assert_eq!(text(&configurations, 1), "true:true:42:1:discharging");

        configurations.add(2, output("DP-2", 2560)).unwrap();
        assert_eq!(text(&configurations, 2), "true:true:42:1:discharging");
        assert!(
            configurations
                .set_power(PowerSnapshot {
                    on_battery: true,
                    ..PowerSnapshot::default()
                })
                .is_err()
        );
        assert!(configurations.refresh().is_empty());
        for key in [1, 2] {
            assert_eq!(text(&configurations, key), "true:true:42:1:discharging");
        }
    }

    #[test]
    fn system_tray_updates_reach_existing_and_later_outputs_together() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            return ui.panel { children = { ui.text { text = function()
                local items = services.system_tray.items:get()
                local id = if #items == 0 then '' else items[1].id
                local protocol_id = if #items == 0 then '' else items[1].protocol_item_id
                local title = if #items == 0 then '' else items[1].title
                local menu_label = if #items == 0 then '' else items[1].menu.children[1].label
                return `{services.system_tray.available:get()}:{services.system_tray.watcher_owned:get()}:{services.system_tray.error:get()}:{#items}:{id}:{protocol_id}:{title}:{menu_label}`
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();

        let item = system_tray_item("tray42");
        let menu = item.menu.as_ref().unwrap().clone();
        configurations
            .set_system_tray(SystemTraySnapshot {
                available: true,
                watcher_owned: true,
                error: String::new(),
                items: vec![item],
            })
            .unwrap();
        assert_eq!(Arc::strong_count(&menu), 4);
        assert_eq!(configurations.refresh(), vec![1, 2]);
        for key in [1, 2] {
            assert_eq!(
                text(&configurations, key),
                "true:true::1:tray42:mail:Mail:Compose"
            );
        }

        configurations.add(3, output("DP-3", 3840)).unwrap();
        assert_eq!(Arc::strong_count(&menu), 5);
        assert_eq!(
            text(&configurations, 3),
            "true:true::1:tray42:mail:Mail:Compose"
        );
        configurations.reload().unwrap();
        assert_eq!(Arc::strong_count(&menu), 5);
        for key in [1, 2, 3] {
            assert_eq!(
                text(&configurations, key),
                "true:true::1:tray42:mail:Mail:Compose"
            );
        }
    }

    fn notification() -> Notification {
        Notification {
            key: "1".into(),
            id: 7,
            revision: 1,
            app_name: "Test".into(),
            app_icon: "test".into(),
            summary: "Saved".into(),
            body: "The file was saved".into(),
            actions: vec![NotificationButton {
                key: "open".into(),
                label: "Open".into(),
            }],
            urgency: NotificationUrgency::Normal,
            raw_expire_timeout_ms: 0,
            effective_timeout_ms: None,
            expiry_paused: false,
            paused_remaining_ms: None,
            time_origin: std::time::Instant::now(),
            created_at_ms: 10,
            updated_at_ms: 10,
            expires_at_ms: None,
            category: String::new(),
            desktop_entry: "test".into(),
            image_path: String::new(),
            resident: false,
            transient: false,
            action_icons: false,
            can_reply: false,
            reply_placeholder: String::new(),
            image: None,
            closed: None,
        }
    }

    #[test]
    fn notification_updates_reach_existing_and_later_outputs_together() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            return ui.panel { children = { ui.text { text = function()
                local active = services.notifications.active:get()
                if #active == 0 then
                    return `{services.notifications.owned:get()}:{services.notifications.error:get()}:0`
                end
                return `{services.notifications.owned:get()}:{services.notifications.error:get()}:{#active}:{active[1].key}:{active[1].summary}:{active[1].actions[1].label}`
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let notification = notification();
        configurations
            .set_notifications(NotificationSnapshot {
                history: Vec::new(),
                owned: true,
                error: String::new(),
                active: vec![notification.clone()],
            })
            .unwrap();
        assert_eq!(configurations.refresh(), vec![1]);
        assert_eq!(text(&configurations, 1), "true::1:1:Saved:Open");

        configurations.add(2, output("DP-2", 2560)).unwrap();
        assert_eq!(text(&configurations, 2), "true::1:1:Saved:Open");
        assert!(
            configurations
                .set_notifications(NotificationSnapshot {
                    history: Vec::new(),
                    owned: false,
                    error: "disconnected".into(),
                    active: vec![notification],
                })
                .is_err()
        );
        assert!(configurations.refresh().is_empty());
        assert_eq!(text(&configurations, 1), "true::1:1:Saved:Open");
        assert_eq!(text(&configurations, 2), "true::1:1:Saved:Open");
    }

    #[test]
    fn expiry_holds_follow_output_and_reload_lifetimes() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        let source = r#"
            return ui.panel { children = { ui.button {
                on_click = function() services.notifications.set_expiry_paused('1', true) end,
                on_secondary_click = function() services.notifications.set_expiry_paused('1', false) end,
            } } }
        "#;
        fs::write(&entry, source).unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        configurations
            .set_notifications(NotificationSnapshot {
                owned: true,
                active: vec![notification()],
                ..NotificationSnapshot::default()
            })
            .unwrap();
        for output in [1, 2] {
            let id = configurations.root(&output).unwrap().children[0].id;
            configurations.dispatch(&output, id, Event::Click).unwrap();
        }
        assert_eq!(
            configurations.notification_expiry_holds(),
            BTreeSet::from([1])
        );
        let id = configurations.root(&1).unwrap().children[0].id;
        configurations
            .dispatch(&1, id, Event::SecondaryClick)
            .unwrap();
        assert_eq!(
            configurations.notification_expiry_holds(),
            BTreeSet::from([1])
        );
        configurations.remove(&2).unwrap();
        assert!(configurations.notification_expiry_holds().is_empty());
        configurations.add(2, output("DP-2", 2560)).unwrap();
        assert!(configurations.notification_expiry_holds().is_empty());
        configurations.dispatch(&1, id, Event::Click).unwrap();
        fs::write(&entry, "error('broken configuration')").unwrap();
        assert!(configurations.reload().is_err());
        assert_eq!(
            configurations.notification_expiry_holds(),
            BTreeSet::from([1])
        );
        fs::write(&entry, source).unwrap();
        assert!(
            configurations
                .prepare_reload_for_checked(
                    vec![(1, output("DP-1", 1920)), (2, output("DP-2", 2560))],
                    |_, _| Err("rejected candidate".into()),
                )
                .is_err()
        );
        assert_eq!(
            configurations.notification_expiry_holds(),
            BTreeSet::from([1])
        );
        configurations.reload().unwrap();
        assert!(configurations.notification_expiry_holds().is_empty());
        for output in [1, 2] {
            let id = configurations.root(&output).unwrap().children[0].id;
            configurations.dispatch(&output, id, Event::Click).unwrap();
        }
        configurations.remove(&1).unwrap();
        assert_eq!(
            configurations.notification_expiry_holds(),
            BTreeSet::from([1])
        );
        configurations
            .set_notifications(NotificationSnapshot::default())
            .unwrap();
        assert!(configurations.notification_expiry_holds().is_empty());
    }

    #[test]
    fn clipboard_updates_reach_existing_and_later_outputs_together() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            return ui.panel { children = { ui.text { text = function()
                return `{services.clipboard.available:get()}:{services.clipboard.loading:get()}:{services.clipboard.text:get() or 'none'}:{services.clipboard.error:get()}`
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations
            .set_clipboard(ClipboardSnapshot {
                available: true,
                text: Some("copied".into()),
                ..ClipboardSnapshot::default()
            })
            .unwrap();
        assert_eq!(configurations.refresh(), vec![1]);
        assert_eq!(text(&configurations, 1), "true:false:copied:");

        configurations.add(2, output("DP-2", 2560)).unwrap();
        assert_eq!(text(&configurations, 2), "true:false:copied:");
        assert!(
            configurations
                .set_clipboard(ClipboardSnapshot {
                    available: false,
                    text: Some("stale".into()),
                    ..ClipboardSnapshot::default()
                })
                .is_err()
        );
        assert!(configurations.refresh().is_empty());
        for key in [1, 2] {
            assert_eq!(text(&configurations, key), "true:false:copied:");
        }
    }

    #[test]
    fn managed_process_state_is_shared_and_output_lifetimes_reconcile() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local key = if services.output.name:get() == 'DP-1' then 'shared' else 'second'
            local process = services.process(key)
            return ui.panel { children = { ui.text { text = function()
                return key .. ':' .. process.status:get() .. ':' .. process.stdout:get()
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        let mut first_definitions = Vec::new();
        configurations
            .add_checked_with_processes(
                1,
                output("DP-1", 1920),
                |_| Ok(()),
                |definitions| {
                    first_definitions = definitions.to_vec();
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            first_definitions
                .iter()
                .map(|definition| definition.key.as_str())
                .collect::<Vec<_>>(),
            ["shared"]
        );
        configurations
            .set_managed_processes(ManagedProcessSnapshot {
                states: vec![managed_process_state(
                    "shared",
                    thing_runtime::ManagedProcessStatus::Running,
                    "ready",
                )],
            })
            .unwrap();
        assert_eq!(configurations.refresh(), vec![1]);

        let mut added_definitions = Vec::new();
        configurations
            .add_checked_with_processes(
                2,
                output("DP-2", 2560),
                |_| Ok(()),
                |definitions| {
                    added_definitions = definitions.to_vec();
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            added_definitions
                .iter()
                .map(|definition| definition.key.as_str())
                .collect::<Vec<_>>(),
            ["second", "shared"]
        );
        assert_eq!(text(&configurations, 1), "shared:running:ready");
        assert_eq!(text(&configurations, 2), "second:idle:");

        let mut remaining_definitions = Vec::new();
        let removed = configurations
            .remove_with_processes(&2, |definitions| {
                remaining_definitions = definitions.to_vec();
                Ok(())
            })
            .unwrap();
        assert!(removed.process_reconciliation.is_ok());
        assert_eq!(
            remaining_definitions
                .iter()
                .map(|definition| definition.key.as_str())
                .collect::<Vec<_>>(),
            ["shared"]
        );
        assert_eq!(text(&configurations, 1), "shared:running:ready");
    }

    #[test]
    fn scheduled_reload_projects_completed_results_into_every_candidate() {
        fn runtime_text(runtime: &Runtime) -> &str {
            fn find(node: &Node) -> Option<&str> {
                if node.kind == Kind::Text {
                    return Some(&node.text);
                }
                node.children.iter().find_map(find)
            }
            find(runtime.root().unwrap()).unwrap()
        }

        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local process = services.process {
                key = 'shared',
                command = { 'query' },
                interval = 3000,
            }
            return ui.panel { children = { ui.text { text = function()
                return process.status:get() .. ':' .. process.stdout:get()
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();
        let completed = ManagedProcessSnapshot {
            states: vec![managed_process_state(
                "shared",
                thing_runtime::ManagedProcessStatus::Exited,
                "complete",
            )],
        };
        configurations
            .set_managed_processes(completed.clone())
            .unwrap();
        assert_eq!(configurations.refresh(), vec![1, 2]);

        let mut replacement = configurations
            .prepare_reload_for_checked(
                vec![(1, output("DP-1", 1920)), (2, output("DP-2", 2560))],
                |_, _| Ok(()),
            )
            .unwrap();
        assert_eq!(replacement.managed_process_definitions().len(), 1);
        assert!(
            replacement.managed_process_definitions()[0]
                .command
                .is_some()
        );
        let mut validated = Vec::new();
        replacement
            .set_managed_processes(completed, |key, runtime| {
                validated.push(*key);
                if runtime_text(runtime) != "exited:complete" {
                    return Err("scheduled result was not projected".into());
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(validated, vec![1, 2]);

        configurations.commit_reload(replacement);
        assert_eq!(text(&configurations, 1), "exited:complete");
        assert_eq!(text(&configurations, 2), "exited:complete");
    }

    #[test]
    fn failed_process_reconciliation_cannot_retain_a_removed_output() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local key = if services.output.name:get() == 'DP-1' then 'shared' else 'retired'
            local process = services.process(key)
            return ui.panel { children = { ui.text { text = function()
                return key .. ':' .. process.status:get() .. ':' .. process.stdout:get()
            end } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        configurations
            .add_checked_with_processes(1, output("DP-1", 1920), |_| Ok(()), |_| Ok(()))
            .unwrap();
        configurations
            .add_checked_with_processes(2, output("DP-2", 2560), |_| Ok(()), |_| Ok(()))
            .unwrap();
        configurations
            .set_managed_processes(ManagedProcessSnapshot {
                states: vec![
                    managed_process_state(
                        "retired",
                        thing_runtime::ManagedProcessStatus::Running,
                        "old",
                    ),
                    managed_process_state(
                        "shared",
                        thing_runtime::ManagedProcessStatus::Running,
                        "ready",
                    ),
                ],
            })
            .unwrap();
        assert_eq!(configurations.refresh(), vec![1, 2]);
        let retired_consumer = configurations.consumer(&2).unwrap().to_owned();
        let mut remaining_definitions = Vec::new();

        let removed = configurations
            .remove_with_processes(&2, |definitions| {
                remaining_definitions = definitions.to_vec();
                Err("managed process worker is shutting down".into())
            })
            .unwrap();

        assert_eq!(removed.consumer, retired_consumer);
        assert_eq!(
            removed.process_reconciliation,
            Err("managed process worker is shutting down".into())
        );
        assert!(!configurations.contains(&2));
        assert_eq!(configurations.consumer(&2), None);
        assert_eq!(text(&configurations, 1), "shared:running:ready");
        assert_eq!(
            remaining_definitions
                .iter()
                .map(|definition| definition.key.as_str())
                .collect::<Vec<_>>(),
            ["shared"]
        );
        assert_eq!(
            configurations
                .services
                .managed_processes()
                .states
                .iter()
                .map(|state| state.key.as_str())
                .collect::<Vec<_>>(),
            ["shared"]
        );
    }

    #[test]
    fn failed_process_scope_preflight_preserves_the_loaded_generation() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            "local process = services.process('old'); return ui.panel { children = { ui.text { text = 'old:' .. process.status:get() } } }",
        )
        .unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let old_consumer = configurations.consumer(&1).unwrap().to_owned();
        fs::write(
            &entry,
            "local process = services.process('new'); return ui.panel { children = { ui.text { text = 'new:' .. process.status:get() } } }",
        )
        .unwrap();
        let mut observed = Vec::new();

        let error = configurations
            .reload_for_checked_with_processes(
                vec![(1, output("DP-1", 1920))],
                |_, _| Ok(()),
                |definitions| {
                    observed = definitions.to_vec();
                    Err("managed process service requires unrestricted procfs".into())
                },
            )
            .unwrap_err();

        assert!(error.contains("unrestricted procfs"));
        assert_eq!(observed[0].key, "new");
        assert_eq!(configurations.consumer(&1), Some(old_consumer.as_str()));
        assert_eq!(text(&configurations, 1), "old:idle");
    }

    #[test]
    fn conflicting_output_process_declarations_fail_before_scope_change() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            r#"
            local signal = if services.output.name:get() == 'DP-1' then 'terminate' else 'interrupt'
            services.process { key = 'shared', shutdown_signal = signal }
            return ui.panel {}
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry, ServiceSnapshot::default());
        let mut scope_changed = false;

        let error = configurations
            .reload_for_checked_with_processes(
                vec![(1, output("DP-1", 1920)), (2, output("DP-2", 2560))],
                |_, _| Ok(()),
                |_| {
                    scope_changed = true;
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(error.contains("conflicting declarations across outputs"));
        assert!(!scope_changed);
        assert!(configurations.keys().next().is_none());
    }

    #[test]
    fn conflicting_sleep_inhibitor_declarations_fail_before_any_change() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        fs::write(
            &entry,
            "services.sleep_inhibitor { key = 'shared', reason = 'same' }\nreturn ui.panel {}",
        )
        .unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        let shared = vec![SleepInhibitorDefinition {
            key: "shared".into(),
            reason: "same".into(),
        }];
        assert_eq!(configurations.sleep_inhibitor_definitions(), shared);

        // a second output may share the key only with the same reason.
        fs::write(
            &entry,
            r#"
            local reason = if services.output.name:get() == 'DP-1' then 'same' else 'other'
            services.sleep_inhibitor { key = 'shared', reason = reason }
            return ui.panel {}
            "#,
        )
        .unwrap();
        let error = match configurations.prepare_add_checked(2, output("DP-2", 2560), |_| Ok(())) {
            Err(error) => error,
            Ok(_) => panic!("conflicting declarations were accepted"),
        };
        assert!(
            error.contains("conflicting declarations across outputs"),
            "{error}"
        );
        let error = configurations
            .prepare_reload_for_checked(
                vec![(1, output("DP-1", 1920)), (2, output("DP-2", 2560))],
                |_, _| Ok(()),
            )
            .err()
            .unwrap();
        assert!(
            error.contains("conflicting declarations across outputs"),
            "{error}"
        );
        assert_eq!(configurations.keys().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(configurations.sleep_inhibitor_definitions(), shared);
    }

    #[test]
    fn watched_files_are_the_union_of_output_specific_imports() {
        let directory = tempdir().unwrap();
        let entry = directory.path().join("panel.luau");
        let first = directory.path().join("first.luau");
        let second = directory.path().join("second.luau");
        fs::write(&first, "return 'first'").unwrap();
        fs::write(&second, "return 'second'").unwrap();
        fs::write(
            &entry,
            r#"
            local value = if services.output.name:get() == 'DP-1'
                then require('./first') else require('./second')
            return ui.panel { children = { ui.text { text = value } } }
            "#,
        )
        .unwrap();
        let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
        configurations.add(1, output("DP-1", 1920)).unwrap();
        configurations.add(2, output("DP-2", 2560)).unwrap();

        assert_eq!(
            configurations.files().into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([entry, first, second])
        );
    }
}
