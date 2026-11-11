//! Dense linear algebra and the FFT the Gaussian-process guider rests on.
//!
//! PHD2's GP (`contributions/MPI_IS_gaussian_process`) uses Eigen for the
//! gram matrix, its `LDLT` factorisation and the solves, and Eigen's
//! `unsupported/FFT` (kissfft) for the period estimate. Neither library is
//! available here, so this file holds the smallest replacement: a row-major
//! `Mat`, Eigen's pivoted `LDLT` with its solves, a plain Cholesky and a 2×2
//! closed form for the tests, and a radix-2 FFT with kissfft's sign
//! convention.
//!
//! The LDLT is ported from the Eigen header installed on this machine (Eigen
//! 3.4.1, `Eigen/src/Cholesky/LDLT.h`); PHD2 does not vendor Eigen, it builds
//! against the system's. The port follows Eigen's summation order down to
//! its kernels as an x86-64 build runs them (SSE2 doubles, no FMA): the
//! factorisation's gemv and inner products, `triangular_solve_vector` for a
//! vector right-hand side, and `triangular_solve_matrix` for a matrix one,
//! whose order also depends on the result's storage: column-major for a
//! solve assigned to a `MatrixXd`, row-major for a `solve(M.transpose())`
//! nested in a product. So the rounding is the one a PHD2 guide log
//! reflects, bit for bit (the tests compare against Eigen itself), with one
//! exception: a matrix right-hand side over 120 rows, where Eigen cuts the
//! solve into depth blocks sized from the CPU's L1 cache (120 rows for
//! 32 KiB); the port keeps one block, as Eigen does up to 120 rows on any
//! x86-64. An FMA build (aarch64) rounds differently anyway.
//!
//! The solve's tolerance on D is 3.4's `numeric_limits::min()`; 3.3 used
//! `1/highest()` and 3.2 `max(max|D|·ε, 1/highest())`. The change is
//! unobservable here: the GP's diagonal is at least the measurement variance
//! (0.25 and up), far above any of these cutoffs.
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
        // LDLT.h:311 — nothing to eliminate; the sign bookkeeping is dropped
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

        // Partition (LDLT.h:347): A10 = row k, cols 0..k; A21 = col k, rows
        // k+1..n; A20 = the block below A10.
        let rs = n - k - 1;
        if k > 0 {
            // temp = D₀₀ · A10ᵀ, then A11 -= A10 · temp and A21 -= A20 · temp
            // (LDLT.h:358-361).
            for j in 0..k {
                temp[j] = m.get(j, j) * m.get(k, j);
            }
            // A10 · temp is an inner product over a strided row, which Eigen
            // sums in order from its first term (Redux.h:192).
            let dot = row_dot(&m, k, &temp[..k]);
            *m.at_mut(k, k) -= dot;
            if rs == 1 {
                // A one-row A20 makes the product an inner product too
                // (ProductEvaluators.h:378).
                let dot = row_dot(&m, k + 1, &temp[..k]);
                *m.at_mut(k + 1, k) -= dot;
            } else if rs > 1 {
                // Column-major gemv (GeneralMatrixVector.h:147): each row sums
                // a block of columns from zero, then subtracts it. Under 128
                // columns the block is all of them; past that it is 16 columns
                // wide, or 4 once a column spans 32,000 bytes (n ≥ 4000).
                let block = if k < 128 {
                    k
                } else if n * 8 < 32000 {
                    16
                } else {
                    4
                };
                let mut j2 = 0;
                while j2 < k {
                    let jend = (j2 + block).min(k);
                    for i in k + 1..n {
                        let mut acc = 0.0;
                        for j in j2..jend {
                            acc += m.get(i, j) * temp[j];
                        }
                        *m.at_mut(i, k) -= acc;
                    }
                    j2 = jend;
                }
            }
        }

        // LDLT.h:369 — the cutoff is 0, like LAPACK; only INF/NaN is avoided.
        let real_akk = m.get(k, k);
        let pivot_is_valid = real_akk.abs() > 0.0;

        if k == 0 && !pivot_is_valid {
            // The whole diagonal is zero (LDLT.h:371): identity transpositions,
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
            ret = false; // factorisation failed (LDLT.h:389)
        } else if !pivot_is_valid {
            found_zero_pivot = true;
        }
    }

    Ldlt { m, transpositions, success: ret }
}

