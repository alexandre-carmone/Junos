//! The gear simulator: a guide camera, a mount and an AO that run with no
//! hardware, for the tests and `--sim`.
//!
//! A port of PHD2's camera simulator: 20 stars at fixed random places,
//! hot pixels, read noise, periodic error, seeing, Dec drift, Dec backlash
//! and stiction, the camera angle and the pier side, the AO's offset, a
//! comet and clouds. Guide pulses move the field through the camera's
//! ST-4 port; the same pulse is the simulated mount's [`GuideOutput`].
//!
//! Everything shares one state behind the handles [`SimCamera`],
//! [`SimMount`], [`SimAo`] and [`SimClock`]. Time is simulated: an exposure
//! advances it by its length plus the 50 ms download, a pulse by its
//! length, an AO step by 5 ms, and [`Clock::sleep_ms`] by what it is given,
//! so a run is fast and the same every time. The star field, the noise and
//! the seeing come from [`Rng`] seeded with 2, re-seeded on each connect.
//!
//! PHD2's mount for the simulator is the "On Camera" ST-4 output with no
//! position. Here [`SimMount`] also reports a position (RA 0 h, Dec 25°,
//! the declination PHD2's pulses assume when none is known), the pier side
//! and the guide rate, so the engine's pointing features have a source.
//! PHD2's cooler and rotator simulators are left out: the camera seam has
//! no cooler and there is no rotator device yet.
//!
//! Ported from PHD2 src/gear_simulator.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Local, TimeDelta};

use crate::engine::calibration::{ProfileAccess, norm, radians};
use crate::engine::camera::{CaptureKind, GuideCamera};
use crate::engine::image::UsImage;
use crate::engine::mount::{GuideOutput, PointingSource};
use crate::engine::rng::Rng;
use crate::engine::traits::{CancelToken, Clock, Dir, MoveResult, PierSide, Rect, c_int};

/// The camera's fixed gain and offset in the noise and star formulas
/// (`CameraSimulator::Capture`).
const GAIN: i32 = 30;
const OFFSET: i32 = 100;
/// `SimCamParams::clouds_inten`.
const CLOUDS_INTEN: f64 = 50.0;
/// The canned periodic error (`FillImage`): periods s, amplitudes ″,
/// phases s, and the peak the scale is relative to.
const PE_PERIOD: [f64; 5] = [230.5, 122.0, 49.4, 9.56, 76.84];
const PE_AMP: [f64; 5] = [2.02, 0.69, 0.22, 0.137, 0.14];
const PE_PHASE: [f64; 5] = [0.0, 1.4, 98.8, 35.9, 150.4];
const PE_MAX_AMP: f64 = 4.85;
/// `SimAoParams`: px per AO step and the AO's angle to the camera, degrees.
const AO_SCALE: f64 = 0.10;
const AO_ANGLE: f64 = 35.0;
/// `StepGuiderSimulator::Step`'s latency.
const AO_MS_PER_STEP: u64 = 5;
/// The star image, `render_star`'s 5×5 kernel.
const STAR: [[f64; 5]; 5] = [
    [0.0, 0.8, 2.2, 0.8, 0.0],
    [0.8, 16.6, 46.1, 16.6, 0.8],
    [2.2, 46.1, 128.0, 46.1, 2.2],
    [0.8, 16.6, 46.1, 16.6, 0.8],
    [0.0, 0.8, 2.2, 0.8, 0.0],
];
/// The simulated clock's wall time at its zero: 2026-01-01 00:00:00 UTC.
const WALL_EPOCH: i64 = 1_767_225_600;
const INTERRUPTED: &str = "capture interrupted";

/// `SimCamParams`, in the simulator's internal units: what
/// `load_sim_params` makes of the `/SimCam/*` keys.
#[derive(Clone, Debug, PartialEq)]
pub struct SimParams {
    /// Unbinned frame size, px.
    pub width: u32,
    pub height: u32,
    /// The star placement margin. PHD2 subtracts it twice from the range
    /// but never adds it back, so stars still reach the left and top edges.
    pub border: u32,
    pub nr_stars: u32,
    pub nr_hot_pixels: u32,
    pub noise_multiplier: f64,
    /// Dec backlash, px.
    pub dec_backlash: f64,
    /// Periodic error, peak to peak, arc-sec.
    pub pe_scale: f64,
    /// Dec drift, px/s.
    pub dec_drift_rate: f64,
    /// Seeing FWHM, arc-sec.
    pub seeing_scale: f64,
    /// Camera angle, degrees.
    pub cam_angle: f64,
    /// Guide rate, arc-sec/s.
    pub guide_rate: f64,
    pub pier_side: PierSide,
    pub reverse_dec_pulse_on_west_side: bool,
    /// Cloud cover, 0–1.
    pub clouds_opacity: f64,
    /// Arc-sec per pixel (`pFrame->GetCameraPixelScale()` at connect, 1.0
    /// without a pixel size and focal length).
    pub image_scale: f64,
    pub use_pe: bool,
    pub use_stiction: bool,
    /// The canned five-term PE, else one cosine of the custom amplitude
    /// (″) and period (s).
    pub use_default_pe_params: bool,
    pub custom_pe_amp: f64,
    pub custom_pe_period: f64,
    pub show_comet: bool,
    /// Comet motion, px per hour.
    pub comet_rate_x: f64,
    pub comet_rate_y: f64,
    pub frame_download_ms: u32,
    /// `/SimAo/max_steps`.
    pub ao_max_position: i32,
}

/// A profile with no entries: every read gives PHD2's default.
struct NoProfile;

impl ProfileAccess for NoProfile {
    fn has(&self, _key: &str) -> bool {
        false
    }
    fn get_f64(&self, _key: &str, default: f64) -> f64 {
        default
    }
    fn get_i32(&self, _key: &str, default: i32) -> i32 {
        default
    }
    fn get_str(&self, _key: &str, default: &str) -> String {
        default.to_string()
    }
    fn set_f64(&mut self, _key: &str, _value: f64) {}
    fn set_i32(&mut self, _key: &str, _value: i32) {}
    fn set_str(&mut self, _key: &str, _value: &str) {}
}

impl SimParams {
    /// PHD2's defaults at `image_scale` ″/px.
    pub fn new(image_scale: f64) -> SimParams {
        SimParams::from_profile(&NoProfile, image_scale)
    }

