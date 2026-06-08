//! Text-box model building and rich-text/entity/font-face parsing (pens 6/16).
//! See the parent module docs.

use super::*;

pub(super) fn build_text(shape: &proto::Shape, ox: f32, oy: f32) -> Option<RenderItem> {
    let style: json_meta::TextStyle =
        json_meta::parse_opt(&shape.text_style_json).unwrap_or_default();
    // Styling (bold/italic/underline) is per-span in the rich-text HTML; plain
    // text (field 10) carries the style flags for the whole box.
    let mut runs = if !shape.rich_text.is_empty() {
        parse_rich_text(&shape.rich_text)
    } else {
        vec![StyledRun {
            text: shape.text_plain.clone(),
            bold: style.text_bold,
            italic: style.text_italic,
            underline: style.text_underline,
        }]
    };
    // Drop zero-width / BOM characters that render as tofu, then empty runs.
    for r in &mut runs {
        r.text
            .retain(|c| !matches!(c, '\u{200b}' | '\u{feff}' | '\u{200c}' | '\u{200d}'));
    }
    runs.retain(|r| !r.text.is_empty());
    if runs.iter().all(|r| r.text.trim().is_empty()) {
        return None;
    }
    let rect: Rect = json_meta::parse_opt(&shape.bbox_json)?;
    let color = argb_to_rgba(shape.color as u32);
    // NOTE: BOOX draws text boxes with no border, so the note's
    // border_color/width are not rendered either.
    Some(RenderItem::Text(RenderText {
        runs,
        left: rect.left + ox,
        top: rect.top + oy,
        right: rect.right + ox,
        bottom: rect.bottom + oy,
        font_size: if style.text_size > 0.0 {
            style.text_size
        } else {
            32.0
        },
        color,
        align: style.align_type,
        line_spacing: if style.text_spacing > 0.0 {
            style.text_spacing
        } else {
            1.2
        },
        font_name: first_font_face(&shape.rich_text),
        font_hint: (!style.font_face.is_empty())
            .then(|| std::path::PathBuf::from(&style.font_face)),
    }))
}

/// Parse the rich-text HTML into styled runs, tracking `<b>/<strong>`,
/// `<i>/<em>`, `<u>` nesting. `<br>` and block ends (`</div>`, `</p>`) become
/// `\n`; other tags (font, span, div, version) are ignored. Entities are decoded.
fn parse_rich_text(html: &str) -> Vec<StyledRun> {
    let (mut b, mut i, mut u) = (0i32, 0i32, 0i32);
    let mut runs: Vec<StyledRun> = Vec::new();
    let push = |c: char, b: i32, i: i32, u: i32, runs: &mut Vec<StyledRun>| {
        let (bo, it, ul) = (b > 0, i > 0, u > 0);
        match runs.last_mut() {
            Some(last) if last.bold == bo && last.italic == it && last.underline == ul => {
                last.text.push(c);
            }
            _ => runs.push(StyledRun {
                text: c.to_string(),
                bold: bo,
                italic: it,
                underline: ul,
            }),
        }
    };
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                let mut tag = String::new();
                for c2 in chars.by_ref() {
                    if c2 == '>' {
                        break;
                    }
                    tag.push(c2);
                }
                let t = tag.trim();
                let closing = t.starts_with('/');
                let name = t
                    .trim_start_matches('/')
                    .split(|ch: char| ch.is_whitespace())
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let d = if closing { -1 } else { 1 };
                match name.as_str() {
                    "b" | "strong" => b = (b + d).max(0),
                    "i" | "em" => i = (i + d).max(0),
                    "u" => u = (u + d).max(0),
                    "br" => push('\n', b, i, u, &mut runs),
                    "div" | "p" if closing => push('\n', b, i, u, &mut runs),
                    _ => {}
                }
            }
            '&' => {
                let mut ent = String::new();
                let mut terminated = false;
                while let Some(&c2) = chars.peek() {
                    if c2 == ';' {
                        chars.next();
                        terminated = true;
                        break;
                    }
                    if ent.len() >= 8 {
                        break; // too long to be an entity; leave c2 for the outer loop
                    }
                    ent.push(c2);
                    chars.next();
                }
                match decode_entity(&ent) {
                    Some(ch) => push(ch, b, i, u, &mut runs),
                    // Not a recognized entity (e.g. a raw "AT&T", a missing
                    // semicolon): keep the literal text rather than dropping it.
                    None => {
                        push('&', b, i, u, &mut runs);
                        for ch in ent.chars() {
                            push(ch, b, i, u, &mut runs);
                        }
                        if terminated {
                            push(';', b, i, u, &mut runs);
                        }
                    }
                }
            }
            _ => push(c, b, i, u, &mut runs),
        }
    }
    runs
}

/// First `<font face="...">` family in the rich-text HTML (the part before any
/// `&`/`&amp;` fallback list), used as the requested font name.
fn first_font_face(html: &str) -> Option<String> {
    let val = html.split("face=\"").nth(1)?.split('"').next()?;
    let first = val.split("&amp;").next().unwrap_or(val);
    let first = first.split(" & ").next().unwrap_or(first).trim();
    (!first.is_empty()).then(|| first.to_string())
}

/// Decode a (named or numeric) HTML entity body (the part between `&` and `;`).
fn decode_entity(ent: &str) -> Option<char> {
    match ent {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let num = ent.strip_prefix('#')?;
            let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                num.parse::<u32>().ok()?
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_text_tracks_bold_italic_underline() {
        let runs = parse_rich_text("<b>A</b><i>B</i><u>C</u>plain");
        assert_eq!(runs.len(), 4);
        assert!(runs[0].bold && !runs[0].italic);
        assert_eq!(runs[0].text, "A");
        assert!(runs[1].italic);
        assert!(runs[2].underline);
        assert!(!runs[3].bold && !runs[3].italic && !runs[3].underline);
        assert_eq!(runs[3].text, "plain");
    }

    #[test]
    fn rich_text_nested_styles_combine() {
        let runs = parse_rich_text("<b><i>X</i></b>");
        assert_eq!(runs.len(), 1);
        assert!(runs[0].bold && runs[0].italic);
    }

    #[test]
    fn rich_text_br_and_entities() {
        let runs = parse_rich_text("a<br>b&amp;c&#65;");
        let joined: String = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(joined, "a\nb&cA");
    }

    #[test]
    fn rich_text_keeps_unrecognized_entities() {
        // A raw ampersand (AT&T) and an unknown entity must survive verbatim
        // instead of being dropped.
        let runs = parse_rich_text("AT&T &foo; &unknown");
        let joined: String = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(joined, "AT&T &foo; &unknown");
    }

    #[test]
    fn first_font_face_takes_primary_family() {
        assert_eq!(
            first_font_face(r#"<font face="Noto Sans CJK JP &amp; sans-serif">x</font>"#),
            Some("Noto Sans CJK JP".to_string())
        );
        assert_eq!(first_font_face("<font>no face</font>"), None);
    }

    #[test]
    fn decode_entity_named_and_numeric() {
        assert_eq!(decode_entity("amp"), Some('&'));
        assert_eq!(decode_entity("lt"), Some('<'));
        assert_eq!(decode_entity("#65"), Some('A'));
        assert_eq!(decode_entity("#x4e2d"), Some('中'));
        assert_eq!(decode_entity("bogus"), None);
    }
}
