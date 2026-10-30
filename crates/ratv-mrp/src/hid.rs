//! Remote buttons, sent as HID usages with `SendButtonEventMessage`.

use crate::proto::SendButtonEventMessage;

const GENERIC_DESKTOP: u32 = 0x01;
const CONSUMER: u32 = 0x0c;

/// Remote button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum Button {
    Up,
    Down,
    Left,
    Right,
    Select,
    Menu,
    Home,
    TopMenu,
    PlayPause,
    Stop,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    Mute,
    Suspend,
    Wakeup,
    /// Siri. While it is held, the device takes voice input from a registered
    /// voice input device, see [`register_voice_input`](crate::messages::register_voice_input).
    Siri,
}

impl Button {
    /// HID usage page and usage.
    #[must_use]
    pub const fn usage(self) -> (u32, u32) {
        match self {
            Self::Up => (GENERIC_DESKTOP, 0x8c),
            Self::Down => (GENERIC_DESKTOP, 0x8d),
            Self::Left => (GENERIC_DESKTOP, 0x8b),
            Self::Right => (GENERIC_DESKTOP, 0x8a),
            Self::Select => (GENERIC_DESKTOP, 0x89),
            Self::Menu => (GENERIC_DESKTOP, 0x86),
            Self::Suspend => (GENERIC_DESKTOP, 0x82),
            Self::Wakeup => (GENERIC_DESKTOP, 0x83),
            Self::Home => (CONSUMER, 0x40),
            Self::Siri => (CONSUMER, 0x04),
            Self::TopMenu => (CONSUMER, 0x60),
            Self::PlayPause => (CONSUMER, 0xcd),
            Self::Stop => (CONSUMER, 0xb7),
            Self::Next => (CONSUMER, 0xb5),
            Self::Previous => (CONSUMER, 0xb6),
            Self::VolumeUp => (CONSUMER, 0xe9),
            Self::VolumeDown => (CONSUMER, 0xea),
            Self::Mute => (CONSUMER, 0xe2),
        }
    }

    /// Button down or up event.
    #[must_use]
    pub const fn event(self, down: bool) -> SendButtonEventMessage {
        let (page, usage) = self.usage();
        SendButtonEventMessage {
            usage_page: Some(page),
            usage: Some(usage),
            button_down: Some(down),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usages() {
        assert_eq!(Button::Select.usage(), (1, 0x89));
        assert_eq!(Button::PlayPause.usage(), (0x0c, 0xcd));
        let e = Button::Home.event(false);
        assert_eq!(
            (e.usage_page, e.usage, e.button_down),
            (Some(0x0c), Some(0x40), Some(false))
        );
    }
}
