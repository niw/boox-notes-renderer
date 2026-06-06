//! Bridge to the `system-fonts` workspace crate (the `system-fonts` cargo
//! feature, in the default set) — the only source of *system* fonts: no
//! fixed system path is ever scanned. It asks the OS font APIs (Core Text on
//! macOS/iOS, DirectWrite on Windows, fontconfig on Linux) and answers with
//! a file path + `.ttc` face index, exactly what the mmap/subset pipeline
//! needs. Results are memoized inside the crate. Without the feature the
//! build is pure Rust and fonts come only from `--fonts-dir` and
//! `--download-fonts`.

use super::fonts::ResolvedFont;

/// Ask the OS for `family` (matched case-insensitively, upright Regular
/// preferred). `None` when the OS doesn't know the family.
pub(crate) fn query(family: &str) -> Option<ResolvedFont> {
    ::system_fonts::find_family(family).map(|face| (face.path, face.index))
}

/// The OS's *system fallback* font for `c` — asked with the actual character
/// (Core Text's `CTFontCreateForString`, DirectWrite's `MapCharacters`,
/// fontconfig's charset matching), so the OS picks something it considers
/// capable of drawing it. `lang` is a BCP-47 hint ("ja", "zh-Hans", …) that
/// breaks Han unification; without it the user's language settings decide.
pub(crate) fn fallback_for_char(c: char, lang: Option<&str>) -> Option<ResolvedFont> {
    ::system_fonts::fallback_for_char(c, lang).map(|face| (face.path, face.index))
}
