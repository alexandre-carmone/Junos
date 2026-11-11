//! The step guider: PHD2's `StepGuider`, an adaptive-optics unit that moves
//! the guide star in whole steps within a small travel, with the mount
//! behind it doing the large corrections.
//!
//! It owns its settings (calibration steps per iteration, samples to
//! average, the bump zone and step size, bump on dither), tracks where the
//! AO sits in its travel (`m_xOffset`, `m_yOffset`), makes the moves
//! (`MoveAxis`: truncated at the travel's end, recentred when the device
//! reports a limit), runs the calibration state machine (lower-right
//! corner, average, full travel left, average, full travel up, average,
//! recentre) and, while guiding, decides when the mount must be "bumped" to
//! bring the AO back towards the centre of its travel.
//!
//! Nothing here moves the mount or waits: the calibration hands back the
//! AO moves PHD2 schedules (`ScheduleAxisMove`), and [`StepGuider::move_offset`]
//! hands back the mount bump (`ScheduleSecondaryMove` with
//! `MoveOpts::AO_BUMP`); the engine makes them. The device comes in through
//! [`StepGuiderDevice`], whatever PHD2 tells the outside as [`MountEvent`]s.
//!
//! An AO calibration ends with [`MountEvent::CalibrationComplete`] like the
//! scope's, but PHD2 only writes the guide-log line for it and never sends
//! the `CalibrationComplete` event (`stepguider.cpp:762`): the event server
//! drops it for the AO, which only shows in the catch-up on connect. The
//! AO's [`MountEvent::CalibrationAxisComplete`] rates are px per step, which
//! the guide log prints × 1000 as "px/sec", as PHD2 does.
//!
//! Ported from PHD2 src/stepguider.h, src/stepguider.cpp, src/myframe.cpp
//! (the dither bump), src/myframe_events.cpp (the AO limit alert) —
//! BSD-3-Clause, see THIRD_PARTY.md

use super::algo::{AlgoCtx, MountClass};
use super::calibration::{
    self, Adjustment, Calibration, CalibrationDetails, CalibrationIssue, ProfileAccess, ROTATOR_POSITION_UNKNOWN,
    UNKNOWN_DECLINATION, degrees, point_angle,
};
use super::log::cfmt::fmt_f;
use super::mount::{
    AlertKind, AxisMover, CalibrationStepInfo, GuiderOffset, Mount, MountEvent, MoveResultInfo, StepFrame,
    dump_move_option_bits,
};
use super::scope::CalibrationEnv;
use super::traits::{Dir, MoveOpts, MoveResult, Parity, PierSide, Point};

/// `DefaultSamplesToAverage`: frames averaged at each end of a calibration leg.
pub const DEFAULT_SAMPLES_TO_AVERAGE: i32 = 3;
/// `DefaultBumpPercentage`: how much of the travel is used before bumping.
pub const DEFAULT_BUMP_PERCENTAGE: i32 = 80;
/// `DefaultBumpMaxStepsPerCycle`: a bump's size per frame, in AO steps.
pub const DEFAULT_BUMP_MAX_STEPS_PER_CYCLE: f64 = 1.0;
/// `DefaultCalibrationStepsPerIteration`: AO steps per calibration frame.
pub const DEFAULT_CALIBRATION_STEPS_PER_ITERATION: i32 = 4;
/// The `/stepguider/BumpSettlingBoost` default: the least bump weight while
/// settling. The key is only read, never written.
pub const DEFAULT_BUMP_SETTLING_BOOST: f64 = 3.0;
/// `AO_CALIBRATION_PIXELS_NEEDED`: what `CalibrationTotDistance` assumes an
/// AO calibration needs from the frame edge, px.
pub const AO_CALIBRATION_PIXELS_NEEDED: i32 = 25;

/// `BumpWarnTime`: a bump still running after this long raises an alert, s.
const BUMP_WARN_TIME: i64 = 240;
/// The calibration gives up past this many failed moves.
const MAX_CALIBRATION_MOVE_ERRORS: i32 = 12;
/// The bump ends within this percentage of the travel from the centre.
const BUMP_CENTER_TOLERANCE_PCT: i32 = 10;
/// `m_avgOffset`'s weight for the newest position.
const AVG_ALPHA: f64 = 0.33;

const KEY_SAMPLES_TO_AVERAGE: &str = "/stepguider/SamplesToAverage";
const KEY_BUMP_PERCENTAGE: &str = "/stepguider/BumpPercentage";
const KEY_BUMP_MAX_STEPS_PER_CYCLE: &str = "/stepguider/BumpMaxStepsPerCycle";
const KEY_CALIBRATION_STEPS_PER_ITERATION: &str = "/stepguider/CalibrationStepsPerIteration";
const KEY_BUMP_ON_DITHER: &str = "/stepguider/BumpOnDither";
const KEY_BUMP_SETTLING_BOOST: &str = "/stepguider/BumpSettlingBoost";

/// The AO device: what PHD2's `StepGuider` subclasses implement (the INDI
/// SX and SBIG AOs, the simulator). The device is connected and
/// disconnected outside the engine (the INDI layer waits up to 30 s for it);
/// the step guider is told with [`StepGuider::connect`].
pub trait StepGuiderDevice {
    /// PHD2's `m_Name`, which each AO class sets in its constructor: the
    /// engine copies it into the mount half (`Mount::set_name`) when the
    /// device is chosen.
    fn name(&self) -> String;
    /// `Step`: move `steps` (> 0) one way, fire-and-forget. `Ok`, `AoLimit`
    /// when the device hit the end of its travel (PHD2's
    /// `STEP_LIMIT_REACHED`), anything else is a failure.
    fn step(&mut self, dir: Dir, steps: u32) -> MoveResult;
    /// `MaxPosition`: the travel from the centre to one end, in steps.
    fn max_position(&self, dir: Dir) -> i32;
    /// `SetMaxPosition`: the AO Travel setting, which the device keeps in
    /// its own profile key. True on error.
    fn set_max_position(&mut self, steps: i32) -> bool;
    /// `Center`: return to the centre of the travel. True when it did; the
    /// step guider then zeroes its position, as each PHD2 device does.
    fn center(&mut self) -> bool;
    /// The device's own limit sensor (INDI `AT_LIMIT`), `None` when it has
    /// none or has not reported yet. PHD2 2.6.14 never asks.
    fn is_at_limit(&self, _dir: Dir) -> Option<bool> {
        None
    }
}

/// `StepInfo`: where a step that hit the travel limit started and how far
/// it asked to go, for the limit alert.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepInfo {
    pub x: i32,
    pub y: i32,
    pub dx: i32,
    pub dy: i32,
}

/// The AO's `CALIBRATION_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AoCalibrationState {
    Cleared,
    GotoLowerRightCorner,
    AverageStartingLocation,
    GoLeft,
    AverageCenterLocation,
    GoUp,
    AverageEndingLocation,
    Recenter,
    Complete,
}

/// What [`StepGuider::update_calibration_state`] asks for next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AoCalibrationNext {
    /// Make these AO moves ([`StepGuider::move_axis`] with
    /// `MoveOpts::CALIBRATION_MOVE`) before the next frame, in this order
    /// (Up, Down, Right, Left — at most two). Empty while averaging, and once
    /// calibrated (see [`StepGuider::is_calibrated`]).
    Moves(Vec<(Dir, i32)>),
    /// The calibration gave up and was cleared (PHD2's `true` return).
    Failed,
}

/// What the bump logic reads from the rest of the rig on each move.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BumpEnv {
    /// A secondary mount exists and is connected.
    pub secondary_connected: bool,
    /// A secondary mount exists and still has a move pending (`IsBusy`).
    pub secondary_busy: bool,
    /// `pGuider->GetMaxMovePixels()`.
    pub max_move_pixels: i32,
    /// `PhdController::IsSettling()`.
    pub settling: bool,
    /// Engine clock, seconds: the bump's start and its 240 s warning
    /// (wall-clock `wxGetUTCTime` seconds in PHD2).
    pub now: f64,
    /// The slow-bump warning was turned off with its "Don't show again"
    /// button (the global `/Confirm/<profile id>/SlowBumpWarningEnabled`
    /// false): only a debug line then.
    pub slow_bump_warning_suppressed: bool,
}

/// `DirectionChar` of the AO (`U`, `D`, `R`, `L`).
fn ao_char(dir: Dir) -> char {
    dir.ao_label().chars().next().unwrap_or('?')
}

fn debug(events: &mut Vec<MountEvent>, line: impl Into<String>) {
    events.push(MountEvent::Debug(line.into()));
}

/// `IntegerPercent`: `percentage · number / 100` in `long`, truncated.
fn integer_percent(percentage: i32, number: i32) -> i32 {
    (percentage as i64 * number as i64 / 100) as i32
}

/// `DIV_ROUND_UP` on ints.
fn div_round_up(x: i32, y: i32) -> i32 {
    (x + y - 1) / y
}

/// Where the AO sits in its travel, apart from the rest so a move can
/// borrow it next to the [`Mount`].
#[derive(Clone, Copy, Debug, Default)]
struct AoPosition {
    /// `m_xOffset`: positive to the right.
    x: i32,
    /// `m_yOffset`: positive up.
    y: i32,
    /// `m_failedStep`.
    failed_step: StepInfo,
}

impl AoPosition {
    /// `CurrentPosition`: steps from the centre towards `dir`.
    fn current_position(&self, dir: Dir) -> i32 {
        match dir {
            Dir::North => self.y,
            Dir::South => -self.y,
            Dir::East => self.x,
            Dir::West => -self.x,
        }
    }

    /// `WouldHitLimit`: the move would reach the end of the travel.
    fn would_hit_limit(&self, dev: &dyn StepGuiderDevice, dir: Dir, steps: i32) -> bool {
        self.current_position(dir) + steps >= dev.max_position(dir)
    }

    /// `StepGuider::MoveAxis` (`stepguider.cpp:902`). A move that would reach
    /// the end of the travel is cut to stop one step short (`limited`). When
    /// the device reports its limit anyway, the failed step is kept for the
    /// alert, the AO is recentred and the move reports `AoLimit`. Nothing
    /// moves with guiding disabled unless the move is manual.
    fn move_axis(
        &mut self,
        guiding_enabled: bool,
        dir: Dir,
        steps: i32,
        opts: MoveOpts,
        dev: &mut dyn StepGuiderDevice,
        events: &mut Vec<MountEvent>,
    ) -> (MoveResult, MoveResultInfo) {
        let mut steps = steps;
        let mut result = MoveResult::Ok;
        let mut limit_reached = false;
        debug(events, format!("MoveAxis({}, {}, {})", ao_char(dir), steps, dump_move_option_bits(opts)));

        let failed = 'move_: {
            if !guiding_enabled && !opts.has(MoveOpts::MANUAL) {
                debug(events, "Throw from stepguider.cpp:916->Guiding disabled");
                break 'move_ true;
            }
            if steps > 0 {
                let (xd, yd) = match dir {
                    Dir::North => (0, 1),
                    Dir::South => (0, -1),
                    Dir::East => (1, 0),
                    Dir::West => (-1, 0),
                };
                debug(events, format!("stepping ({}, {}) + ({}, {})", self.x, self.y, steps * xd, steps * yd));

                if self.would_hit_limit(dev, dir, steps) {
                    // Quirk kept: with the travel shortened below the current
                    // position this goes negative, and is reported as moved.
                    let new_steps = dev.max_position(dir) - 1 - self.current_position(dir);
                    debug(
                        events,
                        format!(
                            "StepGuider step would hit limit: truncate move to ({}, {}) + ({}, {})",
                            self.x,
                            self.y,
                            new_steps * xd,
                            new_steps * yd
                        ),
                    );
                    steps = new_steps;
                    limit_reached = true;
                }

                if steps > 0 {
                    let sres = dev.step(dir, steps as u32);
                    if sres != MoveResult::Ok {
                        if sres == MoveResult::AoLimit {
                            debug(events, "AO: limit reached!");
                            self.failed_step = StepInfo { x: self.x, y: self.y, dx: xd * steps, dy: yd * steps };
                            // Recover by centering.
                            if dev.center() {
                                self.x = 0;
                                self.y = 0;
                            } else {
                                debug(events, "AO Center failed after limit reached");
                            }
                            result = MoveResult::AoLimit;
                        }
                        debug(events, "Error thrown from stepguider.cpp:985->step failed");
                        break 'move_ true;
                    }
                    self.x += xd * steps;
                    self.y += yd * steps;
                    debug(events, format!("stepped: pos ({}, {})", self.x, self.y));
                }
            }
            false
        };
        if failed {
            steps = 0;
            if result == MoveResult::Ok {
                result = MoveResult::Error;
            }
        }
        (result, MoveResultInfo { amount_moved: steps, limited: limit_reached })
    }
}

