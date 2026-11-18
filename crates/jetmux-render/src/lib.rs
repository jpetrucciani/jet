//! Composes a session view onto the outer terminal: pane grids clipped into
//! their layout rectangles, borders, and the status bar.
//!
//! The renderer keeps a copy of every visible pane's grid, so layout changes
//! redraw locally without asking the daemon for frames.

use std::collections::HashMap;
use std::fmt::Write;

use jetmux_proto::{
    Color, Cursor, CursorShape, Frame, Modes, MouseFormat, MouseTracking, Rect, Size, Style,
    Underline, View,
};

const BEGIN_SYNC: &str = "\x1b[?2026h";
const END_SYNC: &str = "\x1b[?2026l";

const BORDER: Style = Style {
    fg: Color::Palette(8),
    ..DEFAULT_STYLE
};
const BORDER_FOCUSED: Style = Style {
    fg: Color::Palette(2),
    ..DEFAULT_STYLE
};
const DEFAULT_STYLE: Style = Style {
    fg: Color::Default,
    bg: Color::Default,
    underline_color: Color::Default,
    underline: Underline::None,
    bold: false,
    faint: false,
    italic: false,
    blink: false,
    inverse: false,
    invisible: false,
    strikethrough: false,
    overline: false,
};

/// Status bar colors and shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Powerline arrows between segments; needs a Powerline or Nerd font.
    pub powerline: bool,
    pub bar: Color,
    pub text: Color,
    /// Hint labels and the help note.
    pub muted: Color,
    /// Hint keys.
    pub key: Color,
    /// The session name's background.
    pub session: Color,
    /// Text on the session and mode badges.
    pub badge_text: Color,
    pub active: Color,
    pub active_text: Color,
    pub prefix: Color,
    pub pane: Color,
    pub tab: Color,
    pub copy: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            powerline: false,
            bar: Color::Palette(236),
            text: Color::Palette(250),
            muted: Color::Palette(244),
            key: Color::Palette(214),
            session: Color::Palette(214),
            badge_text: Color::Palette(235),
            active: Color::Palette(239),
            active_text: Color::Palette(231),
            prefix: Color::Palette(214),
            pane: Color::Palette(114),
            tab: Color::Palette(75),
            copy: Color::Palette(176),
        }
    }
}

/// What the client wants in the status bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub theme: Theme,
    /// Key layer badge on the left ("PANE", "COPY", ...) and its color.
    pub mode: Option<(String, Color)>,
    /// Show the session and tab list.
    pub tabs: bool,
    /// Keys and what they do, after the tabs; whole hints are dropped from
    /// the end when space runs out.
    pub hints: Vec<(String, String)>,
    /// A quiet note on the right, like how to reach the hints.
    pub help: Option<String>,
    /// An active text prompt; replaces the tabs and hints and takes the
    /// cursor.
    pub prompt: Option<String>,
    /// A transient note, shown after everything else on the left.
    pub message: Option<String>,
    /// jet's status segments: left ones after the message, right ones
    /// before the help note.
    pub segments: Vec<Segment>,
}

/// A status segment with something to say, colored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub right: bool,
}

#[derive(Debug, Default)]
struct Grid {
    size: Size,
    rows: Vec<Vec<StyledRun>>,
    cursor: Option<Cursor>,
    modes: Modes,
}

#[derive(Clone, Debug)]
struct StyledRun {
    style: Style,
    text: String,
    cluster: Option<u8>,
    /// OSC 8 target.
    link: Option<String>,
}

impl StyledRun {
    fn cells(&self) -> usize {
        self.cluster
            .map_or_else(|| self.text.chars().count(), usize::from)
    }
}

#[derive(Debug)]
pub struct Renderer {
    size: Size,
    modes: Modes,
    view: Option<View>,
    grids: HashMap<u32, Grid>,
    status: Status,
    /// The outer terminal speaks the kitty keyboard protocol, so a pane's
    /// flags can be mirrored onto it.
    kitty_keyboard: bool,
}

