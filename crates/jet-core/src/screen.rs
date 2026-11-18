//! The whole screen, scrollback included, in positions that count rows from
//! the top of history: for keyboard copy mode, command blocks, captures, and
//! saving a terminal to rebuild it in another process.

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::render::Dirty;
use libghostty_vt::screen::{CellWide, Screen, TrackedGridRef};
use libghostty_vt::selection::Selection;
use libghostty_vt::terminal::{Point, PointCoordinate, PointSpace, ScrollViewport};

use crate::Error;
use crate::Location;
use crate::vt::VtCore;

/// A position in the whole screen: `y` counts rows from the top of the
/// scrollback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScreenPos {
    pub y: u32,
    pub x: u16,
}

/// Where the viewport sits in the scrollable screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub total: u32,
    pub top: u32,
    pub rows: u32,
}

/// A position that follows its row through scrolling, scrollback pruning
/// and reflow, on the primary screen.
#[derive(Debug)]
pub struct Anchor(TrackedGridRef);

impl VtCore {
    /// Columns and rows.
    pub fn size(&self) -> Result<(u16, u16), Error> {
        Ok((self.term.cols()?, self.term.rows()?))
    }

    pub fn title(&self) -> &str {
        self.term.title().unwrap_or("")
    }

    /// Where the shell last said it is (OSC 7, OSC 9;9).
    pub fn location(&self) -> Option<Location> {
        let report = self.term.pwd().ok().filter(|p| !p.is_empty())?;
        Some(crate::vt::location_from_report(report))
    }

    /// Redraw every row next time, for changes the render state doesn't
    /// track (a selection, a cursor drawn somewhere else).
    pub fn invalidate(&mut self) -> Result<(), Error> {
        self.render.update(&self.term)?.set_dirty(Dirty::Full)?;
        Ok(())
    }

    pub fn viewport(&self) -> Result<Viewport, Error> {
        let bar = self.term.scrollbar()?;
        let clamp = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        Ok(Viewport {
            total: clamp(bar.total),
            top: clamp(bar.offset),
            rows: clamp(bar.len),
        })
    }

    pub fn scroll_to(&mut self, top: u32) {
        self.term.scroll_viewport(ScrollViewport::Row(top as usize));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_viewport(ScrollViewport::Bottom);
    }

    /// The terminal cursor in screen coordinates.
    pub fn screen_cursor(&self) -> Result<ScreenPos, Error> {
        let view = self.viewport()?;
        let rows = u32::from(self.term.rows()?);
        Ok(ScreenPos {
            x: self.term.cursor_x()?,
            y: view.total.saturating_sub(rows) + u32::from(self.term.cursor_y()?),
        })
    }

    /// An anchor at the cursor, unless the alternate screen is active.
    pub fn anchor_cursor(&self) -> Result<Option<Anchor>, Error> {
        if self.term.active_screen()? != Screen::Primary {
            return Ok(None);
        }
        let at = self.screen_cursor()?;
        let point = Point::Screen(PointCoordinate { x: at.x, y: at.y });
        Ok(Some(Anchor(self.term.track_grid_ref(point)?)))
    }

    /// Where an anchor is now. `None` once its row is gone, or while the
    /// alternate screen hides the primary one.
    pub fn anchor_pos(&self, anchor: &Anchor) -> Option<ScreenPos> {
        if self.term.active_screen().ok()? != Screen::Primary {
            return None;
        }
        let at = anchor.0.point(PointSpace::Screen).ok()??;
        Some(ScreenPos { y: at.y, x: at.x })
    }

    /// Select from `start` to `end` inclusive, or clear with `None`.
    pub fn set_selection(
        &mut self,
        range: Option<(ScreenPos, ScreenPos)>,
        rectangle: bool,
    ) -> Result<(), Error> {
        match range {
            Some((start, end)) => {
                let at = |p: ScreenPos| {
                    self.term
                        .grid_ref(Point::Screen(PointCoordinate { x: p.x, y: p.y }))
                };
                let selection = Selection::new(at(start)?, at(end)?, rectangle);
                self.term.set_selection(Some(&selection))?;
            }
            None => {
                self.term.set_selection(None)?;
            }
        }
        self.invalidate()
    }

    /// Plain text from `start` up to `end`, joining soft-wrapped rows and
    /// trimming trailing blanks.
    pub fn text_between(&self, start: ScreenPos, end: ScreenPos) -> Result<String, Error> {
        let mut out = String::new();
        for y in start.y..=end.y {
            let chars = self.row_chars(y)?;
            let from = if y == start.y {
                usize::from(start.x)
            } else {
                0
            };
            let to = if y == end.y {
                usize::from(end.x)
            } else {
                chars.len()
            };
            let line: String = chars
                .get(from..to.min(chars.len()))
                .unwrap_or_default()
                .iter()
                .filter(|c| **c != '\0')
                .collect();
            out.push_str(line.trim_end());
            let wrapped = self
                .term
                .grid_ref(Point::Screen(PointCoordinate { x: 0, y }))?
                .row()?
                .is_wrapped()?;
            if y < end.y && !wrapped {
                out.push('\n');
            }
        }
        Ok(out.trim_end_matches('\n').to_owned())
    }

