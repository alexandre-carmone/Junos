//! The Guiding Assistant: with the guide output off, it measures how the
//! star moves on its own — the high-frequency jitter (the seeing), the RA
//! drift and peak-to-peak (periodic error), the Dec drift (polar alignment)
//! — then, if asked, the Dec backlash with [`BacklashTool`], and turns the
//! numbers into recommendations: exposure range, binning, recalibration, a
//! brighter star, polar alignment, focus, the min-moves, backlash
//! compensation and Lowpass2 for Dec. Four of them come with an Apply.
//!
//! The window's flow is kept without the window: Start, one sample per
//! graphed guide step, Stop with PHD2's 2-minute minimum (its countdown
//! dialog is [`GuidingAssistant::tick`] and [`GuidingAssistant::end_wait`]),
//! the backlash test after the sampling, the recommendations, the guide-log
//! `INFO: GA Result - …` lines and the last three runs kept in the profile
//! under `/GA/<start time>`. What PHD2 changes elsewhere — the guide outputs,
//! multi-star mode, the variable delay, subframes, the backlash pulses —
//! leaves as [`GaEvent`]s, in PHD2's order, for the engine to carry out.
//!
//! Ported from PHD2 `src/guiding_assistant.h`, `src/guiding_assistant.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use chrono::{DateTime, Local};
use serde_json::Value;

use crate::engine::algo::{AlgoCtx, AlgoKind, Axis, smart_default_min_move_for};
use crate::engine::backlash::{BacklashComp, BacklashTool, BltEnv, BltState, BltStep, MeasurementResult};
use crate::engine::calibration::{
    Calibration, CalibrationDetails, CalibrationIssue, ProfileAccess, camera_to_mount_logged,
};
use crate::engine::log::cfmt::{fmt_f, fmt_g};
use crate::engine::mount::{GuideStepInfo, Mount, MountEvent};
use crate::engine::scope::{DecGuideMode, Scope};
use crate::engine::stats::{AxisStats, DescriptiveStats, HighPassFilter, LowPassFilter};
use crate::engine::tools::polar_drift::GuideOutputs;
use crate::engine::traits::{MoveOpts, Point};

/// A backlash longer than this (ms) is beyond compensation: guide Dec one
/// way instead.
pub const MAX_BACKLASH_COMP: i32 = 3000;
/// The minimum sampling, seconds: Stop before it waits for the rest.
pub const GA_MIN_SAMPLING_PERIOD: i64 = 120;
/// Runs kept in the profile.
pub const MAX_GA_HISTORY: usize = 3;
/// The profile group of the saved runs, one sub-group per start time.
pub const HISTORY_GROUP: &str = "/GA";

/// The seeing estimate's window over a long run, seconds, and its step.
const MEASUREMENT_WINDOW_SIZE: f64 = 120.0;
const WINDOW_ADJUSTMENT: f64 = MEASUREMENT_WINDOW_SIZE / 2.0;

/// The instructions line (`FillInstructions`).
pub const INSTR_NO_STAR: &str = "Choose a non-saturated star with a good SNR (>= 10) and begin guiding";
pub const INSTR_START_READY: &str =
    "Click Start to begin measurements.  Guiding will be disabled during this time so the star will move around.";
pub const INSTR_MEASURING: &str = "Guiding output is disabled and star movement is being measured.  Click Stop after \
     2 minutes (longer if you're measuring RA tracking accuracy of the mount).";
pub const INSTR_STOPPED: &str = "Guiding has been resumed. Look at the recommendations and make any desired \
     changes.  Click Start to repeat the measurements, or close the window to continue guiding.";
pub const INSTR_BACKLASH: &str = "Measuring backlash... ";

/// The countdown dialog's text (`SampleWait`), with and without the
/// backlash test to follow (PHD2's "meaure" kept).
pub const WAIT_MSG_BLT: &str = "Additional data sampling is being done to better meaure Dec drift. Backlash \
     testing \nwill start automatically when sampling is completed.";
pub const WAIT_MSG: &str =
    "Additional sampling is being done for accurate measurements.  Results will be shown when sampling is complete.";

/// `std::min` / `std::max` and `wxMin` / `wxMax`: which argument comes back
/// when a comparison fails matters with a NaN (a sigma over one sample).
fn std_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}
fn std_max(a: f64, b: f64) -> f64 {
    if a < b { b } else { a }
}
fn wx_min(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}
fn wx_max(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

/// `MyFrame`'s variable delay settings, which the assistant turns off while
/// open and puts back after a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VarDelayCfg {
    pub enabled: bool,
    pub short_ms: i32,
    pub long_ms: i32,
}

/// The dialog's state (`DialogState`), which sets the instructions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogState {
    NoStar,
    StartReady,
    Measuring,
    Stopped,
}

/// A recommendation, by the tag PHD2 saves before its text
/// (`/GA/<time>/recommendations`, one `Tag:text` per line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecKind {
    Exp,
    Bin,
    Cal,
    Star,
    Pa,
    StarHfd,
    RaMinMove,
    DecMinMove,
    Blt,
    DecAlgo,
}

impl RecKind {
    const ALL: [RecKind; 10] = [
        RecKind::Exp,
        RecKind::Bin,
        RecKind::Cal,
        RecKind::Star,
        RecKind::Pa,
        RecKind::StarHfd,
        RecKind::RaMinMove,
        RecKind::DecMinMove,
        RecKind::Blt,
        RecKind::DecAlgo,
    ];

    pub fn tag(self) -> &'static str {
        match self {
            RecKind::Exp => "Exp",
            RecKind::Bin => "Bin",
            RecKind::Cal => "Cal",
            RecKind::Star => "Star",
            RecKind::Pa => "PA",
            RecKind::StarHfd => "StarHFD",
            RecKind::RaMinMove => "RAMinMove",
            RecKind::DecMinMove => "DecMinMove",
            RecKind::Blt => "BLT",
            RecKind::DecAlgo => "DecAlgo",
        }
    }

    /// The recommendation has an Apply button.
    pub fn has_apply(self) -> bool {
        matches!(self, RecKind::RaMinMove | RecKind::DecMinMove | RecKind::Blt | RecKind::DecAlgo)
    }
}

/// One line of the Recommendations box.
#[derive(Clone, Debug, PartialEq)]
pub struct Recommendation {
    pub kind: RecKind,
    pub text: String,
    /// Its Apply button is enabled (never without one).
    pub can_apply: bool,
}

impl Recommendation {
    fn new(kind: RecKind, text: String) -> Self {
        Recommendation { kind, text, can_apply: kind.has_apply() }
    }
}

/// The result grids as text (PHD2's `GADetails`): the live run's cells, or
/// a saved run read back by [`load_results`]. The fields after
/// `backlash_info` only come from the profile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GaDetails {
    pub timestamp: String,
    pub snr: String,
    pub star_mass: String,
    pub sample_count: String,
    pub elapsed_time: String,
    pub exposure_time: String,
    pub ra_hpf_rms: String,
    pub dec_hpf_rms: String,
    pub total_hpf_rms: String,
    pub ra_peak: String,
    pub ra_peak_peak: String,
    pub ra_drift_rate: String,
    pub ra_max_drift_rate: String,
    pub drift_limiting_exposure: String,
    pub dec_drift_rate: String,
    pub dec_peak: String,
    pub pa_error: String,
    pub backlash_info: String,
    pub dec_lf_drift_rate: String,
    pub dec_corrected_rms: String,
    pub rec_ra_min_move: String,
    pub rec_dec_min_move: String,
    /// The backlash test's Dec positions, north then south legs.
    pub blt_north: Vec<f64>,
    pub blt_south: Vec<f64>,
    /// Its pulse, ms; −1 when not saved.
    pub blt_msmt_pulse: i32,
    /// The measured backlash, ms, as saved (`BLT_pulse`).
    pub blt_amount: String,
    /// The recommendations, `Tag:text` lines.
    pub recommendations: String,
}

impl GaDetails {
    /// The string keys under `/GA/<time>/`, in `SaveGAResults`' order (the
    /// `BLT_*` keys are handled apart).
    fn string_fields(&mut self) -> [(&'static str, &mut String); 22] {
        [
            ("timestamp", &mut self.timestamp),
            ("snr", &mut self.snr),
            ("star_mass", &mut self.star_mass),
            ("sample_count", &mut self.sample_count),
            ("elapsed_time", &mut self.elapsed_time),
            ("exposure_time", &mut self.exposure_time),
            ("ra_hpf_rms", &mut self.ra_hpf_rms),
            ("dec_hpf_rms", &mut self.dec_hpf_rms),
            ("total_hpf_rms", &mut self.total_hpf_rms),
            ("ra_peak", &mut self.ra_peak),
            ("ra_peak_peak", &mut self.ra_peak_peak),
            ("ra_drift_rate", &mut self.ra_drift_rate),
            ("ra_peak_drift_rate", &mut self.ra_max_drift_rate),
            ("ra_drift_exposure", &mut self.drift_limiting_exposure),
            ("dec_drift_rate", &mut self.dec_drift_rate),
            ("dec_peak", &mut self.dec_peak),
            ("pa_error", &mut self.pa_error),
            ("dec_corrected_rms", &mut self.dec_corrected_rms),
            ("backlash_info", &mut self.backlash_info),
            ("dec_lf_drift_rate", &mut self.dec_lf_drift_rate),
            ("rec_ra_minmove", &mut self.rec_ra_min_move),
            ("rec_dec_minmove", &mut self.rec_dec_min_move),
        ]
    }
}

/// What the assistant asks of the rest of PHD2, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum GaEvent {
    /// A debug-log line, without its newline.
    Debug(String),
    /// `GuideLog.NotifyGAResult`: the text after `INFO: GA Result - `, with
    /// PHD2's trailing newline.
    Result(String),
    /// `GuideLog.NotifyGACompleted`: the guide-log summary counts the run.
    Completed,
    /// `NotifyGuidingParam`: the `GuideParamChange` event and its guide-log
    /// line.
    ParamChange { name: String, value: Value },
    /// What the scope or a mount said while an Apply ran.
    Mount(MountEvent),
    /// `SetGuidingEnabled` on pMount / pSecondaryMount; `None` leaves a
    /// mount alone (there is none).
    GuideOutputs(GuideOutputs),
    /// `pGuider->SetMultiStarMode`.
    MultiStar(bool),
    /// `pFrame->SetVariableDelayConfig`, which logs and saves it.
    VariableDelay(VarDelayCfg),
    /// `pCamera->UseSubframes`.
    UseSubframes(bool),
    /// A backlash-tool step to carry out: its pulse, measurement mode, its
    /// debug lines (see [`BltStep`]).
    Backlash(BltStep),
}

/// What a guide step's sample needs besides the step itself.
#[derive(Clone, Copy, Debug)]
pub struct StepEnv<'a> {
    /// `wxGetUTCTimeMillis`, ms; its whole seconds stand for
    /// `wxGetCurrentTime`, the clock of the Dec drift fit.
    pub now_ms: i64,
    /// `RequestedExposureDuration`, ms.
    pub exposure_ms: i32,
    /// `GetCameraPixelScale`, ″/px (1.0 when unknown).
    pub pixel_scale: f64,
    /// The pointing source's declination, radians; `None` when unknown (the
    /// polar alignment error is then a lower bound, as at Dec 0).
    pub dec_rad: Option<f64>,
    /// `TheScope()`'s calibration when pMount is an AO: PHD2 turns the
    /// step's camera offset into scope axes through it (with the transform's
    /// three debug lines). `None` when pMount is the scope.
    pub ao_scope_cal: Option<&'a Calibration>,
}

/// What the recommendations read from the camera, the guider and pMount
/// (`MakeRecommendations`). Plain values, read without side effects: the
/// engine fills it before each call that may end the run. What they read
/// from `TheScope()` comes from [`GaCx::scope`] when they are made.
#[derive(Clone, Debug)]
pub struct RecEnv {
    /// `GetCameraPixelScale`, ″/px (1.0 when unknown).
    pub pixel_scale: f64,
    /// `GetFocalLength`, mm; the camera's pixel size (µm), binning and
    /// largest binning.
    pub focal_length: i32,
    pub pixel_size: f64,
    pub binning: u32,
    pub max_binning: u32,
    /// `pMount->HasHPEncoders()`: the scope's flag, false when pMount is an
    /// AO.
    pub hp_encoders: bool,
    /// pMount's algorithms: the min-moves (−1 without one) and the Dec
    /// algorithm's class name.
    pub ra_min_move: f64,
    pub dec_min_move: f64,
    pub dec_algo_class: &'static str,
    /// The primary star's HFD.
    pub star_hfd: f64,
}

/// What the calls that may end the run or drive the backlash test need.
pub struct GaCx<'a, P: ProfileAccess + ?Sized> {
    pub rec: &'a RecEnv,
    /// The results are saved there.
    pub profile: &'a mut P,
    /// `TheScope()`, `None` without one. The backlash test steps through its
    /// calibration and resets its compensation; the recommendations read
    /// its calibration details, compensation pulse and Dec guide mode —
    /// only then, as PHD2 does, since `LoadCalibrationDetails` may log and
    /// clear bogus guide speeds once per session.
    pub scope: Option<&'a mut Scope>,
    /// What else the backlash tool reads on each step: the camera frame
    /// size (px), `Guider::GetMaxMovePixels` and `wxGetUTCTimeMillis` (ms).
    pub frame_size: (i32, i32),
    pub max_move_px: i32,
    pub now_ms: i64,
    /// The guider's `CurrentPosition`, `None` when it has none.
    pub star: Option<Point>,
    /// `pCamera->UseSubframes`.
    pub use_subframes: bool,
}

impl<P: ProfileAccess + ?Sized> GaCx<'_, P> {
    /// The backlash tool's step environment and `TheScope()`'s
    /// compensation; `None` without a scope.
    fn blt(&mut self) -> Option<(BltEnv<'_>, &mut BacklashComp)> {
        let scope = self.scope.as_deref_mut()?;
        let calibration_duration_ms = scope.calibration_duration();
        let (cal, blc) = scope.blt_parts()?;
        let env = BltEnv {
            cal,
            frame_width: self.frame_size.0,
            frame_height: self.frame_size.1,
            max_move_px: self.max_move_px,
            calibration_duration_ms,
            pixel_scale: self.rec.pixel_scale,
            now_ms: self.now_ms,
        };
        Some((env, blc))
    }
}