/// `Σ m(r, j)·v[j]` summed in order from the first term, as Eigen's
/// non-vectorised `sum()` does. `v` is not empty.
fn row_dot(m: &Mat, r: usize, v: &[f64]) -> f64 {
    let mut s = m.get(r, 0) * v[0];
    for (j, x) in v.iter().enumerate().skip(1) {
        s += m.get(r, j) * x;
    }
    s
}

/// Panel width of Eigen's vector triangular solve
/// (`EIGEN_TUNE_TRIANGULAR_PANEL_WIDTH`, Settings.h:38).
const TRSV_PANEL: usize = 8;

/// Panel width of Eigen's matrix triangular solve, `max(mr, nr)` of the gebp
/// kernel (TriangularSolverMatrix.h:64): 4 and 4 for SSE2 doubles.
const TRSM_PANEL: usize = 4;

/// One row of Eigen's row-major gemv with SSE2 doubles
/// (GeneralMatrixVector.h:327): the terms go by pairs into two lanes, each
/// summed from zero, the lanes are added, then an odd last term.
fn gemv_row_sse2(term: impl Fn(usize) -> f64, len: usize) -> f64 {
    let pairs = len / 2 * 2;
    let (mut c0, mut c1) = (0.0, 0.0);
    for j in (0..pairs).step_by(2) {
        c0 += term(j);
        c1 += term(j + 1);
    }
    let mut cc = c0 + c1;
    for j in pairs..len {
        cc += term(j);
    }
    cc
}

/// Eigen's vectorised `sum()` with SSE2 doubles (Redux.h:227, an expression
/// so the aligned start is 0): a first pair of terms, then two pairs at a
/// time into two packets, the packets added, one more pair if left, the two
/// lanes added, then an odd last term. One term is returned as is. `len > 0`.
fn redux_sum_sse2(term: impl Fn(usize) -> f64, len: usize) -> f64 {
    if len < 2 {
        return term(0);
    }
    let size2 = len / 4 * 4;
    let size1 = len / 2 * 2;
    let (mut a0, mut a1) = (term(0), term(1));
    if size1 > 2 {
        let (mut b0, mut b1) = (term(2), term(3));
        for i in (4..size2).step_by(4) {
            a0 += term(i);
            a1 += term(i + 1);
            b0 += term(i + 2);
            b1 += term(i + 3);
        }
        a0 += b0;
        a1 += b1;
        if size1 > size2 {
            a0 += term(size2);
            a1 += term(size2 + 1);
        }
    }
    let mut res = a0 + a1;
    for i in size1..len {
        res += term(i);
    }
    res
}

