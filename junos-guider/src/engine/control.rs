//! The guide / dither / settle controller: PHD2's `PhdController`, which a
//! client drives through `guide` and `dither`, with the parts of `MyFrame`
//! it works through — the capture loop's flags (`StartLooping`,
//! `StartCapturing`, `StopCapturing`, `FinishStop` and the bookkeeping of
//! `OnExposeComplete`), the single exposure's part in them, the pause
//! (`SetPaused`), the dither itself (random
//! or on a spiral), the exposure duration with auto exposure, and the delay
//! between exposures (time lapse or the variable delay).
//!
//! A `guide` starts capturing, auto-selects a star (three tries), waits up
//! to three frames for it to be selected, calibrates the mounts that need
//! it, starts guiding, then waits for the star to settle; a `dither` moves
//! the lock (the guider's fast recenter takes the mount there) and settles
//! the same way. Settling ends after the star stayed within the tolerance
//! for the settle time, or fails at the timeout, and is reported by
//! `SettleDone`. The state machine runs once per frame
//! ([`Control::after_frame`]) and on each request, until it has to wait.
//!
//! The engine owns the camera: it makes the exposure
//! [`Control::exposure_request`] holds, after [`Control::exposure_delay`],
//! then calls [`Control::on_expose_complete`] and, unless that drops the
//! frame, [`Control::on_capture_error`] for a failed exposure or, for a
//! frame: runs it through the guider (stopping it unless
//! [`Control::continue_capturing`]), hands the guider's events to
//! [`Control::forward`] (which takes the auto-exposure step among them),
//! calls [`Control::after_frame`], and only then makes the frame's moves —
//! PHD2 queues them to the worker thread, so the controller sees the frame
//! before they are made.
//!
//! Two rules of PHD2's worker thread are the engine's too. Scheduling an
//! exposure (a new [`Control::exposure_request`]) clears a stop asked for
//! before it (worker_thread.cpp:88), so a stop that came too late to cut
//! the last exposure short does not cancel the next loop's first. And a
//! stop cuts the delay before an exposure short: that is a capture error
//! ([`Control::on_capture_error`]), after the debug line `Error thrown from
//! worker_thread.cpp:184->Time lapse interrupted`.
//!
//! The guider and the mounts come in through a [`ControlCx`]; what PHD2
//! tells the outside comes back as [`ControlEvent`]s, in PHD2's order.
//!
//! Ported from PHD2 src/phdcontrol.h, src/phdcontrol.cpp, src/myframe.cpp (the exposure durations and auto exposure, the exposure delay, ScheduleExposure, StartSingleExposure, StartCapturing, StopCapturing, SetPaused, StartLooping, StartGuiding, AutoSelectStar, DitherSpiral, Dither and their settings), src/myframe_events.cpp (OnExposeComplete, FinishStop, OnExposureDurationSelected, NotifyExposureChanged, RequestedExposureDuration) — BSD-3-Clause, see THIRD_PARTY.md

use serde_json::Value;

use super::calibration::ProfileAccess;
use super::guider::{Guider, GuiderCx, GuiderEvent, GuiderMount, GuiderState, MountRole, PauseType};
use super::log::cfmt::fmt_f;
use super::log::guide_log::ExposureSummary;
use super::mount::MountEvent;
use super::rng::Rng;
use super::scope::DecGuideMode;
use super::traits::{Point, Rect};

/// A settle time (or timeout) that never elapses: the settle of a dither
/// that counts frames.
pub const SETTLING_TIME_DISABLED: i32 = 9999;

/// The exposure setting until one is chosen, ms.
pub const DEFAULT_EXPOSURE_MS: i32 = 1000;

/// The exposure durations the frame offers, ms (`dur_values`,
/// myframe.cpp:963), the custom one last.
const DEFAULT_DURATIONS: [i32; 22] = [
    10, 20, 50, 100, 200, 500, 1000, 1500, 2000, 2500, 3000, 3500, 4000, 4500, 5000, 6000, 7000, 8000, 9000, 10000,
    15000, 30000,
];

/// The profile keys the frame's settings live under.
pub mod keys {
    pub const DITHER_MODE: &str = "/DitherMode";
    pub const DITHER_RA_ONLY: &str = "/DitherRaOnly";
    pub const DITHER_SCALE_FACTOR: &str = "/DitherScaleFactor";
    pub const TIME_LAPSE: &str = "/frame/timeLapse";
    pub const VAR_DELAY_ENABLED: &str = "/frame/var_delay/enabled";
    pub const VAR_DELAY_SHORT: &str = "/frame/var_delay/short_delay";
    pub const VAR_DELAY_LONG: &str = "/frame/var_delay/long_delay";
    pub const AUTO_EXP_MIN: &str = "/auto_exp/exposure_min";
    pub const AUTO_EXP_MAX: &str = "/auto_exp/exposure_max";
    pub const AUTO_EXP_SNR: &str = "/auto_exp/target_snr";
    pub const CUSTOM_EXPOSURE: &str = "/CustomExposureDuration";
    pub const EXPOSURE_MS: &str = "/ExposureDurationMs";
}

/// PHD2's `SettleParams`: when a guide or a dither has settled.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SettleParams {
    /// Settled within this distance of the lock, px.
    pub tolerance_px: f64,
    /// For this long, s; 0 or less: on the first frame in range.
    pub settle_time_s: i32,
    /// Failed after this long, s.
    pub timeout_s: i32,
    /// Settled after this many frames whatever the distance (the event
    /// server asks for 99999).
    pub frames: i32,
}

impl SettleParams {
    /// The settle of PHD2's `Dither(pixels, settleFrames)`: a number of
    /// frames, with no tolerance (99 px) and no time.
    pub fn frames(frames: i32) -> SettleParams {
        SettleParams {
            tolerance_px: 99.0,
            settle_time_s: SETTLING_TIME_DISABLED,
            timeout_s: SETTLING_TIME_DISABLED,
            frames,
        }
    }
}

/// PHD2's `ControlGuideOptions`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuideOpts {
    /// `GUIDEOPT_FORCE_RECAL`: calibrate again even when calibrated.
    pub force_recal: bool,
    /// `GUIDEOPT_USE_STICKY_LOCK`: keep the lock through a calibration, so
    /// that guiding brings the star back where it was (the event server
    /// always asks for it).
    pub use_sticky_lock: bool,
}

/// PHD2's `DitherMode` (`/DitherMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DitherMode {
    /// A random offset on each axis, up to the amount.
    #[default]
    Random = 0,
    /// Unit steps around a square spiral, times the amount.
    Spiral = 1,
}

/// PHD2's `AutoExposureCfg`: the exposure follows the star's SNR.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoExposureCfg {
    pub enabled: bool,
    pub min_ms: i32,
    pub max_ms: i32,
    pub target_snr: f64,
}

/// PHD2's `VarDelayCfg`: a long delay between exposures while guiding
/// calmly, a short one otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VarDelayCfg {
    pub enabled: bool,
    pub short_ms: i32,
    pub long_ms: i32,
}

/// An exposure `ScheduleExposure` asked for: a light frame (`CAPTURE_LIGHT`)
/// of this long, on this subframe (empty: the full frame).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExposureRequest {
    pub duration_ms: i32,
    pub subframe: Rect,
}

/// A single exposure (`capture_single_frame`) as the capture loop sees
/// PHD2's `m_singleExposure`: its duration, ms, and subframe (empty: the
/// full frame). Its binning, gain and saving are the engine's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SingleExposure {
    pub duration_ms: i32,
    pub subframe: Rect,
}

/// PHD2's `DitherSpiral` (myframe.cpp:2012): the next point of a square
/// spiral around the start on each dither. RA-only dithers walk the RA axis
/// out and back (0, 1, −1, −2, 2, 3, −3, …); switching between the two
/// starts over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DitherSpiral {
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    prev_ra_only: bool,
}

impl Default for DitherSpiral {
    fn default() -> Self {
        DitherSpiral { x: 0, y: 0, dx: -1, dy: 0, prev_ra_only: false }
    }
}

/// `ROT`: a quarter turn.
fn rot(dx: &mut i32, dy: &mut i32) {
    (*dx, *dy) = (*dy, -*dx);
}

impl DitherSpiral {
    /// `Reset`: back to the start.
    pub fn reset(&mut self, ev: &mut Vec<ControlEvent>) {
        debug(ev, "reset dither spiral");
        *self = DitherSpiral::default();
    }

    /// `GetDither`: the next offset (RA, Dec), `amount` per step.
    pub fn get_dither(&mut self, amount: f64, ra_only: bool, ev: &mut Vec<ControlEvent>) -> (f64, f64) {
        if ra_only != self.prev_ra_only {
            self.reset(ev);
            self.prev_ra_only = ra_only;
        }
        if ra_only {
            rot(&mut self.dx, &mut self.dy);
            let x0 = self.x;
            if self.dy == 0 {
                self.x = -self.x;
            } else {
                self.x += self.dy;
            }
            ((self.x - x0) as f64 * amount, 0.0)
        } else {
            let (x, y) = (self.x, self.y);
            if x == y || (x > 0 && x == -y) || (x <= 0 && y == 1 - x) {
                rot(&mut self.dx, &mut self.dy);
            }
            self.x += self.dx;
            self.y += self.dy;
            (self.dx as f64 * amount, self.dy as f64 * amount)
        }
    }
}

/// What the controller and the frame tell the outside, in PHD2's order.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlEvent {
    /// What the guider said (its mounts' notices inside).
    Guider(GuiderEvent),
    /// What a mount said when the controller or the frame drove it.
    Mount(MountRole, MountEvent),
    /// The `SettleBegin` event.
    SettleBegin,
    /// The `Settling` event, each frame of a settle: the distance (px), the
    /// time in range so far and the time asked for (s), the star found.
    Settling { distance: f64, time: f64, settle_time: f64, star_locked: bool },
    /// The `SettleDone` event: `Status` 1 with the error when there is one,
    /// else 0; the frames the settle took, and those without the star.
    SettleDone { error: Option<String>, total_frames: i32, dropped_frames: i32 },
    /// The guide log's `INFO: SETTLING STATE CHANGE, <msg>`, written even
    /// when not guiding.
    SettlingStateChange(&'static str),
    /// A dither moved the lock by (`dx`, `dy`) RA/Dec px to `lock`: the
    /// guide log's `INFO: DITHER by …` (while guiding), the
    /// `GuidingDithered` event and the graph's mark.
    GuidingDithered { dx: f64, dy: f64, lock: Point },
    /// The `Paused` event.
    Paused,
    /// The `Resumed` event.
    Resumed,
    /// The guide log's `INFO: Server received <cmd>` (`PAUSE`, `RESUME`).
    ServerCommand(&'static str),
    /// `NotifyGuidingParam`: the `GuideParamChange` event and the guide
    /// log's parameter-change line (`Exposure`, a string).
    ParamChange { name: &'static str, value: Value },
    /// The `LoopingExposuresStopped` event.
    LoopingStopped,
    /// `SingleExposure::Complete`: the single exposure ended, with this
    /// error when it failed. The engine's: the image saved when asked (a
    /// failure there fails it), the debug line, the `SingleFrameComplete`
    /// event, then the camera's binning and gain put back.
    SingleFrameComplete { error: Option<&'static str> },
    /// `StartCapturing` began a loop: the frame counter restarts and the
    /// dark library and defect map are checked against the frame size
    /// (`CheckDarkFrameGeometry`). The engine's, which then runs the
    /// camera's `InitCapture` before the first exposure — PHD2 skips it
    /// while an exposure is out, which never happens when a loop starts
    /// (CaptureActive is cleared only once none is).
    CaptureStarted,
    /// `StartGuiding`'s `CheckLogRollover`: the logs move to a new file at
    /// the start of a new imaging day. The engine's.
    CheckLogRollover,
    /// The exposure changed: `pCamera->SelectDark(ms)`. The engine's, only
    /// when a camera is chosen (`if (pCamera)`), connected or not.
    SelectDark(i32),
    /// AutoSelect set the camera's saturation level (AutoFind's
    /// `SetSaturationByADU(true, level)`): the engine saves it to
    /// `/camera/SaturationADU`.
    SaturationAdu(u16),
    /// `pFrame->StatusMsg`.
    Status(String),
    /// A debug-log line.
    Debug(String),
}

fn debug(ev: &mut Vec<ControlEvent>, line: impl Into<String>) {
    ev.push(ControlEvent::Debug(line.into()));
}

fn wrap(ev: &mut Vec<ControlEvent>, role: MountRole, mev: Vec<MountEvent>) {
    ev.extend(mev.into_iter().map(|e| ControlEvent::Mount(role, e)));
}

/// What a controller or frame call reads from the rest of PHD2: the guider,
/// the guider's own context (the mounts, the profile, …), the camera and
/// the clock.
pub struct ControlCx<'a> {
    pub guider: &'a mut Guider,
    /// Its `settling`, `exposure_ms` and `auto_exposure` are this
    /// controller's ([`Control::is_settling`], [`Control::exposure_info`]).
    pub gcx: GuiderCx<'a>,
    /// `pCamera && pCamera->Connected`.
    pub camera_connected: bool,
    /// The camera's full frame, px: the guider's bounding box is clipped
    /// to it.
    pub frame_size: (i32, i32),
    /// Monotonic ms (the settle stopwatches, `wxStopWatch::Time`).
    pub mono_ms: i64,
}

/// `CanGuide`: the camera and every mount connected.
pub fn can_guide(cx: &ControlCx) -> Result<(), &'static str> {
    let connected = |m: Option<&dyn GuiderMount>| m.is_none_or(|m| m.mount().is_connected());
    if cx.camera_connected && connected(cx.gcx.primary.as_deref()) && connected(cx.gcx.secondary.as_deref()) {
        Ok(())
    } else {
        Err("all equipment must be connected first")
    }
}

/// The controller's states (`State`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle = 0,
    Setup,
    AttemptStart,
    SelectStar,
    WaitSelected,
    Calibrate,
    CalibrationWait,
    Guide,
    SettleBegin,
    SettleWait,
    Finish,
}

impl State {
    /// The name `SETSTATE` writes.
    fn name(self) -> &'static str {
        match self {
            State::Idle => "STATE_IDLE",
            State::Setup => "STATE_SETUP",
            State::AttemptStart => "STATE_ATTEMPT_START",
            State::SelectStar => "STATE_SELECT_STAR",
            State::WaitSelected => "STATE_WAIT_SELECTED",
            State::Calibrate => "STATE_CALIBRATE",
            State::CalibrationWait => "STATE_CALIBRATION_WAIT",
            State::Guide => "STATE_GUIDE",
            State::SettleBegin => "STATE_SETTLE_BEGIN",
            State::SettleWait => "STATE_SETTLE_WAIT",
            State::Finish => "STATE_FINISH",
        }
    }
}

/// What the controller is settling (`SettleOp`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettleOp {
    #[default]
    Dither = 0,
    Guide = 1,
}

