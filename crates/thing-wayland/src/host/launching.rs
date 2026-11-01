use super::*;

impl App {
    pub(super) fn start_activation_fallbacks(&mut self) {
        for fallback in self.activator.take_fallbacks() {
            for (request, token) in fallback.into_launches() {
                self.spawn_launch(request, token.as_deref());
            }
        }
    }
    pub(super) fn request_launch(&mut self, request: PendingLaunch, origin: Option<&InputOrigin>) {
        let (Some(activation), Some(origin)) = (self.activation.clone(), origin) else {
            self.finish_launch(request, None);
            return;
        };
        let queue = self.queue.clone();
        let now = Instant::now();
        if let Err(request) = self.pending_launches.push(now, request, |id| {
            let token = activation.get_activation_token(&queue, id);
            token.set_serial(origin.serial, &origin.seat);
            token.set_surface(&origin.surface);
            token.commit();
            token
        }) {
            self.finish_launch(request, None);
        }
    }
    pub(super) fn finish_launch(&mut self, request: PendingLaunch, token: Option<&str>) {
        // the catalog may have been replaced while the token was requested. a
        // changed or removed entry no longer describes what was planned.
        if let Some((id, revision)) = &request.entry
            && self.application_catalog.revision(id) != Some(*revision)
        {
            eprintln!(
                "thing: application {id:?} changed while its launch waited for activation; launch it again"
            );
            return;
        }
        match request.work {
            PendingWork::Spawn(request) => self.spawn_launch(request, token),
            PendingWork::Activate(call) => {
                if let Err(error) = self.activator.send(*call, token.map(str::to_owned)) {
                    eprintln!("thing: {error}");
                }
            }
        }
    }
    pub(super) fn spawn_launch(&mut self, request: LaunchRequest, token: Option<&str>) {
        let program = request.argv.first().map_or("<missing>", String::as_str);
        if let Err(error) = self
            .launcher
            .spawn(&request.argv, request.cwd.as_deref(), token)
        {
            if let Some(cwd) = &request.cwd {
                eprintln!("thing: cannot launch {program:?} in {cwd:?}: {error}");
            } else {
                eprintln!("thing: cannot launch {program:?}: {error}");
            }
        }
    }
    pub(super) fn expire_launches(&mut self) {
        for (request, token) in self.pending_launches.expire(Instant::now()) {
            token.destroy();
            self.finish_launch(request, None);
        }
    }
    pub(super) fn reap_children(&mut self) {
        for (pid, error) in self.launcher.reap() {
            eprintln!("thing: cannot reap launched process {pid}: {error}");
        }
    }
}
