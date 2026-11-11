//! The numerical helpers of the Gaussian-process guider: the pairwise square
//! distance the kernels are built on, the Hamming window and the power
//! spectrum (`math_tools`), and the three data steps `GaussianProcessGuider::
//! UpdateGP` runs before inference — regularising the raw measurements onto a
//! 5 s grid, detrending them, and estimating the worm period from their
//! spectrum.
//!
//! Only the pure parts of `gaussian_process_guider.cpp` live here; the
//! guider's ring buffer, timing and control law come with the guider itself.
//! Random sampling (`box_muller`, `generate_*_random_matrix`) is left out: it
//! only serves `GP::drawSample`, which the guider never calls.
//!
//! Ported from PHD2 `contributions/MPI_IS_gaussian_process/tools/math_tools.cpp`, `contributions/MPI_IS_gaussian_process/src/gaussian_process_guider.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;

use super::linalg::{Mat, ldlt, next_pow2, power_spectrum};

/// Width of one regularisation cell, seconds (`GRID_INTERVAL`,
/// gaussian_process_guider.cpp:56).
pub const GRID_INTERVAL: f64 = 5.0;

/// Most cells [`regularize_dataset`] keeps (`REGULAR_BUFFER_SIZE`,
/// gaussian_process_guider.cpp:54).
pub const REGULAR_BUFFER_SIZE: usize = 2048;

/// Smallest zero-padded length of the period estimate's FFT (`FFT_SIZE`,
/// gaussian_process_guider.cpp:55).
pub const FFT_SIZE: usize = 4096;

/// `math_tools::squareDistance` for one-dimensional points: the
/// `a.len() × b.len()` matrix of `(a_i − b_j)²`.
///
/// As in PHD2, both inputs are first centred on their pooled mean (which
/// keeps the squares small) and the distance is expanded as
/// `a² + b² − 2ab`; the cancellation can leave a tiny negative, which is
/// clamped to zero. The kernels always pass two distinct temporaries, so the
/// pooled-mean branch is the only one ever taken (math_tools.cpp:82).
pub fn square_distance(a: &[f64], b: &[f64]) -> Mat {
    let (na, nb) = (a.len(), b.len());
    // Eigen's rowwise().mean(): the sum over the size. Eigen sums in SIMD
    // packets, a plain loop may round the last bit differently.
    let mean_a = a.iter().sum::<f64>() / na as f64;
    let mean_b = b.iter().sum::<f64>() / nb as f64;
    let mean = na as f64 / (na + nb) as f64 * mean_a + nb as f64 / (nb + na) as f64 * mean_b;
    Mat::from_fn(na, nb, |i, j| {
        let (am, bm) = (a[i] - mean, b[j] - mean);
        let d2 = (am * am + bm * bm) - 2.0 * am * bm;
        // Eigen's `.max(0)` is std::max(d2, 0): a NaN stays NaN.
        if d2 < 0.0 { 0.0 } else { d2 }
    })
}

/// `math_tools::hamming_window`: `0.54 − 0.46·cos(2π·r)` over Eigen's
/// `LinSpaced(N, 0, 1)` — `r_i = i·(1/(N−1))`, the last one exactly 1, and a
/// single point at 0.
pub fn hamming_window(n: usize) -> Vec<f64> {
    let (alpha, beta) = (0.54, 0.46);
    let step = if n > 1 { 1.0 / (n - 1) as f64 } else { 0.0 };
    (0..n)
        .map(|i| {
            let r = if n > 1 && i == n - 1 { 1.0 } else { i as f64 * step };
            alpha - beta * (2.0 * PI * r).cos()
        })
        .collect()
}

/// `math_tools::compute_spectrum` (math_tools.cpp:174): the power spectrum
/// of `data` zero-padded to `Npad = nextpow2(max(n, data.len()))`, returned
/// as `(amplitudes, frequencies)` for the bins `k ∈ [ceil(Npad/N), Npad/2]`
/// with `f = k/Npad` in cycles per sample. Bins below `ceil(Npad/N)` only
/// exist because of the padding and are dropped.
///
/// Fewer than two samples give empty vectors: the lowest bin would then lie
/// above Nyquist, where PHD2 reads past the end of its FFT.
pub fn compute_spectrum(data: &[f64], n: usize) -> (Vec<f64>, Vec<f64>) {
    let n_data = data.len();
    if n_data < 2 {
        return (Vec::new(), Vec::new());
    }
    let npad = next_pow2(n.max(n_data));
    let power = power_spectrum(data, npad);
    // ceil(N / N_data) on doubles; exact as an integer ceiling for any
    // N · N_data < 2^53.
    let low_index = npad.div_ceil(n_data);
    let amplitudes = power[low_index..=npad / 2].to_vec();
    let frequencies = (low_index..=npad / 2).map(|k| k as f64 / npad as f64).collect();
    (amplitudes, frequencies)
}