impl Renderer {
    pub fn new(size: Size) -> Self {
        Self {
            size,
            modes: Modes::default(),
            view: None,
            grids: HashMap::new(),
            status: Status::default(),
            kitty_keyboard: false,
        }
    }

    /// The outer terminal answered the kitty keyboard query.
    pub fn set_kitty_keyboard(&mut self, out: &mut String) {
        self.kitty_keyboard = true;
        let modes = self.focused_modes();
        self.apply_modes(modes, out);
    }

    pub fn view(&self) -> Option<&View> {
        self.view.as_ref()
    }

    /// Modes of the focused pane, as last reported.
    pub fn focused_modes(&self) -> Modes {
        self.focused()
            .and_then(|id| self.grids.get(&id))
            .map(|g| g.modes)
            .unwrap_or_default()
    }

    /// Switch to the alternate screen. Autowrap is off so drawing at the
    /// right edge never scrolls. Also asks whether the outer terminal speaks
    /// the kitty keyboard protocol; a terminal that does answers `CSI ? n u`.
    pub fn enter(&mut self, out: &mut String) {
        out.push_str("\x1b[?1049h\x1b[?7l\x1b[0m\x1b[H\x1b[2J\x1b[?u");
    }

    /// Undo everything [`Renderer::enter`] and the frames changed.
    pub fn leave(&mut self, out: &mut String) {
        self.apply_modes(Modes::default(), out);
        out.push_str("\x1b[0m\x1b[0 q\x1b[?25h\x1b[?7h\x1b[?1049l");
    }

    /// The outer terminal changed size. The daemon answers the resize with a
    /// new view, which redraws everything.
    pub fn resize(&mut self, size: Size) {
        self.size = size;
    }

    pub fn set_view(&mut self, view: View, out: &mut String) {
        self.grids
            .retain(|id, _| view.panes.iter().any(|p| p.id == *id));
        self.view = Some(view);
        self.redraw(out);
    }

    pub fn set_status(&mut self, status: Status, out: &mut String) {
        if status != self.status {
            self.status = status;
            out.push_str(BEGIN_SYNC);
            self.draw_status(out);
            self.finish(out);
        }
    }

    pub fn pane_frame(&mut self, id: u32, frame: Frame, out: &mut String) {
        let grid = self.grids.entry(id).or_default();
        grid.size = frame.size;
        grid.cursor = frame.cursor;
        grid.modes = frame.modes;
        grid.rows
            .resize_with(usize::from(frame.size.rows), Vec::new);
        let mut changed = Vec::with_capacity(frame.rows.len());
        for row in frame.rows {
            let style = |i: u16| {
                frame
                    .styles
                    .get(usize::from(i))
                    .copied()
                    .unwrap_or_default()
            };
            if let Some(slot) = grid.rows.get_mut(usize::from(row.y)) {
                *slot = row
                    .runs
                    .into_iter()
                    .map(|r| StyledRun {
                        style: style(r.style),
                        text: r.text,
                        cluster: r.cluster,
                        link: usize::from(r.link)
                            .checked_sub(1)
                            .and_then(|i| frame.links.get(i))
                            .cloned(),
                    })
                    .collect();
                changed.push(row.y);
            }
        }
        let Some(rect) = self.rect_of(id) else {
            return;
        };
        out.push_str(BEGIN_SYNC);
        out.push_str("\x1b[?25l");
        if frame.full {
            self.draw_pane(id, rect, None, out);
        } else {
            self.draw_pane(id, rect, Some(&changed), out);
        }
        self.finish(out);
    }

    /// Redraw the whole screen from the stored view and grids.
    pub fn redraw(&mut self, out: &mut String) {
        out.push_str(BEGIN_SYNC);
        out.push_str("\x1b[?25l\x1b[0m\x1b[H\x1b[2J");
        if let Some(view) = &self.view {
            self.draw_borders(view, out);
            for pane in &view.panes {
                self.draw_pane(pane.id, pane.rect, None, out);
            }
        }
        self.draw_status(out);
        self.finish(out);
    }

