//! The remote client's screen as data: the session view and each visible
//! pane's grid. The server numbers versions of it and sends the
//! difference from whatever version the client last acknowledged, so a
//! lost datagram costs nothing but a resend.

use std::collections::{BTreeMap, HashMap};

use jetmux_proto::{Cursor, Frame, Modes, RowUpdate, Run, Size, Style, View};
use serde::{Deserialize, Serialize};

/// A run of same-style cells with its style resolved, so rows from
/// different frames compare directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub style: Style,
    pub text: String,
    /// As [`Run::cluster`].
    pub cluster: Option<u8>,
    pub link: Option<String>,
}

impl Span {
    pub fn cells(&self) -> usize {
        self.cluster
            .map_or_else(|| self.text.chars().count(), usize::from)
    }
}

pub type Row = Vec<Span>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneState {
    pub size: Size,
    pub rows: Vec<Row>,
    pub cursor: Option<Cursor>,
    pub modes: Modes,
}

impl PaneState {
    /// The text of the cell at `x`, `y`.
    pub fn cell(&self, x: u16, y: u16) -> Option<&str> {
        let mut at = 0;
        for span in self.rows.get(usize::from(y))? {
            if span.cluster.is_some() {
                if usize::from(x) < at + span.cells() {
                    return Some(&span.text);
                }
                at += span.cells();
                continue;
            }
            for (i, (offset, ch)) in span.text.char_indices().enumerate() {
                if at + i == usize::from(x) {
                    return span.text.get(offset..offset + ch.len_utf8());
                }
            }
            at += span.cells();
        }
        None
    }
}

/// Changes from one screen to another.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    pub view: Option<View>,
    pub panes: Vec<(u32, Frame)>,
    pub removed: Vec<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Screen {
    pub view: Option<View>,
    pub panes: BTreeMap<u32, PaneState>,
}

fn spans(frame: &Frame, row: &RowUpdate) -> Row {
    row.runs
        .iter()
        .map(|run| Span {
            style: frame
                .styles
                .get(usize::from(run.style))
                .copied()
                .unwrap_or_default(),
            text: run.text.clone(),
            cluster: run.cluster,
            link: usize::from(run.link)
                .checked_sub(1)
                .and_then(|i| frame.links.get(i))
                .cloned(),
        })
        .collect()
}

/// A frame of the given rows, with styles and links interned.
pub fn encode(
    size: Size,
    rows: Vec<(u16, Row)>,
    cursor: Option<Cursor>,
    modes: Modes,
    full: bool,
) -> Frame {
    let mut styles: HashMap<Style, u16> = HashMap::new();
    let mut links: HashMap<String, u16> = HashMap::new();
    let mut frame = Frame {
        full,
        size,
        cursor,
        modes,
        ..Frame::default()
    };
    for (y, row) in rows {
        let runs = row
            .into_iter()
            .map(|span| {
                let next = u16::try_from(styles.len()).unwrap_or(u16::MAX);
                let style = *styles.entry(span.style).or_insert(next);
                let link = match span.link {
                    Some(uri) => {
                        let next = u16::try_from(links.len() + 1).unwrap_or(u16::MAX);
                        *links.entry(uri).or_insert(next)
                    }
                    None => 0,
                };
                Run {
                    style,
                    text: span.text,
                    cluster: span.cluster,
                    link,
                }
            })
            .collect();
        frame.rows.push(RowUpdate { y, runs });
    }
    frame.styles = vec![Style::default(); styles.len()];
    for (style, id) in styles {
        frame.styles[usize::from(id)] = style;
    }
    frame.links = vec![String::new(); links.len()];
    for (uri, id) in links {
        frame.links[usize::from(id) - 1] = uri;
    }
    frame
}

impl Screen {
    /// Keep only the panes the view shows.
    pub fn set_view(&mut self, view: View) {
        self.panes
            .retain(|id, _| view.panes.iter().any(|p| p.id == *id));
        self.view = Some(view);
    }

