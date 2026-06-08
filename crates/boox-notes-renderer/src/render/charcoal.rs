//! Charcoal/pencil (pen 22) rendering — a model of BOOX's charcoal pens
//! (`penAttrs.texture` 1 = V1; texture 2 = V2).
//!
//! The model is **dab-stamping**: a square grain dab is stamped every **3 px**
//! of arc length; the dabs overlap and build up the grainy band. Each V1 dab's
//! alpha is
//!
//! ```text
//! grainAlpha(x,y) = textureBase·pscale·radialDab(x,y)·grainTex(x,y) / 255
//!                   + 0.8·convex[pscale]·pscale·radialDab2(x,y)·grainTex(x,y)/255
//! ```
//!
//! then **Floyd–Steinberg dithered** to a binary stamp in the stroke color:
//! - **`grainTex`** — a 179×179 value-noise field (embedded);
//!   its smooth structure makes the kept pixels cluster into charcoal's streaky
//!   clumps (a white-noise speckle cannot).
//! - **`radialDab`** — a soft erf disc `127.5·(erf(C·r+A) − erf(C·r−A))`,
//!   `A=3.4615`, `C=10.459/size`, `r=hypot(rx, |ry|·1.25)` rotated by a fixed
//!   5.393 rad; the second one is the same at half size (the centre boost).
//! - **`textureBase[pscale]`** and **`convex[pscale]`** — embedded curve
//!   tables indexed by the pressure scale.
//! - **`pscale = sqrt(p/3) + 0.3`** — the pressure scale.
//! - **stamp size** — tilt-driven: `θ=acos(cos(tiltX°)·cos(tiltY°))`,
//!   `m=floor((θ^2.2·3+1)·5)·0.2`, `size=floor((w·1.2+5)·m)+1`.
//!
//! Baked at 1 px/pt, BOOX's native granularity. V2 uses a different,
//! white-noise model (see `coverage_v2`).

use tiny_skia::{Pixmap, PremultipliedColorU8};

use crate::model::Rgba;
use crate::points;

/// Dab spacing along the path (px of arc length).
const DAB_SPACING: f32 = 3.0;

/// The 179×179 value-noise grain field (embedded).
const GRAIN: &[u8] = include_bytes!("charcoal_grain.bin");
const GRAIN_DIM: usize = 179;
/// `textureBase` curve (256 entries) indexed by `pscale`.
const TEXTURE_BASE: &[u8] = include_bytes!("charcoal_texture_base.bin");
/// Convex centre-boost curve (255 entries) indexed by `pscale`.
const CONVEX: &[u8] = include_bytes!("charcoal_convex.bin");

/// Mean grain value / 255 — the grain field multiplies into the alpha, so the
/// post-dither mean coverage carries this factor (`grainTex` mean ≈ 117.8).
const GRAIN_MEAN: f32 = 117.8 / 255.0;

// erf radial-dab constants.
const RAD_A: f32 = 3.4615393;
const RAD_C_NUM: f32 = 10.459; // C = RAD_C_NUM / size
const RAD_YSCALE: f32 = 1.25;
const RAD_ANGLE: f32 = 5.393_07;

/// Read entry `i` of an embedded little-endian f32 table.
fn f32_at(tbl: &[u8], i: usize) -> f32 {
    let o = i * 4;
    f32::from_le_bytes([tbl[o], tbl[o + 1], tbl[o + 2], tbl[o + 3]])
}

/// Abramowitz–Stegun 7.1.26 error function (max abs err ~1.5e-7).
fn erf(x: f32) -> f32 {
    let s = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let y = 1.0
        - (((((1.061_405_4 * t - 1.453_152_1) * t) + 1.421_413_8) * t - 0.284_496_72) * t
            + 0.254_829_6)
            * t
            * (-x * x).exp();
    s * y
}

/// The erf radial dab in `[0,1]` at offset `(rdx, rdy)` from the stamp centre
/// for a dab spanning `sz` px: a soft anisotropic disc, flat core tapering with
/// an error-function edge, 0 beyond radius `sz/2`.
fn radial(rdx: f32, rdy: f32, sz: f32) -> f32 {
    let (s, c) = RAD_ANGLE.sin_cos();
    let rx = rdx * c - rdy * s;
    let ry = rdx * s + rdy * c;
    let r = rx.hypot(ry.abs() * RAD_YSCALE);
    let big_r = sz * 0.5;
    if r > big_r {
        return 0.0;
    }
    let cc = RAD_C_NUM / sz;
    let v = 127.5 * (erf(cc * r + RAD_A) - erf(cc * r - RAD_A));
    (v / 255.0).clamp(0.0, 1.0)
}

