//! Predictive PEC: PHD2's `GuideAlgorithmGaussianProcess`, the RA guide
//! algorithm around the [`GaussianProcessGuider`].
//!
//! It validates and persists the settings, feeds the guider the star's SNR
//! and the requested exposure as the time step, and decides on the
//! lifecycle hooks: the model survives a stop, a pause and a disabled
//! output, and when guiding restarts on the same pier side with the worm
//! moved less than `noreset_max_pct_period` % of a period, it is kept with
//! gear time shifted by the RA change, like a dither.
//!
//! Where PHD2 reads `steady_clock` this reads the engine clock: the time
//! [`crate::engine::algo::make`] passes when it builds the algorithm, and
//! the `now` of the [`AlgoCtx`] each frame and hook carries (the stop time
//! is the stop's).
//!
//! Ported from PHD2 src/guide_algorithm_gaussian_process.h, src/guide_algorithm_gaussian_process.cpp — BSD-3-Clause, see THIRD_PARTY.md

use super::guider::{
    GaussianProcessGuider, GuideParameters, NUM_PARAMETERS, PK_LENGTH_SCALE, PK_PERIOD_LENGTH, PK_SIGNAL_VARIANCE,
    SE0K_LENGTH_SCALE, SE0K_SIGNAL_VARIANCE, SE1K_LENGTH_SCALE, SE1K_SIGNAL_VARIANCE,
};
use crate::engine::algo::{AlgoCtx, AlgoKind, GuideAlgorithm, ParamLookup};
use crate::engine::calibration::norm;
use crate::engine::log::cfmt::fmt_f;
use crate::engine::traits::PierSide;

const DEFAULT_CONTROL_GAIN: f64 = 0.6;
const DEFAULT_PERIOD_LENGTHS_FOR_INFERENCE: f64 = 2.0;
const DEFAULT_MIN_MOVE: f64 = 0.2;
const DEFAULT_LENGTH_SCALE_SE0_KER: f64 = 700.0;
const DEFAULT_SIGNAL_VARIANCE_SE0_KER: f64 = 20.0;
const DEFAULT_LENGTH_SCALE_PER_KER: f64 = 10.0;
const DEFAULT_PERIOD_LENGTH_PER_KER: f64 = 200.0;
const DEFAULT_SIGNAL_VARIANCE_PER_KER: f64 = 20.0;
const DEFAULT_LENGTH_SCALE_SE1_KER: f64 = 25.0;
const DEFAULT_SIGNAL_VARIANCE_SE1_KER: f64 = 10.0;
const DEFAULT_PERIOD_LENGTHS_FOR_PERIOD_ESTIMATION: f64 = 2.0;
const DEFAULT_NUM_POINTS_FOR_APPROXIMATION: i32 = 100;
const DEFAULT_PREDICTION_GAIN: f64 = 0.5;
const DEFAULT_NORESET_MAX_PCT_PERIOD: f64 = 40.0;
const DEFAULT_COMPUTE_PERIOD: bool = true;

/// The hyperparameters' defaults, smallest valid values (a zero length
/// scale or period is unstable) and profile keys, in index order.
const HYPERPARAMETERS: [(f64, f64, &str); NUM_PARAMETERS] = [
    (DEFAULT_LENGTH_SCALE_SE0_KER, 1.0, "gp_length_scale_se0_kern"),
    (DEFAULT_SIGNAL_VARIANCE_SE0_KER, 0.0, "gp_sigvar_se0_kern"),
    (DEFAULT_LENGTH_SCALE_PER_KER, 1.0, "gp_length_scale_per_kern"),
    (DEFAULT_SIGNAL_VARIANCE_PER_KER, 0.0, "gp_sigvar_per_kern"),
    (DEFAULT_LENGTH_SCALE_SE1_KER, 1.0, "gp_length_scale_se1_kern"),
    (DEFAULT_SIGNAL_VARIANCE_SE1_KER, 0.0, "gp_sigvar_se1_kern"),
    (DEFAULT_PERIOD_LENGTH_PER_KER, 1.0, "gp_period_per_kern"),
];