/// PHD2's `PhdController` and the parts of `MyFrame` it works through.
pub struct Control {
    // PhdController (`ControllerState`)
    state: State,
    force_calibration: bool,
    have_save_sticky: bool,
    save_sticky: bool,
    auto_find_attempts_remaining: i32,
    wait_selected_remaining: i32,
    use_sticky_lock: bool,
    settle_op: SettleOp,
    settle: SettleParams,
    roi: Option<Rect>,
    settle_prior_frame_in_range: bool,
    /// When the two stopwatches started, monotonic ms.
    settle_timeout_start: i64,
    settle_in_range_start: i64,
    save_dec_guide_mode: DecGuideMode,
    override_dec_guide_mode: bool,
    settle_frame_count: i32,
    dropped_frame_count: i32,
    succeeded: bool,
    error_msg: String,

    // MyFrame
    capture_active: bool,
    continue_capturing: bool,
    /// `m_exposurePending`, with what was asked for.
    exposure: Option<ExposureRequest>,
    /// `m_singleExposure`, while enabled.
    single: Option<SingleExposure>,
    exposure_ms: i32,
    durations: [i32; 22],
    auto_exp: AutoExposureCfg,
    dither_mode: DitherMode,
    dither_scale: f64,
    dither_ra_only: bool,
    spiral: DitherSpiral,
    time_lapse_ms: i32,
    var_delay: VarDelayCfg,
    /// `GetExposureDelay`'s `s_lastExposureDelay`.
    last_exposure_delay: i32,
    /// The random dither's source (PHD2's `rand()`).
    pub rng: Rng,
}

impl Control {
    /// The controller idle, not capturing, and the frame's settings as
    /// [`Control::load_profile_settings`] reads them.
    pub fn new(profile: &mut dyn ProfileAccess, ev: &mut Vec<ControlEvent>) -> Control {
        let mut c = Control {
            state: State::Idle,
            force_calibration: false,
            have_save_sticky: false,
            save_sticky: false,
            auto_find_attempts_remaining: 0,
            wait_selected_remaining: 0,
            use_sticky_lock: false,
            settle_op: SettleOp::default(),
            settle: SettleParams::default(),
            roi: None,
            settle_prior_frame_in_range: false,
            settle_timeout_start: 0,
            settle_in_range_start: 0,
            save_dec_guide_mode: DecGuideMode::None,
            override_dec_guide_mode: false,
            settle_frame_count: 0,
            dropped_frame_count: 0,
            succeeded: false,
            error_msg: String::new(),
            capture_active: false,
            continue_capturing: false,
            exposure: None,
            single: None,
            exposure_ms: DEFAULT_EXPOSURE_MS,
            durations: DEFAULT_DURATIONS,
            auto_exp: AutoExposureCfg { enabled: false, min_ms: 0, max_ms: 0, target_snr: 0.0 },
            dither_mode: DitherMode::Random,
            dither_scale: 1.0,
            dither_ra_only: false,
            spiral: DitherSpiral::default(),
            time_lapse_ms: 0,
            var_delay: VarDelayCfg::default(),
            last_exposure_delay: -1,
            rng: Rng::default(),
        };
        // `DitherSpiral`'s constructor calls `Reset`, which logs, before
        // the frame reads its settings.
        c.spiral.reset(ev);
        c.load_profile_settings(profile, ev);
        c
    }

    /// The part of `MyFrame::LoadProfileSettings` for these settings, in its
    /// order: dither, time lapse, variable delay, auto exposure (which is
    /// turned on so the exposure starts at its maximum), the custom
    /// duration, then the exposure — a stored duration that is not offered
    /// leaves auto exposure on, as in PHD2. Its noise reduction, focal
    /// length, gamma and image logger settings are not the controller's;
    /// the pre-2017 `/ExposureDuration` string is not read.
    pub fn load_profile_settings(&mut self, profile: &mut dyn ProfileAccess, ev: &mut Vec<ControlEvent>) {
        self.set_dither_scale_factor(profile.get_f64(keys::DITHER_SCALE_FACTOR, 1.0), profile, ev);
        self.set_dither_ra_only(profile.get_i32(keys::DITHER_RA_ONLY, 0) != 0, profile);
        let mode = if profile.get_i32(keys::DITHER_MODE, 0) == 0 { DitherMode::Random } else { DitherMode::Spiral };
        self.set_dither_mode(mode, profile, ev);
        self.set_time_lapse(profile.get_i32(keys::TIME_LAPSE, 0), profile, ev);
        self.set_variable_delay_config(
            profile.get_i32(keys::VAR_DELAY_ENABLED, 0) != 0,
            profile.get_i32(keys::VAR_DELAY_SHORT, 1000),
            profile.get_i32(keys::VAR_DELAY_LONG, 10000),
            profile,
            ev,
        );
        let (min, max) = (profile.get_i32(keys::AUTO_EXP_MIN, 1000), profile.get_i32(keys::AUTO_EXP_MAX, 5000));
        self.store_auto_exposure_cfg(min, max, profile.get_f64(keys::AUTO_EXP_SNR, 6.0), profile, ev);
        self.auto_exp.enabled = true;
        self.reset_auto_exposure(ev);
        self.durations[DEFAULT_DURATIONS.len() - 1] = profile.get_i32(keys::CUSTOM_EXPOSURE, 30000);
        let exposure =
            if profile.has(keys::EXPOSURE_MS) { profile.get_i32(keys::EXPOSURE_MS, 1000) } else { DEFAULT_EXPOSURE_MS };
        self.set_exposure_duration(exposure, profile, ev);
    }

    // ------------------------------------------------------------ controller

    /// `IsSettling`.
    pub fn is_settling(&self) -> bool {
        matches!(self.state, State::SettleBegin | State::SettleWait)
    }

    /// Settling after a dither, not a guide (`guider_state.dithering`).
    pub fn is_dithering(&self) -> bool {
        self.is_settling() && self.settle_op == SettleOp::Dither
    }

    /// `IsIdle`: not working through a guide or a dither.
    pub fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    fn set_state(&mut self, state: State, ev: &mut Vec<ControlEvent>) {
        debug(ev, format!("PhdController: newstate {}", state.name()));
        self.state = state;
    }

    /// `ReentrancyError`.
    fn reentrancy_error(&self, op: &str) -> String {
        let busy = if self.settle_op == SettleOp::Dither { "dither" } else { "guide" };
        format!("Cannot initiate {op} while {busy} is in progress")
    }

    /// `PhdController::Guide`: start guiding — looping, selecting a star and
    /// calibrating first as needed — then settle. `roi` limits the star
    /// search. An error when a guide or dither is already in progress.
    pub fn guide(
        &mut self,
        opts: GuideOpts,
        settle: SettleParams,
        roi: Option<Rect>,
        cx: &mut ControlCx,
        ev: &mut Vec<ControlEvent>,
    ) -> Result<(), String> {
        if self.state != State::Idle {
            debug(
                ev,
                format!("PhdController::Guide reentrancy state = {} op = {}", self.state as i32, self.settle_op as i32),
            );
            return Err(self.reentrancy_error("guide"));
        }
        debug(ev, "PhdController::Guide begins");
        self.force_calibration = opts.force_recal;
        self.use_sticky_lock = opts.use_sticky_lock;
        self.settle_op = SettleOp::Guide;
        self.settle = settle;
        self.roi = roi;
        self.set_state(State::Setup, ev);
        self.update(cx, ev);
        Ok(())
    }

    /// `PhdController::Dither`: move the lock by up to `pixels`, then
    /// settle. RA only when asked, when the profile says so, or when the
    /// scope guides Dec in no direction or (without a settle time) in one
    /// only; a one-direction Dec mode is set to Auto while it settles.
    pub fn dither(
        &mut self,
        pixels: f64,
        force_ra_only: bool,
        settle: SettleParams,
        cx: &mut ControlCx,
        ev: &mut Vec<ControlEvent>,
    ) -> Result<(), String> {
        if self.state != State::Idle {
            debug(
                ev,
                format!(
                    "PhdController::Dither reentrancy state = {} op = {}",
                    self.state as i32, self.settle_op as i32
                ),
            );
            return Err(self.reentrancy_error("dither"));
        }
        debug(ev, "PhdController::Dither begins");
        let mut ra_only = self.dither_ra_only || force_ra_only;
        let mut override_mode = false;
        let mut mode = DecGuideMode::None;
        if !ra_only && let Some(scope) = cx.gcx.primary.as_deref_mut().and_then(|m| m.scope_mut()) {
            mode = scope.dec_guide_mode();
            if mode != DecGuideMode::Auto {
                if matches!(mode, DecGuideMode::North | DecGuideMode::South)
                    && settle.settle_time_s != SETTLING_TIME_DISABLED
                {
                    override_mode = true;
                } else {
                    // Quirk kept: the line names the mode the previous
                    // dither saved, not this one.
                    debug(
                        ev,
                        format!(
                            "PhdController: forcing dither RA-only since Dec guide mode is {}",
                            self.save_dec_guide_mode.label()
                        ),
                    );
                    ra_only = true;
                }
            }
        }
        if !self.frame_dither(pixels, ra_only, cx, ev) {
            debug(ev, "PhdController::Dither pFrame->Dither failed");
            return Err("Dither error".into());
        }
        self.settle_op = SettleOp::Dither;
        self.settle = settle;
        self.override_dec_guide_mode = override_mode;
        self.save_dec_guide_mode = mode;
        self.set_state(State::SettleBegin, ev);
        self.update(cx, ev);
        Ok(())
    }

    /// `DitherCompat`, the manual dither: whatever runs is aborted, then a
    /// dither that settles over 10 frames.
    pub fn dither_compat(&mut self, pixels: f64, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) -> Result<(), String> {
        self.abort("manual or phd1-style dither", cx, ev);
        self.dither(pixels, false, SettleParams::frames(10), cx, ev)
    }

    /// `AbortController`: a guide or dither in progress fails with `reason`.
    pub fn abort(&mut self, reason: &str, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        if self.state != State::Idle {
            self.fail(reason, ev);
            self.update(cx, ev);
        }
    }

    /// `do_fail`.
    fn fail(&mut self, msg: &str, ev: &mut Vec<ControlEvent>) {
        debug(ev, format!("PhdController failed: {msg}"));
        self.succeeded = false;
        self.error_msg = msg.into();
        self.set_state(State::Finish, ev);
    }

    fn succeed(&mut self, ev: &mut Vec<ControlEvent>) {
        self.succeeded = true;
        self.set_state(State::Finish, ev);
    }

    /// `UpdateControllerState`: move on until there is something to wait
    /// for.
    fn update(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        loop {
            match self.state {
                State::Idle => return,
                State::Setup => {
                    debug(ev, "PhdController: setup");
                    self.have_save_sticky = false;
                    self.auto_find_attempts_remaining = 3;
                    // A guide stop/start with no dither.
                    self.override_dec_guide_mode = false;
                    self.set_state(State::AttemptStart, ev);
                }
                State::AttemptStart => {
                    if let Err(err) = can_guide(cx) {
                        debug(ev, format!("PhdController: not ready: {err}"));
                        self.fail(err, ev);
                    } else if cx.guider.is_calibrating_or_guiding() {
                        if self.force_calibration {
                            self.set_state(State::Calibrate, ev);
                        } else {
                            let state = cx.guider.state();
                            debug(ev, format!("PhdController: guider state = {}", state as i32));
                            if matches!(state, GuiderState::Calibrated | GuiderState::Guiding) {
                                self.set_state(State::SettleBegin, ev);
                            } else {
                                self.set_state(State::CalibrationWait, ev);
                                return;
                            }
                        }
                    } else if !self.capture_active {
                        debug(ev, "PhdController: start capturing");
                        // `start_capturing` (phdcontrol.cpp): the camera is
                        // connected, `CanGuide` said so.
                        let mut gev = Vec::new();
                        cx.guider.reset(true, &mut cx.gcx, &mut gev);
                        // A full frame to auto-select a star on.
                        cx.guider.force_full_frame(&mut gev);
                        self.forward(gev, cx, ev);
                        self.reset_auto_exposure(ev);
                        self.start_capturing(cx, ev);
                        self.set_state(State::SelectStar, ev);
                        return;
                    } else if cx.guider.state() == GuiderState::Selected {
                        self.set_state(State::Calibrate, ev);
                    } else {
                        // Capturing with no star selected.
                        self.set_state(State::SelectStar, ev);
                        // Auto exposure starts the search at its longest,
                        // on the next frame.
                        if self.auto_exp.enabled {
                            self.reset_auto_exposure(ev);
                            return;
                        }
                    }
                }
                State::SelectStar => {
                    if self.auto_select_star(self.roi, cx, ev) {
                        self.set_state(State::WaitSelected, ev);
                        self.wait_selected_remaining = 3;
                        return;
                    }
                    debug(
                        ev,
                        format!("auto find star failed, attempts remaining = {}", self.auto_find_attempts_remaining),
                    );
                    self.auto_find_attempts_remaining -= 1;
                    if self.auto_find_attempts_remaining == 0 {
                        self.fail("failed to find a suitable guide star", ev);
                    } else {
                        let mut gev = Vec::new();
                        cx.guider.reset(true, &mut cx.gcx, &mut gev);
                        self.forward(gev, cx, ev);
                        self.set_state(State::AttemptStart, ev);
                        return;
                    }
                }
                State::WaitSelected => {
                    if cx.guider.state() == GuiderState::Selected {
                        self.set_state(State::Calibrate, ev);
                    } else {
                        debug(
                            ev,
                            format!("waiting for star selected, attempts remaining = {}", self.wait_selected_remaining),
                        );
                        self.wait_selected_remaining -= 1;
                        if self.wait_selected_remaining == 0 {
                            self.set_state(State::AttemptStart, ev);
                        }
                        return;
                    }
                }
                State::Calibrate => {
                    if self.force_calibration {
                        debug(ev, "PhdController: clearing calibration");
                        let mounts = [cx.gcx.primary.as_deref_mut(), cx.gcx.secondary.as_deref_mut()];
                        for m in mounts.into_iter().flatten() {
                            m.clear_calibration();
                        }
                    }
                    let uncalibrated = |m: Option<&dyn GuiderMount>| m.is_some_and(|m| !m.is_calibrated());
                    if !uncalibrated(cx.gcx.primary.as_deref()) && !uncalibrated(cx.gcx.secondary.as_deref()) {
                        self.set_state(State::Guide, ev);
                        continue;
                    }
                    debug(ev, "PhdController: start calibration");
                    if self.use_sticky_lock {
                        self.save_sticky = cx.guider.lock_pos_is_sticky();
                        self.have_save_sticky = true;
                        cx.guider.set_lock_pos_is_sticky(true);
                    }
                    if !self.start_guiding(cx, ev) {
                        if self.use_sticky_lock && self.have_save_sticky {
                            cx.guider.set_lock_pos_is_sticky(self.save_sticky);
                        }
                        self.fail("could not start calibration", ev);
                        continue;
                    }
                    self.set_state(State::CalibrationWait, ev);
                    return;
                }
                State::CalibrationWait => {
                    // A failed calibration leaves the controller waiting
                    // here, as in PHD2, until a stop aborts it.
                    let calibrated = |m: Option<&dyn GuiderMount>| m.is_none_or(|m| m.is_calibrated());
                    if !calibrated(cx.gcx.primary.as_deref()) || !calibrated(cx.gcx.secondary.as_deref()) {
                        return;
                    }
                    if self.use_sticky_lock && self.have_save_sticky {
                        cx.guider.set_lock_pos_is_sticky(self.save_sticky);
                    }
                    self.set_state(State::SettleBegin, ev);
                }
                State::Guide => {
                    if !self.start_guiding(cx, ev) {
                        self.fail("could not start guiding", ev);
                        continue;
                    }
                    self.set_state(State::SettleBegin, ev);
                    return;
                }
                State::SettleBegin => {
                    ev.push(ControlEvent::SettleBegin);
                    ev.push(ControlEvent::SettlingStateChange("Settling started"));
                    if self.override_dec_guide_mode {
                        debug(ev, "PhdController: setting Dec guide mode to Auto for dither settle");
                        set_scope_dec_guide_mode(DecGuideMode::Auto, cx, ev);
                    }
                    self.settle_prior_frame_in_range = false;
                    self.settle_frame_count = 0;
                    self.dropped_frame_count = 0;
                    self.settle_timeout_start = cx.mono_ms;
                    self.set_state(State::SettleWait, ev);
                    return;
                }
                State::SettleWait => {
                    if self.settle_wait(cx, ev) {
                        return;
                    }
                }
                State::Finish => {
                    if self.override_dec_guide_mode {
                        let mode = self.save_dec_guide_mode;
                        debug(ev, format!("PhdController: restore Dec guide mode to {} after dither", mode.label()));
                        set_scope_dec_guide_mode(mode, cx, ev);
                        self.override_dec_guide_mode = false;
                    }
                    self.notify(cx, ev);
                    self.set_state(State::Idle, ev);
                    return;
                }
            }
        }
    }