/// The AO's [`AxisMover`] for [`Mount::move_offset`].
struct AoMover<'a> {
    pos: &'a mut AoPosition,
    dev: &'a mut dyn StepGuiderDevice,
}

impl AxisMover for AoMover<'_> {
    fn move_axis(
        &mut self,
        guiding_enabled: bool,
        dir: Dir,
        amount: i32,
        opts: MoveOpts,
        events: &mut Vec<MountEvent>,
    ) -> (MoveResult, MoveResultInfo) {
        self.pos.move_axis(guiding_enabled, dir, amount, opts, self.dev, events)
    }

    fn ao_pos(&self) -> (i32, i32) {
        (self.pos.x, self.pos.y)
    }
}

/// PHD2's `StepGuider`.
pub struct StepGuider {
    /// The `Mount` half: algorithms (Hysteresis on both axes by default),
    /// guiding enabled (`AOGuidingEnabled`), the calibration in use.
    /// Setting or clearing the calibration goes through the step guider's
    /// own methods, which keep its copy and its state machine.
    pub mount: Mount,
    pos: AoPosition,
    samples_to_average: i32,
    bump_percentage: i32,
    bump_max_steps_per_cycle: f64,
    bump_on_dither: bool,
    calibration_steps_per_iteration: i32,
    x_bump_pos1: i32,
    x_bump_pos2: i32,
    y_bump_pos1: i32,
    y_bump_pos2: i32,
    bump_center_tolerance: i32,
    /// `m_avgOffset`: the moving average of the AO position; `None` is
    /// PHD2's invalid point.
    avg_offset: Option<Point>,
    force_start_bump: bool,
    bump_in_progress: bool,
    bump_timeout_alert_sent: bool,
    /// Whole seconds of the engine clock.
    bump_start_time: i64,
    bump_step_weight: f64,
    /// The bump drawn on the AO graph, AO steps (`ShowBump`).
    shown_bump: Option<Point>,
    calibration_iterations: i32,
    calibration_starting_location: Option<Point>,
    calibration_average_samples: i32,
    calibration_averaged_location: Point,
    /// `m_calibration`: the calibration being made, and a copy of the last
    /// one set; the bump reads its rates.
    calibration: Calibration,
    calibration_details: CalibrationDetails,
    calibration_state: AoCalibrationState,
}

impl StepGuider {
    /// `StepGuider::StepGuider` (`stepguider.cpp:53`): the settings from the
    /// profile through their setters, so the effective values are written
    /// back; `BumpOnDither` is only read. [`Mount::new`] builds the guide
    /// algorithms first (`now` is the engine clock); their lines are moved
    /// after the settings', where PHD2's constructor sets them. The bump
    /// zone is set at [`connect`](Self::connect), when the travel is known.
    pub fn new(profile: &mut impl ProfileAccess, now: f64, events: &mut Vec<MountEvent>) -> StepGuider {
        let mut algo_events = Vec::new();
        let mount = Mount::new(MountClass::StepGuider, profile, now, &mut algo_events);
        let mut s = StepGuider {
            mount,
            pos: AoPosition::default(),
            samples_to_average: DEFAULT_SAMPLES_TO_AVERAGE,
            bump_percentage: DEFAULT_BUMP_PERCENTAGE,
            bump_max_steps_per_cycle: DEFAULT_BUMP_MAX_STEPS_PER_CYCLE,
            bump_on_dither: true,
            calibration_steps_per_iteration: DEFAULT_CALIBRATION_STEPS_PER_ITERATION,
            x_bump_pos1: 0,
            x_bump_pos2: 0,
            y_bump_pos1: 0,
            y_bump_pos2: 0,
            bump_center_tolerance: 0,
            avg_offset: None,
            force_start_bump: false,
            bump_in_progress: false,
            bump_timeout_alert_sent: false,
            bump_start_time: 0,
            bump_step_weight: 1.0,
            shown_bump: None,
            calibration_iterations: 0,
            calibration_starting_location: None,
            calibration_average_samples: 0,
            calibration_averaged_location: Point::default(),
            calibration: Calibration::default(),
            // PHD2 leaves the member's pier side uninitialised; it is never
            // set before being stored, so the port stores Unknown.
            calibration_details: CalibrationDetails::default(),
            calibration_state: AoCalibrationState::Cleared,
        };
        let v = profile.get_i32(KEY_SAMPLES_TO_AVERAGE, DEFAULT_SAMPLES_TO_AVERAGE);
        s.set_samples_to_average(v, profile, events);
        let v = profile.get_i32(KEY_BUMP_PERCENTAGE, DEFAULT_BUMP_PERCENTAGE);
        s.set_bump_percentage(v, None, profile, events);
        let v = profile.get_f64(KEY_BUMP_MAX_STEPS_PER_CYCLE, DEFAULT_BUMP_MAX_STEPS_PER_CYCLE);
        s.set_bump_max_steps_per_cycle(v, profile, events);
        let v = profile.get_i32(KEY_CALIBRATION_STEPS_PER_ITERATION, DEFAULT_CALIBRATION_STEPS_PER_ITERATION);
        s.set_calibration_steps_per_iteration(v, profile, events);
        // The guide algorithms come next in PHD2: `Mount::new` built them.
        events.append(&mut algo_events);
        s.bump_on_dither = profile.get_i32(KEY_BUMP_ON_DITHER, 1) != 0;
        s
    }

    // ------------------------------------------------------------ settings

    /// `GetSamplesToAverage`.
    pub fn samples_to_average(&self) -> i32 {
        self.samples_to_average
    }

    /// `SetSamplesToAverage`: not positive → 3. Always written back.
    /// Returns PHD2's error flag.
    pub fn set_samples_to_average(
        &mut self,
        n: i32,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let error = n <= 0;
        if error {
            debug(events, "Error thrown from stepguider.cpp:251->invalid samplesToAverage");
            self.samples_to_average = DEFAULT_SAMPLES_TO_AVERAGE;
        } else {
            self.samples_to_average = n;
        }
        profile.set_i32(KEY_SAMPLES_TO_AVERAGE, self.samples_to_average);
        error
    }

    /// `GetBumpPercentage`.
    pub fn bump_percentage(&self) -> i32 {
        self.bump_percentage
    }

    /// `SetBumpPercentage`: not positive → 80. Always written back. With
    /// the device (PHD2's `updateGraph`, the settings dialog) the bump zone
    /// is recomputed at once; otherwise at the next connect.
    pub fn set_bump_percentage(
        &mut self,
        pct: i32,
        dev: Option<&dyn StepGuiderDevice>,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let error = pct <= 0;
        if error {
            debug(events, "Error thrown from stepguider.cpp:281->invalid bumpPercentage");
            self.bump_percentage = DEFAULT_BUMP_PERCENTAGE;
        } else {
            self.bump_percentage = pct;
        }
        profile.set_i32(KEY_BUMP_PERCENTAGE, self.bump_percentage);
        if let Some(dev) = dev {
            self.init_bump_positions(dev, events);
        }
        error
    }

    /// `GetBumpMaxStepsPerCycle`.
    pub fn bump_max_steps_per_cycle(&self) -> f64 {
        self.bump_max_steps_per_cycle
    }

    /// `SetBumpMaxStepsPerCycle`: not positive → 1.0. Always written back.
    pub fn set_bump_max_steps_per_cycle(
        &mut self,
        steps: f64,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let error = steps <= 0.0;
        if error {
            debug(events, "Error thrown from stepguider.cpp:317->invalid bumpStepsPerCycle");
            self.bump_max_steps_per_cycle = DEFAULT_BUMP_MAX_STEPS_PER_CYCLE;
        } else {
            self.bump_max_steps_per_cycle = steps;
        }
        profile.set_f64(KEY_BUMP_MAX_STEPS_PER_CYCLE, self.bump_max_steps_per_cycle);
        error
    }

    /// `GetCalibrationStepsPerIteration`.
    pub fn calibration_steps_per_iteration(&self) -> i32 {
        self.calibration_steps_per_iteration
    }

    /// `SetCalibrationStepsPerIteration`: not positive → 4. Always written
    /// back.
    pub fn set_calibration_steps_per_iteration(
        &mut self,
        n: i32,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        let error = n <= 0;
        if error {
            debug(events, "Error thrown from stepguider.cpp:353->invalid calibrationStepsPerIteration");
            self.calibration_steps_per_iteration = DEFAULT_CALIBRATION_STEPS_PER_ITERATION;
        } else {
            self.calibration_steps_per_iteration = n;
        }
        profile.set_i32(KEY_CALIBRATION_STEPS_PER_ITERATION, self.calibration_steps_per_iteration);
        error
    }

    /// `GetBumpOnDither`.
    pub fn bump_on_dither(&self) -> bool {
        self.bump_on_dither
    }

    /// `SetBumpOnDither`.
    pub fn set_bump_on_dither(&mut self, val: bool, profile: &mut impl ProfileAccess) {
        self.bump_on_dither = val;
        profile.set_i32(KEY_BUMP_ON_DITHER, val as i32);
    }

    /// `CalibrationMoveSize`: the manual-guide default, AO steps.
    pub fn calibration_move_size(&self) -> i32 {
        self.calibration_steps_per_iteration
    }

    /// `CalibrationTotDistance`: the AO calibration's assumed reach, px.
    pub fn calibration_tot_distance(&self) -> i32 {
        AO_CALIBRATION_PIXELS_NEEDED
    }

    /// `StepGuider::GetSettingsSummary` (`stepguider.cpp:1288`): the mount
    /// block ([`Mount::settings_summary`], `aux_mount` as there), then the
    /// bump settings and the stored calibration details' time stamp. The
    /// details are loaded first, then again by the mount block when
    /// calibrated; `validated` is the pointing source's once-per-session
    /// guide-speed flag, which both loads share.
    pub fn settings_summary(
        &self,
        aux_mount: Option<&str>,
        profile: &mut impl ProfileAccess,
        mut validated: Option<&mut bool>,
        events: &mut Vec<MountEvent>,
    ) -> String {
        let details = self.mount.load_calibration_details(profile, validated.as_deref_mut(), events);
        let summary = self.mount.settings_summary(self.is_calibrated(), aux_mount, None, profile, validated, events);
        format!(
            "{summary}Bump percentage = {}, Bump step = {}, Timestamp = {}\n",
            self.bump_percentage,
            fmt_f(self.bump_max_steps_per_cycle, 2),
            details.orig_timestamp
        )
    }

    /// `CalibrationSettingsSummary`, the calibration header's AO line.
    pub fn calibration_settings_summary(&self) -> String {
        format!(
            "Calibration steps = {}, Samples to average = {}",
            self.calibration_steps_per_iteration, self.samples_to_average
        )
    }

    // ------------------------------------------------- connection, position

    /// `StepGuider::Connect` once the device is connected: the mount half,
    /// then the bump zone from the device's travel.
    pub fn connect(&mut self, dev: &dyn StepGuiderDevice, events: &mut Vec<MountEvent>) {
        self.mount.connect();
        self.init_bump_positions(dev, events);
    }

