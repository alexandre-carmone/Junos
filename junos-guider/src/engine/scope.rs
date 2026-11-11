//! The scope: PHD2's `Scope`, the mount that guides with timed pulses.
//!
//! It owns its settings (calibration step and distance, the per-axis pulse
//! limits, the Dec guide mode, the calibration options), the backlash
//! compensation, `MoveAxis` (the Dec-mode filter, the clamps, the
//! pulse-limit alerts, the slew check) and the calibration state machine
//! that moves the star West, back East, clears the Dec backlash, North,
//! back South, nudges it home and stores the result.
//!
//! The calibration moves are not made here: [`Scope::update_calibration_state`]
//! hands back the move PHD2 schedules (`ScheduleAxisMove`) and the engine
//! makes it with [`Scope::move_axis`] before the next exposure.
//!
//! Ported from PHD2 src/scope.h, src/scope.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;

use super::algo::{AlgoCtx, MountClass};
use super::backlash::{BacklashComp, BacklashConfig, BlcEffects};
use super::calibration::{
    self, AdjustOpts, Adjustment, CALIBRATION_RATE_UNCALIBRATED, CALSTEP_DEFAULT_DISTANCE, CALSTEP_DEFAULT_STEPS,
    Calibration, CalibrationDetails, CalibrationIssue, ProfileAccess, ROTATOR_POSITION_UNKNOWN, ScopePointing,
    UNKNOWN_DECLINATION, degrees, norm_angle, radians, sidereal_rate_from_guide_speed,
};
use super::log::cfmt::fmt_f;
use super::mount::{
    AlertKind, AxisMover, CalibrationStepInfo, GuideOutput, GuiderOffset, Mount, MountEvent, MoveResultInfo,
    PointingSource, StepFrame, dump_move_option_bits, phd_angle,
};
use super::traits::{CancelToken, Dir, MoveOpts, MoveResult, Parity, PierSide, Point, c_int};

/// `DefaultCalibrationDuration`, ms.
pub const DEFAULT_CALIBRATION_DURATION: i32 = 750;
/// `DefaultMaxDecDuration`, ms.
pub const DEFAULT_MAX_DEC_DURATION: i32 = 2500;
/// `DefaultMaxRaDuration`, ms.
pub const DEFAULT_MAX_RA_DURATION: i32 = 2500;
/// The max-duration controls' range (`scope.cpp:49`), ms. The setters only
/// refuse negatives; the top also picks the limit alert's wording.
pub const MAX_DURATION_MIN: i32 = 50;
pub const MAX_DURATION_MAX: i32 = 8000;
/// `MAX_CALIBRATION_STEPS`: the West and North legs give up past it.
pub const MAX_CALIBRATION_STEPS: i32 = 60;

const LIMIT_REACHED_WARN_COUNT: i32 = 5;
const LIMIT_REACHED_GRACE_PERIOD_SECONDS: i64 = 120;
const MAX_NUDGES: i32 = 3;
const NUDGE_TOLERANCE: f64 = 2.0;
const BL_BACKLASH_MIN_COUNT: i32 = 3;
const BL_MAX_CLEARING_TIME: i32 = 60000;
const BL_MIN_CLEARING_DISTANCE: i32 = 3;

/// PHD2's `DEC_GUIDE_MODE`; the discriminants are what the profile stores
/// (`/scope/DecGuideMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecGuideMode {
    None = 0,
    Auto = 1,
    North = 2,
    South = 3,
}

impl DecGuideMode {
    pub fn from_i32(v: i32) -> Option<DecGuideMode> {
        Some(match v {
            0 => DecGuideMode::None,
            1 => DecGuideMode::Auto,
            2 => DecGuideMode::North,
            3 => DecGuideMode::South,
            _ => return None,
        })
    }

    /// `DecGuideModeStr`: `Off`, `Auto`, `North`, `South` — the
    /// `Dec Guide Mode` parameter change, the guide-log header and
    /// `get_dec_guide_mode`.
    pub fn label(self) -> &'static str {
        match self {
            DecGuideMode::None => "Off",
            DecGuideMode::Auto => "Auto",
            DecGuideMode::North => "North",
            DecGuideMode::South => "South",
        }
    }

    /// `set_dec_guide_mode`'s parse: any case of a [`label`](Self::label).
    pub fn parse(s: &str) -> Option<DecGuideMode> {
        [DecGuideMode::None, DecGuideMode::Auto, DecGuideMode::North, DecGuideMode::South]
            .into_iter()
            .find(|m| m.label().eq_ignore_ascii_case(s))
    }
}

/// `CALIBRATION_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibrationState {
    Cleared,
    GoWest,
    GoEast,
    ClearBacklash,
    GoNorth,
    GoSouth,
    NudgeSouth,
    Complete,
}

/// The devices a scope move reaches, and the engine clock.
pub struct ScopeIo<'a> {
    pub output: &'a mut dyn GuideOutput,
    pub pointing: Option<&'a dyn PointingSource>,
    pub cancel: &'a CancelToken,
    /// Engine clock, seconds: the pulse-limit alert's grace period and rate
    /// limit (wall-clock `time_t` seconds in PHD2).
    pub now: f64,
}

/// What the calibration reads from the rest of the rig.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalibrationEnv {
    /// `pCamera->Binning`.
    pub binning: u16,
    /// `pFrame->GetFocalLength()`, mm.
    pub focal_length: i32,
    /// `pCamera->GetCameraPixelSize()`, µm.
    pub pixel_size: f64,
    /// `pFrame->GetCameraPixelScale()`, ″/px.
    pub image_scale: f64,
    /// `Rotator::RotatorPosition()`, degrees; `None` without a rotator.
    pub rotator_angle: Option<f64>,
    /// `pGuider->IsFastRecenterEnabled()`.
    pub fast_recenter: bool,
    /// `pGuider->GetMaxMovePixels()`.
    pub max_move_pixels: i32,
    /// The guide output's `CanPulseGuide`.
    pub can_pulse_guide: bool,
    /// `wxDateTime::Now().Format()`, stamped on what a calibration stores.
    pub timestamp: String,
}

/// What [`Scope::update_calibration_state`] asks for next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibrationNext {
    /// Make this calibration move ([`Scope::move_axis`] with
    /// `MoveOpts::CALIBRATION_MOVE`) before the next frame.
    Move(Dir, i32),
    /// No move: wait for the next frame, or calibrated (see
    /// [`Scope::is_calibrated`]).
    NoMove,
    /// The calibration failed and was cleared (PHD2's `true` return).
    Failed,
}

/// What `MoveAxis` and the backlash compensation use, apart from the rest
/// so a move can borrow it next to the [`Mount`].
struct MoveState {
    max_ra_duration: i32,
    max_dec_duration: i32,
    dec_guide_mode: DecGuideMode,
    stop_guiding_when_slewing: bool,
    /// `m_limitReachedDeferralTime`, whole seconds.
    limit_deferral_time: i64,
    ra_limit_dir: Option<Dir>,
    ra_limit_count: i32,
    dec_limit_dir: Option<Dir>,
    dec_limit_count: i32,
    /// `AlertLimitReached`'s function-static `s_lastLogged`, shared by both
    /// axes.
    last_limit_alert: Option<i64>,
    /// `None` only while the scope is being built, as in PHD2.
    blc: Option<BacklashComp>,
}

fn debug(events: &mut Vec<MountEvent>, line: impl Into<String>) {
    events.push(MountEvent::Debug(line.into()));
}

/// `time_t` seconds of the engine clock.
fn whole_seconds(now: f64) -> i64 {
    now.floor() as i64
}

impl MoveState {
    /// `SetMaxDecDuration` (`scope.cpp:183`): negative is refused (2500
    /// then, without a notification); a change is a `Dec Max Duration`
    /// parameter change. Always written back.
    fn set_max_dec_duration(&mut self, ms: i32, profile: &mut impl ProfileAccess, events: &mut Vec<MountEvent>) -> bool {
        let error = ms < 0;
        if error {
            debug(events, "Error thrown from scope.cpp:191->maxDecDuration < 0");
            self.max_dec_duration = DEFAULT_MAX_DEC_DURATION;
        } else {
            if self.max_dec_duration != ms {
                events.push(MountEvent::GuideParamChange { name: "Dec Max Duration".into(), value: ms.into() });
            }
            self.max_dec_duration = ms;
        }
        profile.set_i32("/scope/MaxDecDuration", self.max_dec_duration);
        error
    }

    /// Carry out what a backlash-compensation call asks: the raised Dec
    /// maximum first (PHD2 raises it inside `SetCompValues`, before its
    /// notification), then the parameter changes, the profile writes and the
    /// debug lines.
    fn absorb(&mut self, fx: BlcEffects, profile: &mut impl ProfileAccess, events: &mut Vec<MountEvent>) {
        if let Some(ms) = fx.max_dec_duration {
            self.set_max_dec_duration(ms, profile, events);
        }
        for (name, value) in fx.param_changes {
            events.push(MountEvent::GuideParamChange { name: name.into(), value });
        }
        for (key, value) in fx.profile_writes {
            profile.set_i32(key, value as i32);
        }
        events.extend(fx.log.into_iter().map(MountEvent::Debug));
    }

    /// `Scope::Guide` with the slew check of PHD2's ASCOM mount
    /// (`scope_ascom.cpp:471`), done for every mount here: with
    /// `StopGuidingWhenSlewing` a slewing mount gets no pulse, and the move
    /// reports `Slewing` so the guider stops.
    fn guide(&self, dir: Dir, ms: i32, io: &mut ScopeIo, events: &mut Vec<MountEvent>) -> MoveResult {
        if self.stop_guiding_when_slewing && io.pointing.is_some_and(|p| p.is_slewing()) {
            debug(events, "Error thrown from scope_ascom.cpp:540->attempt to guide while slewing");
            events.push(MountEvent::Alert {
                msg: "Guiding stopped: the scope started slewing.".into(),
                kind: AlertKind::Warning,
            });
            return MoveResult::Slewing;
        }
        io.output.guide(dir, ms as u32, io.cancel)
    }

    /// `Scope::MoveAxis` (`scope.cpp:703`). Algorithm and deduced moves are
    /// filtered by the Dec guide mode and cut to the axis maximum; five
    /// limited moves in a row the same way raise the limit alert. Nothing
    /// moves with guiding disabled unless the move is manual.
    fn move_axis(
        &mut self,
        guiding_enabled: bool,
        dir: Dir,
        duration: i32,
        opts: MoveOpts,
        io: &mut ScopeIo,
        events: &mut Vec<MountEvent>,
    ) -> (MoveResult, MoveResultInfo) {
        let mut duration = duration;
        let mut result = MoveResult::Ok;
        let mut limit_reached = false;
        debug(events, format!("MoveAxis({}, {}, {})", dir.log_char(), duration, dump_move_option_bits(opts)));

        let failed = 'move_: {
            if !guiding_enabled && !opts.has(MoveOpts::MANUAL) {
                debug(events, "Throw from scope.cpp:716->Guiding disabled");
                break 'move_ true;
            }
            if opts.has(MoveOpts::ALGO_RESULT) || opts.has(MoveOpts::ALGO_DEDUCE) {
                match dir {
                    Dir::North | Dir::South => {
                        let mode = self.dec_guide_mode;
                        if mode == DecGuideMode::None
                            || (dir == Dir::South && mode == DecGuideMode::North)
                            || (dir == Dir::North && mode == DecGuideMode::South)
                        {
                            duration = 0;
                            debug(events, "duration set to 0 by GuideMode");
                        }
                        if duration > self.max_dec_duration {
                            duration = self.max_dec_duration;
                            debug(events, format!("duration set to {duration} by maxDecDuration"));
                            limit_reached = true;
                        }
                        if limit_reached && Some(dir) == self.dec_limit_dir {
                            self.dec_limit_count += 1;
                            if self.dec_limit_count >= LIMIT_REACHED_WARN_COUNT {
                                self.alert_limit_reached(duration, false, io, events);
                            }
                        } else {
                            self.dec_limit_count = 0;
                        }
                        self.dec_limit_dir = limit_reached.then_some(dir);
                    }
                    Dir::East | Dir::West => {
                        if duration > self.max_ra_duration {
                            duration = self.max_ra_duration;
                            debug(events, format!("duration set to {duration} by maxRaDuration"));
                            limit_reached = true;
                        }
                        if limit_reached && Some(dir) == self.ra_limit_dir {
                            self.ra_limit_count += 1;
                            if self.ra_limit_count >= LIMIT_REACHED_WARN_COUNT {
                                self.alert_limit_reached(duration, true, io, events);
                            }
                        } else {
                            self.ra_limit_count = 0;
                        }
                        self.ra_limit_dir = limit_reached.then_some(dir);
                    }
                }
            }
            if duration > 0 {
                result = self.guide(dir, duration, io, events);
                if result != MoveResult::Ok {
                    debug(events, "Error thrown from scope.cpp:795->guide failed");
                    break 'move_ true;
                }
            }
            false
        };
        if failed {
            if result == MoveResult::Ok {
                result = MoveResult::Error;
            }
            duration = 0;
        }
        debug(events, format!("Move returns status {}, amount {}", result as i32, duration));
        (result, MoveResultInfo { amount_moved: duration, limited: limit_reached })
    }

    /// `AlertLimitReached` (`scope.cpp:623`): at most one every 30 s (both
    /// axes together), none during the grace period after guiding is
    /// re-enabled. The wording depends on whether the axis maximum is at the
    /// top of its range, below the default (lowered by the user) or between.
    /// Quirk kept: PHD2 raises it through `SuppressibleAlert`, which drops
    /// the information icon, so the `Alert` event says "warning".
    fn alert_limit_reached(&mut self, duration: i32, ra: bool, io: &ScopeIo, events: &mut Vec<MountEvent>) {
        let now = whole_seconds(io.now);
        if let Some(last) = self.last_limit_alert
            && now < last + 30
        {
            return;
        }
        self.last_limit_alert = Some(now);
        if now < self.limit_deferral_time {
            return;
        }
        let pulse_guide = io.output.can_pulse_guide();
        let msg = if duration < MAX_DURATION_MAX {
            let default = if ra { DEFAULT_MAX_RA_DURATION } else { DEFAULT_MAX_DEC_DURATION };
            if duration >= default {
                // The maximum is probably fine: something else is wrong.
                match (ra, pulse_guide) {
                    (true, true) => "PHD2 is not able to make sufficient corrections in RA.  Check for cable snags, try re-doing your calibration, and check for problems with the mount mechanics.".to_string(),
                    (true, false) => "PHD2 is not able to make sufficient corrections in RA.  Check for cable snags, try re-doing your calibration, and confirm the ST-4 cable is working properly.".to_string(),
                    (false, true) => "PHD2 is not able to make sufficient corrections in Dec.  If the side-of-pier has changed from where you last calibrated, check to see if the 'Reverse Dec output option' on the Advanced Dialog guiding tab is wrong. If so, fix it and recalibrate.  Otherwise, check for cable snags, try re-doing your calibration, and check for problems with the mount mechanics.".to_string(),
                    (false, false) => "PHD2 is not able to make sufficient corrections in Dec.  Check for cable snags, try re-doing your calibration and confirm the ST-4 cable is working properly.".to_string(),
                }
            } else {
                // The user lowered the maximum: suggest the default back.
                let s = if ra { "Max RA Duration setting" } else { "Max Dec Duration setting" };
                format!(
                    "Your {s} is preventing PHD from making adequate corrections to keep the guide star locked. Try restoring {s} to its default value to allow PHD2 to make larger corrections."
                )
            }
        } else {
            format!(
                "Even using the maximum moves, PHD2 can't properly correct for the large guide star movements in {}. Guiding will be impaired until you can eliminate the source of these problems.",
                if ra { "RA" } else { "Dec" }
            )
        };
        events.push(MountEvent::Alert { msg, kind: AlertKind::Warning });
    }
}

