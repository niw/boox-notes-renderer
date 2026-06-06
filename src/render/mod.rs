//! Render a reassembled [`crate::model::Document`] to an output format via a
//! pluggable `Backend` (the crate-internal drawing trait).
//!
//! `model` produces a `Document` of `Canvas`es whose items are in a single
//! global coordinate space with resolved styles. The per-item painters
//! (`stroke`/`shape`/`text`) compute geometry in that global space and emit it
//! through the `Backend` trait; each backend (PDF/SVG/PNG) applies its own
//! coordinate transform and serialization. Adding a format = adding a backend.

mod charcoal;
mod download;
mod emoji;
pub(crate) mod fonts;
mod pdf;
mod png;
mod shape;
mod stroke;
mod svg;
mod text;

pub use pdf::render_pdf;
pub use png::render_png;
pub use svg::render_svg;

use crate::error::{Error, Result};
use crate::model::{Canvas, RenderImage, RenderItem, Rgba};

pub(crate) const WHITE: Rgba = Rgba {
    r: 255,
    g: 255,
    b: 255,
    a: 1.0,
};

/// Font selection options shared by every backend.
///
/// Non-exhaustive (options grow): build one with [`Default`] and the chainable
/// `with_*` setters. The fields stay `pub` for reading.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct FontOptions {
    /// `--font`: force this font file for all text.
    pub explicit: Option<std::path::PathBuf>,
    /// `--fonts-path`: extra directories to search for fonts.
    pub dirs: Vec<std::path::PathBuf>,
    /// `--map-font`: `(requested family, target family)` overrides.
    pub map: Vec<(String, String)>,
    /// `--download-fonts`: fetch Noto fonts for the families the note uses.
    pub download: bool,
}

impl FontOptions {
    /// Force this font file (TTF/OTF/TTC) for all text, bypassing name
    /// resolution (`--font`).
    pub fn with_explicit(mut self, font: Option<std::path::PathBuf>) -> Self {
        self.explicit = font;
        self
    }

    /// Extra directories to search for fonts (`--fonts-path`).
    pub fn with_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.dirs = dirs;
        self
    }

    /// `(requested family, target family)` overrides (`--map-font`).
    pub fn with_map(mut self, map: Vec<(String, String)>) -> Self {
        self.map = map;
        self
    }

    /// Download the note's fonts from Google Fonts into a cache and use them
    /// (`--download-fonts`).
    pub fn with_download(mut self, download: bool) -> Self {
        self.download = download;
        self
    }
    pub(crate) fn build_db(&self) -> fonts::FontDb {
        let (priority, google) = if self.download {
            (download::ensure(), Some(download::GoogleFonts::load()))
        } else {
            (Vec::new(), None)
        };
        fonts::FontDb::build(
            &self.dirs,
            self.map.clone(),
            self.explicit.clone(),
            priority,
            google,
        )
    }
}

/// Which note-pages to emit for single-image formats (SVG/PNG).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum PageSel {
    /// Every page (one output file each when there is more than one).
    All,
    /// A single 1-based page index.
    One(usize),
}

/// How a translucency group composites against its backdrop.
///
/// `Multiply` exists for the marker/highlighter (pen 15), matching BOOX's
/// PDF export. Over white it is identical to `Normal`; over colored content (a
/// highlighter crossing another stroke) it darkens multiplicatively like a real
/// marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GroupBlend {
    Normal,
    Multiply,
}

/// Drawing primitives in **global/canvas coordinates**. Each backend applies its
/// own transform (PDF flips Y to bottom-left; SVG/PNG keep note-space top-left,
/// y-down) and serializes however it needs.
pub(crate) trait Backend {
    /// Paint the page background (the full canvas rect) in `color`.
    fn fill_background(&mut self, canvas: &Canvas, color: Rgba);

    /// Fill one or more rings (nonzero winding) with a solid color.
    fn fill_path(&mut self, canvas: &Canvas, rings: &[Vec<(f32, f32)>], color: Rgba);

    /// Stroke a polyline with round caps and joins. `dash` = on/off lengths (pt)
    /// or `None` for solid.
    fn stroke_polyline(
        &mut self,
        canvas: &Canvas,
        pts: &[(f32, f32)],
        width: f32,
        color: Rgba,
        dash: Option<&[f32]>,
    );

    /// Place a raster image into its note-space rect. The default draws the
    /// encoded bytes inline; `key` = (canvas, item) index for backends that
    /// pre-register decoded images instead (PDF).
    fn draw_image(&mut self, canvas: &Canvas, img: &RenderImage, _key: (usize, usize)) {
        self.draw_inline_image(canvas, &img.bytes, img.left, img.top, img.right, img.bottom);
    }

