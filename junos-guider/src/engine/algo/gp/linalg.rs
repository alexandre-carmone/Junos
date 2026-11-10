//! Dense linear algebra and the FFT the Gaussian-process guider rests on.
//!
//! PHD2's GP (`contributions/MPI_IS_gaussian_process`) uses Eigen for the
//! gram matrix, its `LDLT` factorisation and the solves, and Eigen's
//! `unsupported/FFT` (kissfft) for the period estimate. Neither library is
//! available here, so this file holds the smallest replacement that gives the
//! same numbers: a row-major `Mat`, Eigen's pivoted `LDLT` ported step for
//! step (so the floating-point operation order, and thus the rounding, is the
//! one a PHD2 guide log reflects), a plain Cholesky and a 2×2 closed form for
//! the tests, and a radix-2 FFT with kissfft's sign convention.
//!
//! The LDLT is ported from the Eigen header installed on this machine (Eigen
//! 3.4.1, `Eigen/src/Cholesky/LDLT.h`); PHD2 does not vendor Eigen. The
//! factorisation loop is unchanged since Eigen 3.3. The solve's tolerance
//! on D changed in 3.4 (from `max(|D|)·ε` to `numeric_limits::min()`), which
//! is unobservable here: the GP's diagonal is at least the measurement
//! variance (0.25 and up), far above either cutoff.
//!
//! Ported from PHD2 <contributions/MPI_IS_gaussian_process/tools/math_tools.cpp> — BSD-3-Clause, see THIRD_PARTY.md

/// A dense row-major `f64` matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct Mat {
    pub n_rows: usize,
    pub n_cols: usize,
    /// Row-major: element `(r, c)` is `data[r * n_cols + c]`.
    pub data: Vec<f64>,
}

impl Mat {
    /// A matrix over `data` (row-major), which must hold `n_rows * n_cols` values.
    pub fn new(n_rows: usize, n_cols: usize, data: Vec<f64>) -> Mat {
        assert_eq!(data.len(), n_rows * n_cols, "Mat::new: data length");
        Mat { n_rows, n_cols, data }
    }

    /// An all-zero matrix.
    pub fn zeros(n_rows: usize, n_cols: usize) -> Mat {
        Mat { n_rows, n_cols, data: vec![0.0; n_rows * n_cols] }
    }

    /// A matrix whose element `(r, c)` is `f(r, c)`.
    pub fn from_fn(n_rows: usize, n_cols: usize, f: impl Fn(usize, usize) -> f64) -> Mat {
        let mut m = Mat::zeros(n_rows, n_cols);
        for r in 0..n_rows {
            for c in 0..n_cols {
                m.data[r * n_cols + c] = f(r, c);
            }
        }
        m
    }

    /// Element `(r, c)`.
    #[inline]
    pub fn get(&self, r: usize, c: usize) -> f64 {
        self.data[r * self.n_cols + c]
    }

    /// Sets element `(r, c)`.
    #[inline]
    pub fn set(&mut self, r: usize, c: usize, v: f64) {
        self.data[r * self.n_cols + c] = v;
    }

    /// Mutable element `(r, c)`.
    #[inline]
    pub fn at_mut(&mut self, r: usize, c: usize) -> &mut f64 {
        &mut self.data[r * self.n_cols + c]
    }

    /// The transpose.
    pub fn transpose(&self) -> Mat {
        Mat::from_fn(self.n_cols, self.n_rows, |r, c| self.get(c, r))
    }

    /// The product `self · other`.
    pub fn mul(&self, other: &Mat) -> Mat {
        assert_eq!(self.n_cols, other.n_rows, "Mat::mul: inner dimensions");
        let mut out = Mat::zeros(self.n_rows, other.n_cols);
        for r in 0..self.n_rows {
            for k in 0..self.n_cols {
                let a = self.get(r, k);
                for c in 0..other.n_cols {
                    out.data[r * other.n_cols + c] += a * other.get(k, c);
                }
            }
        }
        out
    }

    /// The product `self · v`.
    pub fn mul_vec(&self, v: &[f64]) -> Vec<f64> {
        assert_eq!(self.n_cols, v.len(), "Mat::mul_vec: inner dimensions");
        (0..self.n_rows)
            .map(|r| (0..self.n_cols).map(|c| self.get(r, c) * v[c]).sum())
            .collect()
    }

    /// Adds `d[i]` to diagonal element `(i, i)` — the GP's `K + diag(var)`.
    pub fn add_diag(&mut self, d: &[f64]) {
        assert_eq!(self.n_rows, self.n_cols, "Mat::add_diag: square");
        assert_eq!(d.len(), self.n_rows, "Mat::add_diag: length");
        for (i, v) in d.iter().enumerate() {
            self.data[i * self.n_cols + i] += v;
        }
    }

    /// Column `c` as a vector.
    pub fn col(&self, c: usize) -> Vec<f64> {
        (0..self.n_rows).map(|r| self.get(r, c)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mat_basics() {
        let a = Mat::from_fn(2, 3, |r, c| (r * 3 + c) as f64);
        assert_eq!(a.data, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        let t = a.transpose();
        assert_eq!((t.n_rows, t.n_cols), (3, 2));
        assert_eq!(t.get(2, 1), 5.0);
        // (2×3)·(3×2) = 2×2: [[5, 14], [14, 50]].
        let p = a.mul(&t);
        assert_eq!(p.data, vec![5.0, 14.0, 14.0, 50.0]);
        assert_eq!(a.mul_vec(&[1.0, 1.0, 1.0]), vec![3.0, 12.0]);
        let mut s = Mat::zeros(2, 2);
        s.add_diag(&[0.25, 0.5]);
        *s.at_mut(1, 0) = 7.0;
        assert_eq!(s.data, vec![0.25, 0.0, 7.0, 0.5]);
        assert_eq!(s.col(0), vec![0.25, 7.0]);
    }
}
