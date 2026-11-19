//! The supervisor: owns the control socket and the session registry. Each
//! connection gets a thread for its handshake; attaching clients are then
//! handed to their session's loop, and RPC connections keep their thread.

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use jetmux_core::Spawn;
use jetmux_core::snapshot::{self, SessionSnapshot, Snapshot};
use jetmux_proto::rpc::{Event, Saved, SessionInfo};
use jetmux_proto::{ClientMsg, FrameKind, NewSession, PROTOCOL_VERSION, ServerMsg, Size};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use rustix::io::FdFlags;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook_mio::v1_0::Signals;

use crate::{config, rpc, session};

const LISTENER: Token = Token(0);
const SIGNALS: Token = Token(1);
const WAKER: Token = Token(2);

/// How long a connecting client gets to send its first frames.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const INFO_TIMEOUT: Duration = Duration::from_millis(200);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
const FREEZE_TIMEOUT: Duration = Duration::from_secs(5);

type BoxError = Box<dyn std::error::Error>;

struct Entry {
    handle: session::Handle,
    thread: JoinHandle<()>,
    /// Creation order, for picking the most recent session.
    serial: u64,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Entry>,
    serial: u64,
}

/// Fans events out to subscribers, dropping the ones that went away.
#[derive(Default)]
pub struct EventBus {
    subscribers: Mutex<Vec<Sender<Event>>>,
}

impl EventBus {
    pub fn publish(&self, event: &Event) {
        lock(&self.subscribers).retain(|s| s.send(event.clone()).is_ok());
    }

    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        lock(&self.subscribers).push(tx);
        rx
    }
}

/// Everything connection threads share with the supervisor.
pub struct Registry {
    server: String,
    state: Mutex<State>,
    pub events: Arc<EventBus>,
    /// Session threads report their name here when they end.
    exits: Sender<String>,
    stop: AtomicBool,
    waker: Arc<Waker>,
    started: SystemTime,
    /// The previous daemon's snapshot has been moved aside.
    rotated: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run the daemon. With `resume`, first take over the sessions a replaced
/// daemon handed over in that file.
pub fn run(server: &str, resume: Option<&Path>) -> Result<(), BoxError> {
    let dir = jetmux_client::socket_dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let path = jetmux_client::socket_path(server);
    let listener = bind(&path)?;

    let mut poll = Poll::new()?;
    poll.registry().register(
        &mut SourceFd(&listener.as_raw_fd()),
        LISTENER,
        Interest::READABLE,
    )?;
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    poll.registry()
        .register(&mut signals, SIGNALS, Interest::READABLE)?;
    let (exits, exited) = mpsc::channel();
    let registry = Arc::new(Registry {
        server: server.to_owned(),
        state: Mutex::default(),
        events: Arc::default(),
        exits,
        stop: AtomicBool::new(false),
        waker: Arc::new(Waker::new(poll.registry(), WAKER)?),
        started: SystemTime::now(),
        rotated: AtomicBool::new(false),
    });
    config::spawn_hooks(&registry.events)?;
    if let Some(resume) = resume
        && let Err(e) = registry.resume_from(resume)
    {
        eprintln!("jetmux: resume: {e}");
    }
    follow_config(&registry)?;

    eprintln!("jetmux: daemon listening on {}", path.display());
    let result = serve(&registry, &mut poll, &listener, &mut signals, &exited);
    // Stopped by a signal, as at shutdown: save first if snapshots are on.
    if !registry.stop.load(Ordering::Relaxed)
        && config::load().is_ok_and(|c| c.persist.interval.is_some())
        && let Err(e) = registry.save()
    {
        eprintln!("jetmux: snapshot: {e}");
    }
    registry.shutdown(&exited);
    let _ = std::fs::remove_file(&path);
    result
}

/// Follow the config: save snapshots every `persist.interval` seconds, and
/// give sessions its colors.
fn follow_config(registry: &Arc<Registry>) -> io::Result<()> {
    let registry = Arc::clone(registry);
    std::thread::Builder::new()
        .name("jetmux-config".into())
        .spawn(move || {
            let (mut watcher, config) = config::Watcher::new();
            let mut interval = config.ok().and_then(|c| c.persist.interval);
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(config::RELOAD_INTERVAL);
                if let Some(Ok(config)) = watcher.poll() {
                    interval = config.persist.interval;
                    registry.broadcast(|| {
                        session::Command::Base(Box::new(config.shared.vt_settings()))
                    });
                }
                let Some(secs) = interval else {
                    continue;
                };
                if last.elapsed() < Duration::from_secs(secs.max(1)) {
                    continue;
                }
                last = std::time::Instant::now();
                // No sessions is nothing to save, not an error.
                let _ = registry.save();
            }
        })?;
    Ok(())
}