    fn focused(&self) -> Option<u32> {
        self.view
            .as_ref()?
            .panes
            .iter()
            .find(|p| p.focused)
            .map(|p| p.id)
    }

    fn rect_of(&self, id: u32) -> Option<Rect> {
        self.view
            .as_ref()?
            .panes
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.rect)
    }

    /// Draw a pane's rows (all, or just `only`) clipped to `rect`. Missing
    /// rows and columns are blanked, so smaller panes letterbox.
    fn draw_pane(&self, id: u32, rect: Rect, only: Option<&[u16]>, out: &mut String) {
        let empty = Grid::default();
        let grid = self.grids.get(&id).unwrap_or(&empty);
        let width = usize::from(rect.cols);
        let rows: Vec<u16> = match only {
            Some(rows) => rows.iter().copied().filter(|y| *y < rect.rows).collect(),
            None => (0..rect.rows).collect(),
        };
        for y in rows {
            let _ = write!(out, "\x1b[{};{}H\x1b[0m", rect.y + y + 1, rect.x + 1);
            let mut current = DEFAULT_STYLE;
            let mut link: Option<&str> = None;
            let mut used = 0;
            for run in grid.rows.get(usize::from(y)).into_iter().flatten() {
                if used >= width {
                    break;
                }
                if run.style != current {
                    sgr(&run.style, out);
                    current = run.style;
                }
                if run.link.as_deref() != link {
                    link = run.link.as_deref();
                    let _ = write!(out, "\x1b]8;;{}\x1b\\", link.unwrap_or(""));
                }
                let cells = run.cells();
                if used + cells <= width {
                    out.push_str(&run.text);
                    used += cells;
                } else if run.cluster.is_some() {
                    // A wide char that doesn't fit: pad instead of spilling
                    // into the neighbor.
                    out.extend(std::iter::repeat_n(' ', width - used));
                    used = width;
                } else {
                    out.extend(run.text.chars().take(width - used));
                    used = width;
                }
            }
            if link.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
            if used < width {
                out.push_str("\x1b[0m");
                out.extend(std::iter::repeat_n(' ', width - used));
            }
        }
    }

    fn draw_borders(&self, view: &View, out: &mut String) {
        let area = view.area;
        let (w, h) = (usize::from(area.cols), usize::from(area.rows));
        let mut covered = vec![false; w * h];
        for pane in &view.panes {
            for y in pane.rect.y..pane.rect.y.saturating_add(pane.rect.rows) {
                for x in pane.rect.x..pane.rect.x.saturating_add(pane.rect.cols) {
                    let (cx, cy) = (usize::from(x - area.x), usize::from(y - area.y));
                    if cx < w && cy < h {
                        covered[cy * w + cx] = true;
                    }
                }
            }
        }
        let border = |x: isize, y: isize| {
            x >= 0
                && y >= 0
                && (x as usize) < w
                && (y as usize) < h
                && !covered[y as usize * w + x as usize]
        };
        let focused = view.panes.iter().find(|p| p.focused).map(|p| p.rect);
        let near_focus = |x: u16, y: u16| {
            focused.is_some_and(|r| {
                let grown = Rect {
                    x: r.x.saturating_sub(1),
                    y: r.y.saturating_sub(1),
                    cols: r.cols + 2,
                    rows: r.rows + 2,
                };
                grown.contains(x, y)
            })
        };
        for cy in 0..h {
            let mut drawn_to = usize::MAX;
            for cx in 0..w {
                let (ix, iy) = (cx as isize, cy as isize);
                if !border(ix, iy) {
                    continue;
                }
                let (up, down) = (border(ix, iy - 1), border(ix, iy + 1));
                let (left, right) = (border(ix - 1, iy), border(ix + 1, iy));
                let ch = match (up, down, left, right) {
                    (true, true, true, true) => '┼',
                    (true, true, false, true) => '├',
                    (true, true, true, false) => '┤',
                    (false, true, true, true) => '┬',
                    (true, false, true, true) => '┴',
                    (_, _, true, _) | (_, _, _, true) => '─',
                    _ => '│',
                };
                let (x, y) = (area.x + cx as u16, area.y + cy as u16);
                if drawn_to != cx {
                    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                }
                sgr(
                    if near_focus(x, y) {
                        &BORDER_FOCUSED
                    } else {
                        &BORDER
                    },
                    out,
                );
                out.push(ch);
                drawn_to = cx + 1;
            }
        }
    }

    fn draw_status(&self, out: &mut String) {
        let width = usize::from(self.size.cols);
        let status = &self.status;
        let theme = status.theme;
        let help = status
            .help
            .as_ref()
            .filter(|_| status.prompt.is_none())
            .map(|h| format!(" {h} "))
            .unwrap_or_default();
        let (right, left): (Vec<&Segment>, Vec<&Segment>) = match status.prompt {
            Some(_) => (Vec::new(), Vec::new()),
            None => status.segments.iter().partition(|s| s.right),
        };
        let cells = |s: &&Segment| s.text.chars().count() + 2 + usize::from(theme.powerline);
        // The left side wins over the right when space runs out.
        let help_cells = help.chars().count();
        let right_cells = help_cells + right.iter().map(cells).sum::<usize>();
        let (left_width, right) = if right_cells * 2 < width {
            (width - right_cells, right)
        } else if help_cells * 2 < width {
            (width - help_cells, Vec::new())
        } else {
            (width, Vec::new())
        };
        let _ = write!(out, "\x1b[{};1H", self.size.rows);
        let mut bar = Bar::new(Line::new(left_width, out), theme);
        if let Some((label, color)) = &status.mode {
            bar.segment(*color, theme.badge_text, true, &format!(" {label} "));
        }
        match &status.prompt {
            Some(prompt) => bar.segment(theme.bar, theme.text, false, &format!(" {prompt}")),
            None => {
                if status.tabs
                    && let Some(view) = &self.view
                {
                    bar.segment(
                        theme.session,
                        theme.badge_text,
                        true,
                        &format!(" {} ", view.session),
                    );
                    for (i, name) in view.tabs.iter().map(|t| &t.name).enumerate() {
                        let zoom = if view.zoomed { " (z)" } else { "" };
                        if i == usize::from(view.active_tab) {
                            let text = format!(" {} {}{zoom} ", i + 1, truncate(name, 24));
                            bar.segment(theme.active, theme.active_text, true, &text);
                        } else {
                            let text = format!(" {} {} ", i + 1, truncate(name, 24));
                            bar.segment(theme.bar, theme.text, false, &text);
                        }
                    }
                }
                for (keys, label) in &status.hints {
                    let cells = keys.chars().count() + label.chars().count() + 3;
                    if bar.line.used + cells > bar.line.width {
                        break;
                    }
                    bar.segment(theme.bar, theme.key, true, &format!(" {keys}"));
                    bar.segment(theme.bar, theme.muted, false, &format!(" {label} "));
                }
            }
        }
        if let Some(message) = &status.message {
            bar.segment(theme.bar, theme.text, false, &format!("  {message}"));
        }
        for segment in left {
            if bar.line.used + cells(&segment) > bar.line.width {
                break;
            }
            bar.segment(
                segment.bg,
                segment.fg,
                false,
                &format!(" {} ", segment.text),
            );
        }
        bar.segment(theme.bar, theme.text, false, "");
        bar.line.pad_to(left_width);
        let mut bar = Bar::new(Line::new(width - left_width, bar.line.out), theme);
        if !right.is_empty() {
            // An arrow from the bar into the first one.
            bar.bg = Some(theme.bar);
        }
        for segment in right {
            bar.segment(
                segment.bg,
                segment.fg,
                false,
                &format!(" {} ", segment.text),
            );
        }
        bar.segment(theme.bar, theme.muted, false, &help);
        bar.line.pad_to(width);
        out.push_str("\x1b[0m");
    }

    /// The cell the prompt's text starts at, after the mode badge.
    fn prompt_start(&self) -> usize {
        let badge = self.status.mode.as_ref().map_or(0, |(label, _)| {
            label.chars().count() + 2 + usize::from(self.status.theme.powerline)
        });
        badge + 1
    }

    /// End a drawing operation: mirror the focused pane's modes and put the
    /// cursor where it belongs.
    fn finish(&mut self, out: &mut String) {
        let focused = self.focused();
        let modes = self.focused_modes();
        self.apply_modes(modes, out);
        if let Some(prompt) = &self.status.prompt {
            let x = (self.prompt_start() + prompt.chars().count())
                .min(usize::from(self.size.cols).saturating_sub(1));
            let _ = write!(out, "\x1b[{};{}H\x1b[6 q\x1b[?25h", self.size.rows, x + 1);
        } else if let (Some(id), Some(rect)) = (focused, focused.and_then(|id| self.rect_of(id)))
            && let Some(cursor) = self.grids.get(&id).and_then(|g| g.cursor)
            && cursor.x < rect.cols
            && cursor.y < rect.rows
        {
            let shape = match (cursor.shape, cursor.blinking) {
                (CursorShape::Block | CursorShape::BlockHollow, true) => 1,
                (CursorShape::Block | CursorShape::BlockHollow, false) => 2,
                (CursorShape::Underline, true) => 3,
                (CursorShape::Underline, false) => 4,
                (CursorShape::Bar, true) => 5,
                (CursorShape::Bar, false) => 6,
            };
            let _ = write!(
                out,
                "\x1b[{};{}H\x1b[{shape} q\x1b[?25h",
                rect.y + cursor.y + 1,
                rect.x + cursor.x + 1
            );
        } else {
            out.push_str("\x1b[?25l");
        }
        out.push_str(END_SYNC);
    }

    fn apply_modes(&mut self, want: Modes, out: &mut String) {
        let have = self.modes;
        let set = |out: &mut String, mode: u16, on: bool| {
            let _ = write!(out, "\x1b[?{mode}{}", if on { 'h' } else { 'l' });
        };
        if want.app_cursor != have.app_cursor {
            set(out, 1, want.app_cursor);
        }
        if want.app_keypad != have.app_keypad {
            out.push_str(if want.app_keypad { "\x1b=" } else { "\x1b>" });
        }
        if want.bracketed_paste != have.bracketed_paste {
            set(out, 2004, want.bracketed_paste);
        }
        if want.focus_events != have.focus_events {
            set(out, 1004, want.focus_events);
        }
        // The outer terminal always reports mouse in SGR form; the client
        // re-encodes events into whatever format the pane asked for.
        if want.mouse != have.mouse {
            if let Some(mode) = mouse_tracking_mode(have.mouse) {
                set(out, mode, false);
            }
            if let Some(mode) = mouse_tracking_mode(want.mouse) {
                set(out, mode, true);
            }
            set(out, 1006, want.mouse != MouseTracking::Off);
        }
        let kitty_keyboard = if self.kitty_keyboard {
            want.kitty_keyboard
        } else {
            0
        };
        if kitty_keyboard != have.kitty_keyboard {
            let _ = write!(out, "\x1b[={kitty_keyboard};1u");
        }
        self.modes = Modes {
            mouse_format: MouseFormat::Sgr,
            kitty_keyboard,
            ..want
        };
    }
}

