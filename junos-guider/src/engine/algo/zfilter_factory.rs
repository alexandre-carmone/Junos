//! The ZFilter's design: a digital low-pass filter built the way A.J.
//! Fisher's *mkfilter* does it, as Ken Self ported it into PHD2.
//!
//! 1. The analogue prototype's poles in the s-plane: Bessel from mkfilter's
//!    table, Butterworth evenly spaced on the left half of the unit circle.
//! 2. Pre-warp the corner, `α = 1 / corner` (a period, in frames), to
//!    `tan(π·α) / π` so the bilinear transform puts it back where asked.
//! 3. Scale the poles by `2π · warped α`.
//! 4. Bilinear transform `z = (2 + s) / (2 − s)`; every zero goes to −1.
//! 5. Expand the zeros and poles into the numerator and denominator
//!    polynomials in z; the recurrence's coefficients are their
//!    coefficients over the denominator's leading one, and the DC gain is
//!    their ratio at z = 1.
//!
//! The filter it describes is
//! `y[n] = Σ_i xcoeffs[i]·x[n−i] + Σ_{i≥1} ycoeffs[i]·y[n−i]`,
//! with `x` scaled by 1 / gain.
//!
//! Left out because PHD2 never reaches them: the Chebyshev design (its
//! ripple member is never set) and the matched z-transform (`mzt`, off in
//! the only caller), and the centre-frequency and high-frequency gains,
//! which are computed and never read.
//!
//! Ported from PHD2 src/zfilterfactory.h, src/zfilterfactory.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;
use std::ops::{Add, Div, Mul, Neg, Sub};

const TWOPI: f64 = 2.0 * PI;

/// mkfilter's `bessel` output: the s-plane poles of the Bessel prototypes of
/// order 1 to 10, one member of each conjugate pair, order N starting at
/// index N²/4.
const BESSEL_POLES: [(f64, f64); 30] = [
    (-1.00000000000e+00, 0.00000000000e+00),
    (-1.10160133059e+00, 6.36009824757e-01),
    (-1.32267579991e+00, 0.00000000000e+00),
    (-1.04740916101e+00, 9.99264436281e-01),
    (-1.37006783055e+00, 4.10249717494e-01),
    (-9.95208764350e-01, 1.25710573945e+00),
    (-1.50231627145e+00, 0.00000000000e+00),
    (-1.38087732586e+00, 7.17909587627e-01),
    (-9.57676548563e-01, 1.47112432073e+00),
    (-1.57149040362e+00, 3.20896374221e-01),
    (-1.38185809760e+00, 9.71471890712e-01),
    (-9.30656522947e-01, 1.66186326894e+00),
    (-1.68436817927e+00, 0.00000000000e+00),
    (-1.61203876622e+00, 5.89244506931e-01),
    (-1.37890321680e+00, 1.19156677780e+00),
    (-9.09867780623e-01, 1.83645135304e+00),
    (-1.75740840040e+00, 2.72867575103e-01),
    (-1.63693941813e+00, 8.22795625139e-01),
    (-1.37384121764e+00, 1.38835657588e+00),
    (-8.92869718847e-01, 1.99832584364e+00),
    (-1.85660050123e+00, 0.00000000000e+00),
    (-1.80717053496e+00, 5.12383730575e-01),
    (-1.65239648458e+00, 1.03138956698e+00),
    (-1.36758830979e+00, 1.56773371224e+00),
    (-8.78399276161e-01, 2.14980052431e+00),
    (-1.92761969145e+00, 2.41623471082e-01),
    (-1.84219624443e+00, 7.27257597722e-01),
    (-1.66181024140e+00, 1.22110021857e+00),
    (-1.36069227838e+00, 1.73350574267e+00),
    (-8.65756901707e-01, 2.29260483098e+00),
];

/// The prototype (`FILTER_DESIGN`, without the unused Chebyshev).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Design {
    Bessel,
    Butterworth,
}

