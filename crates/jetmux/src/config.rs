//! jetmux's settings: key bindings, event hooks and profiles, from
//! `[jetmux]` in jet's config with jetmux's own `config.toml` on top, and
//! the sections both programs read ([`jet_config::SHARED`]). Both the
//! client (keys) and the daemon (hooks) reread them when they change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, SystemTime};

use jetmux_proto::rpc::Event;
use jetmux_proto::{Axis, Color, LayoutSpec, NewSession, PaneSpec, TabSpec};
use jetmux_render::Theme;
use serde::{Deserialize, Serialize};

use crate::daemon::EventBus;

/// How often running processes check the config for changes.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The prefix key, e.g. `"C-a"`, or `"none"` for no prefix.
    pub prefix: Option<String>,
    pub keys: Keys,
    pub on: Hooks,
    pub profile: BTreeMap<String, Profile>,
    pub persist: Persist,
    pub bar: StatusBar,
    /// Read by `jetmux-web`.
    pub web: toml::Table,
    /// Read by `jetmux-notify`.
    pub notify: toml::Table,
    /// Everything else, theme and status segments among it.
    #[serde(skip)]
    pub shared: jet_config::Config,
}

/// Snapshots for `jetmux restore` after a reboot. Off unless `interval` is
/// set.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Persist {
    /// Seconds between snapshots.
    pub interval: Option<u64>,
    /// Commands that are safe to run again on restore, as leading words
    /// (`"tail -f"` matches `tail -f log`). Anything else comes back typed
    /// at a shell prompt, not run.
    pub rerun: Vec<String>,
}

impl Default for Persist {
    fn default() -> Self {
        Self {
            interval: None,
            rerun: ["htop", "btop", "top", "watch", "tail -f", "journalctl -f"]
                .map(String::from)
                .to_vec(),
        }
    }
}

/// The status bar's look (`[bar]`). Unset colors keep the default theme's.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatusBar {
    pub style: BarStyle,
    pub bar: Option<ThemeColor>,
    pub text: Option<ThemeColor>,
    pub muted: Option<ThemeColor>,
    pub key: Option<ThemeColor>,
    pub session: Option<ThemeColor>,
    pub badge_text: Option<ThemeColor>,
    pub active: Option<ThemeColor>,
    pub active_text: Option<ThemeColor>,
    pub prefix: Option<ThemeColor>,
    pub pane: Option<ThemeColor>,
    pub tab: Option<ThemeColor>,
    pub copy: Option<ThemeColor>,
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BarStyle {
    #[default]
    Plain,
    Powerline,
}

/// A color as written in the config: a 256-color index, `"#rrggbb"`, or
/// `"default"` for the terminal's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawColor")]
pub struct ThemeColor(Color);

#[derive(Deserialize)]
#[serde(untagged)]
enum RawColor {
    Index(u8),
    Text(String),
}

impl TryFrom<RawColor> for ThemeColor {
    type Error = String;

    fn try_from(raw: RawColor) -> Result<Self, String> {
        let text = match raw {
            RawColor::Index(i) => return Ok(Self(Color::Palette(i))),
            RawColor::Text(text) => text,
        };
        if text == "default" {
            return Ok(Self(Color::Default));
        }
        let bad = || format!("bad color {text:?}: use 0-255, \"#rrggbb\" or \"default\"");
        let hex = text
            .strip_prefix('#')
            .filter(|h| h.len() == 6)
            .ok_or_else(bad)?;
        let byte = |i: usize| {
            hex.get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
                .ok_or_else(bad)
        };
        Ok(Self(Color::Rgb(byte(0)?, byte(2)?, byte(4)?)))
    }
}