/// The scope's [`AxisMover`] for [`Mount::move_offset`].
struct ScopeMover<'a, 'b, P: ProfileAccess> {
    mv: &'a mut MoveState,
    io: &'a mut ScopeIo<'b>,
    profile: &'a mut P,
}

impl<P: ProfileAccess> AxisMover for ScopeMover<'_, '_, P> {
    fn move_axis(
        &mut self,
        guiding_enabled: bool,
        dir: Dir,
        amount: i32,
        opts: MoveOpts,
        events: &mut Vec<MountEvent>,
    ) -> (MoveResult, MoveResultInfo) {
        self.mv.move_axis(guiding_enabled, dir, amount, opts, self.io, events)
    }

    fn track_blc(&mut self, opts: MoveOpts, y_raw: f64, min_move: f64, y_rate: f64, events: &mut Vec<MountEvent>) {
        if let Some(blc) = self.mv.blc.as_mut() {
            let fx = blc.track_blc_results(opts, y_raw, min_move, y_rate, self.mv.max_dec_duration);
            self.mv.absorb(fx, self.profile, events);
        }
    }

    fn apply_blc(&mut self, opts: MoveOpts, y_distance: f64, y_ms: &mut i32, events: &mut Vec<MountEvent>) {
        if let Some(blc) = self.mv.blc.as_mut() {
            let fx = blc.apply_backlash_comp(opts, y_distance, y_ms);
            self.mv.absorb(fx, self.profile, events);
        }
    }
}

/// The calibration state machine's working variables (`scope.h:98-127`).
#[derive(Clone, Debug)]
struct CalWork {
    state: CalibrationState,
    steps: i32,
    recenter_remaining: i32,
    recenter_duration: i32,
    initial_location: Point,
    /// The current leg's start; `None` until the first frame of a
    /// calibration.
    starting_location: Option<Point>,
    /// RA hours, Dec degrees at the leg's start, for the parities.
    starting_coords: Option<Point>,
    south_starting_location: Point,
    east_starting_location: Point,
    last_location: Point,
    total_south_amt: f64,
    north_dir_cos_x: f64,
    north_dir_cos_y: f64,
    east_alert_shown: bool,
    bl_marker_point: Point,
    bl_expected_backlash_step: f64,
    bl_last_cum_distance: f64,
    bl_accepted_moves: i32,
    bl_distance_moved: f64,
    bl_max_clearing_pulses: i32,
    ra_steps: i32,
    dec_steps: i32,
}

impl Default for CalWork {
    fn default() -> Self {
        CalWork {
            state: CalibrationState::Cleared,
            steps: 0,
            recenter_remaining: 0,
            recenter_duration: 0,
            initial_location: Point::default(),
            starting_location: None,
            starting_coords: None,
            south_starting_location: Point::default(),
            east_starting_location: Point::default(),
            last_location: Point::default(),
            total_south_amt: 0.0,
            north_dir_cos_x: 0.0,
            north_dir_cos_y: 0.0,
            east_alert_shown: false,
            bl_marker_point: Point::default(),
            bl_expected_backlash_step: 0.0,
            bl_last_cum_distance: 0.0,
            bl_accepted_moves: 0,
            bl_distance_moved: 0.0,
            bl_max_clearing_pulses: 0,
            ra_steps: 0,
            dec_steps: 0,
        }
    }
}

/// `GetRADecCoordinates`: (RA hours, Dec degrees), `None` when unknown.
fn ra_dec(pointing: Option<&dyn PointingSource>) -> Option<Point> {
    pointing.and_then(|p| p.coordinates()).map(|(ra, dec, _)| Point::new(ra, dec))
}

/// `MountCoords` (`scope.cpp:1159`): a camera vector on the axes of the
/// calibration being made.
fn mount_coords(v: Point, x_angle: f64, y_angle: f64) -> Point {
    let hyp = v.x.hypot(v.y);
    let theta = phd_angle(v, Point::default());
    let y_angle_error = norm_angle((x_angle - y_angle) + PI / 2.0);
    Point::new(hyp * (theta - x_angle).cos(), hyp * (theta - (x_angle + y_angle_error)).sin())
}

/// `DIV_ROUND_UP` on ints.
fn div_round_up(x: i32, y: i32) -> i32 {
    (x + y - 1) / y
}

/// PHD2's `Scope`.
pub struct Scope {
    /// The `Mount` half. Stopping guiding and re-enabling it go through the
    /// scope's own methods, which add the backlash reset and the alert
    /// deferral.
    pub mount: Mount,
    mv: MoveState,
    calibration_duration: i32,
    calibration_distance: i32,
    save_dec_guide_mode: DecGuideMode,
    cal_flip_requires_dec_flip: bool,
    assume_orthogonal: bool,
    use_dec_compensation: bool,
    has_hp_encoders: bool,
    /// `m_CalDetailsValidated` of the pointing source: its guide speeds are
    /// checked once per session.
    cal_details_validated: bool,
    /// `m_calibration`: the calibration being made, and a copy of the last
    /// one set (its Dec rate is what `is_calibrated` reads).
    calibration: Calibration,
    calibration_details: CalibrationDetails,
    prev_calibration: Calibration,
    prev_calibration_details: CalibrationDetails,
    last_calibration_issue: CalibrationIssue,
    cw: CalWork,
}

impl Scope {
    /// `Scope::Scope` and `Scope::Factory` (`scope.cpp:71`, `:476`): the
    /// settings from the profile through their setters, in PHD2's order, so
    /// the effective values are written back and the max durations and the
    /// Dec mode announce themselves as parameter changes (PHD2 starts them
    /// at 0 and Off). Then the backlash compensation, and the slew check,
    /// whose default is whether the mount can report slewing.
    pub fn new(
        profile: &mut impl ProfileAccess,
        can_check_slewing: bool,
        now: f64,
        events: &mut Vec<MountEvent>,
    ) -> Scope {
        let mount = Mount::new(MountClass::Scope, profile, now);
        let mut s = Scope {
            mount,
            mv: MoveState {
                max_ra_duration: 0,
                max_dec_duration: 0,
                dec_guide_mode: DecGuideMode::None,
                stop_guiding_when_slewing: false,
                limit_deferral_time: whole_seconds(now),
                ra_limit_dir: None,
                ra_limit_count: 0,
                dec_limit_dir: None,
                dec_limit_count: 0,
                last_limit_alert: None,
                blc: None,
            },
            calibration_duration: DEFAULT_CALIBRATION_DURATION,
            calibration_distance: CALSTEP_DEFAULT_DISTANCE,
            save_dec_guide_mode: DecGuideMode::None,
            cal_flip_requires_dec_flip: false,
            assume_orthogonal: false,
            use_dec_compensation: true,
            has_hp_encoders: false,
            cal_details_validated: false,
            calibration: Calibration::default(),
            calibration_details: CalibrationDetails::default(),
            prev_calibration: Calibration::default(),
            prev_calibration_details: CalibrationDetails::default(),
            last_calibration_issue: CalibrationIssue::None,
            cw: CalWork::default(),
        };
        let v = profile.get_i32("/scope/CalibrationDuration", DEFAULT_CALIBRATION_DURATION);
        s.set_calibration_duration(v, profile, events);
        let v = profile.get_i32("/scope/CalibrationDistance", CALSTEP_DEFAULT_DISTANCE);
        s.set_calibration_distance(v, profile, events);
        let v = profile.get_i32("/scope/MaxRaDuration", DEFAULT_MAX_RA_DURATION);
        s.set_max_ra_duration(v, profile, events);
        let v = profile.get_i32("/scope/MaxDecDuration", DEFAULT_MAX_DEC_DURATION);
        s.set_max_dec_duration(v, profile, events);
        let v = profile.get_i32("/scope/DecGuideMode", DecGuideMode::Auto as i32);
        s.set_dec_guide_mode(v, profile, events);
        // The guide algorithms come next in PHD2: `Mount::new` built them.
        let flip = profile.get_i32("/scope/CalFlipRequiresDecFlip", 0) != 0;
        s.set_calibration_flip_requires_dec_flip(flip, profile);
        let ortho = profile.get_i32("/scope/AssumeOrthogonal", 0) != 0;
        s.set_assume_orthogonal(ortho, profile);
        let dec_comp = profile.get_i32("/scope/UseDecComp", 1) != 0;
        s.enable_dec_compensation(dec_comp, profile);
        s.has_hp_encoders = profile.get_i32("/scope/HiResEncoders", 0) != 0;

        let cfg = {
            let p = &*profile;
            BacklashConfig::from_profile(&|k| p.has(k).then(|| f64::from(p.get_i32(k, 0))))
        };
        let (blc, fx) = BacklashComp::new(cfg, s.mv.max_dec_duration);
        s.mv.blc = Some(blc);
        s.mv.absorb(fx, profile, events);

        let stop = profile.get_i32("/scope/StopGuidingWhenSlewing", can_check_slewing as i32) != 0;
        s.enable_stop_guiding_when_slewing(stop, profile, events);
        s
    }

    // ------------------------------------------------------------ settings

    /// `GetCalibrationDuration`: the calibration step, ms.
    pub fn calibration_duration(&self) -> i32 {
        self.calibration_duration
    }

    /// `SetCalibrationDuration`: not positive → 750 ms. Always written back.
    pub fn set_calibration_duration(
        &mut self,
        ms: i32,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let error = ms <= 0;
        if error {
            debug(events, "Error thrown from scope.cpp:142->invalid calibrationDuration");
            self.calibration_duration = DEFAULT_CALIBRATION_DURATION;
        } else {
            self.calibration_duration = ms;
        }
        profile.set_i32("/scope/CalibrationDuration", self.calibration_duration);
        error
    }

    /// `GetCalibrationDistance`: how far each calibration leg goes, px.
    pub fn calibration_distance(&self) -> i32 {
        self.calibration_distance
    }

    /// `SetCalibrationDistance`: not positive → 25 px. Always written back.
    pub fn set_calibration_distance(
        &mut self,
        px: i32,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let error = px <= 0;
        if error {
            debug(events, "Error thrown from scope.cpp:167->invalid calibrationDistance");
            self.calibration_distance = CALSTEP_DEFAULT_DISTANCE;
        } else {
            self.calibration_distance = px;
        }
        profile.set_i32("/scope/CalibrationDistance", self.calibration_distance);
        error
    }

    /// `GetMaxRaDuration`, ms.
    pub fn max_ra_duration(&self) -> i32 {
        self.mv.max_ra_duration
    }

    /// `SetMaxRaDuration` (`scope.cpp:211`): negative is refused (2500
    /// then, without a notification); a change is an `RA Max Duration`
    /// parameter change. Always written back. No range check here: 50–8000
    /// is the controls'.
    pub fn set_max_ra_duration(
        &mut self,
        ms: i32,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let error = ms < 0;
        if error {
            debug(events, "Error thrown from scope.cpp:219->maxRaDuration < 0");
            self.mv.max_ra_duration = DEFAULT_MAX_RA_DURATION;
        } else if self.mv.max_ra_duration != ms {
            events.push(MountEvent::GuideParamChange { name: "RA Max Duration".into(), value: ms.into() });
            self.mv.max_ra_duration = ms;
        }
        profile.set_i32("/scope/MaxRaDuration", self.mv.max_ra_duration);
        error
    }

    /// `GetMaxDecDuration`, ms.
    pub fn max_dec_duration(&self) -> i32 {
        self.mv.max_dec_duration
    }

    /// `SetMaxDecDuration`, like [`set_max_ra_duration`](Self::set_max_ra_duration)
    /// with `Dec Max Duration`.
    pub fn set_max_dec_duration(
        &mut self,
        ms: i32,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        self.mv.set_max_dec_duration(ms, profile, events)
    }

