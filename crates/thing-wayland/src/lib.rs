//! wayland host for the thing runtime.

mod audio;
mod clock;
mod configuration;
mod egl;
mod host;
mod niri;
mod watch;
pub use host::run;