    /// `load_sim_params`: the `/SimCam/*` keys with PHD2's defaults and
    /// range checks, the arc-sec settings turned into px.
    pub fn from_profile(p: &impl ProfileAccess, image_scale: f64) -> SimParams {
        // `range_check`: wxMin(wxMax(v, lo), hi).
        let range = |key: &str, default: f64, lo: f64, hi: f64| p.get_f64(key, default).max(lo).min(hi);
        let flag = |key: &str, default: bool| p.get_i32(key, default as i32) != 0;
        SimParams {
            width: 752,
            height: 580,
            border: 12,
            nr_stars: p.get_i32("/SimCam/nr_stars", 20) as u32,
            nr_hot_pixels: p.get_i32("/SimCam/nr_hot_pixels", 8) as u32,
            noise_multiplier: p.get_f64("/SimCam/noise", 2.0),
            use_pe: flag("/SimCam/use_pe", true),
            use_stiction: flag("/SimCam/use_stiction", false),
            use_default_pe_params: flag("/SimCam/use_default_pe", true),
            custom_pe_amp: p.get_f64("/SimCam/pe_cust_amp", 2.0),
            custom_pe_period: p.get_f64("/SimCam/pe_cust_period", 240.0),
            dec_drift_rate: range("/SimCam/dec_drift", 5.0, -30.0, 30.0) / (image_scale * 60.0),
            dec_backlash: range("/SimCam/dec_backlash", 5.0, 0.0, 100.0) / image_scale,
            pe_scale: range("/SimCam/pe_scale", 5.0, 0.0, 30.0),
            seeing_scale: range("/SimCam/seeing_scale", 2.0, 0.0, 5.0),
            cam_angle: p.get_f64("/SimCam/cam_angle", 15.0),
            clouds_opacity: p.get_f64("/SimCam/clouds_opacity", 0.0),
            guide_rate: range("/SimCam/guide_rate", 15.0, 0.0, 15.0),
            pier_side: match p.get_i32("/SimCam/pier_side", PierSide::East as i32) {
                0 => PierSide::East,
                1 => PierSide::West,
                _ => PierSide::Unknown,
            },
            reverse_dec_pulse_on_west_side: flag("/SimCam/reverse_dec_pulse_on_west_side", true),
            show_comet: flag("/SimCam/show_comet", false),
            comet_rate_x: p.get_f64("/SimCam/comet_rate_x", 555.0),
            comet_rate_y: p.get_f64("/SimCam/comet_rate_y", -123.4),
            frame_download_ms: p.get_i32("/SimCam/frame_download_ms", 50) as u32,
            image_scale,
            ao_max_position: p.get_i32("/SimAo/max_steps", 45),
        }
    }
}

/// `BacklashVal`: a position with backlash. Moving past the upper limit
/// carries it along, as does moving below `upper − amount`; in between the
/// reported value (the upper limit) stays put.
#[derive(Clone, Copy, Debug, PartialEq)]
struct BacklashVal {
    cur: f64,
    upper: f64,
    amount: f64,
}

impl BacklashVal {
    fn new(amount: f64) -> Self {
        BacklashVal { cur: 0.0, upper: amount, amount }
    }

    fn val(&self) -> f64 {
        self.upper
    }

    fn incr(&mut self, d: f64) {
        self.cur += d;
        if d > 0.0 {
            if self.cur > self.upper {
                self.upper = self.cur;
            }
        } else if d < 0.0 && self.cur < self.upper - self.amount {
            self.upper = self.cur + self.amount;
        }
    }
}

/// `StictionSim`: a Dec reversal longer than 300 ms loses a third of its
/// distance, given back on the next pulse if it keeps the new direction.
#[derive(Clone, Copy, Debug, Default)]
struct StictionSim {
    last: Option<Dir>,
    pending: bool,
    adjustment: f64,
}

impl StictionSim {
    fn adjustment(&mut self, dir: Dir, duration: u32, distance: f64) -> f64 {
        let mut rslt = 0.0;
        if let Some(last) = self.last {
            if duration > 300 && dir != last && !self.pending {
                self.adjustment = distance / 3.0;
                self.pending = true;
                rslt = -self.adjustment;
            } else if self.pending {
                if dir == last {
                    rslt = self.adjustment;
                    self.adjustment = 0.0;
                }
                self.pending = false;
            }
        }
        self.last = Some(dir);
        rslt
    }
}

/// A star of the field, in RA/Dec px around the frame centre.
#[derive(Clone, Copy, Debug)]
struct SimStar {
    x: f64,
    y: f64,
    inten: f64,
}

/// `SimCamState` plus what PHD2 keeps elsewhere: `FillImage`'s statics, the
/// camera's binning and connection, the simulated clock, the mount's
/// position and the AO's.
struct SimState {
    p: SimParams,
    stars: Vec<SimStar>,
    hot_pixels: Vec<(i32, i32)>,
    /// The guide pulses' sum in RA, px (no RA backlash).
    ra_ofs: f64,
    dec_ofs: BacklashVal,
    cum_dec_drift: f64,
    /// ms, on the camera's stopwatch.
    last_exposure_time: i64,
    /// Not reset by a reconnect, as in PHD2.
    stiction: StictionSim,
    /// `FillImage`'s function statics `s_prev_ra` (h) and `s_ra_offset`
    /// (s): never reset, as in PHD2.
    prev_ra: f64,
    ra_offset: f64,
    rng: Rng,
    connected: bool,
    binning: u8,
    /// Simulated time, ms since the simulator was made: the clock's
    /// `mono` and the camera's stopwatch.
    now_ms: u64,
    ra_h: f64,
    /// `None`: no position (PHD2's `UNKNOWN_DECLINATION`).
    dec_deg: Option<f64>,
    /// Local sidereal time at time 0, hours.
    lst0_h: f64,
    /// The AO's position in steps (right, up); `None` while it is
    /// disconnected (`s_sim_ao`).
    ao: Option<(i32, i32)>,
}

