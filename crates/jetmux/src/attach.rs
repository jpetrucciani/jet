//! The attached client: resolves keys through the key layers, sends input
//! and actions to the daemon, and composes what it sends back onto the outer
//! terminal.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream as StdUnixStream;

use jet_config::status::{Align, Segment, Session};
use jet_core::{Context, Location, Progress, Rgb};
use jetmux_proto::pane::PaneEvent;
use jetmux_proto::{Action, ClientMsg, Color, CopyAction, Decoder, MouseTracking, ServerMsg, Size};
use jetmux_render::{Renderer, Status, Theme};
use mio::net::UnixStream;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use rustix::termios::{OptionalActions, Termios};
use signal_hook::consts::{SIGHUP, SIGTERM, SIGWINCH};
use signal_hook_mio::v1_0::Signals;

use crate::config::{self, Config, Watcher};
use crate::keymap::{self, Bound, Keymap, Layer, PromptKind};
use crate::keys::{self, Input, Key, Parser};

const STDIN: Token = Token(0);
const SOCKET: Token = Token(1);
const SIGNALS: Token = Token(2);

/// Rows scrolled per wheel step in copy mode.
const WHEEL_ROWS: i32 = 3;
const WHEEL_UP: u16 = 64;
const WHEEL_DOWN: u16 = 65;
/// Shift, alt and ctrl bits in a mouse button code.
const MOUSE_MODIFIERS: u16 = 4 | 8 | 16;

type BoxError = Box<dyn std::error::Error>;

pub enum Outcome {
    Detached,
    Exited,
    /// The daemon closed the connection or the client was signalled.
    Disconnected,
    /// The daemon is being replaced; attach to the new one.
    Reconnect,
}

/// The terminal size of stdout, falling back to 80x24.
pub fn terminal_size() -> Size {
    match rustix::termios::tcgetwinsize(rustix::stdio::stdout()) {
        Ok(ws) if ws.ws_col > 0 && ws.ws_row > 0 => Size {
            cols: ws.ws_col,
            rows: ws.ws_row,
        },
        _ => Size { cols: 80, rows: 24 },
    }
}

/// Puts the terminal in raw mode and restores it on drop.
///
/// stdin stays blocking: it shares its file description with stdout, and
/// O_NONBLOCK there would make large frame writes fail.
struct TtyGuard {
    termios: Termios,
}

impl TtyGuard {
    fn enter() -> Result<Self, BoxError> {
        let stdin = rustix::stdio::stdin();
        let termios = rustix::termios::tcgetattr(stdin)?;
        let mut raw = termios.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(stdin, OptionalActions::Now, &raw)?;
        Ok(Self { termios })
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        let _ =
            rustix::termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &self.termios);
    }
}

struct Client {
    stream: UnixStream,
    decoder: Decoder,
    out: Vec<u8>,
    renderer: Renderer,
    screen: String,
    parser: Parser,
    /// Explicitly entered layer. Copy mode is derived from the view.
    layer: Layer,
    prompt: Option<(PromptKind, String)>,
    message: Option<String>,
    /// Passthrough bytes not sent yet, so a burst of keys is one message.
    input: Vec<u8>,
    keymap: Keymap,
    theme: Theme,
    segments: Segments,
    config: Watcher,
    /// What panes reported, for the segments.
    panes: HashMap<u32, Reported>,
    /// Asked for pane reports, which only the segments need.
    reporting: bool,
    local_user: String,
    local_host: String,
}

/// jet's status segments, from the sections shared with it, and the theme
/// colors that unset segment colors are mixed from.
#[derive(Default)]
struct Segments {
    list: Vec<Segment>,
    fg: Rgb,
    bg: Rgb,
}

/// A pane's reports.
#[derive(Default)]
struct Reported {
    location: Option<Location>,
    progress: Progress,
    vars: HashMap<String, String>,
    context: Option<Context>,
    exit: Option<i32>,
}

impl Reported {
    fn apply(&mut self, event: PaneEvent) {
        match event {
            PaneEvent::Location(location) => self.location = location,
            PaneEvent::Progress(progress) => self.progress = progress,
            PaneEvent::Var { name, value } if value.is_empty() => {
                self.vars.remove(&name);
            }
            PaneEvent::Var { name, value } => {
                self.vars.insert(name, value);
            }
            PaneEvent::Context(context) => self.context = context,
            PaneEvent::Finished(exit) => self.exit = exit,
            PaneEvent::Notify(_) | PaneEvent::Pointer(_) => {}
        }
    }
}

