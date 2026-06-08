//! Reassemble a `.note` into a flat, render-ready model.
//!
//! Infinite notes store content as a grid of fixed-size tiles, each with
//! tile-local stroke coordinates plus a global placement rect. This module
//! resolves pen widths/colors, links shapes to their stroke points, translates
//! every stroke into a single global coordinate space, and computes the content
//! bounding box (trimming empty area).

use std::collections::HashMap;

use crate::container::{self, Container};
use crate::error::{Error, Result};
use crate::ids;
use crate::json_meta::{self, Layer, PenSettings, Rect};
use crate::points::PointsFile;
use crate::proto::{
    self, NoteInfo, PageModelContainer, ShapeContainer, VirtualDoc, VirtualPageContainer,
};

/// A single stroke sample point. Re-exported because it appears in the public
/// [`RenderStroke::points`]; the rest of `points` (the binary parser) is
/// internal.
pub use crate::points::Point;

mod bounds;
mod geo;
mod stroke;
mod text;

/// RGBA color, components 0..=255 except alpha which is 0.0..=1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

/// A single stroke in global coordinates, ready to draw.
#[derive(Debug, Clone)]
pub struct RenderStroke {
    pub points: Vec<Point>,
    pub width: f32,
    pub color: Rgba,
    pub pen_type: i64,
    /// Charcoal (pen 22) only: the `penAttrs.texture` attribute (1 = dense V1
    /// grain, 2 = V2 grain that breaks up at light pressure). Defaults to 1.
    pub charcoal_texture: u8,
    /// Marker (pen 15) only, set by `Document::flatten_markers` (`--flat-marker`):
    /// draw the stroke as a single filled capsule union instead of a stroked
    /// polyline so it composites once in every viewer. Per-stroke 127/255 alpha
    /// + Multiply are kept. See `render/stroke.rs::draw_marker`.
    pub flat_marker: bool,
}

/// A run of text sharing one style (from the rich-text HTML spans).
#[derive(Debug, Clone)]
pub struct StyledRun {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A text box, placed by its (global) bounding rect. The content is a sequence
/// of styled runs (`\n` in a run's text is a hard line break).
#[derive(Debug, Clone)]
pub struct RenderText {
    pub runs: Vec<StyledRun>,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub font_size: f32,
    pub color: Rgba,
    /// 0 = left, 1 = center, 2 = right.
    pub align: u8,
    pub line_spacing: f32,
    /// Font family the note requested (from the rich-text `<font face>`), used to
    /// resolve a host font by name.
    pub font_name: Option<String>,
    /// The BOOX device font path (`text_style.fontFace`), a resolution hint.
    pub font_hint: Option<std::path::PathBuf>,
}

/// An embedded raster image, placed by its (global) bounding rect.
#[derive(Debug, Clone)]
pub struct RenderImage {
    pub bytes: Vec<u8>,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// A single sub-path of shape-tool geometry, in global coordinates.
#[derive(Debug, Clone)]
pub struct GeoPath {
    pub points: Vec<(f32, f32)>,
    pub closed: bool,
    pub fill: Option<Rgba>,
    pub stroke: Option<(Rgba, f32)>,
    /// Dash pattern (on/off lengths in PDF pt) when the edge is dashed; `None` =
    /// solid. Derived from the GeoJSON `lineStyle.dashLineIntervals`, scaled by
    /// the shape's matrix.
    pub dash: Option<Vec<f32>>,
}

/// Shape-tool geometry (lines / polygons drawn with the shape tool), pen 40.
#[derive(Debug, Clone)]
pub struct RenderGeo {
    pub paths: Vec<GeoPath>,
}

/// One drawable item. Marked non-exhaustive: supporting a new BOOX entity kind
/// adds a variant, so external `match`es must keep a wildcard arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RenderItem {
    Stroke(RenderStroke),
    Text(RenderText),
    Image(RenderImage),
    Geometry(RenderGeo),
}

/// One reconstructed note: a single giant canvas trimmed to its content.
#[derive(Debug, Clone)]
pub struct Canvas {
    /// Page size in note points (== PDF points).
    pub width: f32,
    pub height: f32,
    /// Global-space origin (min x/y) subtracted when placing items.
    pub origin_x: f32,
    pub origin_y: f32,
    /// Items in draw order (already sorted: layer, then translucent-before-opaque).
    pub items: Vec<RenderItem>,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub canvases: Vec<Canvas>,
}

/// How to lay out a multi-page infinite note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Layout {
    /// One PDF page per note-page, at the native page size (default; matches
    /// BOOX's export).
    PerPage,
    /// All pages composited onto one giant canvas via their tile placement rects,
    /// trimmed to content.
    SingleCanvas,
}