/// The output of [`regularize_dataset`]: one entry per 5 s cell.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Regularized {
    /// Cell centres, seconds.
    pub timestamps: Vec<f64>,
    /// Mean gear error over each cell.
    pub gear_error: Vec<f64>,
    /// Mean measurement variance over each cell.
    pub variances: Vec<f64>,
}

/// [`regularize_dataset`] ran past the grid it sized from the last timestamp
/// while a dither was active — PHD2 throws `runtime_error("Index over-run in
/// regularize_dataset")` there, and the guider resets. `j` is the cell index
/// PHD2 logs (`PPDbg: Index-over-run in regularize_dataset, j = %d`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegularizeOverrun {
    pub j: usize,
}

/// `GaussianProcessGuider::regularize_dataset` (gaussian_process_guider.cpp:686):
/// resamples the raw (timestamp, gear error, variance) series onto a grid of
/// [`GRID_INTERVAL`] cells starting at t = 0, each holding the trapezoidal
/// integral of the series over the cell divided by the cell width, the cell
/// edges interpolated linearly.
///
/// Kept as PHD2 has them, since they shape the predictions a guide log shows:
/// - Inside a cell only `last_timestamp` advances, not the last gear error or
///   variance, so every trapezoid in a cell starts from the value at the
///   cell's left edge (and edge values interpolate from it too).
/// - The span between a cell edge and the point that crossed it is never
///   integrated; the next point's trapezoid starts at the edge.
/// - The first cell is centred at −2.5 s (edges start at −5 s), and the cell
///   still open after the last point is dropped.
/// - Past [`REGULAR_BUFFER_SIZE`] cells the **oldest** ones are kept
///   (`head(j)`), not the newest.
///
/// PHD2 sizes its output from the last timestamp, `ceil(t_last/5) + 1`, and
/// checks that bound only while dithering ([`RegularizeOverrun`]). Outside a
/// dither an earlier, larger timestamp (a negative dither offset) writes past
/// the end of its Eigen vectors, which is undefined behaviour; this port
/// keeps the extra cells instead.
pub fn regularize_dataset(
    timestamps: &[f64],
    gear_error: &[f64],
    variances: &[f64],
    dithering_active: bool,
) -> Result<Regularized, RegularizeOverrun> {
    assert_eq!(gear_error.len(), timestamps.len(), "regularize_dataset: gear_error length");
    assert_eq!(variances.len(), timestamps.len(), "regularize_dataset: variances length");
    let mut out = Regularized::default();
    let Some(&t_last) = timestamps.last() else {
        return Ok(out);
    };
    let grid_interval = GRID_INTERVAL;
    let mut last_cell_end = -grid_interval;
    let mut last_timestamp = -grid_interval;
    let mut last_gear_error = 0.0;
    let mut last_variance = 0.0;
    let mut gear_error_sum = 0.0;
    let mut variance_sum = 0.0;
    let grid_size = (t_last / grid_interval).ceil() as i64 + 1;

    for i in 0..timestamps.len() {
        if timestamps[i] < last_cell_end + grid_interval {
            gear_error_sum += (timestamps[i] - last_timestamp) * 0.5 * (last_gear_error + gear_error[i]);
            variance_sum += (timestamps[i] - last_timestamp) * 0.5 * (last_variance + variances[i]);
            last_timestamp = timestamps[i];
        } else {
            while timestamps[i] >= last_cell_end + grid_interval {
                let j = out.timestamps.len();
                if dithering_active && j as i64 >= grid_size {
                    return Err(RegularizeOverrun { j });
                }
                let inter_timestamp = last_cell_end + grid_interval;

                let proportion = (inter_timestamp - last_timestamp) / (timestamps[i] - last_timestamp);
                let inter_gear_error = proportion * gear_error[i] + (1.0 - proportion) * last_gear_error;
                let inter_variance = proportion * variances[i] + (1.0 - proportion) * last_variance;

                gear_error_sum += (inter_timestamp - last_timestamp) * 0.5 * (last_gear_error + inter_gear_error);
                variance_sum += (inter_timestamp - last_timestamp) * 0.5 * (last_variance + inter_variance);

                out.timestamps.push(last_cell_end + 0.5 * grid_interval);
                out.gear_error.push(gear_error_sum / grid_interval);
                out.variances.push(variance_sum / grid_interval);

                last_timestamp = inter_timestamp;
                last_gear_error = inter_gear_error;
                last_variance = inter_variance;
                last_cell_end = inter_timestamp;

                gear_error_sum = 0.0;
                variance_sum = 0.0;
            }
        }
    }
    // Parity quirk: keeps the oldest cells (gaussian_process_guider.cpp:750).
    out.timestamps.truncate(REGULAR_BUFFER_SIZE);
    out.gear_error.truncate(REGULAR_BUFFER_SIZE);
    out.variances.truncate(REGULAR_BUFFER_SIZE);
    Ok(out)
}

