//! Turn a snapshot into GPU instance lists. Pure CPU work, no wgpu.
//!
//! Rows are built once and reused while their [`Line`] is unchanged:
//! snapshots share unchanged lines by `Arc`, so pointer equality says a row
//! needs no work. Typing rebuilds a row or two, not the screen.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use jet_core::{CellFlags, Colors, CursorShape, Line, Rgb, Snapshot, Underline, Width};
use jet_font::{CellMetrics, Decoration, Face, FontId, Fonts, ShapedGlyph};

/// A filled rectangle, in pixels from the target's top-left.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct Quad {
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub(crate) struct GlyphInstance {
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub uv_pos: [f32; 2],
    pub uv_size: [f32; 2],
    pub color: [f32; 4],
}

/// Instances in draw order: `under` rects, glyphs, color glyphs, `over`
/// rects.
#[derive(Default)]
struct Instances {
    under: Vec<Quad>,
    glyphs: Vec<GlyphInstance>,
    color_glyphs: Vec<GlyphInstance>,
    over: Vec<Quad>,
}

impl Instances {
    fn clear(&mut self) {
        self.under.clear();
        self.glyphs.clear();
        self.color_glyphs.clear();
        self.over.clear();
    }

    fn extend(&mut self, other: &Self) {
        self.under.extend_from_slice(&other.under);
        self.glyphs.extend_from_slice(&other.glyphs);
        self.color_glyphs.extend_from_slice(&other.color_glyphs);
        self.over.extend_from_slice(&other.over);
    }
}

/// One row's instances and what they were built from.
#[derive(Default)]
struct Row {
    line: Option<Arc<Line>>,
    /// Column of a block cursor drawn into this row's colors.
    block_cursor: Option<usize>,
    instances: Instances,
}

/// Everything rows depend on besides their line; a change rebuilds all rows.
#[derive(Clone, Copy, PartialEq)]
struct RowKey {
    metrics: CellMetrics,
    colors: Colors,
    selection: Selection,
    origin: [u32; 2],
}

/// How selected cells are drawn: unset colors tint the cell's background
/// and keep its text color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub background: Option<Rgb>,
    pub foreground: Option<Rgb>,
}

/// Instances for one frame. Public only so `jet-replay --bench` can time
/// building one without a GPU.
#[derive(Default)]
pub struct Frame {
    pub(crate) clear: [f32; 4],
    out: Instances,
    rows: Vec<Row>,
    key: Option<RowKey>,
    run_text: String,
    run_cols: Vec<(u32, u16)>,
    shaped: Vec<ShapedGlyph>,
    /// Per-cell colors of the row being built.
    colors: Vec<CellColors>,
    pub selection: Selection,
}

/// Final colors of one cell after inverse, selection and a block cursor.
#[derive(Clone, Copy)]
struct CellColors {
    fg: [f32; 4],
    /// `None` leaves the default background showing.
    bg: Option<[f32; 4]>,
}

/// The cell's text if it is exactly one character.
fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    chars.next().filter(|_| chars.next().is_none())
}

/// Selection tint, blended over each selected cell's background so text
/// keeps its own color.
const SELECTION: [f32; 3] = [0.30, 0.45, 0.95];
const SELECTION_ALPHA: f32 = 0.38;

fn mix(base: [f32; 4], tint: [f32; 3], t: f32) -> [f32; 4] {
    let [r, g, b, a] = base;
    [
        r + (tint[0] - r) * t,
        g + (tint[1] - g) * t,
        b + (tint[2] - b) * t,
        a,
    ]
}

fn rgba(c: Rgb, alpha: f32) -> [f32; 4] {
    [
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
        alpha,
    ]
}

/// What the helpers building one row share.
struct RowCtx<'a> {
    snap: &'a Snapshot,
    line: &'a Line,
    top: f32,
    left: f32,
    m: CellMetrics,
    block_cursor: Option<usize>,
    selection: Selection,
}

