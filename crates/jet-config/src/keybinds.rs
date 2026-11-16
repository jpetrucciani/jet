//! Key chords and the actions they run. Names follow Ghostty's
//! (`ctrl+shift+t`, `new_split:right`) so a Ghostty `keybind` line means
//! the same thing in jet.

use jet_core::{Key, Mods};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Which split `goto_split` moves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitTarget {
    Toward(Direction),
    Previous,
    Next,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Copy,
    Paste,
    /// In a `[profiles]` entry, or the default shell.
    NewTab(Option<String>),
    /// Close the focused split (the tab, if it's the last one).
    CloseSurface,
    CloseTab,
    NextTab,
    PreviousTab,
    /// 1-based.
    GotoTab(usize),
    LastTab,
    NewSplit(Direction, Option<String>),
    GotoSplit(SplitTarget),
    /// Points; negative shrinks.
    FontSize(f32),
    ResetFontSize,
    /// Scroll to the `n`th shell prompt (OSC 133) up (negative) or down.
    JumpToPrompt(isize),
    /// Copy the last command's output (OSC 133).
    CopyCommandOutput,
    /// Open the find bar over the focused pane.
    StartSearch,
    EndSearch,
    /// To the `n`th match up (negative, older output) or down.
    NavigateSearch(isize),
    ToggleCommandPalette,
    /// The focused split takes the whole tab, or gives it back.
    ToggleSplitZoom,
    ReloadConfig,
    /// Swallow the key.
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub key: Key,
    /// Only Ctrl, Shift, Alt and Super.
    pub mods: Mods,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub chord: Chord,
    /// `None` unbinds a default.
    pub action: Option<Action>,
}

pub const DEFAULTS: &[(&str, &str)] = &[
    ("ctrl+shift+c", "copy_to_clipboard"),
    ("ctrl+insert", "copy_to_clipboard"),
    ("ctrl+shift+v", "paste_from_clipboard"),
    ("shift+insert", "paste_from_clipboard"),
    ("ctrl+t", "new_tab"),
    ("ctrl+shift+t", "new_tab"),
    ("ctrl+shift+w", "close_surface"),
    ("ctrl+tab", "next_tab"),
    ("ctrl+shift+tab", "previous_tab"),
    ("ctrl+page_down", "next_tab"),
    ("ctrl+page_up", "previous_tab"),
    ("alt+1", "goto_tab:1"),
    ("alt+2", "goto_tab:2"),
    ("alt+3", "goto_tab:3"),
    ("alt+4", "goto_tab:4"),
    ("alt+5", "goto_tab:5"),
    ("alt+6", "goto_tab:6"),
    ("alt+7", "goto_tab:7"),
    ("alt+8", "goto_tab:8"),
    ("alt+9", "last_tab"),
    ("ctrl+shift+o", "new_split:right"),
    ("ctrl+shift+e", "new_split:down"),
    ("ctrl+alt+left", "goto_split:left"),
    ("ctrl+alt+right", "goto_split:right"),
    ("ctrl+alt+up", "goto_split:up"),
    ("ctrl+alt+down", "goto_split:down"),
    ("ctrl+shift+bracket_left", "goto_split:previous"),
    ("ctrl+shift+bracket_right", "goto_split:next"),
    ("ctrl+equal", "increase_font_size:1"),
    ("ctrl+plus", "increase_font_size:1"),
    ("ctrl+minus", "decrease_font_size:1"),
    ("ctrl+0", "reset_font_size"),
    ("ctrl+shift+page_up", "jump_to_prompt:-1"),
    ("ctrl+shift+page_down", "jump_to_prompt:1"),
    ("ctrl+shift+f", "start_search"),
    ("ctrl+shift+p", "toggle_command_palette"),
    ("ctrl+shift+enter", "toggle_split_zoom"),
];

