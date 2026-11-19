mod attach;
mod config;
mod daemon;
mod keymap;
mod keys;
mod remote;
mod rpc;
mod session;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use jetmux_client::{Client, RpcClient};
use jetmux_proto::rpc::{
    BlockInfo, BlockState, Call, Captured, Created, Event, Layout, Matched, PaneCreated, PaneInfo,
    Request, Restored, Saved, SessionInfo, TabInfo,
};
use jetmux_proto::{ClientMsg, Direction, NewSession, ServerMsg, Size};
use serde::Serialize;
use serde_json::Value;

use crate::keys::Key;

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a client waits for an upgraded daemon to come back.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DETACHED_SIZE: Size = Size { cols: 80, rows: 24 };

const USAGE: &str = "\
usage: jetmux [-L server] [--json] [command]

sessions:
  attach [-t session]          attach (default: most recent, or a new session)
  new [-s name] [-d] [-c dir] [-- cmd]
                               create a session and attach (-d: stay detached)
  up profile [-s name] [-d]    attach to a profile's session, creating it first
  ls                           list sessions
  kill -t session              kill a session
  kill-server                  stop the daemon and every session
  snapshot                     save every session for restore (also done
                               every [persist] interval, and at shutdown)
  restore                      bring back saved sessions that aren't running:
                               shells in their old cwd under their old
                               scrollback, last command typed but not run
  upgrade                      replace the daemon with this binary, keeping
                               every session and process
  remote [user@]host [-t session] [--predict adaptive|always|never]
                               attach to a session on another machine over
                               UDP (mosh-style): survives lost packets, sleep
                               and changing networks, and echoes typing at
                               once on slow links. Needs jetmux on the host
                               and UDP 60001-61000 open to it; set up over
                               ssh (--ssh 'ssh -p 2222', --server-command
                               path, --connect host:port to override)
  daemon                       run the daemon in the foreground
  proxy                        connect stdin and stdout to the daemon (started
    [-s name --up profile]     if need be), for clients elsewhere:
                               `ssh host jetmux proxy`; with --up, start
                               session name from profile first

scripting (-t target: session, session:tab, session:tab.pane, session:%id;
tabs are 1-based indexes or names, panes 1-based or %id; inside jetmux the
target defaults to the current pane):
  tab list|close|select [-t target]
  tab new [-t target] [-n name] [-c dir] [-- cmd]
  tab rename [-t target] name
  tab move [-t target] delta
  pane list|close|zoom [-t target]
  pane split [-t target] [right|down|left|up] [-c dir] [-- cmd]
  pane focus [-t target] [left|right|up|down]
  pane swap [-t target] left|right|up|down
  pane resize [-t target] left|right|up|down [cells]
  pane send-keys [-t target] [-l] keys...
                               key names (Enter, C-c, M-x, Up) are translated
                               unless -l; anything else is sent as text
  pane capture [-t target] [-S]
                               print the screen (-S: with scrollback)
  pane wait [-t target] [--timeout secs] regex|--block-done
                               wait for a screen line to match, or for the
                               running command to finish (fails if it did);
                               `jetmux wait` is short for this
  pane blocks [-t target]      list command blocks (needs shell integration)
  pane read-block [-t target] [id]
                               print a block's output (default: the last)
  shell-init bash|zsh          print shell integration for blocks
  layout save [-t session] [-n profile]
                               print the session as a config profile
  events [-t session]          stream events as JSON lines
  rpc method [params-json]     make a raw socket API call

keys (after the ctrl-b prefix; rebind in config.toml):
  d detach            c new tab            n / N next / prev tab   1-9 go to tab
  % or | split right  \" or - split down    h j k l / arrows focus  H J K L resize
  x close pane        & close tab          z zoom                  o next pane
  { } rotate panes    , rename tab         [ copy mode             ] paste
  p pane mode         t tab mode           : command palette       ctrl-b send ctrl-b
without prefix: alt-h/j/k/l focus, alt-1..9 go to tab, click to focus,
wheel to scroll
copy mode: vi motions, v / V / ctrl-v select, y yank, / ? n N search, q quit";

type BoxError = Box<dyn std::error::Error>;

#[derive(Debug, PartialEq, Eq)]
struct Cli {
    server: String,
    json: bool,
    cmd: Cmd,
}

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Attach {
        target: Option<String>,
    },
    New {
        name: Option<String>,
        detached: bool,
        cwd: Option<String>,
        command: Vec<String>,
    },
    Up {
        profile: String,
        name: Option<String>,
        detached: bool,
    },
    List,
    Kill {
        target: String,
    },
    KillServer,
    /// Pipe stdin and stdout to the daemon's socket.
    Proxy {
        /// Made from this profile first unless it's running.
        up: Option<(String, String)>,
    },
    Daemon {
        resume: Option<String>,
    },
    /// A socket API call; `target` gets the current-pane default first.
    Call {
        call: Call,
    },
    LayoutSave {
        target: Option<String>,
        profile: Option<String>,
    },
    Events {
        session: Option<String>,
    },
    Help,
    Version,
    /// Print the OSC 133 hooks for a shell.
    ShellInit(&'static str),
    Snapshot,
    Restore,
    Upgrade,
    Remote(remote::ConnectArgs),
    RemoteServe(remote::ServeArgs),
}