    /// One frame of `STATE_SETTLE_WAIT`; true when it waits for the next.
    /// The two times are compared in whole seconds, as PHD2 divides its
    /// stopwatches' milliseconds.
    fn settle_wait(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) -> bool {
        let locked = cx.guider.multistar().is_locked();
        let error = cx.guider.current_error(cx.gcx.ra_only, cx.gcx.now_ms);
        let in_range = locked && error <= self.settle.tolerance_px;
        // `IsAoBumpInProgress` (never on a scope).
        let ao_bump = cx.gcx.primary.as_deref().is_some_and(|m| m.is_bump_in_progress());
        let mut time_in_range = 0;
        self.settle_frame_count += 1;
        if !locked {
            self.dropped_frame_count += 1;
        }
        debug(
            ev,
            format!(
                "PhdController: settling, locked = {}, distance = {} ({}) aobump = {} frame = {} / {}",
                locked as i32,
                fmt_f(error, 2),
                fmt_f(self.settle.tolerance_px, 2),
                ao_bump as i32,
                self.settle_frame_count,
                self.settle.frames
            ),
        );
        if self.settle_frame_count >= self.settle.frames {
            self.succeed(ev);
            return false;
        }
        if in_range {
            if !self.settle_prior_frame_in_range {
                if self.settle.settle_time_s <= 0 {
                    self.succeed(ev);
                    return false;
                }
                self.settle_in_range_start = cx.mono_ms;
            } else {
                time_in_range = cx.mono_ms - self.settle_in_range_start;
                if time_in_range / 1000 >= self.settle.settle_time_s as i64 && !ao_bump {
                    self.succeed(ev);
                    return false;
                }
            }
        }
        if (cx.mono_ms - self.settle_timeout_start) / 1000 >= self.settle.timeout_s as i64 {
            self.fail("timed-out waiting for guider to settle", ev);
            return false;
        }
        ev.push(ControlEvent::Settling {
            distance: error,
            time: time_in_range as f64 / 1000.0,
            settle_time: self.settle.settle_time_s as f64,
            star_locked: locked,
        });
        self.settle_prior_frame_in_range = in_range;
        true
    }

    /// `do_notify`: `SettleDone`, the guide log's line, the primary mount's
    /// algorithms.
    fn notify(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        let (total_frames, dropped_frames) = (self.settle_frame_count, self.dropped_frame_count);
        if self.succeeded {
            debug(ev, "PhdController complete: success");
            ev.push(ControlEvent::SettleDone { error: None, total_frames, dropped_frames });
            ev.push(ControlEvent::SettlingStateChange("Settling complete"));
        } else {
            debug(ev, format!("PhdController complete: fail: {}", self.error_msg));
            // `Status` is 0 for an empty message, whatever happened.
            let error = (!self.error_msg.is_empty()).then(|| self.error_msg.clone());
            ev.push(ControlEvent::SettleDone { error, total_frames, dropped_frames });
            ev.push(ControlEvent::SettlingStateChange("Settling failed"));
        }
        let algo = cx.gcx.algo;
        if let Some(m) = cx.gcx.primary.as_deref_mut() {
            let mut mev = Vec::new();
            m.mount_mut().notify_guiding_dither_settle_done(self.succeeded, algo, &mut mev);
            wrap(ev, MountRole::Primary, mev);
        }
    }

