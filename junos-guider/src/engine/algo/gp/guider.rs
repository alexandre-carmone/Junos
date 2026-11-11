//! PHD2's `GaussianProcessGuider`, the controller behind Predictive PEC.
//!
//! Every frame it records the RA offset, its noise (from the star's SNR) and
//! the move it then issues, in a ring buffer. The running sum of the moves
//! plus the offset is the mount's gear error; once more than ten points are
//! in, the GP is refitted to that gear error on a 5 s grid (the worm period
//! re-estimated from its spectrum after a few periods) and the move becomes
//! the reactive part `control_gain · input` plus `prediction_gain` times the
//! gear error the GP expects over the next exposure. Until enough periods
//! are in, the result is blended with a hysteresis controller.
//!
//! Time comes from the caller (`now`, monotonic seconds) where PHD2 reads
//! `std::chrono::steady_clock`. PHD2's debug-log traces (`GPDebug`) are left
//! out, as in the other algorithms; with them go the members only the traces
//! read (`control_signal_`, `prediction_`), now locals.
//!
//! Ported from PHD2 contributions/MPI_IS_gaussian_process/src/gaussian_process_guider.h, contributions/MPI_IS_gaussian_process/src/gaussian_process_guider.cpp, src/circbuf.h — BSD-3-Clause, see THIRD_PARTY.md

use std::collections::VecDeque;
use std::f64::consts::PI;

use super::gp::Gp;
use super::math::{RegularizeOverrun, detrend, estimate_period_length, regularize_dataset};

/// Raw data points kept (`CIRCULAR_BUFFER_SIZE`); past it the oldest go.
pub const CIRCULAR_BUFFER_SIZE: usize = 8192;
/// Frames a dither runs on dead reckoning unless settling ends it sooner.
pub const MAX_DITHER_STEPS: i32 = 10;
/// Weight of each new period estimate in the running period.
pub const DEFAULT_LEARNING_RATE: f64 = 0.01;
/// The hysteresis of the controller blended in before the GP takes over.
pub const HYSTERESIS: f64 = 0.1;

// The hyperparameter indices, PHD2's `Hyperparameters` enum.
pub const SE0K_LENGTH_SCALE: usize = 0;
pub const SE0K_SIGNAL_VARIANCE: usize = 1;
pub const PK_LENGTH_SCALE: usize = 2;
pub const PK_SIGNAL_VARIANCE: usize = 3;
pub const SE1K_LENGTH_SCALE: usize = 4;
pub const SE1K_SIGNAL_VARIANCE: usize = 5;
pub const PK_PERIOD_LENGTH: usize = 6;
pub const NUM_PARAMETERS: usize = 7;

/// One frame: the offset measured and the move issued **before** it (point
/// `i` holds measurement `i` and the control computed from measurement
/// `i − 1`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DataPoint {
    /// Gear time, seconds: the midpoint of the frame interval, plus the
    /// dither offset.
    pub timestamp: f64,
    /// The offset, px.
    pub measurement: f64,
    /// The measurement's variance, px².
    pub variance: f64,
    /// The move, px.
    pub control: f64,
}

/// PHD2's `guide_parameters`: the controller settings, and the kernels'
/// hyperparameters (natural units, indexed by [`SE0K_LENGTH_SCALE`] …
/// [`PK_PERIOD_LENGTH`]) the guider starts from. All zero by default, as
/// PHD2's constructor leaves them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuideParameters {
    pub control_gain: f64,
    pub min_move: f64,
    pub prediction_gain: f64,
    pub min_periods_for_inference: f64,
    pub min_periods_for_period_estimation: f64,
    pub points_for_approximation: i32,
    pub compute_period: bool,
    pub hyperparameters: [f64; NUM_PARAMETERS],
}

/// `std::max(a, b)`: `a` unless `a < b`, so a NaN `a` stays NaN.
fn cmax(a: f64, b: f64) -> f64 {
    if a < b { b } else { a }
}

/// `GaussianProcessGuider`.
#[derive(Clone, Debug)]
pub struct GaussianProcessGuider {
    /// Clock reading of gear time 0 (`start_time_`).
    start_time: f64,
    /// Clock reading of the previous data point (`last_time_`).
    last_time: f64,
    /// Gear time the last prediction ended at; negative before the first.
    last_prediction_end: f64,
    dither_steps: i32,
    dithering_active: bool,
    /// Gear time (s) the dithers moved the mount by.
    dither_offset: f64,
    /// Index 0 is the oldest point, the back the newest (`circbuf.h`).
    data: VecDeque<DataPoint>,
    gp: Gp,
    learning_rate: f64,
    parameters: GuideParameters,
}

