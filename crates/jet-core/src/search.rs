//! Finding text on a pane's screen, scrollback included. libghostty has no
//! search of its own, so the screen is formatted as plain text, one row per
//! line, and matched row by row: a match can't span a soft wrap.

use std::ops::Range;

use libghostty_vt::Terminal;
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::screen::TrackedGridRef;
use libghostty_vt::terminal::{Point, PointCoordinate, PointSpace, ScrollViewport};

use crate::Error;
use crate::snapshot::{Found, Matches};

/// A match: its screen row (0 is the oldest line of history) and cells.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Hit {
    row: u64,
    cols: Range<u16>,
}

pub(crate) struct Search {
    /// What's looked for, lowercased when the query has no capitals
    /// (smart case).
    needle: Vec<char>,
    fold: bool,
    /// Oldest first.
    hits: Vec<Hit>,
    current: Option<usize>,
    /// The current match's first cell, followed as history scrolls off the
    /// top and rows renumber.
    anchor: Option<TrackedGridRef>,
    /// The screen changed since the hits were found.
    pub(crate) stale: bool,
}

impl Search {
    /// Search `term` for `query`, starting at the newest match at or above
    /// the viewport's bottom: the one nearest what you're looking at,
    /// going back.
    pub(crate) fn new(term: &Terminal<'_, '_>, query: &str) -> Result<Self, Error> {
        let fold = !query.chars().any(char::is_uppercase);
        let mut search = Self {
            needle: query.chars().map(|c| fold_char(c, fold)).collect(),
            fold,
            hits: Vec::new(),
            current: None,
            anchor: None,
            stale: false,
        };
        search.hits = search.find(term)?;
        let bar = term.scrollbar()?;
        let bottom = bar.offset + bar.len;
        let current = search
            .hits
            .iter()
            .rposition(|h| h.row < bottom)
            .or((!search.hits.is_empty()).then_some(0));
        search.select(term, current)?;
        Ok(search)
    }

    /// Find the matches again after the screen changed, keeping the current
    /// one when it's still there (else the nearest before it).
    pub(crate) fn refresh(&mut self, term: &Terminal<'_, '_>) -> Result<(), Error> {
        self.stale = false;
        let at = match &self.anchor {
            Some(anchor) => anchor.point(PointSpace::Screen)?,
            None => None,
        };
        self.hits = self.find(term)?;
        let current = at.and_then(|at| {
            let (row, col) = (u64::from(at.y), at.x);
            self.hits
                .iter()
                .rposition(|h| (h.row, h.cols.start) <= (row, col))
        });
        let current = current.or((!self.hits.is_empty()).then_some(0));
        self.select(term, current)
    }

    /// Move `n` matches: negative goes up, to older output. Wraps around.
    pub(crate) fn step(&mut self, term: &mut Terminal<'_, '_>, n: isize) -> Result<(), Error> {
        let len = self.hits.len();
        if len == 0 {
            return Ok(());
        }
        let at = self.current.unwrap_or(0) as isize;
        let next = (at + n).rem_euclid(len as isize) as usize;
        self.select(term, Some(next))?;
        self.reveal(term)
    }

    /// Scroll so the current match shows, centered if it was out of view.
    pub(crate) fn reveal(&self, term: &mut Terminal<'_, '_>) -> Result<(), Error> {
        let Some(hit) = self.current.and_then(|i| self.hits.get(i)) else {
            return Ok(());
        };
        let bar = term.scrollbar()?;
        if !(bar.offset..bar.offset + bar.len).contains(&hit.row) {
            let top = hit.row.saturating_sub(bar.len / 2);
            term.scroll_viewport(ScrollViewport::Row(usize::try_from(top).unwrap_or(0)));
        }
        Ok(())
    }

    /// The count and the matches in the viewport, by viewport row.
    pub(crate) fn matches(&self, term: &Terminal<'_, '_>) -> Result<Matches, Error> {
        let bar = term.scrollbar()?;
        let view = bar.offset..bar.offset + bar.len;
        let first = self.hits.partition_point(|h| h.row < view.start);
        let visible = self
            .hits
            .iter()
            .enumerate()
            .skip(first)
            .take_while(|(_, h)| view.contains(&h.row))
            .map(|(i, h)| Found {
                row: u16::try_from(h.row - view.start).unwrap_or(u16::MAX),
                cols: h.cols.clone(),
                current: self.current == Some(i),
            })
            .collect();
        Ok(Matches {
            total: self.hits.len(),
            current: self.current,
            visible,
        })
    }

