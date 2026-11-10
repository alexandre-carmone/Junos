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

/// Eigen's `LDLT<MatrixXd>` (lower storage): `P A Pᵀ = L D Lᵀ` with the
/// diagonal pivoting of `ldlt_inplace<Lower>::unblocked`.
#[derive(Clone, Debug)]
pub struct Ldlt {
    /// Eigen's `m_matrix`: the strictly lower part holds `L` (unit diagonal
    /// implied), the diagonal holds `D`. The upper part is the untouched input.
    m: Mat,
    /// Eigen's `m_transpositions`: step `k` swapped row/column `k` with
    /// `transpositions[k]` (`>= k`).
    transpositions: Vec<usize>,
    /// Eigen's `info() == Success`.
    success: bool,
}

/// Factorises a symmetric matrix the way Eigen's `a.ldlt()` does. Only the
/// lower triangle of `a` is read, like Eigen's `LDLT<_, Lower>`.
///
/// Like Eigen, it always returns a factorisation: a zero pivot followed by a
/// non-zero one marks it `NumericalIssue` (`success() == false`) but PHD2
/// never checks `info()` and solves with it anyway, so neither do we.
pub fn ldlt(a: &Mat) -> Ldlt {
    assert_eq!(a.n_rows, a.n_cols, "ldlt: square");
    let n = a.n_rows;
    let mut m = a.clone();
    let mut transpositions = vec![0usize; n];
    let mut temp = vec![0.0; n];
    let mut found_zero_pivot = false;
    let mut ret = true;

    if n <= 1 {
        // LDLT.h:310 — nothing to eliminate; the sign bookkeeping is dropped
        // since nothing reads it.
        for (j, t) in transpositions.iter_mut().enumerate() {
            *t = j;
        }
        return Ldlt { m, transpositions, success: true };
    }

    for k in 0..n {
        // Find the largest |diagonal| in the trailing corner, k included
        // (LDLT.h:323). `maxCoeff` keeps the first of equal maxima.
        let mut biggest = k;
        let mut biggest_abs = m.get(k, k).abs();
        for i in k + 1..n {
            let v = m.get(i, i).abs();
            if v > biggest_abs {
                biggest_abs = v;
                biggest = i;
            }
        }
        transpositions[k] = biggest;
        if k != biggest {
            // Apply the transposition to the lower triangle only (LDLT.h:331).
            let s = n - biggest - 1;
            for j in 0..k {
                let (x, y) = (m.get(k, j), m.get(biggest, j));
                m.set(k, j, y);
                m.set(biggest, j, x);
            }
            for i in n - s..n {
                let (x, y) = (m.get(i, k), m.get(i, biggest));
                m.set(i, k, y);
                m.set(i, biggest, x);
            }
            let (x, y) = (m.get(k, k), m.get(biggest, biggest));
            m.set(k, k, y);
            m.set(biggest, biggest, x);
            for i in k + 1..biggest {
                let tmp = m.get(i, k);
                m.set(i, k, m.get(biggest, i));
                m.set(biggest, i, tmp);
            }
        }

        // Partition (LDLT.h:349): A10 = row k, cols 0..k; A21 = col k, rows
        // k+1..n; A20 = the block below A10.
        let rs = n - k - 1;
        if k > 0 {
            // temp = D₀₀ · A10ᵀ, then A11 -= A10 · temp and A21 -= A20 · temp.
            for j in 0..k {
                temp[j] = m.get(j, j) * m.get(k, j);
            }
            let mut dot = 0.0;
            for j in 0..k {
                dot += m.get(k, j) * temp[j];
            }
            *m.at_mut(k, k) -= dot;
            if rs > 0 {
                for i in k + 1..n {
                    let mut acc = 0.0;
                    for j in 0..k {
                        acc += m.get(i, j) * temp[j];
                    }
                    *m.at_mut(i, k) -= acc;
                }
            }
        }

        // LDLT.h:369 — the cutoff is 0, like LAPACK; only INF/NaN is avoided.
        let real_akk = m.get(k, k);
        let pivot_is_valid = real_akk.abs() > 0.0;

        if k == 0 && !pivot_is_valid {
            // The whole diagonal is zero (LDLT.h:376): identity transpositions,
            // success iff the matrix is zero.
            for (j, t) in transpositions.iter_mut().enumerate() {
                *t = j;
                for i in j + 1..n {
                    ret = ret && m.get(i, j) == 0.0;
                }
            }
            return Ldlt { m, transpositions, success: ret };
        }

        if rs > 0 && pivot_is_valid {
            for i in k + 1..n {
                *m.at_mut(i, k) /= real_akk;
            }
        } else if rs > 0 {
            for i in k + 1..n {
                ret = ret && m.get(i, k) == 0.0;
            }
        }

        if found_zero_pivot && pivot_is_valid {
            ret = false; // factorisation failed (LDLT.h:394)
        } else if !pivot_is_valid {
            found_zero_pivot = true;
        }
    }

    Ldlt { m, transpositions, success: ret }
}

