//! Lowpass: the median of the last offsets plus a weighted trend. Each frame
//! the offset joins a 10-entry history (zero-filled at reset); the move is
//!
//! ```text
//! r = median(history + input) + slope_weight · slope(history)
//! r = input when |r| > |input|
//! r = 0 when |input| < min_move
//! ```
//!
//! where the slope is the least-squares fit of the offsets against the frame
//! index. PHD2 trims the window by hand so the median sees 11 entries (the new
//! one included) and the fit 10 — kept for parity with the original algorithm.
//!
//! Ported from PHD2 src/guide_algorithm_lowpass.cpp — BSD-3-Clause, see THIRD_PARTY.md

use super::{AlgoCtx, AlgoKind, GuideAlgorithm, ParamLookup};
use crate::engine::log::cfmt::fmt_f;
use crate::engine::stats::WindowedAxisStats;

const DEFAULT_MIN_MOVE: f64 = 0.2;
const DEFAULT_SLOPE_WEIGHT: f64 = 5.0;
const HISTORY_SIZE: usize = 10;

/// `GuideAlgorithmLowpass`.
#[derive(Debug, Clone)]
pub struct Lowpass {
    slope_weight: f64,
    min_move: f64,
    /// Manual window: PHD2 adds, takes the median, then drops the oldest.
    axis_stats: WindowedAxisStats,
    /// The frame index the next offset is filed under.
    time_base: i32,
}

impl Lowpass {
    /// Read `minMove` and `SlopeWeight` (defaults 0.2, 5) through `params`,
    /// each validated by its setter.
    pub fn new(params: ParamLookup) -> Self {
        let mut a = Lowpass {
            slope_weight: DEFAULT_SLOPE_WEIGHT,
            min_move: DEFAULT_MIN_MOVE,
            axis_stats: WindowedAxisStats::new(0),
            time_base: 0,
        };
        a.set_min_move(params("minMove").unwrap_or(DEFAULT_MIN_MOVE));
        a.set_slope_weight(params("SlopeWeight").unwrap_or(DEFAULT_SLOPE_WEIGHT));
        a.reset();
        a
    }

    /// Weight of the trend term, ≥ 0.
    pub fn slope_weight(&self) -> f64 {
        self.slope_weight
    }

    /// `SetSlopeWeight`: a negative weight is an error and the default (5)
    /// is taken. `true` = error.
    pub fn set_slope_weight(&mut self, slope_weight: f64) -> bool {
        let err = slope_weight < 0.0;
        self.slope_weight = if err { DEFAULT_SLOPE_WEIGHT } else { slope_weight };
        err
    }
}

impl GuideAlgorithm for Lowpass {
    fn kind(&self) -> AlgoKind {
        AlgoKind::Lowpass
    }

    /// Ten zero offsets at frame indexes 0..9, so the first real one is
    /// filed at 10.
    fn reset(&mut self) {
        self.axis_stats.clear_all();
        self.time_base = 0;
        while self.axis_stats.count() < HISTORY_SIZE {
            self.axis_stats.add_guide_info(self.time_base as f64, 0.0, 0.0);
            self.time_base += 1;
        }
    }

