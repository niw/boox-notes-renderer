//! Handwriting stroke geometry (backend-agnostic).
//!
//! Constant-width pens draw as a single round-capped polyline. Pressure pens use
//! BOOX's method: a run of short round-capped segments, each stroked at its
//! own pressure-derived width; round caps bridge the joins gap-free. Translucent
//! strokes are wrapped in a backend group so overlapping segments composite once.

use super::{Backend, GroupBlend, charcoal};
use crate::model::{Canvas, RenderStroke, Rgba};
use crate::points;

/// nBrush (pen 21) per-point width — a two-phase width model.
///
/// Three phases:
///
/// - **Pen-down**: `velAccum = 0`, `size = D·√p₀` (`D = 2·nominal`).
/// - **Dwell** — while the pen stays within 3 px of the stroke's first point:
///   ```text
///   size += D·0.01·dt/16          clamp [minWidth, D+3]   (grows ~0/point)
///   velAccum = size − D·p^0.5     clamp [−0.8·D, 3]
///   ```
///   If pressure rises during the dwell, `velAccum` exits negative → the
///   following shaft renders thin. (A stroke that dwells at pen-down renders
///   thinner than one of equal pressure/speed that starts already moving.)
/// - **Move** — the velocity integrator:
///   ```text
///   width = velAccum + D·(smoothed_p)^0.5            clamp [minWidth, D+3]
///   velAccum += {  +0.5·((1−e^−D)·D·C1 + C2·r^E)   if r < 0.95  (decel → thicken)
///                  −(1/3)·( same )                  if r > 1.10  (accel → thin)  }
///               clamp velAccum to [−0.8·D, +3]
///   r = ((d_next + W·d_prev)/(W+1)) / d_prev   (smoothed inter-sample distance ratio)
///   smoothed_p = (W·p_cur + p_next)/(W+1)      (look-ahead pressure blend)
///   ```
///   Deceleration toward the stroke end climbs `velAccum` back up → the thick
///   rounded terminal blob. Together these give the pen's signature look: thin
///   travelling shafts, fat blobs at dwell/turn/stop.
///
/// NOTE: the integrator must run over the **raw** `stroke.points` (decimating
/// first corrupts it), and it is **timestamp-independent** — the model steps
/// with a fixed dt, which cancels in `r`. The recorded timestamps are jittery
/// (many `dt == 0`) and must not be used.
fn neo_brush_widths(nominal: f32, pts: &[points::Point]) -> Vec<f32> {
    let n = pts.len();
    if n == 0 {
        return Vec::new();
    }
    const W: f32 = 0.5; // prev/cur blend weight (smoothing), on pressure and distance
    const E_PRESS: f32 = 0.5; // pressure exponent -> width ∝ √pressure
    const E_VEL: f32 = 0.5; // velocity-ratio exponent
    const C1: f32 = 0.02;
    const C2: f32 = 1.2;
    const T_DECEL: f32 = 0.95; // ratio below this => decelerating => thicken
    const T_ACCEL: f32 = 1.1; // ratio above this => accelerating => thin
    const DWELL_DIST: f32 = 3.0; // pen stays within this (px) of the start => dwelling
    const DWELL_GROW: f32 = 0.01 * 3.0 / 16.0; // size growth per dwell point (·D)
    const MIN_W: f32 = 0.001; // minWidth floor for the size/velAccum maths

    let d = 2.0 * nominal; // brush diameter (full-pressure width)
    let cap = d + 3.0; // size clamp D+3
    let base = (1.0 - (-d).exp()) * d * C1; // per-step base magnitude (constant per stroke)
    let vel_lo = -0.8 * d; // velAccum floor

    // Inter-sample distance d_prev = |pts[i] - pts[i-1]| (the fixed-dt "speed", scaled).
    let seg_dist = |i: usize| -> f32 {
        if i == 0 {
            return 0.0;
        }
        let dx = pts[i].x - pts[i - 1].x;
        let dy = pts[i].y - pts[i - 1].y;
        (dx * dx + dy * dy).sqrt()
    };
    let pressure = |i: usize| -> f32 { (pts[i].pressure as f32 / PRESSURE_MAX).clamp(0.0, 1.0) };
    let dist_from_start = |i: usize| -> f32 { (pts[i].x - pts[0].x).hypot(pts[i].y - pts[0].y) };

    let mut widths = vec![0.0f32; n];
    // Pen-down: velAccum = 0, size = D·√p₀. NOTE: the dwell grows from this
    // size, NOT from minWidth — seeding from minWidth drives velAccum deeply
    // negative and bottoms the shaft out at the floor.
    let mut vel_accum = 0.0f32;
    let mut size = (d * pressure(0).powf(E_PRESS)).clamp(MIN_W, cap);
    widths[0] = size.clamp(0.5, cap);

    // Dwell phase: points within DWELL_DIST of the start (after pen-down). The size
    // keeps growing and `vel_accum = size − D·√p` carries the (now near-zero, or
    // slightly negative if pressure rose during the dwell) accumulator into the move.
    let mut i = 1;
    while i < n && dist_from_start(i) <= DWELL_DIST {
        size = (size + d * DWELL_GROW).clamp(MIN_W, cap);
        vel_accum = (size - d * pressure(i).powf(E_PRESS)).clamp(vel_lo, 3.0);
        widths[i] = size.clamp(0.5, cap);
        i += 1;
    }

    // Move phase: the velocity integrator, continuing from the dwell's vel_accum.
    #[allow(clippy::needless_range_loop)]
    for j in i..n {
        // The integrator needs the previous (j-1→j) and next (j→j+1) segment
        // distances, so it only steps on interior points.
        if j >= 1 && j + 1 < n {
            let d_prev = seg_dist(j);
            if d_prev > 0.0 {
                let d_next = seg_dist(j + 1);
                let r = ((d_next + W * d_prev) / (W + 1.0)) / d_prev;
                if r > 0.0 {
                    let mag = base + C2 * r.powf(E_VEL);
                    if r < T_DECEL {
                        vel_accum += 0.5 * mag;
                    } else if r > T_ACCEL {
                        vel_accum -= mag / 3.0;
                    }
                }
            }
            vel_accum = vel_accum.clamp(vel_lo, 3.0);
        }
        // Look-ahead pressure blend (W·p_cur + p_next)/(W+1).
        let p_cur = pressure(j);
        let p_next = if j + 1 < n { pressure(j + 1) } else { p_cur };
        let p = (W * p_cur + p_next) / (W + 1.0);
        widths[j] = (vel_accum + d * p.powf(E_PRESS)).clamp(0.5, cap);
    }
    // Pen-up: when the last step is NOT accelerating (the usual decel-to-stop
    // of a pen lift), the terminal width resets to `D·√p` with no velAccum —
    // the fat stop blob. Accelerating into the lift keeps the integrated value.
    if n >= 2 {
        let d_last = seg_dist(n - 1); // dist(n-2 → n-1)  ("d_next" of the up point)
        let d_prev = if n >= 3 { seg_dist(n - 2) } else { d_last }; // dist(n-3 → n-2)
        let accelerating = d_prev > 0.0 && (d_last / d_prev) > T_ACCEL;
        if !accelerating {
            let p = pressure(n - 1);
            widths[n - 1] = (d * p.powf(E_PRESS)).clamp(MIN_W, cap).clamp(0.5, cap);
        }
    }
    widths
}