impl Design {
    /// `getname()`: the name the guide log's settings summary prints.
    pub fn name(self) -> &'static str {
        match self {
            Design::Bessel => "Bessel",
            Design::Butterworth => "Butterworth",
        }
    }
}

/// `ZFilterFactory`: the recurrence coefficients and DC gain of one design.
#[derive(Debug, Clone)]
pub struct ZFilterFactory {
    /// Feed-forward coefficients, `x[n]` first: the numerator over the
    /// denominator's leading coefficient.
    pub xcoeffs: Vec<f64>,
    /// Feedback coefficients, `y[n]` first (always −1, unused by the
    /// recurrence): minus the denominator over its leading coefficient.
    pub ycoeffs: Vec<f64>,
    design: Design,
    order: usize,
    raw_alpha1: f64,
    dc_gain: C64,
}

impl ZFilterFactory {
    /// Design a low-pass of `order` (Bessel: 1 ..= 10, the table's reach)
    /// with its corner at a period of `corner` frames. Refuses an order of
    /// 0 and a corner under 2 frames (the Nyquist limit) with PHD2's
    /// messages; a NaN corner passes, as in PHD2, and gives NaN everywhere.
    pub fn new(design: Design, order: usize, corner: f64) -> Result<Self, &'static str> {
        if order == 0 {
            return Err("invalid filter order");
        }
        if corner < 2.0 {
            return Err("invalid corner period multiplier");
        }
        let raw_alpha1 = 1.0 / corner;
        let spoles = splane(design, order);

        // prewarp() then normalize(); raw_alpha2 == raw_alpha1 in a low-pass.
        let warped_alpha1 = (PI * raw_alpha1).tan() / PI;
        let w1 = TWOPI * warped_alpha1;
        let spoles: Vec<C64> = spoles.into_iter().map(|p| p * w1).collect();

        // zplane(): the bilinear transform, and the zeros at −1 (the
        // prototype has none of its own).
        let zpoles: Vec<C64> = spoles.iter().map(|&p| (C64::real(2.0) + p) / (C64::real(2.0) - p)).collect();
        let zzeros = vec![C64::real(-1.0); zpoles.len()];

        // expandpoly()
        let top = expand(&zzeros);
        let bot = expand(&zpoles);
        let one = C64::real(1.0);
        let dc_gain = eval(&top, one) / eval(&bot, one);
        let lead = bot.last().expect("expand() returns len + 1 coefficients").re;
        let xcoeffs = top.iter().rev().map(|c| c.re / lead).collect();
        let ycoeffs = bot.iter().rev().map(|c| -(c.re / lead)).collect();
        Ok(ZFilterFactory { xcoeffs, ycoeffs, design, order, raw_alpha1, dc_gain })
    }

    /// `|H(1)|`, the gain at DC: the filter divides its input by it.
    pub fn gain(&self) -> f64 {
        self.dc_gain.im.hypot(self.dc_gain.re)
    }

    /// The corner period, recomputed as `1 / (1 / corner)` like PHD2 (it can
    /// differ from the requested corner in the last bit).
    pub fn corner(&self) -> f64 {
        1.0 / self.raw_alpha1
    }

    pub fn design(&self) -> Design {
        self.design
    }

    pub fn order(&self) -> usize {
        self.order
    }
}

/// `splane()`: the prototype's s-plane poles, left half-plane only
/// (`setpole`).
fn splane(design: Design, order: usize) -> Vec<C64> {
    let mut spoles = Vec::new();
    let mut setpole = |z: C64| {
        if z.re < 0.0 {
            spoles.push(z);
        }
    };
    match design {
        Design::Bessel => {
            let mut p = order * order / 4;
            if order & 1 == 1 {
                setpole(C64::from(BESSEL_POLES[p]));
                p += 1;
            }
            for _ in 0..order / 2 {
                let pole = C64::from(BESSEL_POLES[p]);
                setpole(pole);
                setpole(pole.conj());
                p += 1;
            }
        }
        Design::Butterworth => {
            for i in 0..2 * order {
                let theta = if order & 1 == 1 {
                    (i as f64 * PI) / order as f64
                } else {
                    ((i as f64 + 0.5) * PI) / order as f64
                };
                setpole(C64::polar(1.0, theta));
            }
        }
    }
    spoles
}

