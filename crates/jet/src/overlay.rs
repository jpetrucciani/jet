//! The find bar and the command palette: boxes jet draws over the focused
//! pane, which take the keyboard while open.

use std::sync::Arc;

use jet_config::keybinds::Action;
use jet_core::{Cell, CellFlags, Cursor, CursorShape, Key, KeyAction, KeyInput, Line, Matches};
use jet_core::{Mods, Rgb, Snapshot};

use crate::layout::PaneId;

/// Search matches: the current one stands out from the rest.
const MATCH_BG: Rgb = Rgb {
    r: 0xe5,
    g: 0xc0,
    b: 0x7b,
};
const CURRENT_BG: Rgb = Rgb {
    r: 0xff,
    g: 0x8c,
    b: 0x3b,
};
const MATCH_FG: Rgb = Rgb {
    r: 0x1b,
    g: 0x1e,
    b: 0x28,
};
/// The find bar's width in cells, when the pane has them.
const FIND_WIDTH: usize = 40;
const PALETTE_WIDTH: usize = 72;
const PALETTE_ROWS: usize = 12;

pub enum Overlay {
    Find(Find),
    Palette(Palette),
    Rename(Rename),
}

/// What a key did to an overlay.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Not a key it uses.
    Nothing,
    /// Redraw: the query (and for the find bar, the search) changed.
    Edited,
    /// Redraw: the palette's selection moved.
    Moved,
    Close,
    /// To the `n`th match up (negative) or down.
    Step(isize),
    /// Quick select the find bar's matches in view.
    Hints,
    /// A palette entry was picked.
    Pick(Choice),
    /// Name the tab this (empty: no name of its own).
    Rename(String),
}

/// The colors overlays draw with: text on a box a step off the terminal's
/// background, and a further step for what's selected.
#[derive(Clone, Copy)]
pub struct Colors {
    pub background: Rgb,
    pub foreground: Rgb,
    pub selected: Rgb,
}

impl Colors {
    fn cell(&self, selected: bool, faint: bool) -> Cell {
        let mut cell = Cell::default();
        cell.fg = Some(self.foreground);
        cell.bg = Some(if selected {
            self.selected
        } else {
            self.background
        });
        cell.flags.set(CellFlags::FAINT, faint);
        cell
    }
}

/// A one-line text field with a cursor.
#[derive(Default)]
struct Field {
    text: String,
    /// The cursor's byte offset, on a char boundary.
    at: usize,
}

/// Where words break, for word moves and deletes.
fn separator(c: char) -> bool {
    c.is_whitespace() || "/\\.-_:".contains(c)
}

impl Field {
    /// `text`, with the cursor at its end.
    fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            at: text.len(),
        }
    }

    fn before(&self) -> &str {
        self.text.get(..self.at).unwrap_or_default()
    }

    fn after(&self) -> &str {
        self.text.get(self.at..).unwrap_or_default()
    }

    fn char_left(&self) -> usize {
        self.before()
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn char_right(&self) -> usize {
        self.at + self.after().chars().next().map_or(0, char::len_utf8)
    }

    /// The start of the word before the cursor, past any separators.
    fn word_left(&self) -> usize {
        let kept = self.before().trim_end_matches(separator);
        kept.char_indices()
            .rev()
            .find(|&(_, c)| separator(c))
            .map_or(0, |(i, c)| i + c.len_utf8())
    }

    /// The end of the word after the cursor, past any separators.
    fn word_right(&self) -> usize {
        let after = self.after();
        let rest = after.trim_start_matches(separator);
        let word = rest.find(separator).unwrap_or(rest.len());
        self.at + (after.len() - rest.len()) + word
    }

    /// Remove the text between the cursor and `to`; whether there was any.
    fn delete_to(&mut self, to: usize) -> bool {
        let range = self.at.min(to)..self.at.max(to);
        if range.is_empty() {
            return false;
        }
        self.at = range.start;
        self.text.replace_range(range, "");
        true
    }

    fn move_to(&mut self, to: usize) -> bool {
        self.at = to;
        false
    }

    fn insert(&mut self, text: &str) -> bool {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.at, &text);
        self.at += text.len();
        !text.is_empty()
    }

    /// Apply an editing key; whether the text changed (else the cursor
    /// may have moved), `None` for a key that isn't one. Ctrl or Alt makes
    /// a move or delete a word's; Cmd on macOS, the line's.
    fn edit(&mut self, input: &KeyInput) -> Option<bool> {
        let mods = input.mods;
        let word = mods.intersects(Mods::CTRL | Mods::ALT);
        let line = mods.contains(Mods::SUPER);
        // not Ctrl+Alt, which is AltGr typing on some layouts
        let ctrl = mods.contains(Mods::CTRL) && !mods.contains(Mods::ALT);
        let end = self.text.len();
        Some(match input.key {
            Key::Backspace if line => self.delete_to(0),
            Key::Backspace if word => self.delete_to(self.word_left()),
            Key::Backspace => self.delete_to(self.char_left()),
            Key::Delete if line => self.delete_to(end),
            Key::Delete if word => self.delete_to(self.word_right()),
            Key::Delete => self.delete_to(self.char_right()),
            Key::ArrowLeft if line => self.move_to(0),
            Key::ArrowLeft if word => self.move_to(self.word_left()),
            Key::ArrowLeft => self.move_to(self.char_left()),
            Key::ArrowRight if line => self.move_to(end),
            Key::ArrowRight if word => self.move_to(self.word_right()),
            Key::ArrowRight => self.move_to(self.char_right()),
            Key::Home => self.move_to(0),
            Key::End => self.move_to(end),
            // as in readline
            Key::A if ctrl => self.move_to(0),
            Key::E if ctrl => self.move_to(end),
            Key::U if ctrl => self.delete_to(0),
            Key::K if ctrl => self.delete_to(end),
            _ => {
                let text = input.text.as_deref()?;
                if mods.intersects(Mods::CTRL | Mods::ALT | Mods::SUPER) {
                    return None;
                }
                self.insert(text)
            }
        })
    }

    /// Typed text pasted in: its first line.
    fn paste(&mut self, text: &str) -> bool {
        self.insert(text.lines().next().unwrap_or_default())
    }

    /// What of the text shows in `cells`, the cursor always among it, and
    /// the cursor's cell in that.
    fn view(&self, cells: usize) -> (&str, usize) {
        let before = tail(self.before(), cells.saturating_sub(1));
        let start = self.at - before.len();
        let shown = self.text.get(start..).unwrap_or_default();
        (shown, Line::text_width(before))
    }
}

