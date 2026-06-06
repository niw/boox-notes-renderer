//! JSON payloads embedded inside the protobuf messages.
//!
//! Boox stores most structured configuration as JSON strings within protobuf
//! fields (pen settings, rectangles, layer lists, line styles). These mirror the
//! shapes seen in real exports; unknown keys are ignored by serde.

use serde::Deserialize;

/// A rectangle in note coordinates. Used both for tile placement (global) and
/// content bounds (local). Origin is top-left, y grows downward.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Rect {
    #[serde(default)]
    pub left: f32,
    #[serde(default)]
    pub top: f32,
    #[serde(default)]
    pub right: f32,
    #[serde(default)]
    pub bottom: f32,
}

impl Rect {
    pub fn width(&self) -> f32 {
        self.right - self.left
    }
    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }
}

/// `virtual/doc` content descriptor; `content_type == "geo_layout"` marks the
/// infinite-note format we support.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocContent {
    #[serde(default)]
    pub content_type: String,
}

/// Pen settings from `note_info`. Drives stroke width/color resolution.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PenSettings {
    /// Default fill/ink color (ARGB as a signed int).
    #[serde(default)]
    pub fill_color: i64,
    /// pen_type -> width (in note points).
    #[serde(default)]
    pub pen_with_map: std::collections::HashMap<String, f32>,
    #[serde(default)]
    pub quick_pen_list: QuickPenList,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickPenList {
    #[serde(default)]
    pub quick_pens: Vec<QuickPen>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickPen {
    #[serde(default)]
    pub color: i64,
    #[serde(rename = "type", default)]
    pub type_: u8,
    #[serde(default)]
    pub width: f32,
}

/// `{"pageNameList":[<uuid>,...]}` — the note's pages in order.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageNameList {
    #[serde(default)]
    pub page_name_list: Vec<String>,
}

/// `pageModel` layer list payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageModelLayers {
    #[serde(default)]
    pub layer_list: Vec<Layer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer {
    #[serde(default)]
    pub id: u32,
    #[serde(default = "default_layer_show")]
    pub show: bool,
    /// Not consumed: a locked layer still renders; locking only affects editing.
    #[serde(default)]
    #[allow(dead_code)]
    pub lock: bool,
}

fn default_layer_show() -> bool {
    true
}

/// Text-box style (`shape` field 9).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextStyle {
    #[serde(default = "default_text_size")]
    pub text_size: f32,
    /// 0 = left, 1 = center, 2 = right.
    #[serde(default)]
    pub align_type: u8,
    #[serde(default)]
    pub text_bold: bool,
    #[serde(default)]
    pub text_italic: bool,
    #[serde(default)]
    pub text_underline: bool,
    #[serde(default)]
    pub text_spacing: f32,
    /// Font file path as stored by the BOOX device (e.g. `/system/fonts/Roboto-Regular.ttf`);
    /// usually not present on the host, so we fall back to a system font.
    #[serde(default)]
    pub font_face: String,
    /// Not consumed: BOOX draws text boxes with no border, so neither do we.
    #[serde(default)]
    #[allow(dead_code)]
    pub border_color: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub border_width: f32,
}

fn default_text_size() -> f32 {
    32.0
}

/// Image descriptor (`shape` field 14). Only the resource path is needed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInfo {
    #[serde(default)]
    pub relative_path: String,
    #[serde(default)]
    pub local_path: String,
}

/// A 3x3 affine matrix container (`{"values":[a,b,tx,c,d,ty,0,0,1]}`).
#[derive(Debug, Clone, Deserialize)]
pub struct MatrixValues {
    pub values: Vec<f32>,
}

/// `createArgs` (shape field 11, `render_scale_json`). Only `penAttrs.texture`
/// is consumed — the charcoal grain variant.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateArgs {
    #[serde(default)]
    pub pen_attrs: PenAttrs,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PenAttrs {
    /// Charcoal (pen 22) grain variant: 1 = dense V1 (the default), 2 = V2
    /// (breaks up at light pressure).
    #[serde(default = "default_texture")]
    pub texture: u8,
}

impl Default for PenAttrs {
    fn default() -> Self {
        Self {
            texture: default_texture(),
        }
    }
}

fn default_texture() -> u8 {
    1
}

/// Shape-level line style (shape field 17): `{"lineStyle":{…}}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineStyleWrap {
    pub line_style: Option<LineStyle>,
}

/// `{type, dashLineIntervals, phase}` — `type != 0` marks a dashed edge.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineStyle {
    #[serde(rename = "type", default)]
    pub type_: i64,
    /// Dash on/off lengths (shape-local units; the consumer scales them).
    #[serde(default)]
    pub dash_line_intervals: Vec<f64>,
}

/// Parse a JSON string, returning `None` (rather than erroring) on empty input
/// or malformed data so a single bad field never aborts a whole note.
pub fn parse_opt<T: for<'de> Deserialize<'de>>(json: &str) -> Option<T> {
    if json.trim().is_empty() {
        return None;
    }
    let sanitized = sanitize_unquoted_keys(json);
    serde_json::from_str(&sanitized).ok()
}

/// Boox serializes maps with integer keys using unquoted keys, e.g.
/// `{0:5.9,5:0.2}`, which is not valid JSON. Quote any bare numeric object key
/// (a number in key position immediately followed by `:`) so serde can parse it.
///
/// The scanner is string-aware so numbers inside string literals or in value
/// position are never touched.
pub fn sanitize_unquoted_keys(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            '{' | ',' => {
                out.push(c);
                i += 1;
                // Skip whitespace following the structural char.
                let mut j = i;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                // Collect a bare numeric key candidate.
                let start = j;
                while j < bytes.len() && matches!(bytes[j] as char, '0'..='9' | '-' | '+' | '.') {
                    j += 1;
                }
                if j > start && j < bytes.len() && bytes[j] as char == ':' {
                    out.push_str(&input[i..start]); // preserved whitespace
                    out.push('"');
                    out.push_str(&input[start..j]);
                    out.push('"');
                    i = j; // continue from the ':'
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_bare_numeric_keys() {
        assert_eq!(
            sanitize_unquoted_keys(r#"{"penWithMap":{0:5.9,15:35.4}}"#),
            r#"{"penWithMap":{"0":5.9,"15":35.4}}"#
        );
    }

    #[test]
    fn leaves_arrays_and_string_numbers_alone() {
        assert_eq!(sanitize_unquoted_keys("[1,2,3]"), "[1,2,3]");
        assert_eq!(
            sanitize_unquoted_keys(r#"{"a":"1:2","b":3}"#),
            r#"{"a":"1:2","b":3}"#
        );
    }

    #[test]
    fn parses_pen_settings_with_int_keys() {
        let json = r#"{"fillColor":-16777216,"penWithMap":{0:5.9,5:5.9,15:35.4},"quickPenList":{"quickPens":[{"color":-40605,"type":5,"width":5.9}]}}"#;
        let pen: PenSettings = parse_opt(json).unwrap();
        assert_eq!(pen.fill_color, -16777216);
        assert_eq!(pen.pen_with_map.get("15").copied(), Some(35.4));
        assert_eq!(pen.quick_pen_list.quick_pens.len(), 1);
    }

    #[test]
    fn layer_defaults_to_visible_when_show_is_absent() {
        let json = r#"{"layerList":[{"id":7}]}"#;
        let layers: PageModelLayers = parse_opt(json).unwrap();
        assert_eq!(layers.layer_list.len(), 1);
        assert!(layers.layer_list[0].show);
    }
}
