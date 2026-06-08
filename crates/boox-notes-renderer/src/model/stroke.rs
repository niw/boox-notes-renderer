//! Per-stroke model building: handwriting strokes, width/color resolution,
//! and the field-7 bbox fit/cull. See the parent module docs.

use super::*;

/// Build a handwriting-stroke item (the default pen-type arm). `None` when the
/// shape has no usable points or is a stray cross-tile duplicate.
pub(super) fn build_stroke(
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
        assert!((fit.0 - 1.0).abs() < 1e-4, "no x scale");
        assert!((fit.1 - 1.0).abs() < 1e-4, "no y scale");
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
