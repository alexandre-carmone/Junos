//! Drift alignment: guide in RA only near the meridian (azimuth) or near
//! the horizon (altitude) and watch the Dec drift's trend line; its slope
//! gives the polar alignment error, drawn as a circle round the star.
//!
//! This module holds the window's texts and slew arithmetic and the guide
//! graph's trend lines (`GraphLogClientWindow`'s running sums) with the
//! error they give; the engine runs the window's modes.
//!
//! Ported from PHD2 `src/drift_tool.cpp`, `src/graph.cpp` (update_trend, trendline, the polar alignment error) — BSD-3-Clause, see THIRD_PARTY.md

use std::collections::VecDeque;

use crate::engine::calibration::{DEC_COMP_LIMIT, ProfileAccess, norm, radians};
use crate::engine::log::cfmt::fmt_f;

/// The slew fields are global keys (degrees); the notes, profile keys.
pub mod keys {
    pub const AZ_RA_OFS: &str = "/DriftTool/Az/SlewRAOfs";
    pub const AZ_DEC: &str = "/DriftTool/Az/SlewDec";
    pub const ALT_RA_OFS: &str = "/DriftTool/Alt/SlewRAOfs";
    pub const ALT_DEC: &str = "/DriftTool/Alt/SlewDec";
    pub const AZ_NOTES: &str = "/DriftTool/Az/Notes";
    pub const ALT_NOTES: &str = "/DriftTool/Alt/Notes";
    pub const GRAPH_LENGTH: &str = "/DriftTool/GraphLength";
}

/// `GraphLogWindow::DefaultMaxLength`: the graph's points.
pub const DEFAULT_GRAPH_LENGTH: i32 = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Az,
    Alt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Drift,
    Adjust,
}

/// `LoadRADec`: the slew fields, truncated as the int spinners show them
/// (`floor`).
pub fn load_slew(global: &impl ProfileAccess, phase: Phase) -> (f64, f64) {
    let (ra, dec) = match phase {
        Phase::Az => (global.get_f64(keys::AZ_RA_OFS, 0.0), global.get_f64(keys::AZ_DEC, 0.0)),
        Phase::Alt => (global.get_f64(keys::ALT_RA_OFS, -65.0), global.get_f64(keys::ALT_DEC, 0.0)),
    };
    (ra.floor(), dec.floor())
}

/// `SaveRADec`.
pub fn save_slew(global: &mut impl ProfileAccess, phase: Phase, ra: f64, dec: f64) {
    let (kr, kd) = match phase {
        Phase::Az => (keys::AZ_RA_OFS, keys::AZ_DEC),
        Phase::Alt => (keys::ALT_RA_OFS, keys::ALT_DEC),
    };
    global.set_f64(kr, ra);
    global.set_f64(kd, dec);
}

pub fn notes_key(phase: Phase) -> &'static str {
    match phase {
        Phase::Az => keys::AZ_NOTES,
        Phase::Alt => keys::ALT_NOTES,
    }
}

/// `UpdatePhaseState`'s title and instructions.
pub fn texts(phase: Phase) -> (&'static str, &'static str) {
    match phase {
        Phase::Az => (
            "Drift Align - Azimuth Adjustment",
            "Slew to near the Meridian and the Equator.\nPress Drift to measure drift, watch the Dec trend line.\nPress \
             Adjust and adjust your mount's azimuth.\nRepeat Drift/Adjust until alignment is complete.\nThen, click \
             Altitude to begin Altitude adjustment.",
        ),
        Phase::Alt => (
            "Drift Align - Altitude Adjustment",
            "Slew to a location near the Equator and the Eastern or Western horizon.\nPress Drift to measure drift, \
             watch the Dec trend line.\nPress Adjust and adjust your mount's altitude.\nRepeat Drift/Adjust until \
             alignment is complete.\nClick Azimuth to repeat Azimuth adjustment.",
        ),
    }
}

/// The Adjust mode's status.
pub fn adjust_status(phase: Phase) -> &'static str {
    match phase {
        Phase::Az => "Adjust azimuth, click Drift when done",
        Phase::Alt => "Adjust altitude, click Drift when done",
    }
}

