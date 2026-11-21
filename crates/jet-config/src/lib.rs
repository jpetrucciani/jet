//! jet's settings file: TOML, found at the first of `$JET_CONFIG`,
//! `~/.jet/config.toml` (every platform) and the platform's config dir
//! (`%APPDATA%\jet\config.toml`, `$XDG_CONFIG_HOME/jet/config.toml` or
//! `~/.config/jet/config.toml`). Every key is optional.
//!
//! Layers, lowest first: an imported Ghostty config, inline `[ghostty]`
//! keys, then jet's own keys. Ghostty keys are translated into jet's
//! layout and the tables merged. A `[theme]` supplies colors beneath
//! whatever `[colors]` sets.

mod ghostty;
pub mod keybinds;
pub mod plumb;
pub mod status;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use toml::{Table, Value};

use jet_core::{CursorShape, Rgb, VtSettings};

use crate::status::Status;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: Font,
    pub window: Window,
    #[serde(rename = "tab-bar")]
    pub tab_bar: TabBar,
    pub shell: Shell,
    pub terminal: Terminal,
    pub cursor: Cursor,
    pub colors: Colors,
    pub theme: Theme,
    /// `"ctrl+shift+t" = "new_tab"`, `"ctrl+a" = "unbind"`; `"defaults" =
    /// false` drops the built-in ones.
    pub keybinds: BTreeMap<String, Bind>,
    /// `[profiles.pwsh]`: tabs to open by name from a keybind.
    pub profiles: BTreeMap<String, Profile>,
    pub quake: Quake,
    pub status: Status,
    /// `[plumb.editor]`: text that does something when Ctrl+clicked.
    pub plumb: BTreeMap<String, plumb::Plumb>,
    /// `[hosts.prod]`: how panes on a host matching its regex look.
    pub hosts: BTreeMap<String, Host>,
    /// jetmux's settings, which jetmux reads (with its own file on top).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jetmux: Option<Table>,
    /// Consumed while layering; here so its presence validates.
    #[serde(skip_serializing)]
    ghostty: Option<Table>,
    /// More jet configs layered on top of this one, in order: relative to
    /// this file or `~/`. Missing ones are skipped (and watched).
    #[serde(skip_serializing)]
    include: Vec<String>,
}

/// `jet quake`: a window a global hotkey shows and hides. `[quake.NAME]`
/// tables make several, each with `[quake]` under what it sets.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Quake {
    /// Chords in keybind syntax, e.g. `"ctrl+grave"`. Windows sharing one
    /// show and hide together.
    pub hotkey: Chords,
    /// Windows: catch the hotkey with a low-level keyboard hook too, so it
    /// works over games that turn hotkeys off while in front.
    pub hotkey_hook: bool,
    pub monitor: Monitor,
    /// Where on the monitor, as fractions of its work area (the screen
    /// less the taskbar): the left third is `width = 0.333`.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Hide when another window takes the focus.
    pub hide_on_blur: bool,
    pub always_on_top: bool,
    /// Show in the taskbar and Alt+Tab while visible.
    pub taskbar: bool,
    /// Windows: place by the whole monitor, covering the taskbar, not its
    /// work area.
    pub over_taskbar: bool,
    /// The profile its first tab opens; unset: `[shell]`'s.
    pub profile: Option<String>,
    /// Windows and macOS: keep `jet quake` running, started at sign-in and by any jet
    /// window that finds it isn't.
    pub autostart: bool,
    /// `[quake.NAME]`: the quake windows, when more than `[quake]` itself.
    /// Unknown keys land here too, refused for not being tables.
    #[serde(flatten)]
    pub windows: BTreeMap<String, QuakeTable>,
}

/// Settings only `[quake]` has, for every quake window at once.
const QUAKE_ONLY: [&str; 2] = ["autostart", "hotkey-hook"];

impl Quake {
    /// The quake windows, in name order: each `[quake.NAME]` over
    /// `[quake]`, else `[quake]` alone, unnamed.
    pub fn resolve(&self) -> Result<Vec<(Option<String>, Quake)>, String> {
        if let Some(key) = self
            .windows
            .iter()
            .find_map(|(k, v)| (!v.0.is_table()).then_some(k))
        {
            return Err(format!("quake.{key} isn't a quake setting"));
        }
        if self.windows.is_empty() {
            return Ok(vec![(None, self.clone())]);
        }
        let base = Quake {
            windows: BTreeMap::new(),
            ..self.clone()
        };
        let base = Table::try_from(&base).map_err(|e| format!("quake: {e}"))?;
        let mut windows = Vec::new();
        for (name, QuakeTable(own)) in &self.windows {
            let at = |what: String| format!("quake.{name}: {what}");
            let Some(own) = own.as_table() else {
                continue;
            };
            if let Some(key) = own.keys().find(|k| QUAKE_ONLY.contains(&k.as_str())) {
                return Err(at(format!("{key} is for [quake], every window at once")));
            }
            if let Some(key) = own.iter().find_map(|(k, v)| v.is_table().then_some(k)) {
                return Err(at(format!("{key} isn't a quake setting")));
            }
            let mut table = base.clone();
            table.extend(own.clone());
            let window: Quake = Value::Table(table)
                .try_into()
                .map_err(|e: toml::de::Error| at(e.message().to_owned()))?;
            windows.push((Some(name.clone()), window));
        }
        Ok(windows)
    }

    /// Window `name`'s settings, or with `None` the first window's; `None`
    /// if there's no such window (or the config doesn't resolve, which
    /// loading already refused).
    pub fn window(&self, name: Option<&str>) -> Option<Quake> {
        let windows = self.resolve().ok()?;
        match name {
            None => windows.into_iter().next().map(|(_, q)| q),
            Some(name) => windows
                .into_iter()
                .find(|(n, _)| n.as_deref() == Some(name))
                .map(|(_, q)| q),
        }
    }
}

/// A `[quake.NAME]` table as written: `[quake]` fills in the rest. Any
/// other value is a key `[quake]` doesn't have, refused by
/// [`Quake::resolve`] with its name (here it would come without).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(transparent)]
pub struct QuakeTable(pub Value);

/// One chord or several: `"ctrl+space"` or `["ctrl+space", "ctrl+alt+1"]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chords(pub Vec<String>);

impl From<&str> for Chords {
    fn from(chord: &str) -> Self {
        Self(vec![chord.to_owned()])
    }
}

impl Serialize for Chords {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0.as_slice() {
            [one] => s.serialize_str(one),
            many => many.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Chords {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }
        Ok(Self(match OneOrMany::deserialize(d)? {
            OneOrMany::One(one) => vec![one],
            OneOrMany::Many(many) => many,
        }))
    }
}

impl Default for Quake {
    fn default() -> Self {
        Self {
            // Ctrl+Space switches input sources on macOS.
            hotkey: if cfg!(target_os = "macos") {
                "ctrl+grave"
            } else {
                "ctrl+space"
            }
            .into(),
            hotkey_hook: false,
            monitor: Monitor::Cursor,
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 0.45,
            hide_on_blur: true,
            always_on_top: true,
            taskbar: false,
            over_taskbar: false,
            profile: None,
            autostart: true,
            windows: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Monitor {
    /// The one under the mouse when shown.
    Cursor,
    Primary,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Bind {
    Action(String),
    /// Only for `defaults`.
    Flag(bool),
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    /// A Ghostty theme file: a path, or a name looked up in the `themes`
    /// dir beside jet's or Ghostty's config.
    pub name: Option<String>,
    /// In place of `name` while the system is in light mode, or dark.
    pub light: Option<String>,
    pub dark: Option<String>,
}

impl Theme {
    /// The key whose theme is in effect in `appearance`: the one to
    /// change for it.
    pub fn key(&self, appearance: Appearance) -> &'static str {
        match appearance {
            Appearance::Light if self.light.is_some() => "light",
            Appearance::Dark if self.dark.is_some() => "dark",
            _ => "name",
        }
    }
}

/// The system's light or dark mode, which picks `[theme] light` or
/// `dark`. Dark where the system doesn't say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Font {
    /// An installed family; the bundled JetBrains Mono when unset or not
    /// found.
    pub family: Option<String>,
    /// Points.
    pub size: f32,
    /// OpenType features on top of the defaults, e.g. `"-calt"`, `"ss01"`.
    pub features: Vec<String>,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: None,
            size: 12.0,
            features: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Window {
    /// Space around the grid, in DIPs (pixels at 100% scaling).
    pub padding: u32,
    /// Initial size in cells.
    pub columns: u16,
    pub rows: u16,
    /// Of the default background only; text and colored cells stay solid.
    /// Below 1 the backdrop (or the desktop) shows through.
    pub opacity: f32,
    pub backdrop: Backdrop,
    pub corners: Corners,
    /// The caption's color: the terminal background by default.
    pub titlebar_color: FrameColor,
    /// The 1px window border's color.
    pub border_color: FrameColor,
    /// Dark or light caption, buttons and backdrop; `auto` follows the
    /// background.
    pub color_scheme: ColorScheme,
    pub decorations: Decorations,
    /// Show progress reports (OSC 9;4) on the taskbar button.
    pub taskbar_progress: bool,
    /// Present without waiting for vblank, paced by the monitor during
    /// floods: a keystroke shows ~3 ms sooner, and a frame may tear.
    pub low_latency: bool,
    /// Open the tabs, splits and directories the last window closed with.
    pub restore: bool,
    /// The profile a new window's first tab runs; unset: `[shell] profile`.
    pub profile: Option<String>,
    /// Ask before closing a tab, split or window with something running in
    /// it besides the shell.
    pub confirm_close: bool,
    /// How much of themselves splits without the keyboard show: 1 doesn't
    /// fade them.
    pub unfocused_split_opacity: f32,
}

/// Who draws the title bar.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decorations {
    /// Windows' own caption.
    System,
    /// jet's: the tabs (when on top) or the title, then the window buttons,
    /// in the terminal's colors.
    Custom,
    /// No title bar; the window still resizes, and drags by the tab bar.
    None,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            padding: 4,
            columns: 100,
            rows: 30,
            opacity: 1.0,
            backdrop: Backdrop::None,
            corners: Corners::Default,
            titlebar_color: FrameColor::Background,
            border_color: FrameColor::System,
            color_scheme: ColorScheme::Auto,
            decorations: Decorations::System,
            taskbar_progress: true,
            low_latency: false,
            restore: false,
            profile: None,
            confirm_close: true,
            unfocused_split_opacity: 0.7,
        }
    }
}

