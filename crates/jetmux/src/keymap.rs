//! Modal key layers and their bindings.
//!
//! Normal mode passes keys through. The prefix (ctrl-b by default) arms a
//! one-shot layer; from there `p` and `t` open the sticky pane and tab
//! layers. Copy mode is a layer too, active whenever the focused pane is in
//! copy mode. The config can rebind any key in any layer to a palette
//! command.

use std::collections::HashMap;

use jetmux_proto::{Action, Color, CopyAction, Direction, Motion, PaneSpec, SelectionKind};
use jetmux_render::Theme;

use crate::config::Config;
use crate::keys::Key;

const DEFAULT_PREFIX: Key = Key::Ctrl('b');

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layer {
    Normal,
    Prefix,
    Pane,
    Tab,
    Copy,
}

impl Layer {
    /// The status bar badge and its color.
    pub fn badge(self, theme: &Theme) -> Option<(&'static str, Color)> {
        match self {
            Self::Normal => None,
            Self::Prefix => Some(("PREFIX", theme.prefix)),
            Self::Pane => Some(("PANE", theme.pane)),
            Self::Tab => Some(("TAB", theme.tab)),
            Self::Copy => Some(("COPY", theme.copy)),
        }
    }

    /// What the status bar suggests in this layer: a label and the
    /// commands whose keys go with it.
    fn hints(self) -> &'static [(&'static str, &'static [&'static str])] {
        const FOCUS: &[&str] = &["focus-left", "focus-down", "focus-up", "focus-right"];
        match self {
            Self::Normal => &[],
            Self::Prefix => &[
                ("commands", &["palette"]),
                ("split", &["split-right", "split-down"]),
                ("focus", FOCUS),
                ("new tab", &["new-tab"]),
                ("zoom", &["zoom"]),
                ("close", &["close-pane"]),
                ("detach", &["detach"]),
                ("copy", &["copy-mode"]),
                ("pane mode", &["pane-mode"]),
                ("tab mode", &["tab-mode"]),
                ("next tab", &["next-tab"]),
                ("cancel", &["leave"]),
            ],
            Self::Pane => &[
                ("focus", FOCUS),
                ("swap", &["swap-left", "swap-down", "swap-up", "swap-right"]),
                ("split", &["split-right", "split-down"]),
                ("zoom", &["zoom"]),
                ("close", &["close-pane"]),
                ("rotate", &["rotate"]),
                ("done", &["leave"]),
            ],
            Self::Tab => &[
                ("prev/next", &["prev-tab", "next-tab"]),
                ("new", &["new-tab"]),
                ("close", &["close-tab"]),
                ("rename", &["rename-tab"]),
                ("move", &["move-tab-left", "move-tab-right"]),
                ("done", &["leave"]),
            ],
            Self::Copy => &[
                ("select", &["copy-select", "copy-select-line"]),
                ("yank", &["copy-yank"]),
                ("search", &["search", "search-up"]),
                ("prompts", &["copy-prev-prompt", "copy-next-prompt"]),
                ("failed", &["copy-prev-failure"]),
                ("output", &["copy-yank-output"]),
                ("quit", &["copy-exit"]),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    RenameTab,
    Search { backward: bool },
    Palette,
}

impl PromptKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::RenameTab => "rename tab: ",
            Self::Search { backward: false } => "search down: ",
            Self::Search { backward: true } => "search up: ",
            Self::Palette => ": ",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    Action(Action),
    Detach,
    Layer(Layer),
    /// Back to normal mode.
    Leave,
    Prompt(PromptKind),
    /// Send the prefix key itself to the pane.
    LiteralPrefix,
}

fn copy(action: CopyAction) -> Option<Bound> {
    Some(Bound::Action(Action::Copy(action)))
}

fn motion(m: Motion) -> Option<Bound> {
    copy(CopyAction::Move(m))
}

fn act(a: Action) -> Option<Bound> {
    Some(Bound::Action(a))
}

fn new_tab() -> Action {
    Action::NewTab(PaneSpec::default())
}

fn split(dir: Direction) -> Action {
    Action::Split(dir, PaneSpec::default())
}

fn hjkl(key: Key) -> Option<Direction> {
    match key {
        Key::Char('h') | Key::Left => Some(Direction::Left),
        Key::Char('j') | Key::Down => Some(Direction::Down),
        Key::Char('k') | Key::Up => Some(Direction::Up),
        Key::Char('l') | Key::Right => Some(Direction::Right),
        _ => None,
    }
}

fn shifted_hjkl(key: Key) -> Option<Direction> {
    match key {
        Key::Char('H') => Some(Direction::Left),
        Key::Char('J') => Some(Direction::Down),
        Key::Char('K') => Some(Direction::Up),
        Key::Char('L') => Some(Direction::Right),
        _ => None,
    }
}

fn digit(key: Key) -> Option<u16> {
    match key {
        Key::Char(c @ '1'..='9') => c.to_digit(10).and_then(|d| u16::try_from(d - 1).ok()),
        _ => None,
    }
}

/// The default bindings plus the config's.
#[derive(Debug)]
pub struct Keymap {
    prefix: Option<Key>,
    /// Config bindings; `None` unbinds the key.
    overrides: HashMap<(Layer, Key), Option<Bound>>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            prefix: Some(DEFAULT_PREFIX),
            overrides: HashMap::new(),
        }
    }
}

