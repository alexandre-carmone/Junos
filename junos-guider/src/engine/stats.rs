//! Guiding statistics: the running descriptive stats, the high / low pass
//! filters and the per-axis star displacement sets the Guiding Assistant, the
//! backlash tool and the calibration sanity checks feed.
//!
//! The numbers these produce end up in the guide log (`INFO: GA Result`, the
//! backlash estimate) and in the GA dialog, so PHD2's arithmetic is kept to
//! the letter, including its quirks (each one is named at the spot).
//!
//! Ported from PHD2 src/guiding_stats.h — BSD-3-Clause, see THIRD_PARTY.md
//! Ported from PHD2 src/guiding_stats.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::ops::Deref;

/// `std::numeric_limits<double>::min()`: the smallest *positive* double, which
/// PHD2 uses as the initial maximum where it meant the most negative one
/// (`guiding_stats.cpp:93, 296, 633`). Kept: an all-negative dataset reports
/// this as its maximum displacement.
const DBL_MIN: f64 = f64::MIN_POSITIVE;

/// Running mean, variance, min, max and max step of a stream of values, with
/// Welford's update (no list of values kept).
#[derive(Debug, Clone)]
pub struct DescriptiveStats {
    count: u32,
    running_s: f64,
    running_mean: f64,
    min_value: f64,
    max_value: f64,
    last_value: f64,
    max_delta: f64,
}

impl Default for DescriptiveStats {
    fn default() -> Self {
        Self::new()
    }
}

impl DescriptiveStats {
    /// An empty dataset.
    pub fn new() -> Self {
        let mut s = Self {
            count: 0,
            running_s: 0.0,
            running_mean: 0.0,
            min_value: 0.0,
            max_value: 0.0,
            last_value: 0.0,
            max_delta: 0.0,
        };
        s.clear_all();
        s
    }

    /// Add a value, update every statistic (`guiding_stats.cpp:58`).
    pub fn add_value(&mut self, val: f64) {
        self.count += 1;
        if self.count == 1 {
            self.running_mean = val;
            self.min_value = val;
            self.max_value = val;
            self.max_delta = 0.0;
        } else {
            let new_mean = self.running_mean + (val - self.running_mean) / self.count as f64;
            let new_s = self.running_s + (val - self.running_mean) * (val - new_mean);
            self.running_mean = new_mean;
            self.running_s = new_s;
            self.min_value = self.min_value.min(val);
            self.max_value = self.max_value.max(val);
            let new_delta = (val - self.last_value).abs();
            self.max_delta = self.max_delta.max(new_delta);
        }
        self.last_value = val;
    }

    /// Start over.
    pub fn clear_all(&mut self) {
        self.count = 0;
        self.running_s = 0.0;
        self.running_mean = 0.0;
        self.last_value = 0.0;
        self.min_value = f64::MAX;
        self.max_value = DBL_MIN; // quirk: DBL_MIN, not -DBL_MAX
        self.max_delta = 0.0;
    }

    /// Number of values added.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The value added last (0 before any).
    pub fn last_value(&self) -> f64 {
        self.last_value
    }

    /// Mean of the values (0 when empty).
    pub fn mean(&self) -> f64 {
        if self.count > 0 { self.running_mean } else { 0.0 }
    }

    /// Sum of the values, as `mean × count` (0 when empty).
    pub fn sum(&self) -> f64 {
        if self.count > 0 { self.running_mean * self.count as f64 } else { 0.0 }
    }

    /// Smallest value (0 when empty).
    pub fn minimum(&self) -> f64 {
        if self.count > 0 { self.min_value } else { 0.0 }
    }

    /// Largest value (0 when empty).
    pub fn maximum(&self) -> f64 {
        if self.count > 0 { self.max_value } else { 0.0 }
    }

    /// Quirk: the raw Welford S (the sum of squared deviations), not S/(n−1)
    /// (`guiding_stats.cpp:112`). The backlash tool's sigma estimate is built
    /// on this value, so it stays. 0 unless at least two values.
    pub fn variance(&self) -> f64 {
        if self.count > 1 { self.running_s } else { 0.0 }
    }

    /// Sample standard deviation, sqrt(S/(n−1)). Quirk: with a single value
    /// this is sqrt(0/0) = NaN, as in PHD2 (`guiding_stats.cpp:121`); 0 when
    /// empty.
    pub fn sigma(&self) -> f64 {
        if self.count > 0 {
            (self.running_s / (self.count as f64 - 1.0)).sqrt()
        } else {
            0.0
        }
    }

