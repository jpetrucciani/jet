//! `jetmux-mcp`: an MCP server (stdio) that lets agents work in jetmux:
//! create panes, run commands and get their exit status and output, read
//! the screen and command blocks, and wait on output and events.

mod tools;

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tools::Tools;

const USAGE: &str = "\
usage: jetmux-mcp [-L server] [--allow session]...

An MCP server on stdin/stdout for agents. With --allow (repeatable), agents
may only see and touch those sessions, and only create sessions by those
names. For example, in an MCP client config:

  { \"command\": \"jetmux-mcp\", \"args\": [\"--allow\", \"agent\"] }";

/// Protocol versions this server speaks, newest first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "jetmux is a terminal multiplexer. Work in a session's panes: \
run_command types a command at a shell prompt and returns its exit status and output; \
read_screen shows what's on screen for interactive programs; send_keys drives them. \
Targets look like `session`, `session:2.1` (tab 2, pane 1) or `session:%3` (pane id 3).";

#[derive(Debug, Deserialize)]
struct Incoming {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    #[serde(default)]
    protocol_version: String,
}

#[derive(Debug, Deserialize)]
struct CallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Serialize)]
struct Error {
    code: i64,
    message: String,
}

fn handle(tools: &Tools, method: &str, params: Value) -> Result<Value, Error> {
    let bad = |e: serde_json::Error| Error {
        code: -32602,
        message: e.to_string(),
    };
    match method {
        "initialize" => {
            let params: InitializeParams = serde_json::from_value(params).unwrap_or_default();
            let version = VERSIONS
                .iter()
                .find(|v| **v == params.protocol_version)
                .or(VERSIONS.first())
                .copied()
                .unwrap_or_default();
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "jetmux-mcp", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::list() })),
        "tools/call" => {
            let call: CallParams = serde_json::from_value(params).map_err(bad)?;
            // A tool that fails is a result the model should see, not a
            // protocol error.
            Ok(match tools.call(&call.name, call.arguments) {
                Ok(out) => json!({
                    "content": [{ "type": "text", "text": out.text }],
                    "structuredContent": { "result": out.data },
                    "isError": false,
                }),
                Err(e) => json!({
                    "content": [{ "type": "text", "text": e }],
                    "isError": true,
                }),
            })
        }
        other => Err(Error {
            code: -32601,
            message: format!("no method {other}"),
        }),
    }
}

fn main() {
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        // writeln, not println: a closed pipe isn't worth a panic
        let _ = writeln!(
            std::io::stdout(),
            "jetmux-mcp {}",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    let mut args = std::env::args().skip(1);
    let mut tools = Tools {
        server: "default".into(),
        allow: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("-L", Some(server)) => tools.server = server,
            ("--allow", Some(session)) => tools.allow.push(session),
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Incoming>(&line) {
            // Notifications (no id) get no answer.
            Ok(Incoming { id: None, .. }) => continue,
            Ok(Incoming {
                id: Some(id),
                method,
                params,
            }) => match handle(&tools, &method, params) {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
            },
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": e.to_string() },
            }),
        };
        if writeln!(stdout, "{reply}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
}
