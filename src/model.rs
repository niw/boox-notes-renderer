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
    let mut last_index: HashMap<String, usize> = HashMap::new();
    for (i, e) in shape_entries.iter().enumerate() {
        last_index.insert(ids::normalize(&e.shape.stroke_uuid), i);
    }

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
    let mut bbox = BBox::default();
    for item in &items {
        expand_bounds(&mut bbox, item);
    }
    Canvas {
        width: (bbox.max_x - bbox.min_x) + 2.0 * MARGIN,
        height: (bbox.max_y - bbox.min_y) + 2.0 * MARGIN,
        origin_x: bbox.min_x - MARGIN,
        origin_y: bbox.min_y - MARGIN,
        items,
    }
}

/// One canvas per note-page (page-local coords), in `pageNameList` order, at the
/// native page size — matching BOOX's export.
fn assemble_per_page(data: &NoteData) -> Vec<Canvas> {
    let order = per_page_order(&data.page_order, &data.shape_entries);

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

/// Page canvases follow `pageNameList` exactly, including blank pages. Shape
/// files that reference pages missing from `pageNameList` are appended as a
/// compatibility fallback for malformed/incomplete archives.
fn per_page_order(page_order: &[String], shape_entries: &[ShapeEntry]) -> Vec<String> {
    let mut pages_with_content: Vec<String> = Vec::new();
    let mut seen_content = std::collections::HashSet::new();
    for e in shape_entries {
        if seen_content.insert(e.page_id.clone()) {
            pages_with_content.push(e.page_id.clone());
        }
    }

    let mut order = Vec::new();
    let mut seen_order = std::collections::HashSet::new();
    for p in page_order {
        if seen_order.insert(p.clone()) {
            order.push(p.clone());
        }
    }
    for p in pages_with_content {
        if seen_order.insert(p.clone()) {
            order.push(p);
        }
    }
    order
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
        if data.last_index.get(&sid) != Some(&i) {
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
            40 => build_geo(shape, ox, oy),
            // Scanline fill: stroke points come in pairs, each the opposite
            // corners of a horizontal fill rectangle. (No bundled example.)
            37 => data
                .strokes
                .get(&sid)
                .and_then(|stroke| build_scanline_fill(shape, stroke, ox, oy)),
            6 | 16 => build_text(shape, ox, oy),
            _ => build_stroke(data, shape, &sid, tile, ox, oy),
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

#[derive(PartialEq, Eq)]
struct SortKey {
    layer: usize,
    alpha: u8,
    created: u64,
    order: usize,
}
impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.layer
            .cmp(&other.layer)
            .then(self.alpha.cmp(&other.alpha))
            .then(self.created.cmp(&other.created))
            .then(self.order.cmp(&other.order))
    }
}
impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Build a handwriting-stroke item (the default pen-type arm). `None` when the
/// shape has no usable points or is a stray cross-tile duplicate.
fn build_stroke(
    data: &NoteData,
    shape: &proto::Shape,
    sid: &str,
    tile: &Rect,
    ox: f32,
    oy: f32,
) -> Option<RenderItem> {
    if shape.points_uuid.is_empty() {
        return None;
    }
    let stroke = data.strokes.get(sid)?;
    if stroke.points.is_empty() {
        return None;
    }
    // Skip stray boundary-crossing duplicates whose field-7 bbox lies entirely
    // outside their own tile (see `bbox_outside_tile`); they would leak into
    // neighbouring pages on a composited canvas.
    if bbox_outside_tile(shape, tile) {
        return None;
    }
    let color = resolve_color(shape, &data.pen);
    // Apply the shape's affine matrix (field 8) to the raw points, then fit the
    // result onto the shape's bbox (field 7), the authoritative tile-local
    // position (see `bbox_fit`).
    let m = stroke_matrix(shape);
    let mscale = (m[0] * m[4] - m[1] * m[3]).abs().sqrt();
    let tpts: Vec<(f32, f32)> = stroke
        .points
        .iter()
        .map(|p| {
            (
                m[0] * p.x + m[1] * p.y + m[2],
                m[3] * p.x + m[4] * p.y + m[5],
            )
        })
        .collect();
    let (fsx, fsy, ocx, ocy, icx, icy) = bbox_fit(shape, &tpts);
    let base = resolve_width(shape, &data.pen);
    let fit_scale = (fsx.abs() * fsy.abs()).sqrt();
    let width = base * (mscale * fit_scale).clamp(0.05, 20.0);
    let points: Vec<Point> = stroke
        .points
        .iter()
        .zip(&tpts)
        .map(|(p, &(tx, ty))| Point {
            x: (tx - icx) * fsx + ocx + ox,
            y: (ty - icy) * fsy + ocy + oy,
            ..*p
        })
        .collect();
    Some(RenderItem::Stroke(RenderStroke {
        points,
        width,
        color,
        pen_type: shape.pen_type,
        charcoal_texture: charcoal_texture(shape),
        flat_marker: false,
    }))
}

/// Expand `bbox` to include a built item's visual bounds — the single place that
/// knows each kind's extent. Strokes use the pen's visual padding
/// ([`crate::pen::stroke_visual_pad`]); geometry pads by half its stroke width;
/// text/image use their placement rect.
fn expand_bounds(bbox: &mut BBox, item: &RenderItem) {
    match item {
        RenderItem::Stroke(s) => {
            let pad = crate::pen::stroke_visual_pad(s.pen_type, s.width, &s.points);
            for p in &s.points {
                bbox.expand_padded(p.x, p.y, pad);
            }
        }
        RenderItem::Geometry(g) => {
            for path in &g.paths {
                let pad = path.stroke.map(|(_, w)| w / 2.0).unwrap_or(0.0);
                for &(x, y) in &path.points {
                    bbox.expand_padded(x, y, pad);
                }
            }
        }
        RenderItem::Text(t) => {
            bbox.expand(t.left, t.top);
            bbox.expand(t.right, t.bottom);
        }
        RenderItem::Image(img) => {
            bbox.expand(img.left, img.top);
            bbox.expand(img.right, img.bottom);
        }
    }
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

/// Charcoal (pen 22) `penAttrs.texture` from the shape's createArgs JSON (proto
/// field 11): `{"…","penAttrs":{"texture":2},…}` → 2 (V2); absent → 1 (V1).
fn charcoal_texture(shape: &proto::Shape) -> u8 {
    json_meta::parse_opt::<json_meta::CreateArgs>(&shape.render_scale_json)
        .map(|args| args.pen_attrs.texture)
        .unwrap_or(1)
}

fn resolve_width(shape: &proto::Shape, pen: &PenSettings) -> f32 {
    if shape.stroke_width.is_finite() && shape.stroke_width > 0.0 {
        return shape.stroke_width;
    }
    if let Some(w) = pen.pen_with_map.get(&shape.pen_type.to_string())
        && w.is_finite()
        && *w > 0.0
    {
        return *w;
    }
    1.0
}

fn resolve_color(shape: &proto::Shape, pen: &PenSettings) -> Rgba {
    let mut argb = shape.color as u32;
    if argb == 0 {
        // Fall back to the nearest quick-pen of the same type, then default fill.
        argb = pen
            .quick_pen_list
            .quick_pens
            .iter()
            .filter(|p| i64::from(p.type_) == shape.pen_type)
            .min_by(|a, b| {
                (a.width - shape.stroke_width)
                    .abs()
                    .total_cmp(&(b.width - shape.stroke_width).abs())
            })
            .map(|p| p.color as u32)
            .unwrap_or(pen.fill_color as u32);
    }
    let mut rgba = argb_to_rgba(argb);
    if shape.pen_type == PEN_TYPE_HIGHLIGHTER {
        rgba.a = HIGHLIGHTER_ALPHA;
    }
    rgba
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

/// Running content bounding box in global coordinates.
struct BBox {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}
impl Default for BBox {
    fn default() -> Self {
        Self {
            min_x: f32::INFINITY,
            min_y: f32::INFINITY,
            max_x: f32::NEG_INFINITY,
            max_y: f32::NEG_INFINITY,
        }
    }
}
impl BBox {
    fn expand(&mut self, x: f32, y: f32) {
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    fn expand_padded(&mut self, x: f32, y: f32, pad: f32) {
        let pad = pad.max(0.0);
        self.expand(x - pad, y - pad);
        self.expand(x + pad, y + pad);
    }
}

fn build_text(shape: &proto::Shape, ox: f32, oy: f32) -> Option<RenderItem> {
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

/// Parse a pen-40 shape's GeoJSON `featureCollection` into transformed paths.
///
/// Coordinates are in shape-local space; we apply the shape's affine matrix
/// (field 8) and then the tile's global origin. Stroke width is scaled by the
/// matrix's linear scale so it matches the appearance on the BOOX device.
fn build_geo(shape: &proto::Shape, ox: f32, oy: f32) -> Option<RenderItem> {
    let outer: serde_json::Value = serde_json::from_str(&shape.extra_json).ok()?;
    let fc_str = outer.get("featureCollection")?.as_str()?;
    let fc: serde_json::Value = serde_json::from_str(fc_str).ok()?;
    let features = fc.get("features")?.as_array()?;

    // Affine matrix [a,b,tx,c,d,ty] from the 3x3 values (identity when absent).
    let m = stroke_matrix(shape);
    let scale = (m[0] * m[4] - m[1] * m[3]).abs().sqrt().max(0.01);
    let tx = |x: f32, y: f32| -> (f32, f32) {
        (
            m[0] * x + m[1] * y + m[2] + ox,
            m[3] * x + m[4] * y + m[5] + oy,
        )
    };

    // A whole-shape dash can live in the protobuf field-17 `line_style_json`
    // (e.g. dashed hexagons), separate from any per-feature GeoJSON `lineStyle`.
    // Feed it as the root inherited dash; per-feature styles still override it.
    let shape_dash = shape_line_style_dash(&shape.line_style_json);

    let mut paths = Vec::new();
    for feature in features {
        collect_geo_feature(feature, &tx, scale, &mut paths, shape_dash.as_deref());
    }

    if paths.is_empty() {
        None
    } else {
        Some(RenderItem::Geometry(RenderGeo { paths }))
    }
}

/// Dash intervals from a shape's field-17 `line_style_json`
/// (`{"lineStyle":{"dashLineIntervals":[…],"type":1}}`), or `None` if solid.
fn shape_line_style_dash(json: &str) -> Option<Vec<f64>> {
    let ls = json_meta::parse_opt::<json_meta::LineStyleWrap>(json)?.line_style?;
    if ls.type_ == 0 {
        return None;
    }
    (ls.dash_line_intervals.len() >= 2).then_some(ls.dash_line_intervals)
}

/// Build filled rectangles from a scanline-fill (pen 37) stroke: its points come
/// in pairs, each pair the opposite corners of one horizontal fill span.
/// Transformed by the shape's field-8 matrix. (Untested — no example uses it.)
fn build_scanline_fill(
    shape: &proto::Shape,
    stroke: &crate::points::Stroke,
    ox: f32,
    oy: f32,
) -> Option<RenderItem> {
    let m = stroke_matrix(shape);
    let tx = |x: f32, y: f32| {
        (
            m[0] * x + m[1] * y + m[2] + ox,
            m[3] * x + m[4] * y + m[5] + oy,
        )
    };
    let color = argb_to_rgba(shape.color as u32);
    let pts = &stroke.points;
    let mut paths = Vec::new();
    let mut i = 0;
    while i + 1 < pts.len() {
        let (a, b) = (&pts[i], &pts[i + 1]);
        paths.push(GeoPath {
            points: vec![tx(a.x, a.y), tx(b.x, a.y), tx(b.x, b.y), tx(a.x, b.y)],
            closed: true,
            fill: Some(color),
            stroke: None,
            dash: None,
        });
        i += 2;
    }
    (!paths.is_empty()).then_some(RenderItem::Geometry(RenderGeo { paths }))
}

/// Turn one GeoJSON feature into `GeoPath`s, recursing into grouped shapes.
///
/// Shape-tool groups (e.g. a 3-D box drawn as several edges) nest a whole
/// `FeatureCollection` under a feature's `geometry`, so the real `LineString`
/// /`Polygon` leaves live one or more levels down. Walk into any node that
/// carries a `features` array; only nodes with `coordinates` are leaves.
fn collect_geo_feature(
    feature: &serde_json::Value,
    tx: &impl Fn(f32, f32) -> (f32, f32),
    scale: f32,
    paths: &mut Vec<GeoPath>,
    inherited_dash: Option<&[f64]>,
) {
    // `dashLineIntervals` may sit under `properties.lineStyle` or directly under
    // the feature, and a grouped shape often carries it on the parent so the leaf
    // LineStrings inherit it. Track the nearest one down the tree.
    let node_dash: Option<Vec<f64>> = feature
        .get("properties")
        .and_then(|p| p.get("lineStyle"))
        .or_else(|| feature.get("lineStyle"))
        .and_then(|ls| ls.get("dashLineIntervals"))
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_f64()).collect::<Vec<f64>>())
        .filter(|v| v.len() >= 2);
    let dash_raw: Option<&[f64]> = node_dash.as_deref().or(inherited_dash);

    // A node may be a FeatureCollection itself …
    if let Some(features) = feature.get("features").and_then(|f| f.as_array()) {
        for f in features {
            collect_geo_feature(f, tx, scale, paths, dash_raw);
        }
        return;
    }
    let Some(geom) = feature.get("geometry") else {
        return;
    };
    // … or its geometry may be a nested FeatureCollection (the grouped case).
    if geom.get("features").is_some() {
        collect_geo_feature(geom, tx, scale, paths, dash_raw);
        return;
    }

    let geo_type = geom.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let coords = geom.get("coordinates");
    let props = feature.get("properties");

    let stroke = props.and_then(|p| p.get("strokeAttr")).map(|s| {
        let color = s.get("color").and_then(|c| c.as_i64()).unwrap_or(-16777216);
        let width = s.get("width").and_then(|w| w.as_f64()).unwrap_or(2.0) as f32;
        (argb_to_rgba(color as u32), (width * scale).max(0.3))
    });
    let fill = props
        .and_then(|p| p.get("fillAttr"))
        .filter(|f| {
            f.get("enableColor")
                .and_then(|e| e.as_bool())
                .unwrap_or(false)
        })
        .map(|f| argb_to_rgba(f.get("color").and_then(|c| c.as_i64()).unwrap_or(0) as u32));

    // Scale the dash on/off lengths by the matrix and pad by ~2x the stroke width
    // (matching BOOX's export), so the dashes read at the rendered line weight.
    let dash = dash_raw.map(|raw| {
        let w = stroke.map(|(_, w)| w).unwrap_or(1.0);
        raw.iter()
            .map(|v| (*v as f32 * scale + w * 2.0).max(0.1))
            .collect::<Vec<f32>>()
    });

    let subtype = props
        .and_then(|p| p.get("subType"))
        .and_then(|s| s.as_str())
        .unwrap_or("");

    // Raw (untransformed) coordinate points, for shapes computed in local space.
    let raw: Vec<(f32, f32)> = coords
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(xy).collect())
        .unwrap_or_default();
    let scolor = stroke.map(|(c, _)| c);
    let sw = stroke.map(|(_, w)| w).unwrap_or(2.0);
    let mut emit = |points: Vec<(f32, f32)>, closed: bool, fill: Option<Rgba>| {
        if points.len() >= 2 {
            paths.push(GeoPath {
                points,
                closed,
                fill,
                stroke,
                dash: dash.clone(),
            });
        }
    };

    match (geo_type, subtype) {
        ("LineString", "WaveLine") => emit(wave_points(coords, props, tx), false, None),
        ("Polygon", _) => emit(edge_points(coords, tx), true, fill),
        ("MultiPoint", "Curve") if raw.len() >= 3 => emit(
            sample_quadratic(raw[0], raw[1], raw[2], 16, tx),
            false,
            None,
        ),
        ("MultiPoint", "Arc") if raw.len() >= 3 => {
            emit(arc_points(raw[0], raw[1], raw[2], tx), fill.is_some(), fill)
        }
        ("MultiPoint", "Bracket") if raw.len() >= 3 => {
            for arm in bracket_arms(raw[0], raw[1], raw[2], tx) {
                emit(arm, false, None);
            }
        }
        // Ellipse: explicit "Oval", or any other MultiPoint (bbox-corner ellipse).
        ("MultiPoint", _) => emit(ellipse_points(coords, tx), true, fill),
        ("MultiLineString", _) => {
            if let Some(segs) = coords.and_then(|c| c.as_array()) {
                for seg in segs {
                    emit(coord_points(Some(seg), tx), false, None);
                }
            }
        }
        ("DirectionLine", _) | ("BidirectionalLine", _) => {
            let line = coord_points(coords, tx);
            if let (Some(&a), Some(&b)) = (line.first(), line.last()) {
                emit(line.clone(), false, None);
                if let Some(c) = scolor {
                    emit(arrowhead(a, b, sw), true, Some(c));
                    if geo_type == "BidirectionalLine" {
                        emit(arrowhead(b, a, sw), true, Some(c));
                    }
                }
            }
        }
        ("LineString", _) => emit(coord_points(coords, tx), false, None),
        // Best-effort: nested arrays => edge ring, flat => polyline.
        _ => {
            let nested = coords
                .and_then(|c| c.as_array())
                .and_then(|p| p.first())
                .and_then(|e| e.as_array())
                .and_then(|e| e.first())
                .map(|x| x.is_array())
                .unwrap_or(false);
            if nested {
                emit(edge_points(coords, tx), true, fill);
            } else {
                emit(coord_points(coords, tx), false, None);
            }
        }
    }
}

/// Arrowhead triangle at `b` (pointing along `a`→`b`), in transformed coords.
fn arrowhead(a: (f32, f32), b: (f32, f32), width: f32) -> Vec<(f32, f32)> {
    let ang = (b.1 - a.1).atan2(b.0 - a.0);
    let hl = (width * 2.0).max(8.0);
    let sp = 0.5;
    vec![
        b,
        (b.0 - hl * (ang - sp).cos(), b.1 - hl * (ang - sp).sin()),
        (b.0 - hl * (ang + sp).cos(), b.1 - hl * (ang + sp).sin()),
    ]
}

/// Sample a quadratic Bézier (`p0`,control `c`,`p1`) in local space → transformed.
fn sample_quadratic(
    p0: (f32, f32),
    c: (f32, f32),
    p1: (f32, f32),
    steps: usize,
    tx: &impl Fn(f32, f32) -> (f32, f32),
) -> Vec<(f32, f32)> {
    (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let mt = 1.0 - t;
            let x = mt * mt * p0.0 + 2.0 * mt * t * c.0 + t * t * p1.0;
            let y = mt * mt * p0.1 + 2.0 * mt * t * c.1 + t * t * p1.1;
            tx(x, y)
        })
        .collect()
}

/// Elliptical arc within the bbox (`min`,`max`) over `angles=(startDeg,sweepDeg)`.
fn arc_points(
    min: (f32, f32),
    max: (f32, f32),
    angles: (f32, f32),
    tx: &impl Fn(f32, f32) -> (f32, f32),
) -> Vec<(f32, f32)> {
    let (cx, cy) = ((min.0 + max.0) / 2.0, (min.1 + max.1) / 2.0);
    let (rx, ry) = (((max.0 - min.0) / 2.0).abs(), ((max.1 - min.1) / 2.0).abs());
    if rx < 0.1 || ry < 0.1 {
        return Vec::new();
    }
    let (start, sweep) = angles;
    let steps = (sweep.abs() as usize).max(2);
    (0..=steps)
        .map(|i| {
            let rad = (start + sweep * (i as f32 / steps as f32)).to_radians();
            tx(cx + rx * rad.cos(), cy + ry * rad.sin())
        })
        .collect()
}

/// Curly-brace arms: tip → knee → end → curl, for each of the two ends.
fn bracket_arms(
    tip: (f32, f32),
    end1: (f32, f32),
    end2: (f32, f32),
    tx: &impl Fn(f32, f32) -> (f32, f32),
) -> Vec<Vec<(f32, f32)>> {
    let (ki, ne, qi) = (20.0_f32, 10.0_f32, 8.0_f32);
    let sign = if end1.0 >= tip.0 { 1.0 } else { -1.0 };
    let mut arms = Vec::new();
    for (end, yd) in [(end1, -1.0_f32), (end2, 1.0_f32)] {
        let knee = (end.0, tip.1 + yd * ki);
        let c1 = (end.0, end.1 + yd * qi);
        let c2 = (end.0 + sign * ne, end.1 + yd * ne);
        let mut pts = vec![tx(tip.0, tip.1), tx(knee.0, knee.1), tx(end.0, end.1)];
        // Cubic from `end` (control `end`,`c1`) to `c2`.
        for i in 1..=8 {
            let t = i as f32 / 8.0;
            let mt = 1.0 - t;
            let x = mt * mt * mt * end.0
                + 3.0 * mt * mt * t * end.0
                + 3.0 * mt * t * t * c1.0
                + t * t * t * c2.0;
            let y = mt * mt * mt * end.1
                + 3.0 * mt * mt * t * end.1
                + 3.0 * mt * t * t * c1.1
                + t * t * t * c2.1;
            pts.push(tx(x, y));
        }
        arms.push(pts);
    }
    arms
}

/// Tessellate an ellipse from its bounding-box corners (the two `MultiPoint`
/// coords). The ellipse is built in local space then transformed by `tx`, so a
/// rotating/scaling matrix is honored.
fn ellipse_points(
    coords: Option<&serde_json::Value>,
    tx: &impl Fn(f32, f32) -> (f32, f32),
) -> Vec<(f32, f32)> {
    let pts: Vec<(f32, f32)> = coords
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(xy).collect())
        .unwrap_or_default();
    if pts.len() < 2 {
        return Vec::new();
    }
    let (minx, maxx) = (pts[0].0.min(pts[1].0), pts[0].0.max(pts[1].0));
    let (miny, maxy) = (pts[0].1.min(pts[1].1), pts[0].1.max(pts[1].1));
    let (cx, cy) = ((minx + maxx) / 2.0, (miny + maxy) / 2.0);
    let (rx, ry) = ((maxx - minx) / 2.0, (maxy - miny) / 2.0);
    let sides = 64;
    (0..sides)
        .map(|k| {
            let t = std::f32::consts::TAU * (k as f32) / sides as f32;
            tx(cx + rx * t.cos(), cy + ry * t.sin())
        })
        .collect()
}

