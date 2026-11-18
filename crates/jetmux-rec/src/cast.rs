//! asciicast v2: a JSON header line, then one `[time, kind, data]` line
//! per event. And a player that scrubs through one.

use std::io::{self, Read, Write};
use std::time::Instant;

use jetmux_proto::Size;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::termios::{OptionalActions, Termios};
use serde::{Deserialize, Serialize};

/// Seconds a seek key moves.
const SEEK_SECS: f64 = 5.0;

#[derive(Debug, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    pub width: u16,
    pub height: u16,
    #[serde(default)]
    pub timestamp: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub env: serde_json::Map<String, serde_json::Value>,
}

impl Header {
    pub fn new(size: Size, timestamp: u64, title: Option<String>) -> Self {
        let mut env = serde_json::Map::new();
        env.insert("TERM".into(), "xterm-256color".into());
        Self {
            version: 2,
            width: size.cols,
            height: size.rows,
            timestamp: Some(timestamp),
            title,
            env,
        }
    }
}

pub fn write_line(out: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")
}

pub fn write_event(out: &mut impl Write, time: f64, kind: &str, data: &str) -> io::Result<()> {
    // Microseconds are plenty, and keep the file small.
    let time = (time * 1e6).round() / 1e6;
    write_line(out, &(time, kind, data))
}

pub struct Event {
    pub time: f64,
    pub kind: String,
    pub data: String,
}

pub struct Cast {
    pub header: Header,
    pub events: Vec<Event>,
}

impl Cast {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: Header = serde_json::from_str(lines.next().ok_or("empty file")?)
            .map_err(|e| format!("header: {e}"))?;
        if header.version != 2 {
            return Err(format!(
                "asciicast version {} isn't supported",
                header.version
            ));
        }
        let events = lines
            .enumerate()
            .map(|(i, line)| {
                let (time, kind, data): (f64, String, String) =
                    serde_json::from_str(line).map_err(|e| format!("event {}: {e}", i + 1))?;
                Ok(Event { time, kind, data })
            })
            .collect::<Result<_, String>>()?;
        Ok(Self { header, events })
    }
}

/// Turns PTY bytes into text without breaking a UTF-8 sequence that a read
/// split in two: the incomplete tail waits for the next chunk.
#[derive(Debug, Default)]
pub struct Utf8Carry {
    pending: Vec<u8>,
}

impl Utf8Carry {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest = &self.pending[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match e.error_len() {
                        // Truncated at the end: keep it for next time.
                        None => {
                            rest = after;
                            break;
                        }
                        Some(bad) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[bad..];
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }
}

/// Puts the terminal in raw mode for the player's keys, and back after.
struct Raw(Termios);

impl Raw {
    fn enter() -> Option<Self> {
        let stdin = rustix::stdio::stdin();
        if !rustix::termios::isatty(stdin) {
            return None;
        }
        let saved = rustix::termios::tcgetattr(stdin).ok()?;
        let mut raw = saved.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(stdin, OptionalActions::Now, &raw).ok()?;
        Some(Self(saved))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &self.0);
    }
}

enum Key {
    Pause,
    Quit,
    Faster,
    Slower,
    Seek(f64),
    Restart,
}

fn keys(bytes: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let key = match &bytes[i..] {
            [0x1b, b'[', b'C', ..] => {
                i += 2;
                Some(Key::Seek(SEEK_SECS))
            }
            [0x1b, b'[', b'D', ..] => {
                i += 2;
                Some(Key::Seek(-SEEK_SECS))
            }
            [b' ', ..] => Some(Key::Pause),
            [b'q' | 3, ..] => Some(Key::Quit),
            [b'+' | b'=', ..] => Some(Key::Faster),
            [b'-', ..] => Some(Key::Slower),
            [b'l', ..] => Some(Key::Seek(SEEK_SECS)),
            [b'h', ..] => Some(Key::Seek(-SEEK_SECS)),
            [b'0', ..] => Some(Key::Restart),
            _ => None,
        };
        out.extend(key);
        i += 1;
    }
    out
}

/// Plays output events on a timeline where idle gaps are capped, so
/// seeking and the progress shown match what the viewer experiences.
pub struct Player<'a> {
    /// (time on the capped timeline, output)
    frames: Vec<(f64, &'a str)>,
    speed: f64,
}

impl<'a> Player<'a> {
    pub fn new(cast: &'a Cast, idle: f64, speed: f64) -> Self {
        let mut frames = Vec::new();
        let (mut at, mut last) = (0.0, 0.0);
        for event in &cast.events {
            at += (event.time - last).clamp(0.0, idle.max(0.0));
            last = event.time;
            if event.kind == "o" {
                frames.push((at, event.data.as_str()));
            }
        }
        Self {
            frames,
            speed: if speed > 0.0 { speed } else { 1.0 },
        }
    }

