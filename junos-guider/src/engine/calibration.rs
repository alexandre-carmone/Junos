//! The calibration model: what a mount calibration is and what a mount
//! holds of it in memory, the camera ↔ mount transforms built on it, the
//! pier flip and the "adjust for where the scope points now" step run at
//! guide start (scope and AO), the post-calibration sanity checks, and the
//! calibration-step calculator.
//!
//! Everything here is pure: the persisted form lives in the profile store
//! (the key names are in [`keys`]), the debug-log and alert wording comes
//! back as strings for the caller to write or show.
//!
//! Ported from PHD2 `src/mount.h`, `src/mount.cpp`, `src/scope.cpp`, `src/stepguider.cpp`, `src/calstep_dialog.cpp`, `src/image_math.h` — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;

use super::algo::MountClass;
use super::log::cfmt::fmt_f;
use super::traits::{Parity, PierSide, Point};

/// `UNKNOWN_DECLINATION` (`mount.h:74`): the declination of a calibration done
/// without a pointing source. Radians everywhere else.
pub const UNKNOWN_DECLINATION: f64 = 997.0;

/// `CALIBRATION_RATE_UNCALIBRATED` (`scope.h:39`): the `y_rate` of a
/// calibration done with Dec guiding off, which stops after the RA leg.
pub const CALIBRATION_RATE_UNCALIBRATED: f64 = 123e4;

/// `Scope::DEC_COMP_LIMIT` (`scope.cpp:68`): no Dec compensation, and no rate
/// sanity check, when the calibration was done further than 60° from the
/// equator.
pub const DEC_COMP_LIMIT: f64 = PI / 2.0 * 2.0 / 3.0;

/// `Rotator::POSITION_UNKNOWN` (`rotator.cpp:42`, a float in PHD2, exact as a
/// double). The stored `rotator_angle` of a calibration made without a rotator.
pub const ROTATOR_POSITION_UNKNOWN: f64 = -888.0;

/// `norm` (`image_math.h:102`): `val` wrapped into `[start, end)`.
pub fn norm(val: f64, start: f64, end: f64) -> f64 {
    let range = end - start;
    let ofs = val - start;
    val - (ofs / range).floor() * range
}

/// `norm_angle` (`image_math.h:110`): radians wrapped into `[-π, π)`.
pub fn norm_angle(val: f64) -> f64 {
    norm(val, -PI, PI)
}

/// `degrees` (`image_math.h:120`): `r * 180 / π`, in that order. Rust's
/// `to_degrees` multiplies by a folded `180 / π` and is 1 ulp off for about
/// a quarter of the inputs, enough to move a `%.1f` or a 50 ms bucket.
pub fn degrees(radians: f64) -> f64 {
    radians * 180.0 / PI
}

/// `radians` (`image_math.h:125`): `d * π / 180`, in that order (see
/// [`degrees`]).
pub fn radians(degrees: f64) -> f64 {
    degrees * PI / 180.0
}

/// `wxMax` (`a > b ? a : b`): a NaN in `b` comes through, unlike `f64::max`.
fn wx_max(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

/// `wxMin` (`a < b ? a : b`), see [`wx_max`].
fn wx_min(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

/// The profile keys under `/<mount class>/calibration/` (`mount.cpp:1582-1849`).
/// The mount class is `scope` or `stepguider`.
pub mod keys {
    /// `"/<class>/calibration/"`.
    pub fn prefix(mount_class: &str) -> String {
        format!("/{mount_class}/calibration/")
    }

    // Calibration (SetCalibration / GetLastCalibration).
    pub const TIMESTAMP: &str = "timestamp";
    pub const X_ANGLE: &str = "xAngle";
    pub const Y_ANGLE: &str = "yAngle";
    pub const X_RATE: &str = "xRate";
    pub const Y_RATE: &str = "yRate";
    pub const BINNING: &str = "binning";
    pub const DECLINATION: &str = "declination";
    pub const PIER_SIDE: &str = "pierSide";
    pub const RA_GUIDE_PARITY: &str = "raGuideParity";
    pub const DEC_GUIDE_PARITY: &str = "decGuideParity";
    pub const ROTATOR_ANGLE: &str = "rotatorAngle";

    // CalibrationDetails (SaveCalibrationDetails / LoadCalibrationDetails).
    pub const FOCAL_LENGTH: &str = "focal_length";
    pub const IMAGE_SCALE: &str = "image_scale";
    pub const RA_GUIDE_RATE: &str = "ra_guide_rate";
    pub const DEC_GUIDE_RATE: &str = "dec_guide_rate";
    pub const ORTHO_ERROR: &str = "ortho_error";
    pub const ORIG_BINNING: &str = "orig_binning";
    pub const ORIG_TIMESTAMP: &str = "orig_timestamp";
    pub const ORIG_PIERSIDE: &str = "orig_pierside";
    pub const RA_STEPS: &str = "ra_steps";
    pub const DEC_STEPS: &str = "dec_steps";
    pub const RA_STEP_COUNT: &str = "ra_step_count";
    pub const DEC_STEP_COUNT: &str = "dec_step_count";
    pub const LAST_ISSUE: &str = "last_issue";
}

/// PHD2's `Calibration` (`mount.h:76`). Rates in px/ms, angles in radians.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Calibration {
    pub x_rate: f64,
    /// [`CALIBRATION_RATE_UNCALIBRATED`] when Dec was not calibrated.
    pub y_rate: f64,
    pub x_angle: f64,
    pub y_angle: f64,
    /// Radians, or [`UNKNOWN_DECLINATION`].
    pub declination: f64,
    /// Degrees, or [`ROTATOR_POSITION_UNKNOWN`].
    pub rotator_angle: f64,
    pub binning: u16,
    pub pier_side: PierSide,
    pub ra_guide_parity: Parity,
    pub dec_guide_parity: Parity,
    /// `isValid`: false for the default-constructed calibration and for a
    /// profile with no stored timestamp; set by `SetCalibration` and never
    /// cleared after. Not "the mount is calibrated", see
    /// [`MountCalibration`].
    pub is_valid: bool,
    /// The stored `timestamp` string, as PHD2 wrote it.
    pub timestamp: String,
}

impl Calibration {
    /// `m_yAngleError` (`mount.cpp:1570`): how far the Dec axis sits from
    /// 90° past the RA axis. 0 when orthogonal, ±π when the Dec axis runs
    /// the other way.
    pub fn y_angle_error(&self) -> f64 {
        norm_angle(self.x_angle - self.y_angle + PI / 2.0)
    }

    /// `Mount::yAngle` (`mount.cpp:879`): the Dec axis angle rebuilt from
    /// `x_angle` and [`y_angle_error`](Self::y_angle_error), so always in
    /// `[-π, π)`. Every reader goes through this — the flip, the settings
    /// summary, `get_calibration_data`'s `yAngle`, the guide-log header —
    /// never the raw `y_angle`, which may sit outside that range (a manual
    /// calibration, an imported profile, `atan2` returning π) and is only
    /// what gets stored.
    pub fn y_angle(&self) -> f64 {
        norm_angle(self.x_angle - self.y_angle_error() + PI / 2.0)
    }

    /// `Scope::IsCalibrated`'s Dec leg (`scope.cpp:1137`): a Dec rate was
    /// measured.
    pub fn dec_calibrated(&self) -> bool {
        self.y_rate != CALIBRATION_RATE_UNCALIBRATED
    }
}

/// PHD2's `CalibrationIssueType` (`mount.h:94`), the verdict of the sanity
/// check. The discriminants are the persisted `last_issue` values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CalibrationIssue {
    #[default]
    None = 0,
    Steps = 1,
    Angle = 2,
    Rates = 3,
    Different = 4,
}

impl CalibrationIssue {
    /// The persisted `last_issue` back to an issue; anything else is `None`.
    pub fn from_i32(v: i32) -> CalibrationIssue {
        match v {
            1 => CalibrationIssue::Steps,
            2 => CalibrationIssue::Angle,
            3 => CalibrationIssue::Rates,
            4 => CalibrationIssue::Different,
            _ => CalibrationIssue::None,
        }
    }

    /// The qualifier of the `/Confirm/<profile>/CalWarning_<q>` suppression
    /// key (`scope.cpp:818`).
    pub fn warning_key_qualifier(self) -> &'static str {
        match self {
            CalibrationIssue::Angle => "Angle",
            CalibrationIssue::Different => "Diff",
            CalibrationIssue::Steps => "Steps",
            CalibrationIssue::Rates => "Rates",
            CalibrationIssue::None => "Bogus",
        }
    }

    /// `CalibrationIssueString` (`mount.h:103`): the name the guide log and
    /// the calibration review print (`Last Cal Issue = %s`).
    pub fn text(self) -> &'static str {
        match self {
            CalibrationIssue::None => "None",
            CalibrationIssue::Steps => "Steps",
            CalibrationIssue::Angle => "Orthogonality",
            CalibrationIssue::Rates => "Rates",
            CalibrationIssue::Different => "Difference",
        }
    }

    /// The advisory alert PHD2 shows for an issue (`scope.cpp:949-962`);
    /// empty for `None`.
    pub fn advisory(self) -> &'static str {
        match self {
            CalibrationIssue::None => "",
            CalibrationIssue::Steps => {
                "Advisory: Calibration completed but few guide steps were used, so accuracy is questionable"
            }
            CalibrationIssue::Angle => {
                "Advisory: Calibration completed but RA/Dec axis angles are questionable and guiding may be impaired"
            }
            CalibrationIssue::Different => {
                "Advisory: This calibration is substantially different from the previous one - have you changed configurations?"
            }
            CalibrationIssue::Rates => {
                "Advisory: Calibration completed but RA and Dec rates vary by an unexpected amount (often caused by large Dec backlash)"
            }
        }
    }
}

