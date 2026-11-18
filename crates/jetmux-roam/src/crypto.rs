//! Datagram encryption: AES-128-GCM under a key the server makes and hands
//! to the client over ssh. Each datagram carries its sequence number in the
//! clear; the nonce is the direction plus that number, so it never repeats.
//! A sliding window drops replays but lets the fragments of one message
//! arrive out of order.

use std::io::Read;

use aes_gcm::Aes128Gcm;
use aes_gcm::aead::{Aead, KeyInit, Nonce};
use jetmux_proto::b64;

/// How far behind the newest datagram an old one may still be accepted.
const WINDOW: u64 = 64;

#[derive(Clone)]
pub struct Key([u8; 16]);

impl Key {
    pub fn random() -> std::io::Result<Self> {
        let mut key = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
        Ok(Self(key))
    }

    pub fn encode(&self) -> String {
        b64::encode(&self.0)
    }

    pub fn decode(text: &str) -> Option<Self> {
        b64::decode(text)?.try_into().ok().map(Self)
    }

    fn cipher(&self) -> Option<Aes128Gcm> {
        Aes128Gcm::new_from_slice(&self.0).ok()
    }
}

/// Who sent a datagram, so the two directions never share a nonce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Client = 0,
    Server = 1,
}

fn nonce(side: Side, seq: u64) -> Nonce<Aes128Gcm> {
    let mut nonce = [0u8; 12];
    nonce[3] = side as u8;
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    Nonce::<Aes128Gcm>::from(nonce)
}

/// Seals what one side sends.
pub struct Sealer {
    cipher: Aes128Gcm,
    side: Side,
    next: u64,
}

impl Sealer {
    pub fn new(key: &Key, side: Side) -> Option<Self> {
        Some(Self {
            cipher: key.cipher()?,
            side,
            next: 1,
        })
    }

    pub fn seal(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        let seq = self.next;
        self.next += 1;
        let sealed = self.cipher.encrypt(&nonce(self.side, seq), payload).ok()?;
        let mut out = seq.to_be_bytes().to_vec();
        out.extend_from_slice(&sealed);
        Some(out)
    }
}

/// Opens what the other side sent.
pub struct Opener {
    cipher: Aes128Gcm,
    from: Side,
    newest: u64,
    /// Bit i: `newest - i` was seen.
    seen: u64,
}

impl Opener {
    pub fn new(key: &Key, from: Side) -> Option<Self> {
        Some(Self {
            cipher: key.cipher()?,
            from,
            newest: 0,
            seen: 0,
        })
    }

    /// The payload of an authentic datagram not seen before.
    pub fn open(&mut self, datagram: &[u8]) -> Option<Vec<u8>> {
        let (seq, sealed) = datagram.split_at_checked(8)?;
        let seq = u64::from_be_bytes(seq.try_into().ok()?);
        if seq == 0 || seq + WINDOW <= self.newest {
            return None;
        }
        let back = self.newest.checked_sub(seq);
        if back.is_some_and(|b| self.seen & (1 << b) != 0) {
            return None;
        }
        let payload = self.cipher.decrypt(&nonce(self.from, seq), sealed).ok()?;
        match back {
            Some(b) => self.seen |= 1 << b,
            None => {
                let ahead = seq - self.newest;
                self.seen = if ahead >= WINDOW {
                    0
                } else {
                    self.seen << ahead
                };
                self.seen |= 1;
                self.newest = seq;
            }
        }
        Some(payload)
    }
}

#[cfg(test)]
mod tests {
    use crate::crypto::{Key, Opener, Sealer, Side};

    #[test]
    fn seals_and_rejects_replays_forgeries_and_reflection() {
        let key = Key::random().unwrap();
        let mut client = Sealer::new(&key, Side::Client).unwrap();
        let mut server = Opener::new(&key, Side::Client).unwrap();
        let a = client.seal(b"a").unwrap();
        let b = client.seal(b"b").unwrap();
        // Out of order is fine; twice is not.
        assert_eq!(server.open(&b).unwrap(), b"b");
        assert_eq!(server.open(&a).unwrap(), b"a");
        assert!(server.open(&a).is_none());
        let mut forged = client.seal(b"c").unwrap();
        let last = forged.len() - 1;
        forged[last] ^= 1;
        assert!(server.open(&forged).is_none());
        // A server's own datagram bounced back doesn't open as the client's.
        let mut from_server = Sealer::new(&key, Side::Server).unwrap();
        assert!(server.open(&from_server.seal(b"x").unwrap()).is_none());
        assert_eq!(Key::decode(&key.encode()).unwrap().0, key.0);
    }
}
