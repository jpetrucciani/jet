//! jetmux's wire frames: the viewport as styled runs, with colors left as
//! palette indices so each client draws them in its own theme. A terminal
//! is drawn either as frames or as [`crate::Snapshot`]s, never both, since
//! each clears the render state's dirty rows as it goes.

use std::collections::HashMap;

use jetmux_proto::{
    AppColors, Color, Cursor, CursorShape, Frame, FrameExtras, ImagePlacement, Modes, MouseFormat,
    MouseTracking, RowUpdate, Run, Size, Style, Underline,
};
use libghostty_vt::render::{self, CursorVisualStyle, Dirty};
use libghostty_vt::screen::{CellContentTag, CellWide};
use libghostty_vt::style::{self as gstyle, StyleColor};
use libghostty_vt::terminal::{Mode, Point, PointCoordinate};

use crate::Error;
use crate::snapshot::Rgb;
use crate::vt::{VtCore, hyperlink_uri};

impl VtCore {
    /// Build a frame.
    ///
    /// A diff frame (`full == false`) holds the rows that changed since the
    /// previous diff and clears dirty state. A full frame holds every row and
    /// leaves dirty state alone, so building one for a newly attached client
    /// doesn't steal rows from the diff the other clients still need.
    pub fn frame(&mut self, full: bool) -> Result<Frame, Error> {
        let modes = self.modes()?;
        // Copy mode shows its own cursor instead.
        let copy_cursor = self.copy_state().map(|c| c.viewport_cursor(self));
        let mut extras = self.extras()?;
        let Self {
            term,
            render,
            rows,
            cells,
            uri_buf,
            ..
        } = self;
        let snapshot = render.update(term)?;
        let dirty = snapshot.dirty()?;
        extras.colors = app_colors(&snapshot.colors()?, &self.theme);
        let size = Size {
            cols: snapshot.cols()?,
            rows: snapshot.rows()?,
        };
        let cursor = if let Some(at) = copy_cursor {
            at.map(|(x, y)| Cursor {
                x,
                y,
                shape: CursorShape::Block,
                blinking: false,
            })
        } else if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|c| Cursor {
                x: c.x,
                y: c.y,
                shape: match snapshot.cursor_visual_style() {
                    Ok(CursorVisualStyle::Bar) => CursorShape::Bar,
                    Ok(CursorVisualStyle::Underline) => CursorShape::Underline,
                    Ok(CursorVisualStyle::BlockHollow) => CursorShape::BlockHollow,
                    _ => CursorShape::Block,
                },
                blinking: snapshot.cursor_blinking().unwrap_or(false),
            })
        } else {
            None
        };

        let mut frame = Frame {
            full: full || dirty == Dirty::Full,
            size,
            cursor,
            modes,
            extras,
            ..Frame::default()
        };
        if !full && dirty == Dirty::Clean {
            return Ok(frame);
        }

        let mut styles: HashMap<Style, u16> = HashMap::new();
        let mut links: HashMap<String, u16> = HashMap::new();
        let mut grapheme = String::new();
        let mut row_iter = rows.update(&snapshot)?;
        let mut y = 0u16;
        while let Some(row) = row_iter.next() {
            if frame.full || row.dirty()? {
                let selected = row.selection()?.map(|s| s.start_x..=s.end_x);
                let mut runs: Vec<Run> = Vec::new();
                let mut cell_iter = cells.update(row)?;
                let mut x = 0u16;
                while let Some(cell) = cell_iter.next() {
                    let col = x;
                    x = x.saturating_add(1);
                    let raw = cell.raw_cell()?;
                    let wide = raw.wide()?;
                    if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                        continue;
                    }
                    let mut style = if cell.has_styling()? {
                        convert_style(cell.style()?)
                    } else {
                        Style::default()
                    };
                    match raw.content_tag()? {
                        CellContentTag::BgColorPalette => {
                            style.bg = Color::Palette(raw.bg_color_palette()?.0);
                        }
                        CellContentTag::BgColorRgb => {
                            let c = raw.bg_color_rgb()?;
                            style.bg = Color::Rgb(c.r, c.g, c.b);
                        }
                        CellContentTag::Codepoint | CellContentTag::CodepointGrapheme => {}
                    }
                    if selected.as_ref().is_some_and(|r| r.contains(&col)) {
                        style.inverse = !style.inverse;
                    }
                    let next_id = u16::try_from(styles.len()).unwrap_or(u16::MAX);
                    let id = *styles.entry(style).or_insert(next_id);
                    let link = if raw.has_hyperlink()? {
                        let at = term.grid_ref(Point::Viewport(PointCoordinate {
                            x: col,
                            y: u32::from(y),
                        }))?;
                        let uri = hyperlink_uri(&at, uri_buf)?.unwrap_or_default().to_owned();
                        let next = u16::try_from(links.len() + 1).unwrap_or(u16::MAX);
                        *links.entry(uri).or_insert(next)
                    } else {
                        0
                    };

                    grapheme.clear();
                    cell.graphemes_utf8(&mut grapheme)?;
                    if grapheme.is_empty() {
                        grapheme.push(' ');
                    }
                    let cluster = if wide == CellWide::Wide {
                        Some(2)
                    } else if grapheme.chars().nth(1).is_some() {
                        Some(1)
                    } else {
                        None
                    };
                    match runs.last_mut() {
                        Some(run)
                            if cluster.is_none()
                                && run.cluster.is_none()
                                && run.style == id
                                && run.link == link =>
                        {
                            run.text.push_str(&grapheme);
                        }
                        _ => runs.push(Run {
                            style: id,
                            text: grapheme.clone(),
                            cluster,
                            link,
                        }),
                    }
                }
                if !full {
                    row.set_dirty(false)?;
                }
                frame.rows.push(RowUpdate { y, runs });
            }
            y = y.saturating_add(1);
        }
        if !full {
            snapshot.set_dirty(Dirty::Clean)?;
        }

        frame.styles = vec![Style::default(); styles.len()];
        for (style, id) in styles {
            frame.styles[usize::from(id)] = style;
        }
        frame.links = vec![String::new(); links.len()];
        for (uri, id) in links {
            frame.links[usize::from(id) - 1] = uri;
        }
        Ok(frame)
    }

    /// Images and search; colors come with the render state.
    fn extras(&mut self) -> Result<FrameExtras, Error> {
        let images = self
            .pictures
            .placements(&self.term, self.cell_size)?
            .into_iter()
            .map(|p| ImagePlacement {
                picture: p.picture.key,
                x: p.x,
                y: p.y,
                width: p.width,
                height: p.height,
                source: p.source,
                z: p.z,
            })
            .collect();
        let search = match &mut self.search {
            Some(search) => {
                if search.stale {
                    search.refresh(&self.term)?;
                }
                Some(search.matches(&self.term)?)
            }
            None => None,
        };
        Ok(FrameExtras {
            images,
            cell: [self.cell_size.0, self.cell_size.1],
            colors: AppColors::default(),
            search,
        })
    }

    fn modes(&self) -> Result<Modes, Error> {
        let on = |mode| self.term.mode(mode);
        let mouse = if on(Mode::ANY_MOUSE)? {
            MouseTracking::Any
        } else if on(Mode::BUTTON_MOUSE)? {
            MouseTracking::Button
        } else if on(Mode::NORMAL_MOUSE)? {
            MouseTracking::Normal
        } else if on(Mode::X10_MOUSE)? {
            MouseTracking::X10
        } else {
            MouseTracking::Off
        };
        let mouse_format = if on(Mode::SGR_PIXELS_MOUSE)? {
            MouseFormat::SgrPixels
        } else if on(Mode::SGR_MOUSE)? {
            MouseFormat::Sgr
        } else if on(Mode::URXVT_MOUSE)? {
            MouseFormat::Urxvt
        } else if on(Mode::UTF8_MOUSE)? {
            MouseFormat::Utf8
        } else {
            MouseFormat::X10
        };
        Ok(Modes {
            app_cursor: on(Mode::DECCKM)?,
            app_keypad: on(Mode::KEYPAD_KEYS)?,
            bracketed_paste: on(Mode::BRACKETED_PASTE)?,
            focus_events: on(Mode::FOCUS_EVENT)?,
            mouse,
            mouse_format,
            kitty_keyboard: self.term.kitty_keyboard_flags()?.bits(),
        })
    }
}