/// What the client takes from the config.
fn settings(config: &Config) -> Result<(Keymap, Theme, Segments), String> {
    let colors = config.shared.vt_settings();
    let segments = Segments {
        list: config.shared.status.segments.clone(),
        fg: colors.foreground,
        bg: colors.background,
    };
    Ok((Keymap::from_config(config)?, config.bar.theme(), segments))
}

/// Drive an attached session until it ends. `stream` has already received
/// `Attached`.
pub fn run(stream: StdUnixStream) -> Result<Outcome, BoxError> {
    if !rustix::termios::isatty(rustix::stdio::stdin()) {
        return Err("attaching needs a terminal on stdin".into());
    }
    stream.set_nonblocking(true)?;
    let (config, loaded) = Watcher::new();
    let ((keymap, theme, segments), message) = match loaded.and_then(|c| settings(&c)) {
        Ok(settings) => (settings, None),
        Err(e) => (
            (Keymap::default(), Theme::default(), Segments::default()),
            Some(format!("config: {e}")),
        ),
    };
    let (local_user, local_host) = local_names();
    let mut client = Client {
        stream: UnixStream::from_std(stream),
        decoder: Decoder::default(),
        out: Vec::new(),
        renderer: Renderer::new(terminal_size()),
        screen: String::new(),
        parser: Parser::default(),
        layer: Layer::Normal,
        prompt: None,
        message,
        input: Vec::new(),
        keymap,
        theme,
        segments,
        config,
        panes: HashMap::new(),
        reporting: false,
        local_user,
        local_host,
    };

    let guard = TtyGuard::enter()?;
    client.renderer.enter(&mut client.screen);
    let result = client.run();
    client.screen.clear();
    client.renderer.leave(&mut client.screen);
    let _ = client.draw();
    drop(guard);
    result
}

impl Client {
    fn run(&mut self) -> Result<Outcome, BoxError> {
        let mut poll = Poll::new()?;
        let stdin_fd = rustix::stdio::stdin().as_raw_fd();
        poll.registry()
            .register(&mut SourceFd(&stdin_fd), STDIN, Interest::READABLE)?;
        poll.registry().register(
            &mut self.stream,
            SOCKET,
            Interest::READABLE | Interest::WRITABLE,
        )?;
        let mut signals = Signals::new([SIGWINCH, SIGTERM, SIGHUP])?;
        poll.registry()
            .register(&mut signals, SIGNALS, Interest::READABLE)?;

        let mut events = Events::with_capacity(16);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Err(e) = poll.poll(&mut events, Some(config::RELOAD_INTERVAL)) {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            self.reload_config();
            self.want_reports()?;
            for event in &events {
                let outcome = match event.token() {
                    STDIN => self.read_stdin(&mut buf)?,
                    SOCKET if event.is_readable() || event.is_read_closed() => {
                        self.read_socket(&mut buf)?
                    }
                    SIGNALS => {
                        let mut outcome = None;
                        for signal in signals.pending() {
                            if signal == SIGWINCH {
                                let size = terminal_size();
                                self.renderer.resize(size);
                                self.send(&ClientMsg::Resize(size))?;
                            } else {
                                outcome = Some(Outcome::Disconnected);
                            }
                        }
                        outcome
                    }
                    _ => None,
                };
                if let Some(outcome) = outcome {
                    return Ok(outcome);
                }
            }
            self.flush_input()?;
            self.flush_socket()?;
            let status = self.status();
            self.renderer.set_status(status, &mut self.screen);
            self.draw()?;
        }
    }

    fn reload_config(&mut self) {
        match self.config.poll() {
            Some(Ok(config)) => match settings(&config) {
                Ok((keymap, theme, segments)) => {
                    self.keymap = keymap;
                    self.theme = theme;
                    self.segments = segments;
                    self.message = Some("config reloaded".into());
                }
                Err(e) => self.message = Some(format!("config: {e}")),
            },
            Some(Err(e)) => self.message = Some(format!("config: {e}")),
            None => {}
        }
    }