/// The detrending step of `UpdateGP` (gaussian_process_guider.cpp:189): a
/// ridge least-squares line `w0 + w1·t` (λ = 1e-3, LDLT solve) fitted to the
/// gear error and subtracted from it. Only the period estimate uses the
/// result; the GP itself models the trend explicitly.
pub fn detrend(timestamps: &[f64], gear_error: &[f64]) -> Vec<f64> {
    assert_eq!(gear_error.len(), timestamps.len(), "detrend: lengths");
    let n = timestamps.len();
    let features = Mat::from_fn(2, n, |r, c| if r == 0 { 1.0 } else { timestamps[c] });
    let mut normal = features.mul(&features.transpose());
    normal.add_diag(&[1e-3, 1e-3]);
    let weights = ldlt(&normal).solve(&features.mul_vec(gear_error));
    (0..n)
        .map(|i| gear_error[i] - (weights[0] + weights[1] * timestamps[i]))
        .collect()
}

/// `GaussianProcessGuider::EstimatePeriodLength` (gaussian_process_guider.cpp:582):
/// the period, seconds, of the strongest line in the Hamming-windowed
/// spectrum of `data` sampled at `time`, ignoring periods over 1500 s, refined
/// by a parabola through the peak and its two neighbours.
///
/// NaN with fewer than two samples (PHD2 would read out of range); the guider
/// then keeps its previous period, as PHD2 does for a NaN estimate.
pub fn estimate_period_length(time: &[f64], data: &[f64]) -> f64 {
    assert_eq!(data.len(), time.len(), "estimate_period_length: lengths");
    let n = data.len();
    if n < 2 {
        return f64::NAN;
    }
    let windowed: Vec<f64> = data.iter().zip(hamming_window(n)).map(|(d, w)| d * w).collect();
    let (mut amplitudes, mut frequencies) = compute_spectrum(&windowed, FFT_SIZE);

    // (t_end − t_begin) / (n − 1): the mean sample spacing.
    let dt = (time[n - 1] - time[0]) / (n - 1) as f64;
    for f in &mut frequencies {
        *f /= dt;
    }
    for (a, f) in amplitudes.iter_mut().zip(&frequencies) {
        if 1.0 / f > 1500.0 {
            *a = 0.0;
        }
    }
    1.0 / peak_frequency(&frequencies, &amplitudes)
}

