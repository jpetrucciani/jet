//! Wire protocol shared by every jetmux binary.
//!
//! Every frame on the socket is `[u32 len LE][u8 kind][payload]`, where `len`
//! counts the kind byte plus the payload. [`FrameKind::Binary`] payloads are
//! postcard-encoded [`ClientMsg`]/[`ServerMsg`] values (the hot path);
//! [`FrameKind::Rpc`] payloads are JSON-RPC 2.0 messages (see [`rpc`]).

pub mod b64;
pub mod input;
pub mod layout;
pub mod pane;
pub mod rpc;

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u16 = 10;

/// Upper bound on a single frame, so a corrupt length can't make us allocate
/// gigabytes.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

const HEADER_LEN: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("encode/decode: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
    #[error("unknown frame kind {0:#04x}")]
    UnknownKind(u8),
    #[error("empty frame")]
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Binary = 0x01,
    Rpc = 0x02,
}

impl TryFrom<u8> for FrameKind {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x01 => Ok(Self::Binary),
            0x02 => Ok(Self::Rpc),
            other => Err(Error::UnknownKind(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on every connection.
    Hello {
        version: u16,
    },
    /// Attach to `target`, or the most recent session when unset. When no
    /// such session exists and `create` is set, create it first.
    Attach {
        target: Option<String>,
        size: Size,
        create: Option<NewSession>,
        /// Follow the session's size instead of setting it: the client gets
        /// the layout the size owner sees and never resizes panes, as a phone
        /// pinch-zooming a desktop session wants.
        follow: bool,
    },
    /// Create a session and attach to it, failing if the name is taken.
    New(NewSession),
    /// Bytes for the client's focused pane.
    Input(Vec<u8>),
    /// A key for a pane, encoded by the daemon for the pane's modes (or
    /// taken by its copy mode).
    Key {
        pane: u32,
        event: input::KeyEvent,
    },
    Mouse(input::MouseEvent),
    /// Mouse wheel notches over a pane (negative is up), at a cell.
    Wheel {
        pane: u32,
        notches: i32,
        col: u16,
        row: u16,
        mods: u16,
    },
    /// Text for a pane, bracketed if the app asked.
    Paste {
        pane: u32,
        text: String,
    },
    /// The client's window gained or lost focus over a pane.
    Focus {
        pane: u32,
        focused: bool,
    },
    /// The client's colors, for answering apps' color queries.
    Theme(input::Theme),
    /// The pixel size of the client's cells, for laying out images and
    /// answering size queries, from the client that acted last.
    CellSize {
        width: u32,
        height: u32,
    },
    /// Send [`ServerMsg::Pane`] reports from now on, each pane's state
    /// first.
    WantPaneEvents,
    Action(Action),
    /// The client's whole terminal size.
    Resize(Size),
    /// Start or stop following the session's size. Stopping makes the
    /// client the size owner ("fit to me").
    Follow(bool),
    Detach,
}

/// Everything a user can do to a session besides typing. The socket API and
/// key bindings both resolve to these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Action {
    /// Open a tab after the current one.
    NewTab(PaneSpec),
    CloseTab,
    NextTab,
    PrevTab,
    /// Zero-based.
    SelectTab(u16),
    /// Move the current tab left (negative) or right.
    MoveTab(i16),
    RenameTab(String),
    /// Split the focused pane, putting the new one on that side.
    Split(Direction, PaneSpec),
    ClosePane,
    /// Focus a pane by id, switching to its tab.
    FocusPane(u32),
    Focus(Direction),
    FocusNext,
    FocusPrev,
    /// Swap the focused pane with its neighbor in that direction.
    Swap(Direction),
    /// Swap the focused pane with the next (`true`) or previous pane.
    Rotate(bool),
    /// Move the focused pane's edge on that side by some cells.
    Resize(Direction, u16),
    /// Put the current tab's `split`th border (in
    /// [`layout::Layout::borders`] order) this many thousandths of the way.
    SetRatio {
        split: u16,
        permille: u16,
    },
    ToggleZoom,
    EnterCopyMode,
    /// Search the focused pane's screen and history (smart case); empty
    /// ends the search.
    Search {
        query: String,
        regex: bool,
    },
    /// Go this many matches up (negative, older) or down, wrapping.
    SearchStep(i32),
    Copy(CopyAction),
    /// Paste the session's copy buffer into the focused pane.
    Paste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum CopyAction {
    Move(Motion),
    /// Scroll the view by rows, keeping the cursor on screen.
    Scroll(i32),
    Select(SelectionKind),
    /// Move the cursor to the selection's other end (vi's `o`).
    SwapAnchor,
    ClearSelection,
    /// Copy the selection (or the cursor's line) and leave copy mode.
    Yank,
    Search {
        pattern: String,
        backward: bool,
    },
    SearchNext,
    SearchPrev,
    /// Jump to the previous or next shell prompt (OSC 133).
    Prompt {
        backward: bool,
    },
    /// Jump back to the output of the last command that failed.
    PrevFailure,
    /// Copy the output of the command under the cursor and leave.
    YankOutput,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    FirstNonBlank,
    LineEnd,
    WordForward,
    WordBackward,
    WordEnd,
    Top,
    Bottom,
    ViewTop,
    ViewMiddle,
    ViewBottom,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum SelectionKind {
    Char,
    Line,
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Axis {
    /// Children side by side, separated by a vertical border.
    Columns,
    /// Children stacked, separated by a horizontal border.
    Rows,
}

/// What to run in a new pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PaneSpec {
    /// Program and arguments. Empty means the user's login shell.
    pub command: Vec<String>,
    /// Defaults to the cwd of the pane it was opened from.
    pub cwd: Option<String>,
}

/// A tab to build when a session starts, or a saved one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabSpec {
    pub name: Option<String>,
    pub layout: LayoutSpec,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutSpec {
    Pane(PaneSpec),
    /// Children share the space in proportion to their weights.
    Split {
        axis: Axis,
        children: Vec<(u16, LayoutSpec)>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewSession {
    /// Defaults to the lowest unused number.
    pub name: Option<String>,
    /// Program and arguments for the first pane when `tabs` is empty. Empty
    /// means the user's login shell.
    pub command: Vec<String>,
    pub cwd: Option<String>,
    /// Extra environment for every pane in the session.
    pub env: Vec<(String, String)>,
    /// Tabs to start with; empty means one tab running `command`.
    pub tabs: Vec<TabSpec>,
    /// Size of the creating client's whole terminal.
    pub size: Size,
    /// The config profile the session comes from, for restores.
    pub profile: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ServerMsg {
    Hello {
        version: u16,
    },
    Attached {
        session: String,
    },
    /// This client's view changed: tabs, layout or focus.
    View(View),
    PaneFrame {
        pane: u32,
        frame: Frame,
    },
    /// Text this client's user copied (copy mode, the mouse), for its
    /// clipboard.
    Clipboard(String),
    /// A program's clipboard write (OSC 52), for clients that allow those.
    AppClipboard(String),
    /// What a pane reported. On attach, every pane's current location,
    /// progress, context and pointer come first.
    Pane {
        pane: u32,
        event: pane::PaneEvent,
    },
    /// A right click with nothing selected: send the clipboard back as a
    /// [`ClientMsg::Paste`].
    WantPaste {
        pane: u32,
    },
    /// A kitty graphics picture, once per connection, for
    /// [`ImagePlacement::picture`].
    Picture {
        key: u64,
        width: u32,
        height: u32,
        /// Straight (not premultiplied) 8-bit RGBA.
        rgba: Vec<u8>,
    },
    Bell,
    /// A short note for the status bar, e.g. "no match".
    Message(String),
    /// The session's last pane exited.
    Exited,
    Detached,
    /// The daemon is being replaced (hot upgrade): reconnect and attach to
    /// the same session again.
    Reconnect,
    Error(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl Rect {
    pub fn size(self) -> Size {
        Size {
            cols: self.cols,
            rows: self.rows,
        }
    }

    pub fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x
            && y >= self.y
            && u32::from(x) < u32::from(self.x) + u32::from(self.cols)
            && u32::from(y) < u32::from(self.y) + u32::from(self.rows)
    }
}

/// One client's view of its session, laid out for that client's size.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct View {
    pub session: String,
    pub tabs: Vec<TabView>,
    pub active_tab: u16,
    /// Visible panes and where to draw them. Everything else in the pane
    /// area is border.
    pub panes: Vec<PaneView>,
    pub zoomed: bool,
    /// The area panes and borders are laid out in.
    pub area: Rect,
}

/// A tab, shown or not: clients that draw splits themselves mirror
/// `layout`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TabView {
    /// Fixed for the tab's life.
    pub id: u32,
    pub name: String,
    pub layout: layout::Layout,
    /// The client's focused pane there.
    pub focused: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PaneView {
    pub id: u32,
    pub rect: Rect,
    pub focused: bool,
    pub copy_mode: bool,
    pub title: String,
}

/// Grid-level render update for one pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Frame {
    /// Every visible row is included; the client should clear first.
    pub full: bool,
    pub size: Size,
    /// Styles referenced by [`Run::style`], interned per frame.
    pub styles: Vec<Style>,
    /// OSC 8 link targets referenced by [`Run::link`], interned per frame.
    pub links: Vec<String>,
    pub rows: Vec<RowUpdate>,
    /// `None` when the cursor is hidden or outside the viewport.
    pub cursor: Option<Cursor>,
    pub modes: Modes,
    pub extras: FrameExtras,
}

/// What a pane shows besides its grid, whole in every frame.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FrameExtras {
    /// Kitty graphics that show, lowest `z` first. Each picture comes in a
    /// [`ServerMsg::Picture`] before the first frame that shows it.
    pub images: Vec<ImagePlacement>,
    /// The pixel size of a cell that `images` are laid out for; a client
    /// with other cells scales them.
    pub cell: [u32; 2],
    /// Colors the app changed (OSC 4, 10, 11, 12) over the client's theme.
    pub colors: AppColors,
    /// The pane's search, while it has a query.
    pub search: Option<Matches>,
}

/// Where a picture shows: in pixels from the pane's first cell, which may
/// put it partly above or left of the pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ImagePlacement {
    pub picture: u64,
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
    /// `[x, y, width, height]` of the picture to show, in its pixels.
    pub source: [u32; 4],
    /// Negative draws under the text, else over it.
    pub z: i32,
}

/// Colors an app set, each `None` (or missing from `palette`) where it
/// left the theme's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AppColors {
    pub foreground: Option<input::Rgb>,
    pub background: Option<input::Rgb>,
    pub cursor: Option<input::Rgb>,
    pub palette: Vec<(u8, input::Rgb)>,
}

/// A search's results, as of a frame or snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Matches {
    /// All of them, screen and history, up to [`Matches::CAP`].
    pub total: usize,
    /// The one navigated to, by its index among all.
    pub current: Option<usize>,
    pub visible: Vec<Found>,
    /// The query is a regular expression that doesn't compile.
    pub invalid: bool,
}

impl Matches {
    /// Most matches a search keeps; at this the count is a floor.
    pub const CAP: usize = 10_000;
}

/// A match in the viewport.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Found {
    pub row: u16,
    pub cols: std::ops::Range<u16>,
    pub current: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RowUpdate {
    pub y: u16,
    /// Consecutive same-style cells, left to right, covering the whole row.
    pub runs: Vec<Run>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Run {
    pub style: u16,
    /// Cell text in order.
    pub text: String,
    /// `None`: every char in `text` is one narrow cell. `Some(width)`: `text`
    /// is a single cell of that width (a wide char or a multi-codepoint
    /// grapheme). Lets clients clip runs to a pane without a width table.
    pub cluster: Option<u8>,
    /// 1-based index into [`Frame::links`]; 0 when the cells aren't a link.
    pub link: u16,
}

impl Run {
    pub fn cells(&self) -> usize {
        match self.cluster {
            Some(width) => usize::from(width),
            None => self.text.chars().count(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Color {
    #[default]
    Default,
    Palette(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub underline: Underline,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum CursorShape {
    #[default]
    Block,
    BlockHollow,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub blinking: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum MouseTracking {
    #[default]
    Off,
    X10,
    Normal,
    Button,
    Any,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum MouseFormat {
    #[default]
    X10,
    Utf8,
    Sgr,
    Urxvt,
    SgrPixels,
}

/// Input-affecting modes the client mirrors onto the outer terminal so the
/// outer terminal encodes keys, pastes and mouse events the way the app
/// expects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Modes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub mouse: MouseTracking,
    pub mouse_format: MouseFormat,
    /// Kitty keyboard protocol flags the app asked for; 0 is legacy keys.
    pub kitty_keyboard: u8,
}

/// Encode `msg` as a complete binary frame appended to `out`.
pub fn encode<T: Serialize>(msg: &T, out: &mut Vec<u8>) -> Result<(), Error> {
    let start = out.len();
    out.extend_from_slice(&[0; HEADER_LEN]);
    *out = postcard::to_extend(msg, std::mem::take(out))?;
    finish_frame(out, start, FrameKind::Binary)
}

/// Encode `msg` as a complete JSON-RPC frame appended to `out`.
pub fn encode_json<T: Serialize>(msg: &T, out: &mut Vec<u8>) -> Result<(), Error> {
    let start = out.len();
    out.extend_from_slice(&[0; HEADER_LEN]);
    if let Err(e) = serde_json::to_writer(&mut *out, msg) {
        out.truncate(start);
        return Err(e.into());
    }
    finish_frame(out, start, FrameKind::Rpc)
}

fn finish_frame(out: &mut Vec<u8>, start: usize, kind: FrameKind) -> Result<(), Error> {
    let len = out.len() - start - 4;
    if len > MAX_FRAME_LEN {
        out.truncate(start);
        return Err(Error::TooLarge(len));
    }
    let len_bytes = u32::try_from(len).map_err(|_| Error::TooLarge(len))?;
    out[start..start + 4].copy_from_slice(&len_bytes.to_le_bytes());
    out[start + 4] = kind as u8;
    Ok(())
}

/// Write one binary frame to a blocking writer.
pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> Result<(), Error> {
    let mut buf = Vec::new();
    encode(msg, &mut buf)?;
    w.write_all(&buf)?;
    Ok(())
}

/// Write one JSON-RPC frame to a blocking writer.
pub fn write_json<T: Serialize>(w: &mut impl Write, msg: &T) -> Result<(), Error> {
    let mut buf = Vec::new();
    encode_json(msg, &mut buf)?;
    w.write_all(&buf)?;
    Ok(())
}

/// Read exactly one binary frame from a blocking reader. Reads no bytes past
/// the frame, so the stream can be handed off afterwards.
pub fn read_msg<T: DeserializeOwned>(r: &mut impl Read) -> Result<T, Error> {
    let (kind, payload) = read_frame(r)?;
    decode_payload(kind as u8, &payload)
}

/// Read one JSON-RPC frame from a blocking reader.
pub fn read_json<T: DeserializeOwned>(r: &mut impl Read) -> Result<T, Error> {
    match read_frame(r)? {
        (FrameKind::Rpc, payload) => Ok(serde_json::from_slice(&payload)?),
        (kind, _) => Err(Error::UnknownKind(kind as u8)),
    }
}

/// Read one raw frame of either kind. Reads no bytes past it.
pub fn read_frame(r: &mut impl Read) -> Result<(FrameKind, Vec<u8>), Error> {
    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header)?;
    let len = frame_len(&header)?;
    let kind = FrameKind::try_from(header[4])?;
    let mut payload = vec![0u8; len - 1];
    r.read_exact(&mut payload)?;
    Ok((kind, payload))
}

fn frame_len(header: &[u8; HEADER_LEN]) -> Result<usize, Error> {
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if len == 0 {
        return Err(Error::Empty);
    }
    if len > MAX_FRAME_LEN {
        return Err(Error::TooLarge(len));
    }
    Ok(len)
}

/// Decode a payload read with [`read_frame`].
pub fn decode_payload<T: DeserializeOwned>(kind: u8, payload: &[u8]) -> Result<T, Error> {
    match FrameKind::try_from(kind)? {
        FrameKind::Binary => Ok(postcard::from_bytes(payload)?),
        FrameKind::Rpc => Err(Error::UnknownKind(kind)),
    }
}

/// Incremental frame decoder for nonblocking sockets.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pop the next complete message, if one is buffered.
    pub fn next_msg<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        let Some(header) = self.buf.first_chunk::<HEADER_LEN>() else {
            return Ok(None);
        };
        let len = frame_len(header)?;
        let total = 4 + len;
        if self.buf.len() < total {
            return Ok(None);
        }
        let msg = decode_payload(self.buf[4], &self.buf[HEADER_LEN..total]);
        self.buf.drain(..total);
        msg.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use crate::{ClientMsg, Decoder, Size, encode};

    #[test]
    fn decoder_reassembles_split_frames() {
        let msgs = [
            ClientMsg::Hello { version: 1 },
            ClientMsg::Input(b"echo hi\r".to_vec()),
            ClientMsg::Resize(Size { cols: 80, rows: 24 }),
        ];
        let mut wire = Vec::new();
        for msg in &msgs {
            encode(msg, &mut wire).unwrap();
        }

        let mut decoder = Decoder::default();
        let mut got = Vec::new();
        for byte in wire {
            decoder.push(&[byte]);
            while let Some(msg) = decoder.next_msg::<ClientMsg>().unwrap() {
                got.push(msg);
            }
        }
        assert_eq!(got, msgs);
    }
}
