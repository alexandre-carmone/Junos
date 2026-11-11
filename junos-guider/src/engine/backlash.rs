//! Dec backlash: the runtime compensation ([`BacklashComp`], an extra pulse
//! added to a Dec guide move when its direction reverses, with the adaptive
//! history that tunes that pulse from the follow-on misses) and the Guiding
//! Assistant's measurement tool ([`BacklashTool`], which clears the backlash
//! north, steps north, steps back south and reads the backlash off the south
//! moves that fell short).
//!
//! Everything here is pure. PHD2 reaches into the scope, the guider, the frame
//! and the profile from these classes; the port takes those values as
//! arguments and hands the side effects back: the pulse to issue, the profile
//! keys to write, the `GuideParamChange` notifications, the raised Dec
//! max-duration and the debug-log lines.
//!
//! Ported from PHD2 `src/backlash_comp.h`, `src/backlash_comp.cpp`, `src/guiding_assistant.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use super::calibration::{Calibration, camera_to_mount};
use super::log::cfmt::fmt_f;
use super::stats::AxisStats;
use super::traits::{Dir, Point};

/// `MOVEOPT_ALGO_RESULT` (`mount.h:127`): the move is a guide algorithm's
/// result. `mount.rs` re-exports the full `MoveOpts` set; these two are the
/// bits the compensation reads.
pub const MOVEOPT_ALGO_RESULT: u32 = 1 << 0;
/// `MOVEOPT_USE_BLC` (`mount.h:129`): apply backlash compensation to the move.
pub const MOVEOPT_USE_BLC: u32 = 1 << 2;

/// Smallest compensation pulse, ms (`backlash_comp.cpp:41`): small enough to
/// disable the compensation in practice.
pub const MIN_COMP_AMOUNT: i32 = 20;
/// Largest compensation pulse, ms (`backlash_comp.cpp:42`).
pub const MAX_COMP_AMOUNT: i32 = 8000;

/// PHD2's `ROUND` (`phd.h:122`): `(int) floor(x + 0.5)`.
fn round(x: f64) -> i32 {
    (x + 0.5).floor() as i32
}

/// A C `(int)` cast of a double on x86-64: NaN and out-of-range values become
/// `INT_MIN` (`cvttsd2si`), where Rust's `as` would give 0 or saturate.
fn c_int(x: f64) -> i32 {
    if x.is_nan() || x >= 2147483648.0 || x < -2147483648.0 { i32::MIN } else { x as i32 }
}

// ---------------------------------------------------------------- BLC history

/// One follow-on deflection of a compensation event (`CorrectionTuple`). PHD2
/// also stores the time of each; nothing reads it, so it is not kept.
#[derive(Clone, Debug, PartialEq)]
struct BlcEvent {
    /// `[0]` is the deflection that triggered the pulse, `[1]` the first miss
    /// after it, `[2]` the optional second one (needed to see stiction).
    corrections: Vec<f64>,
    initial_overshoot: bool,
    initial_undershoot: bool,
    stiction_seen: bool,
}

impl BlcEvent {
    fn new(amount: f64) -> Self {
        Self { corrections: vec![amount], initial_overshoot: false, initial_undershoot: false, stiction_seen: false }
    }

    fn info_count(&self) -> usize {
        self.corrections.len()
    }

    /// `BLCEvent::AddEventInfo` (`backlash_comp.cpp:77`): a follow-on miss is
    /// recorded whatever its size, but only one above `min_move` classifies
    /// the event.
    fn add_event_info(&mut self, amount: f64, min_move: f64) {
        if self.info_count() < 3 {
            self.corrections.push(amount);
            if amount.abs() > min_move {
                if self.info_count() == 2 {
                    if amount > 0.0 {
                        self.initial_undershoot = true;
                    } else {
                        self.initial_overshoot = true;
                    }
                } else if self.info_count() == 3 {
                    // The second follow-on miss was an over-shoot.
                    self.stiction_seen = amount < 0.0 && self.corrections[1] > 0.0;
                }
            }
        }
    }
}

/// Stats over the recent events (`BLCHistory::RecentStats`).
#[derive(Clone, Debug, Default, PartialEq)]
struct RecentStats {
    short_count: i32,
    long_count: i32,
    stiction_count: i32,
    avg_initial_miss: f64,
    avg_stiction_amount: f64,
}

/// `BLCHistory` (`backlash_comp.cpp:114`): the last `HISTORY_DEPTH`
/// compensation events, each with its trigger and up to two follow-on
/// deflections. The window is open from the pulse until an adjustment is
/// made from the first follow-on or two follow-ons were recorded.
#[derive(Clone, Debug)]
struct BlcHistory {
    events: Vec<BlcEvent>,
    blc_index: usize,
    window_open: bool,
    last_increase: i32,
}

const ENTRY_CAPACITY: usize = 3;
const HISTORY_DEPTH: usize = 10;

impl BlcHistory {
    fn new() -> Self {
        Self { events: Vec::new(), blc_index: 0, window_open: false, last_increase: 0 }
    }

    fn close_window(&mut self, log: &mut Vec<String>) {
        self.window_open = false;
        log.push("BLC: window closed".into());
    }

    fn record_new_blc_pulse(&mut self, trigger_deflection: f64, log: &mut Vec<String>) {
        if self.events.len() >= HISTORY_DEPTH {
            self.events.remove(0);
            log.push("BLC: Oldest BLC event removed".into());
        }
        self.events.push(BlcEvent::new(trigger_deflection));
        self.blc_index = self.events.len() - 1;
        self.window_open = true;
    }

    fn add_deflection(&mut self, amt: f64, min_move: f64, log: &mut Vec<String>) -> bool {
        match self.events.get_mut(self.blc_index) {
            Some(evt) if evt.info_count() < ENTRY_CAPACITY => {
                evt.add_event_info(amt, min_move);
                true
            }
            _ => {
                self.close_window(log);
                false
            }
        }
    }

    /// Drop the oldest `how_many` over-shoot events, never the current one.
    fn remove_oldest_overshoots(&mut self, how_many: usize) {
        for _ in 0..how_many {
            let last = self.events.len().saturating_sub(1);
            if let Some(inx) = self.events[..last].iter().position(|e| e.initial_overshoot) {
                self.events.remove(inx);
                self.blc_index = self.events.len() - 1;
            }
        }
    }

    fn remove_oldest_stictions(&mut self, how_many: usize) {
        for _ in 0..how_many {
            let last = self.events.len().saturating_sub(1);
            if let Some(inx) = self.events[..last].iter().position(|e| e.stiction_seen) {
                self.events.remove(inx);
                self.blc_index = self.events.len() - 1;
            }
        }
    }

    fn clear_history(&mut self, log: &mut Vec<String>) {
        self.events.clear();
        self.close_window(log);
        log.push("BLC: History cleared".into());
    }

    /// `BLCHistory::GetStats` (`backlash_comp.cpp:221`): counts and averages
    /// over the `num_events` most recent events; the average of the first
    /// follow-on misses is the return value.
    fn stats(&self, num_events: usize) -> RecentStats {
        let mut r = RecentStats::default();
        let bottom = self.blc_index.saturating_sub(num_events - 1);
        let mut sum = 0.0;
        let mut stiction_sum = 0.0;
        let mut ct = 0;
        for inx in (bottom..=self.blc_index).rev() {
            let evt = &self.events[inx];
            if evt.initial_overshoot {
                r.long_count += 1;
            } else {
                r.short_count += 1;
            }
            if evt.stiction_seen {
                r.stiction_count += 1;
                stiction_sum += evt.corrections[2];
            }
            // Only the initial misses right after the pulses are averaged.
            if evt.info_count() > 1 {
                sum += evt.corrections[1];
                ct += 1;
            }
        }
        r.avg_initial_miss = if ct > 0 { sum / ct as f64 } else { 0.0 };
        r.avg_stiction_amount =
            if r.stiction_count > 0 { stiction_sum / r.stiction_count as f64 } else { 0.0 };
        r
    }

    /// `BLCHistory::AdjustmentNeeded` (`backlash_comp.cpp:257`): the adaptive
    /// rule. Returns the signed correction, ms, when the pulse should change.
    /// The dead `correction = 0` pointer assignment of PHD2's last else branch
    /// (`:353`) is not reproduced: the out value is already 0 there.
    fn adjustment_needed(&mut self, miss: f64, min_move: f64, y_rate: f64, log: &mut Vec<String>) -> Option<f64> {
        if self.events.get(self.blc_index).is_none() {
            return None;
        }
        let stats = self.stats(HISTORY_DEPTH);
        let avg_init_miss = stats.avg_initial_miss;
        let curr = &self.events[self.blc_index];
        // PHD2 reads corrections[1] unconditionally here (std::to_string of
        // a vector element past the end); it is always present since the
        // deflection was just added.
        let mut deflections =
            format!(" Deflections: 0={}, 1:{}", fmt_f(curr.corrections[0], 6), fmt_f(curr.corrections[1], 6));
        if curr.info_count() > 2 {
            deflections += &format!(", 2:{}", fmt_f(curr.corrections[2], 6));
        }
        log.push(format!(
            "BLC: History state: CurrMiss={}, AvgInitMiss={}, ShCount={}, LgCount={}, SticCount={}, {}",
            fmt_f(miss, 2),
            fmt_f(stats.avg_initial_miss, 2),
            stats.short_count,
            stats.long_count,
            stats.stiction_count,
            deflections
        ));

        let mut correction = 0.0;
        let mut adjust = false;
        if miss.abs() >= min_move {
            // The unsigned correction value.
            let corr = ((avg_init_miss.abs() / y_rate).floor() + 0.5) as i32;
            if miss > 0.0 {
                // Under-shoot.
                if avg_init_miss > 0.0 {
                    // Don't change anything before two follow-on displacements.
                    if curr.info_count() == ENTRY_CAPACITY {
                        if stats.stiction_count > 2 {
                            log.push("BLC: Under-shoot, no adjustment because of stiction history".into());
                        } else if stats.long_count >= 2 {
                            log.push("BLC: Under-shoot; no adjustment because of over-shoot history".into());
                        } else {
                            adjust = true;
                            correction = corr as f64;
                            self.last_increase = corr;
                            log.push(format!("BLC: Under-shoot: nominal increase by {corr}"));
                        }
                    } else {
                        log.push("BLC: Under-shoot, no adjustment, waiting for more data".into());
                    }
                } else {
                    log.push("BLC: Under-shoot, no adjustment, avgInitialMiss <= 0".into());
                    self.close_window(log);
                }
            } else {
                // Over-shoot, miss < 0.
                if curr.stiction_seen {
                    // Seeing and a low min-move can look like stiction: don't over-react.
                    if stats.stiction_count > 1 {
                        let stiction_corr = ((stats.avg_stiction_amount.abs() / y_rate).floor() + 0.5) as i32 as f64;
                        correction = -stiction_corr;
                        self.remove_oldest_stictions(1);
                        adjust = true;
                        log.push(format!("BLC: Over-shoot, stiction seen, nominal decrease by {}", fmt_f(correction, 6)));
                    } else {
                        log.push("BLC: Over-shoot, first stiction event, no adjustment".into());
                    }
                } else if stats.long_count > stats.short_count && self.blc_index >= 4 {
                    correction = -(corr as f64);
                    self.remove_oldest_overshoots(2);
                    adjust = true;
                    log.push(format!("BLC: Recent history of over-shoots, nominal decrease by {}", fmt_f(correction, 6)));
                } else if avg_init_miss <= -0.1 {
                    correction = -(corr as f64);
                    adjust = true;
                    log.push(format!(
                        "BLC: Average miss indicates over-shooting, nominal decrease by {}",
                        fmt_f(correction, 6)
                    ));
                } else {
                    log.push("BLC: Over-shoot, no adjustment based on avgInitialMiss".into());
                    self.close_window(log);
                }
            }
        } else {
            log.push("BLC: No correction, Miss < min_move".into());
        }

        if adjust {
            self.close_window(log);
            Some(correction)
        } else {
            None
        }
    }
}