/// V1 post-dither local ink coverage at stamp pixel `(dx, dy)` for a dab of
/// side `size` and pressure scale `pscale`:
/// `GRAIN_MEAN·pscale·(textureBase[pscale]·radial1 + 0.8·convex[pscale]·radial2)`,
/// `radial2` being the half-size convex centre boost.
fn coverage(pscale: f32, dx: f32, dy: f32, size: f32) -> f32 {
    let i2 = ((255.0 * pscale) as usize).min(255);
    let ic = ((254.0 * pscale) as usize).min(254);
    let texture_base = f32_at(TEXTURE_BASE, i2);
    let convex = f32_at(CONVEX, ic);
    let r1 = radial(dx, dy, size);
    let r2 = radial(dx, dy, size * 0.5); // half-size centre boost
    let alpha = texture_base * r1 + 0.8 * convex * r2;
    (GRAIN_MEAN * pscale * alpha).clamp(0.0, 1.0)
}

// V2 (texture 2) is a separate model: a white-noise texture masked by an elliptical
// soft disc, kept per pixel with probability `coverage = P · disc(rn)` where
// `P` is the raw pressure (centre coverage equals P). The stroke region renders
// once with one texture, so overlapping dabs do NOT build up — density tracks
// local pressure, and light pressure stays sparse (the kasure / 掠れ look). The
// grain is uncorrelated white noise, so a procedural per-pixel speckle
// reproduces it.

/// The V2 soft disc in `[0,1]`: full inside `rn ≤ 0.6`, linear taper to 0 at
/// `rn = 1` (`rn` = radius normalized to `size/2`).
fn disc_v2(rdx: f32, rdy: f32, size: f32) -> f32 {
    let rn = rdx.hypot(rdy) / (size * 0.5).max(0.5);
    ((1.0 - rn) / 0.40).clamp(0.0, 1.0)
}

/// V2 per-pixel ink coverage: `P · disc(rn)`, `P` = raw pressure.
fn coverage_v2(pressure: f32, rdx: f32, rdy: f32, size: f32) -> f32 {
    (pressure.clamp(0.0, 1.0) * disc_v2(rdx, rdy, size)).clamp(0.0, 1.0)
}

/// `java.util.Random`-compatible LCG, used for the per-stamp grain jitter.
///
/// NOTE: BOOX's grain is non-deterministic run to run; we use one RNG per
/// stroke, seeded deterministically from the stroke, so our output is
/// reproducible — only the varying-per-stamp sequence affects appearance.
struct JavaRandom {
    seed: u64,
}
impl JavaRandom {
    fn new(seed: u64) -> Self {
        Self {
            seed: (seed ^ 0x5DEE_CE66D) & ((1 << 48) - 1),
        }
    }
    fn next(&mut self, bits: u32) -> u32 {
        self.seed = self.seed.wrapping_mul(0x5DEE_CE66D).wrapping_add(0xB) & ((1 << 48) - 1);
        (self.seed >> (48 - bits)) as u32
    }
    /// `nextFloat()` in `[0,1)` (Java: `next(24) / 2^24`).
    fn next_float(&mut self) -> f32 {
        self.next(24) as f32 / (1u64 << 24) as f32
    }
    /// `nextInt(bound)` (bound > 0), Java's algorithm including the rejection loop.
    fn next_int(&mut self, bound: u32) -> u32 {
        if bound.is_power_of_two() {
            return ((bound as u64 * self.next(31) as u64) >> 31) as u32;
        }
        loop {
            let bits = self.next(31);
            let val = bits % bound;
            if bits.wrapping_sub(val).wrapping_add(bound - 1) < (1 << 31) {
                return val;
            }
        }
    }
}