/// The find bar over a pane.
pub struct Find {
    pub pane: PaneId,
    query: Field,
    /// The query is a regular expression (Alt+R toggles).
    pub regex: bool,
}

impl Find {
    pub fn new(pane: PaneId) -> Self {
        Self {
            pane,
            query: Field::default(),
            regex: false,
        }
    }

    pub fn query(&self) -> &str {
        &self.query.text
    }

    pub fn key(&mut self, input: &KeyInput) -> Outcome {
        if input.action == KeyAction::Release {
            return Outcome::Nothing;
        }
        let back = if input.mods.contains(Mods::SHIFT) {
            1
        } else {
            -1
        };
        match input.key {
            Key::Escape => Outcome::Close,
            Key::R if input.mods.contains(Mods::ALT) => {
                self.regex = !self.regex;
                Outcome::Edited
            }
            Key::Enter | Key::F3 => Outcome::Step(back),
            Key::Tab => Outcome::Hints,
            Key::ArrowUp => Outcome::Step(-1),
            Key::ArrowDown => Outcome::Step(1),
            _ => edited(self.query.edit(input)),
        }
    }

    pub fn paste(&mut self, text: &str) -> Outcome {
        edited(Some(self.query.paste(text)))
    }

    /// Draw the bar over the top right of `snapshot`, with the cursor in
    /// it when `typing`.
    pub fn draw(&self, snapshot: &mut Snapshot, colors: Colors, typing: bool) {
        let cols = usize::from(snapshot.cols);
        let width = cols.min(FIND_WIDTH);
        let count = match &snapshot.search {
            _ if self.query.text.is_empty() => String::new(),
            None => "…".to_owned(),
            Some(Matches { invalid: true, .. }) => "bad regex".to_owned(),
            Some(Matches { total: 0, .. }) => "no matches".to_owned(),
            Some(m) => {
                let more = if m.total >= Matches::CAP { "+" } else { "" };
                let at = m.current.map_or(0, |i| i + 1);
                format!("{at}/{}{more}", m.total)
            }
        };
        let label = if self.regex { " regex " } else { " find " };
        let count_w = Line::text_width(&count) + 1;
        let field_w = width.saturating_sub(Line::text_width(label) + count_w);
        // The query's end shows when it's longer than the field.
        let (query, at) = self.query.view(field_w);
        let mut line = Line::default();
        line.push_text(label, Line::text_width(label), colors.cell(true, true));
        line.push_text(query, field_w, colors.cell(false, false));
        line.push_text(&count, count_w.saturating_sub(1), colors.cell(false, true));
        line.push_text(" ", 1, colors.cell(false, false));
        let x = cols - line.cells.len().min(cols);
        if let Some(top) = snapshot.lines.first_mut() {
            *top = Arc::new(top.with_overlay(x, &line));
        }
        let cursor = x + Line::text_width(label) + at;
        snapshot.cursor = typing.then(|| Cursor {
            x: u16::try_from(cursor.min(cols.saturating_sub(1))).unwrap_or(0),
            y: 0,
            shape: CursorShape::Bar,
            blinking: false,
        });
    }
}