/// What the app changed of the colors `configure` set.
fn app_colors(
    now: &render::Colors,
    (palette, settings): &(libghostty_vt::style::Palette, crate::VtSettings),
) -> AppColors {
    let changed = |now: libghostty_vt::style::RgbColor, theme: Rgb| {
        let now = Rgb::from(now);
        (now != theme).then(|| now.into())
    };
    AppColors {
        foreground: changed(now.foreground, settings.foreground),
        background: changed(now.background, settings.background),
        cursor: now
            .cursor
            .map(Rgb::from)
            .filter(|&c| Some(c) != settings.cursor)
            .map(Into::into),
        palette: (0..=255u8)
            .filter_map(|i| {
                let (now, theme) = (now.palette[usize::from(i)], palette.0[usize::from(i)]);
                (Rgb::from(now) != Rgb::from(theme)).then(|| (i, Rgb::from(now).into()))
            })
            .collect(),
    }
}

fn convert_color(color: StyleColor) -> Color {
    match color {
        StyleColor::None => Color::Default,
        StyleColor::Palette(idx) => Color::Palette(idx.0),
        StyleColor::Rgb(c) => Color::Rgb(c.r, c.g, c.b),
    }
}

fn convert_style(s: gstyle::Style) -> Style {
    Style {
        fg: convert_color(s.fg_color),
        bg: convert_color(s.bg_color),
        underline_color: convert_color(s.underline_color),
        underline: match s.underline {
            gstyle::Underline::None => Underline::None,
            gstyle::Underline::Double => Underline::Double,
            gstyle::Underline::Curly => Underline::Curly,
            gstyle::Underline::Dotted => Underline::Dotted,
            gstyle::Underline::Dashed => Underline::Dashed,
            // Single, plus any style newer than this match.
            _ => Underline::Single,
        },
        bold: s.bold,
        faint: s.faint,
        italic: s.italic,
        blink: s.blink,
        inverse: s.inverse,
        invisible: s.invisible,
        strikethrough: s.strikethrough,
        overline: s.overline,
    }
}

