//! Font resolution: map the font a note asks for (by name, with the BOOX
//! device path as a hint) to an actual font file on this host.
//!
//! Strategy, in order: forced `--font` → existing BOOX device-path hint →
//! explicit user mapping (`--map-font`) → `--download-fonts` Google catalog →
//! curated downloads → exact requested family → built-in BOOX/AOSP family map →
//! deterministic loose match → CJK/system fallback. Fonts are discovered by
//! scanning the system font dirs plus any `--fonts-dir` dirs, reading each
//! file's name table (via mmap, so huge `.ttc`s aren't fully read).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// A resolved font: the file and the face index within it (for `.ttc`).
pub(crate) type ResolvedFont = (PathBuf, u32);

/// Directories scanned for fonts: the system font dirs (macOS, Linux, Windows;
/// `dirs` has no helper for these), then the per-user font dir.
fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "/System/Library/Fonts",
        "/System/Library/Fonts/Supplemental",
        "/Library/Fonts",
        "/usr/share/fonts",
        "/usr/local/share/fonts",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    // Windows system fonts (e.g. C:\Windows\Fonts); `dirs::font_dir()` is None there.
    if let Some(win) = std::env::var_os("windir") {
        dirs.push(PathBuf::from(win).join("Fonts"));
    }
    // Per-user font dir (macOS ~/Library/Fonts; Linux $XDG_DATA_HOME/fonts).
    if let Some(f) = dirs::font_dir() {
        dirs.push(f);
    }
    // Legacy Linux per-user dir, which `font_dir()` doesn't report.
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".fonts"));
    }
    dirs
}

/// Built-in name → candidate host families for the fonts BOOX Notes uses (mostly
/// AOSP: Roboto + Noto). First candidate that exists on the host wins.
const DEFAULT_MAP: &[(&str, &[&str])] = &[
    (
        "noto sans cjk jp",
        &[
            "hiragino sans",
            "hiragino kaku gothic pron",
            "hiragino kaku gothic",
            "yu gothic",
            "noto sans cjk jp",
        ],
    ),
    (
        "noto sans jp",
        &["hiragino sans", "hiragino kaku gothic pron"],
    ),
    (
        "noto sans cjk kr",
        &["apple sd gothic neo", "noto sans cjk kr"],
    ),
    (
        "noto sans cjk sc",
        &["pingfang sc", "noto sans cjk sc", "heiti sc"],
    ),
    ("noto sans cjk tc", &["pingfang tc", "noto sans cjk tc"]),
    ("noto sans cjk hk", &["pingfang hk", "noto sans cjk hk"]),
    (
        "noto sans cjk",
        &["hiragino sans", "pingfang sc", "noto sans cjk jp"],
    ),
    (
        "noto serif cjk",
        &["hiragino mincho pron", "yu mincho", "noto serif cjk jp"],
    ),
    ("noto serif", &["times new roman", "georgia"]),
    (
        "noto sans mono",
        &["menlo", "monaco", "courier new", "noto sans mono"],
    ),
    ("roboto mono", &["menlo", "monaco", "courier new"]),
    (
        "roboto",
        &["roboto", "helvetica neue", "helvetica", "arial"],
    ),
    ("droid sans mono", &["menlo", "monaco", "courier new"]),
    (
        "droid sans",
        &["hiragino sans", "noto sans cjk jp", "droid sans"],
    ),
    ("noto sans", &["arial", "helvetica neue"]),
    ("sans", &["arial", "helvetica neue"]),
];

/// Families tried as the ultimate (CID/CJK-capable) fallback.
const FALLBACK_FAMILIES: &[&str] = &[
    "hiragino sans",
    "hiragino kaku gothic pron",
    "hiragino kaku gothic",
    "pingfang sc",
    "arial unicode ms",
    "noto sans cjk jp",
    "noto sans",
    "arial",
];

/// Color-emoji families tried, in order, for `emoji_font` (the host index, so a
/// `--fonts-dir` copy works too). No note ever names these — the BOOX device
/// falls back to its system emoji font at draw time, and we mirror that.
const EMOJI_FAMILIES: &[&str] = &["apple color emoji", "noto color emoji"];