/// What Windows draws behind a transparent background.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backdrop {
    /// The desktop and windows behind, unblurred.
    None,
    /// Tinted wallpaper, as in Windows' own apps.
    Mica,
    /// Blurred windows behind.
    Acrylic,
    /// Mica with more of the wallpaper's color.
    Tabbed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Corners {
    /// Windows' choice: round unless maximized or snapped.
    Default,
    Round,
    Small,
    Square,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorScheme {
    Auto,
    Dark,
    Light,
}

/// A color for part of the window frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameColor {
    /// Windows' own (the accent color, if the user turned that on).
    System,
    /// No border at all (borders only).
    None,
    /// The terminal's background.
    Background,
    Color(Rgb),
}

impl Serialize for FrameColor {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::System => s.serialize_str("system"),
            Self::None => s.serialize_str("none"),
            Self::Background => s.serialize_str("background"),
            Self::Color(rgb) => Color(*rgb).serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for FrameColor {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Ok(match text.as_str() {
            "system" => Self::System,
            "none" => Self::None,
            "background" => Self::Background,
            _ => Self::Color(parse_color(&text).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "{text:?} is not system, none, background or a #rrggbb color"
                ))
            })?),
        })
    }
}

/// `[tab-bar]`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct TabBar {
    pub show: TabBarShow,
    pub position: TabBarPosition,
    /// Cells across, when on the left or right.
    pub width: u16,
    /// Unset colors come from the terminal's.
    pub background: Option<Color>,
    pub foreground: Option<Color>,
    pub active_background: Option<Color>,
    pub active_foreground: Option<Color>,
    /// How a profile's `tab-color` shows.
    pub color_style: TabColorStyle,
}

/// How a tab shows its profile's `tab-color`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TabColorStyle {
    /// The whole tab, dimmer when it isn't the active one.
    #[default]
    Fill,
    /// A line along that edge of the tab.
    Top,
    Bottom,
    Left,
    Right,
    /// A dot before the title.
    Dot,
    /// The title's text.
    Text,
}

