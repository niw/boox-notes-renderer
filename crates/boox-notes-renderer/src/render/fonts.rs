//! Font resolution: map the font a note asks for (by name, with the BOOX
//! device path's file stem as an extra name hint) to an actual font file on
//! this host.
//!
//! Two axes. [`FontDb::find_one`] tries one family name in every *place*, in
//! priority order: the local index (the `--fonts-dir` dirs, then the
//! `--download-fonts` cache dir, scanned non-recursively), a Google Fonts
//! download (`--download-fonts`), then the OS font APIs (the `system-fonts`
//! feature — the only source of *system* fonts; no fixed system path is ever
//! scanned). [`FontDb::resolve`] tries name *candidates* through `find_one`,
//! in order: the `--map-font` target (an override) → the requested family
//! itself → the built-in BOOX/AOSP substitutes ([`DEFAULT_MAP`]) → a
//! deterministic loose index match → the CJK fallback.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// A resolved font: the file and the face index within it (for `.ttc`).
pub(crate) type ResolvedFont = (PathBuf, u32);

/// Built-in name → candidate families for the fonts BOOX Notes uses (mostly
/// AOSP: Roboto + Noto). Each candidate goes through the full
/// [`FontDb::find_one`] chain; the first found wins. The canonical name leads
/// (a real copy in `--fonts-dir` is the best match), then the Google Fonts
/// name of the same design ("Noto Sans CJK JP" is "Noto Sans JP" there — the
/// downloaded real font beats a system substitute), then system substitutes.
const DEFAULT_MAP: &[(&str, &[&str])] = &[
    (
        "noto sans cjk jp",
        &[
            "noto sans cjk jp",
            "noto sans jp",
            "hiragino sans",
            "hiragino kaku gothic pron",
            "hiragino kaku gothic",
            "yu gothic",
        ],
    ),
    (
        "noto sans jp",
        &["noto sans jp", "hiragino sans", "hiragino kaku gothic pron"],
    ),
    (
        "noto sans cjk kr",
        &["noto sans cjk kr", "noto sans kr", "apple sd gothic neo"],
    ),
    (
        "noto sans cjk sc",
        &[
            "noto sans cjk sc",
            "noto sans sc",
            "pingfang sc",
            "heiti sc",
        ],
    ),
    (
        "noto sans cjk tc",
        &["noto sans cjk tc", "noto sans tc", "pingfang tc"],
    ),
    (
        "noto sans cjk hk",
        &["noto sans cjk hk", "noto sans hk", "pingfang hk"],
    ),
    (
        "noto sans cjk",
        &[
            "noto sans cjk jp",
            "noto sans jp",
            "hiragino sans",
            "pingfang sc",
        ],
    ),
    (
        "noto serif cjk",
        &[
            "noto serif cjk jp",
            "noto serif jp",
            "hiragino mincho pron",
            "yu mincho",
        ],
    ),
    ("noto serif", &["noto serif", "times new roman", "georgia"]),
    (
        "noto sans mono",
        &["noto sans mono", "menlo", "monaco", "courier new"],
    ),
    (
        "roboto mono",
        &["noto sans mono", "menlo", "monaco", "courier new"],
    ),
    (
        "roboto",
        &[
            "roboto",
            "noto sans",
            "helvetica neue",
            "helvetica",
            "arial",
        ],
    ),
    (
        "droid sans mono",
        &["noto sans mono", "menlo", "monaco", "courier new"],
    ),
    (
        "droid sans",
        &[
            "droid sans",
            "noto sans",
            "hiragino sans",
            "noto sans cjk jp",
        ],
    ),
    ("noto sans", &["noto sans", "arial", "helvetica neue"]),
    ("sans", &["arial", "helvetica neue"]),
];

