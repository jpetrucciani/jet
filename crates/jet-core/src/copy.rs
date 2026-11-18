//! Copy mode: a vi-style cursor over the screen and history, with
//! selection, regex search and jumps between command blocks. It's driven
//! by [`CopyAction`]s, so each front end binds its own keys: jet's are
//! [`key_action`], jetmux maps its clients' bytes. Like tmux, it belongs
//! to the terminal, so every jetmux client looking at a pane sees it.

use jetmux_proto::{CopyAction, Motion, SelectionKind};
use libghostty_vt::key::{Key, Mods};
use regex::Regex;

use crate::Error;
use crate::blocks::Span;
use crate::grab::typed;
use crate::screen::ScreenPos;
use crate::vt::{KeyInput, VtCore};

#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    Stay,
    /// Leave copy mode.
    Exit,
    /// Leave copy mode with this text copied.
    Yank(String),
    /// Stay, with a note for the user.
    Message(String),
}

#[derive(Debug)]
pub(crate) struct CopyMode {
    cursor: ScreenPos,
    /// Column to return to when moving through shorter lines.
    want_x: u16,
    anchor: Option<(ScreenPos, SelectionKind)>,
    search: Option<(Regex, bool)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c == ' ' || c == '\0' || c == '\t' {
        Class::Blank
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

impl CopyMode {
    /// Starts at the terminal's cursor, or at the bottom left of the view
    /// when it's scrolled back.
    pub(crate) fn enter(vt: &mut VtCore) -> Result<Self, Error> {
        let view = vt.viewport()?;
        let cursor = if view.top + view.rows < view.total {
            ScreenPos {
                y: view.top + view.rows.saturating_sub(1),
                x: 0,
            }
        } else {
            vt.screen_cursor()?
        };
        let mode = Self {
            cursor,
            want_x: cursor.x,
            anchor: None,
            search: None,
        };
        vt.invalidate()?;
        Ok(mode)
    }

    pub(crate) fn leave(vt: &mut VtCore) -> Result<(), Error> {
        vt.set_selection(None, false)?;
        vt.scroll_to_bottom();
        vt.invalidate()
    }

    /// The copy cursor's cell in the viewport, if it's on screen.
    pub(crate) fn viewport_cursor(&self, vt: &VtCore) -> Option<(u16, u16)> {
        let view = vt.viewport().ok()?;
        let y = self.cursor.y.checked_sub(view.top)?;
        (y < view.rows).then(|| (self.cursor.x, u16::try_from(y).unwrap_or(u16::MAX)))
    }

    /// Apply `action`. `blocks` is where the pane's command blocks are, for
    /// the prompt and output actions.
    pub(crate) fn apply(
        &mut self,
        vt: &mut VtCore,
        action: CopyAction,
        blocks: &[Span],
    ) -> Result<CopyOutcome, Error> {
        let outcome = match action {
            CopyAction::Move(motion) => {
                self.motion(vt, motion)?;
                CopyOutcome::Stay
            }
            CopyAction::Scroll(rows) => {
                let view = vt.viewport()?;
                let max_top = view.total.saturating_sub(view.rows);
                let top = view.top.saturating_add_signed(rows).min(max_top);
                vt.scroll_to(top);
                self.cursor.y = self.cursor.y.clamp(top, top + view.rows.saturating_sub(1));
                CopyOutcome::Stay
            }
            CopyAction::Select(kind) => {
                self.anchor = match self.anchor {
                    Some((_, current)) if current == kind => None,
                    Some((anchor, _)) => Some((anchor, kind)),
                    None => Some((self.cursor, kind)),
                };
                CopyOutcome::Stay
            }
            CopyAction::SwapAnchor => {
                if let Some((anchor, _)) = &mut self.anchor {
                    std::mem::swap(anchor, &mut self.cursor);
                    self.want_x = self.cursor.x;
                }
                CopyOutcome::Stay
            }
            CopyAction::ClearSelection => {
                if self.anchor.take().is_none() {
                    return Ok(CopyOutcome::Exit);
                }
                CopyOutcome::Stay
            }
            CopyAction::Yank => {
                if self.anchor.is_none() {
                    self.anchor = Some((self.cursor, SelectionKind::Line));
                    self.sync_selection(vt)?;
                }
                return Ok(match vt.selection_text()? {
                    Some(text) => CopyOutcome::Yank(text),
                    None => CopyOutcome::Exit,
                });
            }
            CopyAction::Search { pattern, backward } => match Regex::new(&pattern) {
                Ok(re) => {
                    self.search = Some((re, backward));
                    self.find(vt, false)?
                }
                Err(e) => CopyOutcome::Message(format!("bad pattern: {e}")),
            },
            CopyAction::SearchNext => self.find(vt, false)?,
            CopyAction::SearchPrev => self.find(vt, true)?,
            CopyAction::Prompt { backward } => {
                let y = self.cursor.y;
                let target = if backward {
                    blocks.iter().rev().find(|b| b.prompt.y < y)
                } else {
                    blocks.iter().find(|b| b.prompt.y > y)
                };
                self.jump(target.map(|b| b.prompt), "no prompt")
            }
            CopyAction::PrevFailure => {
                let y = self.cursor.y;
                let failed = blocks
                    .iter()
                    .rev()
                    .find(|b| b.failed && b.output.is_some_and(|o| o.y < y));
                self.jump(failed.and_then(|b| b.output), "no failed command")
            }
            CopyAction::YankOutput => {
                let block = blocks.iter().rev().find(|b| b.prompt.y <= self.cursor.y);
                let Some(start) = block.and_then(|b| b.output) else {
                    return Ok(CopyOutcome::Message("no output here".into()));
                };
                let end = match block.and_then(|b| b.end) {
                    Some(end) => end,
                    None => vt.screen_cursor()?,
                };
                return Ok(CopyOutcome::Yank(vt.text_between(start, end)?));
            }
            CopyAction::Exit => return Ok(CopyOutcome::Exit),
        };
        self.follow(vt)?;
        self.sync_selection(vt)?;
        Ok(outcome)
    }

    fn motion(&mut self, vt: &VtCore, motion: Motion) -> Result<(), Error> {
        let view = vt.viewport()?;
        let last_row = view.total.saturating_sub(1);
        let cols = vt.size()?.0;
        let half = (view.rows / 2).max(1);
        let mut keep_want = false;
        match motion {
            Motion::Left => self.cursor.x = self.cursor.x.saturating_sub(1),
            Motion::Right => self.cursor.x = (self.cursor.x + 1).min(cols.saturating_sub(1)),
            Motion::Up
            | Motion::Down
            | Motion::HalfPageUp
            | Motion::HalfPageDown
            | Motion::PageUp
            | Motion::PageDown => {
                let delta: i64 = match motion {
                    Motion::Up => -1,
                    Motion::Down => 1,
                    Motion::HalfPageUp => -i64::from(half),
                    Motion::HalfPageDown => i64::from(half),
                    Motion::PageUp => -i64::from(view.rows),
                    _ => i64::from(view.rows),
                };
                let y = (i64::from(self.cursor.y) + delta).clamp(0, i64::from(last_row));
                self.cursor.y = u32::try_from(y).unwrap_or(0);
                self.cursor.x = self.want_x.min(cols.saturating_sub(1));
                keep_want = true;
            }
            Motion::LineStart => self.cursor.x = 0,
            Motion::FirstNonBlank => {
                let row = vt.row_chars(self.cursor.y)?;
                self.cursor.x = col(row
                    .iter()
                    .position(|c| class(*c) != Class::Blank)
                    .unwrap_or(0));
            }
            Motion::LineEnd => {
                let row = vt.row_chars(self.cursor.y)?;
                self.cursor.x = col(row
                    .iter()
                    .rposition(|c| class(*c) != Class::Blank)
                    .unwrap_or(0));
            }
            Motion::Top => self.cursor = ScreenPos { y: 0, x: 0 },
            Motion::Bottom => self.cursor = ScreenPos { y: last_row, x: 0 },
            Motion::ViewTop => self.cursor.y = view.top,
            Motion::ViewMiddle => self.cursor.y = view.top + view.rows.saturating_sub(1) / 2,
            Motion::ViewBottom => {
                self.cursor.y = (view.top + view.rows.saturating_sub(1)).min(last_row)
            }
            Motion::WordForward => self.cursor = self.word_forward(vt, last_row)?,
            Motion::WordBackward => self.cursor = self.word_backward(vt)?,
            Motion::WordEnd => self.cursor = self.word_end(vt, last_row)?,
        }
        if !keep_want {
            self.want_x = self.cursor.x;
        }
        Ok(())
    }

    /// vim `w`: past the current word, then past blanks (line ends count as
    /// blank).
    fn word_forward(&self, vt: &VtCore, last_row: u32) -> Result<ScreenPos, Error> {
        let mut walk = Walk::new(vt, self.cursor)?;
        let start = walk.class();
        if start != Class::Blank {
            while walk.class() == start {
                if !walk.forward(vt, last_row)? {
                    return Ok(walk.pos);
                }
            }
        }
        while walk.class() == Class::Blank {
            if !walk.forward(vt, last_row)? {
                break;
            }
        }
        Ok(walk.pos)
    }

    /// vim `e`: to the last char of the current or next word.
    fn word_end(&self, vt: &VtCore, last_row: u32) -> Result<ScreenPos, Error> {
        let mut walk = Walk::new(vt, self.cursor)?;
        if !walk.forward(vt, last_row)? {
            return Ok(walk.pos);
        }
        while walk.class() == Class::Blank {
            if !walk.forward(vt, last_row)? {
                return Ok(walk.pos);
            }
        }
        let class = walk.class();
        loop {
            let here = walk.pos;
            if !walk.forward(vt, last_row)? || walk.class() != class {
                return Ok(here);
            }
        }
    }

    /// vim `b`: to the first char of the current or previous word.
    fn word_backward(&self, vt: &VtCore) -> Result<ScreenPos, Error> {
        let mut walk = Walk::new(vt, self.cursor)?;
        if !walk.backward(vt)? {
            return Ok(walk.pos);
        }
        while walk.class() == Class::Blank {
            if !walk.backward(vt)? {
                return Ok(walk.pos);
            }
        }
        let class = walk.class();
        loop {
            let here = walk.pos;
            if !walk.backward(vt)? || walk.class() != class {
                return Ok(here);
            }
        }
    }

    /// Jump to the next match of the last search. `reverse` flips the
    /// search's direction (vim `N`).
    fn find(&mut self, vt: &VtCore, reverse: bool) -> Result<CopyOutcome, Error> {
        let Some((re, backward)) = &self.search else {
            return Ok(CopyOutcome::Message("no search".into()));
        };
        let backward = *backward != reverse;
        // Compare in (row, char offset) space, which is what regex gives us;
        // only the winning match is converted back to a column.
        let here = (
            usize::try_from(self.cursor.y).unwrap_or(usize::MAX),
            col_to_char(&vt.row_chars(self.cursor.y)?, self.cursor.x),
        );
        let lines = vt.screen_lines()?;
        let hits: Vec<(usize, usize)> = lines
            .iter()
            .enumerate()
            .flat_map(|(y, line)| {
                re.find_iter(line)
                    .map(move |m| (y, line[..m.start()].chars().count()))
            })
            .collect();
        let hit = if backward {
            hits.iter().rev().find(|h| **h < here).or(hits.last())
        } else {
            hits.iter().find(|h| **h > here).or(hits.first())
        };
        let found = match hit {
            Some(&(y, chars)) => {
                let y = u32::try_from(y).unwrap_or(u32::MAX);
                Some(ScreenPos {
                    y,
                    x: char_to_col(&vt.row_chars(y)?, chars),
                })
            }
            None => None,
        };
        Ok(match found {
            Some(pos) if pos == self.cursor => CopyOutcome::Message("only match".into()),
            Some(pos) => {
                self.cursor = pos;
                self.want_x = pos.x;
                CopyOutcome::Stay
            }
            None => CopyOutcome::Message("no match".into()),
        })
    }

    fn jump(&mut self, to: Option<ScreenPos>, missing: &str) -> CopyOutcome {
        match to {
            Some(pos) => {
                self.cursor = pos;
                self.want_x = pos.x;
                CopyOutcome::Stay
            }
            None => CopyOutcome::Message(missing.into()),
        }
    }

    /// Scroll so the cursor is visible.
    fn follow(&self, vt: &mut VtCore) -> Result<(), Error> {
        let view = vt.viewport()?;
        if self.cursor.y < view.top {
            vt.scroll_to(self.cursor.y);
        } else if self.cursor.y >= view.top + view.rows {
            vt.scroll_to(self.cursor.y + 1 - view.rows);
        }
        vt.invalidate()
    }

    fn sync_selection(&self, vt: &mut VtCore) -> Result<(), Error> {
        let Some((anchor, kind)) = self.anchor else {
            return vt.set_selection(None, false);
        };
        let (start, end) = if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        let (range, rectangle) = match kind {
            SelectionKind::Char => ((start, end), false),
            SelectionKind::Block => ((anchor, self.cursor), true),
            SelectionKind::Line => {
                let last = vt.size()?.0.saturating_sub(1);
                let line = (
                    ScreenPos { y: start.y, x: 0 },
                    ScreenPos { y: end.y, x: last },
                );
                (line, false)
            }
        };
        vt.set_selection(Some(range), rectangle)
    }
}

/// jet's copy-mode keys: vi's, as jetmux binds them, plus Ctrl+B for a
/// page up (jetmux's prefix there). Not the block jumps: jet doesn't
/// track blocks. `None` for keys that do nothing.
pub(crate) fn key_action(input: &KeyInput) -> Option<CopyAction> {
    let motion = |m| Some(CopyAction::Move(m));
    let select = |k| Some(CopyAction::Select(k));
    if input.mods.contains(Mods::CTRL) {
        return match input.unshifted? {
            'u' => motion(Motion::HalfPageUp),
            'd' => motion(Motion::HalfPageDown),
            'b' => motion(Motion::PageUp),
            'f' => motion(Motion::PageDown),
            'y' => Some(CopyAction::Scroll(-1)),
            'e' => Some(CopyAction::Scroll(1)),
            'v' => select(SelectionKind::Block),
            _ => None,
        };
    }
    match input.key {
        Key::Escape => return Some(CopyAction::ClearSelection),
        Key::Enter => return Some(CopyAction::Yank),
        Key::ArrowLeft => return motion(Motion::Left),
        Key::ArrowRight => return motion(Motion::Right),
        Key::ArrowUp => return motion(Motion::Up),
        Key::ArrowDown => return motion(Motion::Down),
        Key::PageUp => return motion(Motion::PageUp),
        Key::PageDown => return motion(Motion::PageDown),
        Key::Home => return motion(Motion::LineStart),
        Key::End => return motion(Motion::LineEnd),
        _ => {}
    }
    match typed(input)? {
        'h' => motion(Motion::Left),
        'j' => motion(Motion::Down),
        'k' => motion(Motion::Up),
        'l' => motion(Motion::Right),
        'w' => motion(Motion::WordForward),
        'b' => motion(Motion::WordBackward),
        'e' => motion(Motion::WordEnd),
        '0' => motion(Motion::LineStart),
        '^' => motion(Motion::FirstNonBlank),
        '$' => motion(Motion::LineEnd),
        'g' => motion(Motion::Top),
        'G' => motion(Motion::Bottom),
        'H' => motion(Motion::ViewTop),
        'M' => motion(Motion::ViewMiddle),
        'L' => motion(Motion::ViewBottom),
        'v' => select(SelectionKind::Char),
        'V' => select(SelectionKind::Line),
        'o' => Some(CopyAction::SwapAnchor),
        'y' => Some(CopyAction::Yank),
        'n' => Some(CopyAction::SearchNext),
        'N' => Some(CopyAction::SearchPrev),
        'q' => Some(CopyAction::Exit),
        _ => None,
    }
}

fn col(index: usize) -> u16 {
    u16::try_from(index).unwrap_or(u16::MAX)
}

/// Number of printed chars before column `x`.
fn col_to_char(row: &[char], x: u16) -> usize {
    row.iter()
        .take(usize::from(x))
        .filter(|c| **c != '\0')
        .count()
}

/// Column of the `chars`-th printed char in a row, skipping wide spacers.
fn char_to_col(row: &[char], chars: usize) -> u16 {
    let x = row
        .iter()
        .enumerate()
        .filter(|(_, c)| **c != '\0')
        .nth(chars)
        .map_or(row.len().saturating_sub(1), |(x, _)| x);
    col(x)
}

/// A cursor stepping through cells across row boundaries. A row end counts
/// as one blank between rows.
struct Walk {
    pos: ScreenPos,
    row: Vec<char>,
    at_eol: bool,
}

impl Walk {
    fn new(vt: &VtCore, pos: ScreenPos) -> Result<Self, Error> {
        Ok(Self {
            pos,
            row: vt.row_chars(pos.y)?,
            at_eol: false,
        })
    }

    fn class(&self) -> Class {
        if self.at_eol {
            return Class::Blank;
        }
        self.row
            .get(usize::from(self.pos.x))
            .map_or(Class::Blank, |c| class(*c))
    }

    fn forward(&mut self, vt: &VtCore, last_row: u32) -> Result<bool, Error> {
        if self.at_eol {
            self.at_eol = false;
            self.pos.y += 1;
            self.pos.x = 0;
            self.row = vt.row_chars(self.pos.y)?;
            return Ok(true);
        }
        if usize::from(self.pos.x) + 1 < self.row.len() {
            self.pos.x += 1;
            return Ok(true);
        }
        if self.pos.y >= last_row {
            return Ok(false);
        }
        self.at_eol = true;
        Ok(true)
    }

    fn backward(&mut self, vt: &VtCore) -> Result<bool, Error> {
        if self.pos.x > 0 && !self.at_eol {
            self.pos.x -= 1;
            return Ok(true);
        }
        if self.at_eol {
            self.at_eol = false;
            return Ok(true);
        }
        if self.pos.y == 0 {
            return Ok(false);
        }
        self.pos.y -= 1;
        self.row = vt.row_chars(self.pos.y)?;
        self.pos.x = col(self.row.len().saturating_sub(1));
        self.at_eol = true;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use jetmux_proto::{CopyAction, Motion, SelectionKind};

    use crate::blocks::Span;
    use crate::copy::{CopyMode, CopyOutcome};
    use crate::screen::ScreenPos;
    use crate::vt::VtCore;

    fn setup() -> (VtCore, CopyMode) {
        let mut vt = VtCore::new(20, 3, 100).unwrap();
        vt.write(b"alpha beta\r\nfoo.bar(baz)\r\nline three\r\nfour 4\r\nprompt$ ");
        let copy = CopyMode::enter(&mut vt).unwrap();
        (vt, copy)
    }

    fn mv(vt: &mut VtCore, copy: &mut CopyMode, m: Motion) -> ScreenPos {
        copy.apply(vt, CopyAction::Move(m), &[]).unwrap();
        copy.cursor
    }

    #[test]
    fn vi_motions_cross_lines_and_scroll() {
        let (mut vt, mut copy) = setup();
        assert_eq!(copy.cursor, ScreenPos { y: 4, x: 8 });
        assert_eq!(
            mv(&mut vt, &mut copy, Motion::Top),
            ScreenPos { y: 0, x: 0 }
        );
        // Scrolled up to follow the cursor into history.
        assert_eq!(vt.viewport().unwrap().top, 0);
        assert_eq!(
            mv(&mut vt, &mut copy, Motion::WordForward),
            ScreenPos { y: 0, x: 6 }
        );
        assert_eq!(
            mv(&mut vt, &mut copy, Motion::WordForward),
            ScreenPos { y: 1, x: 0 }
        );
        assert_eq!(
            mv(&mut vt, &mut copy, Motion::WordForward),
            ScreenPos { y: 1, x: 3 }
        );
        assert_eq!(
            mv(&mut vt, &mut copy, Motion::WordEnd),
            ScreenPos { y: 1, x: 6 }
        );
        assert_eq!(
            mv(&mut vt, &mut copy, Motion::WordBackward),
            ScreenPos { y: 1, x: 4 }
        );
        assert_eq!(
            mv(&mut vt, &mut copy, Motion::LineEnd),
            ScreenPos { y: 1, x: 11 }
        );
        assert_eq!(mv(&mut vt, &mut copy, Motion::Bottom).y, 4);
        assert_eq!(vt.viewport().unwrap().top, 2);
    }

    #[test]
    fn search_wraps_and_yank_copies_selection() {
        let (mut vt, mut copy) = setup();
        let search = |pattern: &str| CopyAction::Search {
            pattern: pattern.into(),
            backward: true,
        };
        assert_eq!(
            copy.apply(&mut vt, search(r"b\w+"), &[]).unwrap(),
            CopyOutcome::Stay
        );
        assert_eq!(copy.cursor, ScreenPos { y: 1, x: 8 });
        copy.apply(&mut vt, CopyAction::SearchNext, &[]).unwrap();
        assert_eq!(copy.cursor, ScreenPos { y: 1, x: 4 });
        copy.apply(&mut vt, CopyAction::SearchPrev, &[]).unwrap();
        assert_eq!(copy.cursor, ScreenPos { y: 1, x: 8 });
        assert_eq!(
            copy.apply(&mut vt, search("nope"), &[]).unwrap(),
            CopyOutcome::Message("no match".into())
        );

        copy.apply(&mut vt, CopyAction::Select(SelectionKind::Char), &[])
            .unwrap();
        mv(&mut vt, &mut copy, Motion::WordEnd);
        assert_eq!(
            copy.apply(&mut vt, CopyAction::Yank, &[]).unwrap(),
            CopyOutcome::Yank("baz".into())
        );
    }

    #[test]
    fn prompts_failures_and_block_output() {
        let (mut vt, mut copy) = setup();
        let at = |y| ScreenPos { y, x: 0 };
        // "alpha beta" ran and printed two lines; "line three" failed.
        let blocks = [
            Span {
                prompt: at(0),
                output: Some(at(1)),
                end: Some(at(2)),
                failed: false,
            },
            Span {
                prompt: at(2),
                output: Some(at(3)),
                end: Some(at(4)),
                failed: true,
            },
            Span {
                prompt: at(4),
                output: None,
                end: None,
                failed: false,
            },
        ];
        let mut run = |action| copy.apply(&mut vt, action, &blocks).unwrap();
        assert_eq!(
            run(CopyAction::Prompt { backward: true }),
            CopyOutcome::Stay
        );
        assert_eq!(
            run(CopyAction::PrevFailure),
            CopyOutcome::Message("no failed command".into())
        );
        assert_eq!(
            run(CopyAction::Prompt { backward: true }),
            CopyOutcome::Stay
        );
        assert_eq!(
            run(CopyAction::YankOutput),
            CopyOutcome::Yank("foo.bar(baz)".into())
        );
        assert_eq!(
            run(CopyAction::Prompt { backward: false }),
            CopyOutcome::Stay
        );
        assert_eq!(
            run(CopyAction::Prompt { backward: false }),
            CopyOutcome::Stay
        );
        assert_eq!(run(CopyAction::PrevFailure), CopyOutcome::Stay);
        assert_eq!(
            run(CopyAction::YankOutput),
            CopyOutcome::Yank("four 4".into())
        );
    }
}
