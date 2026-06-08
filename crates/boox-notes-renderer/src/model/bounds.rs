//! Content bounding box and per-page ordering for assembly.
//! See the parent module docs.

use super::*;

/// Running content bounding box in global coordinates.
pub(super) struct BBox {
    pub(super) min_x: f32,
    pub(super) min_y: f32,
    pub(super) max_x: f32,
    pub(super) max_y: f32,
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

/// Expand `bbox` to include a built item's visual bounds — the single place that
/// knows each kind's extent. Strokes use the pen's visual padding
/// ([`crate::pen::stroke_visual_pad`]); geometry pads by half its stroke width;
/// text/image use their placement rect.
pub(super) fn expand_bounds(bbox: &mut BBox, item: &RenderItem) {
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

/// Page canvases follow `pageNameList` exactly, including blank pages. Shape
/// files that reference pages missing from `pageNameList` are appended as a
/// compatibility fallback for malformed/incomplete archives.
pub(super) fn per_page_order(page_order: &[String], shape_entries: &[ShapeEntry]) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
