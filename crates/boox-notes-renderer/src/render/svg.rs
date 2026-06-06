//! SVG backend. One self-contained SVG per canvas, in note space (top-left,
//! y-down — no Y flip). Text uses an embedded **subset** font (`@font-face` with
//! a base64 data URI containing only the glyphs the note actually uses), so
//! Japanese text renders without shipping a multi-megabyte font.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use base64::Engine as _;

use super::fonts::ResolvedFont;
use super::{Backend, FontOptions, GroupBlend, PageSel, paint_canvas, selected_pages};
use crate::model::{Canvas, Document, Rgba};

/// Render selected note pages to SVG. Returns `(filename suffix, svg)` pairs.
/// Each page embeds a subset `@font-face` per distinct font it uses.
pub fn render_svg(
    doc: &Document,
    font_opts: &FontOptions,
    sel: PageSel,
) -> crate::error::Result<Vec<(String, String)>> {
    // Validate the page selection before any font work (build_db may download).
    let selected = selected_pages(doc.canvases.len(), sel)?;
    let db = font_opts.build_db();
    Ok(selected
        .into_iter()
        .map(|(suffix, ci)| {
            let canvas = &doc.canvases[ci];

            // Group used codepoints by the font that will actually draw them
            // (the box's resolved font or the per-char CJK fallback).
            let used = super::fonts::used_chars(&db, [canvas]);

            // One subset @font-face per font, with a distinct family name.
            let mut defs = String::new();
            let mut families: HashMap<ResolvedFont, String> = HashMap::new();
            for (i, (font, chars)) in used.iter().enumerate() {
                let family = format!("boox-embed-{i}");
                if let Some(css) = build_font_face(font, chars, &family) {
                    defs.push_str(&css);
                    families.insert(font.clone(), family);
                }
            }

            let mut be = SvgBackend::new(canvas, &defs, families);
            paint_canvas(&mut be, canvas, ci, &db);
            (suffix, be.finish())
        })
        .collect())
}

/// Build a `@font-face` (named `family`) embedding a subset of `font` containing
/// only `chars`. `None` if the file/subsetting fails.
fn build_font_face(font: &ResolvedFont, chars: &BTreeSet<char>, family: &str) -> Option<String> {
    // A minimal valid OpenType font with a Unicode cmap (browsers reject
    // Mac-Roman-only cmaps).
    let subset = super::fonts::subset_font_bytes(font, chars)?;

    // CFF-flavoured fonts (Hiragino) start with "OTTO" → OpenType; glyf fonts are
    // TrueType. The browser sniffs the data, but set a correct hint anyway.
    let (mime, fmt) = if subset.starts_with(b"OTTO") {
        ("font/otf", "opentype")
    } else {
        ("font/ttf", "truetype")
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&subset);
    Some(format!(
        "@font-face{{font-family:\"{family}\";src:url(data:{mime};base64,{b64}) format(\"{fmt}\");}}"
    ))
}

struct SvgBackend {
    buf: String,
    ox: f32,
    oy: f32,
    /// Resolved font → embedded `@font-face` family name.
    families: HashMap<ResolvedFont, String>,
    group_depth: usize,
}

impl SvgBackend {
    fn new(canvas: &Canvas, defs: &str, families: HashMap<ResolvedFont, String>) -> Self {
        let mut buf = String::new();
        let _ = write!(
            buf,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">",
            w = fnum(canvas.width),
            h = fnum(canvas.height),
        );
        if !defs.is_empty() {
            let _ = write!(buf, "<defs><style>{defs}</style></defs>");
        }
        Self {
            buf,
            ox: canvas.origin_x,
            oy: canvas.origin_y,
            families,
            group_depth: 0,
        }
    }

    fn finish(mut self) -> String {
        while self.group_depth > 0 {
            self.buf.push_str("</g>");
            self.group_depth -= 1;
        }
        self.buf.push_str("</svg>");
        self.buf
    }

