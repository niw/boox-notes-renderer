//! fontconfig backend (Linux, FreeBSD), via `yeslogic-fontconfig-sys` with
//! its `dlopen` loader — fontconfig binds at runtime, so building needs no
//! headers and hosts without the library simply resolve nothing.
//!
//! Family lookup is an `FcFontMatch` on `family` (fontconfig matches
//! case-insensitively and its scoring prefers the upright Regular face).
//! Per-character fallback puts the actual character in the pattern's
//! `charset` (fontconfig indexes every font's cmap, so the match is
//! guaranteed to cover the character) plus the language hint in `lang` —
//! without a hint, fontconfig fills `lang` from the user's locale, which is
//! the system's language-settings-dependent behavior. `FcFontMatch` returns
//! the file path and `.ttc` face index directly.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::ptr::null_mut;

use fontconfig_sys::statics::{LIB, LIB_RESULT};
use fontconfig_sys::{
    FcChar8, FcConfig, FcMatchPattern, FcPattern, FcResultMatch,
    constants::{FC_CHARSET, FC_FAMILY, FC_FILE, FC_INDEX, FC_LANG},
};

use crate::FaceRef;

pub(crate) fn find_family(name: &str) -> Option<FaceRef> {
    let cname = CString::new(name).ok()?;
    let query = Query::new()?;
    unsafe {
        (LIB.FcPatternAddString)(
            query.pattern,
            FC_FAMILY.as_ptr(),
            cname.as_ptr() as *const FcChar8,
        );
    }
    // FcFontMatch is best-effort: it always returns *some* font (an unknown
    // family resolves to a default like DejaVu Sans). Confirm the match
    // actually carries the requested family, so a miss is a real miss.
    query.matched_face(Some(name))
}

pub(crate) fn fallback_for_char(c: char, lang: Option<&str>) -> Option<FaceRef> {
    let query = Query::new()?;
    unsafe {
        let charset = (LIB.FcCharSetCreate)();
        if charset.is_null() {
            return None;
        }
        (LIB.FcCharSetAddChar)(charset, c as u32);
        // The pattern copies the charset; destroy our reference either way.
        (LIB.FcPatternAddCharSet)(query.pattern, FC_CHARSET.as_ptr(), charset);
        (LIB.FcCharSetDestroy)(charset);
        if let Some(lang) = lang
            && let Ok(lang) = CString::new(lang.to_lowercase())
        {
            (LIB.FcPatternAddString)(
                query.pattern,
                FC_LANG.as_ptr(),
                lang.as_ptr() as *const FcChar8,
            );
        }
    }
    // No family check here: the match is constrained by charset, so any font
    // it returns genuinely covers the character.
    query.matched_face(None)
}

/// An in-progress fontconfig query: the shared current config and a pattern.
/// The pattern is freed on drop; the config is fontconfig's own (never freed).
struct Query {
    config: *mut FcConfig,
    pattern: *mut FcPattern,
}

impl Query {
    fn new() -> Option<Self> {
        if LIB_RESULT.is_err() {
            return None; // no fontconfig on this host
        }
        unsafe {
            // The current config — initialized (config + font set) once on
            // first use and cached by fontconfig, unlike
            // FcInitLoadConfigAndFonts which rebuilds it every call. Owned by
            // fontconfig; must not be destroyed.
            let config = (LIB.FcConfigGetCurrent)();
            if config.is_null() {
                return None;
            }
            let pattern = (LIB.FcPatternCreate)();
            if pattern.is_null() {
                return None;
            }
            Some(Self { config, pattern })
        }
    }

    /// Run the match and pull `file` + `index` out of the result. When
    /// `expect_family` is set, return `None` unless the matched font carries
    /// that family name (fontconfig's match is best-effort, never a miss).
    fn matched_face(self, expect_family: Option<&str>) -> Option<FaceRef> {
        unsafe {
            (LIB.FcConfigSubstitute)(self.config, self.pattern, FcMatchPattern);
            (LIB.FcDefaultSubstitute)(self.pattern);
            let mut result = FcResultMatch;
            let matched = (LIB.FcFontMatch)(self.config, self.pattern, &mut result);
            if matched.is_null() || result != FcResultMatch {
                return None;
            }
            let face = (|| {
                if let Some(expect) = expect_family
                    && !pattern_has_family(matched, expect)
                {
                    return None;
                }
                let mut file: *mut FcChar8 = null_mut();
                if (LIB.FcPatternGetString)(matched, FC_FILE.as_ptr(), 0, &mut file)
                    != FcResultMatch
                    || file.is_null()
                {
                    return None;
                }
                let path = PathBuf::from(CStr::from_ptr(file as *const c_char).to_str().ok()?);
                let mut index: c_int = 0;
                if (LIB.FcPatternGetInteger)(matched, FC_INDEX.as_ptr(), 0, &mut index)
                    != FcResultMatch
                {
                    index = 0;
                }
                Some(FaceRef {
                    path,
                    index: index.max(0) as u32,
                })
            })();
            (LIB.FcPatternDestroy)(matched);
            face
        }
    }
}

/// Whether `pattern` lists `family` among its (multiple, alias-included)
/// `FC_FAMILY` values, compared normalized.
unsafe fn pattern_has_family(pattern: *mut FcPattern, family: &str) -> bool {
    let want = crate::face::norm(family);
    let mut i = 0;
    loop {
        let mut value: *mut FcChar8 = null_mut();
        unsafe {
            if (LIB.FcPatternGetString)(pattern, FC_FAMILY.as_ptr(), i, &mut value) != FcResultMatch
                || value.is_null()
            {
                return false;
            }
            if let Ok(s) = CStr::from_ptr(value as *const c_char).to_str()
                && crate::face::norm(s) == want
            {
                return true;
            }
        }
        i += 1;
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        unsafe {
            (LIB.FcPatternDestroy)(self.pattern);
        }
    }
}
