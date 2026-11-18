//! One session's event loop. It owns the session's panes (and so their
//! `!Send` libghostty state) and every client attached to it, on one thread.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jet_core::{Block, BlockState as CoreBlockState};
use jetmux_core::snapshot::SessionSnapshot;
use jetmux_core::{ClientId, Error as CoreError, Outcome, Pane, PaneId, Session, Spawn};
use jetmux_proto::rpc::{
    BlockInfo, BlockState, Call, Captured, Event, Layout, Matched, PaneCreated, PaneInfo, RpcError,
    SessionInfo, Streaming, TabInfo, Target,
};
use jetmux_proto::{
    Action, ClientMsg, Cursor, Decoder, Frame, FrameExtras, Modes, PaneSpec, ServerMsg, Size,
    TabSpec, View,
};
use mio::net::UnixStream;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::daemon::EventBus;

/// Frames are coalesced to roughly display rate. Output that arrives after a
/// quiet period is sent at once, so typing latency is unaffected.
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// Past this much unsent output a client stops getting diffs and is sent
/// full frames once it catches up.
const CLIENT_BACKLOG_LIMIT: usize = 4 * 1024 * 1024;
/// PTY output parsed per loop iteration, so frames and clients still get a
/// turn while a pane floods.
const OUTPUT_BUDGET: usize = 1024 * 1024;
/// Batches of PTY output queued between the reader threads and the loop.
/// A full queue blocks the readers, which pushes back on the children.
const OUTPUT_QUEUE: usize = 64;
/// Reader threads only ever sit in poll and read.
const READER_STACK: usize = 64 * 1024;
/// A restored pane's command is typed once its shell has been quiet this
/// long, if the shell doesn't mark its prompt.
const TYPEAHEAD_QUIET: Duration = Duration::from_millis(300);
const TYPEAHEAD_POLL: Duration = Duration::from_millis(100);
/// How long a freeze waits for the PTY readers to stop.
const READER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a pane in a synchronized update (mode 2026) can hold back its
/// frames, in case the app never ends it.
const SYNC_TIMEOUT: Duration = Duration::from_secs(1);
/// How often panes are checked for where their program in front is, while
/// a client wants reports.
const LOCATION_POLL: Duration = Duration::from_secs(1);

const WAKER: Token = Token(0);
/// Pane tokens are `PANE_BASE + id`; client tokens start at `CLIENT_BASE`.
const PANE_BASE: usize = 1;
const CLIENT_BASE: usize = 1 << 32;

type BoxError = Box<dyn std::error::Error>;

pub type CallReply = Sender<Result<Value, RpcError>>;
type FreezeReply = Sender<Result<Frozen, String>>;

pub enum Command {
    /// A client to attach, its size, and whether it follows the session's.
    Attach(StdUnixStream, Size, bool),
    Info(Sender<SessionInfo>),
    /// A socket API call; its session part is already resolved.
    Call(Target, Box<Call>, CallReply),
    /// Stream a pane's raw output (`pane.stream`): the start of the stream
    /// is answered on the second sender, output goes to the first.
    Tap(Target, Sender<Event>, Sender<Result<Streaming, RpcError>>),
    /// Save the session for a restart.
    Snapshot(Sender<Result<SessionSnapshot, String>>),
    /// Stop and hand the session over for a hot upgrade.
    Freeze(Sender<Result<Frozen, String>>),
    /// The config's colors and settings changed.
    Base(Box<jet_core::VtSettings>),
    Kill,
}

/// A session handed over for a hot upgrade: its state, and the PTY masters
/// the state refers to by fd number, which must stay open across the exec.
pub struct Frozen {
    pub snapshot: SessionSnapshot,
    pub masters: Vec<OwnedFd>,
}

/// How a session's loop gets its session.
pub enum Start {
    /// Fresh panes: `tabs`, or one tab running the spawn command.
    New {
        tabs: Vec<TabSpec>,
        profile: Option<String>,
    },
    /// Brought back after a restart. Commands matching `rerun` run again.
    Restore {
        snapshot: SessionSnapshot,
        rerun: Vec<String>,
    },
    /// Taken over from the daemon this one replaced.
    Resume {
        snapshot: SessionSnapshot,
        masters: HashMap<i32, OwnedFd>,
    },
}

/// What the supervisor needs to reach a running session.
pub struct Handle {
    pub commands: Sender<Command>,
    pub waker: Arc<Waker>,
}

impl Handle {
    pub fn send(&self, cmd: Command) -> io::Result<()> {
        self.commands
            .send(cmd)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "session is gone"))?;
        self.waker.wake()
    }
}

pub struct Args {
    pub name: String,
    pub spawn: Spawn,
    pub start: Start,
    pub size: Size,
    pub commands: Receiver<Command>,
    /// Receives the session's waker once its first pane is running, or why
    /// it failed to start.
    pub ready: Sender<Result<Arc<Waker>, String>>,
    pub events: Arc<EventBus>,
}

/// What a pane's reader thread sends the loop.
enum Output {
    Data(PaneId, Vec<u8>),
    /// The child's side of the PTY closed: the pane is done.
    Closed(PaneId),
}

/// A `pane.stream` subscriber, and the pane size it last heard about.
struct Tap {
    output: Sender<Event>,
    size: Size,
}

