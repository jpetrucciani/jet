//! Owns one libghostty-vt `Terminal` and everything bound to it. Not `Send`:
//! create and use it on a single thread.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::render::{CellIterator, CursorVisualStyle, Dirty, RowIterator};
use libghostty_vt::screen::{
    CellContentTag, CellSemanticContent, CellWide, GridRef, RowSemanticPrompt, Screen,
};
use libghostty_vt::selection::gesture::{
    Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
};
use libghostty_vt::style::{Palette, StyleColor, Underline as VtUnderline};
use libghostty_vt::terminal::{
    ClipboardLocation, ClipboardWriteError, ColorScheme, CursorStyle, Mode, Point, PointCoordinate,
    ScrollViewport, SizeReportSize,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions, focus, key, mouse, selection};

use crate::Error;
use crate::images;
use crate::osc::{self, Notification, Progress};
use crate::search::Search;
use crate::snapshot::{
    Cell, CellFlags, Colors, Cursor, CursorShape, Line, Rgb, Snapshot, Underline, Width,
};

/// A key press or release from the window, before encoding. Encoding happens
/// on the pane thread so it always sees the terminal's current modes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub key: key::Key,
    pub action: key::Action,
    pub mods: key::Mods,
    /// Text the key produces in the current layout, without Ctrl/Alt
    /// applied. Never C0 control characters.
    pub text: Option<String>,
    /// The key's character with no modifiers, e.g. `'a'` for Shift+A.
    pub unshifted: Option<char>,
}

/// User settings a terminal follows. Colors are defaults: apps can still
/// override them (OSC 10/11/4) until a reset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VtSettings {
    pub foreground: Rgb,
    pub background: Rgb,
    /// `None` draws the cursor in the text color.
    pub cursor: Option<Rgb>,
    /// Entries replaced in libghostty's built-in 256-color palette.
    pub palette: Vec<(u8, Rgb)>,
    pub cursor_style: CursorShape,
    pub cursor_blink: bool,
    /// A finished drag or click selection goes to the clipboard.
    pub copy_on_select: bool,
    /// Programs may set the clipboard (OSC 52, iTerm2's OSC 1337 Copy).
    /// Reading it is never allowed.
    pub clipboard_write: bool,
}

impl Default for VtSettings {
    fn default() -> Self {
        Self {
            foreground: Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
            background: Rgb { r: 0, g: 0, b: 0 },
            cursor: None,
            palette: Vec::new(),
            cursor_style: CursorShape::Block,
            // Windows convention: the caret blinks unless an app says otherwise.
            cursor_blink: true,
            copy_on_select: true,
            clipboard_write: true,
        }
    }
}

/// Side effects of the bytes fed in since the last [`VtCore::take_effects`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// Replies the terminal wants written back to the PTY (DSR, DA, ...).
    pub pty_reply: Vec<u8>,
    /// New window title, if it changed.
    pub title: Option<String>,
    /// The working directory the shell reported (OSC 7, OSC 9;9), as a
    /// local path.
    pub cwd: Option<PathBuf>,
    /// The same report as reported, host and all.
    pub location: Option<Location>,
    pub bell: bool,
    /// The last progress report (OSC 9;4) since the previous take.
    pub progress: Option<Progress>,
    /// User variables set (OSC 1337 SetUserVar), in order.
    pub vars: Vec<(String, String)>,
    /// Text a program put on the clipboard (OSC 52), the last since the
    /// previous take.
    pub clipboard: Option<String>,
    /// Desktop notifications asked for (OSC 9, OSC 777).
    pub notifications: Vec<Notification>,
    /// Commands that finished (OSC 133;D), with their exit status.
    pub finished: Vec<Option<i32>>,
}

/// Where a shell says it is. Over SSH that's the remote host and a path
/// there, which means nothing locally but is worth showing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Location {
    /// From OSC 7's `file://host/...`; none for OSC 9;9 or an empty host.
    pub host: Option<String>,
    pub path: String,
}

impl Location {
    fn from_report(report: &str) -> Self {
        let Some(rest) = report.strip_prefix("file://") else {
            return Self {
                host: None,
                path: report.to_owned(),
            };
        };
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        Self {
            host: Some(host.to_owned()).filter(|h| !h.is_empty()),
            path: percent_decode(path).unwrap_or_else(|| path.to_owned()),
        }
    }
}

#[derive(Default)]
struct Pending {
    pty_reply: Vec<u8>,
    title_changed: bool,
    pwd_changed: bool,
    bell: bool,
    clipboard: Option<String>,
}

/// What queries are answered with: the terminal's callbacks read it, and
/// [`VtCore::configure`] and [`VtCore::resize`] keep it current.
#[derive(Clone, Copy, Debug, Default)]
struct Answers {
    /// For XTWINOPS size reports (CSI 14/16/18 t).
    size: SizeReportSize,
    /// For the color scheme report (CSI ? 996 n): the background is dark.
    dark: bool,
    clipboard_write: bool,
}

/// Lines per wheel notch, like Windows' default.
const WHEEL_LINES: i32 = 3;
/// What ends a word for double-click selection: Ghostty's default set
/// without `:`, so `C:\Users\me`, `src/main.rs:12:5` and URLs select whole.
const WORD_BOUNDARIES: &[char] = &[
    '\0', ' ', '\t', '\'', '"', '│', '`', '|', ';', ',', '(', ')', '[', ']', '{', '}', '<', '>',
    '$',
];
/// Longest gap between clicks of a double or triple click (Windows' default).
const REPEAT_CLICK: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    Move,
}

/// A mouse event at `(x, y)` pixels from the grid's top-left corner
/// (padding excluded). Outside the grid while dragging past its edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseInput {
    pub action: MouseAction,
    pub x: f64,
    pub y: f64,
    pub mods: key::Mods,
}

/// What a mouse event asks of the app, beyond updating the selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MouseOutcome {
    Nothing,
    /// Report to the app, which tracks the mouse.
    Pty(Vec<u8>),
    /// Put this on the clipboard: a finished selection (copy on select),
    /// or a right click on one.
    Copy(String),
    /// Right click with nothing selected: paste the clipboard.
    Paste,
}

pub struct VtCore {
    term: Terminal<'static, 'static>,
    render: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    pending: Rc<RefCell<Pending>>,
    answers: Rc<std::cell::Cell<Answers>>,
    keys: key::Encoder<'static>,
    key_event: key::Event<'static>,
    mouse: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    /// Pixel size of a cell, as last given to [`VtCore::resize`].
    cell_size: (u32, u32),
    /// Current viewport; only dirty rows are re-read from the render state.
    grid: Snapshot,
    /// Rows replaced while a snapshot still held them, and rows nobody
    /// holds any more: reused for changed rows instead of allocating, so a
    /// scrolling flood writes into warm buffers.
    rows_out: Vec<Arc<Line>>,
    rows_free: Vec<Arc<Line>>,
    graphemes: Vec<char>,
    /// Scratch for OSC 8 URIs.
    uri_buf: Vec<u8>,
    /// Kitty graphics images copied out so far.
    pictures: images::Pictures,
    /// The last write ended in ESC, so a leading `c` completes a reset.
    trailing_esc: bool,
    mouse_input: MouseState,
    /// libghostty's palette, which settings override entry by entry.
    builtin_palette: Palette,
    copy_on_select: bool,
    osc: osc::Scanner,
    /// The find bar's search, while it has a query.
    search: Option<Search>,
}

/// Selection gestures and mouse buttons. libghostty's gesture state turns
/// presses and drags into cell, word (double click), line (triple click)
/// and block (Alt) selections.
struct MouseState {
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    tick: AutoscrollTickEvent<'static>,
    /// Clock for telling double clicks from two clicks.
    epoch: Instant,
    /// A left-button selection drag is in progress.
    selecting: bool,
    /// The last drag position and whether it was a block selection, for
    /// autoscroll ticks.
    last_drag: (f64, f64, bool),
    /// Buttons held, for motion reports.
    held: Vec<MouseButton>,
}

impl MouseState {
    fn new() -> Result<Self, Error> {
        let mut press = PressEvent::new()?;
        press.set_word_boundary_codepoints(WORD_BOUNDARIES)?;
        let mut drag = DragEvent::new()?;
        drag.set_word_boundary_codepoints(WORD_BOUNDARIES)?;
        let mut tick = AutoscrollTickEvent::new()?;
        tick.set_word_boundary_codepoints(WORD_BOUNDARIES)?;
        Ok(Self {
            gesture: Gesture::new()?,
            press,
            drag,
            release: ReleaseEvent::new()?,
            tick,
            epoch: Instant::now(),
            selecting: false,
            last_drag: (0.0, 0.0, false),
            held: Vec::new(),
        })
    }
}

