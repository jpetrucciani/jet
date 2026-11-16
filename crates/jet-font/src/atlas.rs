/// Glyph atlas packed in shelves: glyphs fill a row left to right, and a new
/// row opens below when one doesn't fit. Glyphs of a monospace font at one
/// size are close in height, so waste stays low. Pixels are `channels` bytes:
/// 1 for coverage, 4 for premultiplied RGBA color glyphs.
pub struct Atlas {
    size: u32,
    channels: u32,
    pixels: Vec<u8>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_height: u32,
    /// Rows written since the last [`Atlas::take_dirty`], as `start..end`.
    dirty: Option<(u32, u32)>,
}

/// Gap between glyphs so sampling never bleeds into a neighbor.
const PAD: u32 = 1;

impl Atlas {
    pub fn new(size: u32, channels: u32) -> Self {
        Self {
            size,
            channels,
            pixels: vec![0; (size * size * channels) as usize],
            shelf_x: PAD,
            shelf_y: PAD,
            shelf_height: 0,
            // Starts all zero, like a fresh GPU texture.
            dirty: None,
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    /// Row-major pixels, `size × size × channels` bytes.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The rows changed since the last call, so only those get uploaded.
    pub fn take_dirty(&mut self) -> Option<std::ops::Range<u32>> {
        self.dirty.take().map(|(start, end)| start..end)
    }

    /// Copy a `width × height` bitmap in and return its position, or `None`
    /// if it doesn't fit.
    pub fn insert(&mut self, width: u32, height: u32, data: &[u8]) -> Option<(u32, u32)> {
        let c = self.channels;
        if data.len() < (width * height * c) as usize || width + 2 * PAD > self.size {
            return None;
        }
        if self.shelf_x + width + PAD > self.size {
            self.shelf_y += self.shelf_height + PAD;
            self.shelf_x = PAD;
            self.shelf_height = 0;
        }
        if self.shelf_y + height + PAD > self.size {
            return None;
        }
        let (x, y) = (self.shelf_x, self.shelf_y);
        for row in 0..height {
            let len = (width * c) as usize;
            let src = (row * width * c) as usize;
            let dst = (((y + row) * self.size + x) * c) as usize;
            let (src, dst) = (src..src + len, dst..dst + len);
            if let (Some(to), Some(from)) = (self.pixels.get_mut(dst), data.get(src)) {
                to.copy_from_slice(from);
            }
        }
        self.shelf_x += width + PAD;
        self.shelf_height = self.shelf_height.max(height);
        self.dirty = Some(match self.dirty {
            Some((start, end)) => (start.min(y), end.max(y + height)),
            None => (y, y + height),
        });
        Some((x, y))
    }
}

#[cfg(test)]
mod tests {
    use crate::atlas::Atlas;

    #[test]
    fn packs_rows_then_reports_full() {
        // 9x9 with 1px padding fits exactly four 3x3 glyphs, two per shelf.
        let mut atlas = Atlas::new(9, 1);
        let glyph = [255u8; 9];
        assert_eq!(atlas.insert(3, 3, &glyph), Some((1, 1)));
        assert_eq!(atlas.insert(3, 3, &glyph), Some((5, 1)));
        assert_eq!(atlas.insert(3, 3, &glyph), Some((1, 5)));
        assert_eq!(atlas.insert(3, 3, &glyph), Some((5, 5)));
        assert_eq!(atlas.insert(3, 3, &glyph), None);
        assert_eq!(atlas.pixels()[9 + 1], 255);
        assert_eq!(atlas.pixels()[0], 0, "padding stays empty");
    }
}
