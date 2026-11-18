//! A pane drawn from a jetmux daemon's frames instead of a terminal of its
//! own: the rows as sent, turned into [`Snapshot`]s in the viewer's colors.

use std::collections::HashMap;
use std::sync::Arc;

use jetmux_proto::{Color, Frame, FrameExtras, Size, Style};

use crate::snapshot::{
    Cell, CellFlags, Colors, Cursor, CursorShape, Line, Picture, Placement, Rgb, Snapshot,
    Underline, Width,
};
use crate::{Error, VtCore, VtSettings};

/// The colors a viewer draws frames in: its theme, every palette entry
/// worked out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameColors {
    pub colors: Colors,
    pub palette: [Rgb; 256],
}

impl FrameColors {
    pub fn new(settings: &VtSettings) -> Result<Self, Error> {
        let mut vt = VtCore::new(1, 1, 0)?;
        vt.configure(settings)?;
        Ok(Self {
            colors: Colors {
                foreground: settings.foreground,
                background: settings.background,
                cursor: settings.cursor,
            },
            palette: vt.theme.0.0.map(Rgb::from),
        })
    }

    /// These colors with what the app changed on top.
    fn with(&self, extras: &FrameExtras) -> Self {
        let app = &extras.colors;
        let mut out = self.clone();
        if let Some(c) = app.foreground {
            out.colors.foreground = c.into();
        }
        if let Some(c) = app.background {
            out.colors.background = c.into();
        }
        if let Some(c) = app.cursor {
            out.colors.cursor = Some(c.into());
        }
        for &(i, c) in &app.palette {
            out.palette[usize::from(i)] = c.into();
        }
        out
    }

    fn resolve(&self, color: Color) -> Option<Rgb> {
        match color {
            Color::Default => None,
            Color::Palette(i) => Some(self.palette[usize::from(i)]),
            Color::Rgb(r, g, b) => Some(Rgb { r, g, b }),
        }
    }
}

/// A run of cells with its style and link looked up.
#[derive(Clone, Debug)]
struct Run {
    style: Style,
    link: Option<Arc<str>>,
    text: String,
    cluster: Option<u8>,
}

#[derive(Debug, Default)]
pub struct Mirror {
    size: Size,
    rows: Vec<Vec<Run>>,
    lines: Vec<Arc<Line>>,
    dirty: Vec<bool>,
    cursor: Option<jetmux_proto::Cursor>,
    bracketed_paste: bool,
    extras: FrameExtras,
    /// The colors `lines` were built in.
    drawn: Option<FrameColors>,
    /// A frame came since the last snapshot.
    changed: bool,
}

impl Mirror {
    pub fn apply(&mut self, frame: Frame) {
        let rows = usize::from(frame.size.rows);
        if frame.full || frame.size != self.size {
            self.size = frame.size;
            self.rows = vec![Vec::new(); rows];
            self.lines = vec![Arc::default(); rows];
            self.dirty = vec![true; rows];
        }
        for row in frame.rows {
            let y = usize::from(row.y);
            let Some(slot) = self.rows.get_mut(y) else {
                continue;
            };
            *slot = row
                .runs
                .into_iter()
                .map(|run| Run {
                    style: frame
                        .styles
                        .get(usize::from(run.style))
                        .copied()
                        .unwrap_or_default(),
                    link: usize::from(run.link)
                        .checked_sub(1)
                        .and_then(|i| frame.links.get(i))
                        .map(|uri| Arc::from(uri.as_str())),
                    text: run.text,
                    cluster: run.cluster,
                })
                .collect();
            self.dirty[y] = true;
        }
        self.cursor = frame.cursor;
        self.bracketed_paste = frame.modes.bracketed_paste;
        self.extras = frame.extras;
        self.changed = true;
    }

    /// What changed since the last call, drawn in `theme` with the app's
    /// colors on top. `pictures` are those the daemon sent, and `cell` the
    /// viewer's cell size in pixels, which images scale to.
    pub fn snapshot(
        &mut self,
        theme: &FrameColors,
        pictures: &HashMap<u64, Arc<Picture>>,
        cell: (u32, u32),
    ) -> Option<Snapshot> {
        let colors = theme.with(&self.extras);
        let recolor = self.drawn.as_ref() != Some(&colors);
        if !self.changed && !recolor {
            return None;
        }
        self.changed = false;
        for (y, runs) in self.rows.iter().enumerate() {
            if recolor || self.dirty[y] {
                self.lines[y] = Arc::new(line(runs, &colors));
                self.dirty[y] = false;
            }
        }
        let [cw, ch] = self.extras.cell.map(|c| i64::from(c.max(1)));
        let (vw, vh) = (i64::from(cell.0.max(1)), i64::from(cell.1.max(1)));
        let scale = |v: i64, to: i64, from: i64| v * to / from;
        let images = self
            .extras
            .images
            .iter()
            .filter_map(|p| {
                Some(Placement {
                    picture: pictures.get(&p.picture)?.clone(),
                    x: scale(p.x, vw, cw),
                    y: scale(p.y, vh, ch),
                    width: u32::try_from(scale(p.width.into(), vw, cw)).unwrap_or(0),
                    height: u32::try_from(scale(p.height.into(), vh, ch)).unwrap_or(0),
                    source: p.source,
                    z: p.z,
                })
            })
            .collect();
        let snapshot = Snapshot {
            cols: self.size.cols,
            rows: self.size.rows,
            lines: self.lines.clone(),
            cursor: self.cursor.map(|c| Cursor {
                x: c.x,
                y: c.y,
                shape: match c.shape {
                    jetmux_proto::CursorShape::Block => CursorShape::Block,
                    jetmux_proto::CursorShape::BlockHollow => CursorShape::BlockHollow,
                    jetmux_proto::CursorShape::Bar => CursorShape::Bar,
                    jetmux_proto::CursorShape::Underline => CursorShape::Underline,
                },
                blinking: c.blinking,
            }),
            colors: colors.colors,
            images,
            search: self.extras.search.clone(),
            bracketed_paste: self.bracketed_paste,
        };
        self.drawn = Some(colors);
        Some(snapshot)
    }
}

