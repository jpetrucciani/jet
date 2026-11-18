//! The tools agents get: sessions and panes to work in, commands run to
//! completion with their exit status and output (via OSC 133 blocks), the
//! screen, and waits on text and events.

use std::sync::mpsc;
use std::time::Duration;

use jetmux_client::RpcClient;
use jetmux_proto::rpc::{
    BlockInfo, BlockState, Call, Captured, Created, Event, Matched, PaneCreated, PaneInfo,
    SessionInfo,
};
use jetmux_proto::{Direction, NewSession, Size};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Most output a tool returns; agents get the tail, which is where errors
/// usually are.
const MAX_OUTPUT: usize = 16 * 1024;
const DEFAULT_RUN_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;
/// New sessions are wide: agents read more than they watch.
const AGENT_SIZE: Size = Size {
    cols: 160,
    rows: 48,
};
/// Without shell integration, how long to give a command before reading
/// the screen.
const SETTLE: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize)]
pub struct ToolDef {
    name: &'static str,
    description: &'static str,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

const TARGET: &str =
    "A pane: `session`, `session:tab.pane`, or `session:%id` (ids from list_panes).";

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

pub fn list() -> Vec<ToolDef> {
    let target = json!({ "type": "string", "description": TARGET });
    let timeout = json!({ "type": "integer", "description": "Milliseconds to wait at most." });
    vec![
        ToolDef {
            name: "list_sessions",
            description: "List jetmux sessions with their tab and pane counts.",
            input_schema: schema(json!({}), &[]),
        },
        ToolDef {
            name: "list_panes",
            description: "List a session's panes: ids, titles, working directories, commands, which is focused.",
            input_schema: schema(json!({ "session": { "type": "string" } }), &["session"]),
        },
        ToolDef {
            name: "create_session",
            description: "Start a session (a shell unless `command` is given). Returns its name.",
            input_schema: schema(
                json!({
                    "name": { "type": "string" },
                    "command": { "type": "string", "description": "Run with /bin/sh -c instead of a shell." },
                    "cwd": { "type": "string" }
                }),
                &[],
            ),
        },
        ToolDef {
            name: "split_pane",
            description: "Split a pane, running a shell or `command` in the new one. Returns the new pane's target.",
            input_schema: schema(
                json!({
                    "target": target,
                    "direction": { "type": "string", "enum": ["right", "down"] },
                    "command": { "type": "string" },
                    "cwd": { "type": "string" }
                }),
                &["target"],
            ),
        },
        ToolDef {
            name: "close_pane",
            description: "Close a pane, ending its process.",
            input_schema: schema(json!({ "target": target }), &["target"]),
        },
        ToolDef {
            name: "run_command",
            description: "Type a command at a pane's shell prompt and wait for it to finish. Returns the exit status and output. Needs shell integration (`eval \"$(jetmux shell-init bash)\"`, zsh too; fish has it built in); without it, returns the screen after a moment instead.",
            input_schema: schema(
                json!({ "target": target, "command": { "type": "string" }, "timeout_ms": timeout }),
                &["target", "command"],
            ),
        },
        ToolDef {
            name: "send_keys",
            description: "Send input to a pane: `text` as typed, then `keys` by name (Enter, Tab, Esc, Up, C-c, M-x, ...).",
            input_schema: schema(
                json!({
                    "target": target,
                    "text": { "type": "string" },
                    "keys": { "type": "array", "items": { "type": "string" } }
                }),
                &["target"],
            ),
        },
        ToolDef {
            name: "read_screen",
            description: "A pane's visible screen as text, or with `scrollback` its whole history.",
            input_schema: schema(
                json!({ "target": target, "scrollback": { "type": "boolean" } }),
                &["target"],
            ),
        },
        ToolDef {
            name: "list_blocks",
            description: "The commands run in a pane (OSC 133 blocks): id, command, exit status, duration.",
            input_schema: schema(json!({ "target": target }), &["target"]),
        },
        ToolDef {
            name: "read_block",
            description: "A block's output; the last finished one without `id`.",
            input_schema: schema(
                json!({ "target": target, "id": { "type": "integer" } }),
                &["target"],
            ),
        },
        ToolDef {
            name: "wait_for",
            description: "Wait until a line on a pane's screen matches a regex. Returns the line.",
            input_schema: schema(
                json!({ "target": target, "pattern": { "type": "string" }, "timeout_ms": timeout }),
                &["target", "pattern"],
            ),
        },
        ToolDef {
            name: "wait_event",
            description: "Wait for the next event: pane_exited, bell, title_changed, notify, block_done, session_created, session_exited.",
            input_schema: schema(
                json!({
                    "session": { "type": "string" },
                    "types": { "type": "array", "items": { "type": "string" } },
                    "timeout_ms": timeout
                }),
                &[],
            ),
        },
    ]
}

/// What a tool hands back: text for the model, and the same as data.
pub struct Output {
    pub text: String,
    pub data: Value,
}

fn output(text: String, data: impl Serialize) -> Result<Output, String> {
    Ok(Output {
        text,
        data: serde_json::to_value(data).map_err(|e| e.to_string())?,
    })
}

pub struct Tools {
    pub server: String,
    /// Sessions agents may touch; empty means all.
    pub allow: Vec<String>,
}

#[derive(Deserialize)]
struct SessionArg {
    session: String,
}

#[derive(Deserialize)]
struct TargetArg {
    target: String,
}

#[derive(Deserialize)]
struct CreateArgs {
    name: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct SplitArgs {
    target: String,
    #[serde(default)]
    direction: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct RunArgs {
    target: String,
    command: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct KeysArgs {
    target: String,
    text: Option<String>,
    #[serde(default)]
    keys: Vec<String>,
}

#[derive(Deserialize)]
struct ScreenArgs {
    target: String,
    #[serde(default)]
    scrollback: bool,
}

#[derive(Deserialize)]
struct BlockArgs {
    target: String,
    id: Option<u64>,
}

#[derive(Deserialize)]
struct WaitArgs {
    target: String,
    pattern: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct EventArgs {
    session: Option<String>,
    #[serde(default)]
    types: Vec<String>,
    timeout_ms: Option<u64>,
}

fn args<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|e| format!("bad arguments: {e}"))
}

fn shell(command: Option<String>) -> Vec<String> {
    command.map_or_else(Vec::new, |c| vec!["/bin/sh".into(), "-c".into(), c])
}

/// The tail of `text`, at most [`MAX_OUTPUT`] bytes, on a char boundary.
fn tail(text: &str) -> (&str, bool) {
    if text.len() <= MAX_OUTPUT {
        return (text, false);
    }
    let mut start = text.len() - MAX_OUTPUT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (&text[start..], true)
}

/// Bytes for a key name, as in config.toml; anything else is text.
fn key_bytes(name: &str) -> String {
    let named = match name {
        "Enter" => "\r",
        "Tab" => "\t",
        "BTab" => "\x1b[Z",
        "Esc" => "\x1b",
        "BSpace" => "\x7f",
        "Space" => " ",
        "Up" => "\x1b[A",
        "Down" => "\x1b[B",
        "Right" => "\x1b[C",
        "Left" => "\x1b[D",
        "Home" => "\x1b[H",
        "End" => "\x1b[F",
        "PageUp" => "\x1b[5~",
        "PageDown" => "\x1b[6~",
        "Delete" => "\x1b[3~",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_owned();
    }
    if let Some(rest) = name.strip_prefix("M-") {
        return format!("\x1b{}", key_bytes(rest));
    }
    if let Some(c) = name.strip_prefix("C-").filter(|r| r.chars().count() == 1) {
        let c = c.chars().next().unwrap_or('@').to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            return char::from(c as u8 - b'a' + 1).to_string();
        }
    }
    name.to_owned()
}

fn event_type(event: &Event) -> &'static str {
    match event {
        Event::SessionCreated { .. } => "session_created",
        Event::SessionExited { .. } => "session_exited",
        Event::PaneExited { .. } => "pane_exited",
        Event::Bell { .. } => "bell",
        Event::TitleChanged { .. } => "title_changed",
        Event::Notify { .. } => "notify",
        Event::Output { .. } => "output",
        Event::Resized { .. } => "resized",
        Event::BlockDone { .. } => "block_done",
    }
}

impl Tools {
    fn rpc<T: DeserializeOwned>(&self, call: Call) -> Result<T, String> {
        RpcClient::connect(&self.server)
            .and_then(|mut c| c.call_as(call))
            .map_err(|e| e.to_string())
    }

