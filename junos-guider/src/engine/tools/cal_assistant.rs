//! The Calibration Assistant: where to calibrate (a few degrees from the
//! meridian, near Dec 0 or as low as the site allows), the slews there
//! (the last one northward to clear the Dec backlash), then a forced
//! calibration and its verdict.
//!
//! The engine reads the mount's position and slews it; this module holds
//! the window's arithmetic and texts.
//!
//! Ported from PHD2 `src/calibration_assistant.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use crate::engine::calibration::{
    CALIBRATION_RATE_UNCALIBRATED, Calibration, CalibrationDetails, CalibrationIssue, DEC_COMP_LIMIT,
    ProfileAccess, UNKNOWN_DECLINATION, degrees, norm, norm_angle,
};
use crate::engine::log::cfmt::fmt_f;

/// The custom calibration location (`CalCustomDialog::OnOk`).
pub mod keys {
    /// Meridian offset, degrees; negative east.
    pub const TGT_HA: &str = "/scope/CalSlew/TgtHA";
    pub const TGT_DEC: &str = "/scope/CalSlew/TgtDec";
    /// Keep the custom side rather than the side the mount points on.
    pub const SINGLE_SIDE: &str = "/scope/CalSlew/SingleSide";
}

const DEF_BEST_DEC: i32 = 0;
const DEF_BEST_OFFSET: i32 = 5;
/// The pause after each slew, ms (`slewSettleTime`).
pub const SLEW_SETTLE_MS: i64 = 2000;
/// The target spinners' ranges: offset 5..=50, Dec −50..=50.
pub const OFFSET_RANGE: (i32, i32) = (5, 50);
pub const DEC_RANGE: (i32, i32) = (-50, 50);

pub const MERIDIAN_FLIP: &str = "MERIDIAN FLIP!";
pub const WATCH_SLEWING: &str = "WATCH SCOPE DURING SLEWING TO INSURE SAFETY";

/// `RateX`: a guide speed in deg/s as a multiple of sidereal.
pub fn rate_x(spd: f64) -> f64 {
    spd * 3600.0 / (15.0 * 0.9973)
}

/// The hour angle, hours in [−12, 12).
pub fn hour_angle(ra: f64, lst: f64) -> f64 {
    norm(lst - ra, -12.0, 12.0)
}

/// Where to calibrate: meridian offset (degrees, ≥ 0), Dec, east of the
/// meridian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub offset: i32,
    pub dec: i32,
    pub east: bool,
}

/// `GetCustomLocation`: (meridian offset, negative east; Dec; single
/// side; the defaults).
pub fn custom_location(p: &impl ProfileAccess) -> (i32, i32, bool, bool) {
    let ha = p.get_i32(keys::TGT_HA, DEF_BEST_OFFSET);
    let dec = p.get_i32(keys::TGT_DEC, DEF_BEST_DEC);
    let single = p.get_bool(keys::SINGLE_SIDE, false);
    (ha, dec, single, dec == DEF_BEST_DEC && ha == DEF_BEST_OFFSET && !single)
}

/// `CalCustomDialog::OnOk`.
pub fn save_custom(p: &mut impl ProfileAccess, t: Target, single_side: bool) {
    p.set_i32(keys::TGT_HA, if t.east { -t.offset } else { t.offset });
    p.set_i32(keys::TGT_DEC, t.dec);
    p.set_bool(keys::SINGLE_SIDE, single_side);
}

