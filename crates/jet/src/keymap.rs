//! Windows virtual-key codes to libghostty keys. Plain numbers so the table
//! builds and tests on any platform.

use jet_core::Key;

use crate::app::Kind;

const LETTERS: [Key; 26] = [
    Key::A,
    Key::B,
    Key::C,
    Key::D,
    Key::E,
    Key::F,
    Key::G,
    Key::H,
    Key::I,
    Key::J,
    Key::K,
    Key::L,
    Key::M,
    Key::N,
    Key::O,
    Key::P,
    Key::Q,
    Key::R,
    Key::S,
    Key::T,
    Key::U,
    Key::V,
    Key::W,
    Key::X,
    Key::Y,
    Key::Z,
];
const DIGITS: [Key; 10] = [
    Key::Digit0,
    Key::Digit1,
    Key::Digit2,
    Key::Digit3,
    Key::Digit4,
    Key::Digit5,
    Key::Digit6,
    Key::Digit7,
    Key::Digit8,
    Key::Digit9,
];
const NUMPAD: [Key; 10] = [
    Key::Numpad0,
    Key::Numpad1,
    Key::Numpad2,
    Key::Numpad3,
    Key::Numpad4,
    Key::Numpad5,
    Key::Numpad6,
    Key::Numpad7,
    Key::Numpad8,
    Key::Numpad9,
];
const F_KEYS: [Key; 24] = [
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
    Key::F13,
    Key::F14,
    Key::F15,
    Key::F16,
    Key::F17,
    Key::F18,
    Key::F19,
    Key::F20,
    Key::F21,
    Key::F22,
    Key::F23,
    Key::F24,
];

pub fn lookup(vk: u16) -> Option<(Key, Kind)> {
    use Kind::{Functional, Modifier, Text};
    let index = |base: u16| usize::from(vk - base);
    Some(match vk {
        0x08 => (Key::Backspace, Functional),
        0x09 => (Key::Tab, Functional),
        0x0D => (Key::Enter, Functional),
        0x10 => (Key::ShiftLeft, Modifier),
        0x11 => (Key::ControlLeft, Modifier),
        0x12 => (Key::AltLeft, Modifier),
        0x14 => (Key::CapsLock, Modifier),
        0x1B => (Key::Escape, Functional),
        0x20 => (Key::Space, Text),
        0x21 => (Key::PageUp, Functional),
        0x22 => (Key::PageDown, Functional),
        0x23 => (Key::End, Functional),
        0x24 => (Key::Home, Functional),
        0x25 => (Key::ArrowLeft, Functional),
        0x26 => (Key::ArrowUp, Functional),
        0x27 => (Key::ArrowRight, Functional),
        0x28 => (Key::ArrowDown, Functional),
        0x2D => (Key::Insert, Functional),
        0x2E => (Key::Delete, Functional),
        0x30..=0x39 => (*DIGITS.get(index(0x30))?, Text),
        0x41..=0x5A => (*LETTERS.get(index(0x41))?, Text),
        0x5B | 0x5C => (Key::MetaLeft, Modifier),
        0x60..=0x69 => (*NUMPAD.get(index(0x60))?, Text),
        0x6A => (Key::NumpadMultiply, Text),
        0x6B => (Key::NumpadAdd, Text),
        0x6D => (Key::NumpadSubtract, Text),
        0x6E => (Key::NumpadDecimal, Text),
        0x6F => (Key::NumpadDivide, Text),
        0x70..=0x87 => (*F_KEYS.get(index(0x70))?, Functional),
        0xA0 => (Key::ShiftLeft, Modifier),
        0xA1 => (Key::ShiftRight, Modifier),
        0xA2 => (Key::ControlLeft, Modifier),
        0xA3 => (Key::ControlRight, Modifier),
        0xA4 => (Key::AltLeft, Modifier),
        0xA5 => (Key::AltRight, Modifier),
        0xBA => (Key::Semicolon, Text),
        0xBB => (Key::Equal, Text),
        0xBC => (Key::Comma, Text),
        0xBD => (Key::Minus, Text),
        0xBE => (Key::Period, Text),
        0xBF => (Key::Slash, Text),
        0xC0 => (Key::Backquote, Text),
        0xDB => (Key::BracketLeft, Text),
        0xDC => (Key::Backslash, Text),
        0xDD => (Key::BracketRight, Text),
        0xDE => (Key::Quote, Text),
        _ => return None,
    })
}

/// The virtual key for `key`, to register it as a global hotkey.
pub fn vk_of(key: Key) -> Option<u16> {
    (0..=0xFE).find(|&vk| lookup(vk).is_some_and(|(k, _)| k == key))
}

#[cfg(test)]
mod tests {
    use jet_core::Key;

    use crate::app::Kind;
    use crate::keymap::{lookup, vk_of};

    #[test]
    fn hotkeys_find_their_virtual_key() {
        assert_eq!(vk_of(Key::Backquote), Some(0xC0));
        assert_eq!(vk_of(Key::Space), Some(0x20));
        assert_eq!(vk_of(Key::F12), Some(0x7B));
    }

    #[test]
    fn maps_ranges_and_specials() {
        assert_eq!(lookup(0x41), Some((Key::A, Kind::Text)));
        assert_eq!(lookup(0x5A), Some((Key::Z, Kind::Text)));
        assert_eq!(lookup(0x39), Some((Key::Digit9, Kind::Text)));
        assert_eq!(lookup(0x87), Some((Key::F24, Kind::Functional)));
        assert_eq!(lookup(0x26), Some((Key::ArrowUp, Kind::Functional)));
        assert_eq!(lookup(0x10), Some((Key::ShiftLeft, Kind::Modifier)));
        assert_eq!(lookup(0xFF), None);
    }
}