/// Bind the socket, replacing a stale one but refusing to steal a live one.
fn bind(path: &Path) -> Result<UnixListener, BoxError> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(format!("a daemon is already listening on {}", path.display()).into());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn serve(
    registry: &Arc<Registry>,
    poll: &mut Poll,
    listener: &UnixListener,
    signals: &mut Signals,
    exited: &Receiver<String>,
) -> Result<(), BoxError> {
    let mut events = Events::with_capacity(16);
    loop {
        if let Err(e) = poll.poll(&mut events, None) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        for event in &events {
            match event.token() {
                LISTENER => loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let registry = Arc::clone(registry);
                            std::thread::Builder::new()
                                .name("jetmux-conn".into())
                                .spawn(move || {
                                    if let Err(e) = connection(&registry, stream) {
                                        eprintln!("jetmux: client: {e}");
                                    }
                                })?;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e.into()),
                    }
                },
                SIGNALS => {
                    for signal in signals.pending() {
                        // SIGHUP is ignored: the daemon outlives terminals.
                        if signal != SIGHUP {
                            return Ok(());
                        }
                    }
                }
                WAKER => {
                    while let Ok(name) = exited.try_recv() {
                        registry.reap(&name);
                    }
                    if registry.stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    }
}

/// Serve one connection: a binary client's handshake and first request, or
/// an RPC connection for as long as it stays open.
fn connection(registry: &Arc<Registry>, mut stream: UnixStream) -> Result<(), BoxError> {
    // Its own thread, so blocking. macOS hands out accepted sockets with
    // the listener's O_NONBLOCK (Linux doesn't), and every read failed.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let (kind, payload) = jetmux_proto::read_frame(&mut stream)?;
    if kind == FrameKind::Rpc {
        stream.set_read_timeout(None)?;
        return rpc::serve(registry, stream, &payload);
    }
    match jetmux_proto::decode_payload(kind as u8, &payload)? {
        ClientMsg::Hello { version } => {
            jetmux_proto::write_msg(
                &mut stream,
                &ServerMsg::Hello {
                    version: PROTOCOL_VERSION,
                },
            )?;
            if version != PROTOCOL_VERSION {
                return Ok(());
            }
        }
        other => {
            return reply(
                &mut stream,
                ServerMsg::Error(format!("expected Hello, got {other:?}")),
            );
        }
    }

    let request: ClientMsg = jetmux_proto::read_msg(&mut stream)?;
    stream.set_read_timeout(None)?;
    let (name, size, follow) = match request {
        ClientMsg::New(spec) => {
            let size = spec.size;
            match registry.create(spec) {
                Ok(name) => (name, size, false),
                Err(e) => return reply(&mut stream, ServerMsg::Error(e)),
            }
        }
        ClientMsg::Attach {
            target,
            size,
            create,
            follow,
        } => match registry.find(target.as_deref()) {
            Some(name) => (name, size, follow),
            None => match create {
                Some(spec) => {
                    let spec = NewSession {
                        name: target.or(spec.name),
                        ..spec
                    };
                    match registry.create(spec) {
                        Ok(name) => (name, size, follow),
                        Err(e) => return reply(&mut stream, ServerMsg::Error(e)),
                    }
                }
                None => {
                    let msg = match target {
                        Some(name) => no_session(&name),
                        None => "no sessions".into(),
                    };
                    return reply(&mut stream, ServerMsg::Error(msg));
                }
            },
        },
        other => {
            return reply(
                &mut stream,
                ServerMsg::Error(format!("{other:?} needs an attached session")),
            );
        }
    };
    match registry.send(
        &name,
        session::Command::Attach(stream.try_clone()?, size, follow),
    ) {
        Ok(()) => Ok(()),
        Err(e) => reply(&mut stream, ServerMsg::Error(e)),
    }
}

