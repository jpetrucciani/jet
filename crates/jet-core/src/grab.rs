//! Quick select: labels the URLs, paths, hashes and addresses in view (or,
//! during a search, its matches) so typing a label copies one. (Copy mode, the other way to grab text from
//! the keyboard, is in [`crate::copy`].)

use std::sync::{Arc, LazyLock};

use jetmux_proto::Matches;
use libghostty_vt::key::{Action, Key, Mods};
use regex::Regex;

use crate::snapshot::{Cell, CellFlags, Colors, Line, Width};
use crate::vt::KeyInput;

/// What a key did while copy mode or quick select was on.
#[derive(Debug, PartialEq, Eq)]
pub enum Grabbed {
    Nothing,
    /// Copy this to the clipboard.
    Copy(String),
    /// Copy it and type it into the pane (quick select with Shift).
    CopyAndPaste(String),
}

/// The one character a key typed, if that's what it did.
pub(crate) fn typed(input: &KeyInput) -> Option<char> {
    if input.mods.intersects(Mods::ALT | Mods::SUPER) {
        return None;
    }
    let mut chars = input.text.as_deref()?.chars();
    chars.next().filter(|_| chars.next().is_none())
}

/// Labels, home row first.
const ALPHABET: &str = "asdfghjklqwertyuiopzxcvbnm";

/// What quick select picks out: URLs, paths (`/` or `\`, with `:line:col`), git
/// hashes, IPv4 addresses (with a port).
static PATTERN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"\b(?:https?|ftp|file|ssh)://[^\s<>"'`]+"#,
        r"|(?:~|\.{1,2}|[A-Za-z]:|[\w.@+-]+)?(?:[/\\][\w.@+-]+)+[/\\]?(?::\d+){0,2}",
        r"|\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b",
        r"|\b[0-9a-f]{7,40}\b",
    ))
    .ok()
});

pub(crate) struct QuickSelect {
    typed: String,
    hints: Vec<Hint>,
}

#[derive(Debug, PartialEq, Eq)]
struct Hint {
    row: u16,
    col: u16,
    label: String,
    text: String,
}

impl QuickSelect {
    pub(crate) fn new() -> Self {
        Self {
            typed: String::new(),
            hints: Vec::new(),
        }
    }

    /// Find what's worth picking in `lines` (the viewport), or take the
    /// search's matches there when one is on, and draw the labels of those
    /// still matching what was typed over their first cells.
    pub(crate) fn label(
        &mut self,
        lines: &mut [Arc<Line>],
        colors: &Colors,
        search: Option<&Matches>,
    ) {
        self.hints = match search {
            Some(matches) => labeled(found(lines, matches)),
            None => find(lines),
        };
        let style = Cell {
            fg: Some(colors.background),
            bg: Some(colors.cursor.unwrap_or(colors.foreground)),
            flags: CellFlags::BOLD,
            ..Cell::default()
        };
        for hint in &self.hints {
            let Some(rest) = hint.label.strip_prefix(&self.typed) else {
                continue;
            };
            if let Some(line) = lines.get_mut(usize::from(hint.row)) {
                let at = usize::from(hint.col) + self.typed.len();
                let label = Line::from_text(rest, rest.len(), style);
                *line = Arc::new(line.with_overlay(at, &label));
            }
        }
    }

    /// `None` while more is to be typed; otherwise whether quick select is
    /// over and what it grabbed.
    pub(crate) fn key(&mut self, input: &KeyInput) -> Option<Grabbed> {
        if input.action == Action::Release {
            return None;
        }
        if input.key == Key::Escape {
            return Some(Grabbed::Nothing);
        }
        if input.key == Key::Backspace {
            self.typed.pop();
            return None;
        }
        let c = typed(input).filter(|c| c.is_ascii_alphabetic())?;
        self.typed.push(c.to_ascii_lowercase());
        if let Some(hint) = self.hints.iter().find(|h| h.label == self.typed) {
            let text = hint.text.clone();
            return Some(if c.is_ascii_uppercase() {
                Grabbed::CopyAndPaste(text)
            } else {
                Grabbed::Copy(text)
            });
        }
        if !self.hints.iter().any(|h| h.label.starts_with(&self.typed)) {
            self.typed.clear();
        }
        None
    }
}

