//! The covariance functions of the Gaussian-process guider.
//!
//! Two sums of the same building blocks, over the time distance `d` between
//! two samples:
//! - `SE0(d) = sv0² · exp(−d² / (2·ℓ0²))`, a long-range square exponential
//!   (slow drift);
//! - `Per(d) = svP² · exp(−2·sin²(π·|d|/P) / ℓP²)`, the periodic kernel (the
//!   worm's periodic error, period `P`);
//! - `SE1(d) = sv1² · exp(−d² / (2·ℓ1²))`, a short-range square exponential.
//!
//! [`PeriodicSquareExponential2`] (SE0 + Per + SE1) is the inference kernel;
//! [`PeriodicSquareExponential`] (SE0 + Per) is the projection kernel the
//! prediction uses, so the short-range term is fitted but never extrapolated.
//! Parameters are stored as natural logs, PHD2's `hyperParameters` and its
//! one-element `extraParameters` (log P); the "signal variance" parameters
//! act as standard deviations, the kernel amplitude being their square.
//!
//! Ported from PHD2 `contributions/MPI_IS_gaussian_process/src/covariance_functions.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;

use super::linalg::Mat;
use super::math::square_distance;

/// PHD2's `PeriodicSquareExponential2`: SE0 + Per + SE1, the inference kernel.
#[derive(Clone, Debug, PartialEq)]
pub struct PeriodicSquareExponential2 {
    /// Logs of: SE0 length-scale, SE0 signal variance, periodic length-scale,
    /// periodic signal variance, SE1 length-scale, SE1 signal variance.
    pub hyper_parameters: [f64; 6],
    /// Log of the period length, seconds.
    pub extra_parameter: f64,
}

impl Default for PeriodicSquareExponential2 {
    /// PHD2's default constructor: zero logs, and a log period of `DBL_MAX`
    /// (an infinite period, so the periodic term is a constant).
    fn default() -> Self {
        PeriodicSquareExponential2 { hyper_parameters: [0.0; 6], extra_parameter: f64::MAX }
    }
}

impl PeriodicSquareExponential2 {
    /// The `x.len() × y.len()` covariance matrix (covariance_functions.cpp:139).
    pub fn evaluate(&self, x: &[f64], y: &[f64]) -> Mat {
        let h = &self.hyper_parameters;
        let ls_se0 = h[0].exp();
        let sv_se0 = (2.0 * h[1]).exp();
        let ls_p = h[2].exp();
        let sv_p = (2.0 * h[3]).exp();
        let ls_se1 = h[4].exp();
        let sv_se1 = (2.0 * h[5]).exp();
        let pl_p = self.extra_parameter.exp();

        // std::pow(ls, 2) is the correctly rounded ls·ls.
        let c_se0 = -0.5 / (ls_se0 * ls_se0);
        let c_se1 = -0.5 / (ls_se1 * ls_se1);
        let c_p = PI / pl_p;
        let mut k = square_distance(x, y);
        for v in &mut k.data {
            let d2 = *v;
            let s = (c_p * d2.sqrt()).sin() / ls_p;
            *v = sv_se0 * (c_se0 * d2).exp() + sv_p * (-2.0 * (s * s)).exp() + sv_se1 * (c_se1 * d2).exp();
        }
        k
    }
}

/// PHD2's `PeriodicSquareExponential`: SE0 + Per, the projection kernel.
#[derive(Clone, Debug, PartialEq)]
pub struct PeriodicSquareExponential {
    /// Logs of: SE0 length-scale, SE0 signal variance, periodic length-scale,
    /// periodic signal variance.
    pub hyper_parameters: [f64; 4],
    /// Log of the period length, seconds.
    pub extra_parameter: f64,
}

impl Default for PeriodicSquareExponential {
    /// PHD2's default constructor: zero logs, and a log period of `DBL_MAX`.
    fn default() -> Self {
        PeriodicSquareExponential { hyper_parameters: [0.0; 4], extra_parameter: f64::MAX }
    }
}