    /// The guider's events, passed on. `MyFrame::NotifyGuidingStopped`
    /// aborts the controller right after the `GuidingStopped` event, so the
    /// abort's events come there; the auto-exposure steps are applied where
    /// they come, not passed on. The engine sends every guider call's events
    /// through here.
    pub fn forward(&mut self, gev: Vec<GuiderEvent>, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        for e in gev {
            match e {
                GuiderEvent::AdjustAutoExposure(snr) => self.adjust_auto_exposure(snr, ev),
                GuiderEvent::ResetAutoExposure => self.reset_auto_exposure(ev),
                e => {
                    let stopped = e == GuiderEvent::GuidingStopped;
                    ev.push(ControlEvent::Guider(e));
                    if stopped {
                        self.abort("Guiding stopped", cx, ev);
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------- capture loop

    /// `CaptureActive`: looping, or a guide's exposures.
    pub fn capture_active(&self) -> bool {
        self.capture_active
    }

    /// `m_continueCapturing`: another exposure follows this one; a frame
    /// that arrives without it stops guiding.
    pub fn continue_capturing(&self) -> bool {
        self.continue_capturing
    }

    /// The exposure scheduled and not yet complete (`m_exposurePending`).
    pub fn exposure_request(&self) -> Option<ExposureRequest> {
        self.exposure
    }

    /// `ScheduleExposure`: the next exposure, at the current duration, on
    /// the guider's bounding box — or the single exposure's. PHD2's
    /// raw-image mode (the bad-pixel map refinement) is not ported, so it
    /// is always a light frame.
    fn schedule_exposure(&mut self, cx: &ControlCx, ev: &mut Vec<ControlEvent>) {
        let req = ExposureRequest {
            duration_ms: self.requested_exposure_duration(cx.camera_connected),
            subframe: match self.single {
                Some(s) => s.subframe,
                None => cx.guider.bounding_box(cx.frame_size),
            },
        };
        debug(
            ev,
            format!(
                "ScheduleExposure({},3,{}) exposurePending={}",
                req.duration_ms,
                !req.subframe.is_empty() as i32,
                self.exposure.is_some() as i32
            ),
        );
        debug(ev, "Enqueuing Expose request");
        self.exposure = Some(req);
    }

    /// `MyFrame::StartLooping` (the `loop` method): loop, unless already;
    /// while calibrating or guiding, guiding stops and the loop goes on.
    /// False (PHD2's error) without a camera.
    pub fn start_looping(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) -> bool {
        if !cx.camera_connected {
            debug(ev, "Error thrown from myframe.cpp:1958->Camera not connected");
            return false;
        }
        if self.capture_active {
            if !cx.guider.is_calibrating_or_guiding() {
                return true;
            }
            // No `UpdateImageDisplay` here, unlike `StopCapturing`.
            let mut gev = Vec::new();
            cx.guider.stop_guiding(&mut cx.gcx, &mut gev);
            self.forward(gev, cx, ev);
        }
        ev.push(ControlEvent::Status("Looping".into()));
        self.start_capturing(cx, ev);
        true
    }

    /// The capture loop's part of `MyFrame::StartSingleExposure`, once the
    /// engine has checked the camera, shown "Capturing single exposure" and
    /// set its binning and gain (`SingleExposure::Activate`): `StartCapturing`
    /// for that one exposure, which [`ControlEvent::SingleFrameComplete`]
    /// ends. Only while capture is not active (the event server refuses it
    /// otherwise). Quirk kept: a `loop` during it does nothing, capture
    /// being active, so capture stops after its frame.
    pub fn start_single_exposure(&mut self, single: SingleExposure, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        self.single = Some(single);
        self.start_capturing(cx, ev);
    }

    /// `StartCapturing`: start looping, unless already; a single exposure
    /// takes one frame and stops.
    pub fn start_capturing(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        debug(
            ev,
            format!(
                "StartCapturing CaptureActive={} continueCapturing={} exposurePending={}",
                self.capture_active as i32,
                self.continue_capturing as i32,
                self.exposure.is_some() as i32
            ),
        );
        if !self.capture_active {
            if self.single.is_none() {
                self.continue_capturing = true;
            }
            self.capture_active = true;
            ev.push(ControlEvent::CaptureStarted);
            if self.exposure.is_none() {
                self.schedule_exposure(cx, ev);
            }
        }
    }

    /// `StopCapturing`. A pause ends first, with no exposure scheduled for
    /// it — what Ekos' `set_paused false` then `stop_capture` relies on.
    /// With no exposure out the loop ends now (guiding stopped); with one,
    /// false: the engine interrupts it (`RequestStop`), and its completion
    /// ends the loop.
    pub fn stop_capturing(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) -> bool {
        debug(
            ev,
            format!(
                "StopCapturing CaptureActive={} continueCapturing={} exposurePending={}",
                self.capture_active as i32,
                self.continue_capturing as i32,
                self.exposure.is_some() as i32
            ),
        );
        let mut finished = true;
        let continue_capturing = self.continue_capturing;
        if cx.guider.is_paused() {
            // Cleared first, so that un-pausing schedules no exposure.
            self.continue_capturing = false;
            self.set_paused(PauseType::None, cx, ev);
        }
        if continue_capturing || self.exposure.is_some() {
            ev.push(ControlEvent::Status("Waiting for devices...".into()));
            self.continue_capturing = false;
            if self.exposure.is_some() {
                finished = false;
            } else {
                self.capture_active = false;
                self.stop_guiding(cx, ev);
                self.finish_stop(cx, ev);
            }
        }
        finished
    }

    /// `StopGuiding` and `UpdateImageDisplay` while calibrating or guiding.
    fn stop_guiding(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        if cx.guider.is_calibrating_or_guiding() {
            let mut gev = Vec::new();
            cx.guider.stop_guiding(&mut cx.gcx, &mut gev);
            let img = cx.guider.current_image().clone();
            cx.guider.display_image(img, cx.gcx.gamma, &mut gev);
            self.forward(gev, cx, ev);
        }
    }

    /// `FinishStop`: the loop ended, and a single exposure with it. The next
    /// one starts on a full frame (a client can auto-select a star then) at
    /// the longest auto exposure; a guide or dither in progress fails.
    fn finish_stop(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        if self.single.take().is_some() {
            ev.push(ControlEvent::SingleFrameComplete { error: None });
        }
        ev.push(ControlEvent::LoopingStopped);
        let mut gev = Vec::new();
        cx.guider.force_full_frame(&mut gev);
        self.forward(gev, cx, ev);
        self.reset_auto_exposure(ev);
        ev.push(ControlEvent::Status("Stopped.".into()));
        self.abort("Stopped capturing", cx, ev);
    }

    /// The top of `OnExposeComplete`: the exposure is no longer out. False
    /// when the frame is dropped — a full pause takes no frames and
    /// schedules none.
    pub fn on_expose_complete(&mut self, guider: &Guider, ev: &mut Vec<ControlEvent>) -> bool {
        debug(ev, "OnExposeComplete: enter");
        self.exposure = None;
        if guider.pause_type() == PauseType::Full {
            debug(ev, "guider is paused, ignoring frame, not scheduling exposure");
            return false;
        }
        true
    }

    /// `OnExposeComplete` for a failed exposure, an interrupted one
    /// included: capture and guiding stop, a single exposure fails, and a
    /// guide or dither in progress fails ("Image capture stopped" when a
    /// stop interrupted it). Quirk kept: when the loop was still running,
    /// `FinishStop` has already sent `LoopingExposuresStopped`, and it is
    /// sent again.
    pub fn on_capture_error(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        debug(ev, "OnExposeComplete: Capture Error reported");
        let stopping = !self.continue_capturing;
        self.stop_capturing(cx, ev);
        self.stop_guiding(cx, ev);
        if self.single.take().is_some() {
            ev.push(ControlEvent::SingleFrameComplete { error: Some("camera error -- see debug log") });
        }
        ev.push(ControlEvent::LoopingStopped);
        let mut gev = Vec::new();
        cx.guider.reset(false, &mut cx.gcx, &mut gev);
        self.forward(gev, cx, ev);
        self.capture_active = self.continue_capturing;
        let reason = if stopping { "Image capture stopped" } else { "Error reported capturing image" };
        self.abort(reason, cx, ev);
        ev.push(ControlEvent::Status("Stopped.".into()));
        debug(ev, "Error thrown from myframe_events.cpp:557->Error reported capturing image");
    }

    /// The end of `OnExposeComplete`, once the guider has the frame:
    /// `UpdateControllerState`, then the next exposure, or the end of the
    /// loop when it was stopped.
    pub fn after_frame(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        self.update(cx, ev);
        debug(
            ev,
            format!(
                "OnExposeComplete: CaptureActive={} m_continueCapturing={}",
                self.capture_active as i32, self.continue_capturing as i32
            ),
        );
        self.capture_active = self.continue_capturing;
        if self.capture_active {
            self.schedule_exposure(cx, ev);
        } else {
            self.finish_stop(cx, ev);
        }
    }

    /// `MyFrame::SetPaused`: `Guiding` stops the corrections, `Full` the
    /// exposures too; `None` resumes, telling the primary mount's
    /// algorithms and scheduling the next exposure when the loop goes on
    /// and none is out. Only a change is announced: pausing while paused,
    /// even to another kind, does nothing.
    pub fn set_paused(&mut self, pause: PauseType, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
        let is_paused = cx.guider.is_paused();
        debug(
            ev,
            format!(
                "SetPaused type={} isPaused={} exposurePending={}",
                pause as i32,
                is_paused as i32,
                self.exposure.is_some() as i32
            ),
        );
        if pause != PauseType::None && !is_paused {
            let mut gev = Vec::new();
            cx.guider.set_paused(pause, &mut gev);
            self.forward(gev, cx, ev);
            let kind = if pause == PauseType::Full { "/full" } else { "/looping" };
            ev.push(ControlEvent::Status(format!("Paused{kind}")));
            ev.push(ControlEvent::ServerCommand("PAUSE"));
            ev.push(ControlEvent::Paused);
        } else if pause == PauseType::None && is_paused {
            let mut gev = Vec::new();
            cx.guider.set_paused(PauseType::None, &mut gev);
            self.forward(gev, cx, ev);
            let algo = cx.gcx.algo;
            if let Some(m) = cx.gcx.primary.as_deref_mut() {
                debug(ev, "un-pause: clearing mount guide algorithm history");
                let mut mev = Vec::new();
                m.notify_guiding_resumed(algo, &mut mev);
                wrap(ev, MountRole::Primary, mev);
            }
            if self.continue_capturing && self.exposure.is_none() {
                self.schedule_exposure(cx, ev);
            }
            ev.push(ControlEvent::Status("Resumed".into()));
            ev.push(ControlEvent::ServerCommand("RESUME"));
            ev.push(ControlEvent::Resumed);
        }
    }

    /// `MyFrame::StartGuiding`: with a star selected, the camera and the
    /// mount connected, calibrate or guide (the guider decides) and make
    /// sure capture runs; the spiral starts over. True when it started.
    /// PHD2's refusal while its Refine Bad-pixel Map window is open is not
    /// ported (no such window).
    pub fn start_guiding(&mut self, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) -> bool {
        ev.push(ControlEvent::CheckLogRollover);
        let mount_connected = cx.gcx.primary.as_deref().is_some_and(|m| m.mount().is_connected());
        if !(mount_connected && cx.camera_connected && cx.guider.state() >= GuiderState::Selected) {
            return false;
        }
        let mut gev = Vec::new();
        cx.guider.start_guiding(&mut cx.gcx, &mut gev);
        self.forward(gev, cx, ev);
        self.start_capturing(cx, ev);
        self.spiral.reset(ev);
        true
    }

    /// `MyFrame::AutoSelectStar`: pick and lock on the best star of the
    /// current frame (within `roi`); refused while calibrating or guiding.
    /// True when a star was selected; `find_star` then answers with the
    /// guider's lock position (event_server.cpp:1006), or fails when it is
    /// invalid. Its saturation level comes as [`ControlEvent::SaturationAdu`].
    pub fn auto_select_star(&mut self, roi: Option<Rect>, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) -> bool {
        if cx.guider.is_calibrating_or_guiding() {
            debug(ev, "cannot auto-select star while calibrating or guiding");
            return false;
        }
        // PHD2 reads these live; the engine's copies can be a frame old (the
        // exposure adjusted by the frame this controller update follows),
        // so they are refreshed, and stay so for the rest of the call.
        cx.gcx.settling = self.is_settling();
        (cx.gcx.exposure_ms, cx.gcx.auto_exposure) = self.exposure_info(cx.camera_connected);
        let mut gev = Vec::new();
        // From SELECTING the guider runs the frame again to reach SELECTED:
        // no move comes of it.
        let (star, upd) = cx.guider.auto_select(roi, &mut cx.gcx, &mut gev);
        self.forward(gev, cx, ev);
        if let Some(adu) = upd.saturation_adu {
            ev.push(ControlEvent::SaturationAdu(adu));
        }
        star.is_some()
    }

    // ---------------------------------------------------------------- dither

    /// `MyFrame::Dither`: an offset of up to `amount` × the scale factor
    /// (random, or the spiral's next step), the lock moved by it in mount
    /// coordinates, and an AO bump when the AO is set to bump on a dither.
    /// False when not guiding or when the lock could not move.
    fn frame_dither(&mut self, amount: f64, ra_only: bool, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) -> bool {
        if !cx.guider.is_guiding() {
            debug(ev, "Error thrown from myframe.cpp:2078->cannot dither if not guiding");
            return false;
        }
        let amount = amount * self.dither_scale;
        let (d_ra, d_dec) = match self.dither_mode {
            DitherMode::Spiral => self.spiral.get_dither(amount, ra_only, ev),
            DitherMode::Random => {
                // `rand() / RAND_MAX`, RA first.
                let d_ra = amount * (self.rng.next_f64() * 2.0 - 1.0);
                let d_dec = if ra_only { 0.0 } else { amount * (self.rng.next_f64() * 2.0 - 1.0) };
                (d_ra, d_dec)
            }
        };
        debug(ev, format!("dither: size={}, dRA={} dDec={}", fmt_f(amount, 2), fmt_f(d_ra, 2), fmt_f(d_dec, 2)));
        let mut gev = Vec::new();
        let moved = cx.guider.move_lock_position(Point::new(d_ra, d_dec), &mut cx.gcx, &mut gev);
        self.forward(gev, cx, ev);
        if !moved {
            debug(ev, "Error thrown from myframe.cpp:2102->move lock failed");
            return false;
        }
        ev.push(ControlEvent::Status(format!("Dither by {},{}", fmt_f(d_ra, 2), fmt_f(d_dec, 2))));
        let lock = cx.guider.lock_position().unwrap_or_default();
        ev.push(ControlEvent::GuidingDithered { dx: d_ra, dy: d_dec, lock });
        if let Some(m) = cx.gcx.primary.as_deref_mut() {
            let mut mev = Vec::new();
            m.dither_bump(&mut mev);
            wrap(ev, MountRole::Primary, mev);
        }
        true
    }

    pub fn dither_mode(&self) -> DitherMode {
        self.dither_mode
    }

    /// `SetDitherMode`.
    pub fn set_dither_mode(&mut self, mode: DitherMode, profile: &mut dyn ProfileAccess, ev: &mut Vec<ControlEvent>) {
        debug(ev, format!("set dither mode {}", mode as i32));
        self.dither_mode = mode;
        profile.set_i32(keys::DITHER_MODE, mode as i32);
    }

    pub fn dither_ra_only(&self) -> bool {
        self.dither_ra_only
    }

    /// `SetDitherRaOnly`.
    pub fn set_dither_ra_only(&mut self, ra_only: bool, profile: &mut dyn ProfileAccess) {
        self.dither_ra_only = ra_only;
        profile.set_i32(keys::DITHER_RA_ONLY, ra_only as i32);
    }

    pub fn dither_scale_factor(&self) -> f64 {
        self.dither_scale
    }

    /// `SetDitherScaleFactor`: false (and 1.0) for a factor ≤ 0.
    pub fn set_dither_scale_factor(
        &mut self,
        factor: f64,
        profile: &mut dyn ProfileAccess,
        ev: &mut Vec<ControlEvent>,
    ) -> bool {
        // Written as PHD2's test, so a NaN gets through as it does there.
        let ok = !(factor <= 0.0);
        if ok {
            self.dither_scale = factor;
        } else {
            debug(ev, "Error thrown from myframe.cpp:2231->ditherScaleFactor <= 0");
            self.dither_scale = 1.0;
        }
        profile.set_f64(keys::DITHER_SCALE_FACTOR, self.dither_scale);
        ok
    }

    // -------------------------------------------------------------- exposure

    /// The durations offered, ms, the custom one last (`get_exposure_durations`).
    pub fn exposure_durations(&self) -> &[i32] {
        &self.durations
    }

    /// `m_exposureDuration`: the fixed exposure, or auto exposure's current
    /// one.
    pub fn exposure_duration(&self) -> i32 {
        self.exposure_ms
    }

    /// `GetExposureInfo`: the exposure and whether it is automatic; 0 and
    /// false without a camera.
    pub fn exposure_info(&self, camera_connected: bool) -> (i32, bool) {
        if camera_connected { (self.exposure_ms, self.auto_exp.enabled) } else { (0, false) }
    }

    /// `RequestedExposureDuration` (`get_exposure`): the single exposure's
    /// during one; 0 without a camera.
    pub fn requested_exposure_duration(&self, camera_connected: bool) -> i32 {
        match self.single {
            _ if !camera_connected => 0,
            Some(s) => s.duration_ms,
            None => self.exposure_ms,
        }
    }

    pub fn auto_exposure(&self) -> AutoExposureCfg {
        self.auto_exp
    }

    /// The exposure setting as `ExposureDurationSummary` prints it.
    pub fn exposure_summary(&self) -> ExposureSummary {
        let a = &self.auto_exp;
        let v = &self.var_delay;
        ExposureSummary {
            duration_ms: self.exposure_ms,
            auto: a.enabled.then_some((a.min_ms, a.max_ms, a.target_snr)),
            var_delay: v.enabled.then_some((v.short_ms, v.long_ms)),
        }
    }

    /// `NotifyExposureChanged`: the `Exposure` parameter change, and the
    /// setting saved (−1 for auto).
    fn notify_exposure_changed(&mut self, profile: &mut dyn ProfileAccess, ev: &mut Vec<ControlEvent>) {
        ev.push(ControlEvent::ParamChange { name: "Exposure", value: Value::from(self.exposure_summary().text()) });
        profile.set_i32(keys::EXPOSURE_MS, if self.auto_exp.enabled { -1 } else { self.exposure_ms });
    }

    /// `SetExposureDuration` (`set_exposure`): a negative value turns auto
    /// exposure on (its exposure stays as it was until it adjusts);
    /// otherwise only an offered duration is taken, and false for any
    /// other. A change is announced and selects the matching dark.
    pub fn set_exposure_duration(
        &mut self,
        ms: i32,
        profile: &mut dyn ProfileAccess,
        ev: &mut Vec<ControlEvent>,
    ) -> bool {
        if ms < 0 {
            if !self.auto_exp.enabled {
                debug(ev, "AutoExp: enabled");
                self.auto_exp.enabled = true;
                self.notify_exposure_changed(profile, ev);
            }
            return true;
        }
        if !self.durations.contains(&ms) {
            return false;
        }
        if self.auto_exp.enabled || self.exposure_ms != ms {
            debug(ev, format!("OnExposureDurationSelected: duration = {ms}"));
            self.exposure_ms = ms;
            self.auto_exp.enabled = false;
            self.notify_exposure_changed(profile, ev);
            ev.push(ControlEvent::SelectDark(ms));
        }
        true
    }

    /// `SetCustomExposureDuration`: the last duration offered. False (PHD2's
    /// error) for one of the standard durations, and for one its only
    /// caller, the custom-duration dialog (myframe_events.cpp:85), does not
    /// let through: 0 < s < 3600, truncated to ms — the port refuses the 0
    /// a value under 1 ms truncates to. The exposure follows when it was
    /// the custom one, whether auto exposure is on or not.
    pub fn set_custom_exposure_duration(
        &mut self,
        ms: i32,
        profile: &mut dyn ProfileAccess,
        ev: &mut Vec<ControlEvent>,
    ) -> bool {
        let custom = self.durations.len() - 1;
        if !(1..3_600_000).contains(&ms) || self.durations[..custom].contains(&ms) {
            return false;
        }
        if self.exposure_ms == self.durations[custom] && self.durations[custom] != ms {
            self.exposure_ms = ms;
            self.notify_exposure_changed(profile, ev);
        }
        self.durations[custom] = ms;
        profile.set_i32(keys::CUSTOM_EXPOSURE, ms);
        true
    }

    /// `SetAutoExposureCfg`: saved; true when it changed.
    fn store_auto_exposure_cfg(
        &mut self,
        min_ms: i32,
        max_ms: i32,
        target_snr: f64,
        profile: &mut dyn ProfileAccess,
        ev: &mut Vec<ControlEvent>,
    ) -> bool {
        debug(ev, format!("AutoExp: config min = {min_ms} max = {max_ms} snr = {}", fmt_f(target_snr, 2)));
        profile.set_i32(keys::AUTO_EXP_MIN, min_ms);
        profile.set_i32(keys::AUTO_EXP_MAX, max_ms);
        profile.set_f64(keys::AUTO_EXP_SNR, target_snr);
        let a = &mut self.auto_exp;
        let changed = a.min_ms != min_ms || a.max_ms != max_ms || a.target_snr != target_snr;
        (a.min_ms, a.max_ms, a.target_snr) = (min_ms, max_ms, target_snr);
        changed
    }

    /// The settings dialog's auto exposure (`UnloadValues`): the maximum no
    /// shorter than the minimum, saved, and announced when auto exposure is
    /// on and something changed; whether it changed. `None`, and nothing
    /// set, for what the dialog cannot pick: a minimum or maximum that is
    /// not an offered duration, an SNR outside its 3.5–99.9.
    pub fn set_auto_exposure_cfg(
        &mut self,
        min_ms: i32,
        max_ms: i32,
        target_snr: f64,
        profile: &mut dyn ProfileAccess,
        ev: &mut Vec<ControlEvent>,
    ) -> Option<bool> {
        let offered = |ms| self.durations.contains(&ms);
        if !offered(min_ms) || !offered(max_ms) || !(3.5..=99.9).contains(&target_snr) {
            return None;
        }
        let max_ms = if max_ms < min_ms { min_ms } else { max_ms };
        let changed = self.store_auto_exposure_cfg(min_ms, max_ms, target_snr, profile, ev);
        if self.auto_exp.enabled && changed {
            self.notify_exposure_changed(profile, ev);
        }
        Some(changed)
    }

    /// `ResetAutoExposure`: under auto exposure, back to the longest.
    pub fn reset_auto_exposure(&mut self, ev: &mut Vec<ControlEvent>) {
        if self.auto_exp.enabled {
            debug(ev, format!("AutoExp: reset exp to {}", self.auto_exp.max_ms));
            self.exposure_ms = self.auto_exp.max_ms;
        }
    }

    /// `AdjustAutoExposure`: under auto exposure, towards the exposure that
    /// would give the target SNR (SNR taken as growing with its square
    /// root), quickly below the target (α 0.20), slowly above (α 0.15),
    /// within the limits. Below SNR 1 the star is as good as lost: back to
    /// the longest.
    pub fn adjust_auto_exposure(&mut self, snr: f64, ev: &mut Vec<ControlEvent>) {
        let a = self.auto_exp;
        if !a.enabled {
            return;
        }
        if snr < 1.0 {
            debug(ev, format!("AutoExp: low SNR ({}), reset exp to {}", fmt_f(snr, 2), a.max_ms));
            self.exposure_ms = a.max_ms;
            return;
        }
        let r = a.target_snr / snr;
        let mut exp = self.exposure_ms as f64;
        let new_exp = exp * r * r;
        let alpha = if snr < a.target_snr { 0.20 } else { 0.15 };
        exp += alpha * (new_exp - exp);
        self.exposure_ms = (exp + 0.5).floor() as i32;
        if self.exposure_ms < a.min_ms {
            self.exposure_ms = a.min_ms;
        } else if self.exposure_ms > a.max_ms {
            self.exposure_ms = a.max_ms;
        }
        debug(ev, format!("AutoExp: adjust SNR={} new exposure {}", fmt_f(snr, 2), self.exposure_ms));
    }

    // ----------------------------------------------------------------- delay

    pub fn time_lapse(&self) -> i32 {
        self.time_lapse_ms
    }

    /// `SetTimeLapse`: false (and 0) for a negative delay.
    pub fn set_time_lapse(&mut self, ms: i32, profile: &mut dyn ProfileAccess, ev: &mut Vec<ControlEvent>) -> bool {
        let ok = ms >= 0;
        if ok {
            self.time_lapse_ms = ms;
        } else {
            debug(ev, "Error thrown from myframe.cpp:2707->timeLapse < 0");
            self.time_lapse_ms = 0;
        }
        profile.set_i32(keys::TIME_LAPSE, self.time_lapse_ms);
        ok
    }

    pub fn var_delay(&self) -> VarDelayCfg {
        self.var_delay
    }

    /// `SetVariableDelayConfig`. The long delay used is at least the short
    /// one; quirk kept: the profile keeps the long delay as given.
    pub fn set_variable_delay_config(
        &mut self,
        enabled: bool,
        short_ms: i32,
        long_ms: i32,
        profile: &mut dyn ProfileAccess,
        ev: &mut Vec<ControlEvent>,
    ) {
        debug(
            ev,
            format!(
                "Variable delay: {}, Short = {short_ms} ms, Long = {long_ms} ms",
                if enabled { "Enabled" } else { "Disabled" }
            ),
        );
        let long_used = if long_ms < short_ms { short_ms } else { long_ms }; // wxMax
        self.var_delay = VarDelayCfg { enabled, short_ms, long_ms: long_used };
        profile.set_i32(keys::VAR_DELAY_SHORT, short_ms);
        profile.set_i32(keys::VAR_DELAY_LONG, long_ms);
        profile.set_i32(keys::VAR_DELAY_ENABLED, enabled as i32);
    }

    /// `GetExposureDelay`: the wait before an exposure, ms — the time lapse,
    /// or with the variable delay on, the long delay while guiding with
    /// nothing in progress (no guide or dither in progress, no recenter) and
    /// the mount guiding, else the short one. A new value is logged.
    pub fn exposure_delay(&mut self, guider: &Guider, guiding_enabled: bool, ev: &mut Vec<ControlEvent>) -> i32 {
        let v = self.var_delay;
        let delay = if !v.enabled {
            self.time_lapse_ms
        } else if guider.is_guiding() && self.is_idle() && !guider.is_recentering() && guiding_enabled {
            v.long_ms
        } else {
            v.short_ms
        };
        if delay != self.last_exposure_delay {
            debug(ev, format!("Exposure delay set to {delay}"));
            self.last_exposure_delay = delay;
        }
        delay
    }
}

/// `TheScope()->SetDecGuideMode(mode)`: the scope is the secondary mount
/// behind an AO, else the primary.
fn set_scope_dec_guide_mode(mode: DecGuideMode, cx: &mut ControlCx, ev: &mut Vec<ControlEvent>) {
    let GuiderCx { primary, secondary, profile, .. } = &mut cx.gcx;
    let behind_ao = primary.as_deref().is_some_and(|m| m.mount().is_step_guider());
    let (role, m) = if behind_ao {
        (MountRole::Secondary, secondary.as_deref_mut())
    } else {
        (MountRole::Primary, primary.as_deref_mut())
    };
    if let Some(scope) = m.and_then(|m| m.scope_mut()) {
        let mut mev = Vec::new();
        scope.set_dec_guide_mode(mode as i32, profile, &mut mev);
        wrap(ev, role, mev);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::f64::consts::PI;
    use std::sync::Arc;

    use super::*;
    use crate::engine::algo::AlgoCtx;
    use crate::engine::calibration::{Adjustment, Calibration, ScopePointing};
    use crate::engine::guider::ScheduledMove;
    use crate::engine::image::UsImage;
    use crate::engine::mount::{GuideOutput, Mount, PointingSource, StepFrame};
    use crate::engine::multistar::FindParams;
    use crate::engine::scope::{CalibrationEnv, CalibrationNext, Scope, ScopeIo};
    use crate::engine::star::FindMode;
    use crate::engine::traits::{CancelToken, Dir, MoveOpts, MoveResult, Parity, PierSide};

    #[derive(Default)]
    struct Profile {
        f: HashMap<String, f64>,
        i: HashMap<String, i32>,
        s: HashMap<String, String>,
    }

    impl ProfileAccess for Profile {
        fn has(&self, key: &str) -> bool {
            self.f.contains_key(key) || self.i.contains_key(key) || self.s.contains_key(key)
        }
        fn get_f64(&self, key: &str, default: f64) -> f64 {
            self.f.get(key).copied().or(self.i.get(key).map(|&v| v as f64)).unwrap_or(default)
        }
        fn get_i32(&self, key: &str, default: i32) -> i32 {
            self.i.get(key).copied().unwrap_or(default)
        }
        fn get_str(&self, key: &str, default: &str) -> String {
            self.s.get(key).cloned().unwrap_or_else(|| default.to_string())
        }
        fn set_f64(&mut self, key: &str, value: f64) {
            self.f.insert(key.into(), value);
        }
        fn set_i32(&mut self, key: &str, value: i32) {
            self.i.insert(key.into(), value);
        }
        fn set_str(&mut self, key: &str, value: &str) {
            self.s.insert(key.into(), value.into());
        }
    }

    /// Records the pulses; the bench then moves the star by them.
    #[derive(Default)]
    struct Pulses(Vec<(Dir, u32)>);

    impl GuideOutput for Pulses {
        fn guide(&mut self, dir: Dir, ms: u32, _cancel: &CancelToken) -> MoveResult {
            self.0.push((dir, ms));
            MoveResult::Ok
        }
    }

    const W: i32 = 200;
    const H: i32 = 200;
    /// px per ms of pulse: West moves the star +x, North +y.
    const RATE: f64 = 0.01;
    const FP: FindParams = FindParams { mode: FindMode::Centroid, saturation_by_adu: true, saturation_adu: 0 };

    /// Fixed pattern noise in −20..=20 ADU.
    fn noise(x: i32, y: i32) -> f64 {
        let mut h = (x as u32).wrapping_mul(73856093) ^ (y as u32).wrapping_mul(19349663);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1e995);
        h ^= h >> 15;
        ((h % 41) as i32 - 20) as f64
    }

    /// A 16-bit frame: 1000 ADU, the noise and a Gaussian star (σ 1.6 px,
    /// 2000 ADU high).
    fn render(star: Option<Point>, frame: u32) -> Arc<UsImage> {
        let mut img = UsImage::new(W, H);
        img.bits_per_pixel = 16;
        img.frame_num = frame;
        for y in 0..H {
            for x in 0..W {
                let mut v = 1000.0 + noise(x, y);
                if let Some(s) = star {
                    let (dx, dy) = (x as f64 - s.x, y as f64 - s.y);
                    v += 2000.0 * (-(dx * dx + dy * dy) / (2.0 * 1.6 * 1.6)).exp();
                }
                *img.pixel_mut(x, y) = (v + 0.5).floor() as u16;
            }
        }
        Arc::new(img)
    }

    /// The bench's scope as the primary mount, with an AO bump in progress
    /// when told so — all the controller reads of an AO.
    struct BenchMount<'a> {
        scope: &'a mut Scope,
        bump: bool,
    }

    impl GuiderMount for BenchMount<'_> {
        fn mount(&self) -> &Mount {
            &self.scope.mount
        }
        fn mount_mut(&mut self) -> &mut Mount {
            &mut self.scope.mount
        }
        fn is_calibrated(&self) -> bool {
            GuiderMount::is_calibrated(&*self.scope)
        }
        fn calibration_tot_distance(&self) -> i32 {
            GuiderMount::calibration_tot_distance(&*self.scope)
        }
        fn begin_calibration(
            &mut self,
            location: Option<Point>,
            env: &CalibrationEnv,
            pointing: Option<&dyn PointingSource>,
            profile: &mut dyn ProfileAccess,
            events: &mut Vec<MountEvent>,
        ) -> bool {
            GuiderMount::begin_calibration(self.scope, location, env, pointing, profile, events)
        }
        fn update_calibration_state(
            &mut self,
            cur: Point,
            env: &CalibrationEnv,
            pointing: Option<&dyn PointingSource>,
            profile: &mut dyn ProfileAccess,
            events: &mut Vec<MountEvent>,
        ) -> CalibrationNext {
            GuiderMount::update_calibration_state(self.scope, cur, env, pointing, profile, events)
        }
        fn adjust_calibration_for_scope_pointing(
            &mut self,
            pointing: &ScopePointing,
            timestamp: &str,
            profile: &mut dyn ProfileAccess,
            events: &mut Vec<MountEvent>,
        ) -> Adjustment {
            GuiderMount::adjust_calibration_for_scope_pointing(self.scope, pointing, timestamp, profile, events)
        }
        fn notify_guiding_stopped(&mut self, ctx: &AlgoCtx, profile: &mut dyn ProfileAccess, ev: &mut Vec<MountEvent>) {
            GuiderMount::notify_guiding_stopped(self.scope, ctx, profile, ev)
        }
        fn clear_calibration(&mut self) {
            GuiderMount::clear_calibration(self.scope)
        }
        fn is_bump_in_progress(&self) -> bool {
            self.bump
        }
        fn scope_mut(&mut self) -> Option<&mut Scope> {
            Some(self.scope)
        }
    }

    /// The controller in front of a guider on a scope that moves a star,
    /// with the engine's part played by [`Bench::step`].
    struct Bench {
        ctl: Control,
        guider: Guider,
        scope: Scope,
        /// An AO bump in progress, as the primary mount tells it.
        bump: bool,
        profile: Profile,
        star: Point,
        visible: bool,
        /// Added to the star on each frame.
        drift: Point,
        camera: bool,
        env: CalibrationEnv,
        sp: ScopePointing,
        algo: AlgoCtx,
        mono_ms: i64,
        now_ms: i64,
        frames: u32,
        ev: Vec<ControlEvent>,
    }

    impl Bench {
        fn new() -> Bench {
            let mut profile = Profile::default();
            let mut ev = Vec::new();
            let ctl = Control::new(&mut profile, &mut ev);
            let guider = Guider::new(&mut profile, false, &mut Vec::new());
            let mut scope = Scope::new(&mut profile, false, 0.0, &mut Vec::new());
            scope.mount.connect();
            Bench {
                ctl,
                guider,
                scope,
                bump: false,
                profile,
                star: Point::new(100.0, 100.0),
                visible: true,
                drift: Point::default(),
                camera: true,
                env: CalibrationEnv {
                    binning: 1,
                    focal_length: 400,
                    pixel_size: 3.8,
                    image_scale: 2.0,
                    rotator_angle: None,
                    fast_recenter: true,
                    max_move_pixels: 15,
                    can_pulse_guide: true,
                    timestamp: "T".into(),
                },
                sp: ScopePointing {
                    declination: None,
                    pier_side: PierSide::Unknown,
                    rotator_angle: None,
                    binning: 1,
                    guide_rates: None,
                    camera_pixel_size: 3.8,
                    profile_pixel_size: 3.8,
                },
                algo: AlgoCtx::default(),
                mono_ms: 0,
                now_ms: 1_700_000_000_000,
                frames: 0,
                ev,
            }
        }

        fn call<R>(&mut self, f: impl FnOnce(&mut Control, &mut ControlCx, &mut Vec<ControlEvent>) -> R) -> R {
            let (exposure_ms, auto_exposure) = self.ctl.exposure_info(self.camera);
            let ra_only = self.scope.dec_guide_mode() == DecGuideMode::None;
            let mut primary = BenchMount { scope: &mut self.scope, bump: self.bump };
            let mut cx = ControlCx {
                guider: &mut self.guider,
                gcx: GuiderCx {
                    primary: Some(&mut primary),
                    secondary: None,
                    profile: &mut self.profile,
                    pointing: None,
                    cal_env: &self.env,
                    scope_pointing: &self.sp,
                    algo: &self.algo,
                    find: FP,
                    use_subframes: false,
                    ra_only,
                    settling: self.ctl.is_settling(),
                    exposure_ms,
                    auto_exposure,
                    guide_time: 0.0,
                    now_ms: self.now_ms,
                    busy: false,
                    gamma: 0.4,
                },
                camera_connected: self.camera,
                frame_size: (W, H),
                mono_ms: self.mono_ms,
            };
            f(&mut self.ctl, &mut cx, &mut self.ev)
        }

        /// The engine's cycle: the exposure asked for (the clocks advance
        /// by it), the frame through the guider, the controller, then the
        /// moves. False when no exposure was out.
        fn step(&mut self) -> bool {
            let Some(req) = self.ctl.exposure_request() else {
                return false;
            };
            self.mono_ms += req.duration_ms as i64;
            self.now_ms += req.duration_ms as i64;
            self.frames += 1;
            self.star = self.star + self.drift;
            let img = render(self.visible.then_some(self.star), self.frames);
            if !self.call(|c, cx, ev| c.on_expose_complete(cx.guider, ev)) {
                return true;
            }
            let upd = self.call(|c, cx, ev| {
                let mut gev = Vec::new();
                let stopping = !c.continue_capturing();
                let upd = cx.guider.update_guide_state(Some(img), stopping, &mut cx.gcx, &mut gev);
                c.forward(gev, cx, ev);
                c.after_frame(cx, ev);
                upd
            });
            for mv in &upd.moves {
                let mut out = Pulses::default();
                let cancel = CancelToken::new();
                let mut io = ScopeIo { output: &mut out, pointing: None, cancel: &cancel, now: 0.0 };
                let mut mev = Vec::new();
                match *mv {
                    ScheduledMove::Calibration { dir, amount, .. } => {
                        self.scope.move_axis(dir, amount, MoveOpts::CALIBRATION_MOVE, &mut io, &mut mev);
                    }
                    ScheduledMove::Offset { mut ofs, opts } => {
                        let (algo, profile) = (&self.algo, &mut self.profile);
                        self.scope.move_offset(&mut ofs, opts, StepFrame::default, algo, &mut io, profile, &mut mev);
                    }
                }
                for (dir, ms) in out.0 {
                    let d = RATE * ms as f64;
                    match dir {
                        Dir::West => self.star.x += d,
                        Dir::East => self.star.x -= d,
                        Dir::North => self.star.y += d,
                        Dir::South => self.star.y -= d,
                    }
                }
                self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    cx.guider.move_complete(&mut cx.gcx, &mut gev);
                    c.forward(gev, cx, ev);
                });
            }
            true
        }

        /// Steps until the controller is idle again; the frames taken.
        fn settle(&mut self, max: usize) -> usize {
            for n in 1..=max {
                assert!(self.step(), "no exposure out");
                if self.ctl.is_idle() {
                    return n;
                }
            }
            panic!("not settled in {max} frames");
        }

        /// The calibration the bench's sky gives.
        fn calibrate_scope(&mut self) {
            let cal = Calibration {
                x_rate: RATE,
                y_rate: RATE,
                x_angle: PI,
                y_angle: PI / 2.0,
                ra_guide_parity: Parity::Even,
                dec_guide_parity: Parity::Even,
                is_valid: true,
                ..Calibration::default()
            };
            self.scope.set_calibration(&cal, "T", &mut self.profile, &mut Vec::new());
        }

        /// Guiding on a calibrated scope, settled, the events cleared.
        fn guiding() -> Bench {
            let mut b = Bench::new();
            b.calibrate_scope();
            let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 0, timeout_s: 60, frames: 99999 };
            b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
            b.settle(10);
            assert!(b.guider.is_guiding());
            b.ev.clear();
            b
        }

        fn set_dec_mode(&mut self, mode: DecGuideMode) {
            self.scope.set_dec_guide_mode(mode as i32, &mut self.profile, &mut Vec::new());
        }

        fn debug(&self, line: &str) -> bool {
            self.ev.contains(&ControlEvent::Debug(line.into()))
        }

        fn count(&self, f: impl Fn(&ControlEvent) -> bool) -> usize {
            self.ev.iter().filter(|e| f(e)).count()
        }

        fn settling(&self) -> Vec<(f64, f64, bool)> {
            self.ev
                .iter()
                .filter_map(|e| match *e {
                    ControlEvent::Settling { distance, time, star_locked, .. } => Some((distance, time, star_locked)),
                    _ => None,
                })
                .collect()
        }

        fn settle_done(&self) -> Option<(Option<String>, i32, i32)> {
            self.ev.iter().find_map(|e| match e {
                ControlEvent::SettleDone { error, total_frames, dropped_frames } => {
                    Some((error.clone(), *total_frames, *dropped_frames))
                }
                _ => None,
            })
        }

        fn dithered(&self) -> Option<(f64, f64, Point)> {
            self.ev.iter().find_map(|e| match *e {
                ControlEvent::GuidingDithered { dx, dy, lock } => Some((dx, dy, lock)),
                _ => None,
            })
        }
    }