/// `GuideAlgorithmGaussianProcess` ("Predictive PEC"). Offered on RA only,
/// but built on whatever axis the profile names, as in PHD2; it always
/// takes the mount's X rate for gear time.
#[derive(Debug, Clone)]
pub struct GaussianProcess {
    gpg: GaussianProcessGuider,
    /// Guiding output is off: frames are not fed to the model.
    block_updates: bool,
    /// RA (hours) when guiding last started; NaN when unknown.
    guiding_ra: f64,
    guiding_pier_side: PierSide,
    /// The clock reading guiding last stopped at (0 before the first stop,
    /// as PHD2's default-constructed time point).
    guiding_stopped_time: f64,
    /// The latest clock reading: construction, or the last frame, start,
    /// stop or dither. Only a direct [`GuideAlgorithm::reset`] uses it; PHD2
    /// resets the model only when built and at a start, which carry a time.
    now: f64,
    /// `noreset_max_pct_period`, which PHD2 reads at each start.
    retain_model_pct: f64,
    /// The hyperparameters as last written to the profile (natural units):
    /// the setter's values, then the learned period at each stop.
    stored_hyperparameters: [f64; NUM_PARAMETERS],
}

impl GaussianProcess {
    /// Read the `gp_*` keys and `noreset_max_pct_period` through `params`,
    /// each validated by its setter, then reset at clock reading `now`: gear
    /// time counts from there, and keeps doing so when the star is lost
    /// before the first measurement (which otherwise restarts it). Guiding
    /// output is taken as enabled: when it is off, call
    /// [`GuideAlgorithm::guiding_disabled`] next (PHD2 reads the mount's flag
    /// here).
    ///
    /// The approximation size is read from `gp_points_for_approximation`,
    /// falling back to `gp_points_approximation`: PHD2 reads the first and
    /// writes the second, so an edited PHD2 profile only has the latter.
    pub fn new(params: ParamLookup, now: f64) -> Self {
        let parameters = GuideParameters {
            control_gain: DEFAULT_CONTROL_GAIN,
            min_move: DEFAULT_MIN_MOVE,
            prediction_gain: DEFAULT_PREDICTION_GAIN,
            min_periods_for_inference: DEFAULT_PERIOD_LENGTHS_FOR_INFERENCE,
            min_periods_for_period_estimation: DEFAULT_PERIOD_LENGTHS_FOR_PERIOD_ESTIMATION,
            points_for_approximation: DEFAULT_NUM_POINTS_FOR_APPROXIMATION,
            compute_period: DEFAULT_COMPUTE_PERIOD,
        };
        let mut a = GaussianProcess {
            gpg: GaussianProcessGuider::new(parameters, &HYPERPARAMETERS.map(|(default, _, _)| default), now),
            block_updates: false,
            guiding_ra: f64::NAN,
            guiding_pier_side: PierSide::Unknown,
            guiding_stopped_time: 0.0,
            now,
            retain_model_pct: params("noreset_max_pct_period").unwrap_or(DEFAULT_NORESET_MAX_PCT_PERIOD),
            stored_hyperparameters: HYPERPARAMETERS.map(|(default, _, _)| default),
        };
        a.set_control_gain(params("gp_control_gain").unwrap_or(DEFAULT_CONTROL_GAIN));
        a.set_min_move(params("gp_min_move").unwrap_or(DEFAULT_MIN_MOVE));
        a.set_period_lengths_inference(
            params("gp_period_lengths_inference").unwrap_or(DEFAULT_PERIOD_LENGTHS_FOR_INFERENCE),
        );
        a.set_period_lengths_period_estimation(
            params("gp_period_lengths_period_estimation").unwrap_or(DEFAULT_PERIOD_LENGTHS_FOR_PERIOD_ESTIMATION),
        );
        let points = params("gp_points_for_approximation").or_else(|| params("gp_points_approximation"));
        a.set_num_points_for_approximation(points.map_or(DEFAULT_NUM_POINTS_FOR_APPROXIMATION, |v| v as i32));
        a.set_prediction_gain(params("gp_prediction_gain").unwrap_or(DEFAULT_PREDICTION_GAIN));
        let hyperparameters = HYPERPARAMETERS.map(|(default, _, key)| params(key).unwrap_or(default));
        a.set_gp_hyperparameters(&hyperparameters);
        a.set_compute_period(params("gp_compute_period").map_or(DEFAULT_COMPUTE_PERIOD, |v| v != 0.0));
        a.reset();
        a
    }

    pub fn control_gain(&self) -> f64 {
        self.gpg.control_gain()
    }

    /// The reactive weight; negative is an error and 0.6 is used.
    pub fn set_control_gain(&mut self, control_gain: f64) -> bool {
        let err = control_gain < 0.0;
        self.gpg.set_control_gain(if err { DEFAULT_CONTROL_GAIN } else { control_gain });
        err
    }

    pub fn period_lengths_inference(&self) -> f64 {
        self.gpg.period_lengths_inference()
    }

