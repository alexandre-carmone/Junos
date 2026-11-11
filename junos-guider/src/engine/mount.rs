//! The mount: what PHD2's `Mount` base class does for the scope and the AO
//! alike — the two guide algorithms (RA on X, Dec on Y), the guiding-enabled
//! switch, the lifecycle notifications fanned out to both algorithms, the
//! calibration held in memory with its camera ↔ mount transforms, and
//! `MoveOffset`, which turns a star offset into one guide move per axis.
//!
//! The mount type's own half of a move (PHD2's virtual `MoveAxis`, and the
//! scope's backlash compensation) comes in through [`AxisMover`]; the
//! devices through [`GuideOutput`] and [`PointingSource`], which the INDI
//! client and the simulator implement. Nothing here does I/O: the profile is
//! reached through [`ProfileAccess`], and whatever PHD2 tells the outside
//! (events, guide-log lines, alerts, debug-log lines) comes back as
//! [`MountEvent`]s, in PHD2's order.
//!
//! Ported from PHD2 src/mount.h, src/mount.cpp — BSD-3-Clause, see THIRD_PARTY.md

use serde_json::Value;

use super::algo::{self, AlgoCtx, AlgoKind, Axis, GuideAlgorithm, MountClass};
use super::calibration::{
    self, Calibration, CalibrationDetails, MountCalibration, ProfileAccess, UNKNOWN_DECLINATION, degrees, norm_angle,
    parity_str, point_angle, sidereal_rate_from_guide_speed,
};
use super::log::cfmt::fmt_f;
use super::traits::{CancelToken, Dir, MoveOpts, MoveResult, Parity, PierSide, Point, phd_round};

/// `DumpMoveOptionBits` (`mount.cpp:1505`): one letter per set bit
/// (`A`, `D`, `B`, `G`, `M`), `-` for none.
pub fn dump_move_option_bits(opts: MoveOpts) -> String {
    if opts == MoveOpts::CALIBRATION_MOVE {
        return "-".into();
    }
    [
        (MoveOpts::ALGO_RESULT, 'A'),
        (MoveOpts::ALGO_DEDUCE, 'D'),
        (MoveOpts::USE_BLC, 'B'),
        (MoveOpts::GRAPH, 'G'),
        (MoveOpts::MANUAL, 'M'),
    ]
    .iter()
    .filter(|(bit, _)| opts.has(*bit))
    .map(|(_, c)| *c)
    .collect()
}

/// Where guide pulses go: PHD2's `Scope::Guide` (the mount's pulse-guide
/// command, or an ST-4 port). The INDI client and the simulator implement
/// it.
pub trait GuideOutput {
    /// One pulse of `ms` ms, blocking until it is done or `cancel` is set.
    fn guide(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> MoveResult;

    /// `Scope::CanPulseGuide`: the pulses are mount commands rather than an
    /// ST-4 cable. Only the calibration's east check and the wording of a
    /// few advisories depend on it.
    fn can_pulse_guide(&self) -> bool {
        false
    }
}

/// What the mount (or an aux mount) reports about where it points: PHD2's
/// `pPointingSource`.
pub trait PointingSource {
    /// `GetCoordinates`: RA hours, Dec degrees, local sidereal time hours.
    fn coordinates(&self) -> Option<(f64, f64, f64)>;
    /// `SideOfPier`.
    fn pier_side(&self) -> PierSide;
    /// `GetGuideRates`: (RA, Dec) in deg/s.
    fn guide_rates(&self) -> Option<(f64, f64)>;
    /// `Slewing`.
    fn is_slewing(&self) -> bool;
    /// `CanReportPosition`.
    fn can_report_position(&self) -> bool;
    /// `CanSlewAsync`; the static PA tool's auto mode needs it.
    fn can_slew_async(&self) -> bool {
        false
    }
    /// `SlewToCoordinatesAsync` to RA hours, Dec degrees: `true` when the
    /// slew started (PHD2 returns true on error). Without slewing, refused.
    fn slew_to_async(&mut self, _ra_hours: f64, _dec_deg: f64) -> bool {
        false
    }
    /// `AbortSlew`.
    fn abort_slew(&mut self) {}
    /// `GetSiteLatLong`: the site's latitude and longitude, degrees, east
    /// positive; `None` when the source does not know them.
    fn site_lat_long(&self) -> Option<(f64, f64)> {
        None
    }

    /// `GetDeclinationRadians`, as the INDI mount answers it
    /// (`scope_indi.cpp:751`): the reported Dec clamped to ±89° so nothing
    /// divides by `cos(90°)`.
    fn declination_radians(&self) -> Option<f64> {
        self.coordinates().map(|(_, dec, _)| calibration::radians(dec.clamp(-89.0, 89.0)))
    }
}

/// The `Alert` event's `Type` (`event_server.cpp:2955`), from the wx icon
/// flags of PHD2's `Alert` calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    Info,
    Question,
    Warning,
    Error,
}

impl AlertKind {
    /// The event's `Type` string.
    pub fn type_str(self) -> &'static str {
        match self {
            AlertKind::Info => "info",
            AlertKind::Question => "question",
            AlertKind::Warning => "warning",
            AlertKind::Error => "error",
        }
    }
}

/// One calibration step (`CalibrationStepInfo`, `guidinglog.h:45`): the
/// guide-log row `Direction,Step,dx,dy,x,y,Dist` and the `Calibrating`
/// event's fields. `dx`/`dy` are the leg's start minus the current position.
#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationStepInfo {
    /// `West`, `East`, `Backlash`, `North`, `South` or `NudgeSouth`; the
    /// AO's are `Left` and `Up` too (`stepguider.cpp:643, 695`).
    pub direction: &'static str,
    pub step: i32,
    pub dx: f64,
    pub dy: f64,
    pub pos: Point,
    pub dist: f64,
    /// The status text: the `Calibrating` event's `State`, also shown on the
    /// status bar. Empty in a guide-log row.
    pub msg: String,
}

/// What a move did (`GuideStepInfo`, `guidinglog.h:63`): the guide-log row
/// and the `GuideStep` event.
#[derive(Clone, Debug, PartialEq)]
pub struct GuideStepInfo {
    pub move_options: MoveOpts,
    pub frame_number: i32,
    /// Seconds since guiding started.
    pub time: f64,
    /// The move was an AO's (`"AO"` in the row, a `Pos` in the event).
    pub is_ao: bool,
    pub camera_offset: Point,
    pub mount_offset: Point,
    /// The algorithms' output, px.
    pub guide_distance_ra: f64,
    pub guide_distance_dec: f64,
    /// ms on a scope, steps on an AO.
    pub duration_ra: i32,
    pub direction_ra: Dir,
    pub duration_dec: i32,
    pub direction_dec: Dir,
    pub ra_limited: bool,
    pub dec_limited: bool,
    /// `GetAoPos`: (0, 0) on a scope.
    pub ao_pos: (i32, i32),
    pub star_mass: f64,
    pub star_snr: f64,
    pub star_hfd: f64,
    /// `CurrentGuideError`.
    pub avg_dist: f64,
    pub star_error: i32,
}

/// What `MoveOffset` copies from the frame and the guider into the
/// [`GuideStepInfo`] (`pFrame->m_frameCounter`, `TimeSinceGuidingStarted`,
/// the primary star, `CurrentGuideError`). PHD2 reads them after both
/// pulses (`mount.cpp:1059-1077`), and a pulse blocks until it ends, so the
/// time and the error (which turns to 100 px 20 s after the star was last
/// found) include the pulses: [`Mount::move_offset`] asks for them then.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StepFrame {
    pub frame_number: i32,
    /// Seconds since guiding started, as the clock reads them; the step
    /// keeps them cut to whole ms ([`stopwatch_seconds`]).
    pub time: f64,
    pub star_mass: f64,
    pub star_snr: f64,
    pub star_hfd: f64,
    pub star_error: i32,
    pub avg_dist: f64,
}

/// `MyFrame::TimeSinceGuidingStarted` (`myframe.h:736`) from the elapsed
/// seconds: `wxStopWatch::Time()` counts whole ms, so the time is cut, not
/// rounded, to the ms below (12.3456 s reads 12.345).
pub fn stopwatch_seconds(elapsed: f64) -> f64 {
    (elapsed * 1000.0).trunc() / 1000.0
}

/// PHD2's `GuiderOffset` (`guider.h:131`): the offset to correct, camera
/// px, and its mount-axis form when the guider already has it. A deduced
/// move passes a zero camera offset.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GuiderOffset {
    pub camera_ofs: Point,
    pub mount_ofs: Option<Point>,
}

/// `MoveResultInfo` (`mount.h:145`): what one axis move did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MoveResultInfo {
    /// ms (scope) or steps (AO) actually moved.
    pub amount_moved: i32,
    /// The move was cut to the axis' maximum.
    pub limited: bool,
}

/// The guide outputs a tool turns off while it measures and restores at
/// Stop (`m_savePrimaryMountEnabled`, `m_saveSecondaryMountEnabled` of the
/// Guiding Assistant and the polar drift tool): pMount's and
/// pSecondaryMount's `GetGuidingEnabled`, `None` when there is no such mount.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuideOutputs {
    pub primary: Option<bool>,
    pub secondary: Option<bool>,
}

