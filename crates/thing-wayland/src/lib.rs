//! wayland host for the thing runtime.

mod audio;
mod clock;
mod configuration;
mod egl;
mod host;
mod hover;
mod launch;
mod media;
mod niri;
mod notifications;
mod panel;
mod popup;
mod power;
mod render;
#[cfg(feature = "vulkan")]
mod vulkan;
mod watch;
pub use host::{run, run_with_options};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Renderer {
    #[default]
    OpenGl,
    Vulkan,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunOptions {
    pub renderer: Renderer,
    pub notification_server: bool,
}

impl RunOptions {
    pub fn with_renderer(mut self, renderer: Renderer) -> Self {
        self.renderer = renderer;
        self
    }

    pub fn with_notification_server(mut self, enabled: bool) -> Self {
        self.notification_server = enabled;
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