impl PeriodicSquareExponential {
    /// The `x.len() × y.len()` covariance matrix (covariance_functions.cpp:61).
    pub fn evaluate(&self, x: &[f64], y: &[f64]) -> Mat {
        let h = &self.hyper_parameters;
        let ls_se0 = h[0].exp();
        let sv_se0 = (2.0 * h[1]).exp();
        let ls_p = h[2].exp();
        let sv_p = (2.0 * h[3]).exp();
        let pl_p = self.extra_parameter.exp();

        let c_se0 = -0.5 / (ls_se0 * ls_se0);
        let c_p = PI / pl_p;
        let mut k = square_distance(x, y);
        for v in &mut k.data {
            let d2 = *v;
            let s = (c_p * d2.sqrt()).sin() / ls_p;
            *v = sv_se0 * (c_se0 * d2).exp() + sv_p * (-2.0 * (s * s)).exp();
        }
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PHD2's defaults (ℓ0 700, sv0 20, ℓP 10, svP 20, ℓ1 25, sv1 10, P 200)
    /// as `SetGPHyperparameters` hands them over: the periodic length-scale
    /// in standard notation, 4·sin(ℓP·π/P), then logs.
    fn defaults() -> ([f64; 6], f64) {
        let p: f64 = 200.0;
        let lp = 4.0 * (10.0 * PI / p).sin();
        ([700f64.ln(), 20f64.ln(), lp.ln(), 20f64.ln(), 25f64.ln(), 10f64.ln()], p.ln())
    }

    #[test]
    fn kernels_at_zero_distance_sum_the_amplitudes() {
        let (h, lp) = defaults();
        let k2 = PeriodicSquareExponential2 { hyper_parameters: h, extra_parameter: lp };
        // sv0² + svP² + sv1² = 400 + 400 + 100.
        assert!((k2.evaluate(&[3.0], &[3.0]).data[0] - 900.0).abs() < 1e-9);
        let k = PeriodicSquareExponential { hyper_parameters: [h[0], h[1], h[2], h[3]], extra_parameter: lp };
        assert!((k.evaluate(&[3.0], &[3.0]).data[0] - 800.0).abs() < 1e-9);
    }

    #[test]
    fn kernels_at_half_a_period() {
        let (h, lp) = defaults();
        let ls_p = h[2].exp();
        // d = P/2: sin(π/2) = 1, so the periodic term is at its minimum.
        let se0 = 400.0 * (-(100.0f64 * 100.0) / (2.0 * 700.0 * 700.0)).exp();
        let per = 400.0 * (-2.0 / (ls_p * ls_p)).exp();
        let se1 = 100.0 * (-(100.0f64 * 100.0) / (2.0 * 25.0 * 25.0)).exp();
        let k2 = PeriodicSquareExponential2 { hyper_parameters: h, extra_parameter: lp };
        let v = k2.evaluate(&[0.0], &[100.0]).data[0];
        assert!((v - (se0 + per + se1)).abs() < 1e-9, "{v}");
        assert!((v - 398.39209833585466).abs() < 1e-9, "{v}");
        let k = PeriodicSquareExponential { hyper_parameters: [h[0], h[1], h[2], h[3]], extra_parameter: lp };
        let v = k.evaluate(&[0.0], &[100.0]).data[0];
        assert!((v - (se0 + per)).abs() < 1e-9, "{v}");
        // A full period away the periodic term is back to its peak.
        let full = k.evaluate(&[0.0], &[200.0]).data[0];
        let se0_full = 400.0 * (-(200.0f64 * 200.0) / (2.0 * 700.0 * 700.0)).exp();
        assert!((full - (se0_full + 400.0)).abs() < 1e-9, "{full}");
    }

    #[test]
    fn kernel_matrix_is_symmetric() {
        let (h, lp) = defaults();
        let k2 = PeriodicSquareExponential2 { hyper_parameters: h, extra_parameter: lp };
        let x = [0.0, 5.0, 12.5, 40.0];
        let m = k2.evaluate(&x, &x);
        assert_eq!((m.n_rows, m.n_cols), (4, 4));
        for r in 0..4 {
            for c in 0..4 {
                assert!((m.get(r, c) - m.get(c, r)).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn default_kernels_have_a_constant_periodic_term() {
        // exp(DBL_MAX) = ∞: π/∞ = 0, so Per ≡ svP² = 1; both SE terms
        // (ℓ = 1) vanish far away.
        let v = PeriodicSquareExponential2::default().evaluate(&[0.0], &[1e3]).data[0];
        assert_eq!(v, 1.0);
        let v = PeriodicSquareExponential::default().evaluate(&[0.0], &[1e3]).data[0];
        assert_eq!(v, 1.0);
    }
}