/// macOS's: Command chords, as its apps (and Ghostty) use, leaving Ctrl
/// and Option to the shell.
pub const MAC_DEFAULTS: &[(&str, &str)] = &[
    ("cmd+c", "copy_to_clipboard"),
    ("cmd+v", "paste_from_clipboard"),
    ("cmd+t", "new_tab"),
    ("cmd+w", "close_surface"),
    ("ctrl+tab", "next_tab"),
    ("ctrl+shift+tab", "previous_tab"),
    ("cmd+shift+bracket_right", "next_tab"),
    ("cmd+shift+bracket_left", "previous_tab"),
    ("cmd+1", "goto_tab:1"),
    ("cmd+2", "goto_tab:2"),
    ("cmd+3", "goto_tab:3"),
    ("cmd+4", "goto_tab:4"),
    ("cmd+5", "goto_tab:5"),
    ("cmd+6", "goto_tab:6"),
    ("cmd+7", "goto_tab:7"),
    ("cmd+8", "goto_tab:8"),
    ("cmd+9", "last_tab"),
    ("cmd+d", "new_split:right"),
    ("cmd+shift+d", "new_split:down"),
    ("cmd+alt+left", "goto_split:left"),
    ("cmd+alt+right", "goto_split:right"),
    ("cmd+alt+up", "goto_split:up"),
    ("cmd+alt+down", "goto_split:down"),
    ("cmd+bracket_left", "goto_split:previous"),
    ("cmd+bracket_right", "goto_split:next"),
    ("cmd+equal", "increase_font_size:1"),
    ("cmd+plus", "increase_font_size:1"),
    ("cmd+minus", "decrease_font_size:1"),
    ("cmd+0", "reset_font_size"),
    ("cmd+up", "jump_to_prompt:-1"),
    ("cmd+down", "jump_to_prompt:1"),
    ("cmd+f", "start_search"),
    ("cmd+g", "navigate_search:next"),
    ("cmd+shift+g", "navigate_search:previous"),
    ("cmd+shift+p", "toggle_command_palette"),
    ("cmd+shift+enter", "toggle_split_zoom"),
];

/// This platform's defaults.
const PLATFORM_DEFAULTS: &[(&str, &str)] = if cfg!(target_os = "macos") {
    MAC_DEFAULTS
} else {
    DEFAULTS
};