const MARGIN: f32 = 24.0;
const PEN_TYPE_HIGHLIGHTER: i64 = 15;
// NOTE: BOOX's PDF export draws markers at alpha 127/255 (the exported
// ExtGState is 0.49803922) — not 128/255 and not a round 0.5.
const HIGHLIGHTER_ALPHA: f32 = 127.0 / 255.0;

impl Document {
    pub fn load<R: std::io::Read + std::io::Seek>(reader: R, layout: Layout) -> Result<Self> {
        let container = Container::open(reader)?;
        let prefixes = find_note_prefixes(&container);
        if prefixes.is_empty() {
            return Err(Error::NotANote);
        }
        let mut canvases = Vec::new();
        for prefix in prefixes {
            canvases.extend(assemble_note(&container, &prefix, layout)?);
        }
        Ok(Self { canvases })
    }

    /// `--flat-marker`: keep each marker stroke (pen 15) translucent and
    /// Multiply-blended, but make it composite **exactly once** in every viewer
    /// (a filled capsule union instead of a stroked polyline).
    ///
    /// NOTE: the default output (BOOX's construct — a single long
    /// translucent stroked path) is chunked by some viewers (Quartz/Preview)
    /// into separately-composited passes, producing dark self-crossing bands.
    /// A flattened stroke can't darken against itself; separate strokes still
    /// darken/multiply where they overlap.
    pub fn flatten_markers(&mut self) {
        for canvas in &mut self.canvases {
            for item in &mut canvas.items {
                if let RenderItem::Stroke(s) = item
                    && s.pen_type == PEN_TYPE_HIGHLIGHTER
                {
                    s.flat_marker = true;
                }
            }
        }
    }
}

/// Locate each note's path prefix by finding every `.../note/pb/note_info`.
fn find_note_prefixes(container: &Container) -> Vec<String> {
    const SUFFIX: &str = "note/pb/note_info";
    let mut prefixes: Vec<String> = container
        .paths()
        .filter_map(|p| p.strip_suffix(SUFFIX).map(str::to_string))
        .collect();
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

/// A shape plus the page (tile) it belongs to and its file ordering.
struct ShapeEntry {
    shape: proto::Shape,
    page_id: String,
    timestamp: u64,
    order: usize,
}

/// Everything parsed from one note, shared across layout modes.
struct NoteData<'a> {
    container: &'a Container,
    prefix: &'a str,
    pen: PenSettings,
    tiles: HashMap<String, Rect>,
    layers_by_page: HashMap<String, Vec<Layer>>,
    strokes: HashMap<String, crate::points::Stroke>,
    shape_entries: Vec<ShapeEntry>,
    last_index: HashMap<String, usize>,
    page_order: Vec<String>,
    canvas_w: f32,
    canvas_h: f32,
}