impl Ldlt {
    /// Eigen's `info() == Success`.
    pub fn success(&self) -> bool {
        self.success
    }

    /// Eigen's `vectorD()`.
    pub fn vector_d(&self) -> Vec<f64> {
        (0..self.m.n_rows).map(|i| self.m.get(i, i)).collect()
    }

    /// Eigen's `matrixL()`: unit lower triangular.
    pub fn matrix_l(&self) -> Mat {
        let n = self.m.n_rows;
        Mat::from_fn(n, n, |r, c| {
            if r == c {
                1.0
            } else if r > c {
                self.m.get(r, c)
            } else {
                0.0
            }
        })
    }

    /// Eigen's `transpositionsP()`: step `k` swapped index `k` with this.
    pub fn transpositions(&self) -> &[usize] {
        &self.transpositions
    }

    /// Eigen's `solve(b)` for one right-hand side (`LDLT::_solve_impl`,
    /// LDLT.h:574): `x = Pᵀ L⁻ᵀ D⁻¹ L⁻¹ P b`, with `D`'s pseudo-inverse —
    /// a row is zeroed where `|d| <= numeric_limits::min()`.
    pub fn solve(&self, b: &[f64]) -> Vec<f64> {
        let n = self.m.n_rows;
        assert_eq!(b.len(), n, "Ldlt::solve: length");
        let mut x = b.to_vec();

        // x = P b: the transpositions in order.
        for k in 0..n {
            x.swap(k, self.transpositions[k]);
        }
        // x = L⁻¹ x (forward substitution, unit diagonal).
        for i in 0..n {
            let mut acc = x[i];
            for j in 0..i {
                acc -= self.m.get(i, j) * x[j];
            }
            x[i] = acc;
        }
        // x = D⁺ x.
        let tolerance = f64::MIN_POSITIVE;
        for i in 0..n {
            let d = self.m.get(i, i);
            if d.abs() > tolerance {
                x[i] /= d;
            } else {
                x[i] = 0.0;
            }
        }
        // x = L⁻ᵀ x (back substitution).
        for i in (0..n).rev() {
            let mut acc = x[i];
            for j in i + 1..n {
                acc -= self.m.get(j, i) * x[j];
            }
            x[i] = acc;
        }
        // x = Pᵀ x: the transpositions in reverse order.
        for k in (0..n).rev() {
            x.swap(k, self.transpositions[k]);
        }
        x
    }

    /// Eigen's `solve(B)` for a matrix right-hand side: one solve per column.
    pub fn solve_mat(&self, b: &Mat) -> Mat {
        assert_eq!(b.n_rows, self.m.n_rows, "Ldlt::solve_mat: rows");
        let mut out = Mat::zeros(b.n_rows, b.n_cols);
        for c in 0..b.n_cols {
            let x = self.solve(&b.col(c));
            for (r, v) in x.into_iter().enumerate() {
                out.set(r, c, v);
            }
        }
        out
    }
}

