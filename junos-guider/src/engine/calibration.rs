//! The calibration model: what a mount calibration is, the camera ↔ mount
//! transforms built on it, the pier-flip and the "adjust for where the scope
//! points now" step run at guide start, the post-calibration sanity checks,
//! and the calibration-step calculator.
//!
//! Everything here is pure: the persisted form lives in the profile store
//! (the key names are in [`keys`]), the debug-log and alert wording comes
//! back as strings for the caller to write or show.
//!
//! Ported from PHD2 `src/mount.h`, `src/mount.cpp`, `src/scope.cpp`, `src/calstep_dialog.cpp`, `src/image_math.h` — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;

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
    /// profile with no stored timestamp.
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

/// What [`flip_calibration`] reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlipResult {
    /// The status-bar line, `CAL: East(15,105)->West(-165,105)`.
    pub status: String,
    /// The three debug-log lines, in PHD2's wording.
    pub log: Vec<String>,
}

/// `Mount::FlipCalibration` (`mount.cpp:891`): the calibration as seen from
/// the other side of the pier. RA turns by π; Dec turns by π too when the
/// mount reverses its Dec motor on a flip (`CalFlipRequiresDecFlip`), else
/// the Dec parity inverts instead. The pier side swaps; the RA parity, the
/// rates, the declination and the rotator angle stay. `None` when there is
/// no calibration to flip ("cannot flip if not calibrated").
pub fn flip_calibration(cal: &mut Calibration, dec_flip_required: bool) -> Option<FlipResult> {
    if !cal.is_valid {
        return None;
    }
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

    cal.x_angle = new_x;
    cal.y_angle = new_y;
    cal.pier_side = new_side;
    cal.dec_guide_parity = new_dec_parity;

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
    Some(FlipResult { status, log })
}

/// Where the scope points and what the gear reports when guiding starts:
/// the inputs `Mount::AdjustCalibrationForScopePointing` reads from the
/// pointing source, the camera and the rotator.
#[derive(Clone, Debug, PartialEq)]
pub struct Pointing {
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

/// The mount settings the adjustment depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdjustOpts {
    /// `GetMountClassName()`: `scope` or `stepguider`, for the log line.
    pub mount_class: &'static str,
    /// `UseDecComp` (`Scope::DecCompensationEnabled`; always false for an AO).
    pub use_dec_comp: bool,
    /// `CalFlipRequiresDecFlip`.
    pub cal_flip_requires_dec_flip: bool,
}

/// What [`adjust_for_scope_pointing`] decided.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Adjustment {
    /// The effective RA rate (`m_xRate`), px/ms: the stored one, or the
    /// Dec-compensated one. Never persisted.
    pub x_rate: f64,
    /// The calibration was flipped to the other side of the pier.
    pub flipped: bool,
    /// The calibration was rewritten (binning, flip or rotator) and must be
    /// stored again (PHD2's `SetCalibration`, which also restamps `timestamp`).
    pub cal_changed: bool,
    /// The image scale moved by ≥ 1 %: PHD2 runs `MyFrame::HandleImageScaleChange`
    /// (clears the calibration, resets the step size and min-moves); the
    /// caller must do the same.
    pub image_scale_changed: bool,
    /// The camera-reported pixel size to write into the profile, when it
    /// disagreed with the profile by ≥ 1 µm (`pCamera->SetCameraPixelSize`).
    pub new_pixel_size: Option<f64>,
    /// `pFrame->Alert` texts, in order.
    pub alerts: Vec<String>,
    /// Debug-log lines, in order and in PHD2's wording.
    pub log: Vec<String>,
}