fn assemble_note(container: &Container, prefix: &str, layout: Layout) -> Result<Vec<Canvas>> {
    let note_info: NoteInfo = proto::decode(
        container
            .get(&format!("{prefix}note/pb/note_info"))
            .expect("prefix derived from this entry"),
    )
    .map_err(|e| Error::Malformed {
        what: format!("{prefix}note/pb/note_info"),
        detail: e.to_string(),
    })?;
    let meta = note_info.notes.first();
    let pen: PenSettings = meta
        .and_then(|m| json_meta::parse_opt(&m.pen_settings_json))
        .unwrap_or_default();
    let page_order: Vec<String> = meta
        .and_then(|m| json_meta::parse_opt::<json_meta::PageNameList>(&m.active_pages_json))
        .map(|p| p.page_name_list.iter().map(|u| ids::normalize(u)).collect())
        .unwrap_or_default();
    let canvas_w = meta
        .map(|m| m.canvas_width)
        .filter(|w| *w > 1.0)
        .unwrap_or(1860.0);
    let canvas_h = meta
        .map(|m| m.canvas_height)
        .filter(|h| *h > 1.0)
        .unwrap_or(2480.0);

    require_geo_layout(container, prefix)?;

    let tiles = read_tiles(container, prefix);
    let layers_by_page = read_layers(container, prefix);
    let strokes = read_all_strokes(container, prefix);
    let mut shape_entries = read_shapes(container, prefix);
    shape_entries.sort_by_key(|e| (e.timestamp, e.order));
    let last_index = latest_shape_index(&shape_entries);

    let data = NoteData {
        container,
        prefix,
        pen,
        tiles,
        layers_by_page,
        strokes,
        shape_entries,
        last_index,
        page_order,
        canvas_w,
        canvas_h,
    };

    match layout {
        Layout::SingleCanvas => Ok(vec![assemble_single_canvas(&data)]),
        Layout::PerPage => Ok(assemble_per_page(&data)),
    }
}

/// Composite every tile onto one canvas (tile origins applied), trimmed to content.
fn assemble_single_canvas(data: &NoteData) -> Canvas {
    let items = build_items(data, None, true);
    if items.is_empty() {
        return blank_canvas();
    }
    // Trim the canvas to the content's visual bounds (only this composite
    // layout needs them; per-page canvases are fixed at the tile size).
    let mut bbox = bounds::BBox::default();
    for item in &items {
        bounds::expand_bounds(&mut bbox, item);
    }
    Canvas {
        width: (bbox.max_x - bbox.min_x) + 2.0 * MARGIN,
        height: (bbox.max_y - bbox.min_y) + 2.0 * MARGIN,
        origin_x: bbox.min_x - MARGIN,
        origin_y: bbox.min_y - MARGIN,
        items,
    }
}

/// One canvas per note-page (page-local coords), in `pageNameList` order — with
/// any page that has content but is missing from that list appended after
/// (see `per_page_order`) — at the native page size, matching BOOX's export.
fn assemble_per_page(data: &NoteData) -> Vec<Canvas> {
    let order = bounds::per_page_order(&data.page_order, &data.shape_entries);

    let mut canvases = Vec::new();
    for page_id in &order {
        let items = build_items(data, Some(page_id), false);
        // Page size = the page's own tile dimensions (the drawing area, always
        // ~1860x2480). Note that note_info's canvas size can differ (e.g. report
        // the BOOX device's landscape orientation), so don't use it here.
        let (pw, ph) = data
            .tiles
            .get(page_id)
            .map(|t| (t.width(), t.height()))
            .filter(|(w, h)| *w > 1.0 && *h > 1.0)
            .unwrap_or((data.canvas_w, data.canvas_h));
        canvases.push(Canvas {
            width: pw,
            height: ph,
            origin_x: 0.0,
            origin_y: 0.0,
            items,
        });
    }
    if canvases.is_empty() {
        canvases.push(blank_canvas());
    }
    canvases
}

fn blank_canvas() -> Canvas {
    Canvas {
        width: 200.0,
        height: 200.0,
        origin_x: 0.0,
        origin_y: 0.0,
        items: Vec::new(),
    }
}

/// Map each non-empty `stroke_uuid` to the index of its latest shape, so
/// `build_items` can drop superseded revisions. Shapes that leave `stroke_uuid`
/// empty (text/image/geo may) are excluded: they would otherwise all collapse
/// onto the single empty key, dropping every such shape but the last.
fn latest_shape_index(entries: &[ShapeEntry]) -> HashMap<String, usize> {
    let mut last_index: HashMap<String, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let sid = ids::normalize(&e.shape.stroke_uuid);
        if !sid.is_empty() {
            last_index.insert(sid, i);
        }
    }
    last_index
}