/// Decimate a parallel `(points, widths)` stream, keeping a point when it is at
/// least `MIN_STEP` from the last kept one **or** its width changed appreciably (so
/// the velocity-driven blobs at ends/turns survive). Endpoints always kept.
fn decimate_pairs(pts: &[points::Point], widths: &[f32]) -> (Vec<points::Point>, Vec<f32>) {
    let n = pts.len();
    if n <= 2 {
        return (pts.to_vec(), widths.to_vec());
    }
    const MIN_DW: f32 = 0.4; // pt of width change worth a vertex
    let mut op = Vec::with_capacity(n);
    let mut ow = Vec::with_capacity(n);
    op.push(pts[0]);
    ow.push(widths[0]);
    let mut last = pts[0];
    let mut last_w = widths[0];
    for i in 1..n - 1 {
        let dx = pts[i].x - last.x;
        let dy = pts[i].y - last.y;
        if dx * dx + dy * dy >= MIN_STEP * MIN_STEP || (widths[i] - last_w).abs() >= MIN_DW {
            op.push(pts[i]);
            ow.push(widths[i]);
            last = pts[i];
            last_w = widths[i];
        }
    }
    op.push(pts[n - 1]);
    ow.push(widths[n - 1]);
    (op, ow)
}

/// Draw a variable-width stroke as one round-capped segment per point pair, each at
/// the mean width of its endpoints (BOOX's `#ONYX-STROKE` method).
fn draw_segments(
    b: &mut impl Backend,
    canvas: &Canvas,
    pts: &[points::Point],
    widths: &[f32],
    color: Rgba,
) {
    if pts.len() == 1 {
        b.fill_path(canvas, &[dot(pts[0].x, pts[0].y, widths[0] / 2.0)], color);
        return;
    }
    for i in 0..pts.len() - 1 {
        let w = ((widths[i] + widths[i + 1]) / 2.0).max(0.2);
        let seg = [(pts[i].x, pts[i].y), (pts[i + 1].x, pts[i + 1].y)];
        b.stroke_polyline(canvas, &seg, w, color, None);
    }
}