/// A pending `pane.wait`.
struct Watch {
    pane: PaneId,
    until: Until,
    deadline: Option<Instant>,
    reply: CallReply,
}

enum Until {
    /// A screen line matches.
    Match(Regex),
    /// A block newer than this finished block id finishes.
    BlockDone(u64),
}

struct Conn {
    stream: UnixStream,
    decoder: Decoder,
    out: Vec<u8>,
    view: Option<View>,
    /// Panes this client has a full frame for and gets diffs of.
    synced: HashSet<PaneId>,
    /// Asked for pane reports ([`ClientMsg::WantPaneEvents`]), and with
    /// them pictures.
    reports: bool,
    /// Pictures this client has been sent, by key.
    pictures: HashSet<u64>,
    detaching: bool,
    closed: bool,
}

impl Conn {
    fn queue(&mut self, msg: &ServerMsg) {
        if let Err(e) = jetmux_proto::encode(msg, &mut self.out) {
            eprintln!("jetmux: encoding a message for a client failed: {e}");
            self.closed = true;
        }
    }

    /// The pictures `images` shows that this client lacks, if it wants
    /// pictures at all.
    fn queue_pictures(&mut self, pane: &jetmux_core::Pane, images: &[u64]) {
        if !self.reports {
            return;
        }
        for &key in images {
            if self.pictures.contains(&key) {
                continue;
            }
            let Some(picture) = pane.picture(key) else {
                continue;
            };
            self.pictures.insert(key);
            // One frame holds at most MAX_FRAME_LEN; a picture that big
            // isn't shown.
            if picture.rgba.len() > jetmux_proto::MAX_FRAME_LEN - 1024 {
                continue;
            }
            self.queue(&ServerMsg::Picture {
                key,
                width: picture.width,
                height: picture.height,
                rgba: picture.rgba.clone(),
            });
        }
    }

    fn visible(&self, pane: PaneId) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| v.panes.iter().any(|p| p.id == pane))
    }

    fn flush(&mut self) {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(0) => {
                    self.closed = true;
                    return;
                }
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.closed = true;
                    return;
                }
            }
        }
        if self.detaching {
            self.closed = true;
        }
    }
}

struct Loop {
    poll: Poll,
    session: Session,
    clients: HashMap<Token, Conn>,
    next_client: usize,
    registered: HashSet<PaneId>,
    /// Panes whose state may have changed since the last diff.
    pending: HashSet<PaneId>,
    output: Receiver<Output>,
    readers: HashMap<PaneId, JoinHandle<()>>,
    /// Cloned into each pane's reader thread.
    output_tx: SyncSender<Output>,
    /// Set by a reader that queued output, so only the first one since the
    /// loop last looked has to wake it.
    output_ready: Arc<AtomicBool>,
    /// The output budget ran out with output still queued.
    backlog: bool,
    waker: Arc<Waker>,
    /// Cursor and modes last sent per pane, to skip empty diffs.
    sent: HashMap<PaneId, (Option<Cursor>, Modes, FrameExtras)>,
    /// When each pane in a synchronized update started holding its frames.
    held: HashMap<PaneId, Instant>,
    taps: HashMap<PaneId, Vec<Tap>>,
    views_dirty: bool,
    last_frame: Instant,
    last_location_poll: Instant,
    /// Scratch space for client reads.
    buf: Vec<u8>,
    events: Arc<EventBus>,
    watches: Vec<Watch>,
}

