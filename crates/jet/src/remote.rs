//! jet as a jetmux client: a session's tabs and panes, drawn as jet's own,
//! through `jetmux proxy` on this machine or over ssh.
//!
//! Threads per session: a writer feeding the proxy's stdin; a reader
//! decoding its stdout into the panes' mirrors and the app's events; and
//! one keeping the tail of its stderr, to say why it ended.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use jet_core::{FrameColors, Mirror, Picture, PointerShape, Snapshot, VtSettings};
use jetmux_proto::{
    Action, ClientMsg, Decoder, NewSession, PROTOCOL_VERSION, ServerMsg, Size, View,
};

use crate::pane::{Msg, PaneEvent};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Most of the proxy's stderr kept, for saying why it ended.
const STDERR_TAIL: usize = 2048;

/// What the app hears about the session as a whole.
pub enum SessionEvent {
    View(View),
    /// Copied by the user; `program` when a program's OSC 52 instead.
    Clipboard {
        text: String,
        program: bool,
    },
    Bell,
    Message(String),
    /// Gone: `None` as it should (its shells exited, or it was detached),
    /// else why.
    Ended(Option<String>),
}

/// A connection to one session.
pub struct Remote {
    out: Sender<ClientMsg>,
    events: Receiver<SessionEvent>,
    shared: Arc<Shared>,
    child: Option<Child>,
}

#[derive(Default)]
struct Shared {
    panes: Mutex<HashMap<u32, Arc<Mirrored>>>,
    pictures: Mutex<HashMap<u64, Arc<Picture>>>,
    /// The theme last sent and the colors it draws in.
    theme: Mutex<Option<(jetmux_proto::input::Theme, FrameColors)>>,
    /// The cell size last sent, in pixels.
    cell: Mutex<(u32, u32)>,
}

/// One pane as the reader leaves it for the app.
#[derive(Default)]
struct Mirrored {
    mirror: Mutex<Mirror>,
    events: Mutex<VecDeque<PaneEvent>>,
    title: Mutex<String>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Remote {
    /// Start `proxy` (a command running `jetmux proxy`) and attach to
    /// `session`, made if missing. `wake` runs on the reader thread after
    /// anything arrives.
    pub fn connect(
        proxy: &[String],
        session: &str,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self, BoxError> {
        let (program, args) = proxy.split_first().ok_or("no proxy command")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window for ssh.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("{}: {e}", proxy.join(" ")))?;
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err("the proxy has no pipes".into());
        };

        let (out, outgoing) = mpsc::channel::<ClientMsg>();
        std::thread::Builder::new()
            .name("jetmux writer".into())
            .spawn(move || {
                let mut buf = Vec::new();
                for msg in outgoing {
                    buf.clear();
                    if jetmux_proto::encode(&msg, &mut buf).is_err()
                        || stdin.write_all(&buf).is_err()
                    {
                        return;
                    }
                }
            })?;

        let tail = Arc::new(Mutex::new(String::new()));
        let errors = tail.clone();
        let stderr_thread = std::thread::Builder::new()
            .name("jetmux stderr".into())
            .spawn(move || {
                let mut buf = [0; 1024];
                while let Ok(n @ 1..) = stderr.read(&mut buf) {
                    let mut tail = lock(&errors);
                    tail.push_str(&String::from_utf8_lossy(&buf[..n]));
                    let cut = tail.len().saturating_sub(STDERR_TAIL);
                    let cut = (cut..tail.len())
                        .find(|&i| tail.is_char_boundary(i))
                        .unwrap_or(0);
                    tail.drain(..cut);
                }
            })?;

        let shared = Arc::new(Shared::default());
        let (event_tx, events) = mpsc::channel();
        let reader = Reader {
            shared: shared.clone(),
            events: event_tx,
        };
        std::thread::Builder::new()
            .name("jetmux reader".into())
            .spawn(move || {
                let why = reader.run(stdout, &wake);
                // Whatever ssh or jetmux said says more than a closed pipe.
                let _ = stderr_thread.join();
                let said = lock(&tail).trim().to_owned();
                let why = match (why, said.is_empty()) {
                    (Some(End::Done), _) => None,
                    (Some(End::Problem(why)), _) => Some(why),
                    (None, false) => Some(said),
                    (None, true) => Some("the connection closed".into()),
                };
                let _ = reader.events.send(SessionEvent::Ended(why));
                wake();
            })?;

        let remote = Self {
            out,
            events,
            shared,
            child: Some(child),
        };
        remote.send(ClientMsg::Hello {
            version: PROTOCOL_VERSION,
        });
        remote.send(ClientMsg::Attach {
            target: Some(session.to_owned()),
            size: Size { cols: 80, rows: 24 },
            create: Some(NewSession {
                name: Some(session.to_owned()),
                size: Size { cols: 80, rows: 24 },
                ..NewSession::default()
            }),
            follow: false,
        });
        remote.send(ClientMsg::WantPaneEvents);
        Ok(remote)
    }