pub(crate) struct FontDb {
    /// Lowercased family name → (path, face index). First seen wins.
    by_family: HashMap<String, ResolvedFont>,
    fallback: Option<ResolvedFont>,
    /// User mappings: lowercased requested family → lowercased target family.
    user_map: Vec<(String, String)>,
    /// `--font`: force this font for all text, bypassing resolution.
    explicit: Option<ResolvedFont>,
    /// Downloaded fonts: `(name pattern, file)` tried before the built-in mapping
    /// so a `--download-fonts` Noto wins over a system substitute.
    priority: Vec<(String, ResolvedFont)>,
    /// `--download-fonts`: the dynamic Google Fonts catalog, queried for the
    /// exact requested family before any pattern/substitute matching.
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
    /// Build the index by scanning system dirs + `extra_dirs`. `user_map` entries
    /// are `(requested, target)` family names; `explicit` forces one font;
    /// `priority` are `(name pattern, file)` and `google` the dynamic catalog,
    /// both from `--download-fonts`.
    pub(crate) fn build(
        extra_dirs: &[PathBuf],
        user_map: Vec<(String, String)>,
        explicit: Option<PathBuf>,
        priority: Vec<(String, PathBuf)>,
        google: Option<super::download::GoogleFonts>,
    ) -> Self {
        // Track each family's best face by how close it is to upright Regular, so
        // a family that spans weights/styles (Hiragino W0–W9, Times + Italic)
        // resolves to the Regular face, not a heavy or italic one.
        let mut scored: HashMap<String, (ResolvedFont, i32)> = HashMap::new();
        let mut dirs = extra_dirs.to_vec(); // user dirs take priority
        dirs.extend(system_font_dirs());
        for dir in dirs {
            index_dir(&dir, &mut scored);
        }
        let by_family: HashMap<String, ResolvedFont> =
            scored.into_iter().map(|(k, (f, _))| (k, f)).collect();
        let fallback = FALLBACK_FAMILIES.iter().find_map(|f| lookup(&by_family, f));
        let user_map = user_map
            .into_iter()
            .map(|(k, v)| (norm(&k), norm(&v)))
            .collect();
        Self {
            by_family,
            fallback,
            user_map,
            explicit: explicit.map(|p| (p, 0)),
            priority: priority.into_iter().map(|(k, p)| (k, (p, 0))).collect(),
            google,
            data_cache: RefCell::new(HashMap::new()),
            adv_cache: RefCell::new(HashMap::new()),
            emoji: RefCell::new(None),
        }
    }