impl VtCore {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, Error> {
        let mut term = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback,
        })?;
        images::enable(&mut term)?;
        let pending = Rc::new(RefCell::new(Pending::default()));
        let answers = Rc::new(std::cell::Cell::new(Answers {
            size: SizeReportSize {
                rows,
                columns: cols,
                cell_width: 1,
                cell_height: 1,
            },
            dark: true,
            clipboard_write: true,
        }));
        term.on_pty_write({
            let pending = pending.clone();
            move |_, data| pending.borrow_mut().pty_reply.extend_from_slice(data)
        })?
        .on_title_changed({
            let pending = pending.clone();
            move |_| pending.borrow_mut().title_changed = true
        })?
        .on_pwd_changed({
            let pending = pending.clone();
            move |_| pending.borrow_mut().pwd_changed = true
        })?
        .on_bell({
            let pending = pending.clone();
            move |_| pending.borrow_mut().bell = true
        })?
        .on_clipboard_write({
            let (pending, answers) = (pending.clone(), answers.clone());
            move |_, write| {
                if !answers.get().clipboard_write {
                    return Err(ClipboardWriteError::Denied);
                }
                // The primary selection is X11's; only the clipboard is shared.
                if write.location() != ClipboardLocation::Standard {
                    return Err(ClipboardWriteError::Unsupported);
                }
                let text = write
                    .contents()
                    .find(|c| c.mime.starts_with("text/plain"))
                    .map(|c| c.data)
                    .filter(|text| !text.is_empty())
                    .ok_or(ClipboardWriteError::Unsupported)?;
                pending.borrow_mut().clipboard = Some(text.to_owned());
                Ok(())
            }
        })?
        .on_xtversion(|_| Some(concat!("jet ", env!("CARGO_PKG_VERSION"))))?
        .on_size({
            let answers = answers.clone();
            move |_| Some(answers.get().size)
        })?
        .on_color_scheme({
            let answers = answers.clone();
            move |_| {
                Some(if answers.get().dark {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                })
            }
        })?
        .set_mode(Mode::GRAPHEME_CLUSTER, true)?;
        let builtin_palette = term.default_color_palette()?;
        let mut vt = Self {
            term,
            render: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            pending,
            answers,
            keys: key::Encoder::new()?,
            key_event: key::Event::new()?,
            mouse: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            cell_size: (1, 1),
            grid: Snapshot::default(),
            rows_out: Vec::new(),
            rows_free: Vec::new(),
            graphemes: Vec::new(),
            uri_buf: vec![0; 256],
            pictures: images::Pictures::default(),
            trailing_esc: false,
            mouse_input: MouseState::new()?,
            builtin_palette,
            copy_on_select: true,
            osc: osc::Scanner::default(),
            search: None,
        };
        vt.configure(&VtSettings::default())?;
        Ok(vt)
    }

    /// Apply new settings. libghostty marks the screen dirty, so the next
    /// snapshot redraws everything.
    pub fn configure(&mut self, settings: &VtSettings) -> Result<(), Error> {
        let mut palette = self.builtin_palette;
        for &(i, c) in &settings.palette {
            palette.0[usize::from(i)] = c.into();
        }
        let style = match settings.cursor_style {
            CursorShape::Block => CursorStyle::Block,
            CursorShape::BlockHollow => CursorStyle::BlockHollow,
            CursorShape::Bar => CursorStyle::Bar,
            CursorShape::Underline => CursorStyle::Underline,
        };
        self.term
            // libghostty ignores either unless both are set.
            .set_default_fg_color(Some(settings.foreground.into()))?
            .set_default_bg_color(Some(settings.background.into()))?
            .set_default_cursor_color(settings.cursor.map(Into::into))?
            .set_default_color_palette(Some(palette))?
            .set_default_cursor_style(Some(style))?
            .set_default_cursor_blink(Some(settings.cursor_blink))?;
        self.copy_on_select = settings.copy_on_select;
        let Rgb { r, g, b } = settings.background;
        let luma = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
        self.answers.set(Answers {
            dark: luma < 128.0,
            clipboard_write: settings.clipboard_write,
            ..self.answers.get()
        });
        Ok(())
    }

    /// Mode 2027 is on by default here: a ZWJ sequence, flag or VS16 emoji
    /// is one cluster with one width, not a wide cell per codepoint.
    /// libghostty's default is off and a full reset (RIS, `ESC c`) restores
    /// it, so writes are split after each reset to turn it back on before
    /// the rest is parsed. Apps can still switch it off.
    pub fn write(&mut self, mut bytes: &[u8]) {
        if self.trailing_esc && bytes.first() == Some(&b'c') {
            self.term.vt_write(&bytes[..1]);
            self.cluster_graphemes();
            bytes = &bytes[1..];
        }
        // Hop between ESCs (a vectorized search) looking for `ESC c`.
        let mut from = 0;
        while let Some(i) = bytes[from..]
            .iter()
            .position(|&b| b == 0x1b)
            .map(|i| from + i)
        {
            if bytes.get(i + 1) != Some(&b'c') {
                from = i + 1;
                continue;
            }
            self.term.vt_write(&bytes[..i + 2]);
            self.cluster_graphemes();
            bytes = &bytes[i + 2..];
            from = 0;
        }
        self.term.vt_write(bytes);
        self.trailing_esc = bytes.last() == Some(&0x1b);
        self.osc.feed(bytes);
        if let Some(search) = &mut self.search {
            search.stale = true;
        }
    }

    /// Search the screen and history for `query` (smart case: it only
    /// matches case when it has capitals), showing the match nearest the
    /// viewport's bottom. Empty ends the search.
    pub fn search(&mut self, query: &str) -> Result<(), Error> {
        self.search = None;
        if !query.is_empty() {
            let search = Search::new(&self.term, query)?;
            search.reveal(&mut self.term)?;
            self.search = Some(search);
        }
        Ok(())
    }

    /// Go `n` matches up (negative, older) or down, wrapping around.
    pub fn search_step(&mut self, n: isize) -> Result<(), Error> {
        if let Some(search) = &mut self.search {
            if search.stale {
                search.refresh(&self.term)?;
            }
            search.step(&mut self.term, n)?;
        }
        Ok(())
    }

    fn cluster_graphemes(&mut self) {
        // Only fails for an unknown mode, which this isn't.
        let _ = self.term.set_mode(Mode::GRAPHEME_CLUSTER, true);
    }

    pub fn take_effects(&mut self) -> Result<Effects, Error> {
        let pending = std::mem::take(&mut *self.pending.borrow_mut());
        let title = if pending.title_changed {
            Some(self.term.title()?.to_owned())
        } else {
            None
        };
        let (cwd, location) = if pending.pwd_changed {
            let report = self.term.pwd()?;
            (
                cwd_from_report(report, cfg!(windows)),
                Some(Location::from_report(report)),
            )
        } else {
            (None, None)
        };
        Ok(Effects {
            pty_reply: pending.pty_reply,
            title,
            cwd,
            location,
            bell: pending.bell,
            progress: self.osc.progress.take(),
            vars: std::mem::take(&mut self.osc.vars),
            notifications: std::mem::take(&mut self.osc.notifications),
            finished: std::mem::take(&mut self.osc.finished),
            clipboard: pending.clipboard,
        })
    }

    /// Bytes to send to the PTY for a key event; empty if it encodes to
    /// nothing (e.g. a lone modifier).
    pub fn encode_key(&mut self, input: &KeyInput) -> Result<Vec<u8>, Error> {
        let event = &mut self.key_event;
        event
            .set_key(input.key)
            .set_action(input.action)
            .set_mods(input.mods)
            .set_utf8(input.text.clone());
        if let Some(c) = input.unshifted {
            event.set_unshifted_codepoint(c);
        }
        let mut out = Vec::new();
        self.keys
            .set_options_from_terminal(&self.term)
            .encode_to_vec(event, &mut out)?;
        if !out.is_empty() {
            // Typing jumps back to the live screen and drops the selection.
            self.term.scroll_viewport(ScrollViewport::Bottom);
            self.term.set_selection(None)?;
        }
        Ok(out)
    }

    /// Bytes for pasting `text`: bracketed when the app asked for it (so a
    /// paste can't fake the end marker), otherwise with newlines sent as
    /// Enter like typing would.
    pub fn encode_paste(&mut self, text: &str) -> Result<Vec<u8>, Error> {
        self.term.scroll_viewport(ScrollViewport::Bottom);
        self.term.set_selection(None)?;
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.term.mode(Mode::BRACKETED_PASTE)? {
            let inner = text.replace("\x1b[201~", "");
            Ok([b"\x1b[200~", inner.as_bytes(), b"\x1b[201~"].concat())
        } else {
            Ok(text.into_bytes())
        }
    }

    /// Scroll so the `n`th shell prompt (marked with OSC 133) above
    /// (negative) or below the viewport's top line is at the top. Stays put
    /// when there are fewer than that.
    pub fn jump_to_prompt(&mut self, n: isize) -> Result<(), Error> {
        let bar = self.term.scrollbar()?;
        let term = &self.term;
        let prompt = |y: &u64| {
            let row = u32::try_from(*y).ok().and_then(|y| {
                let at = term
                    .grid_ref(Point::Screen(PointCoordinate { x: 0, y }))
                    .ok()?;
                at.row().ok()?.semantic_prompt().ok()
            });
            row == Some(RowSemanticPrompt::Prompt)
        };
        let skip = n.unsigned_abs().saturating_sub(1);
        let target = match n.signum() {
            -1 => (0..bar.offset).rev().filter(prompt).nth(skip),
            1 => (bar.offset + 1..bar.total).filter(prompt).nth(skip),
            _ => None,
        };
        if let Some(y) = target.and_then(|y| usize::try_from(y).ok()) {
            self.term.scroll_viewport(ScrollViewport::Row(y));
        }
        Ok(())
    }

    /// The wheel turned `notches` (negative is up) over cell `(col, row)`.
    /// Apps that track the mouse get a wheel event per notch; full-screen
    /// apps on the alternate screen get arrow keys (what Ghostty and xterm's
    /// alternate scroll mode do); otherwise the viewport scrolls history.
    pub fn wheel(
        &mut self,
        notches: i32,
        (col, row): (u16, u16),
        mods: key::Mods,
    ) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let lines = notches.saturating_mul(WHEEL_LINES);
        if self.term.is_mouse_tracking()? {
            let (cw, ch) = self.cell_size;
            let button = if notches < 0 {
                mouse::Button::Four
            } else {
                mouse::Button::Five
            };
            let center = (
                f64::from(u32::from(col) * cw + cw / 2),
                f64::from(u32::from(row) * ch + ch / 2),
            );
            for _ in 0..notches.unsigned_abs() {
                out.extend(self.encode_mouse(mouse::Action::Press, Some(button), center, mods)?);
            }
        } else if self.term.active_screen()? == Screen::Alternate {
            let key = if lines < 0 {
                key::Key::ArrowUp
            } else {
                key::Key::ArrowDown
            };
            let input = KeyInput {
                key,
                action: key::Action::Press,
                mods: key::Mods::empty(),
                text: None,
                unshifted: None,
            };
            for _ in 0..lines.unsigned_abs() {
                out.extend(self.encode_key(&input)?);
            }
        } else {
            self.term
                .scroll_viewport(ScrollViewport::Delta(lines as isize));
        }
        Ok(out)
    }

    /// A mouse button or move. Apps tracking the mouse get it reported,
    /// unless Shift is held, which always selects. Otherwise the left button
    /// selects, copying the selection when released (it stays highlighted),
    /// and the right copies the selection, or pastes without one.
    pub fn mouse(&mut self, input: MouseInput) -> Result<MouseOutcome, Error> {
        let tracking = self.term.is_mouse_tracking()? && !input.mods.contains(key::Mods::SHIFT);
        let pos = (input.x, input.y);
        let report = |vt: &mut Self, action, button| {
            vt.encode_mouse(action, button, pos, input.mods)
                .map(MouseOutcome::Pty)
        };
        match input.action {
            MouseAction::Press(button) => {
                self.mouse_input.held.retain(|&b| b != button);
                self.mouse_input.held.push(button);
                if tracking && !self.mouse_input.selecting {
                    return report(self, mouse::Action::Press, Some(vt_button(button)));
                }
                match button {
                    MouseButton::Left => {
                        self.mouse_input.selecting = true;
                        self.select_press(pos)?;
                        Ok(MouseOutcome::Nothing)
                    }
                    MouseButton::Right => Ok(match self.copy_selection()? {
                        Some(text) => MouseOutcome::Copy(text),
                        None => MouseOutcome::Paste,
                    }),
                    MouseButton::Middle => Ok(MouseOutcome::Nothing),
                }
            }
            MouseAction::Release(button) => {
                self.mouse_input.held.retain(|&b| b != button);
                if button == MouseButton::Left && self.mouse_input.selecting {
                    self.mouse_input.selecting = false;
                    let r = cell_ref(&self.term, self.cell_size, pos)?;
                    let m = &mut self.mouse_input;
                    m.release.apply(&mut m.gesture, &self.term, Some(r))?;
                    let text = if self.copy_on_select {
                        self.selection_text()?
                    } else {
                        None
                    };
                    Ok(text.map_or(MouseOutcome::Nothing, MouseOutcome::Copy))
                } else if tracking {
                    report(self, mouse::Action::Release, Some(vt_button(button)))
                } else {
                    Ok(MouseOutcome::Nothing)
                }
            }
            MouseAction::Move if self.mouse_input.selecting => {
                let block = input.mods.contains(key::Mods::ALT);
                self.mouse_input.last_drag = (input.x, input.y, block);
                let geometry = self.geometry()?;
                let r = cell_ref(&self.term, self.cell_size, pos)?;
                let m = &mut self.mouse_input;
                m.drag
                    .set_position(input.x, input.y)?
                    .set_rectangle(block)?;
                let selection = m.drag.apply(&mut m.gesture, &self.term, r, geometry)?;
                self.term.set_selection(selection.as_ref())?;
                Ok(MouseOutcome::Nothing)
            }
            MouseAction::Move if tracking => {
                let held = self.mouse_input.held.first().copied().map(vt_button);
                report(self, mouse::Action::Motion, held)
            }
            MouseAction::Move => Ok(MouseOutcome::Nothing),
        }
    }

    /// While a selection drag is held past the top or bottom edge: scroll
    /// one step and extend the selection. False when there is nothing to
    /// scroll, so the caller can stop ticking.
    pub fn autoscroll_tick(&mut self) -> Result<bool, Error> {
        let m = &self.mouse_input;
        if !m.selecting || m.gesture.autoscroll(&self.term)? == Autoscroll::None {
            return Ok(false);
        }
        let (x, y, block) = m.last_drag;
        let geometry = self.geometry()?;
        let (col, row) = cell_at(&self.term, self.cell_size, (x, y))?;
        let m = &mut self.mouse_input;
        m.tick.set_position(x, y)?.set_rectangle(block)?;
        let viewport = PointCoordinate { x: col, y: row };
        let selection = m
            .tick
            .apply(&mut m.gesture, &self.term, viewport, geometry)?;
        self.term.set_selection(selection.as_ref())?;
        Ok(true)
    }

    /// The selection as plain text (soft wraps joined, trailing blanks
    /// trimmed), clearing it. `None` without a selection.
    pub fn copy_selection(&mut self) -> Result<Option<String>, Error> {
        let text = self.selection_text()?;
        if text.is_some() {
            self.term.set_selection(None)?;
        }
        Ok(text)
    }

    /// The text of the last command's output, as shells mark it with
    /// OSC 133: the output under the newest output text on screen or in
    /// history. `None` without one.
    pub fn last_command_output(&mut self) -> Result<Option<String>, Error> {
        let bar = self.term.scrollbar()?;
        for y in (0..bar.total).rev() {
            let Ok(y) = u32::try_from(y) else { continue };
            let at = self
                .term
                .grid_ref(Point::Screen(PointCoordinate { x: 0, y }))?;
            let cell = at.cell()?;
            if cell.semantic_content()? != CellSemanticContent::Output || !cell.has_text()? {
                continue;
            }
            let Some(output) = self.term.select_output(at)? else {
                return Ok(None);
            };
            self.term.set_selection(Some(&output))?;
            let text = self.selection_text();
            self.term.set_selection(None)?;
            return text;
        }
        Ok(None)
    }

    /// The selection as copied, leaving it in place.
    fn selection_text(&self) -> Result<Option<String>, Error> {
        let options = selection::FormatOptions::new()
            .with_emit_format(Format::Plain)
            .with_unwrap(true)
            .with_trim(true);
        let bytes = self.term.format_selection_alloc(None, options)?;
        Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// Bytes reporting focus to the app, if it asked for them (mode 1004).
    pub fn focus(&mut self, focused: bool) -> Result<Vec<u8>, Error> {
        if !self.term.mode(Mode::FOCUS_EVENT)? {
            return Ok(Vec::new());
        }
        let event = if focused {
            focus::Event::Gained
        } else {
            focus::Event::Lost
        };
        let mut buf = [0u8; 8];
        let n = event.encode(&mut buf)?;
        Ok(buf.get(..n).unwrap_or_default().to_vec())
    }

    fn select_press(&mut self, (x, y): (f64, f64)) -> Result<(), Error> {
        let r = cell_ref(&self.term, self.cell_size, (x, y))?;
        let m = &mut self.mouse_input;
        m.press
            .set_position(x, y)?
            .set_time(click_time(m.epoch))?
            .set_repeat_interval(REPEAT_CLICK)?
            .set_repeat_distance(f64::from(self.cell_size.0))?;
        let selection = m.press.apply(&mut m.gesture, &self.term, r)?;
        self.term.set_selection(selection.as_ref())?;
        Ok(())
    }

    fn geometry(&self) -> Result<Geometry, Error> {
        Ok(Geometry {
            columns: u32::from(self.term.cols()?.max(1)),
            cell_width: self.cell_size.0,
            padding_left: 0,
            screen_height: u32::from(self.term.rows()?.max(1)) * self.cell_size.1,
        })
    }

    fn encode_mouse(
        &mut self,
        action: mouse::Action,
        button: Option<mouse::Button>,
        (x, y): (f64, f64),
        mods: key::Mods,
    ) -> Result<Vec<u8>, Error> {
        let (cw, ch) = self.cell_size;
        self.mouse_event
            .set_action(action)
            .set_button(button)
            .set_mods(mods)
            .set_position(mouse::Position {
                x: x as f32,
                y: y as f32,
            });
        let size = mouse::EncoderSize {
            screen_width: u32::from(self.term.cols()?.max(1)) * cw,
            screen_height: u32::from(self.term.rows()?.max(1)) * ch,
            cell_width: cw,
            cell_height: ch,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let mut out = Vec::new();
        self.mouse
            .set_options_from_terminal(&self.term)
            .set_size(size)
            .set_any_button_pressed(!self.mouse_input.held.is_empty())
            .encode_to_vec(&self.mouse_event, &mut out)?;
        Ok(out)
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<(), Error> {
        self.term.resize(cols, rows, cell_width, cell_height)?;
        self.cell_size = (cell_width.max(1), cell_height.max(1));
        if let Some(search) = &mut self.search {
            search.stale = true;
        }
        self.answers.set(Answers {
            size: SizeReportSize {
                rows,
                columns: cols,
                cell_width: self.cell_size.0,
                cell_height: self.cell_size.1,
            },
            ..self.answers.get()
        });
        Ok(())
    }

    /// A program is drawing a synchronized update (mode 2026): what's on
    /// screen is half a frame.
    pub fn synchronized(&self) -> Result<bool, Error> {
        Ok(self.term.mode(Mode::SYNC_OUTPUT)?)
    }

    /// The active screen, scrollback included, as plain text.
    pub fn plain_text(&self) -> Result<String, Error> {
        let opts = FormatterOptions::new()
            .with_format(Format::Plain)
            .with_trim(true);
        let bytes = Formatter::new(&self.term, opts)?.format_alloc(None)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A snapshot of the viewport if anything changed since the last call.
    /// Rows that didn't change are shared with the previous snapshot.
    pub fn snapshot(&mut self) -> Result<Option<Snapshot>, Error> {
        let Self {
            term,
            render,
            rows,
            cells,
            grid,
            graphemes,
            uri_buf,
            pictures,
            cell_size,
            search,
            rows_out,
            rows_free,
            ..
        } = self;
        let images = pictures.placements(term, *cell_size)?;
        let found = match search {
            Some(search) => {
                if search.stale {
                    search.refresh(term)?;
                }
                Some(search.matches(term)?)
            }
            None => None,
        };
        let state = render.update(term)?;
        let dirty = state.dirty()?;
        let (cols, row_count) = (state.cols()?, state.rows()?);
        let full = dirty == Dirty::Full || (cols, row_count) != (grid.cols, grid.rows);
        if (cols, row_count) != (grid.cols, grid.rows) {
            grid.cols = cols;
            grid.rows = row_count;
            grid.lines = vec![Arc::default(); usize::from(row_count)];
        }

        let palette = state.colors()?;
        let colors = Colors {
            foreground: palette.foreground.into(),
            background: palette.background.into(),
            cursor: palette.cursor.map(Into::into),
        };
        let cursor = match state.cursor_viewport()? {
            Some(pos) if state.cursor_visible()? => Some(Cursor {
                x: pos.x,
                y: pos.y,
                shape: match state.cursor_visual_style()? {
                    CursorVisualStyle::Bar => CursorShape::Bar,
                    CursorVisualStyle::Underline => CursorShape::Underline,
                    CursorVisualStyle::BlockHollow => CursorShape::BlockHollow,
                    _ => CursorShape::Block,
                },
                blinking: state.cursor_blinking()?,
            }),
            _ => None,
        };
        // Dirty tracking covers rows only: hiding, showing or moving the
        // cursor alone leaves the state clean.
        if dirty == Dirty::Clean
            && cursor == grid.cursor
            && colors == grid.colors
            && images == grid.images
            && found == grid.search
        {
            return Ok(None);
        }
        grid.colors = colors;
        grid.cursor = cursor;
        grid.images = images;
        grid.search = found;

        reclaim(rows_out, rows_free, usize::from(row_count));
        let mut row_iter = rows.update(&state)?;
        let mut y = 0;
        while let Some(row) = row_iter.next() {
            let Some(line) = grid.lines.get_mut(y) else {
                break;
            };
            if full || row.dirty()? {
                // A new Arc unless nobody else holds this line, so earlier
                // snapshots keep theirs and `Arc::ptr_eq` means unchanged.
                if Arc::get_mut(line).is_none() {
                    let fresh = rows_free
                        .pop()
                        .unwrap_or_else(|| Arc::new(Line::with_capacity(usize::from(cols))));
                    rows_out.push(std::mem::replace(line, fresh));
                }
                let line = Arc::make_mut(line);
                line.clear();
                let raw_row = row.raw_row()?;
                line.wrapped = raw_row.is_wrapped()?;
                // Per-row flags spare plain rows two queries a cell.
                let row_styled = raw_row.is_styled()?;
                let row_links = raw_row.has_hyperlink()?;
                // One render-state query per cell; the rest reads the raw
                // cell value. Unstyled text needs nothing else, which keeps
                // full-screen rebuilds (every scrolled frame) cheap.
                let selected = row.selection()?;
                let mut cell_iter = cells.update(row)?;
                let mut x = 0u16;
                while let Some(cell) = cell_iter.next() {
                    let raw = cell.raw_cell()?;
                    let tag = raw.content_tag()?;
                    let styled = row_styled && raw.has_styling()?;
                    let mut out = Cell {
                        width: match raw.wide()? {
                            CellWide::Wide => Width::Wide,
                            CellWide::SpacerTail | CellWide::SpacerHead => Width::Spacer,
                            CellWide::Narrow => Width::Narrow,
                        },
                        ..Cell::default()
                    };
                    if styled
                        || matches!(
                            tag,
                            CellContentTag::BgColorPalette | CellContentTag::BgColorRgb
                        )
                    {
                        out.bg = cell.bg_color()?.map(Into::into);
                    }
                    if styled {
                        let style = cell.style()?;
                        let resolve = |color| match color {
                            StyleColor::None => None,
                            StyleColor::Palette(i) => {
                                palette.palette.get(usize::from(i.0)).map(|&c| c.into())
                            }
                            StyleColor::Rgb(c) => Some(c.into()),
                        };
                        out.fg = resolve(style.fg_color);
                        out.underline_color = resolve(style.underline_color);
                        let flags = &mut out.flags;
                        flags.set(CellFlags::BOLD, style.bold);
                        flags.set(CellFlags::ITALIC, style.italic);
                        flags.set(CellFlags::FAINT, style.faint);
                        flags.set(CellFlags::BLINK, style.blink);
                        flags.set(CellFlags::INVERSE, style.inverse);
                        flags.set(CellFlags::INVISIBLE, style.invisible);
                        flags.set(CellFlags::STRIKETHROUGH, style.strikethrough);
                        flags.set(CellFlags::OVERLINE, style.overline);
                        out.underline = match style.underline {
                            VtUnderline::None => Underline::None,
                            VtUnderline::Double => Underline::Double,
                            VtUnderline::Curly => Underline::Curly,
                            VtUnderline::Dotted => Underline::Dotted,
                            VtUnderline::Dashed => Underline::Dashed,
                            _ => Underline::Single,
                        };
                    }
                    out.flags.set(
                        CellFlags::SELECTED,
                        selected.is_some_and(|s| (s.start_x..=s.end_x).contains(&x)),
                    );
                    if row_links && raw.has_hyperlink()? {
                        let at = Point::Viewport(PointCoordinate {
                            x,
                            y: u32::try_from(y).unwrap_or(u32::MAX),
                        });
                        if let Some(uri) = hyperlink_uri(&term.grid_ref(at)?, uri_buf)? {
                            line.link(x, uri);
                        }
                    }
                    x = x.saturating_add(1);
                    match tag {
                        CellContentTag::Codepoint => {
                            let c = char::from_u32(raw.codepoint()?).filter(|&c| c != '\0');
                            line.push(out, c);
                        }
                        CellContentTag::CodepointGrapheme => {
                            graphemes.resize(cell.graphemes_len()?, '\0');
                            cell.graphemes_buf(graphemes)?;
                            line.push(out, graphemes.iter().copied());
                        }
                        CellContentTag::BgColorPalette | CellContentTag::BgColorRgb => {
                            line.push(out, None);
                        }
                    }
                }
                row.set_dirty(false)?;
            }
            y += 1;
        }
        state.set_dirty(Dirty::Clean)?;

        Ok(Some(grid.clone()))
    }
}

/// Move the rows that went out in snapshots and have since been dropped
/// everywhere else to `free`. Keeps a couple of screens' worth: one in a
/// snapshot being drawn, one being built.
fn reclaim(out: &mut Vec<Arc<Line>>, free: &mut Vec<Arc<Line>>, rows: usize) {
    let mut i = 0;
    while let Some(line) = out.get_mut(i) {
        if Arc::get_mut(line).is_some() {
            free.push(out.swap_remove(i));
        } else {
            i += 1;
        }
    }
    // Rows held for long (a scrolled-back view) aren't waited on forever.
    if out.len() > 4 * rows {
        out.drain(..out.len() - 4 * rows);
    }
    free.truncate(2 * rows);
}

/// The OSC 8 URI of the cell at `at`, growing `buf` to fit; `None` for
/// none or one that isn't UTF-8.
fn hyperlink_uri<'b>(at: &GridRef<'_>, buf: &'b mut Vec<u8>) -> Result<Option<&'b str>, Error> {
    let len = match at.hyperlink_uri(buf) {
        Err(libghostty_vt::Error::OutOfSpace { required }) if required > buf.len() => {
            buf.resize(required, 0);
            at.hyperlink_uri(buf)?
        }
        other => other?,
    };
    Ok(buf
        .get(..len)
        .and_then(|uri| std::str::from_utf8(uri).ok())
        .filter(|uri| !uri.is_empty()))
}

/// A local path from what a shell reports as its directory: a
/// `file://host/path` URL (OSC 7, percent-encoded) or a plain path (OSC
/// 9;9). On `windows`, `/C:/x` and MSYS's `/c/x` become `C:/x`.
fn cwd_from_report(report: &str, windows: bool) -> Option<PathBuf> {
    let path = match report.strip_prefix("file://") {
        // The host is ours; what follows it is the path.
        Some(rest) => percent_decode(rest.get(rest.find('/')?..)?)?,
        None => report.to_owned(),
    };
    if !windows {
        return Some(PathBuf::from(path)).filter(|p| p.is_absolute());
    }
    let bytes = path.as_bytes();
    let path = match bytes {
        // `/C:/x`
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => path.get(1..)?.to_owned(),
        // `/c/x` or `/c`
        [b'/', drive, rest @ ..]
            if drive.is_ascii_alphabetic() && rest.first().is_none_or(|&b| b == b'/') =>
        {
            format!(
                "{}:{}",
                char::from(drive.to_ascii_uppercase()),
                path.get(2..)?
            )
        }
        _ => path,
    };
    // Only drive (`C:\\x`) and UNC (`\\\\host\\x`) paths mean something to a
    // Windows shell; `/tmp` from MSYS doesn't.
    let local = match path.as_bytes() {
        [drive, b':', rest @ ..] => {
            drive.is_ascii_alphabetic() && rest.first().is_none_or(|&b| b == b'/' || b == b'\\')
        }
        bytes => bytes.starts_with(b"\\\\"),
    };
    local.then(|| PathBuf::from(path))
}

fn percent_decode(text: &str) -> Option<String> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

/// A press time on the clock libghostty's gestures compare. Its Windows
/// build stores the value unconverted as a `std.time.Instant`, which there
/// is raw QueryPerformanceCounter ticks (`instantFromNs` in
/// `c/selection_gesture.zig`); nanoseconds made double clicks need to land
/// within ~5 ms. So Windows gets the counter itself, elsewhere nanoseconds.
fn click_time(epoch: Instant) -> Duration {
    #[cfg(windows)]
    {
        let mut ticks = 0i64;
        // Never fails on supported Windows; fall through if it somehow does.
        if unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut ticks) }
            .is_ok()
        {
            return Duration::from_nanos(ticks.unsigned_abs());
        }
    }
    epoch.elapsed()
}

fn vt_button(button: MouseButton) -> mouse::Button {
    match button {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Right => mouse::Button::Right,
    }
}

/// The viewport cell under `(x, y)`, clamped onto the grid.
fn cell_at(
    term: &Terminal<'_, '_>,
    (cw, ch): (u32, u32),
    (x, y): (f64, f64),
) -> Result<(u16, u32), Error> {
    let clamp = |px: f64, cell: u32, count: u16| {
        (px / f64::from(cell.max(1)))
            .floor()
            .clamp(0.0, f64::from(count.saturating_sub(1)))
    };
    Ok((
        clamp(x, cw, term.cols()?) as u16,
        clamp(y, ch, term.rows()?) as u32,
    ))
}

fn cell_ref<'t>(
    term: &'t Terminal<'_, '_>,
    cell_size: (u32, u32),
    pos: (f64, f64),
) -> Result<GridRef<'t>, Error> {
    let (x, y) = cell_at(term, cell_size, pos)?;
    Ok(term.grid_ref(Point::Viewport(PointCoordinate { x, y }))?)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::snapshot::{CellFlags, Rgb, Width};
    use crate::vt::{KeyInput, VtCore};
    use libghostty_vt::key::{Action, Key, Mods};

    fn press(key: Key, mods: Mods, text: Option<&str>) -> KeyInput {
        KeyInput {
            key,
            action: Action::Press,
            mods,
            text: text.map(str::to_owned),
            unshifted: text.and_then(|t| t.chars().next()),
        }
    }

    #[test]
    fn keys_encode_against_current_modes() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 3, 0)?;
        let up = press(Key::ArrowUp, Mods::empty(), None);
        assert_eq!(vt.encode_key(&up)?, b"\x1b[A");
        // DECCKM switches arrows to application mode.
        vt.write(b"\x1b[?1h");
        assert_eq!(vt.encode_key(&up)?, b"\x1bOA");

        assert_eq!(
            vt.encode_key(&press(Key::C, Mods::CTRL, Some("c")))?,
            b"\x03"
        );
        assert_eq!(vt.encode_key(&press(Key::A, Mods::SHIFT, Some("A")))?, b"A");
        assert_eq!(
            vt.encode_key(&press(Key::ShiftLeft, Mods::SHIFT, None))?,
            b""
        );
        Ok(())
    }

    #[test]
    fn snapshot_captures_text_styles_and_wide_cells() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 3, 0)?;
        vt.write(b"\x1b[1;38;2;255;0;0mhi\x1b[0m \xe4\xb8\xad!");
        let snap = vt.snapshot()?.ok_or("no snapshot")?;

        assert_eq!(snap.text(), "hi \u{4e2d}!");
        let line = &snap.lines[0];
        assert_eq!(line.cells[0].fg, Some(Rgb { r: 255, g: 0, b: 0 }));
        assert!(line.cells[0].flags.contains(CellFlags::BOLD));
        assert!(!line.cells[2].flags.contains(CellFlags::BOLD));
        assert_eq!(line.cells[3].width, Width::Wide);
        assert_eq!(line.cells[4].width, Width::Spacer);
        assert_eq!(line.cell_text(5), "!");
        assert_eq!(snap.cursor.map(|c| (c.x, c.y)), Some((6, 0)));
        Ok(())
    }

    #[test]
    fn snapshot_resolves_palette_and_erased_backgrounds() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut vt = VtCore::new(6, 2, 0)?;
        vt.configure(&VtSettings {
            palette: vec![(1, Rgb { r: 1, g: 2, b: 3 })],
            ..VtSettings::default()
        })?;
        // Red text, plain text, then a line erased in a 256-color background.
        vt.write(b"\x1b[31mr\x1b[0mp\r\n\x1b[48;5;1m\x1b[K\x1b[0m");
        let snap = vt.snapshot()?.ok_or("no snapshot")?;

        let red = Some(Rgb { r: 1, g: 2, b: 3 });
        let [top, erased] = &snap.lines[..] else {
            return Err("expected two lines".into());
        };
        assert_eq!((top.cells[0].fg, top.cell_text(0)), (red, "r"));
        assert_eq!((top.cells[1].fg, top.cells[1].bg), (None, None));
        assert_eq!(top.cell_text(1), "p");
        assert!(erased.cells.iter().all(|c| c.bg == red));
        assert_eq!(erased.text(), "");
        Ok(())
    }

    #[test]
    fn snapshot_shares_unchanged_rows() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 3, 0)?;
        vt.write(b"a\r\nb\r\nc");
        let first = vt.snapshot()?.ok_or("no snapshot")?;
        assert!(vt.snapshot()?.is_none(), "nothing changed");

        vt.write(b"\x1b[2;1Hx");
        let snap = vt.snapshot()?.ok_or("no snapshot")?;
        assert_eq!(snap.text(), "a\nx\nc");
        assert!(Arc::ptr_eq(&first.lines[0], &snap.lines[0]));
        assert!(!Arc::ptr_eq(&first.lines[1], &snap.lines[1]));
        assert_eq!(
            first.lines[1].text(),
            "b",
            "earlier snapshots keep their rows"
        );
        Ok(())
    }

    #[test]
    fn effects_report_title_bell_and_replies() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 3, 0)?;
        // OSC 2 title, BEL, and a cursor position report request.
        vt.write(b"\x1b]2;hello\x1b\\\x07\x1b[6n");
        let effects = vt.take_effects()?;
        assert_eq!(effects.title.as_deref(), Some("hello"));
        assert!(effects.bell);
        assert_eq!(effects.pty_reply, b"\x1b[1;1R");
        assert_eq!(vt.take_effects()?, Default::default());
        Ok(())
    }

    #[test]
    fn paste_brackets_only_when_asked() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 3, 0)?;
        assert_eq!(vt.encode_paste("a\r\nb\n")?, b"a\rb\r");
        vt.write(b"\x1b[?2004h");
        assert_eq!(
            vt.encode_paste("x\x1b[201~y")?,
            b"\x1b[200~xy\x1b[201~",
            "a pasted end marker is dropped"
        );
        Ok(())
    }

    #[test]
    fn wheel_scrolls_history_or_talks_to_the_app() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 2, 100)?;
        vt.write(b"one\r\ntwo\r\nthree");
        vt.snapshot()?;
        assert!(vt.wheel(-1, (0, 0), Mods::empty())?.is_empty());
        let snap = vt.snapshot()?.ok_or("scrolling changes the view")?;
        assert_eq!(snap.text(), "one\ntwo");

        // Typing jumps back down.
        vt.encode_key(&press(Key::A, Mods::empty(), Some("a")))?;
        assert_eq!(vt.snapshot()?.ok_or("view moved")?.text(), "two\nthree");

        vt.write(b"\x1b[?1049h");
        assert_eq!(vt.wheel(1, (0, 0), Mods::empty())?, b"\x1b[B".repeat(3));

        vt.write(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(vt.wheel(-1, (3, 1), Mods::empty())?, b"\x1b[<64;4;2M");
        Ok(())
    }

    #[test]
    fn cursor_only_changes_make_a_snapshot() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 3, 0)?;
        vt.write(b"PS> ");
        assert!(vt.snapshot()?.is_some());
        vt.write(b"\x1b[?25l");
        assert_eq!(vt.snapshot()?.ok_or("hide")?.cursor, None);
        vt.write(b"\x1b[?25h");
        let shown = vt.snapshot()?.ok_or("show")?.cursor;
        assert_eq!(shown.map(|c| (c.x, c.y)), Some((4, 0)));
        vt.write(b"\x1b[1;2H");
        let moved = vt.snapshot()?.ok_or("move")?.cursor;
        assert_eq!(moved.map(|c| (c.x, c.y)), Some((1, 0)));
        assert!(vt.snapshot()?.is_none(), "nothing changed");
        Ok(())
    }

    #[test]
    fn emoji_sequences_are_one_cluster() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 2, 0)?;
        // Family (ZWJ), flag (regional indicators), heart + VS16.
        vt.write(
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}|\u{1F1FA}\u{1F1F8}|\u{2764}\u{FE0F}|"
                .as_bytes(),
        );
        let snap = vt.snapshot()?.ok_or("no snapshot")?;
        let bars: Vec<_> = (0..20)
            .filter(|&x| snap.lines[0].cell_text(x) == "|")
            .collect();
        assert_eq!(bars, [2, 5, 8]);
        // A full reset (RIS) keeps clustering on, even split across reads.
        vt.write(b"\x1b");
        vt.write("c\u{1F1FA}\u{1F1F8}|".as_bytes());
        let snap = vt.snapshot()?.ok_or("no snapshot after reset")?;
        assert_eq!(snap.lines[0].cell_text(2), "|");
        Ok(())
    }

    use crate::vt::{MouseAction, MouseButton, MouseInput, MouseOutcome, VtSettings};

    /// 10x20 px cells; `at` is the pixel centre of a cell.
    fn mouse(action: MouseAction, (col, row): (u16, u16), mods: Mods) -> MouseInput {
        MouseInput {
            action,
            x: f64::from(col) * 10.0 + 5.0,
            y: f64::from(row) * 20.0 + 10.0,
            mods,
        }
    }

    fn selected(vt: &mut VtCore) -> Result<Vec<(usize, usize)>, Box<dyn std::error::Error>> {
        let snap = vt.snapshot()?.ok_or("selection made no snapshot")?;
        let mut cells = Vec::new();
        for (y, line) in snap.lines.iter().enumerate() {
            for (x, cell) in line.cells.iter().enumerate() {
                if cell.flags.contains(CellFlags::SELECTED) {
                    cells.push((x, y));
                }
            }
        }
        Ok(cells)
    }

    #[test]
    fn drag_selects_and_copy_takes_it() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 3, 0)?;
        vt.resize(20, 3, 10, 20)?;
        vt.write(b"hello world\r\nsecond line");
        vt.snapshot()?;
        let left = MouseButton::Left;
        vt.mouse(mouse(MouseAction::Press(left), (6, 0), Mods::empty()))?;
        vt.mouse(mouse(MouseAction::Move, (4, 1), Mods::empty()))?;
        // Copy on select: releasing copies and keeps the highlight.
        assert_eq!(
            vt.mouse(mouse(MouseAction::Release(left), (4, 1), Mods::empty()))?,
            MouseOutcome::Copy("world\nseco".into())
        );
        assert!(selected(&mut vt)?.contains(&(6, 0)));
        assert_eq!(
            vt.copy_selection()?.as_deref(),
            Some("world\nseco"),
            "the last cell needs the pointer past its middle"
        );
        assert_eq!(vt.copy_selection()?, None, "copying clears the selection");
        assert!(selected(&mut vt)?.is_empty());
        Ok(())
    }

    #[test]
    fn double_click_selects_a_word() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 2, 0)?;
        vt.resize(20, 2, 10, 20)?;
        vt.write(b"git status --short");
        let left = MouseButton::Left;
        for _ in 0..2 {
            vt.mouse(mouse(MouseAction::Press(left), (6, 0), Mods::empty()))?;
            vt.mouse(mouse(MouseAction::Release(left), (6, 0), Mods::empty()))?;
        }
        assert_eq!(vt.copy_selection()?.as_deref(), Some("status"));
        Ok(())
    }

    #[test]
    fn right_click_copies_or_pastes() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 2, 0)?;
        vt.resize(20, 2, 10, 20)?;
        vt.write(b"abc def");
        let (left, right) = (MouseButton::Left, MouseButton::Right);
        assert_eq!(
            vt.mouse(mouse(MouseAction::Press(right), (0, 0), Mods::empty()))?,
            MouseOutcome::Paste
        );
        for _ in 0..2 {
            vt.mouse(mouse(MouseAction::Press(left), (5, 0), Mods::empty()))?;
            vt.mouse(mouse(MouseAction::Release(left), (5, 0), Mods::empty()))?;
        }
        assert_eq!(
            vt.mouse(mouse(MouseAction::Press(right), (0, 0), Mods::empty()))?,
            MouseOutcome::Copy("def".into())
        );
        Ok(())
    }

    #[test]
    fn tracking_apps_get_clicks_unless_shift() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 3, 0)?;
        vt.resize(20, 3, 10, 20)?;
        vt.write(b"\x1b[?1000h\x1b[?1006hclick me");
        let left = MouseButton::Left;
        assert_eq!(
            vt.mouse(mouse(MouseAction::Press(left), (2, 1), Mods::empty()))?,
            MouseOutcome::Pty(b"\x1b[<0;3;2M".to_vec())
        );
        assert_eq!(
            vt.mouse(mouse(MouseAction::Release(left), (2, 1), Mods::empty()))?,
            MouseOutcome::Pty(b"\x1b[<0;3;2m".to_vec())
        );
        // Shift selects instead, through the whole drag.
        let shift = Mods::SHIFT;
        assert_eq!(
            vt.mouse(mouse(MouseAction::Press(left), (0, 0), shift))?,
            MouseOutcome::Nothing
        );
        vt.mouse(mouse(MouseAction::Move, (4, 0), Mods::empty()))?;
        vt.mouse(mouse(MouseAction::Release(left), (4, 0), Mods::empty()))?;
        assert_eq!(vt.copy_selection()?.as_deref(), Some("clic"));
        Ok(())
    }

    #[test]
    fn focus_reports_only_when_asked() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 2, 0)?;
        assert_eq!(vt.focus(true)?, b"");
        vt.write(b"\x1b[?1004h");
        assert_eq!(vt.focus(false)?, b"\x1b[O");
        assert_eq!(vt.focus(true)?, b"\x1b[I");
        Ok(())
    }

    #[test]
    fn cursor_blinks_by_default() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 2, 0)?;
        let blink = |vt: &mut VtCore| -> Result<bool, Box<dyn std::error::Error>> {
            let snap = vt.snapshot()?.ok_or("no snapshot")?;
            Ok(snap.cursor.ok_or("no cursor")?.blinking)
        };
        vt.write(b"x");
        assert!(blink(&mut vt)?);
        // DECSCUSR 2: steady block.
        vt.write(b"\x1b[2 q");
        assert!(!blink(&mut vt)?);
        Ok(())
    }

    #[test]
    fn shells_report_their_directory() -> Result<(), Box<dyn std::error::Error>> {
        use std::path::PathBuf;

        use crate::vt::cwd_from_report;

        let mut vt = VtCore::new(10, 2, 0)?;
        vt.write(b"\x1b]7;file://box/home/me/my%20dir\x07");
        let effects = vt.take_effects()?;
        assert_eq!(effects.cwd.is_some(), !cfg!(windows));
        // Shown even where it isn't a local path: the host is remote.
        let location = effects.location.ok_or("no location")?;
        assert_eq!(location.host.as_deref(), Some("box"));
        assert_eq!(location.path, "/home/me/my dir");
        assert_eq!(vt.take_effects()?.cwd, None, "only on change");
        vt.write(b"\x1b]9;9;C:\\Users\\me\x07");
        assert_eq!(
            vt.take_effects()?.cwd.is_some(),
            cfg!(windows),
            "OSC 9;9 sends a Windows path"
        );

        let win = |r: &str| cwd_from_report(r, true);
        assert_eq!(
            win("file://box/C:/Users/me"),
            Some(PathBuf::from("C:/Users/me"))
        );
        assert_eq!(
            win("file://box/c/Users/me%20too"),
            Some(PathBuf::from("C:/Users/me too"))
        );
        assert_eq!(win("C:\\dev"), Some(PathBuf::from("C:\\dev")));
        assert_eq!(win("file://box/tmp"), None, "MSYS-only paths");
        let unix = |r: &str| cwd_from_report(r, false);
        assert_eq!(unix("file://box/home/me"), Some(PathBuf::from("/home/me")));
        assert_eq!(unix("relative"), None);
        Ok(())
    }

    #[test]
    fn settings_recolor_the_screen() -> Result<(), Box<dyn std::error::Error>> {
        use crate::snapshot::CursorShape;

        let mut vt = VtCore::new(10, 2, 0)?;
        vt.write(b"\x1b[31mred");
        vt.snapshot()?;
        let red = Rgb { r: 1, g: 2, b: 3 };
        let bg = Rgb { r: 9, g: 9, b: 9 };
        vt.configure(&VtSettings {
            background: bg,
            palette: vec![(1, red)],
            cursor_style: CursorShape::Bar,
            ..VtSettings::default()
        })?;
        // Nothing new was written, but the colors changed.
        let snap = vt.snapshot()?.ok_or("no repaint")?;
        assert_eq!(snap.colors.background, bg);
        assert_eq!(snap.lines[0].cells[0].fg, Some(red));
        assert_eq!(snap.cursor.ok_or("no cursor")?.shape, CursorShape::Bar);
        Ok(())
    }

    #[test]
    fn copy_on_select_can_be_off() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 2, 0)?;
        vt.resize(20, 2, 10, 20)?;
        vt.configure(&VtSettings {
            copy_on_select: false,
            ..VtSettings::default()
        })?;
        vt.write(b"hello world");
        let left = MouseButton::Left;
        vt.mouse(mouse(MouseAction::Press(left), (0, 0), Mods::empty()))?;
        vt.mouse(mouse(MouseAction::Move, (4, 0), Mods::empty()))?;
        let release = vt.mouse(mouse(MouseAction::Release(left), (4, 0), Mods::empty()))?;
        assert_eq!(release, MouseOutcome::Nothing);
        assert_eq!(vt.copy_selection()?.as_deref(), Some("hell"));
        Ok(())
    }

    #[test]
    fn programs_set_the_clipboard_only_when_allowed() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 2, 0)?;
        // OSC 52, clipboard selector, base64 "hello".
        vt.write(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(vt.take_effects()?.clipboard.as_deref(), Some("hello"));
        // A read request is never answered.
        vt.write(b"\x1b]52;c;?\x07");
        let effects = vt.take_effects()?;
        assert_eq!((effects.clipboard, effects.pty_reply.len()), (None, 0));
        vt.configure(&VtSettings {
            clipboard_write: false,
            ..VtSettings::default()
        })?;
        vt.write(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(vt.take_effects()?.clipboard, None);
        Ok(())
    }

    #[test]
    fn hyperlinks_open_their_target_not_their_text() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(40, 2, 0)?;
        vt.write(b"see \x1b]8;;https://example.com/a\x1b\\docs\x1b]8;;\x1b\\ ");
        vt.write(b"\x1b]8;;file:///bin/sh\x1b\\https://ok.test\x1b]8;;\x1b\\");
        let snap = vt.snapshot()?.ok_or("no snapshot")?;
        let link = snap.url_at(5, 0).ok_or("no link")?;
        assert_eq!(link.url, "https://example.com/a");
        assert_eq!(link.cells, vec![(0, 4..8)]);
        assert_eq!(snap.url_at(8, 0), None);
        // A file:// target isn't opened, whatever it shows.
        assert_eq!(snap.url_at(12, 0), None);
        Ok(())
    }

    #[test]
    fn jumps_between_shell_prompts() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 3, 1000)?;
        for name in ["one", "two", "three"] {
            vt.write(format!("\x1b]133;A\x07$ {name}\r\n").as_bytes());
            for i in 0..5 {
                vt.write(format!("out {i}\r\n").as_bytes());
            }
        }
        let top = |vt: &mut VtCore, n| -> Result<String, Box<dyn std::error::Error>> {
            vt.jump_to_prompt(n)?;
            let snap = vt.snapshot()?;
            Ok(snap
                .as_ref()
                .and_then(|s| s.lines.first())
                .map(|l| l.text())
                .unwrap_or_default())
        };
        assert_eq!(top(&mut vt, -1)?, "$ three");
        assert_eq!(top(&mut vt, -2)?, "$ one");
        assert_eq!(top(&mut vt, 1)?, "$ two");
        // Nothing that far up: stays put (and nothing redraws).
        assert_eq!(top(&mut vt, -5)?, "");
        Ok(())
    }

    #[test]
    fn search_finds_history_and_steps_to_it() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 4, 1000)?;
        for i in 0..30 {
            let tag = if i % 10 == 3 { " Needle" } else { "" };
            vt.write(format!("line {i}{tag}\r\n").as_bytes());
        }
        // A blank row and a wrapped one keep later rows' numbers right.
        vt.write("\r\n0123456789abcdefghijXY needle".as_bytes());
        vt.snapshot()?;
        // Rows of the visible matches, and which is current.
        let shown = |vt: &mut VtCore| -> Result<_, Box<dyn std::error::Error>> {
            let snap = vt.snapshot()?.ok_or("searching redraws")?;
            let search = snap.search.clone().ok_or("searching")?;
            let rows: Vec<(String, std::ops::Range<u16>, bool)> = search
                .visible
                .iter()
                .map(|f| {
                    let text = snap.lines.get(usize::from(f.row)).map(|l| l.text());
                    (text.unwrap_or_default(), f.cols.clone(), f.current)
                })
                .collect();
            Ok((search.total, search.current, rows))
        };
        vt.search("needle")?;
        let (total, current, rows) = shown(&mut vt)?;
        assert_eq!((total, current), (4, Some(3)), "starts at the newest");
        assert_eq!(rows, vec![("XY needle".to_owned(), 3..9, true)]);
        vt.search_step(-1)?;
        let (_, current, rows) = shown(&mut vt)?;
        assert_eq!(current, Some(2));
        assert_eq!(rows, vec![("line 23 Needle".to_owned(), 8..14, true)]);
        vt.search_step(2)?;
        assert_eq!(shown(&mut vt)?.1, Some(0), "wraps around");
        // Capitals match case.
        vt.search("Needle")?;
        assert_eq!(shown(&mut vt)?.0, 3);
        vt.search("")?;
        let snap = vt.snapshot()?.ok_or("ending the search redraws")?;
        assert_eq!(snap.search, None);
        Ok(())
    }

    #[test]
    fn copies_the_last_commands_output() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 10, 100)?;
        for (cmd, out) in [("ls", "a\r\nb"), ("echo hi", "hi")] {
            let line = format!(
                "\x1b]133;A\x07$ \x1b]133;B\x07{cmd}\r\n\x1b]133;C\x07{out}\r\n\x1b]133;D;0\x07"
            );
            vt.write(line.as_bytes());
        }
        vt.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(vt.last_command_output()?.as_deref(), Some("hi"));
        Ok(())
    }

    #[test]
    fn kitty_images_reach_the_snapshot() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 5, 100)?;
        vt.resize(20, 5, 8, 16)?;
        // A 1x1 red PNG, transmitted and shown at the cursor, two cells in.
        vt.write(b"ab\x1b_Ga=T,f=100,q=2;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\");
        let snap = vt.snapshot()?.ok_or("no snapshot")?;
        let [placement] = snap.images.as_slice() else {
            return Err(format!("placements: {:?}", snap.images).into());
        };
        assert_eq!((placement.x, placement.y), (16, 0));
        assert_eq!((placement.width, placement.height), (1, 1));
        assert_eq!(placement.picture.rgba, vec![255, 0, 0, 255]);
        // Deleting it clears it from the next snapshot.
        vt.write(b"\x1b_Ga=d,q=2\x1b\\");
        let snap = vt.snapshot()?.ok_or("no snapshot after delete")?;
        assert!(snap.images.is_empty());
        Ok(())
    }

    #[test]
    fn queries_get_jets_answers() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(80, 24, 0)?;
        vt.resize(80, 24, 9, 18)?;
        let mut answer = |query: &[u8]| -> Result<String, Box<dyn std::error::Error>> {
            vt.write(query);
            Ok(String::from_utf8(vt.take_effects()?.pty_reply)?)
        };
        assert!(answer(b"\x1b[>q")?.contains("jet "), "XTVERSION");
        assert_eq!(answer(b"\x1b[16t")?, "\x1b[6;18;9t", "cell size");
        assert_eq!(answer(b"\x1b[18t")?, "\x1b[8;24;80t", "grid size");
        assert_eq!(answer(b"\x1b[?996n")?, "\x1b[?997;1n", "dark");
        vt.configure(&VtSettings {
            background: Rgb {
                r: 0xfa,
                g: 0xfa,
                b: 0xfa,
            },
            ..VtSettings::default()
        })?;
        vt.write(b"\x1b[?996n");
        assert_eq!(vt.take_effects()?.pty_reply, b"\x1b[?997;2n", "light");
        Ok(())
    }

    #[test]
    fn dragging_past_the_top_scrolls_back() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 3, 100)?;
        vt.resize(10, 3, 10, 20)?;
        for i in 0..10 {
            vt.write(format!("line{i}\r\n").as_bytes());
        }
        let left = MouseButton::Left;
        vt.mouse(mouse(MouseAction::Press(left), (2, 1), Mods::empty()))?;
        let mut above = mouse(MouseAction::Move, (2, 0), Mods::empty());
        above.y = -15.0;
        vt.mouse(above)?;
        assert!(vt.autoscroll_tick()?, "should autoscroll while above");
        vt.autoscroll_tick()?;
        vt.mouse(MouseInput {
            action: MouseAction::Release(left),
            ..above
        })?;
        assert!(!vt.autoscroll_tick()?, "released");
        let text = vt.copy_selection()?.ok_or("no selection")?;
        assert!(text.lines().count() >= 3, "selection grew upward: {text:?}");
        Ok(())
    }

    /// Human double clicks are ~100-200 ms apart (this once failed on
    /// Windows only; see `click_time`).
    #[test]
    fn double_click_at_human_speed() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 2, 0)?;
        vt.resize(20, 2, 10, 20)?;
        vt.write(b"git status --short");
        let left = MouseButton::Left;
        for _ in 0..3 {
            vt.mouse(mouse(MouseAction::Press(left), (6, 0), Mods::empty()))?;
            vt.mouse(mouse(MouseAction::Release(left), (6, 0), Mods::empty()))?;
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        assert_eq!(
            vt.copy_selection()?.as_deref(),
            Some("git status --short"),
            "triple click takes the line"
        );
        Ok(())
    }

    #[test]
    fn double_click_takes_whole_paths() -> Result<(), Box<dyn std::error::Error>> {
        let line = r#"PS C:\Users\jacobi\dev> rg x src/main.rs:12:5 ("/tmp/a b")"#;
        let word_at = |col: u16| -> Result<Option<String>, Box<dyn std::error::Error>> {
            let mut vt = VtCore::new(80, 2, 0)?;
            vt.resize(80, 2, 10, 20)?;
            vt.write(line.as_bytes());
            let left = MouseButton::Left;
            for _ in 0..2 {
                vt.mouse(mouse(MouseAction::Press(left), (col, 0), Mods::empty()))?;
                vt.mouse(mouse(MouseAction::Release(left), (col, 0), Mods::empty()))?;
            }
            Ok(vt.copy_selection()?)
        };
        assert_eq!(word_at(8)?.as_deref(), Some(r"C:\Users\jacobi\dev"));
        assert_eq!(word_at(33)?.as_deref(), Some("src/main.rs:12:5"));
        // Quotes and brackets still end a word; spaces split the quoted path.
        assert_eq!(word_at(50)?.as_deref(), Some("/tmp/a"));
        Ok(())
    }
}