    const TIMEOUT_ERROR: &str = "timed-out waiting for guider to settle";

    #[test]
    fn guide_calibrates_then_settles_after_ten_seconds_in_range() {
        let mut b = Bench::new();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.guide(GuideOpts { force_recal: false, use_sticky_lock: true }, settle, None, cx, ev))
            .unwrap();
        assert!(b.ctl.capture_active());
        assert!(b.debug("PhdController: start capturing"));
        assert!(b.ev.contains(&ControlEvent::CaptureStarted));
        assert!(b.debug("ScheduleExposure(1000,3,0) exposurePending=0"));

        // The first frame has no star selected: AutoSelect picks it.
        b.step();
        assert!(b.debug("PhdController: newstate STATE_WAIT_SELECTED"));
        // The next one finds it selected: calibration starts.
        b.step();
        assert!(b.debug("PhdController: start calibration"));
        assert!(b.guider.is_calibrating());
        assert!(b.guider.lock_pos_is_sticky());
        let mut frames = 0;
        while !b.ev.contains(&ControlEvent::SettleBegin) {
            assert!(b.step());
            frames += 1;
            assert!(frames < 80, "no calibration");
        }
        assert!(GuiderMount::is_calibrated(&b.scope));
        // The sticky lock is the user's again.
        assert!(!b.guider.lock_pos_is_sticky());
        assert!(b.ctl.is_settling());
        assert!(!b.ctl.is_dithering());

