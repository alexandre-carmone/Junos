//! Resist Switch: PHD2's default Dec algorithm. It guides in one direction
//! only and resists switching to the other: a reversal needs at least three
//! more large offsets one way than the other in the last 10 frames *and* the
//! newest three summing to more than the oldest three in magnitude (the
//! error is getting worse). A move against the current direction is vetoed — the mount must
//! have overshot. With fast switch on, one offset beyond 3 × min-move the
//! other way switches at once.
//!
//! Ported from PHD2 src/guide_algorithm_resistswitch.cpp — BSD-3-Clause, see THIRD_PARTY.md

use super::{AlgoCtx, AlgoKind, GuideAlgorithm, ParamLookup};
use crate::engine::log::cfmt::fmt_f;

const DEFAULT_MIN_MOVE: f64 = 0.2;
const DEFAULT_AGGRESSION: f64 = 1.0;
const HISTORY_SIZE: usize = 10;

/// −1, 0 or 1 (PHD2's local `sign`).
fn sign(x: f64) -> i32 {
    if x > 0.0 {
        1
    } else if x < 0.0 {
        -1
    } else {
        0
    }
}

/// `GuideAlgorithmResistSwitch`.
#[derive(Debug, Clone)]
pub struct ResistSwitch {
    /// The last 10 offsets, oldest first (vetoed ones included).
    history: [f64; HISTORY_SIZE],
    min_move: f64,
    aggression: f64,
    fast_switch_enabled: bool,
    /// The direction guided: −1, 1, or 0 when not decided yet.
    current_side: i32,
}

impl ResistSwitch {
    /// Read `minMove`, `aggression` and `fastSwitch` (defaults 0.2, 1, on)
    /// through `params`, each validated by its setter.
    pub fn new(params: ParamLookup) -> Self {
        let mut a = ResistSwitch {
            history: [0.0; HISTORY_SIZE],
            min_move: DEFAULT_MIN_MOVE,
            aggression: DEFAULT_AGGRESSION,
            fast_switch_enabled: true,
            current_side: 0,
        };
        a.set_min_move(params("minMove").unwrap_or(DEFAULT_MIN_MOVE));
        a.set_aggression(params("aggression").unwrap_or(DEFAULT_AGGRESSION));
        a.set_fast_switch_enabled(params("fastSwitch").map_or(true, |v| v != 0.0));
        a.reset();
        a
    }

    /// Share of the offset applied, 0 ..= 1.
    pub fn aggression(&self) -> f64 {
        self.aggression
    }

    /// `SetAggression`: outside 0 ..= 1 is an error and the default (1) is
    /// taken. `true` = error.
    pub fn set_aggression(&mut self, aggression: f64) -> bool {
        let err = aggression < 0.0 || aggression > 1.0;
        self.aggression = if err { DEFAULT_AGGRESSION } else { aggression };
        err
    }

    /// Whether one large offset the other way switches at once.
    pub fn fast_switch_enabled(&self) -> bool {
        self.fast_switch_enabled
    }

    pub fn set_fast_switch_enabled(&mut self, enable: bool) {
        self.fast_switch_enabled = enable;
    }

    /// The direction being guided: −1, 1, or 0 while undecided.
    pub fn current_side(&self) -> i32 {
        self.current_side
    }

    /// The decision for one offset (the history already holds it): `true`
    /// when the move goes out, `false` on a veto. PHD2 expresses each veto
    /// as a thrown string caught at the end of `result`.
    fn decide(&mut self, input: f64) -> bool {
        if input.abs() < self.min_move {
            return false;
        }

        if self.fast_switch_enabled {
            let thresh = 3.0 * self.min_move;
            if sign(input) != self.current_side && input.abs() > thresh {
                // Force the switch: the history becomes seven zeros and
                // three copies of this offset, which the test below accepts.
                self.current_side = 0;
                let (old, new) = self.history.split_at_mut(HISTORY_SIZE - 3);
                old.fill(0.0);
                new.fill(input);
            }
        }

        let dec_history: i32 = self.history.iter().filter(|h| h.abs() > self.min_move).map(|&h| sign(h)).sum();

        if self.current_side == 0 || sign(self.current_side as f64) == -sign(dec_history as f64) {
            if dec_history.abs() < 3 {
                // Not compelling enough.
                return false;
            }
            let mut oldest = 0.0;
            let mut newest = 0.0;
            for i in 0..3 {
                oldest += self.history[i];
                newest += self.history[HISTORY_SIZE - (i + 1)];
            }
            if newest.abs() <= oldest.abs() {
                // Not getting worse.
                return false;
            }
            self.current_side = sign(dec_history as f64);
        }

        // Must have overshot: veto a move against the guided direction.
        self.current_side == sign(input)
    }
}

