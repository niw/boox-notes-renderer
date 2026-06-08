//! Shape-tool geometry (pen 40) and scanline fill (pen 37): GeoJSON feature
//! walking and the primitive samplers. See the parent module docs.

use super::*;

/// Parse a pen-40 shape's GeoJSON `featureCollection` into transformed paths.
///
/// Coordinates are in shape-local space; we apply the shape's affine matrix
/// (field 8) and then the tile's global origin. Stroke width is scaled by the
/// matrix's linear scale so it matches the appearance on the BOOX device.
pub(super) fn build_geo(shape: &proto::Shape, ox: f32, oy: f32) -> Option<RenderItem> {
    // This shape kind is dropped whole if its GeoJSON can't be read, so warn
    // (rather than silently vanish) when a pen-40 shape carries unparseable
    // geometry — those are the cases worth investigating.
    let id = &shape.stroke_uuid;
    let outer: serde_json::Value = match serde_json::from_str(&shape.extra_json) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("geo shape {id}: extra_json is not valid JSON: {e}");
            return None;
        }
    };
    let Some(fc_str) = outer.get("featureCollection").and_then(|v| v.as_str()) else {
        log::warn!("geo shape {id}: extra_json has no featureCollection string");
        return None;
    };
    let fc: serde_json::Value = match serde_json::from_str(fc_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("geo shape {id}: featureCollection is not valid JSON: {e}");
            return None;
        }
    };
    let Some(features) = fc.get("features").and_then(|v| v.as_array()) else {
        log::warn!("geo shape {id}: featureCollection has no features array");
        return None;
    };

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
pub(super) fn build_scanline_fill(
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
    // `lineStyle` may sit under `properties` or directly under the feature, and
    // a grouped shape often carries it on the parent so the leaf LineStrings
    // inherit it. Track the nearest one down the tree. Only `type != 0` is
    // dashed — `type == 0` (or absent) is solid even if stale `dashLineIntervals`
    // linger, matching the shape-level `shape_line_style_dash`.
    let node_dash: Option<Vec<f64>> = feature
        .get("properties")
        .and_then(|p| p.get("lineStyle"))
        .or_else(|| feature.get("lineStyle"))
        .filter(|ls| ls.get("type").and_then(|t| t.as_i64()).unwrap_or(0) != 0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn geo_paths_for(line_style: serde_json::Value) -> Vec<GeoPath> {
        let feature = serde_json::json!({
            "geometry": {"type": "LineString", "coordinates": [[0.0, 0.0], [10.0, 0.0]]},
            "properties": {
                "lineStyle": line_style,
                "strokeAttr": {"color": -16777216, "width": 2.0},
            },
        });
        let mut paths = Vec::new();
        collect_geo_feature(&feature, &|x, y| (x, y), 1.0, &mut paths, None);
        paths
    }

    #[test]
    fn geo_dash_requires_nonzero_line_style_type() {
        // type != 0 → dashed.
        let dashed = geo_paths_for(serde_json::json!({"type": 1, "dashLineIntervals": [4.0, 4.0]}));
        assert_eq!(dashed.len(), 1);
        assert!(dashed[0].dash.is_some());

        // type == 0 with stale intervals → solid (no dash).
        let solid = geo_paths_for(serde_json::json!({"type": 0, "dashLineIntervals": [4.0, 4.0]}));
        assert_eq!(solid.len(), 1);
        assert!(solid[0].dash.is_none());
    }
}
