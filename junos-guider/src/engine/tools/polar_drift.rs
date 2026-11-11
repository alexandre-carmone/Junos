//! Polar drift alignment: with the mount tracking near the pole and guiding
//! off, the selected star drifts along a straight line whose direction and
//! speed give the direction and size of the polar alignment error. PHD2 fits
//! x(t) and y(t) by least squares on every frame, turns the drift into a
//! target position for the star (where it would sit with the mount aligned)
//! and the user then moves the mount's altitude and azimuth until the star
//! reaches the target circle.
//!
//! Phases: `Drift` while samples are taken (Start → Stop), then `Adjust`
//! once stopped with a result: PHD2 keeps drawing the line from the last
//! measured position to the target until the next Start, which forgets the
//! previous run.
//!
//! Ported from PHD2 `src/polardrift_tool.h`, `src/polardrift_toolwin.h`, `src/polardrift_toolwin.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use crate::engine::calibration::{ProfileAccess, degrees, norm, radians};
use crate::engine::log::cfmt::fmt_f;
use crate::engine::traits::Point;

/// The tool's profile keys (`polardrift_toolwin.cpp:132,141`).
pub mod keys {
    /// 1 north, -1 south.
    pub const HEMISPHERE: &str = "/PolarDriftTool/Hemisphere";
    /// -1 when the image is mirrored (e.g. through an OAG), 1 otherwise.
    pub const MIRROR: &str = "/PolarDriftTool/Mirror";
}

/// The window's instructions (`c_instr`, `polardrift_toolwin.cpp:155`), one
/// step per line.
pub const INSTRUCTIONS: [&str; 7] = [
    "Slew to near the Celestial Pole.",
    "Make sure tracking is ON.",
    "Select a guide star on the main display.",
    "Click Start.",
    "Wait for the display to stabilise.",
    "Click Stop.",
    "Adjust your mount's altitude and azimuth to place the guide star in its target circle.",
];

/// Seconds of time per radian of hour angle: `24 * 3600 / 2 / M_PI`, an
/// integer product halved in integers (43200, exact), then one double
/// division.
const FACTOR: f64 = 43200.0 / std::f64::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No result yet.
    Idle,
    /// Collecting samples.
    Drift,
    /// Stopped with a result: the star is to be moved to `target`.
    Adjust,
}

/// The guide outputs PHD2 turns off while drifting and restores at Stop
/// (`m_savePrimaryMountEnabled`, `m_saveSecondaryMountEnabled`). `None` when
/// there is no such mount.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuideOutputs {
    pub primary: Option<bool>,
    pub secondary: Option<bool>,
}

/// The fit after the latest sample (`WatchDrift`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PolarDriftResult {
    /// Seconds since the first sample.
    pub time: f64,
    /// Drift rate × seconds per radian: the polar alignment error, px.
    pub offset_px: f64,
    /// Direction from the star to the pole in the image, degrees.
    pub alpha_deg: f64,
    /// The latest star position and where it should be.
    pub current: Point,
    pub target: Point,
    /// `offset · scale / 60`.
    pub pa_error_arcmin: f64,
    /// `norm(-alpha, -180, 180)`, the angle PHD2 shows.
    pub angle_deg: f64,
}

/// The tool's state (`PolarDriftToolWin`).
#[derive(Clone, Debug)]
pub struct PolarDrift {
    pixel_scale: f64,
    hemi: i32,
    mirror: i32,
    drifting: bool,
    saved: GuideOutputs,
    t0: f64,
    // PHD2 also sums x², y² and x·y; nothing reads them.
    sum_t: f64,
    sum_t2: f64,
    sum_x: f64,
    sum_y: f64,
    sum_tx: f64,
    sum_ty: f64,
    num: i64,
    tnow: f64,
    offset: f64,
    alpha: f64,
    current: Point,
    target: Point,
    status: [String; 3],
    log: Vec<String>,
}