impl Frame {
    /// Rects plus glyphs in the frame.
    pub fn instances(&self) -> usize {
        let out = &self.out;
        out.under.len() + out.glyphs.len() + out.color_glyphs.len() + out.over.len()
    }

    pub(crate) fn under(&self) -> &[Quad] {
        &self.out.under
    }

    pub(crate) fn glyphs(&self) -> &[GlyphInstance] {
        &self.out.glyphs
    }

    pub(crate) fn color_glyphs(&self) -> &[GlyphInstance] {
        &self.out.color_glyphs
    }

    pub(crate) fn over(&self) -> &[Quad] {
        &self.out.over
    }

    /// Instances for `snap` with its first cell's top-left at `origin`.
    pub fn build(&mut self, snap: &Snapshot, fonts: &mut Fonts, origin: [u32; 2]) {
        let m = fonts.metrics();
        self.clear = rgba(snap.colors.background, 1.0);
        let key = RowKey {
            metrics: m,
            colors: snap.colors,
            selection: self.selection,
            origin,
        };
        if self.key != Some(key) {
            self.key = Some(key);
            self.rows.clear();
        }
        self.rows.resize_with(snap.lines.len(), Row::default);

        let block = snap
            .cursor
            .filter(|c| c.shape == CursorShape::Block)
            .map(|c| (usize::from(c.y), usize::from(c.x)));
        self.out.clear();
        for (y, line) in snap.lines.iter().enumerate() {
            let block_cursor = block.filter(|&(row, _)| row == y).map(|(_, x)| x);
            let Some(row) = self.rows.get_mut(y) else {
                continue;
            };
            let fresh = row.line.as_ref().is_some_and(|l| Arc::ptr_eq(l, line))
                && row.block_cursor == block_cursor;
            if !fresh {
                let mut instances = std::mem::take(&mut row.instances);
                instances.clear();
                let ctx = RowCtx {
                    snap,
                    line,
                    top: (origin[1] + y as u32 * m.height) as f32,
                    left: origin[0] as f32,
                    m,
                    block_cursor,
                    selection: self.selection,
                };
                self.build_row(&ctx, fonts, &mut instances);
                if let Some(row) = self.rows.get_mut(y) {
                    *row = Row {
                        line: Some(Arc::clone(line)),
                        block_cursor,
                        instances,
                    };
                }
            }
            if let Some(row) = self.rows.get(y) {
                self.out.extend(&row.instances);
            }
        }
        self.cursor(snap, origin, m);
    }

