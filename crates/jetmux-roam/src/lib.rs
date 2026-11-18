//! Roaming remote attach, mosh-style: an encrypted UDP link that survives
//! lost packets and changing addresses, synchronising screen state rather
//! than a byte stream, with predictive echo for slow links. `jetmux remote`
//! and `jetmux remote-serve` drive it.

pub mod crypto;
pub mod predict;
pub mod state;
pub mod wire;
