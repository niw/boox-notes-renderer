//! Text-box layout (backend-agnostic) and font resolution.
//!
//! Boox text boxes carry plain text, a font size and an alignment within a
//! bounding rect. Lines break only at the note's explicit `\n`s (no width
//! wrapping); real glyph advances resolve each line to an absolute
//! `(x, baseline)`, and the backend draws the glyphs (PDF embeds the font,
//! SVG embeds a subset, PNG rasterizes outlines).

use super::fonts::{CharDraw, FontDb};
use super::{Backend, emoji};
use crate::model::{Canvas, RenderText};

/// A character with its resolved style, draw decision and advance width.
#[derive(Clone)]
struct SChar {
    c: char,
    bold: bool,
    italic: bool,
    underline: bool,
    /// How the char draws (glyph from which font / emoji image / nothing),
    /// decided once by [`super::fonts::char_draw`].
    draw: CharDraw,
    /// Advance width in note units (already scaled by the font size).
    w: f32,
}

pub(crate) fn draw(b: &mut impl Backend, canvas: &Canvas, t: &RenderText, fonts: &FontDb) {
    let box_w = (t.right - t.left).abs();
    let size = t.font_size;
    // Resolve this box's font once (name request + BOOX device-path hint).
    let Some(font) = fonts.resolve(t.font_name.as_deref(), t.font_hint.as_deref()) else {
        return; // no usable font
    };

    // Flatten styled runs into characters: per char, decide how it draws
    // (`char_draw` — the same decision the subset collection makes) and
    // measure the real advance — the 0.5/1.0 em heuristic drifts from what
    // the backend actually draws, which misplaces every run after a style
    // change.
    let lang = super::fonts::lang_hint(t.font_name.as_deref());
    let mut chars: Vec<SChar> = Vec::new();
    for run in &t.runs {
        for c in run.text.chars() {
            let draw = super::fonts::char_draw(fonts, &font, c, lang);
            let w = match &draw {
                CharDraw::Skip => 0.0,
                CharDraw::Emoji => size, // an inline image, one em square
                CharDraw::Glyph(f) => fonts
                    .advance_em(f, c)
                    .map(|em| em * size)
                    .unwrap_or_else(|| char_em(c) * size),
            };
            chars.push(SChar {
                c,
                bold: run.bold,
                italic: run.italic,
                underline: run.underline,
                draw,
                w,
            });
        }
    }
    let lines = split_lines(&chars);

    let line_height = size * t.line_spacing;
    // First baseline ~0.82em below the box top (typical ascent ratio).
    let mut baseline = t.top + size * 0.82;
    for line in &lines {
        if !line.is_empty() {
            let lw: f32 = line.iter().map(|sc| sc.w).sum();
            let mut x = match t.align {
                1 => t.left + (box_w - lw) / 2.0,
                2 => t.right - lw,
                _ => t.left,
            };
            // Walk the line: color-emoji become inline images, runs of same
            // style+font go through draw_text_line (each with its own underline).
            let mut i = 0;
            while i < line.len() {
                let sc = &line[i];
                let run_font = match &sc.draw {
                    CharDraw::Skip => {
                        i += 1;
                        continue;
                    }
                    CharDraw::Emoji => {
                        if let Some(png) = emoji::png_for(fonts, sc.c) {
                            let top = baseline - size * 0.82;
                            b.draw_inline_image(canvas, &png, x, top, x + size, top + size);
                        }
                        x += sc.w;
                        i += 1;
                        continue;
                    }
                    CharDraw::Glyph(f) => f.clone(),
                };
                let (bold, italic, underline) = (sc.bold, sc.italic, sc.underline);
                let mut s = String::new();
                let mut gw = 0.0;
                while i < line.len() {
                    let c2 = &line[i];
                    match &c2.draw {
                        // Modifiers vanish inside a run too.
                        CharDraw::Skip => i += 1,
                        CharDraw::Glyph(f2)
                            if *f2 == run_font
                                && (c2.bold, c2.italic, c2.underline)
                                    == (bold, italic, underline) =>
                        {
                            s.push(c2.c);
                            gw += c2.w;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                b.draw_text_line(
                    canvas, x, baseline, &s, size, t.color, bold, italic, &run_font,
                );
                if underline {
                    let uy = baseline + size * 0.12;
                    b.stroke_polyline(
                        canvas,
                        &[(x, uy), (x + gw, uy)],
                        (size * 0.05).max(1.0),
                        t.color,
                        None,
                    );
                }
                x += gw;
            }
        }
        baseline += line_height;
    }
}

/// Rough advance width of a character in em units: CJK/full-width glyphs ~1.0,
/// everything else ~0.5. Last resort when no font covers the char.
fn char_em(c: char) -> f32 {
    if emoji::is_emoji_modifier(c) {
        0.0
    } else if emoji::is_emoji(c) || (c as u32) >= 0x2E80 {
        1.0
    } else {
        0.5
    }
}

/// Split measured characters into lines on `\n` only. The note stores
/// BOOX's line breaks as explicit `\n`s, so we never invent width wraps: a
/// substituted font that runs wider than BOOX's simply overflows the
/// box, which beats adding line breaks the note doesn't contain.
fn split_lines(chars: &[SChar]) -> Vec<Vec<SChar>> {
    let mut out = Vec::new();
    let mut cur: Vec<SChar> = Vec::new();
    for sc in chars {
        if sc.c == '\n' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(sc.clone());
        }
    }
    out.push(cur);
    out
}