impl Keymap {
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let prefix = match config.prefix.as_deref() {
            None => Some(DEFAULT_PREFIX),
            Some("none") => None,
            Some(key) => Some(Key::parse(key)?),
        };
        let mut overrides = HashMap::new();
        let keys = &config.keys;
        for (layer, table) in [
            (Layer::Normal, &keys.normal),
            (Layer::Prefix, &keys.prefix),
            (Layer::Pane, &keys.pane),
            (Layer::Tab, &keys.tab),
            (Layer::Copy, &keys.copy),
        ] {
            for (key, name) in table {
                let bound = match name.as_str() {
                    "none" => None,
                    name => Some(command(name).ok_or_else(|| format!("unknown command {name}"))?),
                };
                overrides.insert((layer, Key::parse(key)?), bound);
            }
        }
        Ok(Self { prefix, overrides })
    }

    pub fn prefix(&self) -> Option<Key> {
        self.prefix
    }

    /// The layer's hints with the keys currently bound to them, e.g.
    /// `("hjkl", "focus")`. Hints with an unbound command are left out.
    pub fn hints(&self, layer: Layer) -> Vec<(String, String)> {
        layer
            .hints()
            .iter()
            .filter_map(|(label, names)| {
                let keys = names
                    .iter()
                    .map(|name| self.key_for(layer, &command(name)?))
                    .collect::<Option<Vec<Key>>>()?;
                // hjkl reads as a group; anything else gets a separator.
                let letters = keys.len() > 2
                    && keys
                        .iter()
                        .all(|k| matches!(k, Key::Char(c) if c.is_alphabetic()));
                let keys: Vec<String> = keys.iter().map(Key::to_string).collect();
                let sep = if letters {
                    ""
                } else if keys.iter().any(|k| k == "/") {
                    " "
                } else {
                    "/"
                };
                let keys = keys.join(sep);
                Some((keys, (*label).to_owned()))
            })
            .collect()
    }

    /// The key bound to `bound` in `layer`, preferring letters, then other
    /// printable keys, then modified and named keys.
    fn key_for(&self, layer: Layer, bound: &Bound) -> Option<Key> {
        let chars = ('a'..='z').chain('A'..='Z').chain('!'..='~');
        chars
            .clone()
            .map(Key::Char)
            .chain(('a'..='z').chain(['@', '\\', ']', '^', '_']).map(Key::Ctrl))
            .chain(chars.map(Key::Alt))
            .chain([
                Key::Esc,
                Key::Enter,
                Key::Tab,
                Key::BackTab,
                Key::Char(' '),
                Key::Up,
                Key::Down,
                Key::Left,
                Key::Right,
            ])
            .find(|key| self.lookup(layer, *key).as_ref() == Some(bound))
    }

    pub fn lookup(&self, layer: Layer, key: Key) -> Option<Bound> {
        if let Some(bound) = self.overrides.get(&(layer, key)) {
            return bound.clone();
        }
        if Some(key) == self.prefix {
            return Some(if layer == Layer::Prefix {
                Bound::LiteralPrefix
            } else {
                Bound::Layer(Layer::Prefix)
            });
        }
        match layer {
            Layer::Normal => normal(key),
            Layer::Prefix => prefix(key),
            Layer::Pane => pane(key),
            Layer::Tab => tab(key),
            Layer::Copy => copy_layer(key),
        }
    }
}