impl SimState {
    /// `SimCamState::Initialize`, on each connect: the star field and the
    /// hot pixels from seed 2, the offsets back to zero.
    fn initialize(&mut self) {
        let (w, h, border) = (self.p.width, self.p.height, self.p.border);
        self.rng = Rng::new(2);
        let mut stars: Vec<SimStar> = Vec::with_capacity(self.p.nr_stars as usize);
        for i in 0..self.p.nr_stars {
            let x = self.rng.below(w.wrapping_sub(2 * border)) as f64 - 0.5 * w as f64;
            let y = self.rng.below(h.wrapping_sub(2 * border)) as f64 - 0.5 * h as f64;
            let r = self.rng.below(90) as f64 / 3.0;
            let inten = if i == 10 { 30.1 } else { 0.1 + r * r * r / 9000.0 };
            // Star 3 sits 8 px from star 2, to test AutoFind's pair pruning;
            // star 10 always saturates.
            stars.push(if i == 3 {
                SimStar { x: stars[2].x + 8.0, y: stars[2].y + 8.0, inten: stars[2].inten }
            } else {
                SimStar { x, y, inten }
            });
        }
        self.stars = stars;
        self.hot_pixels =
            (0..self.p.nr_hot_pixels).map(|_| (self.rng.below(w) as i32, self.rng.below(h) as i32)).collect();
        self.ra_ofs = 0.0;
        self.dec_ofs = BacklashVal::new(self.p.dec_backlash);
        self.cum_dec_drift = 0.0;
        self.last_exposure_time = 0;
    }

    /// `WorkerThread::MilliSleep(ms, INT_ANY)`: true when interrupted.
    fn sleep(&mut self, ms: u64, cancel: &CancelToken) -> bool {
        if cancel.is_cancelled() {
            return true;
        }
        self.now_ms += ms;
        false
    }

    fn lst(&self) -> f64 {
        norm(self.lst0_h + self.now_ms as f64 / 1000.0 * 1.002_737_909_35 / 3600.0, 0.0, 24.0)
    }

    /// The declination a pointing source reports: clamped to ±89° like the
    /// INDI mount's (`PointingSource::declination_radians`).
    fn declination_radians(&self) -> Option<f64> {
        self.dec_deg.map(|d| radians(d.clamp(-89.0, 89.0)))
    }

    /// The periodic error at worm time `now` (s), px.
    fn pe(&self, now: f64) -> f64 {
        if !self.p.use_pe {
            return 0.0;
        }
        if self.p.use_default_pe_params {
            let mut pe = 0.0;
            for i in 0..PE_PERIOD.len() {
                pe += PE_AMP[i] * ((now - PE_PHASE[i]) / PE_PERIOD[i] * 2.0 * PI).cos();
            }
            pe * (self.p.pe_scale / (PE_MAX_AMP * self.p.image_scale))
        } else {
            self.p.custom_pe_amp * (now / self.p.custom_pe_period * 2.0 * PI).cos() / self.p.image_scale
        }
    }

    /// `rand_normal`: two independent standard normals (Box–Muller). PHD2
    /// draws `u` in [0, 1], where 0 gives an infinite shift; (0, 1] here.
    fn rand_normal(&mut self) -> (f64, f64) {
        let u = 1.0 - self.rng.next_f64();
        let v = self.rng.next_f64();
        let a = (-2.0 * u.ln()).sqrt();
        let p = 2.0 * PI * v;
        (a * p.cos(), a * p.sin())
    }

    /// `CameraSimulator::ST4PulseGuideScope`: move the field by `ms` of
    /// guiding, then wait for the pulse.
    fn pulse(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) {
        // render_star divides positions by the binning, so the distance is
        // in unbinned px.
        let mut d = self.p.guide_rate * self.binning as f64 * ms as f64 / (1000.0 * self.p.image_scale);
        if matches!(dir, Dir::West | Dir::East) {
            // RA moves less away from the equator; 25° when unknown.
            d *= self.declination_radians().unwrap_or(radians(25.0)).cos();
        }
        if self.p.use_stiction && matches!(dir, Dir::North | Dir::South) {
            d += self.stiction.adjustment(dir, ms, d);
        }
        // After a pier flip North and South swap, like an ASCOM mount's.
        let dir = if self.p.pier_side == PierSide::West && self.p.reverse_dec_pulse_on_west_side {
            match dir {
                Dir::North => Dir::South,
                Dir::South => Dir::North,
                other => other,
            }
        } else {
            dir
        };
        match dir {
            Dir::West => self.ra_ofs += d,
            Dir::East => self.ra_ofs -= d,
            Dir::North => self.dec_ofs.incr(d),
            Dir::South => self.dec_ofs.incr(-d),
        }
        self.sleep(ms as u64, cancel);
    }

    /// `CameraSimulator::Capture`: wait out the exposure but its last
    /// 5 ms, render (so a pulse made meanwhile shows), then wait out the
    /// download.
    fn capture(
        &mut self,
        duration: u32,
        kind: CaptureKind,
        subframe: Option<Rect>,
        cancel: &CancelToken,
    ) -> Result<UsImage, String> {
        let start = self.now_ms;
        if duration > 5 && self.sleep(duration as u64 - 5, cancel) {
            return Err(INTERRUPTED.into());
        }
        let bin = self.binning.max(1) as i32;
        let (w, h) = (self.p.width as i32 / bin, self.p.height as i32 / bin);
        // A subframe that does not fit is dropped. PHD2 lets a negative
        // origin through, to read before the buffer.
        let sub = subframe.filter(|r| r.w > 0 && r.h > 0 && r.x >= 0 && r.y >= 0 && r.right() < w && r.bottom() < h);
        let area = sub.unwrap_or(Rect::new(0, 0, w, h));
        let mut img = UsImage::new(w, h);
        let exptime = duration as i32;
        self.fill_noise(&mut img, area, exptime);
        self.fill_image(&mut img, area, exptime, kind.shutter_closed());
        img.subframe = sub;
        let total = duration as u64 + self.p.frame_download_ms as u64;
        let elapsed = self.now_ms - start;
        if elapsed < total && self.sleep(total - elapsed, cancel) {
            return Err(INTERRUPTED.into());
        }
        Ok(img)
    }

    /// `fill_noise`: the background, uniform noise over a dark level.
    /// Quirk kept: the `(unsigned short)` cast wraps past 65535 (long
    /// exposures at high noise come out dark).
    fn fill_noise(&mut self, img: &mut UsImage, area: Rect, exptime: i32) {
        let dark = GAIN as f64 / 10.0 * OFFSET as f64 * exptime as f64 / 100.0;
        for y in area.y..=area.bottom() {
            for x in area.x..=area.right() {
                let v = self.p.noise_multiplier * (dark + self.rng.below((GAIN * 100) as u32) as f64);
                *img.pixel_mut(x, y) = c_int(v) as u16;
            }
        }
    }