/// `GetCalibPositionRecommendations` from the mount's (RA, Dec, LST) and
/// the site's latitude when known: (signed offset, negative east; Dec).
/// Quirk kept: Dec is an `int` the latitude correction is added to, so it
/// truncates toward zero.
pub fn recommendation(coords: (f64, f64, f64), site_lat: Option<f64>) -> (i32, i32) {
    let (ra, _, lst) = coords;
    let mut best_offset = DEF_BEST_OFFSET;
    let mut best_dec = DEF_BEST_DEC;
    if hour_angle(ra, lst) <= 0.0 {
        best_offset = -best_offset;
    }
    match site_lat {
        None => best_dec = 0,
        Some(lat) if lat >= 0.0 => {
            let eq_altitude = 90.0 - lat;
            if eq_altitude < 30.0 {
                best_dec = (best_dec as f64 + (30.0 - eq_altitude)) as i32;
            }
        }
        Some(lat) => {
            let eq_altitude = 90.0 + lat;
            if eq_altitude < 30.0 {
                best_dec = (best_dec as f64 - (30.0 - eq_altitude)) as i32;
            }
        }
    }
    (best_offset, best_dec)
}

/// `InitializeUI`'s target: the custom location unless `defaults` or none
/// saved (its side, or the mount's when not single-sided), else the
/// recommendation.
pub fn initial_target(p: &impl ProfileAccess, defaults: bool, coords: (f64, f64, f64), site_lat: Option<f64>) -> Target {
    let (ha, dec, single, using_defaults) = custom_location(p);
    if !defaults && !using_defaults {
        let east = if single { ha <= 0 } else { hour_angle(coords.0, coords.2) <= 0.0 };
        return Target { offset: ha.abs(), dec, east };
    }
    let (ha, dec) = recommendation(coords, site_lat);
    Target { offset: ha.abs(), dec, east: ha <= 0 }
}

/// The current pointing as the window shows it: offset from the meridian
/// (`%.1f` of |HA|·15), Dec (`%+.1f`), east (`hourAngle <= 0` when the
/// window opens, `< 0` on the timer's updates — PHD2's two tests).
pub fn current(coords: (f64, f64, f64), from_timer: bool) -> (String, String, bool) {
    let (ra, dec, lst) = coords;
    let ha = hour_angle(ra, lst);
    let east = if from_timer { ha < 0.0 } else { ha <= 0.0 };
    (fmt_f((ha * 15.0).abs(), 1), format!("{}{}", if dec >= 0.0 { "+" } else { "" }, fmt_f(dec, 1)), east)
}

/// `OnSlew`'s RA: the target offset from the sidereal time.
pub fn slew_ra(lst: f64, t: Target) -> f64 {
    let ofs = t.offset as f64 / 15.0;
    norm(if t.east { lst + ofs } else { lst - ofs }, 0.0, 24.0)
}

/// `OnSlew`'s slews, each (RA, Dec, the status shown before it): two when
/// the scope goes south or flips, the last northward to clear the Dec
/// backlash.
pub fn slew_plan(lst: f64, cur_dec: f64, t: Target, meridian_flip: bool) -> Vec<(f64, f64, &'static str)> {
    let (ra, dec) = (slew_ra(lst, t), t.dec as f64);
    if dec <= cur_dec || meridian_flip {
        vec![
            (ra, dec - 1.0, "Initial slew to approximate position"),
            (ra, dec, "Final slew north to pre-clear Dec backlash"),
        ]
    } else {
        vec![(ra, dec, "Slewing to calibration location")]
    }
}

/// `ShowExplanationMsg`.
pub fn explanation(dec: f64, can_slew: bool) -> String {
    let slew = if can_slew {
        "Use the 'slew' button to move the scope to within 20 degrees of Dec = 0 or as close to that as your site will \
         allow."
    } else {
        "Slew the scope to within 20 degrees of Dec = 0 or as close to that as your site will allow."
    };
    if dec.abs() > 80.0 {
        format!("Calibration is likely to fail this close to the pole. {slew}")
    } else if dec.abs() > degrees(DEC_COMP_LIMIT) {
        format!(
            "If you calibrate within 30 degrees of the pole, you will need to recalibrate when you slew to a different \
             target. {slew}"
        )
    } else if dec.abs() > 20.0 {
        format!("Calibration will be more accurate with the scope pointing closer to the celestial equator. {slew}")
    } else {
        String::new()
    }
}

