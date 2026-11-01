//! the pointer cursor over thing's surfaces.
//!
//! sctk's themed pointer sends `wp_cursor_shape_device_v1.set_shape` when the
//! compositor offers cursor-shape-v1. otherwise it draws the xcursor theme
//! named by `XCURSOR_THEME` and `XCURSOR_SIZE` on its own cursor surface.
//! without `wl_shm` there is no fallback surface, and thing leaves the cursor
//! to the compositor.

use std::collections::HashSet;

use smithay_client_toolkit::{
    compositor::SurfaceData,
    seat::pointer::{CursorIcon, PointerData, PointerDataExt, PointerThemeError, ThemedPointer},
};
use thing_core::Cursor;
use wayland_client::{
    Connection, Proxy,
    protocol::{wl_pointer::WlPointer, wl_surface::WlSurface},
};

/// the seat's pointer, with a cursor when the host can set one.
pub(crate) enum SeatPointer {
    Themed(ThemedPointer),
    Plain(WlPointer),
}

impl SeatPointer {
    /// releases the pointer. a themed pointer also destroys its cursor
    /// surface and shape device.
    pub(crate) fn release(self) {
        match self {
            Self::Themed(pointer) => drop(pointer),
            Self::Plain(pointer) => pointer.release(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Shown {
    surface: WlSurface,
    cursor: Cursor,
    serial: u32,
    scale: i32,
}

/// the cursor last sent for the current pointer enter.
#[derive(Default)]
pub(crate) struct CursorState {
    shown: Option<Shown>,
    /// shapes the fallback theme lacks, reported once each.
    missing: HashSet<Cursor>,
}

impl CursorState {
    /// forgets what was shown, after the pointer leaves or is replaced.
    pub(crate) fn reset(&mut self) {
        self.shown = None;
    }

    /// shows `cursor` over `surface`. each enter has its own serial, so a new
    /// enter always sends the cursor again. while a press or capture holds the
    /// pointer, the cursor shown when it began stays.
    pub(crate) fn show(
        &mut self,
        connection: &Connection,
        pointer: &SeatPointer,
        surface: &WlSurface,
        cursor: Cursor,
        held: bool,
    ) {
        let SeatPointer::Themed(pointer) = pointer else {
            return;
        };
        let Some(serial) = pointer
            .pointer()
            .data::<PointerData>()
            .and_then(|data| data.pointer_data().latest_enter_serial())
        else {
            return;
        };
        let current = self
            .shown
            .as_ref()
            .filter(|shown| &shown.surface == surface && shown.serial == serial);
        let cursor = current
            .filter(|_| held)
            .map_or(cursor, |shown| shown.cursor);
        let scale = pointer
            .surface()
            .data::<SurfaceData>()
            .map_or(1, SurfaceData::scale_factor);
        // a stationary cursor still needs a new theme buffer after an output
        // scale change, including while its shape is held during a press.
        if current.is_some_and(|shown| shown.cursor == cursor && shown.scale == scale) {
            return;
        }
        match pointer.set_cursor(connection, icon(cursor)) {
            Ok(()) => {}
            Err(PointerThemeError::MissingEnterSerial) => return,
            Err(error) => {
                // without the shape protocol the theme decides which shapes
                // exist; the compositor keeps whatever cursor it showed.
                if self.missing.insert(cursor) {
                    eprintln!(
                        "thing: cannot show the {:?} cursor from the cursor theme: {error}",
                        cursor.name()
                    );
                }
            }
        }
        // a failed shape is not retried for this enter, which keeps a theme
        // without it from being searched every frame.
        self.shown = Some(Shown {
            surface: surface.clone(),
            cursor,
            serial,
            scale,
        });
    }
}

pub(crate) fn icon(cursor: Cursor) -> CursorIcon {
    match cursor {
        Cursor::Default => CursorIcon::Default,
        Cursor::ContextMenu => CursorIcon::ContextMenu,
        Cursor::Help => CursorIcon::Help,
        Cursor::Pointer => CursorIcon::Pointer,
        Cursor::Progress => CursorIcon::Progress,
        Cursor::Wait => CursorIcon::Wait,
        Cursor::Cell => CursorIcon::Cell,
        Cursor::Crosshair => CursorIcon::Crosshair,
        Cursor::Text => CursorIcon::Text,
        Cursor::VerticalText => CursorIcon::VerticalText,
        Cursor::Alias => CursorIcon::Alias,
        Cursor::Copy => CursorIcon::Copy,
        Cursor::Move => CursorIcon::Move,
        Cursor::NoDrop => CursorIcon::NoDrop,
        Cursor::NotAllowed => CursorIcon::NotAllowed,
        Cursor::Grab => CursorIcon::Grab,
        Cursor::Grabbing => CursorIcon::Grabbing,
        Cursor::EResize => CursorIcon::EResize,
        Cursor::NResize => CursorIcon::NResize,
        Cursor::NeResize => CursorIcon::NeResize,
        Cursor::NwResize => CursorIcon::NwResize,
        Cursor::SResize => CursorIcon::SResize,
        Cursor::SeResize => CursorIcon::SeResize,
        Cursor::SwResize => CursorIcon::SwResize,
        Cursor::WResize => CursorIcon::WResize,
        Cursor::EwResize => CursorIcon::EwResize,
        Cursor::NsResize => CursorIcon::NsResize,
        Cursor::NeswResize => CursorIcon::NeswResize,
        Cursor::NwseResize => CursorIcon::NwseResize,
        Cursor::ColResize => CursorIcon::ColResize,
        Cursor::RowResize => CursorIcon::RowResize,
        Cursor::AllScroll => CursorIcon::AllScroll,
        Cursor::ZoomIn => CursorIcon::ZoomIn,
        Cursor::ZoomOut => CursorIcon::ZoomOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cursor_maps_to_the_standard_name() {
        for cursor in Cursor::ALL {
            assert_eq!(icon(*cursor).name().replace('-', "_"), cursor.name());
        }
    }
}