    fn layer(&self) -> Layer {
        if self.layer != Layer::Normal {
            return self.layer;
        }
        let copy = self
            .renderer
            .view()
            .and_then(|v| v.panes.iter().find(|p| p.focused))
            .is_some_and(|p| p.copy_mode);
        if copy { Layer::Copy } else { Layer::Normal }
    }

    fn status(&self) -> Status {
        let (prompt, hint) = match &self.prompt {
            Some((kind, text)) => {
                let hint = (*kind == PromptKind::Palette).then(|| {
                    keymap::matches(text)
                        .iter()
                        .take(6)
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join("  ")
                });
                (Some(format!("{}{text}", kind.label())), hint)
            }
            None => (None, None),
        };
        let layer = self.layer();
        Status {
            theme: self.theme,
            mode: layer
                .badge(&self.theme)
                .map(|(label, color)| (label.to_owned(), color)),
            tabs: matches!(layer, Layer::Normal | Layer::Tab),
            hints: self.keymap.hints(layer),
            help: (layer == Layer::Normal)
                .then(|| self.keymap.prefix().map(|key| format!("{key} for keys")))
                .flatten(),
            prompt,
            message: hint.or_else(|| self.message.clone()),
            segments: self.shown_segments(),
        }
    }

    /// The segments with something to say about the focused pane.
    fn shown_segments(&self) -> Vec<jetmux_render::Segment> {
        let Some(view) = self.renderer.view() else {
            return Vec::new();
        };
        if self.segments.list.is_empty() {
            return Vec::new();
        }
        let focused = view.panes.iter().find(|p| p.focused);
        let reported = focused.and_then(|p| self.panes.get(&p.id));
        let (time, date) = local_time();
        let session = Session {
            title: focused.map_or("", |p| p.title.as_str()),
            location: reported.and_then(|r| r.location.as_ref()),
            tab: usize::from(view.active_tab) + 1,
            tabs: view.tabs.len(),
            time: &time,
            date: &date,
            progress: reported.map(|r| r.progress),
            exit: reported.and_then(|r| r.exit),
            vars: reported.map(|r| &r.vars),
            context: reported.and_then(|r| r.context.as_ref()),
            local_user: &self.local_user,
            local_host: &self.local_host,
            ..Session::default()
        };
        let Segments { list, fg, bg } = &self.segments;
        let rgb = |c: Rgb| Color::Rgb(c.r, c.g, c.b);
        jet_config::status::shown(list, &session, *fg, *bg)
            .into_iter()
            .map(|shown| jetmux_render::Segment {
                text: shown.text,
                fg: rgb(shown.fg),
                bg: rgb(shown.bg),
                right: shown.align == Align::Right,
            })
            .collect()
    }

    /// Ask for pane reports once there are segments to fill.
    fn want_reports(&mut self) -> Result<(), BoxError> {
        if !self.reporting && !self.segments.list.is_empty() {
            self.reporting = true;
            self.send(&ClientMsg::WantPaneEvents)?;
        }
        Ok(())
    }

