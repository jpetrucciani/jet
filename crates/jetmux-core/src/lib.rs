//! Session, pane and layout model. Terminal state is jet-core's.

mod pane;
mod session;
pub mod snapshot;

pub use crate::pane::{Pane, Reader, Spawn};
pub use crate::session::{Acted, ClientId, Outcome, Session, pane_area};
pub use jetmux_proto::layout::PaneId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("terminal: {0}")]
    Vt(#[from] jet_core::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A request that doesn't fit the session: a missing pane, a bad spec.
    #[error("{0}")]
    Spec(String),
}

/// Lines of history a pane keeps.
const SCROLLBACK_LINES: usize = 10_000;

/// A terminal as jetmux runs one: its history, command blocks, and
/// XTVERSION saying jetmux so apps can tell they're inside a multiplexer.
pub fn vt(size: jetmux_proto::Size) -> Result<jet_core::VtCore, Error> {
    let mut vt = jet_core::VtCore::new(size.cols.max(1), size.rows.max(1), SCROLLBACK_LINES)?;
    vt.set_name(concat!("jetmux ", env!("CARGO_PKG_VERSION")));
    vt.track_blocks();
    Ok(vt)
}

impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Self::Io(e.into())
    }
}