const PRESSURE_MAX: f32 = 4095.0;
/// Drop consecutive points closer than this (in note pt) to bound output size.
const MIN_STEP: f32 = 0.6;

/// Fountain (pen 5) per-point width, BOOX's pressure curve:
/// ```text
/// width(p) = max(2.0, 0.964 · (nominal + 3) · p^0.599)
/// ```
fn fountain_width(nominal: f32, pressure: u16) -> f32 {
    let p = (pressure as f32 / PRESSURE_MAX).clamp(0.0, 1.0);
    (0.964 * (nominal + 3.0) * p.powf(0.599)).max(2.0)
}

pub(crate) fn draw(b: &mut impl Backend, canvas: &Canvas, stroke: &RenderStroke) {
    // Charcoal/pencil (pen 22) bakes into a grain raster (see
    // `render/charcoal.rs`), placed as an inline image like BOOX's export. The
    // model consumes the raw recorded points (a dab every 3 px, per-point tilt
    // for the stamp size), so don't decimate here.
    if stroke.pen_type == 22 {
        if let Some((png, l, t, r, btm)) = charcoal::render(
            &stroke.points,
            stroke.width,
            stroke.color,
            stroke.charcoal_texture,
        ) {
            b.draw_inline_image(canvas, &png, l, t, r, btm);
        }
        return;
    }

    let pts = decimate(&stroke.points);
    if pts.is_empty() {
        return;
    }

    // Marker (15) composites with Multiply (see `draw_marker`); other
    // translucent strokes use normal alpha. begin_group no-ops (returns false)
    // for opaque strokes.
    let blend = if stroke.pen_type == 15 {
        GroupBlend::Multiply
    } else {
        GroupBlend::Normal
    };
    let grouped = b.begin_group(stroke.color.a, blend);
    // Inside a group children draw opaque (the group applies the alpha);
    // ungrouped, the color keeps its own alpha (the backends apply it
    // per-primitive).
    let color = if grouped {
        Rgba {
            a: 1.0,
            ..stroke.color
        }
    } else {
        stroke.color
    };

    match stroke.pen_type {
        // nBrush: compute widths over the raw points (the integrator needs every
        // recorded point), then decimate point+width pairs jointly. No extra
        // smoothing — the integrator's look-ahead blend is the only smoothing.
        21 => {
            let widths = neo_brush_widths(stroke.width, &stroke.points);
            let (rpts, rwidths) = decimate_pairs(&stroke.points, &widths);
            draw_segments(b, canvas, &rpts, &rwidths, color);
        }
        // Marker/highlighter: one constant-width polyline of the raw points,
        // composited by the enclosing 127/255 Multiply group (see `draw_marker`).
        15 => draw_marker(b, canvas, stroke, &pts, color),
        // Calligraphy: stamp the fixed rotated-rectangle nib (flat chisel ends),
        // not a round-capped variable-width centerline.
        60 | 61 => draw_calligraphy(b, canvas, stroke.pen_type, stroke.width, &pts, color),
        // Fountain: pressure-curve segment widths.
        5 => draw_fountain(b, canvas, stroke, &pts, color),
        // Every other pen (ballpoint 2, pen-2000 history, …): constant width.
        _ => draw_constant_width(b, canvas, stroke, &pts, color),
    }

    if grouped {
        b.end_group();
    }
}