    /// Population standard deviation, sqrt(S/n) (0 when empty).
    pub fn population_sigma(&self) -> f64 {
        if self.count > 0 {
            (self.running_s / self.count as f64).sqrt()
        } else {
            0.0
        }
    }

    /// Largest |value − previous value| (0 unless at least two values).
    pub fn max_delta(&self) -> f64 {
        if self.count > 1 { self.max_delta } else { 0.0 }
    }
}

/// Single-pole high-pass filter: keeps the fast fluctuations of a stream and
/// forgets a slow drift. No history kept.
#[derive(Debug, Clone)]
pub struct HighPassFilter {
    alpha_cutoff: f64,
    count: u32,
    prev_val: f64,
    hpf_result: f64,
}

impl HighPassFilter {
    /// `alpha = Tc / (Tc + max(1, Ts))`, cutoff and sample periods in seconds
    /// (`guiding_stats.cpp:187`).
    pub fn new(cutoff_period: f64, sample_period: f64) -> Self {
        Self {
            alpha_cutoff: cutoff_period / (cutoff_period + sample_period.max(1.0)),
            count: 0,
            prev_val: 0.0,
            hpf_result: 0.0,
        }
    }

    /// Add a raw value, return the filtered one: the first value passes
    /// through, then `y = alpha (y + x − x_prev)`.
    pub fn add_value(&mut self, new_val: f64) -> f64 {
        if self.count == 0 {
            self.hpf_result = new_val;
        } else {
            self.hpf_result = self.alpha_cutoff * (self.hpf_result + new_val - self.prev_val);
        }
        self.prev_val = new_val;
        self.count += 1;
        self.hpf_result
    }

    /// The last filtered value.
    pub fn current_hpf(&self) -> f64 {
        self.hpf_result
    }

    /// Forget the stream; the cutoff stays.
    pub fn reset(&mut self) {
        self.count = 0;
        self.prev_val = 0.0;
        self.hpf_result = 0.0;
    }
}

/// Single-pole low-pass filter: keeps the slow drift of a stream and smooths
/// the fast fluctuations. No history kept.
#[derive(Debug, Clone)]
pub struct LowPassFilter {
    alpha_cutoff: f64,
    count: u32,
    lpf_result: f64,
}

impl LowPassFilter {
    /// `alpha = 1 − Tc / (Tc + max(1, Ts))`, periods in seconds
    /// (`guiding_stats.cpp:226`).
    pub fn new(cutoff_period: f64, sample_period: f64) -> Self {
        Self {
            alpha_cutoff: 1.0 - (cutoff_period / (cutoff_period + sample_period.max(1.0))),
            count: 0,
            lpf_result: 0.0,
        }
    }

    /// Add a raw value, return the filtered one: the first value passes
    /// through, then `y += alpha (x − y)`.
    pub fn add_value(&mut self, new_val: f64) -> f64 {
        if self.count == 0 {
            self.lpf_result = new_val;
        } else {
            self.lpf_result += self.alpha_cutoff * (new_val - self.lpf_result);
        }
        self.count += 1;
        self.lpf_result
    }

    /// The last filtered value.
    pub fn current_lpf(&self) -> f64 {
        self.lpf_result
    }

    /// Forget the stream; the cutoff stays.
    pub fn reset(&mut self) {
        self.count = 0;
        self.lpf_result = 0.0;
    }
}

/// One guide-star sample on an axis: a relative time (seconds since the start
/// of guiding, only used by the linear fit), the displacement, and whether a
/// pulse was sent and whether it reversed the previous one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StarDisplacement {
    /// Seconds since the start of the dataset.
    pub delta_time: f64,
    /// Star displacement on the axis.
    pub star_pos: f64,
    /// A non-zero guide pulse went with this sample.
    pub guided: bool,
    /// That pulse reversed the direction of the previous non-zero pulse.
    pub reversal: bool,
}

impl StarDisplacement {
    /// An unguided sample.
    pub fn new(when: f64, where_: f64) -> Self {
        Self { delta_time: when, star_pos: where_, guided: false, reversal: false }
    }
}