// --------------------------------------------------------------- BacklashComp

/// The persisted compensation settings, PHD2's four `/<mount class>/` keys.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BacklashConfig {
    /// `DecBacklashPulse`, ms (default 0: no compensation).
    pub pulse: i32,
    /// `DecBacklashFloor`, ms (0: the minimum).
    pub floor: i32,
    /// `DecBacklashCeiling`, ms (0: `1.5 × pulse`).
    pub ceiling: i32,
    /// `BacklashCompEnabled`; only honoured when `pulse > 0`.
    pub enabled: bool,
}

impl BacklashConfig {
    /// The profile keys, in the order `pulse, floor, ceiling, enabled`.
    pub fn keys(mount_class: &str) -> [String; 4] {
        [
            format!("/{mount_class}/DecBacklashPulse"),
            format!("/{mount_class}/DecBacklashFloor"),
            format!("/{mount_class}/DecBacklashCeiling"),
            format!("/{mount_class}/BacklashCompEnabled"),
        ]
    }

    /// Read the four keys through `get` (`None` = absent, PHD2's defaults
    /// apply; the boolean is any non-zero value, as wxConfig stores it).
    pub fn from_profile(mount_class: &str, get: impl Fn(&str) -> Option<i64>) -> Self {
        let [k_pulse, k_floor, k_ceiling, k_enabled] = Self::keys(mount_class);
        Self {
            pulse: get(&k_pulse).unwrap_or(0) as i32,
            floor: get(&k_floor).unwrap_or(0) as i32,
            ceiling: get(&k_ceiling).unwrap_or(0) as i32,
            enabled: get(&k_enabled).unwrap_or(0) != 0,
        }
    }
}

/// What a [`BacklashComp`] call asks its caller to do — PHD2 does these
/// through `pFrame`, `pConfig` and the scope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlcEffects {
    /// `NotifyGuidingParam` pairs: `("Backlash comp amount", ms)` and
    /// `("Backlash comp enabled", bool)`, the `GuideParamChange` event.
    pub param_changes: Vec<(&'static str, serde_json::Value)>,
    /// Profile writes, key → value (booleans as 0/1).
    pub profile_writes: Vec<(String, i64)>,
    /// `Scope::SetMaxDecDuration`: the pulse exceeded the Dec max duration,
    /// which must be raised to this.
    pub max_dec_duration: Option<i32>,
    /// Debug-log lines, without the trailing newline.
    pub log: Vec<String>,
}

/// `BacklashComp` (`backlash_comp.h:144`): the runtime compensation of one
/// scope.
#[derive(Clone, Debug)]
pub struct BacklashComp {
    mount_class: String,
    comp_active: bool,
    last_direction: Option<Dir>,
    adjustment_floor: i32,
    adjustment_ceiling: i32,
    pulse_width: i32,
    fixed_size: bool,
    history: BlcHistory,
}

impl BacklashComp {
    /// `BacklashComp::BacklashComp` (`backlash_comp.cpp:371`). `mount_class`
    /// is `scope` or `stepguider` (the profile key prefix); `max_dec_duration`
    /// is the scope's current Dec max duration, which the pulse may raise.
    pub fn new(mount_class: &str, cfg: BacklashConfig, max_dec_duration: i32) -> (Self, BlcEffects) {
        let mut fx = BlcEffects::default();
        let mut comp = Self {
            mount_class: mount_class.to_string(),
            comp_active: cfg.pulse > 0 && cfg.enabled,
            last_direction: None,
            adjustment_floor: 0,
            adjustment_ceiling: 0,
            pulse_width: 0,
            fixed_size: false,
            history: BlcHistory::new(),
        };
        comp.set_comp_values(cfg.pulse, cfg.floor, cfg.ceiling, max_dec_duration, &mut fx);
        if comp.comp_active {
            fx.log.push(format!(
                "BLC: Enabled with correction = {} ms, Floor = {}, Ceiling = {}, {}",
                comp.pulse_width,
                comp.adjustment_floor,
                comp.adjustment_ceiling,
                comp.fixed_or_adjustable()
            ));
        } else {
            fx.log.push("BLC: Backlash compensation is disabled".into());
        }
        (comp, fx)
    }