impl PolarDrift {
    /// Open the tool (`PolarDriftToolWin()`): the hemisphere comes from the
    /// site latitude when the pointing source knows it, else the profile.
    pub fn new(p: &impl ProfileAccess, pixel_scale: f64, site_lat: Option<f64>) -> Self {
        let mut t = Self {
            pixel_scale,
            hemi: p.get_i32(keys::HEMISPHERE, 1),
            mirror: p.get_i32(keys::MIRROR, 1),
            drifting: false,
            saved: GuideOutputs::default(),
            t0: 0.0,
            sum_t: 0.0,
            sum_t2: 0.0,
            sum_x: 0.0,
            sum_y: 0.0,
            sum_tx: 0.0,
            sum_ty: 0.0,
            num: 0,
            tnow: 0.0,
            offset: 0.0,
            alpha: 0.0,
            current: Point::default(),
            target: Point::default(),
            status: Default::default(),
            log: Vec::new(),
        };
        t.fill_panel(site_lat);
        t
    }

    /// `FillPanel`'s one side effect: a known site latitude overrides the
    /// hemisphere (and PHD2 greys the choice out).
    fn fill_panel(&mut self, site_lat: Option<f64>) {
        if let Some(lat) = site_lat {
            self.hemi = if lat >= 0.0 { 1 } else { -1 };
        }
    }

    /// `OnHemi`. PHD2 greys the choice out while the site latitude is known,
    /// so nothing happens then and the profile is left alone.
    pub fn set_hemisphere(&mut self, p: &mut impl ProfileAccess, north: bool, site_lat: Option<f64>) {
        if site_lat.is_some() {
            return;
        }
        let hemi = if north { 1 } else { -1 };
        p.set_i32(keys::HEMISPHERE, hemi);
        self.hemi = hemi;
        self.fill_panel(site_lat);
    }

    /// `OnMirror`.
    pub fn set_mirror(&mut self, p: &mut impl ProfileAccess, mirrored: bool, site_lat: Option<f64>) {
        let mirror = if mirrored { -1 } else { 1 };
        p.set_i32(keys::MIRROR, mirror);
        self.mirror = mirror;
        self.fill_panel(site_lat);
    }