/// The result of [`AxisStats::linear_fit`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearFit {
    /// Displacement per second.
    pub slope: f64,
    /// Displacement at t = 0.
    pub intercept: f64,
    /// Sample sigma of the displacements once the fitted drift is removed.
    pub sigma: f64,
    /// R², the coefficient of determination of the fit.
    pub r_squared: f64,
}

/// A set of [`StarDisplacement`] samples on one axis with running sums, min /
/// max, the max step and the move / reversal counters. Unbounded; see
/// [`WindowedAxisStats`] for the windowed variant.
#[derive(Debug, Clone)]
pub struct AxisStats {
    guiding_entries: Vec<StarDisplacement>,
    axis_moves: u32,
    axis_reversals: u32,
    prev_move: f64,
    prev_position: f64,
    sum_x: f64,
    sum_y: f64,
    sum_xy: f64,
    sum_x_sq: f64,
    sum_y_sq: f64,
    max_displacement: f64,
    min_displacement: f64,
    max_delta: f64,
    /// Index of the entry whose step from its predecessor is `max_delta`.
    /// PHD2 never initialises it and never resets it in `InitializeScalars`
    /// (`guiding_stats.cpp:284`); it starts at 0 here (the only use is a "must
    /// recompute" test, so a stale value costs a recomputation at worst). It
    /// goes negative after removals, hence the signed type.
    max_delta_inx: i64,
}

impl Default for AxisStats {
    fn default() -> Self {
        Self::new()
    }
}

impl AxisStats {
    /// An empty dataset.
    pub fn new() -> Self {
        let mut s = Self {
            guiding_entries: Vec::new(),
            axis_moves: 0,
            axis_reversals: 0,
            prev_move: 0.0,
            prev_position: 0.0,
            sum_x: 0.0,
            sum_y: 0.0,
            sum_xy: 0.0,
            sum_x_sq: 0.0,
            sum_y_sq: 0.0,
            max_displacement: 0.0,
            min_displacement: 0.0,
            max_delta: 0.0,
            max_delta_inx: 0,
        };
        s.initialize_scalars();
        s
    }

    /// Drop every sample and reset the scalars.
    pub fn clear_all(&mut self) {
        self.initialize_scalars();
        self.guiding_entries.clear();
    }

    fn initialize_scalars(&mut self) {
        self.axis_moves = 0;
        self.axis_reversals = 0;
        self.sum_y = 0.0;
        self.sum_y_sq = 0.0;
        self.sum_x = 0.0;
        self.sum_xy = 0.0;
        self.sum_x_sq = 0.0;
        self.prev_position = 0.0;
        self.prev_move = 0.0;
        self.min_displacement = f64::MAX;
        self.max_displacement = DBL_MIN; // quirk: DBL_MIN, not -DBL_MAX
        self.max_delta = 0.0;
    }

    /// Number of samples with a non-zero guide pulse.
    pub fn move_count(&self) -> u32 {
        self.axis_moves
    }

    /// Number of times consecutive non-zero pulses changed direction.
    pub fn reversal_count(&self) -> u32 {
        self.axis_reversals
    }

    /// The sample at `inx`, or an all-zero one out of range.
    pub fn entry(&self, inx: usize) -> StarDisplacement {
        self.guiding_entries.get(inx).copied().unwrap_or(StarDisplacement::new(0.0, 0.0))
    }

    /// Add a sample: relative time (a small number, seconds since the start,
    /// not a time of day), displacement and the guide pulse that went with it
    /// (`guiding_stats.cpp:322`).
    pub fn add_guide_info(&mut self, delta_t: f64, star_pos: f64, guide_amt: f64) {
        let mut star_info = StarDisplacement::new(delta_t, star_pos);

        self.min_displacement = star_pos.min(self.min_displacement);
        self.max_displacement = star_pos.max(self.max_displacement);

        self.sum_x += delta_t;
        self.sum_xy += delta_t * star_pos;
        self.sum_x_sq += delta_t * delta_t;
        self.sum_y_sq += star_pos * star_pos;
        self.sum_y += star_pos;

        if guide_amt != 0.0 {
            star_info.guided = true;
            self.axis_moves += 1;
            // A zero pulse in between does not break the pair: prev_move only
            // follows non-zero pulses.
            if guide_amt * self.prev_move < 0.0 {
                self.axis_reversals += 1;
                star_info.reversal = true;
            }
            self.prev_move = guide_amt;
        }

        // Quirk: the step between the first two samples is never measured —
        // PHD2 waits for two entries before looking at a delta
        // (`guiding_stats.cpp:347`).
        if self.guiding_entries.len() > 1 {
            let new_delta = (star_info.star_pos - self.prev_position).abs();
            if new_delta >= self.max_delta {
                self.max_delta = new_delta;
                // Where the entry is going: the furthest down among equals.
                self.max_delta_inx = self.guiding_entries.len() as i64;
            }
        }

        self.guiding_entries.push(star_info);
        self.prev_position = star_pos;
    }

