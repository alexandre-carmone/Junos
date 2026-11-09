//! The tilt plate advice recomputed in the browser: the screws to turn, from
//! a run's measured tilt and the *current* plate settings. KStars bakes the
//! advice into `focus_abins_result` once, at the end of the run, and never
//! rebuilds it when a plate setting changes; the desktop Tilt Correction
//! Advisory recomputes on every control change, and so does this — a port of
//! `TiltCorrectionCalc::compute` (tiltcorrectioncalc.cpp), same constants,
//! same order of operations.

use serde_json::Value;

use crate::ws::{AbInsResult, AbInsScrew, AbInsTiltPlate};

/// Screw angles in the front view (telescope side). 3-point: point 1 at 150°
/// (upper right from behind), 2 at 270° (bottom), 3 at 30° (upper left).
/// 4-point: 45° (top left from behind), 135° (top right), 315° (bottom
/// left), 225° (bottom right).
const THREE_POINT: [f64; 3] = [150.0, 270.0, 30.0];
const FOUR_POINT: [f64; 4] = [45.0, 135.0, 315.0, 225.0];

/// The eight positions seen from behind the camera, by `position_index`.
const POSITIONS: [&str; 8] = ["right", "upperRight", "top", "upperLeft", "left", "lowerLeft", "bottom", "lowerRight"];

/// A screw's 8-way position from behind: the view is mirrored left-right
/// (`right = -cos`) and screen-Y is down (`up = sin`).
fn position_index(angle_rad: f64) -> usize {
    let right = -angle_rad.cos();
    let up = angle_rad.sin();
    let mut deg = up.atan2(right).to_degrees();
    if deg < 0.0 {
        deg += 360.0;
    }
    ((deg / 45.0).round() as usize) % 8
}

/// The advice for the run's tilt under the settings echo `s`
/// (`focus_abins_get_all_settings`: `tiltPlateType`, `tiltMode`,
/// `tiltRadius`, `tiltScrewPitch`, `tiltThreadIndex`, `tiltRotation`; the
/// ETA override is already applied by KStars). `None` when the tilt was not
/// measured or the settings have not loaded — the caller then falls back to
/// the result's own `tiltPlate`.
pub fn plate_advice(r: &AbInsResult, s: &Value) -> Option<AbInsTiltPlate> {
    if !r.tilt.ok || !s.is_object() {
        return None;
    }
    let plate_type = s["tiltPlateType"].as_i64().unwrap_or(0);
    let mode = s["tiltMode"].as_i64().unwrap_or(0);
    let radius_mm = s["tiltRadius"].as_f64().unwrap_or(64.0);
    let screw_pitch_mm = s["tiltScrewPitch"].as_f64().unwrap_or(0.5);
    let thread_index = s["tiltThreadIndex"].as_i64().unwrap_or(8);
    let plate_angle_deg = s["tiltRotation"].as_f64().unwrap_or(0.0);

    let base: &[f64] = if plate_type == 1 { &FOUR_POINT } else { &THREE_POINT };
    let (lr, tb) = (r.tilt.lr_microns, r.tilt.tb_microns);
    let (w, h) = (r.sensor.lr_span_microns, r.sensor.tb_span_microns);
    // Slopes in microns per micron across the sensor; the correction is the
    // opposite of the measured tilt, evaluated at each screw's rotated position.
    let lr_slope = if w > 0.0 { lr / w } else { 0.0 };
    let tb_slope = if h > 0.0 { tb / h } else { 0.0 };
    let radius = radius_mm * 1000.0;
    let phi = plate_angle_deg.to_radians();

    let angles: Vec<f64> = base.iter().map(|a| a.to_radians() + phi).collect();
    let mut adj: Vec<f64> = angles
        .iter()
        .map(|a| -(lr_slope * radius * a.cos() + tb_slope * radius * a.sin()) / 1000.0)
        .collect();
    // Push-only: shift so the smallest adjustment is zero.
    if mode == 1 {
        let min = adj.iter().cloned().fold(f64::INFINITY, f64::min);
        for a in &mut adj {
            *a -= min;
        }
    }

    let points = angles
        .iter()
        .zip(&adj)
        .enumerate()
        .map(|(i, (a, mm))| AbInsScrew {
            index: i as i64 + 1,
            angle_deg: a.to_degrees().rem_euclid(360.0),
            position: POSITIONS[position_index(*a)].to_string(),
            adjustment_mm: *mm,
            turns: if screw_pitch_mm != 0.0 { mm / screw_pitch_mm } else { 0.0 },
        })
        .collect();

    Some(AbInsTiltPlate {
        plate_type,
        mode,
        radius_mm,
        screw_pitch_mm,
        thread_index,
        plate_angle_deg,
        total_tilt_microns: lr.hypot(tb),
        max_adjustment_mm: adj.iter().fold(0.0_f64, |m, a| m.max(a.abs())),
        points,
    })
}
