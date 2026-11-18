//! What a pane reports besides its screen: the shell's directory, progress,
//! notifications, user variables, context, pointer shape and finished
//! commands. jet-core parses these out of the output; the daemon passes
//! them to its clients as [`PaneEvent`]s.

use serde::{Deserialize, Serialize};

/// One thing a pane reported, for clients that show more than the grid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum PaneEvent {
    /// Where the pane is: what the shell says (OSC 7, OSC 9;9), else
    /// where the program in front is. `None` once that's unknown, as while
    /// ssh is in front of a shell that doesn't say.
    Location(Option<Location>),
    Progress(Progress),
    Notify(Notification),
    /// A user variable (OSC 1337 SetUserVar); empty `value` unsets it.
    Var {
        name: String,
        value: String,
    },
    /// The innermost context (OSC 3008), `None` once the last one ended.
    Context(Option<Context>),
    /// The mouse pointer by its CSS name (OSC 22), empty to reset it.
    Pointer(String),
    /// A command finished (OSC 133;D), with its exit status if given.
    Finished(Option<i32>),
}

/// Where a shell says it is. Over SSH that's the remote host and a path
/// there, which means nothing locally but is worth showing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Location {
    /// From OSC 7's `file://host/...`; none for OSC 9;9 or an empty host.
    pub host: Option<String>,
    pub path: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Progress {
    /// No progress (state 0): clear it.
    #[default]
    Hidden,
    Normal(u8),
    Error(u8),
    /// Busy, no percentage.
    Indeterminate,
    Paused(u8),
}

/// A desktop notification a program asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Notification {
    /// Empty for OSC 9, which has only a body.
    pub title: String,
    pub body: String,
    pub occasion: Occasion,
}

/// When a notification is worth showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Occasion {
    /// Unless you're looking at the pane it came from (OSC 9, 777, and
    /// kitty's `o=unfocused`).
    #[default]
    Unwatched,
    /// Even then (kitty's default).
    Always,
    /// Only while the window isn't focused (`o=invisible`).
    Hidden,
}

/// What kind of context (OSC 3008 `type=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ContextKind {
    Boot,
    Container,
    Vm,
    /// More privilege: `run0`, `sudo`.
    Elevate,
    /// Different privilege.
    Chpriv,
    Subcontext,
    Remote,
    Shell,
    Command,
    App,
    Service,
    Session,
}

/// One context from OSC 3008: who and where, as its issuer reported it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Context {
    pub kind: Option<ContextKind>,
    pub user: Option<String>,
    pub hostname: Option<String>,
    pub comm: Option<String>,
    pub cwd: Option<String>,
    pub container: Option<String>,
    pub vm: Option<String>,
    pub target_user: Option<String>,
    pub target_host: Option<String>,
}

impl Context {
    /// The host its programs run on: the target of a `remote`, else the
    /// issuer's.
    pub fn host(&self) -> Option<&str> {
        match self.kind {
            Some(ContextKind::Remote) => self.target_host.as_deref(),
            _ => None,
        }
        .or(self.hostname.as_deref())
    }

    /// The user its programs run as: the target user if it has one.
    pub fn run_as(&self) -> Option<&str> {
        self.target_user.as_deref().or(self.user.as_deref())
    }
}