/// PHD2's `CalibrationDetails` (`mount.h:105`): what the calibration review
/// and the sanity check look at. Guide speeds in deg/s, `-1.0` when unknown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalibrationDetails {
    pub focal_length: i32,
    /// Arcsec per pixel.
    pub image_scale: f64,
    pub ra_guide_speed: f64,
    pub dec_guide_speed: f64,
    /// Degrees, see [`ortho_error_deg`].
    pub ortho_error: f64,
    /// A double in PHD2 (`mount.h:112`), compared to the camera's `u16` binning.
    pub orig_binning: f64,
    pub ra_steps: Vec<Point>,
    pub dec_steps: Vec<Point>,
    pub ra_step_count: i32,
    pub dec_step_count: i32,
    pub last_issue: CalibrationIssue,
    pub orig_timestamp: String,
    pub orig_pier_side: PierSide,
}

impl CalibrationDetails {
    /// `CalibrationDetails::IsValid`: some RA steps were recorded.
    pub fn is_valid(&self) -> bool {
        self.ra_step_count > 0
    }
}

/// `orthoError` (`scope.cpp:1107`, also `scope.cpp:891`): how far the angle
/// between the axes is from 90°, degrees.
pub fn ortho_error_deg(x_angle: f64, y_angle: f64) -> f64 {
    degrees((norm_angle(x_angle - y_angle).abs() - PI / 2.0).abs())
}

/// `Mount::TransformCameraCoordinatesToMountCoordinates` (`mount.cpp:1135`):
/// a camera-plane offset into (RA, Dec) axis amounts. The RA component is the
/// projection on the RA axis. The Dec component, `sin(θ − (x_angle +
/// y_angle_error))`, is the projection on the axis at `2·x_angle − y_angle +
/// π`: the measured Dec axis mirrored about the perpendicular to RA, which is
/// the measured axis itself only when the axes are orthogonal. So the pair is
/// not a rotation when they are not.
pub fn camera_to_mount(offset: Point, cal: &Calibration) -> Point {
    let hyp = offset.x.hypot(offset.y);
    let camera_theta = offset.y.atan2(offset.x);
    let x_angle = camera_theta - cal.x_angle;
    let y_angle = camera_theta - (cal.x_angle + cal.y_angle_error());
    Point::new(x_angle.cos() * hyp, y_angle.sin() * hyp)
}

/// `Mount::TransformMountCoordinatesToCameraCoordinates` (`mount.cpp:1181`):
/// (RA, Dec) axis amounts back onto the camera plane. A plain rotation by
/// `x_angle`, after mirroring the mount angle when the Dec axis runs the
/// other way (`|y_angle_error| > π/2`). Only exact for orthogonal axes.
pub fn mount_to_camera(offset: Point, cal: &Calibration) -> Point {
    let hyp = offset.x.hypot(offset.y);
    let mut mount_theta = offset.y.atan2(offset.x);
    if cal.y_angle_error().abs() > PI / 2.0 {
        mount_theta = -mount_theta;
    }
    let x_angle = mount_theta + cal.x_angle;
    Point::new(x_angle.cos() * hyp, x_angle.sin() * hyp)
}

/// `ParityStr` (`mount.cpp:104`).
fn parity_str(p: Parity) -> &'static str {
    match p {
        Parity::Even => "+",
        Parity::Odd => "-",
        Parity::Unknown => "?",
    }
}

/// `Mount::DeclinationStr` with the default `%.1f`.
fn declination_str(dec: f64) -> String {
    if dec == UNKNOWN_DECLINATION { "Unknown".to_string() } else { fmt_f(degrees(dec), 1) }
}

/// `RotAngleStr` (`mount.cpp:884`).
fn rot_angle_str(rot: f64) -> String {
    if rot == ROTATOR_POSITION_UNKNOWN { "None".to_string() } else { fmt_f(rot, 1) }
}

/// What a mount holds of its calibration in memory: PHD2's `Mount::m_cal`,
/// `m_calibrated` and `m_xRate`. The scope and the AO each own one.
///
/// `calibrated` is not `cal.is_valid`. [`set`](Self::set) turns both on;
/// [`clear`](Self::clear) (PHD2's `ClearCalibration`: the clear RPC, a new
/// calibration starting, an image-scale change, a gear change) drops only
/// `calibrated`. The cleared calibration still transforms, is still in the
/// profile, and is still the "previous calibration" the sanity check and the
/// backlash tool read through `GetLastCalibration`. Whatever asks "is this
/// mount calibrated" — the guider before guiding, the flip, `get_calibrated`,
/// the parities and the declination below — reads `calibrated` (with the
/// connection, see [`is_calibrated`](Self::is_calibrated)), never `is_valid`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MountCalibration {
    /// `m_cal`.
    pub cal: Calibration,
    /// `m_calibrated`.
    pub calibrated: bool,
    /// `m_xRate`: the RA rate guiding divides by, px/ms. `cal.x_rate` after
    /// every [`set`](Self::set), Dec-compensated by
    /// [`adjust_for_scope_pointing`]; never stored.
    pub x_rate: f64,
}

impl MountCalibration {
    /// `Mount::SetCalibration`'s in-memory part (`mount.cpp:1544`): takes
    /// every field but the timestamp (PHD2 stamps only the stored copy),
    /// marks the calibration valid and the mount calibrated, and resets
    /// `x_rate` to the stored rate, so a set while guiding drops the Dec
    /// compensation. Returns its two debug-log lines. PHD2 stores the
    /// calibration with a fresh timestamp in the same call: that is the
    /// caller's. PHD2's `GUIDE_PARITY_UNCHANGED` (keep the old parity) has no
    /// [`Parity`] counterpart; nothing here passes it.
    pub fn set(&mut self, cal: &Calibration, mount_class: MountClass) -> Vec<String> {
        let class = mount_class.class_name();
        let first = format!(
            "Mount::SetCalibration ({class}) -- xAngle={} yAngle={} xRate={} yRate={} bin={} dec={} pierSide={} par={}/{} rotAng={}",
            fmt_f(degrees(cal.x_angle), 1),
            fmt_f(degrees(cal.y_angle), 1),
            fmt_f(cal.x_rate * 1000.0, 3),
            fmt_f(cal.y_rate * 1000.0, 3),
            cal.binning,
            declination_str(cal.declination),
            cal.pier_side as i32,
            parity_str(cal.ra_guide_parity),
            parity_str(cal.dec_guide_parity),
            rot_angle_str(cal.rotator_angle)
        );
        let timestamp = std::mem::take(&mut self.cal.timestamp);
        self.cal = Calibration { is_valid: true, timestamp, ..cal.clone() };
        self.x_rate = cal.x_rate;
        self.calibrated = true;
        let second = format!(
            "Mount::SetCalibration ({class}) -- sets m_xAngle={} m_yAngleError={}",
            fmt_f(degrees(self.cal.x_angle), 1),
            fmt_f(degrees(self.cal.y_angle_error()), 1)
        );
        vec![first, second]
    }

    /// `Mount::ClearCalibration` (`mount.cpp:1536`): the mount is no longer
    /// calibrated; the calibration itself stays. The scope and the AO also
    /// reset their calibration state machine, which is theirs.
    pub fn clear(&mut self) {
        self.calibrated = false;
    }

    /// `Mount::IsCalibrated` (`mount.cpp:1525`): connected and calibrated.
    /// The scope's override also wants a Dec rate unless Dec guiding is off
    /// (`Scope::IsCalibrated`, `scope.cpp:1124`).
    pub fn is_calibrated(&self, connected: bool) -> bool {
        connected && self.calibrated
    }

    /// `Mount::RAParity` (`mount.h:435`): unknown unless calibrated.
    pub fn ra_parity(&self) -> Parity {
        if self.calibrated { self.cal.ra_guide_parity } else { Parity::Unknown }
    }

    /// `Mount::DecParity` (`mount.h:440`): unknown unless calibrated.
    pub fn dec_parity(&self) -> Parity {
        if self.calibrated { self.cal.dec_guide_parity } else { Parity::Unknown }
    }

    /// `Mount::GetCalibrationDeclination` (`mount.h:445`): radians, or
    /// [`UNKNOWN_DECLINATION`] unless calibrated.
    pub fn declination(&self) -> f64 {
        if self.calibrated { self.cal.declination } else { UNKNOWN_DECLINATION }
    }
}

/// What [`flip_calibration`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlipResult {
    /// The status-bar line, `CAL: East(15,105)->West(-165,105)`.
    pub status: String,
    /// The debug-log lines in PHD2's order: before, pre-normalize, after,
    /// then `SetCalibration`'s two.
    pub log: Vec<String>,
}