    fn fixed_or_adjustable(&self) -> &'static str {
        if self.fixed_size { "Fixed" } else { "Adjustable" }
    }

    /// `GetBacklashPulseMinValue`: [`MIN_COMP_AMOUNT`].
    pub fn backlash_pulse_min_value() -> i32 {
        MIN_COMP_AMOUNT
    }

    /// `GetBacklashPulseMaxValue`: [`MAX_COMP_AMOUNT`].
    pub fn backlash_pulse_max_value() -> i32 {
        MAX_COMP_AMOUNT
    }

    /// `GetBacklashCompSettings`: `(pulse width, floor, ceiling)`, ms.
    pub fn backlash_comp_settings(&self) -> (i32, i32, i32) {
        (self.pulse_width, self.adjustment_floor, self.adjustment_ceiling)
    }

    /// `GetBacklashPulseWidth`: the current compensation pulse, ms.
    pub fn backlash_pulse_width(&self) -> i32 {
        self.pulse_width
    }

    /// `IsEnabled`.
    pub fn is_enabled(&self) -> bool {
        self.comp_active
    }

    /// `BacklashComp::SetCompValues` (`backlash_comp.cpp:415`): clamp the
    /// three values into a rational set. A floor above the pulse or under the
    /// minimum falls back to the minimum; a ceiling under the pulse to
    /// `1.5 × pulse` (truncated); the set is fixed-size when ceiling and
    /// floor are less than the minimum apart.
    ///
    /// Quirk kept: PHD2's limits are `unsigned int`, so `wxMin(requested,
    /// MAX_COMP_AMOUNT)` and `floor < MIN_COMP_AMOUNT` compare as unsigned. A
    /// negative request wraps above the maximum and becomes 8000 ms; a
    /// negative floor passes the minimum test and is kept. PHD2's own inputs
    /// (spin controls, the GA) are never negative; a profile can be.
    fn set_comp_values(&mut self, requested: i32, floor: i32, ceiling: i32, max_dec_duration: i32, fx: &mut BlcEffects) {
        self.pulse_width = if requested < 0 { MAX_COMP_AMOUNT } else { requested.min(MAX_COMP_AMOUNT) };
        self.adjustment_floor = if floor > self.pulse_width || (0..MIN_COMP_AMOUNT).contains(&floor) {
            MIN_COMP_AMOUNT
        } else {
            floor
        };
        self.adjustment_ceiling = if ceiling < self.pulse_width {
            (1.50 * self.pulse_width as f64).min(MAX_COMP_AMOUNT as f64) as i32
        } else {
            ceiling.min(MAX_COMP_AMOUNT)
        };
        // C int arithmetic: a floor near INT_MIN wraps, as on x86.
        self.fixed_size =
            self.adjustment_ceiling.wrapping_sub(self.adjustment_floor).wrapping_abs() < MIN_COMP_AMOUNT;
        if self.pulse_width > max_dec_duration && self.comp_active {
            fx.max_dec_duration = Some(self.pulse_width);
        }
    }

    /// `SetBacklashPulseWidth` (`backlash_comp.cpp:432`): ask for a set of
    /// values (`ceiling == 0` computes the default). A change of more than
    /// 100 ms clears the history. The three keys are always written back.
    pub fn set_backlash_pulse_width(&mut self, ms: i32, floor: i32, ceiling: i32, max_dec_duration: i32) -> BlcEffects {
        let mut fx = BlcEffects::default();
        if self.pulse_width != ms || self.adjustment_floor != floor || self.adjustment_ceiling != ceiling {
            let old_blc = self.pulse_width;
            self.set_comp_values(ms, floor, ceiling, max_dec_duration, &mut fx);
            fx.param_changes.push(("Backlash comp amount", self.pulse_width.into()));
            fx.log.push(format!(
                "BLC: Comp pulse set to {} ms, Floor = {} ms, Ceiling = {} ms, {}",
                self.pulse_width,
                self.adjustment_floor,
                self.adjustment_ceiling,
                self.fixed_or_adjustable()
            ));
            if (self.pulse_width - old_blc).abs() > 100 {
                self.history.clear_history(&mut fx.log);
                self.history.close_window(&mut fx.log);
            }
        }
        let [k_pulse, k_floor, k_ceiling, _] = BacklashConfig::keys(&self.mount_class);
        fx.profile_writes.push((k_pulse, self.pulse_width.into()));
        fx.profile_writes.push((k_floor, self.adjustment_floor.into()));
        fx.profile_writes.push((k_ceiling, self.adjustment_ceiling.into()));
        fx
    }

    /// `EnableBacklashComp` (`backlash_comp.cpp:453`). Quirk kept: the reset
    /// on enabling runs before the flag flips, so it does nothing when going
    /// from disabled to enabled (`ResetBLCState` checks `m_compActive`).
    pub fn enable_backlash_comp(&mut self, enable: bool) -> BlcEffects {
        let mut fx = BlcEffects::default();
        if self.comp_active != enable {
            fx.param_changes.push(("Backlash comp enabled", enable.into()));
            if enable {
                self.reset_blc_state_into(&mut fx.log);
            }
        }
        self.comp_active = enable;
        let [_, _, _, k_enabled] = BacklashConfig::keys(&self.mount_class);
        fx.profile_writes.push((k_enabled, self.comp_active as i64));
        fx.log.push(format!(
            "BLC: Backlash comp {}, Comp pulse = {} ms",
            if self.comp_active { "enabled" } else { "disabled" },
            self.pulse_width
        ));
        fx
    }

    fn reset_blc_state_into(&mut self, log: &mut Vec<String>) {
        if self.comp_active {
            self.last_direction = None;
            self.history.close_window(log);
            log.push("BLC: Last direction was reset".into());
        }
    }

    /// `ResetBLCState` (`backlash_comp.cpp:468`): forget the last Dec
    /// direction and close the history window (when enabled). Called after a
    /// calibration-type move, a dither or the backlash tool.
    pub fn reset_blc_state(&mut self) -> BlcEffects {
        let mut fx = BlcEffects::default();
        self.reset_blc_state_into(&mut fx.log);
        fx
    }

    /// `TrackBLCResults` (`backlash_comp.cpp:478`): tell the compensation the
    /// raw Dec offset a frame measured after the prior moves. `min_move` is
    /// the Dec algorithm's (−1 when it has none), `y_rate` the mount
    /// calibration's Dec rate (px/ms), `max_dec_duration` the scope's
    /// current one. An adjusted pulse is written to the profile and may
    /// raise the max duration.
    pub fn track_blc_results(
        &mut self,
        move_options: u32,
        y_raw_offset: f64,
        min_move: f64,
        y_rate: f64,
        max_dec_duration: i32,
    ) -> BlcEffects {
        let mut fx = BlcEffects::default();
        if !self.comp_active {
            return fx;
        }
        if move_options & MOVEOPT_USE_BLC == 0 {
            // A calibration-type move can move the mount in Dec without
            // telling the compensation about the direction.
            self.reset_blc_state_into(&mut fx.log);
            return fx;
        }
        // Only algorithm-result moves are tracked, not the fast recovery
        // after a dither, deduced moves or AO bumps.
        let is_algo_result_move = move_options & MOVEOPT_ALGO_RESULT != 0;
        if !is_algo_result_move {
            // A non-algo BLC move happened before the follow-up of the previous one.
            self.history.close_window(&mut fx.log);
            return fx;
        }
        if !self.history.window_open || self.fixed_size {
            return fx;
        }

        // An earlier pulse was applied: this is its follow-up. The sign is
        // not north/south but "needed more" (+) or "needed less" (−).
        let dir = if y_raw_offset > 0.0 { Dir::DOWN } else { Dir::UP };
        let y_distance = y_raw_offset.abs();
        let miss = if Some(dir) == self.last_direction { y_distance } else { -y_distance };
        let min_move = min_move.max(0.0);

        self.history.add_deflection(miss, min_move, &mut fx.log);

        let Some(adjustment) = self.history.adjustment_needed(miss, min_move, y_rate, &mut fx.log) else {
            return fx;
        };

        let nominal_blc = self.pulse_width as f64 + adjustment;
        let new_blc = if nominal_blc > self.pulse_width as f64 {
            let mut n = round((self.pulse_width as f64 * 1.1).min(nominal_blc));
            if n > self.adjustment_ceiling {
                fx.log.push(format!("BLC: Pulse increase limited by ceiling of {}", self.adjustment_ceiling));
                n = self.adjustment_ceiling;
            }
            n
        } else {
            let mut n = round((0.8 * self.pulse_width as f64).max(nominal_blc));
            if n < self.adjustment_floor {
                fx.log.push(format!("BLC: Pulse decrease limited by floor of {}", self.adjustment_floor));
                n = self.adjustment_floor;
            }
            n
        };
        fx.log.push(format!("BLC: Pulse adjusted to {new_blc}"));
        let [k_pulse, ..] = BacklashConfig::keys(&self.mount_class);
        fx.profile_writes.push((k_pulse, new_blc.into()));
        let (floor, ceiling) = (self.adjustment_floor, self.adjustment_ceiling);
        self.set_comp_values(new_blc, floor, ceiling, max_dec_duration, &mut fx);
        fx
    }

    /// `ApplyBacklashComp` (`backlash_comp.cpp:549`): add the compensation
    /// pulse to `y_ms` when this Dec move (`y_guide_distance`, px, signed)
    /// reverses the last direction and the move asks for it.
    pub fn apply_backlash_comp(&mut self, move_options: u32, y_guide_distance: f64, y_ms: &mut i32) -> BlcEffects {
        let mut fx = BlcEffects::default();
        if move_options & MOVEOPT_USE_BLC == 0 {
            return fx;
        }
        if !self.comp_active || self.pulse_width <= 0 || y_guide_distance == 0.0 {
            return fx;
        }
        let dir = if y_guide_distance > 0.0 { Dir::DOWN } else { Dir::UP };
        let is_algo_result_move = move_options & MOVEOPT_ALGO_RESULT != 0;

        match self.last_direction {
            Some(last) if last != dir => {
                *y_ms += self.pulse_width;
                if is_algo_result_move {
                    // Only algorithm-controlled pulses are tracked and tuned.
                    self.history.record_new_blc_pulse(y_guide_distance, &mut fx.log);
                } else {
                    self.history.close_window(&mut fx.log);
                    fx.log.push("BLC: Compensation needed for non-algo type move".into());
                }
                fx.log.push(format!(
                    "BLC: Dec direction reversal from {} to {}, backlash comp pulse of {} applied",
                    last.mount_label(),
                    dir.mount_label(),
                    self.pulse_width
                ));
            }
            _ => {
                if !is_algo_result_move {
                    fx.log.push("BLC: non-algo type move will not reverse Dec direction, no blc applied".into());
                }
            }
        }
        self.last_direction = Some(dir);
        fx
    }
}

// --------------------------------------------------------------- BacklashTool

/// `BacklashTool::BLT_STATE`, PHD2's numbering (it appears in the debug log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BltState {
    Initialize = 0,
    ClearNorth = 1,
    StepNorth = 2,
    StepSouth = 3,
    Aborted = 4,
    TestCorrection = 5,
    Restore = 6,
    Wrapup = 7,
    Completed = 8,
}

/// `BacklashTool::MeasurementResults`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeasurementResult {
    TooFewNorth,
    TooFewSouth,
    BlNotCleared,
    Sanity,
    Valid,
}

/// `BACKLASH_MIN_COUNT`: consecutive north moves of the expected size that
/// count as cleared backlash.
pub const BACKLASH_MIN_COUNT: i32 = 3;
/// `BACKLASH_EXPECTED_DISTANCE`, px: the size of a clearing move.
pub const BACKLASH_EXPECTED_DISTANCE: i32 = 4;
/// `BACKLASH_EXEMPTION_DISTANCE`, px: cumulative north travel past which the
/// measurement goes ahead even though no move was large enough.
pub const BACKLASH_EXEMPTION_DISTANCE: i32 = 40;
/// `MAX_CLEARING_STEPS`.
pub const MAX_CLEARING_STEPS: i32 = 100;
/// `NORTH_PULSE_SIZE`, ms: the minimum measurement pulse.
pub const NORTH_PULSE_SIZE: i32 = 500;
/// `MAX_NORTH_PULSES`, ms: the north leg lasts at least this long.
pub const MAX_NORTH_PULSES: i32 = 8000;
/// `TRIAL_TOLERANCE_AS`, arc-seconds: the test correction's target.
pub const TRIAL_TOLERANCE_AS: i32 = 2;

/// What PHD2's tool reads from the scope, the guider and the frame on each
/// step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BltEnv<'a> {
    /// The scope's last calibration (`GetLastCalibration`, the camera → mount
    /// transform and the Dec rate).
    pub cal: &'a Calibration,
    /// Camera frame size, px (`pCamera->FrameSize`).
    pub frame_width: i32,
    pub frame_height: i32,
    /// `Guider::GetMaxMovePixels`: the search region.
    pub max_move_px: i32,
    /// `Scope::GetCalibrationDuration`, ms.
    pub calibration_duration_ms: i32,
    /// `MyFrame::GetCameraPixelScale`, arc-sec/px (1.0 when unknown).
    pub pixel_scale: f64,
    /// `wxGetUTCTimeMillis`: the time of this step, ms.
    pub now_ms: i64,
}

/// What one [`BacklashTool::dec_measurement_step`] asks the engine to do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BltStep {
    /// `ScheduleAxisMove(scope, dir, ms, MOVEOPTS_CALIBRATION_MOVE)`: the
    /// pulse to issue, at most one per step.
    pub pulse: Option<(Dir, i32)>,
    /// `Scope::SetGuidingEnabled(true)` then
    /// `Guider::EnableMeasurementMode(true)`: star positions now come to the
    /// tool.
    pub enable_measurement: bool,
    /// `CleanUp`: `BacklashComp::reset_blc_state` then
    /// `EnableMeasurementMode(false)`. PHD2 logs the reset's lines just
    /// before `BLT: Cleanup completed`, which is already in `log`.
    pub cleanup: bool,
    /// Debug-log lines, without the trailing newline.
    pub log: Vec<String>,
}