/// Calligraphy (60/61): stamp the fixed rotated-rectangle nib along the path,
/// so stroke ends are the chisel's flat angled edge (not a round cap) and the
/// stroke's width perpendicular to the travel direction is the nib's projection
/// ```text
/// width(dir) = L·|cos(dir − α)| + w·|sin(dir − α)|
/// L = nominal                                       (the nib's long axis)
/// w = L / clamp(nominal, 1, 10)                     (the thin axis)
/// α = +45° (pen 60) / −45° (pen 61)
/// ```
/// Pressure- and tilt-independent. Each segment emits the convex hull of its
/// two nib rectangles (a gap-free swept quad); all hulls fill at once (nonzero
/// winding absorbs the overlaps).
fn draw_calligraphy(
    b: &mut impl Backend,
    canvas: &Canvas,
    pen_type: i64,
    thickness: f32,
    pts: &[points::Point],
    color: Rgba,
) {
    let l = thickness.max(0.3);
    let half_l = l / 2.0;
    let half_w = l / thickness.clamp(1.0, 10.0) / 2.0;
    let alpha = if pen_type == 60 {
        std::f32::consts::FRAC_PI_4
    } else {
        -std::f32::consts::FRAC_PI_4
    };
    let (ca, sa) = (alpha.cos(), alpha.sin());
    let along = (ca * half_w, sa * half_w); // ±along the nib's short axis (α)
    let perp = (-sa * half_l, ca * half_l); // ±along the nib's long axis (⊥ α)
    // The four nib-rect corners centred at point p.
    let nib = |p: &points::Point| -> [(f32, f32); 4] {
        [
            (p.x + along.0 + perp.0, p.y + along.1 + perp.1),
            (p.x + along.0 - perp.0, p.y + along.1 - perp.1),
            (p.x - along.0 - perp.0, p.y - along.1 - perp.1),
            (p.x - along.0 + perp.0, p.y - along.1 + perp.1),
        ]
    };
    if pts.len() == 1 {
        b.fill_path(canvas, &[nib(&pts[0]).to_vec()], color);
        return;
    }
    // One ring per segment: convex hull of the two endpoints' nib rectangles.
    let rings: Vec<Vec<(f32, f32)>> = (0..pts.len() - 1)
        .map(|i| {
            let mut corners: Vec<(f32, f32)> = nib(&pts[i]).to_vec();
            corners.extend_from_slice(&nib(&pts[i + 1]));
            convex_hull(&corners)
        })
        .collect();
    b.fill_path(canvas, &rings, color);
}

/// Andrew's monotone-chain convex hull (small point sets).
fn convex_hull(pts: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut p = pts.to_vec();
    p.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    p.dedup();
    if p.len() < 3 {
        return p;
    }
    let cross = |o: (f32, f32), a: (f32, f32), b: (f32, f32)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };
    let mut lower: Vec<(f32, f32)> = Vec::new();
    for &pt in &p {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], pt) <= 0.0 {
            lower.pop();
        }
        lower.push(pt);
    }
    let mut upper: Vec<(f32, f32)> = Vec::new();
    for &pt in p.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], pt) <= 0.0 {
            upper.pop();
        }
        upper.push(pt);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// Constant-width pens: one round-capped polyline (a lone point becomes a dot).
