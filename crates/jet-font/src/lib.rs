//! Fonts for the terminal grid: cell metrics, shaping (harfrust), CPU
//! rasterizing (swash) and glyph atlases: coverage for text, RGBA for color
//! glyphs (emoji). No GPU here; the renderer uploads [`Atlas::pixels`].
//!
//! The grid font is the bundled JetBrains Mono. Characters it lacks come
//! from a [`Fallback`] (installed fonts on Windows), picked per character.

mod atlas;
pub mod fallback;
mod sprite;

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::str::FromStr;
use std::sync::Arc;

use harfrust::{Buffer, Direction, Feature, ShapeOptions, ShapePlan, ShapePlanKey, ShaperFont};
use swash::FontRef;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::Format;

pub use crate::atlas::Atlas;
pub use crate::fallback::{Fallback, FontFile};
pub use crate::sprite::{Decoration, is_sprite};

static REGULAR: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");
static BOLD: &[u8] = include_bytes!("../fonts/JetBrainsMono-Bold.ttf");
static ITALIC: &[u8] = include_bytes!("../fonts/JetBrainsMono-Italic.ttf");
static BOLD_ITALIC: &[u8] = include_bytes!("../fonts/JetBrainsMono-BoldItalic.ttf");

const ATLAS_SIZE: u32 = 2048;
/// Fonts before this index are the grid font's faces; the rest fallbacks.
const PRIMARY_FACES: usize = 4;
/// Color glyphs are few (emoji) but 4 bytes a pixel.
const COLOR_ATLAS_SIZE: u32 = 1024;
/// Tried in order: COLR layers (Segoe UI Emoji), embedded color bitmaps
/// (Noto Color Emoji), then a plain outline drawn in the text color.
const SOURCES: &[Source] = &[
    Source::ColorOutline(0),
    Source::ColorBitmap(StrikeWith::BestFit),
    Source::Outline,
];
/// Shaped runs kept per font. A screen redraws mostly the same rows, so a
/// few screens' worth covers it; the cache is simply dropped when full.
const SHAPE_CACHE_RUNS: usize = 4096;

/// Style of the grid font.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Face {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl Face {
    pub fn new(bold: bool, italic: bool) -> Self {
        match (bold, italic) {
            (false, false) => Self::Regular,
            (true, false) => Self::Bold,
            (false, true) => Self::Italic,
            (true, true) => Self::BoldItalic,
        }
    }

    fn bold(self) -> bool {
        matches!(self, Self::Bold | Self::BoldItalic)
    }

    fn italic(self) -> bool {
        matches!(self, Self::Italic | Self::BoldItalic)
    }
}

/// A loaded font: one of the grid font's four faces, or a fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontId(usize);

impl From<Face> for FontId {
    fn from(face: Face) -> Self {
        Self(face as usize)
    }
}

/// Pixel geometry of one grid cell. All offsets are from the cell's top edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    pub baseline: u32,
    pub underline_y: u32,
    pub underline_thickness: u32,
    pub strikeout_y: u32,
}

/// A shaped glyph, positioned relative to the left edge of its cluster.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedGlyph {
    pub glyph_id: u16,
    /// Byte offset of this glyph's cluster in the shaped text.
    pub cluster: u32,
    /// Pixels right of the cluster's cell, including marks stacked within it.
    pub x: f32,
    /// Pixels above the baseline.
    pub y: f32,
}

/// Where a rasterized glyph sits in the atlas and how to place it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphSlot {
    pub atlas_x: u32,
    pub atlas_y: u32,
    pub width: u32,
    pub height: u32,
    /// Bitmap left edge relative to the pen position.
    pub left: i32,
    /// Bitmap top edge above the baseline.
    pub top: i32,
    /// In the color atlas (premultiplied RGBA) instead of the coverage one.
    pub color: bool,
}

#[derive(Debug)]
pub enum Error {
    BadFont(&'static str),
    BadFeature(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadFont(name) => write!(f, "could not parse font {name}"),
            Self::BadFeature(feature) => write!(f, "invalid font feature {feature:?}"),
        }
    }
}

impl std::error::Error for Error {}