/// Plain Cholesky `A = L Lᵀ` (lower `L`), reading the lower triangle of `a`.
/// `None` when `a` is not positive definite. For the tests and for a
/// sample draw (`GP::drawSample`'s `llt()`), not for the guiding path.
pub fn cholesky(a: &Mat) -> Option<Mat> {
    assert_eq!(a.n_rows, a.n_cols, "cholesky: square");
    let n = a.n_rows;
    let mut l = Mat::zeros(n, n);
    for j in 0..n {
        let mut d = a.get(j, j);
        for k in 0..j {
            d -= l.get(j, k) * l.get(j, k);
        }
        if d <= 0.0 {
            return None;
        }
        let d = d.sqrt();
        l.set(j, j, d);
        for i in j + 1..n {
            let mut v = a.get(i, j);
            for k in 0..j {
                v -= l.get(i, k) * l.get(j, k);
            }
            l.set(i, j, v / d);
        }
    }
    Some(l)
}

/// Cramer's rule for a 2×2 system `a · x = b`; `None` when the determinant
/// is zero.
pub fn solve_2x2(a: [[f64; 2]; 2], b: [f64; 2]) -> Option<[f64; 2]> {
    let det = a[0][0] * a[1][1] - a[0][1] * a[1][0];
    if det == 0.0 {
        return None;
    }
    Some([
        (b[0] * a[1][1] - a[0][1] * b[1]) / det,
        (a[0][0] * b[1] - b[0] * a[1][0]) / det,
    ])
}

/// The smallest power of two `>= n` (1 for `n <= 1`). PHD2 computes it as
/// `pow(2, ceil(log(N) / log(2)))` (math_tools.cpp:184); the two agree for
/// every `N` the GP can pass (checked up to 70,000).
pub fn next_pow2(n: usize) -> usize {
    if n <= 1 { 1 } else { n.next_power_of_two() }
}

/// In-place forward FFT of a power-of-two length, iterative radix-2
/// Cooley–Tukey, unscaled: `X_k = Σ x_n e^{-2πi kn/N}` — the convention of
/// kissfft behind Eigen's `FFT::fwd` (and of FFTW / NumPy).
pub fn fft_radix2(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    assert_eq!(im.len(), n, "fft_radix2: lengths");
    assert!(n.is_power_of_two(), "fft_radix2: length must be a power of two");
    if n < 2 {
        return;
    }
    // Bit-reversal permutation.
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    // Butterflies, span doubling each stage; the twiddle for stage `len` is
    // e^{-2πi k/len}, recomputed from the angle (no running product, so the
    // rounding stays independent of the index).
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let step = -2.0 * std::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let (wr, wi) = ((step * k as f64).cos(), (step * k as f64).sin());
                let (a, b) = (start + k, start + k + half);
                let tr = wr * re[b] - wi * im[b];
                let ti = wr * im[b] + wi * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len *= 2;
    }
}

