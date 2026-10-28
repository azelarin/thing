use std::{collections::BTreeSet, path::PathBuf};

use thing_core::{Node, NodeId};
use thing_runtime::{
    Action, AudioSnapshot, Event, NiriState, OutputContext, Runtime, Scalar, ServiceSnapshot,
};

struct Instance<K> {
    key: K,
    output: OutputContext,
    runtime: Runtime,
}

/// loaded configurations and desktop state shared by every output.
pub(crate) struct Configurations<K> {
    entry: PathBuf,
    services: ServiceSnapshot,
    instances: Vec<Instance<K>>,
}

impl<K: Clone + Eq> Configurations<K> {
    pub(crate) fn new(entry: PathBuf, services: ServiceSnapshot) -> Self {
        Self {
            entry,
            services,
            instances: Vec::new(),
        }
    }

    pub(crate) fn contains(&self, key: &K) -> bool {
        self.instances.iter().any(|instance| &instance.key == key)
    }

    pub(crate) fn entry(&self) -> &PathBuf {
        &self.entry
    }

    pub(crate) fn add(&mut self, key: K, output: OutputContext) -> Result<(), String> {
        if self.contains(&key) {
            return Ok(());
        }
        let runtime = Runtime::load_for_output(&self.entry, output.clone(), &self.services)
            .map_err(|error| error.to_string())?;
        self.instances.push(Instance {
            key,
            output,
            runtime,
        });
        Ok(())
    }

    pub(crate) fn remove(&mut self, key: &K) {
        self.instances.retain(|instance| &instance.key != key);
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

    pub(crate) fn output(&self, key: &K) -> Option<&OutputContext> {
        self.instances
            .iter()
            .find(|instance| &instance.key == key)
            .map(|instance| &instance.output)
    }

    fn runtime_mut(&mut self, key: &K) -> Option<&mut Runtime> {
        self.instances
            .iter_mut()
            .find(|instance| &instance.key == key)
            .map(|instance| &mut instance.runtime)
    }

    pub(crate) fn set_output(&mut self, key: &K, output: OutputContext) -> Result<bool, String> {
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
            .refresh()
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

    pub(crate) fn refresh(&mut self) -> Vec<K> {
        self.instances
            .iter_mut()
            .filter_map(|instance| match instance.runtime.refresh() {
                Ok(true) => Some(instance.key.clone()),
                Ok(false) => None,
                Err(error) => {
                    eprintln!("thing: {error}");
                    None
                }
            })
            .collect()
    }

    pub(crate) fn dispatch(
        &mut self,
        key: &K,
        id: NodeId,
        event: Event,
    ) -> Result<(bool, Vec<Action>), String> {
        let Some(runtime) = self.runtime_mut(key) else {
            return Ok((false, Vec::new()));
        };
        let changed = runtime
            .dispatch(id, event)
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

    pub(crate) fn reload_for(&mut self, outputs: Vec<(K, OutputContext)>) -> Result<(), String> {
        let mut candidates = Vec::with_capacity(outputs.len());
        for (key, output) in outputs {
            if candidates
                .iter()
                .any(|instance: &Instance<K>| instance.key == key)
            {
                return Err("duplicate output while reloading configuration".into());
            }
            let runtime = Runtime::load_for_output(&self.entry, output.clone(), &self.services)
                .map_err(|error| error.to_string())?;
            candidates.push(Instance {
                key,
                output,
                runtime,
            });
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

    pub(crate) fn root(&self, key: &K) -> Option<&Node> {
        self.runtime(key).map(Runtime::root)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;
    use thing_core::{Kind, Scale};
    use thing_runtime::{AudioDevice, AudioSource, AudioStream};

    use super::*;

    fn output(name: &str, width: u32) -> OutputContext {
        OutputContext {
            name: Some(name.into()),
            width,
            height: 1080,
            scale: Scale::default(),
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
        assert_eq!(text(&configurations, 1), "DP-1:1920:0");
        assert_eq!(text(&configurations, 2), "DP-2:2560:0");

        let button = configurations.root(&1).unwrap().children[0].id;
        assert!(configurations.dispatch(&1, button, Event::Click).unwrap().0);
        assert_eq!(text(&configurations, 1), "DP-1:1920:1");
        assert_eq!(text(&configurations, 2), "DP-2:2560:0");
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
        fs::write(
            &entry,
            "if services.output.name:get() == 'DP-2' then error('bad output') end\nreturn ui.panel { children = { ui.text { text = 'new' } } }",
        )
        .unwrap();

        assert!(configurations.reload().is_err());
        assert_eq!(text(&configurations, 1), "DP-1");
        assert_eq!(text(&configurations, 2), "DP-2");
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
        fs::write(
            &entry,
            "return ui.panel { children = { ui.text { text = 'new:' .. services.output.name:get() } } }",
        )
        .unwrap();

        configurations.reload().unwrap();
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
        configurations.remove(&1);
        configurations
            .set_service("clock.time", Scalar::String("12:35".into()))
            .unwrap();

        configurations.add(1, output("DP-1", 1920)).unwrap();
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