impl Default for TabBar {
    fn default() -> Self {
        Self {
            show: TabBarShow::Auto,
            position: TabBarPosition::Top,
            width: 24,
            background: None,
            foreground: None,
            active_background: None,
            active_foreground: None,
            color_style: TabColorStyle::Fill,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TabBarShow {
    /// With two tabs or more.
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TabBarPosition {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Shell {
    /// `"pwsh -NoLogo"`, or `["C:/Program Files/Git/bin/bash.exe", "-l"]`.
    /// Unset: the first of pwsh, Windows PowerShell and cmd that starts.
    pub command: Option<Command>,
    /// Shown before its tabs' titles.
    pub icon: Option<Icon>,
    /// bash, zsh and PowerShell load jet's prompt marks and directory
    /// reports on their own.
    pub integration: bool,
    /// A new tab or split from a pane running `ssh` runs it again (the
    /// system's, with the same options), in the directory the remote shell
    /// last reported. Likewise `wsl.exe`, with `--cd`.
    pub follow_ssh: bool,
    /// The profile new tabs and splits run when none is named; unset: the
    /// command above.
    pub profile: Option<String>,
}

impl Default for Shell {
    fn default() -> Self {
        Self {
            command: None,
            icon: None,
            integration: true,
            follow_ssh: false,
            profile: None,
        }
    }
}

/// A tab's icon, from one string: an emoji or any text, an `.svg` file,
/// or an SVG written out in the config (`'<svg ...>...</svg>'`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(from = "String", into = "String")]
pub enum Icon {
    Text(String),
    File(PathBuf),
    Svg(String),
}

impl From<String> for Icon {
    fn from(s: String) -> Self {
        let start = s.trim_start();
        if start.starts_with("<svg") || start.starts_with("<?xml") {
            Self::Svg(s)
        } else if s.to_ascii_lowercase().ends_with(".svg") {
            Self::File(PathBuf::from(s))
        } else {
            Self::Text(s)
        }
    }
}

impl From<Icon> for String {
    fn from(icon: Icon) -> Self {
        match icon {
            Icon::Text(s) | Icon::Svg(s) => s,
            Icon::File(path) => path.to_string_lossy().into_owned(),
        }
    }
}

/// A kind of tab to open by name: `new_tab:pwsh`, `new_split:right:pwsh`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Profile {
    /// Unset: the default shell's.
    pub command: Option<Command>,
    /// Shown before its tabs' titles.
    pub icon: Option<Icon>,
    /// Its panes' theme in place of `[theme]`'s, by name like that one.
    /// Loading folds it into `colors`.
    pub theme: Option<String>,
    /// Its panes' colors, over `theme`'s and then `[colors]`.
    pub colors: Colors,
    /// Its tabs' color in the tab bar, shown per `color_style`.
    pub tab_color: Option<Color>,
    /// How `tab_color` shows; unset: `[tab-bar] color-style`.
    pub color_style: Option<TabColorStyle>,
    /// Start here instead of where the focused shell is.
    pub cwd: Option<PathBuf>,
    /// Show a jetmux session instead of running `command`: its tabs
    /// become tabs here, its splits drawn as jet's.
    pub jetmux: Option<Jetmux>,
    /// Its panes' bell and notification sounds, over `[terminal]`'s.
    pub bell_sound: Option<PathBuf>,
    pub notification_sound: Option<PathBuf>,
}

/// A look for panes whose host (the status's `{host}`: where the shell
/// says it is, else the `ssh` in front, else this machine) matches.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Host {
    #[serde(rename = "match")]
    pub pattern: plumb::Pattern,
    /// As a profile's: over the pane's profile's look.
    pub theme: Option<String>,
    pub colors: Colors,
    pub tab_color: Option<Color>,
    pub color_style: Option<TabColorStyle>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Jetmux {
    /// Through `ssh -T host`; unset: this machine's daemon (not on
    /// Windows).
    pub host: Option<String>,
    pub session: String,
    /// The host's jetmux profile to start the session from when it isn't
    /// running; unset: a plain shell.
    pub up: Option<String>,
    /// jetmux's path on the host, for when a non-interactive shell's PATH
    /// lacks it; unset: found on PATH.
    pub program: Option<String>,
}

impl Jetmux {
    /// What runs `jetmux proxy` for this session.
    pub fn command(&self) -> Vec<String> {
        let program = self.program.clone().unwrap_or_else(|| "jetmux".into());
        let mut argv: Vec<String> = match &self.host {
            Some(host) => vec!["ssh".into(), "-T".into(), host.clone(), program],
            None => vec![program],
        };
        argv.push("proxy".into());
        if let Some(up) = &self.up {
            argv.extend(["-s".into(), self.session.clone(), "--up".into(), up.clone()]);
        }
        argv
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Command {
    /// Split on whitespace; double quotes group (backslashes stay literal,
    /// so Windows paths need no escaping).
    Line(String),
    Argv(Vec<String>),
}

impl Command {
    pub fn argv(&self) -> Vec<String> {
        match self {
            Self::Argv(argv) => argv.clone(),
            Self::Line(line) => split_command(line),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Terminal {
    /// Lines of history.
    pub scrollback: usize,
    pub copy_on_select: bool,
    /// Programs may set the clipboard (OSC 52), as tmux and Neovim do over
    /// SSH. They can never read it.
    pub clipboard_write: bool,
    /// Show programs' desktop notifications (OSC 9, OSC 777), unless they
    /// come from the pane you're looking at.
    pub notifications: bool,
    /// Ask before pasting text with a newline into a program that would
    /// run it (one without bracketed paste, like a plain shell prompt).
    pub paste_protection: bool,
    /// What the bell (`\a`) does; `[]` ignores it.
    pub bell: Vec<Bell>,
    /// A sound file the bell plays (wav, mp3, ogg or flac): setting it
    /// turns `sound` on. Missing or unplayable, the system alert plays.
    pub bell_sound: Option<PathBuf>,
    /// A sound file desktop notifications (OSC 9, OSC 777) play, whether
    /// or not `notifications` shows them, when they'd show.
    pub notification_sound: Option<PathBuf>,
    /// Those sounds' volume, 0 to 1.
    pub sound_volume: f32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Bell {
    /// Flash the window, when it's in front.
    Flash,
    /// Flash the taskbar button (bounce the Dock icon), when it isn't.
    Attention,
    /// `bell-sound`, else the system's alert sound (Windows only).
    Sound,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            scrollback: 10_000,
            copy_on_select: true,
            clipboard_write: true,
            notifications: true,
            paste_protection: true,
            bell: vec![Bell::Flash, Bell::Attention],
            bell_sound: None,
            notification_sound: None,
            sound_volume: 1.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cursor {
    pub style: CursorStyle,
    pub blink: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            style: CursorStyle::Block,
            blink: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Colors {
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub cursor: Option<Color>,
    /// Selected text's cells; unset tints them and keeps the text's color.
    pub selection_background: Option<Color>,
    pub selection_foreground: Option<Color>,
    /// `{ 0 = "#1d1f21", 8 = "#666666" }`: entries of the 256-color
    /// palette to replace.
    pub palette: BTreeMap<String, Color>,
}

impl Colors {
    /// These, with `base`'s where these are unset.
    pub fn over(&self, base: &Colors) -> Colors {
        let mut palette = base.palette.clone();
        palette.extend(self.palette.clone());
        Colors {
            foreground: self.foreground.or(base.foreground),
            background: self.background.or(base.background),
            cursor: self.cursor.or(base.cursor),
            selection_background: self.selection_background.or(base.selection_background),
            selection_foreground: self.selection_foreground.or(base.selection_foreground),
            palette,
        }
    }
}

/// `"#rrggbb"`, `"rrggbb"` or `"#rgb"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub Rgb);

impl Serialize for Color {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let Rgb { r, g, b } = self.0;
        s.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}"))
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        parse_color(&text)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom(format!("{text:?} is not a #rrggbb color")))
    }
}

/// `#rrggbb`, `rrggbb` or `#rgb`.
pub fn parse_color(text: &str) -> Option<Rgb> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    let digit = |i: usize| hex.get(i..=i).and_then(|d| u8::from_str_radix(d, 16).ok());
    let byte = |i: usize| {
        hex.get(i..i + 2)
            .and_then(|d| u8::from_str_radix(d, 16).ok())
    };
    let (r, g, b) = match hex.len() {
        3 => (digit(0)? * 17, digit(1)? * 17, digit(2)? * 17),
        6 => (byte(0)?, byte(2)?, byte(4)?),
        _ => return None,
    };
    Some(Rgb { r, g, b })
}

/// Why a config didn't load, ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.trim_end())
    }
}

impl std::error::Error for Error {}

/// A resolved config, what in it was ignored, and every file it came from
/// (to watch for edits).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
    pub sources: Vec<PathBuf>,
    /// Includes that weren't there: watched so creating one reloads.
    pub absent: Vec<PathBuf>,
}

/// Where configs and themes live. From the environment in the app; tests
/// point it at scratch dirs.
#[derive(Clone, Debug, Default)]
pub struct Dirs {
    /// `$JET_CONFIG`.
    pub explicit: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// `%APPDATA%`, or `$XDG_CONFIG_HOME` / `~/.config`.
    pub platform: Option<PathBuf>,
    /// Where Ghostty keeps its config and themes.
    pub ghostty: Vec<PathBuf>,
    /// Which of `[theme] light` and `dark` to load.
    pub appearance: Appearance,
}

impl Dirs {
    pub fn from_env() -> Self {
        let var = |name| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME").or_else(|| var("USERPROFILE"));
        let xdg = var("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config")));
        let platform = if cfg!(windows) {
            var("APPDATA")
        } else {
            xdg.clone()
        };
        let mut ghostty: Vec<PathBuf> = xdg.into_iter().map(|d| d.join("ghostty")).collect();
        if cfg!(windows) {
            ghostty.extend(var("LOCALAPPDATA").map(|d| d.join("ghostty")));
        }
        if cfg!(target_os = "macos") {
            let support = "Library/Application Support/com.mitchellh.ghostty";
            ghostty.extend(home.as_ref().map(|h| h.join(support)));
        }
        Self {
            explicit: var("JET_CONFIG"),
            home,
            platform,
            ghostty,
            appearance: Appearance::default(),
        }
    }

    /// The config file to edit: the one in use, else where a new one goes
    /// ($JET_CONFIG, else the platform's directory).
    pub fn main_path(&self) -> Option<PathBuf> {
        let used = self.candidates().into_iter().find(|p| p.is_file());
        let platform = self
            .platform
            .as_ref()
            .map(|d| d.join("jet").join("config.toml"));
        let home = self
            .home
            .as_ref()
            .map(|h| h.join(".jet").join("config.toml"));
        used.or_else(|| self.explicit.clone()).or(platform).or(home)
    }

    /// The built-in themes and those in themes/ directories, by name,
    /// sorted without regard to case.
    pub fn themes(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .theme_dirs()
            .iter()
            .filter_map(|d| std::fs::read_dir(d).ok())
            .flatten()
            .filter_map(|e| {
                let file = e.ok()?;
                file.file_type().ok()?.is_file().then_some(())?;
                file.file_name().into_string().ok()
            })
            .chain(bundled_themes().map(|(name, _)| name.to_owned()))
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        names
    }

    /// jet config files, in the order they're tried.
    pub fn candidates(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.explicit.iter().cloned().collect();
        paths.extend(self.jet_dirs().into_iter().map(|d| d.join("config.toml")));
        paths
    }

    fn jet_dirs(&self) -> Vec<PathBuf> {
        let home = self.home.iter().map(|h| h.join(".jet"));
        home.chain(self.platform.iter().map(|d| d.join("jet")))
            .collect()
    }

    /// Ghostty configs imported when `ghostty.config-file` is unset.
    fn ghostty_files(&self) -> Vec<PathBuf> {
        let names = ["config.ghostty", "config"];
        let files = self.ghostty.iter().map(|d| names.map(|n| d.join(n)));
        files.flatten().collect()
    }

    fn theme_dirs(&self) -> Vec<PathBuf> {
        let dirs = self
            .jet_dirs()
            .into_iter()
            .chain(self.ghostty.iter().cloned());
        dirs.map(|d| d.join("themes")).collect()
    }

    /// Files whose appearance or edits should trigger a reload, besides
    /// a loaded config's own sources.
    pub fn watched(&self) -> Vec<PathBuf> {
        let mut paths = self.candidates();
        paths.extend(self.ghostty_files());
        paths
    }

    fn expand(&self, path: &str, relative_to: Option<&Path>) -> PathBuf {
        match (path.strip_prefix("~/"), &self.home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => match relative_to {
                Some(dir) => dir.join(path),
                None => PathBuf::from(path),
            },
        }
    }
}

/// Modification times of `paths` (`None` for missing): changes mean reload.
pub fn stamps(paths: &[PathBuf]) -> Vec<Option<SystemTime>> {
    let modified = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    paths.iter().map(modified).collect()
}

/// Load the first jet config found, with everything it layers in.
pub fn load(dirs: &Dirs) -> Result<Loaded, Error> {
    let Some(path) = dirs.candidates().into_iter().find(|p| p.is_file()) else {
        return resolve(None, None, dirs);
    };
    let text = read(&path)?;
    resolve(Some((&path, &text)), None, dirs)
}

/// [`load`] with theme `name` in place of the configured one, to preview.
pub fn load_with_theme(dirs: &Dirs, name: &str) -> Result<Loaded, Error> {
    let main = dirs.candidates().into_iter().find(|p| p.is_file());
    let text = main.as_ref().map(|p| read(p)).transpose()?;
    // Whichever key is in effect.
    let mut theme = Table::new();
    for key in ["name", "light", "dark"] {
        theme.insert(key.into(), Value::String(name.to_owned()));
    }
    let mut top = Table::new();
    top.insert("theme".into(), Value::Table(theme));
    let preview = Some((Path::new("theme preview"), top));
    resolve(main.as_deref().zip(text.as_deref()), preview, dirs)
}

/// Config `text` with its `[theme]` `key` (name, light or dark) set to
/// `name`, the rest as it was; `None` when that can't be done by line (an
/// inline `theme = {...}`).
pub fn with_theme(text: &str, key: &str, name: &str) -> Option<String> {
    let line = format!("{key} = {}", Value::String(name.to_owned()));
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let key_of = |l: &str| {
        l.split('=')
            .next()
            .map(str::trim)
            .unwrap_or_default()
            .to_owned()
    };
    let mut out: Vec<String> = Vec::new();
    let (mut table, mut header, mut done) = (String::new(), None, false);
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            table = t.trim_matches(|c| c == '[' || c == ']').trim().to_owned();
            out.push(l.to_owned());
            if t == "[theme]" {
                header = Some(out.len());
            }
            continue;
        }
        let ours = (table == "theme" && key_of(t) == key)
            || (table.is_empty() && key_of(t) == format!("theme.{key}"));
        if ours && !done {
            let prefix = if table.is_empty() { "theme." } else { "" };
            out.push(format!("{prefix}{line}"));
            done = true;
        } else {
            out.push(l.to_owned());
        }
    }
    match (done, header) {
        (true, _) => {}
        (false, Some(at)) => out.insert(at, line),
        (false, None) => {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.extend(["[theme]".to_owned(), line]);
        }
    }
    let new = out.join(eol) + eol;
    let table: Table = toml::from_str(&new).ok()?;
    let set = table
        .get("theme")
        .and_then(|t| t.get(key))
        .and_then(Value::as_str);
    (set == Some(name)).then_some(new)
}

/// What a new config file starts as: `docs/config.toml`, every key
/// commented out so the defaults stay jet's.
pub fn starter() -> String {
    let doc = include_str!("../../../docs/config.toml");
    let mut out = String::with_capacity(doc.len() + 4096);
    for line in doc.lines() {
        let t = line.trim_start();
        let plain_header = t.starts_with('[') && !t.starts_with("[[");
        if t.is_empty() || t.starts_with('#') || plain_header {
            out.push_str(line);
        } else {
            out.push_str("# ");
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Sections jet and jetmux both read, the same in either's file.
pub const SHARED: [&str; 3] = ["theme", "colors", "status"];

/// What jetmux reads: jet's config, if any, with jetmux's own file `own`
/// on top. That file's [`SHARED`] sections layer over jet's; the rest of
/// it is jetmux's, as under `[jetmux]` in jet's.
pub fn load_for_jetmux(dirs: &Dirs, own: &Path) -> Result<Loaded, Error> {
    let main = dirs.candidates().into_iter().find(|p| p.is_file());
    let main_text = main.as_ref().map(|p| read(p)).transpose()?;
    let own_table = if own.is_file() {
        let at = |err: String| Error(format!("{}: {err}", own.display()));
        let mut table: Table = toml::from_str(&read(own)?).map_err(|e| at(e.to_string()))?;
        let keys: Vec<String> = table
            .keys()
            .filter(|k| !SHARED.contains(&k.as_str()))
            .cloned()
            .collect();
        let mut jetmux = Table::new();
        for key in keys {
            if let Some(value) = table.remove(&key) {
                jetmux.insert(key, value);
            }
        }
        table.insert("jetmux".into(), Value::Table(jetmux));
        Some((own, table))
    } else {
        None
    };
    let mut loaded = resolve(main.as_deref().zip(main_text.as_deref()), own_table, dirs)?;
    if !own.is_file() {
        loaded.absent.push(own.to_owned());
    }
    Ok(loaded)
}

impl Config {
    /// The sound files set, with where: `terminal.bell-sound`,
    /// `profiles.NAME.notification-sound`.
    pub fn sounds(&self) -> Vec<(String, &Path)> {
        let terminal = &self.terminal;
        let mut sounds = vec![
            (
                "terminal.bell-sound".to_owned(),
                terminal.bell_sound.as_deref(),
            ),
            (
                "terminal.notification-sound".to_owned(),
                terminal.notification_sound.as_deref(),
            ),
        ];
        for (name, p) in &self.profiles {
            sounds.push((
                format!("profiles.{name}.bell-sound"),
                p.bell_sound.as_deref(),
            ));
            let key = format!("profiles.{name}.notification-sound");
            sounds.push((key, p.notification_sound.as_deref()));
        }
        sounds
            .into_iter()
            .filter_map(|(k, p)| Some((k, p?)))
            .collect()
    }

    fn sounds_mut(&mut self) -> Vec<(String, &mut PathBuf)> {
        let terminal = &mut self.terminal;
        let mut sounds = vec![
            (
                "terminal.bell-sound".to_owned(),
                terminal.bell_sound.as_mut(),
            ),
            (
                "terminal.notification-sound".to_owned(),
                terminal.notification_sound.as_mut(),
            ),
        ];
        for (name, p) in &mut self.profiles {
            sounds.push((format!("profiles.{name}.bell-sound"), p.bell_sound.as_mut()));
            let key = format!("profiles.{name}.notification-sound");
            sounds.push((key, p.notification_sound.as_mut()));
        }
        sounds
            .into_iter()
            .filter_map(|(k, p)| Some((k, p?)))
            .collect()
    }

    /// The sound a pane of profile `profile` plays for the bell, or with
    /// `notification` for a notification: the profile's, else
    /// `[terminal]`'s.
    pub fn sound_for(&self, profile: Option<&str>, notification: bool) -> Option<&Path> {
        let own = profile.and_then(|name| self.profiles.get(name));
        let terminal = &self.terminal;
        let (own, default) = if notification {
            let own = own.and_then(|p| p.notification_sound.as_deref());
            (own, terminal.notification_sound.as_deref())
        } else {
            (
                own.and_then(|p| p.bell_sound.as_deref()),
                terminal.bell_sound.as_deref(),
            )
        };
        own.or(default)
    }

    /// As TOML, every setting spelled out.
    pub fn to_toml(&self) -> Result<String, Error> {
        toml::to_string(self).map_err(|e| Error(e.to_string()))
    }

    /// A config from `text` alone, as if it were the file at `config.toml`.
    pub fn parse(text: &str, dirs: &Dirs) -> Result<Loaded, Error> {
        resolve(Some((Path::new("config.toml"), text)), None, dirs)
    }
    /// Key bindings: the defaults (unless turned off) with the config's on
    /// top.
    pub fn bindings(&self) -> Result<Vec<keybinds::Binding>, String> {
        let mut use_defaults = true;
        let mut custom = Vec::new();
        for (chord, bind) in &self.keybinds {
            match (chord.as_str(), bind) {
                ("defaults", Bind::Flag(on)) => use_defaults = *on,
                (_, Bind::Action(action)) => custom.push((chord.clone(), action.clone())),
                (chord, Bind::Flag(_)) => {
                    return Err(format!(
                        "keybinds: {chord:?} needs an action, not true/false"
                    ));
                }
            }
        }
        keybinds::resolve(use_defaults, custom)
    }

    /// What the terminal itself follows; unset colors keep jet's defaults.
    pub fn vt_settings(&self) -> VtSettings {
        self.vt_settings_with(&self.colors)
    }

    /// For a pane of profile `name`: its colors over the config's.
    pub fn profile_vt_settings(&self, name: Option<&str>) -> VtSettings {
        self.pane_vt_settings(name, None)
    }

    /// For a pane of profile `profile` on a host `host` (a `[hosts]` name)
    /// matched: the host's colors over the profile's over the config's.
    pub fn pane_vt_settings(&self, profile: Option<&str>, host: Option<&str>) -> VtSettings {
        let mut colors = self.colors.clone();
        if let Some(profile) = profile.and_then(|name| self.profiles.get(name)) {
            colors = profile.colors.over(&colors);
        }
        if let Some(host) = host.and_then(|name| self.hosts.get(name)) {
            colors = host.colors.over(&colors);
        }
        self.vt_settings_with(&colors)
    }

    /// The first `[hosts]` rule (by name) whose regex matches `host`.
    pub fn host_rule(&self, host: &str) -> Option<&str> {
        self.hosts
            .iter()
            .find(|(_, rule)| rule.pattern.regex().is_some_and(|r| r.is_match(host)))
            .map(|(name, _)| name.as_str())
    }

    fn vt_settings_with(&self, colors: &Colors) -> VtSettings {
        let default = VtSettings::default();
        VtSettings {
            foreground: colors.foreground.map_or(default.foreground, |c| c.0),
            background: colors.background.map_or(default.background, |c| c.0),
            cursor: colors.cursor.map(|c| c.0),
            palette: colors
                .palette
                .iter()
                .filter_map(|(i, c)| Some((i.parse().ok()?, c.0)))
                .collect(),
            cursor_style: match self.cursor.style {
                CursorStyle::Block => CursorShape::Block,
                CursorStyle::Bar => CursorShape::Bar,
                CursorStyle::Underline => CursorShape::Underline,
            },
            cursor_blink: self.cursor.blink,
            copy_on_select: self.terminal.copy_on_select,
            clipboard_write: self.terminal.clipboard_write,
        }
    }
}

fn read(path: &Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|err| Error(format!("reading {}: {err}", path.display())))
}

/// `main` (a jet config) and what it includes, then `top` over it.
fn resolve(
    main: Option<(&Path, &str)>,
    top: Option<(&Path, Table)>,
    dirs: &Dirs,
) -> Result<Loaded, Error> {
    let mut loaded = Loaded::default();
    let mut native = Table::new();
    if let Some((path, text)) = main {
        layer(&mut native, path, text, dirs, &mut loaded)?;
    }
    if let Some((path, table)) = top {
        merge(&mut native, table);
        loaded.sources.push(path.to_owned());
    }
    let inline = match native.remove("ghostty") {
        Some(Value::Table(table)) => table,
        _ => Table::new(),
    };
    let ghostty_file = match inline.get("config-file") {
        Some(Value::String(file)) => {
            let dir = main.and_then(|(p, _)| p.parent());
            Some(dirs.expand(file, dir))
        }
        Some(Value::Boolean(false)) => None,
        None => dirs.ghostty_files().into_iter().find(|p| p.is_file()),
        Some(_) => return Err(Error("ghostty.config-file must be a path or false".into())),
    };

    let mut merged = Table::new();
    if let Some(file) = ghostty_file {
        let (table, skipped) = ghostty::translate(&ghostty::parse(&read(&file)?));
        for key in skipped {
            loaded.warnings.push(format!(
                "{}: {key} isn't supported, ignored",
                file.display()
            ));
        }
        merge(&mut merged, table);
        loaded.sources.push(file);
    }
    let (table, skipped) = ghostty::translate(&ghostty::from_table(&inline));
    for key in skipped {
        loaded
            .warnings
            .push(format!("[ghostty] {key} isn't supported, ignored"));
    }
    merge(&mut merged, table);
    merge(&mut merged, native);

    let mode = match dirs.appearance {
        Appearance::Light => "light",
        Appearance::Dark => "dark",
    };
    let theme = merged.get("theme").and_then(|t| {
        let name = t.get(mode).or_else(|| t.get("name"));
        name.and_then(Value::as_str).map(str::to_owned)
    });
    if let Some(name) = theme {
        theme_under(&mut merged, &name, dirs, &mut loaded)?;
    }
    for looks in ["profiles", "hosts"] {
        let Some(Value::Table(looks)) = merged.get_mut(looks) else {
            continue;
        };
        for (_, look) in looks.iter_mut() {
            let Some(look) = look.as_table_mut() else {
                continue;
            };
            if let Some(name) = look.get("theme").and_then(Value::as_str) {
                let name = name.to_owned();
                theme_under(look, &name, dirs, &mut loaded)?;
            }
        }
    }

    let mut config: Config = Value::Table(merged)
        .try_into()
        .map_err(|e: toml::de::Error| Error(e.to_string()))?;
    // Icon and sound files resolve like other paths: `~/`, or next to the
    // config.
    let dir = main.and_then(|(p, _)| p.parent());
    let icons = config.profiles.values_mut().map(|p| &mut p.icon);
    for icon in std::iter::once(&mut config.shell.icon).chain(icons) {
        if let Some(Icon::File(file)) = icon {
            *file = dirs.expand(&file.to_string_lossy(), dir);
        }
    }
    for (key, sound) in config.sounds_mut() {
        *sound = dirs.expand(&sound.to_string_lossy(), dir);
        if !sound.is_file() {
            let shown = sound.display();
            loaded.warnings.push(format!("{key}: there's no {shown}"));
        }
    }
    check(&config)?;
    loaded.config = config;
    Ok(loaded)
}

/// Theme `name`'s colors under `table`'s own `colors`. A file of the
/// user's wins over the built-in theme of that name.
fn theme_under(
    table: &mut Table,
    name: &str,
    dirs: &Dirs,
    loaded: &mut Loaded,
) -> Result<(), Error> {
    let path = find_theme(name, dirs);
    let text = match &path {
        Some(path) => read(path)?,
        None => bundled_theme(name)
            .ok_or_else(|| {
                let searched: Vec<String> = dirs
                    .theme_dirs()
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect();
                Error(format!(
                    "theme {name:?} isn't built in or in {}",
                    searched.join(", ")
                ))
            })?
            .to_owned(),
    };
    // Themes carry keys jet has no use for: quietly.
    let (mut theme, _) = ghostty::translate(&ghostty::parse(&text));
    let mut colors = match theme.remove("colors") {
        Some(Value::Table(colors)) => colors,
        _ => Table::new(),
    };
    if let Some(Value::Table(own)) = table.remove("colors") {
        merge(&mut colors, own);
    }
    table.insert("colors".into(), Value::Table(colors));
    if !path.as_ref().is_some_and(|p| loaded.sources.contains(p)) {
        loaded.sources.extend(path);
    }
    Ok(())
}

/// `key` names profile `name` for first tabs: it must be a plain one.
fn check_default(config: &Config, key: &str, name: &str) -> Result<(), Error> {
    match config.profiles.get(name) {
        None => Err(Error(format!("{key}: there's no [profiles.{name}]"))),
        Some(p) if p.jetmux.is_some() => Err(Error(format!(
            "{key}: {name} is a jetmux session, which can't be a default shell"
        ))),
        Some(_) => Ok(()),
    }
}

fn check(config: &Config) -> Result<(), Error> {
    let profiles = config
        .profiles
        .iter()
        .map(|(name, p)| (format!("profiles.{name}."), &p.colors));
    let hosts = config
        .hosts
        .iter()
        .map(|(name, h)| (format!("hosts.{name}."), &h.colors));
    let own = profiles.chain(hosts);
    for (at, colors) in std::iter::once((String::new(), &config.colors)).chain(own) {
        if let Some(key) = colors.palette.keys().find(|k| k.parse::<u8>().is_err()) {
            return Err(Error(format!(
                "{at}colors.palette: {key:?} is not an index 0-255"
            )));
        }
    }
    if !(1.0..=200.0).contains(&config.font.size) {
        return Err(Error(format!(
            "font.size {} is out of range",
            config.font.size
        )));
    }
    for binding in config.bindings().map_err(Error)? {
        let profile = match &binding.action {
            Some(
                keybinds::Action::NewTab(Some(name)) | keybinds::Action::NewSplit(_, Some(name)),
            ) => name,
            _ => continue,
        };
        if !config.profiles.contains_key(profile) {
            return Err(Error(format!("keybinds: there's no [profiles.{profile}]")));
        }
    }
    for (name, rule) in &config.plumb {
        rule.check(name).map_err(Error)?;
    }
    if let Some(name) = config
        .hosts
        .iter()
        .find_map(|(n, h)| h.pattern.regex().is_none().then_some(n))
    {
        return Err(Error(format!("hosts.{name}: needs a match regex")));
    }
    let quakes = config.quake.resolve().map_err(Error)?;
    for (name, quake) in &quakes {
        let at = name
            .as_ref()
            .map_or("quake".to_owned(), |n| format!("quake.{n}"));
        if let Some(chord) = quake
            .hotkey
            .0
            .iter()
            .find(|c| keybinds::parse_chord(c).is_none())
        {
            return Err(Error(format!("{at}.hotkey: {chord:?} isn't a key chord")));
        }
        if let Some(name) = &quake.profile {
            check_default(config, &format!("{at}.profile"), name)?;
        }
        let fits = |start: f32, len: f32| len > 0.0 && start >= 0.0 && start + len <= 1.001;
        if !fits(quake.x, quake.width) || !fits(quake.y, quake.height) {
            return Err(Error(format!(
                "{at}: x, y, width and height are fractions of the screen, and must fit on it"
            )));
        }
    }
    let defaults = [
        ("window.profile", &config.window.profile),
        ("shell.profile", &config.shell.profile),
    ];
    for (key, name) in defaults {
        if let Some(name) = name {
            check_default(config, key, name)?;
        }
    }
    if !(0.0..=1.0).contains(&config.window.unfocused_split_opacity) {
        return Err(Error(
            "window.unfocused-split-opacity must be between 0 and 1".into(),
        ));
    }
    if !(0.0..=1.0).contains(&config.terminal.sound_volume) {
        return Err(Error("terminal.sound-volume must be from 0 to 1".into()));
    }
    if !(0.0..=1.0).contains(&config.window.opacity) {
        return Err(Error("window.opacity must be from 0 to 1".into()));
    }
    if config.window.titlebar_color == FrameColor::None {
        return Err(Error(
            "window.titlebar-color can't be none; use system or a color".into(),
        ));
    }
    if !(4..=200).contains(&config.tab_bar.width) {
        return Err(Error("tab-bar.width must be 4 to 200 cells".into()));
    }
    if config.window.columns == 0 || config.window.rows == 0 {
        return Err(Error(
            "window.columns and window.rows must be at least 1".into(),
        ));
    }
    Ok(())
}

/// Ghostty's themes, and jet's own: each from its `[name]` line to the
/// next.
const THEMES: [&str; 2] = [
    include_str!("../themes.txt"),
    include_str!("../jet-themes.txt"),
];

fn bundled_themes() -> impl Iterator<Item = (&'static str, &'static str)> {
    THEMES.into_iter().flat_map(|file| {
        let themes = file.split("\n[").skip(1);
        themes.filter_map(|theme| theme.split_once("]\n"))
    })
}

/// The built-in theme `name`; any case, if no exact match.
fn bundled_theme(name: &str) -> Option<&'static str> {
    let exact = bundled_themes().find(|(n, _)| *n == name);
    exact
        .or_else(|| bundled_themes().find(|(n, _)| n.eq_ignore_ascii_case(name)))
        .map(|(_, text)| text)
}

fn find_theme(name: &str, dirs: &Dirs) -> Option<PathBuf> {
    if name.contains(['/', '\\']) {
        return Some(dirs.expand(name, None)).filter(|p| p.is_file());
    }
    dirs.theme_dirs()
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// `top` over `base`: tables merge key by key, anything else replaces.
/// Merge the jet config `text` from `path` into `native`, then its
/// includes on top of it.
fn layer(
    native: &mut Table,
    path: &Path,
    text: &str,
    dirs: &Dirs,
    loaded: &mut Loaded,
) -> Result<(), Error> {
    let at = |err: String| Error(format!("{}: {err}", path.display()));
    // Typed first: mistakes come back with a line and column.
    let typed = toml::from_str::<Config>(text).map_err(|e| at(e.to_string()))?;
    let mut table: Table = toml::from_str(text).map_err(|e| at(e.to_string()))?;
    table.remove("include");
    merge(native, table);
    loaded.sources.push(path.to_owned());
    for file in typed.include {
        let file = dirs.expand(&file, path.parent());
        if loaded.sources.contains(&file) {
            return Err(at(format!("{} is included twice", file.display())));
        }
        if !file.is_file() {
            loaded.absent.push(file);
            continue;
        }
        layer(native, &file, &read(&file)?, dirs, loaded)?;
    }
    Ok(())
}

fn merge(base: &mut Table, top: Table) {
    for (key, value) in top {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(base)), Value::Table(top)) => merge(base, top),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Split a command line on whitespace, with double quotes grouping.
fn split_command(line: &str) -> Vec<String> {
    let (mut args, mut arg, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => (quoted, any) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if std::mem::take(&mut any) {
                    args.push(std::mem::take(&mut arg));
                }
            }
            c => {
                arg.push(c);
                any = true;
            }
        }
    }
    if any {
        args.push(arg);
    }
    args
}

#[cfg(test)]
mod tests {
    use jet_core::{CursorShape, Rgb};

