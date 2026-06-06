//! PDF backend (`printpdf`). One PDF page per canvas, sized to its content box.
//! Coordinates are flipped from note space (top-left, y-down) to PDF space
//! (bottom-left, y-up). Outline/fill state is coalesced so the per-segment
//! pressure strokes don't re-emit color/cap/join every segment.

use printpdf::{
    BlendMode, Color, ExtendedGraphicsState, ExtendedGraphicsStateId, FontId, Line, LineCapStyle,
    LineDashPattern, LineJoinStyle, LinePoint, Mm, Op, PaintMode, ParsedFont, PdfDocument,
    PdfFontHandle, PdfPage, PdfSaveOptions, Point, Polygon, PolygonRing, Pt, RawImage, Rgb,
    SeperableBlendMode, TextItem, TextMatrix, TextRenderingMode, WindingOrder, XObjectId,
    XObjectTransform,
};

use super::fonts::{self, ResolvedFont};
use super::{Backend, FontOptions, GroupBlend, PageSel, paint_canvas, selected_pages};
use crate::model::{Canvas, Document, RenderImage, Rgba};
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};

/// Render the selected note pages to a single (multi-page) PDF. `PageSel::All`
/// emits every page; `PageSel::One(p)` emits just that 1-based page.
pub fn render_pdf(
    doc: &Document,
    font_opts: &FontOptions,
    sel: PageSel,
) -> crate::error::Result<Vec<u8>> {
    // Validate the page selection before any font work (build_db may download).
    let selected = selected_pages(doc.canvases.len(), sel)?;
    let db = font_opts.build_db();
    // Characters each font draws, so `font_id` embeds glyph subsets — scoped
    // to the selected pages so a --page export doesn't embed glyphs (or, in
    // `PdfBackend::new`, images) the emitted pages never use.
    let subsets = fonts::used_chars(&db, selected.iter().map(|(_, ci)| &doc.canvases[*ci]));
    let mut be = PdfBackend::new(doc, &selected, subsets);
    let mut pages = Vec::new();
    // `ci` stays the original canvas index so the pre-registered image keys
    // (ci, ii) still resolve when only some pages are emitted.
    for (_suffix, ci) in &selected {
        let canvas = &doc.canvases[*ci];
        be.begin_page();
        paint_canvas(&mut be, canvas, *ci, &db);
        pages.push(be.end_page(canvas));
    }
    Ok(be.finish(pages))
}

struct ImageRef {
    id: XObjectId,
    w: usize,
    h: usize,
}

struct PdfBackend {
    doc: PdfDocument,
    alpha_states: HashMap<(u32, GroupBlend), ExtendedGraphicsStateId>,
    images: HashMap<(usize, usize), ImageRef>,
    /// Inline images (emoji bitmaps) registered on demand, keyed by byte hash.
    inline_imgs: HashMap<u64, ImageRef>,
    /// Fonts embedded on demand, keyed by resolved (path, face index).
    fonts: HashMap<ResolvedFont, FontId>,
    /// The characters each font draws (whole document), for subset embedding.
    subsets: HashMap<ResolvedFont, BTreeSet<char>>,
    ops: Vec<Op>,
    // Coalescing caches (reset per page / after state-changing ops).
    cur_outline: Option<Rgba>,
    cur_thickness: Option<f32>,
    caps_set: bool,
    /// Dash state: `None` = unknown (force re-emit, e.g. after a graphics-state
    /// restore), `Some(None)` = solid, `Some(Some(d))` = dashed with pattern `d`.
    /// NOTE: "unknown" must stay distinct from "solid" — after `end_group`'s
    /// `RestoreGraphicsState` the PDF dash reverts to whatever it was at the
    /// matching save, so a plain `None`-means-solid cache would skip
    /// re-asserting solid and leak the dash onto later strokes.
    cur_dash: Option<Option<Vec<f32>>>,
    cur_fill: Option<Rgba>,
}

fn alpha_key(a: f32) -> u32 {
    (a.clamp(0.0, 1.0) * 1000.0).round() as u32
}

impl PdfBackend {
    fn new(
        doc_model: &Document,
        selected: &[(String, usize)],
        subsets: HashMap<ResolvedFont, BTreeSet<char>>,
    ) -> Self {
        let mut doc = PdfDocument::new("boox-notes-renderer");

        // Pre-decode and register the selected pages' embedded images, keyed
        // by the original (canvas, item) index so the paint lookups resolve.
        let mut images = HashMap::new();
        for (_, ci) in selected {
            let canvas = &doc_model.canvases[*ci];
            for (ii, item) in canvas.items.iter().enumerate() {
                if let crate::model::RenderItem::Image(img) = item {
                    let mut warnings = Vec::new();
                    match RawImage::decode_from_bytes(&img.bytes, &mut warnings) {
                        Ok(raw) => {
                            let (w, h) = (raw.width, raw.height);
                            let id = doc.add_image(&raw);
                            images.insert((*ci, ii), ImageRef { id, w, h });
                        }
                        Err(e) => log::warn!("could not decode embedded image: {e}"),
                    }
                }
            }
        }

        Self {
            doc,
            alpha_states: HashMap::new(),
            images,
            inline_imgs: HashMap::new(),
            fonts: HashMap::new(),
            subsets,
            ops: Vec::new(),
            cur_outline: None,
            cur_thickness: None,
            caps_set: false,
            cur_dash: None,
            cur_fill: None,
        }
    }