    /// `SimCamState::FillImage`: move the field by the periodic error, the
    /// guide pulses, the Dec drift and the seeing, rotate it onto the
    /// camera, add the AO's offset and render the stars, the comet, the
    /// clouds and the hot pixels.
    fn fill_image(&mut self, img: &mut UsImage, area: Rect, exptime: i32, shutter_closed: bool) {
        let cur_time = self.now_ms as i64;
        // PHD2 computes `last − cur` (gear_simulator.cpp:1098), which turns
        // the drift against its setting; fixed per the quirk ledger.
        let delta_time_ms = cur_time - self.last_exposure_time;
        self.last_exposure_time = cur_time;

        // The worm phase follows RA slews: a slew east moves the worm back.
        // Without a position PHD2 reads RA 0.
        let ra = if self.dec_deg.is_some() { self.ra_h } else { 0.0 };
        let dra = norm(ra - self.prev_ra, -12.0, 12.0) * (60.0 * 60.0 / 0.9973);
        self.prev_ra = ra;
        self.ra_offset += dra;
        let now = cur_time as f64 / 1000.0 - self.ra_offset;

        let pe = self.pe(now);
        self.cum_dec_drift += delta_time_ms as f64 * self.p.dec_drift_rate / 1000.0;
        let mut shift_x = pe + self.ra_ofs;
        let mut shift_y = self.cum_dec_drift + self.dec_ofs.val();
        if self.p.seeing_scale > 0.0 {
            let (sx, sy) = self.rand_normal();
            // FWHM, geometry, empirical.
            let sigma = self.p.seeing_scale / (2.345 * 1.4 * 2.4 * self.p.image_scale);
            shift_x += sx * sigma;
            shift_y += sy * sigma;
        }

        // PHD2 copies the pointing source's pier side here; the simulator's
        // own mount is the pointing source, so it is already ours.
        let mut angle = radians(self.p.cam_angle);
        if self.p.pier_side == PierSide::West {
            angle += PI;
        }
        let (sin_t, cos_t) = angle.sin_cos();
        let (half_w, half_h) = (self.p.width as f64 / 2.0, self.p.height as f64 / 2.0);
        let to_camera = |x: f64, y: f64| (x * cos_t - y * sin_t + half_w, x * sin_t + y * cos_t + half_h);
        let ao = self.ao.map_or((0.0, 0.0), |(right, up)| {
            let (sin_a, cos_a) = radians(AO_ANGLE).sin_cos();
            let (x, y) = (right as f64 * AO_SCALE, up as f64 * AO_SCALE);
            (x * cos_a - y * sin_a, x * sin_a + y * cos_a)
        });

        let bin = self.binning.max(1) as i32;
        let dark = GAIN as f64 / 10.0 * OFFSET as f64 * exptime as f64 / 100.0;
        if !shutter_closed {
            for i in 0..self.stars.len() {
                let s = self.stars[i];
                let (cx, cy) = to_camera(s.x + shift_x, s.y + shift_y);
                let noise = self.rng.below((GAIN * 100) as u32) as f64;
                let inten = s.inten * exptime as f64 * GAIN as f64 + dark + noise;
                render_star(img, bin, area, cx + ao.0, cy + ao.1, inten);
            }
            if self.p.show_comet {
                // The comet ignores the AO.
                let (cx, cy) = to_camera(
                    shift_x + now * self.p.comet_rate_x / 3600.0,
                    shift_y + now * self.p.comet_rate_y / 3600.0,
                );
                let noise = self.rng.below((GAIN * 100) as u32) as f64;
                let inten = 3.0 * exptime as f64 * GAIN as f64 + dark + noise;
                render_comet(img, bin, area, cx, cy, inten);
            }
        }

        if self.p.clouds_opacity > 0.0 {
            self.render_clouds(img, area, dark);
        }

        for &(x, y) in &self.hot_pixels {
            let (x, y) = (x / bin, y / bin);
            if area.contains(x, y) {
                set_pixel(img, x, y, u16::MAX);
            }
        }
    }

    /// `render_clouds`: blend each pixel toward a bright random level.
    /// Quirk kept: the cloud level wraps in its `(unsigned short)` cast.
    fn render_clouds(&mut self, img: &mut UsImage, area: Rect, dark: f64) {
        let opacity = self.p.clouds_opacity;
        for y in area.y..=area.bottom() {
            for x in area.x..=area.right() {
                let amt = c_int(CLOUDS_INTEN * (dark + self.rng.below((GAIN * 100) as u32) as f64 / 30.0)) as u16;
                let p = img.pixel_mut(x, y);
                *p = c_int(opacity * amt as f64 + (1.0 - opacity) * *p as f64) as u16;
            }
        }
    }
}

fn pixel_in(img: &UsImage, x: i32, y: i32) -> bool {
    x >= 0 && x < img.w && y >= 0 && y < img.h
}

fn set_pixel(img: &mut UsImage, x: i32, y: i32, val: u16) {
    if pixel_in(img, x, y) {
        *img.pixel_mut(x, y) = val;
    }
}

/// `incr_pixel`: add, saturating at 65535. The sum is an `unsigned int`,
/// so a negative increment cast to it wraps and takes the pixel down when
/// it is brighter than the increment is negative.
fn incr_pixel(img: &mut UsImage, x: i32, y: i32, val: u32) {
    if pixel_in(img, x, y) {
        let p = img.pixel_mut(x, y);
        let t = (*p as u32).wrapping_add(val);
        *p = if t > u16::MAX as u32 { u16::MAX } else { t as u16 };
    }
}

/// The star kernel scaled to `inten`, spread over a 6×6 footprint by the
/// sub-pixel position (bilinear), and the footprint's corner. Positions are
/// unbinned px. `modf` keeps the fraction's sign, so a star left of or
/// above the frame gets negative weights.
fn star_footprint(binning: i32, x: f64, y: f64, inten: f64) -> ((i32, i32), [[f64; 6]; 6]) {
    let (x, y) = (x / binning as f64, y / binning as f64);
    let (ix, iy) = (x.trunc(), y.trunc());
    let (fx, fy) = (x - ix, y - iy);
    let f00 = (1.0 - fx) * (1.0 - fy);
    let f01 = (1.0 - fx) * fy;
    let f10 = fx * (1.0 - fy);
    let f11 = fx * fy;
    let mut d = [[0.0; 6]; 6];
    for i in 0..5 {
        for j in 0..5 {
            let mut s = STAR[i][j];
            if s > 0.0 {
                s *= inten / 256.0;
                d[i][j] += f00 * s;
                d[i + 1][j] += f10 * s;
                d[i][j + 1] += f01 * s;
                d[i + 1][j + 1] += f11 * s;
            }
        }
    }
    ((c_int(ix) - 2, c_int(iy) - 2), d)
}