    pub fn send(&self, msg: ClientMsg) {
        // A send error means the writer is gone; the reader says why.
        let _ = self.out.send(msg);
    }

    pub fn act(&self, action: Action) {
        self.send(ClientMsg::Action(action));
    }

    pub fn try_event(&self) -> Option<SessionEvent> {
        self.events.try_recv().ok()
    }

    /// A handle on pane `id` for a [`crate::pane::Pane`].
    pub fn link(&self, id: u32) -> Link {
        let pane = lock(&self.shared.panes).entry(id).or_default().clone();
        Link {
            id,
            out: self.out.clone(),
            pane,
            shared: self.shared.clone(),
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.send(ClientMsg::Detach);
        // The proxy ends once its stdin closes, with the last link.
        if let Some(mut child) = self.child.take() {
            let _ = std::thread::Builder::new()
                .name("jetmux reaper".into())
                .spawn(move || child.wait());
        }
    }
}

/// How the daemon ended a session for this client.
enum End {
    Done,
    Problem(String),
}

struct Reader {
    shared: Arc<Shared>,
    events: Sender<SessionEvent>,
}

impl Reader {
    /// Until the stream ends: `Some` when the daemon said why.
    fn run(&self, mut stdout: impl Read, wake: &impl Fn()) -> Option<End> {
        let mut decoder = Decoder::default();
        let mut buf = vec![0; 64 * 1024];
        loop {
            loop {
                match decoder.next_msg::<ServerMsg>() {
                    Ok(Some(msg)) => {
                        if let Some(why) = self.handle(msg) {
                            return Some(why);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        return Some(End::Problem(format!(
                            "jetmux sent something unreadable: {e}"
                        )));
                    }
                }
            }
            wake();
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => return None,
                Ok(n) => decoder.push(&buf[..n]),
            }
        }
    }

    fn pane(&self, id: u32) -> Arc<Mirrored> {
        lock(&self.shared.panes).entry(id).or_default().clone()
    }

    fn event(&self, id: u32, event: PaneEvent) {
        lock(&self.pane(id).events).push_back(event);
    }

