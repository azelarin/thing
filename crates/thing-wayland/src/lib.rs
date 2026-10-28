//! wayland host for the thing runtime.

mod audio;
mod clock;
mod configuration;
mod egl;
mod host;
mod hover;
mod launch;
mod niri;
mod panel;
mod render;
#[cfg(feature = "vulkan")]
mod vulkan;
mod watch;
pub use host::run;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Renderer {
    #[default]
    OpenGl,
    Vulkan,
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