    /// The sample added last, or an all-zero one when empty.
    pub fn last_entry(&self) -> StarDisplacement {
        self.guiding_entries.last().copied().unwrap_or(StarDisplacement::new(0.0, 0.0))
    }

    /// Largest |step| between consecutive samples (0 unless at least two).
    pub fn max_delta(&self) -> f64 {
        if self.guiding_entries.len() > 1 { self.max_delta } else { 0.0 }
    }

    /// Number of samples in the set.
    pub fn count(&self) -> usize {
        self.guiding_entries.len()
    }

    /// Sum of the displacements.
    pub fn sum(&self) -> f64 {
        self.sum_y
    }

    /// Mean displacement (0 when empty).
    pub fn mean(&self) -> f64 {
        let sz = self.guiding_entries.len();
        if sz > 0 { self.sum_y / sz as f64 } else { 0.0 }
    }

    /// Sample variance, `(n Σy² − (Σy)²) / (n (n−1))` (0 unless at least two).
    pub fn variance(&self) -> f64 {
        let sz = self.guiding_entries.len();
        if sz > 1 {
            let n = sz as f64;
            (n * self.sum_y_sq - self.sum_y * self.sum_y) / (n * (n - 1.0))
        } else {
            0.0
        }
    }

    /// Sample standard deviation; 0 unless at least two samples, and 0 when
    /// cancellation drives the variance negative.
    pub fn sigma(&self) -> f64 {
        let sz = self.guiding_entries.len();
        if sz > 1 {
            let n = sz as f64;
            let variance = (n * self.sum_y_sq - self.sum_y * self.sum_y) / (n * (n - 1.0));
            if variance >= 0.0 { variance.sqrt() } else { 0.0 }
        } else {
            0.0
        }
    }

    /// Population standard deviation; 0 unless at least two samples, and 0
    /// when cancellation drives the variance negative.
    pub fn population_sigma(&self) -> f64 {
        let sz = self.guiding_entries.len();
        if sz > 1 {
            let n = sz as f64;
            let variance = (n * self.sum_y_sq - self.sum_y * self.sum_y) / (n * n);
            if variance >= 0.0 { variance.sqrt() } else { 0.0 }
        } else {
            0.0
        }
    }

    /// Median displacement by sorting a copy; the mean of the two middle
    /// values for an even count, 0 when empty.
    pub fn median(&self) -> f64 {
        let sz = self.guiding_entries.len();
        if sz > 1 {
            let mut sorted: Vec<f64> = self.guiding_entries.iter().map(|e| e.star_pos).collect();
            sorted.sort_by(f64::total_cmp);
            let ctr = sorted.len() / 2;
            if sorted.len() % 2 == 1 {
                sorted[ctr]
            } else {
                (sorted[ctr] + sorted[ctr - 1]) / 2.0
            }
        } else if sz == 1 {
            self.guiding_entries[0].star_pos
        } else {
            0.0
        }
    }

    /// Smallest (signed) displacement (0 when empty).
    pub fn min_displacement(&self) -> f64 {
        if self.guiding_entries.is_empty() { 0.0 } else { self.min_displacement }
    }

    /// Largest (signed) displacement (0 when empty).
    pub fn max_displacement(&self) -> f64 {
        if self.guiding_entries.is_empty() { 0.0 } else { self.max_displacement }
    }

