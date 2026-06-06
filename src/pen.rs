//! Pen geometry shared between `model` (visual-bounds estimation) and `render`
//! (actual rasterization). Pure formulas only — no backend, no I/O.
//!
//! The charcoal stamp size in particular must be identical on both sides: the
//! model uses it to pad the content bounding box, and the charcoal rasterizer
//! uses it to size each dab. Keeping one definition here stops the two from
//! drifting apart.

use crate::points::Point;

/// Charcoal (pen 22) stamp size in px for a point, tilt-modulated:
/// `θ = acos(cos(tiltX°)·cos(tiltY°))`, `m = floor((θ^2.2·3+1)·5)·0.2`,
/// `size = floor((w·1.2+5)·m)+1`. See AGENTS.md "charcoal" for the derivation.
pub(crate) fn charcoal_stamp_size(nominal_width: f32, tilt_x: i8, tilt_y: i8) -> i32 {
    let rx = (tilt_x as f32).to_radians();
    let ry = (tilt_y as f32).to_radians();
    let theta = (rx.cos() * ry.cos()).clamp(-1.0, 1.0).acos();
    let m = ((theta.powf(2.2) * 3.0 + 1.0) * 5.0).floor() * 0.2;
    (((nominal_width * 1.2 + 5.0) * m).floor() as i32 + 1).max(1)
}

/// Half the visual extent (in note pt) a stroke of `pen_type`/`width` adds
/// around its centerline, for trimming the content bbox. These mirror the
/// per-pen width models in `render::stroke` closely enough to bound them:
/// fountain (5) and nBrush (21) by their peak pressure widths, charcoal (22) by
/// the largest tilt-driven dab, calligraphy (60/61) by the chisel-nib diagonal.
pub(crate) fn stroke_visual_pad(pen_type: i64, width: f32, points: &[Point]) -> f32 {
    let width = width.max(0.2);
    match pen_type {
        5 => (0.964 * (width + 3.0)).max(2.0) / 2.0,
        21 => (2.0 * width + 3.0) / 2.0,
        22 => points
            .iter()
            .map(|p| charcoal_stamp_size(width, p.tilt_x, p.tilt_y) as f32 / 2.0 + 1.0)
            .fold(width / 2.0, f32::max),
        60 | 61 => {
            let l = width.max(0.3);
            let thin = l / width.clamp(1.0, 10.0);
            0.5 * l.hypot(thin)
        }
        _ => width / 2.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point_with_tilt(tilt_x: i8, tilt_y: i8) -> Point {
        Point {
            x: 0.0,
            y: 0.0,
            tilt_x,
            tilt_y,
            pressure: 4095,
            t: 0,
        }
    }

    #[test]
    fn stroke_visual_pad_covers_pressure_and_charcoal_widths() {
        assert_eq!(stroke_visual_pad(2, 10.0, &[]), 5.0);
        assert!(stroke_visual_pad(5, 10.0, &[]) > 5.0);
        assert!(stroke_visual_pad(21, 10.0, &[]) > 10.0);

        let charcoal = [point_with_tilt(0, 0), point_with_tilt(60, 0)];
        assert!(stroke_visual_pad(22, 10.0, &charcoal) > 5.0);
    }

    #[test]
    fn charcoal_stamp_size_grows_with_tilt() {
        // More tilt → a larger dab (the charcoal rasterizer and the model's bbox
        // padding both rely on this).
        let flat = charcoal_stamp_size(10.0, 0, 0);
        let tilted = charcoal_stamp_size(10.0, 60, 0);
        assert!(tilted > flat, "tilted {tilted} should exceed flat {flat}");
    }
}
