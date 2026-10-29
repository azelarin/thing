use std::time::Duration;

use smithay_client_toolkit::seat::keyboard::{KeyEvent, Modifiers, RepeatInfo};
use thing_runtime::KeyInput;
use xkbcommon::xkb::{self, Keysym};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootKeyDispatch {
    Missing,
    Unhandled,
    Handled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextKeyAction {
    Insert(String),
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveToStart,
    MoveToEnd,
    SelectAll,
    Submit,
}

impl TextKeyAction {
    pub(crate) fn repeats(&self) -> bool {
        matches!(
            self,
            Self::Insert(_) | Self::Backspace | Self::Delete | Self::MoveLeft | Self::MoveRight
        )
    }

    pub(crate) fn changes_value(&self) -> bool {
        matches!(self, Self::Insert(_) | Self::Backspace | Self::Delete)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RepeatSettings {
    pub(crate) delay: Duration,
    pub(crate) interval: Duration,
}

impl RepeatSettings {
    fn from_info(info: RepeatInfo) -> Option<Self> {
        match info {
            RepeatInfo::Repeat { rate, delay } => Some(Self {
                delay: Duration::from_millis(u64::from(delay)),
                interval: Duration::from_micros((1_000_000 / u64::from(rate.get())).max(1)),
            }),
            RepeatInfo::Disable => None,
        }
    }
}

pub(crate) struct RepeatingKey<S> {
    pub(crate) event: KeyEvent,
    pub(crate) surface: S,
    pub(crate) first: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepeatTimerAction {
    None,
    Arm(Duration),
    Disarm,
}

pub(crate) struct RepeatSession<S> {
    pub(crate) settings: Option<RepeatSettings>,
    pub(crate) key: Option<RepeatingKey<S>>,
    host_armed: bool,
}

impl<S> Default for RepeatSession<S> {
    fn default() -> Self {
        Self {
            settings: None,
            key: None,
            host_armed: false,
        }
    }
}

impl<S: PartialEq> RepeatSession<S> {
    pub(crate) fn press(
        &mut self,
        event: KeyEvent,
        surface: S,
        key_repeats: bool,
        action_repeats: bool,
    ) -> RepeatTimerAction {
        if !key_repeats {
            return RepeatTimerAction::None;
        }
        if !action_repeats {
            return self.clear();
        }
        self.key = Some(RepeatingKey {
            event,
            surface,
            first: true,
        });
        match self.settings {
            Some(settings) => {
                self.host_armed = true;
                RepeatTimerAction::Arm(settings.delay)
            }
            None if self.host_armed => {
                self.host_armed = false;
                RepeatTimerAction::Disarm
            }
            None => RepeatTimerAction::None,
        }
    }

    pub(crate) fn update(&mut self, info: RepeatInfo) -> RepeatTimerAction {
        self.settings = RepeatSettings::from_info(info);
        match (self.settings, self.key.as_mut()) {
            (Some(settings), Some(key)) => {
                key.first = true;
                self.host_armed = true;
                RepeatTimerAction::Arm(settings.delay)
            }
            _ if self.host_armed => {
                self.host_armed = false;
                RepeatTimerAction::Disarm
            }
            _ => RepeatTimerAction::None,
        }
    }

    pub(crate) fn release(&mut self, raw_code: u32) -> RepeatTimerAction {
        if self
            .key
            .as_ref()
            .is_some_and(|key| key.event.raw_code == raw_code)
        {
            self.clear()
        } else {
            RepeatTimerAction::None
        }
    }

    pub(crate) fn clear(&mut self) -> RepeatTimerAction {
        self.key = None;
        if self.host_armed {
            self.host_armed = false;
            RepeatTimerAction::Disarm
        } else {
            RepeatTimerAction::None
        }
    }

    pub(crate) fn accepts_compositor_repeat(&self, raw_code: u32, surface: &S) -> bool {
        self.settings.is_none()
            && self
                .key
                .as_ref()
                .is_some_and(|key| key.event.raw_code == raw_code && &key.surface == surface)
    }

    #[cfg(test)]
    pub(crate) fn host_is_armed(&self) -> bool {
        self.host_armed
    }
}

pub(crate) fn text_key_action(
    event: &KeyEvent,
    modifiers: Modifiers,
    direct: bool,
) -> Option<TextKeyAction> {
    if modifiers.logo || (modifiers.alt && !modifiers.ctrl) {
        return None;
    }
    match event.keysym {
        Keysym::BackSpace => return Some(TextKeyAction::Backspace),
        Keysym::Delete => return Some(TextKeyAction::Delete),
        Keysym::Left => return Some(TextKeyAction::MoveLeft),
        Keysym::Right => return Some(TextKeyAction::MoveRight),
        Keysym::Home if direct => return Some(TextKeyAction::MoveToStart),
        Keysym::End if direct => return Some(TextKeyAction::MoveToEnd),
        Keysym::Return | Keysym::KP_Enter if direct => return Some(TextKeyAction::Submit),
        Keysym::a | Keysym::A if direct && modifiers.ctrl && !modifiers.alt => {
            return Some(TextKeyAction::SelectAll);
        }
        _ => {}
    }
    if modifiers.ctrl && !modifiers.alt {
        return None;
    }
    let text = event.utf8.as_ref()?;
    (!text.is_empty()
        && !text.chars().any(|character| {
            character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
        }))
    .then(|| TextKeyAction::Insert(text.clone()))
}

pub(crate) fn root_key_input(event: &KeyEvent, modifiers: Modifiers, repeated: bool) -> KeyInput {
    let mut key = xkb::keysym_get_name(event.keysym);
    if key.is_empty() {
        key = format!("0x{:08x}", event.keysym.raw());
    }
    KeyInput {
        key,
        text: event.utf8.clone().filter(|text| !text.is_empty()),
        shift: modifiers.shift,
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        logo: modifiers.logo,
        repeated,
    }
}

pub(crate) fn repeat_action_is_current<T: PartialEq>(
    action_repeats: bool,
    before: &Option<T>,
    after: &Option<T>,
) -> bool {
    action_repeats && before == after
}
