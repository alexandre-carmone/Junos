//! ZFilter: a 4th-order digital low-pass over the uncorrected star path.
//!
//! The offsets the guider sees already include every correction made, so
//! the filter adds them back (`sum_corr`) to rebuild the drift the mount
//! would show without guiding, low-passes that, and returns how far the
//! filtered drift is from what has been corrected so far:
//!
//! ```text
//! x[n] = (input + sum_corr) / gain
//! y[n] = Σ_i xcoeff[i]·x[n−i] + Σ_{i≥1} ycoeff[i]·y[n−i]
//! r    = y[n] − sum_corr, 0 when |r| < min_move
//! sum_corr += r
//! ```
//!
//! The corner sits at a period of `exp_factor · 4` frames, with a Bessel
//! design from 6 frames up and a Butterworth one below. The design itself
//! is [`super::zfilter_factory`].
//!
//! Ported from PHD2 src/guide_algorithm_zfilter.h, src/guide_algorithm_zfilter.cpp — BSD-3-Clause, see THIRD_PARTY.md

use super::zfilter_factory::{Design, ZFilterFactory};
use super::{AlgoCtx, AlgoKind, GuideAlgorithm, ParamLookup};
use crate::engine::log::cfmt::fmt_f;

const DEFAULT_MIN_MOVE: f64 = 0.1;
const DEFAULT_EXP_FACTOR: f64 = 2.0;
const ORDER: usize = 4;

/// `GuideAlgorithmZFilter`.
#[derive(Debug, Clone)]
pub struct ZFilter {
    /// Filter inputs, newest first, as many as `xcoeffs`.
    xv: Vec<f64>,
    /// Filter outputs, newest first, as many as `ycoeffs`.
    yv: Vec<f64>,
    factory: ZFilterFactory,
    /// `factory.gain()`, taken once per build.
    gain: f64,
    min_move: f64,
    /// Every correction returned since the last reset.
    sum_corr: f64,
    exp_factor: f64,
}

impl ZFilter {
    /// Read `minMove` and `expFactor` through `params` (defaults 0.1, 2.0),
    /// each validated by its setter, which rebuilds the filter.
    pub fn new(params: ParamLookup) -> Self {
        let mut a = ZFilter {
            xv: Vec::new(),
            yv: Vec::new(),
            factory: design(DEFAULT_EXP_FACTOR),
            gain: 0.0,
            min_move: DEFAULT_MIN_MOVE,
            sum_corr: 0.0,
            exp_factor: DEFAULT_EXP_FACTOR,
        };
        a.set_min_move(params("minMove").unwrap_or(DEFAULT_MIN_MOVE));
        a.set_exp_factor(params("expFactor").unwrap_or(DEFAULT_EXP_FACTOR));
        a.reset();
        a
    }

    /// The corner in units of 4 frames, ≥ 1 (the UI offers 1 ..= 20).
    pub fn exp_factor(&self) -> f64 {
        self.exp_factor
    }

    /// `SetExpFactor`: under 1 is an error and the default (2.0) is taken.
    /// Either way the filter is rebuilt, which drops its history.
    /// `true` = error.
    pub fn set_exp_factor(&mut self, exp_factor: f64) -> bool {
        let err = exp_factor < 1.0;
        self.exp_factor = if err { DEFAULT_EXP_FACTOR } else { exp_factor };
        self.build_filter();
        err
    }

    /// `BuildFilter`: design for the current exp factor, then reset. PHD2
    /// re-checks the order and the exp factor here; both setters already
    /// guarantee them, so the checks are not repeated.
    fn build_filter(&mut self) {
        self.factory = design(self.exp_factor);
        self.gain = self.factory.gain();
        self.reset();
    }
}

/// The ZFilter's design for an exp factor: corner `exp_factor · 4`,
/// Butterworth under 6 frames, Bessel from there, order 4.
fn design(exp_factor: f64) -> ZFilterFactory {
    let corner = exp_factor * 4.0;
    let design = if corner < 6.0 { Design::Butterworth } else { Design::Bessel };
    ZFilterFactory::new(design, ORDER, corner)
        .expect("order 4 and a corner of at least 4 frames (or NaN) pass the factory's checks")
}

impl GuideAlgorithm for ZFilter {
    fn kind(&self) -> AlgoKind {
        AlgoKind::ZFilter
    }

    fn reset(&mut self) {
        self.xv = vec![0.0; self.factory.xcoeffs.len()];
        self.yv = vec![0.0; self.factory.ycoeffs.len()];
        self.sum_corr = 0.0;
    }