    fn build_row(&mut self, ctx: &RowCtx<'_>, fonts: &mut Fonts, out: &mut Instances) {
        let (snap, line, m) = (ctx.snap, ctx.line, ctx.m);
        let cw = m.width as f32;
        let mut colors = std::mem::take(&mut self.colors);
        colors.clear();
        colors.extend(line.cells.iter().enumerate().map(|(x, cell)| {
            let fg = cell.fg.unwrap_or(snap.colors.foreground);
            let inverse = cell.flags.contains(CellFlags::INVERSE);
            let (mut fg, mut bg) = if inverse {
                (cell.bg.unwrap_or(snap.colors.background), Some(fg))
            } else {
                (fg, cell.bg)
            };
            if ctx.block_cursor == Some(x) {
                let cursor = snap.colors.cursor.unwrap_or(snap.colors.foreground);
                fg = bg.unwrap_or(snap.colors.background);
                bg = Some(cursor);
            }
            let alpha = if cell.flags.contains(CellFlags::FAINT) {
                0.5
            } else {
                1.0
            };
            let mut bg = bg.map(|c| rgba(c, 1.0));
            if cell.flags.contains(CellFlags::SELECTED) {
                let under = bg.unwrap_or_else(|| rgba(snap.colors.background, 1.0));
                bg = Some(match ctx.selection.background {
                    Some(c) => rgba(c, 1.0),
                    None => mix(under, SELECTION, SELECTION_ALPHA),
                });
                fg = ctx.selection.foreground.unwrap_or(fg);
            }
            CellColors {
                fg: rgba(fg, alpha),
                bg,
            }
        }));

        for (x, (cell, color)) in line.cells.iter().zip(&colors).enumerate() {
            let left = ctx.left + x as f32 * cw;
            let span = if cell.width == Width::Wide { 2.0 } else { 1.0 };
            if let Some(bg) = color.bg
                && cell.width != Width::Spacer
            {
                out.under.push(Quad {
                    pos: [left, ctx.top],
                    size: [cw * span, m.height as f32],
                    color: bg,
                });
            }
            // A wide cell's spacer is covered by the cell itself.
            if cell.width == Width::Spacer {
                continue;
            }
            let thick = m.underline_thickness as f32;
            let line_at = |y_off: u32, color: [f32; 4], rects: &mut Vec<Quad>| {
                rects.push(Quad {
                    pos: [left, ctx.top + y_off as f32],
                    size: [cw * span, thick],
                    color,
                });
            };
            let underline = cell
                .underline_color
                .map_or(color.fg, |c| rgba(c, color.fg[3]));
            let decoration = match cell.underline {
                Underline::None => None,
                Underline::Single => {
                    line_at(m.underline_y, underline, &mut out.over);
                    None
                }
                Underline::Double => {
                    line_at(m.underline_y, underline, &mut out.over);
                    let below = m.underline_y + 2 * m.underline_thickness;
                    line_at(below, underline, &mut out.over);
                    None
                }
                Underline::Curly => Some(Decoration::Curly),
                Underline::Dotted => Some(Decoration::Dotted),
                Underline::Dashed => Some(Decoration::Dashed),
            };
            // Cell-sized sprites that tile; one per cell a wide one covers.
            if let Some(slot) = decoration.and_then(|d| fonts.decoration(d)) {
                let atlas = fonts.atlas().size() as f32;
                for i in 0..span as u32 {
                    out.glyphs.push(GlyphInstance {
                        pos: [left + (i * m.width) as f32, ctx.top],
                        size: [slot.width as f32, slot.height as f32],
                        uv_pos: [slot.atlas_x as f32 / atlas, slot.atlas_y as f32 / atlas],
                        uv_size: [slot.width as f32 / atlas, slot.height as f32 / atlas],
                        color: underline,
                    });
                }
            }
            if cell.flags.contains(CellFlags::STRIKETHROUGH) {
                line_at(m.strikeout_y, color.fg, &mut out.over);
            }
        }

        // Shape runs of same-font cells and pin each cluster to its column.
        // Never-written cells draw nothing, so they end a run instead of
        // being shaped as spaces.
        let mut run_font = None;
        self.run_text.clear();
        self.run_cols.clear();
        for (x, cell) in line.cells.iter().enumerate() {
            if cell.width == Width::Spacer {
                continue;
            }
            let text = line.cell_text(x);
            if let Some(slot) = single_char(text)
                .filter(|&c| jet_font::is_sprite(c))
                .and_then(|c| fonts.sprite(c))
            {
                self.flush_run(run_font, ctx, &colors, fonts, out);
                run_font = None;
                if let Some(color) = colors.get(x)
                    && !cell.flags.contains(CellFlags::INVISIBLE)
                {
                    let atlas = fonts.atlas().size() as f32;
                    out.glyphs.push(GlyphInstance {
                        pos: [ctx.left + (x as u32 * m.width) as f32, ctx.top],
                        size: [slot.width as f32, slot.height as f32],
                        uv_pos: [slot.atlas_x as f32 / atlas, slot.atlas_y as f32 / atlas],
                        uv_size: [slot.width as f32 / atlas, slot.height as f32 / atlas],
                        color: color.fg,
                    });
                }
                continue;
            }
            let face = Face::new(
                cell.flags.contains(CellFlags::BOLD),
                cell.flags.contains(CellFlags::ITALIC),
            );
            // The first character picks the font; combining marks follow it.
            let font = fonts.font_for(face, text);
            if font.is_none() || run_font.is_some_and(|f| Some(f) != font) {
                self.flush_run(run_font, ctx, &colors, fonts, out);
                run_font = None;
            }
            if font.is_none() {
                continue;
            }
            run_font = font;
            let start = u32::try_from(self.run_text.len()).unwrap_or(u32::MAX);
            self.run_cols
                .push((start, u16::try_from(x).unwrap_or(u16::MAX)));
            self.run_text.push_str(text);
        }
        self.flush_run(run_font, ctx, &colors, fonts, out);
        self.colors = colors;
    }