/// `BacklashTool::GetLastDecGuideRate`: the calibration's Dec rate (px/ms),
/// −1 without a valid calibration.
pub fn last_dec_guide_rate(cal: &Calibration) -> f64 {
    if cal.is_valid { cal.y_rate } else { -1.0 }
}

/// `OutOfRoom` (`backlash_comp.cpp:783`): the star is within `margin` of an
/// edge.
fn out_of_room(env: &BltEnv, cam: Point) -> bool {
    let margin = env.max_move_px as f64;
    cam.x < margin
        || cam.y < margin
        || cam.x >= (env.frame_width - env.max_move_px) as f64
        || cam.y >= (env.frame_height - env.max_move_px) as f64
}

/// `BacklashTool` (`backlash_comp.h:56`): the Guiding Assistant's Dec
/// backlash measurement, a state machine stepped once per guide frame with
/// the star's camera position.
#[derive(Clone, Debug)]
pub struct BacklashTool {
    pulse_width: i32,
    step_count: i32,
    north_pulse_count: i32,
    restore_count: i32,
    accepted_moves: i32,
    last_clear_rslt: f64,
    last_dec_guide_rate: f64,
    backlash_result_px: f64,
    cum_clearing_distance: f64,
    backlash_exemption: bool,
    backlash_result_ms: i32,
    north_rate: f64,
    starting_point: Point,
    marker_point: Point,
    end_south: Point,
    last_status: String,
    north_bl_steps: Vec<f64>,
    south_bl_steps: Vec<f64>,
    drift_per_sec: f64,
    north_stats: AxisStats,
    msmt_start_time: i64,
    msmt_end_time: i64,
    state: BltState,
    rslt: MeasurementResult,
}

impl BacklashTool {
    /// `BacklashTool::BacklashTool` (`backlash_comp.cpp:724`): aborted from
    /// the start without a valid calibration. The debug lines come back
    /// with it.
    pub fn new(cal: &Calibration) -> (Self, Vec<String>) {
        let mut log = Vec::new();
        let last_dec_guide_rate = last_dec_guide_rate(cal);
        let (state, last_status) = if last_dec_guide_rate > 0.0 {
            (BltState::Initialize, String::new())
        } else {
            log.push("BLT: Could not get calibration data".into());
            (BltState::Aborted, "Backlash measurement cannot be run - please re-run your mount calibration".to_string())
        };
        let tool = Self {
            pulse_width: 0,
            step_count: 0,
            north_pulse_count: 0,
            restore_count: 0, // left uninitialised by PHD2
            accepted_moves: 0,
            last_clear_rslt: 0.0,
            last_dec_guide_rate,
            backlash_result_px: 0.0,
            cum_clearing_distance: 0.0,
            backlash_exemption: false,
            backlash_result_ms: 0,
            north_rate: 0.0,
            starting_point: Point::default(),
            marker_point: Point::default(),
            end_south: Point::default(),
            last_status,
            north_bl_steps: Vec::new(),
            south_bl_steps: Vec::new(),
            drift_per_sec: 0.0,
            north_stats: AxisStats::new(),
            msmt_start_time: 0,
            msmt_end_time: 0,
            state,
            rslt: MeasurementResult::Valid,
        };
        (tool, log)
    }

    /// `GetBltState`.
    pub fn blt_state(&self) -> BltState {
        self.state
    }

    /// `GetMeasurementQuality`.
    pub fn measurement_quality(&self) -> MeasurementResult {
        self.rslt
    }

    /// `GetBLTMsmtPulseSize`: the pulse used for the north/south steps, ms.
    pub fn blt_msmt_pulse_size(&self) -> i32 {
        self.pulse_width
    }

    /// `GetBacklashResultPx`.
    pub fn backlash_result_px(&self) -> f64 {
        self.backlash_result_px
    }

    /// `GetBacklashResultMs`.
    pub fn backlash_result_ms(&self) -> i32 {
        self.backlash_result_ms
    }

    /// `GetBacklashExempted`: the clearing went ahead on cumulative travel.
    pub fn backlash_exempted(&self) -> bool {
        self.backlash_exemption
    }

    /// `GetLastStatus`: the status line for the GA dialog.
    pub fn last_status(&self) -> &str {
        &self.last_status
    }

    /// `IsGraphable`: the south steps exist.
    pub fn is_graphable(&self) -> bool {
        !self.south_bl_steps.is_empty()
    }

    /// `GetNorthSteps`: the Dec mount position before each north pulse and
    /// after the last.
    pub fn north_steps(&self) -> &[f64] {
        &self.north_bl_steps
    }

    /// `GetSouthSteps`: the same for the south leg.
    pub fn south_steps(&self) -> &[f64] {
        &self.south_bl_steps
    }

    /// `GetBacklashSigma` (`backlash_comp.cpp:1257`): `(px, ms)`, the sigma
    /// of the mean of the north moves plus that of the two south moves, in
    /// quadrature — `sqrt(var/n + 2 var/(n−1))` with `AxisStats`' sample
    /// variance. Zeros unless the result is Valid or TooFewNorth with at
    /// least two north moves.
    pub fn backlash_sigma(&self) -> (f64, f64) {
        if matches!(self.rslt, MeasurementResult::Valid | MeasurementResult::TooFewNorth) && self.north_stats.count() > 1 {
            let variance = self.north_stats.variance();
            let count = self.north_stats.count() as f64;
            let sigma_px = (variance / count + 2.0 * variance / (count - 1.0)).sqrt();
            (sigma_px, sigma_px / self.north_rate)
        } else {
            (0.0, 0.0)
        }
    }

    /// The Guiding Assistant's result text (`guiding_assistant.cpp:764`),
    /// the `Backlash=…` the guide log writes after `INFO: GA Result - `
    /// (the GA appends a newline). `None` unless the result is Valid or
    /// TooFewNorth. Begins with the `>=` preamble, or a space.
    pub fn ga_result_text(&self, pixel_scale: f64) -> Option<String> {
        let qual = self.rslt;
        if !matches!(qual, MeasurementResult::Valid | MeasurementResult::TooFewNorth) {
            return None;
        }
        let (sigma_px, sigma_ms) = self.backlash_sigma();
        let gear_angle = self.backlash_result_px * pixel_scale;
        let gear_angle_sigma = sigma_px * pixel_scale;
        let preamble =
            if self.backlash_result_ms >= 5000 || qual == MeasurementResult::TooFewNorth { ">=" } else { "" };
        let ms = self.backlash_result_ms.max(0);
        Some(if qual == MeasurementResult::Valid {
            format!(
                "Backlash={preamble} {ms}  +/-  {} ms ({}  +/-  {} arc-sec)",
                fmt_f(sigma_ms, 0),
                fmt_f(gear_angle.max(0.0), 1),
                fmt_f(gear_angle_sigma, 1)
            )
        } else {
            format!("Backlash={preamble} {ms}  +/-  ms (test impaired)")
        })
    }

    /// `StartMeasurement` (`backlash_comp.cpp:767`): `drift_per_min` is the
    /// GA's measured Dec drift, px/min; `cam` the star's current position
    /// (`None` when the guider has none).
    pub fn start_measurement(&mut self, drift_per_min: f64, cam: Option<Point>, env: &BltEnv) -> BltStep {
        self.state = BltState::Initialize;
        self.drift_per_sec = drift_per_min / 60.0;
        self.north_bl_steps.clear();
        self.south_bl_steps.clear();
        self.north_stats.clear_all();
        self.dec_measurement_step(cam, env)
    }

    /// `StopMeasurement`.
    pub fn stop_measurement(&mut self, cam: Option<Point>, env: &BltEnv) -> BltStep {
        self.state = BltState::Aborted;
        self.dec_measurement_step(cam, env)
    }

    /// `ComputeBacklashPx` (`backlash_comp.cpp:791`): the apparent backlash
    /// from the first south moves — how far the south leg fell short of the
    /// north rate before the mount moved consistently. A seed for the
    /// compensation, not a hardware measurement. Returns
    /// `(result, px, ms, north rate)`.
    fn compute_backlash_px(&self) -> (MeasurementResult, f64, i32, f64) {
        let mut early_south_moves = 0.0;
        let mut bl_px = 0.0;
        let mut n_rate = 0.0;
        let rslt;
        if self.north_bl_steps.len() > 3 {
            // The drift over the whole north leg, px.
            let drift_amt_px = self.drift_per_sec * (self.msmt_end_time - self.msmt_start_time) as f64 / 1000.0;
            let step_count = self.north_stats.count() as f64;
            let north_delta = self.north_stats.sum();
            // The drift-corrected empirical north rate.
            n_rate = ((north_delta - drift_amt_px) / (step_count * self.pulse_width as f64)).abs();
            let drift_px_per_frame = drift_amt_px / step_count;
            // The expected move is 90 % of the median north move: accepts a
            // south rate that never quite matches the north one.
            let expected_magnitude = (0.9 * self.north_stats.median()).abs();
            let mut good_south_moves = 0;
            let mut last_south_move = 0.0;
            let mut found = None;
            for step in 1..self.south_bl_steps.len() {
                let south_move = self.south_bl_steps[step] - self.south_bl_steps[step - 1];
                early_south_moves += south_move;
                // Big enough and southward; the smoothing accepts a
                // short-long-short pattern.
                if south_move < 0.0
                    && (south_move.abs() >= expected_magnitude
                        || (south_move + last_south_move / 2.0).abs() > expected_magnitude)
                {
                    good_south_moves += 1;
                    // Two consecutive good moves sidestep a "false start" south.
                    if good_south_moves == 2 {
                        // bl = expected travel − actual travel − the drift of that period.
                        bl_px = step as f64 * expected_magnitude
                            - (early_south_moves - step as f64 * drift_px_per_frame).abs();
                        let r = if bl_px * n_rate < -200.0 {
                            MeasurementResult::Sanity
                        } else if bl_px >= 0.7 * north_delta {
                            MeasurementResult::TooFewNorth
                        } else {
                            MeasurementResult::Valid
                        };
                        if bl_px < 0.0 {
                            bl_px = 0.0;
                        }
                        found = Some(r);
                        break;
                    }
                } else if good_south_moves > 0 {
                    good_south_moves -= 1;
                }
                last_south_move = south_move;
            }
            // `found` is set exactly when the second good move was seen
            // (PHD2's `goodSouthMoves < 2` test).
            rslt = found.unwrap_or(MeasurementResult::TooFewSouth);
        } else {
            rslt = MeasurementResult::TooFewNorth;
        }
        (rslt, bl_px, c_int(bl_px / n_rate), n_rate)
    }