/// `expand()`: the polynomial `Π (z − w)` over `roots`, constant term first.
fn expand(roots: &[C64]) -> Vec<C64> {
    let mut coeffs = vec![C64::real(0.0); roots.len() + 1];
    coeffs[0] = C64::real(1.0);
    for &w in roots {
        multin(w, &mut coeffs);
    }
    // mkfilter exits when a coefficient comes out complex (|im| > 1e-10):
    // the roots were not conjugate pairs. splane() always pairs them; a NaN
    // corner gives NaN parts, which that test lets through.
    debug_assert!(!coeffs.iter().any(|c| c.im.abs() > 1e-10));
    coeffs
}

/// `multin()`: multiply the factor `(z − w)` into `coeffs`.
fn multin(w: C64, coeffs: &mut [C64]) {
    let nw = -w;
    for i in (1..coeffs.len()).rev() {
        coeffs[i] = (nw * coeffs[i]) + coeffs[i - 1];
    }
    coeffs[0] = nw * coeffs[0];
}

/// `eval()`: the polynomial at `z`, by Horner.
fn eval(coeffs: &[C64], z: C64) -> C64 {
    let mut sum = C64::real(0.0);
    for &c in coeffs.iter().rev() {
        sum = (sum * z) + c;
    }
    sum
}

/// The few `std::complex<double>` operations the design needs, computed the
/// way GCC compiles them so the coefficients match PHD2's bit for bit.
#[derive(Debug, Clone, Copy, PartialEq)]
struct C64 {
    re: f64,
    im: f64,
}

impl C64 {
    fn real(re: f64) -> C64 {
        C64 { re, im: 0.0 }
    }

    /// `std::polar`.
    fn polar(rho: f64, theta: f64) -> C64 {
        C64 { re: rho * theta.cos(), im: rho * theta.sin() }
    }

    fn conj(self) -> C64 {
        C64 { re: self.re, im: -self.im }
    }
}

impl From<(f64, f64)> for C64 {
    fn from((re, im): (f64, f64)) -> C64 {
        C64 { re, im }
    }
}

impl Add for C64 {
    type Output = C64;
    fn add(self, o: C64) -> C64 {
        C64 { re: self.re + o.re, im: self.im + o.im }
    }
}

impl Sub for C64 {
    type Output = C64;
    fn sub(self, o: C64) -> C64 {
        C64 { re: self.re - o.re, im: self.im - o.im }
    }
}

impl Neg for C64 {
    type Output = C64;
    fn neg(self) -> C64 {
        C64 { re: -self.re, im: -self.im }
    }
}

impl Mul for C64 {
    type Output = C64;
    fn mul(self, o: C64) -> C64 {
        C64 { re: self.re * o.re - self.im * o.im, im: self.re * o.im + self.im * o.re }
    }
}

/// A complex times a real scales both parts (no cross terms, as in C).
impl Mul<f64> for C64 {
    type Output = C64;
    fn mul(self, k: f64) -> C64 {
        C64 { re: self.re * k, im: self.im * k }
    }
}