/// `Mount::FlipCalibration` (`mount.cpp:891`): the scope's calibration as
/// seen from the other side of the pier. RA turns by π; Dec turns by π too
/// when the mount reverses its Dec motor on a flip (`CalFlipRequiresDecFlip`),
/// else the Dec parity inverts instead. The pier side swaps; the RA parity,
/// the rates, the declination and the rotator angle stay. The angles read
/// are `x_angle` and the normalised [`Calibration::y_angle`].
///
/// `calibrated` is the scope's `IsCalibrated()`: connected,
/// `state.calibrated`, and a Dec rate unless Dec guiding is off. When it is
/// false nothing changes and the error is the debug-log line PHD2 writes
/// (its `__FILE__` is the build's path, shortened here); the flip RPC then
/// answers error 1 "could not flip calibration" and emits no event.
///
/// A flip ends with `SetCalibration` ([`MountCalibration::set`]), so:
/// - the caller stores `state.cal` with a fresh timestamp, which fires
///   `ConfigurationChange`;
/// - `state.x_rate` is back to the stored rate, so a flip while guiding
///   drops the Dec compensation;
/// - only the flip RPC and menu (`FlipCalibrationData`,
///   `myframe_events.cpp:965`) then emit `CalibrationDataFlipped`; the flip
///   inside [`adjust_for_scope_pointing`] does not.
pub fn flip_calibration(
    state: &mut MountCalibration,
    calibrated: bool,
    dec_flip_required: bool,
) -> Result<FlipResult, String> {
    if !calibrated {
        return Err("Error thrown from mount.cpp:899->cannot flip if not calibrated".to_string());
    }
    let cal = state.cal.clone();
    let orig_x = cal.x_angle;
    let orig_y = cal.y_angle();
    let mut log = vec![format!(
        "FlipCalibration before: x={}, y={} decFlipRequired={} sideOfPier={} rotAngle={} parity={}/{}",
        fmt_f(degrees(orig_x), 1),
        fmt_f(degrees(orig_y), 1),
        dec_flip_required as i32,
        cal.pier_side.label(),
        rot_angle_str(cal.rotator_angle),
        parity_str(cal.ra_guide_parity),
        parity_str(cal.dec_guide_parity)
    )];

    let mut new_x = orig_x + PI;
    let mut new_y = orig_y;
    if dec_flip_required {
        new_y += PI;
    }
    log.push(format!(
        "FlipCalibration pre-normalize: x={}, y={}",
        fmt_f(degrees(new_x), 1),
        fmt_f(degrees(new_y), 1)
    ));
    new_x = norm_angle(new_x);
    new_y = norm_angle(new_y);

    let prior_side = cal.pier_side;
    let new_side = cal.pier_side.opposite();
    // Guide(NORTH) moves the star the other way on the other side of the pier,
    // unless the mount already reverses its Dec motor on the flip.
    let new_dec_parity = if dec_flip_required { cal.dec_guide_parity } else { cal.dec_guide_parity.flipped() };
    log.push(format!(
        "FlipCalibration after: x={} y={} sideOfPier={} parity={}/{}",
        fmt_f(degrees(new_x), 1),
        fmt_f(degrees(new_y), 1),
        new_side.label(),
        parity_str(cal.ra_guide_parity),
        parity_str(new_dec_parity)
    ));

    let flipped = Calibration {
        x_angle: new_x,
        y_angle: new_y,
        pier_side: new_side,
        dec_guide_parity: new_dec_parity,
        ..cal
    };
    // Only the scope flips: the flip RPC goes to TheScope(), and the AO's
    // own adjustment never calls it.
    log.extend(state.set(&flipped, MountClass::Scope));

    // PierSideStrTr(side, "") — an unknown side prints as nothing.
    let side_str = |s: PierSide| if s == PierSide::Unknown { "" } else { s.label() };
    let status = format!(
        "CAL: {}({},{})->{}({},{})",
        side_str(prior_side),
        fmt_f(degrees(orig_x), 0),
        fmt_f(degrees(orig_y), 0),
        side_str(new_side),
        fmt_f(degrees(new_x), 0),
        fmt_f(degrees(new_y), 0)
    );
    Ok(FlipResult { status, log })
}

/// Where the scope points and what the gear reports when guiding starts:
/// the inputs `Mount::AdjustCalibrationForScopePointing` reads from the
/// pointing source, the camera and the rotator.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopePointing {
    /// Radians; `None` when the pointing source cannot say (PHD2's
    /// `UNKNOWN_DECLINATION`).
    pub declination: Option<f64>,
    pub pier_side: PierSide,
    /// Degrees; `None` without a rotator (`Rotator::POSITION_UNKNOWN`).
    pub rotator_angle: Option<f64>,
    /// The camera's current binning.
    pub binning: u16,
    /// `(RA, Dec)` guide rates in deg/s, `None` when the mount cannot report
    /// its position or its rates (`CanReportPosition` / `GetGuideRates`).
    pub guide_rates: Option<(f64, f64)>,
    /// The pixel size the camera reports, µm.
    pub camera_pixel_size: f64,
    /// The pixel size in the profile, µm.
    pub profile_pixel_size: f64,
}

/// The scope settings the adjustment depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdjustOpts {
    /// `UseDecComp` (`Scope::DecCompensationEnabled`).
    pub use_dec_comp: bool,
    /// `CalFlipRequiresDecFlip`.
    pub cal_flip_requires_dec_flip: bool,
    /// The scope is connected. PHD2 also adjusts a disconnected scope behind
    /// an AO, whose flip `IsCalibrated()` then refuses.
    pub connected: bool,
}

/// One thing [`adjust_for_scope_pointing`] hands back, in PHD2's order.
#[derive(Clone, Debug, PartialEq)]
pub enum AdjustStep {
    /// A debug-log line.
    Log(String),
    /// `pFrame->Alert`: the caller raises it, which PHD2 also writes to the
    /// debug log at this point as `Alert: <text>`.
    Alert(String),
    /// `pFrame->StatusMsg`: the flip's status-bar line.
    Status(String),
    /// `MyFrame::HandleImageScaleChange` (`myframe.cpp:2837`) runs here. The
    /// mount's `calibrated` is already off, so the flip that may follow is
    /// refused (a rotator turn sets it again). The caller does the rest of
    /// `AdvancedDialog::MakeImageScaleAdjustments` — new calibration step,
    /// the other mount cleared, the calibration state machines reset,
    /// min-moves reset, with their debug lines — and clears the graphs. The
    /// alert that closes it is the next step.
    ImageScaleChange,
}

/// What [`adjust_for_scope_pointing`] or [`adjust_ao_for_scope_pointing`]
/// did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Adjustment {
    /// Debug lines, alerts, the status line and the image-scale change, in
    /// PHD2's order.
    pub steps: Vec<AdjustStep>,
    /// The calibration as the last `SetCalibration` (binning rescale, flip,
    /// rotator turn) left it, for the caller to store with a fresh
    /// timestamp; `None` when nothing was stored. Not always the final
    /// `state.cal`: the rotator-unknown branch changes the angle in memory
    /// only.
    pub stored: Option<Calibration>,
    /// The calibration was flipped to the other side of the pier.
    pub flipped: bool,
    /// The camera-reported pixel size to write into the profile, when it
    /// disagreed with the profile by ≥ 1 µm (`pCamera->SetCameraPixelSize`).
    pub new_pixel_size: Option<f64>,
}

/// `SetCalibration` inside an adjustment: its lines go in place, and what it
/// stores is remembered.
fn set_in_adjustment(state: &mut MountCalibration, cal: &Calibration, class: MountClass, out: &mut Adjustment) {
    out.steps.extend(state.set(cal, class).into_iter().map(AdjustStep::Log));
    out.stored = Some(state.cal.clone());
}

