//! Roaming remote attach over UDP (see `jetmux-roam`).
//!
//! `jetmux remote host` runs `jetmux remote-serve` on the host over ssh. That
//! binds a UDP port, prints it with a fresh key, and leaves a detached
//! process attached to the host's daemon as an ordinary client. From then
//! on only encrypted UDP passes: the server keeps the screen as numbered
//! states and sends each as a diff from what the client last acknowledged;
//! the client resends input until it's acknowledged. Either side's address
//! can change underneath.
//!
//! The client draws with the normal attach UI, talking to a proxy over a
//! socketpair. The proxy holds the screen and the predictions, and turns
//! state diffs back into the views and frames the UI expects.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use jetmux_proto::{ClientMsg, Decoder, NewSession, ServerMsg, Size};
use jetmux_roam::crypto::{Key, Opener, Sealer, Side};
use jetmux_roam::predict::Predictor;
use jetmux_roam::state::Screen;
use jetmux_roam::wire::{self, Body, Echo, FromClient, FromServer, Packet, Reassembler, StateDiff};
use mio::net::{UdpSocket, UnixStream};
use mio::{Events, Interest, Poll, Token};

use crate::attach;

type BoxError = Box<dyn std::error::Error>;

const UDP: Token = Token(0);
const LOCAL: Token = Token(1);

/// The line `remote-serve` prints for the client: `JETMUX-REMOTE ip port key`.
const BANNER: &str = "JETMUX-REMOTE";
const DEFAULT_PORTS: (u16, u16) = (60001, 61000);
/// Pacing: states are batched to at most one per tick.
const TICK: Duration = Duration::from_millis(20);
const HEARTBEAT: Duration = Duration::from_secs(1);
/// Input counts as echoed once it's been with the daemon this long.
const ECHO_DELAY: Duration = Duration::from_millis(50);
/// With no word from the server for this long, say so.
const QUIET: Duration = Duration::from_secs(3);
/// With no word for this long, try a new local port (a NAT may have
/// forgotten us).
const REBIND: Duration = Duration::from_secs(10);
/// A server nobody has talked to for this long gives up.
const ABANDONED: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Over links slower than this, typing is predicted.
const PREDICT_RTT_MS: f64 = 30.0;

/// One end of the UDP link: sealing, fragments, round-trip times.
struct Link {
    socket: UdpSocket,
    sealer: Sealer,
    opener: Opener,
    reassembler: Reassembler,
    next_message: u32,
    peer: Option<SocketAddr>,
    clock: Instant,
    /// The other side's latest timestamp and when it arrived.
    heard: Option<(u32, Instant)>,
    last_heard: Instant,
    /// Smoothed round trip, in milliseconds.
    srtt: Option<f64>,
}

impl Link {
    fn new(
        socket: UdpSocket,
        key: &Key,
        side: Side,
        peer: Option<SocketAddr>,
    ) -> Result<Self, BoxError> {
        let other = match side {
            Side::Client => Side::Server,
            Side::Server => Side::Client,
        };
        Ok(Self {
            socket,
            sealer: Sealer::new(key, side).ok_or("bad key")?,
            opener: Opener::new(key, other).ok_or("bad key")?,
            reassembler: Reassembler::default(),
            next_message: 0,
            peer,
            clock: Instant::now(),
            heard: None,
            last_heard: Instant::now(),
            srtt: None,
        })
    }

    fn now_ms(&self) -> u32 {
        // Wraps after 49 days; only differences are used.
        self.clock.elapsed().as_millis() as u32
    }