fn draw_constant_width(
    b: &mut impl Backend,
    canvas: &Canvas,
    stroke: &RenderStroke,
    pts: &[points::Point],
    color: Rgba,
) {
    let width = stroke.width.max(0.2);
    if pts.len() == 1 {
        b.fill_path(canvas, &[dot(pts[0].x, pts[0].y, width / 2.0)], color);
        return;
    }
    let xy: Vec<(f32, f32)> = pts.iter().map(|p| (p.x, p.y)).collect();
    b.stroke_polyline(canvas, &xy, width, color, None);
}

/// Marker/highlighter (15): BOOX's PDF-export construct — one polyline of
/// every recorded point, stroked **once** at the constant nominal width (round
/// cap/join), composited at alpha 127/255 with blend `Multiply`. `model` sets
/// the alpha; the enclosing group supplies it with `Multiply`, and the single
/// `stroke_polyline` keeps it one stroke op per stroke — the same construct
/// BOOX emits, so any given viewer renders both identically. Feed the points
/// undecimated, like BOOX's export.
///
/// **`--flat-marker`** (`stroke.flat_marker`): same width/color/compositing,
/// but the stroke is emitted as **one nonzero-winding fill** of the
/// constant-width capsule union (per-segment convex hulls of equal-radius disc
/// pairs) over the *decimated* points.
///
/// NOTE: flat-marker exists because some viewers (Quartz/Preview) chunk a long
/// translucent stroked path into separately-composited passes, darkening
/// self-crossings. A single fill op composites once in every viewer; overlaps
/// *between* strokes still alpha/Multiply-darken as before. Decimation is fine
/// here — the geometry is width-constant.
fn draw_marker(
    b: &mut impl Backend,
    canvas: &Canvas,
    stroke: &RenderStroke,
    decimated: &[points::Point],
    color: Rgba,
) {
    let width = stroke.width.max(0.2);
    let r = width / 2.0;
    if stroke.flat_marker {
        let pts = decimated;
        if pts.len() == 1 {
            b.fill_path(canvas, &[dot(pts[0].x, pts[0].y, r)], color);
            return;
        }
        let rings: Vec<Vec<(f32, f32)>> = (0..pts.len() - 1)
            .map(|i| {
                let mut hull = dot(pts[i].x, pts[i].y, r);
                hull.extend(dot(pts[i + 1].x, pts[i + 1].y, r));
                convex_hull(&hull)
            })
            .collect();
        b.fill_path(canvas, &rings, color);
        return;
    }
    let pts = &stroke.points;
    if pts.is_empty() {
        return;
    }
    if pts.len() == 1 {
        b.fill_path(canvas, &[dot(pts[0].x, pts[0].y, r)], color);
        return;
    }
    let xy: Vec<(f32, f32)> = pts.iter().map(|p| (p.x, p.y)).collect();
    b.stroke_polyline(canvas, &xy, width, color, None);
}

/// Fountain (5): one round-capped segment per point pair, each at the mean width
/// of its two endpoints (BOOX's `#ONYX-STROKE` method). The per-point widths
/// come from the pressure curve, low-passed like BOOX does (a zero-phase EMA —
/// forward + backward — so widths stay aligned with the geometry rather than
/// lagging). No synthetic end-taper: stroke ends are simply the curve's 2.0 pt
/// floor where pressure → 0 at lift.
fn draw_fountain(
    b: &mut impl Backend,
    canvas: &Canvas,
    stroke: &RenderStroke,
    pts: &[points::Point],
    color: Rgba,
) {
    let mut widths: Vec<f32> = pts
        .iter()
        .map(|p| fountain_width(stroke.width, p.pressure))
        .collect();
    smooth_widths_zero_phase(&mut widths, 0.2);
    draw_segments(b, canvas, pts, &widths, color);
}

