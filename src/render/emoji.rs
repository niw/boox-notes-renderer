//! Color-emoji support: extract the PNG bitmap a color-emoji font stores for a
//! codepoint, so emoji render as images instead of `.notdef` tofu. The font is
//! resolved through [`FontDb`] like any other font — the emoji families from
//! the host index (Apple Color Emoji, Noto Color Emoji), then the
//! `--download-fonts` mechanisms (see `FontDb::emoji_font`). Single-codepoint
//! emoji only; ZWJ sequences fall back to their components. Results are cached
//! per (font, char).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use super::fonts::FontDb;

/// Modifiers/joiners that shouldn't render on their own.
pub(crate) fn is_emoji_modifier(c: char) -> bool {
    matches!(c as u32, 0xFE0E | 0xFE0F | 0x200D | 0x1F3FB..=0x1F3FF)
}

/// Whether a codepoint is in a block we treat as (color) emoji.
pub(crate) fn is_emoji(c: char) -> bool {
    matches!(c as u32,
        0x1F000..=0x1FAFF      // pictographs, symbols, supplemental, extended-A
        | 0x2600..=0x27BF      // misc symbols + dingbats (☺ ❤ ✅ …)
        | 0x2300..=0x23FF      // technical (⌚ ⏰ …)
        | 0x2B00..=0x2BFF      // misc symbols and arrows (⭐ …)
        | 0x2122 | 0x2139 | 0x24C2 | 0x3030 | 0x303D | 0x3297 | 0x3299
        | 0x25AA..=0x25FE)
}

/// The PNG bitmap for `c` from the resolved color-emoji font, or `None` if no
/// emoji font is available or it has no bitmap for `c`. Returned behind an
/// `Arc` so existence checks and repeat draws don't copy the bytes.
pub(crate) fn png_for(db: &FontDb, c: char) -> Option<Arc<Vec<u8>>> {
    type Cache = Mutex<HashMap<(PathBuf, u32, char), Option<Arc<Vec<u8>>>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let font = db.emoji_font()?;
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (font.0.clone(), font.1, c);
    if let Some(hit) = cache.lock().unwrap().get(&key) {
        return hit.clone();
    }
    let result = (|| {
        // mmapped + cached by the FontDb, so the ~190 MB Apple Color Emoji .ttc
        // is never fully read.
        let data = db.font_data(&font.0)?;
        let face = ttf_parser::Face::parse(&data, font.1).ok()?;
        let gid = face.glyph_index(c)?;
        let img = face.glyph_raster_image(gid, u16::MAX)?;
        (img.format == ttf_parser::RasterImageFormat::PNG).then(|| Arc::new(img.data.to_vec()))
    })();
    cache.lock().unwrap().insert(key, result.clone());
    result
}
