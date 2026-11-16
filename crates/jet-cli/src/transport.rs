//! The local channel: a named pipe `\\.\pipe\jet-<pid>` on Windows, only
//! for the current user and never remote; a Unix socket elsewhere, in a
//! directory only the user can enter.
//!
//! The window side gets requests on a channel ([`listen`]), each with a
//! sender for its reply (and, after `events`, the events that follow);
//! connection threads do the reading and writing, so a slow client never
//! holds up the window.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};

use crate::{Body, Envelope, Event, Failure, Reply, Request};

/// A request for the window, and where its answers go.
pub struct Incoming {
    pub request: Request,
    pub out: Sender<Message>,
}

pub enum Message {
    Reply(Body),
    Event(Event),
}

type Wake = Arc<dyn Fn() + Send + Sync>;

/// One client: requests in order, each answered before the next is read.
fn serve<S: Read + Write>(stream: S, app: &Sender<Incoming>, wake: &Wake) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let (id, body, streaming) = match serde_json::from_str::<Envelope>(&line) {
            Err(err) => (
                0,
                Body::Error(Failure::new("bad_request", err.to_string())),
                None,
            ),
            Ok(Envelope { id, request }) => {
                let streaming = matches!(request, Request::Events(_));
                let (out, answers) = mpsc::channel();
                if app.send(Incoming { request, out }).is_err() {
                    return;
                }
                wake();
                match answers.recv() {
                    Ok(Message::Reply(body)) => (id, body, streaming.then_some(answers)),
                    // The window went away mid-request.
                    Ok(Message::Event(_)) | Err(_) => return,
                }
            }
        };
        let ok = matches!(body, Body::Ok(_));
        if write_line(reader.get_mut(), &Reply { id, body }).is_err() {
            return;
        }
        if let Some(events) = streaming.filter(|_| ok) {
            // Until the window drops the subscription or the client leaves;
            // either way the other side notices on its next send.
            for message in events {
                if let Message::Event(event) = message
                    && write_line(reader.get_mut(), &event).is_err()
                {
                    return;
                }
            }
            return;
        }
    }
}

pub fn write_line<T: serde::Serialize>(out: &mut impl Write, value: &T) -> io::Result<()> {
    let mut text = serde_json::to_string(value).map_err(io::Error::other)?;
    text.push('\n');
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// A connection from `jet-cli`.
pub struct Client {
    reader: BufReader<Stream>,
    next: u64,
}

impl Client {
    pub fn connect(address: &str) -> io::Result<Self> {
        Ok(Self {
            reader: BufReader::new(connect(address)?),
            next: 1,
        })
    }

    /// Send `request` and wait for its reply.
    pub fn call(&mut self, request: Request) -> io::Result<Reply> {
        let id = self.next;
        self.next += 1;
        write_line(self.reader.get_mut(), &Envelope { id, request })?;
        let line = self.read_line()?;
        serde_json::from_str(&line).map_err(io::Error::other)
    }

    /// The next line the window sends: an event, after `events`.
    pub fn read_line(&mut self) -> io::Result<String> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "jet closed the connection",
            ));
        }
        Ok(line)
    }
}

#[cfg(windows)]
use crate::transport::pipe::{Stream, connect};
#[cfg(windows)]
pub use crate::transport::pipe::{address_for, forget, listen, running};
#[cfg(unix)]
use crate::transport::socket::{Stream, connect};
#[cfg(unix)]
pub use crate::transport::socket::{address_for, forget, listen, running};