    /// `StepGuider::Disconnect`.
    pub fn disconnect(&mut self) {
        self.mount.disconnect();
    }

    /// `InitBumpPositions` (`stepguider.cpp:220`): the bump starts past
    /// `BumpPercentage` of the travel, grows faster past half-way from there
    /// to the end, and ends within 10 % of the full travel from the centre.
    fn init_bump_positions(&mut self, dev: &dyn StepGuiderDevice, events: &mut Vec<MountEvent>) {
        let limit2_pct = (100 + self.bump_percentage) / 2;
        self.x_bump_pos1 = integer_percent(self.bump_percentage, dev.max_position(Dir::LEFT));
        self.x_bump_pos2 = integer_percent(limit2_pct, dev.max_position(Dir::LEFT));
        self.y_bump_pos1 = integer_percent(self.bump_percentage, dev.max_position(Dir::UP));
        self.y_bump_pos2 = integer_percent(limit2_pct, dev.max_position(Dir::UP));
        self.bump_center_tolerance = integer_percent(BUMP_CENTER_TOLERANCE_PCT, 2 * dev.max_position(Dir::UP));
        debug(
            events,
            format!(
                "StepGuider: Bump Limits: X: {}, {}; Y: {}, {}; center: {}",
                self.x_bump_pos1, self.x_bump_pos2, self.y_bump_pos1, self.y_bump_pos2, self.bump_center_tolerance
            ),
        );
    }

    /// `ZeroCurrentPosition`: what the simulator's connect does (a device
    /// that centres also zeroes, see [`StepGuiderDevice::center`]).
    pub fn zero_current_position(&mut self) {
        self.pos.x = 0;
        self.pos.y = 0;
    }

    /// `CurrentPosition`: steps from the centre towards `dir`.
    pub fn current_position(&self, dir: Dir) -> i32 {
        self.pos.current_position(dir)
    }

    /// `GetAoPos`: (x right, y up), steps.
    pub fn ao_pos(&self) -> (i32, i32) {
        (self.pos.x, self.pos.y)
    }

    /// `GetAoMaxPos`: the travel right and up, the `Limit` of the AO's
    /// `CalibrationComplete` catch-up event.
    pub fn ao_max_pos(&self, dev: &dyn StepGuiderDevice) -> (i32, i32) {
        (dev.max_position(Dir::RIGHT), dev.max_position(Dir::UP))
    }

    /// `WouldHitLimit`.
    pub fn would_hit_limit(&self, dev: &dyn StepGuiderDevice, dir: Dir, steps: i32) -> bool {
        self.pos.would_hit_limit(dev, dir, steps)
    }

    /// `IsAtLimit`: the device's sensor when it has one, else the base
    /// class' rule — one step short of the end of the travel.
    pub fn is_at_limit(&self, dev: &dyn StepGuiderDevice, dir: Dir) -> bool {
        dev.is_at_limit(dir).unwrap_or_else(|| self.current_position(dir) == dev.max_position(dir) - 1)
    }

    /// `GetFailedStepInfo`: the last step that hit the travel limit.
    pub fn failed_step_info(&self) -> StepInfo {
        self.pos.failed_step
    }

    /// The `MOVE_ERROR_AO_LIMIT_REACHED` alert of the move-complete handler
    /// (`myframe_events.cpp:647`), from [`failed_step_info`](Self::failed_step_info).
    /// While calibrating the guider then stops; while guiding it stops and
    /// restarts with a non-sticky lock.
    pub fn limit_alert(&self, calibrating: bool) -> String {
        let s = self.pos.failed_step;
        let newval = if s.dx != 0 { (s.x + s.dx).abs() } else { (s.y + s.dy).abs() };
        if calibrating {
            format!(
                "The AO exceeded its travel limit stepping from ({}, {}) to ({}, {}) and calibration cannot proceed. Try reducing the AO Travel setting to {} or lower and try again.",
                s.x,
                s.y,
                s.x + s.dx,
                s.y + s.dy,
                newval
            )
        } else {
            format!(
                "The AO exceeded its travel limit stepping from ({}, {}) to ({}, {}) and has been re-centered. Try reducing the AO Travel setting to {} or lower.",
                s.x,
                s.y,
                s.x + s.dx,
                s.y + s.dy,
                newval
            )
        }
    }

    /// `m_avgOffset`, for the AO graph.
    pub fn avg_offset(&self) -> Option<Point> {
        self.avg_offset
    }

    /// The bump the AO graph shows, AO steps; `None` when no bump runs.
    pub fn shown_bump(&self) -> Option<Point> {
        self.shown_bump
    }

    // ------------------------------------------------------------- moves

    /// The three-argument `StepGuider::MoveAxis` (`stepguider.cpp:860`),
    /// which the calibration and manual moves use: an error unless every
    /// step requested was made.
    pub fn move_axis(
        &mut self,
        dir: Dir,
        steps: i32,
        opts: MoveOpts,
        dev: &mut dyn StepGuiderDevice,
        events: &mut Vec<MountEvent>,
    ) -> MoveResult {
        debug(events, format!("stepguider move axis dir= {} steps= {} opts= 0x{:x}", dir as i32, steps, opts.0));
        let (mut result, info) = self.pos.move_axis(self.mount.guiding_enabled(), dir, steps, opts, dev, events);
        if info.amount_moved != steps {
            debug(events, "Throw from stepguider.cpp:872->stepsTaken != stepsRequested");
            if result == MoveResult::Ok {
                result = MoveResult::Error;
            }
        }
        result
    }

    /// `MoveToCenter` (`stepguider.cpp:376`): step back to the centre, Dec
    /// first. Calibration moves, so nothing moves with guiding disabled.
    /// Returns whether it got there; a failed axis ends it.
    pub fn move_to_center(&mut self, dev: &mut dyn StepGuiderDevice, events: &mut Vec<MountEvent>) -> bool {
        let enabled = self.mount.guiding_enabled();
        let opts = MoveOpts::CALIBRATION_MOVE;
        let pos = &mut self.pos;
        let up_down = pos.current_position(Dir::UP);
        if up_down != 0 {
            let (dir, n, line) = if up_down > 0 {
                (Dir::DOWN, up_down, "Error thrown from stepguider.cpp:390->MoveToCenter() failed to step DOWN")
            } else {
                (Dir::UP, -up_down, "Error thrown from stepguider.cpp:401->MoveToCenter() failed to step UP")
            };
            let (_, r) = pos.move_axis(enabled, dir, n, opts, dev, events);
            if r.amount_moved != n {
                debug(events, line);
                return false;
            }
        }
        let left_right = pos.current_position(Dir::LEFT);
        if left_right != 0 {
            let (dir, n, line) = if left_right > 0 {
                (Dir::RIGHT, left_right, "Error thrown from stepguider.cpp:413->MoveToCenter() failed to step RIGHT")
            } else {
                (Dir::LEFT, -left_right, "Error thrown from stepguider.cpp:424->MoveToCenter() failed to step LEFT")
            };
            let (_, r) = pos.move_axis(enabled, dir, n, opts, dev, events);
            if r.amount_moved != n {
                debug(events, line);
                return false;
            }
        }
        true
    }

