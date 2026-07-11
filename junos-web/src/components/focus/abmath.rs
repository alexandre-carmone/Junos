//! Pure math for the aberration inspector: per-tile V-curve fitting and the
//! tilt / backfocus formulas, reproduced from Ekos'
//! `kstars/ekos/focus/aberrationinspector.cpp` (`calcTilt`, `calcBackfocusDelta`).
//!
//! We fit each tile's HFR-vs-focuser-position points with a least-squares
//! **parabola** and take its vertex as the best-focus position. KStars uses a
//! hyperbola for Linear1Pass, but tilt depends only on the *differences* between
//! tile minima, where the curve-model choice largely cancels; a parabola vertex
//! is robust and dependency-free. All outputs are pure functions of the inputs
//! so they are unit-testable without a browser.

/// Tile index order matches KStars' `ImageMosaicMask`:
/// `TL=0 TM=1 TR=2  CL=3 CM=4 CR=5  BL=6 BM=7 BR=8`.
pub const CENTER: usize = 4;

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub pos: f64,
    pub hfr: f64,
}

/// Backfocus tile-selection mode (subset of KStars' modes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackfocusMode {
    /// Outer corners (tiles 0,2,6,8), unweighted — KStars `TILES_OUTER_CORNERS`.
    OuterCorners,
    /// All non-centre tiles, distance-weighted — KStars `TILES_ALL`.
    All,
}

/// Least-squares parabola `y = a·x² + b·x + c` through `samples`.
/// Returns `(a, b, c)`; needs ≥3 points spanning ≥2 distinct x.
fn fit_parabola(samples: &[Sample]) -> Option<(f64, f64, f64)> {
    let n = samples.len();
    if n < 3 { return None; }
    let (mut s0, mut s1, mut s2, mut s3, mut s4) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut t0, mut t1, mut t2) = (0.0, 0.0, 0.0);
    for s in samples {
        let x = s.pos;
        let x2 = x * x;
        s0 += 1.0; s1 += x; s2 += x2; s3 += x2 * x; s4 += x2 * x2;
        t0 += s.hfr; t1 += s.hfr * x; t2 += s.hfr * x2;
    }
    // Solve the 3×3 normal-equations system for (c, b, a):
    // | s0 s1 s2 | | c |   | t0 |
    // | s1 s2 s3 | | b | = | t1 |
    // | s2 s3 s4 | | a |   | t2 |
    let m = [[s0, s1, s2], [s1, s2, s3], [s2, s3, s4]];
    let rhs = [t0, t1, t2];
    let sol = solve3(m, rhs)?;
    Some((sol[2], sol[1], sol[0])) // (a, b, c)
}

/// Cramer's-rule solve of a 3×3 system; `None` if near-singular.
fn solve3(m: [[f64; 3]; 3], r: [f64; 3]) -> Option<[f64; 3]> {
    let det = det3(m);
    if det.abs() < 1e-9 { return None; }
    let mut out = [0.0; 3];
    for i in 0..3 {
        let mut mi = m;
        for row in 0..3 { mi[row][i] = r[row]; }
        out[i] = det3(mi) / det;
    }
    Some(out)
}