/// Build one canvas's render items, already in draw order (layer, then
/// translucent-before-opaque, then creation time). `filter_page` limits to one
/// tile; `use_tile_origin` adds the tile's global placement (composite) vs.
/// page-local.
///
/// This stays thin: it does the common per-shape filtering (superseded
/// revisions, page filter, missing tile, hidden layers), dispatches to a
/// per-kind `build_*` helper, then applies the sort key uniformly. Per-kind
/// geometry lives in the helpers; per-kind bounds live in `expand_bounds`.
fn build_items(
    data: &NoteData,
    filter_page: Option<&str>,
    use_tile_origin: bool,
) -> Vec<RenderItem> {
    let mut staged: Vec<(SortKey, RenderItem)> = Vec::new();
    for (i, entry) in data.shape_entries.iter().enumerate() {
        let shape = &entry.shape;
        let sid = ids::normalize(&shape.stroke_uuid);
        if !sid.is_empty() && data.last_index.get(&sid) != Some(&i) {
            continue; // superseded revision
        }
        if let Some(fp) = filter_page
            && entry.page_id != fp
        {
            continue;
        }
        let Some(tile) = data.tiles.get(&entry.page_id) else {
            continue;
        };
        if !layer_visible(&data.layers_by_page, &entry.page_id, shape.layer_id) {
            continue;
        }
        let (ox, oy) = if use_tile_origin {
            (tile.left, tile.top)
        } else {
            (0.0, 0.0)
        };

        let item = match shape.pen_type {
            19 => build_image(data.container, data.prefix, shape, ox, oy),
            40 => geo::build_geo(shape, ox, oy),
            // Scanline fill: stroke points come in pairs, each the opposite
            // corners of a horizontal fill rectangle. (No bundled example.)
            37 => data
                .strokes
                .get(&sid)
                .and_then(|stroke| geo::build_scanline_fill(shape, stroke, ox, oy)),
            6 | 16 => text::build_text(shape, ox, oy),
            _ => stroke::build_stroke(data, shape, &sid, tile, ox, oy),
        };
        let Some(item) = item else {
            continue;
        };

        // Translucent items (markers, translucent strokes) sort before opaque
        // ones within a layer; everything else is opaque.
        let alpha = match &item {
            RenderItem::Stroke(s) if s.color.a < 1.0 => 0,
            _ => 1,
        };
        staged.push((
            SortKey {
                layer: layer_rank(&data.layers_by_page, &entry.page_id, shape.layer_id),
                alpha,
                created: shape.created,
                order: entry.order,
            },
            item,
        ));
    }
    staged.sort_by(|a, b| a.0.cmp(&b.0));
    staged.into_iter().map(|(_, item)| item).collect()
}

/// Paint order key. The derived `Ord` compares fields top-to-bottom, so the
/// field order *is* the sort priority: layer, then translucent-before-opaque,
/// then creation time, then the original stroke order.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct SortKey {
    layer: usize,
    alpha: u8,
    created: u64,
    order: usize,
}

fn require_geo_layout(container: &Container, prefix: &str) -> Result<()> {
    let dir = format!("{prefix}virtual/doc/pb/");
    for path in container.entries_with_prefix(&dir) {
        let Some(bytes) = container.get(path) else {
            continue;
        };
        let Ok(doc) = proto::decode::<VirtualDoc>(bytes) else {
            continue;
        };
        if let Some(content) = json_meta::parse_opt::<json_meta::DocContent>(&doc.content_json) {
            if content.content_type == "geo_layout" {
                return Ok(());
            }
            return Err(Error::UnsupportedFormat {
                content_type: content.content_type,
            });
        }
    }
    Err(Error::UnknownFormat)
}

fn read_tiles(container: &Container, prefix: &str) -> HashMap<String, Rect> {
    let dir = format!("{prefix}virtual/page/pb/");
    let mut tiles = HashMap::new();
    for path in container.entries_with_prefix(&dir) {
        let Some(bytes) = container.get(path) else {
            continue;
        };
        // A malformed tile container loses its tiles (shapes on them are
        // skipped via the missing-tile guard), but never aborts the note.
        let vpc: VirtualPageContainer = match proto::decode(bytes) {
            Ok(vpc) => vpc,
            Err(e) => {
                log::warn!("{path}: virtual page decode failed: {e}");
                continue;
            }
        };
        for vp in vpc.virtual_page {
            if let Some(rect) = json_meta::parse_opt::<Rect>(&vp.dimensions_json) {
                tiles.insert(ids::normalize(&vp.page_uuid), rect);
            }
        }
    }
    tiles
}

