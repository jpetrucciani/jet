//! A pane: one PTY, the child process on its slave side, and its terminal.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use jetmux_proto::{CopyAction, Frame, Size};
use rustix::event::{PollFd, PollFlags};
use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal, WaitOptions};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;

use crate::snapshot::{Live, PaneSnapshot};
use jet_core::{
    Blocks, Context, CopyOutcome, Grabbed, KeyInput, Mods, MouseInput, MouseOutcome, Progress,
    VtCore, VtSettings,
};
use jetmux_proto::input::MouseEvent;
use jetmux_proto::pane::{Location, PaneEvent};

use crate::{Error, PaneId};

/// Most PTY output a [`Reader`] gathers before handing it over.
const READ_BATCH: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Spawn {
    /// Program and arguments. Empty means the user's login shell.
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Things the terminal reported while processing output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VtEvents {
    pub title_changed: bool,
    pub bell: bool,
}

pub struct Pane {
    vt: VtCore,
    master: OwnedFd,
    /// The child's pid, until the pane hands it on or is dropped.
    pid: Option<Pid>,
    /// Input and query replies waiting for the PTY to accept them.
    pending: Vec<u8>,
    /// What the pane was started with; empty for the login shell.
    command: Vec<String>,
    /// Closing this tells the pane's [`Reader`] to stop.
    reader_stop: Option<OwnedFd>,
    /// Text to type at the shell's first prompt, for restored panes.
    typeahead: Option<String>,
    /// Desktop notifications the app sent (title, body), not yet taken.
    notifications: Vec<(Option<String>, String)>,
    events: VtEvents,
    /// What the pane reported for rich clients, not yet taken.
    reports: Vec<PaneEvent>,
    /// The latest of each report that's a state, for clients that attach
    /// later.
    progress: Progress,
    context: Option<Context>,
    vars: BTreeMap<String, String>,
    /// Where procfs last said the program in front is, as reported, for
    /// shells that don't send OSC 7.
    polled: Option<Option<Location>>,
    pointer: &'static str,
    /// The latest OSC 52 write from the app, for the clients' clipboards.
    clipboard: Option<String>,
    last_output: Option<Instant>,
}

/// The read side of a pane's PTY, for a thread of its own: reading there
/// lets the child keep writing while the session parses earlier output.
pub struct Reader {
    master: OwnedFd,
    stop: OwnedFd,
}

impl Reader {
    /// Hand PTY output to `deliver` in batches until the child's side
    /// closes, the pane is dropped, or `deliver` returns false.
    pub fn run(self, mut deliver: impl FnMut(Vec<u8>) -> bool) -> io::Result<()> {
        let mut buf = vec![0; READ_BATCH];
        loop {
            let mut fds = [
                PollFd::new(&self.master, PollFlags::IN),
                PollFd::new(&self.stop, PollFlags::IN),
            ];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            if !fds[1].revents().is_empty() {
                return Ok(());
            }
            let mut batch = Vec::new();
            let mut closed = false;
            while batch.len() < READ_BATCH {
                match rustix::io::read(&self.master, &mut buf[..READ_BATCH - batch.len()]) {
                    Ok(0) => closed = true,
                    Ok(n) => {
                        batch.extend_from_slice(&buf[..n]);
                        continue;
                    }
                    Err(rustix::io::Errno::AGAIN) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    // Linux reports a closed slave as EIO on the master.
                    Err(rustix::io::Errno::IO) => closed = true,
                    Err(e) => return Err(e.into()),
                }
                break;
            }
            let refused = !batch.is_empty() && !deliver(batch);
            if refused || closed {
                return Ok(());
            }
        }
    }
}

impl Pane {
    pub fn spawn(spec: &Spawn, size: Size) -> Result<Self, Error> {
        let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
        // Other panes' children must not inherit this master, or closing the
        // pane would never deliver EOF/SIGHUP.
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
        rustix::pty::grantpt(&master)?;
        rustix::pty::unlockpt(&master)?;
        let slave_name = rustix::pty::ptsname(&master, Vec::new())?;
        let slave = rustix::fs::open(
            slave_name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        rustix::termios::tcsetwinsize(&slave, winsize(size))?;
        #[cfg(target_os = "linux")]
        {
            let mut termios = rustix::termios::tcgetattr(&slave)?;
            termios
                .input_modes
                .insert(rustix::termios::InputModes::IUTF8);
            rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &termios)?;
        }

        let child = {
            let mut cmd = command(spec);
            cmd.stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave))
                .env("TERM", "xterm-256color")
                .env("COLORTERM", "truecolor")
                .envs(spec.env.iter().map(|(k, v)| (k, v)));
            if let Some(cwd) = &spec.cwd {
                cmd.current_dir(cwd);
            }
            // SAFETY: only async-signal-safe syscalls run between fork and
            // exec: setsid and TIOCSCTTY on the already-dup'd stdin.
            unsafe {
                cmd.pre_exec(|| {
                    rustix::process::setsid()?;
                    rustix::process::ioctl_tiocsctty(BorrowedFd::borrow_raw(0))?;
                    Ok(())
                });
            }
            cmd.spawn()?
        };

        rustix::io::ioctl_fionbio(&master, true)?;
        Ok(Self {
            vt: crate::vt(size)?,
            master,
            pid: i32::try_from(child.id()).ok().and_then(Pid::from_raw),
            pending: Vec::new(),
            command: spec.command.clone(),
            reader_stop: None,
            typeahead: None,
            notifications: Vec::new(),
            events: VtEvents::default(),
            reports: Vec::new(),
            progress: Progress::Hidden,
            context: None,
            vars: BTreeMap::new(),
            polled: None,
            pointer: "",
            clipboard: None,
            last_output: None,
        })
    }

    /// Take over a pane a previous daemon ran, from its hot-upgrade state.
    pub fn adopt(
        master: OwnedFd,
        snapshot: &PaneSnapshot,
        command: Vec<String>,
    ) -> Result<Self, Error> {
        let live = snapshot
            .live
            .as_ref()
            .ok_or_else(|| Error::Spec(format!("pane {} has no live state", snapshot.id)))?;
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
        rustix::io::ioctl_fionbio(&master, true)?;
        let mut vt = crate::vt(snapshot.size)?;
        vt.restore_state(&live.primary, live.alt.as_deref(), &snapshot.title)?;
        Ok(Self {
            vt,
            master,
            pid: Pid::from_raw(live.pid),
            pending: Vec::new(),
            command,
            reader_stop: None,
            typeahead: None,
            notifications: Vec::new(),
            events: VtEvents::default(),
            reports: Vec::new(),
            progress: Progress::Hidden,
            context: None,
            vars: BTreeMap::new(),
            polled: None,
            pointer: "",
            clipboard: None,
            last_output: None,
        })
    }

    /// What a restart needs to bring the pane back: its screen and
    /// scrollback, and the last command typed at its prompt.
    pub fn snapshot(&self, id: PaneId) -> Result<PaneSnapshot, Error> {
        Ok(PaneSnapshot {
            id,
            title: self.title().to_owned(),
            size: self.size()?,
            last_command: self
                .blocks()
                .iter()
                .rev()
                .map(|b| b.command.as_str())
                .find(|c| !c.is_empty())
                .map(str::to_owned),
            history: self.vt.history()?,
            live: None,
        })
    }

    /// Stop reading the PTY, ahead of [`Pane::freeze`].
    pub fn stop_reader(&mut self) {
        self.reader_stop = None;
    }

    /// Hand the pane to a new daemon: its exact state and a PTY master that
    /// stays open. The child keeps running; dropping the pane no longer
    /// signals it.
    pub fn freeze(mut self, id: PaneId) -> Result<(PaneSnapshot, OwnedFd), Error> {
        let mut snapshot = self.snapshot(id)?;
        let (primary, alt) = self.vt.state()?;
        let master = rustix::io::fcntl_dupfd_cloexec(&self.master, 0)?;
        snapshot.live = Some(Live {
            fd: master.as_raw_fd(),
            pid: Pid::as_raw(self.pid.take()),
            primary,
            alt,
        });
        Ok((snapshot, master))
    }

    /// Paint saved output into the terminal, ahead of the child's own.
    pub fn paint(&mut self, data: &[u8]) -> Result<(), Error> {
        self.vt.write(data);
        self.vt.take_effects()?;
        Ok(())
    }

    pub fn raw_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// The PTY's read side. Dropping the pane stops the reader, even if a
    /// stray background job keeps the PTY open.
    pub fn reader(&mut self) -> io::Result<Reader> {
        let (stop, reader_stop) = UnixStream::pair()?;
        self.reader_stop = Some(reader_stop.into());
        Ok(Reader {
            master: rustix::io::fcntl_dupfd_cloexec(&self.master, 0)?,
            stop: stop.into(),
        })
    }

    /// Parse output its [`Reader`] delivered, answering any queries in it.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        self.last_output = Some(Instant::now());
        self.vt.write(data);
        let effects = self.vt.take_effects()?;
        for n in &effects.notifications {
            let title = (!n.title.is_empty()).then(|| n.title.clone());
            self.notifications.push((title, n.body.clone()));
        }
        self.reports
            .extend(effects.notifications.into_iter().map(PaneEvent::Notify));
        if let Some(location) = effects.location {
            // Sent, so polling doesn't send it again.
            self.polled = Some(Some(location.clone()));
            self.reports.push(PaneEvent::Location(Some(location)));
        }
        if let Some(progress) = effects.progress {
            self.progress = progress;
            self.reports.push(PaneEvent::Progress(progress));
        }
        for (name, value) in effects.vars {
            if value.is_empty() {
                self.vars.remove(&name);
            } else {
                self.vars.insert(name.clone(), value.clone());
            }
            self.reports.push(PaneEvent::Var { name, value });
        }
        if let Some(context) = effects.context {
            self.context.clone_from(&context);
            self.reports.push(PaneEvent::Context(context));
        }
        if let Some(pointer) = effects.pointer {
            self.pointer = pointer.name();
            self.reports
                .push(PaneEvent::Pointer(pointer.name().to_owned()));
        }
        self.reports
            .extend(effects.finished.into_iter().map(PaneEvent::Finished));
        self.events.title_changed |= effects.title.is_some();
        self.events.bell |= effects.bell;
        if effects.clipboard.is_some() {
            self.clipboard = effects.clipboard;
        }
        if !effects.pty_reply.is_empty() {
            self.pending.extend_from_slice(&effects.pty_reply);
            self.flush()?;
        }
        if effects.prompt_ready
            && let Some(text) = self.typeahead.take()
        {
            self.send(text.as_bytes())?;
        }
        Ok(())
    }

    /// Type `text` once the shell is at its first prompt. Typed any earlier,
    /// the terminal would echo it before the shell's line editor starts
    /// and it would show twice.
    pub fn type_at_prompt(&mut self, text: String) {
        self.typeahead = Some(text);
    }

    pub fn has_typeahead(&self) -> bool {
        self.typeahead.is_some()
    }

    /// For shells that don't mark their prompts: type once the shell has
    /// printed something and gone quiet for `quiet`.
    pub fn type_when_quiet(&mut self, quiet: Duration) -> io::Result<()> {
        if self.last_output.is_some_and(|t| t.elapsed() >= quiet)
            && let Some(text) = self.typeahead.take()
        {
            self.send(text.as_bytes())?;
        }
        Ok(())
    }

    pub fn send(&mut self, data: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(data);
        self.flush()
    }

    /// Write as much pending input as the PTY accepts.
    pub fn flush(&mut self) -> io::Result<()> {
        while !self.pending.is_empty() {
            match rustix::io::write(&self.master, &self.pending) {
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn wants_write(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn size(&self) -> Result<Size, Error> {
        let (cols, rows) = self.vt.size()?;
        Ok(Size { cols, rows })
    }

    /// Resize to `size` cells, each `cell` pixels in size (zero when no
    /// client said).
    pub fn resize(&mut self, size: Size, cell: (u32, u32)) -> Result<(), Error> {
        if self.size()? == size && self.vt.cell_size() == (cell.0.max(1), cell.1.max(1)) {
            return Ok(());
        }
        self.vt
            .resize(size.cols.max(1), size.rows.max(1), cell.0, cell.1)?;
        rustix::termios::tcsetwinsize(&self.master, winsize(size))?;
        Ok(())
    }

    pub fn frame(&mut self, full: bool) -> Result<Frame, Error> {
        Ok(self.vt.frame(full)?)
    }

    pub fn take_events(&mut self) -> VtEvents {
        std::mem::take(&mut self.events)
    }

    pub fn take_notifications(&mut self) -> Vec<(Option<String>, String)> {
        std::mem::take(&mut self.notifications)
    }

    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    pub fn synchronized(&self) -> bool {
        self.vt.synchronized().unwrap_or(false)
    }

    pub fn title(&self) -> &str {
        self.vt.title()
    }

    pub fn command(&self) -> &[String] {
        &self.command
    }

    /// The visible screen as text, one line per row, trailing blanks trimmed.
    pub fn screen_text(&self) -> Result<Vec<String>, Error> {
        let view = self.vt.viewport()?;
        let rows = u32::from(self.vt.size()?.1);
        let start = view.total.saturating_sub(rows);
        (start..view.total)
            .map(|y| {
                let chars = self.vt.row_chars(y)?;
                let line: String = chars.into_iter().filter(|c| *c != '\0').collect();
                Ok(line.trim_end().to_owned())
            })
            .collect()
    }

    /// The visible screen, or everything including scrollback, as text.
    pub fn capture(&self, scrollback: bool) -> Result<String, Error> {
        let mut lines = if scrollback {
            self.vt.screen_lines()?
        } else {
            self.screen_text()?
        };
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Ok(lines.join("\n"))
    }

    /// The shell's working directory: OSC 7 if it reports one, otherwise
    /// the child's cwd from procfs.
    pub fn cwd(&self) -> Option<String> {
        self.vt.location().map(|l| l.path).or_else(|| {
            let pid = self.pid?.as_raw_nonzero();
            procfs_cwd(pid)
        })
    }

    /// Where the pane is, as jet works it out for its own panes: the
    /// shell's report (OSC 7) while the shell is in front, else where the
    /// program in front is. Under ssh that's the remote shell's report if
    /// it sent one, else nothing (`Some(None)`). `None`: no telling.
    fn location(&self) -> Option<Option<Location>> {
        let front = rustix::termios::tcgetpgrp(&self.master).ok().or(self.pid)?;
        let reported = self.vt.location();
        if Some(front) == self.pid {
            return reported.map(Some).or_else(|| self.front_location(front));
        }
        match self.front_location(front)? {
            None => {
                let here = hostname();
                let remote = reported.filter(|l| {
                    l.host
                        .as_deref()
                        .is_some_and(|h| !h.eq_ignore_ascii_case(&here))
                });
                Some(remote)
            }
            local => Some(local),
        }
    }

    /// Where process `front` is, from procfs: `Some(None)` while that's
    /// ssh to somewhere, whose local directory says nothing about the
    /// remote one, and `None` when there's no telling.
    fn front_location(&self, front: Pid) -> Option<Option<Location>> {
        let front = front.as_raw_nonzero();
        let cmdline = std::fs::read(format!("/proc/{front}/cmdline")).ok()?;
        let argv: Vec<String> = cmdline
            .split(|&b| b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        if jet_core::ssh_target(&argv).is_some() {
            return Some(None);
        }
        let path = procfs_cwd(front)?;
        Some(Some(Location {
            host: Some(hostname()),
            path,
        }))
    }

    /// Where the pane is as a report, when that changed: a program
    /// taking the front, or giving it back, says nothing itself.
    pub fn poll_location(&mut self) -> Option<PaneEvent> {
        let now = self.location()?;
        if self.polled.as_ref() == Some(&now) {
            return None;
        }
        self.polled = Some(now.clone());
        Some(PaneEvent::Location(now))
    }

    pub fn blocks(&self) -> &Blocks {
        self.vt.blocks()
    }

    /// Ids of the blocks that finished since the last call.
    pub fn take_finished_blocks(&mut self) -> Vec<u64> {
        self.vt.take_finished_blocks()
    }

    /// A block's output, or the last finished block's with `None`.
    pub fn block_output(&self, id: Option<u64>) -> Result<Option<String>, Error> {
        let blocks = self.vt.blocks();
        let block = match id {
            Some(id) => blocks.get(id),
            None => blocks.last_done(),
        };
        match block {
            Some(block) => Ok(blocks.output(&self.vt, block)?),
            None => Ok(None),
        }
    }

    pub fn in_copy_mode(&self) -> bool {
        self.vt.in_copy_mode()
    }

    pub fn enter_copy_mode(&mut self) -> Result<(), Error> {
        Ok(self.vt.enter_copy_mode()?)
    }

    pub fn copy_action(&mut self, action: CopyAction) -> Result<CopyOutcome, Error> {
        Ok(self.vt.copy_action(action)?)
    }

    /// Reports since the last call, for rich clients.
    pub fn take_reports(&mut self) -> Vec<PaneEvent> {
        std::mem::take(&mut self.reports)
    }

    /// The pane's state as reports, for a client attaching now.
    pub fn state_reports(&self) -> Vec<PaneEvent> {
        let mut out = vec![
            PaneEvent::Progress(self.progress),
            PaneEvent::Context(self.context.clone()),
            PaneEvent::Pointer(self.pointer.to_owned()),
        ];
        let location = self.location().flatten();
        out.extend(location.map(|l| PaneEvent::Location(Some(l))));
        out.extend(self.vars.iter().map(|(name, value)| PaneEvent::Var {
            name: name.clone(),
            value: value.clone(),
        }));
        out
    }

    /// A picture this pane's frames show, by key.
    pub fn picture(&self, key: u64) -> Option<std::sync::Arc<jet_core::Picture>> {
        self.vt.picture(key)
    }

    /// Search the screen and history; empty ends it.
    pub fn search(&mut self, query: &str, regex: bool) -> Result<(), Error> {
        Ok(self.vt.search(query, regex)?)
    }

    pub fn search_step(&mut self, n: i32) -> Result<(), Error> {
        Ok(self.vt.search_step(n as isize)?)
    }

    /// Colors to answer the app's queries with.
    pub fn configure(&mut self, settings: &VtSettings) -> Result<(), Error> {
        self.vt.configure(settings)?;
        let effects = self.vt.take_effects()?;
        self.send(&effects.pty_reply)?;
        Ok(())
    }

    /// A key from a client that leaves encoding to us. Copy mode takes it
    /// first; what it yanks comes back.
    pub fn key(&mut self, input: &KeyInput) -> Result<Option<String>, Error> {
        if let Some(grabbed) = self.vt.mode_key(input)? {
            return Ok(match grabbed {
                Grabbed::Copy(text) => Some(text),
                Grabbed::CopyAndPaste(text) => {
                    self.paste(&text)?;
                    Some(text)
                }
                Grabbed::Nothing => None,
            });
        }
        let bytes = self.vt.encode_key(input)?;
        self.send(&bytes)?;
        Ok(None)
    }

    /// A mouse event from the wire. Reports go to the app; a selection or
    /// a paste request comes back.
    pub fn mouse(&mut self, event: &MouseEvent) -> Result<MouseOutcome, Error> {
        let input = MouseInput::from_wire(event, self.vt.cell_size());
        match self.vt.mouse(input)? {
            MouseOutcome::Pty(bytes) => {
                self.send(&bytes)?;
                Ok(MouseOutcome::Nothing)
            }
            other => Ok(other),
        }
    }

    /// Wheel notches at a cell: reported to apps that track the mouse,
    /// otherwise a scroll through history.
    pub fn wheel(&mut self, notches: i32, cell: (u16, u16), mods: Mods) -> Result<(), Error> {
        let bytes = self.vt.wheel(notches, cell, mods)?;
        self.send(&bytes)?;
        Ok(())
    }

    /// Tell the app about focus, if it asked (mode 1004).
    pub fn focus(&mut self, focused: bool) -> Result<(), Error> {
        let bytes = self.vt.focus(focused)?;
        self.send(&bytes)?;
        Ok(())
    }

    /// Paste text, bracketed if the app asked for it.
    pub fn paste(&mut self, text: &str) -> Result<(), Error> {
        let bytes = self.vt.encode_paste(text)?;
        self.send(&bytes)?;
        Ok(())
    }
}

impl AsFd for Pane {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };
        let _ = rustix::process::kill_process_group(pid, Signal::HUP);
        // Reap off the session thread: a child that ignores SIGHUP gets a
        // grace period, then SIGKILL.
        let _ = std::thread::Builder::new()
            .name("jetmux-reap".into())
            .spawn(move || {
                for _ in 0..20 {
                    if !matches!(
                        rustix::process::waitpid(Some(pid), WaitOptions::NOHANG),
                        Ok(None)
                    ) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = rustix::process::kill_process(pid, Signal::KILL);
                let _ = rustix::process::waitpid(Some(pid), WaitOptions::empty());
            });
    }
}

fn command(spec: &Spawn) -> Command {
    if let Some((program, args)) = spec.command.split_first() {
        let mut cmd = Command::new(program);
        cmd.args(args);
        return cmd;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let name = shell.rsplit('/').next().unwrap_or("sh");
    let mut cmd = Command::new(&shell);
    cmd.arg0(format!("-{name}"));
    cmd
}

fn winsize(size: Size) -> Winsize {
    Winsize {
        ws_row: size.rows.max(1),
        ws_col: size.cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}
fn procfs_cwd(pid: std::num::NonZeroI32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.display().to_string())
}

/// This machine's name, for paths found here: a client elsewhere must
/// not take them for its own.
fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}
