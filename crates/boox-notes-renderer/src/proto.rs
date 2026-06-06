//! Protobuf message definitions for the `.note` format.
//!
//! Field numbers follow the boox-note-parser project's schema. We
//! declare the messages inline with `#[derive(prost::Message)]` so no `protoc`
//! or `build.rs` is needed. Most "interesting" payloads are JSON strings stored
//! inside these messages; we decode those separately in [`crate::json_meta`].
//!
//! NOTE: on the `.note` files sampled from a BOOX device, the `virtual/page`
//! payload is a *repeated* container (one entry per canvas tile), unlike the
//! single-message form seen in files from some other devices. We model it as
//! repeated.

use prost::Message;

/// `note/pb/note_info` — a container wrapping the note metadata in field 1.
#[derive(Clone, PartialEq, Message)]
pub struct NoteInfo {
    #[prost(message, repeated, tag = "1")]
    pub notes: Vec<NoteMetadata>,
}

/// The note's metadata (inside [`NoteInfo`] field 1). The interesting payloads
/// are JSON strings. Some fields are decoded only to document the format's field
/// numbers and are not consumed (marked below); the rest drive rendering.
#[derive(Clone, PartialEq, Message)]
pub struct NoteMetadata {
    /// Not consumed: per-pen widths come from `pen_settings_json` instead.
    #[prost(float, tag = "9")]
    pub pen_width: f32,
    #[prost(string, tag = "11")]
    pub pen_settings_json: String,
    /// Not consumed (viewport/zoom state).
    #[prost(string, tag = "12")]
    pub canvas_state_json: String,
    /// Not consumed: the ink fill color comes from `pen_settings_json.fillColor`.
    #[prost(uint32, tag = "15")]
    pub fill_color: u32,
    /// `{"pageNameList":[<uuid>,...]}` — the note's pages in order.
    #[prost(string, tag = "20")]
    pub active_pages_json: String,
    #[prost(float, tag = "22")]
    pub canvas_width: f32,
    #[prost(float, tag = "23")]
    pub canvas_height: f32,
}

/// `virtual/page/pb/*` — repeated, one [`VirtualPage`] per tile.
#[derive(Clone, PartialEq, Message)]
pub struct VirtualPageContainer {
    #[prost(message, repeated, tag = "1")]
    pub virtual_page: Vec<VirtualPage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct VirtualPage {
    #[prost(string, tag = "1")]
    pub page_uuid: String,
    /// Global placement rect of this tile on the infinite canvas (JSON).
    #[prost(string, tag = "6")]
    pub dimensions_json: String,
    /// Not consumed (per-tile geo_layout blob; the format gate reads
    /// `virtual/doc` instead).
    #[prost(string, tag = "9")]
    pub geo_layout: String,
    /// Not consumed (template/background path; we don't render backgrounds).
    #[prost(string, tag = "10")]
    pub template_path: String,
}

/// `virtual/doc/pb/*` — used to detect the geo_layout (infinite note) format.
#[derive(Clone, PartialEq, Message)]
pub struct VirtualDoc {
    #[prost(string, tag = "9")]
    pub content_json: String,
}

/// `pageModel/pb/*` — repeated container of per-tile layer/dimension metadata.
#[derive(Clone, PartialEq, Message)]
pub struct PageModelContainer {
    #[prost(message, repeated, tag = "1")]
    pub page_model: Vec<PageModel>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PageModel {
    #[prost(string, tag = "1")]
    pub page_uuid: String,
    #[prost(string, tag = "2")]
    pub layers_json: String,
    /// Not consumed (tile dimensions; placement comes from `virtual/page`).
    #[prost(string, tag = "7")]
    pub dimensions_json: String,
}

/// Inner payload of a `shape/*.zip` — repeated shapes (strokes/geometry/text).
#[derive(Clone, PartialEq, Message)]
pub struct ShapeContainer {
    #[prost(message, repeated, tag = "1")]
    pub shapes: Vec<Shape>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Shape {
    #[prost(string, tag = "1")]
    pub stroke_uuid: String,
    #[prost(uint64, tag = "2")]
    pub created: u64,
    /// ARGB color stored as a signed integer.
    #[prost(int64, tag = "4")]
    pub color: i64,
    #[prost(float, tag = "5")]
    pub stroke_width: f32,
    /// Layer id this shape belongs to.
    #[prost(uint32, tag = "6")]
    pub layer_id: u32,
    #[prost(string, tag = "7")]
    pub bbox_json: String,
    /// 3x3 affine matrix JSON (`{"values":[a,b,tx,c,d,ty,0,0,1]}`).
    #[prost(string, tag = "8")]
    pub matrix_json: String,
    /// Text-box style JSON (font size, alignment, border…). pen_type 6/16.
    #[prost(string, tag = "9")]
    pub text_style_json: String,
    /// Plain text content of a text box. pen_type 6/16.
    #[prost(string, tag = "10")]
    pub text_plain: String,
    #[prost(string, tag = "11")]
    pub render_scale_json: String,
    /// Image resource descriptor JSON (relative path…). pen_type 19.
    #[prost(string, tag = "14")]
    pub image_info_json: String,
    /// Pen/style type key.
    #[prost(int64, tag = "12")]
    pub pen_type: i64,
    #[prost(string, tag = "16")]
    pub points_uuid: String,
    #[prost(string, tag = "17")]
    pub line_style_json: String,
    /// Not consumed (groups shapes together; we draw them individually).
    #[prost(string, tag = "18")]
    pub shape_group_uuid: String,
    /// GeoJSON feature payload (lines/shapes drawn with the shape tool).
    #[prost(string, tag = "20")]
    pub extra_json: String,
    /// Rich-text HTML for text boxes.
    #[prost(string, tag = "22")]
    pub rich_text: String,
    /// Not consumed: a binary vertex list for shape-tool geometry, superseded by
    /// the `extra_json` GeoJSON we actually parse. Kept to document field 25.
    #[prost(bytes = "vec", tag = "25")]
    pub point_list: Vec<u8>,
}

/// Decode a length-delimited protobuf message from a byte slice.
pub fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M, prost::DecodeError> {
    M::decode(bytes)
}