    /// `DecMeasurementStep` (`backlash_comp.cpp:881`): one guide frame. `cam`
    /// is the star's camera position, `None` for PHD2's invalid point (the
    /// camera → mount transform fails and the measurement aborts). The step
    /// says which pulse to issue; the next frame after that pulse drives the
    /// next step.
    pub fn dec_measurement_step(&mut self, cam: Option<Point>, env: &BltEnv) -> BltStep {
        let mut out = BltStep::default();
        if self.step_inner(cam, env, &mut out).is_err() {
            out.log.push(format!("BLT: Exception thrown in logical state {}", self.state as i32));
            self.state = BltState::Aborted;
            out.log.push(format!("BLT: {}", self.last_status));
            out.cleanup = true;
            out.log.push("BLT: Cleanup completed".into());
        }
        out.log.push("BLT: Exiting DecMeasurementStep".into());
        out
    }

    /// The body of the state switch; `Err` is PHD2's thrown exception, a
    /// `return Ok` its `break`. The cases run in PHD2's fall-through order.
    fn step_inner(&mut self, cam: Option<Point>, env: &BltEnv, out: &mut BltStep) -> Result<(), ()> {
        let log = &mut out.log;
        let Some(cam) = cam else {
            log.push("BLT: CamToMount xForm failed".into());
            return Err(());
        };
        let curr = camera_to_mount(cam, env.cal);
        let mut dec_delta = 0.0;
        if self.state != BltState::Initialize {
            dec_delta = curr.y - self.marker_point.y;
            self.cum_clearing_distance += dec_delta; // signed
        }
        log.push(format!("BLT: Entering DecMeasurementStep, state = {}", self.state as i32));

        if self.state == BltState::Initialize {
            self.step_count = 0;
            self.marker_point = curr;
            self.starting_point = curr;
            // The clearing pulse comes from the last known guide rate.
            if self.last_dec_guide_rate <= 0.0 {
                self.last_dec_guide_rate = last_dec_guide_rate(env.cal); // maybe calibrated since
            }
            if self.last_dec_guide_rate > 0.0 {
                // px / (px per ms), bumped to sidestep near misses.
                self.pulse_width = c_int(BACKLASH_EXPECTED_DISTANCE as f64 * 1.25 / self.last_dec_guide_rate);
                self.accepted_moves = 0;
                self.last_clear_rslt = 0.0;
                self.cum_clearing_distance = 0.0;
                self.backlash_exemption = false;
                self.rslt = MeasurementResult::Valid;
                // The guider's state machine drives us from here on.
                self.state = BltState::ClearNorth;
                out.enable_measurement = true;
            } else {
                self.state = BltState::Aborted;
                self.last_status = "Backlash measurement cannot be run - Dec guide rate not available".into();
                log.push("BLT: Could not get calibration data".into());
            }
            return Ok(());
        }

        if self.state == BltState::ClearNorth {
            // Three consecutive north moves of at least the expected distance.
            if self.step_count == 0 {
                log.push(format!(
                    "BLT starting North backlash clearing using pulse width of {}, looking for moves >= {} px",
                    self.pulse_width, BACKLASH_EXPECTED_DISTANCE
                ));
                out.pulse = Some((Dir::North, self.pulse_width));
                self.step_count = 1;
                self.last_status = format!("Clearing North backlash, step {}", self.step_count);
                return Ok(());
            }
            if dec_delta.abs() >= BACKLASH_EXPECTED_DISTANCE as f64 {
                if self.accepted_moves == 0 || self.last_clear_rslt * dec_delta > 0.0 {
                    self.accepted_moves += 1;
                    log.push(format!("BLT accepted clearing move of {}", fmt_f(dec_delta, 2)));
                } else {
                    self.accepted_moves = 0; // a direction reversal
                    log.push(format!("BLT rejected clearing move of {}, direction reversal", fmt_f(dec_delta, 2)));
                }
            } else {
                log.push(format!("BLT backlash clearing move of {} px was not large enough", fmt_f(dec_delta, 2)));
            }
            if self.accepted_moves < BACKLASH_MIN_COUNT {
                if self.step_count < MAX_CLEARING_STEPS {
                    if self.cum_clearing_distance.abs() > BACKLASH_EXEMPTION_DISTANCE as f64 {
                        // Moved a long way north in moves each too small —
                        // probably a bad calibration; measure before the
                        // star goes too far.
                        log.push(format!(
                            "BLT: Cum backlash of {} px is at least half of expected, continue with backlash measurement",
                            fmt_f(self.cum_clearing_distance, 2)
                        ));
                        self.backlash_exemption = true;
                    } else if !out_of_room(env, cam) {
                        out.pulse = Some((Dir::North, self.pulse_width));
                        self.step_count += 1;
                        self.marker_point = curr;
                        self.last_clear_rslt = dec_delta;
                        self.last_status = format!(
                            "Clearing North backlash, step {} (up to limit of {})",
                            self.step_count, MAX_CLEARING_STEPS
                        );
                        log.push(format!("BLT: {}, LastDecDelta = {} px", self.last_status, fmt_f(dec_delta, 2)));
                        return Ok(());
                    }
                } else {
                    self.last_status = "Could not clear North backlash - test failed".into();
                    self.rslt = MeasurementResult::BlNotCleared;
                    return Err(());
                }
            }
            if self.accepted_moves >= BACKLASH_MIN_COUNT || self.backlash_exemption || out_of_room(env, cam) {
                self.state = BltState::StepNorth;
                let total_backlash_cleared = (self.step_count * self.pulse_width) as f64;
                // North pulses of at least 500 ms whatever the image scale,
                // but under 70 % of the tracking region per pulse (room for
                // seeing and drift).
                self.pulse_width = NORTH_PULSE_SIZE.max(env.calibration_duration_ms);
                self.pulse_width = self
                    .pulse_width
                    .min((0.7 * env.max_move_px as f64 / self.last_dec_guide_rate).floor() as i32);
                self.step_count = 0;
                // 50 % more than the cleared backlash or at least 8 s, so the
                // south leg has room to clear and get moving.
                self.north_pulse_count = (((MAX_NORTH_PULSES + self.pulse_width - 1) / self.pulse_width) as f64)
                    .max(total_backlash_cleared * 1.5 / self.pulse_width as f64)
                    as i32;
                log.push(format!("BLT: Starting North moves at Dec={}", fmt_f(curr.y, 2)));
                self.msmt_start_time = env.now_ms;
                // falls through to the north moves
            }
        }

        if self.state == BltState::StepNorth {
            if self.step_count < self.north_pulse_count && !out_of_room(env, cam) {
                self.last_status = format!(
                    "Moving North for {} ms, step {} / {}",
                    self.pulse_width,
                    self.step_count + 1,
                    self.north_pulse_count
                );
                let delta_n;
                if self.step_count >= 1 {
                    delta_n = curr.y - self.north_bl_steps.last().copied().unwrap_or(0.0);
                    self.north_stats.add_guide_info(self.step_count as f64, delta_n, 0.0);
                } else {
                    delta_n = 0.0;
                    self.marker_point = curr; // the marker at the start of the north moves
                }
                log.push(format!(
                    "BLT: {}, DecLoc = {}, DeltaDec = {}",
                    self.last_status,
                    fmt_f(curr.y, 2),
                    fmt_f(delta_n, 2)
                ));
                self.north_bl_steps.push(curr.y);
                out.pulse = Some((Dir::North, self.pulse_width));
                self.step_count += 1;
                return Ok(());
            }
            // Finished, or ran out of room.
            self.msmt_end_time = env.now_ms;
            let mut delta_n = 0.0;
            if self.step_count >= 1 {
                delta_n = curr.y - self.north_bl_steps.last().copied().unwrap_or(0.0);
                self.north_stats.add_guide_info(self.step_count as f64, delta_n, 0.0);
            }
            log.push(format!(
                "BLT: North pulses ended at Dec location {}, TotalDecDelta={} px, LastDeltaDec = {}",
                fmt_f(curr.y, 2),
                fmt_f(dec_delta, 2),
                fmt_f(delta_n, 2)
            ));
            self.north_bl_steps.push(curr.y);
            if self.step_count < self.north_pulse_count {
                if (self.step_count as f64) < 0.5 * self.north_pulse_count as f64 {
                    self.last_status = "Star too close to edge for accurate measurement of backlash. Choose a star farther from the edge.".into();
                    self.rslt = MeasurementResult::TooFewNorth;
                    return Err(());
                }
                log.push("BLT: North pulses truncated, too close to frame edge".into());
            }
            self.north_pulse_count = self.step_count;
            self.step_count = 0;
            self.state = BltState::StepSouth;
            // falls through to the south moves
        }

        if self.state == BltState::StepSouth {
            if self.step_count < self.north_pulse_count {
                self.last_status = format!(
                    "Moving South for {} ms, step {} / {}",
                    self.pulse_width,
                    self.step_count + 1,
                    self.north_pulse_count
                );
                log.push(format!("BLT: {}, DecLoc = {}", self.last_status, fmt_f(curr.y, 2)));
                self.south_bl_steps.push(curr.y);
                out.pulse = Some((Dir::South, self.pulse_width));
                self.step_count += 1;
                return Ok(());
            }
            // See where we ended up: compute and test a correction.
            log.push(format!("BLT: South pulses ended at Dec location {}", fmt_f(curr.y, 2)));
            self.south_bl_steps.push(curr.y);
            self.end_south = curr;
            self.state = BltState::TestCorrection;
            self.step_count = 0;
            // falls through
        }

        if self.state == BltState::TestCorrection {
            if self.step_count == 0 {
                let (rslt, px, ms, n_rate) = self.compute_backlash_px();
                self.rslt = rslt;
                self.backlash_result_px = px;
                self.backlash_result_ms = ms;
                self.north_rate = n_rate;
                match self.rslt {
                    MeasurementResult::Sanity => {
                        self.last_status = "Dec movements too erratic - test failed".into();
                        return Err(());
                    }
                    // Not an exception: the test completed, the result is just
                    // not accurate — the GA UI says so.
                    MeasurementResult::TooFewNorth => {}
                    MeasurementResult::TooFewSouth => {
                        self.last_status = "Mount never established consistent south moves - test failed".into();
                        return Err(());
                    }
                    _ => {}
                }
                let (sigma_px, _) = self.backlash_sigma();
                log.push(format!(
                    "BLT: Trial backlash amount is {} px, {} ms, sigma = {} px",
                    fmt_f(self.backlash_result_px, 2),
                    self.backlash_result_ms,
                    fmt_f(sigma_px, 1)
                ));
                if self.backlash_result_ms > 0 {
                    // Don't push the star outside the tracking region.
                    if self.backlash_result_px < env.max_move_px as f64 {
                        self.last_status = format!("Issuing test backlash correction of {} ms", self.backlash_result_ms);
                        log.push(self.last_status.clone());
                        // Back to roughly where the big north pulse started,
                        // unless the backlash is very large.
                        out.pulse = Some((Dir::South, self.backlash_result_ms));
                        self.step_count += 1;
                        return Ok(());
                    }
                    let max_frame_move = (0.8 * env.max_move_px as f64 / self.north_rate).floor() as i32;
                    log.push(format!("BLT: Clearing pulse is very large, issuing max S move of {max_frame_move}"));
                    // One more pulse to cycle the state machine; no fine
                    // tuning, just restore the star to < max-move of error.
                    out.pulse = Some((Dir::South, max_frame_move));
                    self.step_count = 0;
                    self.state = BltState::Restore;
                    return Ok(());
                }
                self.state = BltState::Restore;
                self.step_count = 0;
                // falls through, no test pulse needed
            }
            // See how close the test pulse came (log only).
            if self.state == BltState::TestCorrection {
                log.push(format!(
                    "BLT: Trial backlash pulse resulted in net DecDelta = {} px, Dec Location {}",
                    fmt_f(dec_delta, 2),
                    fmt_f(curr.y, 2)
                ));
                let tol = TRIAL_TOLERANCE_AS as f64 / env.pixel_scale; // px
                if dec_delta.abs() > tol {
                    let pulse_delta = (curr.y - self.end_south.y).abs(); // moved by the test pulse
                    let target_delta = (self.marker_point.y - self.end_south.y).abs(); // needed
                    if (self.end_south.y - self.marker_point.y) * dec_delta < 0.0 {
                        log.push(format!(
                            "BLT: Nominal backlash value over-shot by {} X",
                            fmt_f(target_delta / pulse_delta, 2)
                        ));
                    } else {
                        log.push(format!(
                            "BLT: Nominal backlash value under-shot by {} X",
                            fmt_f(target_delta / pulse_delta, 2)
                        ));
                    }
                } else {
                    log.push(format!(
                        "BLT: Nominal backlash pulse resulted in final delta of {} a-s",
                        fmt_f(dec_delta.abs() * env.pixel_scale, 1)
                    ));
                }
            }
            self.state = BltState::Restore;
            log.push("BLT: normal result, moving to state=restore".into());
            self.step_count = 0;
            // falls through
        }

        if self.state == BltState::Restore {
            // Get back near the starting point without losing the star.
            if self.step_count == 0 {
                log.push(format!(
                    "BLT: Starting Dec position at {}, Ending Dec position at {}",
                    fmt_f(self.marker_point.y, 2),
                    fmt_f(curr.y, 2)
                ));
                let amt = (curr.y - self.starting_point.y).abs();
                if amt > env.max_move_px as f64 {
                    // A zero north rate (too few north steps) gives INT_MIN
                    // here, as in PHD2: no restore pulse.
                    self.restore_count = c_int(((amt / self.north_rate) / self.pulse_width as f64).floor());
                    self.restore_count = self.restore_count.min(10); // something probably went wrong
                    log.push(format!(
                        "BLT: Final restore distance is {} px, approx {} steps",
                        fmt_f(amt, 1),
                        self.restore_count
                    ));
                    self.step_count = 0;
                } else {
                    self.state = BltState::Wrapup;
                }
            }
            if self.step_count < self.restore_count {
                out.pulse = Some((Dir::South, self.pulse_width));
                self.step_count += 1;
                self.last_status = "Restoring star position".into();
                log.push(format!("BLT: Issuing restore pulse count {} of {} ms", self.step_count, self.pulse_width));
                return Ok(());
            }
            self.state = BltState::Wrapup;
            log.push("BLT: normal result, moving to state=wrap-up".into());
            // falls through
        }

        if self.state == BltState::Wrapup {
            self.last_status = "Measurement complete".into();
            out.cleanup = true;
            log.push("BLT: Cleanup completed".into());
            self.state = BltState::Completed;
            return Ok(()); // cycles the guider's state machine: normal guiding resumes
        }

        if self.state == BltState::Aborted {
            self.last_status = "Measurement halted".into();
            log.push("BLT: measurement process halted by user or by error".into());
            out.cleanup = true;
            log.push("BLT: Cleanup completed".into());
        }
        // Completed: nothing to do.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUIDE: u32 = MOVEOPT_ALGO_RESULT | MOVEOPT_USE_BLC;

    fn comp(pulse: i32, floor: i32, ceiling: i32) -> BacklashComp {
        let cfg = BacklashConfig { pulse, floor, ceiling, enabled: true };
        BacklashComp::new("scope", cfg, 2500).0
    }

    #[test]
    fn config_from_profile_reads_the_four_keys() {
        let cfg = BacklashConfig::from_profile("scope", |k| match k {
            "/scope/DecBacklashPulse" => Some(300),
            "/scope/BacklashCompEnabled" => Some(1),
            _ => None,
        });
        assert_eq!(cfg, BacklashConfig { pulse: 300, floor: 0, ceiling: 0, enabled: true });
    }

    #[test]
    fn set_comp_values_clamps_and_defaults_the_ceiling() {
        // Ceiling 0 → 1.5 × pulse (truncated), floor 0 → the minimum.
        assert_eq!(comp(300, 0, 0).backlash_comp_settings(), (300, 20, 450));
        assert_eq!(comp(25, 0, 0).backlash_comp_settings(), (25, 20, 37));
        // The pulse is capped at 8000; the ceiling too.
        assert_eq!(comp(9000, 0, 9000).backlash_comp_settings(), (8000, 20, 8000));
        // A floor above the pulse falls back to the minimum; one under it is kept.
        assert_eq!(comp(300, 400, 600).backlash_comp_settings(), (300, 20, 600));
        assert_eq!(comp(300, 250, 600).backlash_comp_settings(), (300, 250, 600));
        // A ceiling under the pulse is replaced by the default.
        assert_eq!(comp(300, 250, 100).backlash_comp_settings(), (300, 250, 450));
        // Unsigned comparisons: a negative request becomes the maximum, a
        // negative floor is kept.
        assert_eq!(comp(-5, 0, 0).backlash_comp_settings(), (8000, 20, 8000));
        assert_eq!(comp(300, -10, 600).backlash_comp_settings(), (300, -10, 600));
    }

    #[test]
    fn disabled_without_a_pulse_and_max_dec_raised() {
        let cfg = BacklashConfig { pulse: 0, floor: 0, ceiling: 0, enabled: true };
        let (c, fx) = BacklashComp::new("scope", cfg, 2500);
        assert!(!c.is_enabled());
        assert_eq!(fx.max_dec_duration, None);
        assert_eq!(fx.log, ["BLC: Backlash compensation is disabled"]);

        let cfg = BacklashConfig { pulse: 3000, floor: 0, ceiling: 0, enabled: true };
        let (c, fx) = BacklashComp::new("scope", cfg, 2500);
        assert!(c.is_enabled());
        assert_eq!(fx.max_dec_duration, Some(3000));
        assert_eq!(fx.log, ["BLC: Enabled with correction = 3000 ms, Floor = 20, Ceiling = 4500, Adjustable"]);

        // Disabled: the max duration is left alone.
        let cfg = BacklashConfig { pulse: 3000, floor: 0, ceiling: 0, enabled: false };
        assert_eq!(BacklashComp::new("scope", cfg, 2500).1.max_dec_duration, None);
    }

    #[test]
    fn set_pulse_width_notifies_writes_and_clears_history_past_100ms() {
        let mut c = comp(300, 0, 0);
        let fx = c.set_backlash_pulse_width(350, 0, 0, 2500);
        assert_eq!(fx.param_changes, [("Backlash comp amount", serde_json::json!(350))]);
        assert_eq!(
            fx.profile_writes,
            [
                ("/scope/DecBacklashPulse".to_string(), 350),
                ("/scope/DecBacklashFloor".to_string(), 20),
                ("/scope/DecBacklashCeiling".to_string(), 525),
            ]
        );
        assert_eq!(fx.log, ["BLC: Comp pulse set to 350 ms, Floor = 20 ms, Ceiling = 525 ms, Adjustable"]);
        // Same values again: no notification, the keys are still written.
        let fx = c.set_backlash_pulse_width(350, 20, 525, 2500);
        assert!(fx.param_changes.is_empty());
        assert_eq!(fx.profile_writes.len(), 3);
        // A big change clears the history (window closed twice, PHD2's order).
        let fx = c.set_backlash_pulse_width(600, 0, 0, 2500);
        assert_eq!(fx.log[1..], ["BLC: window closed", "BLC: History cleared", "BLC: window closed"]);
        // Fixed size: ceiling − floor < 20.
        c.set_backlash_pulse_width(100, 95, 110, 2500);
        assert_eq!(c.backlash_comp_settings(), (100, 95, 110));
        assert!(c.fixed_size);
    }

    #[test]
    fn enable_notifies_once_and_the_reset_quirk() {
        let mut c = comp(300, 0, 0);
        c.last_direction = Some(Dir::North);
        let fx = c.enable_backlash_comp(false);
        assert_eq!(fx.param_changes, [("Backlash comp enabled", serde_json::json!(false))]);
        assert_eq!(fx.profile_writes, [("/scope/BacklashCompEnabled".to_string(), 0)]);
        assert_eq!(fx.log, ["BLC: Backlash comp disabled, Comp pulse = 300 ms"]);
        // Re-enabling: the reset runs while still disabled, so it does nothing.
        let fx = c.enable_backlash_comp(true);
        assert_eq!(fx.param_changes, [("Backlash comp enabled", serde_json::json!(true))]);
        assert_eq!(fx.log, ["BLC: Backlash comp enabled, Comp pulse = 300 ms"]);
        assert_eq!(c.last_direction, Some(Dir::North));
        // Enabled again: no notification.
        assert!(c.enable_backlash_comp(true).param_changes.is_empty());
        // A real reset.
        let fx = c.reset_blc_state();
        assert_eq!(c.last_direction, None);
        assert_eq!(fx.log, ["BLC: window closed", "BLC: Last direction was reset"]);
    }

    #[test]
    fn apply_adds_the_pulse_only_on_a_reversal_with_use_blc() {
        let mut c = comp(300, 0, 0);
        let mut ms = 100;
        // First move: a direction is remembered, nothing added.
        c.apply_backlash_comp(GUIDE, 1.0, &mut ms);
        assert_eq!(ms, 100);
        // Same direction: nothing.
        c.apply_backlash_comp(GUIDE, 2.0, &mut ms);
        assert_eq!(ms, 100);
        // Reversal: the pulse is added and a history event opened.
        let fx = c.apply_backlash_comp(GUIDE, -1.5, &mut ms);
        assert_eq!(ms, 400);
        assert!(c.history.window_open);
        assert_eq!(
            fx.log,
            ["BLC: Dec direction reversal from South to North, backlash comp pulse of 300 applied"]
        );
        // A reversal without USE_BLC is ignored entirely (direction not even recorded).
        ms = 100;
        c.apply_backlash_comp(MOVEOPT_ALGO_RESULT, 1.0, &mut ms);
        assert_eq!(ms, 100);
        assert_eq!(c.last_direction, Some(Dir::North));
        // A non-algo reversal gets the pulse but closes the window.
        let fx = c.apply_backlash_comp(MOVEOPT_USE_BLC, 1.0, &mut ms);
        assert_eq!(ms, 400);
        assert!(!c.history.window_open);
        assert_eq!(fx.log[0], "BLC: window closed");
        assert_eq!(fx.log[1], "BLC: Compensation needed for non-algo type move");
        // A zero distance is skipped, direction untouched.
        c.apply_backlash_comp(GUIDE, 0.0, &mut ms);
        assert_eq!(c.last_direction, Some(Dir::South));
        // Disabled: nothing.
        c.enable_backlash_comp(false);
        ms = 100;
        c.apply_backlash_comp(GUIDE, -1.0, &mut ms);
        assert_eq!(ms, 100);
    }

    /// One compensated reversal followed by two follow-on raw offsets.
    fn event(c: &mut BacklashComp, trigger: f64, misses: &[f64], y_rate: f64) -> Vec<BlcEffects> {
        let mut ms = 0;
        // Establish the opposite direction first so `trigger` reverses it.
        c.apply_backlash_comp(GUIDE, -trigger, &mut ms);
        c.apply_backlash_comp(GUIDE, trigger, &mut ms);
        misses.iter().map(|m| c.track_blc_results(GUIDE, *m, 0.2, y_rate, 2500)).collect()
    }

    #[test]
    fn undershoot_increases_the_pulse_by_the_average_miss() {
        // yRate 0.01 px/ms, min-move 0.2. The last direction after the
        // trigger (+1 → DOWN) is DOWN; a positive raw offset is DOWN too,
        // hence an under-shoot (+).
        let mut c = comp(300, 0, 0);
        // First event: +0.5 then +0.5 → one entry of 3, avg init miss 0.5,
        // corr = floor(0.5 / 0.01) = 50 → nominal 350, capped at ×1.1 = 330.
        let fx = event(&mut c, 1.0, &[0.5, 0.5], 0.01);
        assert!(fx[0].profile_writes.is_empty()); // waiting for more data
        assert_eq!(fx[0].log.last().unwrap(), "BLC: Under-shoot, no adjustment, waiting for more data");
        assert_eq!(fx[1].profile_writes, [("/scope/DecBacklashPulse".to_string(), 330)]);
        assert_eq!(c.backlash_pulse_width(), 330);
        assert!(!c.history.window_open);
        // A third follow-on lands on a closed window: ignored.
        assert!(c.track_blc_results(GUIDE, 0.5, 0.2, 0.01, 2500).log.is_empty());
        // Second event with a small miss: avg init miss (0.5 + 0.1)/2 = 0.3
        // → corr 30 → 360, under 330 × 1.1 = 363.
        let fx = event(&mut c, 1.0, &[0.1, 0.4], 0.01);
        assert_eq!(fx[0].log.last().unwrap(), "BLC: No correction, Miss < min_move");
        assert_eq!(c.backlash_pulse_width(), 360);
        assert_eq!(fx[1].log.last().unwrap(), "BLC: Pulse adjusted to 360");
        // Ceiling: 450. A huge miss → corr 300 → min(396, 660) = 396 ✓; then
        // 435.6 → 436; then 479.6 → ceiling 450.
        event(&mut c, 1.0, &[3.0, 3.0], 0.01);
        assert_eq!(c.backlash_pulse_width(), 396);
        event(&mut c, 1.0, &[3.0, 3.0], 0.01);
        assert_eq!(c.backlash_pulse_width(), 436);
        let fx = event(&mut c, 1.0, &[3.0, 3.0], 0.01);
        assert_eq!(c.backlash_pulse_width(), 450);
        assert!(fx[1].log.contains(&"BLC: Pulse increase limited by ceiling of 450".to_string()));
        assert_eq!(fx[1].max_dec_duration, None);
    }

    #[test]
    fn overshoot_decreases_the_pulse_from_the_average_miss() {
        let mut c = comp(300, 0, 0);
        // The trigger is +1 (DOWN); a negative raw offset reverses → miss −.
        // First follow-on −0.5: avg init miss −0.5 ≤ −0.1 → corr 50 →
        // nominal 250, floored at 0.8 × 300 = 240 → 250. Decided on the
        // first follow-on, so the window closes and the second is ignored.
        let fx = event(&mut c, 1.0, &[-0.5, -0.5], 0.01);
        assert_eq!(fx[0].log.last().unwrap(), "BLC: Pulse adjusted to 250");
        assert_eq!(
            fx[0].log[fx[0].log.len() - 3],
            "BLC: Average miss indicates over-shooting, nominal decrease by -50.000000"
        );
        assert_eq!(c.backlash_pulse_width(), 250);
        assert!(fx[1].log.is_empty());
        // A −2 miss: avg (−0.5 − 2)/2 = −1.25 → corr 125 → nominal 125,
        // clamped to 0.8 × 250 = 200.
        event(&mut c, 1.0, &[-2.0], 0.01);
        assert_eq!(c.backlash_pulse_width(), 200);
        // Down to the floor of 20: 160, 128, 102, 82, 66, 53, 42, 34, 27, 22, 20.
        for expect in [160, 128, 102, 82, 66, 53, 42, 34, 27, 22] {
            event(&mut c, 1.0, &[-2.0], 0.01);
            assert_eq!(c.backlash_pulse_width(), expect);
        }
        let fx = event(&mut c, 1.0, &[-2.0], 0.01);
        assert_eq!(c.backlash_pulse_width(), 20);
        assert!(fx[0].log.contains(&"BLC: Pulse decrease limited by floor of 20".to_string()));
        // Ten events are kept at most.
        assert_eq!(c.history.events.len(), 10);
    }

    #[test]
    fn stiction_decreases_and_overshoot_history_blocks_an_increase() {
        let mut c = comp(300, 0, 0);
        // Event 1: +0.5 (under-shoot, waiting for the third entry), then
        // −0.3: an over-shoot right after an under-shoot is stiction. The
        // first one changes nothing.
        let fx = event(&mut c, 1.0, &[0.5, -0.3], 0.01);
        assert_eq!(fx[1].log.last().unwrap(), "BLC: Over-shoot, first stiction event, no adjustment");
        assert!(c.history.events[0].stiction_seen);
        // The helper's set-up move (−1 after event 1's +1) is a reversal
        // too and records an event with no follow-on. Second stiction:
        // decrease by the average stiction amount, floor(0.3 / 0.01) + 0.5
        // → 30 → 270 (≥ 0.8 × 300); the oldest stiction event is dropped,
        // leaving [set-up event, current event].
        let fx = event(&mut c, 1.0, &[0.5, -0.3], 0.01);
        assert!(fx[1].log.contains(&"BLC: Over-shoot, stiction seen, nominal decrease by -30.000000".to_string()));
        assert_eq!(fx[1].log.last().unwrap(), "BLC: Pulse adjusted to 270");
        assert_eq!(c.backlash_pulse_width(), 270);
        assert_eq!(c.history.events.len(), 2);
        assert!(!c.history.events[0].stiction_seen && c.history.events[1].stiction_seen);

        // Two over-shoots the positive average keeps from a decrease, then an
        // under-shoot with its three entries is not increased either.
        let mut c = comp(300, 0, 0);
        event(&mut c, 1.0, &[1.0], 0.01); // under-shoot, waiting
        // Average of the first misses (1.0 − 0.3) / 2 = 0.35 > −0.1.
        let fx = event(&mut c, 1.0, &[-0.3], 0.01);
        assert_eq!(fx[0].log[fx[0].log.len() - 2], "BLC: Over-shoot, no adjustment based on avgInitialMiss");
        event(&mut c, 1.0, &[-0.3], 0.01);
        let fx = event(&mut c, 1.0, &[1.0, 1.0], 0.01);
        assert_eq!(fx[1].log.last().unwrap(), "BLC: Under-shoot; no adjustment because of over-shoot history");
        assert_eq!(c.backlash_pulse_width(), 300);
    }

    #[test]
    fn overshoot_majority_needs_five_events() {
        // Each event followed by a −0.5 over-shoot, yRate 0.01: corr =
        // floor(0.5 / 0.01) + 0.5 → 50 either way. Four events (index 3)
        // take the average-miss rule and keep the history.
        let mut h = BlcHistory::new();
        let mut log = Vec::new();
        for _ in 0..4 {
            h.record_new_blc_pulse(1.0, &mut log);
            h.add_deflection(-0.5, 0.2, &mut log);
        }
        assert_eq!(h.adjustment_needed(-0.5, 0.2, 0.01, &mut log), Some(-50.0));
        assert!(log.contains(&"BLC: Average miss indicates over-shooting, nominal decrease by -50.000000".to_string()));
        assert_eq!(h.events.len(), 4);
        // Five (index 4, long 5 > short 0): the over-shoot history rule,
        // which drops the two oldest over-shoot events.
        h.record_new_blc_pulse(1.0, &mut log);
        h.add_deflection(-0.5, 0.2, &mut log);
        log.clear();
        assert_eq!(h.adjustment_needed(-0.5, 0.2, 0.01, &mut log), Some(-50.0));
        assert!(log.contains(&"BLC: Recent history of over-shoots, nominal decrease by -50.000000".to_string()));
        assert_eq!((h.events.len(), h.blc_index), (3, 2));
        assert!(!h.window_open);
    }

    #[test]
    fn fixed_size_and_non_blc_moves_do_not_track() {
        let mut c = comp(100, 95, 110);
        let fx = event(&mut c, 1.0, &[0.5, 0.5], 0.01);
        assert!(fx[1].log.is_empty());
        assert_eq!(c.backlash_pulse_width(), 100);
        // A move without USE_BLC resets the direction.
        let fx = c.track_blc_results(0, 0.5, 0.2, 0.01, 2500);
        assert_eq!(fx.log, ["BLC: window closed", "BLC: Last direction was reset"]);
        assert_eq!(c.last_direction, None);
    }

    // ---------------------------------------------------------- BacklashTool

    fn cal(y_rate: f64) -> Calibration {
        let y_angle = std::f64::consts::FRAC_PI_2;
        Calibration { x_angle: 0.0, y_angle, x_rate: y_rate, y_rate, is_valid: true, ..Default::default() }
    }

    fn env(cal: &Calibration) -> BltEnv<'_> {
        BltEnv {
            cal,
            frame_width: 1000,
            frame_height: 1000,
            max_move_px: 50,
            calibration_duration_ms: 500,
            pixel_scale: 2.0,
            now_ms: 0,
        }
    }