fn det3(m: [[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// Best-focus focuser position for a tile: parabola vertex `-b/(2a)`, clamped to
/// the sampled position range. `None` if the fit is invalid or opens downward
/// (`a ≤ 0` ⇒ no HFR minimum).
pub fn fit_tile_min(samples: &[Sample]) -> Option<f64> {
    let (a, b, _c) = fit_parabola(samples)?;
    if a <= 0.0 { return None; }
    let vertex = -b / (2.0 * a);
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for s in samples {
        lo = lo.min(s.pos);
        hi = hi.max(s.pos);
    }
    Some(vertex.clamp(lo, hi))
}

/// Mean of the available (`Some`) tile deltas among `idxs`; `None` if none.
fn avg_tiles(deltas: &[Option<f64>; 9], idxs: &[usize]) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0.0;
    for &i in idxs {
        if let Some(d) = deltas[i] { sum += d; count += 1.0; }
    }
    if count == 0.0 { None } else { Some(sum / count) }
}

#[derive(Clone, Debug, Default)]
pub struct TiltResult {
    /// Per-tile focus delta from centre, in microns: `(center − tile)·stepµm`.
    pub deltas: [Option<f64>; 9],
    pub lr_microns: f64,
    pub tb_microns: f64,
    pub diag_microns: f64,
    pub lr_pct: f64,
    pub tb_pct: f64,
    pub diag_pct: f64,
}

/// Geometry needed to convert focus deltas into a tilt percentage.
#[derive(Clone, Copy, Debug)]
pub struct TiltGeometry {
    pub step_microns: f64,
    pub sensor_w_px: f64,
    pub sensor_h_px: f64,
    pub tile_w_px: f64,
    pub pixel_um: f64,
}

/// Reproduce `AberrationInspector::calcTilt`. Requires a valid centre-tile
/// minimum and at least one tile in each edge group. Returns `None` otherwise.
pub fn calc_tilt(minima: &[Option<f64>; 9], geo: TiltGeometry) -> Option<TiltResult> {
    let center = minima[CENTER]?;
    let step = geo.step_microns;

    let mut deltas = [None; 9];
    for t in 0..9 {
        deltas[t] = minima[t].map(|m| (center - m) * step);
    }

    let av_left = avg_tiles(&deltas, &[0, 3, 6])?;
    let av_right = avg_tiles(&deltas, &[2, 5, 8])?;
    let av_top = avg_tiles(&deltas, &[0, 1, 2])?;
    let av_bottom = avg_tiles(&deltas, &[6, 7, 8])?;

    let lr_microns = av_left - av_right;
    let tb_microns = av_top - av_bottom;
    let diag_microns = lr_microns.hypot(tb_microns);

    let lr_span = (geo.sensor_w_px - geo.tile_w_px) * geo.pixel_um;
    let tb_span = (geo.sensor_h_px - geo.tile_w_px) * geo.pixel_um;
    let lr_pct = if lr_span.abs() > f64::EPSILON { lr_microns / lr_span * 100.0 } else { 0.0 };
    let tb_pct = if tb_span.abs() > f64::EPSILON { tb_microns / tb_span * 100.0 } else { 0.0 };
    let diag_pct = lr_pct.hypot(tb_pct);

    Some(TiltResult {
        deltas,
        lr_microns,
        tb_microns,
        diag_microns,
        lr_pct,
        tb_pct,
        diag_pct,
    })
}

/// Reproduce `AberrationInspector::calcBackfocusDelta`. `tile_centers` are the
/// tile centre offsets from the sensor centre in microns (only used for the
/// distance-weighted `All` mode). Returns backfocus in microns, or `None` if the
/// centre tile or the selected outer tiles are unavailable.
pub fn calc_backfocus(
    minima: &[Option<f64>; 9],
    step_microns: f64,
    mode: BackfocusMode,
    tile_centers: &[(f64, f64); 9],
) -> Option<f64> {
    let center = minima[CENTER]?;
    let (mut sum, mut counter) = (0.0, 0.0);
    match mode {
        BackfocusMode::OuterCorners => {
            for &i in &[0usize, 2, 6, 8] {
                if let Some(m) = minima[i] { sum += m; counter += 1.0; }
            }
        }
        BackfocusMode::All => {
            for i in 0..9 {
                if i == CENTER { continue; }
                if let Some(m) = minima[i] {
                    let (cx, cy) = tile_centers[i];
                    let dist = cx.hypot(cy);
                    sum += m * dist;
                    counter += dist;
                }
            }
        }
    }
    if counter == 0.0 { return None; }
    Some((center - sum / counter) * step_microns)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parabola_samples(vertex: f64, curvature: f64, base_hfr: f64) -> Vec<Sample> {
        // Symmetric sweep of 7 points around `vertex`.
        (-3..=3)
            .map(|k| {
                let pos = vertex + k as f64 * 50.0;
                let hfr = base_hfr + curvature * (pos - vertex).powi(2);
                Sample { pos, hfr }
            })
            .collect()
    }

    #[test]
    fn fit_recovers_vertex() {
        let s = parabola_samples(12_345.0, 1e-4, 1.8);
        let min = fit_tile_min(&s).unwrap();
        assert!((min - 12_345.0).abs() < 1.0, "vertex off: {min}");
    }

    #[test]
    fn downward_parabola_has_no_min() {
        let s: Vec<Sample> = (-3..=3)
            .map(|k| Sample { pos: k as f64, hfr: 10.0 - k as f64 * k as f64 })
            .collect();
        assert!(fit_tile_min(&s).is_none());
    }

    #[test]
    fn tilt_sign_left_high_right_low() {
        // Left column best-focus at HIGHER position than right column ⇒ the
        // left side of the sensor is nearer the focuser. Centre in between.
        let mut minima = [None; 9];
        let left = 10_100.0;
        let right = 9_900.0;
        let center = 10_000.0;
        for &i in &[0, 3, 6] { minima[i] = Some(left); }
        for &i in &[2, 5, 8] { minima[i] = Some(right); }
        for &i in &[1, 4, 7] { minima[i] = Some(center); }

        let geo = TiltGeometry {
            step_microns: 5.0,
            sensor_w_px: 6000.0,
            sensor_h_px: 4000.0,
            tile_w_px: 1500.0,
            pixel_um: 3.8,
        };
        let r = calc_tilt(&minima, geo).unwrap();
        // delta = (center - tile)*step. left delta = (10000-10100)*5 = -500.
        // right delta = (10000-9900)*5 = +500. LR = left - right = -1000 µm.
        assert!((r.lr_microns - (-1000.0)).abs() < 1e-6, "lr={}", r.lr_microns);
        // No top/bottom asymmetry.
        assert!(r.tb_microns.abs() < 1e-6);
        // Percentage uses span (6000-1500)*3.8 = 17100 µm ⇒ -1000/17100*100.
        assert!((r.lr_pct - (-1000.0 / 17_100.0 * 100.0)).abs() < 1e-6);
    }

    #[test]
    fn backfocus_center_vs_corners() {
        let mut minima = [None; 9];
        for &i in &[0, 2, 6, 8] { minima[i] = Some(10_050.0); }
        minima[CENTER] = Some(10_000.0);
        let centers = [(0.0, 0.0); 9];
        let bf = calc_backfocus(&minima, 4.0, BackfocusMode::OuterCorners, &centers).unwrap();
        // (10000 - 10050) * 4 = -200 µm.
        assert!((bf - (-200.0)).abs() < 1e-6, "bf={bf}");
    }

    #[test]
    fn tilt_needs_center_and_each_edge() {
        let minima = [None; 9]; // nothing fit
        let geo = TiltGeometry {
            step_microns: 5.0, sensor_w_px: 6000.0, sensor_h_px: 4000.0,
            tile_w_px: 1500.0, pixel_um: 3.8,
        };
        assert!(calc_tilt(&minima, geo).is_none());
    }
}
