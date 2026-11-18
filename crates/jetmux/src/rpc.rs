//! The daemon's side of the JSON-RPC socket API. Each RPC connection runs
//! on its own thread: daemon-wide methods are answered from the registry,
//! session methods are forwarded to the session's loop.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use jetmux_proto::rpc::{
    Call, Created, Event, Reply, Request, Restored, RpcError, Streaming, Target,
};
use serde::Serialize;
use serde_json::Value;

use crate::daemon::Registry;
use crate::session;

type BoxError = Box<dyn std::error::Error>;

/// How long a session gets to answer anything but `pane.wait`.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

pub fn serve(
    registry: &Arc<Registry>,
    mut stream: UnixStream,
    first: &[u8],
) -> Result<(), BoxError> {
    let mut payload = first.to_vec();
    loop {
        let (id, result) = match Request::parse(&payload) {
            Ok(Request {
                id,
                call: Call::EventsSubscribe { session },
                ..
            }) => {
                let reply = Reply::response(id, Ok(Value::Null));
                jetmux_proto::write_json(&mut stream, &reply)?;
                let events = registry.events.subscribe();
                return stream_events(stream, events, session.as_deref());
            }
            Ok(Request {
                id,
                call: call @ Call::PaneStream { .. },
                ..
            }) => match tap(registry, &call) {
                Ok((started, output)) => {
                    let reply = Reply::response(id, to_value(started));
                    jetmux_proto::write_json(&mut stream, &reply)?;
                    return stream_events(stream, output, None);
                }
                Err(e) => (id, Err(e)),
            },
            Ok(Request { id, call, .. }) => (id, call_method(registry, call)),
            Err((id, e)) => (id, Err(e)),
        };
        jetmux_proto::write_json(&mut stream, &Reply::response(id, result))?;
        payload = match jetmux_proto::read_frame(&mut stream) {
            Ok((_, payload)) => payload,
            Err(jetmux_proto::Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
    }
}

fn to_value(value: impl Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::failed(e.to_string()))
}

fn call_method(registry: &Registry, call: Call) -> Result<Value, RpcError> {
    match call {
        Call::SessionList {} => to_value(registry.list()),
        Call::SessionCreate(spec) => {
            let session = registry.create(spec).map_err(RpcError::failed)?;
            to_value(Created { session })
        }
        Call::SessionKill { target } => {
            registry.kill(&target).map_err(RpcError::failed)?;
            Ok(Value::Null)
        }
        Call::ServerStop {} => {
            registry.stop();
            Ok(Value::Null)
        }
        Call::ServerSnapshot {} => to_value(registry.save().map_err(RpcError::failed)?),
        Call::ServerRestore {} => {
            let rerun = crate::config::load()
                .map(|c| c.persist.rerun)
                .map_err(RpcError::failed)?;
            to_value(Restored {
                sessions: registry.restore(rerun).map_err(RpcError::failed)?,
            })
        }
        // A successful upgrade never answers: the exec closes this
        // connection, which is the caller's signal.
        Call::ServerUpgrade { binary } => {
            registry.upgrade(binary).map_err(RpcError::failed)?;
            Ok(Value::Null)
        }
        Call::EventsSubscribe { .. } => Err(RpcError::failed("events.subscribe is handled above")),
        call => {
            let target = session_target(&call)?;
            let name = registry.find(target.session.as_deref()).ok_or_else(|| {
                RpcError::failed(match &target.session {
                    Some(name) => crate::daemon::no_session(name),
                    None => "no sessions".into(),
                })
            })?;
            let wait = matches!(call, Call::PaneWait { .. });
            let (tx, rx) = mpsc::channel();
            registry
                .send(&name, session::Command::Call(target, Box::new(call), tx))
                .map_err(RpcError::failed)?;
            let reply = if wait {
                rx.recv().ok()
            } else {
                rx.recv_timeout(CALL_TIMEOUT).ok()
            };
            reply.unwrap_or_else(|| Err(RpcError::failed(format!("session {name} didn't answer"))))
        }
    }
}

/// Start tapping a pane's output for `pane.stream`.
fn tap(registry: &Registry, call: &Call) -> Result<(Streaming, mpsc::Receiver<Event>), RpcError> {
    let target = session_target(call)?;
    let name = registry
        .find(target.session.as_deref())
        .ok_or_else(|| RpcError::failed("no such session"))?;
    let (output_tx, output) = mpsc::channel();
    let (reply_tx, reply) = mpsc::channel();
    registry
        .send(&name, session::Command::Tap(target, output_tx, reply_tx))
        .map_err(RpcError::failed)?;
    let started = reply
        .recv_timeout(CALL_TIMEOUT)
        .map_err(|_| RpcError::failed(format!("session {name} didn't answer")))??;
    Ok((started, output))
}

/// The parsed target of a session-scoped call.
fn session_target(call: &Call) -> Result<Target, RpcError> {
    let target = match call {
        Call::TabList { target }
        | Call::TabNew { target, .. }
        | Call::TabClose { target }
        | Call::TabSelect { target }
        | Call::TabRename { target, .. }
        | Call::TabMove { target, .. }
        | Call::PaneList { target }
        | Call::PaneSplit { target, .. }
        | Call::PaneClose { target }
        | Call::PaneFocus { target, .. }
        | Call::PaneZoom { target }
        | Call::PaneSwap { target, .. }
        | Call::PaneResize { target, .. }
        | Call::PaneSendKeys { target, .. }
        | Call::PaneCapture { target, .. }
        | Call::PaneWait { target, .. }
        | Call::PaneBlocks { target }
        | Call::PaneReadBlock { target, .. }
        | Call::PaneStream { target }
        | Call::LayoutSave { target } => target.as_deref(),
        Call::SessionList {}
        | Call::SessionCreate(_)
        | Call::SessionKill { .. }
        | Call::ServerStop {}
        | Call::ServerSnapshot {}
        | Call::ServerRestore {}
        | Call::ServerUpgrade { .. }
        | Call::EventsSubscribe { .. } => None,
    };
    Target::parse(target.unwrap_or_default()).map_err(RpcError::failed)
}

/// Forward events until the subscriber hangs up. A reader thread watches
/// for the hangup, since events alone may never come.
fn stream_events(
    mut stream: UnixStream,
    events: mpsc::Receiver<Event>,
    session: Option<&str>,
) -> Result<(), BoxError> {
    let (hangup_tx, hangup) = mpsc::channel::<()>();
    let mut reader = stream.try_clone()?;
    std::thread::Builder::new()
        .name("jetmux-rpc-hangup".into())
        .spawn(move || {
            // Anything sent after subscribing is ignored; EOF ends the stream.
            while jetmux_proto::read_frame(&mut reader).is_ok() {}
            let _ = hangup_tx.send(());
        })?;
    while hangup.try_recv().is_err() {
        let event: Event = match events.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if session.is_some_and(|s| s != event.session()) {
            continue;
        }
        if jetmux_proto::write_json(&mut stream, &Reply::event(event)).is_err() {
            break;
        }
    }
    // The hangup thread holds a clone of the socket; shutting it down is
    // what tells the subscriber the stream ended, and ends that thread.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}