/// A map for the per-glyph caches, whose keys are a few small integers:
/// they're looked up for every drawn cell, where SipHash showed in
/// profiles. The keys are bounded (fonts, glyph ids, characters), so
/// hash flooding isn't a concern the way it is for arbitrary text.
type IntMap<K, V> = HashMap<K, V, BuildHasherDefault<IntHasher>>;

/// Multiply-rotate hashing of whole integers, as FxHash does.
#[derive(Default)]
struct IntHasher(u64);

impl Hasher for IntHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.write_u64(u64::from(n));
    }

    fn write_u16(&mut self, n: u16) {
        self.write_u64(u64::from(n));
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

struct FontData {
    file: FontFile,
    shaper: harfrust::Font,
    /// Pixels per font unit at the current size.
    scale: f32,
    /// Shaped runs by text.
    shaped: HashMap<String, Vec<ShapedGlyph>>,
    /// Compiled shaping plans, one per script and language seen: building
    /// one costs more than most shaping does.
    plans: Vec<ShapePlan>,
}

impl FontData {
    fn new(file: FontFile, size_px: f32) -> Option<Self> {
        let font = FontRef::from_index(&file.bytes, file.index as usize)?;
        let upem = f32::from(font.metrics(&[]).units_per_em.max(1));
        let blob: Arc<dyn AsRef<[u8]> + Send + Sync> = file.bytes.clone();
        Some(Self {
            shaper: harfrust::Font::new(blob, file.index)?,
            scale: size_px / upem,
            file,
            shaped: HashMap::new(),
            plans: Vec::new(),
        })
    }
}

pub struct Fonts {
    /// The grid font's faces in [`Face`] order, then fallbacks as loaded.
    fonts: Vec<FontData>,
    size_px: f32,
    metrics: CellMetrics,
    features: Vec<Feature>,
    fallback: Box<dyn Fallback>,
    /// Which font draws a character, per style and emoji presentation.
    fallback_for: IntMap<(char, Face, bool), FontId>,
    buffer: Option<Buffer>,
    scaler: ScaleContext,
    /// By font, glyph and the cells it spans (fallback glyphs shrink to fit).
    glyphs: IntMap<(FontId, u16, u8), Option<GlyphSlot>>,
    /// Drawn characters (see [`is_sprite`]) by character.
    sprites: IntMap<char, Option<GlyphSlot>>,
    decorations: HashMap<Decoration, Option<GlyphSlot>>,
    atlas: Atlas,
    color_atlas: Atlas,
}

impl Fonts {
    /// `size_px` is the font size in pixels (points × DPI / 72). `features`
    /// are OpenType settings like `"-calt"` or `"ss01"`, applied on top of
    /// the shaper's defaults (which include `liga` and `calt`).
    pub fn new(
        size_px: f32,
        features: &[&str],
        fallback: Box<dyn Fallback>,
    ) -> Result<Self, Error> {
        Self::with_faces(bundled(), size_px, features, fallback)
    }

    /// Like [`Fonts::new`] with another grid font: regular, bold, italic
    /// and bold italic faces, in [`Face`] order.
    pub fn with_faces(
        faces: [FontFile; 4],
        size_px: f32,
        features: &[&str],
        fallback: Box<dyn Fallback>,
    ) -> Result<Self, Error> {
        let names = ["regular", "bold", "italic", "bold italic"];
        let metrics = cell_metrics(&faces[0], size_px).ok_or(Error::BadFont(names[0]))?;
        let fonts = faces
            .into_iter()
            .zip(names)
            .map(|(file, name)| FontData::new(file, size_px).ok_or(Error::BadFont(name)))
            .collect::<Result<_, _>>()?;
        let features = features
            .iter()
            .map(|f| Feature::from_str(f).map_err(|_| Error::BadFeature((*f).to_owned())))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            fonts,
            size_px,
            metrics,
            features,
            fallback,
            fallback_for: IntMap::default(),
            buffer: Some(Buffer::new()),
            scaler: ScaleContext::new(),
            glyphs: IntMap::default(),
            sprites: IntMap::default(),
            decorations: HashMap::new(),
            atlas: Atlas::new(ATLAS_SIZE, 1),
            color_atlas: Atlas::new(COLOR_ATLAS_SIZE, 4),
        })
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub fn atlas(&self) -> &Atlas {
        &self.atlas
    }

    pub fn atlas_mut(&mut self) -> &mut Atlas {
        &mut self.atlas
    }

    pub fn color_atlas(&self) -> &Atlas {
        &self.color_atlas
    }

    pub fn color_atlas_mut(&mut self) -> &mut Atlas {
        &mut self.color_atlas
    }

    /// The font that draws a cell's `text` in `face`, picked by its first
    /// character: the grid font if it has it, otherwise a fallback (looked
    /// up once per character, style and presentation). Emoji, by default
    /// (😀 ✅) or by a U+FE0F selector (❤️), go to a color font first.
    /// Falls back to the grid font, and its missing-glyph box, if nothing
    /// has it. `None` for empty text.
    pub fn font_for(&mut self, face: Face, text: &str) -> Option<FontId> {
        let primary = FontId::from(face);
        let c = text.chars().next()?;
        // The grid font covers ASCII; skip the lookup for the common case.
        // (Keycaps like 1️⃣ start with ASCII too, but aren't one byte.)
        if text.len() == 1 {
            return Some(primary);
        }
        let emoji = fallback::emoji_presentation(c) || text.contains('\u{FE0F}');
        if let Some(&id) = self.fallback_for.get(&(c, face, emoji)) {
            return Some(id);
        }
        let found = if emoji || !self.fonts[primary.0].file.has_char(c) {
            self.find_fallback(face, c, emoji)
        } else {
            None
        };
        let id = found.unwrap_or(primary);
        self.fallback_for.insert((c, face, emoji), id);
        Some(id)
    }

    fn find_fallback(&mut self, face: Face, c: char, emoji: bool) -> Option<FontId> {
        let file = self.fallback.find(c, face.bold(), face.italic(), emoji)?;
        if let Some(i) = self.fonts.iter().position(|f| f.file.id == file.id) {
            return Some(FontId(i));
        }
        self.fonts.push(FontData::new(file, self.size_px)?);
        Some(FontId(self.fonts.len() - 1))
    }

    /// Shape one run of same-font text into `out` (cleared first). Glyphs
    /// are positioned within their cluster; the caller places clusters on
    /// the grid.
    pub fn shape(&mut self, font: FontId, text: &str, out: &mut Vec<ShapedGlyph>) {
        out.clear();
        let Some(data) = self.fonts.get_mut(font.0) else {
            return;
        };
        if let Some(glyphs) = data.shaped.get(text) {
            out.extend_from_slice(glyphs);
            return;
        }
        if data.shaped.len() >= SHAPE_CACHE_RUNS {
            data.shaped.clear();
        }
        self.shape_uncached(font, text, out);
        if let Some(data) = self.fonts.get_mut(font.0) {
            data.shaped.insert(text.to_owned(), out.clone());
        }
    }

    fn shape_uncached(&mut self, font: FontId, text: &str, out: &mut Vec<ShapedGlyph>) {
        let Some(data) = self.fonts.get_mut(font.0) else {
            return;
        };
        let mut buffer = self.buffer.take().unwrap_or_default();
        buffer.clear();
        buffer.push_str(text);
        buffer.guess_segment_properties();
        // Terminals lay text out left to right; bidi is out of scope.
        buffer.set_direction(Direction::LeftToRight);
        let key = ShapePlanKey::new(&data.shaper, buffer.script(), Direction::LeftToRight)
            .language(buffer.language())
            .features(&self.features);
        let plan = match data.plans.iter().position(|plan| key.matches(plan)) {
            Some(i) => i,
            None => {
                data.plans.push(ShapePlan::new(
                    &data.shaper,
                    Direction::LeftToRight,
                    buffer.script(),
                    buffer.language(),
                    &self.features,
                ));
                data.plans.len() - 1
            }
        };
        let data = &*data;
        let shaper = ShaperFont::new(&data.shaper);
        let options = ShapeOptions::new()
            .features(&self.features)
            .plan(data.plans.get(plan));
        if harfrust::shape(&shaper, &mut buffer, options).is_ok() {
            let mut pen = 0.0;
            let mut cluster = None;
            for (info, pos) in buffer.glyph_infos().iter().zip(buffer.glyph_positions()) {
                if cluster != Some(info.cluster) {
                    cluster = Some(info.cluster);
                    pen = 0.0;
                }
                out.push(ShapedGlyph {
                    glyph_id: u16::try_from(info.glyph_id).unwrap_or(0),
                    cluster: info.cluster,
                    x: pen + pos.x_offset as f32 * data.scale,
                    y: pos.y_offset as f32 * data.scale,
                });
                pen += pos.x_advance as f32 * data.scale;
            }
        }
        self.buffer = Some(buffer);
    }

    /// A cell-sized drawing of `c` for [`is_sprite`] characters, placed with
    /// its top-left corner at the cell's: `left` is 0 and `top` the
    /// baseline. `None` if `c` isn't drawn this way (use the font) or the
    /// atlas is full.
    pub fn sprite(&mut self, c: char) -> Option<GlyphSlot> {
        if let Some(slot) = self.sprites.get(&c) {
            return *slot;
        }
        let slot = sprite::draw(c, self.metrics).and_then(|s| self.insert_sprite(&s));
        self.sprites.insert(c, slot);
        slot
    }

    /// A cell-sized underline in style `d`, placed like [`Fonts::sprite`].
    /// `None` only when the atlas is full.
    pub fn decoration(&mut self, d: Decoration) -> Option<GlyphSlot> {
        if let Some(slot) = self.decorations.get(&d) {
            return *slot;
        }
        let slot = self.insert_sprite(&sprite::decoration(d, self.metrics));
        self.decorations.insert(d, slot);
        slot
    }

    fn insert_sprite(&mut self, s: &sprite::Sprite) -> Option<GlyphSlot> {
        let (atlas_x, atlas_y) = self.atlas.insert(s.width, s.height, &s.pixels)?;
        Some(GlyphSlot {
            atlas_x,
            atlas_y,
            width: s.width,
            height: s.height,
            left: 0,
            top: self.metrics.baseline as i32,
            color: false,
        })
    }

    /// Rasterize a glyph for a cluster `cells` wide into the atlas on first
    /// use. `None` for glyphs with no pixels (spaces) or when the atlas is
    /// full.
    pub fn glyph(&mut self, font: FontId, glyph_id: u16, cells: u8) -> Option<GlyphSlot> {
        if let Some(slot) = self.glyphs.get(&(font, glyph_id, cells)) {
            return *slot;
        }
        let slot = self.rasterize(font, glyph_id, cells);
        self.glyphs.insert((font, glyph_id, cells), slot);
        slot
    }

    fn rasterize(&mut self, font: FontId, glyph_id: u16, cells: u8) -> Option<GlyphSlot> {
        let file = &self.fonts.get(font.0)?.file;
        let font_ref = FontRef::from_index(&file.bytes, file.index as usize)?;
        // A fallback glyph wider than its cells (a symbol font's ❤ in one
        // cell) is drawn smaller so its advance fits. The grid font is
        // left alone: its advance can round a hair past the cell width.
        let mut size = self.size_px;
        if font.0 >= PRIMARY_FACES {
            let advance = font_ref
                .glyph_metrics(&[])
                .scale(size)
                .advance_width(glyph_id);
            let room = f32::from(cells.max(1)) * self.metrics.width as f32;
            if advance > room {
                size *= room / advance;
            }
        }
        let mut scaler = self.scaler.builder(font_ref).size(size).hint(true).build();
        let mut image = Render::new(SOURCES)
            .format(Format::Alpha)
            .render(&mut scaler, glyph_id)?;
        let p = image.placement;
        if p.width == 0 || p.height == 0 {
            return None;
        }
        // Shrinking pulls a glyph toward the baseline; raise it so its middle
        // stays where the full-size glyph's was.
        let shrink = size / self.size_px;
        let middle = p.top as f32 - p.height as f32 / 2.0;
        let top = p.top + (middle * (1.0 / shrink - 1.0)).round() as i32;
        let color = image.content == Content::Color;
        // COLR layers come out premultiplied; decoded bitmaps don't.
        if color && matches!(image.source, Source::ColorBitmap(_)) {
            premultiply(&mut image.data);
        }
        let atlas = if color {
            &mut self.color_atlas
        } else {
            &mut self.atlas
        };
        let (atlas_x, atlas_y) = atlas.insert(p.width, p.height, &image.data)?;
        Some(GlyphSlot {
            atlas_x,
            atlas_y,
            width: p.width,
            height: p.height,
            left: p.left,
            top,
            color,
        })
    }
}