    fn allowed(&self, session: &str) -> Result<(), String> {
        if self.allow.is_empty() || self.allow.iter().any(|s| s == session) {
            Ok(())
        } else {
            Err(format!(
                "session {session} isn't open to agents (jetmux-mcp --allow)"
            ))
        }
    }

    /// The target, checked against the allowed sessions.
    fn target(&self, target: String) -> Result<Option<String>, String> {
        let session = target.split(':').next().unwrap_or_default();
        if session.is_empty() && !self.allow.is_empty() {
            return Err("name the session in the target".into());
        }
        if !session.is_empty() {
            self.allowed(session)?;
        }
        Ok(Some(target))
    }

    pub fn call(&self, name: &str, arguments: Value) -> Result<Output, String> {
        match name {
            "list_sessions" => {
                let sessions: Vec<SessionInfo> = self.rpc(Call::SessionList {})?;
                let sessions: Vec<SessionInfo> = sessions
                    .into_iter()
                    .filter(|s| self.allowed(&s.name).is_ok())
                    .collect();
                let text = sessions
                    .iter()
                    .map(|s| format!("{}: {} tabs, {} panes", s.name, s.tabs, s.panes))
                    .collect::<Vec<_>>()
                    .join("\n");
                output(text, sessions)
            }
            "list_panes" => {
                let a: SessionArg = args(arguments)?;
                let target = self.target(a.session)?;
                let panes: Vec<PaneInfo> = self.rpc(Call::PaneList { target })?;
                let text = panes
                    .iter()
                    .map(|p| {
                        format!(
                            "{}:%{} tab {} {} in {}{}",
                            p.session,
                            p.id,
                            p.tab,
                            if p.command.is_empty() {
                                "shell".to_owned()
                            } else {
                                p.command.join(" ")
                            },
                            p.cwd.as_deref().unwrap_or("?"),
                            if p.focused { " (focused)" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                output(text, panes)
            }
            "create_session" => {
                let a: CreateArgs = args(arguments)?;
                if let Some(name) = &a.name {
                    self.allowed(name)?;
                } else if !self.allow.is_empty() {
                    return Err("name the session: only allowed names may be created".into());
                }
                let created: Created = self.rpc(Call::SessionCreate(NewSession {
                    name: a.name,
                    command: shell(a.command),
                    cwd: a.cwd,
                    size: AGENT_SIZE,
                    ..NewSession::default()
                }))?;
                output(format!("created session {}", created.session), created)
            }
            "split_pane" => {
                let a: SplitArgs = args(arguments)?;
                let direction = match a.direction.as_deref() {
                    Some("down") => Direction::Down,
                    _ => Direction::Right,
                };
                let session = a.target.split(':').next().unwrap_or_default().to_owned();
                let created: PaneCreated = self.rpc(Call::PaneSplit {
                    target: self.target(a.target)?,
                    direction,
                    command: shell(a.command),
                    cwd: a.cwd,
                })?;
                output(format!("{session}:%{}", created.pane), created)
            }
            "close_pane" => {
                let a: TargetArg = args(arguments)?;
                let target = self.target(a.target)?;
                let _: Value = self.rpc(Call::PaneClose { target })?;
                output("closed".into(), Value::Null)
            }
            "run_command" => self.run_command(args(arguments)?),
            "send_keys" => {
                let a: KeysArgs = args(arguments)?;
                let mut keys = a.text.unwrap_or_default();
                keys.extend(a.keys.iter().map(|k| key_bytes(k)));
                let target = self.target(a.target)?;
                let _: Value = self.rpc(Call::PaneSendKeys { target, keys })?;
                output("sent".into(), Value::Null)
            }
            "read_screen" => {
                let a: ScreenArgs = args(arguments)?;
                let captured: Captured = self.rpc(Call::PaneCapture {
                    target: self.target(a.target)?,
                    scrollback: a.scrollback,
                })?;
                let (text, _) = tail(&captured.text);
                output(text.to_owned(), &captured)
            }
            "list_blocks" => {
                let a: TargetArg = args(arguments)?;
                let target = self.target(a.target)?;
                let blocks: Vec<BlockInfo> = self.rpc(Call::PaneBlocks { target })?;
                let text = blocks
                    .iter()
                    .map(|b| {
                        let status = match (b.state, b.exit) {
                            (BlockState::Done, Some(code)) => format!("exit {code}"),
                            (state, _) => format!("{state:?}").to_lowercase(),
                        };
                        format!("{} [{status}] {}", b.id, b.command)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                output(text, blocks)
            }
            "read_block" => {
                let a: BlockArgs = args(arguments)?;
                let captured: Captured = self.rpc(Call::PaneReadBlock {
                    target: self.target(a.target)?,
                    block: a.id,
                })?;
                let (text, _) = tail(&captured.text);
                output(text.to_owned(), &captured)
            }
            "wait_for" => {
                let a: WaitArgs = args(arguments)?;
                let matched: Matched = self.rpc(Call::PaneWait {
                    target: self.target(a.target)?,
                    pattern: Some(a.pattern),
                    block_done: false,
                    after_block: None,
                    timeout_ms: Some(a.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)),
                })?;
                output(matched.line.clone(), matched)
            }
            "wait_event" => self.wait_event(args(arguments)?),
            other => Err(format!("no tool named {other}")),
        }
    }

    fn run_command(&self, a: RunArgs) -> Result<Output, String> {
        let target = self.target(a.target)?;
        let blocks: Vec<BlockInfo> = self.rpc(Call::PaneBlocks {
            target: target.clone(),
        })?;
        if blocks
            .last()
            .is_some_and(|b| b.state == BlockState::Running)
        {
            return Err("the pane is still running a command; wait_for it or send C-c".into());
        }
        let keys = format!("{}\r", a.command);
        let send = |keys: String| -> Result<(), String> {
            let _: Value = self.rpc(Call::PaneSendKeys {
                target: target.clone(),
                keys,
            })?;
            Ok(())
        };
        if blocks.is_empty() {
            send(keys)?;
            std::thread::sleep(SETTLE);
            let captured: Captured = self.rpc(Call::PaneCapture {
                target,
                scrollback: false,
            })?;
            let text = format!(
                "(no shell integration in this pane, so no exit status; this is the screen. Run `eval \"$(jetmux shell-init bash)\"` there for exact results.)\n{}",
                captured.text
            );
            return output(text, &captured);
        }
        // Anything newer than the last finished block is this command, even
        // if it finishes before the wait below starts.
        let after = blocks
            .iter()
            .filter(|b| b.state == BlockState::Done)
            .map(|b| b.id)
            .max()
            .unwrap_or(0);
        send(keys)?;
        let timeout = a.timeout_ms.unwrap_or(DEFAULT_RUN_TIMEOUT_MS);
        let done: BlockInfo = self
            .rpc(Call::PaneWait {
                target: target.clone(),
                pattern: None,
                block_done: true,
                after_block: Some(after),
                timeout_ms: Some(timeout),
            })
            .map_err(|e| {
                format!(
                    "{e}: still running after {timeout} ms; read_screen to look, or send_keys C-c"
                )
            })?;
        let captured: Captured = self.rpc(Call::PaneReadBlock {
            target,
            block: Some(done.id),
        })?;
        let (out, truncated) = tail(&captured.text);
        let exit = done.exit.map_or("?".into(), |e| e.to_string());
        let secs = done.duration_ms.unwrap_or(0) as f64 / 1000.0;
        let text = format!(
            "exit {exit} after {secs:.1}s{}\n{out}",
            if truncated {
                " (output cut to its end)"
            } else {
                ""
            }
        );
        output(
            text,
            json!({
                "exit": done.exit,
                "duration_ms": done.duration_ms,
                "output": out,
                "truncated": truncated,
            }),
        )
    }

    fn wait_event(&self, a: EventArgs) -> Result<Output, String> {
        if let Some(session) = &a.session {
            self.allowed(session)?;
        }
        let mut client = RpcClient::connect(&self.server).map_err(|e| e.to_string())?;
        client
            .call(Call::EventsSubscribe {
                session: a.session.clone(),
            })
            .map_err(|e| e.to_string())?;
        let handle = client.shutdown_handle().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel();
        let allow = self.allow.clone();
        std::thread::spawn(move || {
            while let Ok(event) = client.next_event() {
                let wanted = (a.types.is_empty()
                    || a.types.iter().any(|t| t == event_type(&event)))
                    && (allow.is_empty() || allow.iter().any(|s| s == event.session()));
                if wanted && tx.send(event).is_err() {
                    return;
                }
            }
        });
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS));
        let event = rx.recv_timeout(timeout);
        let _ = handle.shutdown(std::net::Shutdown::Both);
        let event = event.map_err(|_| "no matching event before the timeout".to_owned())?;
        let text = serde_json::to_string(&event).map_err(|e| e.to_string())?;
        output(text, event)
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::{MAX_OUTPUT, key_bytes, list, tail};

    #[test]
    fn keys_and_output_tails() {
        assert_eq!(key_bytes("C-c"), "\x03");
        assert_eq!(key_bytes("M-x"), "\x1bx");
        assert_eq!(key_bytes("Enter"), "\r");
        assert_eq!(key_bytes("ls"), "ls");
        let long = "日".repeat(MAX_OUTPUT);
        let (end, cut) = tail(&long);
        assert!(cut && end.len() <= MAX_OUTPUT && end.chars().all(|c| c == '日'));
        assert_eq!(tail("short"), ("short", false));
    }

    #[test]
    fn every_tool_has_an_object_schema() {
        for tool in list() {
            let schema = serde_json::to_value(&tool).unwrap();
            assert_eq!(
                schema["inputSchema"]["type"], "object",
                "{}",
                schema["name"]
            );
        }
    }
}