/// Per-stamp grain-window offset `(jx, jy)` in `[0,99]²`:
/// `v = (int)(clampedPressure·(nextInt(9)+11)·100000)`, `jx = (v%1000)%100`,
/// `jy = ((v/1000)%1000)%100`. Varying it per stamp is what stops the grain
/// repeating (9 windows per pressure).
fn grain_jitter(rng: &mut JavaRandom, pressure: f32) -> (usize, usize) {
    let cp = ((pressure * 1000.0).floor() / 1000.0).clamp(0.0, 1.0);
    // cp >= 0 and the random factor is >= 11, so v is always non-negative and
    // plain `%` matches the documented formula.
    let v = (cp * (rng.next_int(9) as f32 + 11.0) * 100000.0) as i64;
    let jx = ((v % 1000) % 100) as usize;
    let jy = (((v / 1000) % 1000) % 100) as usize;
    (jx, jy)
}

/// Rasterize a charcoal stroke. `nominal_width` is the pen's (matrix-scaled)
/// stroke width; tilt comes from the points. Returns
/// `(png_bytes, left, top, right, bottom)` in global coords, or `None`.
pub(crate) fn render(
    pts: &[points::Point],
    nominal_width: f32,
    color: Rgba,
    texture: u8,
) -> Option<(Vec<u8>, f32, f32, f32, f32)> {
    if pts.len() < 2 {
        return None;
    }
    let v2 = texture == 2; // separate grain path below

    struct Dab {
        cx: f32,
        cy: f32,
        size: i32,
        pscale: f32,
        pressure: f32,
    }
    let press_at = |i: usize| -> f32 { (pts[i].pressure as f32 / 4095.0).clamp(0.0, 1.0) };
    let pscale_of = |p: f32| (p / 3.0).sqrt() + 0.3;
    let size_at =
        |i: usize| crate::pen::charcoal_stamp_size(nominal_width, pts[i].tilt_x, pts[i].tilt_y);

    // Walk the polyline placing a stamp every 3 px of arc length,
    // interpolating x/y/size/pressure.
    let mut dabs: Vec<Dab> = Vec::new();
    let mut push = |cx, cy, size, pressure: f32| {
        dabs.push(Dab {
            cx,
            cy,
            size,
            pscale: pscale_of(pressure),
            pressure,
        })
    };
    push(pts[0].x, pts[0].y, size_at(0), press_at(0));
    let mut carry = 0.0f32;
    for i in 0..pts.len() - 1 {
        let (x0, y0) = (pts[i].x, pts[i].y);
        let (x1, y1) = (pts[i + 1].x, pts[i + 1].y);
        let seg = (x1 - x0).hypot(y1 - y0);
        if seg <= 0.0 {
            continue;
        }
        let mut d = DAB_SPACING - carry;
        while d <= seg {
            let f = d / seg;
            let cx = x0 + (x1 - x0) * f;
            let cy = y0 + (y1 - y0) * f;
            let size = (size_at(i) as f32 + (size_at(i + 1) as f32 - size_at(i) as f32) * f).round()
                as i32;
            let pressure = press_at(i) + (press_at(i + 1) - press_at(i)) * f;
            push(cx, cy, size.max(1), pressure);
            d += DAB_SPACING;
        }
        carry = seg - (d - DAB_SPACING);
    }

    // Bounding box (baked at 1 px/pt).
    let half = |s: i32| (s as f32) / 2.0 + 1.0;
    let mut minx = f32::INFINITY;
    let mut miny = f32::INFINITY;
    let mut maxx = f32::NEG_INFINITY;
    let mut maxy = f32::NEG_INFINITY;
    for d in &dabs {
        minx = minx.min(d.cx - half(d.size));
        miny = miny.min(d.cy - half(d.size));
        maxx = maxx.max(d.cx + half(d.size));
        maxy = maxy.max(d.cy + half(d.size));
    }
    let (l, t, r, b) = (minx.floor(), miny.floor(), maxx.ceil(), maxy.ceil());
    let (w, h) = (
        ((r - l) as i32).clamp(1, 8192),
        ((b - t) as i32).clamp(1, 8192),
    );
    let mut pm = Pixmap::new(w as u32, h as u32)?;

    let (cr, cg, cb) = (color.r, color.g, color.b);
    // RNG for the per-stamp grain-window jitter, seeded per stroke so different
    // strokes don't share identical grain (see `JavaRandom`).
    let mut rng = JavaRandom::new(
        pts.len() as u64 ^ ((pts[0].x.to_bits() as u64) << 20) ^ pts[0].y.to_bits() as u64,
    );

    let pixels = pm.pixels_mut();
    let ink = PremultipliedColorU8::from_rgba(cr, cg, cb, 255).unwrap();

    if v2 {
        // V2 (texture 2): white-noise grain masked by the elliptical disc, applied to
        // the stroke **region** — overlaps don't build up. Accumulate the max
        // coverage per pixel (the region envelope), then ONE white-noise decision
        // per pixel. `coverage = P · disc`. No value-noise field / dither.
        let mut env = vec![0.0f32; (w * h) as usize];
        for d in dabs.iter() {
            let s = d.size as usize;
            let hs = d.size as f32 / 2.0;
            let ox = (d.cx - hs - l).floor() as i32;
            let oy = (d.cy - hs - t).floor() as i32;
            for dy in 0..s {
                let py = oy + dy as i32;
                if py < 0 || py >= h {
                    continue;
                }
                for dx in 0..s {
                    let px = ox + dx as i32;
                    if px < 0 || px >= w {
                        continue;
                    }
                    let cov = coverage_v2(
                        d.pressure,
                        dx as f32 + 0.5 - hs,
                        dy as f32 + 0.5 - hs,
                        s as f32,
                    );
                    let idx = (py * w + px) as usize;
                    if cov > env[idx] {
                        env[idx] = cov;
                    }
                }
            }
        }
        for (idx, &cov) in env.iter().enumerate() {
            if cov > 0.0 && rng.next_float() < cov {
                pixels[idx] = ink;
            }
        }
    } else {
        // V1 (texture 1): bake each dab and blit it (union): build its size×size
        // `grainAlpha = pscale·grainTex·(textureBase·radial1 + 0.8·convex·radial2)
        // / 255`, Floyd–Steinberg dither it to a binary stamp, and OR it in.
        let mut alpha: Vec<f32> = Vec::new();
        for d in dabs.iter() {
            let s = d.size as usize;
            let hs = d.size as f32 / 2.0;
            let ox = (d.cx - hs - l).floor() as i32;
            let oy = (d.cy - hs - t).floor() as i32;
            // Per-stamp grain-window offset, so each dab shows a different window of
            // the noise field — overlapping dabs decorrelate (build the band up) and
            // the grain never repeats along the stroke.
            let (jx, jy) = grain_jitter(&mut rng, d.pressure);
            // grainAlpha buffer (0..=255), grain field sampled stamp-local at this window.
            alpha.clear();
            alpha.resize(s * s, 0.0);
            for dy in 0..s {
                for dx in 0..s {
                    let cov = coverage(
                        d.pscale,
                        dx as f32 + 0.5 - hs,
                        dy as f32 + 0.5 - hs,
                        s as f32,
                    );
                    if cov <= 0.0 {
                        continue;
                    }
                    // local coverage = cov / grain_mean · grainTex/255 (so its mean
                    // over the field is `cov`); the field gives the clumps.
                    let gx = (dx + jx) % GRAIN_DIM;
                    let gy = (dy + jy) % GRAIN_DIM;
                    let g = GRAIN[gy * GRAIN_DIM + gx] as f32 / 255.0;
                    alpha[dy * s + dx] = (cov / GRAIN_MEAN * g * 255.0).min(255.0);
                }
            }
            // Floyd–Steinberg dither (right 1/2, below-left 1/4, below 1/4 of
            // the error), inking where the result is on.
            for dy in 0..s {
                for dx in 0..s {
                    let i = dy * s + dx;
                    let old = alpha[i];
                    let on = old >= 128.0;
                    let err = old - if on { 255.0 } else { 0.0 };
                    if dx + 1 < s {
                        alpha[i + 1] += err * 0.5;
                    }
                    if dy + 1 < s {
                        if dx > 0 {
                            alpha[i + s - 1] += err * 0.25;
                        }
                        alpha[i + s] += err * 0.25;
                    }
                    if on {
                        let px = ox + dx as i32;
                        let py = oy + dy as i32;
                        if px >= 0 && px < w && py >= 0 && py < h {
                            pixels[(py * w + px) as usize] = ink;
                        }
                    }
                }
            }
        }
    }

    let png = pm.encode_png().ok()?;
    Some((png, l, t, r, b))
}
