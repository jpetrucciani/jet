//! Where glyphs the bundled font lacks come from: CJK, emoji, braille,
//! symbols. On Windows, the installed fonts via DirectWrite; on Linux and
//! macOS, via fontdb; or a list of font files (what keeps the goldens identical
//! everywhere).

#[cfg(windows)]
mod directwrite;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod installed;

use std::path::PathBuf;
use std::sync::Arc;

use swash::{FontDataRef, FontRef};

/// A font face from a file: bytes plus the face's index within it (font
/// collections like `.ttc` hold several).
#[derive(Clone)]
pub struct FontFile {
    pub bytes: Arc<Vec<u8>>,
    pub index: u32,
    /// Identifies the face, so asking twice doesn't load it twice.
    pub id: String,
}

impl FontFile {
    pub(crate) fn has_char(&self, c: char) -> bool {
        has_char(&self.bytes, self.index, c)
    }
}

pub(crate) fn has_char(bytes: &[u8], index: u32, c: char) -> bool {
    FontRef::from_index(bytes, index as usize).is_some_and(|f| f.charmap().map(c) != 0)
}

/// Finds a font for a character the primary font lacks, or one that should
/// be drawn as emoji.
pub trait Fallback {
    /// A face with a glyph for `c`, as close to bold/italic as it has; with
    /// `emoji`, a color emoji face first.
    fn find(&mut self, c: char, bold: bool, italic: bool, emoji: bool) -> Option<FontFile>;
}

/// No fallback: missing characters stay missing.
pub struct NoFallback;

impl Fallback for NoFallback {
    fn find(&mut self, _: char, _: bool, _: bool, _: bool) -> Option<FontFile> {
        None
    }
}

/// Font files tried in order, each read on first use. Style is ignored:
/// these are last-resort glyphs.
pub struct FileFallback {
    files: Vec<(PathBuf, Option<Arc<Vec<u8>>>)>,
}

impl FileFallback {
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            files: paths.into_iter().map(|path| (path, None)).collect(),
        }
    }

    /// From `JET_FALLBACK_FONTS`, a path list like `PATH`.
    pub fn from_env() -> Self {
        let paths = std::env::var_os("JET_FALLBACK_FONTS").unwrap_or_default();
        Self::new(std::env::split_paths(&paths).filter(|p| !p.as_os_str().is_empty()))
    }
}

impl Fallback for FileFallback {
    fn find(&mut self, c: char, _: bool, _: bool, emoji: bool) -> Option<FontFile> {
        // Emoji: color faces get the first pass.
        let passes: &[bool] = if emoji { &[true, false] } else { &[false] };
        for &color_only in passes {
            if let Some(file) = self.find_in(c, color_only) {
                return Some(file);
            }
        }
        None
    }
}

impl FileFallback {
    fn find_in(&mut self, c: char, color_only: bool) -> Option<FontFile> {
        for (path, loaded) in &mut self.files {
            if loaded.is_none() {
                match std::fs::read(&*path) {
                    Ok(bytes) => *loaded = Some(Arc::new(bytes)),
                    Err(err) => {
                        eprintln!("jet: fallback font {}: {err}", path.display());
                        // Empty: never retried, never matches.
                        *loaded = Some(Arc::default());
                    }
                }
            }
            let Some(bytes) = loaded else { continue };
            let faces = FontDataRef::new(bytes).map_or(0, |data| data.len());
            for index in 0..faces as u32 {
                if has_char(bytes, index, c) && (!color_only || is_color(bytes, index)) {
                    return Some(FontFile {
                        bytes: Arc::clone(bytes),
                        index,
                        id: format!("{}#{index}", path.display()),
                    });
                }
            }
        }
        None
    }
}

/// Whether the face has color glyph tables (COLR layers, CBDT/sbix bitmaps).
/// Whether the face has glyphs jet can draw: outlines or color. Bitmap-only
/// faces (EBDT, Apple's `bdat`, like macOS's NISC18030) map characters but
/// render nothing.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn drawable(bytes: &[u8], index: u32) -> bool {
    FontRef::from_index(bytes, index as usize).is_some_and(|f| {
        [b"glyf", b"CFF ", b"CFF2", b"COLR", b"CBDT", b"sbix"]
            .iter()
            .any(|tag| f.table(swash::tag_from_bytes(tag)).is_some())
    })
}

fn is_color(bytes: &[u8], index: u32) -> bool {
    FontRef::from_index(bytes, index as usize).is_some_and(|f| {
        [b"COLR", b"CBDT", b"sbix"]
            .iter()
            .any(|tag| f.table(swash::tag_from_bytes(tag)).is_some())
    })
}

/// Whether `c` defaults to emoji presentation (Unicode's
/// `Emoji_Presentation`): everything emoji from U+1F000 up, plus the BMP
/// symbols that are, like ✅ ❌ ⚡ ⭐ ✨.
pub(crate) fn emoji_presentation(c: char) -> bool {
    const BMP: &[(u32, u32)] = &[
        (0x231A, 0x231B),
        (0x23E9, 0x23EC),
        (0x23F0, 0x23F0),
        (0x23F3, 0x23F3),
        (0x25FD, 0x25FE),
        (0x2614, 0x2615),
        (0x2648, 0x2653),
        (0x267F, 0x267F),
        (0x2693, 0x2693),
        (0x26A1, 0x26A1),
        (0x26AA, 0x26AB),
        (0x26BD, 0x26BE),
        (0x26C4, 0x26C5),
        (0x26CE, 0x26CE),
        (0x26D4, 0x26D4),
        (0x26EA, 0x26EA),
        (0x26F2, 0x26F3),
        (0x26F5, 0x26F5),
        (0x26FA, 0x26FA),
        (0x26FD, 0x26FD),
        (0x2705, 0x2705),
        (0x270A, 0x270B),
        (0x2728, 0x2728),
        (0x274C, 0x274C),
        (0x274E, 0x274E),
        (0x2753, 0x2755),
        (0x2757, 0x2757),
        (0x2795, 0x2797),
        (0x27B0, 0x27B0),
        (0x27BF, 0x27BF),
        (0x2B1B, 0x2B1C),
        (0x2B50, 0x2B50),
        (0x2B55, 0x2B55),
    ];
    let c = u32::from(c);
    c >= 0x1F000 || BMP.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}

/// An installed font family's regular, bold, italic and bold italic faces
/// (the nearest match for each).
pub fn system_family(name: &str) -> Option<[FontFile; 4]> {
    #[cfg(windows)]
    {
        directwrite::SystemFonts::new().ok()?.family(name)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        installed::SystemFonts::new().family(name)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = name;
        None
    }
}

/// The platform's fallback: installed fonts on Windows, and on Linux and
/// macOS unless `JET_FALLBACK_FONTS` lists files to use instead.
pub fn system() -> Box<dyn Fallback> {
    #[cfg(windows)]
    match directwrite::SystemFonts::new() {
        Ok(fonts) => return Box::new(fonts),
        Err(err) => eprintln!("jet: DirectWrite unavailable, no font fallback: {err}"),
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if std::env::var_os("JET_FALLBACK_FONTS").is_none() {
        return Box::new(installed::SystemFonts::new());
    }
    Box::new(FileFallback::from_env())
}