#[cfg(windows)]
mod pipe {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::FromRawHandle;
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, mpsc::Sender};
    use std::thread;
    use std::time::Duration;

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{HSTRING, PWSTR};

    use crate::transport::{Incoming, Wake, serve};

    pub type Stream = File;

    const PREFIX: &str = r"\\.\pipe\jet-";

    pub fn address_for(pid: u32) -> String {
        format!("{PREFIX}{pid}")
    }

    /// Serve `address`. Its first instance exists when this returns, so a
    /// client started right after finds it.
    pub fn listen(
        address: &str,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Receiver<Incoming>> {
        let security = Security::current_user()?;
        let name = HSTRING::from(address);
        let first = create(&name, &security, FILE_FLAG_FIRST_PIPE_INSTANCE)?;
        let (tx, rx) = mpsc::channel();
        let wake: Wake = Arc::new(wake);
        thread::Builder::new()
            .name("jet-cli".into())
            // HANDLE isn't Send; the pipe handle it holds can move freely.
            .spawn({
                let first = first.0 as isize;
                move || accept(HANDLE(first as *mut _), &name, &security, &tx, &wake)
            })?;
        Ok(rx)
    }

    fn accept(
        mut pending: HANDLE,
        name: &HSTRING,
        security: &Security,
        app: &Sender<Incoming>,
        wake: &Wake,
    ) {
        loop {
            let connected = match unsafe { ConnectNamedPipe(pending, None) } {
                Ok(()) => true,
                Err(err) => err.code() == ERROR_PIPE_CONNECTED.to_hresult(),
            };
            if connected {
                // SAFETY: a pipe handle we own, handed to the File to close.
                let stream = unsafe { File::from_raw_handle(pending.0) };
                let (app, wake) = (app.clone(), Arc::clone(wake));
                let spawned = thread::Builder::new()
                    .name("jet-cli-client".into())
                    .spawn(move || serve(stream, &app, &wake));
                if spawned.is_err() {
                    return;
                }
            } else {
                let _ = unsafe { CloseHandle(pending) };
            }
            pending = loop {
                match create(name, security, FILE_FLAGS_AND_ATTRIBUTES(0)) {
                    Ok(handle) => break handle,
                    // Out of handles or the like: try again shortly.
                    Err(err) => {
                        eprintln!("jet: control pipe: {err}");
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            };
        }
    }

    fn create(
        name: &HSTRING,
        security: &Security,
        extra: FILE_FLAGS_AND_ATTRIBUTES,
    ) -> io::Result<HANDLE> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: security.0.0,
            bInheritHandle: false.into(),
        };
        let handle = unsafe {
            CreateNamedPipeW(
                name,
                PIPE_ACCESS_DUPLEX | extra,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    }

    /// A descriptor granting the current user, and only them, everything.
    struct Security(PSECURITY_DESCRIPTOR);

    // SAFETY: an immutable LocalAlloc'd descriptor, freed once on drop.
    unsafe impl Send for Security {}

    impl Security {
        fn current_user() -> io::Result<Self> {
            let sid = user_sid()?;
            let sddl = HSTRING::from(format!("D:P(A;;GA;;;{sid})"));
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    &sddl,
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
            }
            .map_err(io::Error::other)?;
            Ok(Self(descriptor))
        }
    }

    impl Drop for Security {
        fn drop(&mut self) {
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    fn user_sid() -> io::Result<String> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(io::Error::other)?;
        let mut size = 0u32;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
        // u64s keep TOKEN_USER's pointer aligned.
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let got = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            )
        };
        let _ = unsafe { CloseHandle(token) };
        got.map_err(io::Error::other)?;
        // SAFETY: filled in as a TOKEN_USER by the call above.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut text = PWSTR::null();
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }.map_err(io::Error::other)?;
        let sid = unsafe { text.to_string() }.map_err(io::Error::other);
        let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
        sid
    }

    /// A pipe goes away with the last handle to it.
    pub fn forget(_: &str) {}

    /// `(pid, address)` of every jet serving a pipe.
    pub fn running() -> io::Result<Vec<(u32, String)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(r"\\.\pipe\")? {
            let name = entry?.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_prefix("jet-"))
                .and_then(|p| p.parse().ok())
            else {
                continue;
            };
            out.push((pid, address_for(pid)));
        }
        Ok(out)
    }

    pub fn connect(address: &str) -> io::Result<File> {
        // Busy between one client being taken and the next instance; brief.
        let mut tries = 0;
        loop {
            match File::options().read(true).write(true).open(address) {
                Err(err) if err.raw_os_error() == Some(231) && tries < 20 => {
                    tries += 1;
                    thread::sleep(Duration::from_millis(50));
                }
                result => return result,
            }
        }
    }
}

#[cfg(unix)]
mod socket {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    use crate::transport::{Incoming, Wake, serve};

    pub type Stream = UnixStream;

