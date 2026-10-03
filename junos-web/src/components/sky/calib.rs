//! Measured camera frame — FOV and PA from the last plate solve, remembered
//! per camera + focal length in localStorage `sky_frame_calib`, so the FOV
//! boxes keep the solved frame across a reload until the next solve.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::local_storage;

const STORAGE_KEY: &str = "sky_frame_calib";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameCalib {
    pub fov_w_arcmin: f64,
    pub fov_h_arcmin: f64,
    pub pa_deg:       f64,
    /// `Date::now()` when the solve arrived.
    pub at_ms:        f64,
}

/// Where the frame in use comes from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CalibSource {
    /// A solve received since this page loaded.
    Solved,
    /// Remembered from an earlier session.
    Saved,
}

/// `"<camera>|<focal mm>"`. KStars keys its effective FOV per train and focal
/// too (`Align::getEffectiveFOV`, align_fov.cpp:457), so another scope or
/// reducer starts again from the nominal frame.
pub fn calib_key(camera: &str, focal_mm: f64) -> Option<String> {
    (!camera.is_empty() && focal_mm > 0.0).then(|| format!("{camera}|{focal_mm:.0}"))
}

fn read_all() -> HashMap<String, FrameCalib> {
    local_storage()
        .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

pub fn load(key: &str) -> Option<FrameCalib> {
    read_all()
        .remove(key)
        .filter(|c| c.fov_w_arcmin > 0.0 && c.fov_h_arcmin > 0.0)
}

pub fn save(key: &str, calib: FrameCalib) {
    let mut all = read_all();
    all.insert(key.to_string(), calib);
    if let (Some(s), Ok(v)) = (local_storage(), serde_json::to_string(&all)) {
        let _ = s.set_item(STORAGE_KEY, &v);
    }
}