/// The peak search of `EstimatePeriodLength`: the frequency of the largest
/// amplitude (the first of equal maxima, as Eigen's `maxCoeff`), refined by
/// a least-squares parabola through it and its neighbours on normalised
/// axes, unless it sits on an end of the spectrum or the three normalised
/// amplitudes agree within 1e-10.
fn peak_frequency(frequencies: &[f64], amplitudes: &[f64]) -> f64 {
    let mut max_index = 0;
    for i in 1..amplitudes.len() {
        if amplitudes[i] > amplitudes[max_index] {
            max_index = i;
        }
    }
    let mut max_frequency = frequencies[max_index];
    if max_index < frequencies.len() - 1 && max_index > 0 {
        let spread = (frequencies[max_index - 1] - frequencies[max_index + 1]).abs();
        let neighbours = [max_index - 1, max_index, max_index + 1];
        // Centred on the peak and scaled by the spread, for conditioning.
        let loc = neighbours.map(|i| (frequencies[i] - max_frequency) / spread);
        let dat = neighbours.map(|i| amplitudes[i] / amplitudes[max_index]);

        // Equal amplitudes make the regression unstable: keep the bin.
        let max = dat[1..].iter().fold(dat[0], |m, &v| if v > m { v } else { m });
        let min = dat[1..].iter().fold(dat[0], |m, &v| if v < m { v } else { m });
        if max - min < 1e-10 {
            return max_frequency;
        }

        // Rows loc², loc, 1 (std::pow(x, 2) is the correctly rounded x·x).
        let phi = Mat::from_fn(3, 3, |r, c| match r {
            0 => loc[c] * loc[c],
            1 => loc[c],
            _ => 1.0,
        });
        let w = ldlt(&phi.mul(&phi.transpose())).solve(&phi.mul_vec(&dat));
        // The vertex −w1/(2·w0), back on the frequency axis.
        max_frequency -= w[1] / (2.0 * w[0]) * spread;
    }
    max_frequency
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: &[f64], b: &[f64], tol: f64) {
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert!((x - y).abs() <= tol, "index {i}: {x} vs {y} (tol {tol})");
        }
    }

    /// 400 samples, 5 s apart (2000 s), of a 230.5 s cosine.
    fn worm_series() -> (Vec<f64>, Vec<f64>) {
        let t: Vec<f64> = (0..400).map(|i| i as f64 * 5.0).collect();
        let y = t.iter().map(|t| (2.0 * PI * t / 230.5).cos()).collect();
        (t, y)
    }

    #[test]
    fn square_distance_is_the_squared_difference() {
        let d = square_distance(&[1.0, 2.0, 4.0], &[0.0, 3.0]);
        assert_eq!((d.n_rows, d.n_cols), (3, 2));
        assert_close(&d.data, &[1.0, 4.0, 4.0, 1.0, 16.0, 1.0], 1e-12);
    }

    #[test]
    fn square_distance_clamps_the_cancellation_at_zero() {
        // a² + b² − 2ab for two points 1e-9 apart, far from the pooled mean,
        // comes out at −5.8e-11 before the clamp (checked in Python).
        let d = square_distance(&[591.562], &[591.562000001, -591.562]);
        assert_eq!(d.data[0], 0.0);
        assert!((d.data[1] - 1399782.3993760003).abs() < 1e-6);
    }

    #[test]
    fn hamming_window_endpoints_and_centre() {
        let w = hamming_window(5);
        // r = 0, .25, .5, .75, 1: 0.54 − 0.46·cos(2πr).
        assert_close(&w, &[0.08, 0.54, 1.0, 0.54, 0.08], 1e-15);
        assert_eq!(w[0], w[4], "the last point is exactly r = 1");
        // A single point sits at r = 0; none gives an empty window.
        assert_close(&hamming_window(1), &[0.08], 1e-15);
        assert!(hamming_window(0).is_empty());
    }

    #[test]
    fn compute_spectrum_drops_the_padding_bins_and_finds_the_line() {
        let (_, y) = worm_series();
        let (amplitudes, frequencies) = compute_spectrum(&y, FFT_SIZE);
        // Npad = 4096, lowest bin ceil(4096 / 400) = 11, up to 2048.
        assert_eq!(frequencies.len(), 2048 - 11 + 1);
        assert_eq!(amplitudes.len(), frequencies.len());
        assert_eq!(frequencies[0], 11.0 / 4096.0);
        assert_eq!(*frequencies.last().unwrap(), 0.5);
        let peak = (0..amplitudes.len()).fold(0, |m, i| if amplitudes[i] > amplitudes[m] { i } else { m });
        // Bin 89: 89/4096 cycles per sample, 0.17 % above 5/230.5.
        assert_eq!(peak, 89 - 11);
        let truth = 5.0 / 230.5;
        assert!((frequencies[peak] / truth - 1.0).abs() < 0.01);
        assert_eq!(compute_spectrum(&[1.0], FFT_SIZE), (Vec::new(), Vec::new()));
    }

    #[test]
    fn estimate_period_length_finds_the_worm() {
        let (t, y) = worm_series();
        let p = estimate_period_length(&t, &y);
        assert!((p / 230.5 - 1.0).abs() < 0.01, "{p}");
        // NumPy port of the same steps: 230.46434400040195.
        assert!((p - 230.46434400040195).abs() < 1e-6, "{p}");
    }

    #[test]
    fn estimate_period_length_edges() {
        let (t, _) = worm_series();
        // Alternating samples peak at Nyquist, the last bin: no refinement,
        // 1 / (0.5 / 5 s) = 10 s.
        let alt: Vec<f64> = (0..400).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        assert_eq!(estimate_period_length(&t, &alt), 10.0);
        // All-zero amplitudes: the first bin wins (Eigen's maxCoeff keeps the
        // first maximum) and its 1861.8 s period comes back even though
        // periods over 1500 s were zeroed — PHD2 does the same.
        let p = estimate_period_length(&t, &[0.0; 400]);
        assert!((p - 4096.0 * 5.0 / 11.0).abs() < 1e-9, "{p}");
        assert!(estimate_period_length(&[0.0], &[1.0]).is_nan());
    }

    #[test]
    fn peak_frequency_refines_by_parabola() {
        // Through (−0.5, 1/3), (0, 1), (0.5, 2/3) on the normalised axes the
        // vertex is at +1/12 of the 2 Hz spread: 11 + 1/6.
        let f = peak_frequency(&[10.0, 11.0, 12.0], &[1.0, 3.0, 2.0]);
        assert!((f - (11.0 + 1.0 / 6.0)).abs() < 1e-12, "{f}");
        // Symmetric neighbours keep the bin.
        assert!((peak_frequency(&[10.0, 11.0, 12.0], &[1.0, 3.0, 1.0]) - 11.0).abs() < 1e-12);
        // Neighbours within 1e-10 of the peak (normalised): no regression.
        let a = 5.0 - 1e-11;
        assert_eq!(peak_frequency(&[1.0, 2.0, 3.0, 4.0], &[a, 5.0, a, 0.0]), 2.0);
    }

    #[test]
    fn regularize_dataset_hand_trace() {
        // Traced by hand through gaussian_process_guider.cpp:686. The first
        // edge is t = 0, so the first cell is [−5, 0] centred at −2.5; the
        // open cell after t = 10 is dropped. Inside a cell the trapezoids
        // start from the left-edge value, not the previous point: over
        // [0, 5] the gear error ge = t averages 1.875, not 2.5 (parity quirk).
        let t = [0.0, 2.5, 5.0, 7.5, 10.0];
        let r = regularize_dataset(&t, &t, &[1.0; 5], false).unwrap();
        assert_eq!(r.timestamps, vec![-2.5, 2.5, 7.5]);
        assert_eq!(r.gear_error, vec![0.0, 1.875, 6.875]);
        // The variance starts at 0 at t = −5, so the first cell averages 0.5.
        assert_eq!(r.variances, vec![0.5, 1.0, 1.0]);
        assert_eq!(regularize_dataset(&[], &[], &[], true), Ok(Regularized::default()));
    }

    #[test]
    fn regularize_dataset_overrun_only_fails_while_dithering() {
        // The grid is sized from the last timestamp: ceil(3/5) + 1 = 2 cells,
        // but t = 12 crosses the edges 5 and 10.
        let t = [0.0, 12.0, 3.0];
        assert_eq!(regularize_dataset(&t, &[0.0; 3], &[1.0; 3], true), Err(RegularizeOverrun { j: 2 }));
        let r = regularize_dataset(&t, &[0.0; 3], &[1.0; 3], false).unwrap();
        assert_eq!(r.timestamps, vec![-2.5, 2.5, 7.5]);
    }

    #[test]
    fn regularize_dataset_keeps_the_oldest_cells() {
        // 2101 points 5 s apart close 2101 cells; the first 2048 are kept.
        let t: Vec<f64> = (0..=2100).map(|i| i as f64 * 5.0).collect();
        let r = regularize_dataset(&t, &t, &vec![1.0; t.len()], false).unwrap();
        assert_eq!(r.timestamps.len(), REGULAR_BUFFER_SIZE);
        assert_eq!(r.gear_error.len(), REGULAR_BUFFER_SIZE);
        assert_eq!(r.variances.len(), REGULAR_BUFFER_SIZE);
        assert_eq!(r.timestamps[0], -2.5);
        assert_eq!(r.timestamps[2047], -2.5 + 2047.0 * 5.0);
    }

    #[test]
    fn detrend_is_a_ridge_line_fit() {
        // ge = 3 + 2t: the ridge term (λ = 1e-3) shrinks the fit to
        // w = [2.99802…, 2.00016…], leaving small residuals (NumPy).
        let t = [0.0, 5.0, 10.0, 15.0];
        let ge = [3.0, 13.0, 23.0, 33.0];
        assert_close(
            &detrend(&t, &ge),
            &[0.00197860514361237, 0.00115920528034863, 0.00033980541708445, -0.00047959444617618],
            1e-13,
        );
    }
}
