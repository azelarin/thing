//! sizing and state for `ui.window` roots, which are xdg toplevels.

use std::num::NonZeroU32;

use smithay_client_toolkit::shell::xdg::window::{
    DecorationMode, Window as XdgWindow, WindowConfigure,
};
use thing_core::{WindowProperties, WindowSize};
use thing_runtime::WindowState;

/// the toplevel role of a root window and what thing last told the compositor.
pub(crate) struct WindowRole {
    pub(crate) shell: XdgWindow,
    /// properties already sent, so unchanged values send no requests.
    pub(crate) sent: WindowProperties,
    /// the compositor's latest configure, published to the configuration.
    pub(crate) state: WindowState,
}

impl WindowRole {
    /// sends every property before the initial commit.
    pub(crate) fn new(shell: XdgWindow, properties: &WindowProperties) -> Self {
        shell.set_title(properties.title.clone());
        if !properties.app_id.is_empty() {
            shell.set_app_id(properties.app_id.clone());
        }
        shell.set_min_size(properties.min_size.map(pair));
        shell.set_max_size(properties.max_size.map(pair));
        Self {
            shell,
            sent: properties.clone(),
            state: WindowState::default(),
        }
    }

    /// sends the properties that differ from what the compositor already has.
    /// size limits are double buffered and need the surface's next commit,
    /// so this returns whether one is needed. an app id cannot be withdrawn,
    /// so clearing it keeps the previous one.
    pub(crate) fn apply(&mut self, next: &WindowProperties) -> bool {
        if self.sent.title != next.title {
            self.shell.set_title(next.title.clone());
        }
        if self.sent.app_id != next.app_id && !next.app_id.is_empty() {
            self.shell.set_app_id(next.app_id.clone());
        }
        let limits = self.sent.min_size != next.min_size || self.sent.max_size != next.max_size;
        if limits {
            self.shell.set_min_size(next.min_size.map(pair));
            self.shell.set_max_size(next.max_size.map(pair));
        }
        self.sent = next.clone();
        limits
    }
}

fn pair(size: WindowSize) -> (u32, u32) {
    (size.width, size.height)
}

/// the size a window asks for before the compositor suggests one. a
/// compositor bound shrinks it, but never below `min_size`.
pub(crate) fn initial_size(
    properties: &WindowProperties,
    bounds: Option<(u32, u32)>,
) -> (u32, u32) {
    let mut size = pair(properties.size);
    if let Some(bounds) = bounds.filter(|bounds| bounds.0 > 0 && bounds.1 > 0) {
        size = (size.0.min(bounds.0), size.1.min(bounds.1));
    }
    clamp(size, properties)
}

/// resolves a configure. a nonzero suggestion is the compositor's decision
/// and is used as is; a zero axis leaves the choice to the client, which
/// keeps its current size or, before the first configure, its initial size.
pub(crate) fn configured_size(
    suggested: (Option<NonZeroU32>, Option<NonZeroU32>),
    current: Option<(u32, u32)>,
    properties: &WindowProperties,
    bounds: Option<(u32, u32)>,
) -> (u32, u32) {
    let fallback = current.map_or_else(
        || initial_size(properties, bounds),
        |size| clamp(size, properties),
    );
    (
        suggested.0.map_or(fallback.0, NonZeroU32::get),
        suggested.1.map_or(fallback.1, NonZeroU32::get),
    )
}

fn clamp(size: (u32, u32), properties: &WindowProperties) -> (u32, u32) {
    let minimum = properties.min_size.map_or((1, 1), pair);
    let maximum = properties.max_size.map_or((u32::MAX, u32::MAX), pair);
    (
        size.0.clamp(minimum.0, maximum.0.max(minimum.0)),
        size.1.clamp(minimum.1, maximum.1.max(minimum.1)),
    )
}

pub(crate) fn state(configure: &WindowConfigure, size: (u32, u32)) -> WindowState {
    use smithay_client_toolkit::reexports::csd_frame::{
        WindowManagerCapabilities, WindowState as Flags,
    };
    WindowState {
        open: true,
        width: size.0,
        height: size.1,
        maximized: configure.state.contains(Flags::MAXIMIZED),
        fullscreen: configure.state.contains(Flags::FULLSCREEN),
        activated: configure.state.contains(Flags::ACTIVATED),
        can_maximize: configure
            .capabilities
            .contains(WindowManagerCapabilities::MAXIMIZE),
        can_fullscreen: configure
            .capabilities
            .contains(WindowManagerCapabilities::FULLSCREEN),
        decorated: configure.decoration_mode == DecorationMode::Server,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn properties(min: Option<(u32, u32)>, max: Option<(u32, u32)>) -> WindowProperties {
        let size = |(width, height)| WindowSize { width, height };
        WindowProperties {
            size: size((640, 480)),
            min_size: min.map(size),
            max_size: max.map(size),
            ..WindowProperties::default()
        }
    }

    #[test]
    fn zero_suggestions_keep_the_client_size_and_nonzero_ones_win() {
        let window = properties(Some((320, 240)), Some((800, 600)));
        let none = (None, None);
        assert_eq!(configured_size(none, None, &window, None), (640, 480));
        // a bound shrinks the initial size, but not below the minimum.
        assert_eq!(
            configured_size(none, None, &window, Some((500, 200))),
            (500, 240)
        );
        assert_eq!(
            configured_size(none, Some((700, 300)), &window, None),
            (700, 300)
        );
        // a size that limits no longer allow is clamped on the next configure.
        assert_eq!(
            configured_size(none, Some((900, 100)), &window, None),
            (800, 240)
        );
        let width = NonZeroU32::new(1000);
        assert_eq!(
            configured_size((width, None), Some((700, 300)), &window, None),
            (1000, 300)
        );
        assert_eq!(
            initial_size(&properties(None, None), Some((0, 0))),
            (640, 480)
        );
    }
}
