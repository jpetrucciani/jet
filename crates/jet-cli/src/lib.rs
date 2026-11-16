//! Talking to running jet windows: the protocol `jet-cli` and the window
//! share, and the local channel it travels over (see [`transport`]).
//!
//! JSON lines. A request is `{"id": 1, "cmd": "split", ...its fields}`; its
//! reply is `{"id": 1, "ok": {...}}` or `{"id": 1, "error": {"code",
//! "message"}}`. After a successful `events` request the connection carries
//! one [`Event`] per line until either side hangs up.

pub mod transport;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bumped when a request or reply changes incompatibly.
pub const VERSION: u32 = 1;

/// The variable shells inside jet find their window's channel in, and the
/// one holding their own pane id.
pub const SOCKET_VAR: &str = "JET_SOCKET";
pub const PANE_VAR: &str = "JET_PANE";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Hello,
    List,
    /// A new tab.
    Spawn(Open),
    Split(Split),
    SendText(SendText),
    GetText(Target),
    /// Focus a pane or tab, raising the window.
    Activate(Target),
    Close(Target),
    /// Pin a tab's title; `None` unpins it.
    SetTitle(SetLabel),
    /// Override a tab's icon; `None` restores its profile's.
    SetIcon(SetLabel),
    /// A user variable on a pane, as OSC 1337 SetUserVar sets.
    SetVar(SetVar),
    /// The share of its split a pane gets.
    Resize(Resize),
    ReloadConfig,
    /// Show or hide `jet quake`'s window, as its hotkey does. A second
    /// `jet quake` sends this.
    ToggleQuake,
    /// Stream events on this connection.
    Events(Events),
}

/// Which pane or tab; neither means the focused one. A pane picks its tab.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Target {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab: Option<u64>,
}

/// What to run in a new pane; unset fields mean what a keybind would do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Open {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// A program and its arguments instead of the profile's shell.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    #[default]
    Right,
    Down,
    Left,
    Up,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Split {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    pub direction: Direction,
    /// The new pane's share, 0 to 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f32>,
    #[serde(flatten)]
    pub open: Open,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendText {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    pub text: String,
    /// As a bracketed paste, so a shell doesn't run it line by line.
    #[serde(default = "yes")]
    pub paste: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetLabel {
    #[serde(default, flatten)]
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetVar {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    pub name: String,
    /// Empty unsets it.
    #[serde(default)]
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Resize {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    pub ratio: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Events {
    /// Only this pane's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    /// Only these kinds (see [`EVENT_KINDS`]); empty is all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Body {
    Ok(Answer),
    Error(Failure),
}

/// What a request returns: which shape follows from the request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Answer {
    Hello(Hello),
    List(Listing),
    Opened(Opened),
    Text(Text),
    Done(Done),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub pid: u32,
    pub quake: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub pid: u32,
    pub quake: bool,
    pub tabs: Vec<TabInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: u64,
    /// 1-based position, as Alt+N counts.
    pub index: usize,
    pub title: String,
    pub active: bool,
    pub panes: Vec<PaneInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: u64,
    pub title: String,
    pub focused: bool,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    pub pane: u64,
    pub tab: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

impl Failure {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// Something that happened in a window, for `events` subscribers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(flatten)]
    pub what: What,
    /// The window's process.
    pub window: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum What {
    PaneOpened,
    PaneClosed,
    Focus,
    Title {
        title: String,
    },
    Cwd {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
    },
    Bell,
    /// `state` is hidden, normal, error, indeterminate or paused.
    Progress {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        percent: Option<u8>,
    },
    Var {
        name: String,
        value: String,
    },
    /// A program asked for a desktop notification (OSC 9, OSC 777).
    Notification {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        body: String,
    },
    /// A command finished (OSC 133;D), with its exit status if the shell
    /// gave one.
    CommandFinished {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<i32>,
    },
    ConfigReloaded,
}

/// Every `event` name, for filters.
pub const EVENT_KINDS: &[&str] = &[
    "pane-opened",
    "pane-closed",
    "focus",
    "title",
    "cwd",
    "bell",
    "progress",
    "var",
    "notification",
    "command-finished",
    "config-reloaded",
];

impl What {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::PaneOpened => "pane-opened",
            Self::PaneClosed => "pane-closed",
            Self::Focus => "focus",
            Self::Title { .. } => "title",
            Self::Cwd { .. } => "cwd",
            Self::Bell => "bell",
            Self::Progress { .. } => "progress",
            Self::Var { .. } => "var",
            Self::Notification { .. } => "notification",
            Self::CommandFinished { .. } => "command-finished",
            Self::ConfigReloaded => "config-reloaded",
        }
    }
}

impl Events {
    pub fn wants(&self, event: &Event) -> bool {
        let pane = self.pane.is_none() || self.pane == event.pane;
        let kind = self.types.is_empty() || self.types.iter().any(|t| t == event.what.kind());
        pane && kind
    }
}

#[cfg(test)]
mod tests {
    use crate::{Answer, Body, Direction, Envelope, Event, Opened, Reply, Request, What};

    #[test]
    fn requests_are_flat_json() -> Result<(), Box<dyn std::error::Error>> {
        let line = r#"{"id":3,"cmd":"split","pane":2,"direction":"down","profile":"pwsh"}"#;
        let env: Envelope = serde_json::from_str(line)?;
        let Request::Split(split) = &env.request else {
            return Err(format!("not a split: {env:?}").into());
        };
        assert_eq!(
            (env.id, split.pane, split.direction),
            (3, Some(2), Direction::Down)
        );
        assert_eq!(split.open.profile.as_deref(), Some("pwsh"));
        // Fields are optional; the id too.
        let env: Envelope = serde_json::from_str(r#"{"cmd":"get-text"}"#)?;
        assert_eq!(env.id, 0);
        assert!(serde_json::from_str::<Envelope>(r#"{"cmd":"nope"}"#).is_err());
        Ok(())
    }

    #[test]
    fn replies_and_events_round_trip() -> Result<(), serde_json::Error> {
        let reply = Reply {
            id: 7,
            body: Body::Ok(Answer::Opened(Opened { pane: 4, tab: 2 })),
        };
        let text = serde_json::to_string(&reply)?;
        assert_eq!(text, r#"{"id":7,"ok":{"pane":4,"tab":2}}"#);
        assert_eq!(serde_json::from_str::<Reply>(&text)?, reply);
        let event = Event {
            what: What::Title {
                title: "vim".into(),
            },
            window: 9,
            tab: Some(1),
            pane: Some(4),
        };
        let text = serde_json::to_string(&event)?;
        assert_eq!(
            text,
            r#"{"event":"title","title":"vim","window":9,"tab":1,"pane":4}"#
        );
        Ok(())
    }
}
