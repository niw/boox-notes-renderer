//! Core Text backend (macOS, iOS, and the other Apple platforms).
//!
//! Family lookup goes through font *descriptor matching*
//! (`CTFontDescriptorCreateMatchingFontDescriptors`), which — unlike
//! `CTFontCreateWithName` — returns nothing for unknown families instead of
//! falling back to a default font. Per-character fallback is
//! `CTFontCreateForString(WithLanguage)`, the system cascade itself.
//!
//! Core Text names a font (family / PostScript name) and a file URL, but not
//! the face index inside a `.ttc` — `crate::face` finds that by reading the
//! file's name tables.

use std::collections::HashSet;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use objc2_core_foundation::{
    CFArray, CFDictionary, CFRange, CFRetained, CFString, CFType, CFURL, CFURLPathStyle,
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use objc2_core_text::{CTFont, CTFontDescriptor, kCTFontFamilyNameAttribute, kCTFontURLAttribute};

use crate::{FaceRef, face};

pub(crate) fn find_family(name: &str) -> Option<FaceRef> {
    let descriptor = family_descriptor(name)?;
    let matched = unsafe { descriptor.matching_font_descriptors(None) }?;
    // The matched array is untyped; every element is a CTFontDescriptor.
    let matched: CFRetained<CFArray<CTFontDescriptor>> =
        unsafe { CFRetained::cast_unchecked(matched) };
    // The family's faces may be spread over several files (macOS ships
    // Hiragino as one file per weight) and a file may carry several families
    // (PingFang's collection has SC/TC/HK variants): inspect every distinct
    // file and keep the requested family's closest-to-Regular face.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut best: Option<(FaceRef, i32)> = None;
    for i in 0..matched.len() {
        let Some(descriptor) = matched.get(i) else {
            continue;
        };
        let Some(path) = descriptor_path(&descriptor) else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Some((index, score)) = face::best_face_for_family(&path, name)
            && best.as_ref().is_none_or(|(_, s)| score < *s)
        {
            best = Some((FaceRef { path, index }, score));
        }
    }
    best.map(|(face, _)| face)
}

pub(crate) fn fallback_for_char(c: char, lang: Option<&str>) -> Option<FaceRef> {
    let mut buf = [0; 4];
    let text = CFString::from_str(c.encode_utf8(&mut buf));
    let range = CFRange {
        location: 0,
        length: text.length(),
    };
    let base = base_font()?;
    let font = unsafe {
        match lang {
            Some(lang) => {
                let lang = CFString::from_str(lang);
                CTFont::for_string_with_language(&base, &text, range, Some(&lang))
            }
            None => CTFont::for_string(&base, &text, range),
        }
    };
    let path = url_to_path(unsafe { font.attribute(kCTFontURLAttribute) })?;
    // Find the exact face Core Text chose; fall back to the family's Regular
    // when the PostScript name isn't in the file's name table.
    let ps_name = unsafe { font.post_script_name() }.to_string();
    let index = face::index_by_postscript_name(&path, &ps_name).or_else(|| {
        let family = unsafe { font.family_name() }.to_string();
        face::best_face_for_family(&path, &family).map(|(index, _)| index)
    })?;
    Some(FaceRef { path, index })
}

/// A descriptor matching exactly `{ family name: name }`.
fn family_descriptor(name: &str) -> Option<CFRetained<CTFontDescriptor>> {
    let family = CFString::from_str(name);
    unsafe {
        let mut keys: [*const c_void; 1] = [(kCTFontFamilyNameAttribute as *const CFString).cast()];
        let mut values: [*const c_void; 1] = [(&*family as *const CFString).cast()];
        let attributes = CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )?;
        Some(CTFontDescriptor::with_attributes(&attributes))
    }
}

/// A neutral base font for `CTFontCreateForString` (an empty descriptor at
/// the default size), so the cascade isn't biased toward any family.
fn base_font() -> Option<CFRetained<CTFont>> {
    unsafe {
        let attributes = CFDictionary::new(None, null_mut(), null_mut(), 0, null(), null())?;
        let descriptor = CTFontDescriptor::with_attributes(&attributes);
        Some(CTFont::with_font_descriptor(&descriptor, 0.0, null()))
    }
}

fn descriptor_path(descriptor: &CTFontDescriptor) -> Option<PathBuf> {
    url_to_path(unsafe { descriptor.attribute(kCTFontURLAttribute) })
}

/// A `kCTFontURLAttribute` value (from a `CTFont` or `CTFontDescriptor`) as a
/// filesystem path.
fn url_to_path(url: Option<CFRetained<CFType>>) -> Option<PathBuf> {
    let url = url?.downcast::<CFURL>().ok()?;
    Some(PathBuf::from(
        url.file_system_path(CFURLPathStyle::CFURLPOSIXPathStyle)?
            .to_string(),
    ))
}