    /// Drive the tool from y = 500 with a mount that answers a north pulse
    /// by `ms × rate` px and a south pulse by `ms × rate × south_factor` px,
    /// less the `backlash_px` of slack it takes up first. With this
    /// calibration the camera y is the mount Dec. Returns the tool, the last
    /// step and the state after each step.
    fn run_tool(
        cal: &Calibration,
        mount_rate: f64,
        backlash_px: f64,
        south_factor: f64,
    ) -> (BacklashTool, BltStep, Vec<BltState>) {
        let (mut tool, _) = BacklashTool::new(cal);
        let mut e = env(cal);
        let mut y = 500.0;
        let mut slack: f64 = 0.0; // backlash taken up going south (0 = cleared north)
        let mut step = tool.start_measurement(0.0, Some(Point::new(500.0, y)), &e);
        let mut states = vec![tool.blt_state()];
        for _ in 0..300 {
            if matches!(tool.blt_state(), BltState::Completed | BltState::Aborted) {
                break;
            }
            if let Some((dir, ms)) = step.pulse {
                let travel = ms as f64 * mount_rate;
                if dir == Dir::North {
                    // Going north gives the slack back first.
                    let given_back = slack.min(travel);
                    slack -= given_back;
                    y += travel - given_back;
                } else {
                    let travel = travel * south_factor;
                    let eaten = travel.min(backlash_px - slack);
                    slack += eaten;
                    y -= travel - eaten;
                }
            }
            e.now_ms += 1000;
            step = tool.dec_measurement_step(Some(Point::new(500.0, y)), &e);
            states.push(tool.blt_state());
        }
        (tool, step, states)
    }