/// A full frame as VT that paints it onto a cleared screen of the frame's
/// size, cursor included: where a recording of the pane starts.
pub fn frame_to_vt(frame: &Frame) -> String {
    let mut out = String::from("\x1b[0m\x1b[H\x1b[2J");
    for row in &frame.rows {
        let _ = write!(out, "\x1b[{};1H", row.y + 1);
        let mut current = DEFAULT_STYLE;
        let mut link = 0;
        for run in &row.runs {
            let style = frame
                .styles
                .get(usize::from(run.style))
                .copied()
                .unwrap_or_default();
            if style != current {
                sgr(&style, &mut out);
                current = style;
            }
            if run.link != link {
                link = run.link;
                let uri = usize::from(link)
                    .checked_sub(1)
                    .and_then(|i| frame.links.get(i))
                    .map_or("", String::as_str);
                let _ = write!(out, "\x1b]8;;{uri}\x1b\\");
            }
            out.push_str(&run.text);
        }
        if link != 0 {
            out.push_str("\x1b]8;;\x1b\\");
        }
        out.push_str("\x1b[0m");
    }
    match frame.cursor {
        Some(cursor) => {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", cursor.y + 1, cursor.x + 1);
        }
        None => out.push_str("\x1b[?25l"),
    }
    out
}

