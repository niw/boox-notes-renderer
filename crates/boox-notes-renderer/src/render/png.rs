//! PNG backend (tiny-skia raster). One PNG per canvas at `scale` px/pt, in note
//! space (top-left, y-down). Vectors are drawn with tiny-skia paths; text is
//! rasterized from glyph outlines (`ttf-parser`); translucent strokes composite
//! on a temporary layer to avoid double-darkening at overlaps.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use tiny_skia::{
    BlendMode, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint,
    PremultipliedColorU8, Stroke, StrokeDash, Transform,
};
use ttf_parser::OutlineBuilder;

use super::fonts::ResolvedFont;
use super::{Backend, FontOptions, GroupBlend, PageSel, paint_canvas, selected_pages};
use crate::model::{Canvas, Document, Rgba};

/// Render selected note pages to PNG at `scale` px per pt. Returns
/// `(filename suffix, png bytes)` pairs.
pub fn render_png(
    doc: &Document,
    font_opts: &FontOptions,
    sel: PageSel,
    scale: f32,
) -> crate::error::Result<Vec<(String, Vec<u8>)>> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    // Validate the page selection before any font work (build_db may download).
    let selected = selected_pages(doc.canvases.len(), sel)?;
    let db = font_opts.build_db();
    let mut out = Vec::new();
    for (suffix, ci) in selected {
        let canvas = &doc.canvases[ci];
        let Some(mut be) = PngBackend::new(canvas, scale) else {
            log::warn!("page too large to rasterize at scale {scale}; skipped");
            continue;
        };
        paint_canvas(&mut be, canvas, ci, &db);
        if let Some(bytes) = be.finish() {
            out.push((suffix, bytes));
        }
    }
    Ok(out)
}

struct PngBackend {
    layers: Vec<Pixmap>,
    group_alpha: Vec<(f32, GroupBlend)>,
    w: u32,
    h: u32,
    scale: f32,
    ox: f32,
    oy: f32,
    /// Font files mmapped on first use, cached by path (a `.ttc` is shared by
    /// every face index in it).
    font_data: HashMap<PathBuf, Option<Rc<memmap2::Mmap>>>,
}

impl PngBackend {
    fn new(canvas: &Canvas, scale: f32) -> Option<Self> {
        let w = (canvas.width * scale).ceil().max(1.0) as u32;
        let h = (canvas.height * scale).ceil().max(1.0) as u32;
        let base = Pixmap::new(w, h)?;
        Some(Self {
            layers: vec![base],
            group_alpha: Vec::new(),
            w,
            h,
            scale,
            ox: canvas.origin_x,
            oy: canvas.origin_y,
            font_data: HashMap::new(),
        })
    }

    fn finish(mut self) -> Option<Vec<u8>> {
        self.layers.drain(1..); // discard any unclosed groups
        self.layers.pop().and_then(|p| p.encode_png().ok())
    }

    fn cur(&mut self) -> &mut Pixmap {
        self.layers.last_mut().expect("base layer always present")
    }

    fn px(&self, x: f32) -> f32 {
        (x - self.ox) * self.scale
    }
    fn py(&self, y: f32) -> f32 {
        (y - self.oy) * self.scale
    }
}

fn paint_for(color: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    // Honor the color's alpha (e.g. a shape's translucent fillAttr). Pen
    // strokes pass opaque colors inside their begin_group layer, so no
    // double-apply.
    let a = (color.a.clamp(0.0, 1.0) * 255.0).round() as u8;
    p.set_color_rgba8(color.r, color.g, color.b, a);
    p.anti_alias = true;
    p
}

impl Backend for PngBackend {
    fn fill_background(&mut self, _canvas: &Canvas, color: Rgba) {
        self.cur()
            .fill(tiny_skia::Color::from_rgba8(color.r, color.g, color.b, 255));
    }