    fn result(&mut self, input: f64, _ctx: &AlgoCtx) -> f64 {
        // Shift the histories; the corrections already made are added back
        // so the filter sees the uncorrected waveform.
        self.xv.insert(0, (input + self.sum_corr) / self.gain);
        self.xv.pop();
        self.yv.insert(0, 0.0);
        self.yv.pop();

        let (xc, yc) = (&self.factory.xcoeffs, &self.factory.ycoeffs);
        for i in 0..xc.len() {
            self.yv[0] += self.xv[i] * xc[i];
        }
        for i in 1..yc.len() {
            self.yv[0] += self.yv[i] * yc[i];
        }
        // The move is what the filtered path asks for beyond what was done.
        let mut r = self.yv[0] - self.sum_corr;
        if r.abs() < self.min_move {
            r = 0.0;
        }
        self.sum_corr += r;
        r
    }

    fn min_move(&self) -> f64 {
        self.min_move
    }

    /// A negative min-move is an error and the default (0.1) is taken.
    /// Quirk kept: PHD2 rebuilds the filter here too, so changing the
    /// min-move also drops the filter's history.
    fn set_min_move(&mut self, min_move: f64) -> bool {
        let err = min_move < 0.0;
        self.min_move = if err { DEFAULT_MIN_MOVE } else { min_move };
        self.build_filter();
        err
    }