    /// The graphics state for a translucent alpha (+ blend mode), registered on
    /// first use.
    fn alpha_gs(&mut self, a: f32, blend: GroupBlend) -> ExtendedGraphicsStateId {
        let key = (alpha_key(a), blend);
        if let Some(gs) = self.alpha_states.get(&key) {
            return gs.clone();
        }
        let mut state = ExtendedGraphicsState::default()
            .with_current_fill_alpha(a)
            .with_current_stroke_alpha(a);
        if blend == GroupBlend::Multiply {
            // The marker's `BM /Multiply`.
            state = state.with_blend_mode(BlendMode::Seperable(SeperableBlendMode::Multiply));
        }
        let gs = self.doc.add_graphics_state(state);
        self.alpha_states.insert(key, gs.clone());
        gs
    }

    /// Embed (once) and return the FontId for a resolved font. The font is
    /// subset to the glyphs the document actually draws — printpdf 0.9's own
    /// subsetting is hard-disabled (`if false` in its serialize.rs), so a full
    /// CJK font would otherwise dominate the file size. Falls back to the full
    /// font if subsetting fails.
    fn font_id(&mut self, font: &ResolvedFont) -> Option<FontId> {
        if let Some(id) = self.fonts.get(font) {
            return Some(id.clone());
        }
        let mut warnings = Vec::new();
        let parsed = self
            .subsets
            .get(font)
            .and_then(|chars| fonts::subset_font_bytes(font, chars))
            .and_then(|sub| ParsedFont::from_bytes(&sub, 0, &mut warnings))
            .or_else(|| {
                let bytes = std::fs::read(&font.0).ok()?;
                ParsedFont::from_bytes(&bytes, font.1 as usize, &mut warnings)
            })?;
        let id = self.doc.add_font(&parsed);
        self.fonts.insert(font.clone(), id.clone());
        Some(id)
    }

    fn begin_page(&mut self) {
        self.ops = Vec::new();
        self.reset_caches();
    }

    fn reset_caches(&mut self) {
        self.cur_outline = None;
        self.cur_thickness = None;
        self.caps_set = false;
        self.cur_dash = None;
        self.cur_fill = None;
    }

    fn end_page(&mut self, canvas: &Canvas) -> PdfPage {
        let ops = std::mem::take(&mut self.ops);
        PdfPage::new(pt_to_mm(canvas.width), pt_to_mm(canvas.height), ops)
    }

    fn finish(mut self, pages: Vec<PdfPage>) -> Vec<u8> {
        self.doc.with_pages(pages);
        let mut warnings = Vec::new();
        self.doc.save(&PdfSaveOptions::default(), &mut warnings)
    }

    fn ensure_round_caps(&mut self) {
        if !self.caps_set {
            self.ops.push(Op::SetLineCapStyle {
                cap: LineCapStyle::Round,
            });
            self.ops.push(Op::SetLineJoinStyle {
                join: LineJoinStyle::Round,
            });
            self.caps_set = true;
        }
    }

    fn set_dash(&mut self, dash: Option<&[f32]>) {
        let next: Option<Vec<f32>> = dash.map(|d| d.to_vec());
        // Re-emit when unknown (cur_dash == None) or when the pattern changes.
        if self.cur_dash.as_ref() != Some(&next) {
            let pat = match &next {
                Some(d) => {
                    let arr: Vec<i64> = d.iter().map(|x| x.round().max(1.0) as i64).collect();
                    LineDashPattern::from_array(&arr, 0)
                }
                None => LineDashPattern::default(),
            };
            self.ops.push(Op::SetLineDashPattern { dash: pat });
            self.cur_dash = Some(next);
        }
    }
}