/// Families tried as the ultimate (CID/CJK-capable) fallback.
const FALLBACK_FAMILIES: &[&str] = &[
    "hiragino sans",
    "hiragino kaku gothic pron",
    "hiragino kaku gothic",
    "pingfang sc",
    "yu gothic",
    "meiryo",
    "arial unicode ms",
    "noto sans cjk jp",
    "noto sans jp",
    "noto sans",
    "arial",
];

/// Color-emoji families tried, in order, for `emoji_font` (the host index, so a
/// `--fonts-dir` copy works too). No note ever names these — the BOOX device
/// falls back to its system emoji font at draw time, and we mirror that.
const EMOJI_FAMILIES: &[&str] = &["apple color emoji", "noto color emoji"];

pub(crate) struct FontDb {
    /// The local index: lowercased family name → (path, face index), from the
    /// `--fonts-dir` dirs and the download cache dir (in that order; the best
    /// score wins, ties go to the first seen).
    by_family: HashMap<String, ResolvedFont>,
    fallback: Option<ResolvedFont>,
    /// User mappings: lowercased requested family → lowercased target family.
    user_map: Vec<(String, String)>,
    /// `--font`: force this font for all text, bypassing resolution.
    explicit: Option<ResolvedFont>,
    /// `--download-fonts`: the dynamic Google Fonts catalog, tried by
    /// `find_one` after the local index and before the OS lookup.
    google: Option<super::download::GoogleFonts>,
    /// Font file bytes, mmapped on first use (metrics queries).
    data_cache: RefCell<HashMap<PathBuf, Option<Rc<memmap2::Mmap>>>>,
    /// `(font, char)` → advance width in em units; `None` = the font has no
    /// glyph for the char (the caller should fall back).
    adv_cache: RefCell<HashMap<(ResolvedFont, char), Option<f32>>>,
    /// Lazily resolved color-emoji font (`None` = not resolved yet; `Some(None)`
    /// = resolved, none available). Lazy because the Google-catalog fallback
    /// downloads on demand — only pay it when a note actually contains emoji.
    emoji: RefCell<Option<Option<ResolvedFont>>>,
}

impl FontDb {
    /// Build the local index by scanning `dirs` (the `--fonts-dir` dirs, then
    /// the download cache dir; non-recursive). `user_map` entries are
    /// `(requested, target)` family names; `explicit` forces one font;
    /// `google` is the `--download-fonts` dynamic catalog.
    pub(crate) fn build(
        dirs: &[PathBuf],
        user_map: Vec<(String, String)>,
        explicit: Option<PathBuf>,
        google: Option<super::download::GoogleFonts>,
    ) -> Self {
        // Track each family's best face by how close it is to upright Regular, so
        // a family that spans weights/styles (Hiragino W0–W9, Times + Italic)
        // resolves to the Regular face, not a heavy or italic one.
        let mut scored: HashMap<String, (ResolvedFont, i32)> = HashMap::new();
        for dir in dirs {
            index_dir(dir, &mut scored);
        }
        let by_family: HashMap<String, ResolvedFont> =
            scored.into_iter().map(|(k, (f, _))| (k, f)).collect();
        let user_map = user_map
            .into_iter()
            .map(|(k, v)| (norm(&k), norm(&v)))
            .collect();
        let mut db = Self {
            by_family,
            fallback: None,
            user_map,
            explicit: explicit.map(|p| (p, 0)),
            google,
            data_cache: RefCell::new(HashMap::new()),
            adv_cache: RefCell::new(HashMap::new()),
            emoji: RefCell::new(None),
        };
        db.fallback = FALLBACK_FAMILIES.iter().find_map(|f| db.find_one(f));
        db
    }

    /// The color-emoji font: [`EMOJI_FAMILIES`] through the same
    /// [`find_one`](Self::find_one) chain as any other family (a `--fonts-dir`
    /// copy or the cached download wins; the OS provides Apple Color Emoji).
    /// Memoized; `--font` is deliberately ignored (it forces *text*, emoji
    /// stay bitmaps).
    pub(crate) fn emoji_font(&self) -> Option<ResolvedFont> {
        if let Some(memo) = self.emoji.borrow().as_ref() {
            return memo.clone();
        }
        let result = EMOJI_FAMILIES.iter().find_map(|f| self.find_one(f));
        *self.emoji.borrow_mut() = Some(result.clone());
        result
    }