/// libgcc's `__divdc3`, which GCC calls for a complex division: Smith's
/// algorithm, and C99 Annex G's recovery of a finite / zero quotient as an
/// infinity. That case is reachable: an absurd exp factor (≳ 1e75, which
/// set_algo_param accepts) puts every z-pole at 1, the denominator's DC
/// value is 0, and the gain comes out infinite, so PHD2's filter outputs 0
/// rather than NaN. libgcc's rescaling of subnormal or near-overflow
/// operands is left out: it only changes the last bits there.
impl Div for C64 {
    type Output = C64;
    fn div(self, o: C64) -> C64 {
        let (a, b, c, d) = (self.re, self.im, o.re, o.im);
        let (x, y) = if c.abs() < d.abs() {
            let ratio = c / d;
            let denom = (c * ratio) + d;
            (((a * ratio) + b) / denom, ((b * ratio) - a) / denom)
        } else {
            let ratio = d / c;
            let denom = (d * ratio) + c;
            (((b * ratio) + a) / denom, (b - (a * ratio)) / denom)
        };
        if x.is_nan() && y.is_nan() && c == 0.0 && d == 0.0 && (!a.is_nan() || !b.is_nan()) {
            let inf = f64::INFINITY.copysign(c);
            return C64 { re: inf * a, im: inf * b };
        }
        C64 { re: x, im: y }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn design_for(exp_factor: f64) -> ZFilterFactory {
        let corner = exp_factor * 4.0;
        let design = if corner < 6.0 { Design::Butterworth } else { Design::Bessel };
        ZFilterFactory::new(design, 4, corner).unwrap()
    }

    fn assert_close(got: &[f64], want: &[f64]) {
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-12, "{got:?} vs {want:?}");
        }
    }

    // The goldens below come from PHD2's C++: zfilterfactory.cpp compiled
    // with g++ 16.2 -O2 (x86-64), printed with %.17g.
    const ZEROS_AT_MINUS_ONE: [f64; 5] = [1.0, 4.0, 6.0, 4.0, 1.0];
    const GOLDENS: [(f64, [f64; 5], f64); 5] = [
        (
            1.0,
            [-1.0, 3.677613769070832e-16, -0.48602882206826942, 5.125525139104113e-17, -0.017664800872441887],
            10.640465421878606,
        ),
        (
            1.5,
            [-1.0, 0.29018301624228776, -0.28457218283092256, 0.048818121598412971, -0.0071976630824096815],
            16.793162773331016,
        ),
        (
            2.0,
            [-1.0, 1.0156103580077445, -0.61669279022878631, 0.18498498966908569, -0.023641293440889],
            36.385241258937739,
        ),
        (
            4.0,
            [-1.0, 2.3300930875256469, -2.1569887450397234, 0.92813007327884278, -0.15532039770701564],
            295.82526609360838,
        ),
        (
            20.0,
            [-1.0, 3.6355907563260215, -4.9654802217494227, 3.0193679248979421, -0.68964524204643018],
            95933.284987479157,
        ),
    ];

    #[test]
    fn coefficients_match_phd2() {
        for (exp_factor, y, gain) in GOLDENS {
            let f = design_for(exp_factor);
            assert_close(&f.xcoeffs, &ZEROS_AT_MINUS_ONE);
            assert_close(&f.ycoeffs, &y);
            assert!((f.gain() - gain).abs() < 1e-12 * gain, "{exp_factor}: {} vs {gain}", f.gain());
        }
    }

    // GCC's complex arithmetic, reproduced: not one bit apart. Pinned to
    // x86-64, where the goldens were made: tan / sin / cos come from the
    // platform's libm and may differ in the last bit elsewhere.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn coefficients_are_bit_exact() {
        for (exp_factor, y, gain) in GOLDENS {
            let f = design_for(exp_factor);
            assert_eq!(f.ycoeffs, y, "{exp_factor}");
            assert_eq!(f.gain(), gain, "{exp_factor}");
        }
    }

    #[test]
    fn design_choice_and_corner() {
        // Below a corner of 6 frames the Bessel prototype is swapped for
        // Butterworth by the caller; the factory just builds what it is told.
        assert_eq!(design_for(1.0).design(), Design::Butterworth);
        assert_eq!(design_for(1.5).design(), Design::Bessel);
        assert_eq!(design_for(2.0).design().name(), "Bessel");
        assert_eq!(Design::Butterworth.name(), "Butterworth");
        assert_eq!(design_for(2.0).order(), 4);
        assert_eq!(design_for(2.0).corner(), 8.0);
        // 1 / (1 / 30.8) is one ulp off 30.8 (PHD2 prints it with %.1f).
        assert_ne!(design_for(7.7).corner(), 30.8);
        assert!((design_for(7.7).corner() - 30.8).abs() < 1e-12);
    }

    #[test]
    fn dc_gain_is_the_recurrence_sum() {
        // At DC the recurrence's output over its input is
        // Σ xcoeffs / (1 − Σ_{i≥1} ycoeffs): the gain the filter divides by.
        for (exp_factor, ..) in GOLDENS {
            let f = design_for(exp_factor);
            let num: f64 = f.xcoeffs.iter().sum();
            let den = 1.0 - f.ycoeffs[1..].iter().sum::<f64>();
            assert!((num / den - f.gain()).abs() < 1e-9 * f.gain(), "{exp_factor}");
        }
    }

    #[test]
    fn bessel_poles_of_order_four() {
        // Table entries 4 and 5 and their conjugates, unscaled.
        let s = splane(Design::Bessel, 4);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0], C64 { re: -1.37006783055, im: 0.410249717494 });
        assert_eq!(s[1], C64 { re: -1.37006783055, im: -0.410249717494 });
        assert_eq!(s[2], C64 { re: -0.995208764350, im: 1.25710573945 });
        assert_eq!(s[3], C64 { re: -0.995208764350, im: -1.25710573945 });
        // Odd orders take the real pole first.
        let s = splane(Design::Bessel, 3);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], C64 { re: -1.32267579991, im: 0.0 });
        assert_eq!(s[1].im, 0.999264436281);
    }

    #[test]
    fn butterworth_poles_are_the_left_half_circle() {
        let s = splane(Design::Butterworth, 4);
        assert_eq!(s.len(), 4);
        for (k, p) in s.iter().enumerate() {
            let theta = ((k as f64 + 2.5) * PI) / 4.0;
            assert_eq!((p.re, p.im), (theta.cos(), theta.sin()));
        }
        // Odd order: θ = iπ/N, the real pole at π included, the right half
        // dropped.
        let s = splane(Design::Butterworth, 3);
        assert_eq!(s.len(), 3);
        assert_eq!(s[1], C64::polar(1.0, (3.0 * PI) / 3.0));
        assert!((s[1].re + 1.0).abs() < 1e-15);
    }

    #[test]
    fn expand_multiplies_out_the_roots() {
        // (z − 1)(z + 2)(z − i)(z + i) = z⁴ + z³ − z² + z − 2.
        let roots = [C64::real(1.0), C64::real(-2.0), C64 { re: 0.0, im: 1.0 }, C64 { re: 0.0, im: -1.0 }];
        let c = expand(&roots);
        let re: Vec<f64> = c.iter().map(|c| c.re).collect();
        assert_eq!(re, [-2.0, 1.0, -1.0, 1.0, 1.0]);
        assert_eq!(eval(&c, C64::real(2.0)), C64::real(20.0));
    }

    #[test]
    fn division_follows_libgcc() {
        let q = C64 { re: 1.0, im: 2.0 } / C64 { re: 3.0, im: -4.0 };
        assert_eq!(q, C64 { re: -0.2, im: 0.4 });
        // Finite over zero is an infinity, not NaN (Annex G).
        let q = C64::real(16.0) / C64::real(0.0);
        assert_eq!(q.re, f64::INFINITY);
        assert!(q.im.is_nan());
        assert_eq!(q.im.hypot(q.re), f64::INFINITY);
        assert!((C64::real(0.0) / C64::real(0.0)).re.is_nan());
    }

    #[test]
    fn refusals() {
        assert_eq!(ZFilterFactory::new(Design::Bessel, 0, 8.0).unwrap_err(), "invalid filter order");
        assert_eq!(ZFilterFactory::new(Design::Bessel, 4, 1.99).unwrap_err(), "invalid corner period multiplier");
        assert!(ZFilterFactory::new(Design::Bessel, 4, 2.0).is_ok());
        // NaN slips through the comparison, as in PHD2.
        assert!(ZFilterFactory::new(Design::Bessel, 4, f64::NAN).unwrap().gain().is_nan());
    }
}