impl StatusBar {
    pub fn theme(&self) -> Theme {
        let d = Theme::default();
        let pick = |c: Option<ThemeColor>, default: Color| c.map_or(default, |c| c.0);
        Theme {
            powerline: self.style == BarStyle::Powerline,
            bar: pick(self.bar, d.bar),
            text: pick(self.text, d.text),
            muted: pick(self.muted, d.muted),
            key: pick(self.key, d.key),
            session: pick(self.session, d.session),
            badge_text: pick(self.badge_text, d.badge_text),
            active: pick(self.active, d.active),
            active_text: pick(self.active_text, d.active_text),
            prefix: pick(self.prefix, d.prefix),
            pane: pick(self.pane, d.pane),
            tab: pick(self.tab, d.tab),
            copy: pick(self.copy, d.copy),
        }
    }
}

/// Whether `command` starts with one of the `allowed` word lists, matching
/// the program by its file name.
pub fn rerun_allowed(allowed: &[String], command: &[String]) -> bool {
    let Some((program, args)) = command.split_first() else {
        return false;
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    allowed.iter().any(|entry| {
        let mut words = entry.split_whitespace();
        words.next() == Some(program)
            && words
                .enumerate()
                .all(|(i, w)| args.get(i).is_some_and(|a| a == w))
    })
}

/// Per-layer bindings from key (`"M-h"`, `"C-x"`, `"%"`) to a palette
/// command name, or `"none"` to unbind.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keys {
    pub normal: BTreeMap<String, String>,
    pub prefix: BTreeMap<String, String>,
    pub pane: BTreeMap<String, String>,
    pub tab: BTreeMap<String, String>,
    pub copy: BTreeMap<String, String>,
}

/// Shell commands run on events, with the event in `JETMUX_EVENT_*`
/// variables.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hooks {
    pub session_created: Option<String>,
    pub session_exited: Option<String>,
    #[serde(alias = "pane_exit")]
    pub pane_exited: Option<String>,
    pub bell: Option<String>,
    pub title_changed: Option<String>,
    pub block_done: Option<String>,
    pub notify: Option<String>,
}

impl Hooks {
    fn command(&self, event: &Event) -> Option<&str> {
        match event {
            Event::SessionCreated { .. } => self.session_created.as_deref(),
            Event::SessionExited { .. } => self.session_exited.as_deref(),
            Event::PaneExited { .. } => self.pane_exited.as_deref(),
            Event::Bell { .. } => self.bell.as_deref(),
            Event::TitleChanged { .. } => self.title_changed.as_deref(),
            Event::BlockDone { .. } => self.block_done.as_deref(),
            // Stream-only, never on the event bus.
            Event::Output { .. } | Event::Resized { .. } => None,
            Event::Notify { .. } => self.notify.as_deref(),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub tab: Vec<Node>,
    /// Extra key bar rows in the web UI for this profile's session: key
    /// names (`"C-c"`, `"Esc"`) or text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keybar: Vec<Vec<String>>,
    /// One-tap snippets in the web UI for this profile's session.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Snippet>,
}

/// A named bit of text to type, e.g. `{ name = "test", send = "cargo test\r" }`.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snippet {
    pub name: String,
    pub send: String,
}

/// A tab or a pane: either a command, or a split of more nodes.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Node {
    /// Tabs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Run with `sh -c`. Unset means the login shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    /// Relative paths are relative to the profile's `cwd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Share of the parent split; defaults to 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u16>,
    /// How `panes` are arranged; `vertical` (the default) puts them side
    /// by side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<Split>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<Node>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    /// Vertical borders: panes side by side.
    Vertical,
    /// Horizontal borders: panes stacked.
    Horizontal,
}

/// `$JETMUX_CONFIG`, else `$XDG_CONFIG_HOME/jetmux/config.toml`, else
/// `~/.config/jetmux/config.toml`.
pub fn path() -> PathBuf {
    jetmux_client::config_path()
}

/// Read the config; missing files are the defaults.
pub fn load() -> Result<Config, String> {
    read(&path()).0
}

