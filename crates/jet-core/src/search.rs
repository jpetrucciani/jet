//! Finding text on a pane's screen, scrollback included. libghostty has no
//! search of its own, so the screen is formatted as plain text, one row per
//! line, and matched row by row: a match can't span a soft wrap. Queries
//! are text or regular expressions, smart case either way.

use std::ops::Range;

use libghostty_vt::Terminal;
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::screen::TrackedGridRef;
use libghostty_vt::terminal::{Point, PointCoordinate, PointSpace, ScrollViewport};
use regex::{Regex, RegexBuilder};

use crate::Error;
use jetmux_proto::{Found, Matches};

/// A match: its screen row (0 is the oldest line of history) and cells.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Hit {
    row: u64,
    cols: Range<u16>,
}

/// What a search looks for.
enum Matcher {
    /// Text, lowercased (and compared lowercased) when the query has no
    /// capitals.
    Text {
        needle: Vec<char>,
        fold: bool,
    },
    Regex(Regex),
    /// A pattern that doesn't compile: matches nothing.
    Invalid,
}

impl Matcher {
    fn new(query: &str, regex: bool) -> Self {
        let fold = !query.chars().any(char::is_uppercase);
        if !regex {
            let needle = query.chars().map(|c| fold_char(c, fold)).collect();
            return Self::Text { needle, fold };
        }
        RegexBuilder::new(query)
            .case_insensitive(fold)
            .build()
            .map_or(Self::Invalid, Self::Regex)
    }
}

pub(crate) struct Search {
    matcher: Matcher,
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
    pub(crate) fn new(term: &Terminal<'_, '_>, query: &str, regex: bool) -> Result<Self, Error> {
        let mut search = Self {
            matcher: Matcher::new(query, regex),
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
            invalid: matches!(self.matcher, Matcher::Invalid),
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
        match &self.matcher {
            Matcher::Text { needle, .. } if needle.is_empty() => return Ok(Vec::new()),
            Matcher::Invalid => return Ok(Vec::new()),
            _ => {}
        }
        let opts = FormatterOptions::new()
            .with_format(Format::Plain)
            .with_unwrap(false)
            .with_trim(true);
        let bytes = Formatter::new(term, opts)?.format_alloc(None)?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(find_in(&text, &self.matcher))
    }
}

fn fold_char(c: char, fold: bool) -> char {
    if fold {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c
    }
}

/// Every match in `text`, a row per line, without overlaps.
fn find_in(text: &str, matcher: &Matcher) -> Vec<Hit> {
    let mut hits = Vec::new();
    // Per char of a row: the char as compared, and the cells its cluster
    // spans.
    let mut chars: Vec<(char, u16, u16)> = Vec::new();
    // Where each char starts in the row, for turning regex byte offsets
    // into chars.
    let mut starts: Vec<usize> = Vec::new();
    for (row, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        chars.clear();
        let mut found = |first: usize, last: usize, chars: &[(char, u16, u16)]| {
            if let (Some(a), Some(b)) = (chars.get(first), chars.get(last)) {
                hits.push(Hit {
                    row: row as u64,
                    cols: a.1..b.2,
                });
            }
        };
        match matcher {
            Matcher::Text { needle, fold } => {
                if line.len() < needle.len() {
                    continue;
                }
                layout(line, *fold, &mut chars);
                let mut i = 0;
                while i + needle.len() <= chars.len() {
                    let window = chars.get(i..i + needle.len()).unwrap_or_default();
                    if window.iter().map(|c| c.0).eq(needle.iter().copied()) {
                        found(i, i + needle.len() - 1, &chars);
                        i += needle.len();
                    } else {
                        i += 1;
                    }
                }
            }
            Matcher::Regex(re) => {
                let mut ms = re.find_iter(line).filter(|m| !m.is_empty()).peekable();
                if ms.peek().is_none() {
                    continue;
                }
                layout(line, false, &mut chars);
                starts.clear();
                starts.extend(line.char_indices().map(|(i, _)| i));
                for m in ms {
                    let first = starts.partition_point(|&b| b < m.start());
                    let end = starts.partition_point(|&b| b < m.end());
                    found(first, end.saturating_sub(1), &chars);
                }
            }
            Matcher::Invalid => return hits,
        }
        if hits.len() >= Matches::CAP {
            hits.truncate(Matches::CAP);
            return hits;
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
    use crate::search::{Hit, Matcher, find_in};

    fn rows(text: &str, query: &str, regex: bool) -> Vec<(u64, std::ops::Range<u16>)> {
        find_in(text, &Matcher::new(query, regex))
            .into_iter()
            .map(|Hit { row, cols }| (row, cols))
            .collect()
    }

    #[test]
    fn matches_by_cell_with_smart_case() {
        let text = "Error one\n\nwide 日本 error\nERROR";
        // The CJK pair takes four cells, pushing the match right.
        assert_eq!(
            rows(text, "error", false),
            [(0, 0..5), (2, 10..15), (3, 0..5)]
        );
        assert_eq!(rows(text, "ERROR", false).len(), 1);
        assert_eq!(rows(text, "日本", false), [(2, 5..9)]);
    }

    #[test]
    fn regexes_match_by_cell_with_smart_case() {
        let text = "ok 200 /a\nERR 503 日本 /b 404";
        assert_eq!(rows(text, r"\b[45]\d\d\b", true), [(1, 4..7), (1, 16..19)]);
        assert_eq!(rows(text, "err|ok", true), [(0, 0..2), (1, 0..3)]);
        assert_eq!(rows(text, "Err", true), []);
        assert_eq!(rows(text, "日.", true), [(1, 8..12)]);
        // Empty matches and bad patterns find nothing.
        assert_eq!(rows(text, "x*", true), []);
        assert!(matches!(Matcher::new("(", true), Matcher::Invalid));
    }
}