    fn tx(&self, x: f32) -> f32 {
        x - self.ox
    }
    fn ty(&self, y: f32) -> f32 {
        y - self.oy
    }
}

impl Backend for SvgBackend {
    fn fill_background(&mut self, canvas: &Canvas, color: Rgba) {
        let _ = write!(
            self.buf,
            "<rect x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" fill=\"{}\"/>",
            fnum(canvas.width),
            fnum(canvas.height),
            hex(color),
        );
    }

    fn fill_path(&mut self, _canvas: &Canvas, rings: &[Vec<(f32, f32)>], color: Rgba) {
        let mut d = String::new();
        for ring in rings {
            for (i, &(x, y)) in ring.iter().enumerate() {
                let cmd = if i == 0 { 'M' } else { 'L' };
                let _ = write!(d, "{cmd}{} {} ", fnum(self.tx(x)), fnum(self.ty(y)));
            }
            d.push('Z');
        }
        let _ = write!(
            self.buf,
            "<path d=\"{d}\" fill=\"{}\" fill-rule=\"nonzero\"{}/>",
            hex(color),
            opacity_attr("fill-opacity", color.a),
        );
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
        let mut points = String::new();
        for &(x, y) in pts {
            let _ = write!(points, "{},{} ", fnum(self.tx(x)), fnum(self.ty(y)));
        }
        let dash_attr = match dash {
            Some(d) if !d.is_empty() => {
                let arr: Vec<String> = d.iter().map(|x| fnum(*x)).collect();
                format!(" stroke-dasharray=\"{}\"", arr.join(","))
            }
            _ => String::new(),
        };
        let _ = write!(
            self.buf,
            "<polyline points=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"{}{}/>",
            points.trim_end(),
            hex(color),
            fnum(width),
            opacity_attr("stroke-opacity", color.a),
            dash_attr,
        );
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
        let mime = if bytes.starts_with(&[0xFF, 0xD8]) {
            "image/jpeg"
        } else {
            "image/png"
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let _ = write!(
            self.buf,
            "<image x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" href=\"data:{mime};base64,{b64}\"/>",
            fnum(self.tx(left)),
            fnum(self.ty(top)),
            fnum((right - left).abs()),
            fnum((bottom - top).abs()),
        );
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
        let Some(family) = self.families.get(font) else {
            return;
        };
        let weight = if bold { " font-weight=\"bold\"" } else { "" };
        let style = if italic { " font-style=\"italic\"" } else { "" };
        let _ = write!(
            self.buf,
            "<text x=\"{}\" y=\"{}\" font-family=\"{family}\" font-size=\"{}\" fill=\"{}\"{weight}{style} xml:space=\"preserve\">{}</text>",
            fnum(self.tx(x)),
            fnum(self.ty(baseline_y)),
            fnum(size),
            hex(color),
            xml_escape(text),
        );
    }

    fn begin_group(&mut self, alpha: f32, blend: GroupBlend) -> bool {
        if alpha >= 1.0 {
            return false;
        }
        let style = match blend {
            GroupBlend::Normal => "",
            GroupBlend::Multiply => " style=\"mix-blend-mode:multiply\"",
        };
        let _ = write!(self.buf, "<g opacity=\"{}\"{}>", fnum(alpha), style);
        self.group_depth += 1;
        true
    }

    fn end_group(&mut self) {
        if self.group_depth > 0 {
            self.buf.push_str("</g>");
            self.group_depth -= 1;
        }
    }
}

/// Format a float compactly (up to 2 decimals, trailing zeros trimmed).
fn fnum(v: f32) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

fn hex(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// An ` attr="a"` fragment when `a < 1`, else empty.
fn opacity_attr(attr: &str, a: f32) -> String {
    if a < 1.0 {
        format!(" {attr}=\"{}\"", fnum(a))
    } else {
        String::new()
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}
