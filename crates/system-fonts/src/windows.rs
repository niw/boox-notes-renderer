//! DirectWrite backend (Windows), via `dwrote`.
//!
//! Family lookup is `IDWriteFontCollection::FindFamilyName` (case-insensitive,
//! misses cleanly) plus `GetFirstMatchingFont` for the upright Regular face.
//! Per-character fallback is `IDWriteFontFallback::MapCharacters` — the same
//! system fallback Windows itself uses (Windows 8.1+; older systems resolve
//! nothing). DirectWrite hands back the font file path and `.ttc` face index
//! directly, so no name-table reading is needed here.

use std::borrow::Cow;

use dwrote::{
    Font, FontCollection, FontFallback, FontStretch, FontStyle, FontWeight, NumberSubstitution,
    TextAnalysisSource,
};
use winapi::um::dwrite::{DWRITE_READING_DIRECTION, DWRITE_READING_DIRECTION_LEFT_TO_RIGHT};

use crate::FaceRef;

pub(crate) fn find_family(name: &str) -> Option<FaceRef> {
    let collection = FontCollection::system();
    let family = collection.font_family_by_name(name).ok()??;
    let font = family
        .first_matching_font(FontWeight::Regular, FontStretch::Normal, FontStyle::Normal)
        .ok()?;
    face_ref(&font)
}

pub(crate) fn fallback_for_char(c: char, lang: Option<&str>) -> Option<FaceRef> {
    let fallback = FontFallback::get_system_fallback()?;
    // MapCharacters takes a text-analysis source; the locale it reports is
    // how the language hint reaches the fallback (Han disambiguation).
    let locale = lang.unwrap_or("");
    let text: Vec<u16> = c.encode_utf16(&mut [0; 2]).to_vec();
    let text_length = text.len() as u32;
    let number_subst = NumberSubstitution::new(
        winapi::um::dwrite::DWRITE_NUMBER_SUBSTITUTION_METHOD_NONE,
        locale,
        true,
    );
    let source = TextAnalysisSource::from_text_and_number_subst(
        Box::new(Analysis {
            locale: locale.to_string(),
            length: text_length,
        }),
        Cow::Owned(text),
        number_subst,
    );
    let collection = FontCollection::system();
    let result = fallback.map_characters(
        &source,
        0,
        text_length,
        &collection,
        None,
        FontWeight::Regular,
        FontStyle::Normal,
        FontStretch::Normal,
    );
    face_ref(&result.mapped_font?)
}

fn face_ref(font: &Font) -> Option<FaceRef> {
    let face = font.create_font_face();
    let file = face.files().ok()?.into_iter().next()?;
    Some(FaceRef {
        path: file.font_file_path().ok()?,
        index: face.get_index(),
    })
}

/// The minimal `IDWriteTextAnalysisSource`: one run, one locale, LTR.
struct Analysis {
    locale: String,
    length: u32,
}

impl dwrote::TextAnalysisSourceMethods for Analysis {
    fn get_locale_name(&self, text_position: u32) -> (Cow<'_, str>, u32) {
        (
            Cow::Borrowed(self.locale.as_str()),
            self.length.saturating_sub(text_position),
        )
    }

    fn get_paragraph_reading_direction(&self) -> DWRITE_READING_DIRECTION {
        DWRITE_READING_DIRECTION_LEFT_TO_RIGHT
    }
}