        let begin = b.mono_ms;
        b.settle(40);
        assert!(b.guider.is_guiding());
        let s = b.settling();
        let (error, total, dropped) = b.settle_done().unwrap();
        assert_eq!((error, dropped), (None, 0));
        assert_eq!(total as usize, s.len() + 1);
        // In range for the last ten frames, timed from the first of them:
        // 0, 1, …, 9 s, and done on the eleventh, 10 s after it.
        let n = s.len();
        let times: Vec<f64> = s[n - 10..].iter().map(|x| x.1).collect();
        assert_eq!(times, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        assert!(s[n - 10..].iter().all(|x| x.0 <= 1.5 && x.2));
        assert!(s[..n - 10].iter().all(|x| x.0 > 1.5 && x.1 == 0.0));
        assert_eq!(b.mono_ms - begin, total as i64 * 1000);
        let tail: Vec<&ControlEvent> = b.ev.iter().rev().take(6).collect();
        assert!(tail.contains(&&ControlEvent::SettlingStateChange("Settling complete")));
        assert!(b.debug("PhdController complete: success"));
        assert!(b.ev.contains(&ControlEvent::Mount(
            MountRole::Primary,
            MountEvent::Debug("Mount: notify guiding dither settle done success=1".into())
        )));
        assert!(b.ctl.is_idle());
        assert_eq!(b.ctl.exposure_delay(&b.guider, true, &mut Vec::new()), 0);
    }

    #[test]
    fn a_zero_settle_time_is_done_on_the_first_frame_in_range() {
        let mut b = Bench::new();
        b.calibrate_scope();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 0, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        // Selected on the first frame, guiding (the calibrated short-cut)
        // on the second; STATE_GUIDE waits a frame before SETTLE_BEGIN, so
        // the settle begins on the third and is done on the fourth.
        assert_eq!(b.settle(10), 4);
        assert_eq!(b.settle_done(), Some((None, 1, 0)));
        assert!(b.settling().is_empty());
        let lines = [
            "PhdController: newstate STATE_CALIBRATE",
            "PhdController: newstate STATE_GUIDE",
            "PhdController: newstate STATE_SETTLE_BEGIN",
            "PhdController: newstate STATE_SETTLE_WAIT",
            "PhdController: settling, locked = 1, distance = 0.00 (1.50) aobump = 0 frame = 1 / 99999",
            "PhdController: newstate STATE_FINISH",
            "PhdController complete: success",
            "PhdController: newstate STATE_IDLE",
        ];
        let at = |l: &str| b.ev.iter().position(|e| *e == ControlEvent::Debug(l.into())).unwrap();
        let pos: Vec<usize> = lines.iter().map(|l| at(l)).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{pos:?}");

        // Already guiding: a new guide only settles.
        b.ev.clear();
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        assert!(b.debug("PhdController: guider state = 6"));
        assert!(b.ctl.is_settling());
        assert_eq!(b.settle(3), 1);
        assert_eq!(b.settle_done(), Some((None, 1, 0)));
    }