/// Prefix-free shortcuts. Alt keeps them clear of most apps.
fn normal(key: Key) -> Option<Bound> {
    match key {
        Key::Alt(c @ ('h' | 'j' | 'k' | 'l')) => act(Action::Focus(hjkl(Key::Char(c))?)),
        Key::Alt(c @ '1'..='9') => act(Action::SelectTab(digit(Key::Char(c))?)),
        _ => None,
    }
}

fn prefix(key: Key) -> Option<Bound> {
    if let Some(dir) = hjkl(key) {
        return act(Action::Focus(dir));
    }
    if let Some(dir) = shifted_hjkl(key) {
        return act(Action::Resize(
            dir,
            if matches!(dir, Direction::Left | Direction::Right) {
                5
            } else {
                2
            },
        ));
    }
    if let Some(i) = digit(key) {
        return act(Action::SelectTab(i));
    }
    match key {
        Key::Char('d') => Some(Bound::Detach),
        Key::Char('c') => act(new_tab()),
        Key::Char('n') => act(Action::NextTab),
        Key::Char('N') => act(Action::PrevTab),
        Key::Char(',') => Some(Bound::Prompt(PromptKind::RenameTab)),
        Key::Char('&') => act(Action::CloseTab),
        Key::Char('%' | '|') => act(split(Direction::Right)),
        Key::Char('"' | '-') => act(split(Direction::Down)),
        Key::Char('x') => act(Action::ClosePane),
        Key::Char('z') => act(Action::ToggleZoom),
        Key::Char('o') => act(Action::FocusNext),
        Key::Char('{') => act(Action::Rotate(false)),
        Key::Char('}') => act(Action::Rotate(true)),
        Key::Char('[') => act(Action::EnterCopyMode),
        Key::Char(']') => act(Action::Paste),
        Key::Char(':' | '?') => Some(Bound::Prompt(PromptKind::Palette)),
        Key::Char('p') => Some(Bound::Layer(Layer::Pane)),
        Key::Char('t') => Some(Bound::Layer(Layer::Tab)),
        Key::Esc => Some(Bound::Leave),
        _ => None,
    }
}

fn pane(key: Key) -> Option<Bound> {
    if let Some(dir) = hjkl(key) {
        return act(Action::Focus(dir));
    }
    if let Some(dir) = shifted_hjkl(key) {
        return act(Action::Swap(dir));
    }
    match key {
        Key::Char('v' | '|') => act(split(Direction::Right)),
        Key::Char('s' | '_') => act(split(Direction::Down)),
        Key::Char('x') => act(Action::ClosePane),
        Key::Char('z') => act(Action::ToggleZoom),
        Key::Char('n') | Key::Tab => act(Action::FocusNext),
        Key::Char('N') | Key::BackTab => act(Action::FocusPrev),
        Key::Char('r') => act(Action::Rotate(true)),
        Key::Char('<') => act(Action::Resize(Direction::Left, 2)),
        Key::Char('>') => act(Action::Resize(Direction::Right, 2)),
        Key::Char('-') => act(Action::Resize(Direction::Up, 1)),
        Key::Char('+' | '=') => act(Action::Resize(Direction::Down, 1)),
        Key::Esc | Key::Enter | Key::Char('q') => Some(Bound::Leave),
        _ => None,
    }
}

fn tab(key: Key) -> Option<Bound> {
    if let Some(i) = digit(key) {
        return act(Action::SelectTab(i));
    }
    match key {
        Key::Char('h' | 'p') | Key::Left => act(Action::PrevTab),
        Key::Char('l' | 'n') | Key::Right | Key::Tab => act(Action::NextTab),
        Key::Char('c') => act(new_tab()),
        Key::Char('x') => act(Action::CloseTab),
        Key::Char('r') => Some(Bound::Prompt(PromptKind::RenameTab)),
        Key::Char('<') => act(Action::MoveTab(-1)),
        Key::Char('>') => act(Action::MoveTab(1)),
        Key::Esc | Key::Enter | Key::Char('q') => Some(Bound::Leave),
        _ => None,
    }
}