/// The config at `own` over jet's, and the files whose edits change it.
fn read(own: &Path) -> (Result<Config, String>, Vec<PathBuf>) {
    let dirs = jet_config::Dirs::from_env();
    let mut watched = dirs.watched();
    watched.push(own.to_owned());
    let result = jet_config::load_for_jetmux(&dirs, own)
        .map_err(|e| e.to_string())
        .and_then(|loaded| {
            watched.extend(loaded.sources.iter().chain(&loaded.absent).cloned());
            let mut config = from_table(loaded.config.jetmux.clone().unwrap_or_default())?;
            config.shared = loaded.config;
            Ok(config)
        });
    watched.sort();
    watched.dedup();
    (result, watched)
}

fn from_table(table: toml::Table) -> Result<Config, String> {
    toml::Value::Table(table)
        .try_into()
        .map_err(|e: toml::de::Error| format!("jetmux settings: {e}"))
}

/// Notices when any file the config came from changes, by modification
/// time.
pub struct Watcher {
    watched: Vec<PathBuf>,
    stamps: Vec<Option<SystemTime>>,
}

impl Watcher {
    /// Load the config and start watching what it came from.
    pub fn new() -> (Self, Result<Config, String>) {
        let (config, watched) = read(&path());
        let stamps = jet_config::stamps(&watched);
        (Self { watched, stamps }, config)
    }

    /// The new config if a file changed since the last call. One that
    /// doesn't load keeps the files watched, so fixing it reloads.
    pub fn poll(&mut self) -> Option<Result<Config, String>> {
        if jet_config::stamps(&self.watched) == self.stamps {
            return None;
        }
        let (config, mut watched) = read(&path());
        if config.is_err() {
            watched.extend(self.watched.iter().cloned());
            watched.sort();
            watched.dedup();
        }
        self.stamps = jet_config::stamps(&watched);
        self.watched = watched;
        Some(config)
    }
}

/// Run hooks from a daemon thread, rereading the config as it changes.
pub fn spawn_hooks(bus: &Arc<EventBus>) -> std::io::Result<()> {
    let events = bus.subscribe();
    let (mut watcher, config) = Watcher::new();
    let mut hooks = match config {
        Ok(config) => config.on,
        Err(e) => {
            eprintln!("jetmux: config: {e}");
            Hooks::default()
        }
    };
    std::thread::Builder::new()
        .name("jetmux-hooks".into())
        .spawn(move || {
            loop {
                let event = match events.recv_timeout(RELOAD_INTERVAL) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                match watcher.poll() {
                    Some(Ok(config)) => hooks = config.on,
                    Some(Err(e)) => eprintln!("jetmux: config: {e}"),
                    None => {}
                }
                if let Some(event) = event
                    && let Some(cmd) = hooks.command(&event)
                {
                    run_hook(cmd, &event);
                }
            }
        })?;
    Ok(())
}

fn run_hook(cmd: &str, event: &Event) {
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if let Ok(json) = serde_json::to_string(event) {
        command.env("JETMUX_EVENT_JSON", json);
    }
    command.env("JETMUX_EVENT_SESSION", event.session());
    match event {
        Event::Output { .. } | Event::Resized { .. } => return,
        Event::SessionCreated { .. } => command.env("JETMUX_EVENT", "session_created"),
        Event::SessionExited { .. } => command.env("JETMUX_EVENT", "session_exited"),
        Event::PaneExited { pane, .. } => command
            .env("JETMUX_EVENT", "pane_exited")
            .env("JETMUX_EVENT_PANE", pane.to_string()),
        Event::Bell { pane, .. } => command
            .env("JETMUX_EVENT", "bell")
            .env("JETMUX_EVENT_PANE", pane.to_string()),
        Event::TitleChanged { pane, title, .. } => command
            .env("JETMUX_EVENT", "title_changed")
            .env("JETMUX_EVENT_PANE", pane.to_string())
            .env("JETMUX_EVENT_TITLE", title),
        Event::Notify {
            pane, title, body, ..
        } => command
            .env("JETMUX_EVENT", "notify")
            .env("JETMUX_EVENT_PANE", pane.to_string())
            .env("JETMUX_EVENT_TITLE", title.as_deref().unwrap_or_default())
            .env("JETMUX_EVENT_BODY", body),
        Event::BlockDone { pane, block, .. } => command
            .env("JETMUX_EVENT", "block_done")
            .env("JETMUX_EVENT_PANE", pane.to_string())
            .env("JETMUX_EVENT_COMMAND", &block.command)
            .env(
                "JETMUX_EVENT_EXIT",
                block.exit.map(|e| e.to_string()).unwrap_or_default(),
            )
            .env(
                "JETMUX_EVENT_DURATION_MS",
                block.duration_ms.map(|d| d.to_string()).unwrap_or_default(),
            ),
    };
    match command.spawn() {
        // Reap it without holding up the next event.
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("jetmux-hook".into())
                .spawn(move || child.wait());
        }
        Err(e) => eprintln!("jetmux: hook {cmd:?}: {e}"),
    }
}