    fn fill_path(&mut self, _canvas: &Canvas, rings: &[Vec<(f32, f32)>], color: Rgba) {
        let mut pb = PathBuilder::new();
        for ring in rings {
            for (i, &(x, y)) in ring.iter().enumerate() {
                let (px, py) = (self.px(x), self.py(y));
                if i == 0 {
                    pb.move_to(px, py);
                } else {
                    pb.line_to(px, py);
                }
            }
            pb.close();
        }
        if let Some(path) = pb.finish() {
            let paint = paint_for(color);
            self.cur().fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    fn stroke_polyline(
        &mut self,
        _canvas: &Canvas,
        pts: &[(f32, f32)],
        width: f32,
        color: Rgba,
        dash: Option<&[f32]>,
    ) {
        if pts.len() < 2 {
            return;
        }
        let mut pb = PathBuilder::new();
        for (i, &(x, y)) in pts.iter().enumerate() {
            let (px, py) = (self.px(x), self.py(y));
            if i == 0 {
                pb.move_to(px, py);
            } else {
                pb.line_to(px, py);
            }
        }
        let Some(path) = pb.finish() else {
            return;
        };
        let stroke = Stroke {
            width: (width * self.scale).max(0.1),
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            dash: dash
                .and_then(|d| StrokeDash::new(d.iter().map(|x| x * self.scale).collect(), 0.0)),
            ..Default::default()
        };
        let paint = paint_for(color);
        self.cur()
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn draw_inline_image(
        &mut self,
        _canvas: &Canvas,
        bytes: &[u8],
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    ) {
        let Some(src) = decode_premultiplied(bytes) else {
            return;
        };
        let (iw, ih) = (src.width() as f32, src.height() as f32);
        if iw == 0.0 || ih == 0.0 {
            return;
        }
        let tw = (right - left).abs() * self.scale;
        let th = (bottom - top).abs() * self.scale;
        let transform =
            Transform::from_row(tw / iw, 0.0, 0.0, th / ih, self.px(left), self.py(top));
        self.cur()
            .draw_pixmap(0, 0, src.as_ref(), &PixmapPaint::default(), transform, None);
    }

    fn draw_text_line(
        &mut self,
        _canvas: &Canvas,
        x: f32,
        baseline_y: f32,
        text: &str,
        size: f32,
        color: Rgba,
        bold: bool,
        italic: bool,
        font: &ResolvedFont,
    ) {
        let data = self
            .font_data
            .entry(font.0.clone())
            .or_insert_with(|| {
                let file = std::fs::File::open(&font.0).ok()?;
                // mmap so a multi-megabyte .ttc isn't read into memory just to
                // outline a few glyphs (same approach as FontDb::font_data).
                unsafe { memmap2::Mmap::map(&file) }.ok().map(Rc::new)
            })
            .clone();
        let Some(data) = data else {
            return;
        };
        let Ok(face) = ttf_parser::Face::parse(&data, font.1) else {
            return;
        };
        let upem = face.units_per_em() as f32;
        let fscale = size / upem;
        let slant = if italic { 0.25 } else { 0.0 };
        let mut pb = PathBuilder::new();
        let mut pen = x; // note-space x of the current glyph origin
        for ch in text.chars() {
            if let Some(gid) = face.glyph_index(ch) {
                let mut o = Outliner {
                    pb: &mut pb,
                    fscale: fscale * self.scale,
                    pen_px: (pen - self.ox) * self.scale,
                    base_px: (baseline_y - self.oy) * self.scale,
                    slant,
                };
                face.outline_glyph(gid, &mut o);
                let adv = face.glyph_hor_advance(gid).unwrap_or(0) as f32;
                pen += adv * fscale;
            }
        }
        if let Some(path) = pb.finish() {
            let paint = paint_for(color);
            self.cur().fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            // Faux bold: stroke the outline on top of the fill.
            if bold {
                let stroke = Stroke {
                    width: (size * self.scale * 0.06).max(1.0),
                    line_cap: LineCap::Round,
                    line_join: LineJoin::Round,
                    ..Default::default()
                };
                self.cur()
                    .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
    }

    fn begin_group(&mut self, alpha: f32, blend: GroupBlend) -> bool {
        // A failed layer allocation reports false: the caller then draws with
        // per-primitive alpha instead of opaque-into-a-missing-group.
        if alpha < 1.0
            && let Some(layer) = Pixmap::new(self.w, self.h)
        {
            self.layers.push(layer);
            self.group_alpha.push((alpha, blend));
            return true;
        }
        false
    }

    fn end_group(&mut self) {
        if let Some((a, blend)) = self.group_alpha.pop() {
            let layer = self.layers.pop().expect("group layer present");
            let paint = PixmapPaint {
                opacity: a,
                blend_mode: match blend {
                    GroupBlend::Normal => BlendMode::SourceOver,
                    GroupBlend::Multiply => BlendMode::Multiply,
                },
                ..Default::default()
            };
            self.cur()
                .draw_pixmap(0, 0, layer.as_ref(), &paint, Transform::identity(), None);
        }
    }
}

/// Glyph outline → tiny-skia path. Font units are y-up; note space is y-down, so
/// glyph y is subtracted from the baseline. All in pixel space.
struct Outliner<'a> {
    pb: &'a mut PathBuilder,
    fscale: f32,
    pen_px: f32,
    base_px: f32,
    slant: f32,
}

impl Outliner<'_> {
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        let py = self.base_px - y * self.fscale;
        // Faux italic: shear x rightward proportional to height above the baseline.
        let px = self.pen_px + x * self.fscale + (self.base_px - py) * self.slant;
        (px, py)
    }
}

impl OutlineBuilder for Outliner<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (px, py) = self.map(x, y);
        self.pb.move_to(px, py);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (px, py) = self.map(x, y);
        self.pb.line_to(px, py);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (cx, cy) = self.map(x1, y1);
        let (px, py) = self.map(x, y);
        self.pb.quad_to(cx, cy, px, py);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (c1x, c1y) = self.map(x1, y1);
        let (c2x, c2y) = self.map(x2, y2);
        let (px, py) = self.map(x, y);
        self.pb.cubic_to(c1x, c1y, c2x, c2y, px, py);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// Decode an embedded PNG/JPEG into a premultiplied tiny-skia pixmap.
fn decode_premultiplied(bytes: &[u8]) -> Option<Pixmap> {
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut pm = Pixmap::new(w, h)?;
    for (dst, px) in pm.pixels_mut().iter_mut().zip(rgba.pixels()) {
        let [r, g, b, a] = px.0;
        *dst = PremultipliedColorU8::from_rgba(premul(r, a), premul(g, a), premul(b, a), a)
            .unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
    Some(pm)
}

fn premul(c: u8, a: u8) -> u8 {
    ((c as u16 * a as u16 + 127) / 255) as u8
}