/// `Mount::AdjustCalibrationForScopePointing` (`mount.cpp:1253-1410`), run
/// when guiding starts: warns when the guide speeds or the pixel size moved
/// since calibration, rescales the rates for a binning change, flips the
/// calibration when the scope is on the other side of the pier, turns the
/// axes with the rotator, then compensates the RA rate for the declination
/// (`cal.x_rate / cos(cal_dec) · cos(dec)` with `dec` clamped to ±89°, only
/// when the calibration was done within [`DEC_COMP_LIMIT`] of the equator and
/// `use_dec_comp` is on). `prev_x_rate` is the effective rate in force before
/// the call, only for the "asserted base xRate" log line.
pub fn adjust_for_scope_pointing(
    cal: &mut Calibration,
    details: &CalibrationDetails,
    pointing: &Pointing,
    opts: &AdjustOpts,
    prev_x_rate: f64,
) -> Adjustment {
    let mut out = Adjustment { x_rate: cal.x_rate, ..Default::default() };
    let new_declination = pointing.declination.unwrap_or(UNKNOWN_DECLINATION);
    let new_rotator = pointing.rotator_angle.unwrap_or(ROTATOR_POSITION_UNKNOWN);
    let binning = pointing.binning;

    out.log.push(format!(
        "AdjustCalibrationForScopePointing ({}): current dec={} pierSide={}, cal dec={} pierSide={} rotAngle={} bin={}",
        opts.mount_class,
        declination_str(new_declination),
        pointing.pier_side as i32,
        declination_str(cal.declination),
        cal.pier_side as i32,
        rot_angle_str(new_rotator),
        binning
    ));

    // Guide speeds changed since calibration: an alert that can't be avoided.
    if let Some((cur_ra, cur_dec)) = pointing.guide_rates
        && details.ra_guide_speed > 0.0
        && details.dec_guide_speed > 0.0
        && ((1.0 - cur_ra / details.ra_guide_speed).abs() > 0.05
            || (1.0 - cur_dec / details.dec_guide_speed).abs() > 0.05)
    {
        out.alerts.push(
            "Mount guide speeds are different from those used in last calibration.  Do a new calibration or reset mount guide speed settings to previous values. "
                .to_string(),
        );
        out.log.push(format!(
            "Guide speeds have changed since calibration.  Orig RA = {}, Orig Dec = {}, Curr RA = {}, Curr Dec = {}, Units are arc-sec/sec",
            fmt_f(details.ra_guide_speed * 3600.0, 1),
            fmt_f(details.dec_guide_speed * 3600.0, 1),
            fmt_f(cur_ra * 3600.0, 1),
            fmt_f(cur_dec * 3600.0, 1)
        ));
    }

    if pointing.pier_side != PierSide::Unknown && cal.pier_side == PierSide::Unknown {
        out.alerts.push(
            "Current calibration did not have side-of-pier information, so PHD2 can't automatically correct for meridian flips. You should do a fresh calibration to correct this problem."
                .to_string(),
        );
    }

    // Binning or pixel-size change. A driver may lie about the binning while
    // changing the reported pixel size, so both are watched.
    let mut scale_adjustment = 1.0;
    if (pointing.camera_pixel_size - pointing.profile_pixel_size).abs() >= 1.0 {
        out.alerts.push(
            "Profile pixel size doesn't match camera-reported pixel size.  Re-calibrate to restore correct guiding."
                .to_string(),
        );
        out.log.push(format!(
            "Camera pixel size changed from {} to {}",
            fmt_f(pointing.profile_pixel_size, 1),
            fmt_f(pointing.camera_pixel_size, 1)
        ));
        scale_adjustment = pointing.camera_pixel_size / pointing.profile_pixel_size;
        out.new_pixel_size = Some(pointing.camera_pixel_size);
    }

    if binning != cal.binning && cal.is_valid {
        // Quirk (mount.cpp:1319): `binning / m_cal.binning` is an unsigned
        // short division, so 2 -> 3 yields 1 and no image-scale change is
        // seen; the rates themselves use the double ratio. PHD2 would trap on
        // a stored binning of 0; a divisor of 1 stands in for it here.
        scale_adjustment *= (binning / cal.binning.max(1)) as f64;
        let adj = cal.binning as f64 / binning as f64;
        let new_x = cal.x_rate * adj;
        let new_y = cal.y_rate * adj;
        out.log.push(format!(
            "Binning {} -> {}, rates ({}, {}) -> ({}, {})",
            cal.binning,
            binning,
            fmt_f(cal.x_rate * 1000.0, 3),
            fmt_f(cal.y_rate * 1000.0, 3),
            fmt_f(new_x * 1000.0, 3),
            fmt_f(new_y * 1000.0, 3)
        ));
        cal.x_rate = new_x;
        cal.y_rate = new_y;
        cal.binning = binning;
        out.x_rate = cal.x_rate;
        out.cal_changed = true;
    }

    if (scale_adjustment - 1.0).abs() >= 0.01 {
        out.log.push("Mount::AdjustCalibrationForScopePointing: imageScaleRatio changed".to_string());
        out.image_scale_changed = true;
        // MyFrame::HandleImageScaleChange's own alert (myframe.cpp:2863).
        out.alerts.push(
            "Binning or camera pixel size changed unexpectedly. You should use separate profiles for different image scales."
                .to_string(),
        );
    }

    if is_opposite_side(pointing.pier_side, cal.pier_side) {
        out.log.push(format!(
            "Guiding starts on opposite side of pier: calibration data side is {}, current side is {}",
            cal.pier_side.label(),
            pointing.pier_side.label()
        ));
        if let Some(flip) = flip_calibration(cal, opts.cal_flip_requires_dec_flip) {
            out.log.extend(flip.log);
            out.flipped = true;
            out.cal_changed = true;
        }
    }

    if let Some(new_rot) = pointing.rotator_angle {
        if cal.rotator_angle == ROTATOR_POSITION_UNKNOWN {
            // No rotator position at calibration time: nothing to adjust from.
            // The angle is remembered in memory only (not via SetCalibration).
            out.alerts.push("Rotator position has changed, recalibration is needed.".to_string());
            cal.rotator_angle = new_rot;
        } else {
            let da = new_rot - cal.rotator_angle;
            if da.abs() > 0.05 {
                out.log.push(format!(
                    "New rotator position {} deg, prev = {} deg, delta = {} deg",
                    fmt_f(new_rot, 1),
                    fmt_f(cal.rotator_angle, 1),
                    fmt_f(da, 1)
                ));
                let da = radians(da);
                cal.x_angle = norm_angle(cal.x_angle - da);
                cal.y_angle = norm_angle(cal.y_angle - da);
                cal.rotator_angle = new_rot;
                out.cal_changed = true;
            }
        }
    }

    // Dec compensation, last, since it reads the rates the steps above may
    // have rewritten. The result is never stored.
    let mut deccomp = false;
    if new_declination != cal.declination
        && new_declination != UNKNOWN_DECLINATION
        && cal.declination != UNKNOWN_DECLINATION
    {
        if cal.declination.abs() > DEC_COMP_LIMIT {
            out.log.push("skipping Dec comp: initial calibration too far from equator".to_string());
            out.alerts.push("Calibration was too far from equator, recalibration is needed.".to_string());
        } else if !opts.use_dec_comp {
            out.log.push("skipping Dec comp: Dec Comp not enabled".to_string());
        } else {
            // Near the pole the rate would blow up downstream: clamp to ±89°.
            // wxMax / wxMin, so a NaN declination comes through.
            let dec = wx_max(radians(-89.0), wx_min(radians(89.0), new_declination));
            out.x_rate = (cal.x_rate / cal.declination.cos()) * dec.cos();
            deccomp = true;
            out.log.push(format!(
                "Dec comp: XRate {} -> {} for dec {} -> dec {}",
                fmt_f(cal.x_rate * 1000.0, 3),
                fmt_f(out.x_rate * 1000.0, 3),
                fmt_f(degrees(cal.declination), 1),
                fmt_f(degrees(dec), 1)
            ));
        }
    }
    if !deccomp {
        if prev_x_rate != cal.x_rate {
            out.log.push(format!("No dec comp, asserted base xRate {}", fmt_f(cal.x_rate * 1000.0, 3)));
        }
        out.x_rate = cal.x_rate;
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

    fn pointing() -> Pointing {
        Pointing {
            declination: Some(radians(20.0)),
            pier_side: PierSide::East,
            rotator_angle: None,
            binning: 1,
            guide_rates: None,
            camera_pixel_size: 3.75,
            profile_pixel_size: 3.75,
        }
    }

    const OPTS: AdjustOpts = AdjustOpts { mount_class: "scope", use_dec_comp: true, cal_flip_requires_dec_flip: false };

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
    fn flip_without_dec_flip_inverts_parity() {
        let mut c = cal(15.0, 105.0);
        let r = flip_calibration(&mut c, false).unwrap();
        assert!(near(degrees(c.x_angle), -165.0, 1e-9));
        assert!(near(degrees(c.y_angle), 105.0, 1e-9));
        assert_eq!(c.pier_side, PierSide::West);
        assert_eq!(c.ra_guide_parity, Parity::Even);
        assert_eq!(c.dec_guide_parity, Parity::Odd);
        assert_eq!(r.status, "CAL: East(15,105)->West(-165,105)");
        assert_eq!(
            r.log[0],
            "FlipCalibration before: x=15.0, y=105.0 decFlipRequired=0 sideOfPier=East rotAngle=None parity=+/+"
        );
        assert_eq!(r.log[1], "FlipCalibration pre-normalize: x=195.0, y=105.0");
        assert_eq!(r.log[2], "FlipCalibration after: x=-165.0 y=105.0 sideOfPier=West parity=+/-");
    }

    #[test]
    fn flip_with_dec_flip_turns_y_and_keeps_parity() {
        let mut c = cal(15.0, 105.0);
        flip_calibration(&mut c, true).unwrap();
        assert!(near(degrees(c.x_angle), -165.0, 1e-9));
        assert!(near(degrees(c.y_angle), -75.0, 1e-9));
        assert_eq!(c.dec_guide_parity, Parity::Even);
        assert_eq!(c.pier_side, PierSide::West);
        // Flipping back restores the angles.
        flip_calibration(&mut c, true).unwrap();
        assert!(near(degrees(c.x_angle), 15.0, 1e-9) && near(degrees(c.y_angle), 105.0, 1e-9));
        assert_eq!(c.pier_side, PierSide::East);
        // Nothing to flip without a calibration.
        let mut none = Calibration::default();
        assert!(flip_calibration(&mut none, false).is_none());
    }

    #[test]
    fn flip_reads_the_normalised_y_angle() {
        // From PHD2's C++: a stored 270° Dec axis flips as -90°.
        let mut c = cal(15.0, 270.0);
        let r = flip_calibration(&mut c, false).unwrap();
        assert_eq!(r.log[0], "FlipCalibration before: x=15.0, y=-90.0 decFlipRequired=0 sideOfPier=East rotAngle=None parity=+/+");
        assert_eq!(r.log[1], "FlipCalibration pre-normalize: x=195.0, y=-90.0");
        assert_eq!(r.log[2], "FlipCalibration after: x=-165.0 y=-90.0 sideOfPier=West parity=+/-");
        assert_eq!(r.status, "CAL: East(15,-90)->West(-165,-90)");
        assert_eq!(c.x_angle, -2.8797932657906435);
        assert_eq!(c.y_angle, -1.5707963267948966);
    }

    #[test]
    fn dec_comp_at_cal_dec_20_pointing_60() {
        let mut c = cal(15.0, 105.0);
        let p = Pointing { declination: Some(radians(60.0)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &OPTS, 0.01);
        assert_eq!(a.x_rate, 0.0053208888623795612); // PHD2's C++
        assert!(near(c.x_rate, 0.01, 0.0)); // the stored rate is untouched
        assert!(!a.flipped && !a.cal_changed && a.alerts.is_empty());
        assert_eq!(a.log[1], "Dec comp: XRate 10.000 -> 5.321 for dec 20.0 -> dec 60.0");
    }

    #[test]
    fn dec_comp_clamps_at_89_and_skips_when_off_or_far() {
        let mut c = cal(15.0, 105.0);
        let p = Pointing { declination: Some(radians(89.5)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &OPTS, 0.01);
        assert_eq!(a.x_rate, 0.00018572463006772492); // PHD2's C++, radians(89)
        assert_eq!(a.log[1], "Dec comp: XRate 10.000 -> 0.186 for dec 20.0 -> dec 89.0");

        // A NaN declination goes through wxMax / wxMin, as in PHD2's C++.
        let p_nan = Pointing { declination: Some(f64::NAN), ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p_nan, &OPTS, 0.01);
        assert!(a.x_rate.is_nan());
        assert_eq!(a.log[0], "AdjustCalibrationForScopePointing (scope): current dec=nan pierSide=0, cal dec=20.0 pierSide=0 rotAngle=None bin=1");
        assert_eq!(a.log[1], "Dec comp: XRate 10.000 -> nan for dec 20.0 -> dec nan");

        // Dec comp off: base rate, plus the "asserted" line when it differed.
        let off = AdjustOpts { use_dec_comp: false, ..OPTS };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &off, 0.005);
        assert_eq!(a.x_rate, 0.01);
        assert_eq!(a.log[1], "skipping Dec comp: Dec Comp not enabled");
        assert_eq!(a.log[2], "No dec comp, asserted base xRate 10.000");

        // Calibrated past 60°: an alert, no compensation.
        let mut far = Calibration { declination: radians(65.0), ..cal(15.0, 105.0) };
        let p = Pointing { declination: Some(radians(10.0)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut far, &details(8, 8), &p, &OPTS, 0.01);
        assert_eq!(a.x_rate, 0.01);
        assert_eq!(a.alerts, vec!["Calibration was too far from equator, recalibration is needed.".to_string()]);

        // Same declination as the calibration, or unknown: nothing happens.
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &pointing(), &OPTS, 0.01);
        assert_eq!(a.x_rate, 0.01);
        assert_eq!(a.log.len(), 1);
        let p = Pointing { declination: None, ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &OPTS, 0.01);
        assert_eq!(a.x_rate, 0.01);
        assert_eq!(a.log[0], "AdjustCalibrationForScopePointing (scope): current dec=Unknown pierSide=0, cal dec=20.0 pierSide=0 rotAngle=None bin=1");
    }

    #[test]
    fn adjust_flips_on_opposite_pier_and_turns_with_rotator() {
        let mut c = Calibration { rotator_angle: 30.0, ..cal(15.0, 105.0) };
        let p = Pointing { pier_side: PierSide::West, rotator_angle: Some(40.0), ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &OPTS, 0.01);
        assert!(a.flipped && a.cal_changed);
        assert_eq!(c.pier_side, PierSide::West);
        assert_eq!(c.dec_guide_parity, Parity::Odd);
        // Flip: 15 → -165, 105; then the 10° rotator delta is subtracted.
        assert!(near(degrees(c.x_angle), -175.0, 1e-9));
        assert!(near(degrees(c.y_angle), 95.0, 1e-9));
        assert_eq!(c.rotator_angle, 40.0);
        assert_eq!(
            a.log[1],
            "Guiding starts on opposite side of pier: calibration data side is East, current side is West"
        );
        assert_eq!(a.log[5], "New rotator position 40.0 deg, prev = 30.0 deg, delta = 10.0 deg");

        // Rotator unknown at calibration: an alert, the angle remembered.
        let mut c = cal(15.0, 105.0);
        let p = Pointing { rotator_angle: Some(40.0), ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &OPTS, 0.01);
        assert_eq!(a.alerts, vec!["Rotator position has changed, recalibration is needed.".to_string()]);
        assert_eq!(c.rotator_angle, 40.0);
        assert!(!a.cal_changed);

        // Unknown pier side in the calibration: an alert, no flip.
        let mut c = Calibration { pier_side: PierSide::Unknown, ..cal(15.0, 105.0) };
        let p = Pointing { pier_side: PierSide::West, ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &OPTS, 0.01);
        assert!(!a.flipped);
        assert!(a.alerts[0].starts_with("Current calibration did not have side-of-pier information"));
    }

    #[test]
    fn adjust_binning_and_guide_speed_changes() {
        // Binning 1 → 2 halves the rates and changes the image scale.
        let mut c = cal(15.0, 105.0);
        let p = Pointing { binning: 2, ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &OPTS, 0.01);
        assert!(near(c.x_rate, 0.005, 1e-15) && near(c.y_rate, 0.0053, 1e-15));
        assert_eq!(c.binning, 2);
        assert_eq!(a.x_rate, 0.005);
        assert!(a.cal_changed && a.image_scale_changed);
        assert_eq!(a.log[1], "Binning 1 -> 2, rates (10.000, 10.600) -> (5.000, 5.300)");
        assert_eq!(a.alerts.len(), 1);

        // Quirk: 2 → 3 is an integer ratio of 1, so no image-scale change.
        let mut c = Calibration { binning: 2, ..cal(15.0, 105.0) };
        let p = Pointing { binning: 3, ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &details(8, 8), &p, &OPTS, 0.01);
        assert!(!a.image_scale_changed);
        assert!(near(c.x_rate, 0.01 * 2.0 / 3.0, 1e-15));

        // Guide speed 20 % off: the alert, speeds printed in arcsec/s.
        let mut c = cal(15.0, 105.0);
        let d = details(8, 8);
        let p = Pointing { guide_rates: Some((d.ra_guide_speed * 1.2, d.dec_guide_speed)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &d, &p, &OPTS, 0.01);
        assert!(a.alerts[0].starts_with("Mount guide speeds are different"));
        assert_eq!(
            a.log[1],
            "Guide speeds have changed since calibration.  Orig RA = 7.5, Orig Dec = 7.5, Curr RA = 9.0, Curr Dec = 7.5, Units are arc-sec/sec"
        );
        // Within 5 %: nothing.
        let p = Pointing { guide_rates: Some((d.ra_guide_speed * 1.04, d.dec_guide_speed)), ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &d, &p, &OPTS, 0.01);
        assert!(a.alerts.is_empty());

        // Pixel size 1 µm off: the profile is updated, the scale changed.
        let p = Pointing { camera_pixel_size: 4.8, ..pointing() };
        let a = adjust_for_scope_pointing(&mut c, &d, &p, &OPTS, 0.01);
        assert_eq!(a.new_pixel_size, Some(4.8));
        assert!(a.image_scale_changed);
        assert_eq!(a.log[1], "Camera pixel size changed from 3.8 to 4.8");
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