/// What an Apply changes: `TheScope()` and pMount.
pub struct ApplyCx<'a, P: ProfileAccess> {
    pub scope: Option<&'a mut Scope>,
    /// pMount's `Mount` half when pMount is an AO; `None` when it is the
    /// scope.
    pub ao: Option<&'a mut Mount>,
    pub profile: &'a mut P,
    pub algo: &'a AlgoCtx,
}

/// The countdown (`SampleWait`) Stop opens before 2 minutes of samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Wait {
    seconds_left: i64,
    perform_blt: bool,
}

/// The Guiding Assistant window's state (`GuidingAsstWin`).
pub struct GuidingAssistant {
    dlg_state: DialogState,
    instructions: String,
    status: String,
    measuring: bool,
    measuring_backlash: bool,
    measurements_taken: bool,
    review_mode: bool,
    wait: Option<Wait>,
    start_time_ms: i64,
    elapsed_secs: i64,
    start_pos: Point,
    start_str: String,
    hpf_ra_stats: DescriptiveStats,
    lpf_ra_stats: DescriptiveStats,
    hpf_dec_stats: DescriptiveStats,
    dec_axis_stats: AxisStats,
    ra_axis_stats: AxisStats,
    axis_timebase: i64,
    ra_hpf: HighPassFilter,
    ra_lpf: LowPassFilter,
    dec_hpf: HighPassFilter,
    sum_snr: f64,
    sum_mass: f64,
    last_time: f64,
    max_rate_ra: f64,
    dec_drift_per_min: f64,
    /// Never assigned by PHD2 (an uninitialised member it saves as
    /// `dec_corrected_rms`): 0 here.
    dec_corrected_rms: f64,
    alignment_error: f64,
    backlash_ms: i32,
    backlash_recommended_ms: i32,
    guide_output_disabled: bool,
    saved_outputs: GuideOutputs,
    orig_sub_frames: i32,
    orig_multistar: bool,
    orig_var_delay: VarDelayCfg,
    measure_backlash: bool,
    backlash_cb_enabled: bool,
    tool: Option<BacklashTool>,
    graph_enabled: bool,
    ra_minmove_rec: f64,
    dec_minmove_rec: f64,
    min_exp_rec: f64,
    max_exp_rec: f64,
    details: GaDetails,
    recs: Vec<Recommendation>,
}

impl GuidingAssistant {
    /// Open the window (`GuidingAsstWin()`). `blt_cal` is `TheScope()`'s last
    /// calibration, `None` without a scope (no backlash test then);
    /// `hp_encoders` pMount's flag, which unticks the backlash test.
    /// Variable delay goes off at once — and, as in PHD2, comes back only
    /// after a run: closing the window without one leaves it off. PHD2 starts
    /// measuring at once when guiding: the caller follows with
    /// [`start`](Self::start).
    pub fn new(
        blt_cal: Option<&Calibration>,
        hp_encoders: bool,
        multistar: bool,
        var_delay: VarDelayCfg,
        guiding: bool,
        ev: &mut Vec<GaEvent>,
    ) -> Self {
        let tool = blt_cal.map(|cal| {
            let (tool, log) = BacklashTool::new(cal);
            ev.extend(log.into_iter().map(GaEvent::Debug));
            tool
        });
        ev.push(GaEvent::VariableDelay(VarDelayCfg { enabled: false, ..var_delay }));
        let mut ga = GuidingAssistant {
            dlg_state: DialogState::NoStar,
            instructions: String::new(),
            status: String::new(),
            measuring: false,
            measuring_backlash: false,
            measurements_taken: false,
            review_mode: false,
            wait: None,
            start_time_ms: 0,
            elapsed_secs: 0,
            start_pos: Point::default(),
            start_str: String::new(),
            hpf_ra_stats: DescriptiveStats::new(),
            lpf_ra_stats: DescriptiveStats::new(),
            hpf_dec_stats: DescriptiveStats::new(),
            dec_axis_stats: AxisStats::new(),
            ra_axis_stats: AxisStats::new(),
            axis_timebase: 0,
            ra_hpf: HighPassFilter::new(1.0, 1.0),
            ra_lpf: LowPassFilter::new(6.0, 1.0),
            dec_hpf: HighPassFilter::new(1.0, 1.0),
            sum_snr: 0.0,
            sum_mass: 0.0,
            last_time: 0.0,
            max_rate_ra: 0.0,
            dec_drift_per_min: 0.0,
            dec_corrected_rms: 0.0,
            alignment_error: 0.0,
            backlash_ms: 0,
            backlash_recommended_ms: 0,
            guide_output_disabled: false,
            saved_outputs: GuideOutputs::default(),
            orig_sub_frames: -1,
            orig_multistar: multistar,
            orig_var_delay: var_delay,
            measure_backlash: blt_cal.is_some() && !hp_encoders,
            backlash_cb_enabled: blt_cal.is_some(),
            tool,
            graph_enabled: false,
            ra_minmove_rec: 0.0,
            dec_minmove_rec: 0.0,
            min_exp_rec: 0.0,
            max_exp_rec: 0.0,
            details: GaDetails::default(),
            recs: Vec::new(),
        };
        ga.app_state_changed(guiding, ev);
        ga
    }

    // ------------------------------------------------------------- reads