fn premultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u16::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
}

/// The bundled JetBrains Mono faces, in [`Face`] order.
pub fn bundled() -> [FontFile; 4] {
    let face = |bytes: &'static [u8], name: &str| FontFile {
        bytes: Arc::new(bytes.to_vec()),
        index: 0,
        id: format!("bundled {name}"),
    };
    [
        face(REGULAR, "regular"),
        face(BOLD, "bold"),
        face(ITALIC, "italic"),
        face(BOLD_ITALIC, "bold italic"),
    ]
}

fn cell_metrics(file: &FontFile, size_px: f32) -> Option<CellMetrics> {
    let font = FontRef::from_index(&file.bytes, file.index as usize)?;
    let m = font.metrics(&[]).scale(size_px);
    let glyph = font.charmap().map('M');
    let advance = font.glyph_metrics(&[]).scale(size_px).advance_width(glyph);

    let height = (m.ascent + m.descent + m.leading).ceil().max(1.0);
    // Split the line gap evenly above and below the text.
    let baseline = (m.leading / 2.0 + m.ascent).round();
    let thickness = m.stroke_size.round().max(1.0);
    let below = |offset: f32| (baseline - offset).round().clamp(0.0, height - 1.0);
    Some(CellMetrics {
        width: advance.round().max(1.0) as u32,
        height: height as u32,
        baseline: baseline as u32,
        underline_y: below(m.underline_offset) as u32,
        underline_thickness: thickness as u32,
        strikeout_y: below(m.strikeout_offset) as u32,
    })
}

