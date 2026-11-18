//! Saved session state, shared by persistence (`jetmux restore` after a
//! reboot) and hot upgrade (a new binary taking over live sessions). The
//! format is versioned, and fields added later need serde defaults, so a
//! newer binary can always read what an older one wrote.

use jetmux_proto::{LayoutSpec, Size};
use serde::{Deserialize, Serialize};

use crate::PaneId;

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub name: String,
    /// The profile the session was started from, if any. Its declared
    /// commands are safe to run again on restore.
    #[serde(default)]
    pub profile: Option<String>,
    pub size: Size,
    /// Index into `tabs` of the tab new clients start on.
    pub active_tab: usize,
    pub tabs: Vec<TabSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub name: Option<String>,
    /// The layout, with each pane's command and cwd at its leaf.
    pub layout: LayoutSpec,
    /// One per layout leaf, in leaf order.
    pub panes: Vec<PaneSnapshot>,
    pub focus: Option<PaneId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub id: PaneId,
    pub title: String,
    pub size: Size,
    /// The last command run at the pane's shell prompt (OSC 133).
    #[serde(default)]
    pub last_command: Option<String>,
    /// Screen and scrollback as VT, to paint above a restored shell.
    #[serde(default)]
    pub history: Option<String>,
    /// Set only for hot upgrade: the running pane to adopt.
    #[serde(default)]
    pub live: Option<Live>,
}

/// A pane that keeps running across a hot upgrade.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Live {
    /// The PTY master, inherited across exec.
    pub fd: i32,
    pub pid: i32,
    /// Exact terminal state, from [`crate::Vt::state`].
    pub primary: String,
    pub alt: Option<String>,
}