/// `PerformSanityChecks`' advice from the guide speeds (deg/s; Dec −1
/// unknown) and the step sizes (current, recommended ms); `None` when all
/// is well.
pub fn sanity_message(ra_spd: f64, dec_spd: f64, steps: impl FnOnce(f64) -> (i32, i32)) -> Option<&'static str> {
    let min = if dec_spd != -1.0 { ra_spd.min(dec_spd) } else { ra_spd };
    let sid_rate = rate_x(min);
    // `PercentChange`: (new − old) / old, as a percentage, unsigned.
    if sid_rate < 0.5 && ((sid_rate - 0.5) / 0.5 * 100.0).abs() > 5.0 {
        return Some(if sid_rate <= 0.2 {
            "Your mount guide speed is too slow for effective calibration and guiding. Use the hand-controller or \
             mount driver to increase the guide speed to at least 0.5x sidereal. Then click the 'Recalc' button so \
             PHD2 can compute a correct calibration step-size."
        } else {
            "Your mount guide speed is below the minimum recommended value of 0.5x sidereal. Use the hand-controller \
             or mount driver to increase the guide speed to at least 0.5x sidereal. Then click the 'Recalc' button so \
             PHD2 can compute a correct calibration step-size."
        });
    }
    let (current, recommended) = steps(sid_rate);
    ((1.0 - current as f64 / recommended as f64).abs() > 0.3).then_some(
        "Your current calibration parameters can be adjusted for more accurate results. Click the 'Recalc' button to \
         restore them to the default values.",
    )
}

/// `EvaluateCalibration`'s verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Good,
    Acceptable,
    Poor,
    /// Not calibrated: "probably because the mount didn't move at all".
    Failed,
}

/// A verdict, the reasons (`m_lastResult`), the status text and the debug
/// lines.
#[derive(Clone, Debug, PartialEq)]
pub struct Evaluation {
    pub verdict: Verdict,
    pub reasons: String,
    pub status: String,
    pub log: Vec<String>,
}

