//! wayland host for the thing runtime.

mod audio;
mod clock;
mod configuration;
mod egl;
mod host;
mod hover;
mod niri;
mod watch;
pub use host::run;