    fn flush_run(
        &mut self,
        font: Option<FontId>,
        ctx: &RowCtx<'_>,
        colors: &[CellColors],
        fonts: &mut Fonts,
        out: &mut Instances,
    ) {
        let Some(font) = font else { return };
        let m = ctx.m;
        fonts.shape(font, &self.run_text, &mut self.shaped);
        let (atlas, color_atlas) = (
            fonts.atlas().size() as f32,
            fonts.color_atlas().size() as f32,
        );
        for g in &self.shaped {
            // The cluster's column: the last run cell starting at or before it.
            let idx = self
                .run_cols
                .partition_point(|&(start, _)| start <= g.cluster);
            let Some(&(_, col)) = idx.checked_sub(1).and_then(|i| self.run_cols.get(i)) else {
                continue;
            };
            let col = usize::from(col);
            let Some(cell) = ctx.line.cells.get(col) else {
                continue;
            };
            let (Some(color), false) = (colors.get(col), cell.flags.contains(CellFlags::INVISIBLE))
            else {
                continue;
            };
            let cells = if cell.width == Width::Wide { 2 } else { 1 };
            let Some(slot) = fonts.glyph(font, g.glyph_id, cells) else {
                continue;
            };
            let x = ctx.left + (col as u32 * m.width) as f32 + g.x + slot.left as f32;
            let y = ctx.top + m.baseline as f32 - g.y - slot.top as f32;
            // Color glyphs keep their own colors; only faint's alpha applies.
            let (list, size, tint) = if slot.color {
                let tint = [1.0, 1.0, 1.0, color.fg[3]];
                (&mut out.color_glyphs, color_atlas, tint)
            } else {
                (&mut out.glyphs, atlas, color.fg)
            };
            list.push(GlyphInstance {
                pos: [x.round(), y.round()],
                size: [slot.width as f32, slot.height as f32],
                uv_pos: [slot.atlas_x as f32 / size, slot.atlas_y as f32 / size],
                uv_size: [slot.width as f32 / size, slot.height as f32 / size],
                color: tint,
            });
        }
        self.run_text.clear();
        self.run_cols.clear();
    }

    fn cursor(&mut self, snap: &Snapshot, origin: [u32; 2], m: CellMetrics) {
        let Some(cursor) = snap.cursor else { return };
        let left = origin[0] as f32 + f32::from(cursor.x) * m.width as f32;
        let top = origin[1] as f32 + f32::from(cursor.y) * m.height as f32;
        let wide = snap
            .lines
            .get(usize::from(cursor.y))
            .and_then(|l| l.cells.get(usize::from(cursor.x)))
            .is_some_and(|c| c.width == Width::Wide);
        let (w, h) = (
            m.width as f32 * if wide { 2.0 } else { 1.0 },
            m.height as f32,
        );
        let t = m.underline_thickness.max(1) as f32;
        let color = rgba(snap.colors.cursor.unwrap_or(snap.colors.foreground), 1.0);
        let mut rect = |x: f32, y: f32, w: f32, h: f32| {
            self.out.over.push(Quad {
                pos: [x, y],
                size: [w, h],
                color,
            });
        };
        match cursor.shape {
            // Drawn under the glyph as the cell background.
            CursorShape::Block => {}
            CursorShape::Bar => rect(left, top, t, h),
            CursorShape::Underline => rect(left, top + h - t, w, t),
            CursorShape::BlockHollow => {
                rect(left, top, w, t);
                rect(left, top + h - t, w, t);
                rect(left, top, t, h);
                rect(left + w - t, top, t, h);
            }
        }
    }
}