    pub fn state(&self) -> DialogState {
        self.dlg_state
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// The status line under the grids (`m_gaStatus`).
    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn is_measuring(&self) -> bool {
        self.measuring
    }

    pub fn is_measuring_backlash(&self) -> bool {
        self.measuring_backlash
    }

    /// Start is enabled: guiding, and nothing running.
    pub fn can_start(&self, guiding: bool) -> bool {
        guiding && !self.measuring && !self.measuring_backlash && self.wait.is_none()
    }

    /// The countdown's seconds left while Stop waits for the minimum
    /// sampling, and whether the backlash test follows it.
    pub fn wait(&self) -> Option<(i64, bool)> {
        self.wait.map(|w| (w.seconds_left.max(0), w.perform_blt))
    }

    /// The result grids, live or a reviewed run's.
    pub fn details(&self) -> &GaDetails {
        &self.details
    }

    /// A saved run is shown ([`review`](Self::review)): its backlash graph
    /// comes from [`details`](Self::details), not the tool.
    pub fn is_review(&self) -> bool {
        self.review_mode
    }

    pub fn recommendations(&self) -> &[Recommendation] {
        &self.recs
    }

    /// The "Measure Declination Backlash" box: ticked, enabled.
    pub fn measure_backlash(&self) -> (bool, bool) {
        (self.measure_backlash, self.backlash_cb_enabled && !self.measuring_backlash)
    }

    pub fn set_measure_backlash(&mut self, on: bool) {
        if self.backlash_cb_enabled && !self.measuring_backlash {
            self.measure_backlash = on;
        }
    }

    /// The backlash tool, for its graph (north / south steps, pulse).
    pub fn backlash_tool(&self) -> Option<&BacklashTool> {
        self.tool.as_ref()
    }

    /// "Show Backlash Graph" is enabled.
    pub fn graph_enabled(&self) -> bool {
        self.graph_enabled
    }

    /// The min-move recommendations (RA, Dec), px.
    pub fn min_move_recs(&self) -> (f64, f64) {
        (self.ra_minmove_rec, self.dec_minmove_rec)
    }

    /// The recommended exposure range, seconds.
    pub fn exposure_range(&self) -> (f64, f64) {
        (self.min_exp_rec, self.max_exp_rec)
    }

    /// The latest polar alignment error, arc-minutes.
    pub fn pa_error_arcmin(&self) -> f64 {
        self.alignment_error
    }

    /// The latest Dec drift, px/min.
    pub fn dec_drift_px_per_min(&self) -> f64 {
        self.dec_drift_per_min
    }

    // ------------------------------------------------------------- flow

    fn fill_instructions(&mut self, state: DialogState) {
        let instr = match state {
            DialogState::NoStar => INSTR_NO_STAR,
            // After a run the line stays as it was.
            DialogState::StartReady if self.measurements_taken => return,
            DialogState::StartReady => INSTR_START_READY,
            DialogState::Measuring => INSTR_MEASURING,
            DialogState::Stopped => INSTR_STOPPED,
        };
        self.instructions = instr.to_string();
    }

    /// `OnAppStateNotify`: guiding stopping ends a measurement; otherwise the
    /// instructions follow whether a run can start.
    pub fn app_state_changed(&mut self, guiding: bool, ev: &mut Vec<GaEvent>) {
        if self.measuring || self.measuring_backlash {
            if !guiding {
                self.do_stop(ev);
            }
        } else {
            self.dlg_state = if guiding { DialogState::StartReady } else { DialogState::NoStar };
            self.fill_instructions(self.dlg_state);
        }
    }

    /// Start (`OnStart`): multi-star off, the statistics reset, the guide
    /// outputs off (`outputs` holds their current state, `None` for a mount
    /// that is not there). `exposure_ms` is `RequestedExposureDuration`,
    /// `wall` names the run. Refused unless guiding, and while a run is on.
    pub fn start(
        &mut self,
        guiding: bool,
        exposure_ms: i32,
        outputs: GuideOutputs,
        wall: DateTime<Local>,
        now_ms: i64,
        ev: &mut Vec<GaEvent>,
    ) -> bool {
        if !guiding || self.measuring || self.measuring_backlash || self.wait.is_some() {
            return false;
        }
        let exposure = f64::from(exposure_ms) / 1000.0;
        let lp_cutoff = wx_max(6.0, 3.0 * exposure);
        let hp_cutoff = 1.0;

        ev.push(GaEvent::MultiStar(false));
        self.hpf_ra_stats.clear_all();
        self.lpf_ra_stats.clear_all();
        self.hpf_dec_stats.clear_all();
        self.dec_axis_stats.clear_all();
        self.ra_axis_stats.clear_all();
        self.ra_hpf = HighPassFilter::new(hp_cutoff, exposure);
        self.ra_lpf = LowPassFilter::new(lp_cutoff, exposure);
        self.dec_hpf = HighPassFilter::new(hp_cutoff, exposure);
        self.sum_snr = 0.0;
        self.sum_mass = 0.0;

        self.review_mode = false;
        self.dlg_state = DialogState::Measuring;
        self.fill_instructions(DialogState::Measuring);
        self.status = "Measuring...".to_string();
        // The Recommendations box is hidden until the next results.
        self.recs.clear();

        ev.push(GaEvent::Debug("GuidingAssistant: Disabling guide output".into()));
        self.saved_outputs = outputs;
        ev.push(GaEvent::GuideOutputs(GuideOutputs {
            primary: outputs.primary.map(|_| false),
            secondary: outputs.secondary.map(|_| false),
        }));
        self.guide_output_disabled = true;

        self.start_str = wall.format("%Y-%m-%d %H:%M:%S").to_string();
        self.measuring = true;
        self.start_time_ms = now_ms;
        true
    }

    /// A guide step (`UpdateInfo`), counted while measuring. Only the steps
    /// PHD2 graphs reach it (`Mount::LogGuideStepInfo`): with an AO, the
    /// AO's own steps, not the scope's bumps.
    pub fn guide_step(&mut self, info: &GuideStepInfo, env: &StepEnv, ev: &mut Vec<GaEvent>) {
        if !self.measuring || !info.move_options.has(MoveOpts::GRAPH) {
            return;
        }
        let mut ra = info.mount_offset.x;
        let mut dec = info.mount_offset.y;
        if let Some(cal) = env.ao_scope_cal {
            let mut log = Vec::new();
            if let Some(m) = camera_to_mount_logged(Some(info.camera_offset), cal, &mut log) {
                ra = m.x;
                dec = m.y;
            }
            ev.extend(log.into_iter().map(GaEvent::Debug));
        }
        let elapsed_ms = env.now_ms - self.start_time_ms;
        self.elapsed_secs = elapsed_ms / 1000;

        let v = self.ra_hpf.add_value(ra);
        self.hpf_ra_stats.add_value(v);
        let mut prev_ra_lpf = self.ra_lpf.current_lpf();
        let new_ra_lpf = self.ra_lpf.add_value(ra);
        if self.lpf_ra_stats.count() == 0 {
            prev_ra_lpf = new_ra_lpf;
        }
        self.lpf_ra_stats.add_value(new_ra_lpf);
        let v = self.dec_hpf.add_value(dec);
        self.hpf_dec_stats.add_value(v);
        // Whole seconds since the first sample: the drift fit's clock.
        let now_s = env.now_ms.div_euclid(1000);
        if self.dec_axis_stats.count() == 0 {
            self.axis_timebase = now_s;
        }
        let t = (now_s - self.axis_timebase) as f64;
        self.dec_axis_stats.add_guide_info(t, dec, 0.0);
        self.ra_axis_stats.add_guide_info(t, ra, 0.0);

        // The fastest RA move between samples, on the low-passed offsets.
        if self.lpf_ra_stats.count() == 1 {
            self.start_pos = info.mount_offset;
            self.max_rate_ra = 0.0;
        } else {
            let dt = info.time - self.last_time;
            if dt > 0.0001 {
                let ra_rate = (new_ra_lpf - prev_ra_lpf).abs() / dt;
                if ra_rate > self.max_rate_ra {
                    self.max_rate_ra = ra_rate;
                }
            }
        }
        self.last_time = info.time;
        self.sum_snr += info.star_snr;
        self.sum_mass += info.star_mass;
        let n = f64::from(self.lpf_ra_stats.count());

        let d = &mut self.details;
        d.timestamp = self.start_str.clone();
        d.exposure_time = format!("{}s", fmt_g(f64::from(env.exposure_ms) / 1000.0));
        d.snr = fmt_f(self.sum_snr / n, 1);
        d.star_mass = fmt_f(self.sum_mass / n, 1);
        d.elapsed_time = format!("{}s", (elapsed_ms / 1000) as u32);
        d.sample_count = fmt_f(n, 0);

        if n > 1.0 {
            let rarms = self.hpf_ra_stats.sigma();
            let decrms = self.hpf_dec_stats.sigma();
            let combined = rarms.hypot(decrms);

            // The polar alignment error from the Dec drift (Barrett,
            // celestialwonders.com/articles/polaralignment).
            let pxscale = env.pixel_scale;
            let cosdec = env.dec_rad.map_or(1.0, f64::cos);
            self.dec_drift_per_min = 60.0 * self.dec_axis_stats.linear_fit().slope;
            self.alignment_error = 3.8197 * self.dec_drift_per_min.abs() * pxscale / cosdec;

            let cell = |px: f64, units1: &str, units2: &str| {
                format!("{:>6} {units1} ({:>6} {units2} )", fmt_f(px, 2), fmt_f(px * pxscale, 2))
            };
            d.ra_hpf_rms = cell(rarms, "px", "arc-sec");
            d.dec_hpf_rms = cell(decrms, "px", "arc-sec");
            d.total_hpf_rms = cell(combined, "px", "arc-sec");
            d.ra_peak = cell(self.ra_axis_stats.max_delta(), "px", "arc-sec");
            d.dec_peak = cell(self.dec_axis_stats.max_delta(), "px", "arc-sec");
            let ra_pk_pk = self.lpf_ra_stats.maximum() - self.lpf_ra_stats.minimum();
            d.ra_peak_peak = cell(ra_pk_pk, "px", "arc-sec");
            // Start to now, unsmoothed.
            let ra_drift_rate = (ra - self.start_pos.x) / self.elapsed_secs as f64 * 60.0;
            d.ra_drift_rate = cell(ra_drift_rate, "px/min", "arc-sec/min");
            d.ra_max_drift_rate = cell(self.max_rate_ra, "px/sec", "arc-sec/sec");
            // Revised once the min-move is known.
            d.drift_limiting_exposure = if self.max_rate_ra <= 0.0 {
                " ".to_string()
            } else {
                format!("{:>6} s ", fmt_f(1.3 * rarms / self.max_rate_ra, 1))
            };
            d.dec_drift_rate = cell(self.dec_drift_per_min, "px/min", "arc-sec/min");
            d.pa_error = format!(
                "{} {} arc-min",
                if env.dec_rad.is_none() { "> " } else { "" },
                fmt_f(self.alignment_error, 1)
            );
        }
    }

    /// Stop (`OnStop`). Before [`GA_MIN_SAMPLING_PERIOD`] of samples PHD2
    /// opens a countdown and keeps measuring: [`tick`](Self::tick) runs it
    /// down, [`end_wait`](Self::end_wait) is its Cancel. Then the backlash
    /// test when ticked, else the recommendations.
    pub fn stop<P: ProfileAccess + ?Sized>(&mut self, cx: &mut GaCx<P>, ev: &mut Vec<GaEvent>) {
        if self.wait.is_some() || !(self.measuring || self.measuring_backlash) {
            return;
        }
        // Read before the countdown, which is modal.
        let perform_blt = self.measure_backlash;
        if self.elapsed_secs < GA_MIN_SAMPLING_PERIOD && !self.measuring_backlash {
            self.wait = Some(Wait { seconds_left: GA_MIN_SAMPLING_PERIOD - self.elapsed_secs, perform_blt });
            return;
        }
        self.stop_continue(true, perform_blt, cx, ev);
    }

    /// The countdown's one-second timer: at zero the sampling is long enough.
    pub fn tick<P: ProfileAccess + ?Sized>(&mut self, cx: &mut GaCx<P>, ev: &mut Vec<GaEvent>) {
        let Some(w) = self.wait.as_mut() else { return };
        w.seconds_left -= 1;
        if w.seconds_left > 0 {
            return;
        }
        let perform_blt = w.perform_blt;
        self.wait = None;
        self.stop_continue(true, perform_blt, cx, ev);
    }

    /// The countdown's Cancel: the run ends without results, unless
    /// `accept` (PHD2's Ctrl-click) takes the samples as they are.
    pub fn end_wait<P: ProfileAccess + ?Sized>(&mut self, accept: bool, cx: &mut GaCx<P>, ev: &mut Vec<GaEvent>) {
        let Some(w) = self.wait.take() else { return };
        self.stop_continue(accept, w.perform_blt, cx, ev);
    }

    /// `OnStop` after the countdown.
    fn stop_continue<P: ProfileAccess + ?Sized>(
        &mut self,
        long_enough: bool,
        perform_blt: bool,
        cx: &mut GaCx<P>,
        ev: &mut Vec<GaEvent>,
    ) {
        self.status.clear();
        if long_enough && perform_blt {
            if self.measuring_backlash {
                // Stop during the backlash test.
                self.end_backlash_test(false, cx, ev);
                return;
            }
            // The backlash test after the sampling, on the full frame. The
            // box is only ticked with a scope, so the tool is there.
            let (star, use_subframes) = (cx.star, cx.use_subframes);
            if let (Some(tool), Some((env, blc))) = (self.tool.as_mut(), cx.blt()) {
                self.measuring_backlash = true;
                if self.orig_sub_frames == -1 {
                    self.orig_sub_frames = use_subframes as i32;
                }
                ev.push(GaEvent::UseSubframes(false));
                self.status = format!("Measuring backlash... {}", tool.last_status());
                self.measuring = false;
                let step = tool.start_measurement(self.dec_drift_per_min, star, &env, blc);
                ev.push(GaEvent::Backlash(step));
                self.instructions = INSTR_BACKLASH.to_string();
                return;
            }
        }
        if long_enough {
            self.make_recommendations(cx, ev);
        }
        self.do_stop(ev);
    }

    /// `DoStop`: the guide outputs, multi-star mode, variable delay and
    /// subframes back as they were.
    fn do_stop(&mut self, ev: &mut Vec<GaEvent>) {
        self.measuring = false;
        self.dlg_state = DialogState::Stopped;
        self.measurements_taken = true;
        self.fill_instructions(DialogState::Stopped);
        if self.guide_output_disabled {
            // PHD2 prints an uninitialised flag for a missing mount; 0 here.
            ev.push(GaEvent::Debug(format!(
                "GuidingAssistant: Re-enabling guide output ({}, {})",
                self.saved_outputs.primary.unwrap_or(false) as i32,
                self.saved_outputs.secondary.unwrap_or(false) as i32
            )));
            ev.push(GaEvent::GuideOutputs(self.saved_outputs));
            self.guide_output_disabled = false;
            ev.push(GaEvent::MultiStar(self.orig_multistar));
            ev.push(GaEvent::VariableDelay(self.orig_var_delay));
        }
        if self.orig_sub_frames != -1 {
            ev.push(GaEvent::UseSubframes(self.orig_sub_frames != 0));
            self.orig_sub_frames = -1;
        }
    }

    /// Close the window (`OnClose`). As in PHD2 a backlash test in progress
    /// is not stopped: the guider stays in measurement mode, its steps going
    /// nowhere, until guiding stops.
    pub fn close(&mut self, ev: &mut Vec<GaEvent>) {
        self.do_stop(ev);
    }

    // ------------------------------------------------------------ backlash

    /// A guide frame in the guider's measurement mode (`BacklashStep`), with
    /// the star's camera position it carries
    /// ([`GuiderEvent::BacklashStep`](crate::engine::guider::GuiderEvent::BacklashStep)):
    /// the tool's next step, and the wrap-up once it is done.
    pub fn backlash_step<P: ProfileAccess + ?Sized>(
        &mut self,
        cam_loc: Point,
        cx: &mut GaCx<P>,
        ev: &mut Vec<GaEvent>,
    ) {
        if !self.measuring_backlash {
            return;
        }
        let (Some(tool), Some((env, blc))) = (self.tool.as_mut(), cx.blt()) else { return };
        let step = tool.dec_measurement_step(Some(cam_loc), &env, blc);
        ev.push(GaEvent::Backlash(step));
        self.status = format!("Measuring backlash: {}", tool.last_status());
        // PHD2 also guards against re-entering this wrap-up from its event
        // loop; nothing re-enters here.
        match tool.blt_state() {
            BltState::Completed => {
                ev.push(GaEvent::Debug("GA-BLT: state = completed".into()));
                let qual = tool.measurement_quality();
                let completed = matches!(qual, MeasurementResult::Valid | MeasurementResult::TooFewNorth);
                if completed {
                    ev.push(GaEvent::Debug("GA-BLT: Wrap-up after normal completion".into()));
                    self.backlash_ms = tool.backlash_result_ms();
                    let text = tool.ga_result_text(cx.rec.pixel_scale).unwrap_or_default();
                    let out = text.strip_prefix("Backlash=").unwrap_or(&text).to_string();
                    self.details.backlash_info = out.clone();
                    ev.push(GaEvent::Result(format!("{text}\n")));
                    ev.push(GaEvent::Debug(format!("BLT: Reported result = {out}")));
                    self.graph_enabled = true;
                } else {
                    self.details.backlash_info.clear();
                }
                self.end_backlash_test(completed, cx, ev);
            }
            BltState::Aborted => self.end_backlash_test(false, cx, ev),
            _ => {}
        }
    }

    /// The guider left Guiding during the backlash test
    /// (`NotifyBacklashError`).
    pub fn backlash_error<P: ProfileAccess + ?Sized>(&mut self, cx: &mut GaCx<P>, ev: &mut Vec<GaEvent>) {
        if self.measuring_backlash {
            self.end_backlash_test(false, cx, ev);
        }
    }

    /// `EndBacklashTest`: the recommendations, with or without the backlash.
    fn end_backlash_test<P: ProfileAccess + ?Sized>(
        &mut self,
        completed: bool,
        cx: &mut GaCx<P>,
        ev: &mut Vec<GaEvent>,
    ) {
        if !completed {
            let star = cx.star;
            if let (Some(tool), Some((env, blc))) = (self.tool.as_mut(), cx.blt()) {
                let step = tool.stop_measurement(star, &env, blc);
                ev.push(GaEvent::Backlash(step));
                self.graph_enabled = tool.is_graphable();
            }
            self.details.backlash_info = "Backlash test aborted, see graph...".to_string();
        }
        self.measuring_backlash = false;
        self.make_recommendations(cx, ev);
        // PHD2 refreshes the app-state controls here on an abort; DoStop
        // overwrites what that sets.
        self.do_stop(ev);
    }

    // ------------------------------------------------------ recommendations

    /// `GetMinMoveRecs`: the min-moves (RA, Dec) from the seeing. The
    /// estimate is the Dec sigma, drift-corrected when that is smaller; over
    /// a run longer than 2.4 minutes, the smallest over 2-minute windows
    /// stepped by a minute. The Dec min-move sits at 1.28 σ (scale < 1.5″)
    /// or 1.65 σ, rounded up to 0.05 px; RA at 65 % of it (100 % with
    /// encoders). Over 1.25″ PHD2 distrusts it and falls back to the
    /// profile wizard's default.
    fn get_min_move_recs(&self, rec: &RecEnv, ev: &mut Vec<GaEvent>) -> (f64, f64) {
        let dbg = |ev: &mut Vec<GaEvent>, s: String| ev.push(GaEvent::Debug(s));
        let stats = &self.dec_axis_stats;
        // PHD2's `GetEntry` gives a zero sample out of range.
        let entry = |inx: i64| if inx < 0 { stats.entry(usize::MAX) } else { stats.entry(inx as usize) };
        let mut best_estimate = 1000.0;
        let mut sel_r_squared = 0.0;
        let mut sel_slope = 0.0;
        let last_inx = stats.count() as i64 - 1;
        let pxscale = rec.pixel_scale;
        let mut val = entry(0);
        let mut t_start = val.delta_time;
        let multiplier_dec = if pxscale < 1.5 { 1.28 } else { 1.65 };
        let mut min_move_floor = 0.1;

        if stats.last_entry().delta_time - t_start > 1.2 * MEASUREMENT_WINDOW_SIZE {
            let mut dec_vals = AxisStats::new();
            let mut inx: i64 = 0;
            let mut window_start: i64 = 0;
            loop {
                val = entry(inx);
                dec_vals.add_guide_info(val.delta_time, val.star_pos, 0.0);
                // The last window counts once it spans 1.6 minutes.
                if val.delta_time - t_start >= MEASUREMENT_WINDOW_SIZE
                    || (inx == last_inx && val.delta_time - t_start >= 0.8 * MEASUREMENT_WINDOW_SIZE)
                {
                    if dec_vals.count() > 1 {
                        let simple_sigma = dec_vals.sigma();
                        let fit = dec_vals.linear_fit();
                        // With little drift the correction only adds noise.
                        if fit.sigma < simple_sigma {
                            if fit.sigma < best_estimate {
                                best_estimate = fit.sigma;
                                sel_r_squared = fit.r_squared;
                                sel_slope = fit.slope;
                            }
                        } else {
                            best_estimate = wx_min(best_estimate, simple_sigma);
                        }
                        dbg(
                            ev,
                            format!(
                                "GA long series, window start={}, window end={}, Uncorrected RMS={}, Drift={}, \
                                 Corrected RMS={}, R-sq={}",
                                fmt_f(t_start, 0),
                                fmt_f(val.delta_time, 0),
                                fmt_f(simple_sigma, 3),
                                fmt_f(fit.slope * 60.0, 3),
                                fmt_f(fit.sigma, 3),
                                fmt_f(fit.r_squared, 3)
                            ),
                        );
                    }
                    // The next window starts a minute before this one ended.
                    let target_time = val.delta_time - WINDOW_ADJUSTMENT;
                    while entry(inx).delta_time > target_time {
                        inx -= 1;
                    }
                    // Fix: after a gap of over 36 s in the samples the next
                    // window is this one again and PHD2 loops for ever; every
                    // window it would compute has been.
                    if inx == window_start {
                        break;
                    }
                    window_start = inx;
                    t_start = entry(inx).delta_time;
                    dec_vals.clear_all();
                } else {
                    inx += 1;
                }
                if inx > last_inx {
                    break;
                }
            }
            dbg(
                ev,
                format!(
                    "Full uncorrected RMS={}px, Selected Dec drift={} px/min, Best seeing estimate={}px, R-sq={}",
                    fmt_f(stats.sigma(), 3),
                    fmt_f(sel_slope * 60.0, 3),
                    fmt_f(best_estimate, 3),
                    fmt_f(sel_r_squared, 3)
                ),
            );
        } else if stats.count() > 1 {
            // A run of up to 2.4 minutes: the whole of it.
            let simple_sigma = stats.sigma();
            let fit = stats.linear_fit();
            best_estimate = if fit.sigma < simple_sigma { fit.sigma } else { simple_sigma };
            dbg(
                ev,
                format!(
                    "Uncorrected Dec RMS={}px, Dec drift={} px/min, Best seeing estimate={}px, R-sq={}",
                    fmt_f(simple_sigma, 3),
                    fmt_f(fit.slope * 60.0, 3),
                    fmt_f(best_estimate, 3),
                    fmt_f(fit.r_squared, 3)
                ),
            );
        }
        if self.orig_multistar {
            best_estimate *= 0.9;
            min_move_floor = 0.05;
        }
        let multiplier_ra = if rec.hp_encoders { 1.0 } else { 0.65 };
        // Up to the next 0.05 px, at least 0.05.
        let unit = 0.05;
        let round_up_est = std_max((best_estimate * multiplier_dec / unit + 0.5).round() * unit, 0.05);
        let (rec_ra, rec_dec);
        if pxscale * round_up_est <= 1.25 {
            rec_dec = round_up_est;
            rec_ra = wx_max(min_move_floor, rec_dec * multiplier_ra);
            dbg(
                ev,
                format!(
                    "GA Min-Move recommendations are seeing-based: Dec={}, RA={}",
                    fmt_f(rec_dec, 3),
                    fmt_f(rec_ra, 3)
                ),
            );
        } else {
            rec_dec = smart_default_min_move_for(rec.focal_length, rec.pixel_size, rec.binning);
            rec_ra = wx_max(min_move_floor, rec_dec * multiplier_ra);
            dbg(
                ev,
                format!(
                    "GA Min-Move calcs failed sanity-check, DecEst={}, Dec-HPF-Sigma={}",
                    fmt_f(round_up_est, 3),
                    fmt_f(self.hpf_dec_stats.sigma(), 3)
                ),
            );
            dbg(
                ev,
                format!(
                    "GA Min-Move recs reverting to smart defaults, RA={}, Dec={}",
                    fmt_f(rec_ra, 3),
                    fmt_f(rec_dec, 3)
                ),
            );
        }
        // PHD2's catch branch for a thrown string is dead code: nothing in
        // here throws.
        (rec_ra, rec_dec)
    }

    /// `LikelyBacklash`: large Dec backlash, measured or inferred, which
    /// relaxes the polar alignment advice. `blc_pulse` and `mode` are
    /// `TheScope()`'s compensation pulse (ms) and Dec guide mode.
    fn likely_backlash(&self, d: &CalibrationDetails, blc_pulse: i32, mode: DecGuideMode) -> bool {
        let completed = self.tool.as_ref().is_some_and(|t| t.blt_state() == BltState::Completed);
        // Just measured, too big to compensate.
        let mut likely = completed && self.backlash_ms > MAX_BACKLASH_COMP;
        if !likely {
            // A compensation tried once with a pulse that large.
            likely = blc_pulse > MAX_BACKLASH_COMP;
        }
        if !likely {
            // Guiding Dec one way gains from a larger alignment error.
            likely = !matches!(mode, DecGuideMode::Auto | DecGuideMode::None);
        }
        if !likely && d.dec_step_count > 0 {
            // Little or no movement south in the last calibration.
            let steps = &d.dec_steps;
            if let (Some(north_start), Some(north_end), Some(south_end)) =
                (steps.first(), steps.get(d.dec_step_count as usize - 1), steps.last())
            {
                let north_dist = ((north_start.x - north_end.x).powi(2) + (north_start.y - north_end.y).powi(2)).sqrt();
                let south_dist = ((north_end.x - south_end.x).powi(2) + (north_end.y - south_end.y).powi(2)).sqrt();
                likely = south_dist <= 0.1 * north_dist;
            }
        }
        likely
    }

    /// `LogResults`: the grids in three guide-log and debug-log lines.
    fn log_results(&self, ev: &mut Vec<GaEvent>) {
        let d = &self.details;
        ev.push(GaEvent::Debug("Guiding Assistant results follow:".into()));
        for line in [
            format!(
                "SNR={}, Samples={}, Elapsed Time={}, RA HPF-RMS={}, Dec HPF-RMS={}, Total HPF-RMS={}",
                d.snr, d.sample_count, d.elapsed_time, d.ra_hpf_rms, d.dec_hpf_rms, d.total_hpf_rms
            ),
            format!(
                "RA Peak={}, RA Peak-Peak {}, RA Drift Rate={}, Max RA Drift Rate={}, Drift-Limiting Exp={}",
                d.ra_peak, d.ra_peak_peak, d.ra_drift_rate, d.ra_max_drift_rate, d.drift_limiting_exposure
            ),
            format!("Dec Drift Rate={}, Dec Peak={}, PA Error={}", d.dec_drift_rate, d.dec_peak, d.pa_error),
        ] {
            ev.push(GaEvent::Result(format!("{line}\n")));
            ev.push(GaEvent::Debug(line));
        }
    }

    /// `MakeRecommendations`, then the run saved in the profile.
    fn make_recommendations<P: ProfileAccess + ?Sized>(&mut self, cx: &mut GaCx<P>, ev: &mut Vec<GaEvent>) {
        let rec = cx.rec;
        let mut mev = Vec::new();
        let (cal_details, blc_pulse, dec_guide_mode) = match cx.scope.as_deref_mut() {
            Some(scope) => (
                scope.load_calibration_details(&mut cx.profile, &mut mev),
                scope.backlash_comp().map_or(0, BacklashComp::backlash_pulse_width),
                scope.dec_guide_mode(),
            ),
            // Fix: PHD2 dereferences a null `TheScope()` here (an AO without
            // a mount); nothing to read then.
            None => (CalibrationDetails::default(), 0, DecGuideMode::None),
        };
        ev.extend(mev.into_iter().map(GaEvent::Mount));
        // Fix: without a scope PHD2 also dereferences its null backlash tool
        // (here, in LikelyBacklash and for the BLT advice); none is not
        // completed, not exempted.
        let completed = self.tool.as_ref().is_some_and(|t| t.blt_state() == BltState::Completed);
        let exempted = self.tool.as_ref().is_some_and(BacklashTool::backlash_exempted);
        let suspect_calibration = cal_details.last_issue != CalibrationIssue::None || exempted;

        (self.ra_minmove_rec, self.dec_minmove_rec) = self.get_min_move_recs(rec, ev);
        // The drift-limiting exposure, now against the RA min-move.
        self.details.drift_limiting_exposure = if self.max_rate_ra <= 0.0 {
            " ".to_string()
        } else {
            format!("{:>6} s ", fmt_f(self.ra_minmove_rec / self.max_rate_ra, 1))
        };
        self.log_results(ev);

        // The exposure range: the ideal one, its low end brought down to the
        // drift-limiting exposure (rounded up to 0.5 s), never under 1 s.
        let rarms = self.hpf_ra_stats.sigma();
        let multiplier_ra = 1.0; // a 66 % prediction interval
        let has_encoders = rec.hp_encoders;
        let ideal_min_exposure = if has_encoders { 4.0 } else { 2.0 };
        let ideal_max_exposure = if has_encoders { 8.0 } else { 4.0 };
        let drift_exp = if self.max_rate_ra > 0.0 {
            ((multiplier_ra * rarms / self.max_rate_ra) / 0.5).ceil() * 0.5
        } else {
            ideal_min_exposure
        };
        let min_rec_range = 2.0;
        let pxscale = rec.pixel_scale;
        self.min_exp_rec = std_max(1.0, std_min(drift_exp, ideal_min_exposure));
        self.max_exp_rec = if drift_exp > self.min_exp_rec {
            if drift_exp < ideal_max_exposure {
                std_max(drift_exp, self.min_exp_rec + min_rec_range)
            } else {
                ideal_max_exposure
            }
        } else {
            self.min_exp_rec + min_rec_range
        };

        self.recs.clear();
        let mut all = String::new();
        let mut add = |recs: &mut Vec<Recommendation>, ev: &mut Vec<GaEvent>, kind: RecKind, msg: String, log: bool| {
            all += &format!("{}:{msg}\n", kind.tag());
            ev.push(GaEvent::Debug(format!("Recommendation: {msg}")));
            if log {
                ev.push(GaEvent::Result(format!("Recommendation: {msg}\n")));
            }
            recs.push(Recommendation::new(kind, msg));
        };

        // Always an exposure range; it and binning stay out of the guide log.
        let msg = if !has_encoders {
            format!(
                "Use exposure times in the range of {}s to {}s",
                fmt_f(self.min_exp_rec, 1),
                fmt_f(self.max_exp_rec, 1)
            )
        } else {
            format!(
                "Use exposure times from {}s to {}s and variable delay feature to control guide cadence",
                fmt_f(ideal_min_exposure, 1),
                fmt_f(ideal_max_exposure, 1)
            )
        };
        add(&mut self.recs, ev, RecKind::Exp, msg, false);
        if pxscale <= 0.5 && rec.binning == 1 && rec.max_binning > 1 {
            add(&mut self.recs, ev, RecKind::Bin, "Try binning your guide camera".into(), false);
        }
        if suspect_calibration {
            let why = if cal_details.last_issue != CalibrationIssue::None {
                "(Prior alert)"
            } else {
                "(Backlash clearing)"
            };
            add(&mut self.recs, ev, RecKind::Cal, format!("Consider re-doing your calibration {why}"), true);
        }
        if self.sum_snr / f64::from(self.lpf_ra_stats.count()) < 10.0 {
            let msg = "Consider using a brighter star for the test or increasing the exposure time";
            add(&mut self.recs, ev, RecKind::Star, msg.into(), true);
        }
        if self.alignment_error > 5.0 {
            const OVER_5: &str = "Polar alignment error > 5 arc-min; that could probably be improved.";
            const OVER_10: &str =
                "Polar alignment error > 10 arc-min; try using the Drift Align tool to improve alignment.";
            // With large Dec backlash an error under 10′ helps.
            let msg = if self.likely_backlash(&cal_details, blc_pulse, dec_guide_mode) {
                if self.alignment_error > 10.0 { OVER_10 } else { "" }
            } else if self.alignment_error < 10.0 {
                OVER_5
            } else {
                OVER_10
            };
            if !msg.is_empty() {
                add(&mut self.recs, ev, RecKind::Pa, msg.into(), true);
            }
        }
        if pxscale > 1.0 && rec.star_hfd > 4.5 {
            let msg = "Consider trying to improve focus on the guide camera";
            add(&mut self.recs, ev, RecKind::StarHfd, msg.into(), true);
        }
        if rec.ra_min_move >= 0.0 {
            let msg = format!("Try setting RA min-move to {}", fmt_f(self.ra_minmove_rec, 2));
            add(&mut self.recs, ev, RecKind::RaMinMove, msg, true);
        }
        if rec.dec_min_move >= 0.0 {
            let msg = format!("Try setting Dec min-move to {}", fmt_f(self.dec_minmove_rec, 2));
            add(&mut self.recs, ev, RecKind::DecMinMove, msg, true);
        }
        let mut small_backlash = false;
        if completed {
            let (msg, recommended, small, can_apply) =
                blt_recommendation(self.backlash_ms, has_encoders, self.dec_drift_per_min);
            self.backlash_recommended_ms = recommended;
            small_backlash = small;
            add(&mut self.recs, ev, RecKind::Blt, msg, true);
            if let Some(r) = self.recs.last_mut() {
                r.can_apply = can_apply;
            }
        }
        // ResistSwitch only runs on a scope, so an AO never gets this one.
        if (has_encoders || small_backlash) && rec.dec_algo_class == AlgoKind::ResistSwitch.class_name() {
            add(&mut self.recs, ev, RecKind::DecAlgo, "Try using Lowpass2 for Dec guiding".into(), true);
        }

        ev.push(GaEvent::Completed);
        self.save_results(&all, &mut *cx.profile, ev);
        ev.push(GaEvent::Debug("End of Guiding Assistant output....".into()));
    }

    // -------------------------------------------------------------- apply

    /// An Apply button (`OnRAMinMove`, `OnDecMinMove`, `OnDecAlgoChange`,
    /// `OnDecBacklash`). Changes pMount's algorithms or `TheScope()`'s
    /// backlash compensation, with PHD2's debug lines and parameter changes;
    /// the button turns off once applied. Returns whether it ran.
    pub fn apply<P: ProfileAccess>(&mut self, kind: RecKind, cx: ApplyCx<P>, ev: &mut Vec<GaEvent>) -> bool {
        let Some(idx) = self.recs.iter().position(|r| r.kind == kind && r.can_apply) else { return false };
        let ApplyCx { scope, ao, profile, algo } = cx;
        let is_ao = ao.is_some();
        let mut mev = Vec::new();
        let mut done = false;
        match kind {
            RecKind::RaMinMove | RecKind::DecMinMove => {
                let (axis, label, short, value) = if kind == RecKind::RaMinMove {
                    (Axis::X, "RA", "RA", self.ra_minmove_rec)
                } else {
                    (Axis::Y, "Declination", "Dec", self.dec_minmove_rec)
                };
                let Some(mount) = ao.or(scope.map(|s| &mut s.mount)) else { return false };
                let a = mount.algo_mut(axis);
                if a.min_move() >= 0.0 {
                    let err = a.set_min_move(value);
                    let class = a.class_name();
                    mount.save_algorithm(axis, profile);
                    if !err {
                        ev.push(GaEvent::Debug(format!(
                            "GuideAssistant changed {short}_MinMove to {}",
                            fmt_f(value, 2)
                        )));
                        // PHD2's trailing space kept.
                        let name = format!("{label} {class} MinMove ");
                        ev.push(GaEvent::ParamChange { name, value: value.into() });
                        done = true;
                    } else {
                        ev.push(GaEvent::Debug(format!("GuideAssistant could not change {short}_MinMove")));
                    }
                } else {
                    ev.push(GaEvent::Debug(format!(
                        "GuideAssistant logic flaw, {short} algorithm has no MinMove property"
                    )));
                }
            }
            RecKind::DecAlgo => {
                // Never offered for an AO.
                let Some(scope) = scope.filter(|_| !is_ao) else { return false };
                scope.set_guiding_enabled(false, algo, &mut mev);
                scope.mount.set_guide_algorithm(Axis::Y, AlgoKind::Lowpass2 as i32, profile, algo.now, &mut mev);
                ev.extend(mev.drain(..).map(GaEvent::Mount));
                ev.push(GaEvent::Debug("GuideAssistant changed Dec algo to Lowpass2".into()));
                if scope.mount.algo(Axis::Y).kind() != AlgoKind::Lowpass2 {
                    ev.push(GaEvent::Debug("GuideAssistant could not set Dec algo to Lowpass2".into()));
                    return true;
                }
                let new_aggr = 80.0;
                let a = scope.mount.algo_mut(Axis::Y);
                a.set_param("aggressiveness", new_aggr);
                a.set_param("minMove", self.dec_minmove_rec);
                scope.mount.save_algorithm(Axis::Y, profile);
                ev.push(GaEvent::Debug(format!(
                    "GuideAssistant set Lowpass2 aggressiveness = {}, min-move = {}",
                    fmt_f(new_aggr, 2),
                    fmt_f(self.dec_minmove_rec, 2)
                )));
                scope.set_guiding_enabled(true, algo, &mut mev);
                ev.extend(mev.drain(..).map(GaEvent::Mount));
                // "aggressivness" (sic), as PHD2 sends it.
                for (name, value) in [
                    ("Declination algorithm", Value::from("Lowpass2")),
                    ("Declination Lowpass2 aggressivness", new_aggr.into()),
                    ("Declination Lowpass2 MinMove", self.dec_minmove_rec.into()),
                ] {
                    ev.push(GaEvent::ParamChange { name: name.into(), value });
                }
                done = true;
            }
            RecKind::Blt => {
                let Some(scope) = scope else { return false };
                scope.set_backlash_pulse_width(self.backlash_recommended_ms, 0, 0, profile, &mut mev);
                scope.enable_backlash_comp(!is_ao, profile, &mut mev);
                ev.extend(mev.drain(..).map(GaEvent::Mount));
                done = true;
            }
            _ => return false,
        }
        if done {
            self.recs[idx].can_apply = false;
        }
        true
    }

    // -------------------------------------------------------- persistence

    /// `SaveGAResults`: the run under `/GA/<start time>`, then the history
    /// trimmed to [`MAX_GA_HISTORY`].
    fn save_results<P: ProfileAccess + ?Sized>(&self, all: &str, profile: &mut P, ev: &mut Vec<GaEvent>) {
        let prefix = format!("{HISTORY_GROUP}/{}", self.start_str);
        let mut d = self.details.clone();
        // `std::to_string(double)`: `%f`.
        d.dec_corrected_rms = fmt_f(self.dec_corrected_rms, 6);
        d.dec_lf_drift_rate = fmt_f(self.dec_drift_per_min, 6);
        d.rec_ra_min_move = fmt_f(self.ra_minmove_rec, 6);
        d.rec_dec_min_move = fmt_f(self.dec_minmove_rec, 6);
        for (key, value) in d.string_fields() {
            profile.set_str(&format!("{prefix}/{key}"), value);
        }
        // The measured backlash, kept while a recommendation from this or an
        // earlier run of the window stands.
        if self.backlash_recommended_ms > 0 {
            profile.set_str(&format!("{prefix}/BLT_pulse"), &self.backlash_ms.to_string());
        }
        profile.set_str(&format!("{prefix}/recommendations"), all);
        let fresh_blt = self.tool.as_ref().is_some_and(BacklashTool::is_graphable);
        if let Some(tool) = self.tool.as_ref().filter(|_| fresh_blt) {
            profile.set_i32(&format!("{prefix}/BLT_MsmtPulse"), tool.blt_msmt_pulse_size());
            profile.set_str(&format!("{prefix}/BLT_north"), &steps_str(tool.north_steps()));
            profile.set_str(&format!("{prefix}/BLT_South"), &steps_str(tool.south_steps()));
        }
        trim_history(profile, fresh_blt, ev);
    }

    /// Show a saved run (`OnGAReviewSelection`): its grids, its backlash
    /// graph and, when it has any, its recommendations with their Apply
    /// buttons, which then apply its min-moves. Not while measuring.
    pub fn review<P: ProfileAccess + ?Sized>(&mut self, profile: &P, timestamp: &str) {
        if self.measuring || self.measuring_backlash || self.wait.is_some() {
            return;
        }
        self.review_mode = true;
        let d = load_results(profile, timestamp);
        self.graph_enabled = !d.blt_north.is_empty();
        // `DisplayStaticRecommendations`, only when the run has some: else
        // the box keeps what it showed.
        if !d.recommendations.is_empty() {
            self.recs.clear();
            for line in d.recommendations.split('\n') {
                let Some((tag, what)) = line.split_once(':') else { continue };
                let Some(kind) = RecKind::ALL.into_iter().find(|k| k.tag() == tag) else { continue };
                let mut r = Recommendation::new(kind, what.to_string());
                match kind {
                    RecKind::RaMinMove => {
                        if let Ok(v) = d.rec_ra_min_move.parse() {
                            self.ra_minmove_rec = v;
                        }
                    }
                    RecKind::DecMinMove => {
                        if let Ok(v) = d.rec_dec_min_move.parse() {
                            self.dec_minmove_rec = v;
                        }
                    }
                    RecKind::Blt => {
                        // `wxAtoi`; the button and its pulse are still the
                        // last live run's recommendation, as in PHD2.
                        self.backlash_ms = d.blt_amount.parse().unwrap_or(0);
                        r.can_apply =
                            self.backlash_ms <= MAX_BACKLASH_COMP && self.backlash_recommended_ms > 100;
                    }
                    _ => {}
                }
                self.recs.push(r);
            }
        }
        self.details = d;
    }
}

/// The backlash recommendation for a completed test: `(text, recommended
/// pulse ms, small, Apply enabled)`. The pulse is the result rounded down to
/// 10 ms (at least 10); none is needed under 100 ms or with encoders; past
/// [`MAX_BACKLASH_COMP`] guide Dec one way, the way the drift goes. The
/// Apply stays enabled with encoders when the pulse is over 100 ms, as in
/// PHD2.
fn blt_recommendation(backlash_ms: i32, hp_encoders: bool, dec_drift_per_min: f64) -> (String, i32, bool, bool) {
    let recommended = if backlash_ms > 0 { (backlash_ms / 10 * 10).max(10) } else { 0 };
    let large = backlash_ms > MAX_BACKLASH_COMP;
    let mut small = false;
    let msg = if backlash_ms < 100 || hp_encoders {
        small = true;
        if hp_encoders {
            "Mount has absolute encoders, no compensation needed".to_string()
        } else {
            // Most likely a small measurement error.
            "Backlash is small, no compensation needed".to_string()
        }
    } else if backlash_ms <= MAX_BACKLASH_COMP {
        format!("Try starting with a Dec backlash compensation of {recommended} ms")
    } else {
        format!(
            "Backlash is >= {backlash_ms} ms; you may need to guide in only one Dec direction (currently {})",
            if dec_drift_per_min >= 0.0 { "South" } else { "North" }
        )
    };
    (msg, recommended, small, !large && recommended > 100)
}

/// A backlash leg as saved: `%0.1f,` per position, then PHD2 cuts two
/// characters — the comma and the last digit of the last value (quirk kept).
fn steps_str(steps: &[f64]) -> String {
    let mut s: String = steps.iter().map(|v| format!("{},", fmt_f(*v, 1))).collect();
    s.truncate(s.len().saturating_sub(2));
    s
}

/// `TrimGAHistory`: past [`MAX_GA_HISTORY`] runs the oldest goes — or the
/// next oldest, when the oldest holds the only backlash measurement and this
/// run made none.
fn trim_history<P: ProfileAccess + ?Sized>(profile: &mut P, fresh_blt: bool, ev: &mut Vec<GaEvent>) {
    let stamps = profile.group_names(HISTORY_GROUP);
    let with_blt: Vec<usize> = (0..stamps.len())
        .filter(|&i| !profile.get_str(&format!("{HISTORY_GROUP}/{}/BLT_north", stamps[i]), "").is_empty())
        .collect();
    let blt_count = with_blt.len();
    let oldest_blt = with_blt.first().copied();
    if stamps.len() > MAX_GA_HISTORY {
        let target = if fresh_blt
            || blt_count == 0
            || oldest_blt.is_some_and(|i| i > 0)
            || blt_count > 1
            || blt_count == stamps.len()
        {
            &stamps[0]
        } else {
            &stamps[1]
        };
        profile.delete_group(&format!("{HISTORY_GROUP}/{target}"));
        ev.push(GaEvent::Debug(format!("GA-History: removed entry for {target}")));
    }
}

/// The saved runs' start times, oldest first (the "Review previous" menu).
pub fn history<P: ProfileAccess + ?Sized>(profile: &P) -> Vec<String> {
    profile.group_names(HISTORY_GROUP)
}

/// A saved run (`LoadGAResults`); empty fields for what it lacks.
pub fn load_results<P: ProfileAccess + ?Sized>(profile: &P, timestamp: &str) -> GaDetails {
    let prefix = format!("{HISTORY_GROUP}/{timestamp}");
    let mut d = GaDetails::default();
    for (key, value) in d.string_fields() {
        *value = profile.get_str(&format!("{prefix}/{key}"), "");
    }
    d.blt_amount = profile.get_str(&format!("{prefix}/BLT_pulse"), "");
    d.recommendations = profile.get_str(&format!("{prefix}/recommendations"), "");
    d.blt_msmt_pulse = profile.get_i32(&format!("{prefix}/BLT_MsmtPulse"), -1);
    // PHD2 reads `BLT_North`, which its case-blind config finds under the
    // `BLT_north` it writes.
    let north = profile.get_str(&format!("{prefix}/BLT_north"), "");
    let south = profile.get_str(&format!("{prefix}/BLT_South"), "");
    if !north.is_empty() && !south.is_empty() {
        // A token that does not parse repeats the previous value, as the
        // unchanged `ptVal` does in PHD2.
        let parse = |s: &str| {
            let mut pt = 0.0;
            s.split(',')
                .map(|t| {
                    if let Ok(v) = t.parse() {
                        pt = v;
                    }
                    pt
                })
                .collect::<Vec<f64>>()
        };
        d.blt_north = parse(&north);
        d.blt_south = parse(&south);
    }
    d
}

/// The latest saved run, with its start time.
pub fn last_run<P: ProfileAccess + ?Sized>(profile: &P) -> Option<(String, GaDetails)> {
    let ts = history(profile).pop()?;
    let d = load_results(profile, &ts);
    Some((ts, d))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::TimeZone;

    use super::*;
    use crate::engine::algo::MountClass;
    use crate::engine::calibration::save_calibration_details;
    use crate::engine::log::cfmt::fmt_gp;
    use crate::engine::traits::Dir;

    /// A profile that keeps the value types apart and knows its groups.
    #[derive(Default)]
    struct Mem(BTreeMap<String, V>);

    #[derive(Clone, Debug, PartialEq)]
    enum V {
        F(f64),
        I(i32),
        S(String),
    }

    impl ProfileAccess for Mem {
        fn has(&self, key: &str) -> bool {
            self.0.contains_key(key)
        }
        fn get_f64(&self, key: &str, default: f64) -> f64 {
            match self.0.get(key) {
                Some(V::F(v)) => *v,
                Some(V::I(v)) => f64::from(*v),
                _ => default,
            }
        }
        fn get_i32(&self, key: &str, default: i32) -> i32 {
            if let Some(V::I(v)) = self.0.get(key) { *v } else { default }
        }
        fn get_str(&self, key: &str, default: &str) -> String {
            if let Some(V::S(v)) = self.0.get(key) { v.clone() } else { default.to_string() }
        }
        fn set_f64(&mut self, key: &str, value: f64) {
            self.0.insert(key.into(), V::F(value));
        }
        fn set_i32(&mut self, key: &str, value: i32) {
            self.0.insert(key.into(), V::I(value));
        }
        fn set_str(&mut self, key: &str, value: &str) {
            self.0.insert(key.into(), V::S(value.into()));
        }
        fn group_names(&self, group: &str) -> Vec<String> {
            let base = format!("{group}/");
            let names: BTreeSet<String> = self
                .0
                .keys()
                .filter_map(|k| k.strip_prefix(&base)?.split_once('/').map(|(g, _)| g.to_string()))
                .collect();
            names.into_iter().collect()
        }
        fn delete_group(&mut self, group: &str) {
            let base = format!("{group}/");
            self.0.retain(|k, _| !k.starts_with(&base));
        }
    }

    fn info(ra: f64, dec: f64, time: f64, snr: f64, mass: f64) -> GuideStepInfo {
        GuideStepInfo {
            move_options: MoveOpts::GUIDE_STEP,
            frame_number: 1,
            time,
            is_ao: false,
            camera_offset: Point::default(),
            mount_offset: Point::new(ra, dec),
            guide_distance_ra: 0.0,
            guide_distance_dec: 0.0,
            duration_ra: 0,
            direction_ra: Dir::West,
            duration_dec: 0,
            direction_dec: Dir::North,
            ra_limited: false,
            dec_limited: false,
            ao_pos: (0, 0),
            star_mass: mass,
            star_snr: snr,
            star_hfd: 2.0,
            avg_dist: 0.0,
            star_error: 0,
        }
    }

    fn rec_env() -> RecEnv {
        RecEnv {
            pixel_scale: 1.0,
            focal_length: 400,
            pixel_size: 3.8,
            binning: 1,
            max_binning: 1,
            hp_encoders: false,
            ra_min_move: 0.2,
            dec_min_move: 0.2,
            dec_algo_class: "ResistSwitch",
            star_hfd: 2.0,
        }
    }

    const T0: i64 = 1_762_891_200_000;
    const OUTPUTS: GuideOutputs = GuideOutputs { primary: Some(true), secondary: None };

    fn wall() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 11, 21, 0, 0).unwrap()
    }

    fn env(t: i64) -> StepEnv<'static> {
        StepEnv { now_ms: T0 + 1000 * t, exposure_ms: 1000, pixel_scale: 1.0, dec_rad: None, ao_scope_cal: None }
    }

    fn started(multistar: bool, exposure_ms: i32, ev: &mut Vec<GaEvent>) -> GuidingAssistant {
        let mut ga = GuidingAssistant::new(None, false, multistar, VarDelayCfg::default(), true, ev);
        assert!(ga.start(true, exposure_ms, OUTPUTS, wall(), T0, ev));
        ga
    }

    fn cx<'a>(rec: &'a RecEnv, profile: &'a mut Mem) -> GaCx<'a, Mem> {
        GaCx {
            rec,
            profile,
            scope: None,
            frame_size: (1000, 1000),
            max_move_px: 50,
            now_ms: 0,
            star: None,
            use_subframes: false,
        }
    }

    /// `TheScope()` on `p`: its Dec guide mode, compensation pulse (ms) and
    /// stored calibration details.
    fn scope_on(p: &mut Mem, mode: DecGuideMode, blc_pulse: i32, details: &CalibrationDetails) -> Scope {
        let mut mev = Vec::new();
        let mut scope = Scope::new(p, false, 0.0, &mut mev);
        scope.set_dec_guide_mode(mode as i32, p, &mut mev);
        scope.set_backlash_pulse_width(blc_pulse, 0, 0, p, &mut mev);
        save_calibration_details(p, MountClass::Scope, details);
        scope
    }

    fn debug_lines(ev: &[GaEvent]) -> Vec<&str> {
        ev.iter().filter_map(|e| if let GaEvent::Debug(s) = e { Some(s.as_str()) } else { None }).collect()
    }

    fn results(ev: &[GaEvent]) -> Vec<&str> {
        ev.iter().filter_map(|e| if let GaEvent::Result(s) = e { Some(s.as_str()) } else { None }).collect()
    }

    // ------------------------------------------------------ PHD2 goldens

    fn noise(k: i32) -> f64 {
        f64::from((k * 37 + 11) % 17 - 8) / 8.0
    }

    fn tri(t: f64, period: f64) -> f64 {
        let ph = (t % period) / period;
        if ph < 0.5 { 4.0 * ph - 1.0 } else { 3.0 - 4.0 * ph }
    }

    /// One measurement set of the C++ harness.
    #[derive(Clone, Copy)]
    struct Scenario {
        name: &'static str,
        exp_ms: i32,
        step_s: i32,
        end_s: i32,
        scale: f64,
        dec_rad: Option<f64>,
        multistar: bool,
        encoders: bool,
        dec_class: &'static str,
        hfd: f64,
        snr: f64,
        focal: i32,
        pixel_size: f64,
        binning: u32,
        max_binning: u32,
        mode: DecGuideMode,
        issue: CalibrationIssue,
        dec_steps: &'static [(f64, f64)],
        dec_step_count: i32,
        dec: fn(f64, i32) -> f64,
        ra: fn(f64, i32) -> f64,
    }

    /// The harness' run: Start, the samples, `MakeRecommendations`, all of
    /// it printed as the harness prints it.
    fn run_scenario(c: &Scenario) -> (Vec<String>, GuidingAssistant) {
        let mut ev = Vec::new();
        let mut ga = started(c.multistar, c.exp_ms, &mut ev);
        for (k, t) in (0..=c.end_s).step_by(c.step_s as usize).enumerate() {
            let k = k as i32;
            let tf = f64::from(t);
            let i = info((c.ra)(tf, k), (c.dec)(tf, k), tf + 0.123, c.snr + noise(k), 12000.0 + 100.0 * noise(k + 5));
            let env = StepEnv {
                now_ms: T0 + 1000 * i64::from(t) + 250,
                exposure_ms: c.exp_ms,
                pixel_scale: c.scale,
                dec_rad: c.dec_rad,
                ao_scope_cal: None,
            };
            ga.guide_step(&i, &env, &mut ev);
        }
        let mut out = vec![format!("== {}", c.name)];
        out.extend(debug_lines(&ev).iter().map(|s| format!("dbg0 {s}")));
        let rec = RecEnv {
            pixel_scale: c.scale,
            focal_length: c.focal,
            pixel_size: c.pixel_size,
            binning: c.binning,
            max_binning: c.max_binning,
            hp_encoders: c.encoders,
            ra_min_move: 0.2,
            dec_min_move: 0.2,
            dec_algo_class: c.dec_class,
            star_hfd: c.hfd,
        };
        let mut profile = Mem::default();
        let details = CalibrationDetails {
            last_issue: c.issue,
            dec_steps: c.dec_steps.iter().map(|&(x, y)| Point::new(x, y)).collect(),
            dec_step_count: c.dec_step_count,
            ..Default::default()
        };
        let mut scope = scope_on(&mut profile, c.mode, 0, &details);
        let mut ev = Vec::new();
        ga.make_recommendations(&mut GaCx { scope: Some(&mut scope), ..cx(&rec, &mut profile) }, &mut ev);

        let d = ga.details();
        for (name, v) in [
            ("timestamp", &d.timestamp),
            ("exposure", &d.exposure_time),
            ("snr", &d.snr),
            ("mass", &d.star_mass),
            ("elapsed", &d.elapsed_time),
            ("count", &d.sample_count),
            ("ra_rms", &d.ra_hpf_rms),
            ("dec_rms", &d.dec_hpf_rms),
            ("total_rms", &d.total_hpf_rms),
            ("ra_peak", &d.ra_peak),
            ("dec_peak", &d.dec_peak),
            ("ra_pkpk", &d.ra_peak_peak),
            ("ra_drift", &d.ra_drift_rate),
            ("ra_peak_drift", &d.ra_max_drift_rate),
            ("ra_drift_exp", &d.drift_limiting_exposure),
            ("dec_drift", &d.dec_drift_rate),
            ("pae", &d.pa_error),
        ] {
            out.push(format!("cell {name}=[{v}]"));
        }
        let g = |v: f64| fmt_gp(v, 17);
        out.push(format!(
            "num ra_mm={} dec_mm={} min_exp={} max_exp={} pae={} drift={} maxRateRA={}",
            g(ga.ra_minmove_rec),
            g(ga.dec_minmove_rec),
            g(ga.min_exp_rec),
            g(ga.max_exp_rec),
            g(ga.alignment_error),
            g(ga.dec_drift_per_min),
            g(ga.max_rate_ra)
        ));
        for r in ga.recommendations() {
            let btn = r.kind.has_apply();
            out.push(format!("rec [{}] btn={} en={}", r.text, btn as i32, (btn && r.can_apply) as i32));
        }
        out.extend(debug_lines(&ev).iter().map(|s| format!("dbg {s}")));
        out.extend(results(&ev).iter().map(|s| format!("glog {}", s.trim_end_matches('\n'))));
        out.push(format!("completed {}", ev.iter().filter(|e| **e == GaEvent::Completed).count()));
        // The harness' profile holds the runs only.
        for (k, v) in profile.0.iter().filter(|(k, _)| k.starts_with("/GA/")) {
            let v = match v {
                V::S(s) => s.clone(),
                V::I(i) => format!("(int){i}"),
                V::F(f) => format!("(double){f}"),
            };
            out.push(format!("prof {k}=[{v}]"));
        }
        // Multi-line profile values print as several lines in the harness.
        (out.join("\n").split('\n').map(str::to_string).collect(), ga)
    }

    const PA: Scenario = Scenario {
        name: "pa",
        exp_ms: 2000,
        step_s: 2,
        end_s: 140,
        scale: 2.0,
        dec_rad: Some(0.0),
        multistar: false,
        encoders: false,
        dec_class: "ResistSwitch",
        hfd: 2.5,
        snr: 25.0,
        focal: 400,
        pixel_size: 3.8,
        binning: 1,
        max_binning: 2,
        mode: DecGuideMode::Auto,
        issue: CalibrationIssue::None,
        dec_steps: &[],
        dec_step_count: 0,
        dec: |t, k| t / 60.0 + 0.05 * noise(k),
        ra: |t, k| 0.4 * tri(t, 40.0) + 0.1 * noise(k + 3),
    };

    const SCENARIOS: [Scenario; 5] = [
        // Dec 1 px/min plus noise at 2″/px, Dec 0: PA ≈ 7.6′, a short run.
        PA,
        // ±0.3 px on Dec, unknown declination, a faint star.
        Scenario {
            name: "sigma",
            exp_ms: 1000,
            step_s: 1,
            end_s: 130,
            scale: 1.0,
            dec_rad: None,
            hfd: 2.0,
            snr: 8.0,
            max_binning: 1,
            dec: |_, k| if k % 2 != 0 { -0.3 } else { 0.3 },
            ra: |_, k| if k % 2 != 0 { -0.2 } else { 0.2 },
            ..PA
        },
        // A long run (sliding windows), multi-star, encoders, 1.6″/px, soft
        // focus, ResistSwitch on Dec.
        Scenario {
            name: "long",
            end_s: 400,
            scale: 1.6,
            dec_rad: Some(0.5),
            multistar: true,
            encoders: true,
            hfd: 5.0,
            snr: 30.0,
            focal: 500,
            max_binning: 1,
            dec: |t, k| 0.5 * t / 60.0 + 0.4 * noise(k),
            ra: |t, k| 1.5 * tri(t, 120.0) + 0.2 * noise(k + 1),
            ..PA
        },
        // Noisy at 3″/px: the smart defaults; a prior calibration alert;
        // Dec North, so likely backlash, and PA over 10′.
        Scenario {
            name: "sanity",
            exp_ms: 3000,
            step_s: 3,
            end_s: 150,
            scale: 3.0,
            dec_rad: Some(0.2),
            dec_class: "Lowpass2",
            hfd: 2.0,
            snr: 50.0,
            pixel_size: 5.8,
            max_binning: 1,
            mode: DecGuideMode::North,
            issue: CalibrationIssue::Steps,
            dec: |t, k| 2.0 * t / 60.0 + 2.0 * noise(k),
            ra: |_, k| 0.5 * noise(k + 2),
            ..PA
        },
        // 0.4″/px unbinned; a calibration with little south movement
        // (likely backlash) hides a PA of 6.4′.
        Scenario {
            name: "bin",
            exp_ms: 500,
            step_s: 1,
            end_s: 125,
            scale: 0.4,
            dec_rad: Some(-0.3),
            hfd: 6.0,
            snr: 40.0,
            focal: 1000,
            pixel_size: 2.0,
            dec_steps: &[(0.0, 0.0), (0.0, 5.0), (0.0, 10.0), (0.0, 9.5)],
            dec_step_count: 3,
            dec: |t, k| 4.0 * t / 60.0 + 0.1 * noise(k),
            ra: |t, k| 3.0 * tri(t, 20.0) + 0.05 * noise(k),
            ..PA
        },
    ];

    /// The cells, numbers, recommendations, debug and guide-log lines and
    /// profile entries of PHD2's own `UpdateInfo` / `MakeRecommendations` /
    /// `SaveGAResults` (with the real guiding_stats.cpp, built against wx
    /// 3.2.11 base) on the same five measurement sets
    /// (target/scratch/harness/ga, not committed).
    #[test]
    fn recommendations_match_phd2() {
        let golden: Vec<&str> = include_str!("testdata/assistant_phd2.txt").lines().collect();
        let mut ours = Vec::new();
        for c in &SCENARIOS {
            ours.extend(run_scenario(c).0);
        }
        for (i, (a, b)) in ours.iter().zip(&golden).enumerate() {
            assert_eq!(a, b, "line {}", i + 1);
        }
        assert_eq!(ours.len(), golden.len());
    }

    #[test]
    fn min_move_from_a_0_3_px_series() {
        // σ ≈ 0.3 px at 1″/px: 1.28 σ, rounded up to 0.40; RA 65 % of it.
        let (_, ga) = run_scenario(&SCENARIOS[1]);
        assert_eq!(ga.min_move_recs(), (0.26, 0.4));
        // Multi-star takes 10 % off the estimate first.
        let (_, ga) = run_scenario(&Scenario { multistar: true, ..SCENARIOS[1] });
        assert_eq!(ga.min_move_recs(), (0.35000000000000003 * 0.65, 0.35000000000000003));
        // Over 1.25″ the estimate is distrusted: the profile wizard's
        // default for 400 mm and 3.8 µm.
        let (_, ga) = run_scenario(&Scenario { scale: 4.0, ..SCENARIOS[1] });
        let smart = smart_default_min_move_for(400, 3.8, 1);
        assert_eq!(ga.min_move_recs(), (smart * 0.65, smart));
    }

    #[test]
    fn pa_error_from_one_px_per_minute() {
        // Dec drifts 1 px/min at 2″/px: 3.8197 × 1 × 2 = 7.6394′ at Dec 0,
        // twice that at Dec 60°.
        for (dec, err) in [(0.0, 7.6394), (60f64.to_radians(), 2.0 * 7.6394)] {
            let mut ev = Vec::new();
            let mut ga = started(false, 2000, &mut ev);
            for t in (0..=150).step_by(2) {
                let env = StepEnv { pixel_scale: 2.0, dec_rad: Some(dec), ..env(t) };
                ga.guide_step(&info(0.0, t as f64 / 60.0, t as f64, 20.0, 1000.0), &env, &mut ev);
            }
            assert!((ga.dec_drift_px_per_min() - 1.0).abs() < 1e-12);
            assert!((ga.pa_error_arcmin() - err).abs() < 1e-9);
            assert_eq!(ga.details().dec_drift_rate, "  1.00 px/min (  2.00 arc-sec/min )");
        }
    }

    #[test]
    fn only_graphed_steps_count() {
        let mut ev = Vec::new();
        let mut ga = started(false, 1000, &mut ev);
        let mut bump = info(1.0, 1.0, 1.0, 10.0, 100.0);
        bump.move_options = MoveOpts::AO_BUMP;
        ga.guide_step(&bump, &env(0), &mut ev);
        assert_eq!(ga.details().sample_count, "");
        ga.guide_step(&info(1.0, 1.0, 1.0, 10.0, 100.0), &env(0), &mut ev);
        assert_eq!(ga.details().sample_count, "1");
    }

    #[test]
    fn ao_steps_go_through_the_scope_calibration() {
        // Scope axes turned 90°: camera +x is mount −Dec.
        let cal = Calibration {
            x_angle: std::f64::consts::FRAC_PI_2,
            y_angle: std::f64::consts::PI,
            is_valid: true,
            ..Default::default()
        };
        let mut ev = Vec::new();
        let mut ga = started(false, 1000, &mut ev);
        let mut step = info(5.0, 5.0, 1.0, 10.0, 100.0);
        step.camera_offset = Point::new(1.0, 0.0);
        ev.clear();
        ga.guide_step(&step, &StepEnv { ao_scope_cal: Some(&cal), ..env(1) }, &mut ev);
        let lines = debug_lines(&ev);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.starts_with("CameraToMount -- ")));
        let e = ga.dec_axis_stats.last_entry();
        assert_eq!(e.star_pos, -1.0);
        assert!(ga.ra_axis_stats.last_entry().star_pos.abs() < 1e-15);
    }

    // ---------------------------------------------- the other triggers

    #[test]
    fn backlash_recommendations() {
        let small = "Backlash is small, no compensation needed".to_string();
        assert_eq!(blt_recommendation(50, false, 1.0), (small.clone(), 50, true, false));
        assert_eq!(blt_recommendation(0, false, 1.0), (small, 0, true, false));
        let try_ = |ms| format!("Try starting with a Dec backlash compensation of {ms} ms");
        assert_eq!(blt_recommendation(1234, false, 1.0), (try_(1230), 1230, false, true));
        // 105 ms rounds down to 100, which the Apply needs to exceed.
        assert_eq!(blt_recommendation(105, false, 1.0), (try_(100), 100, false, false));
        let one_way = "Backlash is >= 4000 ms; you may need to guide in only one Dec direction (currently North)";
        assert_eq!(blt_recommendation(4000, false, -0.5), (one_way.to_string(), 4000, false, false));
        assert!(blt_recommendation(3001, false, 0.0).0.ends_with("(currently South)"));
        // Encoders: no compensation needed, yet the Apply stays on (PHD2).
        let enc = "Mount has absolute encoders, no compensation needed".to_string();
        assert_eq!(blt_recommendation(500, true, 0.0), (enc, 500, true, true));
    }

    /// A short run finished on `rec` and a scope in Dec guide mode `mode`
    /// with a `blc_pulse` ms compensation pulse and `details`, after `tweak`.
    fn recommend_on(
        rec: &RecEnv,
        mode: DecGuideMode,
        blc_pulse: i32,
        details: &CalibrationDetails,
        tweak: impl FnOnce(&mut GuidingAssistant),
    ) -> (GuidingAssistant, Vec<GaEvent>) {
        let mut ev = Vec::new();
        let mut ga = started(false, 1000, &mut ev);
        for t in 0..10 {
            let k = t as i32;
            ga.guide_step(&info(0.1 * noise(k), 0.1 * noise(k + 4), t as f64, 20.0, 1000.0), &env(t), &mut ev);
        }
        tweak(&mut ga);
        let mut profile = Mem::default();
        let mut scope = scope_on(&mut profile, mode, blc_pulse, details);
        let mut ev = Vec::new();
        ga.make_recommendations(&mut GaCx { scope: Some(&mut scope), ..cx(rec, &mut profile) }, &mut ev);
        (ga, ev)
    }

    /// The same on a plain scope: Auto, no compensation, no calibration.
    fn recommend(rec: &RecEnv, tweak: impl FnOnce(&mut GuidingAssistant)) -> (GuidingAssistant, Vec<GaEvent>) {
        recommend_on(rec, DecGuideMode::Auto, 0, &CalibrationDetails::default(), tweak)
    }

    fn kinds(ga: &GuidingAssistant) -> Vec<RecKind> {
        ga.recommendations().iter().map(|r| r.kind).collect()
    }

    #[test]
    fn polar_alignment_thresholds() {
        let pa = |err: f64, mode: DecGuideMode, blc_pulse: i32| {
            let (ga, _) = recommend_on(&rec_env(), mode, blc_pulse, &CalibrationDetails::default(), |ga| {
                ga.alignment_error = err
            });
            ga.recommendations().iter().find(|r| r.kind == RecKind::Pa).map(|r| r.text.clone())
        };
        const OVER_5: &str = "Polar alignment error > 5 arc-min; that could probably be improved.";
        const OVER_10: &str =
            "Polar alignment error > 10 arc-min; try using the Drift Align tool to improve alignment.";
        let auto = DecGuideMode::Auto;
        assert_eq!(pa(5.0, auto, 0), None);
        assert_eq!(pa(7.0, auto, 0).as_deref(), Some(OVER_5));
        assert_eq!(pa(10.0, auto, 0).as_deref(), Some(OVER_10));
        // A compensation pulse past 3000 ms means large backlash: nothing
        // up to 10′.
        assert_eq!(pa(7.0, auto, 3500), None);
        assert_eq!(pa(10.0, auto, 3500), None);
        assert_eq!(pa(10.5, auto, 3500).as_deref(), Some(OVER_10));
        assert_eq!(pa(9.0, DecGuideMode::South, 0), None);
    }

    #[test]
    fn calibration_snr_focus_and_min_move_triggers() {
        let cal = CalibrationDetails { last_issue: CalibrationIssue::Rates, ..Default::default() };
        let (ga, ev) = recommend_on(&rec_env(), DecGuideMode::Auto, 0, &cal, |_| {});
        assert_eq!(ga.recommendations()[1].text, "Consider re-doing your calibration (Prior alert)");
        assert!(results(&ev).contains(&"Recommendation: Consider re-doing your calibration (Prior alert)\n"));
        // An SNR of 20: no star advice; under 10 there is.
        assert!(!kinds(&ga).contains(&RecKind::Star));
        let (ga, _) = recommend(&rec_env(), |ga| ga.sum_snr = 9.9 * 10.0);
        assert!(kinds(&ga).contains(&RecKind::Star));
        // Focus: an HFD over 4.5 only above 1″/px.
        let (ga, _) = recommend(&RecEnv { star_hfd: 4.6, pixel_scale: 1.1, ..rec_env() }, |_| {});
        assert!(kinds(&ga).contains(&RecKind::StarHfd));
        let (ga, _) = recommend(&RecEnv { star_hfd: 4.6, ..rec_env() }, |_| {});
        assert!(!kinds(&ga).contains(&RecKind::StarHfd));
        // An algorithm without a min-move gets no min-move advice; nor does
        // Lowpass2 get the Lowpass2 advice.
        let rec = RecEnv { ra_min_move: -1.0, hp_encoders: true, dec_algo_class: "Lowpass2", ..rec_env() };
        let (ga, _) = recommend(&rec, |_| {});
        assert_eq!(kinds(&ga), [RecKind::Exp, RecKind::DecMinMove]);
    }

    #[test]
    fn exposure_range_rules() {
        let range = |max_rate: f64, enc: bool| {
            let (ga, _) = recommend(&RecEnv { hp_encoders: enc, ..rec_env() }, |ga| ga.max_rate_ra = max_rate);
            (ga.hpf_ra_stats.sigma(), ga.exposure_range(), ga.recommendations()[0].text.clone())
        };
        // No RA rate: the ideal 2 to 4 s.
        let (rarms, r, text) = range(0.0, false);
        assert_eq!((r, text.as_str()), ((2.0, 4.0), "Use exposure times in the range of 2.0s to 4.0s"));
        // A fast drift: ceil(rarms / rate / 0.5) · 0.5 under 1 s, so 1 to 3 s.
        assert_eq!((rarms / 1.0 / 0.5).ceil() * 0.5, 0.5);
        assert_eq!(range(1.0, false).1, (1.0, 3.0));
        // Drift-limited at 1.5 s: 1.5 to 3.5 s.
        let rate = rarms / 1.4;
        assert_eq!(range(rate, false).1, (1.5, 3.5));
        // A slow drift past the ideal max: 2 to 4 s.
        assert_eq!(range(rarms / 5.0, false).1, (2.0, 4.0));
        // Encoders: the text names the 4–8 s ideal range.
        let (_, _, text) = range(0.0, true);
        assert_eq!(text, "Use exposure times from 4.0s to 8.0s and variable delay feature to control guide cadence");
    }

    // --------------------------------------------------------- the flow

    #[test]
    fn open_start_and_stop_events() {
        let mut ev = Vec::new();
        let var = VarDelayCfg { enabled: true, short_ms: 1000, long_ms: 10000 };
        let mut ga = GuidingAssistant::new(None, false, true, var, false, &mut ev);
        assert_eq!(ev, [GaEvent::VariableDelay(VarDelayCfg { enabled: false, ..var })]);
        assert_eq!(ga.instructions(), INSTR_NO_STAR);
        assert_eq!(ga.measure_backlash(), (false, false)); // no scope
        assert!(!ga.start(false, 1000, OUTPUTS, wall(), T0, &mut ev));
        ga.app_state_changed(true, &mut ev);
        assert_eq!(ga.instructions(), INSTR_START_READY);
        ev.clear();
        let outputs = GuideOutputs { primary: Some(true), secondary: Some(false) };
        assert!(ga.start(true, 1000, outputs, wall(), T0, &mut ev));
        assert_eq!(
            ev,
            [
                GaEvent::MultiStar(false),
                GaEvent::Debug("GuidingAssistant: Disabling guide output".into()),
                GaEvent::GuideOutputs(GuideOutputs { primary: Some(false), secondary: Some(false) }),
            ]
        );
        assert_eq!((ga.status(), ga.instructions()), ("Measuring...", INSTR_MEASURING));
        // Guiding stops: the run ends, everything goes back, no results.
        ev.clear();
        ga.app_state_changed(false, &mut ev);
        assert_eq!(
            ev,
            [
                GaEvent::Debug("GuidingAssistant: Re-enabling guide output (1, 0)".into()),
                GaEvent::GuideOutputs(outputs),
                GaEvent::MultiStar(true),
                GaEvent::VariableDelay(var),
            ]
        );
        assert_eq!((ga.state(), ga.instructions()), (DialogState::Stopped, INSTR_STOPPED));
        assert!(ga.recommendations().is_empty());
        // Once restored, closing restores nothing.
        ev.clear();
        ga.close(&mut ev);
        assert!(ev.is_empty());
    }

    #[test]
    fn stop_waits_for_two_minutes() {
        let mut ev = Vec::new();
        let mut ga = started(false, 1000, &mut ev);
        for t in 0..=100 {
            ga.guide_step(&info(0.0, 0.01 * t as f64, t as f64, 20.0, 1000.0), &env(t), &mut ev);
        }
        let rec = rec_env();
        let mut profile = Mem::default();
        ev.clear();
        ga.stop(&mut cx(&rec, &mut profile), &mut ev);
        assert_eq!(ga.wait(), Some((20, false)));
        assert!(ga.is_measuring() && ev.is_empty());
        // Samples still count during the countdown.
        ga.guide_step(&info(0.0, 1.01, 101.0, 20.0, 1000.0), &env(101), &mut ev);
        assert_eq!(ga.details().sample_count, "102");
        for _ in 0..19 {
            ga.tick(&mut cx(&rec, &mut profile), &mut ev);
        }
        assert_eq!(ga.wait(), Some((1, false)));
        assert!(ev.is_empty());
        ga.tick(&mut cx(&rec, &mut profile), &mut ev);
        assert_eq!(ga.wait(), None);
        assert!(ev.contains(&GaEvent::Completed));
        assert_eq!(ga.state(), DialogState::Stopped);
        assert_eq!(history(&profile), ["2026-11-11 21:00:00"]);

        // Cancel during the countdown: no results; Ctrl-Cancel takes them.
        for accept in [false, true] {
            let mut ev = Vec::new();
            let mut ga = started(false, 1000, &mut ev);
            let mut profile = Mem::default();
            ga.stop(&mut cx(&rec, &mut profile), &mut ev);
            ev.clear();
            ga.end_wait(accept, &mut cx(&rec, &mut profile), &mut ev);
            assert_eq!(ev.contains(&GaEvent::Completed), accept);
            assert_eq!(history(&profile).len(), accept as usize);
            assert!(ev.contains(&GaEvent::MultiStar(false)));
        }
    }

    #[test]
    fn the_scope_is_read_when_the_recommendations_are_made() {
        // Bogus stored guide speeds: `LoadCalibrationDetails`' once-per-
        // session check clears them, with its two lines, when the
        // recommendations are made — not at the Stop that opens the
        // countdown.
        let mut profile = Mem::default();
        let details = CalibrationDetails { ra_guide_speed: 0.01, dec_guide_speed: 0.002, ..Default::default() };
        let mut scope = scope_on(&mut profile, DecGuideMode::Auto, 0, &details);
        let rec = rec_env();
        let mut ev = Vec::new();
        let mut ga = started(false, 1000, &mut ev);
        ev.clear();
        ga.stop(&mut GaCx { scope: Some(&mut scope), ..cx(&rec, &mut profile) }, &mut ev);
        assert!(ev.is_empty() && ga.wait().is_some());
        ga.end_wait(true, &mut GaCx { scope: Some(&mut scope), ..cx(&rec, &mut profile) }, &mut ev);
        assert_eq!(
            ev[..2],
            [
                GaEvent::Mount(MountEvent::Debug("Invalid mount guide speeds: RA: 0.0100, Dec: 0.0020".into())),
                GaEvent::Mount(MountEvent::Debug("Bogus guide speeds cleared in LoadCalibrationDetails".into())),
            ]
        );
        assert_eq!(profile.get_f64("/scope/calibration/ra_guide_rate", 0.0), -1.0);
    }

    #[test]
    fn a_gap_in_a_long_run_ends_the_windows() {
        // 0–100 s, nothing until 200 s, then to 300 s: the window from
        // 100 s ends at 220 s and rewinds to 100 s again, where PHD2 loops
        // for ever.
        let mut ev = Vec::new();
        let mut ga = started(false, 1000, &mut ev);
        for t in (0..=100).step_by(2).chain((200..=300).step_by(2)) {
            ga.guide_step(&info(0.0, 0.2 * noise(t as i32), t as f64, 20.0, 1000.0), &env(t), &mut ev);
        }
        let mut ev = Vec::new();
        let (ra, dec) = ga.get_min_move_recs(&rec_env(), &mut ev);
        assert!(ra > 0.0 && dec > 0.0);
        let windows: Vec<&str> = debug_lines(&ev)
            .into_iter()
            .filter(|l| l.starts_with("GA long series"))
            .map(|l| l.split(", Uncorrected").next().unwrap())
            .collect();
        assert_eq!(
            windows,
            [
                "GA long series, window start=0, window end=200",
                "GA long series, window start=100, window end=220",
            ]
        );
    }

    // ------------------------------------------------- the backlash test

    /// The backlash tool's "valid" mount (`backlash.rs`): 0.008 px/ms, 10 px
    /// of backlash, one frame a second.
    #[test]
    fn backlash_test_after_the_sampling() {
        let cal = Calibration {
            x_angle: 0.0,
            y_angle: std::f64::consts::FRAC_PI_2,
            x_rate: 0.01,
            y_rate: 0.01,
            is_valid: true,
            ..Default::default()
        };
        let mut profile = Mem::default();
        let mut mev = Vec::new();
        let mut scope = Scope::new(&mut profile, false, 0.0, &mut mev);
        scope.set_calibration(&cal, "2026-11-11 20:00:00", &mut profile, &mut mev);
        scope.set_calibration_duration(500, &mut profile, &mut mev);
        scope.set_backlash_pulse_width(300, 0, 0, &mut profile, &mut mev);
        scope.enable_backlash_comp(true, &mut profile, &mut mev);
        let mut ev = Vec::new();
        let mut ga = GuidingAssistant::new(Some(&cal), false, false, VarDelayCfg::default(), true, &mut ev);
        assert_eq!(ga.measure_backlash(), (true, true));
        assert!(ga.start(true, 1000, OUTPUTS, wall(), T0, &mut ev));
        for t in 0..=130 {
            let k = t as i32;
            let env = StepEnv { pixel_scale: 2.0, ..env(t) };
            // No Dec drift: the tool's run is backlash.rs' own.
            ga.guide_step(&info(0.05 * noise(k), 0.0, t as f64, 20.0, 1000.0), &env, &mut ev);
        }
        let rec = RecEnv { pixel_scale: 2.0, ..rec_env() };
        let mut now_ms = T0 + 131_000;
        let (mut y, mut slack) = (500.0, 0.0_f64);
        let mut all = Vec::new();
        let mut stop_cx = GaCx {
            scope: Some(&mut scope),
            now_ms,
            star: Some(Point::new(500.0, y)),
            use_subframes: true,
            ..cx(&rec, &mut profile)
        };
        ga.stop(&mut stop_cx, &mut all);
        assert!(ga.is_measuring_backlash() && !ga.is_measuring());
        assert_eq!(all[0], GaEvent::UseSubframes(false));
        assert_eq!(ga.status(), "Measuring backlash... ");
        assert_eq!(ga.measure_backlash(), (true, false));
        for _ in 0..300 {
            if !ga.is_measuring_backlash() {
                break;
            }
            let pulse = all.iter().rev().find_map(|e| if let GaEvent::Backlash(s) = e { Some(s.pulse) } else { None });
            if let Some(Some((dir, ms))) = pulse {
                let travel = f64::from(ms) * 0.008;
                if dir == Dir::North {
                    let given_back = slack.min(travel);
                    slack -= given_back;
                    y += travel - given_back;
                } else {
                    let eaten = travel.min(10.0 - slack);
                    slack += eaten;
                    y -= travel - eaten;
                }
            }
            now_ms += 1000;
            let mut cx = GaCx { scope: Some(&mut scope), now_ms, ..cx(&rec, &mut profile) };
            ga.backlash_step(Point::new(500.0, y), &mut cx, &mut all);
        }
        assert!(!ga.is_measuring_backlash());
        let tool = ga.backlash_tool().unwrap();
        assert_eq!((tool.blt_state(), tool.backlash_result_ms()), (BltState::Completed, 1000));
        // The result first in the guide log, then the three result lines.
        let res = results(&all);
        assert_eq!(res[0], format!("{}\n", tool.ga_result_text(2.0).unwrap()));
        assert!(res[0].starts_with("Backlash= 1000  +/-  "));
        assert!(res[1].starts_with("SNR=20.0, Samples=131, Elapsed Time=130s"));
        assert_eq!(ga.details().backlash_info, res[0].trim_start_matches("Backlash=").trim_end());
        let blt = ga.recommendations().iter().find(|r| r.kind == RecKind::Blt).unwrap();
        assert_eq!(blt.text, "Try starting with a Dec backlash compensation of 1000 ms");
        assert!(blt.can_apply);
        assert!(res.contains(&"Recommendation: Try starting with a Dec backlash compensation of 1000 ms\n"));
        // Variable delay, then the subframes come back last.
        let n = all.len();
        assert_eq!(all[n - 2..], [GaEvent::VariableDelay(VarDelayCfg::default()), GaEvent::UseSubframes(true)]);
        // Saved: the pulse, and the legs as PHD2 cuts them.
        let north = steps_str(tool.north_steps());
        let msmt = tool.blt_msmt_pulse_size();
        let n_north = tool.north_steps().len();
        let (ts, d) = last_run(&profile).unwrap();
        assert_eq!(ts, "2026-11-11 21:00:00");
        assert_eq!((d.blt_amount.as_str(), d.blt_msmt_pulse), ("1000", msmt));
        assert_eq!(profile.get_str("/GA/2026-11-11 21:00:00/BLT_north", ""), north);
        assert_eq!(d.blt_north.len(), n_north);
        assert!(ga.graph_enabled());
    }

    #[test]
    fn steps_lose_their_last_digit() {
        assert_eq!(steps_str(&[1.0, 2.5, 4.25]), "1.0,2.5,4.");
        assert_eq!(steps_str(&[]), "");
        let mut p = Mem::default();
        p.set_str("/GA/t/BLT_north", "1.0,2.5,4.");
        p.set_str("/GA/t/BLT_South", "4.2,x,1.5");
        let d = load_results(&p, "t");
        assert_eq!(d.blt_north, [1.0, 2.5, 4.0]);
        // A token that does not parse repeats the previous value.
        assert_eq!(d.blt_south, [4.2, 4.2, 1.5]);
        assert_eq!(d.blt_msmt_pulse, -1);
    }

    #[test]
    fn history_keeps_three_runs_and_a_backlash_one() {
        let run = |p: &mut Mem, ts: &str, blt: bool| {
            p.set_str(&format!("/GA/{ts}/timestamp"), ts);
            if blt {
                p.set_str(&format!("/GA/{ts}/BLT_north"), "1.0,2.");
            }
        };
        let trim = |p: &mut Mem, fresh: bool| {
            let mut ev = Vec::new();
            trim_history(p, fresh, &mut ev);
            debug_lines(&ev).iter().map(|s| s.to_string()).collect::<Vec<_>>()
        };
        // No backlash anywhere: the oldest goes.
        let mut p = Mem::default();
        for ts in ["a", "b", "c", "d"] {
            run(&mut p, ts, false);
        }
        assert_eq!(trim(&mut p, false), ["GA-History: removed entry for a"]);
        assert_eq!(history(&p), ["b", "c", "d"]);
        assert!(trim(&mut p, false).is_empty());
        // The oldest holds the only backlash run: the next oldest goes...
        let mut p = Mem::default();
        run(&mut p, "a", true);
        for ts in ["b", "c", "d"] {
            run(&mut p, ts, false);
        }
        assert_eq!(trim(&mut p, false), ["GA-History: removed entry for b"]);
        // ...unless this run measured backlash.
        run(&mut p, "e", true);
        assert_eq!(trim(&mut p, true), ["GA-History: removed entry for a"]);
    }

    #[test]
    fn review_shows_a_saved_run() {
        let mut ev = Vec::new();
        let mut ga = started(false, 1000, &mut ev);
        for t in 0..=130 {
            let k = t as i32;
            ga.guide_step(&info(0.0, if k % 2 != 0 { -0.3 } else { 0.3 }, t as f64, 8.0, 1000.0), &env(t), &mut ev);
        }
        let mut profile = Mem::default();
        ga.make_recommendations(&mut cx(&rec_env(), &mut profile), &mut ev);
        let mut other = GuidingAssistant::new(None, false, false, VarDelayCfg::default(), false, &mut ev);
        other.review(&profile, "2026-11-11 21:00:00");
        assert!(other.is_review());
        assert_eq!(other.details().snr, "8.0");
        assert_eq!(other.recommendations(), ga.recommendations());
        assert_eq!(other.min_move_recs(), (0.26, 0.4));
        assert_eq!(other.details().rec_dec_min_move, "0.400000");
        assert_eq!(last_run(&profile).unwrap().1, *other.details());
    }

    // ------------------------------------------------------------ apply

    #[test]
    fn apply_sends_phd2_parameter_names() {
        let mut p = Mem::default();
        let mut mev = Vec::new();
        let mut scope = Scope::new(&mut p, false, 0.0, &mut mev);
        let algo = AlgoCtx::default();
        let (mut ga, _) = recommend(&RecEnv { hp_encoders: true, ..rec_env() }, |_| {});
        let (ra_rec, dec_rec) = ga.min_move_recs();
        let mut apply = |ga: &mut GuidingAssistant, kind| {
            let mut ev = Vec::new();
            let cx = ApplyCx { scope: Some(&mut scope), ao: None, profile: &mut p, algo: &algo };
            (ga.apply(kind, cx, &mut ev), ev)
        };
        let changes = |ev: &[GaEvent]| -> Vec<(String, Value)> {
            ev.iter()
                .filter_map(|e| match e {
                    GaEvent::ParamChange { name, value } => Some((name.clone(), value.clone())),
                    GaEvent::Mount(MountEvent::GuideParamChange { name, value }) => Some((name.clone(), value.clone())),
                    _ => None,
                })
                .collect()
        };
        let mount_debug = |ev: &[GaEvent]| -> Vec<String> {
            ev.iter()
                .filter_map(|e| match e {
                    GaEvent::Debug(s) | GaEvent::Mount(MountEvent::Debug(s)) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        };

        let (ran, ev) = apply(&mut ga, RecKind::RaMinMove);
        assert!(ran);
        assert_eq!(changes(&ev), [("RA Hysteresis MinMove ".to_string(), ra_rec.into())]);
        assert_eq!(mount_debug(&ev), [format!("GuideAssistant changed RA_MinMove to {}", fmt_f(ra_rec, 2))]);
        // The button is off now.
        assert!(!apply(&mut ga, RecKind::RaMinMove).0);

        let (_, ev) = apply(&mut ga, RecKind::DecMinMove);
        assert_eq!(changes(&ev), [("Declination ResistSwitch MinMove ".to_string(), dec_rec.into())]);

        // Encoders on ResistSwitch: Lowpass2 for Dec, guiding off meanwhile.
        let (_, ev) = apply(&mut ga, RecKind::DecAlgo);
        assert_eq!(
            changes(&ev),
            [
                ("MountGuidingEnabled".to_string(), false.into()),
                ("MountGuidingEnabled".to_string(), true.into()),
                ("Declination algorithm".to_string(), "Lowpass2".into()),
                ("Declination Lowpass2 aggressivness".to_string(), 80.0.into()),
                ("Declination Lowpass2 MinMove".to_string(), dec_rec.into()),
            ]
        );
        assert_eq!(
            mount_debug(&ev),
            [
                "MountGuidingEnabled: 0".to_string(),
                "GuideAssistant changed Dec algo to Lowpass2".into(),
                format!("GuideAssistant set Lowpass2 aggressiveness = 80.00, min-move = {}", fmt_f(dec_rec, 2)),
                "MountGuidingEnabled: 1".into(),
            ]
        );
        assert!(ga.recommendations().iter().all(|r| !r.can_apply));
        let x = scope.mount.algo(Axis::X);
        assert_eq!(x.min_move(), ra_rec);
        let y = scope.mount.algo(Axis::Y);
        assert_eq!((y.kind(), y.get_param("aggressiveness"), y.min_move()), (AlgoKind::Lowpass2, Some(80.0), dec_rec));
        assert_eq!(p.get_f64("/scope/GuideAlgorithm/X/Hysteresis/minMove", 0.0), ra_rec);
        assert_eq!(p.get_i32("/scope/YGuideAlgorithm", -1), AlgoKind::Lowpass2 as i32);
        assert_eq!(p.get_f64("/scope/GuideAlgorithm/Y/Lowpass2/Aggressiveness", 0.0), 80.0);
    }

    #[test]
    fn apply_backlash_compensation() {
        let mut p = Mem::default();
        let mut mev = Vec::new();
        let mut scope = Scope::new(&mut p, false, 0.0, &mut mev);
        let algo = AlgoCtx::default();
        let mut ev = Vec::new();
        let mut ga = started(false, 1000, &mut ev);
        // A completed test's recommendation, as `make_recommendations` sets it.
        ga.backlash_recommended_ms = 1230;
        let text = "Try starting with a Dec backlash compensation of 1230 ms";
        ga.recs.push(Recommendation::new(RecKind::Blt, text.into()));
        let mut ev = Vec::new();
        let cx = ApplyCx { scope: Some(&mut scope), ao: None, profile: &mut p, algo: &algo };
        assert!(ga.apply(RecKind::Blt, cx, &mut ev));
        let blc = scope.backlash_comp().unwrap();
        assert_eq!((blc.backlash_pulse_width(), blc.is_enabled()), (1230, true));
        assert_eq!(p.get_i32(crate::engine::backlash::KEY_PULSE, 0), 1230);
        assert!(ev.iter().all(|e| matches!(e, GaEvent::Mount(_))));
        assert!(!ga.recommendations()[0].can_apply);
    }
}
