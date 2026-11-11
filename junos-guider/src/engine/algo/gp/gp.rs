//! PHD2's `GP` class, as the Gaussian-process guider configures it: the
//! [`PeriodicSquareExponential2`] inference kernel, the
//! [`PeriodicSquareExponential`] output projection, heteroscedastic noise
//! (one variance per sample) and an explicit linear trend `β0 + β1·t`.
//!
//! Inference factorises `K = k(X, X) + diag(var)` with Eigen's `LDLT` (not a
//! Cholesky: PHD2 uses `ldlt()` here), solves `α = K⁻¹y`, and fits the trend
//! by generalised least squares, `β = (H K⁻¹ Hᵀ)⁻¹ H α` with `H = [1; t]`.
//! A prediction at `x*` is `k*(x*, X)·α + Rᵀβ`, `R = [1; x*] − H K⁻¹ k*ᵀ`.
//!
//! Left out, as the guider never uses them: the homoscedastic noise path,
//! the configuration switches (no projection, no explicit trend), the
//! un-projected `predict`, the predictive variances and `drawSample`.
//!
//! Ported from PHD2 `contributions/MPI_IS_gaussian_process/src/gaussian_process.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use super::kernels::{PeriodicSquareExponential, PeriodicSquareExponential2};
use super::linalg::{Ldlt, Mat, ldlt};

/// The Gaussian process of the guider (PHD2 `GP` with explicit trend and
/// output projection enabled).
#[derive(Clone, Debug)]
pub struct Gp {
    /// Inference kernel (`covFunc_`).
    cov_func: PeriodicSquareExponential2,
    /// Prediction kernel (`covFuncProj_`).
    cov_func_proj: PeriodicSquareExponential,
    /// Log of the homoscedastic noise sd: stored and returned, unused here.
    log_noise_sd: f64,
    /// The selected training data: times, values, variances.
    data_loc: Vec<f64>,
    data_out: Vec<f64>,
    data_var: Vec<f64>,
    /// LDLT of the gram matrix `k(X, X) + diag(var)`.
    chol_gram_matrix: Ldlt,
    /// `K⁻¹ y`.
    alpha: Vec<f64>,
    /// `H`, 2 × n: a row of ones and the data times.
    feature_vectors: Mat,
    /// The trend's offset and slope.
    beta: Vec<f64>,
}

impl Gp {
    /// PHD2's `GP(covFunc)` followed by `enableExplicitTrend()` and
    /// `enableOutputProjection(...)` (gaussian_process_guider.cpp:71): default
    /// kernels, `log_noise_sd = −1e20`, no data.
    pub fn new() -> Gp {
        Gp {
            cov_func: PeriodicSquareExponential2::default(),
            cov_func_proj: PeriodicSquareExponential::default(),
            log_noise_sd: -1e20,
            data_loc: Vec::new(),
            data_out: Vec::new(),
            data_var: Vec::new(),
            chol_gram_matrix: ldlt(&Mat::zeros(0, 0)),
            alpha: Vec::new(),
            feature_vectors: Mat::zeros(2, 0),
            beta: Vec::new(),
        }
    }

    /// `GP::setHyperParameters` (gaussian_process.cpp:425): all logs,
    /// `[noise sd, θ0 … θ5, period]`. Re-infers when data is held. The
    /// projection kernel takes θ0 … θ3 and the period — PHD2 hands it all
    /// six θ and it reads the first four.
    pub fn set_hyper_parameters(&mut self, hyper_parameters: &[f64; 8]) {
        self.log_noise_sd = hyper_parameters[0];
        self.cov_func.hyper_parameters.copy_from_slice(&hyper_parameters[1..7]);
        self.cov_func.extra_parameter = hyper_parameters[7];
        if !self.data_loc.is_empty() {
            self.infer();
        }
        self.cov_func_proj.hyper_parameters.copy_from_slice(&hyper_parameters[1..5]);
        self.cov_func_proj.extra_parameter = hyper_parameters[7];
    }

    /// `GP::getHyperParameters` (gaussian_process.cpp:445): the logs
    /// `[noise sd, θ0 … θ5, period]`.
    pub fn get_hyper_parameters(&self) -> [f64; 8] {
        let mut out = [0.0; 8];
        out[0] = self.log_noise_sd;
        out[1..7].copy_from_slice(&self.cov_func.hyper_parameters);
        out[7] = self.cov_func.extra_parameter;
        out
    }