    /// `GetDecGuideMode`.
    pub fn dec_guide_mode(&self) -> DecGuideMode {
        self.mv.dec_guide_mode
    }

    /// `SetDecGuideMode` (`scope.cpp:275`) on the stored integer. An unknown
    /// value is refused and changes nothing. A change is a `Dec Guide Mode`
    /// parameter change (`Off`/`Auto`/`North`/`South`); leaving Auto turns
    /// the backlash compensation off (a one-way mode cannot recover from an
    /// overshoot), going to Auto resets it. Written back only on a change.
    pub fn set_dec_guide_mode(
        &mut self,
        mode: i32,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let Some(m) = DecGuideMode::from_i32(mode) else {
            debug(events, "Error thrown from scope.cpp:289->invalid decGuideMode");
            return true;
        };
        if self.mv.dec_guide_mode != m {
            self.mv.dec_guide_mode = m;
            debug(events, format!("DecGuideMode set to {} ({mode})", m.label()));
            events.push(MountEvent::GuideParamChange { name: "Dec Guide Mode".into(), value: m.label().into() });
            if let Some(blc) = self.mv.blc.as_mut() {
                let fx = if m != DecGuideMode::Auto { blc.enable_backlash_comp(false) } else { blc.reset_blc_state() };
                self.mv.absorb(fx, profile, events);
            }
            profile.set_i32("/scope/DecGuideMode", m as i32);
        }
        false
    }

    /// `CalibrationFlipRequiresDecFlip`.
    pub fn calibration_flip_requires_dec_flip(&self) -> bool {
        self.cal_flip_requires_dec_flip
    }

    /// `SetCalibrationFlipRequiresDecFlip`.
    pub fn set_calibration_flip_requires_dec_flip(&mut self, val: bool, profile: &mut impl ProfileAccess) {
        self.cal_flip_requires_dec_flip = val;
        profile.set_i32("/scope/CalFlipRequiresDecFlip", val as i32);
    }

    /// `IsAssumeOrthogonal`.
    pub fn is_assume_orthogonal(&self) -> bool {
        self.assume_orthogonal
    }

    /// `SetAssumeOrthogonal`.
    pub fn set_assume_orthogonal(&mut self, val: bool, profile: &mut impl ProfileAccess) {
        self.assume_orthogonal = val;
        profile.set_i32("/scope/AssumeOrthogonal", val as i32);
    }

    /// `DecCompensationEnabled`.
    pub fn dec_compensation_enabled(&self) -> bool {
        self.use_dec_compensation
    }

    /// `EnableDecCompensation`.
    pub fn enable_dec_compensation(&mut self, enable: bool, profile: &mut impl ProfileAccess) {
        self.use_dec_compensation = enable;
        profile.set_i32("/scope/UseDecComp", enable as i32);
    }

    /// `HasHPEncoders` (`/scope/HiResEncoders`, set by the profile wizard).
    pub fn has_hp_encoders(&self) -> bool {
        self.has_hp_encoders
    }

    /// `IsStopGuidingWhenSlewingEnabled`.
    pub fn is_stop_guiding_when_slewing_enabled(&self) -> bool {
        self.mv.stop_guiding_when_slewing
    }

    /// `EnableStopGuidingWhenSlewing`.
    pub fn enable_stop_guiding_when_slewing(
        &mut self,
        enable: bool,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        debug(
            events,
            if enable {
                "Scope: enabling slew check, guiding will stop when slew is detected"
            } else {
                "Scope: slew check disabled"
            },
        );
        profile.set_i32("/scope/StopGuidingWhenSlewing", enable as i32);
        self.mv.stop_guiding_when_slewing = enable;
    }

    /// The backlash compensation.
    pub fn backlash_comp(&self) -> Option<&BacklashComp> {
        self.mv.blc.as_ref()
    }

    /// `BacklashComp::EnableBacklashComp`, with its effects carried out.
    pub fn enable_backlash_comp(
        &mut self,
        enable: bool,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        if let Some(blc) = self.mv.blc.as_mut() {
            let fx = blc.enable_backlash_comp(enable);
            self.mv.absorb(fx, profile, events);
        }
    }

    /// `BacklashComp::SetBacklashPulseWidth`, with its effects carried out.
    pub fn set_backlash_pulse_width(
        &mut self,
        ms: i32,
        floor: i32,
        ceiling: i32,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        if let Some(blc) = self.mv.blc.as_mut() {
            let fx = blc.set_backlash_pulse_width(ms, floor, ceiling, self.mv.max_dec_duration);
            self.mv.absorb(fx, profile, events);
        }
    }

    /// `StartDecDrift`: Dec guiding off for the drift-align tool, the mode
    /// remembered. No parameter change, no profile write.
    pub fn start_dec_drift(&mut self, events: &mut Vec<MountEvent>) {
        self.save_dec_guide_mode = self.mv.dec_guide_mode;
        self.mv.dec_guide_mode = DecGuideMode::None;
        debug(events, "StartDecDrift: DecGuideMode set to Off (0)");
    }

    /// `EndDecDrift`: the remembered mode back.
    pub fn end_dec_drift(&mut self, events: &mut Vec<MountEvent>) {
        self.mv.dec_guide_mode = self.save_dec_guide_mode;
        let m = self.mv.dec_guide_mode;
        debug(events, format!("EndDecDrift: DecGuideMode set to {} ({})", m.label(), m as i32));
    }

    /// `IsDecDrifting`.
    pub fn is_dec_drifting(&self) -> bool {
        self.mv.dec_guide_mode == DecGuideMode::None
    }

    // ---------------------------------------------------------------- moves

    /// `SetGuidingEnabled`, plus the scope's `DeferPulseLimitAlertCheck`
    /// when guiding is turned back on (no false alert right after).
    pub fn set_guiding_enabled(&mut self, enabled: bool, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        if self.mount.set_guiding_enabled(enabled, ctx, events) && enabled {
            self.defer_pulse_limit_alert_check(ctx.now);
        }
    }

    /// `DeferPulseLimitAlertCheck`: no pulse-limit alert for 120 s.
    pub fn defer_pulse_limit_alert_check(&mut self, now: f64) {
        self.mv.limit_deferral_time = whole_seconds(now) + LIMIT_REACHED_GRACE_PERIOD_SECONDS;
    }

    /// The three-argument `Scope::MoveAxis` (`scope.cpp:571`), which the
    /// calibration and manual moves use: `duration` ms one way, no guide
    /// step recorded.
    pub fn move_axis(
        &mut self,
        dir: Dir,
        duration: i32,
        opts: MoveOpts,
        io: &mut ScopeIo,
        events: &mut Vec<MountEvent>,
    ) -> MoveResult {
        debug(events, format!("scope move axis dir= {} dur= {} opts= 0x{:x}", dir as i32, duration, opts.0));
        let (result, _) = self.mv.move_axis(self.mount.guiding_enabled(), dir, duration, opts, io, events);
        if result != MoveResult::Ok {
            debug(events, "Throw from scope.cpp:584->Move failed");
        }
        result
    }

