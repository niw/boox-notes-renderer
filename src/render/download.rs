//! Opt-in font download (`--download-fonts`), so the note renders in its
//! actual fonts instead of a system substitute. Two mechanisms:
//!
//! 1. A small curated list ([`ensure`], fetched eagerly): the Noto fonts behind
//!    the BOOX-specific family names that don't match Google's catalog naming
//!    ("Noto Sans CJK JP" is "Noto Sans JP" on Google Fonts), plus the color
//!    emoji font `emoji.rs` needs without any family requesting it.
//! 2. The full Google Fonts catalog ([`GoogleFonts`], resolved lazily per
//!    requested family): the family list from `fonts.google.com/metadata/fonts`
//!    (cached), then the family's files via `download/list` — so any catalog
//!    font a note asks for ("Coming Soon", …) downloads on demand.
//!
//! Variable fonts are used at their default (Regular) instance.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read as _;
use std::path::PathBuf;

const BASE: &str = "https://raw.githubusercontent.com/google/fonts/main/ofl";

/// (requested-name patterns, cache filename, URL path under `BASE`).
const DOWNLOADS: &[(&[&str], &str, &str)] = &[
    (
        &["noto sans cjk jp", "noto sans jp"],
        "NotoSansJP.ttf",
        "notosansjp/NotoSansJP%5Bwght%5D.ttf",
    ),
    (
        &["noto serif cjk jp", "noto serif jp"],
        "NotoSerifJP.ttf",
        "notoserifjp/NotoSerifJP%5Bwght%5D.ttf",
    ),
    (
        &["noto sans mono", "roboto mono", "droid sans mono"],
        "NotoSansMono.ttf",
        "notosansmono/NotoSansMono%5Bwdth%2Cwght%5D.ttf",
    ),
    (
        &["noto serif"],
        "NotoSerif.ttf",
        "notoserif/NotoSerif%5Bwdth%2Cwght%5D.ttf",
    ),
    (
        &["noto sans", "roboto", "droid sans"],
        "NotoSans.ttf",
        "notosans/NotoSans%5Bwdth%2Cwght%5D.ttf",
    ),
    (
        &["noto color emoji"],
        "NotoColorEmoji.ttf",
        "notocoloremoji/NotoColorEmoji-Regular.ttf",
    ),
];

/// `<cache>/boox-notes-renderer/fonts`, where downloaded fonts live. The base is
/// the platform cache dir (macOS `~/Library/Caches`, Linux `$XDG_CACHE_HOME` or
/// `~/.cache`, Windows `%LOCALAPPDATA%`).
pub(crate) fn cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("boox-notes-renderer/fonts"))
}

/// Ensure the curated Noto fonts are cached (downloading any missing), and return
/// `(requested-name pattern → cached file)` mappings for resolution.
pub(crate) fn ensure() -> Vec<(String, PathBuf)> {
    let Some(dir) = cache_dir() else {
        return Vec::new();
    };
    let _ = std::fs::create_dir_all(&dir);
    let mut out = Vec::new();
    for (patterns, filename, url_path) in DOWNLOADS {
        let path = dir.join(filename);
        if !path.exists() {
            let url = format!("{BASE}/{url_path}");
            match fetch(&url) {
                Ok(bytes) if bytes.len() > 1024 => {
                    if std::fs::write(&path, &bytes).is_err() {
                        continue;
                    }
                    log::info!("downloaded {filename} ({} bytes)", bytes.len());
                }
                Ok(_) => {
                    log::warn!("{filename} download too small; skipped");
                    continue;
                }
                Err(e) => {
                    log::warn!("could not download {filename}: {e}");
                    continue;
                }
            }
        }
        for pat in *patterns {
            out.push((pat.to_string(), path.clone()));
        }
    }
    out
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).call().map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    resp.into_body()
        .into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

const METADATA_URL: &str = "https://fonts.google.com/metadata/fonts";
const DOWNLOAD_LIST_URL: &str = "https://fonts.google.com/download/list?family=";

