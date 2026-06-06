//! Shape-tool geometry (lines, polygons), backend-agnostic.
//!
//! Each path is already in global coordinates with resolved styles. Open paths
//! are stroked; closed paths are optionally filled then stroked. Stroke
//! width/color/dash come from the GeoJSON `strokeAttr`/`lineStyle`.

use super::Backend;
use crate::model::{Canvas, GeoPath, RenderGeo};

pub(crate) fn draw(b: &mut impl Backend, canvas: &Canvas, geo: &RenderGeo) {
    for path in &geo.paths {
        if path.points.len() < 2 {
            continue;
        }
        if path.closed {
            draw_closed(b, canvas, path);
        } else if let Some((color, width)) = path.stroke {
            b.stroke_polyline(canvas, &path.points, width, color, path.dash.as_deref());
        }
    }
}

fn draw_closed(b: &mut impl Backend, canvas: &Canvas, path: &GeoPath) {
    if let Some(fill) = path.fill {
        b.fill_path(canvas, std::slice::from_ref(&path.points), fill);
    }
    if let Some((color, width)) = path.stroke {
        // Close the ring so the outline meets cleanly.
        let mut ring = path.points.clone();
        if let (Some(&first), Some(&last)) = (ring.first(), ring.last())
            && first != last
        {
            ring.push(first);
        }
        b.stroke_polyline(canvas, &ring, width, color, path.dash.as_deref());
    }
}
