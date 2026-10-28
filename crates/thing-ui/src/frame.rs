use thing_core::{Node, NodeId};

use crate::{Layout, SecretInputEditor, TextInputEditor};

/// transient input state belongs to the host, not to the widget snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct Interaction {
    pub hovered: Option<NodeId>,
    pub pressed: Option<NodeId>,
    pub focused: Option<NodeId>,
    focus_visible: bool,
}

impl Interaction {
    pub fn focus_from_pointer(&mut self, focused: Option<NodeId>) {
        self.focused = focused;
        self.focus_visible = false;
    }

    pub fn focus_from_keyboard(&mut self, focused: NodeId) {
        self.focused = Some(focused);
        self.focus_visible = true;
    }

    pub fn reveal_keyboard_focus(&mut self) {
        self.focus_visible = self.focused.is_some();
    }

    pub fn hide_keyboard_focus(&mut self) {
        self.focus_visible = false;
    }

    pub fn keyboard_focused(&self) -> Option<NodeId> {
        if self.focus_visible {
            self.focused
        } else {
            None
        }
    }
}

/// renderer-independent inputs for one frame.
pub struct PaintFrame<'a> {
    pub root: &'a Node,
    pub layout: &'a Layout,
    pub physical: (u32, u32),
    pub scale: f32,
    pub input: Interaction,
    pub text_input: Option<&'a mut TextInputEditor>,
    pub secret_input: Option<&'a mut SecretInputEditor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_indicator_follows_the_input_method() {
        let mut interaction = Interaction::default();
        interaction.focus_from_pointer(Some(NodeId(1)));
        assert_eq!(interaction.focused, Some(NodeId(1)));
        assert!(!interaction.focus_visible);

        interaction.reveal_keyboard_focus();
        assert!(interaction.focus_visible);

        interaction.focus_from_pointer(Some(NodeId(2)));
        assert_eq!(interaction.focused, Some(NodeId(2)));
        assert!(!interaction.focus_visible);

        interaction.focus_from_keyboard(NodeId(3));
        assert_eq!(interaction.focused, Some(NodeId(3)));
        assert!(interaction.focus_visible);

        interaction.hide_keyboard_focus();
        assert_eq!(interaction.focused, Some(NodeId(3)));
        assert!(!interaction.focus_visible);
    }
}