/// Sample a `WaveLine` (sine between its two endpoints) into a polyline. The wave
/// is built in local coordinates from `waveAttr` (`wavyLength`/`wavyPeak`, padded
/// by the stroke width like BOOX does) then transformed by `tx`.
fn wave_points(
    coords: Option<&serde_json::Value>,
    props: Option<&serde_json::Value>,
    tx: &impl Fn(f32, f32) -> (f32, f32),
) -> Vec<(f32, f32)> {
    let pts: Vec<(f32, f32)> = coords
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(xy).collect())
        .unwrap_or_default();
    if pts.len() < 2 {
        return Vec::new();
    }
    let (p0, p1) = (pts[0], pts[pts.len() - 1]);
    let wa = props.and_then(|p| p.get("waveAttr"));
    let getf = |o: Option<&serde_json::Value>, k: &str, d: f32| {
        o.and_then(|v| v.get(k))
            .and_then(|v| v.as_f64())
            .unwrap_or(d as f64) as f32
    };
    let sw = props
        .and_then(|p| p.get("strokeAttr"))
        .and_then(|s| s.get("width"))
        .and_then(|v| v.as_f64())
        .unwrap_or(2.0) as f32;
    let wave_len = getf(wa, "wavyLength", 24.0) + sw * 2.0;
    let wave_peak = getf(wa, "wavyPeak", 12.0) + sw / 2.0;
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let dist = dx.hypot(dy);
    if dist < 0.1 || wave_len <= 0.1 {
        return vec![tx(p0.0, p0.1), tx(p1.0, p1.1)];
    }
    let (ca, sa) = (dx / dist, dy / dist);
    // Local (along, perp) -> transformed point.
    let map = |s: f32, h: f32| tx(p0.0 + s * ca - h * sa, p0.1 + s * sa + h * ca);
    // Sample a quadratic (P0=(s0,0), C=(sc,hc), P1=(s1,0)) into the output.
    let quad = |s0: f32, sc: f32, hc: f32, s1: f32, out: &mut Vec<(f32, f32)>| {
        const STEPS: usize = 6;
        for k in 1..=STEPS {
            let t = k as f32 / STEPS as f32;
            let mt = 1.0 - t;
            let s = mt * mt * s0 + 2.0 * mt * t * sc + t * t * s1;
            let h = 2.0 * mt * t * hc; // P0/P1 perp = 0
            out.push(map(s, h));
        }
    };
    let mut out = vec![map(0.0, 0.0)];
    let n = (dist / wave_len) as usize;
    let mut ox = 0.0;
    for _ in 0..n {
        quad(
            ox,
            ox + wave_len / 4.0,
            wave_peak,
            ox + wave_len / 2.0,
            &mut out,
        );
        quad(
            ox + wave_len / 2.0,
            ox + wave_len * 0.75,
            -wave_peak,
            ox + wave_len,
            &mut out,
        );
        ox += wave_len;
    }
    out.push(tx(p1.0, p1.1)); // straight tail for any remainder
    out
}