/// What the mount tells the outside, in PHD2's order. The engine maps these
/// onto the PHD2 events, the guide log, the alerts and the debug log.
#[derive(Clone, Debug, PartialEq)]
pub enum MountEvent {
    /// A guide move was made: the `GuideStep` event and the guide-log row.
    GuideStep(GuideStepInfo),
    /// The `Calibrating` event, sent when a calibration move is scheduled
    /// (`CalibrationStatus`).
    Calibrating(CalibrationStepInfo),
    /// A guide-log calibration row (`GuidingLog::CalibrationStep`).
    CalibrationRow(CalibrationStepInfo),
    /// `<West|North> calibration complete. Angle = …, Rate = …, Parity = …`
    /// in the guide log. Angle in radians, rate in px/ms.
    CalibrationAxisComplete { direction: &'static str, angle: f64, rate: f64, parity: Parity },
    /// `CalibrationComplete`: the event and the guide-log line.
    CalibrationComplete,
    /// `CalibrationFailed`: the event and the guide-log line.
    CalibrationFailed { reason: String },
    /// `CalibrationDataFlipped`.
    CalibrationDataFlipped,
    /// `NotifyGuidingParam`: the `GuideParamChange` event and the guide-log
    /// `INFO: Guiding parameter change` line. The value's JSON type is
    /// PHD2's overload (int, double, bool or string), which sets the log's
    /// format.
    GuideParamChange { name: String, value: Value },
    /// `pFrame->Alert`. PHD2 also writes `Alert: <msg>` to the debug log.
    /// `dont_show` is set on a `SuppressibleAlert`: the global key under
    /// `/Confirm/<profile id>/` that its "Don't show again" button sets to
    /// false, for the junos UI (the event server's `Alert` carries `msg`
    /// and `kind` only).
    Alert { msg: String, kind: AlertKind, dont_show: Option<&'static str> },
    /// `pFrame->StatusMsg`.
    Status(String),
    /// A guide-log `INFO:` line.
    Info(String),
    /// A debug-log line, without its newline.
    Debug(String),
    /// A debug-log record PHD2 writes without a newline: the next record
    /// runs on after it on the same line.
    DebugNoNewline(String),
}

/// The mount type's half of [`Mount::move_offset`]: PHD2's virtual
/// `MoveAxis`, and the backlash compensation only a scope has.
pub trait AxisMover {
    /// `MoveAxis(direction, amount, moveOptions, &info)`: ms on a scope,
    /// steps on an AO. `guiding_enabled` is the mount's switch.
    fn move_axis(
        &mut self,
        guiding_enabled: bool,
        dir: Dir,
        amount: i32,
        opts: MoveOpts,
        events: &mut Vec<MountEvent>,
    ) -> (MoveResult, MoveResultInfo);

    /// `BacklashComp::TrackBLCResults` with the raw Dec offset (px), the Dec
    /// algorithm's min-move and the calibration's Dec rate (px/ms).
    fn track_blc(&mut self, _opts: MoveOpts, _y_raw: f64, _min_move: f64, _y_rate: f64, _events: &mut Vec<MountEvent>) {}

    /// `BacklashComp::ApplyBacklashComp`: may lengthen the Dec pulse.
    fn apply_blc(&mut self, _opts: MoveOpts, _y_distance: f64, _y_ms: &mut i32, _events: &mut Vec<MountEvent>) {}

    /// `GetAoPos` after the moves.
    fn ao_pos(&self) -> (i32, i32) {
        (0, 0)
    }
}

/// `Scope::ValidGuideRates` (`scope.cpp:1859`): mount guide speeds (deg/s)
/// between 0.1 and 1.2 × sidereal. Quirk kept: the Dec speed is let through
/// when it *converts* to −1, meant for RA-only devices reporting −1 raw.
/// Sets the session's `validated` flag (PHD2's `m_CalDetailsValidated`).
pub fn valid_guide_rates(ra: f64, dec: f64, validated: &mut bool, events: &mut Vec<MountEvent>) -> bool {
    let mut err = false;
    let spd = sidereal_rate_from_guide_speed(ra);
    if spd > 0.1 && spd < 1.2 {
        let spd = sidereal_rate_from_guide_speed(dec);
        if spd != -1.0 && (spd < 0.1 || spd > 1.2) {
            err = true;
        }
    } else {
        err = true;
    }
    *validated = true;
    if err {
        events.push(MountEvent::Debug(format!(
            "Invalid mount guide speeds: RA: {}, Dec: {}",
            fmt_f(ra, 4),
            fmt_f(dec, 4)
        )));
    }
    !err
}

/// The profile key of an axis' algorithm, `/<class>/XGuideAlgorithm`.
fn algorithm_key(class: MountClass, axis: Axis) -> String {
    format!("/{}/{}GuideAlgorithm", class.class_name(), axis.letter())
}

/// `CreateGuideAlgorithm` plus the setters' profile writes: build what the
/// stored integer asks for (the axis default for −1 or an unknown value,
/// which PHD2's factory refuses with a debug line), persist its effective
/// parameters, then the kind. `guiding_enabled` is the mount's switch,
/// which Predictive PEC reads when it is built.
fn build_algorithm(
    class: MountClass,
    axis: Axis,
    stored: i32,
    profile: &mut impl ProfileAccess,
    now: f64,
    guiding_enabled: bool,
    events: &mut Vec<MountEvent>,
) -> Box<dyn GuideAlgorithm> {
    if AlgoKind::from_i32(stored).is_none() {
        events.push(MountEvent::Debug("Error thrown from mount.cpp:742->invalid guideAlgorithm".into()));
    }
    let (algorithm, _) = {
        let p = &*profile;
        // PHD2's getters fall back to the algorithm's default when the key
        // is missing or does not parse: NaN as the default catches both.
        let lookup = |k: &str| Some(p.get_f64(k, f64::NAN)).filter(|v| !v.is_nan());
        algo::make_or_default(stored, axis, class, &lookup, now, guiding_enabled)
    };
    persist_algorithm(class, axis, &*algorithm, profile);
    algorithm
}

/// The setters' profile writes: an algorithm's effective parameters under
/// its group, then its kind (`SetXGuideAlgorithm`).
fn persist_algorithm(class: MountClass, axis: Axis, algorithm: &dyn GuideAlgorithm, profile: &mut impl ProfileAccess) {
    let kind = algorithm.kind();
    let group = algo::config_path(class, axis, kind);
    for (key, value) in algorithm.params() {
        profile.set_f64(&format!("{group}/{key}"), value);
    }
    profile.set_i32(&algorithm_key(class, axis), kind as i32);
}

/// PHD2's `Mount`: what the scope and the AO share.
pub struct Mount {
    class: MountClass,
    /// `m_Name`.
    name: String,
    connected: bool,
    error_count: i32,
    guiding_enabled: bool,
    x_algo: Box<dyn GuideAlgorithm>,
    y_algo: Box<dyn GuideAlgorithm>,
    cal: MountCalibration,
    /// `m_lastStep`: the last guide step recorded, kept after its event.
    last_step: Option<GuideStepInfo>,
}

impl Mount {
    /// A disconnected, uncalibrated mount with guiding enabled
    /// (`Mount::Mount`), and the algorithms the profile names for its axes
    /// (`/<class>/XGuideAlgorithm`, default Hysteresis on a scope's RA and
    /// Resist Switch on its Dec, Hysteresis on an AO). The algorithms'
    /// effective settings and kinds are written back, as PHD2's setters do.
    /// `now` is the engine clock, where Predictive PEC starts its time.
    /// PHD2 builds them in the scope's (AO's) constructor after its own
    /// settings, so a scope moves `events` there.
    pub fn new(
        class: MountClass,
        profile: &mut impl ProfileAccess,
        now: f64,
        events: &mut Vec<MountEvent>,
    ) -> Mount {
        let stored_x = profile.get_i32(&algorithm_key(class, Axis::X), algo::default_kind(Axis::X, class) as i32);
        let x_algo = build_algorithm(class, Axis::X, stored_x, profile, now, true, events);
        let stored_y = profile.get_i32(&algorithm_key(class, Axis::Y), algo::default_kind(Axis::Y, class) as i32);
        let y_algo = build_algorithm(class, Axis::Y, stored_y, profile, now, true, events);
        Mount {
            class,
            name: String::new(),
            connected: false,
            error_count: 0,
            guiding_enabled: true,
            x_algo,
            y_algo,
            cal: MountCalibration::default(),
            last_step: None,
        }
    }

    /// `m_lastStep`: the last move [`move_offset`](Self::move_offset)
    /// recorded, `None` before the first. The AO's `MoveOffset` reads its
    /// `ra_limited` / `dec_limited` right after the mount's to choose the
    /// large bump (`stepguider.cpp:1172`).
    pub fn last_step(&self) -> Option<&GuideStepInfo> {
        self.last_step.as_ref()
    }

    /// `Name`: what the guide log and the event server call the device
    /// (`INDI Mount [<dev>]`); empty until the device side sets it.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: &str) {
        self.name = name.to_string();
    }

    /// `GetMountClassName`.
    pub fn class(&self) -> MountClass {
        self.class
    }

    /// `IsStepGuider`.
    pub fn is_step_guider(&self) -> bool {
        self.class == MountClass::StepGuider
    }

    /// The algorithm guiding an axis.
    pub fn algo(&self, axis: Axis) -> &dyn GuideAlgorithm {
        match axis {
            Axis::X => &*self.x_algo,
            Axis::Y => &*self.y_algo,
        }
    }

    pub fn algo_mut(&mut self, axis: Axis) -> &mut dyn GuideAlgorithm {
        match axis {
            Axis::X => &mut *self.x_algo,
            Axis::Y => &mut *self.y_algo,
        }
    }

    /// `GetXGuideAlgorithmSelection` / `GetYGuideAlgorithmSelection`.
    pub fn guide_algorithm_selection(&self, axis: Axis) -> AlgoKind {
        self.algo(axis).kind()
    }

    /// `SetXGuideAlgorithm` / `SetYGuideAlgorithm` (`mount.cpp:598-641`) on
    /// the integer the profile stores. Asking for the running kind changes
    /// nothing (the history stays, nothing is written); anything else
    /// builds the algorithm — the axis default for −1 or an unknown value
    /// (with PHD2's `invalid guideAlgorithm` debug line), so −1 rebuilds
    /// even a running default — and persists it.
    pub fn set_guide_algorithm(
        &mut self,
        axis: Axis,
        algorithm: i32,
        profile: &mut impl ProfileAccess,
        now: f64,
        events: &mut Vec<MountEvent>,
    ) {
        let class = self.class;
        let enabled = self.guiding_enabled;
        let slot = self.slot(axis);
        if slot.kind() as i32 != algorithm {
            *slot = build_algorithm(class, axis, algorithm, profile, now, enabled, events);
        }
    }

    /// The Brain's Reset button (`ResetRAGuidingParams` /
    /// `ResetDecGuidingParams`, `mount.cpp:350-398`): the axis' algorithm
    /// back to its defaults, a new one of the same kind. PHD2 deletes the
    /// algorithm's profile group and gives the old algorithm the min-move
    /// `smart_min_move` ([`algo::smart_default_min_move_current`]) when it
    /// has one (`ResetParams`), then `SetXGuideAlgorithm` builds the kind
    /// again from what is left: the defaults and that min-move. Here the new
    /// one is built on the defaults and handed the min-move, then its
    /// settings and kind are written over the old ones: [`ProfileAccess`]
    /// has no group delete, so stray keys stay, but every key an algorithm
    /// reads first is written again. The history is gone. The
    /// scope's Reset also resets the Max RA / Dec duration, Dec mode and
    /// backlash controls, which apply only when the dialog is saved.
    pub fn reset_guide_algorithm_params(
        &mut self,
        axis: Axis,
        smart_min_move: f64,
        profile: &mut impl ProfileAccess,
        now: f64,
    ) {
        let class = self.class;
        let enabled = self.guiding_enabled;
        let slot = self.slot(axis);
        let mut algorithm = algo::make(slot.kind(), axis, class, &|_| None, now, enabled);
        if algorithm.min_move() >= 0.0 {
            algorithm.set_min_move(smart_min_move);
        }
        persist_algorithm(class, axis, &*algorithm, profile);
        *slot = algorithm;
    }

    /// Write an axis' algorithm settings and kind to the profile, what
    /// PHD2's algorithm setters do on each change: the caller runs it after
    /// changing a parameter through [`algo_mut`](Self::algo_mut).
    pub fn save_algorithm(&self, axis: Axis, profile: &mut impl ProfileAccess) {
        persist_algorithm(self.class, axis, self.algo(axis), profile);
    }

    fn slot(&mut self, axis: Axis) -> &mut Box<dyn GuideAlgorithm> {
        match axis {
            Axis::X => &mut self.x_algo,
            Axis::Y => &mut self.y_algo,
        }
    }

    /// `GetGuidingEnabled`.
    pub fn guiding_enabled(&self) -> bool {
        self.guiding_enabled
    }

    /// `SetGuidingEnabled` (`mount.cpp:663`): on a change, the
    /// `MountGuidingEnabled` (`AOGuidingEnabled`) parameter change and the
    /// algorithms' enabled / disabled hook. Returns whether it changed: when
    /// that turned guiding back on, a scope defers its pulse-limit alerts
    /// (`Scope::set_guiding_enabled` does both).
    pub fn set_guiding_enabled(&mut self, enabled: bool, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) -> bool {
        if enabled == self.guiding_enabled {
            return false;
        }
        let name = if self.is_step_guider() { "AOGuidingEnabled" } else { "MountGuidingEnabled" };
        events.push(MountEvent::Debug(format!("{name}: {}", enabled as i32)));
        events.push(MountEvent::GuideParamChange { name: name.into(), value: enabled.into() });
        self.guiding_enabled = enabled;
        if enabled {
            self.x_algo.guiding_enabled(ctx);
            self.y_algo.guiding_enabled(ctx);
        } else {
            self.x_algo.guiding_disabled(ctx);
            self.y_algo.guiding_disabled(ctx);
        }
        true
    }

    /// `IsConnected`.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// `Mount::Connect`: the device side is the caller's.
    pub fn connect(&mut self) {
        self.connected = true;
        self.error_count = 0;
    }

    /// `Mount::Disconnect`.
    pub fn disconnect(&mut self) {
        self.connected = false;
    }

    /// `ErrorCount`: failed moves since the last reset (the AO's calibration
    /// gives up past a limit).
    pub fn error_count(&self) -> i32 {
        self.error_count
    }

    /// `IncrementErrorCount`, after a move that did not return OK.
    pub fn increment_error_count(&mut self) {
        self.error_count += 1;
    }

    /// `ResetErrorCount`, when a calibration begins.
    pub fn reset_error_count(&mut self) {
        self.error_count = 0;
    }

    /// The calibration in memory (`m_cal`, `m_calibrated`, `m_xRate`).
    pub fn calibration(&self) -> &MountCalibration {
        &self.cal
    }

    pub(super) fn calibration_mut(&mut self) -> &mut MountCalibration {
        &mut self.cal
    }

    /// `MountCal`.
    pub fn mount_cal(&self) -> &Calibration {
        &self.cal.cal
    }

    /// `MountIsCalibrated`: calibrated, connected or not.
    pub fn is_mount_calibrated(&self) -> bool {
        self.cal.calibrated
    }

    /// `Mount::IsCalibrated`: connected and calibrated. A scope also wants a
    /// Dec rate (`Scope::is_calibrated`).
    pub fn is_calibrated(&self) -> bool {
        self.cal.is_calibrated(self.connected)
    }

    /// `xRate`: the RA rate guiding divides by, px/ms (Dec-compensated).
    pub fn x_rate(&self) -> f64 {
        self.cal.x_rate
    }

    /// `yRate`, px/ms.
    pub fn y_rate(&self) -> f64 {
        self.cal.cal.y_rate
    }

    /// `xAngle`, radians.
    pub fn x_angle(&self) -> f64 {
        self.cal.cal.x_angle
    }

    /// `yAngle`, radians in `[-π, π)`.
    pub fn y_angle(&self) -> f64 {
        self.cal.cal.y_angle()
    }

    /// `RAParity`: unknown unless calibrated.
    pub fn ra_parity(&self) -> Parity {
        self.cal.ra_parity()
    }

    /// `DecParity`: unknown unless calibrated.
    pub fn dec_parity(&self) -> Parity {
        self.cal.dec_parity()
    }

    /// `GetCalibrationDeclination`: radians, or `UNKNOWN_DECLINATION` unless
    /// calibrated.
    pub fn calibration_declination(&self) -> f64 {
        self.cal.declination()
    }

    /// `Mount::SetCalibration` (`mount.cpp:1544`): in memory, then stored
    /// with `timestamp` (PHD2's `wxDateTime::Now().Format()`).
    pub fn set_calibration(
        &mut self,
        cal: &Calibration,
        timestamp: &str,
        profile: &mut impl ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        events.extend(self.cal.set(cal, self.class).into_iter().map(MountEvent::Debug));
        calibration::store_calibration(profile, self.class, &self.cal.cal, timestamp);
    }

    /// `Mount::ClearCalibration`: no longer calibrated; the calibration
    /// itself stays. A scope also resets its state machine.
    pub fn clear_calibration(&mut self) {
        self.cal.clear();
    }

    /// `GetLastCalibration`: the stored calibration.
    pub fn get_last_calibration(&self, profile: &impl ProfileAccess) -> Calibration {
        calibration::get_last_calibration(profile, self.class)
    }

    /// `Mount::GetSettingsSummary` (`mount.cpp:1878`): the mount's block of
    /// the guide-log header, each line ending in `\n`. `calibrated` is the
    /// mount type's `IsCalibrated` (a virtual call in PHD2:
    /// `Scope::is_calibrated` on a scope); `aux_mount` the pointing
    /// source's name when it is an aux mount, connected and reporting its
    /// position; `blc` the backlash compensation's (enabled, pulse ms),
    /// which only a scope has. A calibrated mount reads its stored details
    /// here, with their guide-speed check (`validated`, see
    /// [`load_calibration_details`](Self::load_calibration_details)). The
    /// parities are the calibration's own (not [`ra_parity`](Self::ra_parity));
    /// `xRate` is the Dec-compensated rate, the "Norm rates" the stored
    /// ones. The scope and the AO append their own lines.
    pub fn settings_summary(
        &self,
        calibrated: bool,
        aux_mount: Option<&str>,
        blc: Option<(bool, i32)>,
        profile: &mut impl ProfileAccess,
        validated: Option<&mut bool>,
        events: &mut Vec<MountEvent>,
    ) -> String {
        let mut s = format!(
            "{} = {},{} connected, guiding {}, ",
            if self.is_step_guider() { "AO" } else { "Mount" },
            self.name,
            if self.connected { "" } else { " not" },
            if self.guiding_enabled { "enabled" } else { "disabled" }
        );
        if let Some(aux) = aux_mount {
            s += &format!("AuxMount={aux}, ");
        }
        if calibrated {
            let cal = &self.cal.cal;
            s += &format!(
                "xAngle = {}, xRate = {}, yAngle = {}, yRate = {}, parity = {}/{}\n",
                fmt_f(degrees(self.x_angle()), 1),
                fmt_f(self.x_rate() * 1000.0, 3),
                fmt_f(degrees(self.y_angle()), 1),
                fmt_f(self.y_rate() * 1000.0, 3),
                parity_str(cal.ra_guide_parity),
                parity_str(cal.dec_guide_parity)
            );
            let det = self.load_calibration_details(profile, validated, events);
            let scale = if det.is_valid() { det.image_scale } else { 1.0 };
            let ortho = if det.is_valid() { format!("{} deg", fmt_f(det.ortho_error, 1)) } else { "unknown".into() };
            if self.is_step_guider() {
                s += &format!(
                    "Norm rates X = {} mas/step, Y = {} mas/step; ortho.err. = {ortho}\n",
                    fmt_f(cal.x_rate * 1000.0 * scale, 0),
                    fmt_f(cal.y_rate * 1000.0 * scale, 0)
                );
            } else {
                let x_rate_d0 = if cal.declination != UNKNOWN_DECLINATION {
                    cal.x_rate * 1000.0 / cal.declination.cos() * scale
                } else {
                    cal.x_rate * 1000.0 * scale
                };
                s += &format!(
                    "Norm rates RA = {}\"/s @ dec 0, Dec = {}\"/s; ortho.err. = {ortho}\n",
                    fmt_f(x_rate_d0, 1),
                    fmt_f(self.y_rate() * 1000.0 * scale, 1)
                );
            }
        } else {
            s += "not calibrated\n";
        }
        for (axis, a) in [("X", &self.x_algo), ("Y", &self.y_algo)] {
            s += &format!("{axis} guide algorithm = {}, {}", a.kind().ui_name(), a.settings_summary());
        }
        if let Some((enabled, pulse)) = blc {
            s += &format!("Backlash comp = {}, pulse = {pulse} ms\n", if enabled { "enabled" } else { "disabled" });
        }
        s
    }

    /// `LoadCalibrationDetails` (`mount.cpp:1781`). With a pointing source
    /// (`validated` is its once-per-session flag), stored guide speeds it
    /// finds bogus are dropped to −1 here and in the profile.
    pub fn load_calibration_details(
        &self,
        profile: &mut impl ProfileAccess,
        validated: Option<&mut bool>,
        events: &mut Vec<MountEvent>,
    ) -> CalibrationDetails {
        let mut d = calibration::load_calibration_details(&*profile, self.class);
        if let Some(validated) = validated
            && !*validated
            && !(d.ra_guide_speed == -1.0 || d.dec_guide_speed == -1.0)
            && !valid_guide_rates(d.ra_guide_speed, d.dec_guide_speed, validated, events)
        {
            d.ra_guide_speed = -1.0;
            d.dec_guide_speed = -1.0;
            let prefix = calibration::keys::prefix(self.class.class_name());
            profile.set_f64(&format!("{prefix}{}", calibration::keys::RA_GUIDE_RATE), -1.0);
            profile.set_f64(&format!("{prefix}{}", calibration::keys::DEC_GUIDE_RATE), -1.0);
            events.push(MountEvent::Debug("Bogus guide speeds cleared in LoadCalibrationDetails".into()));
        }
        d
    }

    /// `SaveCalibrationDetails` (`mount.cpp:1595`). With a pointing source,
    /// guide speeds it finds bogus are stored as −1.
    pub fn save_calibration_details(
        &self,
        details: &CalibrationDetails,
        profile: &mut impl ProfileAccess,
        validated: Option<&mut bool>,
        events: &mut Vec<MountEvent>,
    ) {
        let bogus = match validated {
            Some(validated) if !(details.ra_guide_speed == -1.0 && details.dec_guide_speed == -1.0) => {
                !valid_guide_rates(details.ra_guide_speed, details.dec_guide_speed, validated, events)
            }
            _ => false,
        };
        if bogus {
            let d = CalibrationDetails { ra_guide_speed: -1.0, dec_guide_speed: -1.0, ..details.clone() };
            calibration::save_calibration_details(profile, self.class, &d);
            events.push(MountEvent::Debug("Bogus guide speeds over-written in SaveCalibrationDetails".into()));
        } else {
            calibration::save_calibration_details(profile, self.class, details);
        }
    }

    /// `TransformCameraCoordinatesToMountCoordinates` (`mount.cpp:1135`),
    /// with its three debug lines when `logged`.
    pub fn camera_to_mount(&self, camera: Point, logged: bool, events: &mut Vec<MountEvent>) -> Point {
        if !logged {
            return calibration::camera_to_mount(camera, &self.cal.cal);
        }
        let mut log = Vec::new();
        let mount = calibration::camera_to_mount_logged(Some(camera), &self.cal.cal, &mut log);
        events.extend(log.into_iter().map(MountEvent::Debug));
        mount.unwrap_or_default()
    }

    /// `TransformMountCoordinatesToCameraCoordinates` (`mount.cpp:1181`),
    /// with its two debug lines when `logged`.
    pub fn mount_to_camera(&self, mount: Point, logged: bool, events: &mut Vec<MountEvent>) -> Point {
        let camera = calibration::mount_to_camera(mount, &self.cal.cal);
        if logged {
            let cal = &self.cal.cal;
            let hyp = mount.x.hypot(mount.y);
            let mut theta = point_angle(mount);
            if cal.y_angle_error().abs() > std::f64::consts::PI / 2.0 {
                theta = -theta;
            }
            let x_angle = theta + cal.x_angle;
            events.push(MountEvent::Debug(format!(
                "MountToCamera -- mountTheta ({}) + m_xAngle ({}) = xAngle ({} = {})",
                fmt_f(theta, 2),
                fmt_f(cal.x_angle, 2),
                fmt_f(x_angle, 2),
                fmt_f(norm_angle(x_angle), 2)
            )));
            events.push(MountEvent::Debug(format!(
                "MountToCamera -- mountX={} mountY={} hyp={} mountTheta={} cameraX={}, cameraY={} cameraTheta={}",
                fmt_f(mount.x, 2),
                fmt_f(mount.y, 2),
                fmt_f(hyp, 2),
                fmt_f(theta, 2),
                fmt_f(camera.x, 2),
                fmt_f(camera.y, 2),
                fmt_f(point_angle(camera), 2)
            )));
        }
        camera
    }

    /// `Mount::MoveOffset` (`mount.cpp:978`): one guide move from an offset.
    ///
    /// A deduced move asks the algorithms (nothing at all when both say 0);
    /// otherwise the camera offset is turned into mount axes (unless
    /// `ofs.mount_ofs` already holds them), the backlash compensation sees the
    /// raw Dec offset, and with `MoveOpts::ALGO_RESULT` each axis goes through
    /// its algorithm. A positive RA amount moves West, a positive Dec amount
    /// South; the pulse is `round(|px| / rate)` with the Dec-compensated RA
    /// rate, the Dec one possibly lengthened by the backlash compensation.
    /// Dec is skipped when RA reports a slew or an AO limit; otherwise Dec's
    /// result is the one returned, so an RA error followed by a good Dec move
    /// reads OK (PHD2's quirk). `frame` is called once, after the pulses,
    /// for what the step copies from the guider; the step is kept as
    /// [`last_step`](Self::last_step) and pushed last as a
    /// [`MountEvent::GuideStep`]. PHD2 never sends `MoveOpts::MANUAL` here:
    /// manual moves are scheduled axis moves, which record no step.
    pub fn move_offset(
        &mut self,
        ofs: &mut GuiderOffset,
        opts: MoveOpts,
        frame: impl FnOnce() -> StepFrame,
        ctx: &AlgoCtx,
        axes: &mut dyn AxisMover,
        events: &mut Vec<MountEvent>,
    ) -> MoveResult {
        let (x_distance, y_distance);
        if opts.has(MoveOpts::ALGO_DEDUCE) {
            let x = self.x_algo.deduce_result(ctx);
            let y = self.y_algo.deduce_result(ctx);
            if x == 0.0 && y == 0.0 {
                return MoveResult::Ok;
            }
            ofs.mount_ofs = Some(Point::new(x, y));
            events.push(MountEvent::Debug(format!(
                "Dead-reckoning move xDistance={} yDistance={}",
                fmt_f(x, 2),
                fmt_f(y, 2)
            )));
            (x_distance, y_distance) = (x, y);
        } else {
            let mount = match ofs.mount_ofs {
                Some(m) => m,
                None => {
                    let m = self.camera_to_mount(ofs.camera_ofs, true, events);
                    ofs.mount_ofs = Some(m);
                    m
                }
            };
            events.push(MountEvent::Debug(format!(
                "Moving ({}, {}) raw xDistance={} yDistance={}",
                fmt_f(ofs.camera_ofs.x, 2),
                fmt_f(ofs.camera_ofs.y, 2),
                fmt_f(mount.x, 2),
                fmt_f(mount.y, 2)
            )));
            // The compensation tracks the raw Dec offset, before the algorithm.
            axes.track_blc(opts, mount.y, self.y_algo.min_move(), self.cal.cal.y_rate, events);
            let (mut x, mut y) = (mount.x, mount.y);
            if opts.has(MoveOpts::ALGO_RESULT) {
                x = self.x_algo.result(x, ctx);
                y = self.y_algo.result(y, ctx);
            }
            (x_distance, y_distance) = (x, y);
        }

        let x_dir = if x_distance > 0.0 { Dir::LEFT } else { Dir::RIGHT };
        let y_dir = if y_distance > 0.0 { Dir::DOWN } else { Dir::UP };

        let x_amount = phd_round((x_distance / self.cal.x_rate).abs());
        let (mut result, x_info) = axes.move_axis(self.guiding_enabled, x_dir, x_amount, opts, events);

        let mut y_info = MoveResultInfo::default();
        if result != MoveResult::Slewing && result != MoveResult::AoLimit {
            let mut y_amount = phd_round((y_distance / self.cal.cal.y_rate).abs());
            axes.apply_blc(opts, y_distance, &mut y_amount, events);
            (result, y_info) = axes.move_axis(self.guiding_enabled, y_dir, y_amount, opts, events);
        }

        // Read after the pulses, as PHD2 does.
        let frame = frame();
        let step = GuideStepInfo {
            move_options: opts,
            frame_number: frame.frame_number,
            time: stopwatch_seconds(frame.time),
            is_ao: self.is_step_guider(),
            camera_offset: ofs.camera_ofs,
            mount_offset: ofs.mount_ofs.unwrap_or_default(),
            guide_distance_ra: x_distance,
            guide_distance_dec: y_distance,
            duration_ra: x_info.amount_moved,
            direction_ra: x_dir,
            duration_dec: y_info.amount_moved,
            direction_dec: y_dir,
            ra_limited: x_info.limited,
            dec_limited: y_info.limited,
            ao_pos: axes.ao_pos(),
            star_mass: frame.star_mass,
            star_snr: frame.star_snr,
            star_hfd: frame.star_hfd,
            avg_dist: frame.avg_dist,
            star_error: frame.star_error,
        };
        self.last_step = Some(step.clone());
        events.push(MountEvent::GuideStep(step));
        result
    }

    /// `NotifyGuidingStarted`.
    pub fn notify_guiding_started(&mut self, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        events.push(MountEvent::Debug("Mount: notify guiding started".into()));
        self.x_algo.guiding_started(ctx);
        self.y_algo.guiding_started(ctx);
    }

    /// `NotifyGuidingStopped`. PHD2 also resets the backlash compensation
    /// here, which only a scope has: `Scope::notify_guiding_stopped` does it.
    /// The AO overrides it without calling it (`stepguider.cpp:830`): no
    /// line and no algorithm reset, only its bump state cleared and a move
    /// to the centre. Its `NotifyGuidingResumed` does call the mount's.
    pub fn notify_guiding_stopped(&mut self, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        events.push(MountEvent::Debug("Mount: notify guiding stopped".into()));
        self.x_algo.guiding_stopped(ctx);
        self.y_algo.guiding_stopped(ctx);
    }

    /// `NotifyGuidingResumed`: PHD2 calls it on the primary mount only, when
    /// a pause ends (`myframe.cpp:1940`). Pausing notifies nothing:
    /// `Mount::NotifyGuidingPaused` is never called in PHD2 2.6.14, so it is
    /// not ported.
    pub fn notify_guiding_resumed(&mut self, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        events.push(MountEvent::Debug("Mount: notify guiding resumed".into()));
        self.x_algo.guiding_resumed(ctx);
        self.y_algo.guiding_resumed(ctx);
    }

    /// `NotifyGuidingDithered` (`mount.cpp:1715`): a dither of (`dx`, `dy`)
    /// px, camera axes unless `mount_coords`; each algorithm gets its own
    /// axis' share.
    pub fn notify_guiding_dithered(
        &mut self,
        dx: f64,
        dy: f64,
        mount_coords: bool,
        ctx: &AlgoCtx,
        events: &mut Vec<MountEvent>,
    ) {
        events.push(MountEvent::Debug(format!(
            "Mount: notify guiding dithered ({}, {})",
            fmt_f(dx, 1),
            fmt_f(dy, 1)
        )));
        let (dx, dy) = if mount_coords {
            (dx, dy)
        } else {
            let m = self.camera_to_mount(Point::new(dx, dy), false, events);
            (m.x, m.y)
        };
        self.x_algo.guiding_dithered(dx, ctx);
        self.y_algo.guiding_dithered(dy, ctx);
    }

    /// `NotifyGuidingDitherSettleDone`.
    pub fn notify_guiding_dither_settle_done(&mut self, success: bool, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        events.push(MountEvent::Debug(format!(
            "Mount: notify guiding dither settle done success={}",
            success as i32
        )));
        self.x_algo.dither_settle_done(success, ctx);
        self.y_algo.dither_settle_done(success, ctx);
    }

    /// `NotifyDirectMove`: the guider moved the lock by `dist` (mount px)
    /// without the algorithms.
    pub fn notify_direct_move(&mut self, dist: Point, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        events.push(MountEvent::Debug(format!(
            "Mount: notify direct move {},{}",
            fmt_f(dist.x, 2),
            fmt_f(dist.y, 2)
        )));
        self.x_algo.direct_move_applied(dist.x, ctx);
        self.y_algo.direct_move_applied(dist.y, ctx);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::engine::calibration::{ROTATOR_POSITION_UNKNOWN, UNKNOWN_DECLINATION, radians};

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

    /// An algorithm that halves its input, deduces a set amount and logs
    /// every hook.
    struct Recorder {
        log: Arc<Mutex<Vec<String>>>,
        deduce: f64,
    }

    impl Recorder {
        fn push(&self, s: String) {
            self.log.lock().unwrap().push(s);
        }
    }

    impl GuideAlgorithm for Recorder {
        fn kind(&self) -> AlgoKind {
            AlgoKind::Identity
        }
        fn reset(&mut self) {
            self.push("reset".into());
        }
        fn result(&mut self, input: f64, _ctx: &AlgoCtx) -> f64 {
            input / 2.0
        }
        fn deduce_result(&mut self, _ctx: &AlgoCtx) -> f64 {
            self.deduce
        }
        fn min_move(&self) -> f64 {
            0.15
        }
        fn guiding_started(&mut self, _ctx: &AlgoCtx) {
            self.push("started".into());
        }
        fn guiding_stopped(&mut self, _ctx: &AlgoCtx) {
            self.push("stopped".into());
        }
        fn guiding_paused(&mut self, _ctx: &AlgoCtx) {
            self.push("paused".into());
        }
        fn guiding_resumed(&mut self, _ctx: &AlgoCtx) {
            self.push("resumed".into());
        }
        fn guiding_dithered(&mut self, amt: f64, _ctx: &AlgoCtx) {
            self.push(format!("dithered {}", fmt_f(amt, 6)));
        }
        fn dither_settle_done(&mut self, success: bool, _ctx: &AlgoCtx) {
            self.push(format!("settled {success}"));
        }
        fn direct_move_applied(&mut self, amt: f64, _ctx: &AlgoCtx) {
            self.push(format!("direct {amt}"));
        }
        fn guiding_disabled(&mut self, _ctx: &AlgoCtx) {
            self.push("disabled".into());
        }
        fn guiding_enabled(&mut self, _ctx: &AlgoCtx) {
            self.push("enabled".into());
        }
    }

    /// A mount whose two algorithms are recorders sharing one log.
    fn recorded(class: MountClass) -> (Mount, Arc<Mutex<Vec<String>>>) {
        let mut m = Mount::new(class, &mut Profile::default(), 0.0, &mut Vec::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        m.x_algo = Box::new(Recorder { log: log.clone(), deduce: 0.0 });
        m.y_algo = Box::new(Recorder { log: log.clone(), deduce: 0.0 });
        (m, log)
    }

    fn take(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    /// A scope type's moves, scripted. Each pulse blocks: `clock` (seconds)
    /// moves on by its length.
    #[derive(Default)]
    struct FakeAxes {
        results: Vec<MoveResult>,
        calls: Vec<(bool, Dir, i32, u32)>,
        blc: Vec<String>,
        clock: Rc<Cell<f64>>,
    }

    impl AxisMover for FakeAxes {
        fn move_axis(
            &mut self,
            guiding_enabled: bool,
            dir: Dir,
            amount: i32,
            opts: MoveOpts,
            _events: &mut Vec<MountEvent>,
        ) -> (MoveResult, MoveResultInfo) {
            self.calls.push((guiding_enabled, dir, amount, opts.0));
            self.clock.set(self.clock.get() + amount as f64 / 1000.0);
            let r = if self.results.is_empty() { MoveResult::Ok } else { self.results.remove(0) };
            (r, MoveResultInfo { amount_moved: amount, limited: dir == Dir::South })
        }
        fn track_blc(&mut self, opts: MoveOpts, y_raw: f64, min_move: f64, y_rate: f64, _events: &mut Vec<MountEvent>) {
            self.blc.push(format!("track {} {} {min_move} {y_rate}", opts.0, fmt_f(y_raw, 6)));
        }
        fn apply_blc(&mut self, opts: MoveOpts, y_distance: f64, y_ms: &mut i32, _events: &mut Vec<MountEvent>) {
            self.blc.push(format!("apply {} {} {y_ms}", opts.0, fmt_f(y_distance, 6)));
            *y_ms += 100;
        }
        fn ao_pos(&self) -> (i32, i32) {
            (3, -4)
        }
    }

    /// RA along camera X, Dec along camera Y: the transform is (nearly) the identity.
    fn square_cal() -> Calibration {
        Calibration {
            x_rate: 0.01,
            y_rate: 0.02,
            x_angle: 0.0,
            y_angle: radians(90.0),
            declination: radians(10.0),
            rotator_angle: ROTATOR_POSITION_UNKNOWN,
            binning: 1,
            pier_side: PierSide::East,
            ra_guide_parity: Parity::Even,
            dec_guide_parity: Parity::Odd,
            is_valid: true,
            timestamp: String::new(),
        }
    }

    #[test]
    fn move_option_bits() {
        assert_eq!(dump_move_option_bits(MoveOpts::GUIDE_STEP), "ABG");
        assert_eq!(dump_move_option_bits(MoveOpts::DEDUCED_MOVE), "DBG");
        assert_eq!(dump_move_option_bits(MoveOpts::RECOVERY_MOVE), "B");
        assert_eq!(dump_move_option_bits(MoveOpts::CALIBRATION_MOVE), "-");
        assert_eq!(dump_move_option_bits(MoveOpts::MANUAL), "M");
        assert_eq!(dump_move_option_bits(MoveOpts(31)), "ADBGM");
    }

    #[test]
    fn algorithms_come_from_the_profile_and_are_written_back() {
        let mut p = Profile::default();
        p.i.insert("/scope/XGuideAlgorithm".into(), 2);
        p.i.insert("/scope/YGuideAlgorithm".into(), -1);
        // A setting that does not read as a number is the default (0.2).
        p.s.insert("/scope/GuideAlgorithm/X/Lowpass/minMove".into(), "x".into());
        let mut ev = Vec::new();
        let mut m = Mount::new(MountClass::Scope, &mut p, 0.0, &mut ev);
        assert!(ev.is_empty());
        assert_eq!(m.algo(Axis::X).min_move(), 0.2);
        assert_eq!(m.guide_algorithm_selection(Axis::X), AlgoKind::Lowpass);
        assert_eq!(m.guide_algorithm_selection(Axis::Y), AlgoKind::ResistSwitch);
        assert_eq!((p.i["/scope/XGuideAlgorithm"], p.i["/scope/YGuideAlgorithm"]), (2, 4));
        assert_eq!(p.f["/scope/GuideAlgorithm/X/Lowpass/SlopeWeight"], 5.0);
        assert!(p.f.contains_key("/scope/GuideAlgorithm/Y/ResistSwitch/minMove"));

        // The same kind again keeps the running algorithm and its history.
        let ctx = AlgoCtx::default();
        m.set_guide_algorithm(Axis::X, 1, &mut p, 0.0, &mut ev);
        m.algo_mut(Axis::X).result(1.0, &ctx);
        m.set_guide_algorithm(Axis::X, 1, &mut p, 0.0, &mut ev);
        assert!(m.algo_mut(Axis::X).result(1.0, &ctx) > 0.63 + 1e-12);
        // −1 rebuilds the default even when it runs, silently; an unknown
        // value too, with the factory's error line.
        m.set_guide_algorithm(Axis::X, -1, &mut p, 0.0, &mut ev);
        assert!((m.algo_mut(Axis::X).result(1.0, &ctx) - 0.63).abs() < 1e-12);
        assert!(ev.is_empty());
        m.set_guide_algorithm(Axis::X, 42, &mut p, 0.0, &mut ev);
        assert_eq!((m.guide_algorithm_selection(Axis::X), p.i["/scope/XGuideAlgorithm"]), (AlgoKind::Hysteresis, 1));
        assert_eq!(ev, [MountEvent::Debug("Error thrown from mount.cpp:742->invalid guideAlgorithm".into())]);

        // An AO: Hysteresis on both axes, under its own keys; a stored 7 is
        // refused when it is built.
        let mut p = Profile::default();
        p.i.insert("/stepguider/XGuideAlgorithm".into(), 7);
        ev.clear();
        let ao = Mount::new(MountClass::StepGuider, &mut p, 0.0, &mut ev);
        assert_eq!(ev.len(), 1);
        assert_eq!(p.i["/stepguider/XGuideAlgorithm"], 1);
        assert_eq!(ao.guide_algorithm_selection(Axis::Y), AlgoKind::Hysteresis);
        assert_eq!(p.i["/stepguider/YGuideAlgorithm"], 1);
        assert!(ao.is_step_guider() && ao.guiding_enabled() && !ao.is_connected());
    }

    #[test]
    fn reset_brings_back_the_defaults_and_the_smart_min_move() {
        let ctx = AlgoCtx::default();
        let mut p = Profile::default();
        p.i.insert("/scope/YGuideAlgorithm".into(), 0);
        for (k, v) in [("minMove", 0.5), ("hysteresis", 0.3), ("aggression", 0.9)] {
            p.f.insert(format!("/scope/GuideAlgorithm/X/Hysteresis/{k}"), v);
        }
        let mut m = Mount::new(MountClass::Scope, &mut p, 0.0, &mut Vec::new());
        m.algo_mut(Axis::X).result(1.0, &ctx);
        m.reset_guide_algorithm_params(Axis::X, 0.25, &mut p, 0.0);
        let x = |k: &str| p.f[&format!("/scope/GuideAlgorithm/X/Hysteresis/{k}")];
        assert_eq!((x("minMove"), x("hysteresis"), x("aggression")), (0.25, 0.1, 0.7));
        assert_eq!(p.i["/scope/XGuideAlgorithm"], 1);
        // A new algorithm: no history, 0.9 × 0.7 of a fresh move.
        assert!((m.algo_mut(Axis::X).result(1.0, &ctx) - 0.63).abs() < 1e-12);
        // A refused min-move is the algorithm's default; Identity has none.
        m.set_guide_algorithm(Axis::X, 4, &mut p, 0.0, &mut Vec::new());
        m.reset_guide_algorithm_params(Axis::X, -1.0, &mut p, 0.0);
        assert_eq!(p.f["/scope/GuideAlgorithm/X/ResistSwitch/minMove"], 0.2);
        m.reset_guide_algorithm_params(Axis::Y, 0.25, &mut p, 0.0);
        assert_eq!((m.algo(Axis::Y).min_move(), p.i["/scope/YGuideAlgorithm"]), (-1.0, 0));
    }

    #[test]
    fn guiding_enabled_is_announced_on_a_change() {
        let ctx = AlgoCtx::default();
        let (mut m, log) = recorded(MountClass::Scope);
        let mut ev = Vec::new();
        assert!(m.set_guiding_enabled(false, &ctx, &mut ev));
        assert_eq!(
            ev,
            [
                MountEvent::Debug("MountGuidingEnabled: 0".into()),
                MountEvent::GuideParamChange { name: "MountGuidingEnabled".into(), value: Value::Bool(false) },
            ]
        );
        assert_eq!(take(&log), ["disabled", "disabled"]);
        ev.clear();
        assert!(!m.set_guiding_enabled(false, &ctx, &mut ev));
        assert!(ev.is_empty());
        assert!(m.set_guiding_enabled(true, &ctx, &mut ev));
        assert_eq!(take(&log), ["enabled", "enabled"]);

        let (mut ao, _) = recorded(MountClass::StepGuider);
        ev.clear();
        ao.set_guiding_enabled(false, &ctx, &mut ev);
        assert_eq!(ev[1], MountEvent::GuideParamChange { name: "AOGuidingEnabled".into(), value: Value::Bool(false) });
    }

    #[test]
    fn notifications_reach_both_algorithms() {
        let ctx = AlgoCtx::default();
        let (mut m, log) = recorded(MountClass::Scope);
        let mut ev = Vec::new();
        // RA along camera Y, Dec along camera −X.
        let cal = Calibration { x_angle: radians(90.0), y_angle: radians(180.0), ..square_cal() };
        m.set_calibration(&cal, "T", &mut Profile::default(), &mut ev);
        ev.clear();
        m.notify_guiding_started(&ctx, &mut ev);
        m.notify_guiding_resumed(&ctx, &mut ev);
        m.notify_guiding_stopped(&ctx, &mut ev);
        // A camera-axis dither is handed over on the mount axes.
        m.notify_guiding_dithered(0.0, 2.0, false, &ctx, &mut ev);
        m.notify_guiding_dithered(1.0, -1.0, true, &ctx, &mut ev);
        m.notify_guiding_dither_settle_done(true, &ctx, &mut ev);
        m.notify_direct_move(Point::new(1.5, -2.25), &ctx, &mut ev);
        assert_eq!(
            take(&log),
            [
                "started",
                "started",
                "resumed",
                "resumed",
                "stopped",
                "stopped",
                "dithered 2.000000",
                "dithered 0.000000",
                "dithered 1.000000",
                "dithered -1.000000",
                "settled true",
                "settled true",
                "direct 1.5",
                "direct -2.25",
            ]
        );
        let debug: Vec<&str> = ev.iter().map(|e| if let MountEvent::Debug(l) = e { l.as_str() } else { "" }).collect();
        assert_eq!(
            debug,
            [
                "Mount: notify guiding started",
                "Mount: notify guiding resumed",
                "Mount: notify guiding stopped",
                "Mount: notify guiding dithered (0.0, 2.0)",
                "Mount: notify guiding dithered (1.0, -1.0)",
                "Mount: notify guiding dither settle done success=1",
                "Mount: notify direct move 1.50,-2.25",
            ]
        );
    }

    #[test]
    fn move_offset_pipeline() {
        let ctx = AlgoCtx::default();
        let (mut m, _) = recorded(MountClass::Scope);
        let mut ev = Vec::new();
        m.set_calibration(&square_cal(), "T", &mut Profile::default(), &mut ev);
        let frame = StepFrame {
            frame_number: 7,
            time: 12.5004,
            star_mass: 900.0,
            star_snr: 15.0,
            star_hfd: 2.0,
            star_error: 0,
            avg_dist: 0.3,
        };

        // A guide step: transform, raw Dec to the compensation, the algorithms
        // (which halve), West / North, the compensation's 100 ms on Dec. The
        // time is read after the two 150 ms pulses, cut to whole ms.
        let mut axes = FakeAxes::default();
        axes.clock.set(frame.time);
        let clock = axes.clock.clone();
        let after = || StepFrame { time: clock.get(), ..frame };
        let mut ofs = GuiderOffset { camera_ofs: Point::new(3.0, -2.0), mount_ofs: None };
        ev.clear();
        assert_eq!(m.move_offset(&mut ofs, MoveOpts::GUIDE_STEP, after, &ctx, &mut axes, &mut ev), MoveResult::Ok);
        assert_eq!(axes.calls, [(true, Dir::West, 150, 13), (true, Dir::North, 150, 13)]);
        assert_eq!(axes.blc, ["track 13 -2.000000 0.15 0.02", "apply 13 -1.000000 50"]);
        let mount = ofs.mount_ofs.unwrap();
        assert!((mount.x - 3.0).abs() < 1e-12 && (mount.y + 2.0).abs() < 1e-12);
        assert!(matches!(&ev[0], MountEvent::Debug(l) if l.starts_with("CameraToMount -- cameraTheta (-0.59) - m_xAngle (0.00)")));
        assert_eq!(ev[3], MountEvent::Debug("Moving (3.00, -2.00) raw xDistance=3.00 yDistance=-2.00".into()));
        let Some(MountEvent::GuideStep(step)) = ev.last() else { panic!("no guide step") };
        assert_eq!(
            (step.move_options, step.frame_number, step.time, step.is_ao, step.ao_pos),
            (MoveOpts::GUIDE_STEP, 7, 12.8, false, (3, -4))
        );
        assert_eq!((step.duration_ra, step.direction_ra, step.ra_limited), (150, Dir::West, false));
        assert_eq!((step.duration_dec, step.direction_dec), (150, Dir::North));
        assert_eq!((step.star_mass, step.star_snr, step.star_hfd, step.avg_dist), (900.0, 15.0, 2.0, 0.3));
        assert!((step.guide_distance_ra - 1.5).abs() < 1e-12 && (step.guide_distance_dec + 1.0).abs() < 1e-12);
        assert_eq!(m.last_step(), Some(step));
        assert_eq!(stopwatch_seconds(12.3456), 12.345);

        // Mount axes given, no algorithm: no transform, South flagged limited by the fake.
        let mut axes = FakeAxes::default();
        let mut ofs = GuiderOffset { camera_ofs: Point::new(0.5, 0.5), mount_ofs: Some(Point::new(0.5, 0.5)) };
        ev.clear();
        m.move_offset(&mut ofs, MoveOpts::RECOVERY_MOVE, || frame, &ctx, &mut axes, &mut ev);
        assert_eq!(axes.calls, [(true, Dir::West, 50, 4), (true, Dir::South, 125, 4)]);
        assert_eq!(ev[0], MountEvent::Debug("Moving (0.50, 0.50) raw xDistance=0.50 yDistance=0.50".into()));
        let Some(MountEvent::GuideStep(step)) = ev.last() else { panic!("no guide step") };
        assert!(step.dec_limited && step.time == 12.5);

        // Dead reckoning: nothing at all when both say 0, else no tracking.
        let mut axes = FakeAxes::default();
        let mut ofs = GuiderOffset::default();
        ev.clear();
        assert_eq!(m.move_offset(&mut ofs, MoveOpts::DEDUCED_MOVE, || frame, &ctx, &mut axes, &mut ev), MoveResult::Ok);
        assert!(ev.is_empty() && axes.calls.is_empty() && ofs.mount_ofs.is_none());
        assert!(m.last_step().is_some_and(|s| s.dec_limited));
        m.x_algo = Box::new(Recorder { log: Arc::default(), deduce: 0.2 });
        m.move_offset(&mut ofs, MoveOpts::DEDUCED_MOVE, || frame, &ctx, &mut axes, &mut ev);
        assert_eq!(ev[0], MountEvent::Debug("Dead-reckoning move xDistance=0.20 yDistance=0.00".into()));
        assert_eq!(axes.calls, [(true, Dir::West, 20, 14), (true, Dir::North, 100, 14)]);
        assert_eq!(axes.blc, ["apply 14 0.000000 0"]);
        assert_eq!(ofs.mount_ofs, Some(Point::new(0.2, 0.0)));

        // A slewing RA move skips Dec; an RA error does not, and Dec's OK wins.
        for (results, calls, expect) in [
            (vec![MoveResult::Slewing], 1, MoveResult::Slewing),
            (vec![MoveResult::AoLimit], 1, MoveResult::AoLimit),
            (vec![MoveResult::Error, MoveResult::Ok], 2, MoveResult::Ok),
        ] {
            let mut axes = FakeAxes { results, ..FakeAxes::default() };
            let mut ofs = GuiderOffset { camera_ofs: Point::new(1.0, 1.0), mount_ofs: None };
            let r = m.move_offset(&mut ofs, MoveOpts::GUIDE_STEP, || frame, &ctx, &mut axes, &mut ev);
            assert_eq!((r, axes.calls.len()), (expect, calls));
        }
        // Guiding disabled is the mover's to enforce.
        m.set_guiding_enabled(false, &ctx, &mut ev);
        let mut axes = FakeAxes::default();
        let mut ofs = GuiderOffset { camera_ofs: Point::new(1.0, 1.0), mount_ofs: None };
        m.move_offset(&mut ofs, MoveOpts::GUIDE_STEP, || frame, &ctx, &mut axes, &mut ev);
        assert!(axes.calls.len() == 2 && axes.calls.iter().all(|c| !c.0));
    }

    /// Debug lines out of the events.
    fn debug_lines(ev: &[MountEvent]) -> Vec<&str> {
        ev.iter().filter_map(|e| if let MountEvent::Debug(l) = e { Some(l.as_str()) } else { None }).collect()
    }

    #[test]
    fn transforms_and_moves_match_phd2_off_axis() {
        // PHD2's mount.cpp on the same calibrations (harness mount_r5): the
        // axes 98.6° apart, then a Dec axis the other way round
        // (|yAngleError| > π/2), where MountToCamera mirrors the angle.
        let ctx = AlgoCtx::default();
        let off = Calibration {
            x_rate: 0.004,
            y_rate: 0.005,
            x_angle: 0.3,
            y_angle: 2.0,
            declination: radians(20.0),
            ..square_cal()
        };
        let reversed = Calibration { y_angle: -1.2, ..off.clone() };
        let cases = [
            (
                off,
                -0.1292036732051034,
                [
                    "CameraToMount -- cameraTheta (-0.59) - m_xAngle (0.30) = xAngle (-0.89 = -0.89)",
                    "CameraToMount -- cameraTheta (-0.59) - (m_xAngle (0.30) + m_yAngleError (-0.13)) = yAngle (-0.76 = -0.76)",
                    "CameraToMount -- cameraX=3.00 cameraY=-2.00 hyp=3.61 cameraTheta=-0.59 mountX=2.27 mountY=-2.48, mountTheta=-0.83",
                    "MountToCamera -- mountTheta (-0.46) + m_xAngle (0.30) = xAngle (-0.16 = -0.16)",
                    "MountToCamera -- mountX=1.50 mountY=-0.75 hyp=1.68 mountTheta=-0.46 cameraX=1.65, cameraY=-0.27 cameraTheta=-0.16",
                    "MountToCamera -- mountTheta (0.00) + m_xAngle (0.30) = xAngle (0.30 = 0.30)",
                    "MountToCamera -- mountX=-0.00 mountY=0.00 hyp=0.00 mountTheta=0.00 cameraX=0.00, cameraY=0.00 cameraTheta=0.00",
                ],
                Point::new(2.274969054054139, -2.4808008886776425),
                Point::new(1.6546448886844138, -0.27322205685219514),
                (Dir::North, 248),
            ),
            (
                reversed,
                3.0707963267948966,
                [
                    "CameraToMount -- cameraTheta (-0.59) - m_xAngle (0.30) = xAngle (-0.89 = -0.89)",
                    "CameraToMount -- cameraTheta (-0.59) - (m_xAngle (0.30) + m_yAngleError (3.07)) = yAngle (-3.96 = 2.32)",
                    "CameraToMount -- cameraX=3.00 cameraY=-2.00 hyp=3.61 cameraTheta=-0.59 mountX=2.27 mountY=2.63, mountTheta=0.86",
                    "MountToCamera -- mountTheta (0.46) + m_xAngle (0.30) = xAngle (0.76 = 0.76)",
                    "MountToCamera -- mountX=1.50 mountY=-0.75 hyp=1.68 mountTheta=0.46 cameraX=1.21, cameraY=1.16 cameraTheta=0.76",
                    "MountToCamera -- mountTheta (-0.00) + m_xAngle (0.30) = xAngle (0.30 = 0.30)",
                    "MountToCamera -- mountX=-0.00 mountY=0.00 hyp=0.00 mountTheta=-0.00 cameraX=0.00, cameraY=0.00 cameraTheta=0.00",
                ],
                Point::new(2.274969054054139, 2.6293015458356503),
                Point::new(1.2113645786924043, 1.1597826768362138),
                (Dir::South, 263),
            ),
        ];
        let close = |a: Point, b: Point| (a.x - b.x).abs() < 1e-12 && (a.y - b.y).abs() < 1e-12;
        for (cal, y_angle_error, lines, mount, camera, (y_dir, y_ms)) in cases {
            let (mut m, _) = recorded(MountClass::Scope);
            let mut ev = Vec::new();
            m.set_calibration(&cal, "T", &mut Profile::default(), &mut ev);
            assert!((m.mount_cal().y_angle_error() - y_angle_error).abs() < 1e-15);
            ev.clear();
            assert!(close(m.camera_to_mount(Point::new(3.0, -2.0), true, &mut ev), mount));
            assert!(close(m.mount_to_camera(Point::new(1.5, -0.75), true, &mut ev), camera));
            let zero = m.mount_to_camera(Point::new(-0.0, 0.0), true, &mut ev);
            assert!(zero.x.is_sign_positive() && zero.y.is_sign_positive());
            assert_eq!(debug_lines(&ev), lines);

            // The whole guide step (the recorders halve, the fake's
            // compensation adds 100 ms to Dec): PHD2 asks 284 ms West.
            let mut axes = FakeAxes::default();
            let mut ofs = GuiderOffset { camera_ofs: Point::new(3.0, -2.0), mount_ofs: None };
            ev.clear();
            m.move_offset(&mut ofs, MoveOpts::GUIDE_STEP, StepFrame::default, &ctx, &mut axes, &mut ev);
            assert_eq!(debug_lines(&ev)[..3], lines[..3]);
            let moving = format!("Moving (3.00, -2.00) raw xDistance=2.27 yDistance={}", fmt_f(mount.y, 2));
            assert_eq!(debug_lines(&ev)[3], moving);
            assert_eq!(axes.calls, [(true, Dir::West, 284, 13), (true, y_dir, y_ms + 100, 13)]);
            let step = m.last_step().unwrap();
            assert!(close(step.mount_offset, mount));
            assert!((step.guide_distance_ra - mount.x / 2.0).abs() < 1e-15);
            assert!((step.guide_distance_dec - mount.y / 2.0).abs() < 1e-15);
            assert_eq!((step.direction_ra, step.duration_ra, step.direction_dec), (Dir::West, 284, y_dir));
        }
    }

    #[test]
    fn settings_summary_matches_phd2() {
        // PHD2's GetSettingsSummary on the same mounts (harness mount_r5).
        let mut ev = Vec::new();
        let mut p = Profile::default();
        let mut m = Mount::new(MountClass::Scope, &mut p, 0.0, &mut ev);
        m.set_name("INDI Mount [Telescope Simulator]");
        m.connect();
        let off = Calibration {
            x_rate: 0.004,
            y_rate: 0.005,
            x_angle: 0.3,
            y_angle: 2.0,
            declination: radians(20.0),
            ..square_cal()
        };
        m.set_calibration(&off, "T", &mut p, &mut ev);
        p.f.insert("/scope/calibration/image_scale".into(), 1.5);
        p.f.insert("/scope/calibration/ortho_error".into(), 4.56);
        p.i.insert("/scope/calibration/ra_step_count".into(), 8);
        let algos = "X guide algorithm = Hysteresis, Hysteresis = 0.100, Aggression = 0.700, Minimum move = 0.200\n\
                     Y guide algorithm = Resist Switch, Minimum move = 0.200 Aggression = 100% FastSwitch = enabled\n";
        assert_eq!(
            m.settings_summary(true, Some("INDI Mount [EQMod Mount]"), Some((false, 450)), &mut p, None, &mut ev),
            format!(
                "Mount = INDI Mount [Telescope Simulator], connected, guiding enabled, AuxMount=INDI Mount [EQMod Mount], \
                 xAngle = 17.2, xRate = 4.000, yAngle = 114.6, yRate = 5.000, parity = +/-\n\
                 Norm rates RA = 6.4\"/s @ dec 0, Dec = 7.5\"/s; ortho.err. = 4.6 deg\n\
                 {algos}Backlash comp = disabled, pulse = 450 ms\n"
            )
        );
        // Dec unknown: no Dec compensation; no steps: scale 1, error unknown.
        m.set_calibration(&Calibration { declination: UNKNOWN_DECLINATION, ..off }, "T", &mut p, &mut ev);
        p.i.insert("/scope/calibration/ra_step_count".into(), 0);
        assert_eq!(
            m.settings_summary(true, None, Some((true, 450)), &mut p, None, &mut ev),
            format!(
                "Mount = INDI Mount [Telescope Simulator], connected, guiding enabled, \
                 xAngle = 17.2, xRate = 4.000, yAngle = 114.6, yRate = 5.000, parity = +/-\n\
                 Norm rates RA = 4.0\"/s @ dec 0, Dec = 5.0\"/s; ortho.err. = unknown\n\
                 {algos}Backlash comp = enabled, pulse = 450 ms\n"
            )
        );
        let mut n = Mount::new(MountClass::Scope, &mut Profile::default(), 0.0, &mut ev);
        n.set_name("On-camera");
        assert_eq!(
            n.settings_summary(n.is_calibrated(), None, Some((false, 0)), &mut p, None, &mut ev),
            format!(
                "Mount = On-camera, not connected, guiding enabled, not calibrated\n\
                 {algos}Backlash comp = disabled, pulse = 0 ms\n"
            )
        );

        // An AO: rates per step, no backlash line.
        let mut p = Profile::default();
        let mut ao = Mount::new(MountClass::StepGuider, &mut p, 0.0, &mut ev);
        ao.set_name("INDI AO [SX AO]");
        ao.connect();
        ao.set_guiding_enabled(false, &AlgoCtx::default(), &mut ev);
        let cal = Calibration {
            x_rate: 0.25,
            y_rate: 0.3,
            x_angle: radians(-170.0),
            y_angle: radians(95.0),
            declination: UNKNOWN_DECLINATION,
            ra_guide_parity: Parity::Odd,
            dec_guide_parity: Parity::Even,
            ..square_cal()
        };
        ao.set_calibration(&cal, "T", &mut p, &mut ev);
        p.f.insert("/stepguider/calibration/image_scale".into(), 2.25);
        p.f.insert("/stepguider/calibration/ortho_error".into(), 1.0);
        p.i.insert("/stepguider/calibration/ra_step_count".into(), 3);
        let hyst = "Hysteresis = 0.100, Aggression = 0.700, Minimum move = 0.200\n";
        assert_eq!(
            ao.settings_summary(ao.is_calibrated(), None, None, &mut p, None, &mut ev),
            format!(
                "AO = INDI AO [SX AO], connected, guiding disabled, \
                 xAngle = -170.0, xRate = 250.000, yAngle = 95.0, yRate = 300.000, parity = -/+\n\
                 Norm rates X = 562 mas/step, Y = 675 mas/step; ortho.err. = 1.0 deg\n\
                 X guide algorithm = Hysteresis, {hyst}Y guide algorithm = Hysteresis, {hyst}"
            )
        );
    }

    #[test]
    fn calibration_set_and_clear() {
        let mut p = Profile::default();
        let (mut m, _) = recorded(MountClass::Scope);
        let mut ev = Vec::new();
        m.set_calibration(&square_cal(), "Wed Nov 11 12:00:00 2026", &mut p, &mut ev);
        assert_eq!(
            ev,
            [
                MountEvent::Debug(
                    "Mount::SetCalibration (scope) -- xAngle=0.0 yAngle=90.0 xRate=10.000 yRate=20.000 bin=1 dec=10.0 pierSide=0 par=+/- rotAng=None".into()
                ),
                MountEvent::Debug("Mount::SetCalibration (scope) -- sets m_xAngle=0.0 m_yAngleError=0.0".into()),
            ]
        );
        assert_eq!(p.s["/scope/calibration/timestamp"], "Wed Nov 11 12:00:00 2026");
        assert_eq!(p.f["/scope/calibration/yRate"], 0.02);
        // Not connected: not calibrated, though the calibration is in.
        assert!(m.is_mount_calibrated() && !m.is_calibrated());
        m.connect();
        assert!(m.is_calibrated());
        assert_eq!((m.ra_parity(), m.dec_parity(), m.calibration_declination()), (Parity::Even, Parity::Odd, radians(10.0)));
        assert_eq!((m.x_rate(), m.y_rate()), (0.01, 0.02));
        m.clear_calibration();
        assert!(!m.is_calibrated() && m.mount_cal().is_valid);
        assert_eq!((m.ra_parity(), m.calibration_declination()), (Parity::Unknown, UNKNOWN_DECLINATION));
        assert_eq!(m.get_last_calibration(&p).timestamp, "Wed Nov 11 12:00:00 2026");
    }

    #[test]
    fn bogus_guide_speeds_are_dropped_once_per_session() {
        let mut ev = Vec::new();
        let mut validated = false;
        let half = 0.5 * 15.0 * 0.9973 / 3600.0;
        assert!(valid_guide_rates(half, half, &mut validated, &mut ev));
        assert!(validated && ev.is_empty());
        assert!(!valid_guide_rates(0.01, half, &mut validated, &mut ev));
        assert_eq!(ev, [MountEvent::Debug("Invalid mount guide speeds: RA: 0.0100, Dec: 0.0021".into())]);

        let m = Mount::new(MountClass::Scope, &mut Profile::default(), 0.0, &mut ev);
        let mut p = Profile::default();
        p.f.insert("/scope/calibration/ra_guide_rate".into(), 0.01);
        p.f.insert("/scope/calibration/dec_guide_rate".into(), half);
        let mut validated = false;
        ev.clear();
        let d = m.load_calibration_details(&mut p, Some(&mut validated), &mut ev);
        assert_eq!((d.ra_guide_speed, d.dec_guide_speed), (-1.0, -1.0));
        assert_eq!(p.f["/scope/calibration/ra_guide_rate"], -1.0);
        assert_eq!(ev[1], MountEvent::Debug("Bogus guide speeds cleared in LoadCalibrationDetails".into()));
        // Checked once: the next load keeps what it reads.
        p.f.insert("/scope/calibration/ra_guide_rate".into(), 0.01);
        let d = m.load_calibration_details(&mut p, Some(&mut validated), &mut ev);
        assert_eq!(d.ra_guide_speed, 0.01);
        // Saving bogus speeds stores −1 (the check runs whatever the flag says).
        ev.clear();
        let d = CalibrationDetails { ra_guide_speed: 0.01, dec_guide_speed: half, ..CalibrationDetails::default() };
        m.save_calibration_details(&d, &mut p, Some(&mut validated), &mut ev);
        assert_eq!(p.f["/scope/calibration/ra_guide_rate"], -1.0);
        assert_eq!(ev[1], MountEvent::Debug("Bogus guide speeds over-written in SaveCalibrationDetails".into()));
        // Without a pointing source nothing is checked.
        m.save_calibration_details(&d, &mut p, None, &mut ev);
        assert_eq!(p.f["/scope/calibration/ra_guide_rate"], 0.01);
    }

    #[test]
    fn pointing_declination_is_clamped_like_indi() {
        struct At(f64);
        impl PointingSource for At {
            fn coordinates(&self) -> Option<(f64, f64, f64)> {
                Some((1.0, self.0, 2.0))
            }
            fn pier_side(&self) -> PierSide {
                PierSide::Unknown
            }
            fn guide_rates(&self) -> Option<(f64, f64)> {
                None
            }
            fn is_slewing(&self) -> bool {
                false
            }
            fn can_report_position(&self) -> bool {
                true
            }
        }
        assert_eq!(At(89.5).declination_radians(), Some(radians(89.0)));
        assert_eq!(At(-30.0).declination_radians(), Some(radians(-30.0)));
        assert_eq!(AlertKind::Warning.type_str(), "warning");
    }
}