    /// Advance width of `c` in `font`, in em units (cached); `None` when the
    /// font has no glyph for `c`.
    pub(crate) fn advance_em(&self, font: &ResolvedFont, c: char) -> Option<f32> {
        let key = (font.clone(), c);
        if let Some(adv) = self.adv_cache.borrow().get(&key) {
            return *adv;
        }
        let adv = (|| {
            let data = self.font_data(&font.0)?;
            let face = ttf_parser::Face::parse(&data, font.1).ok()?;
            let gid = face.glyph_index(c)?;
            let upem = face.units_per_em() as f32;
            Some(face.glyph_hor_advance(gid).unwrap_or(0) as f32 / upem)
        })();
        self.adv_cache.borrow_mut().insert(key, adv);
        adv
    }

    /// The font that will actually draw `c` for a box resolved to `font`:
    /// the font itself; else — with `system-fonts` — the OS's locale-aware
    /// system fallback for `c`'s script; else the single global fallback.
    /// Stays on `font` (tofu) when nothing has the glyph.
    pub(crate) fn font_for_char(
        &self,
        font: &ResolvedFont,
        c: char,
        lang: Option<&str>,
    ) -> ResolvedFont {
        if self.advance_em(font, c).is_some() {
            return font.clone();
        }
        #[cfg(not(feature = "system-fonts"))]
        let _ = lang;
        // The OS's system fallback — asked with the actual character, so the
        // OS picks something capable of drawing it (verified against the
        // cmap anyway). `lang` (from the box's requested family) breaks Han
        // unification; without it the user's language settings decide.
        #[cfg(feature = "system-fonts")]
        if let Some(fb) = super::system_fonts::fallback_for_char(c, lang)
            && fb != *font
            && self.advance_em(&fb, c).is_some()
        {
            return fb;
        }
        match self.fallback {
            Some(ref fb) if fb != font && self.advance_em(fb, c).is_some() => fb.clone(),
            _ => font.clone(),
        }
    }

    /// Mmap a font file once and cache it.
    pub(crate) fn font_data(&self, path: &Path) -> Option<Rc<memmap2::Mmap>> {
        if let Some(d) = self.data_cache.borrow().get(path) {
            return d.clone();
        }
        let data = (|| {
            let file = std::fs::File::open(path).ok()?;
            // mmap so a 100+MB .ttc isn't fully read for a few glyph widths.
            let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
            Some(Rc::new(mmap))
        })();
        self.data_cache
            .borrow_mut()
            .insert(path.to_path_buf(), data.clone());
        data
    }

    /// Try one family name in every place we know, in priority order: the
    /// local index (`--fonts-dir` dirs, then the download cache), a Google
    /// Fonts download (`--download-fonts`; the catalog check is a local map,
    /// so misses cost nothing), then the OS font APIs (the `system-fonts`
    /// feature; matched case-insensitively, so normalized names work). Exact
    /// name everywhere — loose matching is `resolve`'s last resort only.
    fn find_one(&self, family: &str) -> Option<ResolvedFont> {
        if family.is_empty() {
            return None;
        }
        if let Some(f) = self.by_family.get(family) {
            return Some(f.clone());
        }
        if let Some(google) = &self.google
            && let Some(p) = google.get(family)
        {
            return Some((p, 0));
        }
        #[cfg(feature = "system-fonts")]
        if let Some(f) = super::system_fonts::query(family) {
            return Some(f);
        }
        None
    }