    /// `StepGuider::MoveOffset` (`stepguider.cpp:1029`): the AO move
    /// ([`Mount::move_offset`]), then the bump logic. Returns the move's
    /// result and the mount bump to schedule, if any: the caller runs it on
    /// the secondary mount as a camera offset with `MoveOpts::AO_BUMP`.
    ///
    /// The AO position is averaged (EMA, α 0.33) on every non-deduced move.
    /// On a guide step with a connected secondary mount: past
    /// `BumpPercentage` of the travel (or on a forced start) a bump starts;
    /// while it runs and the mount is idle, each frame still outside the
    /// zone grows the bump weight by 1/6 (by 1 past the half-way mark to the
    /// end), capped so a bump stays under half the search region, and a frame
    /// back inside halves it (not below 1). The bump ends once the average
    /// is within 10 % of the full travel from the centre; one still running
    /// after 240 s raises an alert, once. Each frame of a running bump with
    /// the mount idle schedules one: after a move cut at the end of the
    /// travel, 70 % of the star offset (at most half the search region);
    /// otherwise a step towards the centre of `BumpMaxStepsPerCycle` ×
    /// weight AO steps (at least the `BumpSettlingBoost` weight while
    /// settling), never longer than the star offset. `frame` is read once,
    /// after the AO moves (see [`Mount::move_offset`]).
    pub fn move_offset(
        &mut self,
        ofs: &mut GuiderOffset,
        opts: MoveOpts,
        frame: impl FnOnce() -> StepFrame,
        ctx: &AlgoCtx,
        env: &BumpEnv,
        dev: &mut dyn StepGuiderDevice,
        profile: &impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> (MoveResult, Option<GuiderOffset>) {
        let mut mover = AoMover { pos: &mut self.pos, dev };
        let mut result = self.mount.move_offset(ofs, opts, frame, ctx, &mut mover, events);
        if result != MoveResult::Ok {
            debug(events, format!("StepGuider::Move: Mount::Move failed! result {}", result as i32));
        }
        if !self.mount.guiding_enabled() {
            debug(events, "Throw from stepguider.cpp:1041->Guiding disabled");
            if result == MoveResult::Ok {
                result = MoveResult::Error;
            }
            return (result, None);
        }
        if opts.has(MoveOpts::ALGO_DEDUCE) {
            if self.bump_in_progress {
                debug(events, "StepGuider: deferring bump for deduced move");
            }
            return (result, None);
        }

        let (x, y) = (self.pos.x as f64, self.pos.y as f64);
        let avg = match self.avg_offset {
            Some(a) => Point::new(a.x + AVG_ALPHA * (x - a.x), a.y + AVG_ALPHA * (y - a.y)),
            None => Point::new(x, y),
        };
        self.avg_offset = Some(avg);

        if opts.has(MoveOpts::ALGO_RESULT) && env.secondary_connected {
            let abs_x = self.current_position(Dir::RIGHT).abs();
            let abs_y = self.current_position(Dir::UP).abs();
            let is_outside = abs_x > self.x_bump_pos1 || abs_y > self.y_bump_pos1;
            let mut force_start = false;
            if self.force_start_bump {
                debug(events, "StepGuider::Move: will start forced bump");
                force_start = true;
                self.force_start_bump = false;
            }

            // The bump step did not bring the AO back inside: bump harder.
            if is_outside && self.bump_in_progress && !env.secondary_busy {
                let w = self.bump_step_weight;
                if abs_x > self.x_bump_pos2 || abs_y > self.y_bump_pos2 {
                    debug(
                        events,
                        format!(
                            "FAR outside bump range, increase bump weight {} => {}",
                            fmt_f(w, 2),
                            fmt_f(w + 1.0, 2)
                        ),
                    );
                    self.bump_step_weight += 1.0;
                } else {
                    debug(
                        events,
                        format!(
                            "outside bump range, increase bump weight {} => {}",
                            fmt_f(w, 2),
                            fmt_f(w + 1.0 / 6.0, 2)
                        ),
                    );
                    self.bump_step_weight += 1.0 / 6.0;
                }
                // No bump larger than half the search region. `wxMax(a, b)`
                // is `a < b ? b : a`.
                let (xr, yr) = (self.calibration.x_rate, self.calibration.y_rate);
                let move_px = (if xr < yr { yr } else { xr }) * self.bump_max_steps_per_cycle;
                let max_move_px = env.max_move_pixels as f64 * 0.5;
                let max_weight = max_move_px / move_px;
                if self.bump_step_weight > max_weight {
                    self.bump_step_weight = max_weight;
                    debug(events, format!("limit bump weight to {}", fmt_f(max_weight, 1)));
                }
            }

            if !is_outside && self.bump_step_weight > 1.0 {
                let prior = self.bump_step_weight;
                self.bump_step_weight *= 0.5;
                if self.bump_step_weight < 1.0 {
                    self.bump_step_weight = 1.0;
                }
                debug(
                    events,
                    format!(
                        "back inside bump range: decrease bump weight {} => {}",
                        fmt_f(prior, 2),
                        fmt_f(self.bump_step_weight, 2)
                    ),
                );
            }

            let now = env.now.floor() as i64;
            if self.bump_in_progress && !self.bump_timeout_alert_sent && now - self.bump_start_time > BUMP_WARN_TIME {
                let msg = "A mount \"bump\" was needed to bring the AO back to its center position,\nbut the bump did not complete in a reasonable amount of time.\nYou probably need to increase the AO Bump Step setting.";
                if env.slow_bump_warning_suppressed {
                    debug(events, format!("Suppressed alert:  {msg}"));
                } else {
                    // Quirk kept: PHD2 asks for the information icon, which
                    // its `SuppressibleAlert` drops, so the event says
                    // "warning".
                    events.push(MountEvent::Alert { msg: msg.into(), kind: AlertKind::Warning });
                }
                self.bump_timeout_alert_sent = true;
            }

            if (is_outside || force_start) && !self.bump_in_progress {
                self.bump_in_progress = true;
                self.bump_start_time = now;
                self.bump_timeout_alert_sent = false;
                debug(events, "starting a new bump");
            }

            // Close enough to the centre: stop. A forced start tests it at once.
            if (!is_outside || force_start) && self.bump_in_progress {
                let min_dist = self.bump_center_tolerance as f64;
                if avg.x * avg.x + avg.y * avg.y <= min_dist * min_dist {
                    debug(events, "Stop bumping, close enough to center -- clearing m_bumpInProgress");
                    self.bump_in_progress = false;
                    self.shown_bump = None;
                }
            }
        }

        if self.bump_in_progress && env.secondary_busy {
            debug(events, "secondary mount is busy, cannot bump");
        }
        if !self.bump_in_progress || env.secondary_busy {
            return (result, None);
        }

        // `m_lastStep`, which a deduced move with nothing to do leaves as it
        // was: the AO move just made, or an earlier one.
        let limited = self.mount.last_step().is_some_and(|s| s.dec_limited || s.ra_limited);
        let this_bump = if limited {
            // The AO ran out of travel: skip the gentle bump and correct with
            // the mount, 70 % of the offset like the default Hysteresis, at
            // most half the search region.
            let mut b = Point::new(ofs.camera_ofs.x * 0.70, ofs.camera_ofs.y * 0.70);
            let max_dist = env.max_move_pixels as f64 * 0.5;
            let d2 = b.x * b.x + b.y * b.y;
            if d2 > max_dist * max_dist {
                let k = max_dist / d2.sqrt();
                b = Point::new(b.x * k, b.y * k);
            }
            debug(events, "AO travel limit exceeded, using large bump correction");
            b
        } else {
            // A step from the average position towards the centre, turned
            // into camera axes for the mount.
            let endpoint = Point::new(self.mount.x_rate() * -avg.x, self.mount.y_rate() * -avg.y);
            let bump_vec = self.mount.mount_to_camera(endpoint, true, events);
            debug(events, format!("incremental bump ({}, {}) isValid = 1", fmt_f(bump_vec.x, 3), fmt_f(bump_vec.y, 3)));
            let mut weight = self.bump_step_weight;
            if env.settling {
                let boost = profile.get_f64(KEY_BUMP_SETTLING_BOOST, DEFAULT_BUMP_SETTLING_BOOST);
                if weight < boost {
                    weight = boost;
                    debug(events, format!("boost bump step weight to {} for settling", fmt_f(weight, 1)));
                }
            }
            let max_bump_x = self.calibration.x_rate * self.bump_max_steps_per_cycle * weight;
            let max_bump_y = self.calibration.y_rate * self.bump_max_steps_per_cycle * weight;
            let len = bump_vec.x.hypot(bump_vec.y);
            let mut b = Point::new(bump_vec.x * max_bump_x / len, bump_vec.y * max_bump_y / len);
            // No longer than the star offset, or it would overshoot.
            let pixels2 = b.x * b.x + b.y * b.y;
            let max_dist2 = ofs.camera_ofs.x * ofs.camera_ofs.x + ofs.camera_ofs.y * ofs.camera_ofs.y;
            if pixels2 > max_dist2 {
                let k = (max_dist2 / pixels2).sqrt();
                b = Point::new(b.x * k, b.y * k);
            }
            b
        };

        let shown = self.mount.camera_to_mount(this_bump, false, events);
        self.shown_bump = Some(Point::new(shown.x / self.mount.x_rate(), shown.y / self.mount.y_rate()));
        debug(events, format!("Scheduling Mount bump of ({}, {})", fmt_f(this_bump.x, 3), fmt_f(this_bump.y, 3)));
        (result, Some(GuiderOffset { camera_ofs: this_bump, mount_ofs: None }))
    }

    /// `ForceStartBump`: start a bump on the next guide step.
    pub fn force_start_bump(&mut self, events: &mut Vec<MountEvent>) {
        debug(events, "StepGuider: force bump");
        self.force_start_bump = true;
    }

    /// The AO half of `MyFrame::Dither` (`myframe.cpp:2115`): with
    /// `BumpOnDither`, force a bump after the lock moved.
    pub fn dither_bump(&mut self, events: &mut Vec<MountEvent>) {
        if self.bump_on_dither {
            debug(events, "Dither: starting AO bump");
            self.force_start_bump(events);
        }
    }

    /// `IsBumpInProgress`: the controller does not call a settle done while
    /// a bump runs.
    pub fn is_bump_in_progress(&self) -> bool {
        self.bump_in_progress
    }

    /// `StepGuider::NotifyGuidingStopped`: forget the bump and step back to
    /// the centre (a failure is ignored). Quirk kept: PHD2's override does
    /// not chain to `Mount::NotifyGuidingStopped`, so the AO's algorithms
    /// are never told guiding stopped.
    pub fn notify_guiding_stopped(&mut self, dev: &mut dyn StepGuiderDevice, events: &mut Vec<MountEvent>) {
        self.avg_offset = None;
        self.force_start_bump = false;
        self.bump_in_progress = false;
        self.bump_step_weight = 1.0;
        self.bump_timeout_alert_sent = false;
        self.shown_bump = None;
        self.move_to_center(dev, events);
    }

    /// `NotifyGuidingResumed`: the algorithms, and a fresh position average.
    pub fn notify_guiding_resumed(&mut self, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        self.mount.notify_guiding_resumed(ctx, events);
        self.avg_offset = None;
    }

    /// `NotifyGuidingDithered`: the algorithms, and a fresh position average.
    pub fn notify_guiding_dithered(
        &mut self,
        dx: f64,
        dy: f64,
        mount_coords: bool,
        ctx: &AlgoCtx,
        events: &mut Vec<MountEvent>,
    ) {
        self.mount.notify_guiding_dithered(dx, dy, mount_coords, ctx, events);
        self.avg_offset = None;
    }

    // ---------------------------------------------------------- calibration

    /// `Mount::IsCalibrated`: connected and calibrated.
    pub fn is_calibrated(&self) -> bool {
        self.mount.is_calibrated()
    }

    /// Where the state machine is.
    pub fn calibration_state(&self) -> AoCalibrationState {
        self.calibration_state
    }

    /// `StepGuider::SetCalibration`: keep the copy, then the mount's (in
    /// memory, stored with `timestamp`).
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

    /// `StepGuider::ClearCalibration`: not calibrated, state machine back to
    /// the start.
    pub fn clear_calibration(&mut self) {
        self.mount.clear_calibration();
        self.calibration_state = AoCalibrationState::Cleared;
    }

    /// `StepGuider::AdjustCalibrationForScopePointing` at guide start (see
    /// [`calibration::adjust_ao_for_scope_pointing`]): only a binning change
    /// rescales the rates. What it set is stored with `timestamp`; the
    /// debug line is the caller's.
    pub fn adjust_calibration_for_scope_pointing(
        &mut self,
        binning: u16,
        timestamp: &str,
        profile: &mut impl ProfileAccess,
    ) -> Adjustment {
        let adj = calibration::adjust_ao_for_scope_pointing(self.mount.calibration_mut(), binning);
        if let Some(stored) = &adj.stored {
            self.calibration = stored.clone();
            calibration::store_calibration(profile, MountClass::StepGuider, stored, timestamp);
        }
        adj
    }

    /// `StepGuider::BeginCalibration` (`stepguider.cpp:474`): refuses a
    /// disconnected AO or no star; otherwise clears the calibration and
    /// heads for the lower-right corner. Returns whether it began.
    pub fn begin_calibration(&mut self, location: Option<Point>, events: &mut Vec<MountEvent>) -> bool {
        if !self.mount.is_connected() {
            debug(events, "Error thrown from stepguider.cpp:483->Not connected");
            return false;
        }
        if location.is_none() {
            debug(events, "Error thrown from stepguider.cpp:488->Must have a valid start position");
            return false;
        }
        self.clear_calibration();
        self.calibration_state = AoCalibrationState::GotoLowerRightCorner;
        self.calibration_starting_location = None;
        self.calibration_details.ra_steps.clear();
        self.calibration_details.dec_steps.clear();
        self.calibration_details.last_issue = CalibrationIssue::None;
        true
    }

    /// `StepGuider::UpdateCalibrationState` (`stepguider.cpp:543`): one frame
    /// of the calibration, the star at `cur`. The AO is assumed centred at
    /// the start.
    ///
    /// - Lower-right corner: step down and right, `CalibrationStepsPerIteration`
    ///   a frame, to one step short of the end of the travel.
    /// - Average the star over `SamplesToAverage` frames (the arrival frame
    ///   counts).
    /// - Left: the whole travel, logging a `Left` row each frame (the step
    ///   number counts down the iterations left), then average: the X angle
    ///   points from that average to the corner's, the X rate is the distance
    ///   over the steps travelled, `(maxL − 1) + (maxR − 1)`, px per step.
    /// - Up: the same for Y, from the left end's average to the top's.
    /// - Recentre: right and down, a full iteration at a time, until less
    ///   than one is left on each axis.
    /// - Complete: no declination, pier side or parity (an AO turns with the
    ///   camera), the rotator and binning stamped, set and stored with the
    ///   details. No sanity check.
    ///
    /// More than 12 failed moves give up. The status line comes every frame.
    pub fn update_calibration_state(
        &mut self,
        cur: Point,
        env: &CalibrationEnv,
        dev: &mut dyn StepGuiderDevice,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> AoCalibrationNext {
        use AoCalibrationState::*;

        let max_up = dev.max_position(Dir::UP);
        let max_down = dev.max_position(Dir::DOWN);
        let max_right = dev.max_position(Dir::RIGHT);
        let max_left = dev.max_position(Dir::LEFT);

        if self.mount.error_count() > MAX_CALIBRATION_MOVE_ERRORS {
            events.push(MountEvent::Alert {
                msg: "The AO is failing to move and calibration cannot complete. Check the Debug Log for more information."
                    .into(),
                kind: AlertKind::Warning,
            });
            debug(
                events,
                format!(
                    "stepguider calibration failure, current pos = {:+},{:+}, required range = {:+}..{:+},{:+}..{:+}",
                    self.pos.x,
                    self.pos.y,
                    -(max_left - 1),
                    max_right - 1,
                    -(max_down - 1),
                    max_up - 1
                ),
            );
            debug(events, "Error thrown from stepguider.cpp:562->too many move errors during calibration");
            self.clear_calibration();
            return AoCalibrationNext::Failed;
        }

        if self.calibration_starting_location.is_none() {
            self.calibration_starting_location = Some(cur);
            debug(
                events,
                format!(
                    "Stepguider::UpdateCalibrationstate: starting location = {},{}",
                    fmt_f(cur.x, 2),
                    fmt_f(cur.y, 2)
                ),
            );
        }

        let spi = self.calibration_steps_per_iteration;
        let steps_remaining_up = max_up - 1 - self.current_position(Dir::UP);
        let steps_remaining_down = max_down - 1 - self.current_position(Dir::DOWN);
        let steps_remaining_right = max_right - 1 - self.current_position(Dir::RIGHT);
        let steps_remaining_left = max_left - 1 - self.current_position(Dir::LEFT);
        let iter_remaining_up = div_round_up(steps_remaining_up, spi);
        let iter_remaining_down = div_round_up(steps_remaining_down, spi);
        let iter_remaining_right = div_round_up(steps_remaining_right, spi);
        let iter_remaining_left = div_round_up(steps_remaining_left, spi);
        let iter_remaining_down_and_right = iter_remaining_down.max(iter_remaining_right);

        let (mut move_up, mut move_down, mut move_right, mut move_left) = (false, false, false, false);
        let mut status0 = String::new();

        // A calibration row: the guide-log line and the `Calibrating` event.
        let step_row = |dir: &'static str, step: i32, start: Point, msg: &str, events: &mut Vec<MountEvent>| {
            let row = CalibrationStepInfo {
                direction: dir,
                step,
                dx: start.x - cur.x,
                dy: start.y - cur.y,
                pos: cur,
                dist: start.distance(&cur),
                msg: String::new(),
            };
            events.push(MountEvent::CalibrationRow(row.clone()));
            events.push(MountEvent::Calibrating(CalibrationStepInfo { msg: msg.to_string(), ..row }));
            Point::new(start.x - cur.x, start.y - cur.y)
        };

        loop {
            let start = self.calibration_starting_location.unwrap_or(cur);
            match self.calibration_state {
                // PHD2 hits its `default: assert(false)` here.
                Cleared => break,

                GotoLowerRightCorner => {
                    if iter_remaining_down_and_right > 0 {
                        status0 = format!("Init Calibration: {:3}", iter_remaining_down_and_right);
                        move_down = steps_remaining_down > 0;
                        move_right = steps_remaining_right > 0;
                        break;
                    }
                    debug(
                        events,
                        format!(
                            "Falling through to state AVERAGE_STARTING_LOCATION, position=({}, {})",
                            fmt_f(cur.x, 2),
                            fmt_f(cur.y, 2)
                        ),
                    );
                    self.calibration_average_samples = 0;
                    self.calibration_averaged_location = Point::default();
                    self.calibration_state = AverageStartingLocation;
                }

                AverageStartingLocation => {
                    if !self.average(cur, &mut status0) {
                        break;
                    }
                    self.calibration_starting_location = Some(self.calibration_averaged_location);
                    self.calibration_iterations = 0;
                    let s = self.calibration_averaged_location;
                    debug(
                        events,
                        format!(
                            "Falling through to state GO_LEFT, startinglocation=({}, {})",
                            fmt_f(s.x, 2),
                            fmt_f(s.y, 2)
                        ),
                    );
                    self.calibration_state = GoLeft;
                }

                GoLeft => {
                    if steps_remaining_left > 0 {
                        status0 = format!("Left Calibration: {:3}", iter_remaining_left);
                        self.calibration_iterations += 1;
                        move_left = true;
                        // "Left" goes in the RA steps.
                        let d = step_row("Left", iter_remaining_left, start, &status0, events);
                        self.calibration_details.ra_steps.push(d);
                        break;
                    }
                    debug(
                        events,
                        format!(
                            "Falling through to state AVERAGE_CENTER_LOCATION, position=({}, {})",
                            fmt_f(cur.x, 2),
                            fmt_f(cur.y, 2)
                        ),
                    );
                    self.calibration_average_samples = 0;
                    self.calibration_averaged_location = Point::default();
                    self.calibration_state = AverageCenterLocation;
                }

                AverageCenterLocation => {
                    if !self.average(cur, &mut status0) {
                        break;
                    }
                    let avg = self.calibration_averaged_location;
                    // `start.Angle(avg)`: from the left end towards the start.
                    self.calibration.x_angle = point_angle(start - avg);
                    self.calibration.x_rate = start.distance(&avg) / ((max_left - 1) + (max_right - 1)) as f64;
                    events.push(MountEvent::CalibrationAxisComplete {
                        direction: "Left",
                        angle: self.calibration.x_angle,
                        rate: self.calibration.x_rate,
                        parity: Parity::Unknown,
                    });
                    debug(
                        events,
                        format!(
                            "LEFT calibration completes with angle={} rate={}",
                            fmt_f(degrees(self.calibration.x_angle), 1),
                            fmt_f(self.calibration.x_rate, 2)
                        ),
                    );
                    debug(
                        events,
                        format!(
                            "distance={} iterations={}",
                            fmt_f(start.distance(&avg), 2),
                            self.calibration_iterations
                        ),
                    );
                    self.calibration_starting_location = Some(avg);
                    self.calibration_iterations = 0;
                    self.calibration_state = GoUp;
                    debug(
                        events,
                        format!(
                            "Falling through to state GO_UP, startinglocation=({}, {})",
                            fmt_f(avg.x, 2),
                            fmt_f(avg.y, 2)
                        ),
                    );
                }

                GoUp => {
                    if steps_remaining_up > 0 {
                        status0 = format!("Up Calibration: {:3}", iter_remaining_up);
                        self.calibration_iterations += 1;
                        move_up = true;
                        // "Up" goes in the Dec steps.
                        let d = step_row("Up", iter_remaining_up, start, &status0, events);
                        self.calibration_details.dec_steps.push(d);
                        break;
                    }
                    debug(
                        events,
                        format!(
                            "Falling through to state AVERAGE_ENDING_LOCATION, position=({}, {})",
                            fmt_f(cur.x, 2),
                            fmt_f(cur.y, 2)
                        ),
                    );
                    self.calibration_average_samples = 0;
                    self.calibration_averaged_location = Point::default();
                    self.calibration_state = AverageEndingLocation;
                }

                AverageEndingLocation => {
                    if !self.average(cur, &mut status0) {
                        break;
                    }
                    let avg = self.calibration_averaged_location;
                    // `avg.Angle(start)`: from the left end towards the top.
                    self.calibration.y_angle = point_angle(avg - start);
                    self.calibration.y_rate = start.distance(&avg) / ((max_up - 1) + (max_down - 1)) as f64;
                    events.push(MountEvent::CalibrationAxisComplete {
                        direction: "Up",
                        angle: self.calibration.y_angle,
                        rate: self.calibration.y_rate,
                        parity: Parity::Unknown,
                    });
                    debug(
                        events,
                        format!(
                            "UP calibration completes with angle={} rate={}",
                            fmt_f(degrees(self.calibration.y_angle), 1),
                            fmt_f(self.calibration.y_rate, 2)
                        ),
                    );
                    debug(
                        events,
                        format!(
                            "distance={} iterations={}",
                            fmt_f(start.distance(&avg), 2),
                            self.calibration_iterations
                        ),
                    );
                    self.calibration_starting_location = Some(avg);
                    self.calibration_state = Recenter;
                    debug(
                        events,
                        format!(
                            "Falling through to state RECENTER, position=({}, {})",
                            fmt_f(cur.x, 2),
                            fmt_f(cur.y, 2)
                        ),
                    );
                }

                Recenter => {
                    status0 = format!("Re-centering: {:3}", iter_remaining_down_and_right / 2);
                    move_right = self.current_position(Dir::LEFT) >= spi;
                    move_down = self.current_position(Dir::UP) >= spi;
                    if move_right || move_down {
                        debug(
                            events,
                            format!(
                                "CurrentPosition(LEFT)={} CurrentPosition(UP)={}",
                                self.current_position(Dir::LEFT),
                                self.current_position(Dir::UP)
                            ),
                        );
                        break;
                    }
                    self.calibration_state = Complete;
                    debug(
                        events,
                        format!(
                            "Falling through to state COMPLETE, position=({}, {})",
                            fmt_f(cur.x, 2),
                            fmt_f(cur.y, 2)
                        ),
                    );
                }

                Complete => {
                    let cal = Calibration {
                        declination: UNKNOWN_DECLINATION,
                        pier_side: PierSide::Unknown,
                        ra_guide_parity: Parity::Unknown,
                        dec_guide_parity: Parity::Unknown,
                        rotator_angle: env.rotator_angle.unwrap_or(ROTATOR_POSITION_UNKNOWN),
                        binning: env.binning,
                        ..self.calibration.clone()
                    };
                    self.set_calibration(&cal, &env.timestamp, profile, events);
                    self.set_calibration_details(env, profile, events);
                    status0 = "Calibration complete".into();
                    events.push(MountEvent::CalibrationComplete);
                    debug(events, "Calibration Complete");
                    break;
                }
            }
        }

        let mut moves = Vec::new();
        if move_up {
            moves.push((Dir::UP, steps_remaining_up.min(spi)));
        }
        if move_down {
            moves.push((Dir::DOWN, steps_remaining_down.min(spi)));
        }
        if move_right {
            moves.push((Dir::RIGHT, steps_remaining_right.min(spi)));
        }
        if move_left {
            moves.push((Dir::LEFT, steps_remaining_left.min(spi)));
        }

        let mut status1 = String::new();
        if self.calibration_state != Complete {
            let dist = self.calibration_starting_location.unwrap_or(cur).distance(&cur);
            status1 = format!("distance {:>4} px", fmt_f(dist, 1));
        }
        if !status0.is_empty() {
            if !status1.is_empty() {
                status0 = format!("{status0}, {status1}");
            }
            events.push(MountEvent::Status(status0));
        } else if !status1.is_empty() {
            events.push(MountEvent::Status(status1));
        }
        AoCalibrationNext::Moves(moves)
    }

    /// One averaging frame: adds `cur`, sets the status. True once
    /// `SamplesToAverage` frames are in, the average then taken.
    fn average(&mut self, cur: Point, status: &mut String) -> bool {
        self.calibration_average_samples += 1;
        self.calibration_averaged_location = self.calibration_averaged_location + cur;
        *status = format!("Averaging: {:3}", self.samples_to_average - self.calibration_average_samples + 1);
        if self.calibration_average_samples < self.samples_to_average {
            return false;
        }
        let n = self.calibration_average_samples as f64;
        let s = self.calibration_averaged_location;
        self.calibration_averaged_location = Point::new(s.x / n, s.y / n);
        true
    }

    /// `StepGuider::SetCalibrationDetails` (`stepguider.cpp:513`): no guide
    /// speeds (−1), the optics, the axes' orthogonality error, the step
    /// counts, the binning and a time stamp, then stored. The pointing
    /// source's guide-speed check never runs on the −1s.
    fn set_calibration_details(
        &mut self,
        env: &CalibrationEnv,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        let d = &mut self.calibration_details;
        d.ra_guide_speed = -1.0;
        d.dec_guide_speed = -1.0;
        d.focal_length = env.focal_length;
        d.image_scale = env.image_scale;
        d.ortho_error = calibration::ortho_error_deg(self.calibration.x_angle, self.calibration.y_angle);
        d.ra_step_count = d.ra_steps.len() as i32;
        d.dec_step_count = d.dec_steps.len() as i32;
        d.orig_binning = env.binning as f64;
        d.orig_timestamp = env.timestamp.clone();
        let details = d.clone();
        self.mount.save_calibration_details(&details, profile, None, events);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::engine::algo::{AlgoKind, Axis};
    use crate::engine::calibration::{AdjustStep, radians};
    use crate::engine::log::cfmt::fmt_gp;

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
            self.f.get(key).copied().unwrap_or(default)
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

    /// The harness' AO: it keeps its own position and refuses a step that
    /// would take either axis past `hard_limit`.
    struct Ao {
        max: i32,
        hard_limit: i32,
        x: i32,
        y: i32,
        center_ok: bool,
    }

    impl Ao {
        fn new(max: i32) -> Ao {
            Ao { max, hard_limit: 999, x: 0, y: 0, center_ok: true }
        }
    }

    impl StepGuiderDevice for Ao {
        fn name(&self) -> String {
            "AO-Simulator".into()
        }
        fn step(&mut self, dir: Dir, steps: u32) -> MoveResult {
            let n = steps as i32;
            let (nx, ny) = match dir {
                Dir::North => (self.x, self.y + n),
                Dir::South => (self.x, self.y - n),
                Dir::East => (self.x + n, self.y),
                Dir::West => (self.x - n, self.y),
            };
            if nx.abs() > self.hard_limit || ny.abs() > self.hard_limit {
                return MoveResult::AoLimit;
            }
            (self.x, self.y) = (nx, ny);
            MoveResult::Ok
        }
        fn max_position(&self, _dir: Dir) -> i32 {
            self.max
        }
        fn set_max_position(&mut self, steps: i32) -> bool {
            self.max = steps;
            false
        }
        fn center(&mut self) -> bool {
            if self.center_ok {
                (self.x, self.y) = (0, 0);
            }
            self.center_ok
        }
    }

    /// The star: 0.3 px per AO step right along 20°, 0.25 px per step up
    /// along 112°, plus what the mount moved and a fixed noise pattern.
    struct Sky {
        mx: f64,
        my: f64,
        noise: f64,
    }

    impl Sky {
        fn star(&self, frame: i32, ao: &Ao) -> Point {
            let (ux, uy) = (0.3 * radians(20.0).cos(), 0.3 * radians(20.0).sin());
            let (vx, vy) = (0.25 * radians(112.0).cos(), 0.25 * radians(112.0).sin());
            let nx = ((frame * 7) % 5 - 2) as f64 * self.noise;
            let ny = ((frame * 3) % 7 - 3) as f64 * self.noise;
            Point::new(
                100.0 + self.mx + nx + ux * ao.x as f64 + vx * ao.y as f64,
                80.0 + self.my + ny + uy * ao.x as f64 + vy * ao.y as f64,
            )
        }
    }

    fn g(v: f64) -> String {
        fmt_gp(v, 17)
    }

    /// The events the way the C++ harness prints PHD2's calls.
    fn render(events: &mut Vec<MountEvent>, out: &mut Vec<String>) {
        for e in events.drain(..) {
            let line = match e {
                MountEvent::Debug(s) => format!("DEBUG: {s}"),
                MountEvent::Alert { msg, .. } => format!("ALERT: {msg}"),
                MountEvent::Status(s) => format!("STATUS: {s}"),
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
                MountEvent::CalibrationComplete => "COMPLETE: AO".into(),
                MountEvent::GuideStep(s) => format!(
                    "STEP: opts={} frame={} ao={} cam={},{} mnt={},{} gd={},{} ra={} {} lim={} dec={} {} lim={} pos={},{}",
                    s.move_options.0,
                    s.frame_number,
                    s.is_ao as i32,
                    g(s.camera_offset.x),
                    g(s.camera_offset.y),
                    g(s.mount_offset.x),
                    g(s.mount_offset.y),
                    g(s.guide_distance_ra),
                    g(s.guide_distance_dec),
                    s.duration_ra,
                    s.direction_ra.ao_label(),
                    s.ra_limited as i32,
                    s.duration_dec,
                    s.direction_dec.ao_label(),
                    s.dec_limited as i32,
                    s.ao_pos.0,
                    s.ao_pos.1
                ),
                other => format!("UNEXPECTED: {other:?}"),
            };
            out.push(line.replace('\n', "\\n"));
        }
    }

    fn rendered(ev: &mut Vec<MountEvent>) -> Vec<String> {
        let mut out = Vec::new();
        render(ev, &mut out);
        out
    }

    fn cal_env() -> CalibrationEnv {
        CalibrationEnv {
            binning: 1,
            focal_length: 1000,
            image_scale: 0.78,
            timestamp: "T".into(),
            ..CalibrationEnv::default()
        }
    }

    fn connected(profile: &mut Profile, ao: &Ao) -> StepGuider {
        let mut sg = StepGuider::new(profile, 0.0, &mut Vec::new());
        sg.connect(ao, &mut Vec::new());
        sg
    }

    /// The AO calibrated with 0.3 px/step along 20° and 0.25 px/step along
    /// 112°, centred.
    fn calibrated(profile: &mut Profile, ao: &Ao) -> StepGuider {
        let mut sg = connected(profile, ao);
        let cal = Calibration {
            x_rate: 0.3,
            y_rate: 0.25,
            x_angle: radians(20.0),
            y_angle: radians(112.0),
            declination: UNKNOWN_DECLINATION,
            ..Calibration::default()
        };
        sg.set_calibration(&cal, "T", profile, &mut Vec::new());
        sg
    }

    fn guide(
        sg: &mut StepGuider,
        ao: &mut Ao,
        profile: &Profile,
        cam: Point,
        opts: MoveOpts,
        env: &BumpEnv,
        ev: &mut Vec<MountEvent>,
    ) -> (MoveResult, Option<GuiderOffset>) {
        let mut ofs = GuiderOffset { camera_ofs: cam, mount_ofs: None };
        let ctx = AlgoCtx { now: env.now, ..AlgoCtx::default() };
        sg.move_offset(&mut ofs, opts, StepFrame::default, &ctx, env, ao, profile, ev)
    }

    #[test]
    fn calibration_guiding_and_bumps_match_phd2() {
        // The whole transcript of PHD2's C++ on the same scripted rig
        // (stepguider.cpp's calibration, MoveAxis, MoveOffset with its bump
        // logic, MoveToCenter and summaries; mount.cpp's MoveOffset and
        // transforms; Hysteresis): a 45-step AO calibrated 5 steps a frame,
        // then 75 guide frames with the star drifting 0.7 px a frame for 30
        // frames — the bump starts past 80 %, grows, is capped by the search
        // region, takes the large correction when a move is cut at the end
        // of the travel, halves back inside — the secondary mount busy every
        // fifth frame, the slow-bump alert, a dither's forced bump, a
        // deduced move, a settling boost, a device limit that recentres,
        // then the stop's recentring and two manual moves.
        const GOLDEN: &str = include_str!("testdata/stepguider_phd2.txt");

        let mut profile = Profile::default();
        profile.set_i32(KEY_CALIBRATION_STEPS_PER_ITERATION, 5);
        profile.set_f64(KEY_BUMP_MAX_STEPS_PER_CYCLE, 1.5);
        let mut out = Vec::new();
        let mut ev = Vec::new();
        let mut ao = Ao::new(45);
        let mut sky = Sky { mx: 0.0, my: 0.0, noise: 0.01 };
        let mut sg = StepGuider::new(&mut profile, 0.0, &mut ev);
        sg.connect(&ao, &mut ev);
        render(&mut ev, &mut out);

        let mut frame = 0;
        let began = sg.begin_calibration(Some(sky.star(frame, &ao)), &mut ev);
        render(&mut ev, &mut out);
        out.push(format!("BEGIN: {}", began as i32));
        let env = cal_env();
        while !sg.is_calibrated() {
            frame += 1;
            out.push(format!("FRAME: {frame}"));
            let next = sg.update_calibration_state(sky.star(frame, &ao), &env, &mut ao, &mut profile, &mut ev);
            render(&mut ev, &mut out);
            let AoCalibrationNext::Moves(moves) = next else { panic!("calibration failed") };
            for (dir, n) in moves {
                let r = sg.move_axis(dir, n, MoveOpts::CALIBRATION_MOVE, &mut ao, &mut ev);
                render(&mut ev, &mut out);
                out.push(format!("CALMOVE: {} {n} -> {}", ao_char(dir), r as i32));
                if r != MoveResult::Ok {
                    sg.mount.increment_error_count();
                }
            }
        }
        let c = sg.mount.mount_cal();
        out.push(format!(
            "CALIBRATION: {} {} {} {} pos={},{}",
            g(c.x_angle),
            g(c.x_rate),
            g(c.y_angle),
            g(c.y_rate),
            sg.ao_pos().0,
            sg.ao_pos().1
        ));
        let d = calibration::load_calibration_details(&profile, MountClass::StepGuider);
        out.push(format!(
            "DETAILS: n={}/{} ortho={} fl={} scale={} speeds={}/{} bin={} ts={} side={} issue={}",
            d.ra_step_count,
            d.dec_step_count,
            g(d.ortho_error),
            d.focal_length,
            g(d.image_scale),
            d.ra_guide_speed,
            d.dec_guide_speed,
            d.orig_binning,
            d.orig_timestamp,
            d.orig_pier_side as i32,
            d.last_issue as i32
        ));

        let lock = sky.star(frame, &ao);
        sky.noise = 0.12;
        sg.mount.algo_mut(Axis::X).set_min_move(0.05);
        sg.mount.algo_mut(Axis::Y).set_min_move(0.05);
        for n in 1..=75 {
            frame += 1;
            sky.mx += if n <= 30 { 0.7 } else { 0.02 };
            sky.my -= if n <= 30 { 0.2 } else { 0.01 };
            let env = BumpEnv {
                secondary_connected: true,
                secondary_busy: n % 5 == 2,
                max_move_pixels: 4,
                settling: (50..54).contains(&n),
                now: n as f64 * 10.0,
                slow_bump_warning_suppressed: false,
            };
            if n == 40 {
                sg.dither_bump(&mut ev);
                render(&mut ev, &mut out);
            }
            ao.hard_limit = if n == 70 { 0 } else { 999 };
            let opts = if n == 45 { MoveOpts::DEDUCED_MOVE } else { MoveOpts::GUIDE_STEP };
            let cam = sky.star(frame, &ao) - lock;
            out.push(format!("GUIDE: {n}"));
            let mut ofs = GuiderOffset { camera_ofs: cam, mount_ofs: None };
            let ctx = AlgoCtx { now: env.now, ..AlgoCtx::default() };
            let sf = StepFrame { frame_number: frame, ..StepFrame::default() };
            let (r, bump) = sg.move_offset(&mut ofs, opts, || sf, &ctx, &env, &mut ao, &profile, &mut ev);
            render(&mut ev, &mut out);
            let avg = sg.avg_offset().unwrap();
            out.push(format!(
                "RESULT: {} pos={},{} avg={},{} inprog={} w={}",
                r as i32,
                sg.ao_pos().0,
                sg.ao_pos().1,
                g(avg.x),
                g(avg.y),
                sg.is_bump_in_progress() as i32,
                g(sg.bump_step_weight)
            ));
            if r == MoveResult::AoLimit {
                out.push(format!("LIMIT: {}", sg.limit_alert(false)));
            }
            if let Some(b) = bump {
                let shown = sg.shown_bump().unwrap();
                out.push(format!("SHOWBUMP: {} {}", g(shown.x), g(shown.y)));
                out.push(format!("BUMP: {} {}", g(b.camera_ofs.x), g(b.camera_ofs.y)));
                assert_eq!(b.mount_ofs, None);
                sky.mx -= b.camera_ofs.x;
                sky.my -= b.camera_ofs.y;
            }
        }
        sg.notify_guiding_stopped(&mut ao, &mut ev);
        render(&mut ev, &mut out);
        out.push(format!("STOPPED: pos={},{}", sg.ao_pos().0, sg.ao_pos().1));
        assert_eq!((sg.avg_offset(), sg.shown_bump(), sg.is_bump_in_progress()), (None, None, false));
        for (dir, steps) in [(Dir::RIGHT, 50), (Dir::LEFT, 44)] {
            let r = sg.move_axis(dir, steps, MoveOpts::MANUAL, &mut ao, &mut ev);
            render(&mut ev, &mut out);
            out.push(format!("MANUAL: {}", r as i32));
        }
        let summary = sg.settings_summary(None, &mut profile, None, &mut ev);
        out.push(format!("SUMMARY: {}\\n", summary.lines().last().unwrap()));
        out.push(format!("SUMMARY: {}", sg.calibration_settings_summary()));
        assert!(ev.is_empty());

        let golden: Vec<&str> = GOLDEN.lines().collect();
        for (k, (r, c)) in out.iter().zip(&golden).enumerate() {
            assert_eq!(r, c, "line {}", k + 1);
        }
        assert_eq!(out.len(), golden.len());
        // The device followed the steps the AO counted.
        assert_eq!((ao.x, ao.y), sg.ao_pos());
    }

    #[test]
    fn settings_fall_back_to_their_defaults_and_are_written_back() {
        let mut profile = Profile::default();
        profile.set_i32(KEY_SAMPLES_TO_AVERAGE, 0);
        profile.set_i32(KEY_BUMP_PERCENTAGE, -5);
        profile.set_f64(KEY_BUMP_MAX_STEPS_PER_CYCLE, 0.0);
        profile.set_i32(KEY_CALIBRATION_STEPS_PER_ITERATION, 0);
        profile.set_i32(KEY_BUMP_ON_DITHER, 0);
        profile.set_i32("/stepguider/XGuideAlgorithm", 9);
        let mut ev = Vec::new();
        let mut sg = StepGuider::new(&mut profile, 0.0, &mut ev);
        // The settings' lines come before the algorithms', as in PHD2.
        assert_eq!(
            rendered(&mut ev),
            [
                "DEBUG: Error thrown from stepguider.cpp:251->invalid samplesToAverage",
                "DEBUG: Error thrown from stepguider.cpp:281->invalid bumpPercentage",
                "DEBUG: Error thrown from stepguider.cpp:317->invalid bumpStepsPerCycle",
                "DEBUG: Error thrown from stepguider.cpp:353->invalid calibrationStepsPerIteration",
                "DEBUG: Error thrown from mount.cpp:742->invalid guideAlgorithm",
            ]
        );
        assert_eq!(sg.samples_to_average(), 3);
        assert_eq!(sg.bump_percentage(), 80);
        assert_eq!(sg.bump_max_steps_per_cycle(), 1.0);
        assert_eq!(sg.calibration_steps_per_iteration(), 4);
        assert!(!sg.bump_on_dither());
        assert_eq!(sg.calibration_move_size(), 4);
        assert_eq!(sg.calibration_tot_distance(), 25);
        assert_eq!(
            (
                profile.i[KEY_SAMPLES_TO_AVERAGE],
                profile.i[KEY_BUMP_PERCENTAGE],
                profile.i[KEY_CALIBRATION_STEPS_PER_ITERATION]
            ),
            (3, 80, 4)
        );
        assert_eq!(profile.f[KEY_BUMP_MAX_STEPS_PER_CYCLE], 1.0);
        assert_eq!(profile.i["/stepguider/XGuideAlgorithm"], AlgoKind::Hysteresis as i32);
        assert_eq!(sg.mount.guide_algorithm_selection(Axis::Y), AlgoKind::Hysteresis);

        sg.set_bump_on_dither(true, &mut profile);
        assert_eq!(profile.i[KEY_BUMP_ON_DITHER], 1);
        // With the device the bump zone follows at once: 50 % of 45 steps,
        // 75 % for the faster growth, 10 % of the 90-step travel.
        let ao = Ao::new(45);
        assert!(!sg.set_bump_percentage(50, Some(&ao), &mut profile, &mut ev));
        assert_eq!(rendered(&mut ev), ["DEBUG: StepGuider: Bump Limits: X: 22, 33; Y: 22, 33; center: 9"]);
        assert!(!sg.set_bump_percentage(90, None, &mut profile, &mut ev));
        assert!(ev.is_empty());
        assert_eq!(sg.calibration_settings_summary(), "Calibration steps = 4, Samples to average = 3");
    }

    #[test]
    fn calibration_is_refused_or_gives_up_after_12_failed_moves() {
        let mut profile = Profile::default();
        let mut ev = Vec::new();
        let mut ao = Ao::new(10);
        let mut sg = StepGuider::new(&mut profile, 0.0, &mut ev);
        let p = Point::new(50.0, 40.0);
        assert!(!sg.begin_calibration(Some(p), &mut ev));
        sg.connect(&ao, &mut ev);
        ev.clear();
        assert!(!sg.begin_calibration(None, &mut ev));
        assert_eq!(sg.calibration_state(), AoCalibrationState::Cleared);
        assert!(sg.begin_calibration(Some(p), &mut ev));
        assert_eq!(sg.calibration_state(), AoCalibrationState::GotoLowerRightCorner);
        assert_eq!(
            rendered(&mut ev),
            ["DEBUG: Error thrown from stepguider.cpp:488->Must have a valid start position"]
        );

        // Twelve failures are tolerated: the first frame heads for the
        // corner, 4 steps down and right.
        for _ in 0..12 {
            sg.mount.increment_error_count();
        }
        let next = sg.update_calibration_state(p, &cal_env(), &mut ao, &mut profile, &mut ev);
        assert_eq!(next, AoCalibrationNext::Moves(vec![(Dir::DOWN, 4), (Dir::RIGHT, 4)]));
        assert_eq!(
            rendered(&mut ev),
            [
                "DEBUG: Stepguider::UpdateCalibrationstate: starting location = 50.00,40.00",
                "STATUS: Init Calibration:   3, distance  0.0 px",
            ]
        );
        sg.mount.increment_error_count();
        let next = sg.update_calibration_state(p, &cal_env(), &mut ao, &mut profile, &mut ev);
        assert_eq!(next, AoCalibrationNext::Failed);
        assert_eq!(
            rendered(&mut ev),
            [
                "ALERT: The AO is failing to move and calibration cannot complete. Check the Debug Log for more information.",
                "DEBUG: stepguider calibration failure, current pos = +0,+0, required range = -9..+9,-9..+9",
                "DEBUG: Error thrown from stepguider.cpp:562->too many move errors during calibration",
            ]
        );
        assert_eq!(sg.calibration_state(), AoCalibrationState::Cleared);
        assert!(!sg.is_calibrated());
    }

    #[test]
    fn moves_stop_short_of_the_travel_end_and_recentre_on_a_limit() {
        let mut profile = Profile::default();
        let mut ev = Vec::new();
        let mut ao = Ao::new(10);
        let mut sg = StepGuider::new(&mut profile, 0.0, &mut ev);
        sg.connect(&ao, &mut ev);
        ev.clear();

        // Cut one step short of the end: an error for the three-argument
        // move, which wants every step.
        assert_eq!(sg.move_axis(Dir::UP, 12, MoveOpts::MANUAL, &mut ao, &mut ev), MoveResult::Error);
        assert_eq!(
            rendered(&mut ev),
            [
                "DEBUG: stepguider move axis dir= 0 steps= 12 opts= 0x10",
                "DEBUG: MoveAxis(U, 12, M)",
                "DEBUG: stepping (0, 0) + (0, 12)",
                "DEBUG: StepGuider step would hit limit: truncate move to (0, 0) + (0, 9)",
                "DEBUG: stepped: pos (0, 9)",
                "DEBUG: Throw from stepguider.cpp:872->stepsTaken != stepsRequested",
            ]
        );
        assert_eq!((sg.ao_pos(), sg.ao_max_pos(&ao)), ((0, 9), (10, 10)));
        assert_eq!((sg.current_position(Dir::UP), sg.current_position(Dir::DOWN)), (9, -9));
        assert!(sg.is_at_limit(&ao, Dir::UP) && !sg.is_at_limit(&ao, Dir::DOWN));
        assert!(sg.would_hit_limit(&ao, Dir::UP, 1) && !sg.would_hit_limit(&ao, Dir::DOWN, 18));

        // The device refuses the step: the AO recentres and reports the limit.
        ao.hard_limit = 8;
        assert_eq!(sg.move_axis(Dir::LEFT, 3, MoveOpts::CALIBRATION_MOVE, &mut ao, &mut ev), MoveResult::AoLimit);
        assert_eq!(
            rendered(&mut ev)[2..],
            [
                "DEBUG: stepping (0, 9) + (-3, 0)",
                "DEBUG: AO: limit reached!",
                "DEBUG: Error thrown from stepguider.cpp:985->step failed",
                "DEBUG: Throw from stepguider.cpp:872->stepsTaken != stepsRequested",
            ]
        );
        assert_eq!((sg.ao_pos(), (ao.x, ao.y)), ((0, 0), (0, 0)));
        assert_eq!(sg.failed_step_info(), StepInfo { x: 0, y: 9, dx: -3, dy: 0 });
        assert_eq!(
            sg.limit_alert(true),
            "The AO exceeded its travel limit stepping from (0, 9) to (-3, 9) and calibration cannot proceed. Try reducing the AO Travel setting to 3 or lower and try again."
        );
        assert_eq!(
            sg.limit_alert(false),
            "The AO exceeded its travel limit stepping from (0, 9) to (-3, 9) and has been re-centered. Try reducing the AO Travel setting to 3 or lower."
        );

        // A failed recentre keeps the count.
        ao.hard_limit = 999;
        sg.move_axis(Dir::RIGHT, 4, MoveOpts::CALIBRATION_MOVE, &mut ao, &mut ev);
        (ao.hard_limit, ao.center_ok) = (0, false);
        assert_eq!(sg.move_axis(Dir::DOWN, 2, MoveOpts::CALIBRATION_MOVE, &mut ao, &mut ev), MoveResult::AoLimit);
        assert!(rendered(&mut ev).contains(&"DEBUG: AO Center failed after limit reached".to_string()));
        assert_eq!(sg.ao_pos(), (4, 0));

        // Guiding disabled: only manual moves step, and the recentring fails.
        ao.hard_limit = 999;
        sg.mount.set_guiding_enabled(false, &AlgoCtx::default(), &mut ev);
        ev.clear();
        assert_eq!(sg.move_axis(Dir::UP, 1, MoveOpts::CALIBRATION_MOVE, &mut ao, &mut ev), MoveResult::Error);
        assert_eq!(rendered(&mut ev)[2], "DEBUG: Throw from stepguider.cpp:916->Guiding disabled");
        assert!(!sg.move_to_center(&mut ao, &mut ev));
        assert_eq!(
            rendered(&mut ev),
            [
                "DEBUG: MoveAxis(L, 4, -)",
                "DEBUG: Throw from stepguider.cpp:916->Guiding disabled",
                "DEBUG: Error thrown from stepguider.cpp:424->MoveToCenter() failed to step LEFT",
            ]
        );
        assert_eq!(sg.move_axis(Dir::LEFT, 4, MoveOpts::MANUAL, &mut ao, &mut ev), MoveResult::Ok);
        assert_eq!(sg.ao_pos(), (0, 0));
    }

    /// Hysteresis with a 5 px minimum move, so a guide step leaves the AO
    /// where it is, 37 steps right: one past the 80 % zone of a 45-step AO.
    fn parked_right(profile: &mut Profile, ao: &mut Ao) -> StepGuider {
        let mut sg = calibrated(profile, ao);
        sg.mount.algo_mut(Axis::X).set_min_move(5.0);
        sg.mount.algo_mut(Axis::Y).set_min_move(5.0);
        sg.move_axis(Dir::RIGHT, 37, MoveOpts::MANUAL, ao, &mut Vec::new());
        sg
    }

    #[test]
    fn a_bump_starts_past_the_zone_and_schedules_mount_moves() {
        let mut profile = Profile::default();
        let mut ao = Ao::new(45);
        let mut sg = parked_right(&mut profile, &mut ao);
        let mut ev = Vec::new();
        let cam = Point::new(2.0, 1.0);
        let mut env = BumpEnv { max_move_pixels: 20, now: 100.0, ..BumpEnv::default() };

        // No secondary mount: nothing starts.
        let (r, bump) = guide(&mut sg, &mut ao, &profile, cam, MoveOpts::GUIDE_STEP, &env, &mut ev);
        assert_eq!((r, bump, sg.is_bump_in_progress()), (MoveResult::Ok, None, false));
        assert_eq!(sg.avg_offset(), Some(Point::new(37.0, 0.0)));

        // With one, the bump starts and moves the mount one AO step (×
        // weight 1) towards the centre: along -160°, against the AO's +x
        // at 20°, 0.3 px per step on X and 0.25 on Y.
        env.secondary_connected = true;
        ev.clear();
        let (r, bump) = guide(&mut sg, &mut ao, &profile, cam, MoveOpts::GUIDE_STEP, &env, &mut ev);
        assert_eq!(r, MoveResult::Ok);
        let b = bump.unwrap();
        assert_eq!(b.mount_ofs, None);
        assert!((b.camera_ofs.x - -0.281907786235772).abs() < 1e-12, "{:?}", b);
        assert!((b.camera_ofs.y - -0.0855050358314172).abs() < 1e-12, "{:?}", b);
        assert!(sg.is_bump_in_progress());
        let lines = rendered(&mut ev);
        let tail: Vec<&str> = lines.iter().map(|s| s.as_str()).skip_while(|l| !l.starts_with("STEP")).skip(1).collect();
        assert_eq!(
            tail,
            [
                "DEBUG: starting a new bump",
                "DEBUG: MountToCamera -- mountTheta (-3.14) + m_xAngle (0.35) = xAngle (-2.79 = -2.79)",
                "DEBUG: MountToCamera -- mountX=-11.10 mountY=-0.00 hyp=11.10 mountTheta=-3.14 cameraX=-10.43, cameraY=-3.80 cameraTheta=-2.79",
                "DEBUG: incremental bump (-10.431, -3.796) isValid = 1",
                "DEBUG: Scheduling Mount bump of (-0.282, -0.086)",
            ]
        );
        // Shown on the AO graph in AO steps.
        let shown = sg.shown_bump().unwrap();
        assert!((shown.x - -0.981).abs() < 0.001 && (shown.y - 0.023).abs() < 0.001, "{shown:?}");

        // Still outside with the mount idle: the weight grows by 1/6.
        let (_, bump) = guide(&mut sg, &mut ao, &profile, cam, MoveOpts::GUIDE_STEP, &env, &mut ev);
        assert!(bump.is_some());
        assert!(
            rendered(&mut ev).contains(&"DEBUG: outside bump range, increase bump weight 1.00 => 1.17".to_string())
        );
        assert_eq!(sg.bump_step_weight, 1.0 + 1.0 / 6.0);

        // The mount still busy with it: no new bump, the weight kept.
        env.secondary_busy = true;
        let (_, bump) = guide(&mut sg, &mut ao, &profile, cam, MoveOpts::GUIDE_STEP, &env, &mut ev);
        assert_eq!(bump, None);
        assert_eq!(rendered(&mut ev).last().unwrap(), "DEBUG: secondary mount is busy, cannot bump");
        assert_eq!(sg.bump_step_weight, 1.0 + 1.0 / 6.0);

        // A deduced move defers it.
        env.secondary_busy = false;
        let (_, bump) = guide(&mut sg, &mut ao, &profile, Point::default(), MoveOpts::DEDUCED_MOVE, &env, &mut ev);
        assert_eq!(bump, None);
        assert_eq!(rendered(&mut ev), ["DEBUG: StepGuider: deferring bump for deduced move"]);

        // Guiding disabled: the move fails and nothing is bumped.
        sg.mount.set_guiding_enabled(false, &AlgoCtx::default(), &mut ev);
        ev.clear();
        let (r, bump) = guide(&mut sg, &mut ao, &profile, cam, MoveOpts::GUIDE_STEP, &env, &mut ev);
        assert_eq!((r, bump), (MoveResult::Error, None));
        let lines = rendered(&mut ev);
        assert_eq!(
            lines[lines.len() - 2..],
            [
                "DEBUG: StepGuider::Move: Mount::Move failed! result 1",
                "DEBUG: Throw from stepguider.cpp:1041->Guiding disabled",
            ]
        );
        sg.mount.set_guiding_enabled(true, &AlgoCtx::default(), &mut ev);

        // Stopping clears the bump and steps back to the centre.
        sg.notify_guiding_stopped(&mut ao, &mut ev);
        assert_eq!((sg.ao_pos(), sg.is_bump_in_progress(), sg.bump_step_weight), ((0, 0), false, 1.0));
    }

    #[test]
    fn a_slow_bump_warns_once_unless_suppressed() {
        const MSG: &str = "A mount \"bump\" was needed to bring the AO back to its center position,\nbut the bump did not complete in a reasonable amount of time.\nYou probably need to increase the AO Bump Step setting.";
        for suppressed in [false, true] {
            let mut profile = Profile::default();
            let mut ao = Ao::new(45);
            let mut sg = parked_right(&mut profile, &mut ao);
            let mut ev = Vec::new();
            let mut env = BumpEnv {
                secondary_connected: true,
                max_move_pixels: 20,
                slow_bump_warning_suppressed: suppressed,
                ..BumpEnv::default()
            };
            let mut warned = Vec::new();
            // Started at 0 s; still running at 240.9 s (whole seconds), at
            // 241 s, then at 300 s.
            for now in [0.0, 240.9, 241.0, 300.0] {
                env.now = now;
                guide(&mut sg, &mut ao, &profile, Point::new(2.0, 1.0), MoveOpts::GUIDE_STEP, &env, &mut ev);
                for e in ev.drain(..) {
                    match e {
                        MountEvent::Alert { msg, kind } => warned.push((now, msg, Some(kind))),
                        MountEvent::Debug(l) if l.starts_with("Suppressed") => warned.push((now, l, None)),
                        _ => {}
                    }
                }
            }
            let want = if suppressed {
                (241.0, format!("Suppressed alert:  {MSG}"), None)
            } else {
                (241.0, MSG.to_string(), Some(AlertKind::Warning))
            };
            assert_eq!(warned, [want]);
        }
    }

    #[test]
    fn a_limited_move_takes_the_large_bump_and_settling_boosts_the_small_one() {
        let mut profile = Profile::default();
        let mut ao = Ao::new(45);
        let mut sg = parked_right(&mut profile, &mut ao);
        sg.mount.algo_mut(Axis::X).set_min_move(0.05);
        let mut ev = Vec::new();
        let env = BumpEnv { secondary_connected: true, max_move_pixels: 4, settling: true, ..BumpEnv::default() };

        // 4 px along the AO's -x (the star at 200°): Hysteresis asks for
        // 2.52 px, 8 steps right, past the 44th step, so the move is cut to
        // 7 and the bump is 70 % of the offset, at most half the 4 px search
        // region.
        let cam = Point::new(4.0 * radians(200.0).cos(), 4.0 * radians(200.0).sin());
        let (r, bump) = guide(&mut sg, &mut ao, &profile, cam, MoveOpts::GUIDE_STEP, &env, &mut ev);
        assert_eq!(r, MoveResult::Ok);
        let step = sg.mount.last_step().unwrap();
        assert_eq!((step.duration_ra, step.direction_ra, step.ra_limited), (7, Dir::RIGHT, true));
        assert_eq!(sg.ao_pos(), (44, 0));
        let b = bump.unwrap().camera_ofs;
        assert!((b.x.hypot(b.y) - 2.0).abs() < 1e-12 && (point_angle(b) - radians(-160.0)).abs() < 1e-12, "{b:?}");
        assert_eq!(rendered(&mut ev).iter().filter(|l| l.contains("large bump correction")).count(), 1);

        // The next step is not cut: the small bump, its weight (2 after the
        // far-outside growth) boosted to 3 while settling, then cut to the
        // 0.01 px offset.
        let (_, bump) = guide(&mut sg, &mut ao, &profile, Point::new(0.01, 0.0), MoveOpts::GUIDE_STEP, &env, &mut ev);
        assert!(bump.is_some());
        let lines = rendered(&mut ev);
        assert!(!lines.iter().any(|l| l.contains("large bump")), "{lines:?}");
        assert!(lines.contains(&"DEBUG: FAR outside bump range, increase bump weight 1.00 => 2.00".to_string()));
        assert!(lines.contains(&"DEBUG: boost bump step weight to 3.0 for settling".to_string()));
        assert_eq!(lines.last().unwrap(), "DEBUG: Scheduling Mount bump of (-0.010, -0.003)");
    }

    #[test]
    fn the_ao_calibration_follows_the_binning_only() {
        let mut profile = Profile::default();
        let ao = Ao::new(45);
        let mut sg = calibrated(&mut profile, &ao);
        let adj = sg.adjust_calibration_for_scope_pointing(1, "T2", &mut profile);
        assert_eq!(adj.steps, [AdjustStep::Log("stepguider: scope pointing change, no change to calibration".into())]);
        assert_eq!(adj.stored, None);

        let adj = sg.adjust_calibration_for_scope_pointing(2, "T2", &mut profile);
        assert_eq!(
            adj.steps[0],
            AdjustStep::Log("Stepguider Cal: Binning 1 -> 2, rates (0.300, 0.250) -> (0.150, 0.125)".into())
        );
        assert_eq!(adj.steps.len(), 3);
        assert_eq!((sg.mount.x_rate(), sg.mount.y_rate(), sg.mount.mount_cal().binning), (0.15, 0.125, 2));
        assert_eq!((sg.calibration.x_rate, sg.calibration.binning), (0.15, 2));
        assert_eq!(profile.f["/stepguider/calibration/xRate"], 0.15);
        assert_eq!(profile.s["/stepguider/calibration/timestamp"], "T2");
    }

    #[test]
    fn settings_summary_appends_the_bump_line_to_the_mount_block() {
        let mut profile = Profile::default();
        let mut ao = Ao::new(45);
        profile.set_f64(KEY_BUMP_MAX_STEPS_PER_CYCLE, 0.125);
        let mut sg = connected(&mut profile, &ao);
        sg.mount.set_name("AO-Simulator");
        let mut ev = Vec::new();
        assert_eq!(
            sg.settings_summary(None, &mut profile, None, &mut ev),
            "AO = AO-Simulator, connected, guiding enabled, not calibrated\n\
             X guide algorithm = Hysteresis, Hysteresis = 0.100, Aggression = 0.700, Minimum move = 0.200\n\
             Y guide algorithm = Hysteresis, Hysteresis = 0.100, Aggression = 0.700, Minimum move = 0.200\n\
             Bump percentage = 80, Bump step = 0.12, Timestamp = Unknown\n"
        );

        // A calibration of its own: the details' time stamp, and stored
        // guide speeds are checked against the shared flag.
        let sky = Sky { mx: 0.0, my: 0.0, noise: 0.0 };
        assert!(sg.begin_calibration(Some(sky.star(0, &ao)), &mut ev));
        let env = CalibrationEnv { timestamp: "2026-11-11 21:00:00".into(), ..cal_env() };
        while !sg.is_calibrated() {
            let AoCalibrationNext::Moves(moves) =
                sg.update_calibration_state(sky.star(0, &ao), &env, &mut ao, &mut profile, &mut ev)
            else {
                panic!("calibration failed")
            };
            for (dir, n) in moves {
                sg.move_axis(dir, n, MoveOpts::CALIBRATION_MOVE, &mut ao, &mut ev);
            }
        }
        profile.set_f64("/stepguider/calibration/ra_guide_rate", 0.5);
        profile.set_f64("/stepguider/calibration/dec_guide_rate", 0.5);
        ev.clear();
        let mut validated = false;
        let s = sg.settings_summary(Some("EQMod Mount"), &mut profile, Some(&mut validated), &mut ev);
        assert!(
            s.starts_with("AO = AO-Simulator, connected, guiding enabled, AuxMount=EQMod Mount, xAngle = 20.0, "),
            "{s}"
        );
        assert!(s.ends_with("Bump percentage = 80, Bump step = 0.12, Timestamp = 2026-11-11 21:00:00\n"), "{s}");
        // Checked once, by the AO's own load, then dropped to -1.
        assert!(validated);
        assert_eq!(
            rendered(&mut ev),
            [
                "DEBUG: Invalid mount guide speeds: RA: 0.5000, Dec: 0.5000",
                "DEBUG: Bogus guide speeds cleared in LoadCalibrationDetails",
            ]
        );
        assert_eq!(profile.f["/stepguider/calibration/ra_guide_rate"], -1.0);
    }
}