    /// The cells of screen row `y`: one char per column, `'\0'` for the
    /// spacer half of a wide char and `' '` for empty cells.
    pub fn row_chars(&self, y: u32) -> Result<Vec<char>, Error> {
        let cols = self.term.cols()?;
        let mut out = Vec::with_capacity(usize::from(cols));
        let mut buf = ['\0'; 16];
        for x in 0..cols {
            let cell = self
                .term
                .grid_ref(Point::Screen(PointCoordinate { x, y }))?;
            if matches!(
                cell.cell()?.wide()?,
                CellWide::SpacerTail | CellWide::SpacerHead
            ) {
                out.push('\0');
                continue;
            }
            let n = cell.graphemes(&mut buf).unwrap_or(0);
            out.push(if n == 0 { ' ' } else { buf[0] });
        }
        Ok(out)
    }

    /// The whole screen as plain text, one line per screen row.
    pub fn screen_lines(&self) -> Result<Vec<String>, Error> {
        let mut formatter = Formatter::new(
            &self.term,
            FormatterOptions::new()
                .with_format(Format::Plain)
                .with_unwrap(false)
                .with_trim(true),
        )?;
        let bytes = formatter.format_alloc(None)?;
        Ok(String::from_utf8_lossy(&bytes)
            .split('\n')
            .map(|l| l.trim_end_matches('\r').to_owned())
            .collect())
    }

    fn alt_screen(&self) -> Result<bool, Error> {
        Ok(self.term.active_screen()? == Screen::Alternate)
    }

    fn format_vt(&self, exact: bool) -> Result<String, Error> {
        let opts = FormatterOptions::new()
            .with_format(Format::Vt)
            .with_trim(true)
            .with_style(true)
            .with_hyperlink(true);
        let opts = if exact {
            opts.with_unwrap(false)
                .with_cursor(true)
                .with_modes(true)
                .with_scrolling_region(true)
                .with_tabstops(true)
                .with_pwd(true)
                .with_keyboard(true)
                .with_protection(true)
                .with_kitty_keyboard(true)
                .with_charsets(true)
        } else {
            // Unwrapped, so the lines reflow into a terminal of another width.
            opts.with_unwrap(true)
        };
        let bytes = Formatter::new(&self.term, opts)?.format_alloc(None)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The primary screen and its scrollback as VT, for painting back into
    /// a new terminal after a restart. `None` while an app holds the
    /// alternate screen, which hides the primary one from the formatter.
    pub fn history(&self) -> Result<Option<String>, Error> {
        if self.alt_screen()? {
            return Ok(None);
        }
        self.format_vt(false).map(Some)
    }

    /// Everything needed to rebuild this terminal in another process: the
    /// primary screen, and the alternate screen if it's active. Reading the
    /// primary screen means leaving the alternate one, so this is only for
    /// a terminal about to be dropped.
    pub fn state(&mut self) -> Result<(String, Option<String>), Error> {
        let alt = if self.alt_screen()? {
            let alt = self.format_vt(true)?;
            self.term.vt_write(b"\x1b[?1049l");
            Some(alt)
        } else {
            None
        };
        Ok((self.format_vt(true)?, alt))
    }

    /// Rebuild what [`VtCore::state`] saved, dropping the effects it has.
    pub fn restore_state(
        &mut self,
        primary: &str,
        alt: Option<&str>,
        title: &str,
    ) -> Result<(), Error> {
        self.write(primary.as_bytes());
        if let Some(alt) = alt {
            // The saved alternate screen switches to itself, but draws from
            // wherever the cursor is, and the switch doesn't move it. Switch
            // first (saving the primary cursor), then draw from home.
            const ENTER: &str = "\x1b[?1049h";
            self.write(ENTER.as_bytes());
            self.write(b"\x1b[H");
            self.write(alt.replacen(ENTER, "", 1).as_bytes());
        }
        if !title.is_empty() {
            let title = title.replace(['\x07', '\x1b'], "");
            self.write(format!("\x1b]2;{title}\x07").as_bytes());
        }
        self.take_effects().map(drop)
    }
}

#[cfg(test)]
mod tests {
    use crate::screen::ScreenPos;
    use crate::vt::VtCore;

    #[test]
    fn screen_lines_map_to_screen_rows() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(8, 3, 100)?;
        vt.write(b"one\r\n\r\nthree\r\nfour\r\nfive");
        assert_eq!(vt.viewport()?.total, 5);
        let lines = vt.screen_lines()?;
        assert_eq!(&lines[..5], ["one", "", "three", "four", "five"]);
        assert_eq!(vt.screen_cursor()?, ScreenPos { y: 4, x: 4 });
        assert_eq!(vt.row_chars(2)?[..5], ['t', 'h', 'r', 'e', 'e']);
        Ok(())
    }

    #[test]
    fn selection_by_screen_position_copies() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(8, 2, 100)?;
        vt.write(b"abc def\r\nghi");
        vt.set_selection(
            Some((ScreenPos { y: 0, x: 4 }, ScreenPos { y: 1, x: 1 })),
            false,
        )?;
        assert_eq!(vt.selection_text()?.as_deref(), Some("def\ngh"));
        Ok(())
    }

    #[test]
    fn exact_state_survives_the_alternate_screen() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(10, 3, 100)?;
        vt.write(b"one\r\ntwo\r\nthree\r\nfour\x1b[?1049h\x1b[Halt");
        let (primary, alt) = vt.state()?;
        let mut back = VtCore::new(10, 3, 100)?;
        back.restore_state(&primary, alt.as_deref(), "t")?;
        assert_eq!(back.title(), "t");
        assert_eq!(back.screen_lines()?[0], "alt");
        back.write(b"\x1b[?1049l");
        assert_eq!(&back.screen_lines()?[..4], ["one", "two", "three", "four"]);
        Ok(())
    }
}
