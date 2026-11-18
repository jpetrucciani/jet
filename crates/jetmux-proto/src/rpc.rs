//! The scripting API: JSON-RPC 2.0 messages carried in
//! [`FrameKind::Rpc`](crate::FrameKind::Rpc) frames.
//!
//! A connection whose first frame is an RPC frame stays an RPC connection.
//! Requests are answered in order. After `events.subscribe` the connection
//! only carries `event` notifications.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Direction, NewSession, Size, TabSpec};

pub const VERSION: &str = "2.0";

/// Standard JSON-RPC error codes, plus one for failed calls.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_PARAMS: i32 = -32602;
pub const CALL_FAILED: i32 = -32000;

/// Every method. Params use these field names; `target` is a target string
/// (see [`Target::parse`]) and may be omitted wherever it is optional.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Call {
    #[serde(rename = "session.list")]
    SessionList {},
    #[serde(rename = "session.create")]
    SessionCreate(NewSession),
    #[serde(rename = "session.kill")]
    SessionKill { target: String },
    #[serde(rename = "server.stop")]
    ServerStop {},
    /// Save every session for `server.restore`, now.
    #[serde(rename = "server.snapshot")]
    ServerSnapshot {},
    /// Bring back the sessions the last snapshot holds that aren't running.
    #[serde(rename = "server.restore")]
    ServerRestore {},
    /// Replace the daemon with `binary` (default: the daemon's own), keeping
    /// every session and process.
    #[serde(rename = "server.upgrade")]
    ServerUpgrade {
        #[serde(default)]
        binary: Option<String>,
    },
    #[serde(rename = "tab.list")]
    TabList {
        #[serde(default)]
        target: Option<String>,
    },
    #[serde(rename = "tab.new")]
    TabNew {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    #[serde(rename = "tab.close")]
    TabClose {
        #[serde(default)]
        target: Option<String>,
    },
    #[serde(rename = "tab.select")]
    TabSelect {
        #[serde(default)]
        target: Option<String>,
    },
    #[serde(rename = "tab.rename")]
    TabRename {
        #[serde(default)]
        target: Option<String>,
        name: String,
    },
    #[serde(rename = "tab.move")]
    TabMove {
        #[serde(default)]
        target: Option<String>,
        delta: i16,
    },
    #[serde(rename = "pane.list")]
    PaneList {
        #[serde(default)]
        target: Option<String>,
    },
    #[serde(rename = "pane.split")]
    PaneSplit {
        #[serde(default)]
        target: Option<String>,
        direction: Direction,
        #[serde(default)]
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    #[serde(rename = "pane.close")]
    PaneClose {
        #[serde(default)]
        target: Option<String>,
    },
    /// Focus the target, or its neighbor in `direction`.
    #[serde(rename = "pane.focus")]
    PaneFocus {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        direction: Option<Direction>,
    },
    #[serde(rename = "pane.zoom")]
    PaneZoom {
        #[serde(default)]
        target: Option<String>,
    },
    #[serde(rename = "pane.swap")]
    PaneSwap {
        #[serde(default)]
        target: Option<String>,
        direction: Direction,
    },
    #[serde(rename = "pane.resize")]
    PaneResize {
        #[serde(default)]
        target: Option<String>,
        direction: Direction,
        cells: u16,
    },
    /// Write `keys` to the pane as if typed.
    #[serde(rename = "pane.send_keys")]
    PaneSendKeys {
        #[serde(default)]
        target: Option<String>,
        keys: String,
    },
    /// The pane's visible screen as text, or its whole scrollback.
    #[serde(rename = "pane.capture")]
    PaneCapture {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        scrollback: bool,
    },
    /// Wait until a line on the pane's screen matches `pattern` (a regex),
    /// or with `block_done`, until the next command block finishes.
    #[serde(rename = "pane.wait")]
    PaneWait {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        pattern: Option<String>,
        #[serde(default)]
        block_done: bool,
        /// With `block_done`: wait for a block newer than this id, so a
        /// command that finishes before the wait starts still counts.
        /// Default: the newest finished block when the wait starts.
        #[serde(default)]
        after_block: Option<u64>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// The pane's command blocks (OSC 133), oldest first.
    #[serde(rename = "pane.blocks")]
    PaneBlocks {
        #[serde(default)]
        target: Option<String>,
    },
    /// A block's output as text; the last finished block's by default.
    #[serde(rename = "pane.read_block")]
    PaneReadBlock {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        block: Option<u64>,
    },
    /// The session's tabs and layouts, in the shape `session.create` takes.
    #[serde(rename = "layout.save")]
    LayoutSave {
        #[serde(default)]
        target: Option<String>,
    },
    /// Turn this connection into a stream of one pane's raw output: the
    /// answer has its visible screen as VT, then `output` and `resized`
    /// events follow until the pane exits. For recorders.
    #[serde(rename = "pane.stream")]
    PaneStream {
        #[serde(default)]
        target: Option<String>,
    },
    /// Turn this connection into a stream of `event` notifications, for one
    /// session or all of them.
    #[serde(rename = "events.subscribe")]
    EventsSubscribe {
        #[serde(default)]
        session: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(flatten)]
    pub call: Call,
}

/// A request as received, before its method and params are checked, so a
/// bad call can still be answered with its id.
#[derive(Deserialize)]
struct RawRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

impl Request {
    pub fn new(id: u64, call: Call) -> Self {
        Self {
            jsonrpc: VERSION,
            id: Some(id.into()),
            call,
        }
    }

    /// Parse a request. On failure, returns the error to answer with and the
    /// request's id if it got that far.
    pub fn parse(payload: &[u8]) -> Result<Self, (Option<Value>, RpcError)> {
        let raw: RawRequest = serde_json::from_slice(payload)
            .map_err(|e| (None, RpcError::new(PARSE_ERROR, e.to_string())))?;
        // Methods without params may leave them out.
        let params = raw
            .params
            .unwrap_or_else(|| Value::Object(Default::default()));
        let tagged = serde_json::json!({ "method": raw.method, "params": params });
        match serde_json::from_value(tagged) {
            Ok(call) => Ok(Self {
                jsonrpc: VERSION,
                id: raw.id,
                call,
            }),
            Err(e) => Err((raw.id, RpcError::new(INVALID_PARAMS, e.to_string()))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(CALL_FAILED, message)
    }
}

/// A response or an `event` notification; which one is told apart by
/// `method`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Event>,
}

impl Reply {
    pub fn response(id: Option<Value>, result: Result<Value, RpcError>) -> Self {
        let (result, error) = match result {
            Ok(value) => (Some(value), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            jsonrpc: VERSION.into(),
            id,
            result,
            error,
            method: None,
            params: None,
        }
    }

    pub fn event(event: Event) -> Self {
        Self {
            jsonrpc: VERSION.into(),
            id: None,
            result: None,
            error: None,
            method: Some("event".into()),
            params: Some(event),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Event {
    SessionCreated {
        session: String,
    },
    SessionExited {
        session: String,
    },
    PaneExited {
        session: String,
        pane: u32,
    },
    Bell {
        session: String,
        pane: u32,
    },
    TitleChanged {
        session: String,
        pane: u32,
        title: String,
    },
    /// The app asked for a desktop notification (OSC 9 or OSC 777).
    Notify {
        session: String,
        pane: u32,
        title: Option<String>,
        body: String,
    },
    /// Raw PTY output, base64. Only on `pane.stream` connections.
    Output {
        session: String,
        pane: u32,
        data: String,
    },
    /// The pane's size changed. Only on `pane.stream` connections.
    Resized {
        session: String,
        pane: u32,
        size: Size,
    },
    /// A command block finished.
    BlockDone {
        session: String,
        pane: u32,
        block: BlockInfo,
    },
}

impl Event {
    pub fn session(&self) -> &str {
        match self {
            Self::SessionCreated { session }
            | Self::SessionExited { session }
            | Self::PaneExited { session, .. }
            | Self::Bell { session, .. }
            | Self::TitleChanged { session, .. }
            | Self::Notify { session, .. }
            | Self::Output { session, .. }
            | Self::Resized { session, .. }
            | Self::BlockDone { session, .. } => session,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SessionInfo {
    pub name: String,
    pub clients: u32,
    pub tabs: u32,
    pub panes: u32,
    /// Size of the first tab's first pane.
    pub size: Size,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Created {
    pub session: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TabInfo {
    pub session: String,
    /// One-based, as shown in the status bar.
    pub index: u16,
    pub name: String,
    pub panes: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PaneInfo {
    pub session: String,
    /// One-based tab index.
    pub tab: u16,
    /// One-based position in the tab's layout order.
    pub index: u16,
    pub id: u32,
    pub title: String,
    pub cwd: Option<String>,
    pub command: Vec<String>,
    pub size: Size,
    /// Focused in the view of the most recently active client.
    pub focused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCreated {
    pub pane: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Captured {
    pub text: String,
}

/// The answer to `pane.stream`: where the stream starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Streaming {
    pub session: String,
    pub pane: u32,
    pub size: Size,
    pub title: String,
    /// The visible screen as VT, cursor and modes included, to start from.
    pub screen: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub path: String,
    pub sessions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restored {
    pub sessions: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum BlockState {
    Prompt,
    Running,
    Done,
}

/// One command block: a prompt, the command typed there, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BlockInfo {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: u64,
    pub command: String,
    pub state: BlockState,
    pub exit: Option<i32>,
    /// How long the command ran, or has run so far.
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matched {
    pub line: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub tabs: Vec<TabSpec>,
}

/// Where a call applies: `session`, `session:tab`, `session:tab.pane`,
/// `session:%id`, `:tab`, `%id` or `` (everything defaulted). Tabs are
/// one-based indexes or names; panes are one-based positions in the tab or
/// `%id`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub session: Option<String>,
    pub tab: Option<TabRef>,
    pub pane: Option<PaneRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabRef {
    Index(u16),
    Name(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneRef {
    Index(u16),
    Id(u32),
}

impl Target {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (session, rest) = match s.split_once(':') {
            Some((session, rest)) => (session, rest),
            None if s.starts_with('%') => ("", s),
            None => (s, ""),
        };
        let session = (!session.is_empty()).then(|| session.to_owned());
        let pane_id = |p: &str| {
            p.parse::<u32>()
                .map(PaneRef::Id)
                .map_err(|_| format!("bad pane id in target {s:?}"))
        };
        if let Some(id) = rest.strip_prefix('%') {
            return Ok(Self {
                session,
                tab: None,
                pane: Some(pane_id(id)?),
            });
        }
        let (tab, pane) = match rest.rsplit_once('.') {
            Some((tab, pane)) => (tab, Some(pane)),
            None => (rest, None),
        };
        let tab = match tab {
            "" => None,
            t => Some(match t.parse::<u16>() {
                Ok(i) if i > 0 => TabRef::Index(i),
                Ok(_) => return Err(format!("tab indexes start at 1 in target {s:?}")),
                Err(_) => TabRef::Name(t.to_owned()),
            }),
        };
        let pane = match pane {
            None | Some("") => None,
            Some(p) => Some(match p.strip_prefix('%') {
                Some(id) => pane_id(id)?,
                None => match p.parse::<u16>() {
                    Ok(i) if i > 0 => PaneRef::Index(i),
                    _ => return Err(format!("bad pane in target {s:?}")),
                },
            }),
        };
        Ok(Self { session, tab, pane })
    }
}

#[cfg(test)]
mod tests {
    use crate::rpc::{Call, PaneRef, Request, TabRef, Target};
    use crate::{Direction, NewSession};

    #[test]
    fn requests_round_trip_and_params_are_optional() {
        let req = Request::new(
            7,
            Call::PaneSplit {
                target: Some("dev:run.1".into()),
                direction: Direction::Right,
                command: vec![],
                cwd: None,
            },
        );
        let json = serde_json::to_vec(&req).unwrap();
        assert_eq!(Request::parse(&json).unwrap(), req);

        let list = Request::parse(br#"{"jsonrpc":"2.0","id":1,"method":"session.list"}"#);
        assert_eq!(list.unwrap().call, Call::SessionList {});
        let create = Request::parse(br#"{"id":2,"method":"session.create","params":{"name":"x"}}"#);
        assert_eq!(
            create.unwrap().call,
            Call::SessionCreate(NewSession {
                name: Some("x".into()),
                ..NewSession::default()
            })
        );
        let (id, err) = Request::parse(br#"{"id":3,"method":"nope"}"#).unwrap_err();
        assert_eq!(id, Some(3.into()));
        assert_eq!(err.code, crate::rpc::INVALID_PARAMS);
    }

    #[test]
    fn targets_parse() {
        let t = |s| Target::parse(s).unwrap();
        assert_eq!(t(""), Target::default());
        assert_eq!(t("dev").session.as_deref(), Some("dev"));
        assert_eq!(
            t("dev:run.2"),
            Target {
                session: Some("dev".into()),
                tab: Some(TabRef::Name("run".into())),
                pane: Some(PaneRef::Index(2)),
            }
        );
        assert_eq!(t(":3").tab, Some(TabRef::Index(3)));
        assert_eq!(t("%4").pane, Some(PaneRef::Id(4)));
        assert_eq!(t("dev:%4").session.as_deref(), Some("dev"));
        assert!(Target::parse("dev:0").is_err());
        assert!(Target::parse("dev:1.x").is_err());
    }
}