    /// Resolve a font request to a host font file by trying name candidates
    /// through [`find_one`](Self::find_one): the `--map-font` target (an
    /// explicit override) → the requested family itself → the built-in
    /// substitutes ([`DEFAULT_MAP`]) → a loose index match. Returns the
    /// fallback (or `None`) if nothing matches. The BOOX device path `hint`
    /// is only a *name* hint — its file stem joins the candidates; the path
    /// itself is never used (it's a device path, not a host path).
    pub(crate) fn resolve(
        &self,
        request: Option<&str>,
        hint: Option<&Path>,
    ) -> Option<ResolvedFont> {
        if self.explicit.is_some() {
            return self.explicit.clone();
        }
        let req = request.map(norm).unwrap_or_default();
        let stem = hint
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(norm)
            .unwrap_or_default();
        let keys = [req.as_str(), stem.as_str()];
        let keys = || keys.into_iter().filter(|k| !k.is_empty());

        // 1. user mapping (an override: applied before the requested name
        //    itself, so `--map-font "X=Y"` wins even when X exists).
        for key in keys() {
            for (from, to) in &self.user_map {
                if key.contains(from.as_str())
                    && let Some(f) = self.find_one(to)
                {
                    return Some(f);
                }
            }
        }
        // 2. the requested family itself. Exact name only, so e.g. "Noto
        //    Sans" can't grab "Noto Sans Syloti Nagri" (loose matching waits
        //    until step 4).
        for key in keys() {
            if let Some(f) = self.find_one(key) {
                return Some(f);
            }
        }
        // 3. built-in substitutes.
        for key in keys() {
            for (pat, targets) in DEFAULT_MAP {
                if key.contains(pat) {
                    for t in *targets {
                        if let Some(f) = self.find_one(t) {
                            return Some(f);
                        }
                    }
                }
            }
        }
        // 4. loose (substring) index match as a last resort (the OS APIs
        //    can't substring-match, so this is index-only).
        for key in keys() {
            if let Some(f) = lookup(&self.by_family, key) {
                return Some(f);
            }
        }
        self.fallback.clone()
    }
}

/// How one character of a text box will be drawn — the single place that
/// decides per-char rendering. Text layout (`render::text`) and the subset
/// collection ([`used_chars`]) both consume this, so they cannot drift apart
/// (a drift would mean tofu: a drawn glyph missing from the embedded subset).
#[derive(Clone, PartialEq)]
pub(crate) enum CharDraw {
    /// An emoji modifier/joiner that renders nothing on its own.
    Skip,
    /// A color-emoji bitmap, drawn as an inline image (one em square).
    Emoji,
    /// A glyph from this font (the box's resolved font, or the per-char CJK
    /// fallback when the box's font lacks the glyph).
    Glyph(ResolvedFont),
}

/// Decide how `c` will be drawn for a box resolved to `box_font`.
pub(crate) fn char_draw(
    db: &FontDb,
    box_font: &ResolvedFont,
    c: char,
    lang: Option<&str>,
) -> CharDraw {
    if super::emoji::is_emoji_modifier(c) {
        CharDraw::Skip
    } else if super::emoji::is_emoji(c) && super::emoji::png_for(db, c).is_some() {
        CharDraw::Emoji
    } else {
        CharDraw::Glyph(db.font_for_char(box_font, c, lang))
    }
}

/// BCP-47 language hint from a box's requested family name ("Noto Sans CJK
/// JP" → "ja"), passed to the OS per-char fallback to break Han unification:
/// kanji in a JP-font box stay Japanese-shaped even when the user's system
/// language prefers Chinese (and vice versa). `None` when the name carries
/// no language, leaving the decision to the OS's language settings.
pub(crate) fn lang_hint(font_name: Option<&str>) -> Option<&'static str> {
    let name = norm(font_name?);
    for (suffix, lang) in [
        ("jp", "ja"),
        ("kr", "ko"),
        ("sc", "zh-Hans"),
        ("tc", "zh-Hant"),
        ("hk", "zh-HK"),
    ] {
        if name.contains(&format!("cjk {suffix}")) || name.ends_with(&format!(" {suffix}")) {
            return Some(lang);
        }
    }
    None
}