    fn select(&mut self, term: &Terminal<'_, '_>, current: Option<usize>) -> Result<(), Error> {
        self.current = current;
        self.anchor = match current.and_then(|i| self.hits.get(i)) {
            Some(hit) => Some(term.track_grid_ref(Point::Screen(PointCoordinate {
                x: hit.cols.start,
                y: u32::try_from(hit.row).unwrap_or(u32::MAX),
            }))?),
            None => None,
        };
        Ok(())
    }

    fn find(&self, term: &Terminal<'_, '_>) -> Result<Vec<Hit>, Error> {
        if self.needle.is_empty() {
            return Ok(Vec::new());
        }
        let opts = FormatterOptions::new()
            .with_format(Format::Plain)
            .with_unwrap(false)
            .with_trim(true);
        let bytes = Formatter::new(term, opts)?.format_alloc(None)?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(find_in(&text, &self.needle, self.fold))
    }
}

fn fold_char(c: char, fold: bool) -> char {
    if fold {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c
    }
}

/// Every match of `needle` in `text`, a row per line, without overlaps.
fn find_in(text: &str, needle: &[char], fold: bool) -> Vec<Hit> {
    let mut hits = Vec::new();
    // Per char of a row: the char as compared, and the cells its cluster
    // spans.
    let mut chars: Vec<(char, u16, u16)> = Vec::new();
    for (row, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.len() < needle.len() {
            continue;
        }
        chars.clear();
        layout(line, fold, &mut chars);
        let mut i = 0;
        while i + needle.len() <= chars.len() {
            let window = chars.get(i..i + needle.len()).unwrap_or_default();
            if window.iter().map(|c| c.0).eq(needle.iter().copied()) {
                let (Some(first), Some(last)) = (window.first(), window.last()) else {
                    break;
                };
                hits.push(Hit {
                    row: row as u64,
                    cols: first.1..last.2,
                });
                if hits.len() >= Matches::CAP {
                    return hits;
                }
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    hits
}

/// `line`'s chars with the cells each one's cluster covers, as the
/// terminal lays them out: ASCII is a cell each, the rest goes by
/// libghostty's widths.
fn layout(line: &str, fold: bool, out: &mut Vec<(char, u16, u16)>) {
    let chars: Vec<char> = line.chars().collect();
    let mut col = 0u16;
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let next_ascii = chars.get(i + 1).is_none_or(char::is_ascii);
        if c.is_ascii() && next_ascii {
            out.push((fold_char(c, fold), col, col.saturating_add(1)));
            col = col.saturating_add(1);
            i += 1;
            continue;
        }
        // A cluster is never this long in practice; capping the window
        // keeps a row of emoji linear.
        let window = chars.get(i..chars.len().min(i + 16)).unwrap_or_default();
        let (len, cells) = libghostty_vt::unicode::grapheme_width(window);
        let len = len.max(1);
        let end = col.saturating_add(u16::from(cells));
        for &c in chars.get(i..i + len).unwrap_or_default() {
            out.push((fold_char(c, fold), col, end));
        }
        col = end;
        i += len;
    }
}

#[cfg(test)]
mod tests {
    use crate::search::{Hit, find_in};

    fn needle(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn matches_by_cell_with_smart_case() {
        let text = "Error one\n\nwide 日本 error\nERROR";
        let hits = find_in(text, &needle("error"), true);
        let rows: Vec<(u64, std::ops::Range<u16>)> = hits
            .into_iter()
            .map(|Hit { row, cols }| (row, cols))
            .collect();
        // The CJK pair takes four cells, pushing the match right.
        assert_eq!(rows, vec![(0, 0..5), (2, 10..15), (3, 0..5)]);
        assert_eq!(find_in(text, &needle("ERROR"), false).len(), 1);
        let wide = find_in(text, &needle("日本"), true);
        assert_eq!(wide.first().map(|h| h.cols.clone()), Some(5..9));
    }
}