impl Registry {
    /// `name` if it exists, or the most recent session when unset.
    pub fn find(&self, name: Option<&str>) -> Option<String> {
        let state = lock(&self.state);
        match name {
            Some(name) => state.sessions.contains_key(name).then(|| name.to_owned()),
            None => state
                .sessions
                .iter()
                .max_by_key(|(_, e)| e.serial)
                .map(|(name, _)| name.clone()),
        }
    }

    pub fn send(&self, name: &str, cmd: session::Command) -> Result<(), String> {
        let state = lock(&self.state);
        let entry = state.sessions.get(name).ok_or_else(|| no_session(name))?;
        entry.handle.send(cmd).map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        // Ask every session first, then collect, so one slow session doesn't
        // hold up the rest or the registry lock.
        let pending: Vec<_> = {
            let state = lock(&self.state);
            let mut entries: Vec<&Entry> = state.sessions.values().collect();
            entries.sort_by_key(|e| e.serial);
            entries
                .into_iter()
                .filter_map(|entry| {
                    let (tx, rx) = mpsc::channel();
                    entry.handle.send(session::Command::Info(tx)).ok()?;
                    Some(rx)
                })
                .collect()
        };
        pending
            .into_iter()
            .filter_map(|rx| rx.recv_timeout(INFO_TIMEOUT).ok())
            .collect()
    }

    pub fn kill(&self, name: &str) -> Result<(), String> {
        self.send(name, session::Command::Kill)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.waker.wake();
    }

    pub fn create(&self, spec: NewSession) -> Result<String, String> {
        let mut state = lock(&self.state);
        let name = match spec.name {
            Some(name) if state.sessions.contains_key(&name) => {
                return Err(format!("session {name} already exists"));
            }
            Some(name) if name.is_empty() || name.contains([':', '.', '%']) => {
                return Err(format!("invalid session name {name:?}"));
            }
            Some(name) => name,
            None => (0..)
                .map(|n: u32| n.to_string())
                .find(|n| !state.sessions.contains_key(n))
                .unwrap_or_default(),
        };
        let spawn = Spawn {
            command: spec.command,
            cwd: spec.cwd,
            env: spec.env,
        };
        let start = session::Start::New {
            tabs: spec.tabs,
            profile: spec.profile,
        };
        self.start(&mut state, name, spawn, spec.size, start)
    }

