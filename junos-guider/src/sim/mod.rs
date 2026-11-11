//! The gear simulator: a guide camera, a mount and an AO that run with no
//! hardware, for the tests and `--sim`.
//!
//! A port of PHD2's camera simulator: 20 stars at fixed random places,
//! hot pixels, read noise, periodic error, seeing, Dec drift, Dec backlash
//! and stiction, the camera angle and the pier side, the AO's offset, a
//! comet and clouds, and the cooler. Guide pulses move the field through
//! the camera's ST-4 port: the mount is PHD2's "On Camera" one,
//! [`OnCamera`](crate::engine::camera::OnCamera) over the [`SimCamera`].
//!
//! Everything shares one state behind the handles [`SimCamera`],
//! [`SimMount`], [`SimAo`] and [`SimClock`]. Time runs one of two ways
//! ([`SimTime`]). On the real clock, for `--sim`, an exposure, a pulse and
//! an AO step take their time as in PHD2, and a stop interrupts a wait
//! within 100 ms. Simulated, for the tests, the same waits advance it at
//! once: an exposure by its length plus the 50 ms download, a pulse by its
//! length, an AO step by 5 ms, and [`Clock::sleep_ms`] by what it is given,
//! so a run is fast and the same every time. The star field, the noise and
//! the seeing come from [`Rng`] seeded with 2, re-seeded on each connect.
//!
//! [`SimMount`] is the rig's pointing source. As in PHD2's rig, where it is
//! the "On Camera" mount, it reports no position, an unknown pier side and
//! no guide rate, and each frame takes the pier side from it. A deliberate
//! addition: once [`SimMount::slew_to`] gives it a position it reports that,
//! the simulator's pier side and its guide rate, so the engine's pointing
//! features have a source. PHD2's debug-log lines wait in
//! [`SimCamera::take_debug`]. PHD2's rotator simulator is left out: there is
//! no rotator device yet.
//!
//! Ported from PHD2 src/gear_simulator.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Local, TimeDelta};

use crate::engine::calibration::{ProfileAccess, norm, pier_side_from, radians};
use crate::engine::camera::{
    CameraSettings, CaptureError, CaptureKind, CoolerStatus, GuideCamera, UNDEFINED_FRAME_SIZE,
};
use crate::engine::image::UsImage;
use crate::engine::log::cfmt::fmt_f;
use crate::engine::mount::PointingSource;
use crate::engine::rng::Rng;
use crate::engine::stepguider::{StepGuiderDevice, StepResult};
use crate::engine::traits::{CancelToken, Clock, Dir, PierSide, Rect, SystemClock, c_int};

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
/// The AO's travel in the profile (`SimAoParams::max_position`).
pub const SIM_AO_MAX_STEPS_KEY: &str = "/SimAo/max_steps";
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
/// The cooler's range, °C: it cannot warm past the ambient temperature.
const AMBIENT_TEMP: f64 = 15.0;
const MIN_COOLER_TEMP: f64 = -15.0;
/// `WorkerThread::MilliSleep`'s slice: how often a wait checks for a stop.
const MAX_SLEEP_MS: i64 = 100;

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
    /// The profile's `int` as PHD2 adds it to an `unsigned` (a negative
    /// value wraps).
    pub frame_download_ms: u32,
    /// [`SIM_AO_MAX_STEPS_KEY`].
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
        let flag = |key: &str, default: bool| p.get_bool(key, default);
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
            pier_side: pier_side_from(p.get_i32("/SimCam/pier_side", PierSide::East as i32)),
            reverse_dec_pulse_on_west_side: flag("/SimCam/reverse_dec_pulse_on_west_side", true),
            show_comet: flag("/SimCam/show_comet", false),
            comet_rate_x: p.get_f64("/SimCam/comet_rate_x", 555.0),
            comet_rate_y: p.get_f64("/SimCam/comet_rate_y", -123.4),
            frame_download_ms: p.get_i32("/SimCam/frame_download_ms", 50) as u32,
            image_scale,
            ao_max_position: p.get_i32(SIM_AO_MAX_STEPS_KEY, 45),
        }
    }
}

/// How the simulator's time runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimTime {
    /// The waits advance it at once (the tests).
    Simulated,
    /// The real clock: the waits take their time (`--sim`).
    Real,
}

/// The simulator's time: the camera's stopwatch, the clock the engine
/// reads, and the waits. Shared by every handle and read without the
/// state's lock, so a real wait never holds it.
struct Timer {
    /// `Some` on the real clock.
    real: Option<SystemClock>,
    /// Simulated ms since the simulator was made.
    sim_ms: AtomicU64,
}

impl Timer {
    fn now_ms(&self) -> i64 {
        match &self.real {
            Some(c) => (c.mono() * 1000.0) as i64,
            None => self.sim_ms.load(Ordering::SeqCst) as i64,
        }
    }

    /// `wxMilliSleep`: not interruptible.
    fn sleep(&self, ms: u64) {
        match &self.real {
            Some(c) => c.sleep_ms(ms),
            None => {
                self.sim_ms.fetch_add(ms, Ordering::SeqCst);
            }
        }
    }

    /// `WorkerThread::MilliSleep(ms, INT_ANY)`: slices of at most 100 ms,
    /// the stop checked after each; true when stopped.
    fn milli_sleep(&self, ms: i64, cancel: &CancelToken) -> bool {
        if ms <= MAX_SLEEP_MS {
            if ms > 0 {
                self.sleep(ms as u64);
            }
            return cancel.is_cancelled();
        }
        let start = self.now_ms();
        let mut elapsed = 0;
        while elapsed < ms {
            self.sleep((ms - elapsed).min(MAX_SLEEP_MS) as u64);
            if cancel.is_cancelled() {
                return true;
            }
            elapsed = self.now_ms() - start;
        }
        false
    }