impl Cmd {
    /// Prints and exits, so it can die of SIGPIPE. Not the ones that
    /// attach or serve: a socket closing under them would kill them with
    /// the terminal still raw, or take a daemon down.
    fn prints_and_exits(&self) -> bool {
        match self {
            Cmd::New { detached, .. } | Cmd::Up { detached, .. } => *detached,
            Cmd::List
            | Cmd::Kill { .. }
            | Cmd::KillServer
            | Cmd::Call { .. }
            | Cmd::LayoutSave { .. }
            | Cmd::Events { .. }
            | Cmd::Help
            | Cmd::Version
            | Cmd::ShellInit(_)
            | Cmd::Snapshot
            | Cmd::Restore => true,
            Cmd::Attach { .. }
            | Cmd::Proxy { .. }
            | Cmd::Daemon { .. }
            | Cmd::Upgrade
            | Cmd::Remote(_)
            | Cmd::RemoteServe(_) => false,
        }
    }
}

/// Flags and positional arguments of one subcommand.
#[derive(Debug, Default)]
struct Args {
    values: BTreeMap<&'static str, String>,
    switches: Vec<&'static str>,
    positional: Vec<String>,
    /// Everything after `--`.
    rest: Vec<String>,
}

impl Args {
    /// Parse `args` given the flags that take values and the ones that don't.
    fn parse(
        args: impl Iterator<Item = String>,
        valued: &[&'static str],
        switches: &[&'static str],
    ) -> Result<Self, String> {
        let mut out = Self::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            if arg == "--" {
                out.rest = args.by_ref().collect();
            } else if let Some(flag) = valued.iter().find(|f| **f == arg) {
                let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
                out.values.insert(flag, value);
            } else if let Some(flag) = switches.iter().find(|f| **f == arg) {
                out.switches.push(flag);
            } else if arg.starts_with('-') && arg.len() > 1 && arg.parse::<i64>().is_err() {
                return Err(format!("unknown option {arg}"));
            } else {
                out.positional.push(arg);
            }
        }
        Ok(out)
    }

    fn value(&mut self, flag: &str) -> Option<String> {
        self.values.remove(flag)
    }

    fn switch(&self, flag: &str) -> bool {
        self.switches.contains(&flag)
    }

    fn at_most(&self, n: usize, usage: &str) -> Result<(), String> {
        if self.positional.len() > n {
            return Err(format!("unexpected {}; usage: {usage}", self.positional[n]));
        }
        Ok(())
    }
}

fn direction(word: &str) -> Result<Direction, String> {
    match word {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        other => Err(format!("{other} is not left, right, up or down")),
    }
}

/// `send-keys` arguments as bytes: key names become their sequences unless
/// `literal`.
fn key_bytes(args: &[String], literal: bool) -> String {
    args.iter()
        .map(|arg| match Key::parse(arg) {
            Ok(key) if !literal && arg.chars().count() > 1 => {
                String::from_utf8_lossy(&key.bytes()).into_owned()
            }
            _ => arg.clone(),
        })
        .collect()
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let mut args = args.into_iter().peekable();
    let mut server = "default".to_owned();
    let mut json = false;

    while let Some(arg) = args.next_if(|a: &String| a.starts_with('-')) {
        match arg.as_str() {
            "-L" => server = args.next().ok_or("-L needs a value")?,
            "--json" => json = true,
            "-h" | "--help" => {
                return Ok(Cli {
                    server,
                    json,
                    cmd: Cmd::Help,
                });
            }
            "-V" | "--version" => {
                return Ok(Cli {
                    server,
                    json,
                    cmd: Cmd::Version,
                });
            }
            other => return Err(format!("unknown option {other}")),
        }
    }

    let word = args.next();
    let sub = match word.as_deref() {
        Some("tab" | "pane" | "layout") => args.next(),
        _ => None,
    };
    let cmd = match (word.as_deref(), sub.as_deref()) {
        (None | Some("attach" | "a"), _) => {
            let mut a = Args::parse(args, &["-t"], &[])?;
            a.at_most(0, "jetmux attach [-t session]")?;
            Cmd::Attach {
                target: a.value("-t"),
            }
        }
        (Some("new" | "n"), _) => {
            let mut a = Args::parse(args, &["-s", "-c"], &["-d"])?;
            a.at_most(0, "jetmux new [-s name] [-d] [-c dir] [-- cmd]")?;
            Cmd::New {
                name: a.value("-s"),
                detached: a.switch("-d"),
                cwd: a.value("-c"),
                command: a.rest,
            }
        }
        (Some("up"), _) => {
            let mut a = Args::parse(args, &["-s"], &["-d"])?;
            a.at_most(1, "jetmux up profile [-s name] [-d]")?;
            Cmd::Up {
                profile: a
                    .positional
                    .pop()
                    .ok_or("usage: jetmux up profile [-s name] [-d]")?,
                name: a.value("-s"),
                detached: a.switch("-d"),
            }
        }
        (Some("ls" | "list"), _) => {
            Args::parse(args, &[], &[])?.at_most(0, "jetmux ls")?;
            Cmd::List
        }
        (Some("kill"), _) => {
            let mut a = Args::parse(args, &["-t"], &[])?;
            a.at_most(0, "jetmux kill -t session")?;
            Cmd::Kill {
                target: a.value("-t").ok_or("usage: jetmux kill -t session")?,
            }
        }
        (Some("kill-server"), _) => Cmd::KillServer,
        (Some("proxy"), _) => {
            let usage = "jetmux proxy [-s session --up profile]";
            let mut a = Args::parse(args, &["-s", "--up"], &[])?;
            a.at_most(0, usage)?;
            let up = match (a.value("-s"), a.value("--up")) {
                (Some(name), Some(profile)) => Some((name, profile)),
                (None, None) => None,
                _ => return Err(format!("usage: {usage}")),
            };
            Cmd::Proxy { up }
        }
        (Some("daemon"), _) => {
            let mut a = Args::parse(args, &["--resume"], &[])?;
            a.at_most(0, "jetmux daemon")?;
            Cmd::Daemon {
                resume: a.value("--resume"),
            }
        }
        (Some("snapshot"), _) => {
            Args::parse(args, &[], &[])?.at_most(0, "jetmux snapshot")?;
            Cmd::Snapshot
        }
        (Some("restore"), _) => {
            Args::parse(args, &[], &[])?.at_most(0, "jetmux restore")?;
            Cmd::Restore
        }
        (Some("upgrade"), _) => {
            Args::parse(args, &[], &[])?.at_most(0, "jetmux upgrade")?;
            Cmd::Upgrade
        }
        (Some("remote"), _) => {
            const USE: &str = "jetmux remote [user@]host [-t session] [--predict adaptive|always|never] [--ssh cmd] [--server-command cmd] [--connect host:port]";
            let mut a = Args::parse(
                args,
                &["-t", "--ssh", "--server-command", "--predict", "--connect"],
                &["--local"],
            )?;
            a.at_most(1, USE)?;
            let predict = match a.value("--predict").as_deref() {
                None | Some("adaptive") => remote::Predict::Adaptive,
                Some("always") => remote::Predict::Always,
                Some("never") => remote::Predict::Never,
                Some(other) => return Err(format!("bad --predict {other}")),
            };
            Cmd::Remote(remote::ConnectArgs {
                host: a.positional.pop().ok_or(format!("usage: {USE}"))?,
                target: a.value("-t"),
                ssh: a.value("--ssh"),
                server_command: a.value("--server-command"),
                predict,
                connect: a.value("--connect"),
                local: a.switch("--local"),
            })
        }
        (Some("remote-serve"), _) => {
            let mut a = Args::parse(args, &["-t", "--size", "--ports"], &["--run"])?;
            a.at_most(
                0,
                "jetmux remote-serve [-t session] [--size COLSxROWS] [--ports low-high]",
            )?;
            Cmd::RemoteServe(remote::ServeArgs {
                server: String::new(),
                target: a.value("-t"),
                size: a.value("--size"),
                ports: a.value("--ports"),
                run: a.switch("--run"),
            })
        }
        (Some("help"), _) => Cmd::Help,
        (Some("tab"), sub) => Cmd::Call {
            call: tab_call(sub, args)?,
        },
        (Some("pane"), sub) => Cmd::Call {
            call: pane_call(sub, args)?,
        },
        (Some("shell-init"), _) => {
            let mut a = Args::parse(args, &[], &[])?;
            a.at_most(1, "jetmux shell-init bash|zsh")?;
            Cmd::ShellInit(match a.positional.pop().as_deref() {
                Some("bash") => BASH_INIT,
                Some("zsh") => ZSH_INIT,
                Some("fish") => return Err("fish 4 marks its prompts itself".into()),
                _ => return Err("usage: jetmux shell-init bash|zsh".into()),
            })
        }
        (Some("wait"), _) => Cmd::Call {
            call: pane_call(Some("wait"), args)?,
        },
        (Some("layout"), Some("save")) => {
            let mut a = Args::parse(args, &["-t", "-n"], &[])?;
            a.at_most(0, "jetmux layout save [-t session] [-n profile]")?;
            Cmd::LayoutSave {
                target: a.value("-t"),
                profile: a.value("-n"),
            }
        }
        (Some("layout"), _) => return Err("usage: jetmux layout save [-t session]".into()),
        (Some("events"), _) => {
            let mut a = Args::parse(args, &["-t"], &[])?;
            a.at_most(0, "jetmux events [-t session]")?;
            Cmd::Events {
                session: a.value("-t"),
            }
        }
        (Some("rpc"), _) => {
            let a = Args::parse(args, &[], &[])?;
            a.at_most(2, "jetmux rpc method [params-json]")?;
            let mut words = a.positional.into_iter();
            let method = words
                .next()
                .ok_or("usage: jetmux rpc method [params-json]")?;
            let params: Value = match words.next() {
                Some(text) => {
                    serde_json::from_str(&text).map_err(|e| format!("bad params: {e}"))?
                }
                None => Value::Null,
            };
            let request = serde_json::json!({ "method": method, "params": params });
            let call = Request::parse(request.to_string().as_bytes())
                .map_err(|(_, e)| e.message)?
                .call;
            Cmd::Call { call }
        }
        (Some(other), _) => return Err(format!("unknown command {other}")),
    };
    Ok(Cli { server, json, cmd })
}

fn tab_call(sub: Option<&str>, args: impl Iterator<Item = String>) -> Result<Call, String> {
    let (valued, switches): (&[&'static str], &[&'static str]) = match sub {
        Some("new") => (&["-t", "-n", "-c"], &[]),
        _ => (&["-t"], &[]),
    };
    let mut a = Args::parse(args, valued, switches)?;
    let target = a.value("-t");
    Ok(match sub {
        Some("list" | "ls") => Call::TabList { target },
        Some("new") => Call::TabNew {
            target,
            name: a.value("-n"),
            command: std::mem::take(&mut a.rest),
            cwd: a.value("-c"),
        },
        Some("close") => Call::TabClose { target },
        Some("select") => Call::TabSelect { target },
        Some("rename") => {
            a.at_most(1, "jetmux tab rename [-t target] name")?;
            Call::TabRename {
                target,
                name: a.positional.pop().ok_or("usage: jetmux tab rename name")?,
            }
        }
        Some("move") => {
            a.at_most(1, "jetmux tab move [-t target] delta")?;
            let delta = a.positional.pop().ok_or("usage: jetmux tab move delta")?;
            Call::TabMove {
                target,
                delta: delta.parse().map_err(|_| format!("bad delta {delta}"))?,
            }
        }
        _ => return Err("usage: jetmux tab list|new|close|select|rename|move".into()),
    })
    .and_then(|call| {
        if !matches!(sub, Some("rename" | "move")) {
            a.at_most(0, "see jetmux help")?;
        }
        Ok(call)
    })
}

fn pane_call(sub: Option<&str>, args: impl Iterator<Item = String>) -> Result<Call, String> {
    let (valued, switches): (&[&'static str], &[&'static str]) = match sub {
        Some("split") => (&["-t", "-c"], &[]),
        Some("send-keys") => (&["-t"], &["-l"]),
        Some("capture") => (&["-t"], &["-S"]),
        Some("wait") => (&["-t", "--timeout"], &["--block-done"]),
        _ => (&["-t"], &[]),
    };
    let mut a = Args::parse(args, valued, switches)?;
    let target = a.value("-t");
    let dir = |a: &Args, i: usize| a.positional.get(i).map(|w| direction(w)).transpose();
    let call = match sub {
        Some("list" | "ls") => Call::PaneList { target },
        Some("split") => {
            a.at_most(1, "jetmux pane split [right|down|left|up] [-- cmd]")?;
            Call::PaneSplit {
                target,
                direction: dir(&a, 0)?.unwrap_or(Direction::Right),
                command: std::mem::take(&mut a.rest),
                cwd: a.value("-c"),
            }
        }
        Some("close") => Call::PaneClose { target },
        Some("zoom") => Call::PaneZoom { target },
        Some("focus") => {
            a.at_most(1, "jetmux pane focus [left|right|up|down]")?;
            Call::PaneFocus {
                target,
                direction: dir(&a, 0)?,
            }
        }
        Some("swap") => {
            a.at_most(1, "jetmux pane swap left|right|up|down")?;
            Call::PaneSwap {
                target,
                direction: dir(&a, 0)?.ok_or("usage: jetmux pane swap left|right|up|down")?,
            }
        }
        Some("resize") => {
            a.at_most(2, "jetmux pane resize left|right|up|down [cells]")?;
            let cells = match a.positional.get(1) {
                Some(n) => n.parse().map_err(|_| format!("bad cell count {n}"))?,
                None => 1,
            };
            Call::PaneResize {
                target,
                direction: dir(&a, 0)?.ok_or("usage: jetmux pane resize direction [cells]")?,
                cells,
            }
        }
        Some("send-keys") => {
            let mut words = std::mem::take(&mut a.positional);
            words.append(&mut a.rest);
            Call::PaneSendKeys {
                target,
                keys: key_bytes(&words, a.switch("-l")),
            }
        }
        Some("capture") => Call::PaneCapture {
            target,
            scrollback: a.switch("-S"),
        },
        Some("wait") => {
            a.at_most(1, "jetmux pane wait [--timeout secs] regex|--block-done")?;
            let timeout = a
                .value("--timeout")
                .map(|t| {
                    t.parse::<f64>()
                        .map_err(|_| format!("bad timeout {t}"))
                        .map(|secs| Duration::from_secs_f64(secs.max(0.0)))
                })
                .transpose()?;
            let block_done = a.switch("--block-done");
            let pattern = a.positional.pop();
            if pattern.is_some() == block_done {
                return Err("usage: jetmux pane wait [--timeout secs] regex|--block-done".into());
            }
            Call::PaneWait {
                target,
                pattern,
                block_done,
                after_block: None,
                timeout_ms: timeout.map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
            }
        }
        Some("blocks") => Call::PaneBlocks { target },
        Some("read-block") => {
            a.at_most(1, "jetmux pane read-block [-t target] [id]")?;
            let block = a
                .positional
                .pop()
                .map(|id| id.parse().map_err(|_| format!("bad block id {id}")))
                .transpose()?;
            Call::PaneReadBlock { target, block }
        }
        _ => {
            return Err(
                "usage: jetmux pane list|split|close|focus|zoom|swap|resize|send-keys|capture|wait|blocks|read-block"
                    .into(),
            );
        }
    };
    if !matches!(
        sub,
        Some("split" | "focus" | "swap" | "resize" | "send-keys" | "wait")
    ) {
        a.at_most(0, "see jetmux help")?;
    }
    Ok(call)
}

/// OSC 133 marks for bash: `eval "$(jetmux shell-init bash)"`. `D` goes out
/// before every prompt; jetmux ignores it when nothing ran.
const BASH_INIT: &str = r#"if [[ -n $JETMUX ]]; then
  _jetmux_prompt() { local s=$?; printf '\e]133;D;%s\a\e]133;A\a' "$s"; return $s; }
  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
    PROMPT_COMMAND=(_jetmux_prompt "${PROMPT_COMMAND[@]}")
  else
    PROMPT_COMMAND="_jetmux_prompt${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
  fi
  PS1="$PS1"'\[\e]133;B\a\]'
  PS0='\e]133;C\a'"${PS0:-}"
fi
"#;

/// OSC 133 marks for zsh: `eval "$(jetmux shell-init zsh)"`.
const ZSH_INIT: &str = r#"if [[ -n $JETMUX ]]; then
  _jetmux_precmd() { printf '\e]133;D;%s\a\e]133;A\a' $?; }
  _jetmux_preexec() { printf '\e]133;C\a'; }
  precmd_functions=(_jetmux_precmd $precmd_functions)
  preexec_functions+=(_jetmux_preexec)
  PS1="$PS1%{"$'\e]133;B\a'"%}"
fi
"#;

fn main() -> ExitCode {
    let cli = match parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("jetmux: {e}\nrun `jetmux help` for usage");
            return ExitCode::from(2);
        }
    };
    if cli.cmd.prints_and_exits() {
        // End quietly when the reader goes (`jetmux ls | head`), as Unix
        // tools do, instead of panicking in println!.
        // SAFETY: no other threads yet; this restores the default action.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jetmux: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Inside an jetmux pane on this server, targets default to that pane, and
/// session-less targets (`:2`, `%3`) mean this session.
fn default_target(server: &str, target: Option<String>) -> Option<String> {
    let here = std::env::var_os("JETMUX")
        .is_some_and(|sock| jetmux_client::socket_path(server).as_os_str() == sock);
    let session = std::env::var("JETMUX_SESSION").ok().filter(|_| here);
    match (target, session) {
        (None, Some(session)) => match std::env::var("JETMUX_PANE") {
            Ok(pane) => Some(format!("{session}:%{pane}")),
            Err(_) => Some(session),
        },
        (Some(t), Some(session)) if t.starts_with([':', '%']) => {
            let t = t.strip_prefix(':').unwrap_or(&t);
            Some(format!("{session}:{t}"))
        }
        (target, _) => target,
    }
}

/// The call's target with [`default_target`] applied.
fn with_default_target(server: &str, mut call: Call) -> Call {
    match &mut call {
        Call::TabList { target }
        | Call::TabNew { target, .. }
        | Call::TabClose { target }
        | Call::TabSelect { target }
        | Call::TabRename { target, .. }
        | Call::TabMove { target, .. }
        | Call::PaneList { target }
        | Call::PaneSplit { target, .. }
        | Call::PaneClose { target }
        | Call::PaneFocus { target, .. }
        | Call::PaneZoom { target }
        | Call::PaneSwap { target, .. }
        | Call::PaneResize { target, .. }
        | Call::PaneSendKeys { target, .. }
        | Call::PaneCapture { target, .. }
        | Call::PaneWait { target, .. }
        | Call::PaneBlocks { target }
        | Call::PaneReadBlock { target, .. }
        | Call::PaneStream { target }
        | Call::LayoutSave { target } => *target = default_target(server, target.take()),
        Call::SessionList {}
        | Call::SessionCreate(_)
        | Call::SessionKill { .. }
        | Call::ServerStop {}
        | Call::ServerSnapshot {}
        | Call::ServerRestore {}
        | Call::ServerUpgrade { .. }
        | Call::EventsSubscribe { .. } => {}
    }
    call
}

fn rpc_connect(server: &str) -> Result<RpcClient, BoxError> {
    match RpcClient::connect(server) {
        Ok(client) => Ok(client),
        Err(jetmux_client::Error::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(format!("no daemon is running on server {server}").into())
        }
        Err(e) => Err(e.into()),
    }
}

fn print_json(value: &impl Serialize) -> Result<(), BoxError> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn run(cli: Cli) -> Result<(), BoxError> {
    let server = cli.server.as_str();
    match cli.cmd {
        Cmd::Help => println!("{USAGE}"),
        Cmd::Version => println!("jetmux {}", env!("CARGO_PKG_VERSION")),
        Cmd::ShellInit(script) => print!("{script}"),
        Cmd::Daemon { resume } => daemon::run(server, resume.as_deref().map(Path::new))?,
        Cmd::Snapshot => {
            let saved: Saved = rpc_connect(server)?.call_as(Call::ServerSnapshot {})?;
            println!("saved {} to {}", saved.sessions.join(", "), saved.path);
        }
        Cmd::Restore => {
            // After a reboot nothing is running yet.
            connect_or_start(server)?;
            let restored: Restored = rpc_connect(server)?.call_as(Call::ServerRestore {})?;
            if restored.sessions.is_empty() {
                println!("nothing to restore");
            } else {
                println!("restored {}", restored.sessions.join(", "));
            }
        }
        Cmd::Upgrade => upgrade(server)?,
        Cmd::Remote(args) => remote::connect(server, args)?,
        Cmd::RemoteServe(args) => remote::serve(remote::ServeArgs {
            server: server.to_owned(),
            ..args
        })?,
        Cmd::List => {
            let sessions: Vec<SessionInfo> = rpc_connect(server)?.call_as(Call::SessionList {})?;
            if cli.json {
                return print_json(&sessions);
            }
            if sessions.is_empty() {
                println!("no sessions");
            }
            for s in sessions {
                let plural =
                    |n: u32, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
                println!(
                    "{}: {}, {}, {}x{} ({})",
                    s.name,
                    plural(s.tabs, "tab"),
                    plural(s.panes, "pane"),
                    s.size.cols,
                    s.size.rows,
                    plural(s.clients, "client"),
                );
            }
        }
        Cmd::Kill { target } => {
            rpc_connect(server)?.call(Call::SessionKill { target })?;
        }
        Cmd::Proxy { up } => {
            if let Some((name, profile)) = up {
                start_profile(server, &name, &profile)?;
            }
            proxy(server)?;
        }
        Cmd::KillServer => {
            rpc_connect(server)?.call(Call::ServerStop {})?;
        }
        Cmd::Attach { target } => {
            let size = attach::terminal_size();
            let create = NewSession {
                size,
                cwd: current_dir(),
                ..NewSession::default()
            };
            let client = connect_or_start(server)?;
            attach_with(
                server,
                client,
                &ClientMsg::Attach {
                    target,
                    size,
                    create: Some(create),
                    follow: false,
                },
            )?;
        }
        Cmd::New {
            name,
            detached,
            cwd,
            command,
        } => {
            let spec = NewSession {
                name,
                command,
                cwd: cwd.or_else(current_dir),
                size: if detached {
                    DETACHED_SIZE
                } else {
                    attach::terminal_size()
                },
                ..NewSession::default()
            };
            if detached {
                connect_or_start(server)?;
                let created: Created = rpc_connect(server)?.call_as(Call::SessionCreate(spec))?;
                println!("{}", created.session);
            } else {
                attach_with(server, connect_or_start(server)?, &ClientMsg::New(spec))?;
            }
        }
        Cmd::Up {
            profile,
            name,
            detached,
        } => {
            let name = name.unwrap_or_else(|| profile.clone());
            if detached {
                start_profile(server, &name, &profile)?;
                println!("{name}");
            } else {
                let mut spec = profile_session(&name, &profile)?;
                spec.size = attach::terminal_size();
                let size = spec.size;
                attach_with(
                    server,
                    connect_or_start(server)?,
                    &ClientMsg::Attach {
                        target: Some(name),
                        size,
                        create: Some(spec),
                        follow: false,
                    },
                )?;
            }
        }
        Cmd::Call { call } => {
            let call = with_default_target(server, call);
            let value = rpc_connect(server)?.call(call.clone())?;
            if cli.json {
                return print_json(&value);
            }
            print_result(&call, value)?;
        }
        Cmd::LayoutSave { target, profile } => {
            let target = default_target(server, target);
            let session = target
                .as_deref()
                .map(|t| t.split(':').next().unwrap_or(t).to_owned());
            let mut rpc = rpc_connect(server)?;
            let layout: Layout = rpc.call_as(Call::LayoutSave {
                target: session.clone(),
            })?;
            if cli.json {
                return print_json(&layout);
            }
            let name = match (profile, session) {
                (Some(name), _) => name,
                (None, Some(session)) if !session.is_empty() => session,
                _ => rpc
                    .call_as::<Vec<SessionInfo>>(Call::SessionList {})?
                    .into_iter()
                    .last()
                    .map_or_else(|| "saved".to_owned(), |s| s.name),
            };
            let profile = config::Profile::from_tabs(&layout.tabs);
            let doc = BTreeMap::from([("profile", BTreeMap::from([(name, profile)]))]);
            print!("{}", toml::to_string(&doc)?);
        }
        Cmd::Events { session } => {
            let mut rpc = rpc_connect(server)?;
            rpc.call(Call::EventsSubscribe { session })?;
            loop {
                let event: Event = rpc.next_event()?;
                println!("{}", serde_json::to_string(&event)?);
            }
        }
    }
    Ok(())
}

/// Print a call's result for people.
/// `id status duration command`, as `pane blocks` lists blocks.
fn block_line(b: &BlockInfo) -> String {
    let status = match (b.state, b.exit) {
        (BlockState::Prompt, _) => "prompt".to_owned(),
        (BlockState::Running, _) => "running".to_owned(),
        (BlockState::Done, Some(exit)) => format!("exit {exit}"),
        (BlockState::Done, None) => "done".to_owned(),
    };
    let duration = b
        .duration_ms
        .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
        .unwrap_or_default();
    format!("{} {status} {duration} {}", b.id, b.command)
}

fn print_result(call: &Call, value: Value) -> Result<(), BoxError> {
    match call {
        Call::TabList { .. } => {
            let tabs: Vec<TabInfo> = serde_json::from_value(value)?;
            for t in tabs {
                let panes: Vec<String> = t.panes.iter().map(|p| format!("%{p}")).collect();
                println!("{}:{} {} [{}]", t.session, t.index, t.name, panes.join(" "));
            }
        }
        Call::PaneList { .. } => {
            let panes: Vec<PaneInfo> = serde_json::from_value(value)?;
            for p in panes {
                println!(
                    "{}:{}.{} %{} {}x{} {}{}",
                    p.session,
                    p.tab,
                    p.index,
                    p.id,
                    p.size.cols,
                    p.size.rows,
                    p.cwd.unwrap_or_default(),
                    if p.focused { " (focused)" } else { "" }
                );
            }
        }
        Call::TabNew { .. } | Call::PaneSplit { .. } => {
            let created: PaneCreated = serde_json::from_value(value)?;
            println!("%{}", created.pane);
        }
        Call::PaneCapture { .. } | Call::PaneReadBlock { .. } => {
            let captured: Captured = serde_json::from_value(value)?;
            println!("{}", captured.text);
        }
        Call::PaneWait {
            block_done: true, ..
        } => {
            let block: BlockInfo = serde_json::from_value(value)?;
            println!("{}", block_line(&block));
            // Scripts can chain on the command's own success.
            if block.exit.is_some_and(|e| e != 0) {
                return Err(format!("{} exited {}", block.command, block.exit.unwrap_or(0)).into());
            }
        }
        Call::PaneWait { .. } => {
            let matched: Matched = serde_json::from_value(value)?;
            println!("{}", matched.line);
        }
        Call::PaneBlocks { .. } => {
            let blocks: Vec<BlockInfo> = serde_json::from_value(value)?;
            for block in &blocks {
                println!("{}", block_line(block));
            }
        }
        _ if value.is_null() => {}
        _ => print_json(&value)?,
    }
    Ok(())
}

fn attach_with(server: &str, mut client: Client, msg: &ClientMsg) -> Result<(), BoxError> {
    let session = match client.request(msg)? {
        ServerMsg::Attached { session } => session,
        other => return Err(jetmux_client::unexpected(other).into()),
    };
    let mut stream = client.into_stream();
    loop {
        match attach::run(stream)? {
            attach::Outcome::Detached => println!("[detached from {session}]"),
            attach::Outcome::Exited => println!("[exited {session}]"),
            attach::Outcome::Disconnected => println!("[lost connection to {session}]"),
            attach::Outcome::Reconnect => {
                stream = reattach(server, &session)?;
                continue;
            }
        }
        return Ok(());
    }
}

/// Attach to `session` again once the daemon that replaced the last one is
/// up.
fn reattach(server: &str, session: &str) -> Result<std::os::unix::net::UnixStream, BoxError> {
    let start = Instant::now();
    loop {
        let attached = Client::connect(server).and_then(|mut client| {
            let reply = client.request(&ClientMsg::Attach {
                target: Some(session.to_owned()),
                size: attach::terminal_size(),
                create: None,
                follow: false,
            })?;
            Ok((client, reply))
        });
        match attached {
            Ok((client, ServerMsg::Attached { .. })) => return Ok(client.into_stream()),
            Ok((_, other)) => return Err(jetmux_client::unexpected(other).into()),
            Err(_) if start.elapsed() < RECONNECT_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("reconnecting to {session}: {e}").into()),
        }
    }
}