/// `Mount::AdjustCalibrationForScopePointing` (`mount.cpp:1253-1410`), run
/// on the scope when guiding starts: warns when the guide speeds or the
/// pixel size moved since calibration, rescales the rates for a binning
/// change, flips the calibration when the scope is on the other side of the
/// pier, turns the axes with the rotator, then compensates `state.x_rate`
/// for the declination (`cal.x_rate / cos(cal_dec) · cos(dec)` with `dec`
/// clamped to ±89°, only when the calibration was done within
/// [`DEC_COMP_LIMIT`] of the equator and `use_dec_comp` is on). `details`
/// are the stored calibration details, read for the guide speeds only.
pub fn adjust_for_scope_pointing(
    state: &mut MountCalibration,
    details: &CalibrationDetails,
    pointing: &ScopePointing,
    opts: &AdjustOpts,
) -> Adjustment {
    use AdjustStep::{Alert, Log};
    let mut out = Adjustment::default();
    let new_declination = pointing.declination.unwrap_or(UNKNOWN_DECLINATION);
    let new_rotator = pointing.rotator_angle.unwrap_or(ROTATOR_POSITION_UNKNOWN);
    let binning = pointing.binning;

    // Only the scope runs this one; the AO has its own.
    out.steps.push(Log(format!(
        "AdjustCalibrationForScopePointing (scope): current dec={} pierSide={}, cal dec={} pierSide={} rotAngle={} bin={}",
        declination_str(new_declination),
        pointing.pier_side as i32,
        declination_str(state.cal.declination),
        state.cal.pier_side as i32,
        rot_angle_str(new_rotator),
        binning
    )));

    // Guide speeds changed since calibration: an alert that can't be avoided.
    if let Some((cur_ra, cur_dec)) = pointing.guide_rates
        && details.ra_guide_speed > 0.0
        && details.dec_guide_speed > 0.0
        && ((1.0 - cur_ra / details.ra_guide_speed).abs() > 0.05
            || (1.0 - cur_dec / details.dec_guide_speed).abs() > 0.05)
    {
        out.steps.push(Alert(
            "Mount guide speeds are different from those used in last calibration.  Do a new calibration or reset mount guide speed settings to previous values. "
                .to_string(),
        ));
        out.steps.push(Log(format!(
            "Guide speeds have changed since calibration.  Orig RA = {}, Orig Dec = {}, Curr RA = {}, Curr Dec = {}, Units are arc-sec/sec",
            fmt_f(details.ra_guide_speed * 3600.0, 1),
            fmt_f(details.dec_guide_speed * 3600.0, 1),
            fmt_f(cur_ra * 3600.0, 1),
            fmt_f(cur_dec * 3600.0, 1)
        )));
    }

    if pointing.pier_side != PierSide::Unknown && state.cal.pier_side == PierSide::Unknown {
        out.steps.push(Alert(
            "Current calibration did not have side-of-pier information, so PHD2 can't automatically correct for meridian flips. You should do a fresh calibration to correct this problem."
                .to_string(),
        ));
    }

    // Binning or pixel-size change. A driver may lie about the binning while
    // changing the reported pixel size, so both are watched.
    let mut scale_adjustment = 1.0;
    if (pointing.camera_pixel_size - pointing.profile_pixel_size).abs() >= 1.0 {
        out.steps.push(Alert(
            "Profile pixel size doesn't match camera-reported pixel size.  Re-calibrate to restore correct guiding."
                .to_string(),
        ));
        out.steps.push(Log(format!(
            "Camera pixel size changed from {} to {}",
            fmt_f(pointing.profile_pixel_size, 1),
            fmt_f(pointing.camera_pixel_size, 1)
        )));
        scale_adjustment = pointing.camera_pixel_size / pointing.profile_pixel_size;
        out.new_pixel_size = Some(pointing.camera_pixel_size);
    }

    if binning != state.cal.binning && state.cal.is_valid {
        let old = &state.cal;
        // Quirk (mount.cpp:1319): `binning / m_cal.binning` is an unsigned
        // short division, so 2 -> 3 yields 1 and no image-scale change is
        // seen; the rates themselves use the double ratio. PHD2 would trap on
        // a stored binning of 0; a divisor of 1 stands in for it here.
        scale_adjustment *= (binning / old.binning.max(1)) as f64;
        let adj = old.binning as f64 / binning as f64;
        let cal = Calibration { x_rate: old.x_rate * adj, y_rate: old.y_rate * adj, binning, ..old.clone() };
        out.steps.push(Log(format!(
            "Binning {} -> {}, rates ({}, {}) -> ({}, {})",
            old.binning,
            binning,
            fmt_f(old.x_rate * 1000.0, 3),
            fmt_f(old.y_rate * 1000.0, 3),
            fmt_f(cal.x_rate * 1000.0, 3),
            fmt_f(cal.y_rate * 1000.0, 3)
        )));
        set_in_adjustment(state, &cal, MountClass::Scope, &mut out);
    }

    if (scale_adjustment - 1.0).abs() >= 0.01 {
        out.steps.push(Log("Mount::AdjustCalibrationForScopePointing: imageScaleRatio changed".to_string()));
        // HandleImageScaleChange clears every mount's calibration
        // (advanced_dialog.cpp:631) before the pier test below.
        out.steps.push(AdjustStep::ImageScaleChange);
        state.clear();
        out.steps.push(Alert(
            "Binning or camera pixel size changed unexpectedly. You should use separate profiles for different image scales."
                .to_string(),
        ));
    }

    if is_opposite_side(pointing.pier_side, state.cal.pier_side) {
        out.steps.push(Log(format!(
            "Guiding starts on opposite side of pier: calibration data side is {}, current side is {}",
            state.cal.pier_side.label(),
            pointing.pier_side.label()
        )));
        // Scope::IsCalibrated's Dec leg always holds here: guiding only
        // starts on a calibrated scope, and a binning rescale only moves
        // y_rate off the uncalibrated sentinel.
        let calibrated = state.is_calibrated(opts.connected);
        match flip_calibration(state, calibrated, opts.cal_flip_requires_dec_flip) {
            Ok(flip) => {
                out.steps.extend(flip.log.into_iter().map(Log));
                out.steps.push(AdjustStep::Status(flip.status));
                out.stored = Some(state.cal.clone());
                out.flipped = true;
            }
            Err(line) => out.steps.push(Log(line)),
        }
    }

    if let Some(new_rot) = pointing.rotator_angle {
        if state.cal.rotator_angle == ROTATOR_POSITION_UNKNOWN {
            // No rotator position at calibration time: nothing to adjust from.
            // The angle is remembered in memory only (not via SetCalibration).
            out.steps.push(Alert("Rotator position has changed, recalibration is needed.".to_string()));
            state.cal.rotator_angle = new_rot;
        } else {
            let da = new_rot - state.cal.rotator_angle;
            if da.abs() > 0.05 {
                out.steps.push(Log(format!(
                    "New rotator position {} deg, prev = {} deg, delta = {} deg",
                    fmt_f(new_rot, 1),
                    fmt_f(state.cal.rotator_angle, 1),
                    fmt_f(da, 1)
                )));
                let da = radians(da);
                let cal = Calibration {
                    x_angle: norm_angle(state.cal.x_angle - da),
                    y_angle: norm_angle(state.cal.y_angle - da),
                    rotator_angle: new_rot,
                    ..state.cal.clone()
                };
                // Calibrated again, even after an image-scale change cleared it.
                set_in_adjustment(state, &cal, MountClass::Scope, &mut out);
            }
        }
    }

    // Dec compensation, last, since it reads the rates the steps above may
    // have rewritten. The result is never stored.
    let mut deccomp = false;
    let cal_dec = state.cal.declination;
    if new_declination != cal_dec && new_declination != UNKNOWN_DECLINATION && cal_dec != UNKNOWN_DECLINATION {
        if cal_dec.abs() > DEC_COMP_LIMIT {
            out.steps.push(Log("skipping Dec comp: initial calibration too far from equator".to_string()));
            out.steps.push(Alert("Calibration was too far from equator, recalibration is needed.".to_string()));
        } else if !opts.use_dec_comp {
            out.steps.push(Log("skipping Dec comp: Dec Comp not enabled".to_string()));
        } else {
            // Near the pole the rate would blow up downstream: clamp to ±89°.
            // wxMax / wxMin, so a NaN declination comes through.
            let dec = wx_max(radians(-89.0), wx_min(radians(89.0), new_declination));
            state.x_rate = (state.cal.x_rate / cal_dec.cos()) * dec.cos();
            deccomp = true;
            out.steps.push(Log(format!(
                "Dec comp: XRate {} -> {} for dec {} -> dec {}",
                fmt_f(state.cal.x_rate * 1000.0, 3),
                fmt_f(state.x_rate * 1000.0, 3),
                fmt_f(degrees(cal_dec), 1),
                fmt_f(degrees(dec), 1)
            )));
        }
    }
    // Any SetCalibration above already reset x_rate to the stored rate.
    if !deccomp && state.x_rate != state.cal.x_rate {
        out.steps.push(Log(format!("No dec comp, asserted base xRate {}", fmt_f(state.cal.x_rate * 1000.0, 3))));
        state.x_rate = state.cal.x_rate;
    }
    out
}

/// `StepGuider::AdjustCalibrationForScopePointing` (`stepguider.cpp:1314`),
/// the AO's version: its calibration only follows the camera binning, since
/// declination, pier side and rotator leave alone an AO that turns with the
/// camera. AO rates are px per step, so this line has no ×1000 (the
/// `SetCalibration` lines still do). PHD2 reads the AO's working copy
/// (`m_calibration`), which is `state.cal` once a calibration is in use.
pub fn adjust_ao_for_scope_pointing(state: &mut MountCalibration, binning: u16) -> Adjustment {
    let mut out = Adjustment::default();
    if binning == state.cal.binning {
        out.steps.push(AdjustStep::Log("stepguider: scope pointing change, no change to calibration".to_string()));
    } else {
        let old = &state.cal;
        let adj = old.binning as f64 / binning as f64;
        let cal = Calibration { x_rate: old.x_rate * adj, y_rate: old.y_rate * adj, binning, ..old.clone() };
        out.steps.push(AdjustStep::Log(format!(
            "Stepguider Cal: Binning {} -> {}, rates ({}, {}) -> ({}, {})",
            old.binning,
            binning,
            fmt_f(old.x_rate, 3),
            fmt_f(old.y_rate, 3),
            fmt_f(cal.x_rate, 3),
            fmt_f(cal.y_rate, 3)
        )));
        set_in_adjustment(state, &cal, MountClass::StepGuider, &mut out);
    }
    out
}

/// `IsOppositeSide` (`mount.cpp:63`): East vs West only; Unknown matches nothing.
pub fn is_opposite_side(a: PierSide, b: PierSide) -> bool {
    (a == PierSide::East && b == PierSide::West) || (a == PierSide::West && b == PierSide::East)
}

/// `CAL_ALERT_MINSTEPS` (`scope.cpp:57`).
pub const CAL_ALERT_MINSTEPS: i32 = 4;
/// `CAL_ALERT_ORTHOGONALITY_TOLERANCE`, degrees.
pub const CAL_ALERT_ORTHOGONALITY_TOLERANCE: f64 = 12.5;
/// `CAL_ALERT_DECRATE_DIFFERENCE`, ratio.
pub const CAL_ALERT_DECRATE_DIFFERENCE: f64 = 0.20;
/// `CAL_ALERT_AXISRATES_TOLERANCE`, ratio.
pub const CAL_ALERT_AXISRATES_TOLERANCE: f64 = 0.20;

/// What [`sanity_check_calibration`] found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SanityResult {
    pub issue: CalibrationIssue,
    /// The `Calibration alert details: %s` text; empty when there is no issue.
    pub detail: String,
}

/// `Scope::SanityCheckCalibration` (`scope.cpp:868`): one verdict, in order
/// of confidence — too few steps (RA < 4, or 0 < Dec < 4: Dec may be off);
/// axes more than 12.5° from orthogonal; RA/Dec rates not in the `cos(dec)`
/// ratio by more than 0.2 (only with a known Dec within [`DEC_COMP_LIMIT`],
/// a Dec rate and Dec comp on, since a SiTech controller may scale the
/// rates); a Dec rate more than 20 % away from the previous calibration's,
/// only when the setup looks the same (image scale within 0.1″/px, RA angle
/// within 5°, both Dec rates measured). `old_cal.is_valid` false skips the
/// last test. The caller writes `last_issue` into the details
/// (`FlagCalibrationIssue`) and shows [`CalibrationIssue::advisory`].
pub fn sanity_check_calibration(
    new_cal: &Calibration,
    new_details: &CalibrationDetails,
    old_cal: &Calibration,
    old_details: &CalibrationDetails,
    use_dec_comp: bool,
) -> SanityResult {
    let mut res = SanityResult::default();
    let x_steps = new_details.ra_step_count;
    let y_steps = new_details.dec_step_count;

    if x_steps < CAL_ALERT_MINSTEPS || (y_steps < CAL_ALERT_MINSTEPS && y_steps > 0) {
        res.issue = CalibrationIssue::Steps;
        res.detail = format!("Actual RA calibration steps = {x_steps}, Dec calibration steps = {y_steps}");
        return res;
    }

    let non_ortho = ortho_error_deg(new_cal.x_angle, new_cal.y_angle);
    if non_ortho > CAL_ALERT_ORTHOGONALITY_TOLERANCE {
        res.issue = CalibrationIssue::Angle;
        res.detail = format!("Non-orthogonality = {}", fmt_f(non_ortho, 3));
    } else if new_cal.declination != UNKNOWN_DECLINATION
        && new_cal.y_rate != CALIBRATION_RATE_UNCALIBRATED
        && new_cal.declination.abs() <= DEC_COMP_LIMIT
        && use_dec_comp
    {
        let expected_ratio = new_cal.declination.cos();
        // Mounts may guide at different speeds on the two axes.
        let speed_ratio = if new_details.ra_guide_speed > 0.0 {
            new_details.dec_guide_speed / new_details.ra_guide_speed
        } else {
            1.0
        };
        let actual_ratio = new_cal.x_rate * speed_ratio / new_cal.y_rate;
        if (expected_ratio - actual_ratio).abs() > CAL_ALERT_AXISRATES_TOLERANCE {
            res.issue = CalibrationIssue::Rates;
            res.detail = format!(
                "Expected ratio at dec={} is {}, actual is {}",
                fmt_f(degrees(new_cal.declination), 1),
                fmt_f(expected_ratio, 3),
                fmt_f(actual_ratio, 3)
            );
        }
    }

    // The "old" values went through the profile store, so no exact compares.
    if res.issue == CalibrationIssue::None
        && old_cal.is_valid
        && (old_details.image_scale - new_details.image_scale).abs() < 0.1
        && degrees(old_cal.x_angle - new_cal.x_angle).abs() < 5.0
        && new_cal.y_rate != CALIBRATION_RATE_UNCALIBRATED
        && old_cal.y_rate != CALIBRATION_RATE_UNCALIBRATED
        && new_cal.y_rate != 0.0
    {
        let ratio = old_cal.y_rate / new_cal.y_rate;
        if (1.0 - ratio).abs() > CAL_ALERT_DECRATE_DIFFERENCE {
            res.issue = CalibrationIssue::Different;
            res.detail = format!("Current/previous Dec rate ratio is {}", fmt_f(ratio, 3));
        }
    }
    res
}