    fn total(&self) -> f64 {
        self.frames.last().map_or(0.0, |f| f.0)
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let raw = Raw::enter();
        let mut stdout = io::stdout().lock();
        let mut stdin = io::stdin();
        let (mut pos, mut next, mut paused) = (0.0f64, 0usize, false);
        let mut tick = Instant::now();
        self.title(&mut stdout, pos, paused)?;
        while next < self.frames.len() || paused {
            if !paused {
                pos += tick.elapsed().as_secs_f64() * self.speed;
            }
            tick = Instant::now();
            while let Some(&(at, data)) = self.frames.get(next)
                && at <= pos
            {
                stdout.write_all(data.as_bytes())?;
                next += 1;
            }
            stdout.flush()?;
            let wait = match self.frames.get(next) {
                Some(&(at, _)) if !paused => Some((at - pos).max(0.0) / self.speed),
                _ => None,
            };
            let Some(pressed) = self.wait_for_keys(raw.is_some(), &mut stdin, wait)? else {
                continue;
            };
            for key in keys(&pressed) {
                match key {
                    Key::Quit => return self.finish(&mut stdout),
                    Key::Pause => paused = !paused,
                    Key::Faster => self.speed = (self.speed * 2.0).min(64.0),
                    Key::Slower => self.speed = (self.speed / 2.0).max(1.0 / 16.0),
                    Key::Seek(by) => {
                        let to = (pos + by).clamp(0.0, self.total());
                        next = self.seek(&mut stdout, pos, to, next)?;
                        pos = to;
                    }
                    Key::Restart => {
                        next = self.seek(&mut stdout, pos, 0.0, next)?;
                        pos = 0.0;
                    }
                }
            }
            self.title(&mut stdout, pos, paused)?;
        }
        self.finish(&mut stdout)
    }

    /// Wait up to `wait` seconds (forever with `None`) for keys; `None` back
    /// means time ran out.
    fn wait_for_keys(
        &self,
        keys: bool,
        stdin: &mut io::Stdin,
        wait: Option<f64>,
    ) -> io::Result<Option<Vec<u8>>> {
        if !keys {
            if let Some(secs) = wait {
                std::thread::sleep(std::time::Duration::from_secs_f64(secs));
            }
            return Ok(None);
        }
        let timeout = wait.map(|secs| Timespec {
            tv_sec: secs.trunc() as i64,
            tv_nsec: (secs.fract() * 1e9) as i64,
        });
        let fd = rustix::stdio::stdin();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(0) | Err(rustix::io::Errno::INTR) => Ok(None),
            Ok(_) => {
                let mut buf = [0u8; 64];
                let n = stdin.read(&mut buf)?;
                Ok(Some(buf[..n].to_vec()))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Jump to `to` from `pos`: forward by writing what's in between,
    /// backward by resetting the terminal and replaying from the start.
    /// Returns the next frame to play.
    fn seek(&self, out: &mut impl Write, pos: f64, to: f64, mut next: usize) -> io::Result<usize> {
        if to < pos {
            out.write_all(b"\x1bc")?;
            next = 0;
        }
        while let Some(&(at, data)) = self.frames.get(next)
            && at <= to
        {
            out.write_all(data.as_bytes())?;
            next += 1;
        }
        out.flush()?;
        Ok(next)
    }

    /// Progress in the window title, out of the recording's way.
    fn title(&self, out: &mut impl Write, pos: f64, paused: bool) -> io::Result<()> {
        let state = if paused { "paused" } else { "playing" };
        write!(
            out,
            "\x1b]2;jetmux-rec {state} {:.1}/{:.1}s x{}\x07",
            pos.min(self.total()),
            self.total(),
            self.speed
        )
    }

    fn finish(&self, out: &mut impl Write) -> Result<(), Box<dyn std::error::Error>> {
        out.write_all(b"\x1b[0m\x1b[?25h\r\n")?;
        out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::cast::{Cast, Player, Utf8Carry};

    #[test]
    fn split_utf8_waits_for_the_rest() {
        let mut carry = Utf8Carry::default();
        let bytes = "a日b".as_bytes();
        assert_eq!(carry.push(&bytes[..2]), "a");
        assert_eq!(carry.push(&bytes[2..4]), "日");
        assert_eq!(carry.push(&bytes[4..]), "b");
        assert_eq!(carry.push(b"\xffz"), "\u{fffd}z");
    }

    #[test]
    fn parses_and_caps_idle_time() {
        let text = r#"{"version": 2, "width": 80, "height": 24}
[0.0, "o", "a"]
[0.5, "o", "b"]
[30.5, "r", "100x30"]
[31.0, "o", "c"]
"#;
        let cast = Cast::parse(text).unwrap();
        assert_eq!((cast.header.width, cast.events.len()), (80, 4));
        let player = Player::new(&cast, 2.0, 1.0);
        let times: Vec<f64> = player.frames.iter().map(|f| f.0).collect();
        assert_eq!(times, [0.0, 0.5, 3.0]);
        assert!(Cast::parse(r#"{"version": 1, "width": 80, "height": 24}"#).is_err());
    }
}