/// `~` and `~/...` expanded with `$HOME`.
fn expand(path: &str) -> String {
    match (path.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(home)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{home}{rest}")
        }
        _ => path.to_owned(),
    }
}

impl Profile {
    /// The session this profile describes, named `name`.
    pub fn session(&self, name: &str) -> Result<NewSession, String> {
        if self.tab.is_empty() {
            return Err(format!("profile {name} has no tabs"));
        }
        let cwd = self.cwd.as_deref().map(expand);
        let tabs = self
            .tab
            .iter()
            .map(|tab| {
                Ok(TabSpec {
                    name: tab.name.clone(),
                    layout: tab.layout(cwd.as_deref())?,
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(NewSession {
            name: Some(name.to_owned()),
            cwd,
            env: self.env.clone().into_iter().collect(),
            tabs,
            ..NewSession::default()
        })
    }

    /// A profile that rebuilds a saved layout. Pane cwds equal to the first
    /// pane's become the profile's cwd and are left out.
    pub fn from_tabs(tabs: &[TabSpec]) -> Self {
        let cwd = tabs.first().and_then(|t| first_cwd(&t.layout));
        let tab = tabs
            .iter()
            .map(|t| Node {
                name: t.name.clone(),
                ..Node::from_layout(&t.layout, cwd.as_deref())
            })
            .collect();
        Self {
            cwd,
            tab,
            ..Self::default()
        }
    }
}

fn first_cwd(layout: &LayoutSpec) -> Option<String> {
    match layout {
        LayoutSpec::Pane(p) => p.cwd.clone(),
        LayoutSpec::Split { children, .. } => children.first().and_then(|(_, c)| first_cwd(c)),
    }
}

impl Node {
    fn layout(&self, base: Option<&str>) -> Result<LayoutSpec, String> {
        let cwd = match (&self.cwd, base) {
            (Some(cwd), _) if cwd.starts_with('/') || cwd.starts_with('~') => Some(expand(cwd)),
            (Some(cwd), Some(base)) => Some(format!("{base}/{cwd}")),
            (Some(cwd), None) => Some(cwd.clone()),
            (None, base) => base.map(str::to_owned),
        };
        if self.panes.is_empty() {
            return Ok(LayoutSpec::Pane(PaneSpec {
                command: self
                    .cmd
                    .as_ref()
                    .map(|c| vec!["/bin/sh".into(), "-c".into(), c.clone()])
                    .unwrap_or_default(),
                cwd,
            }));
        }
        if self.cmd.is_some() {
            return Err("a node has either cmd or panes, not both".into());
        }
        let axis = match self.split.unwrap_or(Split::Vertical) {
            Split::Vertical => Axis::Columns,
            Split::Horizontal => Axis::Rows,
        };
        let children = self
            .panes
            .iter()
            .map(|p| Ok((p.weight.unwrap_or(1), p.layout(cwd.as_deref())?)))
            .collect::<Result<_, String>>()?;
        Ok(LayoutSpec::Split { axis, children })
    }

    fn from_layout(layout: &LayoutSpec, base: Option<&str>) -> Self {
        match layout {
            LayoutSpec::Pane(p) => Self {
                cmd: shell_command(&p.command),
                cwd: p.cwd.clone().filter(|c| Some(c.as_str()) != base),
                ..Self::default()
            },
            LayoutSpec::Split { axis, children } => Self {
                split: Some(match axis {
                    Axis::Columns => Split::Vertical,
                    Axis::Rows => Split::Horizontal,
                }),
                panes: children
                    .iter()
                    .map(|(weight, child)| Self {
                        weight: Some(*weight),
                        ..Self::from_layout(child, base)
                    })
                    .collect(),
                ..Self::default()
            },
        }
    }
}

/// A command as one `sh -c` string.
fn shell_command(command: &[String]) -> Option<String> {
    match command {
        [] => None,
        [sh, flag, cmd] if sh.ends_with("sh") && flag == "-c" => Some(cmd.clone()),
        args => Some(
            args.iter()
                .map(|a| {
                    if !a.is_empty()
                        && a.chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
                    {
                        a.clone()
                    } else {
                        format!("'{}'", a.replace('\'', "'\\''"))
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

#[cfg(test)]
mod tests {
    use jetmux_proto::{Axis, Color, LayoutSpec};
    use jetmux_render::Theme;

    use crate::config::{Config, Profile};

    /// docs/jetmux.toml loads, shared sections and all, without jet's.
    #[test]
    fn documented_config_loads() -> Result<(), String> {
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/jetmux.toml");
        let loaded = jet_config::load_for_jetmux(&jet_config::Dirs::default(), &docs)
            .map_err(|e| e.to_string())?;
        let config = crate::config::from_table(loaded.config.jetmux.unwrap_or_default())?;
        assert_eq!(config.prefix.as_deref(), Some("C-b"));
        Ok(())
    }

    const CONFIG: &str = r#"
prefix = "C-a"

[keys.normal]
"M-n" = "new-tab"

[on]
pane_exit = "notify-send done"

[profile.dev]
cwd = "/src/jetmux"

[[profile.dev.tab]]
name = "edit"
cmd = "nvim"

[[profile.dev.tab]]
name = "run"
split = "vertical"
panes = [{ cmd = "cargo watch -x test", weight = 2 }, { cwd = "crates" }]
"#;

    #[test]
    fn profiles_become_sessions_and_back() {
        let config: Config = toml::from_str(CONFIG).unwrap();
        assert_eq!(config.prefix.as_deref(), Some("C-a"));
        assert_eq!(config.on.pane_exited.as_deref(), Some("notify-send done"));
        let session = config.profile["dev"].session("dev").unwrap();
        assert_eq!(session.tabs.len(), 2);
        let LayoutSpec::Split { axis, children } = &session.tabs[1].layout else {
            panic!("expected a split");
        };
        assert_eq!(*axis, Axis::Columns);
        assert_eq!(children[0].0, 2);
        let LayoutSpec::Pane(second) = &children[1].1 else {
            panic!("expected a pane");
        };
        assert_eq!(second.cwd.as_deref(), Some("/src/jetmux/crates"));
        assert!(second.command.is_empty());

        // Saving the built layout gives back an equivalent profile.
        let saved = Profile::from_tabs(&session.tabs);
        let text = toml::to_string(&saved).unwrap();
        let reloaded: Profile = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.session("dev").unwrap().tabs, session.tabs);
        assert!(toml::from_str::<Config>("bogus = 1").is_err());
    }

    #[test]
    fn status_colors_parse_and_fill_in_defaults() {
        let config: Config = toml::from_str(
            "[bar]\nstyle = \"powerline\"\nbar = 235\nkey = \"#ff8800\"\ntext = \"default\"",
        )
        .unwrap();
        let theme = config.bar.theme();
        assert!(theme.powerline);
        assert_eq!(theme.bar, Color::Palette(235));
        assert_eq!(theme.key, Color::Rgb(255, 136, 0));
        assert_eq!(theme.text, Color::Default);
        assert_eq!(theme.pane, Theme::default().pane);
        assert!(toml::from_str::<Config>("[bar]\nbar = \"#12\"").is_err());
        assert!(toml::from_str::<Config>("[bar]\nstyle = \"fancy\"").is_err());
    }
}
