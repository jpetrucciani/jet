//! Platform-free terminal state on libghostty-vt: parsing, input encoding,
//! search, copy mode and plain-data snapshots. Who owns the PTY and the
//! thread (jet's panes, jetmux's sessions) is up to the caller.

mod blocks;
mod copy;
mod frame;
mod grab;
mod images;
mod mirror;
mod osc;
mod screen;
mod search;
mod snapshot;
mod ssh;
mod vt;

pub use crate::blocks::{Block, BlockState, Blocks, Mark, Span};
pub use crate::copy::CopyOutcome;
pub use crate::grab::Grabbed;
pub use crate::mirror::{FrameColors, Mirror};
pub use crate::osc::PointerShape;
pub use crate::screen::{Anchor, ScreenPos, Viewport};
pub use crate::snapshot::{
    Cell, CellFlags, Colors, Cursor, CursorShape, Line, LineText, Link, Picture, Placement, Rgb,
    Snapshot, SnapshotSlot, Underline, Width,
};
pub use crate::ssh::{Ssh, ssh_destination, ssh_target};
pub use crate::vt::{
    Effects, KeyInput, MouseAction, MouseButton, MouseInput, MouseOutcome, VtCore, VtSettings,
};
pub use jetmux_proto::pane::{Context, ContextKind, Location, Notification, Occasion, Progress};
pub use jetmux_proto::{Found, Matches};
/// Key types are libghostty-vt's own: plain `Copy` data, safe to send.
pub use libghostty_vt::key::{Action as KeyAction, Key, Mods};

#[derive(Debug)]
pub enum Error {
    Vt(libghostty_vt::Error),
    Io(std::io::Error),
    /// A thread that owned the terminal exited before answering.
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
