//! Pick the right face inside a font file. The OS APIs name a font (family /
//! PostScript name) and a file, but a `.ttc` holds many faces — these
//! helpers find the index by reading the file's name tables (mmapped, so a
//! 100+MB collection isn't fully read).

#[cfg(target_vendor = "apple")]
use std::path::Path;

/// The face in `path` whose PostScript name (name id 6) is `ps_name`.
#[cfg(target_vendor = "apple")]
pub(crate) fn index_by_postscript_name(path: &Path, ps_name: &str) -> Option<u32> {
    let mmap = map(path)?;
    let count = ttf_parser::fonts_in_collection(&mmap).unwrap_or(1);
    (0..count).find(|&index| {
        ttf_parser::Face::parse(&mmap, index).is_ok_and(|face| {
            face.names().into_iter().any(|name| {
                name.name_id == ttf_parser::name_id::POST_SCRIPT_NAME
                    && name_string(&name).is_some_and(|s| s == ps_name)
            })
        })
    })
}

/// The face in `path` belonging to `family` (case-insensitively) that is
/// closest to upright Regular; with the score so callers can compare across
/// files (macOS ships e.g. Hiragino as one file per weight).
#[cfg(target_vendor = "apple")]
pub(crate) fn best_face_for_family(path: &Path, family: &str) -> Option<(u32, i32)> {
    let mmap = map(path)?;
    let family = norm(family);
    let count = ttf_parser::fonts_in_collection(&mmap).unwrap_or(1);
    let mut best: Option<(u32, i32)> = None;
    for index in 0..count {
        let Ok(face) = ttf_parser::Face::parse(&mmap, index) else {
            continue;
        };
        let matches = face.names().into_iter().any(|name| {
            // 1 = Family, 16 = Typographic Family.
            (name.name_id == ttf_parser::name_id::FAMILY
                || name.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY)
                && name_string(&name).is_some_and(|s| norm(&s) == family)
        });
        if !matches {
            continue;
        }
        let score = face_score(&face);
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((index, score));
        }
    }
    best
}

#[cfg(target_vendor = "apple")]
fn map(path: &Path) -> Option<memmap2::Mmap> {
    let file = std::fs::File::open(path).ok()?;
    unsafe { memmap2::Mmap::map(&file) }.ok()
}

/// Distance from upright Regular: weight distance from 400, italic/oblique
/// heavily penalized.
#[cfg(target_vendor = "apple")]
fn face_score(face: &ttf_parser::Face) -> i32 {
    let weight = face.weight().to_number() as i32;
    (weight - 400).abs()
        + if face.is_italic() || face.is_oblique() {
            1000
        } else {
            0
        }
}

/// Decode a name record. `ttf_parser` only decodes the Unicode platforms;
/// some Apple system fonts (notably Apple Color Emoji) carry names **only**
/// as Macintosh-platform (Roman) records — ASCII covers every such name.
#[cfg(target_vendor = "apple")]
fn name_string(name: &ttf_parser::name::Name) -> Option<String> {
    if let Some(s) = name.to_string() {
        return Some(s);
    }
    (name.platform_id == ttf_parser::PlatformId::Macintosh
        && name.encoding_id == 0
        && name.name.iter().all(u8::is_ascii))
    .then(|| String::from_utf8(name.name.to_vec()).ok())
    .flatten()
}

/// Lowercase and collapse separators, so "Hiragino  Sans" == "hiragino-sans".
/// Used to compare family names (Apple's face picking, fontconfig's
/// best-effort-match guard); DirectWrite matches names itself.
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "freebsd"))]
pub(crate) fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        let c = if c == '-' || c == '_' { ' ' } else { c };
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.extend(c.to_lowercase());
            prev_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}