    /// Run a loop for session `name` and register it once its panes are up.
    fn start(
        &self,
        state: &mut State,
        name: String,
        mut spawn: Spawn,
        size: Size,
        start: session::Start,
    ) -> Result<String, String> {
        let mut env = vec![
            (
                "JETMUX".into(),
                jetmux_client::socket_path(&self.server)
                    .display()
                    .to_string(),
            ),
            ("JETMUX_SESSION".into(), name.clone()),
        ];
        env.append(&mut spawn.env);
        spawn.env = env;
        let (commands_tx, commands) = mpsc::channel();
        let (ready_tx, ready) = mpsc::channel();
        let args = session::Args {
            name: name.clone(),
            spawn,
            start,
            size,
            commands,
            ready: ready_tx,
            events: Arc::clone(&self.events),
        };
        let exits = self.exits.clone();
        let waker = Arc::clone(&self.waker);
        let exit_name = name.clone();
        let thread = std::thread::Builder::new()
            .name(format!("jetmux-session-{name}"))
            .spawn(move || {
                session::run(args);
                let _ = exits.send(exit_name);
                let _ = waker.wake();
            })
            .map_err(|e| e.to_string())?;

        let session_waker = match ready.recv() {
            Ok(Ok(waker)) => waker,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(format!("starting session {name}: {e}"));
            }
            Err(_) => {
                let _ = thread.join();
                return Err(format!("session {name} died while starting"));
            }
        };
        state.serial += 1;
        let serial = state.serial;
        state.sessions.insert(
            name.clone(),
            Entry {
                handle: session::Handle {
                    commands: commands_tx,
                    waker: session_waker,
                },
                thread,
                serial,
            },
        );
        self.events.publish(&Event::SessionCreated {
            session: name.clone(),
        });
        Ok(name)
    }

    /// Save every session for a later `restore`.
    /// Send every session a command.
    pub fn broadcast(&self, command: impl Fn() -> session::Command) {
        for entry in lock(&self.state).sessions.values() {
            // A session that's gone has nothing to hear.
            let _ = entry.handle.send(command());
        }
    }

    pub fn save(&self) -> Result<Saved, String> {
        let pending: Vec<_> = {
            let state = lock(&self.state);
            let mut entries: Vec<&Entry> = state.sessions.values().collect();
            entries.sort_by_key(|e| e.serial);
            entries
                .into_iter()
                .filter_map(|entry| {
                    let (tx, rx) = mpsc::channel();
                    entry.handle.send(session::Command::Snapshot(tx)).ok()?;
                    Some(rx)
                })
                .collect()
        };
        let sessions: Vec<SessionSnapshot> = pending
            .into_iter()
            .filter_map(|rx| match rx.recv_timeout(SNAPSHOT_TIMEOUT) {
                Ok(Ok(snapshot)) => Some(snapshot),
                Ok(Err(e)) => {
                    eprintln!("jetmux: snapshot: {e}");
                    None
                }
                Err(_) => None,
            })
            .collect();
        if sessions.is_empty() {
            return Err("no sessions to save".into());
        }
        let path = state_path(&self.server);
        // The first save of this daemon keeps the last one's snapshot, which
        // is what a `restore` after a reboot wants.
        if !self.rotated.swap(true, Ordering::Relaxed) && written_before(&path, self.started) {
            let _ = std::fs::rename(&path, path.with_extension("previous.json"));
        }
        let names = sessions.iter().map(|s| s.name.clone()).collect();
        write_private(
            &path,
            &Snapshot {
                version: snapshot::VERSION,
                sessions,
            },
        )?;
        Ok(Saved {
            path: path.display().to_string(),
            sessions: names,
        })
    }

    /// Bring back the sessions a previous daemon saved that aren't running.
    pub fn restore(&self, rerun: Vec<String>) -> Result<Vec<String>, String> {
        let current = state_path(&self.server);
        let previous = current.with_extension("previous.json");
        let path = if !written_before(&current, self.started) && previous.exists() {
            previous
        } else {
            current
        };
        let snapshot = read_snapshot(&path)?;
        let mut state = lock(&self.state);
        let mut restored = Vec::new();
        for session in snapshot.sessions {
            if state.sessions.contains_key(&session.name) {
                continue;
            }
            let (name, size) = (session.name.clone(), session.size);
            let start = session::Start::Restore {
                snapshot: session,
                rerun: rerun.clone(),
            };
            match self.start(&mut state, name, Spawn::default(), size, start) {
                Ok(name) => restored.push(name),
                Err(e) => eprintln!("jetmux: restore: {e}"),
            }
        }
        Ok(restored)
    }

    /// Replace this process with `binary`, handing it every session. Only
    /// returns if the exec failed, after taking the sessions back.
    pub fn upgrade(&self, binary: Option<String>) -> Result<(), String> {
        let binary = match binary {
            Some(binary) => PathBuf::from(binary),
            None => std::env::current_exe().map_err(|e| format!("finding jetmux: {e}"))?,
        };
        if !binary.is_file() {
            return Err(format!("{} is not a file", binary.display()));
        }
        let entries: Vec<(String, Entry)> = lock(&self.state).sessions.drain().collect();
        let mut frozen = Vec::new();
        for (name, entry) in entries {
            let (tx, rx) = mpsc::channel();
            if entry.handle.send(session::Command::Freeze(tx)).is_err() {
                continue;
            }
            match rx.recv_timeout(FREEZE_TIMEOUT) {
                Ok(Ok(session)) => {
                    let _ = entry.thread.join();
                    frozen.push(session);
                }
                Ok(Err(e)) => eprintln!("jetmux: upgrade: session {name}: {e}"),
                Err(_) => eprintln!("jetmux: upgrade: session {name} didn't freeze"),
            }
        }
        let state = Snapshot {
            version: snapshot::VERSION,
            sessions: frozen.iter().map(|f| f.snapshot.clone()).collect(),
        };
        let path = jetmux_client::socket_dir().join(format!("{}.upgrade.json", self.server));
        let handover = write_private(&path, &state).and_then(|()| {
            frozen
                .iter()
                .flat_map(|f| &f.masters)
                .try_for_each(|fd| rustix::io::fcntl_setfd(fd, FdFlags::empty()))
                .map_err(|e| e.to_string())
        });
        let error = match handover {
            Ok(()) => {
                eprintln!("jetmux: upgrading to {}", binary.display());
                let error = std::process::Command::new(&binary)
                    .arg("-L")
                    .arg(&self.server)
                    .arg("daemon")
                    .arg("--resume")
                    .arg(&path)
                    .exec();
                format!("exec {}: {error}", binary.display())
            }
            Err(e) => e,
        };
        // Still here: carry on with the sessions as they were.
        let _ = std::fs::remove_file(&path);
        let mut state = lock(&self.state);
        for session in frozen {
            let masters = session
                .masters
                .into_iter()
                .map(|fd| (fd.as_raw_fd(), fd))
                .collect();
            let (name, size) = (session.snapshot.name.clone(), session.snapshot.size);
            let start = session::Start::Resume {
                snapshot: session.snapshot,
                masters,
            };
            if let Err(e) = self.start(&mut state, name, Spawn::default(), size, start) {
                eprintln!("jetmux: upgrade: {e}");
            }
        }
        Err(error)
    }

    /// Take over the sessions the daemon this one replaced handed over.
    pub fn resume_from(&self, path: &Path) -> Result<(), String> {
        let snapshot = read_snapshot(path);
        let _ = std::fs::remove_file(path);
        let mut state = lock(&self.state);
        for session in snapshot?.sessions {
            let masters = session
                .tabs
                .iter()
                .flat_map(|t| &t.panes)
                .filter_map(|p| p.live.as_ref())
                .map(|live| {
                    // SAFETY: the previous daemon left these fds open across
                    // the exec for exactly this, and nothing else in this
                    // process owns them: fds opened since got new numbers.
                    (live.fd, unsafe { OwnedFd::from_raw_fd(live.fd) })
                })
                .collect();
            let (name, size) = (session.name.clone(), session.size);
            let start = session::Start::Resume {
                snapshot: session,
                masters,
            };
            if let Err(e) = self.start(&mut state, name, Spawn::default(), size, start) {
                eprintln!("jetmux: resume: {e}");
            }
        }
        Ok(())
    }

    fn reap(&self, name: &str) {
        let entry = lock(&self.state).sessions.remove(name);
        if let Some(entry) = entry {
            let _ = entry.thread.join();
            self.events.publish(&Event::SessionExited {
                session: name.to_owned(),
            });
        }
    }

    fn shutdown(&self, exited: &Receiver<String>) {
        let sessions: Vec<Entry> = lock(&self.state).sessions.drain().map(|(_, e)| e).collect();
        for entry in &sessions {
            let _ = entry.handle.send(session::Command::Kill);
        }
        for entry in sessions {
            let _ = entry.thread.join();
        }
        while exited.try_recv().is_ok() {}
    }
}

fn reply(stream: &mut UnixStream, msg: ServerMsg) -> Result<(), BoxError> {
    Ok(jetmux_proto::write_msg(stream, &msg)?)
}

/// Where snapshots for `jetmux restore` live.
fn state_path(server: &str) -> PathBuf {
    jetmux_client::state_dir().join(format!("{server}.json"))
}

/// The file exists and was last written before `time`.
fn written_before(path: &Path, time: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|m| m < time)
}

fn read_snapshot(path: &Path) -> Result<Snapshot, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let snapshot: Snapshot =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if snapshot.version > snapshot::VERSION {
        return Err(format!(
            "{} is from a newer jetmux (format {})",
            path.display(),
            snapshot.version
        ));
    }
    Ok(snapshot)
}

/// Write `value` as JSON readable only by this user (scrollback can hold
/// secrets), replacing `path` atomically.
fn write_private(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let fail = |e: io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(fail)?;
    }
    let json = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(fail)?;
    file.write_all(&json).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)
}

pub fn no_session(name: &str) -> String {
    format!("no session named {name}")
}