/// Collect the characters each font will draw across `canvases` (via
/// [`char_draw`], the same per-char choice the text painter makes) — for
/// backends that embed subset fonts. Pass only the canvases actually being
/// emitted, so subsets don't carry glyphs from unselected pages.
pub(crate) fn used_chars<'a>(
    db: &FontDb,
    canvases: impl IntoIterator<Item = &'a crate::model::Canvas>,
) -> BTreeMap<ResolvedFont, BTreeSet<char>> {
    // BTreeMap (not HashMap): the SVG backend names subset families by iteration
    // order, so a deterministic order keeps the emitted SVG byte-stable.
    let mut used: BTreeMap<ResolvedFont, BTreeSet<char>> = BTreeMap::new();
    for canvas in canvases {
        for item in &canvas.items {
            let crate::model::RenderItem::Text(t) = item else {
                continue;
            };
            let Some(font) = db.resolve(t.font_name.as_deref(), t.font_hint.as_deref()) else {
                continue;
            };
            let lang = lang_hint(t.font_name.as_deref());
            for c in t.runs.iter().flat_map(|r| r.text.chars()) {
                if let CharDraw::Glyph(f) = char_draw(db, &font, c, lang) {
                    used.entry(f).or_default().insert(c);
                }
            }
        }
    }
    used
}

/// Subset `font` to `chars` (+ `.notdef`) as a standalone minimal OpenType font
/// with a Unicode cmap. `None` if reading/subsetting fails — the caller should
/// embed the full font instead.
pub(crate) fn subset_font_bytes(font: &ResolvedFont, chars: &BTreeSet<char>) -> Option<Vec<u8>> {
    if chars.is_empty() {
        return None;
    }
    let bytes = std::fs::read(&font.0).ok()?;
    let face = ttf_parser::Face::parse(&bytes, font.1).ok()?;
    let mut gids: Vec<u16> = chars
        .iter()
        .filter_map(|&c| face.glyph_index(c))
        .map(|g| g.0)
        .collect();
    gids.push(0); // .notdef is required
    gids.sort_unstable();
    gids.dedup();
    let scope = allsorts::binary::read::ReadScope::new(&bytes);
    let otf = scope.read::<allsorts::tables::OpenTypeFont>().ok()?;
    let provider = otf.table_provider(font.1 as usize).ok()?;
    allsorts::subset::subset(
        &provider,
        &gids,
        &allsorts::subset::SubsetProfile::Minimal,
        allsorts::subset::CmapTarget::Unicode,
    )
    .ok()
}

/// Normalize a family/name for matching: lowercase, `-`/`_` → space, collapse.
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
    out.trim_end().to_string()
}

/// Look up a family in the index: exact match, then substring either way.
fn lookup(by_family: &HashMap<String, ResolvedFont>, fam: &str) -> Option<ResolvedFont> {
    if fam.is_empty() {
        return None;
    }
    if let Some(f) = by_family.get(fam) {
        return Some(f.clone());
    }
    by_family
        .iter()
        .filter(|(k, _)| k.contains(fam) || fam.contains(k.as_str()))
        .min_by(|(ka, _), (kb, _)| {
            lookup_match_rank(ka, fam)
                .cmp(&lookup_match_rank(kb, fam))
                .then(
                    ka.len()
                        .abs_diff(fam.len())
                        .cmp(&kb.len().abs_diff(fam.len())),
                )
                .then(ka.cmp(kb))
        })
        .map(|(_, v)| v.clone())
}

fn lookup_match_rank(key: &str, fam: &str) -> u8 {
    if key.starts_with(fam) {
        0
    } else if fam.starts_with(key) {
        1
    } else if key.contains(fam) {
        2
    } else {
        3
    }
}