/// `OnSlew`'s RA: the offset (degrees) from the sidereal time.
pub fn slew_ra(st: f64, ra_ofs: f64) -> f64 {
    norm(st + ra_ofs * 24.0 / 360.0, 0.0, 24.0)
}

/// `UpdateScopeCoordinates`: the RA offset from the meridian, degrees in
/// (−180, 180], and the two fields (`%+.f`).
pub fn current(ra: f64, dec: f64, st: f64) -> (f64, String, String) {
    let mut ofs = (ra - st) * (360.0 / 24.0);
    if ofs > 180.0 {
        ofs -= 360.0;
    }
    if ofs <= -180.0 {
        ofs += 360.0;
    }
    let f = |v: f64| format!("{}{}", if v >= 0.0 { "+" } else { "" }, fmt_f(v, 0));
    (ofs, f(ofs), f(dec))
}

/// `UpdateScopeCoordinates`' polar alignment circle correction, with the
/// site's latitude (degrees).
pub fn circle_correction(phase: Phase, ra_ofs_deg: f64, dec_deg: f64, lat: f64) -> f64 {
    match phase {
        Phase::Az => {
            let dec_r = radians(dec_deg);
            if dec_r.abs() < DEC_COMP_LIMIT {
                radians(90.0 - lat + dec_deg).cos() / dec_r.cos()
            } else {
                1.0
            }
        }
        Phase::Alt => {
            let ha_r = radians(ra_ofs_deg);
            let (cos_dec, cos_ha) = (radians(dec_deg).cos(), ha_r.cos());
            let c = (1.0 + cos_dec * cos_dec * (cos_ha * cos_ha - 1.0)).sqrt();
            if ra_ofs_deg.abs() > 15.0 { c / ha_r.sin().abs() } else { 1.0 }
        }
    }
}

/// `TrendLineAccum`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Accum {
    sum_xy: f64,
    sum_y: f64,
    sum_y2: f64,
}

impl Accum {
    /// `update_trend`.
    fn update(&mut self, nr: usize, max_nr: usize, new: f64, old: f64) {
        if nr < max_nr {
            self.sum_y += new;
            self.sum_xy += nr as f64 * new;
            self.sum_y2 += new * new;
        } else {
            self.sum_xy += (max_nr as f64 - 1.0) * new + old - self.sum_y;
            self.sum_y += new - old;
            self.sum_y2 += new * new - old * old;
        }
    }

    /// `trendline`: (slope per point, intercept).
    fn line(&self, nn: usize) -> (f64, f64) {
        let n = nn as f64;
        let sum_x = 0.5 * n * (n - 1.0);
        let denom = n * n * (n - 1.0) * ((2.0 * n - 1.0) / 6.0 - 0.25 * (n - 1.0));
        let a = (n * self.sum_xy - sum_x * self.sum_y) / denom;
        (a, (self.sum_y - a * sum_x) / n)
    }
}

/// The guide graph in RA/Dec mode, as far as the trend lines go: the
/// mount offsets (px) of the guide steps since it was cleared.
#[derive(Clone, Debug)]
pub struct Trend {
    length: usize,
    /// (UTC ms, RA, Dec).
    history: VecDeque<(i64, f64, f64)>,
    ra: Accum,
    dec: Accum,
}

/// The trend lines as the graph draws them, and the polar alignment error.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrendLines {
    /// Points plotted.
    pub points: usize,
    /// (slope per point, intercept), px; Dec negated (north down).
    pub ra: (f64, f64),
    pub dec: (f64, f64),
    /// ′, and the circle's radius in px; with a known pixel scale and the
    /// Dec within the compensation limit only.
    pub error_arcmin: Option<f64>,
    pub radius_px: Option<f64>,
}

impl Trend {
    pub fn new(length: i32) -> Self {
        Self { length: length.max(1) as usize, history: VecDeque::new(), ra: Accum::default(), dec: Accum::default() }
    }