    /// Place a raster image given inline as encoded bytes (PNG/JPEG) into a
    /// note-space rect — used for color-emoji glyph bitmaps.
    fn draw_inline_image(
        &mut self,
        canvas: &Canvas,
        bytes: &[u8],
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    );

    /// Draw one already-laid-out text line: `x` is the resolved left edge,
    /// `baseline_y` the baseline (both note-space), alignment already applied.
    /// `bold`/`italic` are faux-styled by each backend (underline is drawn by the
    /// shared text painter as a separate stroke). `font` is the resolved host font
    /// (path + face index) for this run.
    #[allow(clippy::too_many_arguments)]
    fn draw_text_line(
        &mut self,
        canvas: &Canvas,
        x: f32,
        baseline_y: f32,
        text: &str,
        size: f32,
        color: Rgba,
        bold: bool,
        italic: bool,
        font: &fonts::ResolvedFont,
    );

    /// Begin a translucency group: children draw opaque and the group composites
    /// once at `alpha` with `blend` against the backdrop. Prevents overlapping
    /// round-capped segments of one translucent stroke from double-darkening.
    ///
    /// Returns whether a group was actually opened (`false` for `alpha >= 1.0`,
    /// or when the backend cannot open one); the caller must call
    /// [`end_group`](Backend::end_group) exactly when it returns `true`, and
    /// must keep per-primitive alpha when it returns `false`.
    #[must_use]
    fn begin_group(&mut self, alpha: f32, blend: GroupBlend) -> bool;
    /// End the most recent [`begin_group`](Backend::begin_group) that returned
    /// `true`.
    fn end_group(&mut self);
}

/// Paint one canvas's background then its items, in order, into `backend`.
pub(crate) fn paint_canvas(
    backend: &mut impl Backend,
    canvas: &Canvas,
    ci: usize,
    fonts: &fonts::FontDb,
) {
    backend.fill_background(canvas, WHITE);
    for (ii, item) in canvas.items.iter().enumerate() {
        match item {
            RenderItem::Stroke(s) => stroke::draw(backend, canvas, s),
            RenderItem::Geometry(g) => shape::draw(backend, canvas, g),
            RenderItem::Text(t) => text::draw(backend, canvas, t, fonts),
            // Image placement is just the backend call — the geometry (target
            // rect) is already on `RenderImage`; `key` lets a backend look up a
            // pre-registered decoded image.
            RenderItem::Image(img) => backend.draw_image(canvas, img, (ci, ii)),
        }
    }
}

/// Resolve a [`PageSel`] to `(filename suffix, canvas index)` pairs. A single
/// page (selected or because the doc has one canvas) gets an empty suffix; a
/// multi-page `All` gets `-1`, `-2`, … so callers can name `stem-N.ext`.
pub(crate) fn selected_pages(n_canvases: usize, sel: PageSel) -> Result<Vec<(String, usize)>> {
    if n_canvases == 0 {
        return Err(Error::NoPages);
    }
    match sel {
        // Page 0 (indices are 1-based) and beyond-the-end both report the
        // valid 1..=n range.
        PageSel::One(p) if p == 0 || p > n_canvases => Err(Error::PageOutOfRange {
            page: p,
            pages: n_canvases,
        }),
        PageSel::One(p) => Ok(vec![(String::new(), p - 1)]),
        PageSel::All if n_canvases == 1 => Ok(vec![(String::new(), 0)]),
        PageSel::All => Ok((0..n_canvases)
            .map(|i| (format!("-{}", i + 1), i))
            .collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_pages_rejects_invalid_selection() {
        assert!(matches!(
            selected_pages(0, PageSel::All),
            Err(Error::NoPages)
        ));
        assert!(matches!(
            selected_pages(2, PageSel::One(0)),
            Err(Error::PageOutOfRange { page: 0, pages: 2 })
        ));
        assert!(matches!(
            selected_pages(2, PageSel::One(3)),
            Err(Error::PageOutOfRange { page: 3, pages: 2 })
        ));
    }

    #[test]
    fn selected_pages_keeps_existing_suffix_rules() {
        assert_eq!(
            selected_pages(1, PageSel::All).unwrap(),
            vec![(String::new(), 0)]
        );
        assert_eq!(
            selected_pages(3, PageSel::One(2)).unwrap(),
            vec![(String::new(), 1)]
        );
        assert_eq!(
            selected_pages(3, PageSel::All).unwrap(),
            vec![
                ("-1".to_string(), 0),
                ("-2".to_string(), 1),
                ("-3".to_string(), 2)
            ]
        );
    }
}