/// Writes at most `width` cells of plain text.
/// Status bar segments in a [`Line`], with a Powerline arrow wherever the
/// background changes if the theme asks for one.
struct Bar<'a> {
    line: Line<'a>,
    theme: Theme,
    /// The background drawn last, if anything has been.
    bg: Option<Color>,
}

impl<'a> Bar<'a> {
    fn new(line: Line<'a>, theme: Theme) -> Self {
        Self {
            line,
            theme,
            bg: None,
        }
    }

    fn segment(&mut self, bg: Color, fg: Color, bold: bool, text: &str) {
        if self.theme.powerline
            && let Some(prev) = self.bg
            && prev != bg
        {
            let arrow = Style {
                fg: prev,
                bg,
                ..DEFAULT_STYLE
            };
            sgr(&arrow, self.line.out);
            self.line.push("\u{e0b0}");
        }
        if text.is_empty() && self.bg.is_some() {
            self.bg = Some(bg);
            return;
        }
        let style = Style {
            fg,
            bg,
            bold,
            ..DEFAULT_STYLE
        };
        sgr(&style, self.line.out);
        self.line.push(text);
        self.bg = Some(bg);
    }
}

struct Line<'a> {
    width: usize,
    used: usize,
    out: &'a mut String,
}

impl<'a> Line<'a> {
    fn new(width: usize, out: &'a mut String) -> Self {
        Self {
            width,
            used: 0,
            out,
        }
    }