/// Keep the first/last point and any point that is either at least `MIN_STEP`
/// away from the last kept one **or** whose pressure changed by `MIN_PRESSURE`.
/// The pressure criterion is essential at stroke ends, where the pen slows and
/// lifts: those points sit <`MIN_STEP` apart but their pressure (hence width)
/// drops fast — dropping them on distance alone collapses the taper into one fat
/// blunt segment instead of a fine point.
fn decimate(points: &[points::Point]) -> Vec<points::Point> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    const MIN_PRESSURE: i32 = 90;
    let mut out = Vec::with_capacity(points.len());
    out.push(points[0]);
    let mut last = points[0];
    for p in &points[1..points.len() - 1] {
        let dx = p.x - last.x;
        let dy = p.y - last.y;
        let dp = (p.pressure as i32 - last.pressure as i32).abs();
        if dx * dx + dy * dy >= MIN_STEP * MIN_STEP || dp >= MIN_PRESSURE {
            out.push(*p);
            last = *p;
        }
    }
    out.push(points[points.len() - 1]);
    out
}

/// Zero-phase EMA low-pass of a width sequence (forward pass then backward pass
/// with the same `alpha`). Two opposite-direction passes cancel the single-pass
/// phase lag, so peaks/valleys stay aligned with the geometry instead of
/// shifting downstream.
fn smooth_widths_zero_phase(widths: &mut [f32], alpha: f32) {
    let n = widths.len();
    if n < 3 {
        return;
    }
    for i in 1..n {
        widths[i] = widths[i - 1] + alpha * (widths[i] - widths[i - 1]);
    }
    for i in (0..n - 1).rev() {
        widths[i] = widths[i + 1] + alpha * (widths[i] - widths[i + 1]);
    }
}

