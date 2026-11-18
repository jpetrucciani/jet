//! One browser attached to one session: a WebSocket on one side, an
//! ordinary attached client connection to the daemon on the other, plus the
//! session's events so the UI can update its block cards. The daemon's
//! socket is blocking, so its two directions run on threads.

use std::os::unix::net::UnixStream;
use std::sync::mpsc as std_mpsc;

use axum::extract::ws::{Message, WebSocket};
use jetmux_client::{Client, RpcClient};
use jetmux_proto::rpc::{Call, Event};
use jetmux_proto::{Action, ClientMsg, ServerMsg, Size};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::auth::Role;

/// What the browser sends.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientIn {
    /// Typed text or key bytes, as UTF-8.
    Input {
        data: String,
    },
    Action {
        action: Action,
    },
    /// The browser's grid size in cells.
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Follow the session's size (the default), or fit it to this browser.
    Follow {
        on: bool,
    },
    Detach,
}

/// What the browser gets.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "t", content = "m", rename_all = "snake_case")]
pub enum ClientOut {
    Server(ServerMsg),
    Event(Event),
}

enum ToWs {
    Json(String),
    /// The daemon connection ended.
    Closed,
}

/// What a role may send. Viewers can only move their own view around.
fn translate(msg: ClientIn, role: Role) -> Option<ClientMsg> {
    let pair = role >= Role::Pair;
    match msg {
        ClientIn::Input { data } if pair => Some(ClientMsg::Input(data.into_bytes())),
        ClientIn::Action { action } if pair || view_only(&action) => {
            Some(ClientMsg::Action(action))
        }
        ClientIn::Resize { cols, rows } => Some(ClientMsg::Resize(Size { cols, rows })),
        ClientIn::Follow { on } if on || pair => Some(ClientMsg::Follow(on)),
        ClientIn::Detach => Some(ClientMsg::Detach),
        _ => None,
    }
}

/// Actions that only change the acting client's view.
fn view_only(action: &Action) -> bool {
    matches!(
        action,
        Action::NextTab
            | Action::PrevTab
            | Action::SelectTab(_)
            | Action::FocusPane(_)
            | Action::Focus(_)
            | Action::FocusNext
            | Action::FocusPrev
            | Action::ToggleZoom
    )
}

fn connect(
    server: &str,
    session: &str,
    size: Size,
    follow: bool,
) -> Result<(UnixStream, RpcClient), String> {
    let mut client = Client::connect(server).map_err(|e| e.to_string())?;
    let reply = client
        .request(&ClientMsg::Attach {
            target: Some(session.to_owned()),
            size,
            create: None,
            follow,
        })
        .map_err(|e| e.to_string())?;
    match reply {
        ServerMsg::Attached { .. } => {}
        ServerMsg::Error(e) => return Err(e),
        other => return Err(format!("unexpected reply {other:?}")),
    }
    let mut events = RpcClient::connect(server).map_err(|e| e.to_string())?;
    events
        .call(Call::EventsSubscribe {
            session: Some(session.to_owned()),
        })
        .map_err(|e| e.to_string())?;
    Ok((client.into_stream(), events))
}

fn json(out: &ClientOut) -> Option<String> {
    serde_json::to_string(out).ok()
}

pub async fn run(
    mut socket: WebSocket,
    server: String,
    session: String,
    size: Size,
    follow: bool,
    role: Role,
) {
    // Only owners and pairs may take the size; viewers always follow.
    let follow = follow || role < Role::Pair;
    let connected = {
        let (server, session) = (server.clone(), session.clone());
        tokio::task::spawn_blocking(move || connect(&server, &session, size, follow)).await
    };
    let (stream, mut events) = match connected {
        Ok(Ok(connected)) => connected,
        failed => {
            let e = match failed {
                Ok(Err(e)) => e,
                Err(e) => e.to_string(),
                Ok(Ok(_)) => String::new(),
            };
            if let Some(msg) = json(&ClientOut::Server(ServerMsg::Error(e))) {
                let _ = socket.send(Message::Text(msg.into())).await;
            }
            return;
        }
    };
    let handles = [stream.try_clone(), events.shutdown_handle()];
    let (to_ws, mut from_daemon) = mpsc::unbounded_channel();
    let (to_daemon, for_daemon) = std_mpsc::channel::<ClientMsg>();

    let mut reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(_) => return,
    };
    let tx = to_ws.clone();
    std::thread::spawn(move || {
        while let Ok(msg) = jetmux_proto::read_msg::<ServerMsg>(&mut reader) {
            let Some(text) = json(&ClientOut::Server(msg)) else {
                continue;
            };
            if tx.send(ToWs::Json(text)).is_err() {
                return;
            }
        }
        let _ = tx.send(ToWs::Closed);
    });
    let mut writer = stream;
    std::thread::spawn(move || {
        for msg in for_daemon {
            if jetmux_proto::write_msg(&mut writer, &msg).is_err() {
                return;
            }
        }
    });
    let tx = to_ws;
    std::thread::spawn(move || {
        while let Ok(event) = events.next_event() {
            let Some(text) = json(&ClientOut::Event(event)) else {
                continue;
            };
            if tx.send(ToWs::Json(text)).is_err() {
                return;
            }
        }
    });

    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let msg = serde_json::from_str::<ClientIn>(text.as_str())
                        .ok()
                        .and_then(|msg| translate(msg, role));
                    if let Some(msg) = msg
                        && to_daemon.send(msg).is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            outgoing = from_daemon.recv() => match outgoing {
                Some(ToWs::Json(text)) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Some(ToWs::Closed) | None => break,
            },
        }
    }
    // Ends the threads blocked on the daemon's sockets.
    for handle in handles.into_iter().flatten() {
        let _ = handle.shutdown(std::net::Shutdown::Both);
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use jetmux_proto::{Action, ClientMsg};

    use crate::auth::Role;
    use crate::bridge::{ClientIn, translate};

    #[test]
    fn viewers_only_move_their_own_view() {
        let input = || ClientIn::Input {
            data: "ls\r".into(),
        };
        assert_eq!(translate(input(), Role::Viewer), None);
        assert_eq!(
            translate(input(), Role::Pair),
            Some(ClientMsg::Input(b"ls\r".to_vec()))
        );
        let action = |action| ClientIn::Action { action };
        assert!(translate(action(Action::NextTab), Role::Viewer).is_some());
        assert!(translate(action(Action::ClosePane), Role::Viewer).is_none());
        assert!(translate(ClientIn::Follow { on: false }, Role::Viewer).is_none());
        assert!(translate(ClientIn::Follow { on: false }, Role::Owner).is_some());
    }
}