    /// `Mount::MoveOffset` on this scope (see [`Mount::move_offset`]): the
    /// backlash compensation may write the profile. `frame` is read after
    /// the pulses.
    pub fn move_offset(
        &mut self,
        ofs: &mut GuiderOffset,
        opts: MoveOpts,
        frame: impl FnOnce() -> StepFrame,
        ctx: &AlgoCtx,
        io: &mut ScopeIo,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> MoveResult {
        let mut mover = ScopeMover { mv: &mut self.mv, io, profile };
        self.mount.move_offset(ofs, opts, frame, ctx, &mut mover, events)
    }

    /// `NotifyGuidingStarted`.
    pub fn notify_guiding_started(&mut self, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        self.mount.notify_guiding_started(ctx, events);
    }

    /// `NotifyGuidingStopped`: the algorithms, then the backlash
    /// compensation forgets its last direction.
    pub fn notify_guiding_stopped(
        &mut self,
        ctx: &AlgoCtx,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        self.mount.notify_guiding_stopped(ctx, events);
        if let Some(blc) = self.mv.blc.as_mut() {
            let fx = blc.reset_blc_state();
            self.mv.absorb(fx, profile, events);
        }
    }

    // ---------------------------------------------------------- calibration

    /// `Scope::IsCalibrated` (`scope.cpp:1124`): the mount's, plus a Dec
    /// rate unless Dec guiding is off.
    pub fn is_calibrated(&self) -> bool {
        if !self.mount.is_calibrated() {
            return false;
        }
        match self.mv.dec_guide_mode {
            DecGuideMode::None => true,
            _ => self.calibration.y_rate != CALIBRATION_RATE_UNCALIBRATED,
        }
    }

    /// Where the state machine is.
    pub fn calibration_state(&self) -> CalibrationState {
        self.cw.state
    }

    /// `Scope::SetCalibration`: keep the copy, then the mount's (in memory,
    /// stored with `timestamp`).
    pub fn set_calibration(
        &mut self,
        cal: &Calibration,
        timestamp: &str,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        self.calibration = cal.clone();
        self.mount.set_calibration(cal, timestamp, profile, events);
    }

    /// `Scope::ClearCalibration`: not calibrated, state machine back to the
    /// start.
    pub fn clear_calibration(&mut self) {
        self.mount.clear_calibration();
        self.cw.state = CalibrationState::Cleared;
    }

    /// `LoadCalibrationDetails`, with the once-per-session guide-speed check.
    pub fn load_calibration_details(
        &mut self,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> CalibrationDetails {
        self.mount.load_calibration_details(profile, Some(&mut self.cal_details_validated), events)
    }

    /// The last sanity-check verdict (`m_lastCalibrationIssue`), with the
    /// calibration and details it compared against: what PHD2's "Details…"
    /// dialog shows.
    pub fn last_calibration_issue(&self) -> (CalibrationIssue, &Calibration, &CalibrationDetails) {
        (self.last_calibration_issue, &self.prev_calibration, &self.prev_calibration_details)
    }

    /// `MyFrame::FlipCalibrationData` (`myframe_events.cpp:965`), what the
    /// flip RPC and menu run: `Mount::FlipCalibration` on this scope, stored
    /// with `timestamp`, then `CalibrationDataFlipped`. Returns whether it
    /// flipped; a scope that is not calibrated is refused.
    pub fn flip_calibration(
        &mut self,
        timestamp: &str,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let calibrated = self.is_calibrated();
        let dec_flip = self.cal_flip_requires_dec_flip;
        match calibration::flip_calibration(self.mount.calibration_mut(), calibrated, dec_flip) {
            Ok(flip) => {
                events.extend(flip.log.into_iter().map(MountEvent::Debug));
                self.calibration = self.mount.mount_cal().clone();
                calibration::store_calibration(profile, MountClass::Scope, self.mount.mount_cal(), timestamp);
                events.push(MountEvent::Status(flip.status));
                events.push(MountEvent::CalibrationDataFlipped);
                true
            }
            Err(line) => {
                debug(events, line);
                false
            }
        }
    }

    /// `Mount::AdjustCalibrationForScopePointing` on this scope, at guide
    /// start (see [`calibration::adjust_for_scope_pointing`]). What it set
    /// is stored with `timestamp` here; its steps (debug lines, alerts, the
    /// status line, the image-scale change) and the new pixel size are the
    /// caller's. The stored details are read only when the mount reports
    /// guide rates (PHD2: when it can report its position).
    pub fn adjust_calibration_for_scope_pointing(
        &mut self,
        pointing: &ScopePointing,
        timestamp: &str,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> Adjustment {
        let details = if pointing.guide_rates.is_some() {
            self.load_calibration_details(profile, events)
        } else {
            CalibrationDetails::default()
        };
        let opts = AdjustOpts {
            use_dec_comp: self.use_dec_compensation,
            cal_flip_requires_dec_flip: self.cal_flip_requires_dec_flip,
            connected: self.mount.is_connected(),
        };
        let adj = calibration::adjust_for_scope_pointing(self.mount.calibration_mut(), &details, pointing, &opts);
        if let Some(stored) = &adj.stored {
            self.calibration = stored.clone();
            calibration::store_calibration(profile, MountClass::Scope, stored, timestamp);
        }
        adj
    }

    /// `CheckCalibrationDuration` (`scope.cpp:995`): before a calibration,
    /// fit the distance to a binning change, and the step to a binning or
    /// a > 5 % RA guide-speed change since the last calibration (never on
    /// the first one, nor without usable mount rates).
    fn check_calibration_duration(
        &mut self,
        curr_duration: i32,
        env: &CalibrationEnv,
        pointing: Option<&dyn PointingSource>,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        let details = self.load_calibration_details(profile, events);
        let binning_change = env.binning as f64 != details.orig_binning;
        if binning_change {
            let prev = self.calibration_distance;
            let new = calibration::calibration_distance(env.focal_length, env.pixel_size, env.binning);
            if new != prev {
                debug(
                    events,
                    format!("CalDistance adjusted at start of calibration from {prev} to {new} because of binning change"),
                );
                self.set_calibration_distance(new, profile, events);
            }
        }
        let Some((ra_spd, _)) = pointing.and_then(|p| p.guide_rates()) else {
            return;
        };
        if details.ra_guide_speed <= 0.0 {
            return;
        }
        if !(binning_change || (1.0 - ra_spd / details.ra_guide_speed).abs() > 0.05) {
            return;
        }
        let (_, step) = calibration::calibration_step_size(
            env.focal_length,
            env.pixel_size,
            env.binning,
            sidereal_rate_from_guide_speed(ra_spd),
            CALSTEP_DEFAULT_STEPS,
            0.0,
            self.calibration_distance,
        );
        let why = if binning_change { " binning " } else { " mount guide speed " };
        debug(
            events,
            format!("CalDuration adjusted at start of calibration from {curr_duration} to {step} because of {why} change"),
        );
        self.set_calibration_duration(step, profile, events);
    }

    /// `Scope::BeginCalibration` (`scope.cpp:1046`): refuses a disconnected
    /// scope or no star; otherwise checks the step size, clears the
    /// calibration and starts West from `location`. Returns whether it
    /// began. The guider then logs and announces the start.
    pub fn begin_calibration(
        &mut self,
        location: Option<Point>,
        env: &CalibrationEnv,
        pointing: Option<&dyn PointingSource>,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        if !self.mount.is_connected() {
            debug(events, "Error thrown from scope.cpp:1054->Not connected");
            return false;
        }
        let Some(location) = location else {
            debug(events, "Error thrown from scope.cpp:1059->Must have a valid lock position");
            return false;
        };
        self.check_calibration_duration(self.calibration_duration, env, pointing, profile, events);
        self.clear_calibration();
        self.cw.steps = 0;
        self.cw.initial_location = location;
        self.cw.starting_location = None;
        self.cw.starting_coords = None;
        self.cw.state = CalibrationState::GoWest;
        self.calibration_details.ra_steps.clear();
        self.calibration_details.dec_steps.clear();
        self.cw.ra_steps = 0;
        self.cw.dec_steps = 0;
        self.calibration_details.last_issue = CalibrationIssue::None;
        self.cw.east_alert_shown = false;
        true
    }

    /// `Scope::UpdateCalibrationState` (`scope.cpp:1198`): one frame of the
    /// calibration, the star at `cur`.
    ///
    /// - West: pulse West until the star is `calibration_distance` away,
    ///   giving up once the step counter has passed 60. PHD2's cap quirk is
    ///   kept: `steps++ > 60` tests before the increment, so 61 moves are
    ///   made and the 62nd frame fails. The RA angle and rate follow, the RA
    ///   parity from the mount's RA (West lowers it).
    /// - East: the same time back, in pulses as long as the search region
    ///   allows with fast recenter. Without pulse guiding (ST-4), an advisory
    ///   when the star came back less than 25 % or more than 30° off. With
    ///   Dec guiding off the calibration completes here (on the next frame),
    ///   the Dec axis set perpendicular and uncalibrated.
    /// - Backlash: pulse North until three moves in a row of at least 60 %
    ///   of an RA step (scaled by the Dec/RA guide-speed ratio) without a
    ///   reversal, or `max(8, 60 s / step)` pulses (going on when the star
    ///   moved ≥ 3 px), or the calibration distance.
    /// - North: as West, the last clearing move counting as step 1; with
    ///   assume-orthogonal the angle snaps to ±90° from RA. Dec parity from
    ///   the mount's Dec (North raises it).
    /// - South: the same time back; the "little south movement" advisory is
    ///   only logged (PHD2 commented the alert out).
    /// - Nudge South: up to four nudges back towards the start (the count is
    ///   tested before its increment) while heading within 40° of the reverse
    ///   of the north moves, 2 px < distance < calibration + backlash
    ///   distance, and still on the side the south moves came from. The
    ///   nudges are announced but not logged.
    /// - Complete: stamp declination, pier side, rotator and binning, set and
    ///   store the calibration and its details, run the sanity check.
    pub fn update_calibration_state(
        &mut self,
        cur: Point,
        env: &CalibrationEnv,
        pointing: Option<&dyn PointingSource>,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> CalibrationNext {
        use CalibrationState::*;

        if self.cw.starting_location.is_none() {
            self.cw.starting_location = Some(cur);
            self.cw.starting_coords = ra_dec(pointing);
            let coords = match self.cw.starting_coords {
                Some(c) => format!("{},{}", fmt_f(c.x, 2), fmt_f(c.y, 1)),
                None => "N/A".to_string(),
            };
            debug(
                events,
                format!(
                    "Scope::UpdateCalibrationState: starting location = {},{} coords = {coords}",
                    fmt_f(cur.x, 2),
                    fmt_f(cur.y, 2)
                ),
            );
        }

        let start = self.cw.starting_location.unwrap_or(cur);
        let mut dx = start.x - cur.x;
        let mut dy = start.y - cur.y;
        let mut dist = dx.hypot(dy);
        let dist_crit = self.calibration_distance as f64;
        let dur = self.calibration_duration;

        let info = |dir: &'static str, step: i32, dx: f64, dy: f64, pos: Point, dist: f64| CalibrationStepInfo {
            direction: dir,
            step,
            dx,
            dy,
            pos,
            dist,
            msg: String::new(),
        };
        // `CalibrationStatus`: the status line and the `Calibrating` event.
        let status = |mut i: CalibrationStepInfo, msg: String, events: &mut Vec<MountEvent>| {
            i.msg = msg;
            events.push(MountEvent::Calibrating(i));
        };

        loop {
            match self.cw.state {
                Cleared => return CalibrationNext::NoMove,

                GoWest => {
                    // The step number in the log is the step that just finished.
                    let row = info("West", self.cw.steps, dx, dy, cur, dist);
                    events.push(MountEvent::CalibrationRow(row.clone()));
                    self.calibration_details.ra_steps.push(Point::new(dx, dy));

                    if dist < dist_crit {
                        let steps = self.cw.steps;
                        self.cw.steps += 1;
                        if steps > MAX_CALIBRATION_STEPS {
                            return self.fail(
                                "RA Calibration Failed: star did not move enough",
                                "Error thrown from scope.cpp:1255->RA calibration failed",
                                events,
                            );
                        }
                        status(
                            row,
                            format!("West step {:3}, dist={:>4}", self.cw.steps, fmt_f(dist, 1)),
                            events,
                        );
                        return CalibrationNext::Move(Dir::West, dur);
                    }

                    // West complete.
                    self.calibration.x_angle = phd_angle(start, cur);
                    self.calibration.x_rate = dist / (self.cw.steps * dur) as f64;
                    self.calibration.ra_guide_parity = Parity::Unknown;
                    if let Some(sc) = self.cw.starting_coords
                        && let Some(ec) = ra_dec(pointing)
                    {
                        // True westward motion decreases RA.
                        let one_arcsec = 24.0 / (360.0 * 60.0 * 60.0); // hours
                        let dra = ec.x - sc.x;
                        if dra < -one_arcsec {
                            self.calibration.ra_guide_parity = Parity::Even;
                        } else if dra > one_arcsec {
                            self.calibration.ra_guide_parity = Parity::Odd;
                        }
                    }
                    debug(
                        events,
                        format!(
                            "WEST calibration completes with steps={} angle={} rate={} parity={}",
                            self.cw.steps,
                            fmt_f(degrees(self.calibration.x_angle), 1),
                            fmt_f(self.calibration.x_rate * 1000.0, 3),
                            self.calibration.ra_guide_parity as i32
                        ),
                    );
                    self.cw.ra_steps = self.cw.steps;
                    events.push(MountEvent::CalibrationAxisComplete {
                        direction: "West",
                        angle: self.calibration.x_angle,
                        rate: self.calibration.x_rate,
                        parity: self.calibration.ra_guide_parity,
                    });

                    // Back East: the largest pulse that keeps the star and
                    // stays under the RA maximum.
                    self.cw.recenter_remaining = self.cw.steps * dur;
                    self.cw.recenter_duration = if env.fast_recenter {
                        let mut d = c_int((env.max_move_pixels as f64 / self.calibration.x_rate).floor());
                        if d > self.mv.max_ra_duration {
                            d = self.mv.max_ra_duration;
                        }
                        if d < dur {
                            d = dur;
                        }
                        d
                    } else {
                        dur
                    };
                    self.cw.steps = div_round_up(self.cw.recenter_remaining, self.cw.recenter_duration);
                    self.cw.state = GoEast;
                    self.cw.east_starting_location = cur;
                    debug(events, "Falling Through to state GO_EAST");
                }

                GoEast => {
                    let row = info("East", self.cw.steps, dx, dy, cur, dist);
                    events.push(MountEvent::CalibrationRow(row.clone()));
                    self.calibration_details.ra_steps.push(Point::new(dx, dy));

                    if self.cw.recenter_remaining > 0 {
                        let d = self.cw.recenter_duration.min(self.cw.recenter_remaining);
                        status(
                            row,
                            format!("East step {:3}, dist={:>4}", self.cw.steps, fmt_f(dist, 1)),
                            events,
                        );
                        self.cw.recenter_remaining -= d;
                        self.cw.steps -= 1;
                        self.cw.last_location = cur;
                        return CalibrationNext::Move(Dir::East, d);
                    }

                    // On an ST-4 cable, check for an obvious cable problem.
                    if !env.can_pulse_guide {
                        let east = self.cw.east_starting_location;
                        let east_dist = east.distance(&cur);
                        let west_dist = start.distance(&east);
                        let east_angle = phd_angle(east, cur);
                        // A real east move retraces the west one within 30°.
                        if east_dist.abs() < 0.25 * west_dist
                            || norm_angle(east_angle - (self.calibration.x_angle + PI)).abs() > radians(30.0)
                        {
                            let msg = "Advisory: Little or no east movement was measured, so guiding will probably be impaired. Check the guide cable and use the Manual Guide tool to confirm basic operation of the mount.";
                            events.push(MountEvent::Alert { msg: msg.into(), kind: AlertKind::Warning });
                            debug(events, format!("Calibration alert: {msg}"));
                            self.cw.east_alert_shown = true;
                        }
                    }

                    // Set up the backlash clearing.
                    self.cw.steps = 0;
                    (dist, dx, dy) = (0.0, 0.0, 0.0);
                    self.cw.starting_location = Some(cur);

                    if self.mv.dec_guide_mode == DecGuideMode::None {
                        debug(events, "Skipping Dec calibration as DecGuideMode == NONE");
                        self.cw.state = Complete;
                        // Any angle perpendicular to RA; the rate says "no Dec".
                        self.calibration.y_angle = norm_angle(self.calibration.x_angle + PI / 2.0);
                        self.calibration.y_rate = CALIBRATION_RATE_UNCALIBRATED;
                        self.calibration.dec_guide_parity = Parity::Unknown;
                        return CalibrationNext::NoMove;
                    }

                    self.cw.state = ClearBacklash;
                    self.cw.bl_marker_point = cur;
                    self.cw.starting_coords = ra_dec(pointing);
                    self.cw.bl_expected_backlash_step = self.calibration.x_rate * dur as f64 * 0.6;
                    if let Some((ra_speed, dec_speed)) = pointing.and_then(|p| p.guide_rates())
                        && ra_speed != 0.0
                        && ra_speed != dec_speed
                    {
                        self.cw.bl_expected_backlash_step *= dec_speed / ra_speed;
                    }
                    self.cw.bl_max_clearing_pulses = 8.max(BL_MAX_CLEARING_TIME / dur);
                    self.cw.bl_last_cum_distance = 0.0;
                    self.cw.bl_accepted_moves = 0;
                    debug(
                        events,
                        format!(
                            "Backlash: Looking for 3 moves of {} px, max attempts = {}",
                            fmt_f(self.cw.bl_expected_backlash_step, 1),
                            self.cw.bl_max_clearing_pulses
                        ),
                    );
                    debug(events, "Falling Through to state CLEAR_BACKLASH");
                }

                ClearBacklash => {
                    let row = info("Backlash", self.cw.steps, dx, dy, cur, dist);
                    events.push(MountEvent::CalibrationRow(row.clone()));
                    let bl_delta = self.cw.bl_marker_point.distance(&cur);
                    let bl_cum_delta = dist;

                    // Three moves North of at least the expected size, no reversal.
                    if self.cw.steps == 0 {
                        debug(events, format!("Backlash: Starting north clearing using pulse width of {dur}"));
                        self.cw.steps = 1;
                        status(row, "Clearing backlash step 1".into(), events);
                        return CalibrationNext::Move(Dir::North, dur);
                    }

                    if bl_delta >= self.cw.bl_expected_backlash_step {
                        if self.cw.bl_accepted_moves == 0 || bl_cum_delta > self.cw.bl_last_cum_distance {
                            self.cw.bl_accepted_moves += 1;
                            debug(events, format!("Backlash: Accepted clearing move of {}", fmt_f(bl_delta, 1)));
                        } else {
                            self.cw.bl_accepted_moves = 0;
                            debug(
                                events,
                                format!("Backlash: Rejected clearing move of {}, direction reversal", fmt_f(bl_delta, 1)),
                            );
                        }
                    } else if bl_cum_delta < self.cw.bl_last_cum_distance {
                        self.cw.bl_accepted_moves = 0;
                        debug(
                            events,
                            format!("Backlash: Rejected small direction reversal of {} px", fmt_f(bl_delta, 1)),
                        );
                    } else {
                        debug(events, format!("Backlash: Rejected small move of {} px", fmt_f(bl_delta, 1)));
                    }

                    if self.cw.bl_accepted_moves < BL_BACKLASH_MIN_COUNT {
                        if self.cw.steps < self.cw.bl_max_clearing_pulses && bl_cum_delta < dist_crit {
                            // Attempts left, and the star has not gone the full distance yet.
                            self.cw.steps += 1;
                            self.cw.bl_marker_point = cur;
                            self.cw.starting_coords = ra_dec(pointing);
                            self.cw.bl_last_cum_distance = bl_cum_delta;
                            let msg = format!("Clearing backlash step {:3}", self.cw.steps);
                            status(row, msg.clone(), events);
                            debug(
                                events,
                                format!(
                                    "Backlash: {msg}, Last Delta = {} px, CumDistance = {} px",
                                    fmt_f(bl_delta, 2),
                                    fmt_f(bl_cum_delta, 2)
                                ),
                            );
                            return CalibrationNext::Move(Dir::North, dur);
                        } else if bl_cum_delta >= BL_MIN_CLEARING_DISTANCE as f64 {
                            // Out of attempts but the mount did move (as PHD1).
                            self.cw.steps = 0;
                            self.cw.starting_location = Some(cur);
                            (dx, dy, dist) = (0.0, 0.0, 0.0);
                            debug(
                                events,
                                "Backlash: Reached clearing limit but total displacement > 3px - proceeding with calibration",
                            );
                        } else {
                            return self.fail(
                                "Backlash Clearing Failed: star did not move enough",
                                "Error thrown from scope.cpp:1480->Clear backlash failed",
                                events,
                            );
                        }
                    } else {
                        // The last clearing move was big enough: it is North step 1.
                        let marker = self.cw.bl_marker_point;
                        events.push(MountEvent::CalibrationRow(info("North", 0, 0.0, 0.0, marker, 0.0)));
                        self.calibration_details.dec_steps.push(Point::new(0.0, 0.0));
                        self.cw.steps = 1;
                        self.cw.starting_location = Some(marker);
                        dx = marker.x - cur.x;
                        dy = marker.y - cur.y;
                        dist = marker.distance(&cur);
                        debug(events, "Backlash: Got 3 acceptable moves, using last move as step 1 of N calibration");
                    }

                    let marker = self.cw.bl_marker_point;
                    // Sets the nudging limit.
                    self.cw.bl_distance_moved = marker.distance(&self.cw.initial_location);
                    debug(
                        events,
                        format!(
                            "Backlash: North calibration moves starting at {{{},{}}}, Offset = {} px",
                            fmt_f(marker.x, 1),
                            fmt_f(marker.y, 1),
                            fmt_f(self.cw.bl_distance_moved, 1)
                        ),
                    );
                    debug(
                        events,
                        format!(
                            "Backlash: Total distance moved = {}",
                            fmt_f(cur.distance(&self.cw.initial_location), 1)
                        ),
                    );
                    self.cw.state = GoNorth;
                    debug(events, "Backlash: Falling Through to state GO_NORTH");
                }

                GoNorth => {
                    let row = info("North", self.cw.steps, dx, dy, cur, dist);
                    events.push(MountEvent::CalibrationRow(row.clone()));
                    self.calibration_details.dec_steps.push(Point::new(dx, dy));

                    if dist < dist_crit {
                        let steps = self.cw.steps;
                        self.cw.steps += 1;
                        if steps > MAX_CALIBRATION_STEPS {
                            return self.fail(
                                "DEC Calibration Failed: star did not move enough",
                                "Error thrown from scope.cpp:1529->Dec calibration failed",
                                events,
                            );
                        }
                        status(
                            row,
                            format!("North step {:3}, dist={:>4}", self.cw.steps, fmt_f(dist, 1)),
                            events,
                        );
                        return CalibrationNext::Move(Dir::North, dur);
                    }

                    // The angle runs from the leg's start to the star, the
                    // reverse of the RA one (which was measured going West).
                    let leg_start = self.cw.starting_location.unwrap_or(cur);
                    let step_time = (self.cw.steps * dur) as f64;
                    if self.assume_orthogonal {
                        let x_angle = self.calibration.x_angle;
                        let a1 = norm_angle(x_angle + PI / 2.0);
                        let a2 = norm_angle(x_angle - PI / 2.0);
                        let y_angle = phd_angle(cur, leg_start);
                        self.calibration.y_angle =
                            if norm_angle(a1 - y_angle).abs() < norm_angle(a2 - y_angle).abs() { a1 } else { a2 };
                        let dec_dist = dist * (y_angle - self.calibration.y_angle).cos();
                        self.calibration.y_rate = dec_dist / step_time;
                        debug(
                            events,
                            format!(
                                "Assuming orthogonal axes: measured Y angle = {}, X angle = {}, orthogonal = {}, {}, best = {}, dist = {}, dec_dist = {}",
                                fmt_f(degrees(y_angle), 1),
                                fmt_f(degrees(x_angle), 1),
                                fmt_f(degrees(a1), 1),
                                fmt_f(degrees(a2), 1),
                                fmt_f(degrees(self.calibration.y_angle), 1),
                                fmt_f(dist, 2),
                                fmt_f(dec_dist, 2)
                            ),
                        );
                    } else {
                        self.calibration.y_angle = phd_angle(cur, leg_start);
                        self.calibration.y_rate = dist / step_time;
                    }
                    self.cw.dec_steps = self.cw.steps;

                    self.calibration.dec_guide_parity = Parity::Unknown;
                    if let Some(sc) = self.cw.starting_coords
                        && let Some(ec) = ra_dec(pointing)
                    {
                        // Real northward motion increases Dec.
                        let one_arcsec = 1.0 / (60.0 * 60.0); // degrees
                        let ddec = ec.y - sc.y;
                        if ddec > one_arcsec {
                            self.calibration.dec_guide_parity = Parity::Even;
                        } else if ddec < -one_arcsec {
                            self.calibration.dec_guide_parity = Parity::Odd;
                        }
                    }
                    debug(
                        events,
                        format!(
                            "NORTH calibration completes with angle={} rate={} parity={}",
                            fmt_f(degrees(self.calibration.y_angle), 1),
                            fmt_f(self.calibration.y_rate * 1000.0, 3),
                            self.calibration.dec_guide_parity as i32
                        ),
                    );
                    events.push(MountEvent::CalibrationAxisComplete {
                        direction: "North",
                        angle: self.calibration.y_angle,
                        rate: self.calibration.y_rate,
                        parity: self.calibration.dec_guide_parity,
                    });

                    // Back South, as East, with 80 % of the search region.
                    self.cw.recenter_remaining = self.cw.steps * dur;
                    self.cw.recenter_duration = if env.fast_recenter {
                        let mut d = c_int((0.8 * env.max_move_pixels as f64 / self.calibration.y_rate).floor());
                        if d > self.mv.max_dec_duration {
                            d = self.mv.max_dec_duration;
                        }
                        if d < dur {
                            d = dur;
                        }
                        d
                    } else {
                        dur
                    };
                    self.cw.steps = div_round_up(self.cw.recenter_remaining, self.cw.recenter_duration);
                    self.cw.state = GoSouth;
                    self.cw.south_starting_location = cur;
                    debug(events, "Falling Through to state GO_SOUTH");
                }

                GoSouth => {
                    let row = info("South", self.cw.steps, dx, dy, cur, dist);
                    events.push(MountEvent::CalibrationRow(row.clone()));
                    self.calibration_details.dec_steps.push(Point::new(dx, dy));

                    if self.cw.recenter_remaining > 0 {
                        let d = self.cw.recenter_duration.min(self.cw.recenter_remaining);
                        status(
                            row,
                            format!("South step {:3}, dist={:>4}", self.cw.steps, fmt_f(dist, 1)),
                            events,
                        );
                        self.cw.recenter_remaining -= d;
                        self.cw.steps -= 1;
                        return CalibrationNext::Move(Dir::South, d);
                    }

                    // A cable problem or a huge Dec backlash shows as little
                    // south movement. PHD2 only logs it (the alert is commented
                    // out), whatever `east_alert_shown` says.
                    let south = self.cw.south_starting_location;
                    let leg_start = self.cw.starting_location.unwrap_or(cur);
                    let south_dist = south.distance(&cur);
                    let north_dist = leg_start.distance(&south);
                    let south_angle = phd_angle(cur, south);
                    if south_dist.abs() < 0.25 * north_dist
                        || norm_angle(south_angle - (self.calibration.y_angle + PI)).abs() > radians(30.0)
                    {
                        let msg = if !env.can_pulse_guide {
                            if south_dist.abs() < 0.10 * north_dist {
                                "Advisory: Calibration successful but little or no south movement was measured, so guiding will probably be impaired.\n This is usually caused by a faulty guide cable or very large Dec backlash. \nCheck the guide cable and read the online Help for how to identify these types of problems (Manual Guide, Declination backlash)."
                            } else {
                                "Advisory: Calibration successful but little south movement was measured, so guiding will probably be impaired. \nThis is usually caused by very large Dec backlash or other problems with the mount mechanics. \nRead the online Help for how to identify these types of problems (Manual Guide, Declination backlash)."
                            }
                        } else {
                            "Advisory: Calibration successful but little south movement was measured, so guiding may be impaired.\n This is usually caused by very large Dec backlash or other problems with the mount mechanics. \nRead the online help for how to deal with this type of problem (Declination backlash)."
                        };
                        debug(events, format!("Omitted calibration alert: {msg}"));
                    }

                    self.cw.last_location = cur;
                    // The north moves' direction, to keep the nudges going south.
                    let initial = self.cw.initial_location;
                    let north_len = initial.distance(&south);
                    self.cw.north_dir_cos_x = (initial.x - south.x) / north_len;
                    self.cw.north_dir_cos_y = (initial.y - south.y) / north_len;
                    // How far, and which way, the south moves went on the Dec axis.
                    self.cw.total_south_amt =
                        mount_coords(south - self.cw.last_location, self.calibration.x_angle, self.calibration.y_angle).y;
                    self.cw.state = NudgeSouth;
                    self.cw.steps = 0;
                    debug(events, "Falling Through to state CALIBRATION_STATE_NUDGE_SOUTH");
                }

                NudgeSouth => {
                    // Nudge back to within 2 px of the start, at most a few
                    // times, and not at all from too far away.
                    let initial = self.cw.initial_location;
                    let nudge_amt = cur.distance(&initial);
                    let nudge_dir_cos_x = (cur.x - initial.x) / nudge_amt;
                    let nudge_dir_cos_y = (cur.y - initial.y) / nudge_amt;
                    // The nudge should reverse the north moves: about 180°.
                    let cos_theta = nudge_dir_cos_x * self.cw.north_dir_cos_x + nudge_dir_cos_y * self.cw.north_dir_cos_y;
                    let theta = cos_theta.acos();
                    debug(events, format!("Nudge: theta = {}", fmt_f(theta, 2)));
                    if (theta.abs() * 180.0 / PI - 180.0).abs() < 40.0 {
                        if self.cw.steps <= MAX_NUDGES
                            && nudge_amt > NUDGE_TOLERANCE
                            && nudge_amt < dist_crit + self.cw.bl_distance_moved
                        {
                            let mut dec_amt =
                                mount_coords(cur - initial, self.calibration.x_angle, self.calibration.y_angle).y;
                            debug(
                                events,
                                format!(
                                    "South nudging, decAmt = {}, Normal south moves = {}",
                                    fmt_f(dec_amt, 3),
                                    fmt_f(self.cw.total_south_amt, 3)
                                ),
                            );
                            // Same sign: still short of the start.
                            if dec_amt * self.cw.total_south_amt > 0.0 {
                                dec_amt = dec_amt.abs();
                                let max_move = env.max_move_pixels as f64;
                                if !(dec_amt < max_move) {
                                    dec_amt = max_move;
                                }
                                let mut pulse = c_int((dec_amt / self.calibration.y_rate).floor());
                                // Conservative: no longer than the north pulses.
                                if pulse > dur {
                                    pulse = dur;
                                }
                                debug(events, format!("Sending NudgeSouth pulse of duration {pulse} ms"));
                                self.cw.steps += 1;
                                let nudge = info("NudgeSouth", self.cw.steps, dx, dy, cur, dist);
                                status(nudge, format!("Nudge South {:3}", self.cw.steps), events);
                                return CalibrationNext::Move(Dir::South, pulse);
                            }
                        }
                    } else {
                        debug(events, format!("Nudging discontinued, wrong direction: {}", fmt_f(theta, 2)));
                    }
                    debug(
                        events,
                        format!(
                            "Final south nudging status: Current loc = {{{},{}}}, targeting {{{},{}}}",
                            fmt_f(cur.x, 3),
                            fmt_f(cur.y, 3),
                            fmt_f(initial.x, 3),
                            fmt_f(initial.y, 3)
                        ),
                    );
                    self.cw.state = Complete;
                    debug(events, "Falling Through to state CALIBRATION_COMPLETE");
                }

                Complete => {
                    self.prev_calibration = self.mount.get_last_calibration(&*profile);
                    self.prev_calibration_details = self.load_calibration_details(profile, events);
                    let cal = Calibration {
                        declination: pointing.and_then(|p| p.declination_radians()).unwrap_or(UNKNOWN_DECLINATION),
                        pier_side: pointing.map_or(PierSide::Unknown, |p| p.pier_side()),
                        rotator_angle: env.rotator_angle.unwrap_or(ROTATOR_POSITION_UNKNOWN),
                        binning: env.binning,
                        ..self.calibration.clone()
                    };
                    self.set_calibration(&cal, &env.timestamp, profile, events);
                    self.calibration_details.ra_step_count = self.cw.ra_steps;
                    self.calibration_details.dec_step_count = self.cw.dec_steps;
                    self.set_calibration_details(env, pointing, profile, events);
                    self.sanity_check_calibration(profile, events);
                    events.push(MountEvent::Status("Calibration complete".into()));
                    events.push(MountEvent::CalibrationComplete);
                    debug(events, "Calibration Complete");
                    return CalibrationNext::NoMove;
                }
            }
        }
    }

    /// A leg gave up: the alert, `CalibrationFailed` (event and guide log),
    /// the thrown error's debug line, then the calibration is cleared.
    fn fail(&mut self, reason: &str, thrown: &str, events: &mut Vec<MountEvent>) -> CalibrationNext {
        events.push(MountEvent::Alert { msg: reason.into(), kind: AlertKind::Warning });
        events.push(MountEvent::CalibrationFailed { reason: reason.into() });
        debug(events, thrown);
        self.clear_calibration();
        CalibrationNext::Failed
    }

    /// `Scope::SetCalibrationDetails` (`scope.cpp:1091`): the working
    /// details completed with the mount's guide rates (−1 when unknown), the
    /// optics, the axes' orthogonality error, the binning, a time stamp and
    /// the pier side, then stored.
    fn set_calibration_details(
        &mut self,
        env: &CalibrationEnv,
        pointing: Option<&dyn PointingSource>,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        let (ra_rate, dec_rate) = pointing.and_then(|p| p.guide_rates()).unwrap_or((-1.0, -1.0));
        let d = &mut self.calibration_details;
        d.ra_guide_speed = ra_rate;
        d.dec_guide_speed = dec_rate;
        d.focal_length = env.focal_length;
        d.image_scale = env.image_scale;
        d.ortho_error = calibration::ortho_error_deg(self.calibration.x_angle, self.calibration.y_angle);
        d.orig_binning = env.binning as f64;
        d.orig_timestamp = env.timestamp.clone();
        d.orig_pier_side = pointing.map_or(PierSide::Unknown, |p| p.pier_side());
        let details = d.clone();
        self.mount.save_calibration_details(&details, profile, Some(&mut self.cal_details_validated), events);
    }

    /// `Scope::SanityCheckCalibration` (`scope.cpp:868`) on the calibration
    /// just stored, read back from the profile, against the previous one
    /// (see [`calibration::sanity_check_calibration`]). An issue is written
    /// into the stored details (`FlagCalibrationIssue`) and raised as its
    /// advisory. PHD2 lets the user suppress each kind of advisory; that
    /// switch is not ported, so the alert always shows.
    fn sanity_check_calibration(&mut self, profile: &mut impl ProfileAccess, events: &mut Vec<MountEvent>) {
        let new_cal = self.mount.get_last_calibration(&*profile);
        let new_details = self.load_calibration_details(profile, events);
        let res = calibration::sanity_check_calibration(
            &new_cal,
            &new_details,
            &self.prev_calibration,
            &self.prev_calibration_details,
            self.use_dec_compensation,
        );
        self.last_calibration_issue = res.issue;
        if res.issue == CalibrationIssue::None {
            debug(events, "Calibration passed sanity checks...");
            return;
        }
        self.calibration_details = CalibrationDetails { last_issue: res.issue, ..new_details };
        let details = self.calibration_details.clone();
        self.mount.save_calibration_details(&details, profile, Some(&mut self.cal_details_validated), events);
        events.push(MountEvent::Alert { msg: res.issue.advisory().into(), kind: AlertKind::Warning });
        debug(events, format!("Calibration alert details: {}", res.detail));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::engine::algo::Axis;
    use crate::engine::log::cfmt::fmt_gp;
    use crate::engine::mount::GuideStepInfo;

    /// A profile that keeps values exactly (PHD2's goes through text).
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

    // The scripted rig of the C++ harness: West moves the star 3 px per
    // 750 ms along 195° (so the RA axis reads 15°), North 3 px along 105°,
    // South 10 % slower, with a 1500 ms Dec backlash at the start and on each
    // reversal. RA falls going West, Dec rises going North, at 7.5″/s.
    const RA_RATE: f64 = 0.004;
    const NORTH_RATE: f64 = 0.004;
    const SOUTH_RATE: f64 = 0.0036;
    const BACKLASH_MS: f64 = 1500.0;
    const KRA: f64 = 7.5 / 1000.0 / 3600.0 / 15.0;
    const KDEC: f64 = 7.5 / 1000.0 / 3600.0;
    const GUIDE_SPEED: f64 = 0.5 * 15.0 * 0.9973 / 3600.0;

    struct Rig {
        x: f64,
        y: f64,
        ra: f64,
        dec: f64,
        slack: f64,
        last_dec: i32,
        west_moves: bool,
        dec_moves: bool,
        slewing: bool,
        north_deg: f64,
        north_rate: f64,
        south_rate: f64,
        east_scale: f64,
        guide_speed: f64,
    }

    impl Default for Rig {
        fn default() -> Self {
            Rig {
                x: 100.0,
                y: 100.0,
                ra: 5.0,
                dec: 20.0,
                slack: BACKLASH_MS,
                last_dec: 0,
                west_moves: true,
                dec_moves: true,
                slewing: false,
                north_deg: 105.0,
                north_rate: NORTH_RATE,
                south_rate: SOUTH_RATE,
                east_scale: 1.0,
                guide_speed: GUIDE_SPEED,
            }
        }
    }

    impl Rig {
        fn pos(&self) -> Point {
            Point::new(self.x, self.y)
        }

        fn pulse(&mut self, dir: Dir, ms: i32) {
            let w = 195.0 * PI / 180.0;
            let n = self.north_deg * PI / 180.0;
            match dir {
                Dir::West | Dir::East => {
                    if !self.west_moves {
                        return;
                    }
                    let (s, ms) = if dir == Dir::West { (1.0, ms as f64) } else { (-1.0, ms as f64 * self.east_scale) };
                    self.x += s * w.cos() * RA_RATE * ms;
                    self.y += s * w.sin() * RA_RATE * ms;
                    self.ra -= s * ms * KRA;
                }
                Dir::North | Dir::South => {
                    if !self.dec_moves {
                        return;
                    }
                    let s = if dir == Dir::North { 1 } else { -1 };
                    if self.last_dec != 0 && self.last_dec != s {
                        self.slack = BACKLASH_MS;
                    }
                    self.last_dec = s;
                    let mut eff = ms as f64 - self.slack;
                    if eff < 0.0 {
                        self.slack -= ms as f64;
                        eff = 0.0;
                    } else {
                        self.slack = 0.0;
                    }
                    let rate = if dir == Dir::North { self.north_rate } else { self.south_rate };
                    self.x += s as f64 * n.cos() * rate * eff;
                    self.y += s as f64 * n.sin() * rate * eff;
                    self.dec += s as f64 * eff * KDEC;
                }
            }
        }
    }

    impl PointingSource for Rig {
        fn coordinates(&self) -> Option<(f64, f64, f64)> {
            Some((self.ra, self.dec, 0.0))
        }
        fn pier_side(&self) -> PierSide {
            PierSide::East
        }
        fn guide_rates(&self) -> Option<(f64, f64)> {
            Some((self.guide_speed, self.guide_speed))
        }
        fn is_slewing(&self) -> bool {
            self.slewing
        }
        fn can_report_position(&self) -> bool {
            true
        }
    }

    /// Records the pulses; the test then applies them to the rig.
    #[derive(Default)]
    struct Pulses {
        sent: Vec<(Dir, u32)>,
        pulse_guide: bool,
    }

    impl GuideOutput for Pulses {
        fn guide(&mut self, dir: Dir, ms: u32, _cancel: &CancelToken) -> MoveResult {
            self.sent.push((dir, ms));
            MoveResult::Ok
        }
        fn can_pulse_guide(&self) -> bool {
            self.pulse_guide
        }
    }

    fn env() -> CalibrationEnv {
        CalibrationEnv {
            binning: 1,
            focal_length: 400,
            pixel_size: 3.8,
            image_scale: 2.0,
            rotator_angle: None,
            fast_recenter: true,
            max_move_pixels: 20,
            can_pulse_guide: true,
            timestamp: "T".into(),
        }
    }

    fn g(v: f64) -> String {
        fmt_gp(v, 17)
    }

    fn step_line(s: &GuideStepInfo) -> String {
        format!(
            "STEP: opts={} frame={} time={} ao={} cam={},{} mnt={},{} gd={},{} ra={} {} lim={} dec={} {} lim={} mass={} snr={} hfd={} avg={} err={}",
            s.move_options.0,
            s.frame_number,
            g(s.time),
            s.is_ao as i32,
            g(s.camera_offset.x),
            g(s.camera_offset.y),
            g(s.mount_offset.x),
            g(s.mount_offset.y),
            g(s.guide_distance_ra),
            g(s.guide_distance_dec),
            s.duration_ra,
            s.direction_ra.mount_label(),
            s.ra_limited as i32,
            s.duration_dec,
            s.direction_dec.mount_label(),
            s.dec_limited as i32,
            g(s.star_mass),
            g(s.star_snr),
            g(s.star_hfd),
            g(s.avg_dist),
            s.star_error
        )
    }

    /// The events the way the C++ harness prints PHD2's calls.
    fn render(events: &mut Vec<MountEvent>, out: &mut Vec<String>) {
        for e in events.drain(..) {
            out.push(match e {
                MountEvent::Debug(s) => format!("DEBUG: {s}"),
                MountEvent::Alert { msg, .. } => format!("ALERT: {msg}"),
                MountEvent::Status(s) => format!("STATUS: {s}"),
                MountEvent::Info(s) => format!("INFO: {s}"),
                MountEvent::GuideParamChange { name, value } => format!("PARAM: {name}={value}"),
                MountEvent::CalibrationRow(i) => format!(
                    "ROW: {},{},{},{},{},{},{}",
                    i.direction,
                    i.step,
                    fmt_f(i.dx, 3),
                    fmt_f(i.dy, 3),
                    fmt_f(i.pos.x, 3),
                    fmt_f(i.pos.y, 3),
                    fmt_f(i.dist, 3)
                ),
                MountEvent::Calibrating(i) => format!(
                    "CAL: {} {} {} {} {},{} {} State={}",
                    i.direction,
                    i.step,
                    g(i.dx),
                    g(i.dy),
                    g(i.pos.x),
                    g(i.pos.y),
                    g(i.dist),
                    i.msg
                ),
                MountEvent::CalibrationAxisComplete { direction, angle, rate, parity } => format!(
                    "AXIS: {direction} calibration complete. Angle = {} deg, Rate = {} px/sec, Parity = {}",
                    fmt_f(degrees(angle), 1),
                    fmt_f(rate * 1000.0, 3),
                    parity.label()
                ),
                MountEvent::CalibrationFailed { reason } => format!("FAILED: {reason}"),
                MountEvent::CalibrationComplete => "COMPLETE".into(),
                MountEvent::CalibrationDataFlipped => "FLIPPED".into(),
                MountEvent::GuideStep(s) => step_line(&s),
            });
        }
    }

    fn connected_scope(profile: &mut Profile, now: f64) -> Scope {
        let mut scope = Scope::new(profile, false, now, &mut Vec::new());
        scope.mount.connect();
        scope
    }

    /// The harness' calibration driver: one update per frame, the scheduled
    /// move made before the next. Returns the number of moves.
    fn run_calibration(scope: &mut Scope, rig: &mut Rig, profile: &mut Profile, out: &mut Vec<String>) -> usize {
        run_calibration_in(&env(), scope, rig, profile, out)
    }

    fn run_calibration_in(
        env: &CalibrationEnv,
        scope: &mut Scope,
        rig: &mut Rig,
        profile: &mut Profile,
        out: &mut Vec<String>,
    ) -> usize {
        let mut ev = Vec::new();
        let mut moves = 0;
        out.push("== calibration".into());
        scope.begin_calibration(Some(rig.pos()), env, Some(&*rig), profile, &mut ev);
        render(&mut ev, out);
        for _ in 0..200 {
            let next = scope.update_calibration_state(rig.pos(), env, Some(&*rig), profile, &mut ev);
            render(&mut ev, out);
            match next {
                CalibrationNext::Failed => {
                    out.push("RESULT: failed".into());
                    return moves;
                }
                CalibrationNext::Move(dir, ms) => {
                    moves += 1;
                    out.push(format!("MOVE: {} {ms}", dir.mount_label()));
                    let mut pulses = Pulses::default();
                    let cancel = CancelToken::new();
                    let mut io = ScopeIo { output: &mut pulses, pointing: Some(&*rig), cancel: &cancel, now: 1000.0 };
                    scope.move_axis(dir, ms, MoveOpts::CALIBRATION_MOVE, &mut io, &mut ev);
                    render(&mut ev, out);
                    for (d, ms) in pulses.sent {
                        rig.pulse(d, ms as i32);
                    }
                }
                CalibrationNext::NoMove => {}
            }
            if scope.is_calibrated() {
                out.push("RESULT: calibrated".into());
                break;
            }
        }
        let c = scope.mount.mount_cal();
        out.push(format!(
            "CALDATA: x={} y={} xr={} yr={} dec={} par={}/{} side={} bin={} rot={}",
            g(c.x_angle),
            g(c.y_angle),
            g(c.x_rate),
            g(c.y_rate),
            g(c.declination),
            c.ra_guide_parity as i32,
            c.dec_guide_parity as i32,
            c.pier_side as i32,
            c.binning,
            g(c.rotator_angle)
        ));
        let k = |key: &str| format!("/scope/calibration/{key}");
        out.push(format!(
            "DETAILS: ra={} dec={} n={}/{} issue={} ortho={}",
            profile.get_str(&k("ra_steps"), ""),
            profile.get_str(&k("dec_steps"), ""),
            profile.get_i32(&k("ra_step_count"), -1),
            profile.get_i32(&k("dec_step_count"), -1),
            profile.get_i32(&k("last_issue"), -1),
            g(profile.get_f64(&k("ortho_error"), -1.0))
        ));
        moves
    }

    /// One guide move from a camera offset, as the harness' `step`.
    fn guide_step(
        scope: &mut Scope,
        profile: &mut Profile,
        now: f64,
        cam: (f64, f64),
        opts: MoveOpts,
        frame: i32,
        out: &mut Vec<String>,
    ) -> Vec<(Dir, u32)> {
        let mut ev = Vec::new();
        let mut ofs = GuiderOffset { camera_ofs: Point::new(cam.0, cam.1), mount_ofs: None };
        let sf = StepFrame {
            frame_number: frame,
            time: frame as f64 * 2.0,
            star_mass: 1000.0,
            star_snr: 20.0,
            star_hfd: 2.5,
            star_error: 0,
            avg_dist: 0.5,
        };
        let ctx = AlgoCtx { now, ..AlgoCtx::default() };
        let mut pulses = Pulses::default();
        let cancel = CancelToken::new();
        let mut io = ScopeIo { output: &mut pulses, pointing: None, cancel: &cancel, now };
        let r = scope.move_offset(&mut ofs, opts, || sf, &ctx, &mut io, profile, &mut ev);
        render(&mut ev, out);
        out.push(format!("RESULT: {}", r as i32));
        pulses.sent
    }

    fn cal_15_105() -> Calibration {
        Calibration {
            x_rate: 0.004,
            y_rate: 0.005,
            x_angle: radians(15.0),
            y_angle: radians(105.0),
            declination: radians(20.0),
            rotator_angle: ROTATOR_POSITION_UNKNOWN,
            binning: 1,
            pier_side: PierSide::East,
            ra_guide_parity: Parity::Even,
            dec_guide_parity: Parity::Even,
            is_valid: true,
            timestamp: String::new(),
        }
    }

    #[test]
    fn calibration_and_moves_match_phd2() {
        // The whole transcript of PHD2's C++ on the same scripted rig
        // (scope.cpp's state machine, MoveAxis, SetCalibration and the
        // details; mount.cpp's MoveOffset; Hysteresis): a calibration with a
        // 2-pulse Dec backlash, fast recenter and four nudges, one with Dec
        // guiding off, then guide moves through Hysteresis on both axes with
        // a 600 ms Dec maximum — the limit alert on the sixth limited move
        // in a row and none 10 s later, Dec North-only, guiding disabled, a
        // zero dead-reckoning move and a recovery move.
        const GOLDEN: &str = include_str!("testdata/scope_phd2.txt");

        let mut profile = Profile::default();
        let mut scope = connected_scope(&mut profile, 1000.0);
        let mut rig = Rig::default();
        let mut out = Vec::new();
        let mut ev = Vec::new();

        run_calibration(&mut scope, &mut rig, &mut profile, &mut out);
        let c = scope.mount.mount_cal().clone();
        assert!((degrees(c.x_angle) - 15.0).abs() < 1e-9 && (degrees(c.y_angle) - 105.0).abs() < 1e-9);
        assert!((c.x_rate - 0.004).abs() < 1e-15 && (c.y_rate - 0.004).abs() < 1e-15);
        assert_eq!((c.ra_guide_parity, c.dec_guide_parity), (Parity::Even, Parity::Even));
        assert_eq!(scope.last_calibration_issue().0, CalibrationIssue::None);

        rig = Rig::default();
        scope.set_dec_guide_mode(DecGuideMode::None as i32, &mut profile, &mut ev);
        render(&mut ev, &mut out);
        run_calibration(&mut scope, &mut rig, &mut profile, &mut out);
        assert_eq!(scope.mount.y_rate(), CALIBRATION_RATE_UNCALIBRATED);
        assert!(scope.is_calibrated());

        out.push("== moves".into());
        scope.mount.set_guide_algorithm(Axis::Y, 1, &mut profile, 0.0);
        scope.set_calibration(&cal_15_105(), "T", &mut profile, &mut ev);
        scope.set_dec_guide_mode(DecGuideMode::Auto as i32, &mut profile, &mut ev);
        scope.set_max_dec_duration(600, &mut profile, &mut ev);
        render(&mut ev, &mut out);

        let p = &mut profile;
        let mut now = 2000.0;
        let sent = guide_step(&mut scope, p, now, (1.0, 0.5), MoveOpts::GUIDE_STEP, 1, &mut out);
        assert_eq!(sent, [(Dir::West, 173), (Dir::South, 28)]);
        guide_step(&mut scope, p, now, (-2.0, 1.5), MoveOpts::GUIDE_STEP, 2, &mut out);
        guide_step(&mut scope, p, now, (0.1, 0.1), MoveOpts::GUIDE_STEP, 3, &mut out);
        for i in 0..7 {
            now += 10.0;
            guide_step(&mut scope, p, now, (-2.2, 8.0), MoveOpts::GUIDE_STEP, 4 + i, &mut out);
        }
        scope.set_dec_guide_mode(DecGuideMode::North as i32, p, &mut ev);
        render(&mut ev, &mut out);
        guide_step(&mut scope, p, now, (0.5, 3.0), MoveOpts::GUIDE_STEP, 11, &mut out);
        let ctx = AlgoCtx { now, ..AlgoCtx::default() };
        scope.set_guiding_enabled(false, &ctx, &mut ev);
        render(&mut ev, &mut out);
        guide_step(&mut scope, p, now, (3.0, 0.0), MoveOpts::GUIDE_STEP, 12, &mut out);
        scope.set_guiding_enabled(true, &ctx, &mut ev);
        render(&mut ev, &mut out);
        guide_step(&mut scope, p, now, (0.0, 0.0), MoveOpts::DEDUCED_MOVE, 13, &mut out);
        guide_step(&mut scope, p, now, (3.0, -3.0), MoveOpts::USE_BLC, 14, &mut out);

        let golden: Vec<&str> = GOLDEN.lines().collect();
        for (k, (r, c)) in out.iter().zip(&golden).enumerate() {
            assert_eq!(r, c, "line {}", k + 1);
        }
        assert_eq!(out.len(), golden.len());
    }

    fn rendered(ev: &mut Vec<MountEvent>) -> Vec<String> {
        let mut out = Vec::new();
        render(ev, &mut out);
        out
    }

    fn alerts(ev: &[MountEvent]) -> Vec<&str> {
        ev.iter().filter_map(|e| if let MountEvent::Alert { msg, .. } = e { Some(msg.as_str()) } else { None }).collect()
    }

    /// `Scope::MoveAxis`, three-argument form, with the pulses it sent.
    fn axis_move(
        scope: &mut Scope,
        dir: Dir,
        ms: i32,
        opts: MoveOpts,
        now: f64,
        pointing: Option<&Rig>,
        pulse_guide: bool,
        ev: &mut Vec<MountEvent>,
    ) -> (MoveResult, Vec<(Dir, u32)>) {
        let mut pulses = Pulses { pulse_guide, ..Pulses::default() };
        let cancel = CancelToken::new();
        let pointing = pointing.map(|r| r as &dyn PointingSource);
        let mut io = ScopeIo { output: &mut pulses, pointing, cancel: &cancel, now };
        let r = scope.move_axis(dir, ms, opts, &mut io, ev);
        (r, pulses.sent)
    }

    #[test]
    fn new_writes_back_the_defaults_and_announces_them() {
        let mut p = Profile::default();
        let mut ev = Vec::new();
        let scope = Scope::new(&mut p, true, 0.0, &mut ev);
        assert_eq!(
            rendered(&mut ev),
            [
                "PARAM: RA Max Duration=2500",
                "PARAM: Dec Max Duration=2500",
                "DEBUG: DecGuideMode set to Auto (1)",
                "PARAM: Dec Guide Mode=\"Auto\"",
                "DEBUG: BLC: Backlash compensation is disabled",
                "DEBUG: Scope: enabling slew check, guiding will stop when slew is detected",
            ]
        );
        for (key, v) in [
            ("CalibrationDuration", 750),
            ("CalibrationDistance", 25),
            ("MaxRaDuration", 2500),
            ("MaxDecDuration", 2500),
            ("DecGuideMode", 1),
            ("XGuideAlgorithm", 1),
            ("YGuideAlgorithm", 4),
            ("CalFlipRequiresDecFlip", 0),
            ("AssumeOrthogonal", 0),
            ("UseDecComp", 1),
            ("StopGuidingWhenSlewing", 1),
        ] {
            assert_eq!(p.i[&format!("/scope/{key}")], v, "{key}");
        }
        assert_eq!(p.f["/scope/GuideAlgorithm/X/Hysteresis/minMove"], 0.2);
        assert_eq!(scope.mount.algo(Axis::Y).kind(), crate::engine::algo::AlgoKind::ResistSwitch);
        assert!(scope.dec_compensation_enabled() && !scope.has_hp_encoders());
        assert_eq!(scope.calibration_state(), CalibrationState::Cleared);
        assert!(!scope.mount.is_connected() && !scope.is_calibrated());
    }

    #[test]
    fn new_refuses_bad_stored_values_like_phd2() {
        let mut p = Profile::default();
        for (key, v) in [
            ("CalibrationDuration", -5),
            ("CalibrationDistance", 0),
            ("MaxRaDuration", -1),
            ("MaxDecDuration", 300),
            ("DecGuideMode", 7),
            ("DecBacklashPulse", 400),
            ("BacklashCompEnabled", 1),
        ] {
            p.i.insert(format!("/scope/{key}"), v);
        }
        let mut ev = Vec::new();
        let scope = Scope::new(&mut p, false, 0.0, &mut ev);
        assert_eq!(
            rendered(&mut ev),
            [
                "DEBUG: Error thrown from scope.cpp:142->invalid calibrationDuration",
                "DEBUG: Error thrown from scope.cpp:167->invalid calibrationDistance",
                "DEBUG: Error thrown from scope.cpp:219->maxRaDuration < 0",
                "PARAM: Dec Max Duration=300",
                "DEBUG: Error thrown from scope.cpp:289->invalid decGuideMode",
                // The compensation pulse is above the Dec maximum: raised.
                "PARAM: Dec Max Duration=400",
                "DEBUG: BLC: Enabled with correction = 400 ms, Floor = 20, Ceiling = 600, Adjustable",
                "DEBUG: Scope: slew check disabled",
            ]
        );
        assert_eq!((scope.calibration_duration(), scope.calibration_distance()), (750, 25));
        assert_eq!((scope.max_ra_duration(), scope.max_dec_duration()), (2500, 400));
        // An unknown stored mode leaves PHD2's initial Off, and stays stored.
        assert_eq!(scope.dec_guide_mode(), DecGuideMode::None);
        assert_eq!(p.i["/scope/DecGuideMode"], 7);
        assert_eq!(p.i["/scope/MaxDecDuration"], 400);
    }

    #[test]
    fn dec_guide_mode_strings() {
        assert_eq!(DecGuideMode::None.label(), "Off");
        assert_eq!(DecGuideMode::parse("north"), Some(DecGuideMode::North));
        assert_eq!(DecGuideMode::parse("OFF"), Some(DecGuideMode::None));
        assert_eq!(DecGuideMode::parse("none"), None);
        assert_eq!(DecGuideMode::from_i32(4), None);
        assert_eq!(DecGuideMode::from_i32(3), Some(DecGuideMode::South));
    }

    #[test]
    fn move_axis_filters_algorithm_moves_only() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut ev = Vec::new();
        let ctx = AlgoCtx::default();

        // Guiding disabled: only a manual move goes, a calibration move too is refused.
        scope.set_guiding_enabled(false, &ctx, &mut ev);
        ev.clear();
        let r = axis_move(&mut scope, Dir::West, 500, MoveOpts::GUIDE_STEP, 0.0, None, false, &mut ev);
        assert_eq!(r, (MoveResult::Error, vec![]));
        assert_eq!(
            rendered(&mut ev),
            [
                "DEBUG: scope move axis dir= 3 dur= 500 opts= 0xd",
                "DEBUG: MoveAxis(W, 500, ABG)",
                "DEBUG: Throw from scope.cpp:716->Guiding disabled",
                "DEBUG: Move returns status 1, amount 0",
                "DEBUG: Throw from scope.cpp:584->Move failed",
            ]
        );
        let r = axis_move(&mut scope, Dir::North, 500, MoveOpts::CALIBRATION_MOVE, 0.0, None, false, &mut ev);
        assert_eq!(r.0, MoveResult::Error);
        let r = axis_move(&mut scope, Dir::West, 500, MoveOpts::MANUAL, 0.0, None, false, &mut ev);
        assert_eq!(r, (MoveResult::Ok, vec![(Dir::West, 500)]));
        scope.set_guiding_enabled(true, &ctx, &mut ev);

        // South-only: a guide step North is dropped, a calibration one is not.
        scope.set_dec_guide_mode(DecGuideMode::South as i32, &mut p, &mut ev);
        let r = axis_move(&mut scope, Dir::North, 400, MoveOpts::GUIDE_STEP, 0.0, None, false, &mut ev);
        assert_eq!(r, (MoveResult::Ok, vec![]));
        let r = axis_move(&mut scope, Dir::North, 400, MoveOpts::CALIBRATION_MOVE, 0.0, None, false, &mut ev);
        assert_eq!(r.1, [(Dir::North, 400)]);
        let r = axis_move(&mut scope, Dir::South, 400, MoveOpts::GUIDE_STEP, 0.0, None, false, &mut ev);
        assert_eq!(r.1, [(Dir::South, 400)]);
        scope.set_dec_guide_mode(DecGuideMode::None as i32, &mut p, &mut ev);
        let r = axis_move(&mut scope, Dir::South, 400, MoveOpts::DEDUCED_MOVE, 0.0, None, false, &mut ev);
        assert_eq!(r.1, []);

        // The clamp, and its flag, on algorithm moves only.
        scope.set_max_ra_duration(300, &mut p, &mut ev);
        let r = axis_move(&mut scope, Dir::East, 500, MoveOpts::GUIDE_STEP, 0.0, None, false, &mut ev);
        assert_eq!(r.1, [(Dir::East, 300)]);
        let r = axis_move(&mut scope, Dir::East, 500, MoveOpts::CALIBRATION_MOVE, 0.0, None, false, &mut ev);
        assert_eq!(r.1, [(Dir::East, 500)]);
        let mut out = Pulses::default();
        let cancel = CancelToken::new();
        let mut io = ScopeIo { output: &mut out, pointing: None, cancel: &cancel, now: 0.0 };
        let (r, info) = scope.mv.move_axis(true, Dir::West, 500, MoveOpts::GUIDE_STEP, &mut io, &mut ev);
        assert_eq!((r, info), (MoveResult::Ok, MoveResultInfo { amount_moved: 300, limited: true }));
    }

    #[test]
    fn limit_alert_after_five_limited_moves_rate_limited_and_deferred() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut ev = Vec::new();
        scope.set_max_ra_duration(300, &mut p, &mut ev);
        ev.clear();
        let west = |scope: &mut Scope, now: f64, ev: &mut Vec<MountEvent>| {
            axis_move(scope, Dir::West, 500, MoveOpts::GUIDE_STEP, now, None, true, ev);
        };
        // The first limited move starts the count; the fifth after it alerts.
        for _ in 0..5 {
            west(&mut scope, 10.0, &mut ev);
        }
        assert!(alerts(&ev).is_empty());
        west(&mut scope, 10.0, &mut ev);
        let lowered = "Your Max RA Duration setting is preventing PHD from making adequate corrections to keep the guide star locked. Try restoring Max RA Duration setting to its default value to allow PHD2 to make larger corrections.";
        assert_eq!(alerts(&ev), [lowered]);
        assert!(matches!(ev.iter().find(|e| matches!(e, MountEvent::Alert { .. })), Some(MountEvent::Alert { kind: AlertKind::Warning, .. })));
        ev.clear();
        // 30 s between alerts, both axes together.
        west(&mut scope, 39.0, &mut ev);
        assert!(alerts(&ev).is_empty());
        west(&mut scope, 40.0, &mut ev);
        assert_eq!(alerts(&ev), [lowered]);
        ev.clear();
        // Re-enabling guiding defers them for 120 s.
        let ctx = AlgoCtx { now: 100.0, ..AlgoCtx::default() };
        scope.set_guiding_enabled(false, &ctx, &mut ev);
        scope.set_guiding_enabled(true, &ctx, &mut ev);
        ev.clear();
        west(&mut scope, 200.0, &mut ev);
        assert!(alerts(&ev).is_empty());
        west(&mut scope, 230.0, &mut ev);
        assert_eq!(alerts(&ev), [lowered]);
        ev.clear();
        // A move in the other direction resets the count.
        axis_move(&mut scope, Dir::East, 500, MoveOpts::GUIDE_STEP, 300.0, None, true, &mut ev);
        west(&mut scope, 300.0, &mut ev);
        assert!(alerts(&ev).is_empty());

        // The other two wordings: at the top of the range, and at or above
        // the default (ST-4 here).
        let mut scope = connected_scope(&mut p, 0.0);
        scope.set_max_ra_duration(8000, &mut p, &mut ev);
        scope.set_max_dec_duration(2500, &mut p, &mut ev);
        ev.clear();
        for _ in 0..6 {
            axis_move(&mut scope, Dir::West, 9000, MoveOpts::GUIDE_STEP, 10.0, None, true, &mut ev);
        }
        assert_eq!(
            alerts(&ev),
            ["Even using the maximum moves, PHD2 can't properly correct for the large guide star movements in RA. Guiding will be impaired until you can eliminate the source of these problems."]
        );
        ev.clear();
        for _ in 0..6 {
            axis_move(&mut scope, Dir::South, 3000, MoveOpts::GUIDE_STEP, 50.0, None, false, &mut ev);
        }
        assert_eq!(
            alerts(&ev),
            ["PHD2 is not able to make sufficient corrections in Dec.  Check for cable snags, try re-doing your calibration and confirm the ST-4 cable is working properly."]
        );
    }

    #[test]
    fn a_slewing_mount_gets_no_pulse() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut ev = Vec::new();
        let rig = Rig { slewing: true, ..Rig::default() };
        // The check is off by default for a mount that cannot report slewing.
        let r = axis_move(&mut scope, Dir::West, 500, MoveOpts::GUIDE_STEP, 0.0, Some(&rig), true, &mut ev);
        assert_eq!(r, (MoveResult::Ok, vec![(Dir::West, 500)]));
        scope.enable_stop_guiding_when_slewing(true, &mut p, &mut ev);
        ev.clear();
        let r = axis_move(&mut scope, Dir::West, 500, MoveOpts::GUIDE_STEP, 0.0, Some(&rig), true, &mut ev);
        assert_eq!(r, (MoveResult::Slewing, vec![]));
        assert_eq!(alerts(&ev), ["Guiding stopped: the scope started slewing."]);

        // A slewing RA move skips the Dec move.
        scope.set_calibration(&cal_15_105(), "T", &mut p, &mut ev);
        ev.clear();
        let mut pulses = Pulses::default();
        let cancel = CancelToken::new();
        let mut io = ScopeIo { output: &mut pulses, pointing: Some(&rig), cancel: &cancel, now: 0.0 };
        let mut ofs = GuiderOffset { camera_ofs: Point::new(2.0, 2.0), mount_ofs: None };
        let ctx = AlgoCtx::default();
        let r = scope.move_offset(&mut ofs, MoveOpts::GUIDE_STEP, StepFrame::default, &ctx, &mut io, &mut p, &mut ev);
        assert_eq!(r, MoveResult::Slewing);
        assert!(!ev.iter().any(|e| matches!(e, MountEvent::Debug(l) if l.starts_with("MoveAxis(S") || l.starts_with("MoveAxis(N"))));
        let Some(MountEvent::GuideStep(step)) = ev.last() else { panic!("no guide step") };
        assert_eq!((step.duration_ra, step.duration_dec), (0, 0));
    }

    #[test]
    fn west_gives_up_after_its_sixty_first_move() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut rig = Rig { west_moves: false, ..Rig::default() };
        let mut out = Vec::new();
        // The cap is tested before the counter moves: steps 0..=60 still move.
        assert_eq!(run_calibration(&mut scope, &mut rig, &mut p, &mut out), 61);
        let tail: Vec<&str> = out.iter().map(String::as_str).skip_while(|l| !l.starts_with("ROW: West,61,")).collect();
        assert_eq!(
            tail[..5],
            [
                "ROW: West,61,0.000,0.000,100.000,100.000,0.000",
                "ALERT: RA Calibration Failed: star did not move enough",
                "FAILED: RA Calibration Failed: star did not move enough",
                "DEBUG: Error thrown from scope.cpp:1255->RA calibration failed",
                "RESULT: failed",
            ]
        );
        assert_eq!(scope.calibration_state(), CalibrationState::Cleared);
        assert!(!scope.is_calibrated());
    }

