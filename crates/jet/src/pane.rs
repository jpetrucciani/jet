//! A terminal pane: a child on a PTY plus the thread that owns its `VtCore`.
//!
//! Threads per pane:
//! - pane thread: owns the `VtCore` (libghostty-vt is `!Send`), applies PTY
//!   output and input, publishes snapshots.
//! - reader thread: blocking PTY reads, forwarded to the pane thread with at
//!   most [`OUTPUT_IN_FLIGHT`] chunks queued, so a flood waits on the child
//!   instead of piling up in memory. Detached: it ends when the PTY closes or
//!   the pane thread is gone, which can outlive the pane if a grandchild
//!   keeps the PTY open.
//! - waiter thread: blocks on the child's exit. EOF can't stand in for it:
//!   ConPTY holds its output pipe open until the pseudoconsole is closed, so
//!   the pane closes the PTY once the child is gone.
//! - writer thread: blocking PTY writes, so a child that stops reading can
//!   never stall the pane thread (and with it, reading the child's output).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jet_pty::{Child, Command, Pty, Size};

use jet_core::{
    Context, Error, Grabbed, KeyInput, Location, MouseInput, MouseOutcome, Notification,
    PointerShape, Progress, Snapshot, SnapshotSlot, VtCore, VtSettings,
};

/// Stop draining queued output after this much so effects (replies to the
/// child, titles) keep flowing during floods like `cat bigfile`.
const MAX_BYTES_PER_FLUSH: usize = 1 << 20;

/// Output chunks read but not yet parsed. The reader blocks past this, which
/// backs pressure up into the child (`cat` waits for us).
const OUTPUT_IN_FLIGHT: usize = 16;
const READ_CHUNK: usize = 64 * 1024;

/// Longest a program's synchronized update (mode 2026) holds the screen,
/// as in Ghostty: one that never ends (a crash mid-frame) can't freeze it.
const SYNC_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct PaneOptions {
    pub command: Command,
    pub size: Size,
    /// Lines of history.
    pub scrollback: usize,
    pub settings: VtSettings,
}

/// Events for the app; snapshots go through [`Pane::take_snapshot`] instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    Title(String),
    /// The shell's working directory, when it reports one.
    Cwd(PathBuf),
    /// Where the shell says it is, local or not: what to show, rather than
    /// where to open new shells.
    /// `None` once it's unknown (a jetmux pane with ssh in front).
    Location(Option<Location>),
    /// A user variable set (OSC 1337 SetUserVar); empty unsets it.
    UserVar(String, String),
    /// A progress report (OSC 9;4), for the taskbar.
    Progress(Progress),
    /// A program asked for a desktop notification (OSC 9, OSC 777).
    Notify(Notification),
    /// A command finished (OSC 133;D), with its exit status if given.
    CommandFinished(Option<i32>),
    /// The mouse pointer a program wants over the pane (OSC 22).
    Pointer(PointerShape),
    /// The innermost context changed (OSC 3008); `None`: none is open.
    Context(Option<Context>),
    Bell,
    /// Put this text on the clipboard: a selection, or a program's OSC 52.
    Copy(String),
    /// Paste the clipboard (right click with nothing selected).
    Paste,
    /// The child exited; the pane keeps showing its last screen.
    Exited(Option<u32>),
    /// The pane thread stopped on an internal error.
    Failed(String),
}

pub(crate) enum Msg {
    Output(Vec<u8>),
    Exited(Option<u32>),
    Input(Vec<u8>),
    Key(KeyInput),
    Paste(String),
    Wheel {
        notches: i32,
        cell: (u16, u16),
        mods: libghostty_vt::key::Mods,
    },
    Mouse(MouseInput),
    Focus(bool),
    Configure(VtSettings),
    Copy,
    JumpToPrompt(isize),
    CopyCommandOutput,
    CopyMode,
    QuickSelect,
    Search(String, bool),
    SearchStep(isize),
    AutoscrollTick,
    Resize(Size),
    /// The app took the published snapshot. Handling any message ends in
    /// `flush`, which publishes the next one if the screen changed meanwhile.
    SnapshotTaken,
    Shutdown,
}

pub struct Pane {
    backend: Backend,
    program: String,
}