/// `|X_k|²` for `k in 0..=npad/2` of `data` zero-padded to `npad` (a power
/// of two `>= data.len()`) — the amplitudes `compute_spectrum`
/// (math_tools.cpp:174) slices its spectrum from.
pub fn power_spectrum(data: &[f64], npad: usize) -> Vec<f64> {
    assert!(npad >= data.len(), "power_spectrum: npad shorter than the data");
    let mut re = vec![0.0; npad];
    let mut im = vec![0.0; npad];
    re[..data.len()].copy_from_slice(data);
    fft_radix2(&mut re, &mut im);
    (0..=npad / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::rng::Rng;

    fn assert_close(a: &[f64], b: &[f64], tol: f64) {
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert!((x - y).abs() <= tol, "index {i}: {x} vs {y} (tol {tol})");
        }
    }

    fn spd4() -> Mat {
        Mat::new(
            4,
            4,
            vec![
                4.0, 1.0, 2.0, 0.5, //
                1.0, 3.0, 0.0, 1.0, //
                2.0, 0.0, 5.0, 1.0, //
                0.5, 1.0, 1.0, 2.0,
            ],
        )
    }

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

    #[test]
    fn ldlt_spd_4x4_matches_numpy() {
        let a = spd4();
        // x = [1, −2, 0.5, 3] → b = A·x (NumPy: [4.5, −2, 7.5, 5]).
        let b = [4.5, -2.0, 7.5, 5.0];
        let f = ldlt(&a);
        assert!(f.success());
        assert_close(&f.solve(&b), &[1.0, -2.0, 0.5, 3.0], 1e-12);
        // Step 0 pivots on the largest diagonal, 5 at index 2, so D[0] is
        // exactly 5 before any elimination.
        assert_eq!(f.transpositions()[0], 2);
        assert_eq!(f.vector_d()[0], 5.0);
        // All pivots positive for an SPD matrix.
        assert!(f.vector_d().iter().all(|&d| d > 0.0));
        // P A Pᵀ = L D Lᵀ, P applied as the transposition sequence.
        let n = 4;
        let mut perm: Vec<usize> = (0..n).collect();
        for k in 0..n {
            perm.swap(k, f.transpositions()[k]);
        }
        let paptt = Mat::from_fn(n, n, |r, c| a.get(perm[r], perm[c]));
        let l = f.matrix_l();
        let d = f.vector_d();
        let ld = Mat::from_fn(n, n, |r, c| l.get(r, c) * d[c]);
        let ldlt_m = ld.mul(&l.transpose());
        assert_close(&ldlt_m.data, &paptt.data, 1e-12);
        // solve_mat on the identity gives A⁻¹: A · A⁻¹ = I.
        let inv = f.solve_mat(&Mat::from_fn(n, n, |r, c| if r == c { 1.0 } else { 0.0 }));
        let id = a.mul(&inv);
        assert_close(&id.data, &Mat::from_fn(n, n, |r, c| if r == c { 1.0 } else { 0.0 }).data, 1e-12);
    }

    #[test]
    fn ldlt_ignores_the_upper_triangle() {
        // Eigen's LDLT<_, Lower> reads the lower triangle only: garbage above
        // the diagonal changes nothing.
        let mut a = spd4();
        a.set(0, 3, 99.0);
        a.set(1, 2, -42.0);
        let x = ldlt(&a).solve(&[4.5, -2.0, 7.5, 5.0]);
        assert_close(&x, &[1.0, -2.0, 0.5, 3.0], 1e-12);
    }

    #[test]
    fn ldlt_indefinite_3x3() {
        // Eigenvalues −3.41, 1.18, 2.23: not SPD, Cholesky fails, LDLT works.
        let a = Mat::new(3, 3, vec![2.0, 1.0, 0.0, 1.0, -3.0, 1.0, 0.0, 1.0, 1.0]);
        assert!(cholesky(&a).is_none());
        let f = ldlt(&a);
        assert!(f.success());
        // x = [1, −2, 3] → b = [0, 10, 1].
        assert_close(&f.solve(&[0.0, 10.0, 1.0]), &[1.0, -2.0, 3.0], 1e-12);
        // Pivot order: |−3| is the largest diagonal, so step 0 takes index 1
        // and D[0] = −3; one negative pivot in all (inertia 2+, 1−).
        assert_eq!(f.transpositions()[0], 1);
        assert_eq!(f.vector_d()[0], -3.0);
        assert_eq!(f.vector_d().iter().filter(|&&d| d < 0.0).count(), 1);
    }

    #[test]
    fn ldlt_kernel_100x100_recovers_x() {
        // The GP's gram matrix: K_ij = 20²·exp(−(i−j)²/(2·140²)) + diag(0.25),
        // condition number ≈ 1.5e5; NumPy recovers x to 1e-11.
        let n = 100;
        let mut k = Mat::from_fn(n, n, |i, j| {
            let d = i as f64 - j as f64;
            400.0 * (-(d * d) / (2.0 * 140.0 * 140.0)).exp()
        });
        k.add_diag(&vec![0.25; n]);
        let x: Vec<f64> = (0..n).map(|i| (i as f64 / 7.0).sin()).collect();
        let b = k.mul_vec(&x);
        let f = ldlt(&k);
        assert!(f.success());
        let got = f.solve(&b);
        let max_x = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        for (g, e) in got.iter().zip(&x) {
            assert!((g - e).abs() / max_x < 1e-6, "{g} vs {e}");
        }
        // Cholesky agrees with LDLT on an SPD matrix.
        let l = cholesky(&k).unwrap();
        let mut y = b.clone();
        for i in 0..n {
            for j in 0..i {
                y[i] -= l.get(i, j) * y[j];
            }
            y[i] /= l.get(i, i);
        }
        for i in (0..n).rev() {
            for j in i + 1..n {
                y[i] -= l.get(j, i) * y[j];
            }
            y[i] /= l.get(i, i);
        }
        assert_close(&y, &got, 1e-9);
    }

    #[test]
    fn ldlt_degenerate_cases() {
        // 1×1 and the zero matrix take Eigen's early returns.
        let one = ldlt(&Mat::new(1, 1, vec![4.0]));
        assert!(one.success());
        assert_eq!(one.solve(&[2.0]), vec![0.5]);
        let zero = ldlt(&Mat::zeros(3, 3));
        assert!(zero.success());
        assert_eq!(zero.transpositions(), &[0, 1, 2]);
        // D's pseudo-inverse: zero pivots give zero rows.
        assert_eq!(zero.solve(&[1.0, 2.0, 3.0]), vec![0.0, 0.0, 0.0]);
        // Zero diagonal with a non-zero below it: not "the zero matrix".
        let mut z = Mat::zeros(2, 2);
        z.set(1, 0, 1.0);
        assert!(!ldlt(&z).success());
        // A zero pivot followed by a valid one is NumericalIssue (LDLT.h:394):
        // diag(1, 0, 0) with a 1 at (2, 1).
        let mut a = Mat::zeros(3, 3);
        a.set(0, 0, 1.0);
        a.set(2, 1, 1.0);
        a.set(1, 2, 1.0);
        assert!(!ldlt(&a).success());
        // PSD diag(0, 0, 1): the zero pivots come last, success.
        let mut p = Mat::zeros(3, 3);
        p.set(2, 2, 1.0);
        let f = ldlt(&p);
        assert!(f.success());
        assert_eq!(f.transpositions(), &[2, 1, 2]);
        assert_eq!(f.solve(&[5.0, 5.0, 5.0]), vec![0.0, 0.0, 5.0]);
    }

    #[test]
    fn cholesky_and_2x2() {
        let a = spd4();
        let l = cholesky(&a).unwrap();
        let llt = l.mul(&l.transpose());
        assert_close(&llt.data, &a.data, 1e-12);
        assert_eq!(l.get(0, 0), 2.0);
        assert_eq!(l.get(0, 1), 0.0);
        // [[3, 1], [1, 2]] · x = [5, 4] → x = [1.2, 1.4].
        let x = solve_2x2([[3.0, 1.0], [1.0, 2.0]], [5.0, 4.0]).unwrap();
        assert_close(&x, &[1.2, 1.4], 1e-15);
        assert!(solve_2x2([[1.0, 2.0], [2.0, 4.0]], [1.0, 2.0]).is_none());
    }

    #[test]
    fn next_pow2_boundaries() {
        assert_eq!(next_pow2(0), 1);
        assert_eq!(next_pow2(1), 1);
        assert_eq!(next_pow2(2), 2);
        assert_eq!(next_pow2(3), 4);
        assert_eq!(next_pow2(4), 4);
        assert_eq!(next_pow2(4095), 4096);
        assert_eq!(next_pow2(4096), 4096);
        assert_eq!(next_pow2(4097), 8192);
        assert_eq!(next_pow2(8192), 8192);
    }

    #[test]
    fn fft_cosine_and_impulse() {
        // cos(2πn/8): X_1 = X_7 = N/2 = 4, everything else 0.
        let mut re: Vec<f64> = (0..8).map(|n| (2.0 * std::f64::consts::PI * n as f64 / 8.0).cos()).collect();
        let mut im = vec![0.0; 8];
        fft_radix2(&mut re, &mut im);
        let mut want = vec![0.0; 8];
        want[1] = 4.0;
        want[7] = 4.0;
        assert_close(&re, &want, 1e-14);
        assert_close(&im, &[0.0; 8], 1e-14);
        // An impulse has a flat spectrum: X_k = 1 for every k.
        let mut re = vec![0.0; 16];
        re[0] = 1.0;
        let mut im = vec![0.0; 16];
        fft_radix2(&mut re, &mut im);
        assert_eq!(re, vec![1.0; 16]);
        assert_eq!(im, vec![0.0; 16]);
        // Length 1 is the identity; length 2 is [a+b, a−b].
        let (mut r1, mut i1) = (vec![3.0], vec![0.0]);
        fft_radix2(&mut r1, &mut i1);
        assert_eq!(r1, vec![3.0]);
        let (mut r2, mut i2) = (vec![3.0, 1.0], vec![0.0, 0.0]);
        fft_radix2(&mut r2, &mut i2);
        assert_eq!(r2, vec![4.0, 2.0]);
    }

    #[test]
    fn fft_16_matches_numpy() {
        let data = [
            0.5, -1.25, 2.0, 0.75, -0.5, 1.5, -2.25, 0.25, 1.0, -0.75, 0.5, 2.5, -1.0, 0.0, 1.25, -0.25,
        ];
        let mut re = data.to_vec();
        let mut im = vec![0.0; 16];
        fft_radix2(&mut re, &mut im);
        let want_re = [
            4.25,
            0.8679332182349091,
            -1.7729707730091961,
            -1.4156088576361459,
            -1.5,
            -6.65545895422933,
            7.772970773009196,
            5.2031345936305655,
            -1.25,
            5.203134593630566,
            7.772970773009196,
            -6.65545895422933,
            -1.5,
            -1.4156088576361459,
            -1.7729707730091961,
            0.8679332182349091,
        ];
        let want_im = [
            0.0,
            1.1451834455009162,
            -3.323223304703363,
            1.8185427042818227,
            3.75,
            -2.009884420464367,
            3.676776695296637,
            -0.6832436792452733,
            0.0,
            0.6832436792452727,
            -3.676776695296637,
            2.0098844204643673,
            -3.75,
            -1.8185427042818219,
            3.323223304703363,
            -1.145183445500917,
        ];
        assert_close(&re, &want_re, 1e-13);
        assert_close(&im, &want_im, 1e-13);
    }

    #[test]
    fn fft_64_matches_naive_dft() {
        let mut rng = Rng::new(11);
        let n = 64;
        let x: Vec<f64> = (0..n).map(|_| rng.uniform(-1.0, 1.0)).collect();
        let mut re = x.clone();
        let mut im = vec![0.0; n];
        fft_radix2(&mut re, &mut im);
        for k in 0..n {
            let (mut sr, mut si) = (0.0, 0.0);
            for (i, v) in x.iter().enumerate() {
                let ang = -2.0 * std::f64::consts::PI * (k * i) as f64 / n as f64;
                sr += v * ang.cos();
                si += v * ang.sin();
            }
            assert!((re[k] - sr).abs() < 1e-9, "k={k} re {} vs {sr}", re[k]);
            assert!((im[k] - si).abs() < 1e-9, "k={k} im {} vs {si}", im[k]);
        }
    }

    #[test]
    fn power_spectrum_pads_and_halves() {
        // [1, 2, 3, 4, 5] padded to 8 (NumPy |X_k|² for k = 0..=4).
        let ps = power_spectrum(&[1.0, 2.0, 3.0, 4.0, 5.0], 8);
        assert_eq!(ps.len(), 5);
        assert_close(&ps, &[225.0, 81.76955262170047, 13.0, 8.23044737829953, 9.0], 1e-12);
        // No padding needed: the sum of the first power-of-two length.
        assert_eq!(power_spectrum(&[1.0, 1.0, 1.0, 1.0], 4), vec![16.0, 0.0, 0.0]);
    }
}