    fn push(&mut self, text: &str) {
        for ch in text.chars() {
            if self.used >= self.width {
                return;
            }
            self.out.push(ch);
            self.used += 1;
        }
    }

    fn pad_to(&mut self, cells: usize) {
        while self.used < cells.min(self.width) {
            self.out.push(' ');
            self.used += 1;
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn mouse_tracking_mode(m: MouseTracking) -> Option<u16> {
    match m {
        MouseTracking::Off => None,
        MouseTracking::X10 => Some(9),
        MouseTracking::Normal => Some(1000),
        MouseTracking::Button => Some(1002),
        MouseTracking::Any => Some(1003),
    }
}

/// Emit a full SGR for `style`, starting from a reset.
fn sgr(style: &Style, out: &mut String) {
    out.push_str("\x1b[0");
    let flags = [
        (style.bold, "1"),
        (style.faint, "2"),
        (style.italic, "3"),
        (style.blink, "5"),
        (style.inverse, "7"),
        (style.invisible, "8"),
        (style.strikethrough, "9"),
        (style.overline, "53"),
    ];
    for (on, code) in flags {
        if on {
            out.push(';');
            out.push_str(code);
        }
    }
    match style.underline {
        Underline::None => {}
        Underline::Single => out.push_str(";4"),
        Underline::Double => out.push_str(";4:2"),
        Underline::Curly => out.push_str(";4:3"),
        Underline::Dotted => out.push_str(";4:4"),
        Underline::Dashed => out.push_str(";4:5"),
    }
    color(style.fg, 30, 90, 38, out);
    color(style.bg, 40, 100, 48, out);
    match style.underline_color {
        Color::Default => {}
        Color::Palette(i) => {
            let _ = write!(out, ";58:5:{i}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";58:2::{r}:{g}:{b}");
        }
    }
    out.push('m');
}

fn color(c: Color, base: u8, bright: u8, extended: u8, out: &mut String) {
    let _ = match c {
        Color::Default => Ok(()),
        Color::Palette(i @ 0..8) => write!(out, ";{}", base + i),
        Color::Palette(i @ 8..16) => write!(out, ";{}", bright + i - 8),
        Color::Palette(i) => write!(out, ";{extended};5;{i}"),
        Color::Rgb(r, g, b) => write!(out, ";{extended};2;{r};{g};{b}"),
    };
}

#[cfg(test)]
mod tests {
    use jetmux_proto::{Color, Frame, PaneView, Rect, Size, View};

    use crate::{Renderer, Segment, Status};

    fn text(frame: &Frame, y: u16) -> String {
        let row = frame.rows.iter().find(|r| r.y == y).unwrap();
        row.runs.iter().map(|r| r.text.as_str()).collect()
    }

    fn pane(id: u32, rect: Rect, focused: bool) -> PaneView {
        PaneView {
            id,
            rect,
            focused,
            ..PaneView::default()
        }
    }

    /// Render two panes side by side, feed the bytes to a second terminal
    /// standing in for the outer terminal, and check what it shows.
    #[test]
    fn composes_panes_borders_and_status() {
        let left = Rect {
            x: 0,
            y: 0,
            cols: 6,
            rows: 2,
        };
        let right = Rect {
            x: 7,
            y: 0,
            cols: 5,
            rows: 2,
        };
        let mut a = jetmux_core::vt(left.size()).unwrap();
        a.write("\x1b[1;38;2;255;128;0mhot\x1b[0m\r\n日本語".as_bytes());
        // Pane b is wider than its rect (another client owns its size), so
        // it must be clipped rather than spill.
        let mut b = jetmux_core::vt(Size { cols: 8, rows: 2 }).unwrap();
        b.write(b"\x1b[44mabcdefgh\x1b[0m\r\nxy");

        let outer = Size { cols: 12, rows: 3 };
        let mut r = Renderer::new(outer);
        let mut bytes = String::new();
        r.enter(&mut bytes);
        r.set_view(
            View {
                session: "s".into(),
                tabs: vec![jetmux_proto::TabView {
                    id: 0,
                    name: "one".into(),
                    layout: jetmux_proto::layout::Layout::Pane(1),
                    focused: 1,
                }],
                panes: vec![pane(1, left, false), pane(2, right, true)],
                area: Rect {
                    x: 0,
                    y: 0,
                    cols: 12,
                    rows: 2,
                },
                ..View::default()
            },
            &mut bytes,
        );
        r.pane_frame(1, a.frame(true).unwrap(), &mut bytes);
        r.pane_frame(2, b.frame(true).unwrap(), &mut bytes);
        r.set_status(
            Status {
                mode: Some(("PANE".into(), Color::Palette(2))),
                tabs: true,
                hints: vec![("x".into(), "close".into())],
                ..Status::default()
            },
            &mut bytes,
        );

        let mut outer_vt = jetmux_core::vt(outer).unwrap();
        outer_vt.write(bytes.as_bytes());
        let shown = outer_vt.frame(true).unwrap();

        assert_eq!(text(&shown, 0), "hot   │abcde");
        assert_eq!(text(&shown, 1), "日本語│xy   ");
        assert!(text(&shown, 2).starts_with(" PANE  s  1"));
        let hot = &shown.rows[0].runs[0];
        assert!(shown.styles[usize::from(hot.style)].bold);
        assert_eq!(
            shown.styles[usize::from(hot.style)].fg,
            Color::Rgb(255, 128, 0)
        );
        // Cursor is the focused pane's, offset by its rect.
        assert_eq!(shown.cursor.map(|c| (c.x, c.y)), Some((9, 1)));
    }

    /// jet's segments sit at their ends of the bar, and go before the left
    /// side runs out of room.
    #[test]
    fn segments_fill_the_bar_from_both_ends() {
        let outer = Size { cols: 30, rows: 2 };
        let mut r = Renderer::new(outer);
        let mut bytes = String::new();
        r.enter(&mut bytes);
        let segment = |text: &str, right| Segment {
            text: text.into(),
            fg: Color::Palette(0),
            bg: Color::Palette(4),
            right,
        };
        let status = |segments| Status {
            message: Some("hi".into()),
            help: Some("help".into()),
            segments,
            ..Status::default()
        };
        r.set_status(
            status(vec![segment("me@box", true), segment("left", false)]),
            &mut bytes,
        );
        let mut outer_vt = jetmux_core::vt(outer).unwrap();
        outer_vt.write(bytes.as_bytes());
        let shown = outer_vt.frame(true).unwrap();
        assert_eq!(text(&shown, 1), "  hi left        me@box  help ");

        r.set_status(status(vec![segment(&"x".repeat(20), true)]), &mut bytes);
        let mut outer_vt = jetmux_core::vt(outer).unwrap();
        outer_vt.write(bytes.as_bytes());
        let shown = outer_vt.frame(true).unwrap();
        assert!(text(&shown, 1).ends_with(" help "));
        assert!(!text(&shown, 1).contains('x'));
    }

    /// Links and the app's kitty keyboard flags make it to the outer
    /// terminal, the flags only once it has said it speaks the protocol.
    #[test]
    fn links_and_keyboard_flags_pass_through() {
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 8,
            rows: 1,
        };
        let mut app = jetmux_core::vt(rect.size()).unwrap();
        app.write(b"\x1b]8;;https://a.example\x1b\\go\x1b]8;;\x1b\\ on\x1b[>1u");
        let outer = Size { cols: 8, rows: 2 };
        let mut r = Renderer::new(outer);
        let mut bytes = String::new();
        r.enter(&mut bytes);
        r.set_view(
            View {
                panes: vec![pane(1, rect, true)],
                area: rect,
                ..View::default()
            },
            &mut bytes,
        );
        r.pane_frame(1, app.frame(true).unwrap(), &mut bytes);
        let mut outer_vt = jetmux_core::vt(outer).unwrap();
        outer_vt.write(bytes.as_bytes());
        let shown = outer_vt.frame(true).unwrap();
        assert_eq!(shown.links, ["https://a.example"]);
        let go = &shown.rows[0].runs[0];
        assert_eq!((go.text.as_str(), go.link), ("go", 1));
        assert_eq!(shown.rows[0].runs[1].link, 0);
        assert_eq!(shown.modes.kitty_keyboard, 0);

        bytes.clear();
        r.set_kitty_keyboard(&mut bytes);
        outer_vt.write(bytes.as_bytes());
        assert_eq!(outer_vt.frame(true).unwrap().modes.kitty_keyboard, 1);
    }

    /// A recording's first screen paints the same grid back.
    #[test]
    fn frames_turn_back_into_the_screen() {
        let size = Size { cols: 12, rows: 3 };
        let mut app = jetmux_core::vt(size).unwrap();
        app.write("\x1b[1;31mred\x1b[0m 日本\r\nsecond".as_bytes());
        let frame = app.frame(true).unwrap();
        let mut replay = jetmux_core::vt(size).unwrap();
        replay.write(b"junk to clear first");
        replay.write(crate::frame_to_vt(&frame).as_bytes());
        let again = replay.frame(true).unwrap();
        assert_eq!(text(&again, 0), text(&frame, 0));
        assert_eq!(text(&again, 1), text(&frame, 1));
        assert_eq!(again.cursor.map(|c| (c.x, c.y)), Some((6, 1)));
        let red = &again.rows[0].runs[0];
        assert_eq!(again.styles[usize::from(red.style)].fg, Color::Palette(1));
    }
}