    fn param_names(&self) -> &'static [&'static str] {
        &["minMove", "expFactor"]
    }

    fn get_param(&self, name: &str) -> Option<f64> {
        match name {
            "minMove" => Some(self.min_move),
            "expFactor" => Some(self.exp_factor),
            _ => None,
        }
    }

    fn set_param(&mut self, name: &str, val: f64) -> bool {
        let err = match name {
            "minMove" => self.set_min_move(val),
            "expFactor" => self.set_exp_factor(val),
            _ => true,
        };
        !err
    }

    fn params(&self) -> Vec<(&'static str, f64)> {
        vec![("minMove", self.min_move), ("expFactor", self.exp_factor)]
    }

    /// Quirk kept: the exp factor printed is the factory's corner / 4, i.e.
    /// `1 / (1 / (4·exp_factor)) / 4`, not the stored value (they can
    /// differ in the last bit, never at one decimal).
    fn settings_summary(&self) -> String {
        format!(
            "Type={}-{}, Exp-factor={}, Minimum move = {}\n",
            self.factory.design().name(),
            self.factory.order(),
            fmt_f(self.factory.corner() / 4.0, 1),
            fmt_f(self.min_move, 3)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(exp_factor: f64, min_move: f64) -> ZFilter {
        ZFilter::new(&move |k| match k {
            "minMove" => Some(min_move),
            "expFactor" => Some(exp_factor),
            _ => None,
        })
    }

    fn run(a: &mut ZFilter, inputs: &[f64]) -> Vec<f64> {
        let ctx = AlgoCtx::default();
        inputs.iter().map(|&x| a.result(x, &ctx)).collect()
    }

    fn assert_close(got: &[f64], want: &[f64]) {
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-12, "{got:?} vs {want:?}");
        }
    }

    const INPUTS: [f64; 7] = [0.5, 0.5, -0.3, 1.0, 0.0, 0.0, 0.0];

    // The sequences below come from PHD2's C++: GuideAlgorithmZFilter's
    // BuildFilter / reset / result over zfilterfactory.cpp, g++ 16.2 -O2.

    #[test]
    fn bessel_sequence_matches_phd2() {
        let mut a = with(2.0, 0.1);
        let want = [0.0, 0.0, 0.20465472927768832, 0.0, 0.1429939260167313, 0.0, 0.1175892699715464];
        assert_close(&run(&mut a, &INPUTS), &want);
        assert!((a.sum_corr - 0.46523792526596602).abs() < 1e-12);
    }

    #[test]
    fn butterworth_sequence_matches_phd2() {
        let mut a = with(1.0, 0.1);
        let want = [
            0.0,
            0.23495212858448611,
            0.20600017298523016,
            0.0,
            0.12530346619507643,
            0.25682452893654362,
            0.0,
        ];
        assert_close(&run(&mut a, &INPUTS), &want);
        assert!((a.sum_corr - 0.82308029670133631).abs() < 1e-12);
    }

    #[test]
    fn open_loop_step_matches_phd2() {
        // A star held 1 px off whatever is done: the corrections pile up.
        let mut a = with(2.0, 0.0);
        let out = run(&mut a, &[1.0; 60]);
        assert!((out[59] - 0.2815533806064856).abs() < 1e-12);
        assert!((a.sum_corr - 16.57084848382808).abs() < 1e-12);
    }

    #[test]
    fn closed_loop_settles_on_the_star() {
        // The star starts 1 px off and each move takes it back: unit DC gain.
        let mut a = with(2.0, 0.0);
        let ctx = AlgoCtx::default();
        let want = [
            0.027483670999552824,
            0.1378473849414342,
            0.28795227621367414,
            0.32245657648337195,
            0.20224576496120938,
            0.056554206241767879,
            -0.014444405247271863,
            -0.019757318635423182,
        ];
        let mut pos = 1.0;
        for i in 0..60 {
            let m = a.result(pos, &ctx);
            if i < want.len() {
                assert!((m - want[i]).abs() < 1e-12, "{i}: {m}");
            }
            pos -= m;
        }
        assert!(pos.abs() < 1e-12);
        assert!((a.sum_corr - 1.0).abs() < 1e-12);
    }

    #[test]
    fn absurd_exp_factor_gives_no_moves() {
        // 1e308 · 4 overflows: every z-pole lands on 1, the gain is infinite
        // and PHD2 outputs 0 (not NaN).
        let mut a = with(1e308, 0.0);
        assert_eq!(a.gain, f64::INFINITY);
        assert_eq!(a.factory.ycoeffs, [-1.0, 4.0, -6.0, 4.0, -1.0]);
        assert_eq!(run(&mut a, &[0.5, 0.5, -0.3]), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn reset_clears_the_history() {
        let mut a = with(2.0, 0.0);
        let first = run(&mut a, &INPUTS);
        a.reset();
        assert_eq!(run(&mut a, &INPUTS), first);
        assert_eq!(a.xv.len(), 5);
        assert_eq!(a.yv.len(), 5);
    }

    #[test]
    fn setters_rebuild_and_reset() {
        let mut a = with(2.0, 0.0);
        let first = run(&mut a, &INPUTS);
        // Setting the same min-move rebuilds the filter and drops the history.
        assert!(!a.set_min_move(0.0));
        assert_eq!(a.sum_corr, 0.0);
        assert_eq!(run(&mut a, &INPUTS), first);
        assert!(!a.set_exp_factor(2.0));
        assert_eq!(run(&mut a, &INPUTS), first);
    }

    #[test]
    fn setters_validate_like_phd2() {
        let mut a = with(2.0, 0.1);
        assert!(!a.set_param("expFactor", 0.99));
        assert_eq!(a.exp_factor(), 2.0);
        assert!(a.set_param("expFactor", 1.0));
        assert_eq!(a.get_param("expFactor"), Some(1.0));
        assert_eq!(a.factory.design(), Design::Butterworth);
        assert!(a.set_param("expFactor", 1.5));
        assert_eq!(a.factory.design(), Design::Bessel);
        assert!(!a.set_param("minMove", -0.1));
        assert_eq!(a.min_move(), 0.1);
        assert!(a.set_param("minMove", 0.0));
        assert_eq!(a.get_param("minMove"), Some(0.0));
        assert!(!a.set_param("aggression", 1.0));
        assert_eq!(a.get_param("aggression"), None);
        assert_eq!(a.param_names(), ["minMove", "expFactor"]);
        assert_eq!(a.params(), vec![("minMove", 0.0), ("expFactor", 1.5)]);
    }

    #[test]
    fn construction_reads_and_validates_the_profile() {
        let a = with(0.5, -1.0);
        assert_eq!((a.min_move(), a.exp_factor()), (0.1, 2.0));
        let a = ZFilter::new(&|_| None);
        assert_eq!((a.min_move(), a.exp_factor()), (0.1, 2.0));
        assert_eq!(a.kind(), AlgoKind::ZFilter);
        assert_eq!(a.class_name(), "ZFilter");
    }

    #[test]
    fn summary() {
        assert_eq!(ZFilter::new(&|_| None).settings_summary(), "Type=Bessel-4, Exp-factor=2.0, Minimum move = 0.100\n");
        assert_eq!(with(1.3, 0.25).settings_summary(), "Type=Butterworth-4, Exp-factor=1.3, Minimum move = 0.250\n");
        assert_eq!(with(7.7, 0.1).settings_summary(), "Type=Bessel-4, Exp-factor=7.7, Minimum move = 0.100\n");
    }
}
