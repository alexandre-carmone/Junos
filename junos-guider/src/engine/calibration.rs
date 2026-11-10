//! The calibration model: what a mount calibration is, its details and
//! sanity verdict, and the camera ↔ mount transforms built on it.
//!
//! Everything here is pure: the persisted form lives in the profile store
//! (the key names are in [`keys`]).
//!
//! Ported from PHD2 `src/mount.h`, `src/mount.cpp`, `src/image_math.h` — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;

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

/// The profile keys under `/<mount class>/calibration/` (`mount.cpp:1580-1813`).
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
}

/// `CalibrationIssueString` (`mount.h:103`): the name the guide log and the
/// calibration review print (`Last Cal Issue = %s`).
pub fn issue_text(issue: &CalibrationIssue) -> &'static str {
    match issue {
        CalibrationIssue::None => "None",
        CalibrationIssue::Steps => "Steps",
        CalibrationIssue::Angle => "Orthogonality",
        CalibrationIssue::Rates => "Rates",
        CalibrationIssue::Different => "Difference",
    }
}

/// The advisory alert PHD2 shows for an issue (`scope.cpp:949-962`); empty
/// for `None`.
pub fn issue_advisory(issue: &CalibrationIssue) -> &'static str {
    match issue {
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
    /// A double in PHD2 (`mount.h:111`), compared to the camera's `u16` binning.
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

/// `orthoError` (`scope.cpp:1107`, also `scope.cpp:891`): the angle between
/// the two axes' deviation from the nearest multiple of 90°, in degrees.
pub fn ortho_error_deg(x_angle: f64, y_angle: f64) -> f64 {
    (norm_angle(x_angle - y_angle).abs() - PI / 2.0).abs().to_degrees()
}

/// `Mount::TransformCameraCoordinatesToMountCoordinates` (`mount.cpp:1135`):
/// a camera-plane offset into (RA, Dec) axis amounts. The RA component is the
/// projection on the RA axis, the Dec component the projection on the Dec
/// axis as measured (`x_angle + y_angle_error`), so the pair is not a
/// rotation when the axes are not orthogonal.
pub fn camera_to_mount(offset: Point, cal: &Calibration) -> Point {
    let hyp = offset.x.hypot(offset.y);
    let camera_theta = offset.y.atan2(offset.x);
    let x_angle = camera_theta - cal.x_angle;
    let y_angle = camera_theta - (cal.x_angle + cal.y_angle_error());
    Point::new(x_angle.cos() * hyp, y_angle.sin() * hyp)
}

/// `Mount::TransformMountCoordinatesToCameraCoordinates` (`mount.cpp:1185`):
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
            x_angle: x_deg.to_radians(),
            y_angle: y_deg.to_radians(),
            declination: 20.0f64.to_radians(),
            rotator_angle: ROTATOR_POSITION_UNKNOWN,
            binning: 1,
            pier_side: PierSide::East,
            ra_guide_parity: Parity::Even,
            dec_guide_parity: Parity::Even,
            is_valid: true,
            timestamp: "t".into(),
        }
    }


    #[test]
    fn norm_angle_wraps_into_minus_pi_pi() {
        assert!(near(norm_angle(PI), -PI, 1e-12)); // the top end is exclusive
        assert!(near(norm_angle(-PI), -PI, 1e-12));
        assert!(near(norm_angle(3.0 * PI), -PI, 1e-12));
        assert!(near(norm_angle(195.0f64.to_radians()), -165.0f64.to_radians(), 1e-12));
        assert!(near(norm(25.0, 0.0, 24.0), 1.0, 1e-12));
    }

    #[test]
    fn y_angle_error_normalisation() {
        // Orthogonal axes: no error. Reversed Dec axis: -π (never +π).
        assert!(near(cal(10.0, 100.0).y_angle_error(), 0.0, 1e-12));
        assert!(near(cal(15.0, -75.0).y_angle_error(), -PI, 1e-12));
        assert!(near(cal(0.0, 270.0).y_angle_error(), -PI, 1e-12));
        assert!(near(cal(170.0, -100.0).y_angle_error(), 0.0, 1e-12)); // 360° wraps to 0
        assert!(near(cal(15.0, 92.0).y_angle_error().to_degrees(), 13.0, 1e-9));
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
        // projects on the measured axis, and the reverse cannot undo it
        // exactly (mount.cpp:1096).
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
    fn issue_names_and_advisories() {
        assert_eq!(issue_text(&CalibrationIssue::Angle), "Orthogonality");
        assert_eq!(issue_text(&CalibrationIssue::Different), "Difference");
        assert_eq!(CalibrationIssue::from_i32(3), CalibrationIssue::Rates);
        assert_eq!(CalibrationIssue::from_i32(9), CalibrationIssue::None);
        assert_eq!(CalibrationIssue::Different as i32, 4);
        assert_eq!(CalibrationIssue::Different.warning_key_qualifier(), "Diff");
        assert!(issue_advisory(&CalibrationIssue::Steps).starts_with("Advisory: Calibration completed but few guide steps"));
        assert_eq!(issue_advisory(&CalibrationIssue::None), "");
    }

    #[test]
    fn ortho_error_and_keys() {
        assert!(near(ortho_error_deg(0.0, 250.0f64.to_radians()), 20.0, 1e-9));
        assert!(near(ortho_error_deg((-170.0f64).to_radians(), 100.0f64.to_radians()), 0.0, 1e-9));
        assert_eq!(keys::prefix("scope"), "/scope/calibration/");
        assert_eq!(keys::prefix("stepguider") + keys::RA_STEPS, "/stepguider/calibration/ra_steps");
        assert!(!Calibration { y_rate: CALIBRATION_RATE_UNCALIBRATED, ..cal(0.0, 90.0) }.dec_calibrated());
        assert!(!CalibrationDetails::default().is_valid());
    }
}