/// `EvaluateCalibration`: `cal` and `details` are the scope's new
/// calibration when it is calibrated; `dec_comp` its Dec compensation.
pub fn evaluate(cal: Option<(&Calibration, &CalibrationDetails)>, dec_comp: bool) -> Evaluation {
    const MIN_STEPS: i32 = 4;
    const ORTHO_TOLERANCE: f64 = 12.5;
    const AXIS_RATES_TOLERANCE: f64 = 0.20;
    let Some((c, d)) = cal else {
        return Evaluation {
            verdict: Verdict::Failed,
            reasons: String::new(),
            status: "Calibration failed - probably because the mount didn't move at all".into(),
            log: vec!["CalAsst: calibration failed".into()],
        };
    };
    let mut acceptable = matches!(d.last_issue, CalibrationIssue::None | CalibrationIssue::Different);
    let mut reasons: Vec<&str> = Vec::new();
    let mut dbg = String::from("CalAsst: ");
    // PHD2 declares the expected ratio twice: the outer one, compared to
    // the tolerance below, stays 1.
    let expected_outer = 1.0;
    let mut actual = 1.0;
    let meaningful = c.declination != UNKNOWN_DECLINATION
        && c.y_rate != CALIBRATION_RATE_UNCALIBRATED
        && c.declination.abs() <= DEC_COMP_LIMIT
        && dec_comp;
    if meaningful {
        let speed_ratio = if d.ra_guide_speed > 0.0 { d.dec_guide_speed / d.ra_guide_speed } else { 1.0 };
        actual = c.x_rate * speed_ratio / c.y_rate;
        dbg += &format!(
            "Spds: {}X,{}X, Dec: {}, Rates: {}, {}, ",
            fmt_f(rate_x(d.ra_guide_speed), 1),
            fmt_f(rate_x(d.dec_guide_speed), 1),
            fmt_f(degrees(c.declination), 1),
            fmt_f(rate_x(c.x_rate), 1),
            fmt_f(rate_x(c.y_rate), 1)
        );
    } else {
        dbg += "Spds: N/A, ";
    }
    let (ra_n, dec_n) = (d.ra_step_count, d.dec_step_count);
    let mut good =
        (ra_n >= 2 * MIN_STEPS || (dec_n >= 2 * MIN_STEPS && dec_n > 0)) && (ra_n <= 30 || (dec_n <= 30 && dec_n > 0));
    dbg += &format!("Steps: {ra_n},{dec_n}, ");
    if !good {
        reasons.push("Steps");
    }
    let non_ortho = degrees(((norm_angle(c.x_angle - c.y_angle)).abs() - std::f64::consts::FRAC_PI_2).abs());
    dbg += &format!("Ortho: {}, ", fmt_f(non_ortho, 2));
    if non_ortho > 5.0 {
        reasons.push("Orthogonality");
        good = false;
        acceptable = acceptable && non_ortho <= ORTHO_TOLERANCE;
    }
    if meaningful {
        dbg += &format!("Rates: {} (Expect) vs {} (Act)", fmt_f(c.declination.cos(), 2), fmt_f(actual, 2));
        if (c.declination.cos() - actual).abs() > 0.1 {
            reasons.push("Rates");
            good = false;
            acceptable = acceptable && (expected_outer - actual).abs() < AXIS_RATES_TOLERANCE;
        }
    }
    let dec_deg = degrees(c.declination).abs();
    if dec_deg > 60.0 {
        good = false;
        acceptable = false;
        reasons.push("Sky location (Dec comp disabled)");
    } else if dec_deg > 20.0 {
        good = false;
        reasons.push("Sky location");
    }
    let why = if reasons.is_empty() { String::new() } else { format!("({})", reasons.join(", ")) };
    let mut log = vec![dbg];
    let (verdict, status) = if good {
        log.push("CalAsst: good result".into());
        (Verdict::Good, "Calibration result was good, guiding is active using the new calibration".to_string())
    } else if acceptable {
        log.push(format!("CalAsst: acceptable result, {why}"));
        (Verdict::Acceptable, format!("Calibration result was acceptable, guiding is active using the new calibration\n{why}"))
    } else {
        log.push(format!("CalAsst: poor result, {why}"));
        (Verdict::Poor, format!("Calibration result was poor, consider re-doing it while following all recommendations\n{why}"))
    };
    Evaluation { verdict, reasons: why, status, log }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::calibration::radians;
    use crate::engine::tools::test_util::Mem;

    // The goldens come from calibration_assistant.cpp's arithmetic
    // (GetCalibPositionRecommendations, OnSlew) built in a harness.
    #[test]
    fn recommendations_and_slews() {
        // (RA, Dec, LST), latitude → (offset, Dec).
        let cases = [
            ((5.0, 30.0, 7.5), Some(45.0), (5, 0)),
            ((10.0, 30.0, 7.5), Some(45.0), (-5, 0)),
            ((7.5, 30.0, 7.5), Some(45.0), (-5, 0)),
            ((23.0, 30.0, 1.0), Some(65.5), (5, 5)),
            ((1.0, 30.0, 23.0), Some(72.25), (-5, 12)),
            ((5.0, 30.0, 7.5), Some(-65.5), (5, -5)),
            ((5.0, 30.0, 7.5), Some(-78.9), (5, -18)),
            ((5.0, 30.0, 7.5), None, (5, 0)),
        ];
        for (coords, lat, want) in cases {
            assert_eq!(recommendation(coords, lat), want, "{coords:?} {lat:?}");
        }
        let east = Target { offset: 10, dec: 5, east: true };
        let west = Target { offset: 10, dec: 5, east: false };
        assert!((slew_ra(23.9, east) - 0.5666666666666664).abs() < 1e-12);
        assert!((slew_ra(0.2, west) - 23.533333333333335).abs() < 1e-12);
        // South or across the meridian: two slews, the last northward.
        let plan = slew_plan(12.0, 20.0, east, false);
        assert_eq!(plan.len(), 2);
        assert_eq!((plan[0].1, plan[1].1), (4.0, 5.0));
        assert_eq!(slew_plan(12.0, -10.0, east, false).len(), 1);
        assert_eq!(slew_plan(12.0, -10.0, east, true).len(), 2);
        assert_eq!(current((5.0, -3.25, 7.5), false), ("37.5".into(), "-3.2".into(), false));
        assert_eq!(current((7.5, 3.0, 7.5), true).2, false);
        assert_eq!(current((7.5, 3.0, 7.5), false).2, true);
    }

    #[test]
    fn custom_location_round_trip() {
        let mut p = Mem::default();
        let coords = (10.0, 30.0, 7.5);
        assert_eq!(initial_target(&p, false, coords, Some(45.0)), Target { offset: 5, dec: 0, east: true });
        save_custom(&mut p, Target { offset: 15, dec: -10, east: false }, false);
        // The mount's side unless single-sided.
        assert_eq!(initial_target(&p, false, coords, Some(45.0)), Target { offset: 15, dec: -10, east: true });
        assert_eq!(initial_target(&p, true, coords, Some(45.0)), Target { offset: 5, dec: 0, east: true });
    }

    #[test]
    fn evaluation_texts() {
        let cal = Calibration {
            x_rate: 0.01,
            y_rate: 0.01,
            x_angle: 0.0,
            y_angle: radians(90.0),
            declination: radians(5.0),
            ..Calibration::default()
        };
        let d = CalibrationDetails { ra_step_count: 12, dec_step_count: 12, ..CalibrationDetails::default() };
        let e = evaluate(Some((&cal, &d)), true);
        assert_eq!(e.verdict, Verdict::Good, "{e:?}");
        // Unknown guide speeds (-1) print as PHD2 prints them.
        assert_eq!(
            e.log[0],
            "CalAsst: Spds: -240.6X,-240.6X, Dec: 5.0, Rates: 2.4, 2.4, Steps: 12,12, Ortho: 0.00, Rates: 1.00 \
             (Expect) vs 1.00 (Act)"
        );
        assert_eq!(evaluate(Some((&cal, &d)), false).log[0], "CalAsst: Spds: N/A, Steps: 12,12, Ortho: 0.00, ");
        let tilted = Calibration { y_angle: radians(99.0), declination: radians(25.0), ..cal.clone() };
        let e = evaluate(Some((&tilted, &d)), false);
        assert_eq!((e.verdict, e.reasons.as_str()), (Verdict::Acceptable, "(Orthogonality, Sky location)"));
        assert_eq!(e.log[1], "CalAsst: acceptable result, (Orthogonality, Sky location)");
        let few = CalibrationDetails { ra_step_count: 3, dec_step_count: 3, ..d.clone() };
        let e = evaluate(Some((&cal, &few)), false);
        assert_eq!(e.verdict, Verdict::Acceptable);
        assert_eq!(evaluate(None, true).verdict, Verdict::Failed);
        assert!(explanation(85.0, true).starts_with("Calibration is likely to fail"));
        assert_eq!(explanation(10.0, false), "");
        assert!(sanity_message(0.0008, -1.0, |_| (750, 750)).unwrap().contains("too slow"));
        assert!(sanity_message(0.0015, -1.0, |_| (750, 750)).unwrap().contains("below the minimum"));
        assert!(sanity_message(0.0042, -1.0, |_| (750, 1500)).unwrap().contains("can be adjusted"));
        assert_eq!(sanity_message(0.0042, -1.0, |_| (750, 800)), None);
    }
}