/// Parse `[[x,y],[x,y],...]` into transformed points.
fn coord_points(
    coords: Option<&serde_json::Value>,
    tx: &impl Fn(f32, f32) -> (f32, f32),
) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    if let Some(arr) = coords.and_then(|c| c.as_array()) {
        for p in arr {
            if let Some((x, y)) = xy(p) {
                out.push(tx(x, y));
            }
        }
    }
    out
}

/// Parse edge-pair `[[[x,y],[x,y]], ...]` into a vertex ring (each edge's start,
/// plus the final edge's end to close the loop).
fn edge_points(
    coords: Option<&serde_json::Value>,
    tx: &impl Fn(f32, f32) -> (f32, f32),
) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    let Some(edges) = coords.and_then(|c| c.as_array()) else {
        return out;
    };
    for edge in edges {
        if let Some(start) = edge.as_array().and_then(|e| e.first())
            && let Some((x, y)) = xy(start)
        {
            out.push(tx(x, y));
        }
    }
    if let Some(last_end) = edges
        .last()
        .and_then(|e| e.as_array())
        .and_then(|e| e.get(1))
        && let Some((x, y)) = xy(last_end)
    {
        out.push(tx(x, y));
    }
    out
}

fn xy(v: &serde_json::Value) -> Option<(f32, f32)> {
    let a = v.as_array()?;
    Some((a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32))
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

/// True if the shape's field-7 bbox (tile-local) lies entirely outside its
/// tile's local rect `[0, 0, tile.width, tile.height]` — the signature of a
/// stray boundary-crossing duplicate that belongs to another tile. A stroke
/// that merely overflows its tile still has its bbox intersect the tile, so it
/// is not culled. Shapes without a bbox are kept.
fn bbox_outside_tile(shape: &proto::Shape, tile: &Rect) -> bool {
    let Some(bb) = json_meta::parse_opt::<Rect>(&shape.bbox_json) else {
        return false;
    };
    let (tw, th) = (tile.width(), tile.height());
    bb.right < 0.0 || bb.left > tw || bb.bottom < 0.0 || bb.top > th
}

/// Fit the (already matrix-transformed) points onto the shape bbox (field 7),
/// the authoritative tile-local position. Returns
/// `(scale_x, scale_y, out_cx, out_cy, in_cx, in_cy)`; a point maps as
/// `final = (t - in_c) * scale + out_c`.
///
/// NOTE: the points' **center** is aligned to the bbox center, not a corner —
/// the bbox pads the points symmetrically by half the stroke width, so
/// corner-snapping would shift wide strokes by that padding. Center alignment
/// also snaps no-matrix boundary-crossing duplicates (whose points sit a whole
/// tile away) back onto their true spot.
///
/// NOTE: scale stays 1 unless the points grossly **overflow** the bbox (well
/// below 1 in both axes — seen with pen_type 2000, whose points live in a
/// larger frame with no matrix). When the bbox is merely larger than the point
/// extent, rescaling would blow up small marks, so native scale is kept.
fn bbox_fit(shape: &proto::Shape, tpts: &[(f32, f32)]) -> (f32, f32, f32, f32, f32, f32) {
    let txmin = tpts.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
    let txmax = tpts.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max);
    let tymin = tpts.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
    let tymax = tpts.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max);
    let (in_cx, in_cy) = ((txmin + txmax) / 2.0, (tymin + tymax) / 2.0);
    let Some(bb) = json_meta::parse_opt::<Rect>(&shape.bbox_json) else {
        // No bbox: leave points where they are (identity transform).
        return (1.0, 1.0, in_cx, in_cy, in_cx, in_cy);
    };
    if !in_cx.is_finite() || !in_cy.is_finite() {
        return (1.0, 1.0, 0.0, 0.0, 0.0, 0.0);
    }
    let (tw, th) = (txmax - txmin, tymax - tymin);
    let sx = if tw > 1e-3 { bb.width() / tw } else { 1.0 };
    let sy = if th > 1e-3 { bb.height() / th } else { 1.0 };
    let overflow = sx < 0.7 && sy < 0.7;
    let (sx, sy) = if overflow { (sx, sy) } else { (1.0, 1.0) };
    let (out_cx, out_cy) = ((bb.left + bb.right) / 2.0, (bb.top + bb.bottom) / 2.0);
    (sx, sy, out_cx, out_cy, in_cx, in_cy)
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
    let mut chars = html.chars();
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
                for c2 in chars.by_ref() {
                    if c2 == ';' || ent.len() >= 8 {
                        break;
                    }
                    ent.push(c2);
                }
                if let Some(ch) = decode_entity(&ent) {
                    push(ch, b, i, u, &mut runs);
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

    fn shape_with_bbox(json: &str) -> proto::Shape {
        proto::Shape {
            bbox_json: json.to_string(),
            ..Default::default()
        }
    }

    fn tile() -> Rect {
        Rect {
            left: 0.0,
            top: 0.0,
            right: 1860.0,
            bottom: 2480.0,
        }
    }

    fn shape_entry(page_id: &str) -> ShapeEntry {
        ShapeEntry {
            shape: proto::Shape::default(),
            page_id: page_id.to_string(),
            timestamp: 0,
            order: 0,
        }
    }

    #[test]
    fn per_page_order_keeps_blank_pages_from_page_name_list() {
        let page_order = vec!["p1".to_string(), "p2".to_string(), "p3".to_string()];
        let shapes = vec![shape_entry("p1"), shape_entry("p3")];
        assert_eq!(per_page_order(&page_order, &shapes), page_order);
    }

    #[test]
    fn per_page_order_appends_shape_pages_missing_from_page_name_list() {
        let page_order = vec!["p1".to_string()];
        let shapes = vec![shape_entry("p2"), shape_entry("p1"), shape_entry("p2")];
        assert_eq!(
            per_page_order(&page_order, &shapes),
            vec!["p1".to_string(), "p2".to_string()]
        );
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
    fn bbox_expand_padded_includes_visual_radius() {
        let mut bbox = BBox::default();
        bbox.expand_padded(10.0, 20.0, 3.0);
        assert_eq!(
            (bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y),
            (7.0, 17.0, 13.0, 23.0)
        );
    }

    #[test]
    fn expand_bounds_widens_with_stroke_width() {
        // A wider stroke must expand the content bbox further around the same
        // centerline (expand_bounds is the single source of per-item bounds).
        let pts = vec![
            Point {
                x: 100.0,
                y: 100.0,
                tilt_x: 0,
                tilt_y: 0,
                pressure: 4095,
                t: 0,
            },
            Point {
                x: 100.0,
                y: 200.0,
                tilt_x: 0,
                tilt_y: 0,
                pressure: 4095,
                t: 0,
            },
        ];
        let stroke = |width: f32| {
            RenderItem::Stroke(RenderStroke {
                points: pts.clone(),
                width,
                color: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 1.0,
                },
                pen_type: 2,
                charcoal_texture: 1,
                flat_marker: false,
            })
        };
        let mut thin = BBox::default();
        expand_bounds(&mut thin, &stroke(2.0));
        let mut thick = BBox::default();
        expand_bounds(&mut thick, &stroke(20.0));
        assert!(
            thick.max_x - thick.min_x > thin.max_x - thin.min_x,
            "wider stroke must expand the bbox further"
        );
    }

    #[test]
    fn bbox_outside_when_fully_left_of_tile() {
        // Stray pen-2000 duplicate: bbox a whole tile to the left (cf. AGENTS.md).
        let s = shape_with_bbox(r#"{"left":-3334,"top":10,"right":-3000,"bottom":50}"#);
        assert!(bbox_outside_tile(&s, &tile()));
    }

    #[test]
    fn bbox_inside_or_overflowing_is_kept() {
        // Legitimately overflowing left edge but still intersects the tile.
        let s = shape_with_bbox(r#"{"left":-100,"top":10,"right":200,"bottom":50}"#);
        assert!(!bbox_outside_tile(&s, &tile()));
        // Fully inside.
        let s = shape_with_bbox(r#"{"left":100,"top":100,"right":300,"bottom":300}"#);
        assert!(!bbox_outside_tile(&s, &tile()));
    }

    #[test]
    fn bbox_missing_json_is_not_culled() {
        assert!(!bbox_outside_tile(&shape_with_bbox(""), &tile()));
        assert!(!bbox_outside_tile(&shape_with_bbox("not json"), &tile()));
    }

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

    /// Apply the bbox_fit transform to a point: `(t - in_c) * scale + out_c`.
    fn fit_apply(fit: (f32, f32, f32, f32, f32, f32), t: (f32, f32)) -> (f32, f32) {
        let (sx, sy, ocx, ocy, icx, icy) = fit;
        ((t.0 - icx) * sx + ocx, (t.1 - icy) * sy + ocy)
    }

    #[test]
    fn bbox_fit_without_bbox_is_identity() {
        let s = proto::Shape::default();
        let pts = [(10.0, 20.0), (30.0, 40.0)];
        let fit = bbox_fit(&s, &pts);
        // Each point maps onto itself.
        for &p in &pts {
            let q = fit_apply(fit, p);
            assert!((q.0 - p.0).abs() < 1e-4 && (q.1 - p.1).abs() < 1e-4);
        }
    }

    #[test]
    fn bbox_fit_translates_center_without_scaling() {
        // Points span 100x100; bbox is the same size but at a different origin →
        // no scale, just a translation that lands the points' center on the bbox center.
        let s = proto::Shape {
            bbox_json: r#"{"left":500,"top":600,"right":600,"bottom":700}"#.to_string(),
            ..Default::default()
        };
        let pts = [(0.0, 0.0), (100.0, 100.0)];
        let fit = bbox_fit(&s, &pts);
        assert!((fit.0 - 1.0).abs() < 1e-4, "no x scale"); // sx
        assert!((fit.1 - 1.0).abs() < 1e-4, "no y scale"); // sy
        // Center (50,50) → bbox center (550,650).
        assert_eq!(fit_apply(fit, (50.0, 50.0)), (550.0, 650.0));
        // Corners shift by the same delta (+500, +600).
        assert_eq!(fit_apply(fit, (0.0, 0.0)), (500.0, 600.0));
        assert_eq!(fit_apply(fit, (100.0, 100.0)), (600.0, 700.0));
    }

    #[test]
    fn bbox_fit_scales_down_grossly_overflowing_points() {
        // pen_type-2000 signature: points span 1000x1000 but the bbox is 100x100
        // (scale 0.1 in both axes, < 0.7) → scale to fit the bbox.
        let s = proto::Shape {
            bbox_json: r#"{"left":0,"top":0,"right":100,"bottom":100}"#.to_string(),
            ..Default::default()
        };
        let pts = [(0.0, 0.0), (1000.0, 1000.0)];
        let fit = bbox_fit(&s, &pts);
        assert!((fit.0 - 0.1).abs() < 1e-4, "sx scaled to fit");
        assert!((fit.1 - 0.1).abs() < 1e-4, "sy scaled to fit");
        // Extremes land on the bbox corners.
        assert_eq!(fit_apply(fit, (0.0, 0.0)), (0.0, 0.0));
        assert_eq!(fit_apply(fit, (1000.0, 1000.0)), (100.0, 100.0));
    }

    #[test]
    fn bbox_fit_keeps_native_scale_when_bbox_is_larger() {
        // bbox bigger than the point extent (sx,sy >= 1) → don't blow the mark up,
        // keep scale 1 and just translate the center.
        let s = proto::Shape {
            bbox_json: r#"{"left":0,"top":0,"right":1000,"bottom":1000}"#.to_string(),
            ..Default::default()
        };
        let pts = [(0.0, 0.0), (100.0, 100.0)];
        let fit = bbox_fit(&s, &pts);
        assert!((fit.0 - 1.0).abs() < 1e-4);
        assert!((fit.1 - 1.0).abs() < 1e-4);
        // Points' center (50,50) → bbox center (500,500).
        assert_eq!(fit_apply(fit, (50.0, 50.0)), (500.0, 500.0));
    }
}