    fn dir() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map_or_else(std::env::temp_dir, PathBuf::from);
        base.join("jet")
    }

    pub fn address_for(pid: u32) -> String {
        address_in(&dir(), pid)
    }

    fn address_in(dir: &Path, pid: u32) -> String {
        dir.join(format!("{pid}.sock"))
            .to_string_lossy()
            .into_owned()
    }

    pub fn listen(
        address: &str,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Receiver<Incoming>> {
        let path = PathBuf::from(address);
        if let Some(parent) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        // A socket left by a crashed jet with a reused pid.
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let (tx, rx) = mpsc::channel();
        let wake: Wake = Arc::new(wake);
        thread::Builder::new()
            .name("jet-cli".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (app, wake) = (tx.clone(), Arc::clone(&wake));
                    let _ = thread::Builder::new()
                        .name("jet-cli-client".into())
                        .spawn(move || serve(stream, &app, &wake));
                }
            })?;
        Ok(rx)
    }

    /// Remove a window's socket once it stops serving.
    pub fn forget(address: &str) {
        let _ = fs::remove_file(address);
    }

    /// `(pid, address)` of every jet serving a socket. Sockets nobody
    /// listens on (a jet that was killed) are removed on the way.
    pub fn running() -> io::Result<Vec<(u32, String)>> {
        running_in(&dir())
    }

    pub fn running_in(dir: &Path) -> io::Result<Vec<(u32, String)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let name = entry?.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".sock"))
                .and_then(|p| p.parse().ok())
            else {
                continue;
            };
            let address = address_in(dir, pid);
            if UnixStream::connect(&address)
                .is_err_and(|err| err.kind() == io::ErrorKind::ConnectionRefused)
            {
                forget(&address);
                continue;
            }
            out.push((pid, address));
        }
        Ok(out)
    }

    pub fn connect(address: &str) -> io::Result<UnixStream> {
        UnixStream::connect(address)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use crate::transport::socket::running_in;
    use crate::transport::{Client, Message, listen};
    use crate::{Answer, Body, Done, Event, Events, Failure, Hello, Request, What};

    #[test]
    fn requests_reach_the_window_and_events_stream_back() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("jet-cli-test-{}", std::process::id()));
        let address = dir.join("1.sock").to_string_lossy().into_owned();
        let rx = listen(&address, || {})?;
        // The window's side: answer, then (for events) send one.
        std::thread::spawn(move || {
            for incoming in rx {
                let streaming = matches!(incoming.request, Request::Events(_));
                let body = match incoming.request {
                    Request::Hello => Body::Ok(Answer::Hello(Hello {
                        version: 1,
                        pid: 42,
                        quake: false,
                    })),
                    Request::Events(_) => Body::Ok(Answer::Done(Done {})),
                    _ => Body::Error(Failure::new("unsupported", "no")),
                };
                let _ = incoming.out.send(Message::Reply(body));
                if streaming {
                    let bell = Event {
                        what: What::Bell,
                        window: 42,
                        tab: None,
                        pane: Some(1),
                    };
                    let _ = incoming.out.send(Message::Event(bell));
                }
            }
        });

        let mut client = Client::connect(&address)?;
        let reply = client.call(Request::Hello)?;
        assert_eq!(reply.id, 1);
        assert!(matches!(
            reply.body,
            Body::Ok(Answer::Hello(Hello { pid: 42, .. }))
        ));
        let reply = client.call(Request::List)?;
        assert!(matches!(reply.body, Body::Error(f) if f.code == "unsupported"));

        let reply = client.call(Request::Events(Events::default()))?;
        assert!(matches!(reply.body, Body::Ok(_)));
        let line = client.read_line()?;
        let event: Event = serde_json::from_str(&line).map_err(std::io::Error::other)?;
        assert_eq!(event.what, What::Bell);
        std::fs::remove_dir_all(dir)
    }

    #[test]
    fn sockets_left_by_dead_windows_are_pruned() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("jet-cli-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let dead = dir.join("7.sock");
        drop(std::os::unix::net::UnixListener::bind(&dead)?);
        let _live = std::os::unix::net::UnixListener::bind(dir.join("8.sock"))?;
        let running = running_in(&dir)?;
        assert_eq!(running.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(), [8]);
        assert!(!dead.exists());
        std::fs::remove_dir_all(dir)
    }
}