    use std::path::PathBuf;

    use crate::{Backdrop, Config, Dirs, FrameColor, Icon, TabBarPosition, split_command};

    fn parse(text: &str) -> Result<crate::Loaded, String> {
        Config::parse(text, &Dirs::default()).map_err(|e| e.0)
    }

    #[test]
    fn default_profiles_must_be_plain_profiles() {
        let ok = "[profiles.bash]\ncommand = \"bash\"\n[quake]\nprofile = \"bash\"\n";
        assert!(parse(ok).is_ok());
        let tabs = "[profiles.bash]\ncommand = \"bash\"\n[shell]\nprofile = \"bash\"\n";
        assert!(parse(tabs).is_ok_and(|l| l.config.shell.profile.as_deref() == Some("bash")));
        let window = parse("[window]\nprofile = \"gone\"\n").err();
        assert!(window.is_some_and(|e| e.contains("window.profile: there's no")));
        let missing = parse("[quake]\nprofile = \"nope\"\n").err();
        assert!(missing.is_some_and(|e| e.contains("no [profiles.nope]")));
        let mux = "[profiles.m]\njetmux = { session = \"s\" }\n[quake]\nprofile = \"m\"\n";
        assert!(parse(mux).err().is_some_and(|e| e.contains("jetmux")));
    }

