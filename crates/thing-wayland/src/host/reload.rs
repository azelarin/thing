use super::*;
use crate::source::SourceWorker;
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll, Wake, Waker},
};
use thing_runtime::{Runtime, RuntimeLoad, SourceReader};

type Output = wl_output::WlOutput;

struct PingWake(calloop::ping::Ping);
impl Wake for PingWake {
    fn wake(self: Arc<Self>) {
        self.0.ping();
    }
}

struct Loaded {
    sources_current: bool,
    runtimes: Vec<(Output, OutputContext, Runtime)>,
}

struct Pending {
    services: ServiceSnapshot,
    outputs: Vec<(Output, OutputContext)>,
    reader: Rc<dyn SourceReader>,
    future: Pin<Box<dyn Future<Output = Result<Loaded, String>>>>,
}

impl Pending {
    fn new(
        entry: PathBuf,
        outputs: Vec<(Output, OutputContext)>,
        services: ServiceSnapshot,
        reader: Rc<dyn SourceReader>,
    ) -> Self {
        let source = reader.clone();
        let baseline_services = services.clone();
        let baseline_outputs = outputs.clone();
        let future = Box::pin(async move {
            let mut initial = services.clone();
            initial
                .set_managed_processes(ManagedProcessSnapshot::default())
                .map_err(|error| error.to_string())?;
            let mut runtimes = Vec::with_capacity(outputs.len());
            for (output, context) in &outputs {
                let runtime = RuntimeLoad::for_output(
                    entry.clone(),
                    context.clone(),
                    initial.clone(),
                    source.clone(),
                )
                .map_err(|error| error.to_string())?
                .await
                .map_err(|error| error.to_string())?;
                runtimes.push((output.clone(), context.clone(), runtime));
                // start each output on a separate turn even if its files are cached.
                let mut yielded = false;
                std::future::poll_fn(|context| {
                    if std::mem::replace(&mut yielded, true) {
                        Poll::Ready(())
                    } else {
                        context.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
            }
            let sources_current = source.validate().await?;
            Ok(Loaded {
                sources_current,
                runtimes,
            })
        });
        Self {
            services: baseline_services,
            outputs: baseline_outputs,
            reader,
            future,
        }
    }
}

pub(super) struct Reloads {
    ping: calloop::ping::Ping,
    worker: Option<Rc<SourceWorker>>,
    pending: Option<Pending>,
}

impl Reloads {
    pub(super) fn new(ping: calloop::ping::Ping) -> Self {
        Self {
            ping,
            worker: None,
            pending: None,
        }
    }

    #[cfg(test)]
    pub(super) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn reader(&mut self) -> Result<Rc<dyn SourceReader>, String> {
        if self.worker.is_none() {
            self.worker = Some(Rc::new(SourceWorker::new()?));
        }
        Ok(self.worker.as_ref().unwrap().session())
    }
}

impl App {
    fn reload_outputs(&self) -> Vec<(Output, OutputContext)> {
        self.outputs
            .outputs()
            .filter_map(|output| {
                self.output_context(&output)
                    .or_else(|| self.configurations.output(&output).cloned())
                    .map(|context| (output, context))
            })
            .collect()
    }

    pub(super) fn schedule_reload(&mut self) {
        if let Some(reloads) = &mut self.reloads {
            reloads.pending = None;
        }
        self.reload_at = Some(Instant::now() + Duration::from_millis(80));
    }

    pub(super) fn begin_reload(&mut self) -> Result<(), String> {
        let Some(reloads) = &mut self.reloads else {
            return Ok(());
        };
        let reader = reloads.reader()?;
        self.begin_reload_with(reader);
        Ok(())
    }

    pub(super) fn begin_reload_with(&mut self, reader: Rc<dyn SourceReader>) {
        let pending = Pending::new(
            self.configurations.entry().clone(),
            self.reload_outputs(),
            self.configurations.service_snapshot().clone(),
            reader,
        );
        self.reloads
            .as_mut()
            .expect("reload support must be installed")
            .pending = Some(pending);
    }

    pub(super) fn advance_reload(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let Some(reloads) = &mut self.reloads else {
            return Ok(());
        };
        let Some(mut pending) = reloads.pending.take() else {
            return Ok(());
        };
        let waker = Waker::from(Arc::new(PingWake(reloads.ping.clone())));
        match pending
            .future
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
        {
            Poll::Pending => {
                reloads.pending = Some(pending);
            }
            Poll::Ready(result) => {
                if pending.services != *self.configurations.service_snapshot()
                    || pending.outputs != self.reload_outputs()
                {
                    // reuse immutable sources, but rebuild against changes made
                    // while loading instead of overwriting current desktop state.
                    self.begin_reload_with(pending.reader);
                } else {
                    match result {
                        Ok(loaded) if loaded.sources_current => {
                            self.publish_reload(qh, Some(loaded))?
                        }
                        Ok(_) => {
                            if let Err(error) = self.begin_reload() {
                                eprintln!(
                                    "thing: reload failed; keeping the previous surfaces: {error}"
                                );
                            }
                        }
                        Err(error) => eprintln!(
                            "thing: reload failed; keeping the previous surfaces: {error}"
                        ),
                    }
                }
            }
        }

        Ok(())
    }

    #[cfg(test)]
    pub(super) fn reload(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.publish_reload(qh, None)
    }

    fn publish_reload(
        &mut self,
        qh: &QueueHandle<Self>,
        loaded: Option<Loaded>,
    ) -> Result<(), String> {
        let outputs: Vec<_> = self
            .outputs
            .outputs()
            .filter_map(|output| {
                self.output_context(&output)
                    .or_else(|| self.configurations.output(&output).cloned())
                    .map(|context| (output, context))
            })
            .collect();
        let layer_shell_version = self.layer_shell_version;
        let entries = outputs
            .iter()
            .map(|(output, context)| {
                self.replacement_layout_validation_entry(output.clone(), context.clone())
            })
            .collect();
        let retired_consumers = self.configurations.consumers();
        let loop_handle = self.loop_handle.clone();
        let replacement = (|| {
            let preflight = LayoutPreflight::new(
                &mut self.render,
                layer_shell_version,
                self.idle_inhibit.active().is_some(),
                entries,
            );
            let mut configurations = match loaded {
                Some(loaded) => self
                    .configurations
                    .prepare_loaded_reload_checked(loaded.runtimes, |output, runtime| {
                        preflight.validate(output, runtime.surfaces())
                    })?,
                None => self
                    .configurations
                    .prepare_reload_for_checked(outputs, |output, runtime| {
                        preflight.validate(output, runtime.surfaces())
                    })?,
            };
            let process_definitions = configurations.managed_process_definitions().to_vec();
            let projected_processes = project_managed_processes(
                self.managed_processes.as_ref(),
                &process_definitions,
                ManagedProcessScopeChange::Replace,
            )?;
            configurations
                .set_managed_processes(projected_processes.clone(), |output, runtime| {
                    preflight.validate(output, runtime.surfaces())
                })?;
            let activation = prepare_and_commit_managed_processes(
                &mut self.managed_processes,
                &loop_handle,
                &process_definitions,
                ManagedProcessScopeChange::Replace,
                projected_processes,
                |snapshot| {
                    configurations.set_managed_processes(snapshot, |output, runtime| {
                        preflight.validate(output, runtime.surfaces())
                    })
                },
            )?;
            let prepared = preflight.into_prepared();
            Ok::<_, String>((configurations, activation, prepared))
        })();
        let (configurations, activation, prepared) = match replacement {
            Ok(replacement) => replacement,
            Err(error) => {
                eprintln!("thing: reload failed; keeping the previous surfaces: {error}");
                return Ok(());
            }
        };
        self.configurations.commit_reload(configurations);
        // a successful reload retires every widget identity; a rejected one
        // returned above and keeps the highlight.
        self.clear_highlight();
        self.reconcile_notification_expiry();
        self.cancel_pending_text_paste();
        // the replacement runtimes never inherit an in-flight pointer sequence.
        if let Some(capture) = self.pointer_capture.take() {
            self.clear_pressed_for_capture(&capture);
        }
        for consumer in retired_consumers {
            if let Err(error) = release_system_tray_consumer(self.system_tray.as_ref(), &consumer) {
                eprintln!("thing: {error}");
            }
        }
        // release the previous configuration's glyph atlas and text layouts.
        let wake = self.image_ping.clone();
        self.render.reset_painter(move || wake.ping())?;
        self.hover.clear();
        self.clear_key_repeat();
        self.pending_ui_actions.clear();
        while let Some(index) = self
            .surfaces
            .iter()
            .rposition(|surface| matches!(surface.role, Role::Popup(_)))
        {
            self.remove_popup(index, false, false);
        }
        self.closed_surfaces.clear();
        let removed: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| {
                surface.role.is_root() && !self.configurations.contains(&surface.output)
            })
            .map(Surface::owner)
            .collect();
        for owner in removed {
            self.remove_root(&owner, false);
        }
        let outputs: Vec<_> = self.configurations.keys().cloned().collect();
        for output in &outputs {
            self.sync_output_surfaces(output, true, qh)?;
        }
        self.install_prepared_layouts(prepared);
        arm_managed_processes(self.managed_processes.as_ref(), activation);
        self.reconcile_sleep();
        self.reconcile_idle_watches();
        self.reconcile_file_watches();
        if self
            .keyboard_surface
            .as_ref()
            .is_some_and(|wl| self.index(wl).is_none())
        {
            self.clear_keyboard_focus();
        }
        self.apply_pending_ui_actions(MissingSurfacePolicy::Expire)?;
        self.update_watch();
        self.reconcile_input_method(InputMethodChangeCause::Other);
        eprintln!("thing: reloaded {}", self.configurations.entry().display());
        Ok(())
    }
}
