//! Predictive echo: over a slow link, characters typed at a prompt appear
//! at once, underlined, instead of after a round trip. Each prediction is
//! confirmed when the server's screen shows that character in that cell,
//! or thrown out, with all the others, once the server says the input has
//! had time to echo and it didn't. Control keys stop prediction until the
//! server catches up, since what they do can't be guessed.

use jetmux_proto::{Cursor, Frame, Underline};

use crate::state::{PaneState, Row, Screen, Span, encode};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Prediction {
    pane: u32,
    x: u16,
    y: u16,
    ch: char,
    /// The input it came from.
    seq: u64,
}

#[derive(Debug, Default)]
pub struct Predictor {
    pending: Vec<Prediction>,
    /// Where the next character would land.
    cursor: Option<(u32, u16, u16)>,
    /// No new predictions until the echo of this input is in.
    paused_until: u64,
}

/// The pane typing goes to, unless it's in copy mode.
fn focused(screen: &Screen) -> Option<(u32, &PaneState)> {
    let view = screen.view.as_ref()?;
    let pane = view.panes.iter().find(|p| p.focused && !p.copy_mode)?;
    Some((pane.id, screen.panes.get(&pane.id)?))
}

impl Predictor {
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Input `seq` was typed. Returns the panes whose display changed.
    pub fn input(&mut self, screen: &Screen, seq: u64, data: &[u8], echo_ack: u64) -> Vec<u32> {
        let Ok(text) = std::str::from_utf8(data) else {
            self.pause(seq);
            return Vec::new();
        };
        let Some((id, pane)) = focused(screen) else {
            return Vec::new();
        };
        if echo_ack < self.paused_until {
            return Vec::new();
        }
        let start = match self.cursor {
            Some((p, x, y)) if p == id => (x, y),
            _ => match pane.cursor {
                Some(c) => (c.x, c.y),
                None => return Vec::new(),
            },
        };
        let (mut x, y) = start;
        let mut changed = false;
        for ch in text.chars() {
            // Narrow printable characters only; anything else, and the
            // right edge, could do something we can't foresee.
            let narrow =
                ch == ' ' || ch.is_ascii_graphic() || (!ch.is_control() && (ch as u32) < 0x1100);
            if !narrow || x + 1 >= pane.size.cols {
                self.pause(seq);
                break;
            }
            self.pending.push(Prediction {
                pane: id,
                x,
                y,
                ch,
                seq,
            });
            x += 1;
            changed = true;
        }
        if changed {
            self.cursor = Some((id, x, y));
            vec![id]
        } else {
            Vec::new()
        }
    }

    fn pause(&mut self, seq: u64) {
        self.paused_until = self.paused_until.max(seq);
        self.cursor = None;
    }

    /// The server's screen moved on (or its echo did): keep what's still
    /// pending, drop what's confirmed, and drop everything on a wrong
    /// guess. Returns the panes whose display changed.
    pub fn reconcile(&mut self, screen: &Screen, echo_ack: u64) -> Vec<u32> {
        let before: Vec<u32> = self.pending.iter().map(|p| p.pane).collect();
        let wrong = self.pending.iter().any(|p| {
            let shown = screen.panes.get(&p.pane).and_then(|s| s.cell(p.x, p.y));
            let mut buf = [0u8; 4];
            shown != Some(p.ch.encode_utf8(&mut buf)) && p.seq <= echo_ack
        });
        if wrong {
            let newest = self.pending.iter().map(|p| p.seq).max().unwrap_or(0);
            self.pending.clear();
            self.pause(newest);
        } else {
            self.pending.retain(|p| {
                let shown = screen.panes.get(&p.pane).and_then(|s| s.cell(p.x, p.y));
                let mut buf = [0u8; 4];
                shown != Some(p.ch.encode_utf8(&mut buf))
            });
        }
        if self.pending.is_empty() {
            self.cursor = None;
        }
        let mut changed = before;
        changed.dedup();
        changed
    }

    /// A pane's frame: `rows` as the server has them, plus any row with a
    /// prediction, with predictions drawn in and the cursor after them.
    pub fn frame(&self, screen: &Screen, pane: u32, rows: &[u16], full: bool) -> Option<Frame> {
        let state = screen.panes.get(&pane)?;
        let mut ys: Vec<u16> = if full {
            (0..state.size.rows).collect()
        } else {
            rows.to_vec()
        };
        ys.extend(self.pending.iter().filter(|p| p.pane == pane).map(|p| p.y));
        ys.sort_unstable();
        ys.dedup();
        let rows = ys
            .into_iter()
            .map(|y| (y, self.overlay(state, pane, y)))
            .collect();
        let cursor = match self.cursor {
            Some((p, x, y)) if p == pane && !self.pending.is_empty() => {
                state.cursor.map(|c| Cursor { x, y, ..c })
            }
            _ => state.cursor,
        };
        Some(encode(state.size, rows, cursor, state.modes, full))
    }