    /// Ordinary least squares of displacement over time from the running
    /// sums (`guiding_stats.cpp:524`): the slope gives a drift (a calibration
    /// angle, a polar alignment error), the detrended sigma the seeing. All
    /// zero with fewer than two samples.
    pub fn linear_fit(&self) -> LinearFit {
        let num_vals = self.guiding_entries.len();
        if num_vals <= 1 {
            return LinearFit { slope: 0.0, intercept: 0.0, sigma: 0.0, r_squared: 0.0 };
        }
        let n = num_vals as f64;

        let slope = ((n * self.sum_xy) - (self.sum_x * self.sum_y))
            / ((n * self.sum_x_sq) - (self.sum_x * self.sum_x));
        let intcpt = (self.sum_y - (slope * self.sum_x)) / n;

        // Sigma of the samples with the fit removed. Note the Welford-like
        // update divides the delta by the total count, not the running one,
        // and squares the delta from the old mean alone — PHD2's arithmetic,
        // kept as is.
        let mut current_variance = 0.0;
        let mut current_mean = 0.0;
        for (inx, e) in self.guiding_entries.iter().enumerate() {
            let new_val = e.star_pos - (e.delta_time * slope + intcpt);
            if inx == 0 {
                current_mean = new_val;
            } else {
                let delta = new_val - current_mean;
                let new_mean = current_mean + delta / n;
                current_variance += delta * delta;
                current_mean = new_mean;
            }
        }
        let sigma = (current_variance / (n - 1.0)).sqrt();

        let syy = self.sum_y_sq - (self.sum_y * self.sum_y) / n;
        let sxy = self.sum_xy - (self.sum_x * self.sum_y) / n;
        let sxx = self.sum_x_sq - (self.sum_x * self.sum_x) / n;
        let sse = syy - (sxy * sxy) / sxx;
        let r_squared = (syy - sse) / syy;

        LinearFit { slope, intercept: intcpt, sigma, r_squared }
    }
}

/// An [`AxisStats`] limited to the most recent samples: trimmed on each add
/// when built with a window size, or by hand through
/// [`remove_oldest_entry`](Self::remove_oldest_entry) otherwise. Every
/// read-only accessor of `AxisStats` is reachable through `Deref`.
#[derive(Debug, Clone)]
pub struct WindowedAxisStats {
    stats: AxisStats,
    auto_windowing: bool,
    window_size: usize,
}

impl Default for WindowedAxisStats {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Deref for WindowedAxisStats {
    type Target = AxisStats;

    fn deref(&self) -> &AxisStats {
        &self.stats
    }
}

impl WindowedAxisStats {
    /// Auto-trimmed to `auto_window_size` samples when > 0; manual trimming
    /// only when 0.
    pub fn new(auto_window_size: usize) -> Self {
        Self { stats: AxisStats::new(), auto_windowing: auto_window_size > 0, window_size: auto_window_size }
    }

    /// Drop every sample; the window setting stays.
    pub fn clear_all(&mut self) {
        self.stats.clear_all();
    }

    /// Change the auto window, trimming the oldest samples down to `new_size`
    /// at once. 0 turns auto-trimming off without discarding anything.
    /// Always true (PHD2 returns false only for a negative size, which the
    /// unsigned parameter rules out).
    pub fn change_window_size(&mut self, new_size: usize) -> bool {
        if new_size > 0 {
            while self.stats.count() > new_size {
                self.remove_oldest_entry();
            }
            self.window_size = new_size;
            self.auto_windowing = true;
        } else {
            self.auto_windowing = false;
            self.window_size = 0;
        }
        true
    }

    /// Recompute min, max and max step before entry 0 goes, and only when
    /// that entry holds one of them (`guiding_stats.cpp:620`). Quirk kept:
    /// the "max step involves the leaving entry" test is `max_delta_inx ==
    /// 0`, while the step stored at index 1 is the one measured from entry 0.
    fn adjust_min_max_values(&mut self) {
        let s = &mut self.stats;
        let target = s.guiding_entries[0];
        let mut recal_needed = false;
        let mut prev = target.star_pos;

        if s.guiding_entries.len() > 1 {
            recal_needed = target.star_pos == s.max_displacement
                || target.star_pos == s.min_displacement
                || s.max_delta_inx == 0;
            if recal_needed {
                s.min_displacement = f64::MAX;
                s.max_displacement = DBL_MIN;
                s.max_delta = 0.0;
            }
        }

        if recal_needed {
            // Skip index 0, it is about to be removed.
            for (inx, entry) in s.guiding_entries.iter().enumerate().skip(1) {
                s.min_displacement = s.min_displacement.min(entry.star_pos);
                s.max_displacement = s.max_displacement.max(entry.star_pos);
                if inx > 1 && (entry.star_pos - prev).abs() > s.max_delta {
                    s.max_delta = (entry.star_pos - prev).abs();
                    s.max_delta_inx = inx as i64;
                }
                prev = entry.star_pos;
            }
        }
    }