/// `CalstepDialog::DEFAULT_STEPS` (`calstep_dialog.h:76`).
pub const CALSTEP_DEFAULT_STEPS: i32 = 12;
/// `CalstepDialog::DEFAULT_DISTANCE`, px.
pub const CALSTEP_DEFAULT_DISTANCE: i32 = 25;
/// `MIN_STEPS` (`calstep_dialog.cpp:45`): the pulse is capped so Dec, at
/// `cos(dec)` of the RA travel per pulse, still gets this many steps.
pub const CALSTEP_MIN_STEPS: f64 = 6.0;

/// `MyFrame::GetPixelScale` (`myframe.h:731`): arcsec per (binned) pixel.
pub fn pixel_scale(pixel_um: f64, focal_mm: i32, binning: u16) -> f64 {
    206.265 * pixel_um * binning as f64 / focal_mm as f64
}

/// `CalstepDialog::GetCalibrationDistance` (`calstep_dialog.cpp:208`): 20″
/// or 25 px, whichever is more.
pub fn calibration_distance(focal_mm: i32, pixel_um: f64, binning: u16) -> i32 {
    let scale = pixel_scale(pixel_um, focal_mm, binning);
    let nominal_distance_arcsecs = 20.0;
    let min_distance_px = CALSTEP_DEFAULT_DISTANCE as f64;
    min_distance_px.max(nominal_distance_arcsecs / scale).ceil() as i32
}

/// `CalstepDialog::GetCalibrationStepSize` (`calstep_dialog.cpp:230`): the
/// pulse, ms, that covers `distance` px in `desired_steps` steps at this
/// declination. `guide_speed` is a fraction of sidereal (15″/s); the pulse
/// grows by `1/cos(dec)` but never past the length that gives
/// [`CALSTEP_MIN_STEPS`] at the equator, and rounds up to 50 ms. Returns
/// `(image_scale, step_ms)`.
pub fn calibration_step_size(
    focal_mm: i32,
    pixel_um: f64,
    binning: u16,
    guide_speed: f64,
    desired_steps: i32,
    declination_deg: f64,
    distance: i32,
) -> (f64, i32) {
    let image_scale = pixel_scale(pixel_um, focal_mm, binning);
    let total_duration = distance as f64 * image_scale / (15.0 * guide_speed);
    let pulse = total_duration / desired_steps as f64 * 1000.0;
    let max_pulse = total_duration / CALSTEP_MIN_STEPS * 1000.0;
    let pulse = wx_min(max_pulse, pulse / radians(declination_deg).cos());
    // Too-small pulses lead to calibration problems: round up, not to nearest.
    let step = (pulse / 50.0).ceil() as i32 * 50;
    (image_scale, step)
}

