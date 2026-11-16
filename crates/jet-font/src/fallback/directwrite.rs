//! Fallback from the installed fonts through DirectWrite. A short list of
//! Windows' own coverage fonts is tried first so common scripts land on the
//! fonts Windows Terminal would pick; anything else scans every family.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL,
    DWriteCreateFactory, IDWriteFactory, IDWriteFont, IDWriteFontCollection, IDWriteFontFile,
    IDWriteLocalFontFileLoader,
};
use windows::core::{HSTRING, Interface};

use crate::fallback::{Fallback, FontFile};

/// Tried first for emoji; Segoe UI Symbol would otherwise answer with a
/// monochrome version.
const EMOJI: &str = "Segoe UI Emoji";

/// Tried before the full scan, in order.
const PREFERRED: &[&str] = &[
    "Segoe UI Symbol",
    "Segoe UI Emoji",
    "Microsoft YaHei",
    "Yu Gothic",
    "Malgun Gothic",
    "Microsoft JhengHei",
    "Nirmala UI",
    "Segoe UI",
    "Segoe UI Historic",
    "Cambria Math",
];

pub struct SystemFonts {
    collection: IDWriteFontCollection,
    /// Bytes per file path, shared by faces of one collection file.
    files: HashMap<String, Arc<Vec<u8>>>,
}

impl SystemFonts {
    pub fn new() -> windows::core::Result<Self> {
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let mut collection = None;
        unsafe { factory.GetSystemFontCollection(&mut collection, false)? };
        Ok(Self {
            collection: collection.ok_or_else(windows::core::Error::empty)?,
            files: HashMap::new(),
        })
    }

    fn matching(&self, family: u32, bold: bool, italic: bool) -> Option<IDWriteFont> {
        let family = unsafe { self.collection.GetFontFamily(family) }.ok()?;
        let weight = if bold {
            DWRITE_FONT_WEIGHT_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        };
        let style = if italic {
            DWRITE_FONT_STYLE_ITALIC
        } else {
            DWRITE_FONT_STYLE_NORMAL
        };
        unsafe { family.GetFirstMatchingFont(weight, DWRITE_FONT_STRETCH_NORMAL, style) }.ok()
    }

    fn family_index(&self, name: &str) -> Option<u32> {
        let (mut index, mut exists) = (0, false.into());
        unsafe {
            self.collection
                .FindFamilyName(&HSTRING::from(name), &mut index, &mut exists)
        }
        .ok()?;
        exists.as_bool().then_some(index)
    }

    pub fn family(&mut self, name: &str) -> Option<[FontFile; 4]> {
        let family = self.family_index(name)?;
        let mut face = |bold, italic| {
            let font = self.matching(family, bold, italic)?;
            self.load(&font)
        };
        Some([
            face(false, false)?,
            face(true, false)?,
            face(false, true)?,
            face(true, true)?,
        ])
    }

    /// The font's file on disk, read once per path.
    fn load(&mut self, font: &IDWriteFont) -> Option<FontFile> {
        let face = unsafe { font.CreateFontFace() }.ok()?;
        let mut count = 1;
        let mut files: [Option<IDWriteFontFile>; 1] = [None];
        unsafe { face.GetFiles(&mut count, Some(files.as_mut_ptr())) }.ok()?;
        let [Some(file)] = files else { return None };
        let path = local_path(&file)?;
        let index = unsafe { face.GetIndex() };
        let bytes = match self.files.get(&path) {
            Some(bytes) => Arc::clone(bytes),
            None => {
                let bytes = Arc::new(std::fs::read(&path).ok()?);
                self.files.insert(path.clone(), Arc::clone(&bytes));
                bytes
            }
        };
        Some(FontFile {
            bytes,
            index,
            id: format!("{path}#{index}"),
        })
    }
}

impl Fallback for SystemFonts {
    fn find(&mut self, c: char, bold: bool, italic: bool, emoji: bool) -> Option<FontFile> {
        let has = |font: &IDWriteFont| {
            unsafe { font.HasCharacter(u32::from(c)) }.is_ok_and(|has| has.as_bool())
        };
        let emoji = emoji.then_some(EMOJI);
        let preferred = emoji
            .iter()
            .chain(PREFERRED)
            .filter_map(|name| self.family_index(name));
        let all = 0..unsafe { self.collection.GetFontFamilyCount() };
        let font = preferred
            .chain(all)
            .filter_map(|family| self.matching(family, bold, italic))
            .find(has)?;
        // DirectWrite says yes for some synthesized or remote faces; make
        // sure the file we read really has the glyph.
        self.load(&font).filter(|file| file.has_char(c))
    }
}

/// Path of a font file that lives on local disk (all installed fonts do).
fn local_path(file: &IDWriteFontFile) -> Option<String> {
    let mut key: *mut c_void = std::ptr::null_mut();
    let mut key_size = 0;
    unsafe { file.GetReferenceKey(&mut key, &mut key_size) }.ok()?;
    let loader: IDWriteLocalFontFileLoader = unsafe { file.GetLoader() }.ok()?.cast().ok()?;
    let len = unsafe { loader.GetFilePathLengthFromKey(key, key_size) }.ok()?;
    let mut path = vec![0u16; len as usize + 1];
    unsafe { loader.GetFilePathFromKey(key, key_size, &mut path) }.ok()?;
    path.truncate(len as usize);
    Some(String::from_utf16_lossy(&path))
}