    /// `GP::inferSD` (gaussian_process.cpp:237), the subset-of-data
    /// approximation: keeps the `n` samples with the largest inference-kernel
    /// covariance to `prediction_point` (to the last sample when it is NaN),
    /// in decreasing covariance order, or all of them in their own order
    /// when there are no more than `n`, then infers.
    ///
    /// PHD2 orders with `std::sort`, whose order among equal covariances is
    /// unspecified; this keeps the earlier sample first.
    pub fn infer_sd(&mut self, data_loc: &[f64], data_out: &[f64], n: usize, data_var: &[f64], prediction_point: f64) {
        assert_eq!(data_out.len(), data_loc.len(), "infer_sd: data_out length");
        assert_eq!(data_var.len(), data_loc.len(), "infer_sd: data_var length");
        let prediction_loc = if prediction_point.is_nan() {
            // With no data PHD2 reads out of range here; nothing is kept anyway.
            data_loc.last().copied().unwrap_or(f64::NAN)
        } else {
            prediction_point
        };

        if n < data_loc.len() {
            let covariance = self.cov_func.evaluate(data_loc, &[prediction_loc]).data;
            let mut index: Vec<usize> = (0..data_loc.len()).collect();
            // Descending; total_cmp keeps the sort total if a NaN shows up.
            index.sort_by(|&a, &b| covariance[b].total_cmp(&covariance[a]));
            let keep = &index[..n];
            self.data_loc = keep.iter().map(|&i| data_loc[i]).collect();
            self.data_out = keep.iter().map(|&i| data_out[i]).collect();
            self.data_var = keep.iter().map(|&i| data_var[i]).collect();
        } else {
            self.data_loc = data_loc.to_vec();
            self.data_out = data_out.to_vec();
            self.data_var = data_var.to_vec();
        }
        self.infer();
    }

    /// `GP::infer` (gaussian_process.cpp:186), heteroscedastic with the
    /// explicit trend. With no data (PHD2 asserts in debug builds, and in
    /// release factorises an empty matrix) it does nothing:
    /// [`Gp::predict_projected`] answers zeros then either way.
    fn infer(&mut self) {
        if self.data_loc.is_empty() {
            return;
        }
        let mut gram = self.cov_func.evaluate(&self.data_loc, &self.data_loc);
        gram.add_diag(&self.data_var);
        self.chol_gram_matrix = ldlt(&gram);
        self.alpha = self.chol_gram_matrix.solve(&self.data_out);

        let n = self.data_loc.len();
        self.feature_vectors = Mat::from_fn(2, n, |r, c| if r == 0 { 1.0 } else { self.data_loc[c] });
        // A = H K⁻¹ Hᵀ (2×2), itself LDLT-solved: β = (A⁻¹ H) α, in that order.
        let feature_matrix =
            self.feature_vectors.mul(&self.chol_gram_matrix.solve_mat(&self.feature_vectors.transpose()));
        self.beta = ldlt(&feature_matrix).solve_mat(&self.feature_vectors).mul_vec(&self.alpha);
    }

    /// `GP::clearData` (gaussian_process.cpp:304): forgets the training
    /// data, so predictions are zero until the next inference.
    pub fn clear_data(&mut self) {
        self.chol_gram_matrix = ldlt(&Mat::zeros(0, 0));
        self.data_loc.clear();
        self.data_out.clear();
    }