fn line(runs: &[Run], colors: &FrameColors) -> Line {
    let mut line = Line::default();
    for run in runs {
        let style = &run.style;
        let mut flags = CellFlags::default();
        for (on, flag) in [
            (style.bold, CellFlags::BOLD),
            (style.italic, CellFlags::ITALIC),
            (style.faint, CellFlags::FAINT),
            (style.blink, CellFlags::BLINK),
            (style.inverse, CellFlags::INVERSE),
            (style.invisible, CellFlags::INVISIBLE),
            (style.strikethrough, CellFlags::STRIKETHROUGH),
            (style.overline, CellFlags::OVERLINE),
        ] {
            flags.set(flag, on);
        }
        let cell = Cell {
            fg: colors.resolve(style.fg),
            bg: colors.resolve(style.bg),
            flags,
            underline: match style.underline {
                jetmux_proto::Underline::None => Underline::None,
                jetmux_proto::Underline::Single => Underline::Single,
                jetmux_proto::Underline::Double => Underline::Double,
                jetmux_proto::Underline::Curly => Underline::Curly,
                jetmux_proto::Underline::Dotted => Underline::Dotted,
                jetmux_proto::Underline::Dashed => Underline::Dashed,
            },
            underline_color: colors.resolve(style.underline_color),
            ..Cell::default()
        };
        let start = line.cells.len();
        let blank = |text: &str| text == " ";
        match run.cluster {
            Some(2) => {
                let text = if blank(&run.text) { "" } else { &run.text };
                line.push(
                    Cell {
                        width: Width::Wide,
                        ..cell
                    },
                    text.chars(),
                );
                line.push(
                    Cell {
                        width: Width::Spacer,
                        ..cell
                    },
                    [],
                );
            }
            Some(_) => line.push(cell, run.text.chars()),
            None => {
                for c in run.text.chars() {
                    line.push(cell, (c != ' ').then_some(c));
                }
            }
        }
        if let Some(uri) = &run.link {
            for x in start..line.cells.len() {
                line.link(u16::try_from(x).unwrap_or(u16::MAX), uri);
            }
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::mirror::{FrameColors, Mirror};
    use crate::snapshot::{CellFlags, Rgb, Width};
    use crate::{VtCore, VtSettings};

    #[test]
    fn frames_become_snapshots_in_the_viewers_colors() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(10, 2, 100)?;
        vt.write("\x1b[1;31mred\x1b[0m 漢\r\n\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\".as_bytes());
        let mut mirror = Mirror::default();
        mirror.apply(vt.frame(true)?);
        let settings = VtSettings {
            palette: vec![(1, Rgb { r: 1, g: 2, b: 3 })],
            ..VtSettings::default()
        };
        let theme = FrameColors::new(&settings)?;
        let snap = mirror
            .snapshot(&theme, &HashMap::new(), (8, 16))
            .ok_or("no snapshot")?;
        assert_eq!(snap.lines[0].text(), "red 漢");
        let red = snap.lines[0].cells[0];
        assert_eq!(red.fg, Some(Rgb { r: 1, g: 2, b: 3 }));
        assert!(red.flags.contains(CellFlags::BOLD));
        assert_eq!(snap.lines[0].cells[4].width, Width::Wide);
        assert_eq!(snap.lines[0].cells[5].width, Width::Spacer);
        assert_eq!(
            snap.lines[1].links.first().map(|l| (l.0.clone(), &*l.1)),
            Some((0..4, "https://x"))
        );
        // Nothing new, nothing to draw; a new theme redraws.
        assert!(mirror.snapshot(&theme, &HashMap::new(), (8, 16)).is_none());
        let theme = FrameColors::new(&VtSettings::default())?;
        let snap = mirror
            .snapshot(&theme, &HashMap::new(), (8, 16))
            .ok_or("no recolor")?;
        assert_eq!(snap.lines[0].cells[0].fg, Some(theme.palette[1]));
        Ok(())
    }
}