    /// Read everything stdin has buffered. mio is edge-triggered, so loop
    /// until FIONREAD reports nothing left rather than reading once.
    fn read_stdin(&mut self, buf: &mut [u8]) -> Result<Option<Outcome>, BoxError> {
        let stdin = rustix::stdio::stdin();
        loop {
            match rustix::io::read(stdin, &mut *buf) {
                Ok(0) => return Ok(Some(Outcome::Disconnected)),
                Ok(n) => {
                    for input in self.parser.parse(&buf[..n]) {
                        self.handle(input)?;
                    }
                    if rustix::io::ioctl_fionread(stdin)? == 0 {
                        break;
                    }
                }
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    fn handle(&mut self, input: Input) -> Result<(), BoxError> {
        if self.prompt.is_some() {
            return self.edit_prompt(input);
        }
        self.message = None;
        match input {
            Input::Paste(raw) => {
                if self.layer() != Layer::Copy {
                    self.input.extend_from_slice(&raw);
                }
            }
            Input::Mouse(mouse) => self.mouse(mouse)?,
            Input::KittyKeyboard => self.renderer.set_kitty_keyboard(&mut self.screen),
            Input::Key(Key::Event, raw) => {
                if self.layer() == Layer::Normal {
                    self.input.extend_from_slice(&raw);
                }
            }
            Input::Key(key, raw) => {
                let layer = self.layer();
                match self.keymap.lookup(layer, key) {
                    Some(bound) => {
                        if layer == Layer::Prefix {
                            self.layer = Layer::Normal;
                        }
                        self.dispatch(bound)?;
                    }
                    None if layer == Layer::Normal => self.input.extend_from_slice(&raw),
                    None if layer == Layer::Prefix => self.layer = Layer::Normal,
                    None => {}
                }
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, bound: Bound) -> Result<(), BoxError> {
        match bound {
            Bound::Action(action) => self.action(action)?,
            Bound::Detach => {
                self.flush_input()?;
                self.send(&ClientMsg::Detach)?;
            }
            Bound::Layer(layer) => self.layer = layer,
            Bound::Leave => self.layer = Layer::Normal,
            Bound::Prompt(kind) => self.prompt = Some((kind, String::new())),
            Bound::LiteralPrefix => {
                if self.layer() != Layer::Copy
                    && let Some(prefix) = self.keymap.prefix()
                {
                    self.input.extend_from_slice(&prefix.bytes());
                }
            }
        }
        Ok(())
    }

    fn action(&mut self, action: Action) -> Result<(), BoxError> {
        self.flush_input()?;
        self.send(&ClientMsg::Action(action))
    }

    fn edit_prompt(&mut self, input: Input) -> Result<(), BoxError> {
        if input == Input::KittyKeyboard {
            self.renderer.set_kitty_keyboard(&mut self.screen);
            return Ok(());
        }
        let Some((kind, text)) = &mut self.prompt else {
            return Ok(());
        };
        let kind = *kind;
        match input {
            Input::Key(Key::Char(c), _) => text.push(c),
            Input::Paste(raw) => {
                let raw = String::from_utf8_lossy(&raw);
                let inner = raw
                    .trim_start_matches("\x1b[200~")
                    .trim_end_matches("\x1b[201~");
                text.extend(inner.chars().filter(|c| !c.is_control()));
            }
            Input::Key(Key::Backspace, _) => {
                text.pop();
            }
            Input::Key(Key::Ctrl('u'), _) => text.clear(),
            Input::Key(Key::Tab, _) if kind == PromptKind::Palette => {
                if let Some((name, _)) = keymap::matches(text).first() {
                    *text = (*name).to_owned();
                }
            }
            Input::Key(Key::Esc | Key::Ctrl('c'), _) => self.prompt = None,
            Input::Key(Key::Enter, _) => {
                let text = std::mem::take(text);
                self.prompt = None;
                match kind {
                    PromptKind::RenameTab => self.action(Action::RenameTab(text))?,
                    PromptKind::Search { backward } => {
                        let copy = if text.is_empty() {
                            CopyAction::SearchNext
                        } else {
                            CopyAction::Search {
                                pattern: text,
                                backward,
                            }
                        };
                        self.action(Action::Copy(copy))?;
                    }
                    PromptKind::Palette => match keymap::matches(&text).into_iter().next() {
                        Some((_, bound)) => self.dispatch(bound)?,
                        None => self.message = Some(format!("unknown command {text}")),
                    },
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Clicking a pane focuses it. The wheel scrolls panes that don't
    /// want the mouse through copy mode. Everything else goes to the
    /// focused pane, translated into its coordinates and the encoding it
    /// asked for.
    fn mouse(&mut self, mouse: keys::Mouse) -> Result<(), BoxError> {
        let Some(view) = self.renderer.view() else {
            return Ok(());
        };
        let Some(under) = view
            .panes
            .iter()
            .find(|p| p.rect.contains(mouse.x, mouse.y))
            .cloned()
        else {
            return Ok(());
        };
        let modes = self.renderer.focused_modes();
        let button = mouse.button & !MOUSE_MODIFIERS;
        if !under.focused {
            if mouse.press && button == 0 {
                self.action(Action::FocusPane(under.id))?;
            }
            return Ok(());
        }
        if mouse.press && matches!(button, WHEEL_UP | WHEEL_DOWN) {
            let rows = if button == WHEEL_UP {
                -WHEEL_ROWS
            } else {
                WHEEL_ROWS
            };
            if under.copy_mode {
                return self.action(Action::Copy(CopyAction::Scroll(rows)));
            }
            if modes.mouse == MouseTracking::Off {
                if button == WHEEL_UP {
                    self.action(Action::EnterCopyMode)?;
                    self.action(Action::Copy(CopyAction::Scroll(rows)))?;
                }
                return Ok(());
            }
        }
        if modes.mouse == MouseTracking::Off || under.copy_mode {
            return Ok(());
        }
        let local = keys::Mouse {
            x: mouse.x - under.rect.x,
            y: mouse.y - under.rect.y,
            ..mouse
        };
        if let Some(bytes) = keys::encode_mouse(local, modes.mouse_format) {
            self.input.extend_from_slice(&bytes);
        }
        Ok(())
    }

    fn read_socket(&mut self, buf: &mut [u8]) -> Result<Option<Outcome>, BoxError> {
        // The daemon's last message (Exited, Detached) usually arrives in the
        // same read as the EOF, so decode before acting on the EOF.
        let mut eof = false;
        loop {
            match self.stream.read(buf) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        while let Some(msg) = self.decoder.next_msg::<ServerMsg>()? {
            match msg {
                ServerMsg::View(view) => {
                    self.panes
                        .retain(|id, _| view.panes.iter().any(|p| p.id == *id));
                    self.renderer.set_view(view, &mut self.screen);
                }
                ServerMsg::Pane { pane, event } => self.panes.entry(pane).or_default().apply(event),
                ServerMsg::PaneFrame { pane, frame } => {
                    self.renderer.pane_frame(pane, frame, &mut self.screen);
                }
                // The outer terminal applies its own rule to the OSC 52.
                ServerMsg::Clipboard(text) | ServerMsg::AppClipboard(text) => {
                    self.screen.push_str("\x1b]52;c;");
                    self.screen.push_str(&base64(text.as_bytes()));
                    self.screen.push('\x07');
                    self.message = Some(format!("copied {} chars", text.chars().count()));
                }
                ServerMsg::Bell => self.screen.push('\x07'),
                ServerMsg::Message(msg) => self.message = Some(msg),
                ServerMsg::Detached => return Ok(Some(Outcome::Detached)),
                ServerMsg::Exited => return Ok(Some(Outcome::Exited)),
                ServerMsg::Reconnect => return Ok(Some(Outcome::Reconnect)),
                ServerMsg::Error(e) => return Err(format!("daemon: {e}").into()),
                _ => {}
            }
        }
        Ok(eof.then_some(Outcome::Disconnected))
    }

    fn send(&mut self, msg: &ClientMsg) -> Result<(), BoxError> {
        jetmux_proto::encode(msg, &mut self.out)?;
        Ok(())
    }

    fn flush_input(&mut self) -> Result<(), BoxError> {
        if !self.input.is_empty() {
            let input = std::mem::take(&mut self.input);
            self.send(&ClientMsg::Input(input))?;
        }
        Ok(())
    }

    fn flush_socket(&mut self) -> Result<(), BoxError> {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Write pending screen output. stdout is a blocking tty, so this
    /// completes the frame before returning.
    fn draw(&mut self) -> io::Result<()> {
        if self.screen.is_empty() {
            return Ok(());
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(self.screen.as_bytes())?;
        stdout.flush()?;
        self.screen.clear();
        Ok(())
    }
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(TABLE[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// This computer's user and short host name.
fn local_names() -> (String, String) {
    let host = rustix::system::uname();
    let host = host.nodename().to_string_lossy();
    let host = host.split('.').next().unwrap_or_default().to_lowercase();
    (std::env::var("USER").unwrap_or_default(), host)
}

/// Local `HH:MM` and `YYYY-MM-DD`, in the zone `TZ` or the system says.
fn local_time() -> (String, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Typed by localtime_r: libc deprecates naming `time_t` on musl.
    let Ok(now) = now.try_into() else {
        return (String::new(), String::new());
    };
    // SAFETY: all-zero is a valid `tm`; localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return (String::new(), String::new());
    }
    (
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
        format!(
            "{}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        ),
    )
}

#[cfg(test)]
mod tests {
    use crate::attach::base64;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b"hi"), "aGk=");
        assert_eq!(base64(b"jetmux"), "amV0bXV4");
        assert_eq!(base64(b"abc"), "YWJj");
    }
}