enum Backend {
    Local(Local),
    /// A jetmux daemon's pane.
    Remote(crate::remote::Link),
}

/// A child on a PTY here, and its pane thread.
struct Local {
    tx: Sender<Msg>,
    snapshots: SnapshotSlot,
    events: Receiver<PaneEvent>,
    thread: Option<JoinHandle<()>>,
    pid: Option<u32>,
}

impl Pane {
    /// Spawn the child and the pane's threads. `wake` runs on the pane thread
    /// after each new snapshot or event, so it must be cheap (e.g. poke an
    /// event loop).
    pub fn spawn(opts: PaneOptions, wake: impl Fn() + Send + 'static) -> Result<Self, Error> {
        let (pty, child, reader, writer) = Pty::spawn(&opts.command, opts.size)?;
        let pid = pty.pid();
        let program = opts.command.program.as_ref().map(std::path::Path::new);
        let program = program
            .and_then(std::path::Path::file_stem)
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let snapshots = SnapshotSlot::default();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let reader_tx = tx.clone();
        let (credit_tx, credits) = mpsc::sync_channel(OUTPUT_IN_FLIGHT);
        std::thread::Builder::new()
            .name("jet-pty-read".into())
            .spawn(move || read_loop(reader, &reader_tx, &credit_tx))?;
        let write_tx = spawn_writer(writer)?;
        spawn_waiter(child, tx.clone())?;
        let slot = snapshots.clone();
        let thread = std::thread::Builder::new()
            .name("jet-pane".into())
            .spawn(move || {
                let size = opts.size;
                // The cell size too: mouse positions map to cells with it.
                let vt = VtCore::new(size.cols, size.rows, opts.scrollback).and_then(|mut vt| {
                    let (cw, ch) = (u32::from(size.cell_width), u32::from(size.cell_height));
                    vt.resize(size.cols, size.rows, cw, ch)?;
                    vt.configure(&opts.settings)?;
                    Ok(vt)
                });
                let vt = match vt {
                    Ok(vt) => {
                        let _ = ready_tx.send(Ok(()));
                        vt
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };
                let mut pane = PaneThread {
                    vt,
                    pty: Some(pty),
                    rx,
                    credits,
                    write_tx,
                    event_tx,
                    slot,
                    wake: Box::new(wake),
                    exited: false,
                    pending_wake: false,
                    sync: SyncHold::default(),
                };
                if let Err(err) = pane.run() {
                    let _ = pane.event_tx.send(PaneEvent::Failed(err.to_string()));
                    (pane.wake)();
                }
                pane.shutdown();
            })?;
        ready_rx.recv().map_err(|_| Error::Closed)??;

        Ok(Self {
            backend: Backend::Local(Local {
                tx,
                snapshots,
                events,
                thread: Some(thread),
                pid,
            }),
            program,
        })
    }

    /// A jetmux daemon's pane.
    pub(crate) fn remote(link: crate::remote::Link) -> Self {
        Self {
            backend: Backend::Remote(link),
            program: "jetmux".into(),
        }
    }

    pub(crate) fn is_remote(&self) -> bool {
        matches!(self.backend, Backend::Remote(_))
    }

    fn send(&self, msg: Msg) {
        match &self.backend {
            // A send error means the pane thread is gone; it already
            // reported why.
            Backend::Local(local) => {
                let _ = local.tx.send(msg);
            }
            Backend::Remote(link) => link.send(msg),
        }
    }

    /// What was run, without its directory or extension (`pwsh`); empty
    /// for the login shell.
    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn pid(&self) -> Option<u32> {
        match &self.backend {
            Backend::Local(local) => local.pid,
            Backend::Remote(_) => None,
        }
    }

    /// Bytes to send to the child as typed input.
    pub fn input(&self, bytes: impl Into<Vec<u8>>) {
        self.send(Msg::Input(bytes.into()));
    }

    /// A key event, encoded on the pane thread against the current modes.
    pub fn key(&self, input: KeyInput) {
        self.send(Msg::Key(input));
    }

    /// Text from the clipboard; bracketed if the app enabled that.
    pub fn paste(&self, text: String) {
        self.send(Msg::Paste(text));
    }

    /// Mouse wheel over cell `(col, row)`; `notches` is negative going up.
    pub fn wheel(&self, notches: i32, cell: (u16, u16), mods: libghostty_vt::key::Mods) {
        self.send(Msg::Wheel {
            notches,
            cell,
            mods,
        });
    }

    /// A mouse button or move; see [`VtCore::mouse`].
    pub fn mouse(&self, input: MouseInput) {
        self.send(Msg::Mouse(input));
    }

    /// The window gained or lost focus.
    pub fn focus(&self, focused: bool) {
        self.send(Msg::Focus(focused));
    }

    /// New colors, cursor or selection behavior.
    pub fn configure(&self, settings: VtSettings) {
        self.send(Msg::Configure(settings));
    }

    /// Scroll to the `n`th prompt up (negative) or down; see
    /// [`VtCore::jump_to_prompt`].
    pub fn jump_to_prompt(&self, n: isize) {
        self.send(Msg::JumpToPrompt(n));
    }

    /// Copy the last command's output (OSC 133), if any: arrives as
    /// [`PaneEvent::Copy`].
    pub fn copy_command_output(&self) {
        self.send(Msg::CopyCommandOutput);
    }

    /// Toggle copy mode: keys walk and select; see [`VtCore::copy_mode`].
    /// Copied text arrives as [`PaneEvent::Copy`].
    pub fn copy_mode(&self) {
        self.send(Msg::CopyMode);
    }

    /// Toggle quick select: labels on what's worth copying in view; see
    /// [`VtCore::quick_select`]. The pick arrives as [`PaneEvent::Copy`].
    pub fn quick_select(&self) {
        self.send(Msg::QuickSelect);
    }

    /// Search the screen and history; results come in snapshots. Empty
    /// ends the search. See [`VtCore::search`].
    pub fn search(&self, query: String, regex: bool) {
        self.send(Msg::Search(query, regex));
    }

    /// Go to the `n`th match up (negative, older output) or down.
    pub fn search_step(&self, n: isize) {
        self.send(Msg::SearchStep(n));
    }

    /// Copy the selection, if any: arrives as [`PaneEvent::Copy`].
    pub fn copy(&self) {
        self.send(Msg::Copy);
    }

    /// Sent periodically while a selection drag is held past the top or
    /// bottom edge; scrolls and extends the selection.
    pub fn autoscroll_tick(&self) {
        self.send(Msg::AutoscrollTick);
    }

    pub fn resize(&self, size: Size) {
        self.send(Msg::Resize(size));
    }

    /// The newest snapshot since the last call, if any. Its `dirty` rows are
    /// relative to the previous snapshot taken.
    pub fn take_snapshot(&self) -> Option<Snapshot> {
        match &self.backend {
            Backend::Local(local) => {
                let snapshot = local.snapshots.take();
                if snapshot.is_some() {
                    self.send(Msg::SnapshotTaken);
                }
                snapshot
            }
            Backend::Remote(link) => link.take_snapshot(),
        }
    }

    pub fn try_event(&self) -> Option<PaneEvent> {
        match &self.backend {
            Backend::Local(local) => local.events.try_recv().ok(),
            Backend::Remote(link) => link.try_event(),
        }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(thread) = self.thread.take() {
            // A panic on the pane thread has nothing left to clean up here.
            let _ = thread.join();
        }
    }
}

/// Each chunk takes a credit first; the pane thread returns it once parsed.
fn read_loop(mut reader: jet_pty::Reader, tx: &Sender<Msg>, credits: &SyncSender<()>) {
    let mut buf = vec![0u8; READ_CHUNK];
    while let Ok(n @ 1..) = reader.read(&mut buf) {
        let chunk = buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default();
        if credits.send(()).is_err() || tx.send(Msg::Output(chunk)).is_err() {
            return;
        }
    }
}

fn spawn_waiter(mut child: Child, tx: Sender<Msg>) -> Result<(), Error> {
    std::thread::Builder::new()
        .name("jet-pty-wait".into())
        .spawn(move || {
            let code = child.wait().ok();
            let _ = tx.send(Msg::Exited(code));
        })?;
    Ok(())
}

fn spawn_writer(mut writer: jet_pty::Writer) -> Result<Sender<Vec<u8>>, Error> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("jet-pty-write".into())
        .spawn(move || {
            for bytes in rx {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        })?;
    Ok(tx)
}

struct PaneThread {
    vt: VtCore,
    /// `None` once the child exited and the PTY was closed.
    pty: Option<Pty>,
    rx: Receiver<Msg>,
    /// One per queued output chunk; see [`read_loop`].
    credits: Receiver<()>,
    write_tx: Sender<Vec<u8>>,
    event_tx: Sender<PaneEvent>,
    slot: SnapshotSlot,
    wake: Box<dyn Fn() + Send>,
    exited: bool,
    /// Events were queued since the last wake.
    pending_wake: bool,
    sync: SyncHold,
}

/// Snapshots wait while a program draws a synchronized update (mode 2026),
/// so a frame never shows half-drawn.
#[derive(Debug, Default)]
struct SyncHold {
    /// When the update began.
    since: Option<Instant>,
    /// A snapshot is owed once it ends or times out.
    held: bool,
}

impl SyncHold {
    /// Whether to hold the snapshot now, given that mode 2026 is `on`.
    fn hold(&mut self, on: bool, now: Instant) -> bool {
        if !on {
            *self = Self::default();
            return false;
        }
        let since = *self.since.get_or_insert(now);
        self.held = now.duration_since(since) < SYNC_TIMEOUT;
        self.held
    }

