use std::{collections::BTreeSet, path::PathBuf, time::Instant};

use thing_core::{Node, NodeId};
use thing_runtime::{
    Action, AudioSnapshot, ClipboardSnapshot, Event, MediaSnapshot, NiriState,
    NotificationSnapshot, OutputContext, PowerSnapshot, Runtime, Scalar, ServiceSnapshot,
    SurfaceKey, SurfaceTree, SystemTraySnapshot,
};
use thing_ui::Layout;

struct Instance<K> {
    key: K,
    consumer: String,
    output: OutputContext,
    runtime: Runtime,
}

/// loaded configurations and desktop state shared by every output.
pub(crate) struct Configurations<K> {
    entry: PathBuf,
    services: ServiceSnapshot,
    instances: Vec<Instance<K>>,
    next_consumer: u64,
}

impl<K: Clone + Eq> Configurations<K> {
    pub(crate) fn new(entry: PathBuf, services: ServiceSnapshot) -> Self {
        Self {
            entry,
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
        if self.contains(&key) {
            return Ok(());
        }
        let runtime = Runtime::load_for_output(&self.entry, output.clone(), &self.services)
            .map_err(|error| error.to_string())?;
        validate(&runtime)?;
        let consumer = allocate_consumer(&mut self.next_consumer)?;
        self.instances.push(Instance {
            key,
            consumer,
            output,
            runtime,
        });
        Ok(())
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<String> {
        let index = self
            .instances
            .iter()
            .position(|instance| &instance.key == key)?;
        Some(self.instances.remove(index).consumer)
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

    pub(crate) fn has_scroll_handler(&self, key: &K, id: NodeId) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.has_scroll_handler(id))
    }

    pub(crate) fn has_active_press(&self, key: &K, id: NodeId) -> bool {
        self.runtime(key)
            .is_some_and(|runtime| runtime.has_active_press(id))
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

    pub(crate) fn output(&self, key: &K) -> Option<&OutputContext> {
        self.instances
            .iter()
            .find(|instance| &instance.key == key)
            .map(|instance| &instance.output)
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
            return Ok(false);
        }
        instance
            .runtime
            .set_output(output.clone())
            .map_err(|error| error.to_string())?;
        instance.output = output;
        instance
            .runtime
            .refresh_checked(validate)
            .map_err(|error| error.to_string())
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
    ) -> Result<(bool, Vec<Action>), String> {
        self.dispatch_checked(key, id, event, |_| Ok(()))
    }

    pub(crate) fn dispatch_checked(
        &mut self,
        key: &K,
        id: NodeId,
        event: Event,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<(bool, Vec<Action>), String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok((false, Vec::new()));
        };
        let changed = runtime
            .dispatch_checked(id, event, validate)
            .map_err(|error| error.to_string())?;
        Ok((changed, runtime.take_actions()))
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
        let mut candidates = Vec::with_capacity(outputs.len());
        let mut next_consumer = self.next_consumer;
        for (key, output) in outputs {
            if candidates
                .iter()
                .any(|instance: &Instance<K>| instance.key == key)
            {
                return Err("duplicate output while reloading configuration".into());
            }
            let runtime = Runtime::load_for_output(&self.entry, output.clone(), &self.services)
                .map_err(|error| error.to_string())?;
            validate(&key, &runtime)?;
            let consumer = allocate_consumer(&mut next_consumer)?;
            candidates.push(Instance {
                key,
                consumer,
                output,
                runtime,
            });
        }
        self.next_consumer = next_consumer;
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

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, time::Duration};

    use tempfile::tempdir;
    use thing_core::{KeyboardMode, Kind, Scale};
    use thing_runtime::{
        AudioDevice, AudioSource, AudioStream, MediaPlayer, Notification, NotificationButton,
        NotificationUrgency, PowerDevice, SystemTrayItem, SystemTrayMenu, SystemTrayMenuItem,
        SystemTraySnapshot,
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

    fn text(configurations: &Configurations<u8>, key: u8) -> &str {
        fn find(node: &Node) -> Option<&str> {
            if node.kind == Kind::Text {
                return Some(&node.text);
            }
            node.children.iter().find_map(find)
        }
        find(configurations.root(&key).unwrap()).unwrap()
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
        assert!(configurations.dispatch(&1, button, Event::Click).unwrap().0);
        assert_eq!(text(&configurations, 1), "DP-1:1920:1");
        assert_eq!(text(&configurations, 2), "DP-2:2560:0");
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

        assert!(configurations.dispatch(&1, first, Event::Click).unwrap().0);
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
        let notification = Notification {
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
            created_at_ms: 10,
            updated_at_ms: 10,
            expires_at_ms: None,
            category: String::new(),
            desktop_entry: "test".into(),
            image_path: String::new(),
            resident: false,
            transient: false,
            action_icons: false,
        };
        configurations
            .set_notifications(NotificationSnapshot {
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
