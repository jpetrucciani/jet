//! Platform-free terminal core: owns libghostty-vt terminals on their own
//! threads and hands plain-data snapshots to the renderer.

mod images;
mod osc;
mod pane;
mod search;
mod snapshot;
mod vt;

pub use crate::osc::{Notification, Progress};
pub use crate::pane::{Pane, PaneEvent, PaneOptions};
pub use crate::snapshot::{
    Cell, CellFlags, Colors, Cursor, CursorShape, Found, Line, Link, Matches, Picture, Placement,
    Rgb, Snapshot, SnapshotSlot, Underline, Width,
};
pub use crate::vt::{
    Effects, KeyInput, Location, MouseAction, MouseButton, MouseInput, MouseOutcome, VtCore,
    VtSettings,
};
/// Key types are libghostty-vt's own: plain `Copy` data, safe to send.
pub use libghostty_vt::key::{Action as KeyAction, Key, Mods};

#[derive(Debug)]
pub enum Error {
    Vt(libghostty_vt::Error),
    Io(std::io::Error),
    /// The pane thread exited before answering.
    Closed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vt(err) => write!(f, "libghostty-vt: {err}"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Closed => f.write_str("pane thread exited"),
        }
    }
}

impl std::error::Error for Error {}

impl From<libghostty_vt::Error> for Error {
    fn from(err: libghostty_vt::Error) -> Self {
        Self::Vt(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