/// The defaults (unless `use_defaults` is off) with `custom` on top; later
/// bindings of a chord replace earlier ones.
pub fn resolve(
    use_defaults: bool,
    custom: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<Binding>, String> {
    resolve_over(PLATFORM_DEFAULTS, use_defaults, custom)
}

fn resolve_over(
    defaults: &[(&str, &str)],
    use_defaults: bool,
    custom: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<Binding>, String> {
    let defaults = defaults
        .iter()
        .filter(|_| use_defaults)
        .map(|(c, a)| ((*c).to_owned(), (*a).to_owned()));
    let mut out: Vec<Binding> = Vec::new();
    for (chord, action) in defaults.chain(custom) {
        let binding = parse(&chord, &action)?;
        out.retain(|b| b.chord != binding.chord);
        out.push(binding);
    }
    Ok(out)
}

pub fn parse(chord: &str, action: &str) -> Result<Binding, String> {
    Ok(Binding {
        chord: parse_chord(chord)
            .ok_or_else(|| format!("keybinds: {chord:?} isn't a key chord"))?,
        action: match action {
            "unbind" => None,
            action => Some(
                parse_action(action)
                    .ok_or_else(|| format!("keybinds: {action:?} isn't an action"))?,
            ),
        },
    })
}

/// The binding for `key` with exactly `mods` (sides and locks ignored).
pub fn find(bindings: &[Binding], key: Key, mods: Mods) -> Option<Option<Action>> {
    let mods = mods & (Mods::CTRL | Mods::SHIFT | Mods::ALT | Mods::SUPER);
    bindings
        .iter()
        .rev()
        .find(|b| b.chord.key == key && b.chord.mods == mods)
        .map(|b| b.action.clone())
}

pub fn parse_chord(text: &str) -> Option<Chord> {
    let lower = text.trim().to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    let key = key_named(parts.pop()?)?;
    let mut mods = Mods::empty();
    for part in parts {
        mods |= match part {
            "ctrl" | "control" => Mods::CTRL,
            "shift" => Mods::SHIFT,
            "alt" | "opt" | "option" => Mods::ALT,
            "super" | "cmd" | "command" | "win" | "meta" => Mods::SUPER,
            _ => return None,
        };
    }
    Some(Chord { key, mods })
}

fn key_named(name: &str) -> Option<Key> {
    const LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";
    const LETTER_KEYS: [Key; 26] = [
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
    const DIGIT_KEYS: [Key; 10] = [
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
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some(i) = LETTERS.find(c) {
            return LETTER_KEYS.get(i).copied();
        }
        if let Some(d) = c.to_digit(10) {
            return DIGIT_KEYS.get(d as usize).copied();
        }
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<usize>().ok()) {
        return F_KEYS.get(n.checked_sub(1)?).copied();
    }
    NAMED
        .iter()
        .find(|(names, _)| names.contains(&name))
        .map(|(_, key)| *key)
}

/// Keys by name, the first being the one [`chord_name`] shows.
const NAMED: &[(&[&str], Key)] = &[
    (&["enter", "return"], Key::Enter),
    (&["tab"], Key::Tab),
    (&["escape", "esc"], Key::Escape),
    (&["space"], Key::Space),
    (&["backspace"], Key::Backspace),
    (&["delete"], Key::Delete),
    (&["insert"], Key::Insert),
    (&["home"], Key::Home),
    (&["end"], Key::End),
    (&["page_up", "pageup"], Key::PageUp),
    (&["page_down", "pagedown"], Key::PageDown),
    (&["left", "arrow_left"], Key::ArrowLeft),
    (&["right", "arrow_right"], Key::ArrowRight),
    (&["up", "arrow_up"], Key::ArrowUp),
    (&["down", "arrow_down"], Key::ArrowDown),
    // `+` shares the `=` key; `+` itself separates chord parts.
    (&["equal", "=", "plus"], Key::Equal),
    (&["minus", "-"], Key::Minus),
    (&["comma", ","], Key::Comma),
    (&["period", "."], Key::Period),
    (&["slash", "/"], Key::Slash),
    (&["semicolon", ";"], Key::Semicolon),
    (&["quote", "apostrophe", "'"], Key::Quote),
    (&["bracket_left", "["], Key::BracketLeft),
    (&["bracket_right", "]"], Key::BracketRight),
    (&["backslash", "\\"], Key::Backslash),
    (&["grave", "backquote", "`"], Key::Backquote),
];

/// `chord` as the config writes it: `ctrl+shift+t`.
pub fn chord_name(chord: Chord) -> String {
    let mut out = String::new();
    for (mods, name) in [
        (Mods::CTRL, "ctrl"),
        (
            Mods::ALT,
            if cfg!(target_os = "macos") {
                "opt"
            } else {
                "alt"
            },
        ),
        (Mods::SHIFT, "shift"),
        (
            Mods::SUPER,
            if cfg!(target_os = "macos") {
                "cmd"
            } else {
                "super"
            },
        ),
    ] {
        if chord.mods.contains(mods) {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(&key_name(chord.key));
    out
}

fn key_name(key: Key) -> String {
    let named = NAMED.iter().find(|(_, k)| *k == key);
    if let Some(name) = named.and_then(|(names, _)| names.first()) {
        return (*name).to_owned();
    }
    let all = "abcdefghijklmnopqrstuvwxyz0123456789"
        .chars()
        .map(String::from)
        .chain((1..=24).map(|n| format!("f{n}")));
    all.into_iter()
        .find(|name| key_named(name) == Some(key))
        .unwrap_or_else(|| format!("{key:?}").to_ascii_lowercase())
}

/// The chord bound to `action`, if any: the newest binding wins a chord.
pub fn chord_for(bindings: &[Binding], action: &Action) -> Option<Chord> {
    bindings
        .iter()
        .filter(|b| b.action.as_ref() == Some(action))
        .map(|b| b.chord)
        .find(|chord| find(bindings, chord.key, chord.mods).flatten().as_ref() == Some(action))
}

fn parse_action(text: &str) -> Option<Action> {
    let (name, arg) = match text.split_once(':') {
        Some((name, arg)) => (name, Some(arg)),
        None => (text, None),
    };
    let direction = |arg: Option<&str>| match arg? {
        "left" => Some(Direction::Left),
        "right" => Some(Direction::Right),
        "up" | "top" => Some(Direction::Up),
        "down" | "bottom" => Some(Direction::Down),
        _ => None,
    };
    let points = |arg: Option<&str>| arg.map_or(Some(1.0), |a| a.parse::<f32>().ok());
    Some(match (name, arg) {
        ("copy_to_clipboard", _) => Action::Copy,
        ("paste_from_clipboard", None) => Action::Paste,
        ("new_tab", profile) => Action::NewTab(profile.map(str::to_owned)),
        ("close_surface", None) => Action::CloseSurface,
        ("close_tab", None) => Action::CloseTab,
        ("next_tab", None) => Action::NextTab,
        ("previous_tab", None) => Action::PreviousTab,
        ("last_tab", None) => Action::LastTab,
        ("goto_tab", Some(n)) => Action::GotoTab(n.parse().ok().filter(|&n| n > 0)?),
        // Ghostty's default direction for a bare new_split.
        ("new_split", None) => Action::NewSplit(Direction::Right, None),
        // `new_split:right:pwsh` opens a profile.
        ("new_split", Some(arg)) => {
            let (side, profile) = match arg.split_once(':') {
                Some((side, profile)) => (side, Some(profile.to_owned())),
                None => (arg, None),
            };
            Action::NewSplit(direction(Some(side))?, profile)
        }
        ("goto_split", Some("previous")) => Action::GotoSplit(SplitTarget::Previous),
        ("goto_split", Some("next")) => Action::GotoSplit(SplitTarget::Next),
        ("goto_split", arg) => Action::GotoSplit(SplitTarget::Toward(direction(arg)?)),
        ("increase_font_size", arg) => Action::FontSize(points(arg)?),
        ("decrease_font_size", arg) => Action::FontSize(-points(arg)?),
        ("reset_font_size", None) => Action::ResetFontSize,
        ("copy_command_output", None) => Action::CopyCommandOutput,
        ("jump_to_prompt", Some(n)) => Action::JumpToPrompt(n.parse().ok().filter(|&n| n != 0)?),
        ("start_search", None) => Action::StartSearch,
        ("end_search", None) => Action::EndSearch,
        ("navigate_search", Some("next")) => Action::NavigateSearch(-1),
        ("navigate_search", Some("previous")) => Action::NavigateSearch(1),
        ("toggle_command_palette", None) => Action::ToggleCommandPalette,
        ("toggle_split_zoom", None) => Action::ToggleSplitZoom,
        ("reload_config", None) => Action::ReloadConfig,
        ("ignore", None) => Action::Ignore,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use jet_core::{Key, Mods};

    use crate::keybinds::{
        Action, DEFAULTS, Direction, MAC_DEFAULTS, chord_for, chord_name, find, resolve,
        resolve_over,
    };

    #[test]
    fn defaults_with_overrides_and_unbinds() -> Result<(), String> {
        let custom = [
            ("ctrl+shift+d", "new_split:down"),
            ("Ctrl+Shift+T", "unbind"),
            ("ctrl+tab", "goto_tab:3"),
        ]
        .map(|(c, a)| (c.to_owned(), a.to_owned()));
        let b = resolve_over(DEFAULTS, true, custom)?;
        let ctrl_shift = Mods::CTRL | Mods::SHIFT;
        assert_eq!(
            find(&b, Key::D, ctrl_shift),
            Some(Some(Action::NewSplit(Direction::Down, None)))
        );
        assert_eq!(find(&b, Key::T, ctrl_shift), Some(None), "unbound");
        assert_eq!(
            find(&b, Key::Tab, Mods::CTRL),
            Some(Some(Action::GotoTab(3)))
        );
        assert_eq!(find(&b, Key::C, ctrl_shift), Some(Some(Action::Copy)));
        assert_eq!(
            find(&b, Key::C, Mods::CTRL),
            None,
            "ctrl+c stays the shell's"
        );
        assert_eq!(
            find(&b, Key::Minus, Mods::CTRL),
            Some(Some(Action::FontSize(-1.0)))
        );
        assert!(resolve(false, [])?.is_empty());
        let zoom = Action::ToggleSplitZoom;
        assert_eq!(
            chord_for(&b, &zoom).map(chord_name).as_deref(),
            Some("ctrl+shift+enter")
        );
        Ok(())
    }

    #[test]
    fn mac_defaults_use_command() -> Result<(), String> {
        let b = resolve_over(MAC_DEFAULTS, true, [])?;
        assert_eq!(find(&b, Key::C, Mods::SUPER), Some(Some(Action::Copy)));
        assert_eq!(
            find(&b, Key::T, Mods::CTRL),
            None,
            "ctrl+t stays the shell's"
        );
        assert_eq!(
            find(&b, Key::D, Mods::SUPER | Mods::SHIFT),
            Some(Some(Action::NewSplit(Direction::Down, None)))
        );
        Ok(())
    }

    #[test]
    fn bad_chords_and_actions_say_which() {
        let err = |c: &str, a: &str| resolve(false, [(c.to_owned(), a.to_owned())]).err();
        assert!(err("ctrl+nope", "new_tab").is_some_and(|e| e.contains("\"ctrl+nope\"")));
        assert!(err("hyper+a", "new_tab").is_some());
        assert!(err("ctrl+a", "launch_rockets").is_some_and(|e| e.contains("launch_rockets")));
        assert!(err("ctrl+a", "goto_tab:0").is_some());
    }
}