/// The tab-name prompt, filled with the name it has.
pub struct Rename {
    /// The tab's id.
    pub tab: u64,
    name: Field,
}

impl Rename {
    pub fn new(tab: u64, name: &str) -> Self {
        Self {
            tab,
            name: Field::new(name),
        }
    }

    pub fn key(&mut self, input: &KeyInput) -> Outcome {
        if input.action == KeyAction::Release {
            return Outcome::Nothing;
        }
        match input.key {
            Key::Escape => Outcome::Close,
            Key::Enter => Outcome::Rename(self.name.text.trim().to_owned()),
            _ => edited(self.name.edit(input)),
        }
    }

    pub fn paste(&mut self, text: &str) -> Outcome {
        edited(Some(self.name.paste(text)))
    }

    /// Draw the prompt over the top of `snapshot`, like the find bar.
    pub fn draw(&self, snapshot: &mut Snapshot, colors: Colors, typing: bool) {
        let cols = usize::from(snapshot.cols);
        let width = cols.min(FIND_WIDTH);
        let label = " tab name ";
        let field_w = width.saturating_sub(Line::text_width(label) + 1);
        let (name, at) = self.name.view(field_w);
        let mut line = Line::default();
        line.push_text(label, Line::text_width(label), colors.cell(true, true));
        line.push_text(name, field_w, colors.cell(false, false));
        line.push_text(" ", 1, colors.cell(false, false));
        let x = cols - line.cells.len().min(cols);
        if let Some(top) = snapshot.lines.first_mut() {
            *top = Arc::new(top.with_overlay(x, &line));
        }
        let cursor = x + Line::text_width(label) + at;
        snapshot.cursor = typing.then(|| Cursor {
            x: u16::try_from(cursor.min(cols.saturating_sub(1))).unwrap_or(0),
            y: 0,
            shape: CursorShape::Bar,
            blinking: false,
        });
    }
}

/// What picking a palette entry does.
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    Run(Action),
    /// Close this after all, with something running in it.
    Close(Closing),
    /// Paste this into the pane after all.
    Paste(PaneId, String),
    /// Use this theme, and write it to the config.
    Theme(String),
    /// Leave things as they are.
    Cancel,
}

/// What's closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closing {
    Pane(PaneId),
    /// A tab, by its id.
    Tab(u64),
    Window,
}

/// A palette entry.
pub struct Command {
    pub label: String,
    /// Its key chord, if bound.
    pub hint: String,
    pub choice: Choice,
}

/// A filtered list to pick from: the commands, or a short question's
/// answers.
pub struct Palette {
    /// Before the query: what's being asked, if not for a command.
    prompt: String,
    query: Field,
    commands: Vec<Command>,
    /// Indexes into `commands` that match the query, best first.
    shown: Vec<usize>,
    selected: usize,
}

/// Where the palette goes in a pane.
struct Frame {
    x: usize,
    y: usize,
    width: usize,
    /// Rows of commands.
    items: usize,
}

impl Palette {
    pub fn new(prompt: &str, commands: Vec<Command>) -> Self {
        let shown = (0..commands.len()).collect();
        Self {
            prompt: prompt.to_owned(),
            query: Field::default(),
            commands,
            shown,
            selected: 0,
        }
    }

    pub fn key(&mut self, input: &KeyInput, rows: u16) -> Outcome {
        if input.action == KeyAction::Release {
            return Outcome::Nothing;
        }
        let page = frame(80, rows).items.max(1) as isize;
        let shift = input.mods.contains(Mods::SHIFT);
        match input.key {
            Key::Escape => Outcome::Close,
            Key::Enter => self.chosen().map_or(Outcome::Close, Outcome::Pick),
            Key::ArrowUp => self.move_by(-1),
            Key::ArrowDown => self.move_by(1),
            Key::Tab => self.move_by(if shift { -1 } else { 1 }),
            Key::PageUp => self.move_by(-page),
            Key::PageDown => self.move_by(page),
            _ => {
                let outcome = edited(self.query.edit(input));
                if outcome == Outcome::Edited {
                    self.filter();
                }
                outcome
            }
        }
    }

