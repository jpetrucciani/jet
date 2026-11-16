//! Characters drawn from code instead of a font, sized to the cell so they
//! join seamlessly with their neighbors: box drawing, block elements,
//! braille and powerline separators. Fonts draw these at their own size,
//! which leaves gaps between rows once the cell is taller than the glyph.
//!
//! Each one is drawn once per cell size into the atlas, after which it costs
//! the same as any glyph.

use crate::CellMetrics;

/// A coverage bitmap the size of one cell.
pub struct Sprite {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Arm weights for box drawing: none, light, heavy, double.
const N: u8 = 0;
const L: u8 = 1;
const H: u8 = 2;
const D: u8 = 3;

/// Arms of U+2500..=U+257F as [up, right, down, left]. Dashes, arcs and
/// diagonals (all `None` here) are drawn separately.
#[rustfmt::skip]
const BOX: [Option<[u8; 4]>; 128] = [
    // 2500 ─ ━ │ ┃ ┄ ┅ ┆ ┇ ┈ ┉ ┊ ┋ ┌ ┍ ┎ ┏
    Some([N,L,N,L]), Some([N,H,N,H]), Some([L,N,L,N]), Some([H,N,H,N]),
    None, None, None, None, None, None, None, None,
    Some([N,L,L,N]), Some([N,H,L,N]), Some([N,L,H,N]), Some([N,H,H,N]),
    // 2510 ┐ ┑ ┒ ┓ └ ┕ ┖ ┗ ┘ ┙ ┚ ┛ ├ ┝ ┞ ┟
    Some([N,N,L,L]), Some([N,N,L,H]), Some([N,N,H,L]), Some([N,N,H,H]),
    Some([L,L,N,N]), Some([L,H,N,N]), Some([H,L,N,N]), Some([H,H,N,N]),
    Some([L,N,N,L]), Some([L,N,N,H]), Some([H,N,N,L]), Some([H,N,N,H]),
    Some([L,L,L,N]), Some([L,H,L,N]), Some([H,L,L,N]), Some([L,L,H,N]),
    // 2520 ┠ ┡ ┢ ┣ ┤ ┥ ┦ ┧ ┨ ┩ ┪ ┫ ┬ ┭ ┮ ┯
    Some([H,L,H,N]), Some([H,H,L,N]), Some([L,H,H,N]), Some([H,H,H,N]),
    Some([L,N,L,L]), Some([L,N,L,H]), Some([H,N,L,L]), Some([L,N,H,L]),
    Some([H,N,H,L]), Some([H,N,L,H]), Some([L,N,H,H]), Some([H,N,H,H]),
    Some([N,L,L,L]), Some([N,L,L,H]), Some([N,H,L,L]), Some([N,H,L,H]),
    // 2530 ┰ ┱ ┲ ┳ ┴ ┵ ┶ ┷ ┸ ┹ ┺ ┻ ┼ ┽ ┾ ┿
    Some([N,L,H,L]), Some([N,L,H,H]), Some([N,H,H,L]), Some([N,H,H,H]),
    Some([L,L,N,L]), Some([L,L,N,H]), Some([L,H,N,L]), Some([L,H,N,H]),
    Some([H,L,N,L]), Some([H,L,N,H]), Some([H,H,N,L]), Some([H,H,N,H]),
    Some([L,L,L,L]), Some([L,L,L,H]), Some([L,H,L,L]), Some([L,H,L,H]),
    // 2540 ╀ ╁ ╂ ╃ ╄ ╅ ╆ ╇ ╈ ╉ ╊ ╋ ╌ ╍ ╎ ╏
    Some([H,L,L,L]), Some([L,L,H,L]), Some([H,L,H,L]), Some([H,L,L,H]),
    Some([H,H,L,L]), Some([L,L,H,H]), Some([L,H,H,L]), Some([H,H,L,H]),
    Some([L,H,H,H]), Some([H,L,H,H]), Some([H,H,H,L]), Some([H,H,H,H]),
    None, None, None, None,
    // 2550 ═ ║ ╒ ╓ ╔ ╕ ╖ ╗ ╘ ╙ ╚ ╛ ╜ ╝ ╞ ╟
    Some([N,D,N,D]), Some([D,N,D,N]), Some([N,D,L,N]), Some([N,L,D,N]),
    Some([N,D,D,N]), Some([N,N,L,D]), Some([N,N,D,L]), Some([N,N,D,D]),
    Some([L,D,N,N]), Some([D,L,N,N]), Some([D,D,N,N]), Some([L,N,N,D]),
    Some([D,N,N,L]), Some([D,N,N,D]), Some([L,D,L,N]), Some([D,L,D,N]),
    // 2560 ╠ ╡ ╢ ╣ ╤ ╥ ╦ ╧ ╨ ╩ ╪ ╫ ╬ ╭ ╮ ╯
    Some([D,D,D,N]), Some([L,N,L,D]), Some([D,N,D,L]), Some([D,N,D,D]),
    Some([N,D,L,D]), Some([N,L,D,L]), Some([N,D,D,D]), Some([L,D,N,D]),
    Some([D,L,N,L]), Some([D,D,N,D]), Some([L,D,L,D]), Some([D,L,D,L]),
    Some([D,D,D,D]), None, None, None,
    // 2570 ╰ ╱ ╲ ╳ ╴ ╵ ╶ ╷ ╸ ╹ ╺ ╻ ╼ ╽ ╾ ╿
    None, None, None, None,
    Some([N,N,N,L]), Some([L,N,N,N]), Some([N,L,N,N]), Some([N,N,L,N]),
    Some([N,N,N,H]), Some([H,N,N,N]), Some([N,H,N,N]), Some([N,N,H,N]),
    Some([N,H,N,L]), Some([L,N,H,N]), Some([N,L,N,H]), Some([H,N,L,N]),
];

/// Whether `c` is drawn here rather than by a font. Cheap: a range check.
pub fn is_sprite(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{259F}' | '\u{2801}'..='\u{28FF}' | '\u{E0B0}'..='\u{E0B7}')
}

/// Draw `c` for a cell of `m`, or `None` if it isn't one of ours.
pub fn draw(c: char, m: CellMetrics) -> Option<Sprite> {
    let mut canvas = Canvas::new(m.width.max(1), m.height.max(1));
    let light = m.underline_thickness.max(1);
    let code = u32::from(c);
    match code {
        0x2500..=0x257F => canvas.box_char(code - 0x2500, light)?,
        0x2580..=0x259F => canvas.block(code - 0x2580),
        0x2801..=0x28FF => canvas.braille(code - 0x2800),
        0xE0B0..=0xE0B7 => canvas.powerline(code - 0xE0B0, light),
        _ => return None,
    }
    Some(Sprite {
        width: canvas.w,
        height: canvas.h,
        pixels: canvas.px,
    })
}

/// Underline styles drawn as cell-wide sprites that tile across a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Decoration {
    Curly,
    Dotted,
    Dashed,
}