    #[test]
    fn tool_measures_the_backlash_from_the_short_south_moves() {
        // Calibrated at 0.01 px/ms, the mount really moves 0.008: the
        // clearing pulse is 4 × 1.25 / 0.01 = 500 ms → 4 px moves, just
        // enough to count.
        let cal = cal(0.01);
        let (mut t, _) = BacklashTool::new(&cal);
        let e = env(&cal);
        // Initialize only hands over to the clearing, which pulses on the
        // next frame.
        let s0 = t.start_measurement(0.0, Some(Point::new(500.0, 500.0)), &e);
        assert!(s0.enable_measurement && s0.pulse.is_none());
        assert_eq!(t.blt_state(), BltState::ClearNorth);
        let s1 = t.dec_measurement_step(Some(Point::new(500.0, 500.0)), &e);
        assert_eq!(s1.pulse, Some((Dir::North, 500)));

        let (tool, step, _) = run_tool(&cal, 0.008, 10.0, 1.0);
        assert_eq!(tool.blt_state(), BltState::Completed);
        assert!(step.cleanup);
        assert_eq!(tool.measurement_quality(), MeasurementResult::Valid);
        // North pulse: max(500, 500) capped at floor(0.7 × 50 / 0.01) = 3500
        // → 500 ms, 4 px; count max(⌈8000 / 500⌉, 1.5 × 1500 / 500) = 16.
        assert_eq!(tool.blt_msmt_pulse_size(), 500);
        assert_eq!(tool.north_steps().len(), 17);
        // South moves 0, 0, −2, −4, −4: the 10 px of slack go first. The
        // expected move is 0.9 × 4 = 3.6; the good moves are steps 4 and 5:
        // bl = 5 × 3.6 − |−10| = 8 px (the 90 % tolerance reads 10 px of
        // backlash as 8 here). nRate is the measured 64 / (16 × 500) =
        // 0.008, so ms = 8 / 0.008 = 1000.
        assert_eq!(tool.backlash_result_px(), 8.0);
        assert_eq!(tool.backlash_result_ms(), 1000);
        // Identical north moves: zero variance, zero sigma.
        assert_eq!(tool.backlash_sigma(), (0.0, 0.0));
        assert_eq!(tool.ga_result_text(2.0).unwrap(), "Backlash= 1000  +/-  0 ms (16.0  +/-  0.0 arc-sec)");
        assert!(tool.is_graphable());
        assert_eq!(tool.last_status(), "Measurement complete");
    }

