//! Hysteresis: PHD2's default RA algorithm. The move blends this frame's
//! offset with the previous move, then scales it by the aggression:
//!
//! ```text
//! r = ((1 − hysteresis) · input + hysteresis · last_move) · aggression
//! r = 0 when |input| < min_move
//! last_move = r
//! ```
//!
//! Ported from PHD2 src/guide_algorithm_hysteresis.cpp — BSD-3-Clause, see THIRD_PARTY.md

use super::{AlgoCtx, AlgoKind, GuideAlgorithm, ParamLookup};
use crate::engine::log::cfmt::fmt_f;

const DEFAULT_MIN_MOVE: f64 = 0.2;
const DEFAULT_HYSTERESIS: f64 = 0.1;
const DEFAULT_AGGRESSION: f64 = 0.7;
const MAX_AGGRESSION: f64 = 2.0;
const MAX_HYSTERESIS: f64 = 0.99;

/// `GuideAlgorithmHysteresis`.
#[derive(Debug, Clone)]
pub struct Hysteresis {
    min_move: f64,
    hysteresis: f64,
    aggression: f64,
    last_move: f64,
}

impl Hysteresis {
    /// Read `minMove`, `hysteresis` and `aggression` through `params`
    /// (defaults 0.2, 0.1, 0.7), each validated by its setter.
    pub fn new(params: ParamLookup) -> Self {
        let mut a = Hysteresis {
            min_move: DEFAULT_MIN_MOVE,
            hysteresis: DEFAULT_HYSTERESIS,
            aggression: DEFAULT_AGGRESSION,
            last_move: 0.0,
        };
        a.set_min_move(params("minMove").unwrap_or(DEFAULT_MIN_MOVE));
        a.set_hysteresis(params("hysteresis").unwrap_or(DEFAULT_HYSTERESIS));
        a.set_aggression(params("aggression").unwrap_or(DEFAULT_AGGRESSION));
        a.reset();
        a
    }

    /// Share of the previous move in this one, 0 ..= 0.99.
    pub fn hysteresis(&self) -> f64 {
        self.hysteresis
    }

    /// `SetHysteresis`: outside 0 ..= 0.99 is an error and the value is
    /// clamped into that range (not reset to the default). `true` = error.
    pub fn set_hysteresis(&mut self, hysteresis: f64) -> bool {
        if hysteresis < 0.0 || hysteresis > MAX_HYSTERESIS {
            self.hysteresis = hysteresis.max(0.0).min(MAX_HYSTERESIS);
            return true;
        }
        self.hysteresis = hysteresis;
        false
    }

    /// Share of the computed move applied, 0 ..= 2.
    pub fn aggression(&self) -> f64 {
        self.aggression
    }

    /// `SetAggression`: outside 0 ..= 2 is an error and the default (0.7)
    /// is taken. Either way the last move is forgotten. `true` = error.
    pub fn set_aggression(&mut self, aggression: f64) -> bool {
        let err = aggression < 0.0 || aggression > MAX_AGGRESSION;
        self.aggression = if err { DEFAULT_AGGRESSION } else { aggression };
        self.last_move = 0.0;
        err
    }
}

impl GuideAlgorithm for Hysteresis {
    fn kind(&self) -> AlgoKind {
        AlgoKind::Hysteresis
    }

    fn reset(&mut self) {
        self.last_move = 0.0;
    }

    fn result(&mut self, input: f64, _ctx: &AlgoCtx) -> f64 {
        let mut r = (1.0 - self.hysteresis) * input + self.hysteresis * self.last_move;
        r *= self.aggression;
        if input.abs() < self.min_move {
            r = 0.0;
        }
        self.last_move = r;
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
        &["minMove", "hysteresis", "aggression"]
    }

    fn get_param(&self, name: &str) -> Option<f64> {
        match name {
            "minMove" => Some(self.min_move),
            "hysteresis" => Some(self.hysteresis),
            "aggression" => Some(self.aggression),
            _ => None,
        }
    }