impl GaussianProcessGuider {
    /// `GaussianProcessGuider(parameters)` at clock reading `now`: one point
    /// with a zero control, the hyperparameters set from `parameters`.
    pub fn new(parameters: GuideParameters, now: f64) -> Self {
        let mut data = VecDeque::with_capacity(CIRCULAR_BUFFER_SIZE);
        data.push_back(DataPoint::default());
        let mut g = GaussianProcessGuider {
            start_time: now,
            last_time: now,
            // 0 here, −1 after a reset: the first result overwrites it.
            last_prediction_end: 0.0,
            dither_steps: 0,
            dithering_active: false,
            dither_offset: 0.0,
            data,
            gp: Gp::new(),
            learning_rate: DEFAULT_LEARNING_RATE,
            parameters,
        };
        let hyperparameters = g.parameters.hyperparameters;
        g.set_gp_hyperparameters(&hyperparameters);
        g
    }

    /// Number of points held, the open one (control only) included.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    fn last_point(&mut self) -> &mut DataPoint {
        self.data.back_mut().expect("the buffer always holds a point")
    }

    /// `add_one_point`: `circular_buffer::push_front`, dropping the oldest
    /// point when full.
    fn add_one_point(&mut self) {
        if self.data.len() == CIRCULAR_BUFFER_SIZE {
            self.data.pop_front();
        }
        self.data.push_back(DataPoint::default());
    }

    /// `SetTimestamp`: the midpoint of the interval since the previous
    /// point, in gear time.
    fn set_timestamp(&mut self, now: f64) {
        let delta_measurement_time = now - self.last_time;
        self.last_time = now;
        let timestamp = (now - self.start_time) - delta_measurement_time / 2.0 + self.dither_offset;
        self.last_point().timestamp = timestamp;
    }

    /// `HandleGuiding`: records the offset and its variance; predictions
    /// restart from this point, since what lies before it was measured.
    fn handle_guiding(&mut self, input: f64, snr: f64, now: f64) {
        self.set_timestamp(now);
        let variance = calculate_variance(snr);
        let p = self.last_point();
        p.measurement = input;
        p.variance = variance;
        self.last_prediction_end = p.timestamp;
    }

    /// `HandleDarkGuiding`: no measurement, a zero with a huge variance.
    fn handle_dark_guiding(&mut self, now: f64) {
        self.set_timestamp(now);
        let p = self.last_point();
        p.measurement = 0.0;
        p.variance = 1e4;
    }

    fn handle_controls(&mut self, control_input: f64) {
        self.last_point().control = control_input;
    }

    /// `UpdateGP`: refits the GP to the gear error of every point but the
    /// newest, the one just measured (parity: PHD2 calls it before the open
    /// point gets its control, and leaves that point out).
    fn update_gp(&mut self, prediction_point: f64) -> Result<(), RegularizeOverrun> {
        let n = self.data.len() - 1;
        let mut timestamps = Vec::with_capacity(n);
        let mut gear_error = Vec::with_capacity(n);
        let mut variances = Vec::with_capacity(n);
        let mut sum_control = 0.0;
        for p in self.data.iter().take(n) {
            sum_control += p.control;
            timestamps.push(p.timestamp);
            gear_error.push(sum_control + p.measurement);
            variances.push(p.variance);
        }

        let reg = regularize_dataset(&timestamps, &gear_error, &variances, self.dithering_active)?;

        let period_length = self.gp_hyperparameters()[PK_PERIOD_LENGTH];
        if self.parameters.compute_period
            && self.data[n].timestamp > self.parameters.min_periods_for_period_estimation * period_length
        {
            // PHD2 detrends on every update; only the period estimate reads it.
            let gear_error_detrend = detrend(&reg.timestamps, &reg.gear_error);
            self.update_period_length(estimate_period_length(&reg.timestamps, &gear_error_detrend));
        }

        // The GP models the trend itself: it gets the raw gear error.
        let points = self.parameters.points_for_approximation.max(0) as usize;
        self.gp.infer_sd(&reg.timestamps, &reg.gear_error, points, &reg.variances, prediction_point);
        Ok(())
    }

    /// `UpdatePeriodLength`: moves the period 1 % of the way to the new
    /// estimate (a NaN estimate keeps it), through the hyperparameter setter
    /// so the periodic length scale is re-converted for the new period.
    fn update_period_length(&mut self, mut period_length: f64) {
        let mut hypers = self.gp_hyperparameters();
        if period_length.is_nan() {
            period_length = hypers[PK_PERIOD_LENGTH];
        }
        hypers[PK_PERIOD_LENGTH] =
            (1.0 - self.learning_rate) * hypers[PK_PERIOD_LENGTH] + self.learning_rate * period_length;
        self.set_gp_hyperparameters(&hypers);
    }

    /// `PredictGearError`: the gear error the GP expects between the end of
    /// the last prediction and `prediction_location` (plus the dither
    /// offset). Parity: [`Self::update_gp`] is centred without that offset.
    fn predict_gear_error(&mut self, prediction_location: f64, now: f64) -> f64 {
        if self.last_prediction_end < 0.0 {
            self.last_prediction_end = now - self.start_time;
        }
        let next_location = [self.last_prediction_end, prediction_location + self.dither_offset];
        let prediction = self.gp.predict_projected(&next_location);
        self.last_prediction_end = next_location[1];
        prediction[1] - prediction[0]
    }

    /// `result`: the move (px) for this frame's offset `input` (px), the
    /// star's SNR and the time to the next frame (`time_step`, s), at clock
    /// reading `now`.
    pub fn result(&mut self, input: f64, snr: f64, time_step: f64, now: f64) -> f64 {
        // While dithering the measurement is not trusted: record a dark
        // point, but move proportionally (no min-move).
        if self.dithering_active {
            self.dither_steps -= 1;
            if self.dither_steps <= 0 {
                self.dithering_active = false;
            }
            if self.deduce_result(time_step, now).is_err() {
                // PHD2 catches the regularisation's runtime_error and starts over.
                self.reset(now);
            }
            return self.parameters.control_gain * input;
        }

        // Gear time starts at the first measurement after a reset.
        if self.data.len() == 1 {
            self.start_time = now;
            self.last_time = now;
        }

        self.handle_guiding(input, snr, now);

        // Parity: `get_second_last_point` is the point measured last frame,
        // whose control is the move issued two frames ago, not the last one.
        let mut last_control = 0.0;
        if self.data.len() > 1 {
            last_control = self.data[self.data.len() - 2].control;
        }
        let mut hysteresis_control = (1.0 - HYSTERESIS) * input + HYSTERESIS * last_control;
        hysteresis_control *= self.parameters.control_gain;

        let mut control_signal = self.parameters.control_gain * input;
        if input.abs() < self.parameters.min_move {
            control_signal = 0.0;
            hysteresis_control = 0.0;
        }

        if self.data.len() > 10 {
            let prediction_point = now - self.start_time;
            // The highest precision should lie between now and the next frame.
            self.update_gp(prediction_point + 0.5 * time_step)
                .expect("regularize_dataset only fails while dithering");
            let prediction = self.predict_gear_error(prediction_point + time_step, now);
            // Added even when min-move zeroed the reactive part.
            control_signal += self.parameters.prediction_gain * prediction;

            // Blend from hysteresis to the GP over the first periods.
            let period_length = self.gp_hyperparameters()[PK_PERIOD_LENGTH];
            let timestamp = self.last_point().timestamp;
            if timestamp < self.parameters.min_periods_for_inference * period_length {
                let mut percentage = timestamp / (self.parameters.min_periods_for_inference * period_length);
                if 1.0 < percentage {
                    percentage = 1.0;
                }
                control_signal = percentage * control_signal + (1.0 - percentage) * hysteresis_control;
            }
        }

        if control_signal.is_nan() {
            control_signal = hysteresis_control;
        }

        // The control belongs to the next point in time.
        self.add_one_point();
        self.handle_controls(control_signal);
        control_signal
    }

    /// `deduceResult`: dead reckoning when no star was measured — a dark
    /// point, and the full predicted gear error over `time_step` once the GP
    /// has `min_periods_for_inference` periods behind it (else 0).
    ///
    /// `Err` where PHD2 throws: regularisation overran its grid while
    /// dithering (a negative dither offset), before any point was added.
    pub fn deduce_result(&mut self, time_step: f64, now: f64) -> Result<f64, RegularizeOverrun> {
        self.handle_dark_guiding(now);

        let mut control_signal = 0.0;
        let period_length = self.gp_hyperparameters()[PK_PERIOD_LENGTH];
        if self.data.len() > 10
            && self.last_point().timestamp > self.parameters.min_periods_for_inference * period_length
        {
            let prediction_point = now - self.start_time;
            self.update_gp(prediction_point + 0.5 * time_step)?;
            control_signal += self.predict_gear_error(prediction_point + time_step, now);
        }

        self.add_one_point();
        // Parity: stored before the NaN guard below.
        self.handle_controls(control_signal);

        if control_signal.is_nan() {
            control_signal = 0.0;
        }
        Ok(control_signal)
    }

    /// `reset` at clock reading `now`: forgets the data, the GP fit and any
    /// dither.
    pub fn reset(&mut self, now: f64) {
        self.data.clear();
        self.gp.clear_data();
        // Measurements are relative to the control: the first point has none.
        self.data.push_back(DataPoint::default());
        self.last_prediction_end = -1.0;
        self.start_time = now;
        self.last_time = now;
        self.dither_offset = 0.0;
        self.dither_steps = 0;
        self.dithering_active = false;
    }

    /// `GuidingDithered`: a dither of `amt` px at `rate` px per second of
    /// gear time shifts gear time by `amt / rate`; the next
    /// [`MAX_DITHER_STEPS`] frames run on dead reckoning.
    pub fn guiding_dithered(&mut self, amt: f64, rate: f64) {
        self.dither_offset += amt / rate;
        self.dithering_active = true;
        self.dither_steps = MAX_DITHER_STEPS;
    }

    /// `GuidingDitherSettleDone`: on success the next frame ends the dither
    /// (it still runs through the dithering branch, which corrects for the
    /// time offset).
    pub fn guiding_dither_settle_done(&mut self, success: bool) {
        if success {
            self.dither_steps = 1;
        }
    }

    // `DirectMoveApplied` is a no-op in PHD2 (its body is commented out),
    // so it is not ported.

    pub fn control_gain(&self) -> f64 {
        self.parameters.control_gain
    }

    pub fn set_control_gain(&mut self, control_gain: f64) {
        self.parameters.control_gain = control_gain;
    }

    pub fn min_move(&self) -> f64 {
        self.parameters.min_move
    }

    pub fn set_min_move(&mut self, min_move: f64) {
        self.parameters.min_move = min_move;
    }

    pub fn period_lengths_inference(&self) -> f64 {
        self.parameters.min_periods_for_inference
    }

    pub fn set_period_lengths_inference(&mut self, num_periods: f64) {
        self.parameters.min_periods_for_inference = num_periods;
    }

    pub fn period_lengths_period_estimation(&self) -> f64 {
        self.parameters.min_periods_for_period_estimation
    }

    pub fn set_period_lengths_period_estimation(&mut self, num_periods: f64) {
        self.parameters.min_periods_for_period_estimation = num_periods;
    }

    pub fn num_points_for_approximation(&self) -> i32 {
        self.parameters.points_for_approximation
    }

    pub fn set_num_points_for_approximation(&mut self, num_points: i32) {
        self.parameters.points_for_approximation = num_points;
    }

    pub fn compute_period(&self) -> bool {
        self.parameters.compute_period
    }

    pub fn set_compute_period(&mut self, active: bool) {
        self.parameters.compute_period = active;
    }

    pub fn prediction_gain(&self) -> f64 {
        self.parameters.prediction_gain
    }

    pub fn set_prediction_gain(&mut self, prediction_gain: f64) {
        self.parameters.prediction_gain = prediction_gain;
    }

    /// `SetLearningRate` (PHD2's tests set 0 to freeze the period).
    pub fn set_learning_rate(&mut self, learning_rate: f64) {
        self.learning_rate = learning_rate;
    }

    /// `GetGPHyperparameters`: the GP's parameters back in natural units,
    /// the periodic length scale converted back with
    /// `asin(ℓ/4) · P / π` — which folds a length scale above P/2 back
    /// below it (`4·sin` is not invertible past there).
    pub fn gp_hyperparameters(&self) -> [f64; NUM_PARAMETERS] {
        let full = self.gp.get_hyper_parameters();
        let mut h = [0.0; NUM_PARAMETERS];
        for (i, v) in h.iter_mut().enumerate() {
            // Index 0 of the GP's vector is its unused noise sd.
            *v = full[i + 1].exp();
        }
        h[PK_LENGTH_SCALE] = (h[PK_LENGTH_SCALE] / 4.0).asin() * h[PK_PERIOD_LENGTH] / PI;
        h
    }

    /// `SetGPHyperparameters`: length scales clamped to ≥ 1 (smaller ones
    /// make the GP unstable), the periodic one converted from natural units
    /// with `4 · sin(ℓ · π / P)`, everything floored at 1e-10 for the log,
    /// and handed to the GP as logs behind a unit noise sd.
    pub fn set_gp_hyperparameters(&mut self, hyperparameters: &[f64; NUM_PARAMETERS]) {
        let mut h = *hyperparameters;
        for i in [SE0K_LENGTH_SCALE, PK_LENGTH_SCALE, SE1K_LENGTH_SCALE] {
            h[i] = cmax(h[i], 1.0);
        }
        h[PK_LENGTH_SCALE] = 4.0 * (h[PK_LENGTH_SCALE] * PI / h[PK_PERIOD_LENGTH]).sin();
        let mut full = [0.0; NUM_PARAMETERS + 1];
        full[0] = 1.0f64.ln();
        for i in 0..NUM_PARAMETERS {
            full[i + 1] = cmax(h[i], 1e-10).ln();
        }
        self.gp.set_hyper_parameters(&full);
    }
}