    pub fn paste(&mut self, text: &str) -> Outcome {
        let outcome = edited(Some(self.query.paste(text)));
        self.filter();
        outcome
    }

    fn chosen(&self) -> Option<Choice> {
        self.current().cloned()
    }

    /// The selected entry's choice.
    pub fn current(&self) -> Option<&Choice> {
        let i = *self.shown.get(self.selected)?;
        self.commands.get(i).map(|c| &c.choice)
    }

    /// Start on the entry for `choice`, if there is one.
    pub fn select(&mut self, choice: &Choice) {
        let same = |&i: &usize| self.commands.get(i).is_some_and(|c| c.choice == *choice);
        if let Some(at) = self.shown.iter().position(same) {
            self.selected = at;
        }
    }

    fn move_by(&mut self, n: isize) -> Outcome {
        let len = self.shown.len();
        if len == 0 {
            return Outcome::Nothing;
        }
        let at = (self.selected as isize + n).clamp(0, len as isize - 1);
        self.selected = at as usize;
        Outcome::Moved
    }

    fn filter(&mut self) {
        let query = self.query.text.as_str();
        let mut scored: Vec<(i32, usize)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| Some((score(&c.label, query)?, i)))
            .collect();
        // Stable: ties keep the list's order.
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.shown = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
    }

    /// The command under pane cell `(col, row)`, `None` outside the box,
    /// and `Some(None)` on its query row.
    pub fn at(&self, (cols, rows): (u16, u16), col: u16, row: u16) -> Option<Option<Choice>> {
        let f = frame(cols, rows);
        let (col, row) = (usize::from(col), usize::from(row));
        if !(f.x..f.x + f.width).contains(&col) || !(f.y..=f.y + f.items).contains(&row) {
            return None;
        }
        if row == f.y {
            return Some(None);
        }
        let first = self.scroll(f.items);
        let i = *self.shown.get(first + row - f.y - 1)?;
        Some(self.commands.get(i).map(|c| c.choice.clone()))
    }

    /// The first shown command that's in view: the selection stays on
    /// screen.
    fn scroll(&self, items: usize) -> usize {
        (self.selected + 1).saturating_sub(items)
    }

    /// Draw the palette over the top of `snapshot`, with the cursor in its
    /// query when `typing`.
    pub fn draw(&self, snapshot: &mut Snapshot, colors: Colors, typing: bool) {
        let f = frame(snapshot.cols, snapshot.rows);
        if f.width == 0 {
            return;
        }
        let prompt = match self.prompt.as_str() {
            "" => " > ".to_owned(),
            asked => format!(" {asked} > "),
        };
        let prompt = prompt.as_str();
        let mut rows = Vec::new();
        let mut input = Line::default();
        input.push_text(prompt, Line::text_width(prompt), colors.cell(false, true));
        let field = f.width.saturating_sub(Line::text_width(prompt));
        let (query, at) = self.query.view(field);
        input.push_text(query, field, colors.cell(false, false));
        rows.push(input);
        let first = self.scroll(f.items);
        for (n, &i) in self.shown.iter().enumerate().skip(first).take(f.items) {
            let Some(command) = self.commands.get(i) else {
                continue;
            };
            let active = n == self.selected;
            let hint_w = Line::text_width(&command.hint).min(f.width / 2);
            let label_w = f.width.saturating_sub(hint_w + 3);
            let mut line = Line::default();
            line.push_text(" ", 1, colors.cell(active, false));
            line.push_text(&command.label, label_w, colors.cell(active, false));
            line.push_text(" ", 1, colors.cell(active, false));
            line.push_text(&command.hint, hint_w, colors.cell(active, true));
            line.push_text(" ", 1, colors.cell(active, false));
            rows.push(line);
        }
        if self.shown.is_empty() {
            let none = "  nothing matches";
            rows.push(Line::from_text(none, f.width, colors.cell(false, true)));
        }
        for (row, line) in rows.iter().enumerate() {
            if let Some(under) = snapshot.lines.get_mut(f.y + row) {
                *under = Arc::new(under.with_overlay(f.x, line));
            }
        }
        let x = f.x + Line::text_width(prompt) + at;
        snapshot.cursor = typing.then(|| Cursor {
            x: u16::try_from(x).unwrap_or(0),
            y: u16::try_from(f.y).unwrap_or(0),
            shape: CursorShape::Bar,
            blinking: false,
        });
    }
}

fn frame(cols: u16, rows: u16) -> Frame {
    let (cols, rows) = (usize::from(cols), usize::from(rows));
    let width = PALETTE_WIDTH.min(cols.saturating_sub(4)).max(cols.min(20));
    let y = usize::from(rows > 6);
    Frame {
        x: (cols - width) / 2,
        y,
        width,
        items: PALETTE_ROWS.min(rows.saturating_sub(y + 2)),
    }
}

