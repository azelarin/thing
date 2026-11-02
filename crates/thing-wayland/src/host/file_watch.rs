use super::*;

impl App {
    pub(super) fn reconcile_file_watches(&mut self) {
        let desired = self.configurations.file_watches();
        if let Some(watches) = &mut self.file_watches
            && let Err(error) = watches.reconcile(desired)
        {
            eprintln!("thing: cannot reconcile file watches: {error}");
        }
    }

    pub(super) fn update_file_watches(&mut self) {
        let deliveries = self
            .file_watches
            .as_mut()
            .map_or_else(Vec::new, |watches| watches.take_deliveries());
        for delivery in deliveries {
            let output = delivery.output;
            if self.configurations.consumer(&output) != Some(delivery.consumer.as_str()) {
                continue;
            }
            let Some(entry) = self.current_layout_validation_entry(&output) else {
                continue;
            };
            let preflight = LayoutPreflight::new(
                &mut self.render,
                self.layer_shell_version,
                self.idle_inhibit.active().is_some(),
                vec![entry],
            );
            let result = self.configurations.dispatch_file_watch_checked(
                &output,
                &delivery.consumer,
                &delivery.key,
                &delivery.snapshot,
                |surfaces| preflight.validate(&output, surfaces),
            );
            let prepared = preflight.into_prepared();
            match result {
                Ok(Some(outcome)) => {
                    self.finish_dispatch(
                        output,
                        Some(delivery.consumer),
                        Ok(outcome),
                        None,
                        prepared,
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    self.finish_dispatch(
                        output,
                        Some(delivery.consumer),
                        Err(error),
                        None,
                        prepared,
                    );
                }
            }
        }
    }
}
