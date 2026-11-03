use super::*;

impl<K: Clone + Eq> Configurations<K> {
    pub(crate) fn service_snapshot(&self) -> &ServiceSnapshot {
        &self.services
    }

    pub(crate) fn prepare_loaded_reload_checked(
        &self,
        runtimes: Vec<(K, OutputContext, Runtime)>,
        validate: impl Fn(&K, &Runtime) -> Result<(), String>,
    ) -> Result<PreparedConfigurations<K>, String> {
        let mut instances = Vec::with_capacity(runtimes.len());
        let mut next_consumer = self.next_consumer;
        for (key, output, runtime) in runtimes {
            if instances
                .iter()
                .any(|instance: &Instance<K>| instance.key == key)
            {
                return Err("duplicate output while reloading configuration".into());
            }
            validate(&key, &runtime)?;
            instances.push(Instance {
                key,
                output,
                runtime,
                consumer: allocate_consumer(&mut next_consumer)?,
            });
        }
        let mut services = self.services.clone();
        services
            .set_managed_processes(ManagedProcessSnapshot::default())
            .map_err(|error| error.to_string())?;
        self.finish_prepared_reload(services, instances, next_consumer)
    }

    pub(super) fn finish_prepared_reload(
        &self,
        mut services: ServiceSnapshot,
        instances: Vec<Instance<K>>,
        next_consumer: u64,
    ) -> Result<PreparedConfigurations<K>, String> {
        let definitions = collect_managed_process_definitions(
            instances.iter().map(|instance| &instance.runtime),
        )?;
        collect_sleep_inhibitor_definitions(instances.iter().map(|instance| &instance.runtime))?;
        file_watch::validate(instances.iter().map(|instance| &instance.runtime))?;
        let shared = project_replaced(
            self.services.shared_states(),
            instances.iter().map(|instance| &instance.runtime),
        )?;
        services.set_shared_states(shared);
        Ok(PreparedConfigurations {
            services,
            instances,
            next_consumer,
            process_definitions: definitions,
        })
    }
}