/// How far a face is from upright Regular (lower = preferred). Italic/oblique is
/// penalized so the Regular weight wins for a family that spans styles.
fn face_score(face: &ttf_parser::Face) -> i32 {
    let weight = face.weight().to_number() as i32;
    (weight - 400).abs()
        + if face.is_italic() || face.is_oblique() {
            1000
        } else {
            0
        }
}

/// Index every font file directly in `dir` (non-recursive — these are
/// user-supplied flat dirs: `--fonts-dir` and the download cache), keeping the
/// most Regular face per family.
fn index_dir(dir: &Path, scored: &mut HashMap<String, (ResolvedFont, i32)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("ttf" | "otf" | "ttc" | "otc")) {
            continue;
        }
        index_file(&path, scored);
    }
}

/// Index every face of one font file into `scored`, keeping the
/// closest-to-Regular face per family.
fn index_file(path: &Path, scored: &mut HashMap<String, (ResolvedFont, i32)>) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    // mmap so a 100+MB .ttc isn't fully read just to get its name.
    let Ok(mmap) = (unsafe { memmap2::Mmap::map(&file) }) else {
        return;
    };
    let count = ttf_parser::fonts_in_collection(&mmap).unwrap_or(1);
    for index in 0..count {
        let Ok(face) = ttf_parser::Face::parse(&mmap, index) else {
            continue;
        };
        let score = face_score(&face);
        for name in face.names() {
            // 1 = Family, 16 = Typographic Family.
            if (name.name_id == 1 || name.name_id == 16)
                && let Some(s) = name_string(&name)
            {
                let key = norm(&s);
                let better = scored.get(&key).is_none_or(|(_, s)| score < *s);
                if better {
                    scored.insert(key, ((path.to_path_buf(), index), score));
                }
            }
        }
    }
}