    /// The color-emoji font, resolved like any other font request: the known
    /// emoji families ([`EMOJI_FAMILIES`]) from the host index first, then the
    /// `--download-fonts` mechanisms — the curated download cache (`ensure`
    /// fetches Noto Color Emoji eagerly), then the Google Fonts catalog as a
    /// retry. Memoized; `--font` is deliberately ignored (it forces *text*,
    /// emoji stay bitmaps).
    pub(crate) fn emoji_font(&self) -> Option<ResolvedFont> {
        if let Some(memo) = self.emoji.borrow().as_ref() {
            return memo.clone();
        }
        let result = EMOJI_FAMILIES
            .iter()
            .find_map(|f| lookup(&self.by_family, f))
            .or_else(|| {
                self.priority
                    .iter()
                    .find(|(pat, _)| pat.contains("emoji"))
                    .map(|(_, f)| f.clone())
            })
            .or_else(|| {
                let google = self.google.as_ref()?;
                google.get("noto color emoji").map(|p| (p, 0))
            });
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

    /// The font that will actually draw `c` for a box resolved to `font`: the
    /// font itself, or the global fallback when the font lacks the glyph and
    /// the fallback has it.
    pub(crate) fn font_for_char(&self, font: &ResolvedFont, c: char) -> ResolvedFont {
        if self.advance_em(font, c).is_some() {
            return font.clone();
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

    /// Resolve a font request (a family name and/or the BOOX device path hint) to a
    /// host font file. Returns the fallback (or `None`) if nothing matches.
    pub(crate) fn resolve(
        &self,
        request: Option<&str>,
        hint: Option<&Path>,
    ) -> Option<ResolvedFont> {
        if self.explicit.is_some() {
            return self.explicit.clone();
        }
        // If the hint is an existing file on this host, use it directly.
        if let Some(p) = hint
            && p.is_file()
        {
            return Some((p.to_path_buf(), 0));
        }
        let req = request.map(norm).unwrap_or_default();
        let stem = hint
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(norm)
            .unwrap_or_default();
        let keys = [req.as_str(), stem.as_str()];

        // 1. user mapping (requested family is contained in the key).
        for key in keys.iter().filter(|k| !k.is_empty()) {
            for (from, to) in &self.user_map {
                if key.contains(from.as_str())
                    && let Some(f) = lookup(&self.by_family, to)
                {
                    return Some(f);
                }
            }
        }
        // 1b. dynamic Google Fonts download (--download-fonts): the exact
        //     requested family from the catalog (style suffixes stripped), so a
        //     real "Coming Soon" beats any substitute. Exact-name lookup, so it
        //     never hijacks names the catalog doesn't have ("Noto Sans CJK JP"
        //     falls through to the curated list below).
        if let Some(google) = &self.google {
            for key in keys.iter().filter(|k| !k.is_empty()) {
                if let Some(p) = google.get(key) {
                    return Some((p, 0));
                }
            }
        }
        // 1c. curated downloads (--download-fonts) match by name pattern.
        for key in keys.iter().filter(|k| !k.is_empty()) {
            for (pat, font) in &self.priority {
                if key.contains(pat.as_str()) {
                    return Some(font.clone());
                }
            }
        }
        // 2. exact match on the requested family (avoids loose matches grabbing a
        //    same-prefix script font, e.g. "Noto Sans" → "Noto Sans Syloti Nagri").
        for key in keys.iter().filter(|k| !k.is_empty()) {
            if let Some(f) = self.by_family.get(*key) {
                return Some(f.clone());
            }
        }
        // 3. built-in mapping → concrete host families.
        for key in keys.iter().filter(|k| !k.is_empty()) {
            for (pat, targets) in DEFAULT_MAP {
                if key.contains(pat) {
                    for t in *targets {
                        if let Some(f) = lookup(&self.by_family, t) {
                            return Some(f);
                        }
                    }
                }
            }
        }
        // 4. loose (substring) match as a last resort.
        for key in keys.iter().filter(|k| !k.is_empty()) {
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
pub(crate) fn char_draw(db: &FontDb, box_font: &ResolvedFont, c: char) -> CharDraw {
    if super::emoji::is_emoji_modifier(c) {
        CharDraw::Skip
    } else if super::emoji::is_emoji(c) && super::emoji::png_for(db, c).is_some() {
        CharDraw::Emoji
    } else {
        CharDraw::Glyph(db.font_for_char(box_font, c))
    }
}

/// Collect the characters each font will draw across `canvases` (via
/// [`char_draw`], the same per-char choice the text painter makes) — for
/// backends that embed subset fonts. Pass only the canvases actually being
/// emitted, so subsets don't carry glyphs from unselected pages.
pub(crate) fn used_chars<'a>(
    db: &FontDb,
    canvases: impl IntoIterator<Item = &'a crate::model::Canvas>,
) -> HashMap<ResolvedFont, BTreeSet<char>> {
    let mut used: HashMap<ResolvedFont, BTreeSet<char>> = HashMap::new();
    for canvas in canvases {
        for item in &canvas.items {
            let crate::model::RenderItem::Text(t) = item else {
                continue;
            };
            let Some(font) = db.resolve(t.font_name.as_deref(), t.font_hint.as_deref()) else {
                continue;
            };
            for c in t.runs.iter().flat_map(|r| r.text.chars()) {
                if let CharDraw::Glyph(f) = char_draw(db, &font, c) {
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

/// Index every font file in `dir` (non-recursive), keeping the most Regular face
/// per family.
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
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        // mmap so a 100+MB .ttc isn't fully read just to get its name.
        let Ok(mmap) = (unsafe { memmap2::Mmap::map(&file) }) else {
            continue;
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
                        scored.insert(key, ((path.clone(), index), score));
                    }
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

    /// Build a `FontDb` without touching the filesystem.
    fn db(
        families: &[(&str, &str)],
        user_map: &[(&str, &str)],
        explicit: Option<&str>,
        priority: &[(&str, &str)],
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
            priority: priority
                .iter()
                .map(|(k, p)| (k.to_string(), (PathBuf::from(p), 0)))
                .collect(),
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
            &[("noto sans", "/sys.ttf")],
            &[("noto sans", "arial")],
            Some("/forced.ttf"),
            &[("noto sans", "/dl.ttf")],
            None,
        );
        assert_eq!(
            resolved(&d, "Noto Sans"),
            Some(PathBuf::from("/forced.ttf"))
        );
    }

    #[test]
    fn user_map_beats_download_and_default() {
        let d = db(
            &[("hiragino sans", "/hira.ttc")],
            &[("noto sans cjk jp", "hiragino sans")],
            None,
            &[("noto sans cjk jp", "/dl-notosansjp.ttf")],
            None,
        );
        // --map-font wins over --download-fonts.
        assert_eq!(
            resolved(&d, "Noto Sans CJK JP"),
            Some(PathBuf::from("/hira.ttc"))
        );
    }

    #[test]
    fn download_beats_exact_and_default_map() {
        // Hiragino exists on the host AND a Noto was downloaded: the download wins.
        let d = db(
            &[("hiragino sans", "/hira.ttc")],
            &[],
            None,
            &[("noto sans cjk jp", "/dl-notosansjp.ttf")],
            None,
        );
        assert_eq!(
            resolved(&d, "Noto Sans CJK JP"),
            Some(PathBuf::from("/dl-notosansjp.ttf"))
        );
    }

    #[test]
    fn emoji_font_prefers_local_families_then_downloads() {
        // Apple Color Emoji beats Noto Color Emoji when both are local.
        let d = db(
            &[
                ("noto color emoji", "/noto-emoji.ttf"),
                ("apple color emoji", "/apple-emoji.ttc"),
            ],
            &[],
            None,
            &[],
            None,
        );
        assert_eq!(d.emoji_font(), Some((PathBuf::from("/apple-emoji.ttc"), 0)));
        // Local Noto when Apple is absent.
        let d = db(
            &[("noto color emoji", "/noto-emoji.ttf")],
            &[],
            None,
            &[],
            None,
        );
        assert_eq!(d.emoji_font(), Some((PathBuf::from("/noto-emoji.ttf"), 0)));
        // Nothing local → the downloaded (curated) emoji font.
        let d = db(
            &[("arial", "/arial.ttf")],
            &[],
            None,
            &[("noto color emoji", "/dl-emoji.ttf")],
            None,
        );
        assert_eq!(d.emoji_font(), Some((PathBuf::from("/dl-emoji.ttf"), 0)));
        // Nothing anywhere → None. `--font` must not hijack emoji.
        let d = db(&[], &[], Some("/forced.ttf"), &[], None);
        assert_eq!(d.emoji_font(), None);
    }

    #[test]
    fn exact_match_beats_default_map() {
        // No download; the requested family is present verbatim → used directly,
        // not redirected through DEFAULT_MAP.
        let d = db(
            &[("noto serif", "/host-notoserif.ttf")],
            &[],
            None,
            &[],
            None,
        );
        assert_eq!(
            resolved(&d, "Noto Serif"),
            Some(PathBuf::from("/host-notoserif.ttf"))
        );
    }

    #[test]
    fn default_map_redirects_when_no_exact() {
        // "Noto Serif" not on host → DEFAULT_MAP → Times New Roman.
        let d = db(&[("times new roman", "/times.ttf")], &[], None, &[], None);
        assert_eq!(
            resolved(&d, "Noto Serif"),
            Some(PathBuf::from("/times.ttf"))
        );
    }

    #[test]
    fn falls_back_when_nothing_matches() {
        let d = db(&[], &[], None, &[], Some("/fallback.ttc"));
        assert_eq!(
            resolved(&d, "Totally Unknown Family"),
            Some(PathBuf::from("/fallback.ttc"))
        );
    }
}