pub fn run(args: Args) {
    let Args {
        name,
        spawn,
        start,
        size,
        commands,
        ready,
        events,
    } = args;
    let session = match start_session(name, spawn, start, size) {
        Ok(session) => session,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let mut lp = match Loop::start(session, events) {
        Ok((lp, waker)) => {
            let _ = ready.send(Ok(waker));
            lp
        }
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    match lp.run(&commands) {
        Ok(Some(reply)) => {
            let _ = reply.send(lp.freeze().map_err(|e| e.to_string()));
            return;
        }
        Ok(None) => {}
        Err(e) => eprintln!("jetmux: session {}: {e}", lp.session.name()),
    }
    for conn in lp.clients.values_mut() {
        conn.flush();
    }
}

fn start_session(
    name: String,
    spawn: Spawn,
    start: Start,
    size: Size,
) -> Result<Session, BoxError> {
    let mut session = match start {
        Start::New { tabs, profile } => {
            let mut session = Session::new(name, spawn, size, &tabs)?;
            session.set_profile(profile);
            session
        }
        Start::Restore { snapshot, rerun } => {
            // A profile declared its commands, so they're safe to run again.
            let declared = snapshot.profile.is_some();
            Session::restore(&snapshot, spawn, |command| {
                declared || crate::config::rerun_allowed(&rerun, command)
            })?
        }
        Start::Resume {
            snapshot,
            mut masters,
        } => Session::resume(&snapshot, spawn, |pane, command| {
            let master = pane
                .live
                .as_ref()
                .and_then(|live| masters.remove(&live.fd))
                .ok_or_else(|| CoreError::Spec(format!("pane {} has no PTY", pane.id)))?;
            Pane::adopt(master, pane, command)
        })?,
    };
    if let Ok(config) = crate::config::load() {
        session.set_base(config.shared.vt_settings())?;
    }
    Ok(session)
}

fn pane_token(id: PaneId) -> Token {
    Token(PANE_BASE + id as usize)
}

fn client_id(token: Token) -> ClientId {
    token.0
}

impl Loop {
    fn start(session: Session, events: Arc<EventBus>) -> Result<(Self, Arc<Waker>), BoxError> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKER)?);
        let (output_tx, output) = std::sync::mpsc::sync_channel(OUTPUT_QUEUE);
        let mut lp = Self {
            poll,
            session,
            readers: HashMap::new(),
            clients: HashMap::new(),
            next_client: CLIENT_BASE,
            registered: HashSet::new(),
            pending: HashSet::new(),
            output,
            output_tx,
            output_ready: Arc::new(AtomicBool::new(false)),
            backlog: false,
            waker: Arc::clone(&waker),
            sent: HashMap::new(),
            held: HashMap::new(),
            taps: HashMap::new(),
            views_dirty: false,
            last_frame: Instant::now(),
            last_location_poll: Instant::now(),
            buf: vec![0; 64 * 1024],
            events,
            watches: Vec::new(),
        };
        lp.sync_panes()?;
        Ok((lp, waker))
    }

    /// Run until the session ends, or until it's asked to freeze, which
    /// returns where to send the frozen session.
    fn run(&mut self, commands: &Receiver<Command>) -> Result<Option<FreezeReply>, BoxError> {
        let mut events = Events::with_capacity(64);
        loop {
            let deadline = self
                .watches
                .iter()
                .filter_map(|w| w.deadline)
                .min()
                .map(|d| d.saturating_duration_since(Instant::now()));
            let timeout = if self.backlog {
                Some(Duration::ZERO)
            } else if !self.pending.is_empty() {
                Some(FRAME_INTERVAL.saturating_sub(self.last_frame.elapsed()))
            } else {
                None
            };
            let typing = self
                .session
                .panes_mut()
                .any(|p| p.has_typeahead())
                .then_some(TYPEAHEAD_POLL);
            let location = self
                .clients
                .values()
                .any(|c| c.reports)
                .then(|| LOCATION_POLL.saturating_sub(self.last_location_poll.elapsed()));
            let timeout = [timeout, deadline, typing, location]
                .into_iter()
                .flatten()
                .min();
            if let Err(e) = self.poll.poll(&mut events, timeout) {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }

            let mut exited = self.drain_output();
            for event in &events {
                match event.token() {
                    WAKER => {
                        while let Ok(cmd) = commands.try_recv() {
                            match cmd {
                                Command::Attach(stream, size, follow) => {
                                    self.attach(stream, size, follow)?
                                }
                                Command::Info(reply) => {
                                    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                                    let _ = reply.send(SessionInfo {
                                        name: self.session.name().to_owned(),
                                        clients: count(self.session.clients()),
                                        tabs: count(self.session.tab_count()),
                                        panes: count(self.session.pane_ids().len()),
                                        size: self.session.size(),
                                    });
                                }
                                Command::Call(target, call, reply) => {
                                    self.call(&target, *call, reply);
                                    self.sync_panes()?;
                                    self.touch_all();
                                }
                                Command::Tap(target, output, reply) => {
                                    let _ = reply.send(self.tap(&target, output));
                                }
                                Command::Snapshot(reply) => {
                                    let _ = reply
                                        .send(self.session.snapshot().map_err(|e| e.to_string()));
                                }
                                Command::Freeze(reply) => return Ok(Some(reply)),
                                Command::Base(settings) => self.session.set_base(*settings)?,
                                Command::Kill => {
                                    for conn in self.clients.values_mut() {
                                        conn.queue(&ServerMsg::Exited);
                                    }
                                    return Ok(None);
                                }
                            }
                        }
                    }
                    token if token.0 >= CLIENT_BASE => {
                        if event.is_readable() || event.is_read_closed() {
                            self.read_client(token)?;
                        }
                        if let Some(conn) = self.clients.get_mut(&token) {
                            conn.flush();
                        }
                    }
                    token => {
                        let Ok(id) = PaneId::try_from(token.0 - PANE_BASE) else {
                            continue;
                        };
                        if event.is_writable()
                            && let Some(pane) = self.session.pane_mut(id)
                            && pane.flush().is_err()
                        {
                            exited.push(id);
                        }
                    }
                }
            }

            for id in exited {
                self.session.remove_pane(id);
                self.views_dirty = true;
            }
            if self.session.is_empty() {
                for conn in self.clients.values_mut() {
                    conn.queue(&ServerMsg::Exited);
                }
                return Ok(None);
            }
            for pane in self.session.panes_mut() {
                if pane.has_typeahead()
                    && let Err(e) = pane.type_when_quiet(TYPEAHEAD_QUIET)
                {
                    eprintln!("jetmux: typing a restored command: {e}");
                }
            }
            self.sync_panes()?;
            self.resize_taps();
            let frame_due = !self.pending.is_empty() && self.last_frame.elapsed() >= FRAME_INTERVAL;
            self.check_watches(frame_due);
            self.reap_clients();
            if self.views_dirty {
                self.send_views();
            }
            self.poll_locations();
            if frame_due {
                self.send_diffs()?;
            }
            self.send_full_frames()?;
        }
    }

    /// Feed queued PTY output to its panes, up to the budget. Returns the
    /// panes whose children have exited.
    fn drain_output(&mut self) -> Vec<PaneId> {
        // Cleared before draining, so output queued from here on wakes us.
        self.output_ready.store(false, Ordering::SeqCst);
        let mut exited = Vec::new();
        let mut bytes = 0;
        self.backlog = false;
        while bytes < OUTPUT_BUDGET {
            match self.output.try_recv() {
                Ok(Output::Data(id, data)) => {
                    bytes += data.len();
                    self.feed(id, &data);
                }
                Ok(Output::Closed(id)) if self.session.pane(id).is_some() => exited.push(id),
                Ok(Output::Closed(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return exited,
            }
        }
        self.backlog = true;
        exited
    }

    fn feed(&mut self, id: PaneId, data: &[u8]) {
        let session = self.session.name().to_owned();
        let Some(pane) = self.session.pane_mut(id) else {
            return;
        };
        if let Err(e) = pane.feed(data) {
            eprintln!("jetmux: pane {id}: {e}");
        }
        if let Some(taps) = self.taps.get_mut(&id) {
            let data = jetmux_proto::b64::encode(data);
            taps.retain(|tap| {
                let output = Event::Output {
                    session: session.clone(),
                    pane: id,
                    data: data.clone(),
                };
                tap.output.send(output).is_ok()
            });
        }
        self.pending.insert(id);
        for block in pane.take_finished_blocks() {
            if let Some(block) = pane.blocks().get(block) {
                self.events.publish(&Event::BlockDone {
                    session: session.clone(),
                    pane: id,
                    block: block_info(block),
                });
            }
        }
        for (title, body) in pane.take_notifications() {
            self.events.publish(&Event::Notify {
                session: session.clone(),
                pane: id,
                title,
                body,
            });
        }
        let reports = pane.take_reports();
        for conn in self.clients.values_mut().filter(|c| c.reports) {
            for event in &reports {
                conn.queue(&ServerMsg::Pane {
                    pane: id,
                    event: event.clone(),
                });
            }
        }
        if let Some(text) = pane.take_clipboard() {
            for conn in self.clients.values_mut() {
                conn.queue(&ServerMsg::AppClipboard(text.clone()));
            }
        }
        let events = pane.take_events();
        let title = events.title_changed.then(|| pane.title().to_owned());
        if let Some(title) = title {
            self.views_dirty = true;
            self.events.publish(&Event::TitleChanged {
                session: session.clone(),
                pane: id,
                title,
            });
        }
        if events.bell {
            for conn in self.clients.values_mut().filter(|c| c.visible(id)) {
                conn.queue(&ServerMsg::Bell);
            }
            self.events.publish(&Event::Bell { session, pane: id });
        }
    }

    /// Read a pane's PTY on a thread of its own until the pane goes away.
    fn spawn_reader(&mut self, id: PaneId) -> io::Result<()> {
        let Some(pane) = self.session.pane_mut(id) else {
            return Ok(());
        };
        let reader = pane.reader()?;
        let tx = self.output_tx.clone();
        let ready = Arc::clone(&self.output_ready);
        let waker = Arc::clone(&self.waker);
        let send = move |output| {
            if tx.send(output).is_err() {
                return false;
            }
            if !ready.swap(true, Ordering::SeqCst) {
                let _ = waker.wake();
            }
            true
        };
        let thread = std::thread::Builder::new()
            .name(format!("jetmux-pty-{id}"))
            .stack_size(READER_STACK)
            .spawn(move || {
                if let Err(e) = reader.run(|data| send(Output::Data(id, data))) {
                    eprintln!("jetmux: pane {id}: {e}");
                }
                send(Output::Closed(id));
            })?;
        self.readers.insert(id, thread);
        Ok(())
    }

    /// Hand the session over for a hot upgrade. Clients are told to
    /// reconnect; output the readers already took is parsed first, so
    /// nothing read from a PTY is lost.
    fn freeze(mut self) -> Result<Frozen, BoxError> {
        for conn in self.clients.values_mut() {
            conn.queue(&ServerMsg::Reconnect);
            conn.flush();
        }
        self.session.stop_readers();
        let deadline = Instant::now() + READER_STOP_TIMEOUT;
        loop {
            // Readers block on a full queue, so keep draining while they stop.
            // `Closed` only means a reader stopped; the panes live on.
            while let Ok(output) = self.output.try_recv() {
                if let Output::Data(id, data) = output {
                    self.feed(id, &data);
                }
            }
            self.readers.retain(|_, r| !r.is_finished());
            if self.readers.is_empty() || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        while let Ok(Output::Data(id, data)) = self.output.try_recv() {
            self.feed(id, &data);
        }
        let (snapshot, masters) = self.session.freeze()?;
        Ok(Frozen { snapshot, masters })
    }

    /// Start streaming a pane's output, from its visible screen.
    fn tap(&mut self, target: &Target, output: Sender<Event>) -> Result<Streaming, RpcError> {
        let id = self.resolve(target)?;
        let session = self.session.name().to_owned();
        let pane = self
            .session
            .pane_mut(id)
            .ok_or_else(|| RpcError::failed("pane is gone"))?;
        let frame = pane
            .frame(true)
            .map_err(|e| RpcError::failed(e.to_string()))?;
        let started = Streaming {
            session,
            pane: id,
            size: frame.size,
            title: pane.title().to_owned(),
            screen: jetmux_render::frame_to_vt(&frame),
        };
        self.taps.entry(id).or_default().push(Tap {
            output,
            size: frame.size,
        });
        Ok(started)
    }

    /// Tell streams about panes whose size changed.
    fn resize_taps(&mut self) {
        let session = self.session.name();
        for (id, taps) in &mut self.taps {
            let Some(size) = self.session.pane(*id).and_then(|p| p.size().ok()) else {
                continue;
            };
            taps.retain_mut(|tap| {
                if tap.size == size {
                    return true;
                }
                tap.size = size;
                let resized = Event::Resized {
                    session: session.to_owned(),
                    pane: *id,
                    size,
                };
                tap.output.send(resized).is_ok()
            });
        }
    }

    /// Keep poll registrations in step with the session's panes. A dropped
    /// pane's fd is already closed, which removes it from the poll set.
    fn sync_panes(&mut self) -> Result<(), BoxError> {
        let live: HashSet<PaneId> = self.session.pane_ids().into_iter().collect();
        for id in self.registered.difference(&live) {
            self.events.publish(&Event::PaneExited {
                session: self.session.name().to_owned(),
                pane: *id,
            });
        }
        // Output comes from the reader threads; the loop only writes.
        for id in live
            .difference(&self.registered)
            .copied()
            .collect::<Vec<_>>()
        {
            if let Some(pane) = self.session.pane(id) {
                self.poll.registry().register(
                    &mut SourceFd(&pane.raw_fd()),
                    pane_token(id),
                    Interest::WRITABLE,
                )?;
                self.spawn_reader(id)?;
                self.pending.insert(id);
            }
        }
        self.registered = live;
        let live = &self.registered;
        self.pending.retain(|id| live.contains(id));
        self.sent.retain(|id, _| live.contains(id));
        self.held.retain(|id, _| live.contains(id));
        // Dropping a gone pane's taps ends their streams.
        self.taps.retain(|id, _| live.contains(id));
        Ok(())
    }

    fn attach(&mut self, stream: StdUnixStream, size: Size, follow: bool) -> Result<(), BoxError> {
        stream.set_nonblocking(true)?;
        let mut stream = UnixStream::from_std(stream);
        let token = Token(self.next_client);
        self.next_client += 1;
        self.poll.registry().register(
            &mut stream,
            token,
            Interest::READABLE | Interest::WRITABLE,
        )?;
        let mut conn = Conn {
            stream,
            decoder: Decoder::default(),
            out: Vec::new(),
            view: None,
            synced: HashSet::new(),
            reports: false,
            pictures: HashSet::new(),
            detaching: false,
            closed: false,
        };
        conn.queue(&ServerMsg::Attached {
            session: self.session.name().to_owned(),
        });
        self.clients.insert(token, conn);
        self.session.add_client(client_id(token), size, follow)?;
        self.touch_all();
        Ok(())
    }

    fn read_client(&mut self, token: Token) -> Result<(), BoxError> {
        let Some(conn) = self.clients.get_mut(&token) else {
            return Ok(());
        };
        loop {
            match conn.stream.read(&mut self.buf) {
                Ok(0) => {
                    conn.closed = true;
                    break;
                }
                Ok(n) => conn.decoder.push(&self.buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    conn.closed = true;
                    break;
                }
            }
        }
        let client = client_id(token);
        loop {
            let Some(conn) = self.clients.get_mut(&token) else {
                return Ok(());
            };
            let msg = match conn.decoder.next_msg::<ClientMsg>() {
                Ok(Some(msg)) => msg,
                Ok(None) => return Ok(()),
                Err(e) => {
                    eprintln!("jetmux: dropping client sending bad frames: {e}");
                    conn.closed = true;
                    return Ok(());
                }
            };
            let reply = match msg {
                ClientMsg::Input(data) => {
                    self.session.input(client, &data)?;
                    None
                }
                ClientMsg::Action(action) => reply(self.session.action(client, action)),
                ClientMsg::Key { pane, event } => reply(self.session.key(client, pane, &event)),
                ClientMsg::Mouse(mouse) => reply(self.session.mouse(client, &mouse)),
                ClientMsg::Wheel {
                    pane,
                    notches,
                    col,
                    row,
                    mods,
                } => {
                    self.session.wheel(pane, notches, (col, row), mods)?;
                    None
                }
                ClientMsg::Paste { pane, text } => {
                    self.session.paste(client, pane, &text)?;
                    None
                }
                ClientMsg::Focus { pane, focused } => {
                    self.session.focus(pane, focused)?;
                    None
                }
                ClientMsg::Theme(theme) => {
                    self.session.set_theme(client, &theme)?;
                    None
                }
                ClientMsg::CellSize { width, height } => {
                    self.session.set_cell_size(client, (width, height))?;
                    None
                }
                ClientMsg::WantPaneEvents => {
                    conn.reports = true;
                    for id in self.session.pane_ids() {
                        let Some(pane) = self.session.pane(id) else {
                            continue;
                        };
                        for event in pane.state_reports() {
                            conn.queue(&ServerMsg::Pane { pane: id, event });
                        }
                    }
                    None
                }
                ClientMsg::Resize(size) => {
                    self.session.resize_client(client, size)?;
                    // The client cleared its screen; resend everything.
                    conn.view = None;
                    conn.synced.clear();
                    None
                }
                ClientMsg::Follow(follow) => {
                    self.session.set_follow(client, follow)?;
                    conn.view = None;
                    conn.synced.clear();
                    None
                }
                ClientMsg::Detach => {
                    conn.detaching = true;
                    Some(ServerMsg::Detached)
                }
                other => Some(ServerMsg::Error(format!(
                    "{other:?} is not valid while attached"
                ))),
            };
            if let (Some(reply), Some(conn)) = (reply, self.clients.get_mut(&token)) {
                conn.queue(&reply);
            }
            // Input can move pane sizes to this client and actions can change
            // anything. Unchanged views and empty diffs aren't sent, so this
            // stays cheap.
            self.sync_panes()?;
            self.touch_all();
        }
    }

    fn call(&mut self, target: &Target, call: Call, reply: CallReply) {
        if let Call::PaneWait {
            pattern,
            block_done,
            after_block,
            timeout_ms,
            ..
        } = &call
        {
            let watch = self.resolve(target).and_then(|pane| {
                let until = match (pattern, block_done) {
                    (None, true) => Until::BlockDone(after_block.unwrap_or_else(|| {
                        self.session
                            .pane(pane)
                            .and_then(|p| p.blocks().last_done())
                            .map_or(0, |b| b.id)
                    })),
                    (Some(pattern), false) => Until::Match(
                        Regex::new(pattern).map_err(|e| RpcError::failed(e.to_string()))?,
                    ),
                    _ => return Err(RpcError::failed("wait for a pattern or block_done")),
                };
                Ok(Watch {
                    pane,
                    until,
                    deadline: timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms)),
                    reply: reply.clone(),
                })
            });
            match watch {
                Ok(watch) => match watch_answer(&self.session, &watch, true) {
                    Some(answer) => {
                        let _ = reply.send(answer);
                    }
                    None => self.watches.push(watch),
                },
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            }
            return;
        }
        let _ = reply.send(self.answer(target, call));
    }

    fn resolve(&self, target: &Target) -> Result<PaneId, RpcError> {
        self.session
            .resolve(target.tab.as_ref(), target.pane)
            .map_err(|e| RpcError::failed(e.to_string()))
    }

    fn act(&mut self, target: &Target, action: Action) -> Result<jetmux_core::Acted, RpcError> {
        let pane = self.resolve(target)?;
        let acted = self
            .session
            .act_on(pane, action)
            .map_err(|e| RpcError::failed(e.to_string()))?;
        match &acted.outcome {
            Outcome::Message(msg) => Err(RpcError::failed(msg.clone())),
            Outcome::None | Outcome::Clipboard(_) | Outcome::WantPaste(_) => Ok(acted),
        }
    }

    fn answer(&mut self, target: &Target, call: Call) -> Result<Value, RpcError> {
        let failed = |e: jetmux_core::Error| RpcError::failed(e.to_string());
        let session = self.session.name().to_owned();
        let created = |acted: jetmux_core::Acted| match acted.focus {
            Some(pane) => to_value(PaneCreated { pane }),
            None => Err(RpcError::failed("no pane was created")),
        };
        match call {
            Call::TabList { .. } => {
                let tabs: Vec<TabInfo> = (1..)
                    .zip(self.session.tab_list())
                    .map(|(index, (name, panes))| TabInfo {
                        session: session.clone(),
                        index,
                        name,
                        panes,
                    })
                    .collect();
                to_value(tabs)
            }
            Call::PaneList { .. } => {
                let focus = self.session.reference_focus();
                let mut panes = Vec::new();
                for (tab, (_, ids)) in (1..).zip(self.session.tab_list()) {
                    if target.tab.is_some()
                        && self.resolve(target).ok().is_none_or(|p| !ids.contains(&p))
                    {
                        continue;
                    }
                    for (index, id) in (1..).zip(ids) {
                        let Some(pane) = self.session.pane_mut(id) else {
                            continue;
                        };
                        panes.push(PaneInfo {
                            session: session.clone(),
                            tab,
                            index,
                            id,
                            title: pane.title().to_owned(),
                            cwd: pane.cwd(),
                            command: pane.command().to_vec(),
                            size: pane.size().map_err(failed)?,
                            focused: Some(id) == focus,
                        });
                    }
                }
                to_value(panes)
            }
            Call::TabNew {
                name, command, cwd, ..
            } => {
                let acted = self.act(target, Action::NewTab(PaneSpec { command, cwd }))?;
                if let (Some(name), Some(pane)) = (name, acted.focus) {
                    self.session
                        .act_on(pane, Action::RenameTab(name))
                        .map_err(failed)?;
                }
                created(acted)
            }
            Call::TabClose { .. } => self.act(target, Action::CloseTab).map(|_| Value::Null),
            Call::TabSelect { .. } => {
                let pane = self.resolve(target)?;
                self.act(target, Action::FocusPane(pane))
                    .map(|_| Value::Null)
            }
            Call::TabRename { name, .. } => self
                .act(target, Action::RenameTab(name))
                .map(|_| Value::Null),
            Call::TabMove { delta, .. } => self
                .act(target, Action::MoveTab(delta))
                .map(|_| Value::Null),
            Call::PaneSplit {
                direction,
                command,
                cwd,
                ..
            } => created(self.act(target, Action::Split(direction, PaneSpec { command, cwd }))?),
            Call::PaneClose { .. } => self.act(target, Action::ClosePane).map(|_| Value::Null),
            Call::PaneFocus { direction, .. } => {
                let action = match direction {
                    Some(dir) => Action::Focus(dir),
                    None => Action::FocusPane(self.resolve(target)?),
                };
                self.act(target, action).map(|_| Value::Null)
            }
            Call::PaneZoom { .. } => self.act(target, Action::ToggleZoom).map(|_| Value::Null),
            Call::PaneSwap { direction, .. } => self
                .act(target, Action::Swap(direction))
                .map(|_| Value::Null),
            Call::PaneResize {
                direction, cells, ..
            } => self
                .act(target, Action::Resize(direction, cells))
                .map(|_| Value::Null),
            Call::PaneSendKeys { keys, .. } => {
                let id = self.resolve(target)?;
                let pane = self
                    .session
                    .pane_mut(id)
                    .ok_or_else(|| RpcError::failed("pane is gone"))?;
                pane.send(keys.as_bytes())
                    .map_err(|e| RpcError::failed(e.to_string()))?;
                Ok(Value::Null)
            }
            Call::PaneCapture { scrollback, .. } => {
                let id = self.resolve(target)?;
                let pane = self
                    .session
                    .pane_mut(id)
                    .ok_or_else(|| RpcError::failed("pane is gone"))?;
                to_value(Captured {
                    text: pane.capture(scrollback).map_err(failed)?,
                })
            }
            Call::PaneBlocks { .. } => {
                let id = self.resolve(target)?;
                let pane = self
                    .session
                    .pane(id)
                    .ok_or_else(|| RpcError::failed("pane is gone"))?;
                to_value(pane.blocks().iter().map(block_info).collect::<Vec<_>>())
            }
            Call::PaneReadBlock { block, .. } => {
                let id = self.resolve(target)?;
                let pane = self
                    .session
                    .pane(id)
                    .ok_or_else(|| RpcError::failed("pane is gone"))?;
                match pane.block_output(block).map_err(failed)? {
                    Some(text) => to_value(Captured { text }),
                    None => Err(RpcError::failed("no such block")),
                }
            }
            Call::LayoutSave { .. } => to_value(Layout {
                tabs: self.session.layout_spec(),
            }),
            other => Err(RpcError::failed(format!("{other:?} is not a session call"))),
        }
    }

    /// Answer waits whose pattern now matches, whose pane is gone, or whose
    /// time ran out.
    /// Answer the watches that matched or timed out. Screens are only
    /// matched on a frame tick and only for panes with new output: scanning
    /// a busy pane on every read would cost more than parsing its output.
    fn check_watches(&mut self, frame_due: bool) {
        let (session, pending) = (&self.session, &self.pending);
        self.watches.retain(|w| {
            let scan = frame_due && pending.contains(&w.pane);
            match watch_answer(session, w, scan) {
                Some(answer) => {
                    let _ = w.reply.send(answer);
                    false
                }
                None => true,
            }
        });
    }

    fn touch_all(&mut self) {
        self.pending.extend(self.session.pane_ids());
        self.views_dirty = true;
    }

    /// Tell clients that want reports where programs in panes without OSC 7
    /// moved.
    fn poll_locations(&mut self) {
        if self.last_location_poll.elapsed() < LOCATION_POLL
            || !self.clients.values().any(|c| c.reports)
        {
            return;
        }
        self.last_location_poll = Instant::now();
        for id in self.session.pane_ids() {
            let Some(event) = self.session.pane_mut(id).and_then(|p| p.poll_location()) else {
                continue;
            };
            for conn in self.clients.values_mut().filter(|c| c.reports) {
                conn.queue(&ServerMsg::Pane {
                    pane: id,
                    event: event.clone(),
                });
                conn.flush();
            }
        }
    }

    fn send_views(&mut self) {
        self.views_dirty = false;
        for (token, conn) in &mut self.clients {
            let Some(view) = self.session.view(client_id(*token)) else {
                continue;
            };
            if conn.view.as_ref() == Some(&view) {
                continue;
            }
            conn.synced
                .retain(|id| view.panes.iter().any(|p| p.id == *id));
            conn.queue(&ServerMsg::View(view.clone()));
            conn.view = Some(view);
            conn.flush();
        }
    }

    fn send_diffs(&mut self) -> Result<(), BoxError> {
        let pending: Vec<PaneId> = self.pending.drain().collect();
        for id in pending {
            let Some(pane) = self.session.pane_mut(id) else {
                continue;
            };
            if pane.synchronized() {
                let since = *self.held.entry(id).or_insert_with(Instant::now);
                if since.elapsed() < SYNC_TIMEOUT {
                    self.pending.insert(id);
                    continue;
                }
            }
            self.held.remove(&id);
            let frame = pane.frame(false)?;
            let state = (frame.cursor, frame.modes, frame.extras.clone());
            if !frame.full && frame.rows.is_empty() && self.sent.get(&id) == Some(&state) {
                continue;
            }
            self.sent.insert(id, state);
            let images: Vec<u64> = frame.extras.images.iter().map(|i| i.picture).collect();
            let encoded = encode_frame(id, frame)?;
            for conn in self.clients.values_mut() {
                if !conn.synced.contains(&id) || !conn.visible(id) {
                    continue;
                }
                if conn.out.len() > CLIENT_BACKLOG_LIMIT {
                    conn.synced.remove(&id);
                } else {
                    conn.queue_pictures(pane, &images);
                    conn.out.extend_from_slice(&encoded);
                }
            }
        }
        for conn in self.clients.values_mut() {
            conn.flush();
        }
        self.last_frame = Instant::now();
        Ok(())
    }

    /// Clients get a pane's diffs only after a full frame of it: when they
    /// attach, start looking at it, resize, or catch up after falling behind.
    fn send_full_frames(&mut self) -> Result<(), BoxError> {
        let mut cache: HashMap<PaneId, (Vec<u8>, Vec<u64>)> = HashMap::new();
        for conn in self.clients.values_mut() {
            if conn.out.len() > CLIENT_BACKLOG_LIMIT {
                continue;
            }
            let Some(view) = &conn.view else {
                continue;
            };
            let missing: Vec<PaneId> = view
                .panes
                .iter()
                .map(|p| p.id)
                .filter(|id| !conn.synced.contains(id))
                .collect();
            for id in missing {
                let Some(pane) = self.session.pane_mut(id) else {
                    continue;
                };
                let (encoded, images) = match cache.entry(id) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let frame = pane.frame(true)?;
                        let images = frame.extras.images.iter().map(|i| i.picture).collect();
                        entry.insert((encode_frame(id, frame)?, images))
                    }
                };
                conn.queue_pictures(pane, images);
                conn.out.extend_from_slice(encoded);
                conn.synced.insert(id);
            }
            conn.flush();
        }
        Ok(())
    }

    fn reap_clients(&mut self) {
        let closed: Vec<Token> = self
            .clients
            .iter()
            .filter(|(_, c)| c.closed)
            .map(|(t, _)| *t)
            .collect();
        for token in closed {
            if let Some(mut conn) = self.clients.remove(&token) {
                let _ = self.poll.registry().deregister(&mut conn.stream);
            }
            self.session.remove_client(client_id(token));
            self.views_dirty = true;
        }
    }
}