/// `CalculateVariance`: the measurement variance (px²) for a star's SNR,
/// `(2.1752 / (SNR − 3.3) + 0.5)²` from PHD2's simulations, the SNR
/// floored at 3.4.
fn calculate_variance(snr: f64) -> f64 {
    let snr = cmax(snr, 3.4);
    let standard_deviation = 2.1752 / (snr - 3.3) + 0.5;
    standard_deviation * standard_deviation
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PHD2's defaults (guide_algorithm_gaussian_process.cpp:55).
    fn phd2_defaults() -> GuideParameters {
        GuideParameters {
            control_gain: 0.6,
            min_move: 0.2,
            prediction_gain: 0.5,
            min_periods_for_inference: 2.0,
            min_periods_for_period_estimation: 2.0,
            points_for_approximation: 100,
            compute_period: true,
            hyperparameters: [700.0, 20.0, 10.0, 20.0, 25.0, 10.0, 200.0],
        }
    }

    /// The harness' generator: a 64-bit LCG, 53 bits to [0, 1).
    fn next_unit(s: &mut u64) -> f64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*s >> 11) as f64 / 9007199254740992.0
    }

    const T0_NS: i64 = 1_000_000_000_000;

    /// The closed loop the C++ harness runs: frames 2 s apart (plus up to
    /// 1/8 s of jitter in 1/512 s steps, so the clock is exact in both
    /// languages), an RA error of a 230.5 s, 2 px sine plus a 0.002 px/s
    /// drift, uniform ±0.2 px noise, SNR 10–50; the input is that error less
    /// the moves made so far. With `events`, a 1.5 px dither at a rate of
    /// 2 px/s after frame 150 (the lock moves by 1.5 px), its settle after
    /// frame 153, and a lost star (dead reckoning) at frame 260.
    fn run(frames: usize, events: bool) -> (GaussianProcessGuider, Vec<f64>) {
        let mut gpg = GaussianProcessGuider::new(phd2_defaults(), T0_NS as f64 / 1e9);
        gpg.reset(T0_NS as f64 / 1e9);
        let mut s: u64 = 0x9E3779B97F4A7C15;
        let (mut applied, mut shift) = (0.0, 0.0);
        let mut controls = Vec::with_capacity(frames);
        for k in 0..frames {
            let jitter = (next_unit(&mut s) * 64.0) as i64;
            let t_ns = T0_NS + k as i64 * 2_000_000_000 + jitter * 1_953_125;
            let now = t_ns as f64 / 1e9;
            let noise = (next_unit(&mut s) - 0.5) * 0.4;
            let snr = 10.0 + 40.0 * next_unit(&mut s);
            let t = (t_ns - T0_NS) as f64 / 1e9;
            let pe = 2.0 * (2.0 * PI * t / 230.5).sin() + 0.002 * t;
            let input = pe - applied + noise + shift;
            let u = if events && k == 260 {
                gpg.deduce_result(2.0, now).unwrap()
            } else {
                gpg.result(input, snr, 2.0, now)
            };
            applied += u;
            if events && k == 150 {
                gpg.guiding_dithered(1.5, 2.0);
                shift = 1.5;
            }
            if events && k == 153 {
                gpg.guiding_dither_settle_done(true);
            }
            controls.push(u);
        }
        (gpg, controls)
    }

    fn close(rust: f64, cpp: f64) -> bool {
        (rust - cpp).abs() <= 1e-6 * cpp.abs() || (rust - cpp).abs() <= 1e-9
    }

    /// The controls of [`run`] with events, printed (`%.17g`) by a harness
    /// that compiles PHD2's own gaussian_process_guider.cpp, gaussian_process.cpp,
    /// covariance_functions.cpp and math_tools.cpp against Eigen 3.4.1, with
    /// `steady_clock` swapped for a settable clock.
    #[rustfmt::skip]
    const CPP_CONTROLS: [f64; 300] = [
        0.0, 0.0, 0.0,
        0.31190734104337331, 0.0, 0.16944031257372857,
        0.22491565602864413, 0.0, 0.21512366410107123,
        0.0, 0.00091808212744759845, 0.21058736035959852,
        -0.00056031969976758891, 0.16437202237726206, 0.11175626742482096,
        0.0016360666110502022, 0.15561365522711382, 0.0069133083003575287,
        0.19057736693361801, 0.0051266246195877533, 0.0051132424080473906,
        0.0050922719052854046, 0.13340140203217141, 0.0052550450505810479,
        0.14785133161408029, -0.0041627014681395917, -0.0050133024389987539,
        -0.0026840911295407067, -0.0025823967534279878, 0.011340966390587692,
        0.13432897651832235, 0.014905652631069018, 0.0086093967378781285,
        0.0086738758229623729, -0.16640517554374015, -0.12809279434845317,
        -0.0092996883813929333, -0.012592766834242691, -0.12496775979922443,
        0.00035347527608558475, 0.0024915375510876228, 0.0046551047913408541,
        -0.0044319534133438043, -0.0032737640962411611, 0.0062511445393527119,
        -0.28357089871531033, -0.12649459503796412, -0.01226482295412489,
        -0.2179728389975622, -0.01142972314116269, -0.16425451377368455,
        -0.0092031445031490484, -0.025990080061525207, -0.1987380515626887,
        -0.16243950994623516, 0.009288221075887261, 0.013672691220477488,
        -0.19024950350246533, -0.0088235289062616636, -0.22361854831351766,
        -0.22217407926175117, -0.16543614809357154, -0.024432778792919919,
        -0.023807077711503782, -0.0078373107225817151, -0.14460194464889553,
        -0.20474677833626798, -0.0077291355902866718, -0.0050502248458888423,
        -0.010870221850528917, -0.2646880608888898, -0.13471928434457389,
        -0.021168814997014451, -0.019489011159359582, -0.017373460708135776,
        -0.0050675506058811947, -0.12255126487283016, -0.14605669266143967,
        -0.13761845496225394, -0.03204331557871766, -0.036105417872651695,
        -0.038893094016907367, -0.038994619768349899, -0.041019131947279061,
        -0.011518907060992664, -0.011473799815872583, -0.012212171633268499,
        0.14382549632090921, -0.12769861574195857, 0.0095932519147471328,
        0.0071565845170588963, 0.0044443895060297493, -0.025038447221954085,
        -0.025676844493135165, -0.011635164084796788, 0.19858426817603753,
        -0.0017335627163489614, 0.24226088520936431, 0.019262366285838755,
        0.038848543274714924, 0.17957471042686401, 0.048153206126078131,
        0.043383675760556489, 0.16651281349004388, 0.022835399009717039,
        0.020193546868650714, 0.018040707704758626, 0.29876146948709464,
        0.021998731815748993, 0.026477415502754537, 0.14209896402519137,
        0.026087510806976241, 0.16869720799721738, 0.14343689637578599,
        0.22261839856405274, 0.028608563172218358, 0.02613132616957967,
        0.24296399770182517, 0.15099837845754252, 0.17071468644258972,
        0.027078699623627019, 0.22073982334928532, 0.026727861595945077,
        0.025371400156761179, 0.018753518847976245, 0.016208640565543206,
        0.30045418665028523, 0.0079954115034806018, 0.0020718448008362231,
        0.21397044144038346, 0.16008996449645244, -0.0083245615975067744,
        0.17520348887490486, -0.0068586951669050876, 0.17266505467176771,
        -0.010096610935307289, -0.011871781016710627, 0.13165683633274242,
        0.00039839993821421277, 0.13421952947438989, -0.012157191173340355,
        -0.017202852517945826, -0.030084060147366871, -0.037426620642741493,
        -0.045813297565866128, 0.18746804762582067, -0.051312838995518063,
        -0.037226976127206275, -0.042478002936556521, -0.042606100927545944,
        -0.045344832794947512, 0.99568408086480664, 0.35609344750694299,
        0.22243539760898093, 0.05036376582067268, -0.045718769632258877,
        -0.22374575901168137, -0.051614885526808485, -0.053587724149027682,
        -0.058198142636125383, -0.061078323071997273, -0.063624168554720209,
        -0.066283891793595007, -0.06434839736187814, -0.19970247314017306,
        -0.059418560783460271, -0.054506746414775674, -0.049201867150444535,
        -0.043355327631858141, -0.24091927269067112, -0.036759913892343302,
        -0.22822226753504732, -0.036954273175312684, -0.038566977695651483,
        -0.25791190884059129, -0.041365733413422229, -0.041936084366327016,
        -0.18213796467376273, -0.17428406461569018, -0.03072838662891314,
        -0.024827858619212686, -0.019808927821557694, -0.15468733331578391,
        -0.012458308338575461, -0.17484752183272775, -0.01187419804376488,
        -0.013144788307289677, -0.017876023665443226, -0.17045579465939337,
        -0.16828544480173108, -0.1984983993110821, -0.0047461089081924325,
        -0.0045201970051058499, -0.14402384466671891, 0.020374729114509479,
        0.032849011063922046, 0.0428087774221017, -0.11201859278510339,
        0.051008372748410732, -0.12724675714845285, 0.059290362808289719,
        -0.11015297780540462, 0.050446400694058369, 0.052081254737870064,
        0.052717152614707274, 0.054321386788059089, 0.054760291530131122,
        0.050722218389354146, 0.053510033789323092, 0.052312173353992242,
        0.056532972139160864, -0.098675660805754906, 0.065090002163899041,
        0.072197992482618767, 0.070116257610668795, 0.07293026487577893,
        0.079138075170396144, 0.080085254645866577, 0.082462428201972526,
        0.074862712911492504, 0.076602032667577896, 0.073253967582367768,
        0.072612602724561548, 0.068289850763134319, 0.053001169489840433,
        0.048688611632845269, 0.047420712819321087, 0.053871124965724215,
        0.29860453961333144, 0.046440577617502088, 0.047390480506962096,
        0.23663706220684202, 0.063033896205029638, 0.062565631003872246,
        0.062129319901250035, 0.39018522279476053, 0.060795806958586329,
        0.066861864038584295, 0.06475867257522816, 0.042043746460846854,
        0.037295507225767466, 0.032318578985768842, 0.20808358660380255,
        0.1814676862373292, 0.14547694854906845, 0.011233900458313739,
        0.13771392047902645, 0.1741851025820175, 0.018633735013313668,
        0.0089540732394395306, 0.13805746210832409, 0.0011169575430811651,
        0.001524864314237151, -0.0013228013036072905, 0.10944767613033614,
        -0.013716141314357611, -0.016191797400929797, 0.18632208334831096,
        -0.0099301384023142703, -0.0066339539959048288, -0.015922494749739613,
        -0.015386863143862728, -0.018255817364298377, -0.02457585937570439,
        -0.037544169175406816, 0.092641503603871189, -0.050578510468998417,
        -0.1913423276765617, -0.058503879730583019, -0.069942562182732093,
        0.052966487639645093, -0.20925799493996514, -0.05907194476202493,
        -0.059678032999233133, -0.054148362410716411, 0.097164062815635444,
        -0.051655161960561191, -0.18489396869021424, -0.17427525054451939,
        -0.056425474843329604, -0.056663818098689478, -0.059643792821840069,
        -0.22882901000674208, -0.079610438347497148, -0.068764590086593635,
        -0.070674848832470993, -0.07126009614822626, -0.060799665212592391,
        -0.2005209631988655, -0.053105813657584044, -0.051278508338703344,
        -0.049203508146992814, -0.23641291581734306, -0.064120486122579201,
        -0.060096793931585707, -0.059235339910820473, -0.059696862447456478,
        -0.057764356853036242, -0.058923837812648872, -0.051944712529174375,
    ];

    #[test]
    fn controls_match_phd2() {
        let (gpg, controls) = run(300, true);
        for (k, (&r, &c)) in controls.iter().zip(CPP_CONTROLS.iter()).enumerate() {
            assert!(close(r, c), "frame {k}: rust {r}, C++ {c}");
        }
        // PHD2's final period and length scale (natural units).
        let h = gpg.gp_hyperparameters();
        assert!(close(h[PK_PERIOD_LENGTH], 224.94963215588226), "{h:?}");
        assert!(close(h[PK_LENGTH_SCALE], 9.9999999999998224), "{h:?}");
        assert_eq!(gpg.len(), 301);
    }

    #[test]
    fn learns_the_period() {
        // 1500 s of the same closed loop, no events.
        let (gpg, _) = run(750, false);
        let period = gpg.gp_hyperparameters()[PK_PERIOD_LENGTH];
        assert!((period - 230.5).abs() < 0.03 * 230.5, "period {period}");
        // PHD2's C++ ends on 230.43236227904831.
        assert!(close(period, 230.43236227904831), "period {period}");
    }

    #[test]
    fn variance_from_snr() {
        let sd = 2.1752 / (20.0 - 3.3) + 0.5;
        assert_eq!(calculate_variance(20.0), sd * sd);
        // Floored at an SNR of 3.4: σ = 21.752 + 0.5.
        assert_eq!(calculate_variance(1.0), calculate_variance(3.4));
        assert!((calculate_variance(3.4) - 22.252 * 22.252).abs() < 1e-9);
    }

    #[test]
    fn first_points_are_proportional_with_min_move() {
        let mut gpg = GaussianProcessGuider::new(phd2_defaults(), 5.0);
        gpg.reset(5.0);
        // The first result starts gear time: timestamp 0.
        assert_eq!(gpg.result(1.0, 20.0, 2.0, 10.0), 0.6);
        let first = DataPoint { timestamp: 0.0, measurement: 1.0, variance: calculate_variance(20.0), control: 0.0 };
        assert_eq!(gpg.data[0], first);
        assert_eq!(gpg.data[1].control, 0.6);
        // Below min-move, no move; the timestamp is the interval's midpoint.
        assert_eq!(gpg.result(0.1, 20.0, 2.0, 12.0), 0.0);
        assert_eq!(gpg.data[1].timestamp, 1.0);
        assert_eq!(gpg.len(), 3);
    }

    #[test]
    fn dead_reckoning_before_the_model_is_ready() {
        let mut gpg = GaussianProcessGuider::new(phd2_defaults(), 0.0);
        gpg.reset(0.0);
        gpg.result(1.0, 20.0, 2.0, 0.0);
        // Too few points: no move, a dark point with a huge variance.
        assert_eq!(gpg.deduce_result(2.0, 2.0), Ok(0.0));
        assert_eq!(gpg.data[1], DataPoint { timestamp: 1.0, measurement: 0.0, variance: 1e4, control: 0.6 });
        assert_eq!(gpg.data[2].control, 0.0);
        assert_eq!(gpg.len(), 3);
    }

    #[test]
    fn dither_runs_proportional_then_settles() {
        let mut gpg = GaussianProcessGuider::new(phd2_defaults(), 0.0);
        gpg.reset(0.0);
        gpg.result(1.0, 20.0, 2.0, 0.0);
        // 3 px at 2 px/s: gear time moves 1.5 s ahead.
        gpg.guiding_dithered(3.0, 2.0);
        assert_eq!((gpg.dither_offset, gpg.dither_steps, gpg.dithering_active), (1.5, 10, true));
        // Proportional, min-move ignored; a dark point is recorded.
        assert_eq!(gpg.result(0.1, 20.0, 2.0, 2.0), 0.6 * 0.1);
        assert_eq!(gpg.data[1], DataPoint { timestamp: 2.5, measurement: 0.0, variance: 1e4, control: 0.6 });
        assert_eq!(gpg.dither_steps, 9);
        // Settling ends the dither on the next frame, which still dithers.
        gpg.guiding_dither_settle_done(false);
        assert_eq!(gpg.dither_steps, 9);
        gpg.guiding_dither_settle_done(true);
        assert_eq!(gpg.result(0.1, 20.0, 2.0, 4.0), 0.6 * 0.1);
        assert!(!gpg.dithering_active);
        assert_eq!(gpg.result(0.1, 20.0, 2.0, 6.0), 0.0);
        // The offset stays until a reset.
        assert_eq!(gpg.data[3].timestamp, 6.0 - 1.0 + 1.5);
        gpg.reset(8.0);
        assert_eq!((gpg.dither_offset, gpg.len(), gpg.last_prediction_end), (0.0, 1, -1.0));
    }

    #[test]
    fn negative_dither_overrun_resets() {
        let mut p = phd2_defaults();
        // Let dead reckoning use the GP at once.
        p.min_periods_for_inference = 0.0;
        let mut gpg = GaussianProcessGuider::new(p, 0.0);
        gpg.reset(0.0);
        for k in 0..40 {
            gpg.result(0.5, 20.0, 2.0, 2.0 * k as f64);
        }
        // 30 s back in gear time. The shifted point is the newest, left out
        // of the fit: no overrun yet.
        gpg.guiding_dithered(-30.0, 1.0);
        assert!(gpg.deduce_result(2.0, 80.0).is_ok());
        // Next frame its timestamp (49 s) sizes the grid for 11 cells while
        // the older points span 16.
        assert_eq!(gpg.deduce_result(2.0, 82.0), Err(RegularizeOverrun { j: 11 }));
        // Inside result() the error resets the model; the move is proportional.
        assert_eq!(gpg.result(0.5, 20.0, 2.0, 82.0), 0.3);
        assert_eq!((gpg.len(), gpg.dither_offset, gpg.dithering_active), (1, 0.0, false));
    }

    #[test]
    fn hyperparameter_conversion() {
        let mut gpg = GaussianProcessGuider::new(phd2_defaults(), 0.0);
        let h = gpg.gp_hyperparameters();
        for (got, want) in h.iter().zip([700.0, 20.0, 10.0, 20.0, 25.0, 10.0, 200.0]) {
            assert!((got - want).abs() < 1e-9 * want, "{h:?}");
        }
        // The periodic length scale is stored as 4·sin(ℓπ/P) …
        let logs = gpg.gp.get_hyper_parameters();
        assert_eq!(logs[0], 0.0);
        assert!((logs[3].exp() - 4.0 * (10.0 * PI / 200.0).sin()).abs() < 1e-12);
        // … so one above P/2 reads back folded (150 → 50); length scales
        // are clamped to 1 and the rest floored at 1e-10.
        gpg.set_gp_hyperparameters(&[0.5, 0.0, 150.0, 20.0, -3.0, 10.0, 200.0]);
        let h = gpg.gp_hyperparameters();
        assert!((h[SE0K_LENGTH_SCALE] - 1.0).abs() < 1e-12);
        assert!((h[SE0K_SIGNAL_VARIANCE] - 1e-10).abs() < 1e-22);
        assert!((h[PK_LENGTH_SCALE] - 50.0).abs() < 1e-9);
        assert!((h[SE1K_LENGTH_SCALE] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn period_update_keeps_old_value_on_nan() {
        let mut gpg = GaussianProcessGuider::new(phd2_defaults(), 0.0);
        gpg.update_period_length(f64::NAN);
        assert!((gpg.gp_hyperparameters()[PK_PERIOD_LENGTH] - 200.0).abs() < 1e-9);
        gpg.update_period_length(300.0);
        assert!((gpg.gp_hyperparameters()[PK_PERIOD_LENGTH] - 201.0).abs() < 1e-9);
        gpg.set_learning_rate(0.0);
        gpg.update_period_length(300.0);
        assert!((gpg.gp_hyperparameters()[PK_PERIOD_LENGTH] - 201.0).abs() < 1e-9);
    }

    #[test]
    fn ring_buffer_drops_the_oldest() {
        let mut gpg = GaussianProcessGuider::new(phd2_defaults(), 0.0);
        gpg.data[0].control = 7.0;
        for _ in 0..CIRCULAR_BUFFER_SIZE {
            gpg.add_one_point();
        }
        assert_eq!(gpg.len(), CIRCULAR_BUFFER_SIZE);
        assert_eq!(gpg.data[0].control, 0.0);
    }
}
