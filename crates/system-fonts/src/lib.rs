//! Resolve *system* fonts through the OS font APIs — no font directory is
//! ever scanned. Two queries, answered with a font file path plus the face
//! index inside it (for `.ttc` collections), so callers can mmap/parse/subset
//! the file themselves:
//!
//! - [`find_family`]: a family name (matched case-insensitively) to its
//!   upright Regular face. The OS sees fonts living outside any public font
//!   dir (e.g. PingFang sits in a private framework / on-demand asset on
//!   modern macOS/iOS) and keeps working when the OS moves files around.
//! - [`fallback_for_char`]: the font the OS itself would draw a character
//!   with when the current font lacks a glyph — the locale-aware *system
//!   font fallback* (for Han it follows the user's language settings unless
//!   a language hint is given).
//!
//! Backends: Core Text on Apple platforms (macOS/iOS/…), DirectWrite on
//! Windows, fontconfig on Linux/FreeBSD (loaded at runtime via dlopen; hosts
//! without it just resolve nothing). Other platforms resolve nothing.
//! Results are memoized per thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

#[cfg(target_vendor = "apple")]
#[path = "apple.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod platform;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[path = "fontconfig.rs"]
mod platform;

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd"
)))]
mod platform {
    pub(crate) fn find_family(_name: &str) -> Option<super::FaceRef> {
        None
    }
    pub(crate) fn fallback_for_char(_c: char, _lang: Option<&str>) -> Option<super::FaceRef> {
        None
    }
}

mod face;

/// A font face on disk: the file and the face index within it (0 for plain
/// `.ttf`/`.otf`; the member index for `.ttc`/`.otc` collections).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FaceRef {
    pub path: PathBuf,
    pub index: u32,
}

/// Memo key for [`fallback_for_char`]: the character and the language hint.
type FallbackKey = (char, Option<String>);

thread_local! {
    /// Family name → result (`None` = known miss).
    static FAMILY_MEMO: RefCell<HashMap<String, Option<FaceRef>>> = RefCell::new(HashMap::new());
    /// (char, language hint) → result (`None` = known miss).
    static FALLBACK_MEMO: RefCell<HashMap<FallbackKey, Option<FaceRef>>> =
        RefCell::new(HashMap::new());
}

/// Ask the OS for the family `name` (matched case-insensitively), preferring
/// the upright Regular face. `None` when the OS doesn't know the family.
pub fn find_family(name: &str) -> Option<FaceRef> {
    FAMILY_MEMO.with(|memo| {
        if let Some(hit) = memo.borrow().get(name) {
            return hit.clone();
        }
        let result = platform::find_family(name);
        memo.borrow_mut().insert(name.to_string(), result.clone());
        result
    })
}

/// The font the OS's *system font fallback* picks for `c` — asked with the
/// actual character, so the answer is guaranteed to be a font the OS
/// considers capable of drawing it. `lang` is a BCP-47 hint ("ja",
/// "zh-Hans", …) used to break script ambiguity (Han unification); without
/// it the user's language settings decide. `None` when the OS has nothing.
pub fn fallback_for_char(c: char, lang: Option<&str>) -> Option<FaceRef> {
    FALLBACK_MEMO.with(|memo| {
        let key = (c, lang.map(str::to_string));
        if let Some(hit) = memo.borrow().get(&key) {
            return hit.clone();
        }
        let result = platform::fallback_for_char(c, lang);
        memo.borrow_mut().insert(key, result.clone());
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_family_misses_unknown() {
        assert_eq!(find_family("definitely not a font family"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn find_family_resolves_pingfang_outside_public_font_dirs() {
        // PingFang lives in a private framework / on-demand asset — only the
        // OS knows where. Lowercase queried too: callers pass normalized
        // names, so the match must be case-insensitive.
        for req in ["PingFang SC", "pingfang sc"] {
            let face = find_family(req).expect("macOS ships PingFang");
            assert!(
                face.path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains("pingfang"),
                "unexpected path for {req}: {}",
                face.path.display()
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn find_family_picks_the_regular_face() {
        // macOS ships Hiragino as one file per weight; the match must land
        // on the W4 (Regular, weight 400) file, not whichever comes first.
        let face = find_family("Hiragino Sans").expect("macOS ships Hiragino Sans");
        assert!(
            face.path.to_string_lossy().contains("W4"),
            "expected the W4 file: {}",
            face.path.display()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fallback_asks_with_the_actual_character() {
        // 简 is simplified-Chinese-specific: a script-level answer on a
        // Japanese-language system (Hiragino) has no glyph for it, so only a
        // per-character ask can resolve it.
        let face = fallback_for_char('简', None).expect("macOS draws 简");
        assert!(face.path.exists(), "missing file: {}", face.path.display());
        assert!(fallback_for_char('한', None).is_some());
        assert!(fallback_for_char('あ', None).is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fallback_honors_the_language_hint() {
        // Han unification: the same kanji resolves to different fonts under
        // different language hints (e.g. Hiragino vs PingFang on macOS).
        let ja = fallback_for_char('豆', Some("ja")).expect("ja font");
        let zh = fallback_for_char('豆', Some("zh-Hans")).expect("zh-Hans font");
        assert_ne!(
            ja.path, zh.path,
            "expected different fonts for ja vs zh-Hans"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fontconfig_resolves_real_families_and_fallback() {
        // Needs fontconfig + some fonts; skip on a bare host rather than fail.
        let Some(dejavu) = find_family("DejaVu Sans") else {
            return;
        };
        assert!(dejavu.path.exists());
        // A real family resolves; an unknown one misses (fontconfig's
        // best-effort match must not pass as a hit).
        assert!(find_family("DejaVu Sans").is_some());
        assert_eq!(find_family("Totally Unknown Family XYZ"), None);
        // With a CJK font installed (fonts-noto-cjk), per-char fallback for a
        // kanji resolves to a file that exists.
        if let Some(face) = fallback_for_char('簡', Some("ja")) {
            assert!(face.path.exists());
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn directwrite_resolves_real_families_and_fallback() {
        // Arial ships on every Windows; it must resolve to a real file.
        let arial = find_family("Arial").expect("Windows ships Arial");
        assert!(arial.path.exists());
        // DirectWrite's FindFamilyName misses cleanly (it does not best-effort
        // match like fontconfig), so an unknown family is None.
        assert_eq!(find_family("Totally Unknown Family XYZ"), None);
        // Per-char system fallback (MapCharacters) for a kanji: Windows ships
        // a CJK font (Yu Gothic / MS Gothic), so this resolves to a real file.
        if let Some(face) = fallback_for_char('簡', Some("ja")) {
            assert!(face.path.exists());
        }
    }
}
