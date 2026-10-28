//! wayland host for the thing runtime.

mod audio;
pub mod authentication;
mod clipboard;
mod clock;
mod configuration;
mod data_transfer;
mod egl;
mod focus;
mod focused_clipboard;
mod host;
mod hover;
mod input_method;
mod keyboard;
mod launch;
mod lock;
mod managed_process;
mod media;
mod niri;
mod notifications;
mod output;
mod panel;
mod popup;
mod power;
mod render;
#[cfg(feature = "vulkan")]
mod skia_vulkan;
mod system_tray;
mod text_data;
mod watch;
pub use host::{run, run_with_options};
pub use lock::run as run_lock;
pub use system_tray::{SystemTray, SystemTrayMode};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Renderer {
    #[default]
    OpenGl,
    Vulkan,
}

/// process-wide options for the wayland host.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunOptions {
    /// graphics backend used by every surface.
    pub renderer: Renderer,
    /// whether thing should provide the desktop notification service.
    pub notification_server: bool,
    /// status notifier policy, or `None` to leave tray support disabled.
    pub system_tray: Option<SystemTrayMode>,
    /// whether thing may read and control the regular clipboard.
    pub clipboard: bool,
}

impl RunOptions {
    /// select the graphics backend.
    pub fn with_renderer(mut self, renderer: Renderer) -> Self {
        self.renderer = renderer;
        self
    }

    /// enable or disable the desktop notification service.
    pub fn with_notification_server(mut self, enabled: bool) -> Self {
        self.notification_server = enabled;
        self
    }

    /// select a status notifier policy, or disable the tray worker with `None`.
    pub fn with_system_tray(mut self, system_tray: Option<SystemTrayMode>) -> Self {
        self.system_tray = system_tray;
        self
    }

    /// enable or disable regular clipboard access.
    pub fn with_clipboard(mut self, enabled: bool) -> Self {
        self.clipboard = enabled;
        self
    }
}

impl std::str::FromStr for Renderer {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "opengl" => Ok(Self::OpenGl),
            "vulkan" => Ok(Self::Vulkan),
            _ => Err(format!(
                "unknown renderer {value:?}; expected opengl or vulkan"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_options_leave_opt_in_services_disabled_by_default() {
        assert!(!RunOptions::default().notification_server);
        assert_eq!(RunOptions::default().system_tray, None);
        assert!(!RunOptions::default().clipboard);
    }
}