/// The matches in `lines`, soft-wrapped rows read as one, labeled in order.
fn find(lines: &[Arc<Line>]) -> Vec<Hint> {
    let Some(pattern) = PATTERN.as_ref() else {
        return Vec::new();
    };
    let mut found: Vec<(u16, u16, String)> = Vec::new();
    let mut text = String::new();
    // The cell each byte offset of `text` starts at.
    let mut cells: Vec<(usize, u16, u16)> = Vec::new();
    for (y, line) in lines.iter().enumerate() {
        let y = u16::try_from(y).unwrap_or(u16::MAX);
        for (x, cell) in line.cells.iter().enumerate() {
            if cell.width == Width::Spacer {
                continue;
            }
            cells.push((text.len(), y, u16::try_from(x).unwrap_or(u16::MAX)));
            match line.cell_text(x) {
                "" => text.push(' '),
                t => text.push_str(t),
            }
        }
        if line.wrapped {
            continue;
        }
        for m in pattern.find_iter(&text) {
            let s = m
                .as_str()
                .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']);
            // Hex alone is a hash only with both digits and letters, not a
            // plain number or word.
            let hex = s.bytes().all(|b| b.is_ascii_hexdigit());
            let mixed =
                s.bytes().any(|b| b.is_ascii_digit()) && s.bytes().any(|b| b.is_ascii_alphabetic());
            if s.len() < 3 || (hex && !mixed) {
                continue;
            }
            let at = cells.partition_point(|&(offset, ..)| offset <= m.start());
            if let Some(&(_, y, x)) = at.checked_sub(1).and_then(|i| cells.get(i)) {
                found.push((y, x, s.to_owned()));
            }
        }
        text.clear();
        cells.clear();
    }
    labeled(found)
}

/// The text of a search's matches in `lines`, with where each starts.
fn found(lines: &[Arc<Line>], matches: &Matches) -> Vec<(u16, u16, String)> {
    matches
        .visible
        .iter()
        .filter_map(|m| {
            let line = lines.get(usize::from(m.row))?;
            let text = m
                .cols
                .clone()
                .map(usize::from)
                .filter(|&x| line.cells.get(x).is_some_and(|c| c.width != Width::Spacer))
                .map(|x| match line.cell_text(x) {
                    "" => " ",
                    t => t,
                })
                .collect();
            Some((m.row, m.cols.start, text))
        })
        .collect()
}

fn labeled(found: Vec<(u16, u16, String)>) -> Vec<Hint> {
    let labels = labels(found.len());
    found
        .into_iter()
        .zip(labels)
        .map(|((row, col, text), label)| Hint {
            row,
            col,
            label,
            text,
        })
        .collect()
}

/// `n` labels no one of which starts another: single letters while they
/// last, then pairs.
fn labels(n: usize) -> Vec<String> {
    let letters: Vec<char> = ALPHABET.chars().collect();
    if n <= letters.len() {
        return letters.iter().take(n).map(|c| c.to_string()).collect();
    }
    letters
        .iter()
        .flat_map(|&a| letters.iter().map(move |&b| format!("{a}{b}")))
        .take(n)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::grab::{find, labels};
    use crate::snapshot::{Cell, Line};

    fn screen(rows: &[&str]) -> Vec<Arc<Line>> {
        rows.iter()
            .map(|r| Arc::new(Line::from_text(r, 60, Cell::default())))
            .collect()
    }

    #[test]
    fn finds_urls_paths_hashes_and_addresses() {
        let lines = screen(&[
            "see https://example.com/a?b=1, then",
            "error at src/main.rs:12:5 and ~/notes.txt",
            "commit 3f2a9c1 on 10.0.0.12:8080 (1234567 is a number)",
            r"in C:\Users\me\jet.toml or src\app.rs",
        ]);
        let found: Vec<(u16, u16, String)> = find(&lines)
            .into_iter()
            .map(|h| (h.row, h.col, h.text))
            .collect();
        assert_eq!(
            found,
            [
                (0, 4, "https://example.com/a?b=1".to_owned()),
                (1, 9, "src/main.rs:12:5".to_owned()),
                (1, 30, "~/notes.txt".to_owned()),
                (2, 7, "3f2a9c1".to_owned()),
                (2, 18, "10.0.0.12:8080".to_owned()),
                (3, 3, r"C:\Users\me\jet.toml".to_owned()),
                (3, 27, r"src\app.rs".to_owned()),
            ]
        );
    }

    #[test]
    fn labels_never_prefix_each_other() {
        assert_eq!(labels(3), ["a", "s", "d"]);
        let many = labels(30);
        assert_eq!(many.len(), 30);
        assert!(many.iter().all(|l| l.len() == 2));
    }
}