    /// Start (`OnStart` while not drifting). On `Ok` the engine turns the
    /// guide outputs of `outputs` off; [`stop`](Self::stop) hands them back.
    /// The previous result is forgotten at the first sample.
    pub fn start(
        &mut self,
        calibrating_or_guiding: bool,
        locked: bool,
        outputs: GuideOutputs,
        site_lat: Option<f64>,
    ) -> Result<(), &'static str> {
        if self.drifting {
            return Ok(());
        }
        let refusal = if calibrating_or_guiding {
            "Please wait till Calibration is done and/or stop guiding"
        } else if !locked {
            "Please select a star"
        } else {
            ""
        };
        if !refusal.is_empty() {
            self.status[0] = refusal.to_string();
            return Err(refusal);
        }
        self.saved = outputs;
        self.num = 0;
        self.drifting = true;
        self.fill_panel(site_lat);
        Ok(())
    }

    /// Stop (`OnStart` while drifting): the guide outputs to restore, or
    /// `None` when the tool was not drifting. The result stays.
    pub fn stop(&mut self, site_lat: Option<f64>) -> Option<GuideOutputs> {
        if !self.drifting {
            return None;
        }
        self.drifting = false;
        // PHD2 shows "Polar Drift alignment stopped", then at once this, in
        // the first field. The offset and angle are the last fit's, or a
        // previous run's when no two samples came in.
        self.status[0] = format!(
            "PA err(arcmin): {} Angle (deg): {}",
            fmt_f(self.offset * self.pixel_scale / 60.0, 1),
            fmt_f(norm(-self.alpha, -180.0, 180.0), 1)
        );
        self.log.push("Polar Drift alignment stopped".to_string());
        self.fill_panel(site_lat);
        Some(self.saved)
    }

    /// `OnClose`: stops a drift first.
    pub fn close(&mut self, site_lat: Option<f64>) -> Option<GuideOutputs> {
        let restore = self.stop(site_lat);
        self.log.push("Close PolarDriftTool".to_string());
        restore
    }

    /// One frame in the Selected state (`UpdateState` → `WatchDrift`): `t`
    /// is the UTC wall time as integer milliseconds over 1000 (PHD2:
    /// `wxGetUTCTimeMillis().GetValue() / 1000.0`; a finer time changes the
    /// slopes' last digits), `pos` the guider's current star position.
    /// Nothing happens unless drifting.
    pub fn add_sample(&mut self, t: f64, pos: Point) {
        if !self.drifting {
            return;
        }
        let mut tnow = t;
        self.current = pos;
        self.num += 1;
        if self.num <= 1 {
            self.sum_t = 0.0;
            self.sum_t2 = 0.0;
            self.sum_x = 0.0;
            self.sum_y = 0.0;
            self.sum_tx = 0.0;
            self.sum_ty = 0.0;
            self.offset = 0.0;
            self.alpha = 0.0;
            self.t0 = tnow;
        }
        tnow -= self.t0;
        self.tnow = tnow;
        self.sum_t += tnow;
        self.sum_t2 += tnow * tnow;
        self.sum_x += pos.x;
        self.sum_y += pos.y;
        self.sum_tx += tnow * pos.x;
        self.sum_ty += tnow * pos.y;
        if self.num <= 1 {
            return;
        }
        let n = self.num as f64;
        let xslope = (n * self.sum_tx - self.sum_t * self.sum_x) / (n * self.sum_t2 - self.sum_t * self.sum_t);
        let yslope = (n * self.sum_ty - self.sum_t * self.sum_y) / (n * self.sum_t2 - self.sum_t * self.sum_t);
        let theta = degrees(yslope.atan2(xslope));
        // The star turns clockwise round the north pole and anticlockwise
        // round the south one, so the pole lies 90° to one side of the drift;
        // pixels run y-down, and a mirrored image swaps the side again.
        self.alpha = theta + (self.hemi * 90 * self.mirror) as f64;
        self.offset = xslope.hypot(yslope) * FACTOR;
        self.target = Point::new(
            pos.x + self.offset * radians(self.alpha).cos(),
            pos.y + self.offset * radians(self.alpha).sin(),
        );
        self.log.push(format!(
            "Polar Drift: m_hemi {} m_mirror {} m_pxScale {}",
            self.hemi,
            self.mirror,
            fmt_f(self.pixel_scale, 1)
        ));
        self.log.push(format!(
            "Polar Drift: m_num {} m_t0 {} tnow {} m_current(X,Y): {},{}",
            self.num,
            fmt_f(self.t0, 1),
            fmt_f(tnow, 1),
            fmt_f(pos.x, 1),
            fmt_f(pos.y, 1)
        ));
        self.log.push(format!(
            "Polar Drift: slope(X,Y) {},{} m_offset {} theta {} m_alpha {}",
            fmt_f(xslope, 4),
            fmt_f(yslope, 4),
            fmt_f(self.offset, 1),
            fmt_f(theta, 1),
            fmt_f(self.alpha, 1)
        ));
        self.log.push(format!(
            "Polar Drift: m_target(X,Y) {},{}",
            fmt_f(self.target.x, 1),
            fmt_f(self.target.y, 1)
        ));
        // `%.f` here, `%.1f` at stop.
        self.status = [
            format!("Time {}s", fmt_f(tnow, 0)),
            format!("PA Err: {} min", fmt_f(self.offset * self.pixel_scale / 60.0, 0)),
            format!("Angle: {} deg", fmt_f(norm(-self.alpha, -180.0, 180.0), 0)),
        ];
    }

    /// The fit, once two samples are in (PHD2 draws nothing before).
    pub fn result(&self) -> Option<PolarDriftResult> {
        if self.num < 2 {
            return None;
        }
        Some(PolarDriftResult {
            time: self.tnow,
            offset_px: self.offset,
            alpha_deg: self.alpha,
            current: self.current,
            target: self.target,
            pa_error_arcmin: self.offset * self.pixel_scale / 60.0,
            angle_deg: norm(-self.alpha, -180.0, 180.0),
        })
    }

    pub fn phase(&self) -> Phase {
        if self.drifting {
            Phase::Drift
        } else if self.num >= 2 {
            Phase::Adjust
        } else {
            Phase::Idle
        }
    }

    pub fn is_drifting(&self) -> bool {
        self.drifting
    }

    /// 1 north, -1 south.
    pub fn hemisphere(&self) -> i32 {
        self.hemi
    }

    pub fn mirrored(&self) -> bool {
        self.mirror == -1
    }

    /// The window's three status-bar fields.
    pub fn status(&self) -> &[String; 3] {
        &self.status
    }

    /// The debug-log lines written since the last call.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tools::test_util::{Mem, fnv1a};

    fn drifting(scale: f64, site_lat: Option<f64>) -> PolarDrift {
        let mut t = PolarDrift::new(&Mem::default(), scale, site_lat);
        t.start(false, true, GuideOutputs { primary: Some(true), secondary: None }, site_lat).unwrap();
        t
    }

    #[test]
    fn linear_track_gives_offset_and_angle() {
        // x = 0.01 t, y = 0.02 t from (100, 200), one frame every 2 s.
        let mut t = drifting(1.5, None);
        let t0 = 1_762_862_400.0;
        for k in 0..30 {
            let s = 2.0 * k as f64;
            t.add_sample(t0 + s, Point::new(100.0 + 0.01 * s, 200.0 + 0.02 * s));
        }
        let r = t.result().unwrap();
        // From PHD2's C++ (WatchDrift on the same samples).
        assert_eq!(r.offset_px, 307.48141875627363);
        assert_eq!(r.alpha_deg, 153.43494882292202);
        assert_eq!(r.target.x, -174.43974166284272);
        assert_eq!(r.target.y, 338.66987083142124);
        assert_eq!(r.angle_deg, -153.43494882292202);
        assert_eq!(r.time, 58.0);
        // hypot(0.01, 0.02)·13751 and atan2(0.02, 0.01) + 90°.
        assert!((r.offset_px - 0.01f64.hypot(0.02) * FACTOR).abs() < 1e-6);
        assert!((r.alpha_deg - (degrees(0.02f64.atan2(0.01)) + 90.0)).abs() < 1e-9);
        assert!((r.pa_error_arcmin - r.offset_px * 1.5 / 60.0).abs() < 1e-12);
        let log = t.take_log();
        assert_eq!(log.len(), 29 * 4);
        // FNV-1a of the whole debug log of PHD2's C++ run.
        assert_eq!(fnv1a(&log), 0x24c484989e35a344);
        assert_eq!(
            &log[log.len() - 4..],
            [
                "Polar Drift: m_hemi 1 m_mirror 1 m_pxScale 1.5",
                "Polar Drift: m_num 30 m_t0 1762862400.0 tnow 58.0 m_current(X,Y): 100.6,201.2",
                "Polar Drift: slope(X,Y) 0.0100,0.0200 m_offset 307.5 theta 63.4 m_alpha 153.4",
                "Polar Drift: m_target(X,Y) -174.4,338.7",
            ]
        );
        // The status bar, from the harness too: `%.f` per frame, then the
        // stop line in the first field.
        assert_eq!(t.status(), &["Time 58s", "PA Err: 8 min", "Angle: -153 deg"]);
        t.stop(None).unwrap();
        assert_eq!(t.status(), &["PA err(arcmin): 7.7 Angle (deg): -153.4", "PA Err: 8 min", "Angle: -153 deg"]);
    }

    #[test]
    fn hemisphere_and_mirror_turn_the_target() {
        // South (from the site) and mirrored: -1 · 90 · -1 = +90 again.
        let mut t = PolarDrift::new(&Mem::default(), 1.0, Some(-33.0));
        assert_eq!(t.hemisphere(), -1);
        let mut p = Mem::default();
        t.set_mirror(&mut p, true, Some(-33.0));
        assert_eq!(p.get_i32(keys::MIRROR, 0), -1);
        t.start(false, true, GuideOutputs::default(), Some(-33.0)).unwrap();
        t.add_sample(0.0, Point::new(10.0, 10.0));
        t.add_sample(10.0, Point::new(10.0, 10.1));
        assert!((t.result().unwrap().alpha_deg - 180.0).abs() < 1e-9);
        // South, not mirrored: the pole is on the other side.
        let mut t = drifting(1.0, Some(-33.0));
        t.add_sample(0.0, Point::new(10.0, 10.0));
        t.add_sample(10.0, Point::new(10.0, 10.1));
        assert!(t.result().unwrap().alpha_deg.abs() < 1e-9);
    }

    #[test]
    fn site_latitude_overrides_the_saved_hemisphere() {
        let mut p = Mem::default();
        p.set_i32(keys::HEMISPHERE, -1);
        assert_eq!(PolarDrift::new(&p, 1.0, None).hemisphere(), -1);
        let mut t = PolarDrift::new(&p, 1.0, Some(45.0));
        assert_eq!(t.hemisphere(), 1);
        // The choice is greyed out while the site is known: nothing saved.
        t.set_hemisphere(&mut p, true, Some(45.0));
        assert_eq!(p.get_i32(keys::HEMISPHERE, 0), -1);
        assert_eq!(t.hemisphere(), 1);
        t.set_hemisphere(&mut p, true, None);
        assert_eq!(p.get_i32(keys::HEMISPHERE, 0), 1);
        t.set_hemisphere(&mut p, false, None);
        assert_eq!((t.hemisphere(), p.get_i32(keys::HEMISPHERE, 0)), (-1, -1));
    }

    #[test]
    fn phases_and_what_start_resets() {
        let mut t = PolarDrift::new(&Mem::default(), 1.0, None);
        assert_eq!(t.phase(), Phase::Idle);
        let outputs = GuideOutputs { primary: Some(true), secondary: Some(false) };
        assert_eq!(t.start(true, true, outputs, None), Err("Please wait till Calibration is done and/or stop guiding"));
        assert_eq!(t.start(false, false, outputs, None), Err("Please select a star"));
        assert_eq!(t.status()[0], "Please select a star");
        assert_eq!(t.stop(None), None);
        t.start(false, true, outputs, None).unwrap();
        assert_eq!(t.phase(), Phase::Drift);
        t.add_sample(100.0, Point::new(1.0, 1.0));
        assert_eq!(t.result(), None); // one sample: nothing to fit
        t.add_sample(101.0, Point::new(1.1, 1.0));
        assert!(t.result().is_some());
        assert_eq!(t.stop(None), Some(outputs));
        assert_eq!(t.phase(), Phase::Adjust);
        // Frames after Stop are ignored; the result stays.
        let r = t.result();
        t.add_sample(200.0, Point::new(50.0, 50.0));
        assert_eq!(t.result(), r);
        // A new start hides the old fit (m_num = 0) and drops its sums and
        // t0 at the first sample.
        t.start(false, true, GuideOutputs::default(), None).unwrap();
        assert_eq!(t.result(), None);
        assert_eq!(t.phase(), Phase::Drift);
        t.add_sample(500.0, Point::new(5.0, 5.0));
        assert_eq!(t.result(), None);
        t.add_sample(504.0, Point::new(5.0, 5.4));
        let r = t.result().unwrap();
        assert_eq!(r.time, 4.0);
        assert!((r.offset_px - 0.1 * FACTOR).abs() < 1e-9);
        assert_eq!(t.close(None), Some(GuideOutputs::default()));
        assert_eq!(t.take_log().last().map(String::as_str), Some("Close PolarDriftTool"));
    }
}
