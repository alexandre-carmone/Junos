//! Pure math for the focus HFR chart: the least-squares parabola drawn through
//! the V-curve samples, and "nice" axis ticks. Pure functions, so they are
//! unit-testable without a browser.

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub pos: f64,
    pub hfr: f64,
}

/// Round `raw` up to the next "nice" axis step on the 1 / 2 / 2.5 / 5 × 10^k
/// ladder, so tick labels read as round numbers rather than whatever the data
/// range happened to be. Non-finite or non-positive input yields `1.0`.
pub fn nice_step(raw: f64) -> f64 {
    if !(raw > 0.0) || !raw.is_finite() {
        return 1.0;
    }
    let pow = 10f64.powf(raw.log10().floor());
    let f = raw / pow; // 1.0 ..< 10.0
    let m = if f <= 1.0 {
        1.0
    } else if f <= 2.0 {
        2.0
    } else if f <= 2.5 {
        2.5
    } else if f <= 5.0 {
        5.0
    } else {
        10.0
    };
    m * pow
}

/// Nice axis bounds covering `[lo, hi]` in roughly `target` intervals.
/// Returns `(axis_min, axis_max, step)`, both bounds snapped outward to a
/// multiple of the step so the end gridlines land on labelled values.
pub fn nice_axis(lo: f64, hi: f64, target: usize) -> (f64, f64, f64) {
    let step = nice_step((hi - lo).max(1e-9) / target.max(1) as f64);
    ((lo / step).floor() * step, (hi / step).ceil() * step, step)
}

/// Least-squares parabola `y = a·x² + b·x + c` through `samples`.
/// Returns `(a, b, c)`; needs ≥3 points spanning ≥2 distinct x.
pub fn fit_parabola(samples: &[Sample]) -> Option<(f64, f64, f64)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_step_climbs_the_1_2_2p5_5_ladder() {
        assert_eq!(nice_step(0.8), 1.0);
        assert_eq!(nice_step(1.0), 1.0);
        assert_eq!(nice_step(1.3), 2.0);
        assert_eq!(nice_step(2.4), 2.5);
        assert_eq!(nice_step(4.0), 5.0);
        assert_eq!(nice_step(7.0), 10.0);
        // Scale-invariant across decades.
        assert!((nice_step(0.013) - 0.02).abs() < 1e-12);
        assert!((nice_step(130.0) - 200.0).abs() < 1e-9);
    }

    #[test]
    fn nice_step_rejects_degenerate_input() {
        assert_eq!(nice_step(0.0), 1.0);
        assert_eq!(nice_step(-5.0), 1.0);
        assert_eq!(nice_step(f64::NAN), 1.0);
        assert_eq!(nice_step(f64::INFINITY), 1.0);
    }

    #[test]
    fn nice_axis_snaps_both_bounds_outward() {
        let (lo, hi, step) = nice_axis(2.3, 5.9, 4);
        assert_eq!(step, 1.0);
        assert_eq!(lo, 2.0);
        assert_eq!(hi, 6.0);
        // Every bound is an exact multiple of the step.
        assert!((lo / step).fract().abs() < 1e-9);
        assert!((hi / step).fract().abs() < 1e-9);
        // And the original range is covered.
        assert!(lo <= 2.3 && hi >= 5.9);
    }

    #[test]
    fn nice_axis_survives_a_flat_range() {
        let (lo, hi, step) = nice_axis(3.0, 3.0, 4);
        assert!(step > 0.0 && step.is_finite());
        assert!(lo <= 3.0 && hi >= 3.0);
    }

    #[test]
    fn fit_recovers_parabola() {
        // Symmetric sweep of 7 points around a known vertex.
        let (vertex, curvature, base_hfr) = (12_345.0, 1e-4, 1.8);
        let s: Vec<Sample> = (-3..=3)
            .map(|k| {
                let pos = vertex + k as f64 * 50.0;
                Sample { pos, hfr: base_hfr + curvature * (pos - vertex).powi(2) }
            })
            .collect();
        let (a, b, _c) = fit_parabola(&s).unwrap();
        // Raw focuser positions make the normal equations ill-conditioned, so
        // the curvature is only good to a fraction of a percent.
        assert!(((a - curvature) / curvature).abs() < 1e-2, "a off: {a}");
        let v = -b / (2.0 * a);
        assert!((v - vertex).abs() < 1.0, "vertex off: {v}");
    }

    #[test]
    fn fit_needs_three_points() {
        let s = [Sample { pos: 0.0, hfr: 1.0 }, Sample { pos: 1.0, hfr: 2.0 }];
        assert!(fit_parabola(&s).is_none());
    }
}