impl Backend for PdfBackend {
    fn fill_background(&mut self, canvas: &Canvas, color: Rgba) {
        self.ops.push(Op::SetFillColor {
            col: rgba_to_color(color),
        });
        self.cur_fill = Some(color);
        self.ops.push(Op::DrawPolygon {
            polygon: poly(
                vec![vec![
                    Point {
                        x: Pt(0.0),
                        y: Pt(0.0),
                    },
                    Point {
                        x: Pt(canvas.width),
                        y: Pt(0.0),
                    },
                    Point {
                        x: Pt(canvas.width),
                        y: Pt(canvas.height),
                    },
                    Point {
                        x: Pt(0.0),
                        y: Pt(canvas.height),
                    },
                ]],
                PaintMode::Fill,
            ),
        });
    }

    fn fill_path(&mut self, canvas: &Canvas, rings: &[Vec<(f32, f32)>], color: Rgba) {
        if self.cur_fill != Some(color) {
            self.ops.push(Op::SetFillColor {
                col: rgba_to_color(color),
            });
            self.cur_fill = Some(color);
        }
        // A translucent fill (e.g. a shape's fillAttr alpha) needs an alpha
        // graphics state; PDF fill color itself has no alpha. The color ops
        // above sit outside the q..Q, so the coalescing caches stay valid.
        let translucent = color.a < 1.0;
        if translucent {
            self.ops.push(Op::SaveGraphicsState);
            let gs = self.alpha_gs(color.a, GroupBlend::Normal);
            self.ops.push(Op::LoadGraphicsState { gs });
        }
        let rings: Vec<Vec<Point>> = rings
            .iter()
            .map(|r| r.iter().map(|&(x, y)| to_page(canvas, x, y)).collect())
            .collect();
        self.ops.push(Op::DrawPolygon {
            polygon: poly(rings, PaintMode::Fill),
        });
        if translucent {
            self.ops.push(Op::RestoreGraphicsState);
        }
    }

    fn stroke_polyline(
        &mut self,
        canvas: &Canvas,
        pts: &[(f32, f32)],
        width: f32,
        color: Rgba,
        dash: Option<&[f32]>,
    ) {
        if pts.len() < 2 {
            return;
        }
        if self.cur_outline != Some(color) {
            self.ops.push(Op::SetOutlineColor {
                col: rgba_to_color(color),
            });
            self.cur_outline = Some(color);
        }
        if self.cur_thickness != Some(width) {
            self.ops.push(Op::SetOutlineThickness { pt: Pt(width) });
            self.cur_thickness = Some(width);
        }
        self.ensure_round_caps();
        self.set_dash(dash);
        // Translucent stroke color → alpha graphics state (see fill_path).
        // Pen strokes pass opaque colors inside their begin_group, so this only
        // triggers for colors that genuinely carry alpha (shape strokeAttr).
        let translucent = color.a < 1.0;
        if translucent {
            self.ops.push(Op::SaveGraphicsState);
            let gs = self.alpha_gs(color.a, GroupBlend::Normal);
            self.ops.push(Op::LoadGraphicsState { gs });
        }
        self.ops.push(Op::DrawLine {
            line: Line {
                points: pts
                    .iter()
                    .map(|&(x, y)| LinePoint {
                        p: to_page(canvas, x, y),
                        bezier: false,
                    })
                    .collect(),
                is_closed: false,
            },
        });
        if translucent {
            self.ops.push(Op::RestoreGraphicsState);
        }
    }

    fn draw_image(&mut self, canvas: &Canvas, img: &RenderImage, key: (usize, usize)) {
        let Some(iref) = self.images.get(&key) else {
            return;
        };
        if iref.w == 0 || iref.h == 0 {
            return;
        }
        let w_pt = (img.right - img.left).abs();
        let h_pt = (img.bottom - img.top).abs();
        let x0 = img.left - canvas.origin_x;
        let y_bottom = canvas.height - (img.bottom - canvas.origin_y);
        self.ops.push(Op::UseXobject {
            id: iref.id.clone(),
            transform: XObjectTransform {
                translate_x: Some(Pt(x0)),
                translate_y: Some(Pt(y_bottom)),
                rotate: None,
                scale_x: Some(w_pt / iref.w as f32),
                scale_y: Some(h_pt / iref.h as f32),
                dpi: Some(72.0),
            },
        });
    }