/// One result of Eigen's gebp kernel: `depth` terms summed from zero, in
/// order. With `dual` (the 1pX4 micro kernel, GeneralBlockPanelKernel.h:1262:
/// a lone pair of rows times a 4-column panel), each run of 8 terms
/// alternates between two accumulators, added together before the terms
/// left over.
fn gebp_sum(term: impl Fn(usize) -> f64, depth: usize, dual: bool) -> f64 {
    let peeled = if dual { depth / 8 * 8 } else { 0 };
    let (mut c, mut d) = (0.0, 0.0);
    for k in (0..peeled).step_by(2) {
        c += term(k);
        d += term(k + 1);
    }
    let mut c = c + d;
    for k in peeled..depth {
        c += term(k);
    }
    c
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

    /// Eigen's `solve(b)` for a vector right-hand side (`LDLT::_solve_impl`,
    /// LDLT.h:574): `x = Pᵀ L⁻ᵀ D⁻¹ L⁻¹ P b`, with `D`'s pseudo-inverse —
    /// a row is zeroed where `|d| <= numeric_limits::min()` — and both
    /// triangular solves in `triangular_solve_vector`'s order.
    pub fn solve(&self, b: &[f64]) -> Vec<f64> {
        assert_eq!(b.len(), self.m.n_rows, "Ldlt::solve: length");
        let mut x = b.to_vec();
        self.permute(&mut x);
        self.forward_vector(&mut x);
        self.divide_by_d(&mut x);
        self.backward_vector(&mut x);
        self.unpermute(&mut x);
        x
    }

    /// Eigen's `solve(B)` into a column-major result: a solve assigned to a
    /// `MatrixXd` (`gamma` in `GP::predict`), or nested in a product with a
    /// column-major `B` (`A⁻¹H` in `GP::infer`). It goes through
    /// `triangular_solve_matrix`, whose order differs from the vector
    /// solve's, so a column of the result can differ from [`Ldlt::solve`] of
    /// that column in the last bits, as in PHD2.
    pub fn solve_mat(&self, b: &Mat) -> Mat {
        assert_eq!(b.n_rows, self.m.n_rows, "Ldlt::solve_mat: rows");
        let mut out = Mat::zeros(b.n_rows, b.n_cols);
        for c in 0..b.n_cols {
            // Eigen interleaves the columns, but each is solved on its own.
            let mut x = b.col(c);
            self.permute(&mut x);
            self.forward_matrix(&mut x);
            self.divide_by_d(&mut x);
            self.backward_matrix(&mut x);
            self.unpermute(&mut x);
            for (r, v) in x.into_iter().enumerate() {
                out.set(r, c, v);
            }
        }
        out
    }

    /// Eigen's `solve(B)` into a row-major result: what a
    /// `solve(M.transpose())` nested in a product evaluates to, the Solve's
    /// plain type following its transposed (row-major) right-hand side — as
    /// `GP::infer`'s `H · K⁻¹Hᵀ` (gaussian_process.cpp:218). A solve assigned
    /// straight to a `MatrixXd` is [`Ldlt::solve_mat`] instead. Eigen runs
    /// `triangular_solve_matrix` on the transposed problem then
    /// (TriangularSolverMatrix.h:200), in yet another order.
    pub fn solve_mat_row_major(&self, b: &Mat) -> Mat {
        assert_eq!(b.n_rows, self.m.n_rows, "Ldlt::solve_mat_row_major: rows");
        let cols = b.n_cols;
        // The gebp kernel takes the right-hand sides by fours, then a pair,
        // then one; only the pair sums in two accumulators.
        let quads = cols / 4 * 4;
        let pair_end = quads + (cols - quads) / 2 * 2;
        let mut out = Mat::zeros(b.n_rows, cols);
        for c in 0..cols {
            let dual = c >= quads && c < pair_end;
            let mut x = b.col(c);
            self.permute(&mut x);
            self.forward_right(&mut x, dual);
            self.divide_by_d(&mut x);
            self.backward_right(&mut x, dual);
            self.unpermute(&mut x);
            for (r, v) in x.into_iter().enumerate() {
                out.set(r, c, v);
            }
        }
        out
    }

    /// `x = P x`: the transpositions in order.
    fn permute(&self, x: &mut [f64]) {
        for (k, &t) in self.transpositions.iter().enumerate() {
            x.swap(k, t);
        }
    }

    /// `x = Pᵀ x`: the transpositions in reverse order.
    fn unpermute(&self, x: &mut [f64]) {
        for (k, &t) in self.transpositions.iter().enumerate().rev() {
            x.swap(k, t);
        }
    }

    /// `x = D⁺ x` (LDLT.h:595).
    fn divide_by_d(&self, x: &mut [f64]) {
        for (i, xi) in x.iter_mut().enumerate() {
            let d = self.m.get(i, i);
            if d.abs() > f64::MIN_POSITIVE {
                *xi /= d;
            } else {
                *xi = 0.0;
            }
        }
    }

    /// `x = L⁻¹ x`, `triangular_solve_vector` on the column-major unit lower
    /// `L` (TriangularSolverVector.h:89): panels of 8 columns, each solved by
    /// column updates that skip a zero `x[i]`, then taken off every row below
    /// by a column-major gemv (one sum per row, from zero).
    fn forward_vector(&self, x: &mut [f64]) {
        let n = x.len();
        let mut start = 0;
        while start < n {
            let end = (start + TRSV_PANEL).min(n);
            for i in start..end {
                let xi = x[i];
                if xi != 0.0 {
                    for s in i + 1..end {
                        x[s] -= xi * self.m.get(s, i);
                    }
                }
            }
            for r in end..n {
                let mut c = 0.0;
                for j in start..end {
                    c += self.m.get(r, j) * x[j];
                }
                x[r] -= c;
            }
            start = end;
        }
    }

    /// `x = L⁻ᵀ x`, `triangular_solve_vector` on `Lᵀ`, a row-major unit upper
    /// matrix (TriangularSolverVector.h:31): panels of 8 rows from the
    /// bottom; every row of a panel first loses a row-major gemv over the
    /// rows solved below the panel, then, in turn from the bottom, the
    /// vectorised sum over the panel rows solved before it.
    fn backward_vector(&self, x: &mut [f64]) {
        let n = x.len();
        let mut end = n;
        while end > 0 {
            let start = end.saturating_sub(TRSV_PANEL);
            if end < n {
                for i in start..end {
                    let c = gemv_row_sse2(|t| self.m.get(end + t, i) * x[end + t], n - end);
                    x[i] -= c;
                }
            }
            for i in (start..end - 1).rev() {
                let c = redux_sum_sse2(|t| self.m.get(i + 1 + t, i) * x[i + 1 + t], end - i - 1);
                x[i] -= c;
            }
            end = start;
        }
    }

    /// `x = L⁻¹ x` for one column, `triangular_solve_matrix` with a single
    /// depth block (TriangularSolverMatrix.h:48): panels of 4 columns, each
    /// solved by column updates (no zero skip), then taken off every row
    /// below by the gebp kernel (one sum per row, from zero).
    fn forward_matrix(&self, x: &mut [f64]) {
        let n = x.len();
        let mut start = 0;
        while start < n {
            let end = (start + TRSM_PANEL).min(n);
            for i in start..end {
                let xi = x[i];
                for s in i + 1..end {
                    x[s] -= xi * self.m.get(s, i);
                }
            }
            for r in end..n {
                let mut c = 0.0;
                for j in start..end {
                    c += self.m.get(r, j) * x[j];
                }
                x[r] -= c;
            }
            start = end;
        }
    }

    /// `x = L⁻ᵀ x` for one column, `triangular_solve_matrix` on the
    /// row-major `Lᵀ`: panels of 4 rows from the bottom; each row loses the
    /// sum, from zero, over the panel rows solved before it, then the gebp
    /// kernel takes the panel off every row above (one sum per row, from
    /// zero).
    fn backward_matrix(&self, x: &mut [f64]) {
        let mut end = x.len();
        while end > 0 {
            let start = end.saturating_sub(TRSM_PANEL);
            for i in (start..end).rev() {
                let mut b = 0.0;
                for s in i + 1..end {
                    b += self.m.get(s, i) * x[s];
                }
                x[i] -= b;
            }
            for r in 0..start {
                let mut c = 0.0;
                for j in start..end {
                    c += self.m.get(j, r) * x[j];
                }
                x[r] -= c;
            }
            end = start;
        }
    }

    /// `x = L⁻¹ x` for one column of a row-major result: Eigen solves
    /// `xᵀ Lᵀ = bᵀ` by `triangular_solve_matrix` on the right with a single
    /// depth block. Panels of 4 from the top; every panel row first loses
    /// the gebp sum over all the rows solved before the panel, then
    /// subtracts the panel rows solved before it one by one.
    fn forward_right(&self, x: &mut [f64], dual: bool) {
        let n = x.len();
        for start in (0..n).step_by(TRSM_PANEL) {
            let end = (start + TRSM_PANEL).min(n);
            let full = end - start == TRSM_PANEL;
            if start > 0 {
                for j in start..end {
                    let c = gebp_sum(|t| x[t] * self.m.get(j, t), start, dual && full);
                    x[j] -= c;
                }
            }
            for j in start..end {
                for t in start..j {
                    x[j] -= x[t] * self.m.get(j, t);
                }
            }
        }
    }

    /// `x = L⁻ᵀ x` for one column of a row-major result (`xᵀ L = bᵀ` on the
    /// right): panels of 4 aligned on the top, taken from the bottom; every
    /// panel row first loses the gebp sum over the rows solved below the
    /// panel, then, from the bottom, subtracts the panel rows solved before
    /// it one by one.
    fn backward_right(&self, x: &mut [f64], dual: bool) {
        let n = x.len();
        let mut start = (n.saturating_sub(1) / TRSM_PANEL) * TRSM_PANEL;
        loop {
            let end = (start + TRSM_PANEL).min(n);
            let full = end - start == TRSM_PANEL;
            if end < n {
                for j in start..end {
                    let c = gebp_sum(|t| x[end + t] * self.m.get(end + t, j), n - end, dual && full);
                    x[j] -= c;
                }
            }
            for j in (start..end).rev() {
                for t in j + 1..end {
                    x[j] -= x[t] * self.m.get(t, j);
                }
            }
            if start == 0 {
                break;
            }
            start -= TRSM_PANEL;
        }
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
        // The GP's gram matrix: K_ij = 20²·exp(−(i−j)²/(2·140²)) plus one
        // variance per point, as in PHD2's heteroscedastic GP; condition
        // number ≈ 1.5e5. The uneven diagonal makes the pivoting reorder.
        let n = 100;
        let mut k = Mat::from_fn(n, n, |i, j| {
            let d = i as f64 - j as f64;
            400.0 * (-(d * d) / (2.0 * 140.0 * 140.0)).exp()
        });
        k.add_diag(&(0..n).map(|i| 0.25 + 0.01 * (i % 7) as f64).collect::<Vec<_>>());
        let x: Vec<f64> = (0..n).map(|i| (i as f64 / 7.0).sin()).collect();
        let b = k.mul_vec(&x);
        let f = ldlt(&k);
        assert!(f.success());
        assert!(f.transpositions().iter().enumerate().any(|(k, &t)| t != k));
        let got = f.solve(&b);
        let max_x = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        for (g, e) in got.iter().zip(&x) {
            assert!((g - e).abs() / max_x < 1e-10, "{g} vs {e}");
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

    /// FNV-1a over the bit patterns: a golden's fingerprint of a vector.
    fn fnv(v: &[f64]) -> u64 {
        v.iter().fold(0xcbf2_9ce4_8422_2325, |h, x| (h ^ x.to_bits()).wrapping_mul(0x100_0000_01b3))
    }

    fn bits(v: &[f64]) -> Vec<u64> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    /// The goldens' gram matrix: rational quadratic, one variance per point,
    /// built from correctly rounded operations only so that the C++ harness
    /// starts from the same bits.
    fn golden_gram(n: usize) -> Mat {
        let mut k = Mat::from_fn(n, n, |i, j| {
            let d = i as f64 - j as f64;
            400.0 / (1.0 + d * d / 39200.0)
        });
        k.add_diag(&(0..n).map(|i| 0.25 + 0.01 * (i % 7) as f64).collect::<Vec<_>>());
        k
    }

    fn golden_rhs(n: usize, mul: usize, modulo: usize, div: f64, off: f64) -> Vec<f64> {
        (0..n).map(|i| ((i * mul) % modulo) as f64 / div - off).collect()
    }

    // The goldens below come from PHD2's C++ side: Eigen 3.4.1's `k.ldlt()`,
    // `vectorD()`, `solve(b)` and `solve([b c])` (an n×2 MatrixXd), built by
    // g++ 16 -O2 on x86-64 (SSE2, no FMA) in a throwaway harness. Hashes are
    // [`fnv`] over the bits.

    #[test]
    fn ldlt_matches_eigen_bit_for_bit() {
        // (n, vectorD, solve(b), solve([b c]) column 0, column 1); None: a
        // matrix solve over 120 rows depends on the CPU's L1 in Eigen.
        let cases: [(usize, u64, u64, Option<(u64, u64)>); 5] = [
            (3, 0x6f96_5377_5801_e9df, 0x6c9b_d5c9_ce09_e591, Some((0x6c9b_d5c9_ce09_e591, 0xb20e_bd8a_f7a4_a10f))),
            (9, 0x44f5_4103_7eab_31e4, 0xbedf_3c88_f8bc_8094, Some((0x04e8_f117_4b48_e794, 0x365e_c4c1_ece3_46d1))),
            (17, 0xf6f2_6143_787c_9864, 0xd305_2507_908c_8540, Some((0x5518_a945_d2d4_9747, 0x0123_6829_2250_75cf))),
            (100, 0x00cc_3bb1_8c53_05cc, 0x4afe_822d_3c80_b125, Some((0xb820_9032_ead8_b5bc, 0x8ffe_b4d2_5be1_1166))),
            // Past 127 columns the factorisation's gemv sums 16 at a time.
            (150, 0xd546_7eb0_7db4_ceac, 0xa252_aa17_95c7_3a4c, None),
        ];
        for (n, d_hash, x_hash, xm_hash) in cases {
            let f = ldlt(&golden_gram(n));
            assert!(f.success());
            let b = golden_rhs(n, 37, 101, 50.0, 1.0);
            assert_eq!(fnv(&f.vector_d()), d_hash, "n={n} vectorD");
            assert_eq!(fnv(&f.solve(&b)), x_hash, "n={n} solve");
            if let Some((h0, h1)) = xm_hash {
                let c = golden_rhs(n, 53, 97, 40.0, 1.2);
                let xm = f.solve_mat(&Mat::from_fn(n, 2, |r, k| if k == 0 { b[r] } else { c[r] }));
                assert_eq!((fnv(&xm.col(0)), fnv(&xm.col(1))), (h0, h1), "n={n} solve_mat");
            }
        }

        // n = 9 in full: Eigen's vector and matrix solves already differ in
        // the last bits (panels of 8 vs 4), and so does the port.
        let f = ldlt(&golden_gram(9));
        assert_eq!(f.transpositions(), &[6, 5, 4, 3, 4, 5, 8, 7, 8]);
        let b = golden_rhs(9, 37, 101, 50.0, 1.0);
        let x = [
            -1.9549595368859736,
            0.52940867837508798,
            2.8293620521173564,
            -2.2495025435481555,
            -0.012704867841058232,
            2.0749713723327861,
            -2.4880596360416596,
            -0.58007752423928738,
            1.8513977043988747,
        ];
        let xm0 = [
            -1.9549595368859736,
            0.52940867837508798,
            2.8293620521173564,
            -2.2495025435481559,
            -0.01270486784105801,
            2.0749713723327856,
            -2.4880596360416596,
            -0.58007752423928749,
            1.851397704398875,
        ];
        assert_eq!(bits(&f.solve(&b)), bits(&x));
        assert_eq!(bits(&f.solve_mat(&Mat::new(9, 1, b.clone())).col(0)), bits(&xm0));
        assert_eq!(
            ldlt(&golden_gram(17)).transpositions(),
            &[6, 13, 5, 12, 4, 11, 10, 12, 9, 11, 16, 11, 13, 15, 14, 15, 16]
        );
    }

    #[test]
    fn ldlt_row_major_solve_matches_eigen() {
        // Eigen solving into a row-major result (`RowMat x = f.solve(B)`,
        // the plain type of a `solve(M.transpose())` nested in a product):
        // one hash per right-hand side, for 1, 2, 3 and 5 of them. Only a
        // lone pair sums in two accumulators, so a column's bits depend on
        // how many there are.
        let cases: [(usize, &[u64]); 8] = [
            (17, &[0x0a64_b0ed_2d86_40a3]),
            (17, &[0x99f5_dcd8_07b1_df03, 0xa633_0e89_1490_7954]),
            (17, &[0x99f5_dcd8_07b1_df03, 0xa633_0e89_1490_7954, 0x0f8d_9256_4518_e6fb]),
            (
                17,
                &[
                    0x0a64_b0ed_2d86_40a3,
                    0x975e_4ef0_3375_9372,
                    0x0f8d_9256_4518_e6fb,
                    0x235d_f9af_ec79_a65e,
                    0x39c7_0f19_7dbf_69dd,
                ],
            ),
            (100, &[0xa93b_5bce_c896_4de2]),
            (100, &[0x2fa9_2451_245e_ba3f, 0xf7f2_a053_b5d2_0e1d]),
            (100, &[0x2fa9_2451_245e_ba3f, 0xf7f2_a053_b5d2_0e1d, 0x52c3_e522_6a53_3050]),
            (
                100,
                &[
                    0xa93b_5bce_c896_4de2,
                    0x5a48_608c_f5fa_1ac8,
                    0x52c3_e522_6a53_3050,
                    0x7a2e_4174_30c6_480c,
                    0x31f3_0cdc_520e_1fca,
                ],
            ),
        ];
        for (n, want) in cases {
            let f = ldlt(&golden_gram(n));
            let cols = want.len();
            let b = Mat::from_fn(n, cols, |i, c| ((i * (37 + 16 * c)) % 101) as f64 / 50.0 - 1.0);
            let x = f.solve_mat_row_major(&b);
            let got: Vec<u64> = (0..cols).map(|c| fnv(&x.col(c))).collect();
            assert_eq!(got, want, "n={n} cols={cols}");
        }

        // GP::infer's A = H · K⁻¹Hᵀ (H = [1; 5i]) as Eigen evaluates it.
        let h = Mat::from_fn(2, 100, |r, c| if r == 0 { 1.0 } else { c as f64 * 5.0 });
        let a = h.mul(&ldlt(&golden_gram(100)).solve_mat_row_major(&h.transpose()));
        let want = [0.0031477010871853771, 0.77851430595066606, 0.7785143059506634, 2380.5337303737788];
        assert_eq!(bits(&a.data), bits(&want));
    }

    #[test]
    fn ldlt_trend_2x2_matches_eigen() {
        // GP::infer's A = H K⁻¹ Hᵀ (as Eigen computed it, H = [1; 5i]) solved
        // against H, a 2×100 right-hand side: Eigen's `A^-1 H` rows, hashed.
        let a = Mat::new(
            2,
            2,
            vec![0.0031477010871853771, 0.77851430595066606, 0.7785143059506634, 2380.5337303737788],
        );
        let h = Mat::from_fn(2, 100, |r, c| if r == 0 { 1.0 } else { c as f64 * 5.0 });
        let s = ldlt(&a).solve_mat(&h);
        let row = |r: usize| (0..100).map(|c| s.get(r, c)).collect::<Vec<_>>();
        assert_eq!((fnv(&row(0)), fnv(&row(1))), (0x8c72_7520_1dcc_ef46, 0xb228_615e_96c5_73c2));
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