/// Style/weight tokens stripped from the end of a requested name to find the
/// catalog family ("coming soon regular" → "coming soon").
const STYLE_WORDS: &[&str] = &[
    "regular",
    "italic",
    "oblique",
    "bold",
    "semibold",
    "demibold",
    "extrabold",
    "ultrabold",
    "light",
    "extralight",
    "ultralight",
    "thin",
    "medium",
    "black",
    "heavy",
    "book",
    "condensed",
    "expanded",
    "narrow",
];

/// Dynamic Google Fonts lookup: the full catalog of family names (fetched once,
/// cached on disk), with per-family font files downloaded on demand.
pub(crate) struct GoogleFonts {
    /// Normalized family name → canonical name ("coming soon" → "Coming Soon").
    families: HashMap<String, String>,
    /// Per-run memo: normalized request → cached file (`None` = known miss).
    memo: RefCell<HashMap<String, Option<PathBuf>>>,
}

impl GoogleFonts {
    /// Load the catalog (from the disk cache, fetching it on first use). An
    /// empty catalog (offline, no cache) makes every lookup miss gracefully.
    pub(crate) fn load() -> Self {
        Self {
            families: catalog().unwrap_or_default(),
            memo: RefCell::new(HashMap::new()),
        }
    }

    /// Resolve a normalized family request to a downloaded font file: the name
    /// as-is, then with trailing style tokens stripped. `None` if the family is
    /// not on Google Fonts (or the download failed).
    pub(crate) fn get(&self, name: &str) -> Option<PathBuf> {
        if let Some(hit) = self.memo.borrow().get(name) {
            return hit.clone();
        }
        let result = candidates(name)
            .into_iter()
            .find_map(|c| fetch_family(self.families.get(&c)?));
        self.memo
            .borrow_mut()
            .insert(name.to_string(), result.clone());
        result
    }
}

/// The requested name plus progressively style-stripped variants, e.g.
/// "coming soon regular" → ["coming soon regular", "coming soon"].
fn candidates(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    let mut words: Vec<&str> = name.split(' ').collect();
    while words.len() > 1 {
        let last = *words.last().expect("non-empty");
        if !STYLE_WORDS.contains(&last) && !last.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        words.pop();
        out.push(words.join(" "));
    }
    out
}

/// The normalized → canonical family-name catalog, cached as a slim JSON map in
/// the font cache dir (delete it to pick up newly published families).
fn catalog() -> Option<HashMap<String, String>> {
    let dir = cache_dir()?;
    let path = dir.join("gf-catalog.json");
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(map) = serde_json::from_slice(&bytes)
    {
        return Some(map);
    }
    let bytes = match fetch(METADATA_URL) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("could not fetch the Google Fonts catalog: {e}");
            return None;
        }
    };
    let v: serde_json::Value = serde_json::from_slice(strip_xssi(&bytes)).ok()?;
    let map: HashMap<String, String> = v
        .get("familyMetadataList")?
        .as_array()?
        .iter()
        .filter_map(|f| f.get("family")?.as_str().map(String::from))
        .map(|fam| (super::fonts::norm(&fam), fam))
        .collect();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_vec(&map) {
        let _ = std::fs::write(&path, json);
    }
    log::info!(
        "downloaded the Google Fonts catalog ({} families)",
        map.len()
    );
    Some(map)
}