    /// Worm cycles before the prediction is fully used; negative is an
    /// error and 2 is used.
    pub fn set_period_lengths_inference(&mut self, num_periods: f64) -> bool {
        let err = num_periods < 0.0;
        self.gpg.set_period_lengths_inference(if err { DEFAULT_PERIOD_LENGTHS_FOR_INFERENCE } else { num_periods });
        err
    }

    pub fn period_lengths_period_estimation(&self) -> f64 {
        self.gpg.period_lengths_period_estimation()
    }

    /// Worm cycles before the period is estimated; negative is an error and
    /// 2 is used.
    pub fn set_period_lengths_period_estimation(&mut self, num_periods: f64) -> bool {
        let err = num_periods < 0.0;
        self.gpg.set_period_lengths_period_estimation(if err {
            DEFAULT_PERIOD_LENGTHS_FOR_PERIOD_ESTIMATION
        } else {
            num_periods
        });
        err
    }

    pub fn num_points_for_approximation(&self) -> i32 {
        self.gpg.num_points_for_approximation()
    }

    /// Points the GP keeps; negative is an error and 100 is used.
    pub fn set_num_points_for_approximation(&mut self, num_points: i32) -> bool {
        let err = num_points < 0;
        self.gpg.set_num_points_for_approximation(if err { DEFAULT_NUM_POINTS_FOR_APPROXIMATION } else { num_points });
        err
    }

    pub fn prediction_gain(&self) -> f64 {
        self.gpg.prediction_gain()
    }

    /// The predictive weight; outside 0..=1 is an error and 0.5 is used.
    pub fn set_prediction_gain(&mut self, prediction_gain: f64) -> bool {
        let err = prediction_gain < 0.0 || prediction_gain > 1.0;
        self.gpg.set_prediction_gain(if err { DEFAULT_PREDICTION_GAIN } else { prediction_gain });
        err
    }

    pub fn compute_period(&self) -> bool {
        self.gpg.compute_period()
    }

    /// Auto-adjust the period length.
    pub fn set_compute_period(&mut self, active: bool) {
        self.gpg.set_compute_period(active);
    }

    /// The model's hyperparameters, natural units (the period as learned).
    pub fn gp_hyperparameters(&self) -> [f64; NUM_PARAMETERS] {
        self.gpg.gp_hyperparameters()
    }

    /// Set the hyperparameters (natural units, indexed by
    /// `guider::SE0K_LENGTH_SCALE` …). A length scale or period below 1 or a
    /// negative signal variance is an error and takes its default; `true`
    /// if any was refused.
    pub fn set_gp_hyperparameters(&mut self, hyperparameters: &[f64; NUM_PARAMETERS]) -> bool {
        let mut h = *hyperparameters;
        let mut err = false;
        for (v, (default, min, _)) in h.iter_mut().zip(HYPERPARAMETERS) {
            if *v < min {
                err = true;
                *v = default;
            }
        }
        self.stored_hyperparameters = h;
        self.gpg.set_gp_hyperparameters(&h);
        err
    }

    /// `noreset_max_pct_period`: how much of a worm period may pass between
    /// a stop and the next start for the model to be kept (PHD2's dialog
    /// offers 0–80; nothing is validated).
    pub fn retain_model_pct(&self) -> f64 {
        self.retain_model_pct
    }

    pub fn set_retain_model_pct(&mut self, pct: f64) {
        self.retain_model_pct = pct;
    }
}

/// `GetRAGuideRate` (guide_algorithm_gaussian_process.cpp:1116): px per
/// second of gear time. `ctx.ra_guide_speed` is the calibration's RA guide
/// speed in degrees per second (−1 when unknown, then sidereal is assumed),
/// normalised to sidereal's 15″/s.
fn ra_guide_rate(ctx: &AlgoCtx) -> f64 {
    let mut guide_speed = 1.0;
    if ctx.ra_guide_speed != -1.0 {
        guide_speed = 3600.0 * ctx.ra_guide_speed / 15.0;
    }
    1000.0 * ctx.x_rate_px_ms / guide_speed
}

impl GuideAlgorithm for GaussianProcess {
    fn kind(&self) -> AlgoKind {
        AlgoKind::GaussianProcess
    }

    fn reset(&mut self) {
        self.gpg.reset(self.now);
    }

    /// 0 while guiding output is disabled. The guider's time step is the
    /// requested exposure, not the measured frame interval (parity).
    fn result(&mut self, input: f64, ctx: &AlgoCtx) -> f64 {
        self.now = ctx.now;
        if self.block_updates {
            return 0.0;
        }
        self.gpg.result(input, ctx.snr, ctx.exposure_s, ctx.now)
    }

