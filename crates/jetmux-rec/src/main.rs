//! `jetmux-rec`: records a pane's output to asciicast v2 straight from the
//! byte stream the daemon reads from its PTY, and plays recordings back
//! with scrubbing.

mod cast;

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use jetmux_client::RpcClient;
use jetmux_proto::rpc::{Call, Event, Streaming};

use crate::cast::{Cast, Header, Utf8Carry};

const USAGE: &str = "\
usage: jetmux-rec record [-L server] [-t target] [-o file.cast] [--title title]
       jetmux-rec play file.cast [--speed x] [--idle secs]
       jetmux-rec cat file.cast

record   write a pane's output to asciicast v2 until it exits (or ctrl-c);
         the target is a pane as in `jetmux pane` (default: the current one
         inside jetmux, else the most recent session's focused pane)
play     replay with timing; idle gaps are cut to --idle (default 2s).
         space pause, left/right seek 5s, +/- speed, 0 restart, q quit
cat      print the output without timing";

type BoxError = Box<dyn std::error::Error>;

fn main() {
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        // writeln, not println: a closed pipe isn't worth a panic
        let _ = writeln!(
            std::io::stdout(),
            "jetmux-rec {}",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("record") => record(&args[1..]),
        Some("play") => play(&args[1..]),
        Some("cat") => cat(&args[1..]),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        eprintln!("jetmux-rec: {e}");
        std::process::exit(1);
    }
}

/// `--flag value` pairs and positionals.
struct Args {
    values: Vec<(String, String)>,
    positional: Vec<String>,
}

impl Args {
    fn parse(args: &[String], valued: &[&str]) -> Result<Self, BoxError> {
        let mut out = Self {
            values: Vec::new(),
            positional: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if valued.contains(&arg.as_str()) {
                let value = it.next().ok_or_else(|| format!("{arg} needs a value"))?;
                out.values.push((arg.clone(), value.clone()));
            } else if arg.starts_with('-') {
                return Err(format!("unknown option {arg}\n{USAGE}").into());
            } else {
                out.positional.push(arg.clone());
            }
        }
        Ok(out)
    }

    fn value(&self, flag: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(f, _)| f == flag)
            .map(|(_, v)| v.as_str())
    }

    fn number(&self, flag: &str, default: f64) -> Result<f64, BoxError> {
        match self.value(flag) {
            Some(v) => Ok(v.parse().map_err(|_| format!("bad {flag} {v}"))?),
            None => Ok(default),
        }
    }
}

/// Inside jetmux, a session-less target means this session, as the CLI does.
fn default_target(target: Option<&str>) -> Option<String> {
    let session = std::env::var("JETMUX_SESSION").ok();
    match (target, session) {
        (None, Some(session)) => Some(match std::env::var("JETMUX_PANE") {
            Ok(pane) => format!("{session}:%{pane}"),
            Err(_) => session,
        }),
        (Some(t), Some(session)) if t.starts_with([':', '%']) => {
            Some(format!("{session}:{}", t.trim_start_matches(':')))
        }
        (target, _) => target.map(str::to_owned),
    }
}

fn record(args: &[String]) -> Result<(), BoxError> {
    let args = Args::parse(args, &["-L", "-t", "-o", "--title"])?;
    let server = args.value("-L").unwrap_or("default");
    let mut client = RpcClient::connect(server)?;
    let started: Streaming = client.call_as(Call::PaneStream {
        target: default_target(args.value("-t")),
    })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = args
        .value("-o")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}-{}-{now}.cast", started.session, started.pane));
    let mut out = BufWriter::new(File::create(&path)?);
    let title = args
        .value("--title")
        .map(str::to_owned)
        .or_else(|| (!started.title.is_empty()).then(|| started.title.clone()));
    let header = Header::new(started.size, now, title);
    cast::write_line(&mut out, &header)?;
    cast::write_event(&mut out, 0.0, "o", &started.screen)?;
    out.flush()?;
    eprintln!(
        "jetmux-rec: recording {}:%{} to {path} (ctrl-c stops)",
        started.session, started.pane
    );

    let start = Instant::now();
    let mut carry = Utf8Carry::default();
    // Ends when the pane exits or the daemon goes away.
    while let Ok(event) = client.next_event() {
        let time = start.elapsed().as_secs_f64();
        match event {
            Event::Output { data, .. } => {
                let bytes = jetmux_proto::b64::decode(&data).ok_or("bad output from daemon")?;
                let text = carry.push(&bytes);
                if !text.is_empty() {
                    cast::write_event(&mut out, time, "o", &text)?;
                }
            }
            Event::Resized { size, .. } => {
                cast::write_event(&mut out, time, "r", &format!("{}x{}", size.cols, size.rows))?;
            }
            _ => continue,
        }
        // Line by line, so a recording cut short by ctrl-c is still whole.
        out.flush()?;
    }
    eprintln!(
        "jetmux-rec: recorded {:.1}s to {path}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn load(args: &Args) -> Result<Cast, BoxError> {
    let path = args.positional.first().ok_or(USAGE)?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(Cast::parse(&text).map_err(|e| format!("{path}: {e}"))?)
}

fn cat(args: &[String]) -> Result<(), BoxError> {
    let cast = load(&Args::parse(args, &[])?)?;
    let mut stdout = io::stdout().lock();
    for event in cast.events.iter().filter(|e| e.kind == "o") {
        stdout.write_all(event.data.as_bytes())?;
    }
    stdout.flush()?;
    Ok(())
}

fn play(args: &[String]) -> Result<(), BoxError> {
    let args = Args::parse(args, &["--speed", "--idle"])?;
    let cast = load(&args)?;
    let speed = args.number("--speed", 1.0)?;
    let idle = args.number("--idle", 2.0)?;
    let (cols, rows) = (cast.header.width, cast.header.height);
    if let Ok(ws) = rustix::termios::tcgetwinsize(rustix::stdio::stdout())
        && (ws.ws_col < cols || ws.ws_row < rows)
    {
        eprintln!(
            "jetmux-rec: recorded at {cols}x{rows}, this terminal is {}x{}",
            ws.ws_col, ws.ws_row
        );
    }
    let mut player = cast::Player::new(&cast, idle, speed);
    player.run()
}
