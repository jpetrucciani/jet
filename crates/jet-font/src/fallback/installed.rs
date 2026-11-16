//! Installed fonts, read by fontdb: on Linux from the directories
//! fontconfig's config names, on macOS from the system's font folders.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use fontdb::{Database, Family, ID, Query, Source, Style, Weight};

use crate::fallback::{Fallback, FontFile, drawable, has_char, is_color};

pub struct SystemFonts {
    db: Database,
    /// Characters nothing installed has, so they aren't searched for again.
    missing: HashSet<(char, bool, bool, bool)>,
}

impl SystemFonts {
    pub fn new() -> Self {
        let mut db = Database::new();
        db.load_system_fonts();
        Self {
            db,
            missing: HashSet::new(),
        }
    }

    /// A family's regular, bold, italic and bold italic faces (the nearest
    /// it has of each), by name in any case.
    pub fn family(&self, name: &str) -> Option<[FontFile; 4]> {
        let name = self
            .db
            .faces()
            .flat_map(|face| &face.families)
            .map(|(family, _)| family)
            .find(|family| family.eq_ignore_ascii_case(name))?;
        let face = |weight, style| {
            let id = self.db.query(&Query {
                families: &[Family::Name(name)],
                weight,
                style,
                ..Query::default()
            })?;
            self.file(id)
        };
        Some([
            face(Weight::NORMAL, Style::Normal)?,
            face(Weight::BOLD, Style::Normal)?,
            face(Weight::NORMAL, Style::Italic)?,
            face(Weight::BOLD, Style::Italic)?,
        ])
    }

    fn file(&self, id: ID) -> Option<FontFile> {
        let (source, index) = self.db.face_source(id)?;
        let (path, bytes) = match source {
            Source::File(path) => {
                let bytes = std::fs::read(&path)
                    .inspect_err(|err| eprintln!("jet: font {}: {err}", path.display()))
                    .ok()?;
                (path, bytes)
            }
            Source::SharedFile(path, data) => (path, (*data).as_ref().to_vec()),
            Source::Binary(data) => (PathBuf::from("memory"), (*data).as_ref().to_vec()),
        };
        Some(FontFile {
            bytes: Arc::new(bytes),
            index,
            id: format!("{}#{index}", path.display()),
        })
    }
}

impl Fallback for SystemFonts {
    /// The best installed face with `c`: color for emoji, then monospaced
    /// (symbols sized for a grid), then the closest style.
    fn find(&mut self, c: char, bold: bool, italic: bool, emoji: bool) -> Option<FontFile> {
        let key = (c, bold, italic, emoji);
        if self.missing.contains(&key) {
            return None;
        }
        let best = 4 * u8::from(emoji) + 2 + 1;
        let mut found: Option<(u8, ID)> = None;
        for face in self.db.faces() {
            let Some((has, color)) = self.db.with_face_data(face.id, |data, index| {
                let has = has_char(data, index, c) && drawable(data, index);
                (has, has && emoji && is_color(data, index))
            }) else {
                continue;
            };
            if !has {
                continue;
            }
            let style = (face.weight >= Weight::SEMIBOLD) == bold
                && (face.style != Style::Normal) == italic;
            let score = 4 * u8::from(color) + 2 * u8::from(face.monospaced) + u8::from(style);
            if found.is_none_or(|(s, _)| score > s) {
                found = Some((score, face.id));
            }
            if score == best {
                break;
            }
        }
        let file = found.and_then(|(_, id)| self.file(id));
        if file.is_none() {
            self.missing.insert(key);
        }
        file
    }
}