    fn set_param(&mut self, name: &str, val: f64) -> bool {
        let err = match name {
            "minMove" => self.set_min_move(val),
            "hysteresis" => self.set_hysteresis(val),
            "aggression" => self.set_aggression(val),
            _ => true,
        };
        !err
    }

    fn params(&self) -> Vec<(&'static str, f64)> {
        vec![("minMove", self.min_move), ("hysteresis", self.hysteresis), ("aggression", self.aggression)]
    }

    fn settings_summary(&self) -> String {
        format!(
            "Hysteresis = {}, Aggression = {}, Minimum move = {}\n",
            fmt_f(self.hysteresis, 3),
            fmt_f(self.aggression, 3),
            fmt_f(self.min_move, 3)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Hysteresis {
        Hysteresis::new(&|_| None)
    }

    #[test]
    fn vector_with_defaults() {
        // h = 0.1, aggression 0.7, min-move 0.2:
        // 0.9·1·0.7 = 0.63; (0.9 + 0.063)·0.7 = 0.6741;
        // (0.9 + 0.06741)·0.7 = 0.677187; 0.1 < min-move → 0; then the last
        // move is 0, so −0.9·0.7 = −0.63.
        let mut a = defaults();
        let ctx = AlgoCtx::default();
        let out: Vec<f64> = [1.0, 1.0, 1.0, 0.1, -1.0].iter().map(|&x| a.result(x, &ctx)).collect();
        let want = [0.63, 0.6741, 0.677187, 0.0, -0.63];
        for (o, w) in out.iter().zip(want) {
            assert!((o - w).abs() < 1e-9, "{out:?}");
        }
    }

    #[test]
    fn min_move_boundary_is_strict() {
        // |input| == min-move moves; only strictly below is dropped.
        let mut a = defaults();
        let ctx = AlgoCtx::default();
        assert!((a.result(0.2, &ctx) - 0.9 * 0.2 * 0.7).abs() < 1e-15);
        a.reset();
        assert_eq!(a.result(0.19999, &ctx), 0.0);
    }

    #[test]
    fn setters_validate_like_phd2() {
        let mut a = defaults();
        // Hysteresis out of range: clamped, not defaulted.
        assert!(!a.set_param("hysteresis", 1.5));
        assert_eq!(a.hysteresis(), 0.99);
        assert!(!a.set_param("hysteresis", -0.1));
        assert_eq!(a.hysteresis(), 0.0);
        assert!(a.set_param("hysteresis", 0.25));
        assert_eq!(a.get_param("hysteresis"), Some(0.25));
        // Aggression out of range: the default.
        assert!(!a.set_param("aggression", 2.5));
        assert_eq!(a.aggression(), 0.7);
        assert!(a.set_param("aggression", 2.0));
        assert_eq!(a.aggression(), 2.0);
        // Min-move: negative → default; 0 is fine.
        assert!(!a.set_param("minMove", -1.0));
        assert_eq!(a.min_move(), 0.2);
        assert!(a.set_param("minMove", 0.0));
        assert_eq!(a.min_move(), 0.0);
        assert!(!a.set_param("slopeWeight", 1.0));
        assert_eq!(a.get_param("slopeWeight"), None);
        assert_eq!(a.params(), vec![("minMove", 0.0), ("hysteresis", 0.25), ("aggression", 2.0)]);
    }

    #[test]
    fn set_aggression_forgets_the_last_move() {
        let mut a = defaults();
        let ctx = AlgoCtx::default();
        a.result(1.0, &ctx);
        assert!(!a.set_aggression(0.7));
        assert!((a.result(1.0, &ctx) - 0.63).abs() < 1e-15);
    }

    #[test]
    fn construction_reads_and_validates_the_profile() {
        let a = Hysteresis::new(&|k| match k {
            "minMove" => Some(0.35),
            "hysteresis" => Some(5.0),
            "aggression" => Some(-1.0),
            _ => None,
        });
        assert_eq!((a.min_move(), a.hysteresis(), a.aggression()), (0.35, 0.99, 0.7));
    }

    #[test]
    fn summary() {
        assert_eq!(
            defaults().settings_summary(),
            "Hysteresis = 0.100, Aggression = 0.700, Minimum move = 0.200\n"
        );
    }
}