fn copy_layer(key: Key) -> Option<Bound> {
    match key {
        Key::Char('h') | Key::Left => motion(Motion::Left),
        Key::Char('j') | Key::Down => motion(Motion::Down),
        Key::Char('k') | Key::Up => motion(Motion::Up),
        Key::Char('l') | Key::Right => motion(Motion::Right),
        Key::Char('w') => motion(Motion::WordForward),
        Key::Char('b') => motion(Motion::WordBackward),
        Key::Char('e') => motion(Motion::WordEnd),
        Key::Char('0') | Key::Home => motion(Motion::LineStart),
        Key::Char('^') => motion(Motion::FirstNonBlank),
        Key::Char('$') | Key::End => motion(Motion::LineEnd),
        Key::Char('g') => motion(Motion::Top),
        Key::Char('G') => motion(Motion::Bottom),
        Key::Char('H') => motion(Motion::ViewTop),
        Key::Char('M') => motion(Motion::ViewMiddle),
        Key::Char('L') => motion(Motion::ViewBottom),
        Key::Ctrl('u') => motion(Motion::HalfPageUp),
        Key::Ctrl('d') => motion(Motion::HalfPageDown),
        Key::PageUp => motion(Motion::PageUp),
        Key::Ctrl('f') | Key::PageDown => motion(Motion::PageDown),
        Key::Ctrl('y') => copy(CopyAction::Scroll(-1)),
        Key::Ctrl('e') => copy(CopyAction::Scroll(1)),
        Key::Char('v') => copy(CopyAction::Select(SelectionKind::Char)),
        Key::Char('V') => copy(CopyAction::Select(SelectionKind::Line)),
        Key::Ctrl('v') => copy(CopyAction::Select(SelectionKind::Block)),
        Key::Char('o') => copy(CopyAction::SwapAnchor),
        Key::Char('y') | Key::Enter => copy(CopyAction::Yank),
        Key::Char('/') => Some(Bound::Prompt(PromptKind::Search { backward: false })),
        Key::Char('?') => Some(Bound::Prompt(PromptKind::Search { backward: true })),
        Key::Char('n') => copy(CopyAction::SearchNext),
        Key::Char('N') => copy(CopyAction::SearchPrev),
        Key::Char('{') => copy(CopyAction::Prompt { backward: true }),
        Key::Char('}') => copy(CopyAction::Prompt { backward: false }),
        Key::Char('F') => copy(CopyAction::PrevFailure),
        Key::Char('Y') => copy(CopyAction::YankOutput),
        Key::Esc => copy(CopyAction::ClearSelection),
        Key::Char('q') => copy(CopyAction::Exit),
        Key::Char(':') => Some(Bound::Prompt(PromptKind::Palette)),
        _ => None,
    }
}