/// Replace the daemon with this binary. The daemon execs without
/// answering, so a closed connection means it went ahead.
fn upgrade(server: &str) -> Result<(), BoxError> {
    let binary = std::env::current_exe()?.display().to_string();
    let call = Call::ServerUpgrade {
        binary: Some(binary.clone()),
    };
    // A refusal comes back as an answer; going ahead closes the socket.
    if let Err(jetmux_client::Error::Daemon(e)) = rpc_connect(server)?.call(call) {
        return Err(format!("upgrade: {e}").into());
    }
    let start = Instant::now();
    loop {
        match rpc_connect(server)
            .and_then(|mut c| Ok(c.call_as::<Vec<SessionInfo>>(Call::SessionList {})?))
        {
            Ok(sessions) => {
                println!("upgraded to {binary} ({} sessions)", sessions.len());
                return Ok(());
            }
            Err(_) if start.elapsed() < RECONNECT_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("the new daemon didn't come up: {e}").into()),
        }
    }
}

fn current_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
}

/// Connect to the daemon, starting it in the background if nothing is
/// listening yet.
/// Bytes between stdin/stdout and the daemon, untouched: the far end
/// speaks the protocol, handshake and all.
/// Session `name` as config profile `profile` describes it.
fn profile_session(name: &str, profile: &str) -> Result<NewSession, BoxError> {
    let config = config::load()?;
    let found = config.profile.get(profile).ok_or_else(|| {
        format!(
            "no profile {profile} in {} or jet's config",
            config::path().display()
        )
    })?;
    let mut spec = found.session(name)?;
    spec.profile = Some(profile.to_owned());
    spec.cwd = spec.cwd.or_else(current_dir);
    Ok(spec)
}