    #[test]
    fn jetmux_runs_the_program_given() {
        let mut spec = crate::Jetmux {
            host: Some("voyager".into()),
            session: "main".into(),
            up: None,
            program: None,
        };
        assert_eq!(spec.command(), ["ssh", "-T", "voyager", "jetmux", "proxy"]);
        spec.program = Some("~/.nix-profile/bin/jetmux".into());
        assert_eq!(spec.command()[3], "~/.nix-profile/bin/jetmux");
    }

    /// A fresh dir under the system temp dir with `files` written in it.
    fn scratch(name: &str, files: &[(&str, &str)]) -> std::io::Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!("jet-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (file, text) in files {
            let path = dir.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, text)?;
        }
        Ok(dir)
    }

    #[test]
    fn includes_layer_on_top() -> Result<(), Box<dyn std::error::Error>> {
        let dir = scratch(
            "include",
            &[
                (
                    "config.toml",
                    "include = [\"box.toml\", \"missing.toml\"]\n[font]\nsize = 12\n\
                     [profiles.pwsh]\ncommand = \"pwsh\"\n",
                ),
                (
                    "box.toml",
                    "[font]\nsize = 15\n[profiles.voyager]\ncommand = \"ssh voyager\"\n",
                ),
                ("loop.toml", "include = [\"loop.toml\"]\n"),
            ],
        )?;
        let dirs = Dirs {
            explicit: Some(dir.join("config.toml")),
            ..Dirs::default()
        };
        let loaded = crate::load(&dirs)?;
        assert_eq!(loaded.config.font.size, 15.0, "the include wins");
        let names: Vec<&String> = loaded.config.profiles.keys().collect();
        assert_eq!(names, ["pwsh", "voyager"]);
        assert_eq!(
            loaded.sources,
            [dir.join("config.toml"), dir.join("box.toml")]
        );
        assert_eq!(loaded.absent, [dir.join("missing.toml")]);

        let looped = Dirs {
            explicit: Some(dir.join("loop.toml")),
            ..Dirs::default()
        };
        assert!(crate::load(&looped).is_err_and(|e| e.0.contains("included twice")));
        Ok(())
    }

    #[test]
    fn jetmux_layers_its_own_file_over_jets() -> Result<(), Box<dyn std::error::Error>> {
        let dir = scratch(
            "jetmux",
            &[
                (
                    "config.toml",
                    "[font]\nsize = 12\n[[status.segments]]\ntext = \"jet\"\n\
                     [jetmux]\nprefix = \"C-a\"\n[jetmux.persist]\ninterval = 60\n",
                ),
                (
                    "jetmux.toml",
                    "prefix = \"C-b\"\n[[status.segments]]\ntext = \"mux\"\n",
                ),
            ],
        )?;
        let dirs = Dirs {
            explicit: Some(dir.join("config.toml")),
            ..Dirs::default()
        };
        let loaded = crate::load_for_jetmux(&dirs, &dir.join("jetmux.toml"))?;
        let jetmux = loaded.config.jetmux.clone().unwrap_or_default();
        assert_eq!(jetmux["prefix"].as_str(), Some("C-b"), "its own file wins");
        assert!(
            jetmux.contains_key("persist"),
            "the rest of jet's [jetmux] stays"
        );
        assert_eq!(
            loaded.config.status.segments[0]
                .text
                .render(|_| None)
                .as_deref(),
            Some("mux"),
            "shared, on top"
        );
        assert_eq!(loaded.config.font.size, 12.0);
        assert_eq!(
            loaded.sources,
            [dir.join("config.toml"), dir.join("jetmux.toml")]
        );

        // Either alone works.
        let alone = crate::load_for_jetmux(&Dirs::default(), &dir.join("jetmux.toml"))?;
        assert_eq!(
            alone.config.status.segments[0]
                .text
                .render(|_| None)
                .as_deref(),
            Some("mux")
        );
        let without = crate::load_for_jetmux(&dirs, &dir.join("none.toml"))?;
        assert_eq!(without.absent, [dir.join("none.toml")]);
        assert_eq!(
            without.config.jetmux.unwrap_or_default()["prefix"].as_str(),
            Some("C-a")
        );
        Ok(())
    }

    #[test]
    fn ghostty_layers_under_jet_keys() -> Result<(), Box<dyn std::error::Error>> {
        let dir = scratch(
            "layers",
            &[
                (
                    "ghostty/config",
                    "# mine\nfont-family = Iosevka\nfont-family = Symbols\nfont-size = 11\n\
                     theme = light:Day,dark:Night\nbackground = 000000\npalette = 1=#ff0000\n\
                     window-padding-x = 6,2\ncopy-on-select = clipboard\nkeybind = ctrl+a=noop\nkeybind = ctrl+shift+d=new_split:down\n",
                ),
                (
                    "ghostty/themes/Night",
                    "background = #101010\nforeground = #e0e0e0\npalette = 2=#00ff00\n\
                     selection-background = #333333\n",
                ),
            ],
        )?;
        let dirs = Dirs {
            ghostty: vec![dir.join("ghostty")],
            ..Dirs::default()
        };
        let loaded = Config::parse(
            "[font]\nsize = 14\n[ghostty]\ncursor-style = \"bar\"\nfont-size = 12\n\
             [colors]\nforeground = \"#abcdef\"\n",
            &dirs,
        )?;
        let c = &loaded.config;
        assert_eq!(
            c.font.family.as_deref(),
            Some("Iosevka"),
            "first family wins"
        );
        assert_eq!(c.font.size, 14.0, "jet keys beat inline and imported ones");
        assert_eq!(c.window.padding, 6);
        assert_eq!(c.theme.light.as_deref(), Some("Day"));
        assert_eq!(c.theme.dark.as_deref(), Some("Night"));
        let vt = c.vt_settings();
        assert_eq!(
            vt.cursor_style,
            jet_core::CursorShape::Bar,
            "inline keys apply"
        );
        let rgb = |r, g, b| Rgb { r, g, b };
        // As in Ghostty, colors set anywhere beat the theme's.
        assert_eq!(vt.background, rgb(0, 0, 0));
        assert_eq!(
            c.colors.selection_background,
            Some(crate::Color(rgb(0x33, 0x33, 0x33)))
        );
        assert_eq!(vt.foreground, rgb(0xab, 0xcd, 0xef));
        assert_eq!(vt.palette, [(1, rgb(255, 0, 0)), (2, rgb(0, 255, 0))]);
        assert!(vt.copy_on_select);
        let ghostty = dir.join("ghostty");
        assert_eq!(
            loaded.sources,
            [
                PathBuf::from("config.toml"),
                ghostty.join("config"),
                ghostty.join("themes").join("Night")
            ]
        );
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("keybind = ctrl+a=noop"));
        let bindings = c.bindings()?;
        let split = crate::keybinds::find(
            &bindings,
            jet_core::Key::D,
            jet_core::Mods::CTRL | jet_core::Mods::SHIFT,
        );
        assert_eq!(
            split,
            Some(Some(crate::keybinds::Action::NewSplit(
                crate::keybinds::Direction::Down,
                None
            )))
        );
        let opted_out = Config::parse("[ghostty]\nconfig-file = false\n", &dirs)?;
        assert_eq!(opted_out.config.font.family, None);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// `docs/config.toml` loads, warns about nothing, and its values are
    /// the defaults, so copying it changes nothing.
    #[test]
    fn documented_config_is_the_defaults() -> Result<(), String> {
        let loaded = parse(include_str!("../../../docs/config.toml"))?;
        assert_eq!(loaded.warnings, Vec::<String>::new());
        let mut defaults = Config::default();
        // The file shows the other platforms' hotkey and names macOS's.
        defaults.quake.hotkey = "ctrl+space".into();
        assert_eq!(loaded.config, defaults);
        Ok(())
    }

    #[test]
    fn the_starter_config_is_the_defaults() -> Result<(), String> {
        let loaded = parse(&crate::starter())?;
        assert_eq!(loaded.warnings, Vec::<String>::new());
        assert_eq!(loaded.config, Config::default());
        Ok(())
    }

    #[test]
    fn with_theme_edits_only_the_theme() {
        let set = |text: &str| crate::with_theme(text, "name", "Night");
        let named = "[font]\nsize = 13\n\n[theme]\nname = \"Day\" # mine\n[colors]\n";
        assert_eq!(
            set(named).as_deref(),
            Some("[font]\nsize = 13\n\n[theme]\nname = \"Night\"\n[colors]\n")
        );
        let empty = set("[theme]\n# name = \"Day\"\n");
        assert_eq!(
            empty.as_deref(),
            Some("[theme]\nname = \"Night\"\n# name = \"Day\"\n")
        );
        let none = set("[font]\r\nsize = 13\r\n");
        assert_eq!(
            none.as_deref(),
            Some("[font]\r\nsize = 13\r\n\r\n[theme]\r\nname = \"Night\"\r\n")
        );
        assert_eq!(set("theme = { name = \"Day\" }\n"), None);
        let paired = "[theme]\nname = \"Day\"\nlight = \"Day\"\n";
        assert_eq!(
            crate::with_theme(paired, "light", "Night").as_deref(),
            Some("[theme]\nname = \"Day\"\nlight = \"Night\"\n")
        );
    }

    #[test]
    fn light_mode_picks_its_theme() -> Result<(), crate::Error> {
        let text = "[theme]\nname = \"poimandres\"\nlight = \"Nu Disco Light\"\n";
        let background = |appearance| -> Result<_, crate::Error> {
            let dirs = Dirs {
                appearance,
                ..Dirs::default()
            };
            Ok(Config::parse(text, &dirs)?.config.vt_settings().background)
        };
        let white = Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        };
        assert_eq!(background(crate::Appearance::Light)?, white);
        assert_ne!(background(crate::Appearance::Dark)?, white, "no dark: name");
        Ok(())
    }

    #[test]
    fn hosts_restyle_their_panes() -> Result<(), String> {
        let loaded = parse(
            "[profiles.box]\ntab-color = \"#00ff00\"\ncolors.cursor = \"#00ff00\"\n\
             [hosts.prod]\nmatch = '^prod-'\ntheme = \"grapebox\"\ntab-color = \"#ff0000\"\n",
        )?;
        let c = &loaded.config;
        assert_eq!(c.host_rule("prod-db1"), Some("prod"));
        assert_eq!(c.host_rule("dev-prod-1"), None);
        let prod = c.pane_vt_settings(Some("box"), Some("prod"));
        let rgb = |r, g, b| Rgb { r, g, b };
        assert_eq!(prod.background, rgb(0x43, 0x33, 0x53), "the host's theme");
        assert_eq!(
            prod.cursor,
            Some(rgb(0xb5, 0x94, 0xd6)),
            "over the profile's colors"
        );
        let err = parse("[hosts.prod]\ntheme = \"grapebox\"\n").err();
        assert!(err.is_some_and(|e| e.contains("hosts.prod: needs a match regex")));
        Ok(())
    }

    #[test]
    fn missing_theme_is_an_error() {
        let err = parse("[theme]\nname = \"Nope\"\n").err();
        assert!(err.is_some_and(|e| e.contains("theme \"Nope\" isn't built in")));
    }

    #[test]
    fn profiles_take_their_own_theme() -> Result<(), String> {
        let loaded = parse(
            "[theme]\nname = \"poimandres\"\n[colors]\ncursor = \"#ff0000\"\n\
             [profiles.prod]\ntheme = \"grapebox\"\ncolors.foreground = \"#00ff00\"\n",
        )?;
        let c = &loaded.config;
        let prod = c.profile_vt_settings(Some("prod"));
        // The profile's theme, its own colors over that, the rest from [colors].
        assert_eq!(
            prod.background,
            Rgb {
                r: 0x43,
                g: 0x33,
                b: 0x53
            }
        );
        assert_eq!(
            prod.foreground,
            Rgb {
                r: 0,
                g: 0xff,
                b: 0
            }
        );
        assert_eq!(
            prod.cursor,
            Some(Rgb {
                r: 0xb5,
                g: 0x94,
                b: 0xd6
            })
        );
        // Other panes keep [theme]'s.
        assert_eq!(c.profile_vt_settings(None), c.vt_settings());
        assert_eq!(
            c.vt_settings().background,
            Rgb {
                r: 0x1a,
                g: 0x1e,
                b: 0x28
            }
        );
        assert!(parse("[profiles.prod]\ntheme = \"no such theme\"\n").is_err());
        Ok(())
    }

    #[test]
    fn ghosttys_themes_are_built_in() -> Result<(), String> {
        let loaded = parse("[theme]\nname = \"poimandres\"\n")?;
        let background = loaded.config.colors.background.map(|c| c.0);
        assert_eq!(
            background,
            Some(Rgb {
                r: 0x1a,
                g: 0x1e,
                b: 0x28
            })
        );
        // Names a file couldn't have on Windows work too.
        assert!(parse("[theme]\nname = \"Paper Crane: Midnight Ink\"\n").is_ok());
        // And jet's own.
        let grape = parse("[theme]\nname = \"grapebox\"\n")?;
        let background = grape.config.colors.background.map(|c| c.0);
        assert_eq!(
            background,
            Some(Rgb {
                r: 0x43,
                g: 0x33,
                b: 0x53
            })
        );
        for name in [
            "Dank Neon",
            "Dank Neon Light",
            "Nu Disco Dark",
            "Nu Disco Light",
        ] {
            let theme = parse(&format!("[theme]\nname = \"{name}\"\n"))?;
            assert!(theme.config.colors.background.is_some(), "{name}");
        }
        assert!(Dirs::default().themes().len() > 900);
        Ok(())
    }

    #[test]
    fn empty_file_is_the_defaults() {
        let loaded = Config::parse("", &Dirs::default()).ok();
        assert_eq!(loaded.map(|l| l.config), Some(Config::default()));
    }

    #[test]
    fn icon_files_resolve_like_paths() -> Result<(), String> {
        let dirs = Dirs {
            home: Some(PathBuf::from("/home/me")),
            ..Dirs::default()
        };
        let text = r#"
            [shell]
            icon = "~/icons/bash.svg"
            [profiles.pwsh]
            icon = "pwsh.SVG"
            [profiles.cmd]
            icon = "⚡"
            [profiles.dot]
            icon = '<svg viewBox="0 0 2 2"><circle cx="1" cy="1" r="1"/></svg>'
        "#;
        let config = Config::parse(text, &dirs)
            .map_err(|e| e.to_string())?
            .config;
        let icon = |name: &str| config.profiles.get(name).and_then(|p| p.icon.clone());
        let home = PathBuf::from("/home/me").join("icons/bash.svg");
        assert_eq!(config.shell.icon, Some(Icon::File(home)));
        assert_eq!(icon("pwsh"), Some(Icon::File("pwsh.SVG".into())));
        assert_eq!(icon("cmd"), Some(Icon::Text("⚡".into())));
        assert!(matches!(icon("dot"), Some(Icon::Svg(s)) if s.ends_with("</svg>")));
        Ok(())
    }

    #[test]
    fn full_config_parses() -> Result<(), String> {
        let loaded = parse(
            r##"
            [font]
            family = "Cascadia Mono"
            size = 13.5
            features = ["-calt"]

            [window]
            padding = 8
            opacity = 0.85
            backdrop = "acrylic"
            border-color = "none"
            titlebar-color = "#101010"

            [profiles.pwsh]
            command = "powershell.exe -NoLogo"
            icon = "⚡"

            [tab-bar]
            position = "left"
            width = 30
            active-background = "#333333"

            [shell]
            command = '"C:\Program Files\Git\bin\bash.exe" -l'

            [terminal]
            scrollback = 50000
            copy-on-select = false

            [cursor]
            style = "bar"
            blink = false

            [colors]
            background = "#1b1e28"
            foreground = "a6accd"
            palette = { 1 = "#d0679d", 15 = "#fff" }

            [quake]
            hotkey = "ctrl+grave"
            width = 0.333
            height = 1.0
            "##,
        )?;
        let c = &loaded.config;
        assert_eq!(c.font.family.as_deref(), Some("Cascadia Mono"));
        assert_eq!(c.font.size, 13.5);
        assert_eq!(c.window.padding, 8);
        assert_eq!(c.window.columns, 100, "unset keys keep their defaults");
        assert_eq!(
            c.shell.command.as_ref().map(|cmd| cmd.argv()),
            Some(vec![
                r"C:\Program Files\Git\bin\bash.exe".to_owned(),
                "-l".to_owned()
            ])
        );
        assert_eq!(c.terminal.scrollback, 50_000);
        let vt = c.vt_settings();
        assert_eq!(
            vt.background,
            Rgb {
                r: 0x1b,
                g: 0x1e,
                b: 0x28
            }
        );
        assert_eq!(
            vt.foreground,
            Rgb {
                r: 0xa6,
                g: 0xac,
                b: 0xcd
            }
        );
        let white = Rgb {
            r: 255,
            g: 255,
            b: 255,
        };
        assert_eq!(
            vt.palette,
            vec![
                (
                    1,
                    Rgb {
                        r: 0xd0,
                        g: 0x67,
                        b: 0x9d
                    }
                ),
                (15, white)
            ]
        );
        assert_eq!(vt.cursor_style, CursorShape::Bar);
        assert!(!vt.cursor_blink && !vt.copy_on_select);
        assert_eq!(c.window.opacity, 0.85);
        assert_eq!(c.window.backdrop, Backdrop::Acrylic);
        assert_eq!(c.window.border_color, FrameColor::None);
        assert_eq!(
            c.window.titlebar_color,
            FrameColor::Color(Rgb {
                r: 0x10,
                g: 0x10,
                b: 0x10
            })
        );
        assert_eq!(c.tab_bar.position, TabBarPosition::Left);
        assert_eq!(c.tab_bar.width, 30);
        assert!(c.tab_bar.active_background.is_some() && c.tab_bar.foreground.is_none());
        assert_eq!(c.profiles["pwsh"].icon, Some(Icon::Text("⚡".into())));
        assert_eq!(c.quake.hotkey, "ctrl+grave".into());
        assert_eq!((c.quake.width, c.quake.y), (0.333, 0.0));
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        Ok(())
    }

    #[test]
    fn mistakes_are_errors_with_a_location() {
        let typo = parse("[font]\nsise = 12\n").err().unwrap_or_default();
        assert!(typo.contains("line 2") && typo.contains("sise"), "{typo}");
        let color = parse("[colors]\nbackground = \"blue\"\n").err();
        assert!(color.is_some_and(|e| e.contains("not a #rrggbb color")));
        let index = parse("[colors.palette]\n300 = \"#000\"\n").err();
        assert!(index.is_some_and(|e| e.contains("0-255")));
        let profile = parse("[keybinds]\n\"ctrl+t\" = \"new_tab:pwsh\"\n").err();
        assert!(profile.is_some_and(|e| e.contains("[profiles.pwsh]")));
        let off_screen = parse("[quake]\nx = 0.8\nwidth = 0.5\n").err();
        assert!(off_screen.is_some_and(|e| e.contains("must fit")));
    }

    #[test]
    fn named_quakes_inherit_quake_and_can_share_a_hotkey() -> Result<(), String> {
        let text = "[quake]\nheight = 1.0\n[quake.left]\nwidth = 0.3\n\
                    [quake.right]\nx = 0.7\nwidth = 0.3\nhotkey = [\"ctrl+space\", \"f9\"]\n";
        let quakes = parse(text)?.config.quake.resolve()?;
        let [(Some(left), l), (Some(right), r)] = quakes.as_slice() else {
            return Err(format!("{quakes:?}"));
        };
        assert_eq!((left.as_str(), right.as_str()), ("left", "right"));
        assert_eq!((l.x, l.width, l.height), (0.0, 0.3, 1.0));
        assert_eq!((r.x, r.height), (0.7, 1.0));
        assert_eq!(l.hotkey, crate::Quake::default().hotkey);
        assert_eq!(r.hotkey.0, ["ctrl+space", "f9"]);
        let typo = parse("[quake]\nhieght = 1.0\n").err().unwrap_or_default();
        assert!(typo.contains("hieght"), "{typo}");
        let only = parse("[quake.left]\nautostart = false\n").err();
        assert!(only.is_some_and(|e| e.contains("quake.left: autostart is for [quake]")));
        let off = parse("[quake.left]\nx = 0.9\n").err();
        assert!(off.is_some_and(|e| e.contains("quake.left: x, y")));
        Ok(())
    }

    #[test]
    fn sounds_come_from_the_profile_then_terminal_and_missing_ones_warn() -> Result<(), String> {
        let text = "[terminal]\nbell-sound = \"nope.ogg\"\nnotification-sound = \"n.mp3\"\n\
                    [profiles.claude]\nbell-sound = \"claude.wav\"\n";
        let loaded = parse(text)?;
        let c = &loaded.config;
        let bell = |p| c.sound_for(p, false).map(|s| s.ends_with("claude.wav"));
        assert_eq!(bell(Some("claude")), Some(true));
        assert_eq!(bell(None), Some(false));
        assert!(
            c.sound_for(Some("claude"), true)
                .is_some_and(|s| s.ends_with("n.mp3"))
        );
        let warned = |key: &str| loaded.warnings.iter().any(|w| w.starts_with(key));
        assert!(warned("terminal.bell-sound: there's no") && warned("profiles.claude.bell-sound"));
        assert!(parse("[terminal]\nsound-volume = 2.0\n").is_err());
        Ok(())
    }

    #[test]
    fn commands_split_like_a_shell_line() {
        assert_eq!(split_command("pwsh  -NoLogo"), ["pwsh", "-NoLogo"]);
        assert_eq!(split_command(r#"a "" "b c"d"#), ["a", "", "b cd"]);
    }
}