/// `render_star`: add the star at camera position (`x`, `y`) inside the
/// subframe.
fn render_star(img: &mut UsImage, binning: i32, sub: Rect, x: f64, y: f64, inten: f64) {
    let ((c_x, c_y), d) = star_footprint(binning, x, y, inten);
    for (i, column) in d.iter().enumerate() {
        let cx = c_x + i as i32;
        if cx < sub.x || cx > sub.right() {
            continue;
        }
        for (j, &v) in column.iter().enumerate() {
            let cy = c_y + j as i32;
            if cy < sub.y || cy > sub.bottom() {
                continue;
            }
            incr_pixel(img, cx, cy, c_int(v).min(u16::MAX as i32) as u32);
        }
    }
}

/// `render_comet`: the kernel's peak spread over a fan of five rays, ten
/// pixels long. Its clip tests are PHD2's: strict on the right, bottom and
/// top, none on the left.
fn render_comet(img: &mut UsImage, binning: i32, sub: Rect, x: f64, y: f64, inten: f64) {
    let ((c_x, c_y), d) = star_footprint(binning, x, y, inten);
    let v = c_int(d[2][2]) as u32;
    for x_inc in 0..10 {
        for k in 0..5 {
            let slope = -1.0 + 0.5 * k as f64;
            let cx = c_x + x_inc;
            let cy = c_int(c_y as f64 + slope * x_inc as f64);
            if cx < sub.right() && cy < sub.bottom() && cy > sub.y {
                incr_pixel(img, cx, cy, v);
            }
        }
    }
}

type Shared = Arc<Mutex<SimState>>;

fn lock(s: &Shared) -> MutexGuard<'_, SimState> {
    s.lock().unwrap()
}

/// The camera simulator (PHD2's "Simulator" camera). Make one per rig and
/// take the mount, AO and clock from it.
#[derive(Clone)]
pub struct SimCamera(Shared);

impl SimCamera {
    pub fn new(params: SimParams) -> SimCamera {
        let state = SimState {
            dec_ofs: BacklashVal::new(params.dec_backlash),
            p: params,
            stars: Vec::new(),
            hot_pixels: Vec::new(),
            ra_ofs: 0.0,
            cum_dec_drift: 0.0,
            last_exposure_time: 0,
            stiction: StictionSim::default(),
            prev_ra: 0.0,
            ra_offset: 0.0,
            rng: Rng::new(2),
            connected: false,
            binning: 1,
            now_ms: 0,
            ra_h: 0.0,
            dec_deg: Some(25.0),
            lst0_h: 0.0,
            ao: None,
        };
        SimCamera(Arc::new(Mutex::new(state)))
    }

    /// The mount: its pulses and position.
    pub fn mount(&self) -> SimMount {
        SimMount(self.0.clone())
    }

    pub fn ao(&self) -> SimAo {
        SimAo(self.0.clone())
    }

    pub fn clock(&self) -> SimClock {
        SimClock(self.0.clone())
    }

    /// New settings (what `load_sim_params` reads at each connect). They
    /// apply at once; the star field, the hot pixels and the backlash are
    /// rebuilt at the next connect.
    pub fn set_params(&self, params: SimParams) {
        lock(&self.0).p = params;
    }

    /// `CameraSimulator::FlipPierSide` (the settings dialog's Pier Flip):
    /// the other side, East from Unknown.
    pub fn flip_pier_side(&self) {
        let mut s = lock(&self.0);
        s.p.pier_side = if s.p.pier_side == PierSide::East { PierSide::West } else { PierSide::East };
    }
}

impl GuideCamera for SimCamera {
    fn name(&self) -> String {
        "Simulator".into()
    }

    fn connect(&mut self) -> Result<(), String> {
        let mut s = lock(&self.0);
        s.initialize();
        s.connected = true;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), String> {
        lock(&self.0).connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        lock(&self.0).connected
    }

    fn full_size(&self) -> (i32, i32) {
        let s = lock(&self.0);
        let bin = s.binning.max(1) as i32;
        (s.p.width as i32 / bin, s.p.height as i32 / bin)
    }

    fn binning(&self) -> u8 {
        lock(&self.0).binning
    }

    fn set_binning(&mut self, binning: u8) {
        lock(&self.0).binning = binning;
    }

    fn max_binning(&self) -> u8 {
        3
    }

    /// The simulator reports no pixel size: the profile's is used.
    fn pixel_size_um(&self) -> Option<f64> {
        None
    }

    fn bits_per_pixel(&self) -> u8 {
        16
    }

    fn has_gain_control(&self) -> bool {
        true
    }

    fn has_shutter(&self) -> bool {
        true
    }

    fn has_subframes(&self) -> bool {
        true
    }

    fn has_guide_output(&self) -> bool {
        true
    }

    fn st4_pulse(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> Result<(), String> {
        lock(&self.0).pulse(dir, ms, cancel);
        Ok(())
    }

    fn capture(
        &mut self,
        dur_ms: u32,
        kind: CaptureKind,
        subframe: Option<Rect>,
        cancel: &CancelToken,
    ) -> Result<UsImage, String> {
        lock(&self.0).capture(dur_ms, kind, subframe, cancel)
    }
}

/// The simulated mount: the camera's ST-4 pulses as a guide output, and a
/// pointing source at a set RA/Dec on the simulator's pier side, guiding at
/// its guide rate on both axes.
#[derive(Clone)]
pub struct SimMount(Shared);

impl SimMount {
    /// Point at `ra_h` hours, `dec_deg` degrees at once (`None`: no
    /// position). The next frame moves the worm phase by the RA change.
    pub fn slew_to(&self, ra_h: f64, dec_deg: Option<f64>) {
        let mut s = lock(&self.0);
        s.ra_h = ra_h;
        s.dec_deg = dec_deg;
    }
}

impl GuideOutput for SimMount {
    fn guide(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> MoveResult {
        lock(&self.0).pulse(dir, ms, cancel);
        MoveResult::Ok
    }
}

impl PointingSource for SimMount {
    fn coordinates(&self) -> Option<(f64, f64, f64)> {
        let s = lock(&self.0);
        s.dec_deg.map(|dec| (s.ra_h, dec, s.lst()))
    }

