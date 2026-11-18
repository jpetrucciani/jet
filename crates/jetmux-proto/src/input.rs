//! Input from clients that leave encoding to the daemon (jet), which knows
//! each pane's current modes. jetmux's terminal client sends bytes instead.
//! Keys and modifiers are libghostty's, by number.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    /// libghostty's `key::Key`, as its `u32`.
    pub key: u32,
    pub action: KeyAction,
    /// libghostty's `key::Mods` bits.
    pub mods: u16,
    /// Text the key produces in the client's layout, without Ctrl/Alt.
    pub text: Option<String>,
    /// The key's character with no modifiers.
    pub unshifted: Option<char>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyAction {
    Press,
    Release,
    Repeat,
}

/// A mouse event over one pane, at `x`, `y` in cells from its top left
/// (fractions for where in the cell).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MouseEvent {
    pub pane: u32,
    pub action: MouseAction,
    pub x: f32,
    pub y: f32,
    pub mods: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A client's colors. The daemon answers apps' color queries (OSC 10/11/4,
/// light or dark) with the theme of whichever client typed last.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    /// `None` draws the cursor in the text color.
    pub cursor: Option<Rgb>,
    /// Entries replaced in the built-in 256-color palette.
    pub palette: Vec<(u8, Rgb)>,
}