#[cfg(test)]
mod tests {
    use crate::fallback::NoFallback;
    use crate::{Face, Fonts};

    #[test]
    fn metrics_are_sane_for_16px() -> Result<(), crate::Error> {
        let fonts = Fonts::new(16.0, &[], Box::new(NoFallback))?;
        let m = fonts.metrics();
        assert!((8..=11).contains(&m.width), "{m:?}");
        assert!((18..=23).contains(&m.height), "{m:?}");
        assert!(m.baseline < m.height && m.underline_y > m.baseline, "{m:?}");
        Ok(())
    }

    #[test]
    fn ligatures_follow_features() -> Result<(), crate::Error> {
        let mut glyphs = Vec::new();
        let mut on = Fonts::new(16.0, &[], Box::new(NoFallback))?;
        on.shape(Face::Regular.into(), "a->b", &mut glyphs);
        let with_liga: Vec<u16> = glyphs.iter().map(|g| g.glyph_id).collect();

        let mut off = Fonts::new(16.0, &["-calt", "-liga"], Box::new(NoFallback))?;
        off.shape(Face::Regular.into(), "a->b", &mut glyphs);
        let without: Vec<u16> = glyphs.iter().map(|g| g.glyph_id).collect();

        // JetBrains Mono swaps `->` for arrow glyphs via calt, one per cell.
        assert_eq!(with_liga.len(), 4);
        assert_ne!(with_liga, without);
        assert_eq!(
            glyphs.iter().map(|g| g.cluster).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        Ok(())
    }

    #[test]
    fn glyphs_are_cached_in_the_atlas() -> Result<(), crate::Error> {
        let mut fonts = Fonts::new(16.0, &[], Box::new(NoFallback))?;
        let mut glyphs = Vec::new();
        fonts.shape(Face::Bold.into(), "W ", &mut glyphs);
        let w = fonts.glyph(Face::Bold.into(), glyphs[0].glyph_id, 1);
        assert!(w.is_some_and(|s| s.width > 0 && s.height > 0));
        assert_eq!(fonts.glyph(Face::Bold.into(), glyphs[0].glyph_id, 1), w);
        assert_eq!(
            fonts.glyph(Face::Bold.into(), glyphs[1].glyph_id, 1),
            None,
            "space has no pixels"
        );
        Ok(())
    }
}
