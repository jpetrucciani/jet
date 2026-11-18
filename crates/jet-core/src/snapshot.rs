//! Plain-data copy of what a pane shows. Owns no libghostty-vt handles, so it
//! is `Send` and can cross to the render thread.

use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use jetmux_proto::Matches;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<Rgb> for libghostty_vt::style::RgbColor {
    fn from(c: Rgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

impl From<libghostty_vt::style::RgbColor> for Rgb {
    fn from(c: libghostty_vt::style::RgbColor) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

impl From<Rgb> for jetmux_proto::input::Rgb {
    fn from(Rgb { r, g, b }: Rgb) -> Self {
        Self { r, g, b }
    }
}

impl From<jetmux_proto::input::Rgb> for Rgb {
    fn from(jetmux_proto::input::Rgb { r, g, b }: jetmux_proto::input::Rgb) -> Self {
        Self { r, g, b }
    }
}

/// Boolean SGR attributes of a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellFlags(u16);

impl CellFlags {
    pub const BOLD: Self = Self(1);
    pub const ITALIC: Self = Self(1 << 1);
    pub const FAINT: Self = Self(1 << 2);
    pub const BLINK: Self = Self(1 << 3);
    pub const INVERSE: Self = Self(1 << 4);
    pub const INVISIBLE: Self = Self(1 << 5);
    pub const STRIKETHROUGH: Self = Self(1 << 6);
    pub const OVERLINE: Self = Self(1 << 7);
    pub const SELECTED: Self = Self(1 << 8);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn set(&mut self, other: Self, on: bool) {
        if on {
            self.0 |= other.0;
        } else {
            self.0 &= !other.0;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Width {
    #[default]
    Narrow,
    /// First half of a double-width character.
    Wide,
    /// Placeholder cell after a wide character, or before one that wrapped.
    /// Draw nothing.
    Spacer,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub(crate) text_start: u32,
    pub(crate) text_len: u16,
    /// `None` means the default foreground.
    pub fg: Option<Rgb>,
    /// `None` means the default background.
    pub bg: Option<Rgb>,
    pub flags: CellFlags,
    pub underline: Underline,
    /// `None` means the text color.
    pub underline_color: Option<Rgb>,
    pub width: Width,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// Grapheme text of all cells, indexed by each cell's range.
    text: String,
    /// Soft-wrapped into the next line.
    pub wrapped: bool,
    /// OSC 8 hyperlinks: the cells each covers and its URI.
    pub links: Vec<(Range<u16>, Arc<str>)>,
}

/// `text`'s grapheme clusters and the cells each takes (1 or 2).
fn clusters(text: &str) -> Vec<(Vec<char>, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut rest = chars.as_slice();
    while !rest.is_empty() {
        let (len, cells) = libghostty_vt::unicode::grapheme_width(rest);
        let Some((cluster, tail)) = rest.split_at_checked(len.max(1)) else {
            break;
        };
        rest = tail;
        if cells > 0 {
            out.push((cluster.to_vec(), usize::from(cells.min(2))));
        }
    }
    out
}

impl Line {
    /// The grapheme cluster in cell `x`; empty for blank cells.
    pub fn cell_text(&self, x: usize) -> &str {
        self.cells.get(x).map_or("", |cell| {
            let start = cell.text_start as usize;
            self.text
                .get(start..start + usize::from(cell.text_len))
                .unwrap_or("")
        })
    }

    /// The line as plain text with trailing blanks trimmed. Blank cells are
    /// spaces and spacer cells are skipped.
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.cells.len());
        for (x, cell) in self.cells.iter().enumerate() {
            match (cell.width, self.cell_text(x)) {
                (Width::Spacer, _) => {}
                (_, "") => out.push(' '),
                (_, text) => out.push_str(text),
            }
        }
        out.truncate(out.trim_end_matches(' ').len());
        out
    }

    /// `text` as one row of `width` cells styled like `style`: for
    /// app-drawn overlays.
    pub fn from_text(text: &str, width: usize, style: Cell) -> Self {
        let mut line = Self::default();
        line.push_text(text, width, style);
        line
    }

    /// Cells `text` takes, as [`Line::push_text`] lays it out.
    pub fn text_width(text: &str) -> usize {
        clusters(text).iter().map(|(_, w)| w).sum()
    }

    /// Append `text` as exactly `width` cells styled like `style`, cut with
    /// `…` if too long and padded with blanks. Clusters take the cells the
    /// terminal would give them, so emoji are wide.
    pub fn push_text(&mut self, text: &str, width: usize, style: Cell) {
        let clusters = clusters(text);
        let total: usize = clusters.iter().map(|(_, w)| w).sum();
        // Too long: keep a cell for the ellipsis.
        let room = if total > width {
            width.saturating_sub(1)
        } else {
            width
        };
        let mut used = 0;
        for (cluster, cells) in clusters {
            if used + cells > room {
                break;
            }
            if cells == 2 {
                self.push(
                    Cell {
                        width: Width::Wide,
                        ..style
                    },
                    cluster.iter().copied(),
                );
                self.push(
                    Cell {
                        width: Width::Spacer,
                        ..style
                    },
                    [],
                );
            } else {
                self.push(style, cluster.iter().copied());
            }
            used += cells;
        }
        if total > width && used < width {
            self.push(style, ['…']);
            used += 1;
        }
        for _ in used..width {
            self.push(style, []);
        }
    }

    /// This line with `text` written over it from cell `x`, underlined in
    /// the colors of the cells it covers (an input method's uncommitted
    /// text), and how many cells `text` took. Cut at the line's end.
    pub fn with_preedit(&self, x: usize, text: &str) -> (Self, usize) {
        let width = self.cells.len();
        let x = x.min(width);
        let mut out = Self {
            wrapped: self.wrapped,
            ..Self::default()
        };
        // A wide character cut in half shows as a blank.
        let blank = |cell: &Cell| Cell {
            width: Width::Narrow,
            ..*cell
        };
        for (col, cell) in self.cells.iter().enumerate().take(x) {
            if cell.width == Width::Wide && col + 1 >= x {
                out.push(blank(cell), []);
            } else {
                out.push(*cell, self.cell_text(col).chars());
            }
        }
        let mut used = 0;
        for (cluster, cells) in clusters(text) {
            if x + used + cells > width {
                break;
            }
            let mut style = self.cells.get(x + used).copied().unwrap_or_default();
            style.underline = Underline::Single;
            style.flags.set(CellFlags::INVISIBLE, false);
            if cells == 2 {
                out.push(
                    Cell {
                        width: Width::Wide,
                        ..style
                    },
                    cluster.iter().copied(),
                );
                out.push(
                    Cell {
                        width: Width::Spacer,
                        ..style
                    },
                    [],
                );
            } else {
                out.push(
                    Cell {
                        width: Width::Narrow,
                        ..style
                    },
                    cluster.iter().copied(),
                );
            }
            used += cells;
        }
        for (col, cell) in self.cells.iter().enumerate().skip(x + used) {
            if cell.width == Width::Spacer {
                out.push(blank(cell), []);
            } else {
                out.push(*cell, self.cell_text(col).chars());
            }
        }
        (out, used)
    }

    /// This line with `over`'s cells in place of its own from cell `x`,
    /// cut at the line's end: an app-drawn box over the terminal.
    pub fn with_overlay(&self, x: usize, over: &Line) -> Self {
        let width = self.cells.len();
        let x = x.min(width);
        let mut out = Self {
            wrapped: self.wrapped,
            ..Self::default()
        };
        // Half a wide character left showing is a blank.
        let blank = |cell: &Cell| Cell {
            width: Width::Narrow,
            ..*cell
        };
        for (col, cell) in self.cells.iter().enumerate().take(x) {
            if cell.width == Width::Wide && col + 1 >= x {
                out.push(blank(cell), []);
            } else {
                out.push(*cell, self.cell_text(col).chars());
            }
        }
        let room = width - x;
        for (col, cell) in over.cells.iter().enumerate().take(room) {
            // A wide character without room for its second half.
            if cell.width == Width::Wide && col + 1 >= room {
                out.push(blank(cell), []);
            } else {
                out.push(*cell, over.cell_text(col).chars());
            }
        }
        let end = x + over.cells.len().min(room);
        for (col, cell) in self.cells.iter().enumerate().skip(end) {
            if cell.width == Width::Spacer && col == end {
                out.push(blank(cell), []);
            } else {
                out.push(*cell, self.cell_text(col).chars());
            }
        }
        out
    }

    /// Empty, with room for a row of `cols` single-codepoint cells.
    pub(crate) fn with_capacity(cols: usize) -> Self {
        Self {
            cells: Vec::with_capacity(cols),
            text: String::with_capacity(cols),
            ..Self::default()
        }
    }

    pub(crate) fn clear(&mut self) {
        self.cells.clear();
        self.text.clear();
        self.wrapped = false;
        self.links.clear();
    }

    /// Mark cell `x` as part of a link to `uri`, joining the link that ends
    /// just before it when the URI matches.
    pub(crate) fn link(&mut self, x: u16, uri: &str) {
        match self.links.last_mut() {
            Some((cells, last)) if cells.end == x && **last == *uri => cells.end = x + 1,
            _ => self.links.push((x..x + 1, Arc::from(uri))),
        }
    }

    pub(crate) fn push(&mut self, mut cell: Cell, text: impl IntoIterator<Item = char>) {
        let start = self.text.len();
        self.text.extend(text);
        cell.text_start = u32::try_from(start).unwrap_or(u32::MAX);
        cell.text_len = u16::try_from(self.text.len() - start).unwrap_or(0);
        self.cells.push(cell);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    BlockHollow,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub blinking: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Colors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<Rgb>,
}

/// The visible viewport of one pane.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    /// Rows that didn't change since the previous snapshot are the same
    /// `Arc`, so `Arc::ptr_eq` tells a renderer what to redraw.
    pub lines: Vec<Arc<Line>>,
    /// `None` when hidden or scrolled out of view.
    pub cursor: Option<Cursor>,
    pub colors: Colors,
    /// Kitty graphics placements that show, lowest `z` first.
    pub images: Vec<Placement>,
    /// While searching: the count and the matches in view.
    pub search: Option<Matches>,
    /// The program asked for bracketed paste, so pasted newlines don't
    /// run anything.
    pub bracketed_paste: bool,
}

/// A Kitty graphics image's pixels, shared by every snapshot showing it.
#[derive(Debug)]
pub struct Picture {
    /// Unique to these pixels for the life of the process, so renderers
    /// can cache by it.
    pub key: u64,
    pub width: u32,
    pub height: u32,
    /// Straight (not premultiplied) 8-bit RGBA.
    pub rgba: Vec<u8>,
}

impl PartialEq for Picture {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Picture {}

/// Where a picture shows: in pixels from the pane's first cell, which may
/// put it partly above or left of the pane (clip to the pane).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub picture: Arc<Picture>,
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
    /// `[x, y, width, height]` of the picture to show, in its pixels.
    pub source: [u32; 4],
    /// Negative draws under the text, else over it.
    pub z: i32,
}

/// A URL on screen and the cells it covers, row by row: one that wrapped
/// spans several.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub cells: Vec<(u16, Range<u16>)>,
}

/// Characters a URL can hold unescaped (RFC 3986).
fn url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// A logical line's text, for matching against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineText {
    pub text: String,
    /// The byte the asked-for cell is.
    pub at: usize,
    /// Where each character starts in `text`, and its cell.
    spots: Vec<(usize, u16, u16)>,
}

impl LineText {
    /// The cells bytes `range` of the text cover, as column runs per row.
    pub fn cells(&self, range: Range<usize>) -> Vec<(u16, Range<u16>)> {
        let inside = self.spots.iter().filter(|(at, ..)| range.contains(at));
        runs(inside.map(|&(_, row, col)| (row, col)))
    }
}

/// Consecutive cells as `(row, columns)` runs.
fn runs(cells: impl Iterator<Item = (u16, u16)>) -> Vec<(u16, Range<u16>)> {
    let mut out: Vec<(u16, Range<u16>)> = Vec::new();
    for (row, col) in cells {
        match out.last_mut() {
            Some((r, cols)) if *r == row => cols.end = col + 1,
            _ => out.push((row, col..col + 1)),
        }
    }
    out
}

impl Snapshot {
    /// The http(s) URL at cell `(x, y)`, following soft wraps. Trailing
    /// punctuation and unbalanced closing brackets aren't part of it, so a
    /// URL ending a sentence or inside parentheses comes out clean.
    pub fn url_at(&self, x: u16, y: u16) -> Option<Link> {
        let line = self.lines.get(usize::from(y))?;
        if let Some((cells, uri)) = line.links.iter().find(|(cells, _)| cells.contains(&x)) {
            // A program picks what a link shows, so only open schemes that
            // can't hide a local program behind harmless text.
            let scheme = uri.split_once(':').map(|(s, _)| s.to_ascii_lowercase());
            return matches!(scheme.as_deref(), Some("http" | "https" | "mailto")).then(|| Link {
                url: uri.to_string(),
                cells: vec![(y, cells.clone())],
            });
        }
        let chars = self.logical_chars(y)?;
        let at = chars.iter().position(|&(r, c, _)| r == y && c == x)?;
        let in_url = |i: usize| chars.get(i).is_some_and(|&(_, _, c)| url_char(c));
        if !in_url(at) {
            return None;
        }
        let mut start = at;
        while start > 0 && in_url(start - 1) {
            start -= 1;
        }
        let mut end = at + 1;
        while in_url(end) {
            end += 1;
        }
        let token: String = chars.get(start..end)?.iter().map(|&(_, _, c)| c).collect();
        // URL characters are ASCII, so byte and character offsets agree.
        let rel = at - start;
        let begin = ["https://", "http://"]
            .iter()
            .flat_map(|scheme| token.match_indices(scheme).map(|(i, _)| (i, scheme.len())))
            .filter(|&(i, _)| i <= rel)
            .max_by_key(|&(i, _)| i);
        let (begin, scheme) = begin?;
        let mut url = token.get(begin..)?.to_owned();
        loop {
            let unbalanced = |open: char, close: char| {
                url.ends_with(close) && url.matches(open).count() < url.matches(close).count()
            };
            if url.ends_with(['.', ',', ';', ':', '!', '?', '\'', '"'])
                || unbalanced('(', ')')
                || unbalanced('[', ']')
            {
                url.pop();
            } else {
                break;
            }
        }
        if url.len() <= scheme || rel >= begin + url.len() {
            return None;
        }
        let spots = chars.get(start + begin..start + begin + url.len())?;
        let cells = runs(spots.iter().map(|&(row, col, _)| (row, col)));
        Some(Link { url, cells })
    }

    /// The logical line through row `y` (soft wraps followed) as text, and
    /// which byte of it cell `(x, y)` is.
    pub fn line_text(&self, x: u16, y: u16) -> Option<LineText> {
        let chars = self.logical_chars(y)?;
        let (mut text, mut spots, mut at) = (String::new(), Vec::new(), None);
        for (row, col, c) in chars {
            if (row, col) == (y, x) {
                at = Some(text.len());
            }
            spots.push((text.len(), row, col));
            text.push(c);
        }
        Some(LineText {
            text,
            at: at?,
            spots,
        })
    }

    /// Each cell of the logical line through row `y`, in order, with its
    /// character: wide characters' spacers skipped, clusters as a space.
    fn logical_chars(&self, y: u16) -> Option<Vec<(u16, u16, char)>> {
        let y = usize::from(y);
        let wrapped = |row: usize| self.lines.get(row).is_some_and(|l| l.wrapped);
        let mut top = y;
        while top > 0 && wrapped(top - 1) {
            top -= 1;
        }
        let mut bottom = y;
        while wrapped(bottom) && bottom + 1 < self.lines.len() {
            bottom += 1;
        }
        let mut chars = Vec::new();
        for row in top..=bottom {
            let line = self.lines.get(row)?;
            for (col, cell) in line.cells.iter().enumerate() {
                if cell.width == Width::Spacer {
                    continue;
                }
                let mut text = line.cell_text(col).chars();
                let c = match (text.next(), text.next()) {
                    (Some(c), None) => c,
                    _ => ' ',
                };
                let (row, col) = (u16::try_from(row).ok()?, u16::try_from(col).ok()?);
                chars.push((row, col, c));
            }
        }
        Some(chars)
    }

    /// All lines as text, one per row, trailing blank rows trimmed.
    pub fn text(&self) -> String {
        let mut out: Vec<String> = self.lines.iter().map(|line| line.text()).collect();
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        out.join("\n")
    }
}

/// Handoff of the newest snapshot from a pane thread to the render thread.
#[derive(Clone, Debug, Default)]
pub struct SnapshotSlot(Arc<Mutex<Option<Snapshot>>>);

impl SnapshotSlot {
    /// Only the pane thread publishes, and only into an empty slot (see
    /// `PaneThread::flush`), so nothing is ever overwritten.
    pub fn publish(&self, snapshot: Snapshot) {
        // Snapshots are plain data, so a panic elsewhere can't leave one half-built.
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(snapshot);
    }

    pub fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }

    pub fn take(&self) -> Option<Snapshot> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::snapshot::{Cell, Line, Link, Snapshot, Underline, Width};

    fn screen(rows: &[(&str, bool)], cols: usize) -> Snapshot {
        let lines = rows
            .iter()
            .map(|&(text, wrapped)| {
                let mut line = Line::from_text(text, cols, Cell::default());
                line.wrapped = wrapped;
                Arc::new(line)
            })
            .collect();
        Snapshot {
            cols: u16::try_from(cols).unwrap_or_default(),
            rows: u16::try_from(rows.len()).unwrap_or_default(),
            lines,
            ..Snapshot::default()
        }
    }

    #[test]
    fn urls_end_before_punctuation_and_stray_brackets() {
        let s = screen(&[("see (https://x.dev/a_(b)?q=1). ok", false)], 40);
        let link = s.url_at(10, 0);
        assert_eq!(
            link,
            Some(Link {
                url: "https://x.dev/a_(b)?q=1".into(),
                cells: vec![(0, 5..28)],
            })
        );
        assert_eq!(s.url_at(28, 0), None, "the closing paren");
        assert_eq!(s.url_at(1, 0), None, "plain text");
        assert_eq!(screen(&[("ftp://x.dev", false)], 20).url_at(3, 0), None);
    }

    #[test]
    fn urls_follow_soft_wraps() {
        let s = screen(&[("go http://exam", true), ("ple.com/x now", false)], 14);
        let link = s.url_at(2, 1);
        assert_eq!(
            link,
            Some(Link {
                url: "http://example.com/x".into(),
                cells: vec![(0, 3..14), (1, 0..9)],
            })
        );
    }

    #[test]
    fn preedit_overlays_and_underlines() {
        let line = Line::from_text("ab日cde", 8, Cell::default());
        // Starts on the wide character's second half: its first half blanks.
        let (out, used) = line.with_preedit(3, "かx");
        assert_eq!(used, 3);
        assert_eq!(out.text(), "ab かxe");
        assert_eq!(out.cells.len(), 8);
        assert_eq!(out.cells[3].underline, Underline::Single);
        assert_eq!(out.cells[6].underline, Underline::None);
        // Too long for the line: cut.
        assert_eq!(line.with_preedit(6, "xyz").1, 2);
    }

    #[test]
    fn overlay_text_gives_emoji_two_cells() {
        let line = Line::from_text("🚀ab", 5, Cell::default());
        let widths: Vec<Width> = line.cells.iter().map(|c| c.width).collect();
        assert_eq!(
            widths,
            [
                Width::Wide,
                Width::Spacer,
                Width::Narrow,
                Width::Narrow,
                Width::Narrow
            ]
        );
        assert_eq!(line.text(), "🚀ab");
        // Cut short, a wide cluster that doesn't fit before the ellipsis goes.
        assert_eq!(Line::from_text("ab🚀cd", 3, Cell::default()).text(), "ab…");
        assert_eq!(Line::from_text("abcd", 3, Cell::default()).text(), "ab…");
    }
}