    fn pier_side(&self) -> PierSide {
        lock(&self.0).p.pier_side
    }

    /// The guide rate on both axes, deg/s.
    fn guide_rates(&self) -> Option<(f64, f64)> {
        let rate = lock(&self.0).p.guide_rate / 3600.0;
        Some((rate, rate))
    }

    fn is_slewing(&self) -> bool {
        false
    }

    fn can_report_position(&self) -> bool {
        lock(&self.0).dec_deg.is_some()
    }
}

/// The AO simulator (`StepGuiderSimulator`, "AO-Simulator"): 0.1 px per
/// step at 35° to the camera. PHD2's `StepGuider` base class keeps the
/// position and only steps inside the travel; the device keeps its own copy
/// here, which the frames show.
#[derive(Clone)]
pub struct SimAo(Shared);

impl SimAo {
    pub fn name(&self) -> String {
        "AO-Simulator".into()
    }

    /// Connect, centred (`ZeroCurrentPosition`).
    pub fn connect(&mut self) {
        lock(&self.0).ao = Some((0, 0));
    }

    pub fn disconnect(&mut self) {
        lock(&self.0).ao = None;
    }

    pub fn is_connected(&self) -> bool {
        lock(&self.0).ao.is_some()
    }

    /// `Step`: `steps` steps one way (North = up, East = right), 5 ms
    /// each. Never fails: the base class keeps the moves inside the travel.
    pub fn step(&mut self, dir: Dir, steps: u32) -> MoveResult {
        let mut s = lock(&self.0);
        let n = steps as i32;
        if let Some((x, y)) = s.ao.as_mut() {
            match dir {
                Dir::North => *y += n,
                Dir::South => *y -= n,
                Dir::East => *x += n,
                Dir::West => *x -= n,
            }
        }
        // `wxMilliSleep`: not interruptible.
        s.now_ms += steps as u64 * AO_MS_PER_STEP;
        MoveResult::Ok
    }

    /// `MaxPosition`: the same travel every way.
    pub fn max_position(&self, _dir: Dir) -> i32 {
        lock(&self.0).p.ao_max_position
    }

    /// `SetMaxPosition`; the caller stores `/SimAo/max_steps`.
    pub fn set_max_position(&mut self, steps: i32) {
        lock(&self.0).p.ao_max_position = steps;
    }

    /// `Center`: back to the middle of the travel; always succeeds.
    pub fn center(&mut self) -> bool {
        let mut s = lock(&self.0);
        if s.ao.is_some() {
            s.ao = Some((0, 0));
        }
        true
    }

    /// The position in steps (right, up).
    pub fn position(&self) -> (i32, i32) {
        lock(&self.0).ao.unwrap_or_default()
    }
}

/// The simulated clock: monotonic time is the simulator's, and the wall
/// clock runs from 2026-01-01 00:00 UTC.
#[derive(Clone)]
pub struct SimClock(Shared);

impl Clock for SimClock {
    fn mono(&self) -> f64 {
        lock(&self.0).now_ms as f64 / 1000.0
    }

    fn wall(&self) -> DateTime<Local> {
        let ms = lock(&self.0).now_ms as i64;
        let epoch: DateTime<Local> = DateTime::from_timestamp(WALL_EPOCH, 0).unwrap_or_default().into();
        epoch + TimeDelta::milliseconds(ms)
    }

    fn sleep_ms(&self, ms: u64) {
        lock(&self.0).now_ms += ms;
    }
}


#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::engine::calibration::{CALIBRATION_RATE_UNCALIBRATED, CalibrationIssue, degrees, norm_angle};
    use crate::engine::mount::MountEvent;
    use crate::engine::scope::{CalibrationEnv, CalibrationNext, DecGuideMode, Scope, ScopeIo};
    use crate::engine::star::{FindMode, Star};
    use crate::engine::traits::{MoveOpts, Point};

    /// Star 12 of the seed-2 field: bright, alone, well inside the frame.
    const BRIGHT: usize = 12;

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
            self.f.get(key).copied().or(self.i.get(key).map(|&v| v as f64)).unwrap_or(default)
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

    /// No seeing, no periodic error, no drift and no read noise: the stars
    /// move only with the pulses, and `Star::find` puts them exactly where
    /// they are drawn (the noise alone moves a centroid by up to 0.2 px).
    fn quiet(scale: f64) -> SimParams {
        SimParams {
            seeing_scale: 0.0,
            use_pe: false,
            dec_drift_rate: 0.0,
            noise_multiplier: 0.0,
            ..SimParams::new(scale)
        }
    }

    fn connected(p: SimParams) -> SimCamera {
        let mut cam = SimCamera::new(p);
        cam.connect().unwrap();
        cam
    }

    /// Where star `i` sits on the camera with the pulses and the drift but
    /// no periodic error nor seeing (`FillImage`'s geometry).
    fn field_pos(cam: &SimCamera, i: usize) -> Point {
        let s = lock(&cam.0);
        let mut angle = radians(s.p.cam_angle);
        if s.p.pier_side == PierSide::West {
            angle += PI;
        }
        let (sin_t, cos_t) = angle.sin_cos();
        let x = s.stars[i].x + s.ra_ofs;
        let y = s.stars[i].y + s.cum_dec_drift + s.dec_ofs.val();
        Point::new(x * cos_t - y * sin_t + s.p.width as f64 / 2.0, x * sin_t + y * cos_t + s.p.height as f64 / 2.0)
    }

    /// One frame, and the star found near `near`.
    fn locate(cam: &mut SimCamera, near: Point, subframe: Option<Rect>) -> Point {
        let img = cam.capture(1000, CaptureKind::Light, subframe, &CancelToken::new()).unwrap();
        let star = Star::find(&img, 15, near.x, near.y, FindMode::Centroid, 0.0, 100.0, 0);
        assert!(star.was_found(), "{:?}", star.result);
        star.position()
    }

    /// One frame, and the bright star found where the pulses put it.
    fn bright(cam: &mut SimCamera) -> Point {
        let near = field_pos(cam, BRIGHT);
        locate(cam, near, None)
    }

    /// Unit vectors of RA (West) and Dec (North) on the camera at 15°.
    fn axes() -> (Point, Point) {
        let (sin_t, cos_t) = radians(15.0).sin_cos();
        (Point::new(cos_t, sin_t), Point::new(-sin_t, cos_t))
    }

    fn dot(a: Point, b: Point) -> f64 {
        a.x * b.x + a.y * b.y
    }