/// Start session `name` from `profile`, detached, unless it's running.
fn start_profile(server: &str, name: &str, profile: &str) -> Result<(), BoxError> {
    connect_or_start(server)?;
    let mut rpc = rpc_connect(server)?;
    let sessions: Vec<SessionInfo> = rpc.call_as(Call::SessionList {})?;
    if !sessions.iter().any(|s| s.name == name) {
        let mut spec = profile_session(name, profile)?;
        spec.size = DETACHED_SIZE;
        let _: Created = rpc.call_as(Call::SessionCreate(spec))?;
    }
    Ok(())
}

fn proxy(server: &str) -> Result<(), BoxError> {
    use std::io::{Read, Write};
    use std::net::Shutdown;

    drop(connect_or_start(server)?);
    let socket = std::os::unix::net::UnixStream::connect(jetmux_client::socket_path(server))?;
    let mut to_daemon = socket.try_clone()?;
    let upstream = std::thread::spawn(move || {
        let mut buf = vec![0; 64 * 1024];
        let mut stdin = std::io::stdin().lock();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || to_daemon.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to_daemon.shutdown(Shutdown::Write);
    });
    let mut from_daemon = socket;
    let mut stdout = std::io::stdout().lock();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = from_daemon.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
    // The daemon hung up; the far end is told by stdout closing. stdin
    // may never close, so the reader thread isn't waited for.
    drop(upstream);
    Ok(())
}