fn read_layers(container: &Container, prefix: &str) -> HashMap<String, Vec<Layer>> {
    let dir = format!("{prefix}pageModel/pb/");
    let mut out = HashMap::new();
    for path in container.entries_with_prefix(&dir) {
        let Some(bytes) = container.get(path) else {
            continue;
        };
        let Ok(pmc) = proto::decode::<PageModelContainer>(bytes) else {
            continue;
        };
        for pm in pmc.page_model {
            if let Some(layers) =
                json_meta::parse_opt::<json_meta::PageModelLayers>(&pm.layers_json)
            {
                out.insert(ids::normalize(&pm.page_uuid), layers.layer_list);
            }
        }
    }
    out
}

fn read_all_strokes(container: &Container, prefix: &str) -> HashMap<String, crate::points::Stroke> {
    let point_root = format!("{prefix}point/");
    let mut strokes = HashMap::new();
    for path in container
        .entries_with_prefix(&point_root)
        .filter(|p| p.ends_with("#points"))
    {
        let Some(bytes) = container.get(path) else {
            continue;
        };
        match PointsFile::parse(bytes) {
            Ok(pf) => {
                for (id, stroke) in pf.strokes {
                    strokes.insert(id, stroke);
                }
            }
            Err(e) => log::warn!("failed to parse {path}: {e}"),
        }
    }
    strokes
}

fn read_shapes(container: &Container, prefix: &str) -> Vec<ShapeEntry> {
    let dir = format!("{prefix}shape/");
    let mut entries = Vec::new();
    let mut order = 0usize;
    for path in container
        .entries_with_prefix(&dir)
        .filter(|p| p.ends_with(".zip"))
    {
        let file_name = path.rsplit('/').next().unwrap_or(path);
        let (page_id, timestamp) = match parse_shape_file_name(file_name) {
            Some(v) => v,
            None => continue,
        };
        let Some(bytes) = container.get(path) else {
            continue;
        };
        let inner = match container::read_nested_zip_single(bytes) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("{path}: {e}");
                continue;
            }
        };
        let sc: ShapeContainer = match proto::decode(&inner) {
            Ok(sc) => sc,
            Err(e) => {
                log::warn!("{path}: shape decode failed: {e}");
                continue;
            }
        };
        for shape in sc.shapes {
            entries.push(ShapeEntry {
                shape,
                page_id: page_id.clone(),
                timestamp,
                order,
            });
            order += 1;
        }
    }
    entries
}

/// `<page_id>#<group_id>#<timestamp>.zip` -> (normalized page_id, timestamp).
fn parse_shape_file_name(name: &str) -> Option<(String, u64)> {
    let mut parts = name.split('#');
    let page_id = parts.next()?;
    let _group = parts.next()?;
    let ts = parts.next()?.strip_suffix(".zip")?;
    Some((ids::normalize(page_id), ts.parse().ok()?))
}

fn argb_to_rgba(argb: u32) -> Rgba {
    let a = ((argb >> 24) & 0xff) as f32 / 255.0;
    Rgba {
        r: ((argb >> 16) & 0xff) as u8,
        g: ((argb >> 8) & 0xff) as u8,
        b: (argb & 0xff) as u8,
        // NOTE: many strokes store alpha 0 meaning "opaque"; treat 0 as opaque.
        a: if a == 0.0 { 1.0 } else { a },
    }
}

fn layer_rank(layers_by_page: &HashMap<String, Vec<Layer>>, page_id: &str, layer_id: u32) -> usize {
    layers_by_page
        .get(page_id)
        .and_then(|layers| layers.iter().position(|l| l.id == layer_id))
        .unwrap_or(layer_id as usize)
}

fn layer_visible(
    layers_by_page: &HashMap<String, Vec<Layer>>,
    page_id: &str,
    layer_id: u32,
) -> bool {
    layers_by_page
        .get(page_id)
        .and_then(|layers| layers.iter().find(|l| l.id == layer_id))
        .map(|l| l.show)
        .unwrap_or(true)
}