    fn draw_inline_image(
        &mut self,
        canvas: &Canvas,
        bytes: &[u8],
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    ) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        let key = h.finish();
        if !self.inline_imgs.contains_key(&key) {
            let mut warnings = Vec::new();
            let Ok(raw) = RawImage::decode_from_bytes(bytes, &mut warnings) else {
                return;
            };
            let (w, h) = (raw.width, raw.height);
            let id = self.doc.add_image(&raw);
            self.inline_imgs.insert(key, ImageRef { id, w, h });
        }
        let iref = &self.inline_imgs[&key];
        if iref.w == 0 || iref.h == 0 {
            return;
        }
        let (iw, ih) = (iref.w as f32, iref.h as f32);
        let w_pt = (right - left).abs();
        let h_pt = (bottom - top).abs();
        let x0 = left - canvas.origin_x;
        let y_bottom = canvas.height - (bottom - canvas.origin_y);
        self.ops.push(Op::UseXobject {
            id: iref.id.clone(),
            transform: XObjectTransform {
                translate_x: Some(Pt(x0)),
                translate_y: Some(Pt(y_bottom)),
                rotate: None,
                scale_x: Some(w_pt / iw),
                scale_y: Some(h_pt / ih),
                dpi: Some(72.0),
            },
        });
    }

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
        font: &ResolvedFont,
    ) {
        let Some(font_id) = self.font_id(font) else {
            return;
        };
        let pos = to_page(canvas, x, baseline_y);
        self.ops.push(Op::StartTextSection);
        self.ops.push(Op::SetFont {
            font: PdfFontHandle::External(font_id),
            size: Pt(size),
        });
        self.ops.push(Op::SetFillColor {
            col: rgba_to_color(color),
        });
        // Faux bold = fill + stroke the glyph outlines.
        if bold {
            self.ops.push(Op::SetTextRenderingMode {
                mode: TextRenderingMode::FillStroke,
            });
            self.ops.push(Op::SetOutlineColor {
                col: rgba_to_color(color),
            });
            self.ops.push(Op::SetOutlineThickness {
                pt: Pt(size * 0.03),
            });
        }
        // Faux italic = shear the text matrix; this also sets the position.
        if italic {
            self.ops.push(Op::SetTextMatrix {
                matrix: TextMatrix::Raw([1.0, 0.0, 0.25, 1.0, pos.x.0, pos.y.0]),
            });
        } else {
            self.ops.push(Op::SetTextCursor { pos });
        }
        self.ops.push(Op::ShowText {
            items: vec![TextItem::Text(text.to_string())],
        });
        if bold {
            self.ops.push(Op::SetTextRenderingMode {
                mode: TextRenderingMode::Fill,
            });
        }
        self.ops.push(Op::EndTextSection);
        // The text section set its own fill/stroke; invalidate the caches.
        self.cur_fill = None;
        self.cur_outline = None;
        self.cur_thickness = None;
    }

    fn begin_group(&mut self, alpha: f32, blend: GroupBlend) -> bool {
        if alpha >= 1.0 {
            return false;
        }
        self.ops.push(Op::SaveGraphicsState);
        let gs = self.alpha_gs(alpha, blend);
        self.ops.push(Op::LoadGraphicsState { gs });
        true
    }

    fn end_group(&mut self) {
        // Balanced by contract (begin_group returned true). Restore reverts
        // graphics state, so caches are stale afterwards.
        self.ops.push(Op::RestoreGraphicsState);
        self.reset_caches();
    }
}

fn pt_to_mm(pt: f32) -> Mm {
    Pt(pt).into()
}

/// Convert a global-space point into page-space PDF points (y flipped).
fn to_page(canvas: &Canvas, gx: f32, gy: f32) -> Point {
    Point {
        x: Pt(gx - canvas.origin_x),
        y: Pt(canvas.height - (gy - canvas.origin_y)),
    }
}

fn rgba_to_color(c: Rgba) -> Color {
    Color::Rgb(Rgb::new(
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        None,
    ))
}

fn poly(rings: Vec<Vec<Point>>, mode: PaintMode) -> Polygon {
    Polygon {
        rings: rings
            .into_iter()
            .map(|points| PolygonRing {
                points: points
                    .into_iter()
                    .map(|p| LinePoint { p, bezier: false })
                    .collect(),
            })
            .collect(),
        mode,
        winding_order: WindingOrder::NonZero,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Document;

    fn backend() -> PdfBackend {
        PdfBackend::new(&Document { canvases: vec![] }, &[], HashMap::new())
    }

    #[test]
    fn dash_reasserts_solid_after_state_reset() {
        let mut b = backend();
        b.set_dash(Some(&[4.0, 4.0])); // dashed
        let after_dashed = b.ops.len();
        // A translucent group's RestoreGraphicsState reverts the PDF dash back to
        // dashed and reset_caches() marks the cache unknown.
        b.reset_caches();
        b.set_dash(None); // must re-emit solid, or later strokes leak the dash
        assert!(
            b.ops.len() > after_dashed,
            "solid dash re-asserted after a state reset"
        );
        assert!(matches!(b.ops.last(), Some(Op::SetLineDashPattern { .. })));
    }

    #[test]
    fn dash_coalesces_when_unchanged() {
        let mut b = backend();
        b.set_dash(None); // unknown -> solid: emits once
        let n = b.ops.len();
        b.set_dash(None); // already solid: coalesced, no new op
        assert_eq!(b.ops.len(), n, "redundant solid dash not re-emitted");
    }
}