/// A cell-sized sprite with `d` drawn at the underline position.
pub fn decoration(d: Decoration, m: CellMetrics) -> Sprite {
    let mut canvas = Canvas::new(m.width.max(1), m.height.max(1));
    let (w, h) = (canvas.w, canvas.h);
    let t = m.underline_thickness.max(1);
    let y = m.underline_y.min(h.saturating_sub(t));
    match d {
        Decoration::Curly => {
            // One sine period per cell, so neighbors join. Kept inside the
            // cell so it never paints over the row below.
            let stroke = t as f32;
            let amp = (stroke * 1.25).max(1.5);
            let reach = amp + stroke / 2.0;
            let mid = (y as f32 + stroke / 2.0).min(h as f32 - reach).max(reach);
            let k = std::f32::consts::TAU / w as f32;
            canvas.shape(|x, py| {
                let (curve, slope) = (mid - amp * (k * x).sin(), -amp * k * (k * x).cos());
                // Distance to the curve, not just vertical offset, keeps
                // the stroke even on the slopes.
                (py - curve).abs() / slope.hypot(1.0) <= stroke / 2.0
            });
        }
        Decoration::Dotted => {
            let count = (w / (2 * t)).max(1);
            for i in 0..count {
                let x = (2 * i + 1) * w / (2 * count) - t / 2;
                canvas.rect((x, x + t), (y, y + t), 255);
            }
        }
        // Dash in the middle half: dash, gap, dash across cells.
        Decoration::Dashed => canvas.rect((w / 4, w - w / 4), (y, y + t), 255),
    }
    Sprite {
        width: w,
        height: h,
        pixels: canvas.px,
    }
}

struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

/// Lines of one arm across its width: `(start, end)`, in order.
type Lines = Vec<(u32, u32)>;

/// Samples per pixel side for shapes with curves or slopes.
const SS: u32 = 4;

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            px: vec![0; (w * h) as usize],
        }
    }

    /// Fill `x0..x1 × y0..y1`, clamped to the cell.
    fn rect(&mut self, (x0, x1): (u32, u32), (y0, y1): (u32, u32), alpha: u8) {
        for y in y0.min(self.h)..y1.min(self.h) {
            for x in x0.min(self.w)..x1.min(self.w) {
                if let Some(p) = self.px.get_mut((y * self.w + x) as usize) {
                    *p = (*p).max(alpha);
                }
            }
        }
    }

    /// Fill where `inside(x, y)` holds, antialiased by supersampling. Takes
    /// pixel coordinates with (0, 0) at the cell's top-left corner.
    fn shape(&mut self, inside: impl Fn(f32, f32) -> bool) {
        let step = 1.0 / SS as f32;
        for y in 0..self.h {
            for x in 0..self.w {
                let mut hits = 0u32;
                for sy in 0..SS {
                    for sx in 0..SS {
                        let fx = x as f32 + (sx as f32 + 0.5) * step;
                        let fy = y as f32 + (sy as f32 + 0.5) * step;
                        hits += u32::from(inside(fx, fy));
                    }
                }
                let alpha = (hits * 255 / (SS * SS)) as u8;
                if let Some(p) = self.px.get_mut((y * self.w + x) as usize) {
                    *p = (*p).max(alpha);
                }
            }
        }
    }

    fn box_char(&mut self, index: u32, light: u32) -> Option<()> {
        let arms = match index {
            // ┄ ┅ ┆ ┇ ┈ ┉ ┊ ┋: 3 or 4 dashes, light/heavy, horizontal/vertical.
            0x04..=0x0B => {
                let i = index - 0x04;
                self.dashes(if i < 4 { 3 } else { 4 }, i % 2 == 1, i % 4 >= 2, light);
                return Some(());
            }
            // ╌ ╍ ╎ ╏: 2 dashes.
            0x4C..=0x4F => {
                let i = index - 0x4C;
                self.dashes(2, i % 2 == 1, i >= 2, light);
                return Some(());
            }
            0x6D..=0x70 => {
                self.arc(index - 0x6D, light);
                return Some(());
            }
            0x71..=0x73 => {
                self.diagonals(index - 0x71, light);
                return Some(());
            }
            _ => BOX.get(index as usize).copied().flatten()?,
        };
        self.arms(arms, light);
        Some(())
    }

    fn lines(weight: u8, across: u32, light: u32) -> Lines {
        let at = |thick: u32| across.saturating_sub(thick) / 2;
        match weight {
            L => vec![(at(light), at(light) + light)],
            H => {
                let t = light * 2;
                vec![(at(t), at(t) + t)]
            }
            D => {
                let s = at(light * 3);
                vec![(s, s + light), (s + 2 * light, s + 3 * light)]
            }
            _ => Vec::new(),
        }
    }

    /// Lines of each arm meet so corners, tees and crosses close up: a line
    /// stops at the matching line of the arm it turns into, or runs through
    /// to the far side when the turn is the other way.
    fn arms(&mut self, [up, right, down, left]: [u8; 4], light: u32) {
        let (w, h) = (self.w, self.h);
        // Vertical arms' lines sit across x; horizontal arms' across y.
        let (up_l, down_l) = (Self::lines(up, w, light), Self::lines(down, w, light));
        let (left_l, right_l) = (Self::lines(left, h, light), Self::lines(right, h, light));

        // Where a line travelling toward the center stops, given the lines of
        // the perpendicular arms on its two sides (`neg` is up or left).
        // `side` says which of the arm's lines this is: 0 for a single
        // line, -1/1 for the two of a double, -1 nearer `neg`. A line stops
        // at the matching line it turns into, runs through to the far side
        // when the turn goes the other way, and with nothing to meet ends
        // at `center`.
        let stop = |side: i8, neg: &Lines, pos: &Lines, forward: bool, center: u32| -> u32 {
            let end = |line: Option<(u32, u32)>| line.map(|(a, b)| if forward { b } else { a });
            // The perpendicular line met first / last when travelling on.
            let near = |l: &Lines| {
                if forward {
                    l.first().copied()
                } else {
                    l.last().copied()
                }
            };
            let far = |l: &Lines| {
                if forward {
                    l.last().copied()
                } else {
                    l.first().copied()
                }
            };
            let found = match side {
                0 => {
                    let ends = neg.iter().chain(pos);
                    if forward {
                        ends.map(|&(_, b)| b).max()
                    } else {
                        ends.map(|&(a, _)| a).min()
                    }
                }
                -1 if !neg.is_empty() => end(near(neg)),
                -1 => end(far(pos)),
                1 if !pos.is_empty() => end(near(pos)),
                _ => end(far(neg)),
            };
            found.unwrap_or(center)
        };
        let sides = |lines: &Lines| -> Vec<(i8, (u32, u32))> {
            match lines.as_slice() {
                [one] => vec![(0, *one)],
                [a, b] => vec![(-1, *a), (1, *b)],
                _ => Vec::new(),
            }
        };
        // With nothing to meet, arms end just past the middle so two of
        // them (─) overlap and a lone one (╴) reaches the center.
        let past_mid = |len: u32| (len + light).div_ceil(2);
        let before_mid = |len: u32| len.saturating_sub(light) / 2;
        for (side, ys) in sides(&left_l) {
            let x1 = stop(side, &up_l, &down_l, true, past_mid(w));
            self.rect((0, x1), ys, 255);
        }
        for (side, ys) in sides(&right_l) {
            let x0 = stop(side, &up_l, &down_l, false, before_mid(w));
            self.rect((x0, w), ys, 255);
        }
        for (side, xs) in sides(&up_l) {
            let y1 = stop(side, &left_l, &right_l, true, past_mid(h));
            self.rect(xs, (0, y1), 255);
        }
        for (side, xs) in sides(&down_l) {
            let y0 = stop(side, &left_l, &right_l, false, before_mid(h));
            self.rect(xs, (y0, h), 255);
        }
    }

    fn dashes(&mut self, count: u32, heavy: bool, vertical: bool, light: u32) {
        let weight = if heavy { H } else { L };
        let (len, across) = if vertical {
            (self.h, self.w)
        } else {
            (self.w, self.h)
        };
        let gap = (len / (count * 4)).max(1);
        for (a, b) in Self::lines(weight, across, light) {
            for i in 0..count {
                let start = i * len / count + gap / 2;
                let end = ((i + 1) * len / count).saturating_sub(gap - gap / 2);
                if vertical {
                    self.rect((a, b), (start, end), 255);
                } else {
                    self.rect((start, end), (a, b), 255);
                }
            }
        }
    }

    /// ╭ ╮ ╯ ╰: a quarter circle joining the two arms through the center.
    fn arc(&mut self, which: u32, light: u32) {
        let (w, h) = (self.w as f32, self.h as f32);
        let t = light as f32;
        // Line centers, matching the straight arms of other box characters.
        let cx = (self.w.saturating_sub(light) / 2) as f32 + t / 2.0;
        let cy = (self.h.saturating_sub(light) / 2) as f32 + t / 2.0;
        let r = cx.min(cy);
        // Which way the arms go: ╭ right+down, ╮ left+down, ╯ left+up, ╰ right+up.
        let (dx, dy) = match which {
            0 => (1.0, 1.0),
            1 => (-1.0, 1.0),
            2 => (-1.0, -1.0),
            _ => (1.0, -1.0),
        };
        let (ox, oy) = (cx + dx * r, cy + dy * r);
        self.shape(|x, y| {
            let (rx, ry) = (x - ox, y - oy);
            // Past the arc's center along an arm: a straight line.
            let along_x = (x - ox) * dx >= 0.0;
            let along_y = (y - oy) * dy >= 0.0;
            if along_x && !along_y {
                return (y - cy).abs() <= t / 2.0 && x >= 0.0 && x <= w;
            }
            if along_y && !along_x {
                return (x - cx).abs() <= t / 2.0 && y >= 0.0 && y <= h;
            }
            if along_x && along_y {
                return false;
            }
            ((rx * rx + ry * ry).sqrt() - r).abs() <= t / 2.0
        });
    }

    /// ╱ ╲ ╳, corner to corner so they continue into neighboring cells.
    fn diagonals(&mut self, which: u32, light: u32) {
        let (w, h) = (self.w as f32, self.h as f32);
        let half = light as f32 / 2.0;
        let len = (w * w + h * h).sqrt();
        // Distance from (x, y) to the line through two corners.
        let rising = move |x: f32, y: f32| ((h * x + w * y - w * h).abs() / len) <= half;
        let falling = move |x: f32, y: f32| ((h * x - w * y).abs() / len) <= half;
        match which {
            0 => self.shape(rising),
            1 => self.shape(falling),
            _ => self.shape(|x, y| rising(x, y) || falling(x, y)),
        }
    }

    fn block(&mut self, index: u32) {
        let (w, h) = (self.w, self.h);
        let eighth_h = |n: u32| (h * n + 4) / 8;
        let eighth_w = |n: u32| (w * n + 4) / 8;
        let (hw, hh) = (w / 2, h / 2);
        let quads = |s: &mut Self, ul: bool, ur: bool, ll: bool, lr: bool| {
            for (on, xs, ys) in [
                (ul, (0, hw), (0, hh)),
                (ur, (hw, w), (0, hh)),
                (ll, (0, hw), (hh, h)),
                (lr, (hw, w), (hh, h)),
            ] {
                if on {
                    s.rect(xs, ys, 255);
                }
            }
        };
        match index {
            // ▀
            0x00 => self.rect((0, w), (0, hh), 255),
            // ▁ ▂ ▃ ▄ ▅ ▆ ▇ █: lower n/8.
            0x01..=0x08 => self.rect((0, w), (h - eighth_h(index), h), 255),
            // ▉ ▊ ▋ ▌ ▍ ▎ ▏: left (8 - n)/8.
            0x09..=0x0F => self.rect((0, eighth_w(16 - index)), (0, h), 255),
            // ▐
            0x10 => self.rect((hw, w), (0, h), 255),
            // ░ ▒ ▓
            0x11..=0x13 => self.rect((0, w), (0, h), [64, 128, 191][(index - 0x11) as usize]),
            // ▔
            0x14 => self.rect((0, w), (0, eighth_h(1)), 255),
            // ▕
            0x15 => self.rect((w - eighth_w(1), w), (0, h), 255),
            // ▖ ▗ ▘ ▙ ▚ ▛ ▜ ▝ ▞ ▟
            0x16 => quads(self, false, false, true, false),
            0x17 => quads(self, false, false, false, true),
            0x18 => quads(self, true, false, false, false),
            0x19 => quads(self, true, false, true, true),
            0x1A => quads(self, true, false, false, true),
            0x1B => quads(self, true, true, true, false),
            0x1C => quads(self, true, true, false, true),
            0x1D => quads(self, false, true, false, false),
            0x1E => quads(self, false, true, true, false),
            _ => quads(self, false, true, true, true),
        }
    }

    /// Eight dots in two columns of four; bit n is dot n+1.
    fn braille(&mut self, bits: u32) {
        let (w, h) = (self.w as f32, self.h as f32);
        // Dot centers on a 2×4 grid, radius from the tighter axis.
        let (sx, sy) = (w / 2.0, h / 4.0);
        let r = (sx.min(sy) * 0.3).max(0.75);
        // (bit, column, row) per Unicode's dot numbering.
        const DOTS: [(u32, f32, f32); 8] = [
            (0x01, 0.0, 0.0),
            (0x02, 0.0, 1.0),
            (0x04, 0.0, 2.0),
            (0x08, 1.0, 0.0),
            (0x10, 1.0, 1.0),
            (0x20, 1.0, 2.0),
            (0x40, 0.0, 3.0),
            (0x80, 1.0, 3.0),
        ];
        let centers: Vec<(f32, f32)> = DOTS
            .iter()
            .filter(|(bit, _, _)| bits & bit != 0)
            .map(|&(_, col, row)| ((col + 0.5) * sx, (row + 0.5) * sy))
            .collect();
        self.shape(|x, y| {
            centers
                .iter()
                .any(|&(cx, cy)| (x - cx).powi(2) + (y - cy).powi(2) <= r * r)
        });
    }

    /// U+E0B0..=U+E0B7: solid and outlined triangles and half circles.
    fn powerline(&mut self, index: u32, light: u32) {
        let (w, h) = (self.w as f32, self.h as f32);
        let mid = h / 2.0;
        let t = light as f32;
        // E0B2, E0B3, E0B6, E0B7 point left: mirror the right-pointing ones.
        let flip = |x: f32| if index % 4 >= 2 { w - x } else { x };
        match index {
            // Solid triangles: wide side on one edge, point mid-cell on the other.
            0 | 2 => self.shape(|x, y| flip(x) <= w * (1.0 - (y - mid).abs() / mid)),
            // Thin chevrons: the triangle's two slanted sides.
            1 | 3 => self.shape(|x, y| {
                let edge = w * (1.0 - (y - mid).abs() / mid);
                // Horizontal distance scaled to the slope's normal.
                let scale = (1.0 + (w / mid).powi(2)).sqrt();
                (flip(x) - edge).abs() <= t * scale / 2.0
            }),
            // Solid half circles.
            4 | 6 => self.shape(|x, y| {
                let fx = flip(x);
                (fx / w).powi(2) + ((y - mid) / mid).powi(2) <= 1.0
            }),
            // Outlined half circles.
            _ => self.shape(|x, y| {
                let fx = flip(x);
                let d = ((fx / w).powi(2) + ((y - mid) / mid).powi(2)).sqrt();
                (d - 1.0).abs() * w.min(mid) <= t / 2.0 + 0.5
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::CellMetrics;
    use crate::sprite::{Sprite, draw};

    const M: CellMetrics = CellMetrics {
        width: 9,
        height: 19,
        baseline: 15,
        underline_y: 17,
        underline_thickness: 1,
        strikeout_y: 10,
    };

    fn at(s: &Sprite, x: u32, y: u32) -> u8 {
        s.pixels[(y * s.width + x) as usize]
    }

    fn sprite(c: char) -> Result<Sprite, String> {
        draw(c, M).ok_or(format!("{c} is not drawn"))
    }

    /// Lines reach every edge they point at, so neighbors join.
    #[test]
    fn lines_reach_the_edges_they_point_at() -> Result<(), String> {
        let h = sprite('─')?;
        assert!((0..M.width).all(|x| at(&h, x, M.height / 2) == 255));
        let v = sprite('│')?;
        assert!((0..M.height).all(|y| at(&v, M.width / 2, y) == 255));

        let corner = sprite('┌')?;
        assert_eq!(at(&corner, M.width - 1, M.height / 2), 255, "reaches right");
        assert_eq!(at(&corner, M.width / 2, M.height - 1), 255, "reaches down");
        assert_eq!(at(&corner, 0, M.height / 2), 0, "not left");
        assert_eq!(at(&corner, M.width / 2, 0), 0, "not up");
        Ok(())
    }

    #[test]
    fn double_corner_leaves_its_inside_open() -> Result<(), String> {
        let s = sprite('╔')?;
        // Outer lines meet at the corner, inner ones leave a gap inside.
        let edges = |s: &Sprite, y: u32| (0..s.width).filter(|&x| at(s, x, y) == 255).count();
        assert!(
            edges(&s, M.height - 1) == 2,
            "two vertical lines at the bottom"
        );
        assert!((0..M.height).any(|y| edges(&s, y) > 2), "a horizontal run");
        Ok(())
    }

    #[test]
    fn full_block_fills_the_cell() -> Result<(), String> {
        assert!(sprite('█')?.pixels.iter().all(|&p| p == 255));
        assert!(draw('a', M).is_none());
        Ok(())
    }
}