impl GuideAlgorithm for ResistSwitch {
    fn kind(&self) -> AlgoKind {
        AlgoKind::ResistSwitch
    }

    fn reset(&mut self) {
        self.history = [0.0; HISTORY_SIZE];
        self.current_side = 0;
    }

    fn result(&mut self, input: f64, _ctx: &AlgoCtx) -> f64 {
        // Every offset enters the history, vetoed or not.
        self.history.rotate_left(1);
        self.history[HISTORY_SIZE - 1] = input;

        let r = if self.decide(input) { input } else { 0.0 };
        r * self.aggression
    }

    fn min_move(&self) -> f64 {
        self.min_move
    }

    /// The min-move must be > 0, else the default (0.2) is taken. A valid
    /// one also forgets the guided direction.
    fn set_min_move(&mut self, min_move: f64) -> bool {
        if min_move <= 0.0 {
            self.min_move = DEFAULT_MIN_MOVE;
            return true;
        }
        self.min_move = min_move;
        self.current_side = 0;
        false
    }

    fn param_names(&self) -> &'static [&'static str] {
        &["minMove", "fastSwitch", "aggression"]
    }

    fn get_param(&self, name: &str) -> Option<f64> {
        match name {
            "minMove" => Some(self.min_move),
            "fastSwitch" => Some(if self.fast_switch_enabled { 1.0 } else { 0.0 }),
            "aggression" => Some(self.aggression),
            _ => None,
        }
    }

    fn set_param(&mut self, name: &str, val: f64) -> bool {
        let err = match name {
            "minMove" => self.set_min_move(val),
            "fastSwitch" => {
                self.set_fast_switch_enabled(val != 0.0);
                false
            }
            "aggression" => self.set_aggression(val),
            _ => true,
        };
        !err
    }

    /// `fastSwitch` is a boolean in the profile: 0 / 1 here.
    fn params(&self) -> Vec<(&'static str, f64)> {
        vec![
            ("minMove", self.min_move),
            ("aggression", self.aggression),
            ("fastSwitch", if self.fast_switch_enabled { 1.0 } else { 0.0 }),
        ]
    }

    /// No comma before "Aggression", the aggression as a whole percentage:
    /// PHD2's spelling.
    fn settings_summary(&self) -> String {
        format!(
            "Minimum move = {} Aggression = {}% FastSwitch = {}\n",
            fmt_f(self.min_move, 3),
            fmt_f(self.aggression * 100.0, 0),
            if self.fast_switch_enabled { "enabled" } else { "disabled" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> ResistSwitch {
        ResistSwitch::new(&|_| None)
    }

    fn run(a: &mut ResistSwitch, inputs: &[f64]) -> Vec<f64> {
        let ctx = AlgoCtx::default();
        inputs.iter().map(|&x| a.result(x, &ctx)).collect()
    }

    #[test]
    fn steady_offset_needs_three_frames() {
        // Ten 0.3 offsets (min-move 0.2): one and two large offsets in the
        // history are not compelling (|dec| < 3); the third makes dec = 3
        // and newest 0.9 > oldest 0, so the side becomes +1 and the move
        // goes out; from then on the side holds.
        let mut a = defaults();
        let out = run(&mut a, &[0.3; 10]);
        assert_eq!(out, [0.0, 0.0, 0.3, 0.3, 0.3, 0.3, 0.3, 0.3, 0.3, 0.3]);
        assert_eq!(a.current_side(), 1);
    }

    #[test]
    fn sign_flip_is_resisted() {
        // After ten +0.3, a run of −0.3. k = number of −0.3 so far:
        //   k 1..5: dec = 10 − 2k ≥ 0, side +1 holds, move against it → veto
        //   k 6:    dec = −2, |dec| < 3 → veto
        //   k 7:    dec = −4, but oldest three 0.9 vs newest −0.9 → not worse → veto
        //   k 8:    oldest 0.3+0.3−0.3 = 0.3 < |newest| 0.9 → side −1, move out
        let mut a = defaults();
        run(&mut a, &[0.3; 10]);
        let out = run(&mut a, &[-0.3; 9]);
        assert_eq!(out, [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -0.3, -0.3]);
        assert_eq!(a.current_side(), -1);
    }

    #[test]
    fn small_offsets_never_move() {
        let mut a = defaults();
        let out = run(&mut a, &[0.3, 0.3, 0.3, 0.1, -0.15, 0.19]);
        assert_eq!(out, [0.0, 0.0, 0.3, 0.0, 0.0, 0.0]);
        // A small offset vetoes but leaves the side alone.
        assert_eq!(a.current_side(), 1);
    }

    #[test]
    fn fast_switch_seeds_the_history() {
        // −0.7 > 3 · 0.2 against side +1: the history becomes seven zeros
        // and three −0.7, dec = −3, newest −2.1 beats oldest 0 → side −1 and
        // the move goes out at once.
        let mut a = defaults();
        run(&mut a, &[0.3; 10]);
        assert_eq!(run(&mut a, &[-0.7]), [-0.7]);
        assert_eq!(a.current_side(), -1);
        assert_eq!(a.history, [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -0.7, -0.7, -0.7]);
        // 3 · min-move itself is not beyond the threshold: no fast switch.
        let mut a = defaults();
        run(&mut a, &[0.3; 10]);
        assert_eq!(run(&mut a, &[-0.6]), [0.0]);
        assert_eq!(a.current_side(), 1);
    }

    #[test]
    fn fast_switch_off_resists() {
        let mut a = ResistSwitch::new(&|k| (k == "fastSwitch").then_some(0.0));
        assert!(!a.fast_switch_enabled());
        run(&mut a, &[0.3; 10]);
        assert_eq!(run(&mut a, &[-0.7]), [0.0]);
        assert_eq!(a.current_side(), 1);
    }

    #[test]
    fn aggression_scales_the_move() {
        let mut a = ResistSwitch::new(&|k| (k == "aggression").then_some(0.5));
        assert_eq!(run(&mut a, &[0.4, 0.4, 0.4]), [0.0, 0.0, 0.2]);
    }

    #[test]
    fn setters_validate_like_phd2() {
        let mut a = defaults();
        // min-move must be strictly positive.
        assert!(!a.set_param("minMove", 0.0));
        assert_eq!(a.min_move(), 0.2);
        // A valid min-move forgets the side; a refused one does not.
        run(&mut a, &[0.3; 3]);
        assert_eq!(a.current_side(), 1);
        assert!(!a.set_param("minMove", -1.0));
        assert_eq!(a.current_side(), 1);
        assert!(a.set_param("minMove", 0.25));
        assert_eq!(a.current_side(), 0);
        assert!(!a.set_param("aggression", 1.5));
        assert_eq!(a.get_param("aggression"), Some(1.0));
        assert!(a.set_param("aggression", 0.0));
        assert!(a.set_param("fastSwitch", 0.0));
        assert_eq!(a.get_param("fastSwitch"), Some(0.0));
        assert!(a.set_param("fastSwitch", 2.0));
        assert_eq!(a.get_param("fastSwitch"), Some(1.0));
        assert!(!a.set_param("hysteresis", 0.1));
        assert_eq!(a.params(), vec![("minMove", 0.25), ("aggression", 0.0), ("fastSwitch", 1.0)]);
        let b = ResistSwitch::new(&|k| (k == "minMove").then_some(0.0));
        assert_eq!(b.min_move(), 0.2);
    }

    #[test]
    fn summary() {
        let mut a = defaults();
        assert_eq!(a.settings_summary(), "Minimum move = 0.200 Aggression = 100% FastSwitch = enabled\n");
        a.set_aggression(0.255);
        a.set_fast_switch_enabled(false);
        assert_eq!(a.settings_summary(), "Minimum move = 0.200 Aggression = 26% FastSwitch = disabled\n");
    }
}