fn to_value(value: impl Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::failed(e.to_string()))
}

fn encode_frame(pane: PaneId, frame: Frame) -> Result<Vec<u8>, jetmux_proto::Error> {
    let mut out = Vec::new();
    jetmux_proto::encode(&ServerMsg::PaneFrame { pane, frame }, &mut out)?;
    Ok(out)
}
fn block_info(b: &Block) -> BlockInfo {
    BlockInfo {
        id: b.id,
        command: b.command.clone(),
        state: match b.state() {
            CoreBlockState::Prompt => BlockState::Prompt,
            CoreBlockState::Running => BlockState::Running,
            CoreBlockState::Done => BlockState::Done,
        },
        exit: b.exit,
        duration_ms: b
            .elapsed()
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
    }
}

/// A watch's answer: its first matching screen line (when `scan`), an error
/// once its pane is gone or its deadline passes, or None to keep waiting.
fn watch_answer(session: &Session, w: &Watch, scan: bool) -> Option<Result<Value, RpcError>> {
    let answer = match session.pane(w.pane) {
        None => Some(Err(RpcError::failed("the pane exited"))),
        Some(_) if !scan => None,
        Some(pane) => match &w.until {
            Until::Match(pattern) => match pane.screen_text() {
                Err(e) => Some(Err(RpcError::failed(e.to_string()))),
                Ok(lines) => lines
                    .into_iter()
                    .find(|l| pattern.is_match(l))
                    .map(|line| to_value(Matched { line })),
            },
            Until::BlockDone(after) => pane
                .blocks()
                .last_done()
                .filter(|b| b.id > *after)
                .map(|b| to_value(block_info(b))),
        },
    };
    answer.or_else(|| {
        w.deadline
            .is_some_and(|d| d <= Instant::now())
            .then(|| Err(RpcError::failed("timed out")))
    })
}

/// What a client hears back from something it did.
fn reply(outcome: Result<Outcome, jetmux_core::Error>) -> Option<ServerMsg> {
    match outcome {
        Ok(Outcome::Clipboard(text)) => Some(ServerMsg::Clipboard(text)),
        Ok(Outcome::Message(msg)) => Some(ServerMsg::Message(msg)),
        Ok(Outcome::WantPaste(pane)) => Some(ServerMsg::WantPaste { pane }),
        Ok(Outcome::None) => None,
        Err(e) => Some(ServerMsg::Message(e.to_string())),
    }
}
