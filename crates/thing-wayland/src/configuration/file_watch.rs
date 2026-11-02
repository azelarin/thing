use thing_runtime::{FileWatchSnapshot, Runtime};

use super::{Configurations, DispatchOutcome, SurfaceTree};
use crate::file_watch::Subscription;

pub(super) fn validate<'a>(runtimes: impl IntoIterator<Item = &'a Runtime>) -> Result<(), String> {
    let definitions: Vec<_> = runtimes
        .into_iter()
        .flat_map(Runtime::file_watches)
        .collect();
    crate::file_watch::validate(definitions.iter())
}

impl<K: Clone + Eq> Configurations<K> {
    pub(crate) fn file_watches(&self) -> Vec<Subscription<K>> {
        self.instances
            .iter()
            .flat_map(|instance| {
                instance
                    .runtime
                    .file_watches()
                    .into_iter()
                    .map(|definition| Subscription {
                        output: instance.key.clone(),
                        consumer: instance.consumer.clone(),
                        definition,
                    })
            })
            .collect()
    }

    pub(crate) fn dispatch_file_watch_checked(
        &mut self,
        output: &K,
        consumer: &str,
        key: &str,
        snapshot: &FileWatchSnapshot,
        validate: impl Fn(&[SurfaceTree]) -> Result<(), String>,
    ) -> Result<Option<DispatchOutcome>, String> {
        let Some(instance) = self
            .instances
            .iter_mut()
            .find(|instance| &instance.key == output && instance.consumer == consumer)
        else {
            return Ok(None);
        };
        let runtime = &mut instance.runtime;
        let Some(changed) = runtime
            .dispatch_file_watch_checked(key, snapshot, validate)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::Arc};
    use thing_core::Scale;
    use thing_runtime::{OutputContext, ServiceSnapshot};

    fn output(name: &str) -> OutputContext {
        OutputContext {
            name: Some(name.into()),
            width: 640,
            height: 480,
            scale: Scale::default(),
        }
    }
    #[test]
    fn rejected_reload_keeps_subscriptions_and_replaced_consumers_ignore_late_events() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        let source = "local text=ui.state('waiting')\nservices.watch_file {key='data',path='./data',on_change=function(event) text:set(event.text or event.status) end}\nreturn ui.panel {children={ui.text{text=function() return text:get() end}}}";
        fs::write(&path, source).unwrap();
        let mut configurations = Configurations::new(path.clone(), ServiceSnapshot::default());
        configurations.add(1u32, output("one")).unwrap();
        configurations.add(2, output("two")).unwrap();
        let before = configurations.file_watches();
        fs::write(&path, "return ui.panel { style={width=-1} }").unwrap();
        assert!(configurations.reload().is_err());
        assert_eq!(configurations.file_watches(), before);
        let first = before.iter().find(|item| item.output == 1).unwrap();
        configurations
            .dispatch_file_watch_checked(
                &1,
                &first.consumer,
                "data",
                &FileWatchSnapshot::Ready(Arc::from("first")),
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(configurations.root(&1).unwrap().children[0].text, "first");
        fs::write(&path, source).unwrap();
        configurations.reload().unwrap();
        assert!(
            configurations
                .dispatch_file_watch_checked(
                    &1,
                    &first.consumer,
                    "data",
                    &FileWatchSnapshot::Ready(Arc::from("stale")),
                    |_| Ok(())
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(configurations.root(&1).unwrap().children[0].text, "waiting");
    }

    #[test]
    fn path_limits_reject_a_candidate_before_output_ownership_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("panel.luau");
        fs::write(&path,"for i=1,17 do services.watch_file {key=tostring(i),path='./' .. services.output.name:get() .. i,on_change=function() end} end\nreturn ui.panel {}").unwrap();
        let mut configurations = Configurations::new(path, ServiceSnapshot::default());
        configurations.add(1u32, output("one")).unwrap();
        let before = configurations.file_watches();
        assert!(configurations.add(2, output("two")).is_err());
        assert_eq!(configurations.file_watches(), before);
        assert!(!configurations.contains(&2));
    }
}