    /// `OnButtonClear`.
    pub fn clear(&mut self) {
        *self = Trend::new(self.length as i32);
    }

    /// `AppendData`.
    pub fn push(&mut self, t_ms: i64, ra: f64, dec: f64) {
        let items = self.history.len().min(self.length);
        let oldest = self.history.get(self.history.len() - items).copied().unwrap_or((0, 0.0, 0.0));
        self.ra.update(items, self.length, ra, oldest.1);
        self.dec.update(items, self.length, dec, oldest.2);
        self.history.push_back((t_ms, ra, dec));
        if self.history.len() > self.length {
            self.history.pop_front();
        }
    }

    /// What the graph draws once it has 5 points: `sampling` is the pixel
    /// scale (1.0 unknown, then no error) and `dec` the pointing's
    /// (radians, `None` unknown, taken as 0).
    pub fn lines(&self, sampling: f64, dec: Option<f64>) -> Option<TrendLines> {
        let n = self.history.len().min(self.length);
        if n < 5 {
            return None;
        }
        let ra = self.ra.line(n);
        let d = self.dec.line(n);
        let dec_line = (-d.0, -d.1);
        let declination = dec.unwrap_or(0.0);
        let error = (sampling != 1.0 && declination.abs() <= DEC_COMP_LIMIT).then(|| {
            let (t0, t1) = (self.history[self.history.len() - n].0, self.history[self.history.len() - 1].0);
            let dt = (t1 - t0) as f64 / (1000.0 * 60.0);
            let ddec = (n as f64 - 1.0) * dec_line.0 * sampling;
            3.82 * ddec / (dt * declination.cos())
        });
        Some(TrendLines {
            points: n,
            ra,
            dec: dec_line,
            error_arcmin: error,
            radius_px: error.map(|e| (e * 60.0 / sampling).abs()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The trend goldens come from graph.cpp's update_trend and trendline
    // built in a harness, fed the same points.
    #[test]
    fn trend_lines_and_the_error() {
        let mut t = Trend::new(6);
        for i in 0..9 {
            let x = i as f64;
            t.push(i * 30_000, 0.1 * x - 0.3, 0.25 * x + 0.5 * (x * 1.7).sin());
        }
        let l = t.lines(2.0, Some(radians(10.0))).unwrap();
        assert_eq!(l.points, 6);
        assert!((l.ra.0 - 0.1).abs() < 1e-12 && (l.ra.1 - 0.0).abs() < 1e-12, "{l:?}");
        assert!((l.dec.0 - -0.30842527565110256).abs() < 1e-12, "{l:?}");
        assert!((l.dec.1 - -0.59626479391060083).abs() < 1e-12, "{l:?}");
        assert!((l.error_arcmin.unwrap() - -4.7854397952637013).abs() < 1e-9, "{l:?}");
        assert!(Trend::new(400).lines(2.0, None).is_none());
        let mut u = Trend::new(400);
        for i in 0..5 {
            u.push(i * 1000, 0.0, i as f64);
        }
        assert_eq!(u.lines(1.0, None).unwrap().error_arcmin, None);
        assert_eq!(u.lines(2.0, Some(1.2)).unwrap().error_arcmin, None);
    }

    #[test]
    fn slews_fields_and_correction() {
        assert!((slew_ra(23.0, 30.0) - 1.0).abs() < 1e-12);
        assert!((slew_ra(1.0, -65.0) - 20.666666666666668).abs() < 1e-12);
        assert_eq!(current(1.0, -4.6, 2.0), (-15.0, "-15".into(), "-5".into()));
        assert_eq!(current(14.0, 0.4, 1.0).0, -165.0);
        assert!((circle_correction(Phase::Az, 0.0, 0.0, 45.0) - radians(45.0).cos()).abs() < 1e-12);
        assert_eq!(circle_correction(Phase::Alt, 10.0, 0.0, 45.0), 1.0);
        let c = circle_correction(Phase::Alt, -60.0, 0.0, 45.0);
        assert!((c - 0.5 / radians(60.0).sin()).abs() < 1e-12, "{c}");
    }
}
