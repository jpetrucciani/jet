//! `jet-cli`: control running jet windows. A console program, unlike
//! `jet.exe`, so shells wait for it and capture what it prints.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use jet_cli::transport::{self, Client};
use jet_cli::{
    Answer, Body, Direction, EVENT_KINDS, Events, Listing, Open, PANE_VAR, Request, Resize,
    SOCKET_VAR, SendText, SetLabel, SetVar, Split, Target,
};

const USAGE: &str = "\
usage: jet-cli [--socket ADDRESS] [--json] COMMAND [OPTIONS]

commands:
  list                         tabs and panes
  spawn [--profile P] [--cwd DIR] [-- CMD...]
                               a new tab; prints its pane id
  split [--pane N] [--direction right|down|left|up] [--ratio R]
        [--profile P] [--cwd DIR] [-- CMD...]
                               split a pane; prints the new pane id
  send-text [--pane N] [--no-paste] [TEXT]
                               type TEXT (or stdin) into a pane
  get-text [--pane N]          the pane's screen as text
  activate [--pane N | --tab T]
  close [--pane N | --tab T]
  set-title [--tab T | --pane N] [TITLE]   no TITLE unpins
  set-icon [--tab T | --pane N] [ICON]     text, an .svg path, or <svg...>
  set-color [--tab T | --pane N] [COLOR]   #rrggbb; no COLOR: the config's
  set-var [--pane N] NAME [VALUE]          no VALUE unsets
  resize [--pane N] --ratio R  the pane's share of its split
  reload-config
  events [--pane N] [--types a,b]          JSON lines until interrupted