    /// `Some` when the session is over for this client.
    fn handle(&self, msg: ServerMsg) -> Option<End> {
        let session = |e| {
            let _ = self.events.send(e);
        };
        match msg {
            ServerMsg::Hello { version } if version != PROTOCOL_VERSION => {
                return Some(End::Problem(format!(
                    "jetmux there speaks protocol {version} and jet {PROTOCOL_VERSION}: \
                     update the older one"
                )));
            }
            ServerMsg::Hello { .. } | ServerMsg::Attached { .. } => {}
            ServerMsg::View(view) => {
                let live: Vec<u32> = view.tabs.iter().flat_map(|t| t.layout.panes()).collect();
                lock(&self.shared.panes).retain(|id, _| live.contains(id));
                for shown in &view.panes {
                    let pane = self.pane(shown.id);
                    let mut title = lock(&pane.title);
                    if *title != shown.title {
                        title.clone_from(&shown.title);
                        lock(&pane.events).push_back(PaneEvent::Title(shown.title.clone()));
                    }
                }
                session(SessionEvent::View(view));
            }
            ServerMsg::PaneFrame { pane, frame } => lock(&self.pane(pane).mirror).apply(frame),
            ServerMsg::Pane { pane, event } => {
                use jetmux_proto::pane::PaneEvent as Wire;
                let event = match event {
                    Wire::Location(location) => PaneEvent::Location(location),
                    Wire::Progress(p) => PaneEvent::Progress(p),
                    Wire::Notify(n) => PaneEvent::Notify(n),
                    Wire::Var { name, value } => PaneEvent::UserVar(name, value),
                    Wire::Context(c) => PaneEvent::Context(c),
                    Wire::Pointer(name) => PaneEvent::Pointer(
                        PointerShape::parse(&name).unwrap_or(PointerShape::RESET),
                    ),
                    Wire::Finished(status) => PaneEvent::CommandFinished(status),
                };
                self.event(pane, event);
            }
            ServerMsg::WantPaste { pane } => self.event(pane, PaneEvent::Paste),
            ServerMsg::Picture {
                key,
                width,
                height,
                rgba,
            } => {
                let picture = Picture {
                    key,
                    width,
                    height,
                    rgba,
                };
                lock(&self.shared.pictures).insert(key, Arc::new(picture));
            }
            ServerMsg::Clipboard(text) => session(SessionEvent::Clipboard {
                text,
                program: false,
            }),
            ServerMsg::AppClipboard(text) => session(SessionEvent::Clipboard {
                text,
                program: true,
            }),
            ServerMsg::Bell => session(SessionEvent::Bell),
            ServerMsg::Message(text) | ServerMsg::Error(text) => {
                session(SessionEvent::Message(text));
            }
            ServerMsg::Exited | ServerMsg::Detached => return Some(End::Done),
            ServerMsg::Reconnect => {
                return Some(End::Problem(
                    "jetmux restarted: open the session again".into(),
                ));
            }
        }
        None
    }
}

/// One remote pane, for a [`crate::pane::Pane`].
pub struct Link {
    id: u32,
    out: Sender<ClientMsg>,
    pane: Arc<Mirrored>,
    shared: Arc<Shared>,
}

impl Link {
    pub fn send(&self, msg: Msg) {
        let pane = self.id;
        let wire = match msg {
            // For the focused pane: only `jet-cli send-text` sends bytes.
            Msg::Input(bytes) => ClientMsg::Input(bytes),
            Msg::Key(key) => ClientMsg::Key {
                pane,
                event: key.to_wire(),
            },
            Msg::Paste(text) => ClientMsg::Paste { pane, text },
            Msg::Wheel {
                notches,
                cell: (col, row),
                mods,
            } => ClientMsg::Wheel {
                pane,
                notches,
                col,
                row,
                mods: mods.bits(),
            },
            Msg::Mouse(input) => ClientMsg::Mouse(input.to_wire(pane, *lock(&self.shared.cell))),
            Msg::Focus(focused) => ClientMsg::Focus { pane, focused },
            Msg::Configure(settings) => match self.theme(&settings) {
                Some(theme) => ClientMsg::Theme(theme),
                None => return,
            },
            Msg::CopyMode => ClientMsg::Action(Action::EnterCopyMode),
            Msg::Search(query, regex) => ClientMsg::Action(Action::Search { query, regex }),
            Msg::SearchStep(n) => {
                ClientMsg::Action(Action::SearchStep(i32::try_from(n).unwrap_or(0)))
            }
            // The daemon sizes its panes; jet's cells place its images.
            Msg::Resize(size) => {
                let cell = (u32::from(size.cell_width), u32::from(size.cell_height));
                if std::mem::replace(&mut *lock(&self.shared.cell), cell) == cell {
                    return;
                }
                ClientMsg::CellSize {
                    width: cell.0,
                    height: cell.1,
                }
            }
            // Not over the wire (yet): command blocks and quick select live
            // with the daemon's own keys; the rest is the pane thread's.
            Msg::JumpToPrompt(_)
            | Msg::CopyCommandOutput
            | Msg::QuickSelect
            | Msg::Copy
            | Msg::AutoscrollTick
            | Msg::Output(_)
            | Msg::Exited(_)
            | Msg::SnapshotTaken
            | Msg::Shutdown => return,
        };
        let _ = self.out.send(wire);
    }

    /// The theme to send for `settings`, unless it already went.
    fn theme(&self, settings: &VtSettings) -> Option<jetmux_proto::input::Theme> {
        let theme = settings.theme();
        let mut last = lock(&self.shared.theme);
        if last.as_ref().is_some_and(|(sent, _)| *sent == theme) {
            return None;
        }
        let colors = FrameColors::new(settings).ok()?;
        *last = Some((theme.clone(), colors));
        Some(theme)
    }

    pub fn take_snapshot(&self) -> Option<Snapshot> {
        let theme = lock(&self.shared.theme);
        let colors = match &*theme {
            Some((_, colors)) => colors.clone(),
            None => FrameColors::new(&VtSettings::default()).ok()?,
        };
        drop(theme);
        let cell = *lock(&self.shared.cell);
        let pictures = lock(&self.shared.pictures);
        lock(&self.pane.mirror).snapshot(&colors, &pictures, cell)
    }

    pub fn try_event(&self) -> Option<PaneEvent> {
        lock(&self.pane.events).pop_front()
    }
}