    fn wall(&self) -> DateTime<Local> {
        match &self.real {
            Some(c) => c.wall(),
            None => {
                let epoch: DateTime<Local> = DateTime::from_timestamp(WALL_EPOCH, 0).unwrap_or_default().into();
                epoch + TimeDelta::milliseconds(self.now_ms())
            }
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
    /// `GetAdjustment`, its debug-log lines in `debug`.
    fn adjustment(&mut self, dir: Dir, duration: u32, distance: f64, debug: &mut Vec<String>) -> f64 {
        let mut rslt = 0.0;
        if let Some(last) = self.last {
            if duration > 300 && dir != last && !self.pending {
                self.adjustment = distance / 3.0;
                self.pending = true;
                debug.push(format!("Stiction: reduced distance by {}", fmt_f(self.adjustment, 2)));
                rslt = -self.adjustment;
            } else if self.pending {
                if dir == last {
                    rslt = self.adjustment;
                    debug.push(format!("Stiction: increased distance by {}", fmt_f(self.adjustment, 2)));
                    self.adjustment = 0.0;
                }
                self.pending = false;
            }
        }
        self.last = Some(dir);
        rslt
    }
}

/// The simulated cooler (`Cooler`): it moves at 1/8 °C a second towards
/// its target, held to −15…15 °C. Times are wall-clock seconds
/// (`wxDateTime::GetTimeNow`).
#[derive(Clone, Copy, Debug)]
struct Cooler {
    on: bool,
    start_temp: f64,
    end_temp: f64,
    set_temp: f64,
    end_time: i64,
    /// °C per second.
    rate: f64,
    /// −1 cooling, +1 warming.
    direction: f64,
}

impl Cooler {
    fn new() -> Self {
        Cooler {
            on: false,
            start_temp: AMBIENT_TEMP,
            end_temp: AMBIENT_TEMP,
            set_temp: AMBIENT_TEMP,
            end_time: 0,
            rate: 1.0 / 8.0,
            direction: 0.0,
        }
    }

    fn current_temp(&self, now: i64) -> f64 {
        if now >= self.end_time {
            return self.end_temp;
        }
        self.end_temp - self.rate * self.direction * (self.end_time - now) as f64
    }

    fn turn_on(&mut self, now: i64) {
        if !self.on {
            self.go_to(self.current_temp(now), now);
            self.on = true;
        }
    }

    fn turn_off(&mut self, now: i64) {
        if self.on {
            self.go_to(AMBIENT_TEMP, now);
            self.on = false;
        }
    }

    /// `_SetTemp`.
    fn go_to(&mut self, temp: f64, now: i64) {
        self.start_temp = self.current_temp(now);
        // `std::max(std::min(newtemp, AMBIENT_TEMP), MIN_COOLER_TEMP)`.
        let t = if AMBIENT_TEMP < temp { AMBIENT_TEMP } else { temp };
        self.end_temp = if t < MIN_COOLER_TEMP { MIN_COOLER_TEMP } else { t };
        let dt = ((self.end_temp - self.start_temp).abs() / self.rate).ceil();
        self.end_time = now + dt as i64;
        self.direction = if self.end_temp < self.start_temp { -1.0 } else { 1.0 };
    }

    fn set_temp(&mut self, temp: f64, now: i64) {
        self.go_to(temp, now);
        self.set_temp = temp;
    }

    /// `CameraSimulator::GetCoolerStatus`: the power is how far below the
    /// ambient temperature the sensor is, of the cooler's whole range.
    fn status(&self, now: i64) -> CoolerStatus {
        let cur = self.current_temp(now);
        let (setpoint, power) = if !self.on {
            (0.0, 0.0)
        } else if cur < MIN_COOLER_TEMP {
            (self.set_temp, 100.0)
        } else if cur >= AMBIENT_TEMP {
            (self.set_temp, 0.0)
        } else {
            (self.set_temp, (AMBIENT_TEMP - cur) * 100.0 / (AMBIENT_TEMP - MIN_COOLER_TEMP))
        };
        CoolerStatus { on: self.on, setpoint, power, temperature: cur }
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
/// camera's binning and connection, the mount's position and the AO's.
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
    /// `FrameSize`: set by each capture, kept across reconnects.
    frame_size: (i32, i32),
    /// Kept across reconnects, as in PHD2.
    cooler: Cooler,
    time: Arc<Timer>,
    ra_h: f64,
    /// `None`: PHD2's rig, no position (`UNKNOWN_DECLINATION`).
    dec_deg: Option<f64>,
    /// Local sidereal time at time 0, hours.
    lst0_h: f64,
    /// The AO's position in steps (right, up); `None` while it is
    /// disconnected (`s_sim_ao`).
    ao: Option<(i32, i32)>,
    /// A new AO travel for [`SIM_AO_MAX_STEPS_KEY`], not yet saved.
    ao_max_steps_to_save: Option<i32>,
    /// Debug-log lines not yet taken.
    debug: Vec<String>,
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

    fn lst(&self) -> f64 {
        norm(self.lst0_h + self.time.now_ms() as f64 / 1000.0 * 1.002_737_909_35 / 3600.0, 0.0, 24.0)
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

    /// `CameraSimulator::ST4PulseGuideScope` but its wait: move the field
    /// by `ms` of guiding.
    fn pulse_move(&mut self, dir: Dir, ms: u32) {
        // render_star divides positions by the binning, so the distance is
        // in unbinned px.
        let mut d = self.p.guide_rate * self.binning as f64 * ms as f64 / (1000.0 * self.p.image_scale);
        if matches!(dir, Dir::West | Dir::East) {
            // RA moves less away from the equator; 25° when unknown.
            d *= self.declination_radians().unwrap_or(radians(25.0)).cos();
        }
        if self.p.use_stiction && matches!(dir, Dir::North | Dir::South) {
            d += self.stiction.adjustment(dir, ms, d, &mut self.debug);
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
    }

    /// The frame of `CameraSimulator::Capture`, between its waits: its size
    /// becomes `FrameSize`, then the noise and the field.
    fn render(&mut self, duration: u32, subframe: Option<Rect>, shutter_closed: bool) -> UsImage {
        let bin = self.binning.max(1) as i32;
        let (w, h) = (self.p.width as i32 / bin, self.p.height as i32 / bin);
        self.frame_size = (w, h);
        // A subframe that does not fit is dropped. PHD2 lets a negative
        // origin through, to read before the buffer.
        let sub = subframe.filter(|r| r.w > 0 && r.h > 0 && r.x >= 0 && r.y >= 0 && r.right() < w && r.bottom() < h);
        let area = sub.unwrap_or(Rect::new(0, 0, w, h));
        let mut img = UsImage::new(w, h);
        let exptime = duration as i32;
        self.fill_noise(&mut img, area, exptime);
        self.fill_image(&mut img, area, exptime, shutter_closed);
        img.subframe = sub;
        img
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
        let cur_time = self.time.now_ms();
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

        // The pier side comes from the pointing source, the simulator's own
        // mount: in PHD2's rig it reports none, so the West turn and the
        // Dec reversal never apply there.
        let side = self.pier_side();
        if side != self.p.pier_side {
            self.debug.push(format!(
                "Cam simulator: pointing source pier side changed from {} to {}",
                self.p.pier_side as i32, side as i32
            ));
            self.p.pier_side = side;
        }
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

/// The rig's pointing source (see the module doc).
impl PointingSource for SimState {
    fn coordinates(&self) -> Option<(f64, f64, f64)> {
        self.dec_deg.map(|dec| (self.ra_h, dec, self.lst()))
    }

    fn pier_side(&self) -> PierSide {
        if self.dec_deg.is_some() { self.p.pier_side } else { PierSide::Unknown }
    }

    /// The guide rate on both axes, deg/s.
    fn guide_rates(&self) -> Option<(f64, f64)> {
        let rate = self.p.guide_rate / 3600.0;
        self.dec_deg.map(|_| (rate, rate))
    }

    fn is_slewing(&self) -> bool {
        false
    }

    fn can_report_position(&self) -> bool {
        self.dec_deg.is_some()
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
/// so a negative increment cast to it wraps: it takes a pixel at least as
/// bright down by its size, and saturates a darker one to 65535.
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
/// above the frame gets negative weights. The corner wraps like C's `int`
/// for a position `(int)` turns into `INT_MIN` (NaN or huge).
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
    ((c_int(ix).wrapping_sub(2), c_int(iy).wrapping_sub(2)), d)
}

/// `render_star`: add the star at camera position (`x`, `y`) inside the
/// subframe.
fn render_star(img: &mut UsImage, binning: i32, sub: Rect, x: f64, y: f64, inten: f64) {
    let ((c_x, c_y), d) = star_footprint(binning, x, y, inten);
    for (i, column) in d.iter().enumerate() {
        let cx = c_x.wrapping_add(i as i32);
        if cx < sub.x || cx > sub.right() {
            continue;
        }
        for (j, &v) in column.iter().enumerate() {
            let cy = c_y.wrapping_add(j as i32);
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
            let cx = c_x.wrapping_add(x_inc);
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

/// `CameraSimulator::Capture`: wait out the exposure but its last 5 ms,
/// render (so a pulse made meanwhile shows), then wait out the download. A
/// wait that ends past the exposure plus the camera timeout
/// (`CameraWatchdog`) disconnects the camera. The waits run without the
/// lock.
fn capture(
    sh: &Shared,
    duration: u32,
    subframe: Option<Rect>,
    settings: &CameraSettings,
    cancel: &CancelToken,
) -> Result<UsImage, CaptureError> {
    let time = lock(sh).time.clone();
    let start = time.now_ms();
    // `Watchdog(unsigned timeout, unsigned grace)`: the sum wraps as an
    // unsigned int.
    let limit = duration.wrapping_add(settings.timeout_ms as u32) as i64;
    let timed_out = || {
        let out = time.now_ms() - start > limit;
        if out {
            lock(sh).connected = false;
        }
        out
    };
    if duration > 5 {
        if time.milli_sleep(duration as i64 - 5, cancel) {
            return Err(CaptureError::Interrupted);
        }
        if timed_out() {
            return Err(CaptureError::Timeout);
        }
    }
    let (img, download_ms) = {
        let mut s = lock(sh);
        (s.render(duration, subframe, settings.shutter_closed), s.p.frame_download_ms)
    };
    // `unsigned tot_dur = duration + frame_download_ms`: a negative
    // download time wraps, and the `int` sleep it gives is then ≤ 0.
    let tot_dur = duration.wrapping_add(download_ms) as i64;
    let elapsed = time.now_ms() - start;
    if elapsed < tot_dur {
        if time.milli_sleep((tot_dur - elapsed) as i32 as i64, cancel) {
            return Err(CaptureError::Interrupted);
        }
        if timed_out() {
            return Err(CaptureError::Timeout);
        }
    }
    Ok(img)
}

/// `CameraSimulator::ST4PulseGuideScope`: move the field, then wait for the
/// pulse; a stop cuts the wait short, and the pulse still counts as made.
fn pulse(sh: &Shared, dir: Dir, ms: u32, cancel: &CancelToken) {
    let time = {
        let mut s = lock(sh);
        s.pulse_move(dir, ms);
        s.time.clone()
    };
    time.milli_sleep(ms as i64, cancel);
}

/// The camera simulator (PHD2's "Simulator" camera). Make one per rig and
/// take the mount, AO and clock from it.
#[derive(Clone)]
pub struct SimCamera(Shared);

impl SimCamera {
    pub fn new(params: SimParams, time: SimTime) -> SimCamera {
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
            frame_size: UNDEFINED_FRAME_SIZE,
            cooler: Cooler::new(),
            time: Arc::new(Timer {
                real: (time == SimTime::Real).then(SystemClock::default),
                sim_ms: AtomicU64::new(0),
            }),
            ra_h: 0.0,
            dec_deg: None,
            lst0_h: 0.0,
            ao: None,
            ao_max_steps_to_save: None,
            debug: Vec::new(),
        };
        SimCamera(Arc::new(Mutex::new(state)))
    }

    /// The mount: the rig's pointing source.
    pub fn mount(&self) -> SimMount {
        SimMount(self.0.clone())
    }

    pub fn ao(&self) -> SimAo {
        SimAo(self.0.clone())
    }

    pub fn clock(&self) -> SimClock {
        SimClock(lock(&self.0).time.clone())
    }

    /// The settings in use, the pier side as the frames left it.
    pub fn params(&self) -> SimParams {
        lock(&self.0).p.clone()
    }

    /// New settings, as the simulator's settings dialog applies them
    /// (`ShowPropertyDialog`): a new star or hot-pixel count, backlash or
    /// camera angle draws a new field at once (`Initialize`), the rest
    /// shows from the next frame. Start from [`SimCamera::params`] to keep
    /// the pier side, as the dialog does. The AO's travel is the AO's own
    /// setting ([`StepGuiderDevice::set_max_position`]) and stays.
    pub fn set_params(&self, params: SimParams) {
        let mut s = lock(&self.0);
        let new_field = params.nr_stars != s.p.nr_stars
            || params.nr_hot_pixels != s.p.nr_hot_pixels
            || params.dec_backlash != s.p.dec_backlash
            || params.cam_angle != s.p.cam_angle;
        s.p = SimParams { ao_max_position: s.p.ao_max_position, ..params };
        if new_field {
            s.initialize();
        }
    }

    /// `CameraSimulator::FlipPierSide`, which only the legacy socket server
    /// calls (`MSG_FLIP_SIM_CAMERA`): the other side, East from Unknown. In
    /// PHD2's rig the next frame takes Unknown back from the mount. The
    /// settings dialog's Pier Flip differs: it also turns the camera angle
    /// by 180° (truncated to a whole degree, mod 360), through
    /// [`SimCamera::set_params`].
    pub fn flip_pier_side(&self) {
        let mut s = lock(&self.0);
        s.p.pier_side = if s.p.pier_side == PierSide::East { PierSide::West } else { PierSide::East };
        let line = format!(
            "CamSimulator FlipPierSide: side = {}  cam_angle = {}",
            s.p.pier_side as i32,
            fmt_f(s.p.cam_angle, 1)
        );
        s.debug.push(line);
    }

    /// `CameraSimulator::Connect`: a new star field (`Initialize`) from
    /// the params, which the caller loads first (`load_sim_params`).
    pub fn connect(&self) {
        let mut s = lock(&self.0);
        s.initialize();
        s.connected = true;
    }

    pub fn disconnect(&self) {
        lock(&self.0).connected = false;
    }

    /// The debug-log lines of the camera, the mount and the AO since the
    /// last call, oldest first.
    pub fn take_debug(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.0).debug)
    }
}

impl GuideCamera for SimCamera {
    fn name(&self) -> String {
        "Simulator".into()
    }

    fn is_connected(&self) -> bool {
        lock(&self.0).connected
    }

    fn frame_size(&self) -> (i32, i32) {
        lock(&self.0).frame_size
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

    fn has_cooler(&self) -> bool {
        true
    }

    fn set_cooler_on(&mut self, on: bool) -> bool {
        let mut s = lock(&self.0);
        let now = s.time.wall().timestamp();
        if on {
            s.cooler.turn_on(now)
        } else {
            s.cooler.turn_off(now)
        }
        false
    }

    /// Refused while the cooler is off.
    fn set_cooler_setpoint(&mut self, celsius: f64) -> bool {
        let mut s = lock(&self.0);
        if !s.cooler.on {
            return true;
        }
        let now = s.time.wall().timestamp();
        s.cooler.set_temp(celsius, now);
        false
    }

    fn cooler_status(&self) -> Option<CoolerStatus> {
        let s = lock(&self.0);
        Some(s.cooler.status(s.time.wall().timestamp()))
    }

    fn sensor_temperature(&self) -> Option<f64> {
        self.cooler_status().map(|c| c.temperature)
    }

    fn has_guide_output(&self) -> bool {
        true
    }

    fn st4_pulse(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> Result<(), String> {
        pulse(&self.0, dir, ms, cancel);
        Ok(())
    }

    /// The camera's, the mount's and the AO's lines ([`SimCamera::take_debug`]).
    fn take_debug(&mut self) -> Vec<String> {
        SimCamera::take_debug(self)
    }

    /// The frame kind is the engine's business: the simulator reads the
    /// shutter from `settings`.
    fn capture(
        &mut self,
        dur_ms: u32,
        _kind: CaptureKind,
        subframe: Option<Rect>,
        settings: &CameraSettings,
        cancel: &CancelToken,
    ) -> Result<UsImage, CaptureError> {
        capture(&self.0, dur_ms, subframe, settings, cancel)
    }
}

/// The simulated mount: the rig's pointing source (see the module doc). Its
/// pulses go through the camera ([`crate::engine::camera::OnCamera`]).
#[derive(Clone)]
pub struct SimMount(Shared);

impl SimMount {
    /// Point at `ra_h` hours, `dec_deg` degrees at once; `None` goes back
    /// to PHD2's rig, with no position. The next frame moves the worm phase
    /// by the RA change. Choose before the first frame: in PHD2's rig that
    /// frame sets the simulator's pier side to Unknown.
    pub fn slew_to(&self, ra_h: f64, dec_deg: Option<f64>) {
        let mut s = lock(&self.0);
        s.ra_h = ra_h;
        s.dec_deg = dec_deg;
    }
}

impl PointingSource for SimMount {
    fn coordinates(&self) -> Option<(f64, f64, f64)> {
        lock(&self.0).coordinates()
    }

    fn pier_side(&self) -> PierSide {
        lock(&self.0).pier_side()
    }

    fn guide_rates(&self) -> Option<(f64, f64)> {
        lock(&self.0).guide_rates()
    }

    fn is_slewing(&self) -> bool {
        false
    }

    fn can_report_position(&self) -> bool {
        lock(&self.0).can_report_position()
    }
}

/// The AO simulator (`StepGuiderSimulator`, "AO-Simulator"): 0.1 px per
/// step at 35° to the camera. PHD2's `StepGuider` base class keeps the
/// position and only steps inside the travel; the device keeps its own copy
/// here, which the frames show.
#[derive(Clone)]
pub struct SimAo(Shared);

impl SimAo {
    /// `StepGuiderSimulator::Connect`, centred (`ZeroCurrentPosition`).
    /// PHD2's alert when the camera is not the simulator cannot happen: the
    /// AO comes from the [`SimCamera`].
    pub fn connect(&mut self) {
        let mut s = lock(&self.0);
        s.ao = Some((0, 0));
        s.debug.push("AO Simulator Connected".into());
    }

    pub fn disconnect(&mut self) {
        let mut s = lock(&self.0);
        if s.ao.take().is_some() {
            s.debug.push("AO Simulator Disconnected".into());
        }
    }

    pub fn is_connected(&self) -> bool {
        lock(&self.0).ao.is_some()
    }

    /// The position in steps (right, up).
    pub fn position(&self) -> (i32, i32) {
        lock(&self.0).ao.unwrap_or_default()
    }

    /// The profile write [`StepGuiderDevice::set_max_position`] owes:
    /// PHD2's simulator stores its travel itself, the device seam has no
    /// profile, so the caller saves it.
    pub fn take_profile_write(&self) -> Option<(&'static str, i32)> {
        lock(&self.0).ao_max_steps_to_save.take().map(|steps| (SIM_AO_MAX_STEPS_KEY, steps))
    }
}

impl StepGuiderDevice for SimAo {
    fn name(&self) -> String {
        "AO-Simulator".into()
    }

    /// North is up, East right, 5 ms each, never refused: the step guider
    /// keeps the moves inside the travel.
    fn step(&mut self, dir: Dir, steps: u32) -> StepResult {
        let time = {
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
            s.time.clone()
        };
        // `wxMilliSleep`: not interruptible.
        time.sleep(steps as u64 * AO_MS_PER_STEP);
        StepResult::Ok
    }

    /// The same travel every way.
    fn max_position(&self, _dir: Dir) -> i32 {
        lock(&self.0).p.ao_max_position
    }

    /// Never fails; the profile write waits in [`SimAo::take_profile_write`].
    fn set_max_position(&mut self, steps: i32) -> bool {
        let mut s = lock(&self.0);
        s.p.ao_max_position = steps;
        s.ao_max_steps_to_save = Some(steps);
        false
    }

    fn center(&mut self) -> bool {
        let mut s = lock(&self.0);
        if s.ao.is_some() {
            s.ao = Some((0, 0));
        }
        true
    }
}

/// The simulator's clock: monotonic time is the simulator's, and the wall
/// clock is the system's on the real clock, else it runs from 2026-01-01
/// 00:00 UTC.
#[derive(Clone)]
pub struct SimClock(Arc<Timer>);

impl Clock for SimClock {
    fn mono(&self) -> f64 {
        self.0.now_ms() as f64 / 1000.0
    }

    fn wall(&self) -> DateTime<Local> {
        self.0.wall()
    }

    fn sleep_ms(&self, ms: u64) {
        self.0.sleep(ms);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::engine::algo::AlgoCtx;
    use crate::engine::calibration::{CALIBRATION_RATE_UNCALIBRATED, CalibrationIssue, degrees, norm_angle};
    use crate::engine::camera::OnCamera;
    use crate::engine::mount::{GuideOutput, GuiderOffset, MountEvent, StepFrame};
    use crate::engine::scope::{CalibrationEnv, CalibrationNext, DecGuideMode, Scope, ScopeIo};
    use crate::engine::star::{FindMode, Star};
    use crate::engine::stepguider::{BumpEnv, StepGuider};
    use crate::engine::traits::{MoveOpts, MoveResult, Point};

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

    /// A connected camera on simulated time, in PHD2's rig.
    fn connected(p: SimParams) -> SimCamera {
        let cam = SimCamera::new(p, SimTime::Simulated);
        cam.connect();
        cam
    }

    /// The same with the mount pointing at RA 0 h, Dec 25° (the
    /// declination PHD2's pulses assume when none is known).
    fn pointing(p: SimParams) -> SimCamera {
        let cam = connected(p);
        cam.mount().slew_to(0.0, Some(25.0));
        cam
    }

    /// PHD2's camera defaults, shutter open.
    fn settings() -> CameraSettings {
        CameraSettings::load(&NoProfile)
    }

    /// A pulse of the "On Camera" mount.
    fn guide(cam: &SimCamera, dir: Dir, ms: u32) -> MoveResult {
        let mut cam = cam.clone();
        OnCamera(&mut cam, &mut Vec::new()).guide(dir, ms, &CancelToken::new())
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
        let img = cam.capture(1000, CaptureKind::Light, subframe, &settings(), &CancelToken::new()).unwrap();
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
            // The frame size is unknown until the first frame.
            assert_eq!(cam.frame_size(), (0, 0));
            let img = cam.capture(1000, CaptureKind::Light, None, &settings(), &cancel).unwrap();
            assert_eq!(cam.frame_size(), (752, 580));
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

        // With the shutter closed: the noise, 2 × (3000 + 0..3000), and the
        // hot pixels.
        let closed = CameraSettings { shutter_closed: true, ..settings() };
        let dark = cam.capture(1000, CaptureKind::Dark, None, &closed, &cancel).unwrap();
        let hot = dark.data.iter().filter(|&&v| v == u16::MAX).count();
        assert_eq!(hot, 8);
        assert!(dark.data.iter().all(|&v| v == u16::MAX || (6000..=11998).contains(&v)));

        // A subframe is the only area read; one past the frame edge is dropped.
        let sub = Rect::new(600, 420, 100, 100);
        let img = cam.capture(500, CaptureKind::Light, Some(sub), &settings(), &cancel).unwrap();
        assert_eq!(img.subframe, Some(sub));
        assert_eq!((img.pixel(599, 420), img.pixel(600, 520)), (0, 0));
        assert!(img.pixel(600, 420) >= 3000 && img.pixel(699, 519) >= 3000);
        let img = cam.capture(500, CaptureKind::Light, Some(Rect::new(701, 0, 52, 10)), &settings(), &cancel).unwrap();
        assert_eq!(img.subframe, None);
        assert!(img.data.iter().all(|&v| v > 0));
    }

    #[test]
    fn frame_size_watchdog_and_download_time() {
        let cancel = CancelToken::new();
        let mut cam = connected(quiet(1.0));
        cam.capture(100, CaptureKind::Light, None, &settings(), &cancel).unwrap();
        // A new binning shows with the next frame.
        cam.set_binning(2);
        assert_eq!(cam.frame_size(), (752, 580));
        cam.capture(100, CaptureKind::Light, None, &settings(), &cancel).unwrap();
        assert_eq!(cam.frame_size(), (376, 290));

        // A negative download time wraps to a sleep ≤ 0: none at all.
        let clock = cam.clock();
        for (dur, download, waited) in [(1000, -100, 0.995), (50, -100, 0.045), (50, 20, 0.07)] {
            cam.set_params(SimParams { frame_download_ms: download as u32, ..quiet(1.0) });
            let t = clock.mono();
            cam.capture(dur, CaptureKind::Light, None, &settings(), &cancel).unwrap();
            assert!((clock.mono() - t - waited).abs() < 1e-9, "{dur} {download}");
        }

        // A negative timeout in the profile makes the watchdog's unsigned
        // sum wrap below the exposure: the camera drops at once.
        let short = CameraSettings { timeout_ms: -1500, ..settings() };
        assert_eq!(cam.capture(2000, CaptureKind::Light, None, &short, &cancel).unwrap_err(), CaptureError::Timeout);
        assert!(!cam.is_connected());
    }

    #[test]
    fn the_cooler_runs_at_an_eighth_of_a_degree_a_second() {
        let mut cam = connected(quiet(1.0));
        let clock = cam.clock();
        let off = CoolerStatus { on: false, setpoint: 0.0, power: 0.0, temperature: 15.0 };
        assert_eq!(cam.cooler_status(), Some(off));
        assert!(cam.set_cooler_setpoint(-10.0), "refused while off");
        assert!(!cam.set_cooler_on(true));
        assert!(!cam.set_cooler_setpoint(-10.0));
        // 25 °C to go: 200 s.
        clock.sleep_ms(100_000);
        let s = cam.cooler_status().unwrap();
        assert_eq!((s.on, s.setpoint, s.temperature), (true, -10.0, 2.5));
        assert!((s.power - 12.5 * 100.0 / 30.0).abs() < 1e-12);
        clock.sleep_ms(100_000);
        assert_eq!(cam.sensor_temperature(), Some(-10.0));
        // Held to −15 °C, the setpoint shown as asked.
        cam.set_cooler_setpoint(-40.0);
        clock.sleep_ms(60_000);
        let s = cam.cooler_status().unwrap();
        assert_eq!((s.setpoint, s.temperature, s.power), (-40.0, -15.0, 100.0));
        cam.set_cooler_on(false);
        clock.sleep_ms(240_000);
        assert_eq!(cam.cooler_status(), Some(CoolerStatus { setpoint: 0.0, ..off }));
    }

    #[test]
    fn real_time_takes_the_waits_and_a_stop_cuts_them() {
        let cam = SimCamera::new(quiet(1.0), SimTime::Real);
        cam.connect();
        let clock = cam.clock();
        let mut c = cam.clone();
        let t = Instant::now();
        c.capture(120, CaptureKind::Light, None, &settings(), &CancelToken::new()).unwrap();
        // The exposure and the 50 ms download, in fact and on the clock.
        assert!(t.elapsed() >= Duration::from_millis(170));
        assert!(clock.mono() >= 0.17);
        assert!((clock.wall() - Local::now()).num_seconds().abs() < 5);

        // A stop from another thread ends a 10 s exposure within a slice.
        let cancel = CancelToken::new();
        let stop = cancel.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            stop.cancel();
        });
        let t = Instant::now();
        let r = c.capture(10_000, CaptureKind::Light, None, &settings(), &cancel);
        stopper.join().unwrap();
        assert_eq!(r.unwrap_err(), CaptureError::Interrupted);
        assert!(t.elapsed() >= Duration::from_millis(150) && t.elapsed() < Duration::from_secs(5));

        // A pulse waits its length, an AO step 5 ms.
        let t = Instant::now();
        assert_eq!(guide(&cam, Dir::North, 60), MoveResult::Ok);
        let mut ao = cam.ao();
        ao.connect();
        ao.step(Dir::East, 8);
        assert!(t.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn a_pulse_moves_the_star_along_the_camera_angle() {
        let p = SimParams { dec_backlash: 0.0, ..quiet(1.0) };
        let mut cam = connected(p);
        let (ra, dec) = axes();

        let a = bright(&mut cam);
        // Star 12 at (319, 109) turned 15° about the frame centre.
        assert!(a.distance(&Point::new(655.919, 477.849)) < 0.001, "{a:?}");
        assert_eq!(guide(&cam, Dir::West, 1000), MoveResult::Ok);
        let b = bright(&mut cam);
        // 15″/s for 1 s at 1″/px, times cos 25° in RA.
        let d = b - a;
        assert!((d.x.hypot(d.y) - 15.0 * radians(25.0).cos()).abs() < 0.01, "{d:?}");
        assert!((degrees(d.y.atan2(d.x)) - 15.0).abs() < 0.05, "{d:?}");
        assert!(dot(d, dec).abs() < 0.01);

        guide(&cam, Dir::North, 1000);
        let c = bright(&mut cam);
        let d = c - b;
        assert!((dot(d, dec) - 15.0).abs() < 0.01 && dot(d, ra).abs() < 0.01, "{d:?}");

        // Three frames and two pulses of simulated time.
        assert!((cam.clock().mono() - 5.15).abs() < 1e-9);

        // A pulse through a camera that is gone is refused.
        cam.disconnect();
        assert_eq!(guide(&cam, Dir::North, 1000), MoveResult::Error);
    }

    #[test]
    fn dec_backlash_eats_the_first_reversal() {
        // 5″ at 1″/px, the state of a mount last moved South.
        let mut cam = connected(quiet(1.0));
        let (_, dec) = axes();
        let mut moves = Vec::new();
        let mut last = bright(&mut cam);
        for (dir, ms) in [(Dir::North, 1000), (Dir::South, 200), (Dir::South, 400)] {
            guide(&cam, dir, ms);
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
        let mut debug = Vec::new();
        assert_eq!(s.adjustment(Dir::North, 500, 9.0, &mut debug), 0.0);
        assert_eq!(s.adjustment(Dir::South, 600, 9.0, &mut debug), -3.0);
        // Given back when the next pulse keeps the direction.
        assert_eq!(s.adjustment(Dir::South, 600, 9.0, &mut debug), 3.0);
        assert_eq!(s.adjustment(Dir::North, 200, 3.0, &mut debug), 0.0);
        // A reversal followed by a reversal gives nothing back.
        assert_eq!(s.adjustment(Dir::South, 900, 6.0, &mut debug), -2.0);
        assert_eq!(s.adjustment(Dir::North, 900, 6.0, &mut debug), 0.0);
        assert_eq!(
            debug,
            [
                "Stiction: reduced distance by 3.00",
                "Stiction: increased distance by 3.00",
                "Stiction: reduced distance by 2.00",
            ]
        );
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
    fn in_phd2_rig_the_mount_knows_nothing_and_the_pier_side_follows_it() {
        let mut cam = connected(SimParams { dec_backlash: 0.0, ..quiet(1.0) });
        let mount = cam.mount();
        assert_eq!(
            (mount.coordinates(), mount.pier_side(), mount.guide_rates(), mount.can_report_position()),
            (None, PierSide::Unknown, None, false)
        );
        assert_eq!(mount.declination_radians(), None);
        let a = bright(&mut cam);
        // The first frame takes the mount's side over the profile's East.
        assert_eq!(cam.take_debug(), ["Cam simulator: pointing source pier side changed from 0 to -1"]);
        // So a flip lasts until the next frame: no turn of the field.
        cam.flip_pier_side();
        let b = bright(&mut cam);
        assert!(b.distance(&a) < 0.01, "{a:?} {b:?}");
        assert_eq!(
            cam.take_debug(),
            [
                "CamSimulator FlipPierSide: side = 0  cam_angle = 15.0",
                "Cam simulator: pointing source pier side changed from 0 to -1",
            ]
        );
        assert_eq!(cam.params().pier_side, PierSide::Unknown);
    }

    #[test]
    fn a_pier_flip_turns_the_field_and_the_dec_pulses() {
        let mut cam = pointing(SimParams { dec_backlash: 0.0, ..quiet(1.0) });
        let mount = cam.mount();
        let a = bright(&mut cam);
        cam.flip_pier_side();
        assert_eq!(mount.pier_side(), PierSide::West);
        let b = bright(&mut cam);
        assert!(b.distance(&Point::new(752.0 - a.x, 580.0 - a.y)) < 0.01, "{a:?} {b:?}");
        // North is reversed on the West side, so a North pulse still moves
        // the star along +Dec on the camera; West now goes the other way.
        let (ra, dec) = axes();
        guide(&cam, Dir::North, 1000);
        let c = bright(&mut cam);
        assert!((dot(c - b, dec) - 15.0).abs() < 0.01, "{:?}", c - b);
        guide(&cam, Dir::West, 1000);
        let d = bright(&mut cam);
        assert!((dot(d - c, ra) + 15.0 * radians(25.0).cos()).abs() < 0.01, "{:?}", d - c);
    }

    #[test]
    fn new_settings_draw_a_new_field_only_when_phd2_does() {
        let cam = connected(quiet(1.0));
        guide(&cam, Dir::West, 1000);
        // The seeing is not part of the field: the pulse stays.
        cam.set_params(SimParams { seeing_scale: 1.0, ..cam.params() });
        assert!(lock(&cam.0).ra_ofs > 0.0);
        // The camera angle is: back to a new field.
        cam.set_params(SimParams { cam_angle: 20.0, ..cam.params() });
        assert_eq!(lock(&cam.0).ra_ofs, 0.0);
        // The AO's travel is the AO's.
        let mut ao = cam.ao();
        ao.set_max_position(30);
        cam.set_params(SimParams { ao_max_position: 99, ..quiet(1.0) });
        assert_eq!(ao.max_position(Dir::UP), 30);
    }

    /// The engine's calibration loop on the simulator: a frame, the star
    /// found near its last place, the state machine, its move before the
    /// next frame. Returns the `Calibrating` events' directions in order,
    /// once each, and the number of backlash-clearing pulses.
    fn calibrate(scope: &mut Scope, cam: &mut SimCamera, profile: &mut Profile) -> (Vec<&'static str>, usize) {
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
        let mut output = cam.clone();
        let mut lines = Vec::new();
        let pointing = cam.mount();
        let clock = cam.clock();
        let cancel = CancelToken::new();
        let mut ev = Vec::new();
        let mut legs = Vec::new();
        let mut clearing = 0;
        let mut pos = bright(cam);
        assert!(scope.begin_calibration(Some(pos), &env, Some(&pointing), profile, &mut ev));
        for _ in 0..200 {
            pos = locate(cam, pos, None);
            let next = scope.update_calibration_state(pos, &env, Some(&pointing), profile, &mut ev);
            for e in ev.drain(..) {
                if let MountEvent::Calibrating(i) = e {
                    clearing += i.msg.starts_with("Clearing backlash") as usize;
                    if legs.last() != Some(&i.direction) {
                        legs.push(i.direction);
                    }
                }
            }
            match next {
                CalibrationNext::Moves(moves) => {
                    for (dir, ms) in moves {
                        let now = clock.mono();
                        let mut out = OnCamera(&mut output, &mut lines);
                        let mut io = ScopeIo { output: &mut out, pointing: Some(&pointing), cancel: &cancel, now };
                        assert_eq!(scope.move_axis(dir, ms, MoveOpts::CALIBRATION_MOVE, &mut io, &mut ev), MoveResult::Ok);
                    }
                }
                CalibrationNext::Failed => panic!("calibration failed"),
            }
            if scope.is_calibrated() {
                return (legs, clearing);
            }
        }
        panic!("calibration did not end");
    }

    #[test]
    fn the_engine_calibrates_on_the_simulator() {
        // PHD2's defaults at 2″/px (PE, seeing, read noise, Dec backlash)
        // but the Dec drift: its 5″/min over the RA legs tilts the RA axis
        // by about 1° (−164° here), in PHD2 as well.
        let mut cam = pointing(SimParams { dec_drift_rate: 0.0, ..SimParams::new(2.0) });
        let mut profile = Profile::default();
        let mut scope = Scope::new(&mut profile, false, 0.0, &mut Vec::new());
        scope.mount.connect();

        let (legs, clearing) = calibrate(&mut scope, &mut cam, &mut profile);
        // The South leg ends more than 2 px short of the start (2.5 px of
        // Dec backlash), so PHD2 nudges South back towards it.
        assert_eq!(legs, ["West", "East", "Backlash", "North", "South", "NudgeSouth"]);
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
        let (legs, clearing) = calibrate(&mut scope, &mut cam, &mut profile);
        assert_eq!((legs, clearing), (vec!["West", "East"], 0));
        assert_eq!(scope.mount.y_rate(), CALIBRATION_RATE_UNCALIBRATED);
        assert!(degrees(norm_angle(scope.mount.x_angle() - radians(-165.0))).abs() < 1.0);
    }

    /// PHD2's step guider on its AO simulator: the calibration finds the
    /// simulator's 0.1 px per step at 35° to the camera.
    #[test]
    fn the_step_guider_calibrates_on_the_simulated_ao() {
        let mut cam = connected(quiet(1.0));
        let mut dev = cam.ao();
        dev.connect();
        let mut profile = Profile::default();
        let mut ev = Vec::new();
        let mut sg = StepGuider::new(&mut profile, 0.0, &mut ev);
        sg.mount_mut().set_name(&dev.name());
        sg.connect(&dev, &mut ev);
        let env = CalibrationEnv { binning: 1, image_scale: 1.0, timestamp: "T".into(), ..CalibrationEnv::default() };
        let mut pos = bright(&mut cam);
        assert!(sg.begin_calibration(Some(pos), &mut ev));
        for _ in 0..200 {
            pos = locate(&mut cam, pos, None);
            let next = sg.update_calibration_state(pos, &env, &mut dev, &mut profile, &mut ev);
            let CalibrationNext::Moves(moves) = next else { panic!("calibration failed") };
            for (dir, steps) in moves {
                assert_eq!(sg.move_axis(dir, steps, MoveOpts::CALIBRATION_MOVE, &mut dev, &mut ev), MoveResult::Ok);
            }
            if sg.is_calibrated() {
                break;
            }
        }
        let c = sg.mount().mount_cal();
        assert!((degrees(c.x_angle) - 35.0).abs() < 0.5, "{}", degrees(c.x_angle));
        assert!((degrees(c.y_angle) - 125.0).abs() < 0.5, "{}", degrees(c.y_angle));
        assert!((c.x_rate - 0.1).abs() < 0.002 && (c.y_rate - 0.1).abs() < 0.002, "{} {}", c.x_rate, c.y_rate);
        // The device followed, back at the centre: 44 steps each way are 11
        // calibration iterations of 4.
        assert_eq!((sg.ao_pos(), dev.position()), ((0, 0), (0, 0)));
    }

    #[test]
    fn the_ao_offsets_the_field_and_the_clock_runs_with_its_steps() {
        let mut cam = connected(quiet(1.0));
        let mut ao = cam.ao();
        let before = field_pos(&cam, BRIGHT);
        ao.connect();
        assert_eq!(ao.step(Dir::East, 20), StepResult::Ok);
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

        // The AO Travel setting: PHD2's profile write is the caller's.
        assert_eq!(ao.take_profile_write(), None);
        assert!(!ao.set_max_position(30));
        assert_eq!((ao.max_position(Dir::UP), ao.take_profile_write()), (30, Some(("/SimAo/max_steps", 30))));
        assert_eq!(ao.take_profile_write(), None);
        ao.disconnect();
        ao.disconnect();
        assert_eq!(
            cam.take_debug(),
            [
                "AO Simulator Connected",
                "Cam simulator: pointing source pier side changed from 0 to -1",
                "AO Simulator Disconnected",
            ]
        );
    }

    /// The step guider's calibration on the AO simulator; returns the star.
    fn calibrate_ao(sg: &mut StepGuider, cam: &mut SimCamera, ao: &mut SimAo, profile: &mut Profile) -> Point {
        let env = CalibrationEnv {
            binning: 1,
            focal_length: 400,
            image_scale: 2.0,
            timestamp: "T".into(),
            ..CalibrationEnv::default()
        };
        let mut ev = Vec::new();
        let mut pos = bright(cam);
        assert!(sg.begin_calibration(Some(pos), &mut ev));
        for _ in 0..200 {
            pos = locate(cam, pos, None);
            match sg.update_calibration_state(pos, &env, ao, profile, &mut ev) {
                CalibrationNext::Moves(moves) => {
                    for (dir, n) in moves {
                        assert_eq!(sg.move_axis(dir, n, MoveOpts::CALIBRATION_MOVE, ao, &mut ev), MoveResult::Ok);
                    }
                }
                CalibrationNext::Failed => panic!("AO calibration failed"),
            }
            if sg.is_calibrated() {
                return pos;
            }
        }
        panic!("AO calibration did not end");
    }

    #[test]
    fn the_step_guider_calibrates_and_bumps_on_the_simulated_ao() {
        let mut cam = pointing(SimParams { dec_backlash: 0.0, ..quiet(2.0) });
        let mut profile = Profile::default();
        let mut scope = Scope::new(&mut profile, false, 0.0, &mut Vec::new());
        scope.mount.connect();
        calibrate(&mut scope, &mut cam, &mut profile);

        let mut ao = cam.ao();
        ao.connect();
        ao.set_max_position(20);
        let mut sg = StepGuider::new(&mut profile, 0.0, &mut Vec::new());
        sg.connect(&ao, &mut Vec::new());
        let lock_pos = calibrate_ao(&mut sg, &mut cam, &mut ao, &mut profile);
        // 0.1 px a step, right along 35° and up along 125° on the camera.
        let c = sg.mount().mount_cal().clone();
        assert!(degrees(norm_angle(c.x_angle - radians(35.0))).abs() < 1.0, "{}", degrees(c.x_angle));
        assert!(degrees(norm_angle(c.y_angle - radians(125.0))).abs() < 1.0, "{}", degrees(c.y_angle));
        assert!((c.x_rate / 0.1 - 1.0).abs() < 0.02 && (c.y_rate / 0.1 - 1.0).abs() < 0.02, "{c:?}");
        assert_eq!(sg.ao_pos(), ao.position());

        // Guiding on the AO through PHD2's default Dec drift, 5″/min: 0.44
        // AO steps a frame, until the mount is bumped.
        cam.set_params(SimParams { dec_drift_rate: 5.0 / (2.0 * 60.0), ..cam.params() });
        let mut output = cam.clone();
        let pointing = cam.mount();
        let clock = cam.clock();
        let cancel = CancelToken::new();
        let mut ev = Vec::new();
        let (mut cycles, mut highest, mut worst) = (0, 0, 0.0f64);
        let mut pos = lock_pos;
        for _ in 0..200 {
            pos = locate(&mut cam, pos, None);
            let now = clock.mono();
            let ctx = AlgoCtx { now, ..AlgoCtx::default() };
            let env = BumpEnv { secondary_connected: true, max_move_pixels: 15, now, ..BumpEnv::default() };
            let mut ofs = GuiderOffset { camera_ofs: pos - lock_pos, mount_ofs: None };
            let bumping = sg.is_bump_in_progress();
            let (r, bump) = sg.move_offset(
                &mut ofs,
                MoveOpts::GUIDE_STEP,
                StepFrame::default,
                &ctx,
                &env,
                &mut ao,
                &profile,
                &mut ev,
            );
            assert_eq!(r, MoveResult::Ok);
            if let Some(mut bump) = bump {
                let mut lines = Vec::new();
                let mut out = OnCamera(&mut output, &mut lines);
                let mut io = ScopeIo { output: &mut out, pointing: Some(&pointing), cancel: &cancel, now };
                scope.move_offset(
                    &mut bump,
                    MoveOpts::AO_BUMP,
                    StepFrame::default,
                    &ctx,
                    &mut io,
                    &mut profile,
                    &mut ev,
                );
            }
            cycles += (bumping && !sg.is_bump_in_progress()) as i32;
            let (x, y) = ao.position();
            highest = highest.max(x.abs()).max(y.abs());
            worst = worst.max(pos.distance(&lock_pos));
            ev.clear();
        }
        assert_eq!(sg.ao_pos(), ao.position());
        // Bumps start past 80 % of the 20-step travel and run until the AO
        // is back within 10 % of the travel from the centre: never at its
        // end, the star never half a pixel off.
        assert!(cycles >= 2 && highest >= 16 && highest < 19, "{cycles} {highest}");
        assert!(worst < 0.5, "{worst}");
    }

    // PHD2's own render_star, render_comet, BacklashVal, StictionSim and
    // canned PE sum, compiled from gear_simulator.cpp in a throwaway
    // harness: the pixels each renderer sets, `(x,y,value)` row by row.
    const STAR1: &str = "(9,6,19) (10,6,62) (11,6,42) (12,6,8) (8,7,19) (9,7,462) (10,7,1455) (11,7,994) (12,7,214) \
        (13,7,8) (8,8,100) (9,8,2130) (10,8,6691) (11,8,4572) (12,8,994) (13,8,42) (8,9,146) (9,9,3118) \
        (10,9,9794) (11,9,6691) (12,9,1455) (13,9,62) (8,10,45) (9,10,992) (10,10,3118) (11,10,2130) \
        (12,10,462) (13,10,19) (9,11,45) (10,11,146) (11,11,100) (12,11,19)";
    const STAR2: &str = "(9,5,12) (10,5,84) (11,5,150) (12,5,50) (8,6,12) (9,6,328) (10,6,1884) (11,6,3365) (12,6,1125) \
        (13,6,50) (8,7,53) (9,7,1321) (10,7,7517) (11,7,13431) (12,7,4490) (13,7,212) (8,8,64) \
        (9,8,1596) (10,8,9079) (11,8,16221) (12,8,5423) (13,8,256) (8,9,18) (9,9,476) (10,9,2721) \
        (11,9,4860) (12,9,1625) (13,9,75) (9,10,18) (10,10,126) (11,10,225) (12,10,75)";
    const STAR_CLIP: &str = "(9,8,2130) (10,8,6691) (11,8,4572) (9,9,3118) (10,9,9794) (11,9,6691)";
    const STAR_SAT: &str = "(4,3,9375) (5,3,25781) (6,3,9375) (3,4,9375) (4,4,65535) (5,4,65535) (6,4,65535) (7,4,9375) \
        (3,5,25781) (4,5,65535) (5,5,65535) (6,5,65535) (7,5,25781) (3,6,9375) (4,6,65535) (5,6,65535) \
        (6,6,65535) (7,6,9375) (4,7,9375) (5,7,25781) (6,7,9375)";
    const STAR_NEG: &str = "(0,1,19) (1,1,65535) (0,2,402) (1,2,65535) (2,2,65535) (0,3,1263) (1,3,65535) (2,3,65535) \
        (0,4,862) (1,4,65535) (2,4,65535) (0,5,188) (1,5,65535) (2,5,65535) (0,6,8) (1,6,65535)";
    const COMET: &str = "(17,1,25313) (16,2,25313) (15,3,25313) (14,4,25313) (13,5,25313) (17,5,25313) (12,6,25313) \
        (15,6,25313) (16,6,25313) (11,7,25313) (13,7,25313) (14,7,25313) (10,8,25313) (11,8,25313) \
        (12,8,25313) (9,9,50626) (10,9,25313) (8,10,65535) (9,10,50626) (10,10,25313) (11,10,25313) \
        (12,10,25313) (13,10,25313) (14,10,25313) (15,10,25313) (16,10,25313) (17,10,25313) (9,11,25313) \
        (10,11,25313) (11,11,25313) (10,12,25313) (12,12,25313) (13,12,25313) (11,13,25313) \
        (14,13,25313) (15,13,25313) (12,14,25313) (16,14,25313) (17,14,25313) (13,15,25313) \
        (14,16,25313) (15,17,25313) (16,18,25313) (17,19,25313)";

    fn dump(img: &UsImage) -> String {
        let mut out = Vec::new();
        for y in 0..img.h {
            for x in 0..img.w {
                let v = img.pixel(x, y);
                if v != 0 {
                    out.push(format!("({x},{y},{v})"));
                }
            }
        }
        out.join(" ")
    }

    fn star(w: i32, h: i32, bin: i32, sub: Rect, x: f64, y: f64, inten: f64) -> String {
        let mut img = UsImage::new(w, h);
        render_star(&mut img, bin, sub, x, y, inten);
        dump(&img)
    }

    #[test]
    fn the_renderers_draw_phd2s_pixels() {
        assert_eq!(star(20, 20, 1, Rect::new(0, 0, 20, 20), 10.3, 8.7, 30000.0), STAR1);
        assert_eq!(star(20, 20, 2, Rect::new(0, 0, 20, 20), 21.6, 15.2, 50000.0), STAR2);
        assert_eq!(star(20, 20, 1, Rect::new(9, 8, 3, 2), 10.3, 8.7, 30000.0), STAR_CLIP);
        assert_eq!(star(12, 12, 1, Rect::new(0, 0, 12, 12), 5.0, 5.0, 3e6), STAR_SAT);
        // Left of the frame: negative weights wrap to 65535.
        assert_eq!(star(8, 8, 1, Rect::new(0, 0, 8, 8), -1.4, 3.3, 30000.0), STAR_NEG);
        let mut img = UsImage::new(30, 30);
        render_comet(&mut img, 1, Rect::new(0, 0, 30, 30), 10.2, 12.6, 3000.0 * 30.0 + 3000.0 + 1234.0);
        assert_eq!(dump(&img), COMET);
        // A NaN position is `INT_MIN` in C, and its corner wraps off the
        // frame instead of overflowing.
        assert_eq!(star(8, 8, 1, Rect::new(0, 0, 8, 8), f64::NAN, 3.0, 30000.0), "");
        render_comet(&mut img, 1, Rect::new(0, 0, 30, 30), f64::NAN, f64::NAN, 30000.0);
        assert_eq!(dump(&img), COMET);
    }

    #[test]
    fn backlash_stiction_and_pe_match_phd2() {
        let mut b = BacklashVal::new(2.5);
        let vals: Vec<f64> = [1.0, 3.0, -1.0, -2.0, -1.0, 0.5, 2.0, -6.0, 1.0]
            .iter()
            .map(|&d| {
                b.incr(d);
                b.val()
            })
            .collect();
        assert_eq!(vals, [2.5, 4.0, 4.0, 3.5, 2.5, 2.5, 2.5, -1.0, -1.0]);

        let (n, s) = (Dir::North, Dir::South);
        let pulses = [(n, 500, 3.0), (s, 500, 3.0), (s, 200, 1.0), (n, 400, 2.0), (n, 400, 2.0), (s, 250, 1.0)];
        let pulses = pulses.into_iter().chain([(n, 301, 6.0), (s, 301, 6.0), (s, 100, 9.0)]);
        let mut st = StictionSim::default();
        let mut debug = Vec::new();
        let adj: Vec<f64> = pulses.map(|(dir, ms, d)| st.adjustment(dir, ms, d, &mut debug)).collect();
        assert_eq!(adj, [0.0, -1.0, 1.0, -0.66666666666666663, 0.66666666666666663, 0.0, -2.0, 0.0, 0.0]);
        assert_eq!(
            debug,
            [
                "Stiction: reduced distance by 1.00",
                "Stiction: increased distance by 1.00",
                "Stiction: reduced distance by 0.67",
                "Stiction: increased distance by 0.67",
                "Stiction: reduced distance by 2.00",
            ]
        );

        // The canned PE at 5″ and 2″/px.
        let cam = connected(SimParams::new(2.0));
        let want = [
            1.5812903257789219,
            1.3346424175144089,
            -0.24680422967631901,
            -0.85225770600772333,
            1.2662156979642507,
            -0.29507853327753197,
            1.3904051594067719,
        ];
        for (t, want) in [0.0, 10.5, 57.25, 115.0, 230.5, 1000.0, -12.0].into_iter().zip(want) {
            let got = lock(&cam.0).pe(t);
            assert!((got - want).abs() < 1e-15, "{t}: {got} {want}");
        }
        // At 1″/px, sampled every 10 ms over the 230.5 s worm turn.
        let cam = connected(SimParams::new(1.0));
        let s = lock(&cam.0);
        let pe: Vec<f64> = (0..=23050).map(|k| s.pe(k as f64 * 0.01)).collect();
        let lo = pe.iter().copied().fold(f64::MAX, f64::min);
        let hi = pe.iter().copied().fold(f64::MIN, f64::max);
        assert!((lo + 1.8345325386719686).abs() < 1e-14 && (hi - 3.1625806515578438).abs() < 1e-14, "{lo} {hi}");
    }

    #[test]
    fn unsigned_short_casts_wrap_as_in_c() {
        // `(unsigned short)` of a double, as PHD2 casts noise and clouds.
        let wrapped: Vec<u16> =
            [66000.0, 65535.9, 70000.7, 131077.5, 150000.0, 95998.0].iter().map(|&v| c_int(v) as u16).collect();
        assert_eq!(wrapped, [464, 65535, 4464, 5, 18928, 30462]);
        // So full clouds over a 1 s frame, 50 × (3000 + 0..100), come out
        // as 18928..=23926: darker than the noise.
        let mut cam = connected(SimParams { clouds_opacity: 1.0, ..SimParams::new(1.0) });
        let img = cam.capture(1000, CaptureKind::Light, None, &settings(), &CancelToken::new()).unwrap();
        assert!(img.data.iter().all(|&v| v == u16::MAX || (18928..=23926).contains(&v)));
        assert_eq!(img.data.iter().filter(|&&v| v == u16::MAX).count(), 8);
    }
}