    /// Not blocked by a disabled output, as in PHD2. PHD2 does not catch
    /// the guider's grid overrun here (it ends the process); the model is
    /// reset and no move made instead.
    fn deduce_result(&mut self, ctx: &AlgoCtx) -> f64 {
        self.now = ctx.now;
        match self.gpg.deduce_result(ctx.exposure_s, ctx.now) {
            Ok(control) => control,
            Err(_) => {
                self.gpg.reset(ctx.now);
                0.0
            }
        }
    }

    fn min_move(&self) -> f64 {
        self.gpg.min_move()
    }

    /// A negative min-move is an error and the default (0.2) is taken.
    fn set_min_move(&mut self, min_move: f64) -> bool {
        let err = min_move < 0.0;
        self.gpg.set_min_move(if err { DEFAULT_MIN_MOVE } else { min_move });
        err
    }

    fn param_names(&self) -> &'static [&'static str] {
        &["minMove", "predictiveWeight", "reactiveWeight", "periodLength"]
    }

    fn get_param(&self, name: &str) -> Option<f64> {
        match name {
            "minMove" => Some(self.min_move()),
            "predictiveWeight" => Some(self.prediction_gain()),
            "reactiveWeight" => Some(self.control_gain()),
            "periodLength" => Some(self.gp_hyperparameters()[PK_PERIOD_LENGTH]),
            _ => None,
        }
    }

    fn set_param(&mut self, name: &str, val: f64) -> bool {
        let err = match name {
            "minMove" => self.set_min_move(val),
            "predictiveWeight" => self.set_prediction_gain(val),
            "reactiveWeight" => self.set_control_gain(val),
            "periodLength" => {
                // Parity: PHD2 passes a zeroed vector with only the period
                // set, so the length scales fall back to their defaults
                // (reported as an error: the call answers false although the
                // period is applied) and the signal variances become 0.
                let mut h = [0.0; NUM_PARAMETERS];
                h[PK_PERIOD_LENGTH] = val;
                self.set_gp_hyperparameters(&h)
            }
            _ => true,
        };
        !err
    }

    /// The keys PHD2's setters write, with the points-for-approximation fix
    /// (written under `gp_points_for_approximation`, the key it reads), and
    /// `noreset_max_pct_period`. The hyperparameters are the profile's: the
    /// period is the learned one only after a stop, as in PHD2.
    fn params(&self) -> Vec<(&'static str, f64)> {
        let mut p = vec![
            ("gp_control_gain", self.control_gain()),
            ("gp_min_move", self.min_move()),
            ("gp_period_lengths_inference", self.period_lengths_inference()),
            ("gp_period_lengths_period_estimation", self.period_lengths_period_estimation()),
            ("gp_points_for_approximation", self.num_points_for_approximation() as f64),
            ("gp_prediction_gain", self.prediction_gain()),
        ];
        for ((_, _, key), v) in HYPERPARAMETERS.into_iter().zip(self.stored_hyperparameters) {
            p.push((key, v));
        }
        p.push(("gp_compute_period", if self.compute_period() { 1.0 } else { 0.0 }));
        p.push(("noreset_max_pct_period", self.retain_model_pct));
        p
    }

    fn settings_summary(&self) -> String {
        let h = self.gp_hyperparameters();
        format!(
            "Control gain = {}\n\
             Prediction gain = {}\n\
             Minimum move = {}\n\
             Hyperparameters\n\
             \tLength scale long range SE kernel = {}\n\
             \tSignal variance long range SE kernel = {}\n\
             \tLength scale periodic kernel = {}\n\
             \tSignal variance periodic kernel = {}\n\
             \tLength scale short range SE kernel = {}\n\
             \tSignal variance short range SE kernel = {}\n\
             \tPeriod length periodic kernel = {}\n\
             \tFFT called after = {} worm cycles\n\
             \tAuto-adjust period length = {}\n",
            fmt_f(self.control_gain(), 3),
            fmt_f(self.prediction_gain(), 3),
            fmt_f(self.min_move(), 3),
            fmt_f(h[SE0K_LENGTH_SCALE], 3),
            fmt_f(h[SE0K_SIGNAL_VARIANCE], 3),
            fmt_f(h[PK_LENGTH_SCALE], 3),
            fmt_f(h[PK_SIGNAL_VARIANCE], 3),
            fmt_f(h[SE1K_LENGTH_SCALE], 3),
            fmt_f(h[SE1K_SIGNAL_VARIANCE], 3),
            fmt_f(h[PK_PERIOD_LENGTH], 3),
            fmt_f(self.period_lengths_period_estimation(), 3),
            if self.compute_period() { "On" } else { "Off" },
        )
    }

    /// Keep the model when guiding resumes on the same pier side and the
    /// worm moved by less than `noreset_max_pct_period` % of a period (and
    /// not backwards, which the guider cannot handle); gear time is then
    /// shifted by the RA change. Otherwise reset.
    fn guiding_started(&mut self, ctx: &AlgoCtx) {
        self.now = ctx.now;
        let mut need_reset = true;
        let mut ra_offset = 0.0;

        let prev_ra = self.guiding_ra;
        self.guiding_ra = ctx.ra_h.unwrap_or(f64::NAN);
        let prev_side = self.guiding_pier_side;
        self.guiding_pier_side = ctx.pier;

        if !self.guiding_ra.is_nan()
            && !prev_ra.is_nan()
            && prev_side != PierSide::Unknown
            && prev_side == self.guiding_pier_side
        {
            const SECONDS_PER_HOUR: f64 = 60.0 * 60.0;
            const SIDEREAL_SECONDS_PER_SEC: f64 = 0.9973;
            // An RA increase is the worm moving backwards in gear time.
            ra_offset = norm(prev_ra - self.guiding_ra, -12.0, 12.0);
            ra_offset *= SECONDS_PER_HOUR / SIDEREAL_SECONDS_PER_SEC;

            let elapsed_time = ctx.now - self.guiding_stopped_time;
            let worm_offset = elapsed_time + ra_offset;
            let period_length = self.gpg.gp_hyperparameters()[PK_PERIOD_LENGTH];
            if worm_offset.abs() < self.retain_model_pct / 100.0 * period_length {
                need_reset = false;
            }
            if !need_reset && worm_offset < 0.0 {
                need_reset = true;
            }
        }

        if need_reset {
            self.gpg.reset(ctx.now);
        } else {
            self.gpg.guiding_dithered(ra_offset, 1.0);
        }
    }

    /// No reset: the learned period goes to the profile and the stop time
    /// is noted for the next start.
    fn guiding_stopped(&mut self, ctx: &AlgoCtx) {
        self.now = ctx.now;
        self.stored_hyperparameters[PK_PERIOD_LENGTH] = self.gpg.gp_hyperparameters()[PK_PERIOD_LENGTH];
        self.guiding_stopped_time = ctx.now;
    }

    fn guiding_paused(&mut self, _ctx: &AlgoCtx) {}

    fn guiding_resumed(&mut self, _ctx: &AlgoCtx) {}

    /// Gear time moves by the dither over the RA guide rate; no reset.
    fn guiding_dithered(&mut self, amt: f64, ctx: &AlgoCtx) {
        self.now = ctx.now;
        self.gpg.guiding_dithered(amt, ra_guide_rate(ctx));
    }

    fn dither_settle_done(&mut self, success: bool, _ctx: &AlgoCtx) {
        self.gpg.guiding_dither_settle_done(success);
    }

    // `direct_move_applied`: PHD2 forwards it to the guider, whose handler
    // is a no-op; the trait's default no-op stands.

    /// Stop feeding the model, keeping it (as a stop, at this time).
    fn guiding_disabled(&mut self, ctx: &AlgoCtx) {
        self.block_updates = true;
        self.guiding_stopped(ctx);
    }

    /// Resume feeding the model, without a reset.
    fn guiding_enabled(&mut self, _ctx: &AlgoCtx) {
        self.block_updates = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_params(_: &str) -> Option<f64> {
        None
    }

    fn ctx(now: f64) -> AlgoCtx {
        AlgoCtx { snr: 20.0, exposure_s: 2.0, now, ra_guide_speed: -1.0, ..AlgoCtx::default() }
    }

    fn get(params: &[(&'static str, f64)], key: &str) -> f64 {
        params.iter().find(|(k, _)| *k == key).unwrap().1
    }

    #[test]
    fn defaults_and_summary() {
        let a = GaussianProcess::new(&no_params, 0.0);
        assert_eq!(a.kind(), AlgoKind::GaussianProcess);
        assert_eq!(a.class_name(), "Predictive PEC");
        assert_eq!(a.param_names(), ["minMove", "predictiveWeight", "reactiveWeight", "periodLength"]);
        assert_eq!(a.get_param("minMove"), Some(0.2));
        assert_eq!(a.get_param("predictiveWeight"), Some(0.5));
        assert_eq!(a.get_param("reactiveWeight"), Some(0.6));
        assert!((a.get_param("periodLength").unwrap() - 200.0).abs() < 1e-9);
        assert_eq!(a.get_param("aggression"), None);
        assert_eq!((a.num_points_for_approximation(), a.retain_model_pct(), a.compute_period()), (100, 40.0, true));
        assert_eq!(
            a.settings_summary(),
            "Control gain = 0.600\n\
             Prediction gain = 0.500\n\
             Minimum move = 0.200\n\
             Hyperparameters\n\
             \tLength scale long range SE kernel = 700.000\n\
             \tSignal variance long range SE kernel = 20.000\n\
             \tLength scale periodic kernel = 10.000\n\
             \tSignal variance periodic kernel = 20.000\n\
             \tLength scale short range SE kernel = 25.000\n\
             \tSignal variance short range SE kernel = 10.000\n\
             \tPeriod length periodic kernel = 200.000\n\
             \tFFT called after = 2.000 worm cycles\n\
             \tAuto-adjust period length = On\n"
        );
    }

    #[test]
    fn reads_and_writes_the_profile() {
        let profile = |k: &str| match k {
            "gp_control_gain" => Some(0.8),
            "gp_min_move" => Some(0.15),
            "gp_points_approximation" => Some(250.0),
            "gp_prediction_gain" => Some(0.7),
            "gp_period_per_kern" => Some(480.0),
            "gp_compute_period" => Some(0.0),
            "noreset_max_pct_period" => Some(25.0),
            _ => None,
        };
        let a = GaussianProcess::new(&profile, 0.0);
        // PHD2's written key is read when the read key is absent …
        assert_eq!(a.num_points_for_approximation(), 250);
        let p = a.params();
        // … and the read key is the one written back.
        assert_eq!(get(&p, "gp_points_for_approximation"), 250.0);
        assert!(p.iter().all(|(k, _)| *k != "gp_points_approximation"));
        assert_eq!(p.len(), 15);
        assert_eq!(get(&p, "gp_control_gain"), 0.8);
        assert_eq!(get(&p, "gp_min_move"), 0.15);
        assert_eq!(get(&p, "gp_prediction_gain"), 0.7);
        assert_eq!(get(&p, "gp_period_per_kern"), 480.0);
        assert_eq!(get(&p, "gp_length_scale_se0_kern"), 700.0);
        assert_eq!(get(&p, "gp_compute_period"), 0.0);
        assert_eq!(get(&p, "noreset_max_pct_period"), 25.0);
        assert!(a.settings_summary().ends_with("\tAuto-adjust period length = Off\n"));
        // … and the read key wins when both are there.
        let both = |k: &str| match k {
            "gp_points_for_approximation" => Some(60.0),
            "gp_points_approximation" => Some(250.0),
            _ => None,
        };
        assert_eq!(GaussianProcess::new(&both, 0.0).num_points_for_approximation(), 60);
    }

    #[test]
    fn invalid_values_take_the_defaults() {
        let profile = |k: &str| match k {
            "gp_control_gain" => Some(-1.0),
            "gp_points_for_approximation" => Some(-5.0),
            "gp_length_scale_se0_kern" => Some(0.5),
            "gp_sigvar_per_kern" => Some(-2.0),
            _ => None,
        };
        let mut a = GaussianProcess::new(&profile, 0.0);
        assert_eq!(a.control_gain(), 0.6);
        assert_eq!(a.num_points_for_approximation(), 100);
        let p = a.params();
        assert_eq!(get(&p, "gp_length_scale_se0_kern"), 700.0);
        assert_eq!(get(&p, "gp_sigvar_per_kern"), 20.0);
        assert!(!a.set_param("predictiveWeight", 1.5));
        assert_eq!(a.prediction_gain(), 0.5);
        assert!(!a.set_param("minMove", -0.1));
        assert_eq!(a.min_move(), 0.2);
        assert!(a.set_param("reactiveWeight", 0.9));
        assert_eq!(a.control_gain(), 0.9);
        assert!(!a.set_param("bogus", 1.0));
        assert!(a.set_period_lengths_inference(-1.0));
        assert_eq!(a.period_lengths_inference(), 2.0);
    }

    #[test]
    fn period_length_param_quirk() {
        let mut a = GaussianProcess::new(&no_params, 0.0);
        // Applied, yet reported as refused; the variances are zeroed.
        assert!(!a.set_param("periodLength", 300.0));
        assert!((a.get_param("periodLength").unwrap() - 300.0).abs() < 1e-9);
        let p = a.params();
        assert_eq!(get(&p, "gp_period_per_kern"), 300.0);
        assert_eq!(get(&p, "gp_length_scale_per_kern"), 10.0);
        assert_eq!(get(&p, "gp_sigvar_se0_kern"), 0.0);
        assert!(a.settings_summary().contains("\tSignal variance periodic kernel = 0.000\n"));
    }

    #[test]
    fn blocked_while_disabled() {
        let mut a = GaussianProcess::new(&no_params, 0.0);
        a.guiding_started(&ctx(0.0));
        assert_eq!(a.result(1.0, &ctx(0.0)), 0.6);
        a.guiding_disabled(&ctx(1.0));
        assert_eq!(a.result(1.0, &ctx(2.0)), 0.0);
        // Dead reckoning still runs (no model yet: no move).
        assert_eq!(a.deduce_result(&ctx(4.0)), 0.0);
        a.guiding_enabled(&ctx(5.0));
        // Not reset: two points (measured, dark) plus the open one.
        assert_eq!(a.gpg.len(), 3);
        assert_eq!(a.result(1.0, &ctx(6.0)), 0.6);
    }

    #[test]
    fn hooks_keep_the_model() {
        let mut a = GaussianProcess::new(&no_params, 0.0);
        a.guiding_started(&ctx(0.0));
        a.result(1.0, &ctx(0.0));
        a.guiding_paused(&ctx(1.0));
        a.guiding_resumed(&ctx(2.0));
        a.guiding_stopped(&ctx(3.0));
        a.direct_move_applied(1.0, &ctx(4.0));
        assert_eq!(a.gpg.len(), 2);
        a.reset();
        assert_eq!(a.gpg.len(), 1);
    }

    #[test]
    fn dither_uses_the_ra_guide_rate() {
        // 0.005 px/ms at 0.5× sidereal (7.5″/s in degrees per second):
        // 5 px/s at sidereal, 10 px/s of gear time.
        let c = AlgoCtx { x_rate_px_ms: 0.005, ra_guide_speed: 0.5 * 15.0 / 3600.0, ..ctx(10.0) };
        assert!((ra_guide_rate(&c) - 10.0).abs() < 1e-12);
        // Unknown guide speed: sidereal.
        assert_eq!(ra_guide_rate(&AlgoCtx { ra_guide_speed: -1.0, ..c }), 5.0);

        let mut a = GaussianProcess::new(&no_params, 0.0);
        a.guiding_started(&ctx(0.0));
        a.result(1.0, &ctx(0.0));
        a.guiding_dithered(3.0, &AlgoCtx { ra_guide_speed: -1.0, ..c });
        // Proportional, min-move ignored, while dithering.
        assert_eq!(a.result(0.1, &ctx(2.0)), 0.6 * 0.1);
        assert_eq!(a.gpg.len(), 3);
        a.dither_settle_done(true, &ctx(3.0));
        assert_eq!(a.result(0.1, &ctx(4.0)), 0.6 * 0.1);
        assert_eq!(a.result(0.1, &ctx(6.0)), 0.0);
    }

    fn pointing(now: f64, ra_h: f64, pier: PierSide) -> AlgoCtx {
        AlgoCtx { ra_h: Some(ra_h), pier, ..ctx(now) }
    }

    #[test]
    fn restart_retains_or_resets_the_model() {
        let mut a = GaussianProcess::new(&no_params, 0.0);
        // First start: no previous RA, reset.
        a.guiding_started(&pointing(0.0, 5.0, PierSide::West));
        a.result(1.0, &ctx(0.0));
        a.result(1.0, &ctx(2.0));
        a.guiding_stopped(&ctx(2.0));
        // 30 s later, RA unchanged: 30 s of a 200 s period (< 40 %), kept
        // with no gear time shift.
        a.guiding_started(&pointing(32.0, 5.0, PierSide::West));
        assert_eq!(a.gpg.len(), 3);
        // A dither of 0 s: the next frame runs the dither branch.
        assert_eq!(a.result(0.1, &ctx(34.0)), 0.6 * 0.1);
        a.guiding_stopped(&ctx(34.0));

        // RA moved 20 s of time east (−20 s of gear time) after 30 s: a
        // 10 s worm offset, kept.
        let ra = 5.0 + 20.0 * 0.9973 / 3600.0;
        a.guiding_started(&pointing(64.0, ra, PierSide::West));
        assert_eq!(a.gpg.len(), 4);
        a.guiding_stopped(&ctx(64.0));

        // Backwards (RA moved more than the time elapsed): reset.
        a.guiding_started(&pointing(66.0, ra + 60.0 / 3600.0, PierSide::West));
        assert_eq!(a.gpg.len(), 1);
        a.result(1.0, &ctx(68.0));
        a.guiding_stopped(&ctx(68.0));

        // Too long a stop (90 s > 80 s), another pier side, an unknown RA:
        // reset each time.
        let ra = ra + 60.0 / 3600.0;
        a.guiding_started(&pointing(158.0, ra, PierSide::West));
        assert_eq!(a.gpg.len(), 1);
        a.result(1.0, &ctx(160.0));
        a.guiding_stopped(&ctx(160.0));
        a.guiding_started(&pointing(162.0, ra, PierSide::East));
        assert_eq!(a.gpg.len(), 1);
        a.result(1.0, &ctx(164.0));
        a.guiding_stopped(&ctx(164.0));
        a.guiding_started(&AlgoCtx { pier: PierSide::East, ..ctx(166.0) });
        assert_eq!(a.gpg.len(), 1);
    }

    #[test]
    fn the_stop_time_is_the_stops() {
        // Last frame at 2 s, stop at 50 s, restart at 120 s: stopped 70 s,
        // under the 80 s limit, so the model is kept. Timed from the last
        // frame it would be 118 s and a reset.
        let mut a = GaussianProcess::new(&no_params, 0.0);
        a.guiding_started(&pointing(0.0, 5.0, PierSide::West));
        a.result(1.0, &ctx(0.0));
        a.result(1.0, &ctx(2.0));
        a.guiding_stopped(&ctx(50.0));
        a.guiding_started(&pointing(120.0, 5.0, PierSide::West));
        assert_eq!(a.gpg.len(), 3);

        // Disabling the output stops at its own time too: 79 s before the
        // restart, kept (81 s from the last frame would reset).
        a.result(1.0, &ctx(122.0));
        a.guiding_disabled(&ctx(124.0));
        a.guiding_enabled(&ctx(150.0));
        a.guiding_started(&pointing(203.0, 5.0, PierSide::West));
        assert_eq!(a.gpg.len(), 4);
    }

    /// PHD2's C++ for [`gear_time_starts_at_construction`]: its
    /// gaussian_process_guider.cpp (with the other GP sources, Eigen 3.4.1)
    /// on a settable clock, built and reset at 100 s, `deduceResult(2)` at
    /// 104 s, then `result(sin(t/10), 20, 2)` every 2 s from 106 s.
    #[rustfmt::skip]
    const CPP_CTOR_CONTROLS: [f64; 31] = [
        0.0,
        -0.55366525296768387, -0.58856173803989487, -0.59999412393042206,
        -0.58750663749079046, -0.5515971153988054, -0.49369715698122529,
        -0.41611505086627343, -0.32194375080026094, -0.21493756934209721,
        -0.023289211552096692, -0.02531780475943891, 0.12979745115360886,
        0.23480461221989934, 0.33873027415654561, 0.432600035267658,
        0.50358232924542279, 0.56518367586440021, 0.59420851551227993,
        0.59992225927561971, 0.5948309244708766, 0.55407393930975091,
        0.4873041165320498, 0.40501144911546888, 0.30676121920701049,
        0.21722712343969708, 0.026794446224196109, -0.012566792184050667,
        -0.17631441903011483, -0.28567598089187485, -0.38840641839814932,
    ];

    #[test]
    fn gear_time_starts_at_construction() {
        // The star is lost before the first measurement, so gear time runs
        // from the construction time, as in PHD2.
        let close = |r: f64, c: f64| (r - c).abs() <= 1e-6 * c.abs() || (r - c).abs() <= 1e-9;
        let mut a = GaussianProcess::new(&no_params, 100.0);
        let mut b = GaussianProcess::new(&no_params, 0.0);
        assert_eq!(a.deduce_result(&ctx(104.0)), CPP_CTOR_CONTROLS[0]);
        b.deduce_result(&ctx(104.0));
        let mut differs = false;
        for k in 0..30 {
            let now = 106.0 + 2.0 * k as f64;
            let input = (now / 10.0).sin();
            let u = a.result(input, &ctx(now));
            let c = CPP_CTOR_CONTROLS[k + 1];
            assert!(close(u, c), "frame {k}: rust {u}, C++ {c}");
            differs |= !close(b.result(input, &ctx(now)), c);
        }
        // Built at 0 s instead, the blend into the prediction moves.
        assert!(differs);
    }

    #[test]
    fn stop_persists_the_learned_period() {
        let mut a = GaussianProcess::new(&no_params, 0.0);
        a.gpg.set_gp_hyperparameters(&[700.0, 20.0, 10.0, 20.0, 25.0, 10.0, 230.0]);
        // Learned but not yet stored …
        assert_eq!(get(&a.params(), "gp_period_per_kern"), 200.0);
        a.guiding_stopped(&ctx(0.0));
        // … until guiding stops (or is disabled).
        assert!((get(&a.params(), "gp_period_per_kern") - 230.0).abs() < 1e-9);
    }
}