/// Decode a name record to a string. `ttf_parser` only decodes the Unicode
/// platforms; some Apple system fonts (notably `Apple Color Emoji.ttc`) carry
/// their family name **only** as a Macintosh-platform (Roman) record, which
/// would otherwise never be indexed. ASCII covers every such name we care
/// about, so non-ASCII Mac-Roman bytes are simply rejected.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_lowercases_and_collapses() {
        assert_eq!(norm("Noto_Sans-CJK  JP"), "noto sans cjk jp");
        assert_eq!(norm("  Hiragino   Sans  "), "hiragino sans");
        assert_eq!(norm("ROBOTO"), "roboto");
    }

    #[test]
    fn lookup_exact_then_substring() {
        let mut by = HashMap::new();
        by.insert("hiragino sans".to_string(), (PathBuf::from("/h.ttc"), 0));
        by.insert("arial".to_string(), (PathBuf::from("/a.ttf"), 0));
        // exact
        assert_eq!(lookup(&by, "arial").unwrap().0, PathBuf::from("/a.ttf"));
        // query is a substring of a key
        assert_eq!(lookup(&by, "hiragino").unwrap().0, PathBuf::from("/h.ttc"));
        // empty never matches
        assert!(lookup(&by, "").is_none());
    }

    #[test]
    fn index_dir_is_flat() {
        // Plant a real host font (smallest .ttf found; skip if none) both
        // directly in the dir and in a subdirectory: only the flat copy is
        // indexed (`--fonts-dir` is non-recursive by contract).
        let host_dirs = [
            "/System/Library/Fonts",
            "/System/Library/Fonts/Supplemental",
            "/usr/share/fonts/truetype/dejavu",
        ];
        let Some(src) = host_dirs
            .iter()
            .filter_map(|d| std::fs::read_dir(d).ok())
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ttf"))
            .min_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX))
        else {
            return;
        };
        let root = std::env::temp_dir().join(format!("bnr-fonts-scan-{}", std::process::id()));
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::copy(&src, nested.join(src.file_name().unwrap())).unwrap();

        // Only the nested copy exists → nothing indexed.
        let mut scored = HashMap::new();
        index_dir(&root, &mut scored);
        let nested_only = scored.is_empty();
        // Add a flat copy → indexed.
        std::fs::copy(&src, root.join(src.file_name().unwrap())).unwrap();
        let mut scored = HashMap::new();
        index_dir(&root, &mut scored);
        let flat_found = !scored.is_empty();

        let _ = std::fs::remove_dir_all(&root);
        assert!(nested_only);
        assert!(flat_found);
    }

    #[test]
    fn lookup_loose_match_is_deterministic() {
        let mut by = HashMap::new();
        by.insert(
            "noto sans syloti nagri".to_string(),
            (PathBuf::from("/syloti.ttf"), 0),
        );
        by.insert("noto sans jp".to_string(), (PathBuf::from("/jp.ttf"), 0));
        by.insert(
            "some noto sans decorative".to_string(),
            (PathBuf::from("/decorative.ttf"), 0),
        );

        assert_eq!(
            lookup(&by, "noto sans").unwrap().0,
            PathBuf::from("/jp.ttf")
        );
    }

    /// Resolution-order tests with a hand-built index. They assert exact
    /// outcomes of the name-candidate logic, so they must run with the OS out
    /// of the loop — `find_one` consults the live system fonts when
    /// `system-fonts` is on, and e.g. a host that ships "Noto Sans CJK JP"
    /// (Linux + fonts-noto-cjk) would resolve the requested name directly
    /// instead of through the cache/substitute path under test. Run under
    /// `cargo test --no-default-features`; the OS path has its own tests.
    #[cfg(not(feature = "system-fonts"))]
    mod resolution {
        use super::*;

        /// Build a `FontDb` without touching the filesystem. `families` plays
        /// the local index (`--fonts-dir` files and cached downloads alike,
        /// keyed by their real family names).
        fn db(
            families: &[(&str, &str)],
            user_map: &[(&str, &str)],
            explicit: Option<&str>,
            fallback: Option<&str>,
        ) -> FontDb {
            FontDb {
                by_family: families
                    .iter()
                    .map(|(k, p)| (k.to_string(), (PathBuf::from(p), 0)))
                    .collect(),
                fallback: fallback.map(|p| (PathBuf::from(p), 0)),
                user_map: user_map
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                explicit: explicit.map(|p| (PathBuf::from(p), 0)),
                google: None,
                data_cache: RefCell::new(HashMap::new()),
                adv_cache: RefCell::new(HashMap::new()),
                emoji: RefCell::new(None),
            }
        }

        fn resolved(db: &FontDb, req: &str) -> Option<PathBuf> {
            db.resolve(Some(req), None).map(|(p, _)| p)
        }

        #[test]
        fn explicit_beats_everything() {
            let d = db(
                &[("noto sans", "/local.ttf")],
                &[("noto sans", "arial")],
                Some("/forced.ttf"),
                None,
            );
            assert_eq!(
                resolved(&d, "Noto Sans"),
                Some(PathBuf::from("/forced.ttf"))
            );
        }

        #[test]
        fn user_map_overrides_the_requested_family() {
            // The mapping applies even though the requested family itself is
            // in the local index (an override, not a rescue).
            let d = db(
                &[
                    ("noto sans jp", "/cache-notosansjp.ttf"),
                    ("hiragino sans", "/hira.ttc"),
                ],
                &[("noto sans cjk jp", "hiragino sans")],
                None,
                None,
            );
            assert_eq!(
                resolved(&d, "Noto Sans CJK JP"),
                Some(PathBuf::from("/hira.ttc"))
            );
        }

        #[test]
        fn cached_download_beats_substitutes() {
            // The cache dir is indexed by real family names; DEFAULT_MAP tries
            // "noto sans jp" (the Google Fonts name of Noto Sans CJK JP)
            // before any system substitute, so the cached real font wins over
            // Hiragino.
            let d = db(
                &[
                    ("hiragino sans", "/hira.ttc"),
                    ("noto sans jp", "/cache-notosansjp.ttf"),
                ],
                &[],
                None,
                None,
            );
            assert_eq!(
                resolved(&d, "Noto Sans CJK JP"),
                Some(PathBuf::from("/cache-notosansjp.ttf"))
            );
        }

        #[test]
        fn emoji_font_prefers_apple_then_noto() {
            // Apple Color Emoji beats Noto Color Emoji when both are local.
            let d = db(
                &[
                    ("noto color emoji", "/noto-emoji.ttf"),
                    ("apple color emoji", "/apple-emoji.ttc"),
                ],
                &[],
                None,
                None,
            );
            assert_eq!(d.emoji_font(), Some((PathBuf::from("/apple-emoji.ttc"), 0)));
            // Local (cached) Noto when Apple is absent from the index.
            let d = db(&[("noto color emoji", "/noto-emoji.ttf")], &[], None, None);
            assert_eq!(d.emoji_font(), Some((PathBuf::from("/noto-emoji.ttf"), 0)));
            // `--font` must not hijack emoji (it forces text only).
            let d = db(&[], &[], Some("/forced.ttf"), None);
            assert_eq!(d.emoji_font(), None);
        }

        #[test]
        fn exact_match_beats_default_map() {
            // The requested family is present verbatim → used directly, not
            // redirected through DEFAULT_MAP.
            let d = db(&[("noto serif", "/local-notoserif.ttf")], &[], None, None);
            assert_eq!(
                resolved(&d, "Noto Serif"),
                Some(PathBuf::from("/local-notoserif.ttf"))
            );
        }

        #[test]
        fn default_map_redirects_when_no_exact() {
            // "Noto Serif" not local (and not a real OS family) → DEFAULT_MAP
            // → Times New Roman from the local index.
            let d = db(&[("times new roman", "/times.ttf")], &[], None, None);
            assert_eq!(
                resolved(&d, "Noto Serif"),
                Some(PathBuf::from("/times.ttf"))
            );
        }

        #[test]
        fn falls_back_when_nothing_matches() {
            let d = db(&[], &[], None, Some("/fallback.ttc"));
            assert_eq!(
                resolved(&d, "Totally Unknown Family"),
                Some(PathBuf::from("/fallback.ttc"))
            );
        }
    }

    #[cfg(all(feature = "system-fonts", target_os = "macos"))]
    #[test]
    fn per_char_fallback_asks_the_system_cascade() {
        // Arial has no Hangul; the system fallback must supply a real font
        // that does (which one depends on the user's language settings).
        let d = FontDb::build(&[], Vec::new(), None, None);
        let arial = d.resolve(Some("Arial"), None).expect("macOS ships Arial");
        let hangul = d.font_for_char(&arial, '한', None);
        assert_ne!(hangul, arial);
        assert!(d.advance_em(&hangul, '한').is_some());
        // 简 (simplified-Chinese-specific) used to be tofu: a script-level
        // answer (Hiragino on ja systems) has no glyph for it — asking with
        // the actual character must find one.
        let hans = d.font_for_char(&arial, '简', Some("zh-Hans"));
        assert_ne!(hans, arial);
        assert!(d.advance_em(&hans, '简').is_some());
        // A char Arial covers stays on Arial.
        assert_eq!(d.font_for_char(&arial, 'a', None), arial);
    }

    #[test]
    fn lang_hint_reads_the_cjk_variant() {
        assert_eq!(lang_hint(Some("Noto Sans CJK JP")), Some("ja"));
        assert_eq!(lang_hint(Some("Noto Sans JP")), Some("ja"));
        assert_eq!(lang_hint(Some("Noto Sans CJK SC")), Some("zh-Hans"));
        assert_eq!(lang_hint(Some("Noto Sans TC")), Some("zh-Hant"));
        assert_eq!(lang_hint(Some("Noto Sans CJK KR")), Some("ko"));
        assert_eq!(lang_hint(Some("Noto Sans")), None);
        assert_eq!(lang_hint(Some("Arial")), None);
        assert_eq!(lang_hint(None), None);
    }
}