Inside jet, the window and pane come from $JET_SOCKET and $JET_PANE.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("jet-cli: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Options before the command, and the command itself.
#[derive(Debug, PartialEq)]
struct Invocation {
    socket: Option<String>,
    json: bool,
    request: Request,
}

fn run(args: Vec<String>) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("jet-cli {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    if args.iter().any(|a| a == "-h" || a == "--help") || args.is_empty() {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let here = std::env::var(PANE_VAR).ok().and_then(|p| p.parse().ok());
    let mut stdin = || {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map(|_| text)
            .map_err(|e| e.to_string())
    };
    let invocation = parse(args, here, &mut stdin)?;
    let address = find_window(invocation.socket)?;
    let mut client = Client::connect(&address).map_err(|e| format!("{address}: {e}"))?;
    #[cfg(windows)]
    if matches!(invocation.request, Request::Activate(_)) {
        allow_foreground(&mut client);
    }
    let streaming = matches!(invocation.request, Request::Events(_));
    let reply = client.call(invocation.request).map_err(|e| e.to_string())?;
    if invocation.json {
        println!(
            "{}",
            serde_json::to_string(&reply).map_err(|e| e.to_string())?
        );
    }
    let answer = match reply.body {
        Body::Ok(answer) => answer,
        Body::Error(failure) => {
            if !invocation.json {
                eprintln!("jet-cli: {} ({})", failure.message, failure.code);
            }
            return Ok(ExitCode::FAILURE);
        }
    };
    if streaming {
        let mut out = io::stdout().lock();
        loop {
            let line = client.read_line().map_err(|e| e.to_string())?;
            if out
                .write_all(line.as_bytes())
                .and_then(|()| out.flush())
                .is_err()
            {
                // The reader went away (`| head`): done.
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
    if !invocation.json {
        print_answer(&answer);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_answer(answer: &Answer) {
    match answer {
        Answer::List(listing) => print_listing(listing),
        Answer::Opened(opened) => println!("{}", opened.pane),
        Answer::Text(text) => println!("{}", text.text),
        Answer::Hello(hello) => println!("jet {} (protocol {})", hello.pid, hello.version),
        Answer::Done(_) => {}
    }
}

fn print_listing(listing: &Listing) {
    println!(
        "{:<5} {:<5} {:<9} {:<28} CWD",
        "TAB", "PANE", "SIZE", "TITLE"
    );
    for tab in &listing.tabs {
        for pane in &tab.panes {
            let mark = if tab.active && pane.focused { "*" } else { " " };
            let cwd = match (&pane.host, &pane.cwd) {
                (Some(host), Some(cwd)) => format!("{host}:{cwd}"),
                (None, Some(cwd)) => cwd.clone(),
                _ => String::new(),
            };
            let title: String = pane.title.chars().take(28).collect();
            println!(
                "{:<5} {:<5} {:<9} {:<28} {cwd}",
                tab.id,
                format!("{}{mark}", pane.id),
                format!("{}x{}", pane.cols, pane.rows),
                title,
            );
        }
    }
}

/// `--socket`, else `$JET_SOCKET`, else the one jet running.
fn find_window(flag: Option<String>) -> Result<String, String> {
    if let Some(address) = flag.or_else(|| std::env::var(SOCKET_VAR).ok().filter(|s| !s.is_empty()))
    {
        return Ok(address);
    }
    let running = transport::running().unwrap_or_default();
    match running.as_slice() {
        [] => Err("no jet is running".into()),
        [(_, address)] => Ok(address.clone()),
        many => {
            let list: Vec<String> = many.iter().map(|(_, a)| a.clone()).collect();
            Err(format!(
                "several jets are running; pick one with --socket:\n  {}",
                list.join("\n  ")
            ))
        }
    }
}

/// The window may raise itself only with the foreground's say-so, and this
/// console has it.
#[cfg(windows)]
fn allow_foreground(client: &mut Client) {
    use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    if let Ok(reply) = client.call(Request::Hello)
        && let Body::Ok(Answer::Hello(hello)) = reply.body
    {
        let _ = unsafe { AllowSetForegroundWindow(hello.pid) };
    }
}

/// Reads flags off the front of the arguments.
struct Args {
    rest: std::vec::IntoIter<String>,
    /// After `--`.
    command: Vec<String>,
}

impl Args {
    fn new(args: Vec<String>) -> Self {
        let mut before = Vec::new();
        let mut command = Vec::new();
        let mut iter = args.into_iter();
        for arg in iter.by_ref() {
            if arg == "--" {
                break;
            }
            before.push(arg);
        }
        command.extend(iter);
        Self {
            rest: before.into_iter(),
            command,
        }
    }

    fn value(&mut self, flag: &str) -> Result<String, String> {
        self.rest
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))
    }

    fn number<T: std::str::FromStr>(&mut self, flag: &str) -> Result<T, String> {
        let value = self.value(flag)?;
        value
            .parse()
            .map_err(|_| format!("{flag}: {value:?} isn't a number"))
    }
}

fn parse(
    args: Vec<String>,
    here: Option<u64>,
    stdin: &mut dyn FnMut() -> Result<String, String>,
) -> Result<Invocation, String> {
    let mut args = Args::new(args);
    let mut socket = None;
    let mut json = false;
    let command = loop {
        match args.rest.next().as_deref() {
            Some("--socket") => socket = Some(args.value("--socket")?),
            Some("--json") => json = true,
            Some(command) => break command.to_owned(),
            None => return Err(format!("no command\n\n{USAGE}")),
        }
    };
    // Shared by most commands; each takes what it uses.
    let mut target = Target::default();
    let mut open = Open::default();
    let mut direction = Direction::Right;
    let mut ratio = None;
    let mut paste = true;
    let mut types = Vec::new();
    let mut words = Vec::new();
    while let Some(arg) = args.rest.next() {
        match arg.as_str() {
            "--pane" => target.pane = Some(args.number("--pane")?),
            "--tab" => target.tab = Some(args.number("--tab")?),
            "--profile" => open.profile = Some(args.value("--profile")?),
            "--cwd" => open.cwd = Some(PathBuf::from(args.value("--cwd")?)),
            "--ratio" => ratio = Some(args.number::<f32>("--ratio")?),
            "--no-paste" => paste = false,
            "--json" => json = true,
            "--direction" => {
                direction = match args.value("--direction")?.as_str() {
                    "right" => Direction::Right,
                    "down" => Direction::Down,
                    "left" => Direction::Left,
                    "up" => Direction::Up,
                    other => {
                        return Err(format!(
                            "--direction: {other:?} isn't right, down, left or up"
                        ));
                    }
                };
            }
            "--types" => {
                for kind in args.value("--types")?.split(',') {
                    if !EVENT_KINDS.contains(&kind) {
                        return Err(format!(
                            "--types: no event {kind:?}; there's {}",
                            EVENT_KINDS.join(", ")
                        ));
                    }
                    types.push(kind.to_owned());
                }
            }
            flag if flag.starts_with("--") => {
                return Err(format!("{command}: unknown option {flag}"));
            }
            _ => words.push(arg),
        }
    }
    open.command = std::mem::take(&mut args.command);
    // Inside jet, "this pane" unless told otherwise; events from all of
    // them unless asked for one.
    let explicit = target.pane;
    if target.pane.is_none() && target.tab.is_none() {
        target.pane = here;
    }
    let one = |words: &mut Vec<String>| -> Result<Option<String>, String> {
        match words.len() {
            0 | 1 => Ok(words.pop()),
            _ => Err(format!("{command}: one value, quoted if it has spaces")),
        }
    };
    let request = match command.as_str() {
        "hello" => Request::Hello,
        "list" => Request::List,
        "spawn" => Request::Spawn(open),
        "split" => Request::Split(Split {
            pane: target.pane,
            direction,
            ratio,
            open,
        }),
        "send-text" => Request::SendText(SendText {
            pane: target.pane,
            text: match one(&mut words)? {
                Some(text) => text,
                None => stdin()?,
            },
            paste,
        }),
        "get-text" => Request::GetText(target),
        "activate" => Request::Activate(target),
        "close" => Request::Close(target),
        "set-title" => Request::SetTitle(SetLabel {
            target,
            value: one(&mut words)?,
        }),
        "set-icon" => Request::SetIcon(SetLabel {
            target,
            value: one(&mut words)?,
        }),
        "set-color" => Request::SetColor(SetLabel {
            target,
            value: one(&mut words)?,
        }),
        "set-var" => {
            let mut words = words.into_iter();
            let name = words.next().ok_or("set-var: which variable?")?;
            Request::SetVar(SetVar {
                pane: target.pane,
                name,
                value: words.next().unwrap_or_default(),
            })
        }
        "resize" => Request::Resize(Resize {
            pane: target.pane,
            ratio: ratio.ok_or("resize: --ratio R, the pane's share from 0 to 1")?,
        }),
        "reload-config" => Request::ReloadConfig,
        "events" => Request::Events(Events {
            pane: explicit,
            types,
        }),
        other => return Err(format!("no command {other:?}\n\n{USAGE}")),
    };
    Ok(Invocation {
        socket,
        json,
        request,
    })
}

#[cfg(test)]
mod tests {
    use jet_cli::{Direction, Request};

    use crate::parse;

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    fn no_stdin() -> Result<String, String> {
        Err("no stdin here".into())
    }

    #[test]
    fn commands_parse_with_this_pane_by_default() -> Result<(), String> {
        let got = parse(
            args("--json split --direction down --ratio 0.3 -- nvim ."),
            Some(5),
            &mut no_stdin,
        )?;
        assert!(got.json);
        let Request::Split(split) = got.request else {
            return Err(format!("not a split: {:?}", got.request));
        };
        assert_eq!(
            (split.pane, split.direction, split.ratio),
            (Some(5), Direction::Down, Some(0.3))
        );
        assert_eq!(split.open.command, ["nvim", "."]);

        let got = parse(args("close --tab 2"), Some(5), &mut no_stdin)?;
        assert!(matches!(got.request, Request::Close(t) if t.tab == Some(2) && t.pane.is_none()));

        let mut stdin = || Ok("from stdin".to_owned());
        let got = parse(args("send-text --no-paste"), None, &mut stdin)?;
        assert!(matches!(got.request, Request::SendText(s) if s.text == "from stdin" && !s.paste));
        Ok(())
    }

    #[test]
    fn mistakes_say_what_is_wrong() {
        let err = |line: &str| {
            parse(args(line), None, &mut no_stdin)
                .err()
                .unwrap_or_default()
        };
        assert!(err("split --direction sideways").contains("isn't right, down"));
        assert!(err("events --types bel").contains("no event \"bel\""));
        assert!(err("resize").contains("--ratio"));
        assert!(err("frobnicate").contains("no command"));
        assert!(err("list --frob").contains("unknown option --frob"));
    }
}