    #[test]
    fn tool_sanity_threshold_is_on_bl_times_rate() {
        // No backlash; each south move is 5 px × factor. The first two are
        // the good ones: bl = 2 × 4.5 − 2 × 5 × factor, nRate 0.01.
        // Factor 2000: bl = −19991, × nRate = −199.91, not < −200: Valid,
        // forced to 0, the test completes.
        let cal = cal(0.01);
        let (tool, _, _) = run_tool(&cal, 0.01, 0.0, 2000.0);
        assert_eq!(tool.blt_state(), BltState::Completed);
        assert_eq!(tool.measurement_quality(), MeasurementResult::Valid);
        assert_eq!((tool.backlash_result_px(), tool.backlash_result_ms()), (0.0, 0));
        // Factor 3000: −29991 × 0.01 = −299.91 → Sanity, aborted.
        let (tool, step, _) = run_tool(&cal, 0.01, 0.0, 3000.0);
        assert_eq!(tool.blt_state(), BltState::Aborted);
        assert_eq!(tool.measurement_quality(), MeasurementResult::Sanity);
        assert_eq!(tool.last_status(), "Dec movements too erratic - test failed");
        assert!(step.cleanup);
        assert!(step.log.contains(&"BLT: Exception thrown in logical state 5".to_string()));
        assert_eq!(tool.ga_result_text(2.0), None);
    }

    #[test]
    fn tool_too_few_north_when_backlash_eats_most_of_the_leg() {
        // 16 north steps of 5 px = 80 px; a backlash of 70 px: the south leg
        // finds its two good moves at steps 15-16, bl = 16 × 4.5 − |−10| = 62
        // ≥ 0.7 × 80 = 56 → TooFewNorth, test completed, result impaired.
        let cal = cal(0.01);
        let (tool, _, _) = run_tool(&cal, 0.01, 70.0, 1.0);
        assert_eq!(tool.blt_state(), BltState::Completed);
        assert_eq!(tool.measurement_quality(), MeasurementResult::TooFewNorth);
        assert!((tool.backlash_result_px() - 62.0).abs() < 1e-9);
        assert_eq!(tool.ga_result_text(2.0).unwrap(), "Backlash=>= 6200  +/-  ms (test impaired)");
    }

    #[test]
    fn tool_aborts_on_a_lost_star() {
        let cal = cal(0.01);
        let (mut t, _) = BacklashTool::new(&cal);
        let e = env(&cal);
        t.start_measurement(0.0, Some(Point::new(500.0, 500.0)), &e);
        let step = t.dec_measurement_step(None, &e);
        assert_eq!(t.blt_state(), BltState::Aborted);
        assert!(step.cleanup && step.pulse.is_none());
        assert_eq!(
            step.log,
            [
                "BLT: CamToMount xForm failed",
                "BLT: Exception thrown in logical state 1",
                "BLT: ",
                "BLT: Cleanup completed",
                "BLT: Exiting DecMeasurementStep",
            ]
        );
    }

    #[test]
    fn tool_aborts_without_a_calibration() {
        let cal = Calibration::default();
        let (tool, log) = BacklashTool::new(&cal);
        assert_eq!(tool.blt_state(), BltState::Aborted);
        assert_eq!(log, ["BLT: Could not get calibration data"]);
        assert_eq!(tool.last_status(), "Backlash measurement cannot be run - please re-run your mount calibration");
    }

    #[test]
    fn c_int_cast_matches_x86() {
        assert_eq!(c_int(f64::NAN), i32::MIN);
        assert_eq!(c_int(f64::INFINITY), i32::MIN);
        assert_eq!(c_int(812.9), 812);
        assert_eq!(c_int(-0.5), 0);
    }
}