/// Download (or reuse from the cache) the best font file of a canonical
/// family: its `download/list` manifest names every file; prefer the static
/// Regular, else the first TTF/OTF (a variable font's default instance).
fn fetch_family(family: &str) -> Option<PathBuf> {
    let dir = cache_dir()?;
    let slug: String = family
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    for ext in ["ttf", "otf"] {
        let path = dir.join(format!("gf-{slug}.{ext}"));
        if path.exists() {
            return Some(path);
        }
    }
    let url = format!("{DOWNLOAD_LIST_URL}{}", family.replace(' ', "%20"));
    let body = match fetch(&url) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("could not list Google Fonts family {family}: {e}");
            return None;
        }
    };
    let v: serde_json::Value = serde_json::from_slice(strip_xssi(&body)).ok()?;
    let refs: Vec<(String, String)> = v
        .get("manifest")?
        .get("fileRefs")?
        .as_array()?
        .iter()
        .filter_map(|r| {
            Some((
                r.get("filename")?.as_str()?.to_string(),
                r.get("url")?.as_str()?.to_string(),
            ))
        })
        .collect();
    let (filename, file_url) = pick_font_ref(&refs)?;
    let bytes = match fetch(file_url) {
        Ok(b) if b.len() > 1024 => b,
        Ok(_) => {
            log::warn!("{family} download too small; skipped");
            return None;
        }
        Err(e) => {
            log::warn!("could not download {family}: {e}");
            return None;
        }
    };
    let ext = if filename.to_lowercase().ends_with(".otf") {
        "otf"
    } else {
        "ttf"
    };
    let path = dir.join(format!("gf-{slug}.{ext}"));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(&path, &bytes).ok()?;
    log::info!("downloaded {family} ({} bytes)", bytes.len());
    Some(path)
}

/// The font file to use from a family's manifest: the static Regular if there
/// is one, else the first TTF/OTF.
fn pick_font_ref(refs: &[(String, String)]) -> Option<&(String, String)> {
    let is_font =
        |f: &str| f.to_lowercase().ends_with(".ttf") || f.to_lowercase().ends_with(".otf");
    refs.iter()
        .find(|(f, _)| is_font(f) && f.to_lowercase().contains("-regular."))
        .or_else(|| refs.iter().find(|(f, _)| is_font(f)))
}

/// Strip Google's `)]}'` XSSI prefix line, if present.
fn strip_xssi(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(b")]}'") {
        match bytes.iter().position(|&b| b == b'\n') {
            Some(i) => &bytes[i + 1..],
            None => &[],
        }
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_strip_trailing_style_words() {
        assert_eq!(
            candidates("coming soon regular"),
            vec!["coming soon regular", "coming soon"]
        );
        assert_eq!(
            candidates("noto sans jp bold italic"),
            vec![
                "noto sans jp bold italic",
                "noto sans jp bold",
                "noto sans jp"
            ]
        );
        // Non-style last word stops the stripping.
        assert_eq!(candidates("noto sans cjk jp"), vec!["noto sans cjk jp"]);
        // Numeric weights strip too, but never below one word.
        assert_eq!(candidates("roboto 400"), vec!["roboto 400", "roboto"]);
        assert_eq!(candidates("regular"), vec!["regular"]);
    }

    #[test]
    fn pick_font_ref_prefers_static_regular() {
        let refs = vec![
            ("LICENSE.txt".to_string(), "u0".to_string()),
            ("NotoSansJP[wght].ttf".to_string(), "u1".to_string()),
            ("static/NotoSansJP-Bold.ttf".to_string(), "u2".to_string()),
            (
                "static/NotoSansJP-Regular.ttf".to_string(),
                "u3".to_string(),
            ),
        ];
        assert_eq!(pick_font_ref(&refs).unwrap().1, "u3");
        // No Regular → first font file (the variable font).
        let refs = vec![
            ("LICENSE.txt".to_string(), "u0".to_string()),
            ("ComingSoon[wght].ttf".to_string(), "u1".to_string()),
        ];
        assert_eq!(pick_font_ref(&refs).unwrap().1, "u1");
        let none = vec![("LICENSE.txt".to_string(), "u0".to_string())];
        assert!(pick_font_ref(&none).is_none());
    }

    #[test]
    fn strip_xssi_removes_prefix_line() {
        assert_eq!(strip_xssi(b")]}'\n{\"a\":1}"), b"{\"a\":1}");
        assert_eq!(strip_xssi(b"{\"a\":1}"), b"{\"a\":1}");
    }
}
