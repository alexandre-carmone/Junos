//! Lowpass2: a trend follower. The last 10 offsets are fitted against the
//! frame index and the move is the fitted drift over the window, scaled by
//! the aggressiveness (a percentage):
//!
//! ```text
//! fewer than 4 samples:     r = input · aggr
//! |input| > 4 · min_move:   r = input · aggr, history dropped (outlier)
//! otherwise:                r = slope · n · aggr, 0 if it points against the input
//! |r| > |input|:            r = input · aggr; a 4th such frame in a row drops the history
//! |input| < min_move:       r = 0
//! ```
//!
//! Ported from PHD2 src/guide_algorithm_lowpass2.cpp — BSD-3-Clause, see THIRD_PARTY.md

use super::{AlgoCtx, AlgoKind, GuideAlgorithm, ParamLookup};
use crate::engine::log::cfmt::fmt_f;
use crate::engine::stats::WindowedAxisStats;

const DEFAULT_MIN_MOVE: f64 = 0.2;
const DEFAULT_AGGRESSIVENESS: f64 = 80.0;
const HISTORY_SIZE: usize = 10;

/// `GuideAlgorithmLowpass2`.
#[derive(Debug, Clone)]
pub struct Lowpass2 {
    /// Percent of the fitted drift applied.
    aggressiveness: f64,
    min_move: f64,
    /// Corrections in a row that came out larger than the offset.
    rejects: u32,
    /// Auto-windowed to the last 10 offsets.
    axis_stats: WindowedAxisStats,
    /// The frame index the next offset is filed under.
    time_base: i32,
}

impl Lowpass2 {
    /// Read `minMove` and `Aggressiveness` (defaults 0.2, 80 %) through
    /// `params`, each validated by its setter.
    pub fn new(params: ParamLookup) -> Self {
        let mut a = Lowpass2 {
            aggressiveness: DEFAULT_AGGRESSIVENESS,
            min_move: DEFAULT_MIN_MOVE,
            rejects: 0,
            axis_stats: WindowedAxisStats::new(HISTORY_SIZE),
            time_base: 0,
        };
        a.set_min_move(params("minMove").unwrap_or(DEFAULT_MIN_MOVE));
        a.set_aggressiveness(params("Aggressiveness").unwrap_or(DEFAULT_AGGRESSIVENESS));
        a.reset();
        a
    }

    /// Percent of the fitted drift applied, ≥ 0 (the UI offers 0 ..= 100).
    pub fn aggressiveness(&self) -> f64 {
        self.aggressiveness
    }

    /// `SetAggressiveness`: a negative value is an error and the default
    /// (80) is taken. `true` = error.
    ///
    /// Quirk-ledger fix: PHD2 stores the default in the profile but leaves
    /// the member at its previous value (uninitialised when it happens in the
    /// constructor). Here the member takes the default too, so the value in
    /// use is the one persisted.
    pub fn set_aggressiveness(&mut self, aggressiveness: f64) -> bool {
        let err = aggressiveness < 0.0;
        self.aggressiveness = if err { DEFAULT_AGGRESSIVENESS } else { aggressiveness };
        err
    }
}

impl GuideAlgorithm for Lowpass2 {
    fn kind(&self) -> AlgoKind {
        AlgoKind::Lowpass2
    }

    fn reset(&mut self) {
        self.axis_stats.clear_all();
        self.time_base = 0;
        self.rejects = 0;
    }

    fn result(&mut self, input: f64, _ctx: &AlgoCtx) -> f64 {
        self.axis_stats.add_guide_info(self.time_base as f64, input, 0.0);
        self.time_base += 1;
        let numpts = self.axis_stats.count();
        let attenuation = self.aggressiveness / 100.0;
        let mut r;

        if numpts < 4 {
            // Don't fall behind while the trend is being learnt.
            r = input * attenuation;
        } else if input.abs() > 4.0 * self.min_move {
            // Outlier deflection: dump the history (this offset included).
            r = input * attenuation;
            self.reset();
        } else {
            let slope = self.axis_stats.linear_fit().slope;
            r = slope * numpts as f64 * attenuation;
            // Never push the star further the wrong way.
            if input * r < 0.0 {
                r = 0.0;
            }
        }

        // Keep the pulse below the last deflection; three rejected slopes
        // in a row and a fourth means the slope is not useful.
        if r.abs() > input.abs() {
            r = input * attenuation;
            self.rejects += 1;
            if self.rejects > 3 {
                self.reset();
            }
        } else {
            self.rejects = 0;
        }

        if input.abs() < self.min_move {
            r = 0.0;
        }
        r
    }

    fn min_move(&self) -> f64 {
        self.min_move
    }

    /// A negative min-move is an error and the default (0.2) is taken.
    fn set_min_move(&mut self, min_move: f64) -> bool {
        let err = min_move < 0.0;
        self.min_move = if err { DEFAULT_MIN_MOVE } else { min_move };
        err
    }