    /// Drop the oldest sample and take it out of every statistic.
    pub fn remove_oldest_entry(&mut self) {
        if self.stats.guiding_entries.is_empty() {
            return;
        }
        let target = self.stats.guiding_entries[0];
        let val = target.star_pos;
        let delta_t = target.delta_time;
        let s = &mut self.stats;
        s.sum_y -= val;
        s.sum_y_sq -= val * val;
        s.sum_x -= delta_t;
        s.sum_x_sq -= delta_t * delta_t;
        s.sum_xy -= delta_t * val;
        if target.reversal {
            s.axis_reversals -= 1;
        }
        if target.guided {
            s.axis_moves -= 1;
        }
        self.adjust_min_max_values();
        self.stats.guiding_entries.remove(0);
        self.stats.max_delta_inx -= 1;
    }

    /// [`AxisStats::add_guide_info`], then drop the oldest sample when the
    /// window overflows.
    pub fn add_guide_info(&mut self, delta_t: f64, star_pos: f64, guide_amt: f64) {
        self.stats.add_guide_info(delta_t, star_pos, guide_amt);
        if self.auto_windowing && self.stats.guiding_entries.len() > self.window_size {
            self.remove_oldest_entry();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn descriptive_stats_of_1_2_3_4() {
        let mut d = DescriptiveStats::new();
        for v in [1.0, 2.0, 3.0, 4.0] {
            d.add_value(v);
        }
        assert_eq!(d.count(), 4);
        assert_eq!(d.mean(), 2.5);
        assert_eq!(d.sum(), 10.0);
        assert_eq!(d.minimum(), 1.0);
        assert_eq!(d.maximum(), 4.0);
        assert_eq!(d.max_delta(), 1.0);
        assert_eq!(d.last_value(), 4.0);
        // Quirk: raw S, not S/(n-1).
        assert_eq!(d.variance(), 5.0);
        assert!(close(d.sigma(), (5.0f64 / 3.0).sqrt()));
        assert!(close(d.population_sigma(), (5.0f64 / 4.0).sqrt()));
    }

    #[test]
    fn descriptive_stats_welford_and_max_delta() {
        let mut d = DescriptiveStats::new();
        for v in [2.5, -1.0, 4.25, 0.5, 3.0] {
            d.add_value(v);
        }
        assert!(close(d.mean(), 1.85));
        assert!(close(d.variance(), 17.45));
        assert_eq!(d.minimum(), -1.0);
        assert_eq!(d.maximum(), 4.25);
        assert_eq!(d.max_delta(), 5.25);
    }

    #[test]
    fn descriptive_stats_boundaries() {
        let mut d = DescriptiveStats::new();
        assert_eq!(d.sigma(), 0.0);
        assert_eq!(d.mean(), 0.0);
        assert_eq!(d.maximum(), 0.0);
        d.add_value(7.0);
        // Quirk: sqrt(0/0) with a single value.
        assert!(d.sigma().is_nan());
        assert_eq!(d.population_sigma(), 0.0);
        assert_eq!(d.variance(), 0.0);
        assert_eq!(d.max_delta(), 0.0);
        assert_eq!(d.minimum(), 7.0);
        d.clear_all();
        assert_eq!(d.count(), 0);
        assert_eq!(d.sum(), 0.0);
    }

    #[test]
    fn hpf_of_a_constant_decays_to_zero() {
        // alpha = 1 / (1 + 1) = 0.5: y_k = 3 * 0.5^(k-1).
        let mut h = HighPassFilter::new(1.0, 1.0);
        assert_eq!(h.add_value(3.0), 3.0);
        assert_eq!(h.add_value(3.0), 1.5);
        assert_eq!(h.add_value(3.0), 0.75);
        assert_eq!(h.add_value(3.0), 0.375);
        assert_eq!(h.add_value(3.0), 0.1875);
        for _ in 0..60 {
            h.add_value(3.0);
        }
        assert!(h.current_hpf().abs() < 1e-12);
        h.reset();
        assert_eq!(h.current_hpf(), 0.0);
        assert_eq!(h.add_value(3.0), 3.0);
    }

    #[test]
    fn hpf_sample_period_is_clamped_to_one_second() {
        // max(1, 0.5) = 1 → same alpha as (1, 1).
        let mut a = HighPassFilter::new(1.0, 0.5);
        let mut b = HighPassFilter::new(1.0, 1.0);
        for v in [1.0, 2.0, -3.0, 0.5] {
            assert_eq!(a.add_value(v), b.add_value(v));
        }
    }

    #[test]
    fn lpf_converges_to_a_step() {
        // alpha = 1 - 10/12 = 1/6.
        let mut l = LowPassFilter::new(10.0, 2.0);
        assert_eq!(l.add_value(0.0), 0.0);
        assert!(close(l.add_value(12.0), 2.0));
        assert!(close(l.add_value(12.0), 3.666666666666666));
        assert!(close(l.add_value(12.0), 5.0555555555555545));
        for _ in 0..300 {
            l.add_value(12.0);
        }
        assert!((l.current_lpf() - 12.0).abs() < 1e-9);
        l.reset();
        assert_eq!(l.add_value(5.0), 5.0);
    }

    fn axis(samples: &[(f64, f64, f64)]) -> AxisStats {
        let mut a = AxisStats::new();
        for &(t, y, g) in samples {
            a.add_guide_info(t, y, g);
        }
        a
    }

    #[test]
    fn axis_stats_sums_and_sigmas() {
        let a = axis(&[(0.0, 5.0, 0.0), (1.0, 1.0, 0.0), (2.0, 9.0, 0.0), (3.0, 2.0, 0.0)]);
        assert_eq!(a.count(), 4);
        assert_eq!(a.sum(), 17.0);
        assert_eq!(a.mean(), 4.25);
        assert!(close(a.variance(), 12.916666666666666));
        assert!(close(a.sigma(), 3.593976442141304));
        assert!(close(a.population_sigma(), 3.112474899497183));
        assert_eq!(a.min_displacement(), 1.0);
        assert_eq!(a.max_displacement(), 9.0);
        // Quirk: the 5 → 1 step is never measured; 1 → 9 is the max.
        assert_eq!(a.max_delta(), 8.0);
        assert_eq!(a.last_entry().star_pos, 2.0);
        assert_eq!(a.entry(1).delta_time, 1.0);
        assert_eq!(a.entry(9), StarDisplacement::new(0.0, 0.0));
    }

    #[test]
    fn axis_stats_median_odd_even_single_empty() {
        assert_eq!(AxisStats::new().median(), 0.0);
        assert_eq!(axis(&[(0.0, 4.5, 0.0)]).median(), 4.5);
        assert_eq!(axis(&[(0.0, 3.0, 0.0), (1.0, 1.0, 0.0), (2.0, 2.0, 0.0)]).median(), 2.0);
        assert_eq!(
            axis(&[(0.0, 4.0, 0.0), (1.0, 1.0, 0.0), (2.0, 3.0, 0.0), (3.0, 2.0, 0.0)]).median(),
            2.5
        );
    }

    #[test]
    fn axis_stats_moves_and_reversals() {
        // A zero pulse does not break the pair (-1 then -2 is no reversal).
        let a = axis(&[
            (0.0, 0.0, 1.0),
            (1.0, 0.0, -1.0),
            (2.0, 0.0, 0.0),
            (3.0, 0.0, -2.0),
            (4.0, 0.0, 3.0),
            (5.0, 0.0, 0.0),
        ]);
        assert_eq!(a.move_count(), 4);
        assert_eq!(a.reversal_count(), 2);
        assert!(a.entry(1).reversal && a.entry(1).guided);
        assert!(!a.entry(3).reversal && a.entry(3).guided);
        assert!(a.entry(4).reversal);
        assert!(!a.entry(5).guided);
    }

    #[test]
    fn axis_stats_max_init_is_dbl_min() {
        // Quirk: an all-negative set reports DBL_MIN as its maximum.
        let a = axis(&[(0.0, -2.0, 0.0), (1.0, -1.0, 0.0)]);
        assert_eq!(a.max_displacement(), f64::MIN_POSITIVE);
        assert_eq!(a.min_displacement(), -2.0);
    }

    #[test]
    fn axis_stats_empty_and_single_are_zero() {
        let mut a = AxisStats::new();
        assert_eq!(a.sigma(), 0.0);
        assert_eq!(a.max_displacement(), 0.0);
        assert_eq!(a.last_entry(), StarDisplacement::new(0.0, 0.0));
        a.add_guide_info(0.0, 3.0, 0.0);
        assert_eq!(a.variance(), 0.0);
        assert_eq!(a.max_delta(), 0.0);
        assert_eq!(a.linear_fit(), LinearFit { slope: 0.0, intercept: 0.0, sigma: 0.0, r_squared: 0.0 });
        a.clear_all();
        assert_eq!(a.count(), 0);
        assert_eq!(a.sum(), 0.0);
    }

    #[test]
    fn linear_fit_of_an_exact_line() {
        let a = axis(&[(0.0, 1.0, 0.0), (1.0, 3.0, 0.0), (2.0, 5.0, 0.0), (3.0, 7.0, 0.0), (4.0, 9.0, 0.0)]);
        let f = a.linear_fit();
        assert!(close(f.slope, 2.0));
        assert!(close(f.intercept, 1.0));
        assert!(close(f.sigma, 0.0));
        assert!(close(f.r_squared, 1.0));
    }

    #[test]
    fn linear_fit_of_a_noisy_line() {
        let a = axis(&[(0.0, 1.0, 0.0), (1.5, 3.2, 0.0), (3.0, 4.9, 0.0), (4.5, 7.1, 0.0), (6.0, 9.0, 0.0)]);
        let f = a.linear_fit();
        assert!(close(f.slope, 1.3266666666666667));
        assert!(close(f.intercept, 1.06));
        assert!(close(f.sigma, 0.13380747961156744));
        assert!(close(f.r_squared, 0.9987138101482906));
    }

    #[test]
    fn windowed_stats_drop_the_oldest() {
        let mut w = WindowedAxisStats::new(3);
        w.add_guide_info(0.0, 5.0, 1.0);
        w.add_guide_info(1.0, 1.0, -1.0);
        w.add_guide_info(2.0, 9.0, 0.0);
        assert_eq!(w.count(), 3);
        assert_eq!(w.move_count(), 2);
        w.add_guide_info(3.0, 2.0, 0.0);
        // The 5 at t=0 (a guided entry) is gone.
        assert_eq!(w.count(), 3);
        assert_eq!(w.entry(0).star_pos, 1.0);
        assert_eq!(w.sum(), 12.0);
        assert_eq!(w.mean(), 4.0);
        assert_eq!(w.min_displacement(), 1.0);
        assert_eq!(w.max_displacement(), 9.0);
        assert_eq!(w.move_count(), 1);
        assert_eq!(w.reversal_count(), 1);
        assert_eq!(w.max_delta(), 8.0);
        assert!(close(w.sigma(), 4.358898943540674));
        // The sums follow: the fit sees only t = 1..3.
        let f = w.linear_fit();
        assert!(close(f.intercept, 3.0));
        assert!(close(f.slope, 0.5));
    }

    #[test]
    fn windowed_stats_recompute_when_the_max_leaves() {
        let mut w = WindowedAxisStats::new(3);
        w.add_guide_info(0.0, 9.0, 0.0);
        w.add_guide_info(1.0, 1.0, 0.0);
        w.add_guide_info(2.0, 5.0, 0.0);
        w.add_guide_info(3.0, 2.0, 0.0);
        assert_eq!(w.max_displacement(), 5.0);
        assert_eq!(w.min_displacement(), 1.0);
        assert_eq!(w.max_delta(), 4.0);
    }

    #[test]
    fn windowed_stats_manual_and_resize() {
        let mut w = WindowedAxisStats::new(0);
        for i in 0..5 {
            w.add_guide_info(i as f64, i as f64, 0.0);
        }
        assert_eq!(w.count(), 5);
        w.remove_oldest_entry();
        assert_eq!(w.count(), 4);
        assert_eq!(w.sum(), 10.0);
        assert!(w.change_window_size(2));
        assert_eq!(w.count(), 2);
        assert_eq!(w.entry(0).star_pos, 3.0);
        assert_eq!(w.sum(), 7.0);
        assert!(w.change_window_size(0));
        w.add_guide_info(5.0, 5.0, 0.0);
        assert_eq!(w.count(), 3);
        w.clear_all();
        assert_eq!(w.count(), 0);
        let mut empty = WindowedAxisStats::new(0);
        empty.remove_oldest_entry();
        assert_eq!(empty.count(), 0);
    }
}