/// Everything the command palette offers.
pub fn commands() -> Vec<(&'static str, Bound)> {
    let a = Bound::Action;
    vec![
        ("new-tab", a(new_tab())),
        ("close-tab", a(Action::CloseTab)),
        ("next-tab", a(Action::NextTab)),
        ("prev-tab", a(Action::PrevTab)),
        ("rename-tab", Bound::Prompt(PromptKind::RenameTab)),
        ("move-tab-left", a(Action::MoveTab(-1))),
        ("move-tab-right", a(Action::MoveTab(1))),
        ("split-right", a(split(Direction::Right))),
        ("split-down", a(split(Direction::Down))),
        ("split-left", a(split(Direction::Left))),
        ("split-up", a(split(Direction::Up))),
        ("close-pane", a(Action::ClosePane)),
        ("zoom", a(Action::ToggleZoom)),
        ("focus-next", a(Action::FocusNext)),
        ("focus-prev", a(Action::FocusPrev)),
        ("focus-left", a(Action::Focus(Direction::Left))),
        ("focus-right", a(Action::Focus(Direction::Right))),
        ("focus-up", a(Action::Focus(Direction::Up))),
        ("focus-down", a(Action::Focus(Direction::Down))),
        ("swap-left", a(Action::Swap(Direction::Left))),
        ("swap-right", a(Action::Swap(Direction::Right))),
        ("swap-up", a(Action::Swap(Direction::Up))),
        ("swap-down", a(Action::Swap(Direction::Down))),
        ("resize-left", a(Action::Resize(Direction::Left, 5))),
        ("resize-right", a(Action::Resize(Direction::Right, 5))),
        ("resize-up", a(Action::Resize(Direction::Up, 2))),
        ("resize-down", a(Action::Resize(Direction::Down, 2))),
        ("rotate", a(Action::Rotate(true))),
        ("copy-mode", a(Action::EnterCopyMode)),
        (
            "search",
            Bound::Prompt(PromptKind::Search { backward: false }),
        ),
        (
            "search-up",
            Bound::Prompt(PromptKind::Search { backward: true }),
        ),
        ("paste", a(Action::Paste)),
        ("pane-mode", Bound::Layer(Layer::Pane)),
        ("tab-mode", Bound::Layer(Layer::Tab)),
        ("detach", Bound::Detach),
        ("send-prefix", Bound::LiteralPrefix),
        ("palette", Bound::Prompt(PromptKind::Palette)),
        ("leave", Bound::Leave),
    ]
}

/// A command by exact name: a palette entry, `select-tab-N` or a copy-mode
/// action.
pub fn command(name: &str) -> Option<Bound> {
    if let Some(n) = name.strip_prefix("select-tab-") {
        let n: u16 = n.parse().ok()?;
        return act(Action::SelectTab(n.checked_sub(1)?));
    }
    let copy_command = match name {
        "copy-left" => motion(Motion::Left),
        "copy-down" => motion(Motion::Down),
        "copy-up" => motion(Motion::Up),
        "copy-right" => motion(Motion::Right),
        "copy-select" => copy(CopyAction::Select(SelectionKind::Char)),
        "copy-select-line" => copy(CopyAction::Select(SelectionKind::Line)),
        "copy-select-block" => copy(CopyAction::Select(SelectionKind::Block)),
        "copy-yank" => copy(CopyAction::Yank),
        "copy-exit" => copy(CopyAction::Exit),
        "search-next" => copy(CopyAction::SearchNext),
        "search-prev" => copy(CopyAction::SearchPrev),
        "copy-prev-prompt" => copy(CopyAction::Prompt { backward: true }),
        "copy-next-prompt" => copy(CopyAction::Prompt { backward: false }),
        "copy-prev-failure" => copy(CopyAction::PrevFailure),
        "copy-yank-output" => copy(CopyAction::YankOutput),
        _ => None,
    };
    copy_command.or_else(|| {
        commands()
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, bound)| bound)
    })
}

/// Palette entries matching `query` as a subsequence, best first.
pub fn matches(query: &str) -> Vec<(&'static str, Bound)> {
    if query.is_empty() {
        return commands();
    }
    let mut found: Vec<_> = commands()
        .into_iter()
        .filter_map(|(name, bound)| Some((score(name, query)?, name, bound)))
        .collect();
    // Exact prefixes first, then tighter matches, then names starting with
    // the query's first letter, then shorter names.
    let first = query.chars().next();
    found.sort_by_key(|(score, name, _)| {
        (
            !name.starts_with(query),
            std::cmp::Reverse(*score),
            name.chars().next() != first,
            name.len(),
        )
    });
    found
        .into_iter()
        .map(|(_, name, bound)| (name, bound))
        .collect()
}