/// A filled disc (n-gon) in global coords, for lone-point dots. Circumscribed so
/// the flats reach `r`.
fn dot(cx: f32, cy: f32, r: f32) -> Vec<(f32, f32)> {
    let r = r.max(0.3);
    let sides = if r > 6.0 { 16 } else { 8 };
    let rr = r / (std::f32::consts::PI / sides as f32).cos();
    (0..sides)
        .map(|k| {
            let t = std::f32::consts::TAU * (k as f32) / sides as f32;
            (cx + rr * t.cos(), cy + rr * t.sin())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fountain_curve_follows_boox_formula() {
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-3;
        let t = 2.0_f32;
        // Peak 0.964·(t+3) at full pressure; the curve's 2.0 pt floor at zero
        // (stroke ends taper to this, not to 0).
        assert!(approx(fountain_width(t, 4095), 0.964 * (t + 3.0)));
        assert!(approx(fountain_width(t, 0), 2.0));
    }

    #[test]
    fn neo_brush_pressure_scales_width() {
        // Two strokes with identical geometry/dynamics differing only in pressure:
        // the harder one must be wider (the √pressure term). Comparing matched
        // dynamics isolates pressure from the velAccum integrator.
        let run = |pressure: u16| -> Vec<points::Point> {
            (0..30)
                .map(|i| points::Point {
                    x: i as f32 * 4.0, // start moving immediately (minimal dwell), steady speed
                    y: 0.0,
                    tilt_x: 0,
                    tilt_y: 0,
                    pressure,
                    t: i as u32 * 3,
                })
                .collect()
        };
        let nominal = 3.0_f32;
        let avg = |v: &[f32]| v[10..].iter().sum::<f32>() / (v.len() - 10) as f32;
        let heavy = avg(&neo_brush_widths(nominal, &run(4095)));
        let light = avg(&neo_brush_widths(nominal, &run(400)));
        assert!(heavy > light * 1.5, "heavy {heavy:.2} vs light {light:.2}");
    }

    #[test]
    fn neo_brush_dwell_thins_shaft_then_decel_blobs() {
        // The pen's signature: a pen-down *dwell* (pressure ramps while ~stationary)
        // drives velAccum negative so the travelling shaft renders thin; deceleration
        // toward the stop then thickens it into a terminal blob. Build a stroke with
        // all three phases and assert thin-shaft < both ends.
        let nominal = 6.0_f32;
        let mut pts = Vec::new();
        let mut t = 0u32;
        // (1) Dwell: ~stationary at origin, pressure ramping 0 -> high.
        for i in 0..20 {
            pts.push(points::Point {
                x: (i as f32 * 0.05).sin() * 0.5, // jitter < 3 px from start
                y: (i as f32 * 0.07).cos() * 0.5,
                tilt_x: 0,
                tilt_y: 0,
                pressure: (i as f32 / 19.0 * 2400.0) as u16,
                t: {
                    t += 3;
                    t
                },
            });
        }
        // (2) Fast travelling shaft (large, steady steps) at steady pressure.
        for i in 0..30 {
            pts.push(points::Point {
                x: 10.0 + i as f32 * 4.0,
                y: 0.0,
                tilt_x: 0,
                tilt_y: 0,
                pressure: 2400,
                t: {
                    t += 3;
                    t
                },
            });
        }
        // (3) Decelerate to a stop (shrinking steps) at steady pressure.
        let mut x = 130.0;
        let mut step = 4.0;
        for _ in 0..20 {
            step *= 0.8;
            x += step;
            pts.push(points::Point {
                x,
                y: 0.0,
                tilt_x: 0,
                tilt_y: 0,
                pressure: 2400,
                t: {
                    t += 3;
                    t
                },
            });
        }
        let w = neo_brush_widths(nominal, &pts);
        let shaft = w[25..50].iter().cloned().fold(f32::MAX, f32::min); // thinnest shaft
        let blob = w[pts.len() - 6..].iter().cloned().fold(0.0_f32, f32::max); // terminal
        assert!(
            blob > shaft * 1.8,
            "decel terminal blob ({blob:.2}) should be much wider than the travelling shaft ({shaft:.2})"
        );
    }

    #[test]
    fn neo_brush_width_is_timestamp_independent() {
        // The per-point width depends only on point geometry/pressure — never on
        // the (jittery, often dt==0) recorded timestamps. Same geometry +
        // pressure with arbitrarily different timestamps must yield identical
        // widths.
        let make = |ts: &[u32]| -> Vec<points::Point> {
            // A varying-speed zig so the velocity integrator actually moves.
            (0..ts.len())
                .map(|i| points::Point {
                    x: (i as f32).powf(1.3), // non-uniform spacing => r != 1
                    y: ((i as f32) * 0.7).sin() * 4.0,
                    tilt_x: 0,
                    tilt_y: 0,
                    pressure: 1800,
                    t: ts[i],
                })
                .collect()
        };
        let n = 40;
        let steady: Vec<u32> = (0..n).map(|i| i as u32 * 3).collect();
        let jittery: Vec<u32> = (0..n)
            .map(|i| if i % 3 == 0 { 0 } else { i as u32 * 7 }) // includes dt==0 and dt<0
            .collect();
        let a = neo_brush_widths(3.0, &make(&steady));
        let b = neo_brush_widths(3.0, &make(&jittery));
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert!((x - y).abs() < 1e-4, "width[{i}] differs: {x} vs {y}");
        }
    }

    /// A Backend recording `fill_path` rings; everything else is a no-op.
    /// (`draw_calligraphy` emits only fills.)
    struct CaptureFills {
        rings: Vec<Vec<(f32, f32)>>,
    }
    impl Backend for CaptureFills {
        fn fill_background(&mut self, _: &Canvas, _: Rgba) {}
        fn fill_path(&mut self, _: &Canvas, rings: &[Vec<(f32, f32)>], _: Rgba) {
            self.rings.extend(rings.iter().cloned());
        }
        fn stroke_polyline(
            &mut self,
            _: &Canvas,
            _: &[(f32, f32)],
            _: f32,
            _: Rgba,
            _: Option<&[f32]>,
        ) {
        }
        fn draw_inline_image(&mut self, _: &Canvas, _: &[u8], _: f32, _: f32, _: f32, _: f32) {}
        fn draw_text_line(
            &mut self,
            _: &Canvas,
            _: f32,
            _: f32,
            _: &str,
            _: f32,
            _: Rgba,
            _: bool,
            _: bool,
            _: &crate::render::fonts::ResolvedFont,
        ) {
        }
        fn begin_group(&mut self, _: f32, _: GroupBlend) -> bool {
            false
        }
        fn end_group(&mut self) {}
    }

    /// Draw a straight calligraphy stroke along `(dx, dy)` and measure the
    /// filled band's extent perpendicular to the travel direction.
    fn calligraphy_band_width(pen_type: i64, nominal: f32, dx: f32, dy: f32) -> f32 {
        let pts: Vec<points::Point> = (0..30)
            .map(|i| points::Point {
                x: 100.0 + i as f32 * dx,
                y: 100.0 + i as f32 * dy,
                tilt_x: 0,
                tilt_y: 0,
                pressure: 2000, // structurally ignored: the nib reads no pressure
                t: 0,
            })
            .collect();
        let canvas = Canvas {
            width: 1000.0,
            height: 1000.0,
            origin_x: 0.0,
            origin_y: 0.0,
            items: Vec::new(),
        };
        let mut b = CaptureFills { rings: Vec::new() };
        let black = Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 1.0,
        };
        draw_calligraphy(&mut b, &canvas, pen_type, nominal, &pts, black);
        // Project every emitted vertex onto the unit normal of the travel
        // direction; the band width is the projection's spread.
        let len = dx.hypot(dy);
        let (px, py) = (-dy / len, dx / len);
        let proj: Vec<f32> = b
            .rings
            .iter()
            .flatten()
            .map(|&(x, y)| x * px + y * py)
            .collect();
        let max = proj.iter().fold(f32::NEG_INFINITY, |a, &v| a.max(v));
        let min = proj.iter().fold(f32::INFINITY, |a, &v| a.min(v));
        max - min
    }

    #[test]
    fn calligraphy_stamps_a_directional_rotated_rectangle() {
        // The chisel is a fixed rotated-rectangle nib, so a straight stroke's
        // width is the nib's projection perpendicular to travel:
        // width(dir) = L·|cos(dir−α)| + w·|sin(dir−α)|,
        // L = nominal, w = L/clamp(nominal,1,10), α = ±45°.
        let l = 6.0_f32;
        let thin = l / l.clamp(1.0, 10.0); // = 1.0

        // Pen 60 (α=+45°): full L when travelling ∥ 45° (down-right, dx=dy>0),
        // the thin axis when ⊥ (up-right, dx=-dy).
        let thick60 = calligraphy_band_width(60, l, 1.0, 1.0);
        let thin60 = calligraphy_band_width(60, l, 1.0, -1.0);
        assert!((thick60 - l).abs() < 0.1, "pen60 thick {thick60} ~ L={l}");
        assert!((thin60 - thin).abs() < 0.1, "pen60 thin {thin60} ~ {thin}");

        // Pen 61 (α=−45°) is the mirror: full L when travelling ∥ −45°.
        let thick61 = calligraphy_band_width(61, l, 1.0, -1.0);
        assert!((thick61 - l).abs() < 0.1, "pen61 thick {thick61} ~ L={l}");
        let thin61 = calligraphy_band_width(61, l, 1.0, 1.0);
        assert!((thin61 - thin).abs() < 0.1, "pen61 thin {thin61} ~ {thin}");
    }

    #[test]
    fn zero_phase_smoothing_reduces_jumps_without_shifting_peak() {
        // A single sharp spike: smoothing must lower the spike (less abrupt) while
        // keeping its peak at the same index (zero phase — no downstream lag).
        let mut w = vec![2.0, 2.0, 2.0, 8.0, 2.0, 2.0, 2.0];
        smooth_widths_zero_phase(&mut w, 0.2);
        let peak_idx = w
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(peak_idx, 3, "peak must stay centered, not lag");
        assert!(w[3] < 8.0, "spike must be attenuated");
        assert!(w[3] > 2.0, "but not flattened away entirely");
        // The spike's energy spreads to both neighbours (lifts them above 2.0).
        assert!(w[2] > 2.0 && w[4] > 2.0, "smoothing must spread both ways");
    }
}