    #[test]
    fn backlash_clearing_gives_up_without_dec_motion() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut rig = Rig { dec_moves: false, ..Rig::default() };
        let mut out = Vec::new();
        // 9 West, 3 East, then max(8, 60000 / 750) = 80 clearing pulses.
        assert_eq!(run_calibration(&mut scope, &mut rig, &mut p, &mut out), 9 + 3 + 80);
        assert!(out.iter().any(|l| l == "DEBUG: Backlash: Looking for 3 moves of 1.8 px, max attempts = 80"));
        let n = out.len();
        assert_eq!(
            out[n - 4..],
            [
                "ALERT: Backlash Clearing Failed: star did not move enough",
                "FAILED: Backlash Clearing Failed: star did not move enough",
                "DEBUG: Error thrown from scope.cpp:1480->Clear backlash failed",
                "RESULT: failed",
            ]
        );
    }

    #[test]
    fn assume_orthogonal_snaps_the_dec_axis() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        scope.set_assume_orthogonal(true, &mut p);
        let mut rig = Rig { north_deg: 110.0, ..Rig::default() };
        let mut out = Vec::new();
        run_calibration(&mut scope, &mut rig, &mut p, &mut out);
        assert!(scope.is_calibrated());
        assert!(out.iter().any(|l| l
            == "DEBUG: Assuming orthogonal axes: measured Y angle = 110.0, X angle = 15.0, orthogonal = 105.0, -75.0, best = 105.0, dist = 27.00, dec_dist = 26.90"));
        let c = scope.mount.mount_cal();
        assert_eq!(c.y_angle, norm_angle(c.x_angle + PI / 2.0));
        assert!((c.y_rate - 0.004 * radians(5.0).cos()).abs() < 1e-12);
    }

    #[test]
    fn advisories_for_an_st4_cable_problem() {
        // On an ST-4 cable, a star that does not come back East raises the
        // east advisory; one that does not come back South only logs its own
        // (PHD2 commented that alert out). No nudge from that far.
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut rig = Rig { east_scale: 0.0, south_rate: 0.0, ..Rig::default() };
        let env = CalibrationEnv { can_pulse_guide: false, ..env() };
        let mut out = Vec::new();
        run_calibration_in(&env, &mut scope, &mut rig, &mut p, &mut out);
        assert!(scope.is_calibrated());
        let east = "Advisory: Little or no east movement was measured, so guiding will probably be impaired. Check the guide cable and use the Manual Guide tool to confirm basic operation of the mount.";
        let k = out.iter().position(|l| l.starts_with("ALERT:")).expect("east advisory");
        assert_eq!(out[k..k + 2], [format!("ALERT: {east}"), format!("DEBUG: Calibration alert: {east}")]);
        assert_eq!(out.iter().filter(|l| l.starts_with("ALERT:")).count(), 1);
        assert!(out.iter().any(|l| l
            == "DEBUG: Omitted calibration alert: Advisory: Calibration successful but little or no south movement was measured, so guiding will probably be impaired.\n This is usually caused by a faulty guide cable or very large Dec backlash. \nCheck the guide cable and read the online Help for how to identify these types of problems (Manual Guide, Declination backlash)."));
        assert!(!out.iter().any(|l| l.contains("NudgeSouth")));
    }

    #[test]
    fn a_different_dec_rate_is_flagged() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut out = Vec::new();
        run_calibration(&mut scope, &mut Rig::default(), &mut p, &mut out);
        out.clear();
        let mut rig = Rig { north_rate: 0.0052, ..Rig::default() };
        run_calibration(&mut scope, &mut rig, &mut p, &mut out);
        assert!(scope.is_calibrated());
        assert_eq!(scope.last_calibration_issue().0, CalibrationIssue::Different);
        assert_eq!(p.i["/scope/calibration/last_issue"], 4);
        let k = out.iter().position(|l| l.starts_with("ALERT:")).expect("advisory");
        assert_eq!(
            out[k..k + 2],
            [
                "ALERT: Advisory: This calibration is substantially different from the previous one - have you changed configurations?",
                "DEBUG: Calibration alert details: Current/previous Dec rate ratio is 0.769",
            ]
        );
    }

    #[test]
    fn a_guide_speed_change_resizes_the_step() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        run_calibration(&mut scope, &mut Rig::default(), &mut p, &mut Vec::new());
        // 0.5× → 1× sidereal: 25 px at 1.96″/px in 12 steps is 272 ms → 300.
        let rig = Rig { guide_speed: 15.0 * 0.9973 / 3600.0, ..Rig::default() };
        let mut ev = Vec::new();
        assert!(scope.begin_calibration(Some(rig.pos()), &env(), Some(&rig), &mut p, &mut ev));
        assert_eq!(
            rendered(&mut ev),
            ["DEBUG: CalDuration adjusted at start of calibration from 750 to 300 because of  mount guide speed  change"]
        );
        assert_eq!((scope.calibration_duration(), p.i["/scope/CalibrationDuration"]), (300, 300));
        // No star, or not connected: refused.
        assert!(!scope.begin_calibration(None, &env(), Some(&rig), &mut p, &mut ev));
        scope.mount.disconnect();
        assert!(!scope.begin_calibration(Some(rig.pos()), &env(), Some(&rig), &mut p, &mut ev));
    }

    #[test]
    fn flip_calibration_moves_to_the_other_side() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut ev = Vec::new();
        assert!(!scope.flip_calibration("T1", &mut p, &mut ev));
        assert_eq!(rendered(&mut ev), ["DEBUG: Error thrown from mount.cpp:899->cannot flip if not calibrated"]);

        scope.set_calibration(&cal_15_105(), "T0", &mut p, &mut ev);
        ev.clear();
        assert!(scope.flip_calibration("T1", &mut p, &mut ev));
        let n = ev.len();
        assert_eq!(ev[n - 2], MountEvent::Status("CAL: East(15,105)->West(-165,105)".into()));
        assert_eq!(ev[n - 1], MountEvent::CalibrationDataFlipped);
        let c = scope.mount.mount_cal();
        assert_eq!((c.pier_side, c.dec_guide_parity), (PierSide::West, Parity::Odd));
        assert_eq!(c.x_angle, norm_angle(radians(15.0) + PI));
        assert_eq!(p.s["/scope/calibration/timestamp"], "T1");
        assert_eq!(p.i["/scope/calibration/pierSide"], 1);
        assert!(scope.is_calibrated());
    }

    #[test]
    fn scope_pointing_adjustment_compensates_and_stores_a_flip() {
        let mut p = Profile::default();
        let mut scope = connected_scope(&mut p, 0.0);
        let mut ev = Vec::new();
        scope.set_calibration(&cal_15_105(), "T0", &mut p, &mut ev);
        let mut sp = ScopePointing {
            declination: Some(radians(60.0)),
            pier_side: PierSide::East,
            rotator_angle: None,
            binning: 1,
            guide_rates: None,
            camera_pixel_size: 3.8,
            profile_pixel_size: 3.8,
        };
        let adj = scope.adjust_calibration_for_scope_pointing(&sp, "T1", &mut p, &mut ev);
        assert!(adj.stored.is_none() && !adj.flipped);
        assert_eq!(scope.mount.x_rate(), (0.004 / radians(20.0).cos()) * radians(60.0).cos());
        assert_eq!(p.s["/scope/calibration/timestamp"], "T0");

        sp.pier_side = PierSide::West;
        let adj = scope.adjust_calibration_for_scope_pointing(&sp, "T2", &mut p, &mut ev);
        assert!(adj.flipped);
        assert_eq!(p.s["/scope/calibration/timestamp"], "T2");
        assert_eq!(p.i["/scope/calibration/pierSide"], 1);
        assert!(scope.is_calibrated());
    }
}