fn build_image(
    container: &Container,
    prefix: &str,
    shape: &proto::Shape,
    ox: f32,
    oy: f32,
) -> Option<RenderItem> {
    let info: json_meta::ImageInfo = json_meta::parse_opt(&shape.image_info_json)?;
    let raw = if !info.relative_path.is_empty() {
        &info.relative_path
    } else {
        &info.local_path
    };
    let file = raw.rsplit('/').next().filter(|s| !s.is_empty())?;
    let bytes = container.get(&format!("{prefix}resource/data/{file}"))?;
    let rect: Rect = json_meta::parse_opt(&shape.bbox_json)?;
    Some(RenderItem::Image(RenderImage {
        bytes: bytes.to_vec(),
        left: rect.left + ox,
        top: rect.top + oy,
        right: rect.right + ox,
        bottom: rect.bottom + oy,
    }))
}

/// The stroke shape's affine matrix `[a, b, tx, c, d, ty]` from field 8, used to
/// place/scale the raw points (identity when absent). Content the user moves or
/// resizes carries a non-identity matrix; ignoring it puts strokes in the wrong
/// place and at the wrong size.
fn stroke_matrix(shape: &proto::Shape) -> [f32; 6] {
    json_meta::parse_opt::<json_meta::MatrixValues>(&shape.matrix_json)
        .filter(|m| m.values.len() >= 6)
        .map(|m| {
            [
                m.values[0],
                m.values[1],
                m.values[2],
                m.values[3],
                m.values[4],
                m.values[5],
            ]
        })
        .unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape_entry_uuid(uuid: &str) -> ShapeEntry {
        ShapeEntry {
            shape: proto::Shape {
                stroke_uuid: uuid.to_string(),
                ..Default::default()
            },
            page_id: "p1".to_string(),
            timestamp: 0,
            order: 0,
        }
    }

    #[test]
    fn latest_shape_index_supersedes_by_uuid() {
        // Two revisions of the same stroke: only the later index survives.
        let entries = vec![shape_entry_uuid("aaaa"), shape_entry_uuid("aaaa")];
        let idx = latest_shape_index(&entries);
        assert_eq!(idx.get(&ids::normalize("aaaa")), Some(&1));
    }

    #[test]
    fn latest_shape_index_ignores_empty_uuid() {
        // Shapes without a stroke_uuid (text/image/geo) must not collapse onto
        // one another via the empty key — none of them are tracked here, so
        // build_items keeps them all.
        let entries = vec![
            shape_entry_uuid(""),
            shape_entry_uuid(""),
            shape_entry_uuid("bbbb"),
        ];
        let idx = latest_shape_index(&entries);
        assert!(!idx.contains_key(""));
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.get(&ids::normalize("bbbb")), Some(&2));
    }

    #[test]
    fn layer_visible_respects_hidden_layers_but_keeps_unknowns() {
        let mut layers_by_page = std::collections::HashMap::new();
        layers_by_page.insert(
            "p1".to_string(),
            vec![
                Layer {
                    id: 1,
                    show: true,
                    lock: false,
                },
                Layer {
                    id: 2,
                    show: false,
                    lock: false,
                },
            ],
        );
        assert!(layer_visible(&layers_by_page, "p1", 1));
        assert!(!layer_visible(&layers_by_page, "p1", 2));
        assert!(layer_visible(&layers_by_page, "p1", 99));
        assert!(layer_visible(&layers_by_page, "missing", 2));
    }

    #[test]
    fn stroke_matrix_defaults_to_identity() {
        let s = proto::Shape::default();
        assert_eq!(stroke_matrix(&s), [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        // Fewer than 6 values → still identity.
        let s = proto::Shape {
            matrix_json: r#"{"values":[2,0,0]}"#.to_string(),
            ..Default::default()
        };
        assert_eq!(stroke_matrix(&s), [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn stroke_matrix_reads_first_six_values() {
        let s = proto::Shape {
            matrix_json: r#"{"values":[1.5,0,30,0,1.5,40,0,0,1]}"#.to_string(),
            ..Default::default()
        };
        assert_eq!(stroke_matrix(&s), [1.5, 0.0, 30.0, 0.0, 1.5, 40.0]);
    }
}