/// How well `query` matches `name` as a subsequence: the number of query
/// chars that land on a word start or right after the previous match.
/// `None` when it doesn't match at all.
fn score(name: &str, query: &str) -> Option<usize> {
    let chars: Vec<char> = name.chars().collect();
    let mut at = 0;
    let mut last: Option<usize> = None;
    let mut score = 0;
    for q in query.chars() {
        let i = at + chars[at..].iter().position(|c| *c == q)?;
        if i == 0 || chars[i - 1] == '-' || last.is_some_and(|l| l + 1 == i) {
            score += 1;
        }
        last = Some(i);
        at = i + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use jetmux_proto::{Action, Direction};

    use crate::config::Config;
    use crate::keymap::{Bound, Keymap, Layer, matches, split};
    use crate::keys::Key;

    #[test]
    fn layers_resolve_bindings() {
        let keymap = Keymap::default();
        let lookup = |layer, key| keymap.lookup(layer, key);
        assert_eq!(lookup(Layer::Normal, Key::Char('x')), None);
        assert_eq!(
            lookup(Layer::Normal, Key::Ctrl('b')),
            Some(Bound::Layer(Layer::Prefix))
        );
        assert_eq!(
            lookup(Layer::Prefix, Key::Char('%')),
            Some(Bound::Action(split(Direction::Right)))
        );
        assert_eq!(
            lookup(Layer::Prefix, Key::Ctrl('b')),
            Some(Bound::LiteralPrefix)
        );
        assert_eq!(
            lookup(Layer::Pane, Key::Char('L')),
            Some(Bound::Action(Action::Swap(Direction::Right)))
        );
        assert_eq!(
            lookup(Layer::Tab, Key::Char('3')),
            Some(Bound::Action(Action::SelectTab(2)))
        );
    }

    #[test]
    fn hints_follow_bindings() {
        let hint = |keymap: &Keymap, layer, label: &str| {
            keymap
                .hints(layer)
                .into_iter()
                .find(|(_, l)| l == label)
                .map(|(keys, _)| keys)
        };
        let keymap = Keymap::default();
        assert_eq!(
            hint(&keymap, Layer::Prefix, "focus").as_deref(),
            Some("hjkl")
        );
        assert_eq!(
            hint(&keymap, Layer::Prefix, "split").as_deref(),
            Some("%/\"")
        );
        assert_eq!(
            hint(&keymap, Layer::Prefix, "cancel").as_deref(),
            Some("Esc")
        );
        assert_eq!(hint(&keymap, Layer::Copy, "search").as_deref(), Some("/ ?"));

        let config: Config = toml::from_str(
            r#"
[keys.prefix]
"s" = "split-down"
"x" = "none"
"#,
        )
        .unwrap();
        let keymap = Keymap::from_config(&config).unwrap();
        assert_eq!(
            hint(&keymap, Layer::Prefix, "split").as_deref(),
            Some("%/s")
        );
        assert_eq!(hint(&keymap, Layer::Prefix, "close"), None);
    }

    #[test]
    fn config_rebinds_keys_and_prefix() {
        let config: Config = toml::from_str(
            r#"
prefix = "C-a"
[keys.normal]
"M-n" = "new-tab"
"M-h" = "none"
[keys.prefix]
"v" = "split-right"
"#,
        )
        .unwrap();
        let keymap = Keymap::from_config(&config).unwrap();
        assert_eq!(
            keymap.lookup(Layer::Normal, Key::Ctrl('a')),
            Some(Bound::Layer(Layer::Prefix))
        );
        assert_eq!(keymap.lookup(Layer::Normal, Key::Ctrl('b')), None);
        assert_eq!(keymap.lookup(Layer::Normal, Key::Alt('h')), None);
        assert!(matches!(
            keymap.lookup(Layer::Normal, Key::Alt('n')),
            Some(Bound::Action(Action::NewTab(_)))
        ));
        assert_eq!(
            keymap.lookup(Layer::Prefix, Key::Char('v')),
            Some(Bound::Action(split(Direction::Right)))
        );
        let bad: Config = toml::from_str("[keys.normal]\n\"M-x\" = \"explode\"").unwrap();
        assert!(Keymap::from_config(&bad).is_err());
    }

    #[test]
    fn palette_matches_subsequences() {
        // Prefix matches rank first; other subsequence matches follow.
        let names: Vec<_> = matches("spr").into_iter().map(|(n, _)| n).collect();
        assert_eq!(names[0], "split-right");
        assert!(names.contains(&"focus-prev"));
        assert_eq!(matches("zoom")[0].0, "zoom");
        assert_eq!(matches("")[0].0, "new-tab");
    }
}