fn connect_or_start(server: &str) -> Result<Client, BoxError> {
    match Client::connect(server) {
        Ok(client) => return Ok(client),
        Err(jetmux_client::Error::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(e.into()),
    }

    let dir = jetmux_client::socket_dir();
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{server}.log")))?;
    // Its own process group keeps the daemon clear of the terminal's SIGHUP
    // and job-control signals.
    std::os::unix::process::CommandExt::process_group(
        std::process::Command::new(std::env::current_exe()?)
            .args(["-L", server, "daemon"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log),
        0,
    )
    .spawn()?;

    let start = Instant::now();
    loop {
        match Client::connect(server) {
            Ok(client) => return Ok(client),
            Err(e) if start.elapsed() > DAEMON_START_TIMEOUT => {
                return Err(format!("daemon did not start: {e}").into());
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[cfg(test)]
mod tests {
    use jetmux_proto::Direction;
    use jetmux_proto::rpc::Call;

    use crate::{Cli, Cmd, parse};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse(args("")).unwrap(),
            Cli {
                server: "default".into(),
                json: false,
                cmd: Cmd::Attach { target: None }
            }
        );
        assert_eq!(
            parse(args("-L work new -s dev -d -- htop -d 5"))
                .unwrap()
                .cmd,
            Cmd::New {
                name: Some("dev".into()),
                detached: true,
                cwd: None,
                command: args("htop -d 5"),
            }
        );
        assert!(parse(args("kill")).is_err());
        assert!(parse(args("attach -t")).is_err());
        assert!(parse(args("pane frob")).is_err());
    }

    #[test]
    fn parses_scripting_calls() {
        let call = |s: &str| match parse(args(s)).unwrap().cmd {
            Cmd::Call { call } => call,
            other => panic!("expected a call, got {other:?}"),
        };
        assert_eq!(
            call("pane split -t dev:run down -- htop"),
            Call::PaneSplit {
                target: Some("dev:run".into()),
                direction: Direction::Down,
                command: vec!["htop".into()],
                cwd: None,
            }
        );
        assert_eq!(
            call("pane send-keys -t dev make Enter"),
            Call::PaneSendKeys {
                target: Some("dev".into()),
                keys: "make\r".into(),
            }
        );
        assert_eq!(
            call("pane send-keys -l Enter"),
            Call::PaneSendKeys {
                target: None,
                keys: "Enter".into(),
            }
        );
        assert_eq!(
            call("tab move -t dev -1"),
            Call::TabMove {
                target: Some("dev".into()),
                delta: -1,
            }
        );
        assert_eq!(
            call(r#"rpc pane.capture {"scrollback":true}"#),
            Call::PaneCapture {
                target: None,
                scrollback: true,
            }
        );
        assert!(parse(args("pane swap")).is_err());
    }
}