/// The calibration-step calculator as the dialog runs it: the recommended
/// distance for this scale, then the pulse for it. Returns
/// `(distance_px, pulse_ms)`.
pub fn calstep(pixel_um: f64, binning: u16, focal_mm: i32, guide_speed: f64, dec_deg: f64, steps: i32) -> (i32, i32) {
    let distance = calibration_distance(focal_mm, pixel_um, binning);
    let (_, pulse) = calibration_step_size(focal_mm, pixel_um, binning, guide_speed, steps, dec_deg, distance);
    (distance, pulse)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    fn cal(x_deg: f64, y_deg: f64) -> Calibration {
        Calibration {
            x_rate: 0.01,
            y_rate: 0.0106,
            x_angle: radians(x_deg),
            y_angle: radians(y_deg),
            declination: radians(20.0),
            rotator_angle: ROTATOR_POSITION_UNKNOWN,
            binning: 1,
            pier_side: PierSide::East,
            ra_guide_parity: Parity::Even,
            dec_guide_parity: Parity::Even,
            is_valid: true,
            timestamp: "t".into(),
        }
    }

    fn details(ra: i32, dec: i32) -> CalibrationDetails {
        CalibrationDetails {
            focal_length: 400,
            image_scale: 1.93,
            ra_guide_speed: 0.5 * 15.0 / 3600.0,
            dec_guide_speed: 0.5 * 15.0 / 3600.0,
            ortho_error: 0.0,
            orig_binning: 1.0,
            ra_step_count: ra,
            dec_step_count: dec,
            ..Default::default()
        }
    }

    fn pointing() -> ScopePointing {
        ScopePointing {
            declination: Some(radians(20.0)),
            pier_side: PierSide::East,
            rotator_angle: None,
            binning: 1,
            guide_rates: None,
            camera_pixel_size: 3.75,
            profile_pixel_size: 3.75,
        }
    }

    const OPTS: AdjustOpts = AdjustOpts { use_dec_comp: true, cal_flip_requires_dec_flip: false, connected: true };

    /// A scope that took `c` through SetCalibration, as at guide start.
    fn mounted(c: Calibration) -> MountCalibration {
        let mut m = MountCalibration::default();
        m.set(&c, MountClass::Scope);
        m
    }

    /// The steps spelled the way the C++ harness printed PHD2's run.
    fn render(a: &Adjustment) -> Vec<String> {
        a.steps
            .iter()
            .map(|s| match s {
                AdjustStep::Log(l) => format!("LOG: {l}"),
                AdjustStep::Alert(t) => format!("ALERT: {t}"),
                AdjustStep::Status(t) => format!("STATUS: {t}"),
                AdjustStep::ImageScaleChange => "IMAGESCALECHANGE".to_string(),
            })
            .collect()
    }

    fn logs(a: &Adjustment) -> Vec<&str> {
        a.steps.iter().filter_map(|s| if let AdjustStep::Log(l) = s { Some(l.as_str()) } else { None }).collect()
    }

    fn alerts(a: &Adjustment) -> Vec<&str> {
        a.steps.iter().filter_map(|s| if let AdjustStep::Alert(t) = s { Some(t.as_str()) } else { None }).collect()
    }

    #[test]
    fn norm_angle_wraps_into_minus_pi_pi() {
        assert!(near(norm_angle(PI), -PI, 1e-12)); // the top end is exclusive
        assert!(near(norm_angle(-PI), -PI, 1e-12));
        assert!(near(norm_angle(3.0 * PI), -PI, 1e-12));
        assert!(near(norm_angle(radians(195.0)), -radians(165.0), 1e-12));
        assert!(near(norm(25.0, 0.0, 24.0), 1.0, 1e-12));
    }

    #[test]
    fn degrees_and_radians_in_phd2_order() {
        // From PHD2's C++ (image_math.h): `d * M_PI / 180.` is 1 ulp above
        // Rust's `to_radians` at 89°.
        assert_eq!(radians(89.0), 1.5533430342749535);
        assert_eq!(89.0f64.to_radians(), 1.5533430342749532);
        assert_eq!(radians(20.0), 0.3490658503988659);
        assert_eq!(degrees(1.0), 57.295779513082323);
        assert_eq!(ortho_error_deg(0.0, radians(250.0)), 20.000000000000028);
    }

    #[test]
    fn y_angle_error_normalisation() {
        // Orthogonal axes: no error. Reversed Dec axis: -π (never +π).
        assert!(near(cal(10.0, 100.0).y_angle_error(), 0.0, 1e-12));
        assert!(near(cal(15.0, -75.0).y_angle_error(), -PI, 1e-12));
        assert!(near(cal(0.0, 270.0).y_angle_error(), -PI, 1e-12));
        assert!(near(cal(170.0, -100.0).y_angle_error(), 0.0, 1e-12)); // 360° wraps to 0
        assert!(near(degrees(cal(15.0, 92.0).y_angle_error()), 13.0, 1e-9));
    }

    #[test]
    fn y_angle_reads_back_normalised() {
        // A stored 270° reads as -90°; an in-range angle reads as itself.
        assert!(near(cal(15.0, 270.0).y_angle(), -PI / 2.0, 1e-15));
        assert!(near(cal(15.0, 105.0).y_angle(), radians(105.0), 1e-15));
        assert!(near(cal(-170.0, -260.0).y_angle(), radians(100.0), 1e-12));
    }

    #[test]
    fn transforms_round_trip_orthogonal() {
        // 15° camera angle, Dec axis 90° on: an exact round trip.
        let c = cal(15.0, 105.0);
        let m = camera_to_mount(Point::new(10.0, 0.0), &c);
        assert!(near(m.x, 9.659258262890683, 1e-9));
        assert!(near(m.y, -2.588190451025205, 1e-9));
        let back = mount_to_camera(m, &c);
        assert!(near(back.x, 10.0, 1e-9) && near(back.y, 0.0, 1e-9));
        let m = camera_to_mount(Point::new(3.0, -4.0), &c);
        assert!(near(m.x, 1.8625012984571219, 1e-9) && near(m.y, -4.640160440463835, 1e-9));
        let back = mount_to_camera(m, &c);
        assert!(near(back.x, 3.0, 1e-9) && near(back.y, -4.0, 1e-9));
    }

    #[test]
    fn transforms_non_orthogonal_92_degree_y_axis() {
        // Dec axis at 92° absolute, 13° off orthogonal: the forward transform
        // projects Dec on the axis at 2·15 − 92 + 180 = 118° (cos(−118°)·10 =
        // −4.6947), not on the measured 92° (which would give −0.349), and the
        // reverse cannot undo it exactly (mount.cpp:1096).
        let c = cal(15.0, 92.0);
        let m = camera_to_mount(Point::new(10.0, 0.0), &c);
        assert!(near(m.x, 9.659258262890683, 1e-9));
        assert!(near(m.y, -4.694715627858907, 1e-9));
        let back = mount_to_camera(m, &c);
        assert!(near(back.x, 10.545208834752517, 1e-9));
        assert!(near(back.y, -2.0347470720318164, 1e-9));
    }

    #[test]
    fn transforms_reversed_dec_axis_mirrors_theta() {
        // Dec axis at x − 90°: |y_angle_error| = π > π/2, so the reverse
        // transform negates the mount angle and the round trip holds.
        let c = cal(15.0, -75.0);
        let m = camera_to_mount(Point::new(10.0, 0.0), &c);
        assert!(near(m.x, 9.659258262890683, 1e-9) && near(m.y, 2.58819045102521, 1e-9));
        let back = mount_to_camera(m, &c);
        assert!(near(back.x, 10.0, 1e-9) && near(back.y, 0.0, 1e-9));
        let m = camera_to_mount(Point::new(3.0, -4.0), &c);
        let back = mount_to_camera(m, &c);
        assert!(near(back.x, 3.0, 1e-9) && near(back.y, -4.0, 1e-9));
    }

    #[test]
    fn set_and_clear_keep_valid_apart_from_calibrated() {
        let mut m = MountCalibration::default();
        assert!(!m.cal.is_valid && !m.calibrated);
        let log = m.set(&cal(15.0, 270.0), MountClass::Scope);
        // From PHD2's C++: the raw yAngle in the first line.
        assert_eq!(
            log,
            [
                "Mount::SetCalibration (scope) -- xAngle=15.0 yAngle=270.0 xRate=10.000 yRate=10.600 bin=1 dec=20.0 pierSide=0 par=+/+ rotAng=None",
                "Mount::SetCalibration (scope) -- sets m_xAngle=15.0 m_yAngleError=-165.0",
            ]
        );
        assert!(m.cal.is_valid && m.calibrated && m.is_calibrated(true) && !m.is_calibrated(false));
        assert_eq!(m.x_rate, 0.01);
        assert_eq!(m.cal.timestamp, ""); // only the stored copy is stamped
        assert_eq!((m.ra_parity(), m.dec_parity(), m.declination()), (Parity::Even, Parity::Even, radians(20.0)));
        // ClearCalibration: still valid, no longer calibrated.
        m.clear();
        assert!(m.cal.is_valid && !m.calibrated && !m.is_calibrated(true));
        assert_eq!((m.ra_parity(), m.dec_parity(), m.declination()), (Parity::Unknown, Parity::Unknown, UNKNOWN_DECLINATION));
    }

    #[test]
    fn flip_without_dec_flip_inverts_parity() {
        let mut m = mounted(cal(15.0, 105.0));
        m.x_rate = 0.007; // Dec-compensated while guiding
        let r = flip_calibration(&mut m, true, false).unwrap();
        // From PHD2's C++.
        assert_eq!(
            r.log,
            [
                "FlipCalibration before: x=15.0, y=105.0 decFlipRequired=0 sideOfPier=East rotAngle=None parity=+/+",
                "FlipCalibration pre-normalize: x=195.0, y=105.0",
                "FlipCalibration after: x=-165.0 y=105.0 sideOfPier=West parity=+/-",
                "Mount::SetCalibration (scope) -- xAngle=-165.0 yAngle=105.0 xRate=10.000 yRate=10.600 bin=1 dec=20.0 pierSide=1 par=+/- rotAng=None",
                "Mount::SetCalibration (scope) -- sets m_xAngle=-165.0 m_yAngleError=-180.0",
            ]
        );
        assert_eq!(r.status, "CAL: East(15,105)->West(-165,105)");
        assert_eq!(m.cal.x_angle, -2.8797932657906435);
        assert_eq!(m.cal.y_angle, 1.8325957145940461);
        assert_eq!(m.cal.pier_side, PierSide::West);
        assert_eq!((m.cal.ra_guide_parity, m.cal.dec_guide_parity), (Parity::Even, Parity::Odd));
        assert_eq!(m.x_rate, 0.01); // the flip's SetCalibration drops the Dec compensation
    }

    #[test]
    fn flip_with_dec_flip_turns_y_and_keeps_parity() {
        let mut m = mounted(cal(15.0, 105.0));
        flip_calibration(&mut m, true, true).unwrap();
        assert!(near(degrees(m.cal.x_angle), -165.0, 1e-9));
        assert!(near(degrees(m.cal.y_angle), -75.0, 1e-9));
        assert_eq!(m.cal.dec_guide_parity, Parity::Even);
        assert_eq!(m.cal.pier_side, PierSide::West);
        // Flipping back restores the angles.
        flip_calibration(&mut m, true, true).unwrap();
        assert!(near(degrees(m.cal.x_angle), 15.0, 1e-9) && near(degrees(m.cal.y_angle), 105.0, 1e-9));
        assert_eq!(m.cal.pier_side, PierSide::East);
    }

    #[test]
    fn flip_refused_unless_calibrated() {
        // From PHD2's C++: a cleared calibration stays valid but does not flip.
        let mut m = mounted(cal(15.0, 105.0));
        m.clear();
        let before = m.clone();
        let calibrated = m.is_calibrated(true);
        let err = flip_calibration(&mut m, calibrated, false).unwrap_err();
        assert_eq!(err, "Error thrown from mount.cpp:899->cannot flip if not calibrated");
        assert_eq!(m, before);
        assert!(m.cal.is_valid);
    }

    #[test]
    fn flip_reads_the_normalised_y_angle() {
        // From PHD2's C++: a stored 270° Dec axis flips as -90°.
        let mut m = mounted(cal(15.0, 270.0));
        let r = flip_calibration(&mut m, true, false).unwrap();
        assert_eq!(
            r.log,
            [
                "FlipCalibration before: x=15.0, y=-90.0 decFlipRequired=0 sideOfPier=East rotAngle=None parity=+/+",
                "FlipCalibration pre-normalize: x=195.0, y=-90.0",
                "FlipCalibration after: x=-165.0 y=-90.0 sideOfPier=West parity=+/-",
                "Mount::SetCalibration (scope) -- xAngle=-165.0 yAngle=-90.0 xRate=10.000 yRate=10.600 bin=1 dec=20.0 pierSide=1 par=+/- rotAng=None",
                "Mount::SetCalibration (scope) -- sets m_xAngle=-165.0 m_yAngleError=15.0",
            ]
        );
        assert_eq!(r.status, "CAL: East(15,-90)->West(-165,-90)");
        assert_eq!(m.cal.x_angle, -2.8797932657906435);
        assert_eq!(m.cal.y_angle, -1.5707963267948966);
    }

    #[test]
    fn dec_comp_at_cal_dec_20_pointing_60() {
        let mut m = mounted(cal(15.0, 105.0));
        let p = ScopePointing { declination: Some(radians(60.0)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert_eq!(m.x_rate, 0.0053208888623795612); // PHD2's C++
        assert_eq!(m.cal.x_rate, 0.01); // the stored rate is untouched
        assert!(!a.flipped && a.stored.is_none() && alerts(&a).is_empty());
        assert_eq!(
            render(&a),
            [
                "LOG: AdjustCalibrationForScopePointing (scope): current dec=60.0 pierSide=0, cal dec=20.0 pierSide=0 rotAngle=None bin=1",
                "LOG: Dec comp: XRate 10.000 -> 5.321 for dec 20.0 -> dec 60.0",
            ]
        );
    }

    #[test]
    fn dec_comp_clamps_at_89_and_skips_when_off_or_far() {
        let mut m = mounted(cal(15.0, 105.0));
        let p = ScopePointing { declination: Some(radians(89.5)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert_eq!(m.x_rate, 0.00018572463006772492); // PHD2's C++, radians(89)
        assert_eq!(logs(&a)[1], "Dec comp: XRate 10.000 -> 0.186 for dec 20.0 -> dec 89.0");

        // A NaN declination goes through wxMax / wxMin, as in PHD2's C++.
        let p_nan = ScopePointing { declination: Some(f64::NAN), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p_nan, &OPTS);
        assert!(m.x_rate.is_nan());
        assert_eq!(
            logs(&a),
            [
                "AdjustCalibrationForScopePointing (scope): current dec=nan pierSide=0, cal dec=20.0 pierSide=0 rotAngle=None bin=1",
                "Dec comp: XRate 10.000 -> nan for dec 20.0 -> dec nan",
            ]
        );

        // Dec comp off: base rate, plus the "asserted" line when it differed.
        let off = AdjustOpts { use_dec_comp: false, ..OPTS };
        m.x_rate = 0.005;
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &off);
        assert_eq!(m.x_rate, 0.01);
        assert_eq!(logs(&a)[1..], ["skipping Dec comp: Dec Comp not enabled", "No dec comp, asserted base xRate 10.000"]);

        // Calibrated past 60°: an alert, no compensation.
        let mut far = mounted(Calibration { declination: radians(65.0), ..cal(15.0, 105.0) });
        let p = ScopePointing { declination: Some(radians(10.0)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut far, &details(8, 8), &p, &OPTS);
        assert_eq!(far.x_rate, 0.01);
        assert_eq!(alerts(&a), ["Calibration was too far from equator, recalibration is needed."]);

        // Same declination as the calibration, or unknown: nothing happens.
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &pointing(), &OPTS);
        assert_eq!(m.x_rate, 0.01);
        assert_eq!(a.steps.len(), 1);
        let p = ScopePointing { declination: None, ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert_eq!(m.x_rate, 0.01);
        assert_eq!(
            logs(&a),
            ["AdjustCalibrationForScopePointing (scope): current dec=Unknown pierSide=0, cal dec=20.0 pierSide=0 rotAngle=None bin=1"]
        );
    }

    #[test]
    fn adjust_flips_on_opposite_pier_and_turns_with_rotator() {
        let mut m = mounted(Calibration { rotator_angle: 30.0, ..cal(15.0, 105.0) });
        let p = ScopePointing { pier_side: PierSide::West, rotator_angle: Some(40.0), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        // From PHD2's C++: the flip, then the 10° rotator delta subtracted.
        assert_eq!(
            render(&a),
            [
                "LOG: AdjustCalibrationForScopePointing (scope): current dec=20.0 pierSide=1, cal dec=20.0 pierSide=0 rotAngle=40.0 bin=1",
                "LOG: Guiding starts on opposite side of pier: calibration data side is East, current side is West",
                "LOG: FlipCalibration before: x=15.0, y=105.0 decFlipRequired=0 sideOfPier=East rotAngle=30.0 parity=+/+",
                "LOG: FlipCalibration pre-normalize: x=195.0, y=105.0",
                "LOG: FlipCalibration after: x=-165.0 y=105.0 sideOfPier=West parity=+/-",
                "LOG: Mount::SetCalibration (scope) -- xAngle=-165.0 yAngle=105.0 xRate=10.000 yRate=10.600 bin=1 dec=20.0 pierSide=1 par=+/- rotAng=30.0",
                "LOG: Mount::SetCalibration (scope) -- sets m_xAngle=-165.0 m_yAngleError=-180.0",
                "STATUS: CAL: East(15,105)->West(-165,105)",
                "LOG: New rotator position 40.0 deg, prev = 30.0 deg, delta = 10.0 deg",
                "LOG: Mount::SetCalibration (scope) -- xAngle=-175.0 yAngle=95.0 xRate=10.000 yRate=10.600 bin=1 dec=20.0 pierSide=1 par=+/- rotAng=40.0",
                "LOG: Mount::SetCalibration (scope) -- sets m_xAngle=-175.0 m_yAngleError=-180.0",
            ]
        );
        assert!(a.flipped);
        let stored = a.stored.unwrap();
        assert_eq!((stored.x_angle, stored.y_angle, stored.rotator_angle), (-3.0543261909900763, 1.6580627893946132, 40.0));
        assert_eq!((stored.pier_side, stored.dec_guide_parity), (PierSide::West, Parity::Odd));
        assert_eq!(m.cal, Calibration { timestamp: String::new(), ..stored });

        // A disconnected scope (behind an AO) refuses the flip.
        let mut m = mounted(cal(15.0, 105.0));
        let p = ScopePointing { pier_side: PierSide::West, ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &AdjustOpts { connected: false, ..OPTS });
        assert!(!a.flipped && a.stored.is_none());
        assert_eq!(logs(&a)[2], "Error thrown from mount.cpp:899->cannot flip if not calibrated");

        // Rotator unknown at calibration: an alert, the angle remembered.
        let mut m = mounted(cal(15.0, 105.0));
        let p = ScopePointing { rotator_angle: Some(40.0), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert_eq!(alerts(&a), ["Rotator position has changed, recalibration is needed."]);
        assert_eq!(m.cal.rotator_angle, 40.0);
        assert!(a.stored.is_none());

        // Unknown pier side in the calibration: an alert, no flip.
        let mut m = mounted(Calibration { pier_side: PierSide::Unknown, ..cal(15.0, 105.0) });
        let p = ScopePointing { pier_side: PierSide::West, ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert!(!a.flipped);
        assert!(alerts(&a)[0].starts_with("Current calibration did not have side-of-pier information"));
    }

    #[test]
    fn adjust_image_scale_change_refuses_the_flip() {
        // From PHD2's C++: HandleImageScaleChange clears the calibration, so
        // the flip is refused; the rotator's SetCalibration calibrates again.
        let mut m = mounted(Calibration { rotator_angle: 30.0, ..cal(15.0, 105.0) });
        let p = ScopePointing { binning: 2, pier_side: PierSide::West, rotator_angle: Some(40.0), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert_eq!(
            render(&a),
            [
                "LOG: AdjustCalibrationForScopePointing (scope): current dec=20.0 pierSide=1, cal dec=20.0 pierSide=0 rotAngle=40.0 bin=2",
                "LOG: Binning 1 -> 2, rates (10.000, 10.600) -> (5.000, 5.300)",
                "LOG: Mount::SetCalibration (scope) -- xAngle=15.0 yAngle=105.0 xRate=5.000 yRate=5.300 bin=2 dec=20.0 pierSide=0 par=+/+ rotAng=30.0",
                "LOG: Mount::SetCalibration (scope) -- sets m_xAngle=15.0 m_yAngleError=-0.0",
                "LOG: Mount::AdjustCalibrationForScopePointing: imageScaleRatio changed",
                "IMAGESCALECHANGE",
                "ALERT: Binning or camera pixel size changed unexpectedly. You should use separate profiles for different image scales.",
                "LOG: Guiding starts on opposite side of pier: calibration data side is East, current side is West",
                "LOG: Error thrown from mount.cpp:899->cannot flip if not calibrated",
                "LOG: New rotator position 40.0 deg, prev = 30.0 deg, delta = 10.0 deg",
                "LOG: Mount::SetCalibration (scope) -- xAngle=5.0 yAngle=95.0 xRate=5.000 yRate=5.300 bin=2 dec=20.0 pierSide=0 par=+/+ rotAng=40.0",
                "LOG: Mount::SetCalibration (scope) -- sets m_xAngle=5.0 m_yAngleError=-0.0",
            ]
        );
        assert!(!a.flipped && m.calibrated);
        let stored = a.stored.unwrap();
        assert_eq!((stored.x_angle, stored.y_angle, stored.binning), (0.08726646259971646, 1.6580627893946132, 2));
        assert_eq!(stored.pier_side, PierSide::East);

        // Without a rotator turn the mount stays cleared, and the rate reset
        // by the binning's SetCalibration logs no "asserted" line.
        let mut m = mounted(cal(15.0, 105.0));
        m.x_rate = 0.007;
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &ScopePointing { binning: 2, ..pointing() }, &OPTS);
        assert_eq!(render(&a).len(), 7);
        assert_eq!(render(&a)[6], "ALERT: Binning or camera pixel size changed unexpectedly. You should use separate profiles for different image scales.");
        assert_eq!(m.x_rate, 0.005);
        assert!(!m.calibrated && m.cal.is_valid);

        // An unknown rotator at calibration: the stored copy keeps -888, the
        // one in memory takes the new angle.
        let mut m = mounted(cal(15.0, 105.0));
        let p = ScopePointing { binning: 2, rotator_angle: Some(40.0), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert_eq!(a.stored.as_ref().unwrap().rotator_angle, ROTATOR_POSITION_UNKNOWN);
        assert_eq!(m.cal.rotator_angle, 40.0);
        assert_eq!(render(&a).last().unwrap(), "ALERT: Rotator position has changed, recalibration is needed.");
    }

    #[test]
    fn adjust_binning_and_guide_speed_changes() {
        // Binning 1 → 2 halves the rates and changes the image scale.
        let mut m = mounted(cal(15.0, 105.0));
        let p = ScopePointing { binning: 2, ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert!(near(m.cal.x_rate, 0.005, 1e-15) && near(m.cal.y_rate, 0.0053, 1e-15));
        assert_eq!(m.cal.binning, 2);
        assert_eq!(m.x_rate, 0.005);
        assert!(a.stored.is_some() && a.steps.contains(&AdjustStep::ImageScaleChange));
        assert_eq!(logs(&a)[1], "Binning 1 -> 2, rates (10.000, 10.600) -> (5.000, 5.300)");
        assert_eq!(alerts(&a).len(), 1);

        // Quirk: 2 → 3 is an integer ratio of 1, so no image-scale change.
        let mut m = mounted(Calibration { binning: 2, ..cal(15.0, 105.0) });
        let p = ScopePointing { binning: 3, ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &details(8, 8), &p, &OPTS);
        assert!(!a.steps.contains(&AdjustStep::ImageScaleChange));
        assert!(near(m.cal.x_rate, 0.01 * 2.0 / 3.0, 1e-15));

        // Guide speed 20 % off: the alert, speeds printed in arcsec/s.
        let mut m = mounted(cal(15.0, 105.0));
        let d = details(8, 8);
        let p = ScopePointing { guide_rates: Some((d.ra_guide_speed * 1.2, d.dec_guide_speed)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &d, &p, &OPTS);
        assert!(alerts(&a)[0].starts_with("Mount guide speeds are different"));
        assert_eq!(
            logs(&a)[1],
            "Guide speeds have changed since calibration.  Orig RA = 7.5, Orig Dec = 7.5, Curr RA = 9.0, Curr Dec = 7.5, Units are arc-sec/sec"
        );
        // Within 5 %: nothing.
        let p = ScopePointing { guide_rates: Some((d.ra_guide_speed * 1.04, d.dec_guide_speed)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &d, &p, &OPTS);
        assert!(alerts(&a).is_empty());

        // Pixel size 1 µm off: the profile is updated, the scale changed.
        let p = ScopePointing { camera_pixel_size: 4.8, ..pointing() };
        let a = adjust_for_scope_pointing(&mut m, &d, &p, &OPTS);
        assert_eq!(a.new_pixel_size, Some(4.8));
        assert!(a.steps.contains(&AdjustStep::ImageScaleChange));
        assert_eq!(logs(&a)[1], "Camera pixel size changed from 3.8 to 4.8");
    }

    #[test]
    fn ao_follows_only_the_binning() {
        let c = Calibration {
            x_rate: 1.25,
            y_rate: 1.5,
            declination: UNKNOWN_DECLINATION,
            pier_side: PierSide::Unknown,
            ra_guide_parity: Parity::Unknown,
            dec_guide_parity: Parity::Unknown,
            ..cal(15.0, 105.0)
        };
        let mut m = MountCalibration::default();
        m.set(&c, MountClass::StepGuider);
        // From PHD2's C++: no ×1000 in the AO's own line, ×1000 in SetCalibration's.
        let a = adjust_ao_for_scope_pointing(&mut m, 2);
        assert_eq!(
            render(&a),
            [
                "LOG: Stepguider Cal: Binning 1 -> 2, rates (1.250, 1.500) -> (0.625, 0.750)",
                "LOG: Mount::SetCalibration (stepguider) -- xAngle=15.0 yAngle=105.0 xRate=625.000 yRate=750.000 bin=2 dec=Unknown pierSide=-1 par=?/? rotAng=None",
                "LOG: Mount::SetCalibration (stepguider) -- sets m_xAngle=15.0 m_yAngleError=-0.0",
            ]
        );
        assert_eq!(a.stored.map(|s| (s.x_rate, s.y_rate, s.binning)), Some((0.625, 0.75, 2)));
        let a = adjust_ao_for_scope_pointing(&mut m, 2);
        assert_eq!(render(&a), ["LOG: stepguider: scope pointing change, no change to calibration"]);
        assert!(a.stored.is_none());
    }

    #[test]
    fn sanity_each_issue() {
        let old = cal(15.0, 105.0);
        let od = details(8, 8);

        // Few steps: RA < 4, or Dec in 1..3; Dec 0 (Dec off) is fine.
        let r = sanity_check_calibration(&cal(15.0, 105.0), &details(3, 8), &old, &od, true);
        assert_eq!(r.issue, CalibrationIssue::Steps);
        assert_eq!(r.detail, "Actual RA calibration steps = 3, Dec calibration steps = 8");
        assert_eq!(sanity_check_calibration(&cal(15.0, 105.0), &details(8, 2), &old, &od, true).issue, CalibrationIssue::Steps);
        assert_eq!(sanity_check_calibration(&cal(15.0, 105.0), &details(8, 0), &old, &od, true).issue, CalibrationIssue::None);

        // 13° off orthogonal.
        let r = sanity_check_calibration(&cal(15.0, 92.0), &details(8, 8), &old, &od, true);
        assert_eq!(r.issue, CalibrationIssue::Angle);
        assert_eq!(r.detail, "Non-orthogonality = 13.000");
        // 12° passes; a reversed Dec axis (x − 90°) is orthogonal.
        assert_eq!(sanity_check_calibration(&cal(15.0, 93.0), &details(8, 8), &old, &od, true).issue, CalibrationIssue::None);
        assert_eq!(sanity_check_calibration(&cal(15.0, -75.0), &details(8, 8), &old, &od, true).issue, CalibrationIssue::None);

        // Rates: expected cos(20°) = 0.940, actual x/y = 0.01/0.02 = 0.5.
        let c = Calibration { y_rate: 0.02, ..cal(15.0, 105.0) };
        let r = sanity_check_calibration(&c, &details(8, 8), &old, &od, true);
        assert_eq!(r.issue, CalibrationIssue::Rates);
        assert_eq!(r.detail, "Expected ratio at dec=20.0 is 0.940, actual is 0.500");
        // Not checked with Dec comp off, past 60° or an unknown Dec: the
        // verdict falls through to the Dec-rate difference (0.0106/0.02).
        let r = sanity_check_calibration(&c, &details(8, 8), &old, &od, false);
        assert_eq!(r.issue, CalibrationIssue::Different);
        assert_eq!(r.detail, "Current/previous Dec rate ratio is 0.530");
        let far = Calibration { declination: radians(65.0), ..c.clone() };
        assert_eq!(sanity_check_calibration(&far, &details(8, 8), &old, &od, true).issue, CalibrationIssue::Different);
        let unk = Calibration { declination: UNKNOWN_DECLINATION, ..c.clone() };
        assert_eq!(sanity_check_calibration(&unk, &details(8, 8), &old, &od, true).issue, CalibrationIssue::Different);
        // ... and to None when the previous Dec rate is the same.
        let same = Calibration { y_rate: 0.02, ..old.clone() };
        assert_eq!(sanity_check_calibration(&c, &details(8, 8), &same, &od, false).issue, CalibrationIssue::None);
        // The Dec guide speed scales the expected ratio: at half-speed Dec the
        // ratio of c (0.25) is still off, while c2, whose Dec rate halved too,
        // passes.
        let d = CalibrationDetails { dec_guide_speed: od.ra_guide_speed * 0.5, ..details(8, 8) };
        assert_eq!(sanity_check_calibration(&c, &d, &old, &od, true).issue, CalibrationIssue::Rates);
        let c2 = Calibration { y_rate: 0.0053, ..cal(15.0, 105.0) };
        let old2 = Calibration { y_rate: 0.0053, ..old.clone() };
        assert_eq!(sanity_check_calibration(&c2, &d, &old2, &od, true).issue, CalibrationIssue::None);

        // Different: old Dec rate 0.0106 vs new 0.0134 → ratio 0.791, while
        // 0.01/0.0134 = 0.746 still passes the rate check (0.940 − 0.746 < 0.2).
        let c = Calibration { y_rate: 0.0134, ..cal(15.0, 105.0) };
        let r = sanity_check_calibration(&c, &details(8, 8), &old, &od, true);
        assert_eq!(r.issue, CalibrationIssue::Different);
        assert_eq!(r.detail, "Current/previous Dec rate ratio is 0.791");
        // Skipped when the old one is invalid, the scale moved, the RA angle
        // moved ≥ 5°, or either Dec rate is uncalibrated.
        let invalid = Calibration { is_valid: false, ..old.clone() };
        assert_eq!(sanity_check_calibration(&c, &details(8, 8), &invalid, &od, true).issue, CalibrationIssue::None);
        let d = CalibrationDetails { image_scale: 2.1, ..details(8, 8) };
        assert_eq!(sanity_check_calibration(&c, &d, &old, &od, true).issue, CalibrationIssue::None);
        let turned = Calibration { y_rate: 0.0134, ..cal(21.0, 111.0) };
        assert_eq!(sanity_check_calibration(&turned, &details(8, 8), &old, &od, true).issue, CalibrationIssue::None);
        let no_dec = Calibration { y_rate: CALIBRATION_RATE_UNCALIBRATED, ..old.clone() };
        assert_eq!(sanity_check_calibration(&c, &details(8, 8), &no_dec, &od, true).issue, CalibrationIssue::None);
    }

    #[test]
    fn issue_names_and_advisories() {
        assert_eq!(CalibrationIssue::Angle.text(), "Orthogonality");
        assert_eq!(CalibrationIssue::Different.text(), "Difference");
        assert_eq!(CalibrationIssue::from_i32(3), CalibrationIssue::Rates);
        assert_eq!(CalibrationIssue::from_i32(9), CalibrationIssue::None);
        assert_eq!(CalibrationIssue::Different as i32, 4);
        assert_eq!(CalibrationIssue::Different.warning_key_qualifier(), "Diff");
        assert!(CalibrationIssue::Steps.advisory().starts_with("Advisory: Calibration completed but few guide steps"));
        assert_eq!(CalibrationIssue::None.advisory(), "");
    }

    #[test]
    fn calstep_3_75um_400mm_half_sidereal() {
        // scale 1.9337″/px, 20″ = 10.3 px < 25 px → 25 px; 6.4458 s of travel;
        // 537 ms per step at dec 0 → 550.
        assert!(near(pixel_scale(3.75, 400, 1), 1.933734375, 1e-12));
        assert_eq!(calstep(3.75, 1, 400, 0.5, 0.0, CALSTEP_DEFAULT_STEPS), (25, 550));
        // dec 45: 759.6 → 800. dec 60: the Dec term 1074.2968749999998 sits
        // just under the 6-step cap 1074.296875 and wins → 1100.
        assert_eq!(calstep(3.75, 1, 400, 0.5, 45.0, CALSTEP_DEFAULT_STEPS), (25, 800));
        assert_eq!(calstep(3.75, 1, 400, 0.5, 60.0, CALSTEP_DEFAULT_STEPS), (25, 1100));
        // Long focal length: 20″ wins the distance (0.2475″/px → 81 px).
        assert_eq!(calstep(2.4, 1, 2000, 0.5, 0.0, CALSTEP_DEFAULT_STEPS), (81, 250));
        let (scale, step) = calibration_step_size(400, 3.75, 1, 0.5, 12, 0.0, 25);
        assert!(near(scale, 1.933734375, 1e-12));
        assert_eq!(step, 550);
    }

    #[test]
    fn ortho_error_and_keys() {
        assert!(near(ortho_error_deg(0.0, radians(250.0)), 20.0, 1e-9));
        assert!(near(ortho_error_deg(radians(-170.0), radians(100.0)), 0.0, 1e-9));
        assert_eq!(keys::prefix("scope"), "/scope/calibration/");
        assert_eq!(keys::prefix("stepguider") + keys::RA_STEPS, "/stepguider/calibration/ra_steps");
        assert!(!Calibration { y_rate: CALIBRATION_RATE_UNCALIBRATED, ..cal(0.0, 90.0) }.dec_calibrated());
        assert!(!CalibrationDetails::default().is_valid());
    }
}