    fn param_names(&self) -> &'static [&'static str] {
        &["minMove", "aggressiveness"]
    }

    fn get_param(&self, name: &str) -> Option<f64> {
        match name {
            "minMove" => Some(self.min_move),
            "aggressiveness" => Some(self.aggressiveness),
            _ => None,
        }
    }

    fn set_param(&mut self, name: &str, val: f64) -> bool {
        let err = match name {
            "minMove" => self.set_min_move(val),
            "aggressiveness" => self.set_aggressiveness(val),
            _ => true,
        };
        !err
    }

    /// The profile key is `Aggressiveness` (capital A), the parameter name
    /// `aggressiveness`.
    fn params(&self) -> Vec<(&'static str, f64)> {
        vec![("minMove", self.min_move), ("Aggressiveness", self.aggressiveness)]
    }

    fn settings_summary(&self) -> String {
        format!(
            "Aggressiveness = {}, Minimum move = {}\n",
            fmt_f(self.aggressiveness, 3),
            fmt_f(self.min_move, 3)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Lowpass2 {
        Lowpass2::new(&|_| None)
    }

    fn run(a: &mut Lowpass2, inputs: &[f64], want: &[f64]) {
        let ctx = AlgoCtx::default();
        for (k, (&x, &w)) in inputs.iter().zip(want).enumerate() {
            let r = a.result(x, &ctx);
            assert!((r - w).abs() < 1e-9, "step {k}: {r} vs {w}");
        }
    }

    #[test]
    fn first_three_samples_are_attenuated_input() {
        // 0.8 · input for n < 4; then the fitted drift 0.02 / frame · n · 0.8.
        let mut a = defaults();
        run(
            &mut a,
            &[0.3, 0.32, 0.34, 0.36, 0.38, 0.40],
            &[0.24, 0.256, 0.272, 0.064, 0.08, 0.096],
        );
    }

    #[test]
    fn window_stays_at_ten() {
        let mut a = defaults();
        let inputs: Vec<f64> = (0..12).map(|k| 0.3 + 0.02 * k as f64).collect();
        // n = 10 from the 10th frame on: 0.02 · 10 · 0.8 = 0.16.
        run(&mut a, &inputs, &[0.24, 0.256, 0.272, 0.064, 0.08, 0.096, 0.112, 0.128, 0.144, 0.16, 0.16, 0.16]);
        assert_eq!(a.axis_stats.count(), 10);
    }

    #[test]
    fn outlier_resets() {
        // 0.81 > 4 · 0.2 on the 4th frame: attenuated input, history gone
        // (the outlier with it), so the next three are the n < 4 case again.
        let mut a = defaults();
        run(&mut a, &[0.3, 0.3, 0.3, 0.81], &[0.24, 0.24, 0.24, 0.648]);
        assert_eq!(a.axis_stats.count(), 0);
        run(&mut a, &[0.3, 0.3, 0.3, 0.3], &[0.24, 0.24, 0.24, 0.0]);
        assert_eq!(a.axis_stats.count(), 4);
    }

    #[test]
    fn four_rejects_in_a_row_reset() {
        // A steep ramp: slope 0.2 · n · 0.8 is larger than the input from
        // the 4th frame on, so each frame falls back to 0.8 · input and
        // counts a reject; the 4th reject drops the history.
        let mut a = defaults();
        run(
            &mut a,
            &[-0.4, -0.2, 0.0, 0.2, 0.4, 0.6],
            &[-0.32, -0.16, 0.0, 0.16, 0.32, 0.48],
        );
        assert_eq!((a.rejects, a.axis_stats.count()), (3, 6));
        run(&mut a, &[0.8], &[0.64]);
        assert_eq!((a.rejects, a.axis_stats.count(), a.time_base), (0, 0, 0));
        run(&mut a, &[0.5], &[0.4]);
        assert_eq!(a.axis_stats.count(), 1);
    }

    #[test]
    fn no_move_against_the_input() {
        // Ramp 0 … 1.6 (min-move 0.5, so no outlier up to 2.0), then −0.5:
        // the fitted slope is still +0.0745 / frame, a move the wrong way,
        // so nothing is sent even though |−0.5| ≥ min-move.
        let mut a = Lowpass2::new(&|k| (k == "minMove").then_some(0.5));
        let ctx = AlgoCtx::default();
        for k in 0..9 {
            a.result(0.2 * k as f64, &ctx);
        }
        assert!((a.axis_stats.linear_fit().slope - 0.2).abs() < 1e-12);
        assert_eq!(a.result(-0.5, &ctx), 0.0);
        assert_eq!(a.rejects, 0);
    }

    #[test]
    fn below_min_move_is_zero() {
        let mut a = defaults();
        run(&mut a, &[0.19, -0.1], &[0.0, 0.0]);
    }

    #[test]
    fn setters_validate_like_phd2() {
        let mut a = defaults();
        // The fixed quirk: the default lands in the member too.
        assert!(a.set_param("aggressiveness", 50.0));
        assert!(!a.set_param("aggressiveness", -5.0));
        assert_eq!(a.get_param("aggressiveness"), Some(80.0));
        // Above 100 % is accepted (only negatives are refused).
        assert!(a.set_param("aggressiveness", 150.0));
        assert!(!a.set_param("minMove", -0.1));
        assert_eq!(a.min_move(), 0.2);
        assert!(!a.set_param("Aggressiveness", 1.0));
        assert_eq!(a.params(), vec![("minMove", 0.2), ("Aggressiveness", 150.0)]);
        let b = Lowpass2::new(&|k| (k == "Aggressiveness").then_some(-1.0));
        assert_eq!(b.aggressiveness(), 80.0);
    }

    #[test]
    fn summary() {
        assert_eq!(defaults().settings_summary(), "Aggressiveness = 80.000, Minimum move = 0.200\n");
    }
}
