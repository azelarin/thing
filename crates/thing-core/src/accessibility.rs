use crate::{Kind, TreeError};

/// maximum UTF-8 bytes in one explicit accessibility string.
pub const MAX_ACCESSIBILITY_TEXT_BYTES: usize = 4096;

/// an override for how assistive technology describes a widget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessibilityRole {
    Group,
    Toolbar,
    List,
    ListItem,
    Menu,
    MenuItem,
    MenuItemCheckbox,
    MenuItemRadio,
    Dialog,
    Status,
    Alert,
    Checkbox,
    Radio,
    Switch,
    Image,
}

impl AccessibilityRole {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "group" => Self::Group,
            "toolbar" => Self::Toolbar,
            "list" => Self::List,
            "list_item" => Self::ListItem,
            "menu" => Self::Menu,
            "menu_item" => Self::MenuItem,
            "menu_item_checkbox" => Self::MenuItemCheckbox,
            "menu_item_radio" => Self::MenuItemRadio,
            "dialog" => Self::Dialog,
            "status" => Self::Status,
            "alert" => Self::Alert,
            "checkbox" => Self::Checkbox,
            "radio" => Self::Radio,
            "switch" => Self::Switch,
            "image" => Self::Image,
            _ => return None,
        })
    }

    fn supports(self, kind: Kind) -> bool {
        let container = kind.is_container() && !kind.is_interactive() && kind != Kind::PointerArea;
        match self {
            Self::Group | Self::Toolbar | Self::List | Self::Menu | Self::Dialog => container,
            Self::ListItem => container || kind == Kind::Button,
            Self::Status | Self::Alert => container || kind == Kind::Text,
            Self::Image => matches!(kind, Kind::Image | Kind::Path),
            Self::MenuItem
            | Self::MenuItemCheckbox
            | Self::MenuItemRadio
            | Self::Checkbox
            | Self::Radio
            | Self::Switch => kind == Kind::Button,
        }
    }
}

/// mixed values are available only to checkbox roles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessibilityChecked {
    Unchecked,
    Checked,
    Mixed,
}

/// how urgently changes to a region should be announced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessibilityLive {
    Polite,
    Assertive,
}

/// a widget's spoken name, semantic role, and resolved control states.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Accessibility {
    pub label: Option<String>,
    pub description: Option<String>,
    pub role: Option<AccessibilityRole>,
    pub selected: Option<bool>,
    pub checked: Option<AccessibilityChecked>,
    pub expanded: Option<bool>,
    pub value_text: Option<String>,
    pub live: Option<AccessibilityLive>,
    pub hidden: bool,
}

impl Accessibility {
    pub(crate) fn validate(&self, kind: Kind, interactive: bool) -> Result<(), TreeError> {
        for (name, value) in [
            ("label", self.label.as_deref()),
            ("description", self.description.as_deref()),
            ("value_text", self.value_text.as_deref()),
        ] {
            if value.is_some_and(|value| {
                value.len() > MAX_ACCESSIBILITY_TEXT_BYTES || value.contains('\0')
            }) {
                return Err(TreeError(format!(
                    "accessibility {name} must be at most 4096 bytes without NUL"
                )));
            }
        }
        if let Some(role) = self.role
            && (!role.supports(kind) || (kind == Kind::Text && interactive))
        {
            return Err(TreeError(format!(
                "accessibility role {role:?} is not supported on {kind:?}"
            )));
        }
        if self.hidden && (interactive || !matches!(kind, Kind::Text | Kind::Image | Kind::Path)) {
            return Err(TreeError(
                "accessibility hidden is available only on decorative text, images, and paths"
                    .into(),
            ));
        }
        if self.checked.is_some()
            && !matches!(
                self.role,
                Some(
                    AccessibilityRole::Checkbox
                        | AccessibilityRole::Radio
                        | AccessibilityRole::Switch
                        | AccessibilityRole::MenuItemCheckbox
                        | AccessibilityRole::MenuItemRadio
                )
            )
        {
            return Err(TreeError(
                "accessibility checked needs a checkbox, radio, or switch role".into(),
            ));
        }
        if self.checked == Some(AccessibilityChecked::Mixed)
            && !matches!(
                self.role,
                Some(AccessibilityRole::Checkbox | AccessibilityRole::MenuItemCheckbox)
            )
        {
            return Err(TreeError(
                "accessibility mixed state needs a checkbox role".into(),
            ));
        }
        if self.selected.is_some()
            && !matches!(
                self.role,
                Some(
                    AccessibilityRole::ListItem
                        | AccessibilityRole::MenuItem
                        | AccessibilityRole::MenuItemCheckbox
                        | AccessibilityRole::MenuItemRadio
                )
            )
        {
            return Err(TreeError(
                "accessibility selected needs a list or menu item role".into(),
            ));
        }
        if self.expanded.is_some()
            && !((kind == Kind::Button && self.role.is_none())
                || matches!(
                    self.role,
                    Some(
                        AccessibilityRole::ListItem
                            | AccessibilityRole::MenuItem
                            | AccessibilityRole::MenuItemCheckbox
                            | AccessibilityRole::MenuItemRadio
                    )
                ))
        {
            return Err(TreeError(
                "accessibility expanded needs a button or item role".into(),
            ));
        }
        if self.value_text.is_some()
            && !matches!(kind, Kind::Slider | Kind::Range | Kind::TextInput)
        {
            return Err(TreeError(
                "accessibility value_text needs a range or text input".into(),
            ));
        }
        if self.live.is_some()
            && (interactive
                || !matches!(
                    kind,
                    Kind::Text
                        | Kind::Panel
                        | Kind::Window
                        | Kind::Popup
                        | Kind::Row
                        | Kind::Column
                        | Kind::CenterBox
                        | Kind::Stack
                        | Kind::Crossfade
                        | Kind::Scroll
                ))
        {
            return Err(TreeError(
                "accessibility live needs text or a noninteractive container".into(),
            ));
        }
        Ok(())
    }
}
