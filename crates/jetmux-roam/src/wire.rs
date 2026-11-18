//! What goes in a datagram. Both sides send whole-state messages that can
//! be lost or arrive twice without harm: the client acknowledges the
//! server's state number and input sequence numbers, and each side resends
//! what wasn't acknowledged. Messages too big for one datagram travel as
//! fragments.

use std::collections::BTreeMap;

use jetmux_proto::{ClientMsg, ServerMsg};
use serde::{Deserialize, Serialize};

use crate::state::Delta;

/// Fragment payload size, so a sealed datagram stays under a typical MTU.
pub const MAX_FRAGMENT: usize = 1200;
/// Partially received messages kept at once.
const MAX_PARTIAL: usize = 8;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Packet {
    /// Sender's clock in milliseconds, wrapping, for round-trip times.
    pub time: u32,
    /// The latest `time` heard from the other side, and how long ago.
    pub echo: Option<Echo>,
    pub body: Body,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Echo {
    pub time: u32,
    pub held_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Body {
    Client(FromClient),
    Server(FromServer),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FromClient {
    /// Input not yet acknowledged, oldest first, numbered from 1.
    pub inputs: Vec<(u64, ClientMsg)>,
    /// The newest server state the client has.
    pub state_ack: u64,
    /// The newest server event the client has.
    pub events_ack: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FromServer {
    /// The newest input the server has passed to the daemon.
    pub input_ack: u64,
    /// The newest input whose echo has had time to show in the state.
    pub echo_ack: u64,
    /// One-off messages (bell, clipboard, exit) not yet acknowledged.
    pub events: Vec<(u64, ServerMsg)>,
    pub state: Option<StateDiff>,
}

/// Turns state `from` into state `to`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateDiff {
    pub from: u64,
    pub to: u64,
    pub delta: Delta,
}

#[derive(Debug, Serialize, Deserialize)]
struct Fragment {
    id: u32,
    index: u16,
    count: u16,
    data: Vec<u8>,
}

pub fn encode(packet: &Packet) -> Option<Vec<u8>> {
    postcard::to_stdvec(packet).ok()
}

pub fn decode(bytes: &[u8]) -> Option<Packet> {
    postcard::from_bytes(bytes).ok()
}

/// A message as datagram payloads, each small enough for one datagram.
pub fn fragments(id: u32, message: &[u8]) -> Option<Vec<Vec<u8>>> {
    let chunks: Vec<&[u8]> = message.chunks(MAX_FRAGMENT).collect();
    let count = u16::try_from(chunks.len().max(1)).ok()?;
    if chunks.is_empty() {
        let fragment = Fragment {
            id,
            index: 0,
            count,
            data: Vec::new(),
        };
        return Some(vec![postcard::to_stdvec(&fragment).ok()?]);
    }
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, data)| {
            let fragment = Fragment {
                id,
                index: u16::try_from(index).ok()?,
                count,
                data: data.to_vec(),
            };
            postcard::to_stdvec(&fragment).ok()
        })
        .collect()
}

/// Puts messages back together from fragments in any order. A message
/// with a lost fragment is dropped once newer ones crowd it out; its
/// content gets resent anyway.
#[derive(Debug, Default)]
pub struct Reassembler {
    partial: BTreeMap<u32, Vec<Option<Vec<u8>>>>,
}

impl Reassembler {
    pub fn push(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        let fragment: Fragment = postcard::from_bytes(payload).ok()?;
        let count = usize::from(fragment.count);
        let index = usize::from(fragment.index);
        if count == 1 {
            return Some(fragment.data);
        }
        if index >= count {
            return None;
        }
        let parts = self
            .partial
            .entry(fragment.id)
            .or_insert_with(|| vec![None; count]);
        if parts.len() != count {
            return None;
        }
        parts[index] = Some(fragment.data);
        if parts.iter().all(Option::is_some) {
            let parts = self.partial.remove(&fragment.id)?;
            return Some(parts.into_iter().flatten().flatten().collect());
        }
        while self.partial.len() > MAX_PARTIAL {
            self.partial.pop_first();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::wire::{MAX_FRAGMENT, Reassembler, fragments};

    #[test]
    fn fragments_reassemble_in_any_order() {
        let message: Vec<u8> = (0..MAX_FRAGMENT * 3 + 10).map(|i| i as u8).collect();
        let mut parts = fragments(7, &message).unwrap();
        assert_eq!(parts.len(), 4);
        parts.reverse();
        let mut r = Reassembler::default();
        let done: Vec<_> = parts.iter().filter_map(|p| r.push(p)).collect();
        assert_eq!(done, [message]);
        let small = fragments(8, b"hi").unwrap();
        assert_eq!(r.push(&small[0]).unwrap(), b"hi");
    }
}