    fn result(&mut self, input: f64, _ctx: &AlgoCtx) -> f64 {
        self.axis_stats.add_guide_info(self.time_base as f64, input, 0.0);
        self.time_base += 1;
        // Parity: the median is taken over 11 entries (the new one
        // included), before the oldest is dropped; the fit over the 10 left.
        let median = self.axis_stats.median();
        self.axis_stats.remove_oldest_entry();
        let slope = self.axis_stats.linear_fit().slope;
        let mut r = median + self.slope_weight * slope;

        if r.abs() > input.abs() {
            r = input;
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
        &["minMove", "slopeWeight"]
    }

    fn get_param(&self, name: &str) -> Option<f64> {
        match name {
            "minMove" => Some(self.min_move),
            "slopeWeight" => Some(self.slope_weight),
            _ => None,
        }
    }

    fn set_param(&mut self, name: &str, val: f64) -> bool {
        let err = match name {
            "minMove" => self.set_min_move(val),
            "slopeWeight" => self.set_slope_weight(val),
            _ => true,
        };
        !err
    }

    /// The profile key is `SlopeWeight` (capital S), the parameter name
    /// `slopeWeight`.
    fn params(&self) -> Vec<(&'static str, f64)> {
        vec![("minMove", self.min_move), ("SlopeWeight", self.slope_weight)]
    }

    fn settings_summary(&self) -> String {
        format!(
            "Slope weight = {}, Minimum move = {}\n",
            fmt_f(self.slope_weight, 3),
            fmt_f(self.min_move, 3)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Lowpass {
        Lowpass::new(&|_| None)
    }

    #[test]
    fn ramp_is_damped() {
        // Inputs 0.5, 1.0, …, 3.0 after the zero fill (slope weight 5,
        // min-move 0.2): r = median of 11 + 5 · OLS slope of the 10 kept.
        //   0.5: median 0,   slope 0.5 · (10 − 5.5) / 82.5 = 0.0272727 → 0.136364
        //   1.0: median 0,   slope 0.0757576 → 0.378788
        //   1.5: median 0,   slope 0.139394  → 0.696970
        //   2.0: median 0,   slope 0.212121  → 1.060606
        //   2.5: median 0,   slope 0.287879  → 1.439394
        //   3.0: median 0.5, slope 0.360606  → 2.303030
        let mut a = defaults();
        let ctx = AlgoCtx::default();
        let want = [
            0.13636363636363635,
            0.3787878787878788,
            0.696969696969697,
            1.0606060606060606,
            1.4393939393939394,
            2.3030303030303028,
        ];
        for (k, w) in want.iter().enumerate() {
            let x = 0.5 * (k + 1) as f64;
            let r = a.result(x, &ctx);
            assert!((r - w).abs() < 1e-9, "step {k}: {r} vs {w}");
        }
    }

    #[test]
    fn result_never_exceeds_the_input() {
        // After −1 … −6 the history is [0, 0, 0, 0, −1, …, −6]. −0.3 joins:
        // median of 11 = −1, slope −0.483030, so −1 + 5 · −0.483030 =
        // −3.415152, larger than the input in magnitude: the input is
        // returned as the move.
        let mut a = defaults();
        let ctx = AlgoCtx::default();
        let want = [
            -0.2727272727272727,
            -0.7575757575757576,
            -1.393939393939394,
            -2.121212121212121,
            -2.878787878787879,
            -4.6060606060606055,
        ];
        for (k, w) in want.iter().enumerate() {
            let r = a.result(-1.0 - k as f64, &ctx);
            assert!((r - w).abs() < 1e-9, "step {k}: {r} vs {w}");
        }
        assert_eq!(a.result(-0.3, &ctx), -0.3);
    }

    #[test]
    fn below_min_move_is_zero() {
        let mut a = defaults();
        let ctx = AlgoCtx::default();
        for _ in 0..12 {
            a.result(1.0, &ctx);
        }
        assert_eq!(a.result(0.1, &ctx), 0.0);
        assert_eq!(a.result(-0.19, &ctx), 0.0);
    }

    #[test]
    fn reset_refills_ten_zeros() {
        let mut a = defaults();
        let ctx = AlgoCtx::default();
        a.result(1.0, &ctx);
        a.result(2.0, &ctx);
        a.reset();
        assert_eq!(a.axis_stats.count(), 10);
        assert_eq!(a.time_base, 10);
        assert!((a.result(0.5, &ctx) - 0.13636363636363635).abs() < 1e-9);
    }

    #[test]
    fn setters_validate_like_phd2() {
        let mut a = defaults();
        assert!(!a.set_param("slopeWeight", -1.0));
        assert_eq!(a.get_param("slopeWeight"), Some(5.0));
        assert!(a.set_param("slopeWeight", 0.0));
        assert!(!a.set_param("minMove", -0.5));
        assert_eq!(a.get_param("minMove"), Some(0.2));
        assert!(!a.set_param("SlopeWeight", 1.0));
        assert_eq!(a.params(), vec![("minMove", 0.2), ("SlopeWeight", 0.0)]);
        let b = Lowpass::new(&|k| (k == "SlopeWeight").then_some(2.5));
        assert_eq!(b.slope_weight(), 2.5);
    }

    #[test]
    fn summary() {
        assert_eq!(defaults().settings_summary(), "Slope weight = 5.000, Minimum move = 0.200\n");
    }
}