    #[test]
    fn frames_are_the_same_every_time() {
        let cancel = CancelToken::new();
        let frame = || {
            let mut cam = connected(SimParams::new(1.0));
            let img = cam.capture(1000, CaptureKind::Light, None, &cancel).unwrap();
            (img, cam.clock().mono())
        };
        let (a, t) = frame();
        let (b, _) = frame();
        assert_eq!((a.w, a.h), (752, 580));
        assert_eq!(a.data, b.data);
        // The exposure and the 50 ms download.
        assert_eq!(t, 1.05);

        // The seed-2 hot pixels, and star 10 saturating.
        let mut cam = connected(SimParams::new(1.0));
        assert_eq!(lock(&cam.0).hot_pixels[..3], [(295, 575), (219, 143), (695, 110)]);
        assert_eq!(a.pixel(295, 575), u16::MAX);
        let saturated = a.data.iter().filter(|&&v| v == u16::MAX).count();
        assert!(saturated > 8, "{saturated}");

        // A dark frame is the noise, 2 × (3000 + 0..3000), and the hot pixels.
        let dark = cam.capture(1000, CaptureKind::Dark, None, &cancel).unwrap();
        let hot = dark.data.iter().filter(|&&v| v == u16::MAX).count();
        assert_eq!(hot, 8);
        assert!(dark.data.iter().all(|&v| v == u16::MAX || (6000..=11998).contains(&v)));

        // A subframe is the only area read; one past the frame edge is dropped.
        let sub = Rect::new(600, 420, 100, 100);
        let img = cam.capture(500, CaptureKind::Light, Some(sub), &cancel).unwrap();
        assert_eq!(img.subframe, Some(sub));
        assert_eq!((img.pixel(599, 420), img.pixel(600, 520)), (0, 0));
        assert!(img.pixel(600, 420) >= 3000 && img.pixel(699, 519) >= 3000);
        let img = cam.capture(500, CaptureKind::Light, Some(Rect::new(701, 0, 52, 10)), &cancel).unwrap();
        assert_eq!(img.subframe, None);
        assert!(img.data.iter().all(|&v| v > 0));
    }

    #[test]
    fn a_pulse_moves_the_star_along_the_camera_angle() {
        let p = SimParams { dec_backlash: 0.0, ..quiet(1.0) };
        let mut cam = connected(p);
        let mut mount = cam.mount();
        let cancel = CancelToken::new();
        let (ra, dec) = axes();

        let a = bright(&mut cam);
        // Star 12 at (319, 109) turned 15° about the frame centre.
        assert!(a.distance(&Point::new(655.919, 477.849)) < 0.001, "{a:?}");
        assert_eq!(mount.guide(Dir::West, 1000, &cancel), MoveResult::Ok);
        let b = bright(&mut cam);
        // 15″/s for 1 s at 1″/px, times cos 25° in RA.
        let d = b - a;
        assert!((d.x.hypot(d.y) - 15.0 * radians(25.0).cos()).abs() < 0.01, "{d:?}");
        assert!((degrees(d.y.atan2(d.x)) - 15.0).abs() < 0.05, "{d:?}");
        assert!(dot(d, dec).abs() < 0.01);

        mount.guide(Dir::North, 1000, &cancel);
        let c = bright(&mut cam);
        let d = c - b;
        assert!((dot(d, dec) - 15.0).abs() < 0.01 && dot(d, ra).abs() < 0.01, "{d:?}");

        // Three frames and two pulses of simulated time.
        assert!((cam.clock().mono() - 5.15).abs() < 1e-9);
    }

    #[test]
    fn dec_backlash_eats_the_first_reversal() {
        // 5″ at 1″/px, the state of a mount last moved South.
        let mut cam = connected(quiet(1.0));
        let mut mount = cam.mount();
        let cancel = CancelToken::new();
        let (_, dec) = axes();
        let mut moves = Vec::new();
        let mut last = bright(&mut cam);
        for (dir, ms) in [(Dir::North, 1000), (Dir::South, 200), (Dir::South, 400)] {
            mount.guide(dir, ms, &cancel);
            let p = bright(&mut cam);
            moves.push(dot(p - last, dec));
            last = p;
        }
        // 15 px North less the 5 of slack; 3 px South all slack; 6 px
        // South, 2 more of slack.
        for (got, want) in moves.iter().zip([10.0, 0.0, -4.0]) {
            assert!((got - want).abs() < 0.01, "{moves:?}");
        }
    }

    #[test]
    fn stiction_takes_a_third_of_a_long_reversal() {
        let mut s = StictionSim::default();
        assert_eq!(s.adjustment(Dir::North, 500, 9.0), 0.0);
        assert_eq!(s.adjustment(Dir::South, 600, 9.0), -3.0);
        // Given back when the next pulse keeps the direction.
        assert_eq!(s.adjustment(Dir::South, 600, 9.0), 3.0);
        assert_eq!(s.adjustment(Dir::North, 200, 3.0), 0.0);
        // A reversal followed by a reversal gives nothing back.
        assert_eq!(s.adjustment(Dir::South, 900, 6.0), -2.0);
        assert_eq!(s.adjustment(Dir::North, 900, 6.0), 0.0);
    }

    #[test]
    fn periodic_error_over_a_worm_period() {
        let mut cam = connected(SimParams { use_pe: true, ..quiet(1.0) });
        let clock = cam.clock();
        let (ra, _) = axes();
        let sub = Some(Rect::new(610, 440, 90, 80));
        // The RA shift of a frame, and the model at its render time (5 ms
        // before the exposure ends).
        let frame = |cam: &mut SimCamera, worm_shift: f64| {
            let t = clock.mono() + 0.995;
            let still = field_pos(cam, BRIGHT);
            let got = dot(locate(cam, still, sub) - still, ra);
            (got, lock(&cam.0).pe(t - worm_shift))
        };
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        while clock.mono() < 230.5 {
            let (got, want) = frame(&mut cam, 0.0);
            assert!((got - want).abs() < 0.01, "{got} {want}");
            lo = lo.min(got);
            hi = hi.max(got);
        }
        // The canned PE at 5″ and 1″/px: 5.00 px peak to peak over this
        // worm turn, 4.947 at the frames' times.
        assert!((hi - lo - 4.947).abs() < 0.001, "{}", hi - lo);

        // A slew of 1 h East turns the worm back 1 h of sidereal time.
        cam.mount().slew_to(1.0, Some(25.0));
        let (got, want) = frame(&mut cam, 3600.0 / 0.9973);
        assert!((got - want).abs() < 0.01, "{got} {want}");
    }