    /// Row `y` with this pane's predictions drawn in, underlined.
    fn overlay(&self, state: &PaneState, pane: u32, y: u16) -> Row {
        let row = state.rows.get(usize::from(y)).cloned().unwrap_or_default();
        let predicted: Vec<&Prediction> = self
            .pending
            .iter()
            .filter(|p| p.pane == pane && p.y == y)
            .collect();
        if predicted.is_empty() {
            return row;
        }
        // Flatten to cells, write the guesses in, and let `encode` regroup.
        let mut cells: Vec<Span> = Vec::new();
        for span in row {
            if span.cluster.is_some() {
                cells.push(span);
                continue;
            }
            cells.extend(span.text.chars().map(|ch| Span {
                text: ch.to_string(),
                ..span.clone()
            }));
        }
        for p in predicted {
            let mut x = 0;
            let mut slot = None;
            for (i, cell) in cells.iter().enumerate() {
                if x == usize::from(p.x) {
                    slot = Some(i);
                    break;
                }
                x += cell.cells();
            }
            let blank = Span {
                style: Default::default(),
                text: " ".into(),
                cluster: None,
                link: None,
            };
            while slot.is_none() && x <= usize::from(p.x) {
                cells.push(blank.clone());
                if x == usize::from(p.x) {
                    slot = Some(cells.len() - 1);
                }
                x += 1;
            }
            if let Some(cell) = slot.and_then(|i| cells.get_mut(i)) {
                cell.text = p.ch.to_string();
                cell.cluster = None;
                cell.style.underline = Underline::Single;
            }
        }
        // Merge neighbours that share a style back into runs.
        let mut out: Row = Vec::new();
        for cell in cells {
            match out.last_mut() {
                Some(last)
                    if last.cluster.is_none()
                        && cell.cluster.is_none()
                        && last.style == cell.style
                        && last.link == cell.link =>
                {
                    last.text.push_str(&cell.text);
                }
                _ => out.push(cell),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use jetmux_proto::{
        Cursor, CursorShape, Frame, PaneView, Rect, RowUpdate, Run, Size, Style, Underline, View,
    };

    use crate::predict::Predictor;
    use crate::state::Screen;

    fn screen(row: &str, cursor_x: u16) -> Screen {
        let mut s = Screen::default();
        s.set_view(View {
            panes: vec![PaneView {
                id: 1,
                focused: true,
                rect: Rect {
                    x: 0,
                    y: 0,
                    cols: 20,
                    rows: 2,
                },
                ..PaneView::default()
            }],
            ..View::default()
        });
        s.apply_frame(
            1,
            &Frame {
                full: true,
                size: Size { cols: 20, rows: 2 },
                styles: vec![Style::default()],
                rows: vec![RowUpdate {
                    y: 0,
                    runs: vec![Run {
                        style: 0,
                        text: row.into(),
                        cluster: None,
                        link: 0,
                    }],
                }],
                cursor: Some(Cursor {
                    x: cursor_x,
                    y: 0,
                    shape: CursorShape::Block,
                    blinking: false,
                }),
                ..Frame::default()
            },
        );
        s
    }

    fn text(frame: &Frame) -> String {
        frame.rows[0].runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn typing_shows_at_once_and_is_confirmed_by_the_echo() {
        let mut p = Predictor::default();
        let before = screen("$ ", 2);
        assert_eq!(p.input(&before, 1, b"ls", 0), [1]);
        let frame = p.frame(&before, 1, &[], false).unwrap();
        assert!(text(&frame).starts_with("$ ls"));
        assert_eq!(frame.cursor.map(|c| c.x), Some(4));
        let underlined = frame.rows[0]
            .runs
            .iter()
            .find(|r| r.text.starts_with('l'))
            .unwrap();
        assert_eq!(
            frame.styles[usize::from(underlined.style)].underline,
            Underline::Single
        );

        // The echo arrives: nothing is pending, and the server's row shows.
        let echoed = screen("$ ls", 4);
        p.reconcile(&echoed, 1);
        assert!(p.is_empty());
        let frame = p.frame(&echoed, 1, &[0], false).unwrap();
        assert!(frame.styles.iter().all(|s| s.underline == Underline::None));
    }

    #[test]
    fn wrong_guesses_are_dropped_and_control_keys_pause() {
        let mut p = Predictor::default();
        // A password prompt doesn't echo.
        let prompt = screen("Password: ", 10);
        p.input(&prompt, 1, b"hunter2", 0);
        assert!(!p.is_empty());
        p.reconcile(&prompt, 0);
        assert!(!p.is_empty(), "not echoed yet: keep waiting");
        p.reconcile(&prompt, 1);
        assert!(p.is_empty(), "had time to echo and didn't");
        // Paused until the echo of input 1 is in; Enter pauses again.
        assert!(p.input(&prompt, 2, b"\r", 1).is_empty());
        assert!(p.input(&prompt, 3, b"x", 1).is_empty());
        assert_eq!(p.input(&prompt, 4, b"x", 2), [1]);
    }
}