fn edited(changed: Option<bool>) -> Outcome {
    match changed {
        Some(true) => Outcome::Edited,
        Some(false) => Outcome::Moved,
        None => Outcome::Nothing,
    }
}

/// The end of `text` that fits `cells`.
fn tail(text: &str, cells: usize) -> &str {
    let mut start = text.len();
    let mut used = 0;
    for (i, c) in text.char_indices().rev() {
        used += Line::text_width(c.encode_utf8(&mut [0; 4]));
        if used > cells {
            break;
        }
        start = i;
    }
    text.get(start..).unwrap_or_default()
}

/// How well `query` fuzzy-matches `label` (its chars in order, any case),
/// higher being better: runs and word starts count most. `None` for no
/// match.
fn score(label: &str, query: &str) -> Option<i32> {
    let mut want = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .peekable();
    let (mut score, mut run, mut prev) = (0, false, None::<char>);
    for c in label.chars() {
        let Some(&next) = want.peek() else {
            break;
        };
        let start = prev.is_none_or(|p| !p.is_alphanumeric());
        if c.to_lowercase().eq(std::iter::once(next)) {
            want.next();
            score += 1 + if run { 4 } else { 0 } + if start { 6 } else { 0 };
            run = true;
        } else {
            run = false;
        }
        prev = Some(c);
    }
    want.peek().is_none().then_some(score)
}

/// Color the search matches in view.
pub fn mark_matches(snapshot: &mut Snapshot) {
    let Some(matches) = snapshot.search.take() else {
        return;
    };
    for found in &matches.visible {
        let Some(line) = snapshot.lines.get_mut(usize::from(found.row)) else {
            continue;
        };
        let mut marked = (**line).clone();
        let range = usize::from(found.cols.start)..usize::from(found.cols.end);
        for cell in marked.cells.get_mut(range).unwrap_or_default() {
            cell.bg = Some(if found.current { CURRENT_BG } else { MATCH_BG });
            cell.fg = Some(MATCH_FG);
            cell.flags.set(CellFlags::INVERSE, false);
            cell.flags.set(CellFlags::SELECTED, false);
            cell.flags.set(CellFlags::FAINT, false);
        }
        *line = Arc::new(marked);
    }
    snapshot.search = Some(matches);
}

#[cfg(test)]
mod tests {
    use jet_core::{Key, KeyAction, KeyInput, Mods};

    use crate::overlay::{Field, score, tail};

    fn key(field: &mut Field, key: Key, mods: Mods) -> Option<bool> {
        field.edit(&KeyInput {
            key,
            action: KeyAction::Press,
            mods,
            text: None,
            unshifted: None,
        })
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        assert!(score("Split right", "spr").is_some());
        assert_eq!(score("Split right", "xyz"), None);
        assert!(score("New tab", "nt") > score("Reset font", "nt"));
        assert!(score("Reset font", "nt").is_some());
        assert_eq!(score("anything", ""), Some(0));
    }

    #[test]
    fn long_queries_show_their_end() {
        assert_eq!(tail("hello world", 5), "world");
        assert_eq!(tail("hi", 5), "hi");
    }

    #[test]
    fn fields_edit_at_the_cursor_by_char_and_word() {
        let mut f = Field::new("git log --oneline");
        assert_eq!(key(&mut f, Key::ArrowLeft, Mods::CTRL), Some(false));
        assert_eq!(f.after(), "oneline");
        key(&mut f, Key::ArrowLeft, Mods::CTRL);
        assert_eq!(f.after(), "log --oneline");
        assert_eq!(key(&mut f, Key::Backspace, Mods::CTRL), Some(true));
        assert_eq!((f.text.as_str(), f.at), ("log --oneline", 0));
        key(&mut f, Key::ArrowRight, Mods::CTRL);
        assert_eq!(f.before(), "log");
        assert!(f.insert("ö"));
        key(&mut f, Key::ArrowLeft, Mods::empty());
        assert_eq!(key(&mut f, Key::Delete, Mods::empty()), Some(true));
        assert_eq!(f.text, "log --oneline");
        assert_eq!(key(&mut f, Key::Delete, Mods::CTRL), Some(true));
        assert_eq!(f.text, "log");
        // the view keeps the cursor in it
        f = Field::new("hello world");
        key(&mut f, Key::Home, Mods::empty());
        assert_eq!(f.view(5), ("hello world", 0));
        key(&mut f, Key::End, Mods::empty());
        assert_eq!(f.view(5), ("orld", 4));
    }
}