    #[test]
    fn a_pier_flip_turns_the_field_and_the_dec_pulses() {
        let p = SimParams { dec_backlash: 0.0, ..quiet(1.0) };
        let mut cam = connected(p);
        let mut mount = cam.mount();
        let cancel = CancelToken::new();
        let a = bright(&mut cam);
        cam.flip_pier_side();
        assert_eq!(mount.pier_side(), PierSide::West);
        let b = bright(&mut cam);
        assert!(b.distance(&Point::new(752.0 - a.x, 580.0 - a.y)) < 0.01, "{a:?} {b:?}");
        // North is reversed on the West side, so a North pulse still moves
        // the star along +Dec on the camera; West now goes the other way.
        let (ra, dec) = axes();
        mount.guide(Dir::North, 1000, &cancel);
        let c = bright(&mut cam);
        assert!((dot(c - b, dec) - 15.0).abs() < 0.01, "{:?}", c - b);
        mount.guide(Dir::West, 1000, &cancel);
        let d = bright(&mut cam);
        assert!((dot(d - c, ra) + 15.0 * radians(25.0).cos()).abs() < 0.01, "{:?}", d - c);
    }

    /// The engine's calibration loop on the simulator: a frame, the star
    /// found near its last place, the state machine, its move before the
    /// next frame. Returns the number of backlash-clearing pulses.
    fn calibrate(scope: &mut Scope, cam: &mut SimCamera, profile: &mut Profile) -> usize {
        let env = CalibrationEnv {
            binning: 1,
            focal_length: 400,
            pixel_size: 3.88,
            image_scale: 2.0,
            rotator_angle: None,
            fast_recenter: true,
            max_move_pixels: 15,
            can_pulse_guide: false,
            timestamp: "T".into(),
        };
        let mut output = cam.mount();
        let pointing = cam.mount();
        let clock = cam.clock();
        let cancel = CancelToken::new();
        let mut ev = Vec::new();
        let mut clearing = 0;
        let mut pos = bright(cam);
        assert!(scope.begin_calibration(Some(pos), &env, Some(&pointing), profile, &mut ev));
        for _ in 0..200 {
            pos = locate(cam, pos, None);
            let next = scope.update_calibration_state(pos, &env, Some(&pointing), profile, &mut ev);
            for e in ev.drain(..) {
                if let MountEvent::Calibrating(i) = e
                    && i.msg.starts_with("Clearing backlash")
                {
                    clearing += 1;
                }
            }
            match next {
                CalibrationNext::Move(dir, ms) => {
                    let now = clock.mono();
                    let mut io = ScopeIo { output: &mut output, pointing: Some(&pointing), cancel: &cancel, now };
                    assert_eq!(scope.move_axis(dir, ms, MoveOpts::CALIBRATION_MOVE, &mut io, &mut ev), MoveResult::Ok);
                }
                CalibrationNext::NoMove => {}
                CalibrationNext::Failed => panic!("calibration failed"),
            }
            if scope.is_calibrated() {
                return clearing;
            }
        }
        panic!("calibration did not end");
    }

    #[test]
    fn the_engine_calibrates_on_the_simulator() {
        // PHD2's defaults at 2″/px (PE, seeing, read noise, Dec backlash)
        // but the Dec drift: its 5″/min over the RA legs tilts the RA axis
        // by about 1° (−164° here), in PHD2 as well.
        let mut cam = connected(SimParams { dec_drift_rate: 0.0, ..SimParams::new(2.0) });
        let mut profile = Profile::default();
        let mut scope = Scope::new(&mut profile, false, 0.0, &mut Vec::new());
        scope.mount.connect();

        let clearing = calibrate(&mut scope, &mut cam, &mut profile);
        let c = scope.mount.mount_cal().clone();
        // West moves the stars along the camera angle, and PHD2's RA angle
        // points from the star back to where it started: 15° − 180°.
        assert!(degrees(norm_angle(c.x_angle - radians(-165.0))).abs() < 1.0, "{}", degrees(c.x_angle));
        assert!(degrees(norm_angle(c.y_angle - radians(105.0))).abs() < 1.0, "{}", degrees(c.y_angle));
        // px/ms: 15″/s at 2″/px, RA slowed by cos 25°.
        let x_rate = 15.0 * radians(25.0).cos() / 2.0 / 1000.0;
        assert!((c.x_rate / x_rate - 1.0).abs() < 0.05, "{}", c.x_rate);
        assert!((c.y_rate / 0.0075 - 1.0).abs() < 0.05, "{}", c.y_rate);
        assert!((degrees(c.declination) - 25.0).abs() < 1e-9);
        assert_eq!(c.pier_side, PierSide::East);
        assert!(clearing >= 2, "{clearing}");
        assert_eq!(scope.last_calibration_issue().0, CalibrationIssue::None);

        // Dec guiding off: done after the RA legs, Dec left uncalibrated.
        scope.set_dec_guide_mode(DecGuideMode::None as i32, &mut profile, &mut Vec::new());
        let clearing = calibrate(&mut scope, &mut cam, &mut profile);
        assert_eq!(clearing, 0);
        assert_eq!(scope.mount.y_rate(), CALIBRATION_RATE_UNCALIBRATED);
        assert!(degrees(norm_angle(scope.mount.x_angle() - radians(-165.0))).abs() < 1.0);
    }

    #[test]
    fn the_ao_offsets_the_field_and_the_clock_runs_with_its_steps() {
        let mut cam = connected(quiet(1.0));
        let mut ao = cam.ao();
        let before = field_pos(&cam, BRIGHT);
        ao.connect();
        assert_eq!(ao.step(Dir::East, 20), MoveResult::Ok);
        ao.step(Dir::North, 10);
        assert_eq!(ao.position(), (20, 10));
        assert!((cam.clock().mono() - 0.15).abs() < 1e-9);
        // 2 px right and 1 px up at 35° to the camera.
        let (sin_a, cos_a) = radians(35.0).sin_cos();
        let want = before + Point::new(2.0 * cos_a - sin_a, 2.0 * sin_a + cos_a);
        let got = locate(&mut cam, want, None);
        assert!(got.distance(&want) < 0.01, "{got:?} {want:?}");
        assert!(ao.center());
        assert_eq!((ao.position(), ao.max_position(Dir::West)), ((0, 0), 45));
    }
}