    #[test]
    fn the_timeout_fails_the_settle() {
        let mut b = Bench::new();
        b.calibrate_scope();
        // No corrections and a drift: the star walks away from the lock.
        b.scope.mount.set_guiding_enabled(false, &b.algo, &mut Vec::new());
        b.drift = Point::new(1.0, 0.0);
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 5, frames: 99999 };
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        for _ in 0..3 {
            b.step();
        }
        assert!(b.ev.contains(&ControlEvent::SettleBegin));
        let begin = b.mono_ms;
        assert_eq!(b.settle(10), 5);
        assert_eq!(b.mono_ms - begin, 5000);
        assert_eq!(b.settle_done(), Some((Some(TIMEOUT_ERROR.into()), 5, 0)));
        let s = b.settling();
        assert_eq!(s.len(), 4);
        assert!(s.iter().all(|x| x.0 > 1.5 && x.1 == 0.0 && x.2), "{s:?}");
        assert!(b.debug(&format!("PhdController failed: {TIMEOUT_ERROR}")));
        assert!(b.ev.contains(&ControlEvent::SettlingStateChange("Settling failed")));
    }

    #[test]
    fn three_failed_auto_finds_fail_the_guide() {
        let mut b = Bench::new();
        b.visible = false;
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        assert_eq!(b.settle(5), 3);
        for n in [3, 2, 1] {
            assert!(b.debug(&format!("auto find star failed, attempts remaining = {n}")));
        }
        assert_eq!(b.settle_done(), Some((Some("failed to find a suitable guide star".into()), 0, 0)));
        // Looping goes on.
        assert!(b.ctl.capture_active());
        assert!(b.ctl.exposure_request().is_some());
    }

    #[test]
    fn a_forced_recalibration_clears_the_calibration_then_calibrates() {
        let mut b = Bench::guiding();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 0, timeout_s: 60, frames: 99999 };
        let opts = GuideOpts { force_recal: true, use_sticky_lock: true };
        b.call(|c, cx, ev| c.guide(opts, settle, None, cx, ev)).unwrap();
        // Guiding, straight to the calibration: no settle on the way.
        let lines = [
            "PhdController: newstate STATE_ATTEMPT_START",
            "PhdController: newstate STATE_CALIBRATE",
            "PhdController: clearing calibration",
            "PhdController: start calibration",
            "PhdController: newstate STATE_CALIBRATION_WAIT",
        ];
        let at = |l: &str| b.ev.iter().position(|e| *e == ControlEvent::Debug(l.into())).unwrap();
        let pos: Vec<usize> = lines.iter().map(|l| at(l)).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{pos:?}");
        assert!(!b.ev.contains(&ControlEvent::SettleBegin));
        assert!(!GuiderMount::is_calibrated(&b.scope));
        assert!(b.guider.is_calibrating());
        assert!(b.guider.lock_pos_is_sticky());

        b.ev.clear();
        b.settle(100);
        assert!(GuiderMount::is_calibrated(&b.scope));
        assert!(b.guider.is_guiding());
        assert_eq!(b.settle_done().unwrap().0, None);
        assert!(!b.guider.lock_pos_is_sticky());
    }

    #[test]
    fn a_calibration_that_cannot_start_gives_the_sticky_lock_back() {
        let mut b = Bench::new();
        assert!(!b.guider.lock_pos_is_sticky());
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 };
        let opts = GuideOpts { force_recal: false, use_sticky_lock: true };
        b.call(|c, cx, ev| c.guide(opts, settle, None, cx, ev)).unwrap();
        b.step();
        assert!(b.debug("PhdController: newstate STATE_WAIT_SELECTED"));
        // The camera drops out before the star is selected: the next frame
        // finds it selected, and calibration cannot start.
        b.camera = false;
        b.ev.clear();
        b.step();
        assert!(b.debug("PhdController: start calibration"));
        assert_eq!(b.settle_done(), Some((Some("could not start calibration".into()), 0, 0)));
        assert!(!b.guider.lock_pos_is_sticky());
        assert!(b.ctl.is_idle());
    }

    #[test]
    fn frames_without_the_star_are_dropped_from_the_settle() {
        let mut b = Bench::guiding();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 0, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        b.visible = false;
        b.step();
        b.step();
        b.visible = true;
        assert_eq!(b.settle(3), 1);
        assert_eq!(b.settle_done(), Some((None, 3, 2)));
        let locked: Vec<bool> = b.settling().iter().map(|s| s.2).collect();
        assert_eq!(locked, [false, false]);
        assert!(b.ev.iter().any(
            |e| matches!(e, ControlEvent::Debug(l) if l.starts_with("PhdController: settling, locked = 0,"))
        ));
    }

    #[test]
    fn an_ao_bump_holds_the_settle_back() {
        let mut b = Bench::guiding();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 2, timeout_s: 60, frames: 99999 };
        b.bump = true;
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        for _ in 0..5 {
            assert!(b.step());
        }
        // In range from the first frame, past the 2 s, still settling.
        assert!(b.ctl.is_settling());
        let times: Vec<f64> = b.settling().iter().map(|s| s.1).collect();
        assert_eq!(times, [0.0, 1.0, 2.0, 3.0, 4.0]);
        let bump_line = |l: &String| l.contains("aobump = 1 frame = 5 / 99999");
        assert!(b.ev.iter().any(|e| matches!(e, ControlEvent::Debug(l) if bump_line(l))));
        b.bump = false;
        assert_eq!(b.settle(3), 1);
        assert_eq!(b.settle_done(), Some((None, 6, 0)));
    }

    #[test]
    fn a_manual_dither_aborts_the_settle_in_progress() {
        let mut b = Bench::guiding();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.dither(3.0, false, settle, cx, ev)).unwrap();
        b.step();
        b.ev.clear();
        b.call(|c, cx, ev| c.dither_compat(2.0, cx, ev)).unwrap();
        assert_eq!(b.settle_done(), Some((Some("manual or phd1-style dither".into()), 1, 0)));
        let done = b.ev.iter().position(|e| matches!(e, ControlEvent::SettleDone { .. })).unwrap();
        let dithered = b.ev.iter().position(|e| matches!(e, ControlEvent::GuidingDithered { .. })).unwrap();
        assert!(done < dithered);
        assert!(b.ctl.is_dithering());
        // The new dither settles over its ten frames.
        b.ev.clear();
        assert_eq!(b.settle(12), 10);
        assert_eq!(b.settle_done(), Some((None, 10, 0)));
    }

    #[test]
    fn a_guide_or_dither_in_progress_refuses_another() {
        let mut b = Bench::new();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        let err = b.call(|c, cx, ev| c.dither(3.0, false, settle, cx, ev)).unwrap_err();
        assert_eq!(err, "Cannot initiate dither while guide is in progress");
        assert!(b.debug("PhdController::Dither reentrancy state = 3 op = 1"));
        let err = b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap_err();
        assert_eq!(err, "Cannot initiate guide while guide is in progress");

        // Not guiding: the dither itself fails.
        let mut b = Bench::new();
        let err = b.call(|c, cx, ev| c.dither(3.0, false, settle, cx, ev)).unwrap_err();
        assert_eq!(err, "Dither error");
        assert!(b.debug("Error thrown from myframe.cpp:2078->cannot dither if not guiding"));
        assert!(b.ctl.is_idle());

        b.camera = false;
        assert_eq!(b.call(|_, cx, _| can_guide(cx)), Err("all equipment must be connected first"));
    }

    #[test]
    fn a_random_dither_moves_the_lock_then_settles() {
        let mut b = Bench::guiding();
        b.ctl.rng = Rng::new(7);
        let lock0 = b.guider.lock_position().unwrap();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 0, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.dither(8.0, false, settle, cx, ev)).unwrap();
        let (dx, dy, lock) = b.dithered().unwrap();
        // amount·U(−1, 1), RA then Dec, from the seeded source.
        let mut r = Rng::new(7);
        assert_eq!(dx, 8.0 * (r.next_f64() * 2.0 - 1.0));
        assert_eq!(dy, 8.0 * (r.next_f64() * 2.0 - 1.0));
        assert_eq!((fmt_f(dx, 6), fmt_f(dy, 6)), ("-1.554966".into(), "-1.506540".into()));
        assert_eq!(b.guider.lock_position(), Some(lock));
        // Equal rates at right angles: the lock moved by the dither's length.
        assert!(((lock - lock0).distance(&Point::default()) - dx.hypot(dy)).abs() < 1e-9);
        assert!(b.debug("dither: size=8.00, dRA=-1.55 dDec=-1.51"));
        assert!(b.ev.contains(&ControlEvent::Status("Dither by -1.55,-1.51".into())));
        let i = b.ev.iter().position(|e| matches!(e, ControlEvent::GuidingDithered { .. })).unwrap();
        assert_eq!(b.ev[i + 2], ControlEvent::SettleBegin);
        assert!(b.ctl.is_settling());
        assert!(b.ctl.is_dithering());

        // The error jumps by the dither, then the fast recenter brings the
        // star to the new lock.
        b.settle(20);
        assert_eq!(b.settle_done().unwrap().0, None);
        assert!(b.settling()[0].0 > 1.5);
        let recenter = |l: &String| l.starts_with("dither recenter");
        assert!(b.ev.iter().any(|e| matches!(e, ControlEvent::Guider(GuiderEvent::Debug(l)) if recenter(l))));
        assert!(b.star.distance(&lock) < 1.5);

        // RA only: one draw, no Dec.
        b.ev.clear();
        b.call(|c, cx, ev| c.dither(3.0, true, settle, cx, ev)).unwrap();
        let (dx, dy, _) = b.dithered().unwrap();
        assert_eq!((dx, dy), (3.0 * (r.next_f64() * 2.0 - 1.0), 0.0));
    }

    #[test]
    fn the_dec_guide_mode_shapes_the_dither() {
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 0, timeout_s: 60, frames: 99999 };
        let mode_change = |m: &str| {
            ControlEvent::Mount(
                MountRole::Primary,
                MountEvent::GuideParamChange { name: "Dec Guide Mode".into(), value: Value::from(m) },
            )
        };

        // North with a settle: Auto while settling, North again after.
        let mut b = Bench::guiding();
        b.set_dec_mode(DecGuideMode::North);
        b.call(|c, cx, ev| c.dither(3.0, false, settle, cx, ev)).unwrap();
        assert_ne!(b.dithered().unwrap().1, 0.0);
        assert!(b.debug("PhdController: setting Dec guide mode to Auto for dither settle"));
        assert!(b.ev.contains(&mode_change("Auto")));
        assert_eq!(b.scope.dec_guide_mode(), DecGuideMode::Auto);
        b.settle(20);
        assert!(b.debug("PhdController: restore Dec guide mode to North after dither"));
        assert!(b.ev.contains(&mode_change("North")));
        assert_eq!(b.scope.dec_guide_mode(), DecGuideMode::North);

        // North with a frame-count settle: RA only. The line names the mode
        // the last dither saved, which is North here too.
        b.ev.clear();
        b.call(|c, cx, ev| c.dither_compat(3.0, cx, ev)).unwrap();
        assert_eq!(b.dithered().unwrap().1, 0.0);
        assert!(b.debug("PhdController: forcing dither RA-only since Dec guide mode is North"));
        assert_eq!(b.scope.dec_guide_mode(), DecGuideMode::North);
        assert_eq!(b.settle(15), 10);
        assert_eq!(b.settle_done().unwrap().1, 10);

        // Off: RA only. Quirk kept: the line names the North the last dither
        // saved, not Off.
        b.ev.clear();
        b.set_dec_mode(DecGuideMode::None);
        b.call(|c, cx, ev| c.dither(3.0, false, settle, cx, ev)).unwrap();
        assert_eq!(b.dithered().unwrap().1, 0.0);
        assert!(b.debug("PhdController: forcing dither RA-only since Dec guide mode is North"));
        b.settle(20);

        // The profile's RA-only setting.
        b.ev.clear();
        b.set_dec_mode(DecGuideMode::Auto);
        b.call(|c, cx, _| c.set_dither_ra_only(true, cx.gcx.profile));
        b.call(|c, cx, ev| c.dither(3.0, false, settle, cx, ev)).unwrap();
        assert_eq!(b.dithered().unwrap().1, 0.0);
        assert_eq!(b.profile.get_i32(keys::DITHER_RA_ONLY, 0), 1);
    }

    /// From a harness of PHD2's `DitherSpiral` (myframe.cpp:2012-2061).
    #[test]
    fn the_spiral_matches_phd2() {
        let mut s = DitherSpiral::default();
        let mut ev = Vec::new();
        let two_d: Vec<(f64, f64)> = (0..16).map(|_| s.get_dither(1.0, false, &mut ev)).collect();
        assert_eq!(
            two_d,
            [
                (0.0, 1.0),
                (1.0, 0.0),
                (0.0, -1.0),
                (0.0, -1.0),
                (-1.0, 0.0),
                (-1.0, 0.0),
                (0.0, 1.0),
                (0.0, 1.0),
                (0.0, 1.0),
                (1.0, 0.0),
                (1.0, 0.0),
                (1.0, 0.0),
                (0.0, -1.0),
                (0.0, -1.0),
                (0.0, -1.0),
                (0.0, -1.0),
            ]
        );
        assert_eq!((s.x, s.y), (2, -2));
        assert!(ev.is_empty());
        // RA only starts over: x = 1, −1, −2, 2, 3, −3, −4, 4, 5, −5.
        let ra: Vec<f64> = (0..10).map(|_| s.get_dither(1.0, true, &mut ev).0).collect();
        assert_eq!(ra, [1.0, -2.0, -1.0, 4.0, 1.0, -6.0, -1.0, 8.0, 1.0, -10.0]);
        assert_eq!(s.x, -5);
        assert_eq!(ev, [ControlEvent::Debug("reset dither spiral".into())]);
        // And back.
        let back: Vec<(f64, f64)> = (0..4).map(|_| s.get_dither(2.5, false, &mut ev)).collect();
        assert_eq!(back, [(0.0, 2.5), (2.5, 0.0), (0.0, -2.5), (0.0, -2.5)]);
        assert_eq!(ev.len(), 2);
    }

    #[test]
    fn a_spiral_dither_takes_the_scale_factor_and_restarts_with_guiding() {
        let mut b = Bench::guiding();
        b.call(|c, cx, ev| {
            c.set_dither_mode(DitherMode::Spiral, cx.gcx.profile, ev);
            assert!(c.set_dither_scale_factor(2.0, cx.gcx.profile, ev));
        });
        let frames = SettleParams::frames(1);
        let mut steps = Vec::new();
        for _ in 0..3 {
            b.ev.clear();
            b.call(|c, cx, ev| c.dither(1.5, false, frames, cx, ev)).unwrap();
            let (dx, dy, _) = b.dithered().unwrap();
            steps.push((dx, dy));
            assert_eq!(b.settle(3), 1);
        }
        assert_eq!(steps, [(0.0, 3.0), (3.0, 0.0), (0.0, -3.0)]);
        assert_eq!(b.profile.get_i32(keys::DITHER_MODE, -1), 1);
        assert_eq!(b.profile.get_f64(keys::DITHER_SCALE_FACTOR, 0.0), 2.0);

        // A factor ≤ 0 falls back to 1.
        b.call(|c, cx, ev| assert!(!c.set_dither_scale_factor(0.0, cx.gcx.profile, ev)));
        assert!(b.debug("Error thrown from myframe.cpp:2231->ditherScaleFactor <= 0"));
        assert_eq!(b.ctl.dither_scale_factor(), 1.0);

        // Guiding again resets the spiral.
        b.ev.clear();
        b.call(|c, cx, ev| {
            let mut gev = Vec::new();
            cx.guider.stop_guiding(&mut cx.gcx, &mut gev);
            c.forward(gev, cx, ev);
            assert!(c.start_guiding(cx, ev));
        });
        assert!(b.debug("reset dither spiral"));
        assert!(b.ev.contains(&ControlEvent::CheckLogRollover));
        b.step();
        b.ev.clear();
        b.call(|c, cx, ev| c.dither(1.5, false, frames, cx, ev)).unwrap();
        assert_eq!(b.dithered().unwrap().1, 1.5);
    }

    #[test]
    fn pausing_and_resuming_announce_only_changes() {
        let mut b = Bench::guiding();
        assert!(b.ctl.exposure_request().is_some());
        b.call(|c, cx, ev| c.set_paused(PauseType::Guiding, cx, ev));
        assert_eq!(
            b.ev,
            [
                ControlEvent::Debug("SetPaused type=1 isPaused=0 exposurePending=1".into()),
                ControlEvent::Guider(GuiderEvent::Debug("Guider::SetPaused(1)".into())),
                ControlEvent::Status("Paused/looping".into()),
                ControlEvent::ServerCommand("PAUSE"),
                ControlEvent::Paused,
            ]
        );
        // Still looping, the guider only deduces.
        b.step();
        assert!(b.ctl.exposure_request().is_some());
        // A full pause on top changes nothing.
        b.ev.clear();
        b.call(|c, cx, ev| c.set_paused(PauseType::Full, cx, ev));
        assert_eq!(b.ev, [ControlEvent::Debug("SetPaused type=2 isPaused=1 exposurePending=1".into())]);
        assert_eq!(b.guider.pause_type(), PauseType::Guiding);

        b.ev.clear();
        b.call(|c, cx, ev| c.set_paused(PauseType::None, cx, ev));
        assert_eq!(
            b.ev,
            [
                ControlEvent::Debug("SetPaused type=0 isPaused=1 exposurePending=1".into()),
                ControlEvent::Guider(GuiderEvent::Debug("Guider::SetPaused(0)".into())),
                ControlEvent::Debug("un-pause: clearing mount guide algorithm history".into()),
                ControlEvent::Mount(MountRole::Primary, MountEvent::Debug("Mount: notify guiding resumed".into())),
                ControlEvent::Status("Resumed".into()),
                ControlEvent::ServerCommand("RESUME"),
                ControlEvent::Resumed,
            ]
        );
        b.ev.clear();
        b.call(|c, cx, ev| c.set_paused(PauseType::None, cx, ev));
        assert_eq!(b.ev.len(), 1);

        // A full pause drops the frame out and schedules no other; resuming
        // schedules the next.
        b.call(|c, cx, ev| c.set_paused(PauseType::Full, cx, ev));
        assert!(b.step());
        assert!(b.debug("guider is paused, ignoring frame, not scheduling exposure"));
        assert_eq!(b.ctl.exposure_request(), None);
        assert!(!b.step());
        b.ev.clear();
        b.call(|c, cx, ev| c.set_paused(PauseType::None, cx, ev));
        assert!(b.debug("ScheduleExposure(1000,3,1) exposurePending=0"));
        assert!(b.ev.contains(&ControlEvent::Resumed));
        assert!(b.step());
        assert!(b.guider.is_guiding());
    }

    #[test]
    fn stopping_while_paused_schedules_no_exposure() {
        let scheduled =
            |b: &Bench| b.count(|e| matches!(e, ControlEvent::Debug(l) if l.starts_with("ScheduleExposure")));

        // Paused fully, stopped: resumed without an exposure, then stopped.
        let mut b = Bench::guiding();
        b.call(|c, cx, ev| c.set_paused(PauseType::Full, cx, ev));
        b.step();
        b.ev.clear();
        assert!(b.call(|c, cx, ev| c.stop_capturing(cx, ev)));
        assert!(b.ev.contains(&ControlEvent::Resumed));
        assert_eq!(scheduled(&b), 0);
        assert!(b.ev.contains(&ControlEvent::Guider(GuiderEvent::GuidingStopped)));
        assert!(b.ev.contains(&ControlEvent::LoopingStopped));
        assert!(!b.ctl.capture_active());
        assert_eq!(b.ctl.exposure_request(), None);
        assert!(!b.guider.is_paused());

        // Ekos' order: set_paused false (an exposure goes out), then
        // stop_capture, which waits for it; when it lands, guiding stops
        // and nothing more is scheduled.
        let mut b = Bench::guiding();
        b.call(|c, cx, ev| c.set_paused(PauseType::Full, cx, ev));
        b.step();
        b.ev.clear();
        b.call(|c, cx, ev| c.set_paused(PauseType::None, cx, ev));
        assert_eq!(scheduled(&b), 1);
        assert!(!b.call(|c, cx, ev| c.stop_capturing(cx, ev)));
        assert!(b.ev.contains(&ControlEvent::Status("Waiting for devices...".into())));
        assert!(b.ctl.capture_active());
        assert!(!b.ctl.continue_capturing());
        assert!(b.step());
        assert_eq!(scheduled(&b), 1);
        assert!(b.ev.contains(&ControlEvent::Guider(GuiderEvent::GuidingStopped)));
        assert!(b.ev.contains(&ControlEvent::LoopingStopped));
        assert!(!b.ctl.capture_active());
        assert!(!b.step());
        assert_eq!(b.guider.state(), GuiderState::Selected);
    }

    #[test]
    fn stopping_aborts_the_controller() {
        // Guiding: stopping guiding aborts it before the loop's end does.
        let mut b = Bench::new();
        b.calibrate_scope();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        for _ in 0..4 {
            b.step();
        }
        assert_eq!(b.settling().len(), 1);
        b.ev.clear();
        // The exposure out lands after the stop, without the loop.
        assert!(!b.call(|c, cx, ev| c.stop_capturing(cx, ev)));
        assert!(b.step());
        let stopped = b.ev.iter().position(|e| *e == ControlEvent::Guider(GuiderEvent::GuidingStopped)).unwrap();
        assert_eq!(b.ev[stopped + 1], ControlEvent::Debug("PhdController failed: Guiding stopped".into()));
        assert_eq!(b.settle_done(), Some((Some("Guiding stopped".into()), 1, 0)));
        assert!(b.ctl.is_idle());
        assert_eq!(b.count(|e| matches!(e, ControlEvent::SettleDone { .. })), 1);

        // Looking for a star: the interrupted exposure fails it.
        let mut b = Bench::new();
        b.visible = false;
        b.call(|c, cx, ev| c.guide(GuideOpts::default(), settle, None, cx, ev)).unwrap();
        b.step();
        b.ev.clear();
        assert!(!b.call(|c, cx, ev| c.stop_capturing(cx, ev)));
        b.call(|c, cx, ev| {
            assert!(c.on_expose_complete(cx.guider, ev));
            c.on_capture_error(cx, ev);
        });
        assert_eq!(b.settle_done(), Some((Some("Image capture stopped".into()), 0, 0)));
        assert_eq!(b.count(|e| *e == ControlEvent::LoopingStopped), 1);
        assert!(b.debug("Error thrown from myframe_events.cpp:557->Error reported capturing image"));
        assert!(!b.ctl.capture_active());
        assert_eq!(b.ctl.exposure_request(), None);

        // A camera error while looping: the loop ends (twice announced).
        let mut b = Bench::new();
        b.call(|c, cx, ev| c.start_capturing(cx, ev));
        b.ev.clear();
        b.call(|c, cx, ev| {
            c.on_expose_complete(cx.guider, ev);
            c.on_capture_error(cx, ev);
        });
        assert_eq!(b.count(|e| *e == ControlEvent::LoopingStopped), 2);
        assert!(!b.ctl.capture_active());
        assert!(b.settle_done().is_none());
    }

    #[test]
    fn a_single_exposure_takes_one_frame_on_its_subframe() {
        let mut b = Bench::new();
        let single = SingleExposure { duration_ms: 2500, subframe: Rect::new(10, 20, 64, 48) };
        b.call(|c, cx, ev| c.start_single_exposure(single, cx, ev));
        assert_eq!(b.ctl.exposure_request(), Some(ExposureRequest { duration_ms: 2500, subframe: single.subframe }));
        assert!(b.debug("ScheduleExposure(2500,3,1) exposurePending=0"));
        assert!(b.ctl.capture_active());
        assert!(!b.ctl.continue_capturing());
        assert_eq!(b.call(|c, cx, _| c.requested_exposure_duration(cx.camera_connected)), 2500);
        // A loop asked for meanwhile does nothing.
        assert!(b.call(|c, cx, ev| c.start_looping(cx, ev)));
        assert!(!b.ctl.continue_capturing());

        // The frame ends it, then the loop.
        b.ev.clear();
        assert!(b.step());
        let done = ControlEvent::SingleFrameComplete { error: None };
        let i = b.ev.iter().position(|e| *e == done).unwrap();
        assert_eq!(b.ev[i + 1], ControlEvent::LoopingStopped);
        assert_eq!(b.count(|e| *e == done), 1);
        assert!(!b.ctl.capture_active());
        assert!(!b.step());
        assert_eq!(b.call(|c, cx, _| c.requested_exposure_duration(cx.camera_connected)), 1000);

        // A camera error fails it, once.
        b.call(|c, cx, ev| c.start_single_exposure(single, cx, ev));
        b.ev.clear();
        b.call(|c, cx, ev| {
            assert!(c.on_expose_complete(cx.guider, ev));
            c.on_capture_error(cx, ev);
        });
        let failed = ControlEvent::SingleFrameComplete { error: Some("camera error -- see debug log") };
        let i = b.ev.iter().position(|e| *e == failed).unwrap();
        assert_eq!(b.ev[i + 1], ControlEvent::LoopingStopped);
        assert_eq!(b.count(|e| matches!(e, ControlEvent::SingleFrameComplete { .. })), 1);
        assert!(!b.ctl.capture_active());

        // Looping again is on the guider's own frame and duration.
        b.ev.clear();
        assert!(b.call(|c, cx, ev| c.start_looping(cx, ev)));
        assert!(b.debug("ScheduleExposure(1000,3,0) exposurePending=0"));
        assert!(b.ctl.continue_capturing());
    }

    #[test]
    fn loop_starts_looping_and_stops_guiding() {
        let mut b = Bench::new();
        b.camera = false;
        assert!(!b.call(|c, cx, ev| c.start_looping(cx, ev)));
        assert!(b.debug("Error thrown from myframe.cpp:1958->Camera not connected"));
        assert!(!b.ctl.capture_active());

        b.camera = true;
        b.ev.clear();
        assert!(b.call(|c, cx, ev| c.start_looping(cx, ev)));
        assert_eq!(b.ev[0], ControlEvent::Status("Looping".into()));
        assert!(b.ev.contains(&ControlEvent::CaptureStarted));
        assert!(b.ctl.continue_capturing());
        // Already looping: nothing at all.
        b.ev.clear();
        assert!(b.call(|c, cx, ev| c.start_looping(cx, ev)));
        assert!(b.ev.is_empty());

        // Guiding: guiding stops (the controller with it), looping goes on
        // with the exposure already out.
        let mut b = Bench::guiding();
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.dither(3.0, false, settle, cx, ev)).unwrap();
        b.ev.clear();
        assert!(b.call(|c, cx, ev| c.start_looping(cx, ev)));
        assert!(!b.guider.is_calibrating_or_guiding());
        assert_eq!(b.settle_done(), Some((Some("Guiding stopped".into()), 0, 0)));
        let stopped = b.ev.iter().position(|e| *e == ControlEvent::Guider(GuiderEvent::GuidingStopped)).unwrap();
        let looping = b.ev.iter().position(|e| *e == ControlEvent::Status("Looping".into())).unwrap();
        assert!(stopped < looping);
        let display = |l: &String| l.starts_with("UpdateImageDisplay");
        assert!(!b.ev.iter().any(|e| matches!(e, ControlEvent::Guider(GuiderEvent::Debug(l)) if display(l))));
        assert!(b.debug("StartCapturing CaptureActive=1 continueCapturing=1 exposurePending=1"));
        assert!(b.step());
        assert!(b.ctl.capture_active());
        assert!(b.ctl.exposure_request().is_some());
    }

    #[test]
    fn only_offered_exposure_durations_are_taken() {
        let mut b = Bench::new();
        let summary = |s: &str| ControlEvent::ParamChange { name: "Exposure", value: Value::from(s) };
        assert_eq!(b.ctl.exposure_durations().len(), 22);
        assert_eq!(b.ctl.exposure_duration(), 1000);
        assert!(!b.ctl.auto_exposure().enabled);
        b.ev.clear();
        b.call(|c, cx, ev| {
            assert!(!c.set_exposure_duration(1234, cx.gcx.profile, ev));
            assert!(c.set_exposure_duration(2000, cx.gcx.profile, ev));
            assert!(c.set_exposure_duration(2000, cx.gcx.profile, ev));
        });
        assert_eq!(
            b.ev,
            [
                ControlEvent::Debug("OnExposureDurationSelected: duration = 2000".into()),
                summary("2000 ms"),
                ControlEvent::SelectDark(2000),
            ]
        );
        assert_eq!(b.profile.get_i32(keys::EXPOSURE_MS, 0), 2000);
        assert_eq!(b.call(|c, cx, _| c.requested_exposure_duration(cx.camera_connected)), 2000);

        // Auto: the exposure stays until it adjusts.
        b.ev.clear();
        b.call(|c, cx, ev| assert!(c.set_exposure_duration(-1, cx.gcx.profile, ev)));
        assert_eq!(
            b.ev,
            [ControlEvent::Debug("AutoExp: enabled".into()), summary("Auto (min = 1000 ms, max = 5000 ms, SNR = 6.00)")]
        );
        assert_eq!(b.profile.get_i32(keys::EXPOSURE_MS, 0), -1);
        assert_eq!(b.ctl.exposure_info(true), (2000, true));
        assert_eq!(b.ctl.exposure_info(false), (0, false));

        // The custom duration: the last one offered.
        b.ev.clear();
        b.call(|c, cx, ev| {
            assert!(!c.set_custom_exposure_duration(1500, cx.gcx.profile, ev));
            assert!(c.set_custom_exposure_duration(45000, cx.gcx.profile, ev));
            assert!(!c.set_exposure_duration(30000, cx.gcx.profile, ev));
            assert!(c.set_exposure_duration(45000, cx.gcx.profile, ev));
            // The exposure follows the custom duration it was.
            assert!(c.set_custom_exposure_duration(60000, cx.gcx.profile, ev));
        });
        assert_eq!(b.ctl.exposure_duration(), 60000);
        assert_eq!(b.ev.last(), Some(&summary("60000 ms")));
        assert_eq!(b.profile.get_i32(keys::CUSTOM_EXPOSURE, 0), 60000);
        // Only what the dialog lets through: 0 < s < 3600, in ms.
        b.call(|c, cx, ev| {
            for ms in [0, -1000, 3_600_000] {
                assert!(!c.set_custom_exposure_duration(ms, cx.gcx.profile, ev));
            }
            assert!(c.set_custom_exposure_duration(3_599_999, cx.gcx.profile, ev));
            assert!(c.set_custom_exposure_duration(60000, cx.gcx.profile, ev));
        });
        assert_eq!(b.ctl.exposure_duration(), 60000);

        // The variable delay joins the summary.
        b.call(|c, cx, ev| c.set_variable_delay_config(true, 2000, 1000, cx.gcx.profile, ev));
        assert_eq!(b.ctl.exposure_summary().text(), "60000 ms, VarDelay (short = 2000 ms, long = 2000 ms)");
        // Quirk kept: the profile keeps the long delay as given.
        assert_eq!(b.profile.get_i32(keys::VAR_DELAY_LONG, 0), 1000);
    }

    #[test]
    fn a_stored_duration_not_offered_leaves_auto_exposure_on() {
        let mut profile = Profile::default();
        profile.set_i32(keys::EXPOSURE_MS, 1234);
        profile.set_i32(keys::AUTO_EXP_MAX, 4000);
        let mut ev = Vec::new();
        let c = Control::new(&mut profile, &mut ev);
        assert!(c.auto_exposure().enabled);
        assert_eq!(c.exposure_duration(), 4000);
        let lines: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                ControlEvent::Debug(l) => Some(l.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            [
                "reset dither spiral",
                "set dither mode 0",
                "Variable delay: Disabled, Short = 1000 ms, Long = 10000 ms",
                "AutoExp: config min = 1000 max = 4000 snr = 6.00",
                "AutoExp: reset exp to 4000",
            ]
        );
        // A new profile is written with the defaults.
        assert_eq!(profile.get_f64(keys::DITHER_SCALE_FACTOR, 0.0), 1.0);
        assert_eq!(profile.get_i32(keys::TIME_LAPSE, -1), 0);
        assert_eq!(profile.get_f64(keys::AUTO_EXP_SNR, 0.0), 6.0);
    }

    #[test]
    fn auto_exposure_steps_towards_the_target_snr() {
        let mut b = Bench::new();
        let mut ev = Vec::new();
        b.ctl.set_exposure_duration(-1, &mut b.profile, &mut ev);
        b.ctl.reset_auto_exposure(&mut ev);
        assert_eq!(b.ctl.exposure_duration(), 5000);
        let mut exp = Vec::new();
        // Above the target, slowly down: 5000 + 0.15·(1250 − 5000) = 4437.5.
        for snr in [12.0, 12.0, 60.0, 60.0, 60.0, 3.0, 4.0, 0.5] {
            b.ctl.adjust_auto_exposure(snr, &mut ev);
            exp.push(b.ctl.exposure_duration());
        }
        assert_eq!(exp, [4438, 3939, 3354, 2856, 2432, 3891, 4864, 5000]);
        assert!(ev.contains(&ControlEvent::Debug("AutoExp: adjust SNR=12.00 new exposure 4438".into())));
        assert!(ev.contains(&ControlEvent::Debug("AutoExp: low SNR (0.50), reset exp to 5000".into())));
        // Only durations offered, and an SNR the dialog allows.
        assert_eq!(b.ctl.set_auto_exposure_cfg(1234, 3000, 6.0, &mut b.profile, &mut ev), None);
        assert_eq!(b.ctl.set_auto_exposure_cfg(2000, 3001, 6.0, &mut b.profile, &mut ev), None);
        assert_eq!(b.ctl.set_auto_exposure_cfg(2000, 3000, 3.0, &mut b.profile, &mut ev), None);
        assert_eq!(b.ctl.auto_exposure().max_ms, 5000);
        // Bounded by the limits.
        assert_eq!(b.ctl.set_auto_exposure_cfg(2000, 3000, 6.0, &mut b.profile, &mut ev), Some(true));
        assert_eq!(
            ev.last(),
            Some(&ControlEvent::ParamChange {
                name: "Exposure",
                value: Value::from("Auto (min = 2000 ms, max = 3000 ms, SNR = 6.00)")
            })
        );
        b.ctl.adjust_auto_exposure(60.0, &mut ev);
        assert_eq!(b.ctl.exposure_duration(), 3000);
        let mut exp = Vec::new();
        for _ in 0..3 {
            b.ctl.adjust_auto_exposure(600.0, &mut ev);
            exp.push(b.ctl.exposure_duration());
        }
        assert_eq!(exp, [2550, 2168, 2000]);
        b.ctl.adjust_auto_exposure(2.0, &mut ev);
        assert_eq!(b.ctl.exposure_duration(), 3000);

        // On the frames, in the guider's lines where PHD2 writes them
        // (inside `UpdateCurrentPosition`): a star found adjusts it, a star
        // lost resets it.
        fn line_at(ev: &[ControlEvent], start: &str) -> usize {
            let starts = |e: &ControlEvent| match e {
                ControlEvent::Debug(l) | ControlEvent::Guider(GuiderEvent::Debug(l)) => l.starts_with(start),
                _ => false,
            };
            ev.iter().position(starts).unwrap()
        }
        let mut b = Bench::guiding();
        b.call(|c, cx, ev| c.set_exposure_duration(-1, cx.gcx.profile, ev));
        b.ev.clear();
        b.step();
        assert!(b.ctl.exposure_duration() < 5000);
        assert!(line_at(&b.ev, "AutoExp: adjust SNR=") < line_at(&b.ev, "UpdateGuideState exits"));
        b.visible = false;
        b.ev.clear();
        b.step();
        assert_eq!(b.ctl.exposure_duration(), 5000);
        assert_eq!(b.ctl.exposure_request().unwrap().duration_ms, 5000);
        let reset = line_at(&b.ev, "AutoExp: reset exp to 5000");
        assert!(line_at(&b.ev, "Error thrown from guider_multistar.cpp:958->") < reset);
        assert!(reset < line_at(&b.ev, "Throw from guider.cpp:1371->"));
    }

    #[test]
    fn the_exposure_delay_follows_the_guiding() {
        let mut b = Bench::guiding();
        let mut ev = Vec::new();
        assert_eq!(b.ctl.exposure_delay(&b.guider, true, &mut ev), 0);
        b.ctl.set_time_lapse(500, &mut b.profile, &mut ev);
        assert!(!b.ctl.set_time_lapse(-1, &mut b.profile, &mut ev));
        assert_eq!(b.ctl.time_lapse(), 0);
        b.ctl.set_time_lapse(500, &mut b.profile, &mut ev);
        assert_eq!(b.ctl.exposure_delay(&b.guider, true, &mut ev), 500);

        b.ctl.set_variable_delay_config(true, 1000, 10000, &mut b.profile, &mut ev);
        ev.clear();
        assert_eq!(b.ctl.exposure_delay(&b.guider, true, &mut ev), 10000);
        assert_eq!(b.ctl.exposure_delay(&b.guider, true, &mut ev), 10000);
        // Logged once per change.
        assert_eq!(ev, [ControlEvent::Debug("Exposure delay set to 10000".into())]);
        // Guiding disabled, or a settle running: the short one.
        assert_eq!(b.ctl.exposure_delay(&b.guider, false, &mut ev), 1000);
        let settle = SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 };
        b.call(|c, cx, ev| c.dither(3.0, false, settle, cx, ev)).unwrap();
        assert_eq!(b.ctl.exposure_delay(&b.guider, true, &mut ev), 1000);
        // Not guiding: the short one.
        let mut b = Bench::new();
        b.ctl.set_variable_delay_config(true, 1000, 10000, &mut b.profile, &mut ev);
        assert_eq!(b.ctl.exposure_delay(&b.guider, true, &mut ev), 1000);
    }
}