    pub fn apply_frame(&mut self, pane: u32, frame: &Frame) {
        let state = self.panes.entry(pane).or_default();
        if frame.full || state.size != frame.size {
            state.rows = vec![Row::new(); usize::from(frame.size.rows)];
        }
        state.size = frame.size;
        state.rows.resize(usize::from(frame.size.rows), Row::new());
        for row in &frame.rows {
            if let Some(slot) = state.rows.get_mut(usize::from(row.y)) {
                *slot = spans(frame, row);
            }
        }
        state.cursor = frame.cursor;
        state.modes = frame.modes;
    }

    pub fn apply(&mut self, delta: &Delta) {
        if let Some(view) = &delta.view {
            self.view = Some(view.clone());
        }
        for (pane, frame) in &delta.panes {
            self.apply_frame(*pane, frame);
        }
        for pane in &delta.removed {
            self.panes.remove(pane);
        }
    }

    /// What turns `self` into `to`.
    pub fn diff(&self, to: &Screen) -> Delta {
        let mut delta = Delta {
            view: (self.view != to.view).then(|| to.view.clone()).flatten(),
            ..Delta::default()
        };
        for (id, new) in &to.panes {
            let old = self.panes.get(id).filter(|old| old.size == new.size);
            let rows: Vec<(u16, Row)> = (0..new.size.rows)
                .filter(|y| {
                    let row = |p: &PaneState| p.rows.get(usize::from(*y)).cloned();
                    old.is_none_or(|old| row(old) != row(new))
                })
                .map(|y| (y, new.rows.get(usize::from(y)).cloned().unwrap_or_default()))
                .collect();
            let unchanged =
                old.is_some_and(|old| old.cursor == new.cursor && old.modes == new.modes);
            if rows.is_empty() && unchanged {
                continue;
            }
            delta.panes.push((
                *id,
                encode(new.size, rows, new.cursor, new.modes, old.is_none()),
            ));
        }
        delta.removed = self
            .panes
            .keys()
            .filter(|id| !to.panes.contains_key(id))
            .copied()
            .collect();
        delta
    }
}

#[cfg(test)]
mod tests {
    use jetmux_proto::{
        Color, Cursor, CursorShape, Frame, PaneView, Rect, RowUpdate, Run, Size, Style, View,
    };

    use crate::state::Screen;

    fn frame(rows: &[&str], full: bool) -> Frame {
        Frame {
            full,
            size: Size { cols: 10, rows: 3 },
            styles: vec![
                Style::default(),
                Style {
                    fg: Color::Palette(1),
                    ..Style::default()
                },
            ],
            rows: rows
                .iter()
                .enumerate()
                .filter(|(_, text)| !text.is_empty())
                .map(|(y, text)| RowUpdate {
                    y: y as u16,
                    runs: vec![Run {
                        style: 1,
                        text: (*text).into(),
                        cluster: None,
                        link: 0,
                    }],
                })
                .collect(),
            cursor: Some(Cursor {
                x: 1,
                y: 0,
                shape: CursorShape::Block,
                blinking: false,
            }),
            ..Frame::default()
        }
    }

    fn view() -> View {
        View {
            panes: vec![PaneView {
                id: 1,
                rect: Rect {
                    x: 0,
                    y: 0,
                    cols: 10,
                    rows: 3,
                },
                ..PaneView::default()
            }],
            ..View::default()
        }
    }

    #[test]
    fn diffs_carry_only_changes_and_rebuild_the_screen() {
        let mut server = Screen::default();
        server.set_view(view());
        server.apply_frame(1, &frame(&["one", "two", "three"], true));
        let first = server.clone();
        server.apply_frame(1, &frame(&["", "TWO", ""], false));

        // From nothing, everything; from the first state, one row.
        let mut client = Screen::default();
        client.apply(&Screen::default().diff(&first));
        assert_eq!(client, first);
        let delta = first.diff(&server);
        assert!(delta.view.is_none());
        assert_eq!(delta.panes[0].1.rows.len(), 1);
        assert_eq!(delta.panes[0].1.styles[0].fg, Color::Palette(1));
        client.apply(&delta);
        assert_eq!(client, server);
        assert_eq!(client.panes[&1].cell(2, 1), Some("O"));
        assert!(server.diff(&server).panes.is_empty());

        // A pane leaving the view leaves the screen.
        server.set_view(View::default());
        let delta = client.diff(&server);
        assert_eq!(delta.removed, [1]);
        client.apply(&delta);
        assert_eq!(client, server);
    }
}