    /// When a held snapshot is due regardless.
    fn deadline(&self) -> Option<Instant> {
        self.since.filter(|_| self.held).map(|t| t + SYNC_TIMEOUT)
    }
}

impl PaneThread {
    /// The next message, or `None` once the pane should stop. A held
    /// synchronized update is flushed when it times out.
    fn next(&mut self) -> Result<Option<Msg>, Error> {
        loop {
            let Some(deadline) = self.sync.deadline() else {
                return Ok(self.rx.recv().ok());
            };
            match self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(msg) => return Ok(Some(msg)),
                Err(RecvTimeoutError::Timeout) => self.flush()?,
                Err(RecvTimeoutError::Disconnected) => return Ok(None),
            }
        }
    }

    fn run(&mut self) -> Result<(), Error> {
        // The empty screen, at once: the window's first frame needs a
        // snapshot, and the shell may take a while to say anything.
        self.flush()?;
        while let Some(mut msg) = self.next()? {
            let mut budget = MAX_BYTES_PER_FLUSH;
            loop {
                match msg {
                    Msg::Output(bytes) => {
                        self.vt.write(&bytes);
                        // Always present: the reader takes it before sending.
                        let _ = self.credits.try_recv();
                        budget = budget.saturating_sub(bytes.len());
                    }
                    Msg::Exited(code) => self.child_exited(code),
                    Msg::Input(bytes) => self.send_to_pty(bytes),
                    Msg::Key(input) => match self.vt.mode_key(&input)? {
                        None => {
                            let bytes = self.vt.encode_key(&input)?;
                            self.send_to_pty(bytes);
                        }
                        Some(Grabbed::Nothing) => {}
                        Some(Grabbed::Copy(text)) => self.event(PaneEvent::Copy(text)),
                        Some(Grabbed::CopyAndPaste(text)) => {
                            let bytes = self.vt.encode_paste(&text)?;
                            self.send_to_pty(bytes);
                            self.event(PaneEvent::Copy(text));
                        }
                    },
                    Msg::Paste(text) => {
                        let bytes = self.vt.encode_paste(&text)?;
                        self.send_to_pty(bytes);
                    }
                    Msg::Wheel {
                        notches,
                        cell,
                        mods,
                    } => {
                        let bytes = self.vt.wheel(notches, cell, mods)?;
                        self.send_to_pty(bytes);
                    }
                    Msg::Mouse(input) => match self.vt.mouse(input)? {
                        MouseOutcome::Nothing => {}
                        MouseOutcome::Pty(bytes) => self.send_to_pty(bytes),
                        MouseOutcome::Copy(text) => self.event(PaneEvent::Copy(text)),
                        MouseOutcome::Paste => self.event(PaneEvent::Paste),
                    },
                    Msg::Focus(focused) => {
                        let bytes = self.vt.focus(focused)?;
                        self.send_to_pty(bytes);
                    }
                    Msg::Configure(settings) => self.vt.configure(&settings)?,
                    Msg::JumpToPrompt(n) => self.vt.jump_to_prompt(n)?,
                    Msg::CopyMode => self.vt.copy_mode()?,
                    Msg::QuickSelect => self.vt.quick_select()?,
                    Msg::Search(query, regex) => self.vt.search(&query, regex)?,
                    Msg::SearchStep(n) => self.vt.search_step(n)?,
                    Msg::CopyCommandOutput => {
                        if let Some(text) = self.vt.last_command_output()? {
                            self.event(PaneEvent::Copy(text));
                        }
                    }
                    Msg::Copy => {
                        if let Some(text) = self.vt.copy_selection()? {
                            self.event(PaneEvent::Copy(text));
                        }
                    }
                    Msg::AutoscrollTick => {
                        self.vt.autoscroll_tick()?;
                    }
                    Msg::Resize(size) => {
                        self.vt.resize(
                            size.cols,
                            size.rows,
                            u32::from(size.cell_width),
                            u32::from(size.cell_height),
                        )?;
                        if let Some(pty) = &self.pty {
                            pty.resize(size)?;
                        }
                    }
                    Msg::SnapshotTaken => {}
                    Msg::Shutdown => return Ok(()),
                }
                // The app took the last snapshot and waits for the next:
                // hand it over now, not after the rest of the queue, or a
                // flood (up to `OUTPUT_IN_FLIGHT` chunks, several ms to
                // parse) makes the app miss frames.
                if budget == 0 || (budget < MAX_BYTES_PER_FLUSH && self.slot.is_empty()) {
                    break;
                }
                match self.rx.try_recv() {
                    Ok(next) => msg = next,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            self.flush()?;
        }
        Ok(())
    }

    /// Hand effects and the new snapshot to their consumers.
    fn flush(&mut self) -> Result<(), Error> {
        let effects = self.vt.take_effects()?;
        if !effects.pty_reply.is_empty() {
            self.send_to_pty(effects.pty_reply);
        }
        let mut notify = std::mem::take(&mut self.pending_wake);
        if let Some(title) = effects.title {
            notify |= self.event_tx.send(PaneEvent::Title(title)).is_ok();
        }
        if let Some(cwd) = effects.cwd {
            notify |= self.event_tx.send(PaneEvent::Cwd(cwd)).is_ok();
        }
        if let Some(location) = effects.location {
            notify |= self
                .event_tx
                .send(PaneEvent::Location(Some(location)))
                .is_ok();
        }
        for (name, value) in effects.vars {
            notify |= self.event_tx.send(PaneEvent::UserVar(name, value)).is_ok();
        }
        if let Some(progress) = effects.progress {
            notify |= self.event_tx.send(PaneEvent::Progress(progress)).is_ok();
        }
        if effects.bell {
            notify |= self.event_tx.send(PaneEvent::Bell).is_ok();
        }
        if let Some(text) = effects.clipboard {
            notify |= self.event_tx.send(PaneEvent::Copy(text)).is_ok();
        }
        for notification in effects.notifications {
            notify |= self.event_tx.send(PaneEvent::Notify(notification)).is_ok();
        }
        if let Some(shape) = effects.pointer {
            notify |= self.event_tx.send(PaneEvent::Pointer(shape)).is_ok();
        }
        if let Some(context) = effects.context {
            notify |= self.event_tx.send(PaneEvent::Context(context)).is_ok();
        }
        for status in effects.finished {
            notify |= self
                .event_tx
                .send(PaneEvent::CommandFinished(status))
                .is_ok();
        }
        // Snapshots cost real time on big grids, so build one only once the
        // app took the last (it sends `SnapshotTaken`): at most one per frame
        // drawn. Changes in between pile up as dirty rows in the terminal.
        let hold = self.sync.hold(self.vt.synchronized()?, Instant::now());
        if !hold
            && self.slot.is_empty()
            && let Some(snapshot) = self.vt.snapshot()?
        {
            self.slot.publish(snapshot);
            notify = true;
        }
        if notify {
            (self.wake)();
        }
        Ok(())
    }

    /// Queue an event; `flush` wakes the app.
    fn event(&mut self, event: PaneEvent) {
        self.pending_wake |= self.event_tx.send(event).is_ok();
    }

    fn send_to_pty(&self, bytes: Vec<u8>) {
        if !self.exited && !bytes.is_empty() {
            // The writer only stops once the PTY is gone; dropping input then is right.
            let _ = self.write_tx.send(bytes);
        }
    }

    fn child_exited(&mut self, code: Option<u32>) {
        self.exited = true;
        // Closing the PTY lets the reader drain the rest and stop.
        self.pty = None;
        let _ = self.event_tx.send(PaneEvent::Exited(code));
        (self.wake)();
    }

    fn shutdown(&mut self) {
        if let Some(pty) = &mut self.pty {
            // Best effort: the child may have exited since we last looked.
            // The waiter thread reaps it.
            let _ = pty.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::pane::{SYNC_TIMEOUT, SyncHold};

    #[test]
    fn synchronized_updates_hold_until_they_end_or_time_out() {
        let start = Instant::now();
        let mut sync = SyncHold::default();
        assert!(!sync.hold(false, start));
        assert!(sync.hold(true, start), "update begins");
        assert_eq!(sync.deadline(), Some(start + SYNC_TIMEOUT));
        assert!(sync.hold(true, start + Duration::from_millis(500)));
        assert!(
            !sync.hold(false, start + Duration::from_millis(600)),
            "ends"
        );
        assert_eq!(sync.deadline(), None);
        // One that never ends shows anyway after the timeout, and then
        // keeps showing.
        assert!(sync.hold(true, start));
        assert!(!sync.hold(true, start + SYNC_TIMEOUT));
        assert_eq!(sync.deadline(), None);
        assert!(!sync.hold(true, start + 2 * SYNC_TIMEOUT));
    }

    /// Real children on a real PTY; they run bash, cat and sleep.
    #[cfg(unix)]
    mod pty {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        use jet_core::{Mods, MouseAction, MouseButton, MouseInput, VtSettings};
        use jet_pty::{Command, Size};

        use crate::pane::{Pane, PaneEvent, PaneOptions};

        const TIMEOUT: Duration = Duration::from_secs(10);

        /// Waits on pane wakeups until `done` holds for the latest screen text and
        /// the events seen so far.
        fn wait_for(
            pane: &Pane,
            wakes: &mpsc::Receiver<()>,
            text: &mut String,
            events: &mut Vec<PaneEvent>,
            done: impl Fn(&str, &[PaneEvent]) -> bool,
        ) -> bool {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                if let Some(snapshot) = pane.take_snapshot() {
                    *text = snapshot.text();
                }
                events.extend(std::iter::from_fn(|| pane.try_event()));
                if done(text, events) {
                    return true;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() || wakes.recv_timeout(left).is_err() {
                    return false;
                }
            }
        }

        #[test]
        fn bash_round_trips_through_the_pty() -> Result<(), jet_core::Error> {
            let command = Command::new("bash").arg("-c").arg(
                r#"printf '\033]2;jet-test\007ready\n'; read -r line; printf 'got:%s\n' "$line"; exit 3"#,
            );
            let (wake_tx, wakes) = mpsc::channel();
            let opts = PaneOptions {
                command,
                size: Size {
                    cols: 40,
                    rows: 10,
                    cell_width: 8,
                    cell_height: 16,
                },
                scrollback: 100,
                settings: VtSettings::default(),
            };
            let pane = Pane::spawn(opts, move || {
                let _ = wake_tx.send(());
            })?;

            let (mut text, mut events) = (String::new(), Vec::new());
            assert!(
                wait_for(&pane, &wakes, &mut text, &mut events, |t, _| t
                    .contains("ready")),
                "never saw ready; screen:\n{text}"
            );

            pane.input("ping\r");
            let exited = |t: &str, e: &[PaneEvent]| {
                t.contains("got:ping") && e.contains(&PaneEvent::Exited(Some(3)))
            };
            assert!(
                wait_for(&pane, &wakes, &mut text, &mut events, exited),
                "no round trip; screen:\n{text}\nevents: {events:?}"
            );
            assert!(events.contains(&PaneEvent::Title("jet-test".into())));
            Ok(())
        }

        /// A flood larger than the reader's in-flight window comes through whole and
        /// the pane still sees the child exit.
        #[test]
        fn flood_completes() -> Result<(), Box<dyn std::error::Error>> {
            let path = std::env::temp_dir().join(format!("jet-flood-{}.txt", std::process::id()));
            let mut flood = String::new();
            for i in 0..20_000u32 {
                flood.push_str(&format!("word{i} "));
                if i % 8 == 7 {
                    flood.push('\n');
                }
            }
            flood.push_str("\nflood-end\n");
            std::fs::write(&path, flood)?;

            let command = Command::new("cat").arg(&path);
            let (wake_tx, wakes) = mpsc::channel();
            let opts = PaneOptions {
                command,
                size: Size {
                    cols: 100,
                    rows: 30,
                    cell_width: 8,
                    cell_height: 16,
                },
                scrollback: 1000,
                settings: VtSettings::default(),
            };
            let pane = Pane::spawn(opts, move || {
                let _ = wake_tx.send(());
            })?;
            let (mut text, mut events) = (String::new(), Vec::new());
            let done = wait_for(&pane, &wakes, &mut text, &mut events, |t, e| {
                t.contains("flood-end") && e.contains(&PaneEvent::Exited(Some(0)))
            });
            let _ = std::fs::remove_file(&path);
            assert!(done, "flood stalled; screen:\n{text}\nevents: {events:?}");
            Ok(())
        }

        /// Mouse positions map to cells using the cell size the pane was spawned
        /// with, before any resize: a double click on the top row picks its word.
        #[test]
        fn clicks_use_the_spawn_cell_size() -> Result<(), jet_core::Error> {
            let command = Command::new("bash")
                .arg("-c")
                .arg("printf 'top\\n\\nbottom'; sleep 10");
            let (wake_tx, wakes) = mpsc::channel();
            let opts = PaneOptions {
                command,
                size: Size {
                    cols: 20,
                    rows: 5,
                    cell_width: 10,
                    cell_height: 20,
                },
                scrollback: 0,
                settings: VtSettings::default(),
            };
            let pane = Pane::spawn(opts, move || {
                let _ = wake_tx.send(());
            })?;
            let (mut text, mut events) = (String::new(), Vec::new());
            assert!(
                wait_for(&pane, &wakes, &mut text, &mut events, |t, _| t
                    .contains("bottom")),
                "never saw output; screen:\n{text}"
            );
            // Row 0, column 1, in pixels.
            let click = |action| MouseInput {
                action,
                x: 15.0,
                y: 10.0,
                mods: Mods::empty(),
            };
            for _ in 0..2 {
                pane.mouse(click(MouseAction::Press(MouseButton::Left)));
                pane.mouse(click(MouseAction::Release(MouseButton::Left)));
            }
            pane.copy();
            let copied =
                |_: &str, e: &[PaneEvent]| e.iter().any(|e| matches!(e, PaneEvent::Copy(_)));
            assert!(
                wait_for(&pane, &wakes, &mut text, &mut events, copied),
                "nothing copied; events: {events:?}"
            );
            assert!(
                events.contains(&PaneEvent::Copy("top".into())),
                "{events:?}"
            );
            Ok(())
        }

        #[test]
        fn a_quiet_shell_still_gets_a_first_frame() -> Result<(), jet_core::Error> {
            let (wake_tx, wakes) = mpsc::channel();
            let opts = PaneOptions {
                command: Command::new("sleep").arg("5"),
                size: Size {
                    cols: 20,
                    rows: 5,
                    cell_width: 8,
                    cell_height: 16,
                },
                scrollback: 10,
                settings: VtSettings::default(),
            };
            let pane = Pane::spawn(opts, move || {
                let _ = wake_tx.send(());
            })?;
            // The window's first frame waits on a snapshot; one must come without
            // any output or input.
            assert!(wakes.recv_timeout(TIMEOUT).is_ok());
            assert!(pane.take_snapshot().is_some());
            Ok(())
        }
    }
}
