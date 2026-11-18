//! Kitty graphics: libghostty-vt stores images and placements; this decodes
//! PNGs for it and copies what's on screen into snapshots.

use std::collections::HashMap;
use std::sync::Arc;

use libghostty_vt::Terminal;
use libghostty_vt::alloc::{Allocator, Bytes};
use libghostty_vt::kitty::graphics::{
    self, Compression, DecodePng, DecodedImage, Image, ImageFormat, Layer, PlacementIterator,
};

use crate::Error;
use crate::snapshot::{Picture, Placement};

/// What images may take in memory per pane, as in Ghostty.
pub(crate) const STORAGE_LIMIT: u64 = 320 * 1024 * 1024;

/// PNGs to 8-bit RGBA with the `png` crate.
struct PngDecoder;

impl DecodePng for PngDecoder {
    fn decode_png<'alloc>(
        &mut self,
        alloc: &'alloc Allocator<'_>,
        data: &[u8],
    ) -> Option<DecodedImage<'alloc>> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
        // Palette, gray and 16-bit images come out as 8-bit RGB(A).
        decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        let pixels = buf.get(..info.buffer_size())?;
        let rgba = to_rgba(pixels, info.color_type)?;
        let mut bytes = Bytes::new_with_alloc(alloc, rgba.len()).ok()?;
        bytes.copy_from_slice(&rgba);
        Some(DecodedImage {
            width: info.width,
            height: info.height,
            data: bytes,
        })
    }
}

fn to_rgba(pixels: &[u8], color: png::ColorType) -> Option<Vec<u8>> {
    Some(match color {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    })
}

/// Turn Kitty graphics on for `term`, decoding PNGs on this thread.
pub(crate) fn enable(term: &mut Terminal<'_, '_>) -> Result<(), Error> {
    graphics::set_png_decoder(Some(Box::new(PngDecoder)))?;
    term.set_kitty_image_storage_limit(STORAGE_LIMIT)?;
    Ok(())
}

/// Images already copied out, by their libghostty generation stamp (unique
/// per stored pixels, process-wide), so a still image is copied once.
#[derive(Default)]
pub(crate) struct Pictures {
    cache: HashMap<u64, Arc<Picture>>,
}

impl Pictures {
    /// Every placement that shows in the viewport, in drawing order, with
    /// positions in pixels from the first cell (`cell` is a cell's size).
    pub fn placements(
        &mut self,
        term: &Terminal<'_, '_>,
        cell: (u32, u32),
    ) -> Result<Vec<Placement>, Error> {
        let graphics = term.kitty_graphics()?;
        if graphics.generation()? == 0 {
            self.cache.clear();
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut iter = PlacementIterator::new()?;
        let mut placements = iter.update(&graphics)?;
        placements.set_layer(Layer::All)?;
        let mut seen = Vec::new();
        while let Some(placement) = placements.next() {
            let Some(image) = graphics.image(placement.image_id()?) else {
                continue;
            };
            let info = placement.placement_render_info(&image, term)?;
            if !info.viewport_visible || info.pixel_width == 0 || info.pixel_height == 0 {
                continue;
            }
            let Some(picture) = self.picture(&image)? else {
                continue;
            };
            seen.push(picture.key);
            let at = |cells: i32, size: u32, offset: u32| {
                i64::from(cells) * i64::from(size) + i64::from(offset)
            };
            out.push(Placement {
                picture,
                x: at(info.viewport_col, cell.0, placement.x_offset()?),
                y: at(info.viewport_row, cell.1, placement.y_offset()?),
                width: info.pixel_width,
                height: info.pixel_height,
                source: [
                    info.source_x,
                    info.source_y,
                    info.source_width,
                    info.source_height,
                ],
                z: placement.z()?,
            });
        }
        self.cache.retain(|key, _| seen.contains(key));
        out.sort_by_key(|p| p.z);
        Ok(out)
    }

    /// A picture placements showed, until none does.
    pub fn get(&self, key: u64) -> Option<Arc<Picture>> {
        self.cache.get(&key).cloned()
    }

    /// `image` as RGBA, copied the first time it's seen. `None` for a
    /// format this can't convert.
    fn picture(&mut self, image: &Image<'_>) -> Result<Option<Arc<Picture>>, Error> {
        let key = image.generation()?;
        if let Some(picture) = self.cache.get(&key) {
            return Ok(Some(picture.clone()));
        }
        if image.compression()? != Compression::None {
            return Ok(None);
        }
        let (width, height, data) = (image.width()?, image.height()?, image.data()?);
        let rgba = match image.format()? {
            ImageFormat::Rgba => data.to_vec(),
            ImageFormat::Rgb => to_rgba(data, png::ColorType::Rgb).unwrap_or_default(),
            ImageFormat::GrayAlpha => {
                to_rgba(data, png::ColorType::GrayscaleAlpha).unwrap_or_default()
            }
            ImageFormat::Gray => to_rgba(data, png::ColorType::Grayscale).unwrap_or_default(),
            _ => return Ok(None),
        };
        let expected = usize::try_from(u64::from(width) * u64::from(height) * 4).ok();
        if Some(rgba.len()) != expected {
            return Ok(None);
        }
        let picture = Arc::new(Picture {
            key,
            width,
            height,
            rgba,
        });
        self.cache.insert(key, picture.clone());
        Ok(Some(picture))
    }
}