    fn send(&mut self, body: Body) -> io::Result<()> {
        let Some(peer) = self.peer else {
            return Ok(());
        };
        let packet = Packet {
            time: self.now_ms(),
            echo: self.heard.map(|(time, at)| Echo {
                time,
                held_ms: at.elapsed().as_millis() as u32,
            }),
            body,
        };
        let message = wire::encode(&packet).ok_or_else(|| io::Error::other("encoding"))?;
        self.next_message = self.next_message.wrapping_add(1);
        for fragment in wire::fragments(self.next_message, &message).unwrap_or_default() {
            let Some(datagram) = self.sealer.seal(&fragment) else {
                continue;
            };
            match self.socket.send_to(&datagram, peer) {
                Ok(_) => {}
                // A full buffer or a network that just went away: the
                // next send carries the same state.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Authentic packets waiting on the socket, and where each came from.
    fn recv(&mut self) -> Vec<(SocketAddr, Packet)> {
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = self.socket.recv_from(&mut buf) {
            let Some(payload) = self.opener.open(&buf[..n]) else {
                continue;
            };
            let Some(message) = self.reassembler.push(&payload) else {
                continue;
            };
            let Some(packet) = wire::decode(&message) else {
                continue;
            };
            self.last_heard = Instant::now();
            self.heard = Some((packet.time, Instant::now()));
            if let Some(echo) = packet.echo {
                let rtt = self
                    .now_ms()
                    .wrapping_sub(echo.time)
                    .saturating_sub(echo.held_ms);
                let rtt = f64::from(rtt);
                self.srtt = Some(self.srtt.map_or(rtt, |s| 0.875 * s + 0.125 * rtt));
            }
            out.push((from, packet));
        }
        out
    }

    /// How long to wait before resending what wasn't acknowledged.
    fn rto(&self) -> Duration {
        let ms = self.srtt.map_or(200.0, |s| s * 2.0).clamp(50.0, 1000.0);
        Duration::from_millis(ms as u64)
    }
}

fn bind_in(range: (u16, u16)) -> io::Result<UdpSocket> {
    for port in range.0..=range.1 {
        if let Ok(socket) = std::net::UdpSocket::bind(("0.0.0.0", port)) {
            socket.set_nonblocking(true)?;
            return Ok(UdpSocket::from_std(socket));
        }
    }
    Err(io::Error::other(format!(
        "no free UDP port in {}-{}",
        range.0, range.1
    )))
}

fn parse_size(text: &str) -> Option<Size> {
    let (cols, rows) = text.split_once('x')?;
    Some(Size {
        cols: cols.parse().ok()?,
        rows: rows.parse().ok()?,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub struct ServeArgs {
    pub server: String,
    pub target: Option<String>,
    pub size: Option<String>,
    pub ports: Option<String>,
    /// Set in the detached child; the parent only starts it.
    pub run: bool,
}

/// `jetmux remote-serve`: start a detached server and print how to reach it.
pub fn serve(args: ServeArgs) -> Result<(), BoxError> {
    if args.run {
        return run_server(&args);
    }
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command.args(["-L", &args.server, "remote-serve", "--run"]);
    for (flag, value) in [
        ("-t", &args.target),
        ("--size", &args.size),
        ("--ports", &args.ports),
    ] {
        if let Some(value) = value {
            command.args([flag, value]);
        }
    }
    let dir = jetmux_client::socket_dir();
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("remote.log"))?;
    // Its own process group, so the ssh session ending doesn't take it.
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()?;
    let stdout = child.stdout.take().ok_or("no pipe")?;
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line)?;
    let (port, key) = line
        .trim()
        .split_once(' ')
        .ok_or("the server didn't start; see remote.log")?;
    // sshd tells us which of this host's addresses the client reached.
    let ip = std::env::var("SSH_CONNECTION")
        .ok()
        .and_then(|c| c.split_whitespace().nth(2).map(str::to_owned))
        .unwrap_or_else(|| "-".into());
    println!("{BANNER} {ip} {port} {key}");
    Ok(())
}

fn run_server(args: &ServeArgs) -> Result<(), BoxError> {
    let ports = match &args.ports {
        Some(range) => {
            let (a, b) = range.split_once('-').ok_or("--ports wants low-high")?;
            (a.parse()?, b.parse()?)
        }
        None => DEFAULT_PORTS,
    };
    let socket = bind_in(ports)?;
    let key = Key::random()?;
    {
        // The parent reads exactly this line; nothing else goes to stdout.
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{} {}", socket.local_addr()?.port(), key.encode())?;
        stdout.flush()?;
    }
    let size = args
        .size
        .as_deref()
        .and_then(parse_size)
        .unwrap_or(Size { cols: 80, rows: 24 });
    let (daemon, session) = attach_daemon(&args.server, args.target.clone(), size)?;
    let mut server = Server {
        link: Link::new(socket, &key, Side::Server, None)?,
        daemon,
        daemon_out: Vec::new(),
        decoder: Decoder::default(),
        server: args.server.clone(),
        session,
        screen: Screen::default(),
        state: 0,
        history: BTreeMap::new(),
        state_ack: 0,
        events: Vec::new(),
        next_event: 0,
        input_ack: 0,
        received: Vec::new(),
        echoed: 0,
        last_send: Instant::now(),
        sent: (0, 0, 0),
        ending: None,
    };
    server.history.insert(0, Screen::default());
    server.run()
}

/// Attach to the local daemon (starting it, and the session, if needed).
fn attach_daemon(
    server: &str,
    target: Option<String>,
    size: Size,
) -> Result<(UnixStream, String), BoxError> {
    let mut client = crate::connect_or_start(server)?;
    let create = Some(NewSession {
        name: target.clone(),
        size,
        ..NewSession::default()
    });
    let reply = client.request(&ClientMsg::Attach {
        target,
        size,
        create,
        follow: false,
    })?;
    let ServerMsg::Attached { session } = reply else {
        return Err(jetmux_client::unexpected(reply).into());
    };
    let stream = client.into_stream();
    stream.set_nonblocking(true)?;
    Ok((UnixStream::from_std(stream), session))
}

struct Server {
    link: Link,
    daemon: UnixStream,
    daemon_out: Vec<u8>,
    decoder: Decoder,
    server: String,
    session: String,
    screen: Screen,
    /// The newest numbered state, and the states the client may diff from.
    state: u64,
    history: BTreeMap<u64, Screen>,
    state_ack: u64,
    events: Vec<(u64, ServerMsg)>,
    next_event: u64,
    input_ack: u64,
    /// When each recent input reached the daemon, for the echo ack.
    received: Vec<(u64, Instant)>,
    echoed: u64,
    last_send: Instant,
    /// (state, input ack, echo ack) in the last packet sent.
    sent: (u64, u64, u64),
    /// The session ended or the client detached: leave once it knows.
    ending: Option<Instant>,
}

impl Server {
    fn run(&mut self) -> Result<(), BoxError> {
        let mut poll = Poll::new()?;
        poll.registry()
            .register(&mut self.link.socket, UDP, Interest::READABLE)?;
        poll.registry().register(
            &mut self.daemon,
            LOCAL,
            Interest::READABLE | Interest::WRITABLE,
        )?;
        let mut events = Events::with_capacity(16);
        loop {
            if let Err(e) = poll.poll(&mut events, Some(TICK))
                && e.kind() != io::ErrorKind::Interrupted
            {
                return Err(e.into());
            }
            if !self.read_daemon()? {
                // The daemon went away (an upgrade, say): attach again.
                poll.registry().deregister(&mut self.daemon)?;
                let (daemon, _) =
                    attach_daemon(&self.server, Some(self.session.clone()), self.size())?;
                self.daemon = daemon;
                self.decoder = Decoder::default();
                poll.registry().register(
                    &mut self.daemon,
                    LOCAL,
                    Interest::READABLE | Interest::WRITABLE,
                )?;
            }
            for (from, packet) in self.link.recv() {
                if let Body::Client(msg) = packet.body {
                    // Roaming: answer wherever the client is now.
                    self.link.peer = Some(from);
                    self.handle_client(msg);
                }
            }
            self.flush_daemon()?;
            self.maybe_send()?;
            let abandoned = self.link.last_heard.elapsed() > ABANDONED;
            let done = self
                .ending
                .is_some_and(|t| self.events.is_empty() || t.elapsed() > Duration::from_secs(10));
            if abandoned || done {
                return Ok(());
            }
        }
    }

    fn size(&self) -> Size {
        self.screen
            .view
            .as_ref()
            .map_or(Size { cols: 80, rows: 24 }, |v| Size {
                cols: v.area.cols,
                rows: v.area.rows + 1,
            })
    }

    /// False once the daemon connection is gone.
    fn read_daemon(&mut self) -> Result<bool, BoxError> {
        let mut buf = [0u8; 65536];
        loop {
            match self.daemon.read(&mut buf) {
                Ok(0) => return Ok(self.ending.is_some()),
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Ok(self.ending.is_some()),
            }
        }
        while let Some(msg) = self.decoder.next_msg::<ServerMsg>()? {
            match msg {
                ServerMsg::View(view) => self.screen.set_view(view),
                ServerMsg::PaneFrame { pane, frame } => self.screen.apply_frame(pane, &frame),
                ServerMsg::Reconnect => return Ok(false),
                msg @ (ServerMsg::Exited | ServerMsg::Detached) => {
                    self.ending = Some(Instant::now());
                    self.event(msg);
                }
                msg @ (ServerMsg::Bell
                | ServerMsg::Clipboard(_)
                | ServerMsg::AppClipboard(_)
                | ServerMsg::Message(_)
                | ServerMsg::Error(_)) => {
                    self.event(msg);
                }
                // Rich-client messages this terminal client never asks for.
                ServerMsg::Hello { .. }
                | ServerMsg::Attached { .. }
                | ServerMsg::Pane { .. }
                | ServerMsg::WantPaste { .. }
                | ServerMsg::Picture { .. } => {}
            }
        }
        Ok(true)
    }

    fn event(&mut self, msg: ServerMsg) {
        self.next_event += 1;
        self.events.push((self.next_event, msg));
    }

    fn handle_client(&mut self, msg: FromClient) {
        if msg.state_ack <= self.state && self.history.contains_key(&msg.state_ack) {
            self.state_ack = self.state_ack.max(msg.state_ack);
        }
        self.events.retain(|(seq, _)| *seq > msg.events_ack);
        for (seq, input) in msg.inputs {
            if seq != self.input_ack + 1 {
                continue;
            }
            self.input_ack = seq;
            self.received.push((seq, Instant::now()));
            if let Err(e) = jetmux_proto::encode(&input, &mut self.daemon_out) {
                eprintln!("jetmux remote-serve: {e}");
            }
        }
    }

    fn flush_daemon(&mut self) -> io::Result<()> {
        while !self.daemon_out.is_empty() {
            match self.daemon.write(&self.daemon_out) {
                Ok(n) => {
                    self.daemon_out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The newest input that has been with the daemon long enough for its
    /// echo to be in the screen.
    fn echo_ack(&mut self) -> u64 {
        let echoed = self
            .received
            .iter()
            .filter(|(_, at)| at.elapsed() >= ECHO_DELAY)
            .map(|(seq, _)| *seq)
            .max();
        if let Some(seq) = echoed {
            self.received.retain(|(s, _)| *s > seq);
            self.echoed = self.echoed.max(seq);
        }
        self.echoed
    }

    fn maybe_send(&mut self) -> io::Result<()> {
        if self.link.peer.is_none() || self.last_send.elapsed() < TICK {
            return Ok(());
        }
        // Number the screen if it changed.
        if self.history.get(&self.state) != Some(&self.screen) {
            self.state += 1;
            self.history.insert(self.state, self.screen.clone());
        }
        let echo_ack = self.echo_ack();
        let behind = self.state > self.state_ack;
        let news =
            self.state != self.sent.0 || self.input_ack != self.sent.1 || echo_ack != self.sent.2;
        let resend =
            (behind || !self.events.is_empty()) && self.last_send.elapsed() >= self.link.rto();
        if !(news || resend || self.last_send.elapsed() >= HEARTBEAT) {
            return Ok(());
        }
        let state = if behind {
            let base = self.history.get(&self.state_ack);
            let (from, base) = match base {
                Some(base) => (self.state_ack, base.clone()),
                None => (0, Screen::default()),
            };
            Some(StateDiff {
                from,
                to: self.state,
                delta: base.diff(&self.screen),
            })
        } else {
            None
        };
        // States older than the acknowledged one are never diffed from.
        let ack = self.state_ack;
        self.history.retain(|n, _| *n >= ack);
        self.link.send(Body::Server(FromServer {
            input_ack: self.input_ack,
            echo_ack,
            events: self.events.clone(),
            state,
        }))?;
        self.sent = (self.state, self.input_ack, echo_ack);
        self.last_send = Instant::now();
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Predict {
    Adaptive,
    Always,
    Never,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConnectArgs {
    pub host: String,
    pub target: Option<String>,
    pub ssh: Option<String>,
    pub server_command: Option<String>,
    pub predict: Predict,
    /// Use this UDP endpoint instead of the one the server reports.
    pub connect: Option<String>,
    /// Run the server command here instead of over ssh (for testing).
    pub local: bool,
}

/// `jetmux remote`: start a server over ssh, then attach over UDP.
pub fn connect(server: &str, args: ConnectArgs) -> Result<(), BoxError> {
    let size = attach::terminal_size();
    let mut remote: Vec<String> = args
        .server_command
        .as_deref()
        .unwrap_or("jetmux")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    remote.extend(["-L".into(), server.into(), "remote-serve".into()]);
    remote.extend(["--size".into(), format!("{}x{}", size.cols, size.rows)]);
    if let Some(target) = &args.target {
        remote.extend(["-t".into(), target.clone()]);
    }
    let mut command = if args.local {
        let mut c = Command::new(&remote[0]);
        c.args(&remote[1..]);
        c
    } else {
        let ssh: Vec<&str> = args
            .ssh
            .as_deref()
            .unwrap_or("ssh")
            .split_whitespace()
            .collect();
        let (program, flags) = ssh.split_first().ok_or("empty --ssh")?;
        let mut c = Command::new(program);
        c.args(flags).arg(&args.host).arg("--").args(&remote);
        c
    };
    eprintln!("jetmux: starting the server on {}…", args.host);
    let mut child = command.stdout(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().ok_or("no pipe")?;
    let banner = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .find(|line| line.starts_with(BANNER))
        .ok_or("the remote didn't start a server (is jetmux installed there?)")?;
    child.wait()?;
    let mut parts = banner.split_whitespace().skip(1);
    let (ip, port, key) = match (parts.next(), parts.next(), parts.next()) {
        (Some(ip), Some(port), Some(key)) => (ip, port, key),
        _ => return Err(format!("bad server banner {banner:?}").into()),
    };
    let key = Key::decode(key).ok_or("bad key from the server")?;
    let endpoint = match &args.connect {
        Some(addr) => addr.clone(),
        None => {
            let host = args.host.rsplit('@').next().unwrap_or(&args.host);
            let host = if ip == "-" { host } else { ip };
            format!("{host}:{port}")
        }
    };
    let peer = endpoint
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| format!("can't resolve {endpoint}"))?;

    let (ui, proxy) = StdUnixStream::pair()?;
    let predict = args.predict;
    let thread = std::thread::Builder::new()
        .name("jetmux-remote".into())
        .spawn(move || {
            if let Err(e) = run_proxy(proxy, peer, &key, size, predict) {
                eprintln!("jetmux remote: {e}");
            }
        })?;
    let outcome = attach::run(ui);
    let _ = thread.join();
    match outcome? {
        attach::Outcome::Detached => println!("[detached from {}]", args.host),
        attach::Outcome::Exited => println!("[exited]"),
        attach::Outcome::Disconnected | attach::Outcome::Reconnect => {
            println!("[lost connection to {}]", args.host);
        }
    }
    Ok(())
}

fn client_socket(peer: SocketAddr) -> io::Result<UdpSocket> {
    let any: SocketAddr = if peer.is_ipv6() {
        "[::]:0".parse().map_err(io::Error::other)?
    } else {
        "0.0.0.0:0".parse().map_err(io::Error::other)?
    };
    UdpSocket::bind(any)
}

struct Proxy {
    link: Link,
    ui: UnixStream,
    ui_out: Vec<u8>,
    decoder: Decoder,
    screen: Screen,
    state: u64,
    predictor: Predictor,
    predict: Predict,
    inputs: Vec<(u64, ClientMsg)>,
    next_input: u64,
    events_ack: u64,
    echo_ack: u64,
    last_send: Instant,
    /// Something new to acknowledge since the last send.
    owe_ack: bool,
    last_rebind: Instant,
    last_notice: Instant,
    done: bool,
}

fn run_proxy(
    ui: StdUnixStream,
    peer: SocketAddr,
    key: &Key,
    size: Size,
    predict: Predict,
) -> Result<(), BoxError> {
    ui.set_nonblocking(true)?;
    let mut proxy = Proxy {
        link: Link::new(client_socket(peer)?, key, Side::Client, Some(peer))?,
        ui: UnixStream::from_std(ui),
        ui_out: Vec::new(),
        decoder: Decoder::default(),
        screen: Screen::default(),
        state: 0,
        predictor: Predictor::default(),
        predict,
        inputs: Vec::new(),
        next_input: 0,
        events_ack: 0,
        echo_ack: 0,
        last_send: Instant::now() - HEARTBEAT,
        owe_ack: true,
        last_rebind: Instant::now(),
        last_notice: Instant::now(),
        done: false,
    };
    // The server attached at the size it was started with; say ours again
    // in case the window changed meanwhile.
    proxy.queue(ClientMsg::Resize(size));
    proxy.run()
}

impl Proxy {
    fn run(&mut self) -> Result<(), BoxError> {
        let mut poll = Poll::new()?;
        poll.registry()
            .register(&mut self.link.socket, UDP, Interest::READABLE)?;
        poll.registry()
            .register(&mut self.ui, LOCAL, Interest::READABLE | Interest::WRITABLE)?;
        let mut events = Events::with_capacity(16);
        while !self.done {
            if let Err(e) = poll.poll(&mut events, Some(TICK))
                && e.kind() != io::ErrorKind::Interrupted
            {
                return Err(e.into());
            }
            if !self.read_ui()? {
                // The UI is gone without detaching; detach for it.
                self.queue(ClientMsg::Detach);
                let deadline = Instant::now() + Duration::from_secs(2);
                while !self.inputs.is_empty() && Instant::now() < deadline {
                    self.send(true)?;
                    std::thread::sleep(TICK);
                    for (_, packet) in self.link.recv() {
                        if let Body::Server(msg) = packet.body {
                            self.inputs.retain(|(seq, _)| *seq > msg.input_ack);
                        }
                    }
                }
                return Ok(());
            }
            for (_, packet) in self.link.recv() {
                if let Body::Server(msg) = packet.body {
                    self.handle_server(msg);
                }
            }
            self.keep_alive(&mut poll)?;
            self.send(false)?;
            self.flush_ui()?;
        }
        // Make sure the server hears that we got the last word; spaced out,
        // so one bad moment on the network doesn't eat them all.
        for _ in 0..3 {
            self.send(true)?;
            std::thread::sleep(TICK * 3);
        }
        self.flush_ui()?;
        Ok(())
    }

    fn predicting(&self) -> bool {
        match self.predict {
            Predict::Always => true,
            Predict::Never => false,
            Predict::Adaptive => self.link.srtt.is_some_and(|s| s > PREDICT_RTT_MS),
        }
    }

    fn queue(&mut self, msg: ClientMsg) {
        self.next_input += 1;
        self.inputs.push((self.next_input, msg));
    }

    fn send_ui(&mut self, msg: &ServerMsg) {
        if let Err(e) = jetmux_proto::encode(msg, &mut self.ui_out) {
            eprintln!("jetmux remote: {e}");
        }
    }

    /// False once the UI hung up.
    fn read_ui(&mut self) -> Result<bool, BoxError> {
        let mut buf = [0u8; 65536];
        loop {
            match self.ui.read(&mut buf) {
                Ok(0) => return Ok(false),
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Ok(false),
            }
        }
        let mut typed = false;
        while let Some(msg) = self.decoder.next_msg::<ClientMsg>()? {
            if let ClientMsg::Input(data) = &msg {
                let seq = self.next_input + 1;
                if self.predicting() {
                    for pane in self.predictor.input(&self.screen, seq, data, self.echo_ack) {
                        self.show(pane, &[], false);
                    }
                }
            }
            self.queue(msg);
            typed = true;
        }
        if typed {
            self.send(true)?;
        }
        Ok(true)
    }

    /// Send a pane's frame to the UI, predictions drawn in.
    fn show(&mut self, pane: u32, rows: &[u16], full: bool) {
        if let Some(frame) = self.predictor.frame(&self.screen, pane, rows, full) {
            self.send_ui(&ServerMsg::PaneFrame { pane, frame });
        }
    }

    fn handle_server(&mut self, msg: FromServer) {
        self.inputs.retain(|(seq, _)| *seq > msg.input_ack);
        self.echo_ack = self.echo_ack.max(msg.echo_ack);
        for (seq, event) in msg.events {
            if seq <= self.events_ack {
                continue;
            }
            self.events_ack = seq;
            self.owe_ack = true;
            if matches!(event, ServerMsg::Exited | ServerMsg::Detached) {
                self.done = true;
            }
            self.send_ui(&event);
        }
        let mut changed: Vec<(u32, Vec<u16>, bool)> = Vec::new();
        if let Some(diff) = msg.state
            && diff.to > self.state
            && (diff.from == self.state || diff.from == 0)
        {
            if diff.from == 0 {
                self.screen = Screen::default();
            }
            self.screen.apply(&diff.delta);
            self.state = diff.to;
            self.owe_ack = true;
            if let Some(view) = &diff.delta.view {
                self.send_ui(&ServerMsg::View(view.clone()));
            }
            for (pane, frame) in &diff.delta.panes {
                changed.push((*pane, frame.rows.iter().map(|r| r.y).collect(), frame.full));
            }
        }
        // Rows that lost a prediction need the server's version again.
        let reconciled = self.predictor.reconcile(&self.screen, self.echo_ack);
        for pane in reconciled {
            if !changed.iter().any(|(p, _, _)| *p == pane) {
                let rows = self
                    .screen
                    .panes
                    .get(&pane)
                    .map_or_else(Vec::new, |p| (0..p.size.rows).collect());
                changed.push((pane, rows, false));
            }
        }
        for (pane, rows, full) in changed {
            self.show(pane, &rows, full);
        }
    }

    /// Tell the user when the server's gone quiet, and try a fresh port
    /// in case a NAT forgot us.
    fn keep_alive(&mut self, poll: &mut Poll) -> io::Result<()> {
        let quiet = self.link.last_heard.elapsed();
        if quiet > QUIET && self.last_notice.elapsed() >= Duration::from_secs(1) {
            self.last_notice = Instant::now();
            let msg = format!("no contact with the server for {}s", quiet.as_secs());
            self.send_ui(&ServerMsg::Message(msg));
        }
        if quiet > REBIND
            && self.last_rebind.elapsed() > REBIND
            && let Some(peer) = self.link.peer
        {
            self.last_rebind = Instant::now();
            poll.registry().deregister(&mut self.link.socket)?;
            self.link.socket = client_socket(peer)?;
            poll.registry()
                .register(&mut self.link.socket, UDP, Interest::READABLE)?;
        }
        Ok(())
    }

    fn send(&mut self, now: bool) -> io::Result<()> {
        let since = self.last_send.elapsed();
        let due = now
            || since >= HEARTBEAT
            || (self.owe_ack && since >= TICK)
            || (!self.inputs.is_empty() && since >= self.link.rto());
        if !due {
            return Ok(());
        }
        self.link.send(Body::Client(FromClient {
            inputs: self.inputs.clone(),
            state_ack: self.state,
            events_ack: self.events_ack,
        }))?;
        self.owe_ack = false;
        self.last_send = Instant::now();
        Ok(())
    }

    fn flush_ui(&mut self) -> io::Result<()> {
        while !self.ui_out.is_empty() {
            match self.ui.write(&self.ui_out) {
                Ok(n) => {
                    self.ui_out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    self.ui_out.clear();
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
