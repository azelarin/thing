use crate::Kind;

macro_rules! cursors {
    ($($variant:ident => $name:literal),* $(,)?) => {
        /// a pointer cursor shape. names follow the standard css and
        /// cursor-shape-v1 names, written with underscores.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub enum Cursor {
            #[default]
            $($variant),*
        }

        impl Cursor {
            pub const ALL: &[Cursor] = &[$(Cursor::$variant),*];

            pub fn name(self) -> &'static str {
                match self {
                    $(Cursor::$variant => $name),*
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Cursor::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

cursors! {
    Default => "default",
    ContextMenu => "context_menu",
    Help => "help",
    Pointer => "pointer",
    Progress => "progress",
    Wait => "wait",
    Cell => "cell",
    Crosshair => "crosshair",
    Text => "text",
    VerticalText => "vertical_text",
    Alias => "alias",
    Copy => "copy",
    Move => "move",
    NoDrop => "no_drop",
    NotAllowed => "not_allowed",
    Grab => "grab",
    Grabbing => "grabbing",
    EResize => "e_resize",
    NResize => "n_resize",
    NeResize => "ne_resize",
    NwResize => "nw_resize",
    SResize => "s_resize",
    SeResize => "se_resize",
    SwResize => "sw_resize",
    WResize => "w_resize",
    EwResize => "ew_resize",
    NsResize => "ns_resize",
    NeswResize => "nesw_resize",
    NwseResize => "nwse_resize",
    ColResize => "col_resize",
    RowResize => "row_resize",
    AllScroll => "all_scroll",
    ZoomIn => "zoom_in",
    ZoomOut => "zoom_out",
}

impl Kind {
    /// the cursor an enabled control shows when it sets none of its own.
    /// like a browser's controls, this beats a cursor inherited from an
    /// ancestor, so a text field inside a `pointer` card still shows `text`.
    pub fn default_cursor(self) -> Option<Cursor> {
        match self {
            Kind::Button | Kind::Slider | Kind::Range => Some(Cursor::Pointer),
            Kind::TextInput | Kind::SecretInput => Some(Cursor::Text),
            _ => None,
        }
    }
}

/// the cursor a widget contributes before inheritance: its own explicit
/// value, else its control default while it is enabled.
pub fn own_cursor(explicit: Option<Cursor>, kind: Kind, enabled: bool) -> Option<Cursor> {
    explicit.or_else(|| enabled.then(|| kind.default_cursor()).flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_unknown_names_are_rejected() {
        for cursor in Cursor::ALL {
            assert_eq!(Cursor::from_name(cursor.name()), Some(*cursor));
            assert!(!cursor.name().contains('-'));
        }
        assert_eq!(Cursor::ALL.len(), 34);
        for name in ["", "hand", "not-allowed", "Pointer", "dnd_ask"] {
            assert_eq!(Cursor::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn explicit_cursors_beat_control_defaults_which_need_an_enabled_control() {
        assert_eq!(own_cursor(None, Kind::Button, true), Some(Cursor::Pointer));
        assert_eq!(own_cursor(None, Kind::TextInput, true), Some(Cursor::Text));
        assert_eq!(own_cursor(None, Kind::Button, false), None);
        assert_eq!(own_cursor(None, Kind::Row, true), None);
        assert_eq!(
            own_cursor(Some(Cursor::NotAllowed), Kind::Button, false),
            Some(Cursor::NotAllowed)
        );
        assert_eq!(
            own_cursor(Some(Cursor::Default), Kind::Button, true),
            Some(Cursor::Default)
        );
    }
}