#[cfg(test)]
mod tests {
    use jetmux_proto::input::Rgb;
    use jetmux_proto::{Color, Frame, MouseTracking, Run};

    use crate::screen::ScreenPos;
    use crate::vt::VtCore;

    #[test]
    fn frames_carry_images_colors_and_search() -> Result<(), Box<dyn std::error::Error>> {
        let mut vt = VtCore::new(20, 5, 100)?;
        vt.resize(20, 5, 8, 16)?;
        // A 1x1 red PNG two cells in, palette 1 and the background changed.
        vt.write(b"ab\x1b_Ga=T,f=100,q=2;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\");
        vt.write(b"\x1b]4;1;rgb:12/34/56\x07\x1b]11;rgb:01/02/03\x07 needle");
        vt.search("needle", false)?;
        let extras = vt.frame(false)?.extras;
        let [image] = extras.images.as_slice() else {
            return Err(format!("images: {:?}", extras.images).into());
        };
        assert_eq!((image.x, image.y, extras.cell), (16, 0, [8, 16]));
        assert_eq!(
            vt.picture(image.picture).ok_or("no picture")?.rgba,
            [255, 0, 0, 255]
        );
        let rgb = |r, g, b| Rgb { r, g, b };
        assert_eq!(extras.colors.palette, [(1, rgb(0x12, 0x34, 0x56))]);
        assert_eq!(extras.colors.background, Some(rgb(1, 2, 3)));
        assert_eq!(extras.colors.foreground, None);
        assert_eq!(extras.search.ok_or("no search")?.total, 1);
        // The client's own theme doesn't count as the app's.
        let mut vt = VtCore::new(20, 5, 100)?;
        vt.configure(&crate::VtSettings {
            background: crate::Rgb { r: 9, g: 9, b: 9 },
            ..crate::VtSettings::default()
        })?;
        assert_eq!(vt.frame(true)?.extras.colors, Default::default());
        Ok(())
    }

    fn row_text(frame: &Frame, y: u16) -> String {
        frame
            .rows
            .iter()
            .find(|r| r.y == y)
            .map(|row| row.runs.iter().map(|r| r.text.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn diff_frames_carry_only_changed_rows() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(10, 3, 100)?;
        vt.write(b"hi\x1b[31mred\x1b[0m");
        let first = vt.frame(false)?;
        assert_eq!(row_text(&first, 0).trim_end(), "hired");
        let red = first.rows[0]
            .runs
            .iter()
            .find(|r| r.text.starts_with("red"));
        let style = red.map(|r| first.styles[usize::from(r.style)].fg);
        assert_eq!(style, Some(Color::Palette(1)), "palette stays an index");

        vt.write(b"\r\nnext");
        let second = vt.frame(false)?;
        assert!(!second.full);
        assert!(second.rows.iter().all(|r| r.y != 2));
        assert_eq!(row_text(&second, 1).trim_end(), "next");

        // A full frame for a new client must not consume the pending diff.
        vt.write(b"!");
        let full = vt.frame(true)?;
        assert!(full.full);
        assert_eq!(full.rows.len(), 3);
        assert_eq!(row_text(&vt.frame(false)?, 1).trim_end(), "next!");
        Ok(())
    }

    #[test]
    fn wide_chars_become_single_cell_clusters() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(6, 1, 0)?;
        vt.write("a日b".as_bytes());
        let frame = vt.frame(true)?;
        let runs = &frame.rows[0].runs;
        assert_eq!(runs[1].text, "日");
        assert_eq!(runs[1].cluster, Some(2));
        assert_eq!(runs.iter().map(Run::cells).sum::<usize>(), 6);
        Ok(())
    }

    #[test]
    fn selection_is_highlighted() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(8, 2, 100)?;
        vt.write(b"abc def\r\nghi");
        vt.frame(false)?;
        vt.set_selection(
            Some((ScreenPos { y: 0, x: 4 }, ScreenPos { y: 1, x: 1 })),
            false,
        )?;
        let frame = vt.frame(false)?;
        assert!(frame.full);
        let inverse: String = frame.rows[0]
            .runs
            .iter()
            .filter(|r| frame.styles[usize::from(r.style)].inverse)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(inverse.trim_end(), "def");
        Ok(())
    }

    #[test]
    fn restored_state_keeps_mouse_modes() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(10, 3, 100)?;
        vt.write(b"\x1b[?1049h\x1b[?1000h");
        let (primary, alt) = vt.state()?;
        let mut back = VtCore::new(10, 3, 100)?;
        back.restore_state(&primary, alt.as_deref(), "")?;
        assert_eq!(back.frame(true)?.modes.mouse, MouseTracking::Normal);
        Ok(())
    }
}