    /// `GP::predictProjected` (gaussian_process.cpp:343) without variances:
    /// the projected posterior mean at each location, zeros with no data.
    pub fn predict_projected(&self, locations: &[f64]) -> Vec<f64> {
        if self.data_loc.is_empty() {
            return vec![0.0; locations.len()];
        }
        // GP::predict (gaussian_process.cpp:388).
        let mixed_cov = self.cov_func_proj.evaluate(locations, &self.data_loc);
        let mut m = mixed_cov.mul_vec(&self.alpha);
        let gamma = self.chol_gram_matrix.solve_mat(&mixed_cov.transpose());
        let h_gamma = self.feature_vectors.mul(&gamma);
        for (i, (mi, &x)) in m.iter_mut().zip(locations).enumerate() {
            // R = [1; x*] − H γ, then m += Rᵀ β.
            let r0 = 1.0 - h_gamma.get(0, i);
            let r1 = x - h_gamma.get(1, i);
            *mi += r0 * self.beta[0] + r1 * self.beta[1];
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// PHD2's default hyperparameters as `SetGPHyperparameters` passes them:
    /// noise sd 1, ℓ0 700, sv0 20, ℓP 10 → 4·sin(10π/200), svP 20, ℓ1 25,
    /// sv1 10, P 200, all as logs.
    fn defaults() -> [f64; 8] {
        let p: f64 = 200.0;
        let natural = [1.0, 700.0, 20.0, 4.0 * (10.0 * PI / p).sin(), 20.0, 25.0, 10.0, p];
        natural.map(f64::ln)
    }

    /// 20 samples, 5 s apart, of a drift plus a 200 s periodic error.
    fn series() -> (Vec<f64>, Vec<f64>) {
        let t: Vec<f64> = (0..20).map(|i| i as f64 * 5.0).collect();
        let y = t.iter().map(|t| 0.5 * t + 3.0 * (2.0 * PI * t / 200.0).sin()).collect();
        (t, y)
    }

    #[test]
    fn predicts_zero_without_data() {
        let mut gp = Gp::new();
        gp.set_hyper_parameters(&defaults());
        assert_eq!(gp.predict_projected(&[1.0, 2.0]), vec![0.0, 0.0]);
        let (t, y) = series();
        gp.infer_sd(&t, &y, 100, &[0.25; 20], 100.0);
        assert_ne!(gp.predict_projected(&[100.0])[0], 0.0);
        gp.clear_data();
        assert_eq!(gp.predict_projected(&[100.0]), vec![0.0]);
    }

    #[test]
    fn predicts_trend_plus_periodic_error() {
        let mut gp = Gp::new();
        gp.set_hyper_parameters(&defaults());
        let (t, y) = series();
        gp.infer_sd(&t, &y, 100, &[0.25; 20], 100.0);
        let m = gp.predict_projected(&[100.0, 50.0, 0.0]);
        // The next sample, t = 100: 0.5·100 + 3·sin(π) = 50.
        assert!((m[0] - 50.0).abs() < 0.3, "{}", m[0]);
        // NumPy (np.linalg.solve in place of the LDLT) on the same model.
        let numpy = [50.26564386119022, 27.550637799149023, 0.12632512093846948];
        for (a, b) in m.iter().zip(numpy) {
            assert!((a - b).abs() < 1e-8, "{a} vs {b}");
        }
    }

    #[test]
    fn recovers_a_pure_linear_trend() {
        // Kernel amplitudes of 1e-12 leave the explicit trend alone; its GLS
        // fit of an exact line is exact, far outside the data too.
        let mut h = defaults();
        for i in [2, 4, 6] {
            h[i] = 1e-6f64.ln();
        }
        let mut gp = Gp::new();
        gp.set_hyper_parameters(&h);
        let (t, _) = series();
        let y: Vec<f64> = t.iter().map(|t| 2.0 + 0.3 * t).collect();
        gp.infer_sd(&t, &y, 100, &[0.25; 20], f64::NAN);
        let m = gp.predict_projected(&[1000.0])[0];
        assert!((m - 302.0).abs() < 1e-6, "{m}");
    }

    #[test]
    fn infer_sd_keeps_the_most_covariant_points() {
        let mut gp = Gp::new();
        gp.set_hyper_parameters(&defaults());
        let (t, y) = series();
        // No prediction point: the reference is the last sample, which is
        // the only one kept with n = 1.
        gp.infer_sd(&t, &y, 1, &[0.25; 20], f64::NAN);
        assert_eq!(gp.data_loc, vec![95.0]);
        assert_eq!(gp.data_out, vec![y[19]]);
        // Toward t = 100, the three nearest, most covariant first.
        gp.infer_sd(&t, &y, 3, &[0.25; 20], 100.0);
        assert_eq!(gp.data_loc, vec![95.0, 90.0, 85.0]);
        assert_eq!(gp.data_var, vec![0.25; 3]);
        // n at or above the count keeps everything in order.
        gp.infer_sd(&t, &y, 20, &[0.25; 20], 0.0);
        assert_eq!(gp.data_loc, t);
    }

    #[test]
    fn set_hyper_parameters_reinfers_and_round_trips() {
        let (t, y) = series();
        let mut changed = defaults();
        changed[7] = 230.0f64.ln();
        // Infer, then change the period ...
        let mut a = Gp::new();
        a.set_hyper_parameters(&defaults());
        a.infer_sd(&t, &y, 100, &[0.25; 20], 100.0);
        a.set_hyper_parameters(&changed);
        // ... or change it first, then infer: the same model.
        let mut b = Gp::new();
        b.set_hyper_parameters(&changed);
        b.infer_sd(&t, &y, 100, &[0.25; 20], 100.0);
        assert_eq!(a.predict_projected(&[100.0, 105.0]), b.predict_projected(&[100.0, 105.0]));
        assert_eq!(a.get_hyper_parameters(), changed);
        assert_eq!(Gp::new().get_hyper_parameters()[0], -1e20);
    }
}
