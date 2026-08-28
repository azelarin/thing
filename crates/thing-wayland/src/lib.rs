//! wayland host for the thing runtime.

mod audio;
mod clock;
mod egl;
mod host;
mod niri;
mod watch;
pub use host::run;
