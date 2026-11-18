//! Socket client for the jetmux daemon, shared by the CLI and extensions.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use jetmux_proto::rpc::{Call, Event, Reply, Request};
use jetmux_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg};
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Proto(#[from] jetmux_proto::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("daemon speaks protocol {0}, this client speaks {PROTOCOL_VERSION}")]
    Version(u16),
    #[error("daemon: {0}")]
    Daemon(String),
    #[error("unexpected reply from daemon: {0:?}")]
    Unexpected(Box<ServerMsg>),
    #[error("unexpected reply from daemon: {0}")]
    BadReply(String),
}

/// `$XDG_RUNTIME_DIR/jetmux`, or `/tmp/jetmux-<uid>` when that isn't set.
pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("jetmux"),
        _ => PathBuf::from(format!(
            "/tmp/jetmux-{}",
            rustix::process::getuid().as_raw()
        )),
    }
}

pub fn socket_path(server: &str) -> PathBuf {
    socket_dir().join(format!("{server}.sock"))
}

/// `config.toml`: `$JETMUX_CONFIG`, else under `$XDG_CONFIG_HOME/jetmux`, else
/// `~/.config/jetmux`. Every jetmux binary reads its own part of it.
pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("JETMUX_CONFIG").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    xdg_dir("XDG_CONFIG_HOME", ".config").join("config.toml")
}

/// Where jetmux keeps state between runs: snapshots, push keys.
/// `$XDG_STATE_HOME/jetmux`, else `~/.local/state/jetmux`.
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

fn xdg_dir(var: &str, home_fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(home_fallback)))
        .unwrap_or_else(std::env::temp_dir)
        .join("jetmux")
}

/// A connection that has completed the version handshake.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn connect(server: &str) -> Result<Self, Error> {
        let mut stream = UnixStream::connect(socket_path(server))?;
        jetmux_proto::write_msg(
            &mut stream,
            &ClientMsg::Hello {
                version: PROTOCOL_VERSION,
            },
        )?;
        match jetmux_proto::read_msg(&mut stream)? {
            ServerMsg::Hello { version } if version == PROTOCOL_VERSION => Ok(Self { stream }),
            ServerMsg::Hello { version } => Err(Error::Version(version)),
            other => Err(unexpected(other)),
        }
    }

    pub fn send(&mut self, msg: &ClientMsg) -> Result<(), Error> {
        Ok(jetmux_proto::write_msg(&mut self.stream, msg)?)
    }

    /// Send `msg` and wait for one reply. Daemon errors become [`Error::Daemon`].
    pub fn request(&mut self, msg: &ClientMsg) -> Result<ServerMsg, Error> {
        self.send(msg)?;
        match jetmux_proto::read_msg(&mut self.stream)? {
            ServerMsg::Error(e) => Err(Error::Daemon(e)),
            reply => Ok(reply),
        }
    }

    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

/// A JSON-RPC connection to the socket API.
#[derive(Debug)]
pub struct RpcClient {
    stream: UnixStream,
    next_id: u64,
}

impl RpcClient {
    /// A handle on the connection that can shut it down from another
    /// thread, ending a blocked [`RpcClient::next_event`].
    pub fn shutdown_handle(&self) -> io::Result<UnixStream> {
        self.stream.try_clone()
    }

    pub fn connect(server: &str) -> Result<Self, Error> {
        Ok(Self {
            stream: UnixStream::connect(socket_path(server))?,
            next_id: 1,
        })
    }

    /// Make one call and wait for its result.
    pub fn call(&mut self, call: Call) -> Result<Value, Error> {
        let id = self.next_id;
        self.next_id += 1;
        jetmux_proto::write_json(&mut self.stream, &Request::new(id, call))?;
        let reply: Reply = jetmux_proto::read_json(&mut self.stream)?;
        if reply.id != Some(id.into()) {
            return Err(Error::BadReply(format!("{reply:?}")));
        }
        match (reply.result, reply.error) {
            (_, Some(e)) => Err(Error::Daemon(e.message)),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(Value::Null),
        }
    }

    /// Make a call and decode its result.
    pub fn call_as<T: DeserializeOwned>(&mut self, call: Call) -> Result<T, Error> {
        let value = self.call(call)?;
        serde_json::from_value(value).map_err(|e| Error::Proto(e.into()))
    }

    /// The next event, after `events.subscribe`.
    pub fn next_event(&mut self) -> Result<Event, Error> {
        let reply: Reply = jetmux_proto::read_json(&mut self.stream)?;
        match reply.params {
            Some(event) => Ok(event),
            None => Err(Error::BadReply(format!(
                "expected an event, got {:?}",
                reply.result
            ))),
        }
    }
}

pub fn unexpected(msg: ServerMsg) -> Error {
    Error::Unexpected(Box::new(msg))
}
