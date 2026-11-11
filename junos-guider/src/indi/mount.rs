//! The INDI mount, PHD2's `ScopeINDI`, as the guide output and as the
//! pointing source, and as the aux mount that only reports where it points.
//!
//! Pulses go through `TELESCOPE_TIMED_GUIDE_NS/WE` in milliseconds, each
//! waiting for its axis to answer anything but Busy after the request (see
//! [`TimedGuide`]); a driver without them but with `TELESCOPE_MOTION_RATE`
//! and the motion switches is moved at 0.3× sidereal for the pulse's length.
//! Position from `EQUATORIAL_EOD_COORD` (else `EQUATORIAL_COORD`), slewing
//! while it is Busy, the pier side from `TELESCOPE_PIER_SIDE`, the guide
//! rates from `GUIDE_RATE` (× sidereal), the site from `GEOGRAPHIC_COORD`
//! and the sidereal time from `TIME_LST`, else computed from the system
//! clock and the site's longitude the way libnova does (no INDI driver
//! defines `TIME_LST`, so this is the usual path).
//!
//! The mount is connected while its driver's `CONNECT` switch is On. It
//! drops when the server goes ("INDI server disconnected"), when the device
//! goes (no alert), or when `CONNECT` turns Off once the mount had its guide
//! properties, or its coordinates for the aux mount ("INDI mount was
//! disconnected"); [`IndiMount::check_connection`] tells.
//!
//! Ported from PHD2 src/scope_indi.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::cell::RefCell;
use std::time::Duration;

use super::client::{self, IndiClient, Link, PulseEnd, TIMED_GUIDE_NS, TIMED_GUIDE_WE, TimedGuide};
use super::store::Property;
use super::xml::{Kind, PropState};
use crate::engine::calibration::{ProfileAccess, norm};
use crate::engine::mount::{GuideOutput, PointingSource};
use crate::engine::traits::{CancelToken, Dir, MoveResult, PierSide};

/// `/indi/INDImount`'s default: no mount chosen yet.
pub const UNSET_MOUNT: &str = "INDI Mount";
/// `dSiderealSecondPerSec` of `GetGuideRates`.
const SIDEREAL_SECOND_PER_SEC: f64 = 0.9973;
/// How long `SlewToCoordinates` waits for the slew to end.
const SLEW_TIMEOUT: Duration = Duration::from_secs(90);

/// The mount's profile keys (`ScopeINDI`'s constructor). PHD2's aux mount
/// is the same class and reads the same device key.
#[derive(Clone, Debug, PartialEq)]
pub struct MountOptions {
    /// `/indi/INDImount`.
    pub device: String,
    /// `/indi/PulseMinWait`.
    pub pulse_min_wait: bool,
}

impl MountOptions {
    pub fn from_profile(p: &impl ProfileAccess) -> Self {
        MountOptions { device: p.get_str("/indi/INDImount", UNSET_MOUNT), pulse_min_wait: client::pulse_min_wait(p) }
    }
}

pub struct IndiMount {
    client: IndiClient,
    device: String,
    /// The aux mount (`pGearDialog->AuxScope() == this`): ready with
    /// coordinates alone.
    aux: bool,
    /// The link generation the mount connected in.
    attached: Option<u64>,
    pulse: TimedGuide,
    /// Interior: `SideOfPier` logs from a `&self` call.
    debug: RefCell<Vec<String>>,
}

impl IndiMount {
    pub fn new(client: IndiClient, opts: &MountOptions, aux: bool) -> Self {
        IndiMount {
            client,
            device: opts.device.clone(),
            aux,
            attached: None,
            pulse: TimedGuide::new(opts.pulse_min_wait),
            debug: RefCell::default(),
        }
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// `m_Name`.
    pub fn name(&self) -> String {
        format!("INDI Mount [{}]", self.device)
    }

    /// `Connect`: waits up to 30 s for the device to connect.
    pub fn connect(&mut self, cancel: &CancelToken) -> Result<(), String> {
        if self.attached.is_some() {
            return Ok(());
        }
        if self.device.is_empty() || self.device == UNSET_MOUNT {
            return Err("No INDI mount is selected".into());
        }
        self.log(format!("INDI Mount connecting to device [{}]", self.device));
        self.log(format!("Waiting for 30s for [{}] to connect...", self.device));
        self.attached = Some(client::connect_device(&self.client, &self.device, cancel)?);
        // The guide properties come right after `CONNECT` (see the camera).
        if self.client.wait(client::READY_GRACE, Some(cancel), |l| self.ready(l).then_some(())).is_ok() {
            let link = self.client.lock();
            let has = |n: &str, k: Kind| self.has(&link, n, k) as i32;
            let line = format!(
                "INDI Telescope{} is ready MotionRate={} moveNS={} moveEW={} guideNS={} guideEW={} coord={}",
                if self.aux { " (AUX)" } else { "" },
                has("TELESCOPE_MOTION_RATE", Kind::Number),
                has("TELESCOPE_MOTION_NS", Kind::Switch),
                has("TELESCOPE_MOTION_WE", Kind::Switch),
                has(TIMED_GUIDE_NS, Kind::Number),
                has(TIMED_GUIDE_WE, Kind::Number),
                coord(&link, &self.device).is_some() as i32,
            );
            drop(link);
            self.log(line);
        }
        Ok(())
    }

    /// `Disconnect`: the mount lets go of the device, leaving the driver
    /// connected.
    pub fn disconnect(&mut self) {
        if self.attached.take().is_some() {
            self.log("INDI Mount: serverDisconnected".into());
        }
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected_in(&self.client.lock())
    }

    /// The drop PHD2 notices on its INDI thread, noticed here: the mount is
    /// disconnected, and the alert PHD2 raises for it, if any.
    pub fn check_connection(&mut self) -> Option<String> {
        let alert = self.lost(&self.client.lock())?;
        self.disconnect();
        alert.map(str::to_owned)
    }

    /// PHD2's debug-log lines since the last call.
    pub fn take_debug(&mut self) -> Vec<String> {
        std::mem::take(self.debug.get_mut())
    }

    /// `SlewToCoordinates`: the slew, then up to 90 s while the coordinates
    /// are Busy; true when they no longer are. Quirk kept: the first look
    /// comes right after the request, before the driver could answer Busy,
    /// so a slew the driver has not started yet counts as done.
    pub fn slew_to(&mut self, ra_hours: f64, dec_deg: f64, cancel: &CancelToken) -> bool {
        if !self.slew_to_async(ra_hours, dec_deg) {
            return false;
        }
        let device = &self.device;
        let done = |l: &mut Link| coord(l, device).is_none_or(|p| p.state != PropState::Busy);
        self.client.wait(SLEW_TIMEOUT, Some(cancel), |l| done(l).then_some(())).is_ok()
    }

    fn log(&self, line: String) {
        self.debug.borrow_mut().push(line);
    }

    fn has(&self, link: &Link, name: &str, kind: Kind) -> bool {
        link.store.property(&self.device, name).is_some_and(|p| p.kind == kind)
    }

    /// `CheckState`'s `m_ready`.
    fn ready(&self, link: &Link) -> bool {
        if self.aux {
            return coord(link, &self.device).is_some();
        }
        let motion = self.has(link, "TELESCOPE_MOTION_RATE", Kind::Number)
            && self.has(link, "TELESCOPE_MOTION_NS", Kind::Switch)
            && self.has(link, "TELESCOPE_MOTION_WE", Kind::Switch);
        motion || (self.has(link, TIMED_GUIDE_NS, Kind::Number) && self.has(link, TIMED_GUIDE_WE, Kind::Number))
    }

    /// Whether the mount dropped, and the alert PHD2 raises for it.
    fn lost(&self, link: &Link) -> Option<Option<&'static str>> {
        let generation = self.attached?;
        if !link.is_up() || link.generation() != generation {
            return Some(Some("INDI server disconnected"));
        }
        // `removeDevice`: a plain Disconnect.
        if link.store.device(&self.device).is_none() {
            return Some(None);
        }
        (self.ready(link) && link.device_connected(&self.device) == Some(false))
            .then_some(Some("INDI mount was disconnected"))
    }

    fn is_connected_in(&self, link: &Link) -> bool {
        self.attached.is_some() && self.lost(link).is_none()
    }

    /// `GetSiteLatLong`: `GEOGRAPHIC_COORD`.
    fn site(&self, link: &Link) -> Option<(f64, f64)> {
        Some((self.number(link, "GEOGRAPHIC_COORD", "LAT")?, self.number(link, "GEOGRAPHIC_COORD", "LONG")?))
    }

    /// A number of a property of the connected mount.
    fn number(&self, link: &Link, property: &str, member: &str) -> Option<f64> {
        let p = link.store.property(&self.device, property).filter(|p| p.kind == Kind::Number)?;
        p.number(member).filter(|_| self.is_connected_in(link))
    }

    /// The fallback guide: `TELESCOPE_MOTION_RATE` at 0.3× sidereal in
    /// arc-minutes per second, the direction's motion switch On for the
    /// pulse's length, then both Off. `None` without those properties.
    fn motion_guide(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> Option<MoveResult> {
        let (prop, on, other) = match dir {
            Dir::East => ("TELESCOPE_MOTION_WE", "MOTION_EAST", "MOTION_WEST"),
            Dir::West => ("TELESCOPE_MOTION_WE", "MOTION_WEST", "MOTION_EAST"),
            Dir::North => ("TELESCOPE_MOTION_NS", "MOTION_NORTH", "MOTION_SOUTH"),
            Dir::South => ("TELESCOPE_MOTION_NS", "MOTION_SOUTH", "MOTION_NORTH"),
        };
        {
            let link = self.client.lock();
            let rate = link.store.property(&self.device, "TELESCOPE_MOTION_RATE").filter(|p| p.kind == Kind::Number);
            let rate_member = rate.and_then(|p| p.members.first()).map(|m| m.name.clone());
            let switches = self.has(&link, "TELESCOPE_MOTION_NS", Kind::Switch)
                && self.has(&link, "TELESCOPE_MOTION_WE", Kind::Switch);
            let (Some(rate_member), true) = (rate_member, switches) else { return None };
            self.client.send_number(&link, &self.device, "TELESCOPE_MOTION_RATE", &[(&rate_member, 0.3 * 15.0 / 60.0)]);
            self.client.send_switch(&link, &self.device, prop, &[(on, true), (other, false)]);
        }
        // PHD2 sleeps the pulse out whatever happens; a stop cuts it short
        // here, and the motion still stops.
        client::sleep_ms(ms as u64, cancel);
        let link = self.client.lock();
        self.client.send_switch(&link, &self.device, prop, &[(on, false), (other, false)]);
        Some(MoveResult::Ok)
    }
}

/// The coordinates property: `EQUATORIAL_EOD_COORD`, else the J2000
/// `EQUATORIAL_COORD`.
fn coord<'a>(link: &'a Link, device: &str) -> Option<&'a Property> {
    let number = |name: &str| link.store.property(device, name).filter(|p| p.kind == Kind::Number);
    number("EQUATORIAL_EOD_COORD").or_else(|| number("EQUATORIAL_COORD"))
}

impl GuideOutput for IndiMount {
    fn guide(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> MoveResult {
        let client = self.client.clone();
        // A dropped mount has no properties left (`ClearStatus`).
        let end = if self.is_connected() {
            let mut debug = Vec::new();
            let end = self.pulse.pulse(&client, &self.device, dir, ms, cancel, "INDI Mount", &mut debug);
            self.debug.get_mut().extend(debug);
            end
        } else {
            PulseEnd::Missing
        };
        match end {
            PulseEnd::Done => MoveResult::Ok,
            PulseEnd::Busy | PulseEnd::Interrupted | PulseEnd::TimedOut => MoveResult::Error,
            PulseEnd::Missing => {
                let moved = if self.is_connected() { self.motion_guide(dir, ms, cancel) } else { None };
                moved.unwrap_or_else(|| {
                    self.log("INDI Mount: pulse guide properties unavailable!".into());
                    MoveResult::Error
                })
            }
        }
    }

    fn can_pulse_guide(&self) -> bool {
        let link = self.client.lock();
        self.is_connected_in(&link)
            && self.has(&link, TIMED_GUIDE_NS, Kind::Number)
            && self.has(&link, TIMED_GUIDE_WE, Kind::Number)
    }
}

impl PointingSource for IndiMount {
    /// `GetCoordinates`: RA hours and Dec degrees of the coordinates
    /// property, and `TIME_LST`, else the sidereal time computed here.
    fn coordinates(&self) -> Option<(f64, f64, f64)> {
        let link = self.client.lock();
        let p = coord(&link, &self.device).filter(|_| self.is_connected_in(&link))?;
        let (ra, dec) = (p.number("RA")?, p.number("DEC")?);
        let lst = self.number(&link, "TIME_LST", "LST").unwrap_or_else(|| {
            let jd = chrono::Utc::now().timestamp_micros() as f64 / 86_400e6 + UNIX_EPOCH_JD;
            libnova_lst(jd, self.site(&link).map(|(_, lon)| lon))
        });
        Some((ra, dec, lst))
    }

    /// `SideOfPier`.
    fn pier_side(&self) -> PierSide {
        let link = self.client.lock();
        let side = link
            .store
            .property(&self.device, "TELESCOPE_PIER_SIDE")
            .filter(|p| p.kind == Kind::Switch && self.is_connected_in(&link))
            .map_or(PierSide::Unknown, |p| {
                if p.switch("PIER_EAST") == Some(true) {
                    PierSide::East
                } else if p.switch("PIER_WEST") == Some(true) {
                    PierSide::West
                } else {
                    PierSide::Unknown
                }
            });
        self.log(format!("INDI Mount: SideOfPier returns {}", side as i32));
        side
    }

    /// `GetGuideRates`: `GUIDE_RATE` is in multiples of the sidereal rate.
    fn guide_rates(&self) -> Option<(f64, f64)> {
        let link = self.client.lock();
        let rate = |m| self.number(&link, "GUIDE_RATE", m).map(|x| x * (15.0 * SIDEREAL_SECOND_PER_SEC) / 3600.0);
        Some((rate("GUIDE_RATE_WE")?, rate("GUIDE_RATE_NS")?))
    }

    /// `Slewing`: the coordinates are Busy.
    fn is_slewing(&self) -> bool {
        let link = self.client.lock();
        self.is_connected_in(&link) && coord(&link, &self.device).is_some_and(|p| p.state == PropState::Busy)
    }

    fn can_report_position(&self) -> bool {
        let link = self.client.lock();
        self.is_connected_in(&link) && coord(&link, &self.device).is_some()
    }

    /// "INDI slew is always async": true even without coordinates.
    fn can_slew_async(&self) -> bool {
        true
    }

    /// `SlewToCoordinatesAsync`: `ON_COORD_SET` to SLEW, then the target
    /// written to the coordinates property.
    fn slew_to_async(&mut self, ra_hours: f64, dec_deg: f64) -> bool {
        let link = self.client.lock();
        let set = self.has(&link, "ON_COORD_SET", Kind::Switch);
        let Some(name) =
            coord(&link, &self.device).filter(|_| set && self.is_connected_in(&link)).map(|p| p.name.clone())
        else {
            return false;
        };
        let modes = [("SLEW", true), ("TRACK", false), ("SYNC", false)];
        self.client.send_switch(&link, &self.device, "ON_COORD_SET", &modes);
        self.client.send_number(&link, &self.device, &name, &[("RA", ra_hours), ("DEC", dec_deg)]);
        true
    }

    /// `AbortSlew`: `TELESCOPE_ABORT_MOTION.ABORT` On.
    fn abort_slew(&mut self) {
        let link = self.client.lock();
        if self.is_connected_in(&link) {
            self.client.send_switch(&link, &self.device, "TELESCOPE_ABORT_MOTION", &[("ABORT", true)]);
        }
    }

    fn site_lat_long(&self) -> Option<(f64, f64)> {
        self.site(&self.client.lock())
    }
}

/// The Julian day of the Unix epoch.
const UNIX_EPOCH_JD: f64 = 2440587.5;

/// `libnova_LST`: the apparent sidereal time at the site's longitude,
/// hours, 0 without a site.
fn libnova_lst(jd: f64, longitude: Option<f64>) -> f64 {
    longitude.map_or(0.0, |lon| norm(apparent_sidereal_time(jd) + lon / 15.0, 0.0, 24.0))
}

/// `ln_get_apparent_sidereal_time`, hours: the mean sidereal time plus
/// `Δψ / 15 · cos(Δε)`. Quirk kept: libnova takes the cosine of the
/// nutation in obliquity where Meeus has the obliquity itself, which adds
/// up to 0.1 s. libnova computes in `long double`; doubles differ by less
/// than 1e-10 h.
fn apparent_sidereal_time(jd: f64) -> f64 {
    let (dpsi, deps) = nutation(jd);
    mean_sidereal_time(jd) + dpsi / 15.0 * deps.to_radians().cos()
}

/// `ln_get_mean_sidereal_time`, hours (Meeus 12.4).
fn mean_sidereal_time(jd: f64) -> f64 {
    let t = (jd - 2451545.0) / 36525.0;
    let s = 280.46061837 + 360.98564736629 * (jd - 2451545.0) + 0.000387933 * t * t - t * t * t / 38710000.0;
    norm(s, 0.0, 360.0) * 24.0 / 360.0
}

/// `ln_get_nutation`: the nutation in longitude and in obliquity, degrees,
/// from Meeus' table 22.A. libnova evaluates it at the ephemeris day; the
/// day is used here, which changes Δψ by less than 0.0002″ (3e-9 h of
/// sidereal time).
fn nutation(jd: f64) -> (f64, f64) {
    let t = (jd - 2451545.0) / 36525.0;
    let (t2, t3) = (t * t, t * t * t);
    let args = [
        297.85036 + 445267.111480 * t - 0.0019142 * t2 + t3 / 189474.0,
        357.52772 + 35999.050340 * t - 0.0001603 * t2 - t3 / 300000.0,
        134.96298 + 477198.867398 * t + 0.0086972 * t2 + t3 / 56250.0,
        93.2719100 + 483202.017538 * t - 0.0036825 * t2 + t3 / 327270.0,
        125.04452 - 1934.136261 * t + 0.0020708 * t2 + t3 / 450000.0,
    ]
    .map(f64::to_radians);
    let (mut dpsi, mut deps) = (0.0, 0.0);
    for (m, (p0, p1, e0, e1)) in NUTATION {
        let a: f64 = m.iter().zip(args).map(|(&k, x)| k as f64 * x).sum();
        dpsi += (p0 + p1 * t) * a.sin();
        deps += (e0 + e1 * t) * a.cos();
    }
    // 0.0001″ to degrees.
    (dpsi / 10000.0 / 3600.0, deps / 10000.0 / 3600.0)
}

/// Meeus' table 22.A: the multiples of D, M, M′, F and Ω, then Δψ and Δε
/// (constant and per century), in 0.0001″.
#[rustfmt::skip]
const NUTATION: [([i8; 5], (f64, f64, f64, f64)); 63] = [
    ([0, 0, 0, 0, 1], (-171996.0, -174.2, 92025.0, 8.9)),
    ([-2, 0, 0, 2, 2], (-13187.0, -1.6, 5736.0, -3.1)),
    ([0, 0, 0, 2, 2], (-2274.0, -0.2, 977.0, -0.5)),
    ([0, 0, 0, 0, 2], (2062.0, 0.2, -895.0, 0.5)),
    ([0, 1, 0, 0, 0], (1426.0, -3.4, 54.0, -0.1)),
    ([0, 0, 1, 0, 0], (712.0, 0.1, -7.0, 0.0)),
    ([-2, 1, 0, 2, 2], (-517.0, 1.2, 224.0, -0.6)),
    ([0, 0, 0, 2, 1], (-386.0, -0.4, 200.0, 0.0)),
    ([0, 0, 1, 2, 2], (-301.0, 0.0, 129.0, -0.1)),
    ([-2, -1, 0, 2, 2], (217.0, -0.5, -95.0, 0.3)),
    ([-2, 0, 1, 0, 0], (-158.0, 0.0, 0.0, 0.0)),
    ([-2, 0, 0, 2, 1], (129.0, 0.1, -70.0, 0.0)),
    ([0, 0, -1, 2, 2], (123.0, 0.0, -53.0, 0.0)),
    ([2, 0, 0, 0, 0], (63.0, 0.0, 0.0, 0.0)),
    ([0, 0, 1, 0, 1], (63.0, 0.1, -33.0, 0.0)),
    ([2, 0, -1, 2, 2], (-59.0, 0.0, 26.0, 0.0)),
    ([0, 0, -1, 0, 1], (-58.0, -0.1, 32.0, 0.0)),
    ([0, 0, 1, 2, 1], (-51.0, 0.0, 27.0, 0.0)),
    ([-2, 0, 2, 0, 0], (48.0, 0.0, 0.0, 0.0)),
    ([0, 0, -2, 2, 1], (46.0, 0.0, -24.0, 0.0)),
    ([2, 0, 0, 2, 2], (-38.0, 0.0, 16.0, 0.0)),
    ([0, 0, 2, 2, 2], (-31.0, 0.0, 13.0, 0.0)),
    ([0, 0, 2, 0, 0], (29.0, 0.0, 0.0, 0.0)),
    ([-2, 0, 1, 2, 2], (29.0, 0.0, -12.0, 0.0)),
    ([0, 0, 0, 2, 0], (26.0, 0.0, 0.0, 0.0)),
    ([-2, 0, 0, 2, 0], (-22.0, 0.0, 0.0, 0.0)),
    ([0, 0, -1, 2, 1], (21.0, 0.0, -10.0, 0.0)),
    ([0, 2, 0, 0, 0], (17.0, -0.1, 0.0, 0.0)),
    ([2, 0, -1, 0, 1], (16.0, 0.0, -8.0, 0.0)),
    ([-2, 2, 0, 2, 2], (-16.0, 0.1, 7.0, 0.0)),
    ([0, 1, 0, 0, 1], (-15.0, 0.0, 9.0, 0.0)),
    ([-2, 0, 1, 0, 1], (-13.0, 0.0, 7.0, 0.0)),
    ([0, -1, 0, 0, 1], (-12.0, 0.0, 6.0, 0.0)),
    ([0, 0, 2, -2, 0], (11.0, 0.0, 0.0, 0.0)),
    ([2, 0, -1, 2, 1], (-10.0, 0.0, 5.0, 0.0)),
    ([2, 0, 1, 2, 2], (-8.0, 0.0, 3.0, 0.0)),
    ([0, 1, 0, 2, 2], (7.0, 0.0, -3.0, 0.0)),
    ([-2, 1, 1, 0, 0], (-7.0, 0.0, 0.0, 0.0)),
    ([0, -1, 0, 2, 2], (-7.0, 0.0, 3.0, 0.0)),
    ([2, 0, 0, 2, 1], (-7.0, 0.0, 3.0, 0.0)),
    ([2, 0, 1, 0, 0], (6.0, 0.0, 0.0, 0.0)),
    ([-2, 0, 2, 2, 2], (6.0, 0.0, -3.0, 0.0)),
    ([-2, 0, 1, 2, 1], (6.0, 0.0, -3.0, 0.0)),
    ([2, 0, -2, 0, 1], (-6.0, 0.0, 3.0, 0.0)),
    ([2, 0, 0, 0, 1], (-6.0, 0.0, 3.0, 0.0)),
    ([0, -1, 1, 0, 0], (5.0, 0.0, 0.0, 0.0)),
    ([-2, -1, 0, 2, 1], (-5.0, 0.0, 3.0, 0.0)),
    ([-2, 0, 0, 0, 1], (-5.0, 0.0, 3.0, 0.0)),
    ([0, 0, 2, 2, 1], (-5.0, 0.0, 3.0, 0.0)),
    ([-2, 0, 2, 0, 1], (4.0, 0.0, 0.0, 0.0)),
    ([-2, 1, 0, 2, 1], (4.0, 0.0, 0.0, 0.0)),
    ([0, 0, 1, -2, 0], (4.0, 0.0, 0.0, 0.0)),
    ([-1, 0, 1, 0, 0], (-4.0, 0.0, 0.0, 0.0)),
    ([-2, 1, 0, 0, 0], (-4.0, 0.0, 0.0, 0.0)),
    ([1, 0, 0, 0, 0], (-4.0, 0.0, 0.0, 0.0)),
    ([0, 0, 1, 2, 0], (3.0, 0.0, 0.0, 0.0)),
    ([0, 0, -2, 2, 2], (-3.0, 0.0, 0.0, 0.0)),
    ([-1, -1, 1, 0, 0], (-3.0, 0.0, 0.0, 0.0)),
    ([0, 1, 1, 0, 0], (-3.0, 0.0, 0.0, 0.0)),
    ([0, -1, 1, 2, 2], (-3.0, 0.0, 0.0, 0.0)),
    ([2, -1, -1, 2, 2], (-3.0, 0.0, 0.0, 0.0)),
    ([0, 0, 3, 2, 2], (-3.0, 0.0, 0.0, 0.0)),
    ([2, -1, 0, 2, 2], (-3.0, 0.0, 0.0, 0.0)),
];

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Instant;

    use super::*;
    use crate::indi::fake::{self, Fake, Profile};

    const DEV: &str = "Telescope Simulator";

    /// How the fake Telescope Simulator behaves.
    #[derive(Clone, Copy)]
    struct Scope {
        /// Answer a pulse Ok at once, as the CCD Simulator's ST-4 does,
        /// rather than Busy, then Ok when the pulse ends.
        instant: bool,
        /// `TELESCOPE_TIMED_GUIDE_*`, else the motion-rate properties.
        timed: bool,
        /// `TIME_LST`.
        lst: bool,
        /// `EQUATORIAL_EOD_COORD`, else `EQUATORIAL_COORD`.
        eod: bool,
    }

    const SIM: Scope = Scope { instant: false, timed: true, lst: true, eod: true };

    fn coord_name(cfg: Scope) -> &'static str {
        if cfg.eod { "EQUATORIAL_EOD_COORD" } else { "EQUATORIAL_COORD" }
    }

    /// What the mount defines once connected: pointing at 5.5 h +20°, west
    /// of the pier, guide rates 0.5×, in Paris, LST 7.25 h.
    fn connected_props(cfg: Scope) -> String {
        let mut s = fake::def_number(DEV, coord_name(cfg), "Ok", &[("RA", 5.5, 0.0, 24.0), ("DEC", 20.0, -90.0, 90.0)]);
        s += &fake::def_switch(DEV, "ON_COORD_SET", "OneOfMany", &[("TRACK", true), ("SLEW", false), ("SYNC", false)]);
        s += &fake::def_switch(DEV, "TELESCOPE_ABORT_MOTION", "AtMostOne", &[("ABORT", false)]);
        if cfg.timed {
            s += &fake::def_number(
                DEV,
                TIMED_GUIDE_NS,
                "Idle",
                &[("TIMED_GUIDE_N", 0.0, 0.0, 6e4), ("TIMED_GUIDE_S", 0.0, 0.0, 6e4)],
            );
            s += &fake::def_number(
                DEV,
                TIMED_GUIDE_WE,
                "Idle",
                &[("TIMED_GUIDE_W", 0.0, 0.0, 6e4), ("TIMED_GUIDE_E", 0.0, 0.0, 6e4)],
            );
        } else {
            s += &fake::def_number(DEV, "TELESCOPE_MOTION_RATE", "Idle", &[("MOTION_RATE", 1.0, 0.0, 10.0)]);
            s += &fake::def_switch(
                DEV,
                "TELESCOPE_MOTION_NS",
                "AtMostOne",
                &[("MOTION_NORTH", false), ("MOTION_SOUTH", false)],
            );
            s += &fake::def_switch(
                DEV,
                "TELESCOPE_MOTION_WE",
                "AtMostOne",
                &[("MOTION_WEST", false), ("MOTION_EAST", false)],
            );
        }
        s += &fake::def_number(
            DEV,
            "GUIDE_RATE",
            "Idle",
            &[("GUIDE_RATE_WE", 0.5, 0.0, 1.0), ("GUIDE_RATE_NS", 0.5, 0.0, 1.0)],
        );
        s += &fake::def_switch(DEV, "TELESCOPE_PIER_SIDE", "OneOfMany", &[("PIER_WEST", true), ("PIER_EAST", false)]);
        s += &fake::def_number(
            DEV,
            "GEOGRAPHIC_COORD",
            "Ok",
            &[("LAT", 48.85, -90.0, 90.0), ("LONG", 2.35, 0.0, 360.0), ("ELEV", 35.0, -200.0, 1e4)],
        );
        if cfg.lst {
            s += &fake::def_number(DEV, "TIME_LST", "Ok", &[("LST", 7.25, 0.0, 24.0)]);
        }
        s
    }

    fn scope(cfg: Scope) -> Fake {
        Fake::start(move |e, peer| {
            let values = fake::numbers(e);
            let members: Vec<(&str, f64)> = values.iter().map(|(n, v)| (n.as_str(), *v)).collect();
            match (e.name.as_str(), e.attr("name").unwrap_or_default()) {
                ("getProperties", _) => peer.send(&fake::driver(DEV, 5, false)),
                ("newSwitchVector", "CONNECTION") => {
                    peer.send(&fake::connection_reply(e).unwrap());
                    peer.send(&connected_props(cfg));
                }
                ("newNumberVector", p @ (TIMED_GUIDE_NS | TIMED_GUIDE_WE)) if cfg.instant => {
                    peer.send(&fake::set_number(DEV, p, "Ok", &members));
                }
                ("newNumberVector", p @ (TIMED_GUIDE_NS | TIMED_GUIDE_WE)) => {
                    let ms = values.iter().map(|(_, v)| *v).fold(0.0, f64::max);
                    peer.send(&fake::set_number(DEV, p, "Busy", &members));
                    let zeros: Vec<(&str, f64)> = members.iter().map(|(n, _)| (*n, 0.0)).collect();
                    peer.send_after(ms as u64, fake::set_number(DEV, p, "Ok", &zeros));
                }
                ("newNumberVector", p @ ("EQUATORIAL_EOD_COORD" | "EQUATORIAL_COORD")) => {
                    peer.send(&fake::set_number(DEV, p, "Busy", &members));
                    peer.send_after(100, fake::set_number(DEV, p, "Ok", &members));
                }
                _ => {}
            }
        })
    }

    fn connected(fake: &Fake, aux: bool) -> IndiMount {
        let opts = MountOptions { device: DEV.into(), pulse_min_wait: true };
        let mut mount = IndiMount::new(IndiClient::start("127.0.0.1", fake.port, &[DEV]), &opts, aux);
        mount.connect(&CancelToken::new()).unwrap();
        mount
    }

    fn sent(fake: &Fake) -> Vec<String> {
        fake.received().iter().filter(|e| e.name != "getProperties").map(fake::summary).collect()
    }

    /// From target/scratch/harness/libnova (libnova 0.16): JD, mean and
    /// apparent sidereal time, hours.
    const LIBNOVA: [(f64, f64, f64); 6] = [
        (2451545.0, 18.697374558, 18.697116721939711),
        (2460000.25, 4.2903688654613994, 4.2901973587638595),
        (2461355.123456, 18.281810246594251, 18.281957249555816),
        (2440587.5, 6.6819758108506599, 6.6820593452316412),
        (2470000.75, 1.421473188015322, 1.4217257428975358),
        (2461355.17, 19.401924645683419, 19.402071713125647),
    ];

    #[test]
    fn the_sidereal_time_is_libnova_s() {
        for (jd, mean, apparent) in LIBNOVA {
            assert!((mean_sidereal_time(jd) - mean).abs() < 1e-10, "{jd}");
            assert!((apparent_sidereal_time(jd) - apparent).abs() < 3e-9, "{jd}");
        }
        // Paris, 2.35° east: 9.4 minutes later than Greenwich.
        assert!((libnova_lst(2461355.123456, Some(2.35)) - 18.438623916222483).abs() < 3e-9);
        assert!((libnova_lst(2451545.0, Some(-100.0)) - 12.030450055273044).abs() < 3e-9);
        assert_eq!(libnova_lst(2451545.0, None), 0.0);
        let p = Profile::default();
        assert_eq!(MountOptions::from_profile(&p), MountOptions { device: "INDI Mount".into(), pulse_min_wait: true });
    }

    #[test]
    fn pulses_wait_for_the_axis_to_answer() {
        let fake = scope(SIM);
        let mut mount = connected(&fake, false);
        assert!(mount.can_pulse_guide());
        let cancel = CancelToken::new();
        let t = Instant::now();
        assert_eq!(mount.guide(Dir::North, 200, &cancel), MoveResult::Ok);
        assert!(t.elapsed() >= Duration::from_millis(200));
        // A stop while the axis is Busy: an error, and the axis stays taken
        // until the driver answers.
        let stop = cancel.clone();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            stop.cancel();
        });
        assert_eq!(mount.guide(Dir::West, 400, &cancel), MoveResult::Error);
        stopper.join().unwrap();
        let fresh = CancelToken::new();
        assert_eq!(mount.guide(Dir::South, 100, &fresh), MoveResult::Error);
        thread::sleep(Duration::from_millis(500));
        assert_eq!(mount.guide(Dir::South, 100, &fresh), MoveResult::Ok);
        assert_eq!(
            sent(&fake)[1..],
            [
                "newNumberVector Telescope Simulator.TELESCOPE_TIMED_GUIDE_NS TIMED_GUIDE_N=200 TIMED_GUIDE_S=0",
                "newNumberVector Telescope Simulator.TELESCOPE_TIMED_GUIDE_WE TIMED_GUIDE_W=400 TIMED_GUIDE_E=0",
                "newNumberVector Telescope Simulator.TELESCOPE_TIMED_GUIDE_NS TIMED_GUIDE_N=0 TIMED_GUIDE_S=100",
            ]
        );
        let debug = mount.take_debug();
        assert_eq!(
            debug[2..],
            [
                "INDI Telescope is ready MotionRate=0 moveNS=0 moveEW=0 guideNS=1 guideEW=1 coord=1",
                "interrupt requested",
                "Cannot guide with guide pulse in progress!",
            ]
        );
        mount.client.shutdown();
    }

    #[test]
    fn an_early_answer_is_waited_out() {
        let fake = scope(Scope { instant: true, ..SIM });
        let mut mount = connected(&fake, false);
        let cancel = CancelToken::new();
        let t = Instant::now();
        assert_eq!(mount.guide(Dir::West, 150, &cancel), MoveResult::Ok);
        assert_eq!(mount.guide(Dir::East, 50, &cancel), MoveResult::Ok);
        assert!(t.elapsed() >= Duration::from_millis(200));
        let debug = mount.take_debug();
        assert_eq!(debug.iter().filter(|l| l.contains("PulseMinWait")).count(), 1);
        assert!(debug.iter().any(|l| l.starts_with("INDI Mount: the driver completed a 150 ms pulse after ")));
        mount.client.shutdown();
    }

    #[test]
    fn without_timed_guiding_the_motion_rate_moves() {
        let fake = scope(Scope { timed: false, ..SIM });
        let mut mount = connected(&fake, false);
        assert!(!mount.can_pulse_guide());
        let t = Instant::now();
        assert_eq!(mount.guide(Dir::East, 100, &CancelToken::new()), MoveResult::Ok);
        assert!(t.elapsed() >= Duration::from_millis(100));
        assert!(fake.wait_for(|r| r.len() == 5));
        assert_eq!(
            sent(&fake)[1..],
            [
                "newNumberVector Telescope Simulator.TELESCOPE_MOTION_RATE MOTION_RATE=0.075",
                "newSwitchVector Telescope Simulator.TELESCOPE_MOTION_WE MOTION_WEST=Off MOTION_EAST=On",
                "newSwitchVector Telescope Simulator.TELESCOPE_MOTION_WE MOTION_WEST=Off MOTION_EAST=Off",
            ]
        );
        mount.client.shutdown();
    }

    #[test]
    fn pointing_and_slews() {
        let fake = scope(SIM);
        let mut mount = connected(&fake, false);
        assert_eq!(mount.name(), "INDI Mount [Telescope Simulator]");
        assert_eq!(mount.coordinates(), Some((5.5, 20.0, 7.25)));
        assert_eq!(mount.pier_side(), PierSide::West);
        let rate = 0.5 * 15.0 * 0.9973 / 3600.0;
        assert_eq!(mount.guide_rates(), Some((rate, rate)));
        assert_eq!(mount.site_lat_long(), Some((48.85, 2.35)));
        assert!(mount.can_report_position() && mount.can_slew_async() && !mount.is_slewing());
        assert!((mount.declination_radians().unwrap() - 20f64.to_radians()).abs() < 1e-15);

        assert!(mount.slew_to_async(6.0, 30.0));
        mount.client.wait(Duration::from_secs(2), None, |l| mount_busy(l).then_some(())).unwrap();
        assert!(mount.is_slewing());
        // A synchronous slew waits while the coordinates are Busy (here the
        // first slew's), not for its own.
        assert!(mount.slew_to(6.5, 31.0, &CancelToken::new()));
        let at = |l: &mut Link| coord(l, DEV).is_some_and(|p| p.number("RA") == Some(6.5) && p.state == PropState::Ok);
        mount.client.wait(Duration::from_secs(2), None, |l| at(l).then_some(())).unwrap();
        assert_eq!(mount.coordinates().map(|c| (c.0, c.1)), Some((6.5, 31.0)));
        mount.abort_slew();
        assert!(fake.wait_for(|r| r.len() == 7));
        assert_eq!(
            sent(&fake)[1..],
            [
                "newSwitchVector Telescope Simulator.ON_COORD_SET SLEW=On",
                "newNumberVector Telescope Simulator.EQUATORIAL_EOD_COORD RA=6 DEC=30",
                "newSwitchVector Telescope Simulator.ON_COORD_SET SLEW=On",
                "newNumberVector Telescope Simulator.EQUATORIAL_EOD_COORD RA=6.5 DEC=31",
                "newSwitchVector Telescope Simulator.TELESCOPE_ABORT_MOTION ABORT=On",
            ]
        );
        assert!(mount.take_debug().contains(&"INDI Mount: SideOfPier returns 1".to_string()));
        mount.client.shutdown();

        // J2000 coordinates and no TIME_LST: libnova's sidereal time at the
        // site's longitude.
        let fake = scope(Scope { lst: false, eod: false, ..SIM });
        let mount = connected(&fake, false);
        let (ra, dec, lst) = mount.coordinates().unwrap();
        let jd = chrono::Utc::now().timestamp_micros() as f64 / 86_400e6 + UNIX_EPOCH_JD;
        assert_eq!((ra, dec), (5.5, 20.0));
        assert!((lst - libnova_lst(jd, Some(2.35))).abs() < 1e-4);
        mount.client.shutdown();
    }

    fn mount_busy(l: &mut Link) -> bool {
        coord(l, DEV).is_some_and(|p| p.state == PropState::Busy)
    }

    #[test]
    fn an_aux_mount_and_the_ways_a_mount_drops() {
        let fake = scope(SIM);
        let mut mount = connected(&fake, true);
        assert!(mount.take_debug().iter().any(|l| l.starts_with("INDI Telescope (AUX) is ready ")));
        // The server goes: an alert, and disconnected until connected again.
        fake.drop_conns();
        mount.client.wait(Duration::from_secs(3), None, |l| (l.generation() == 2).then_some(())).unwrap();
        assert!(!mount.is_connected());
        assert_eq!(mount.coordinates(), None);
        assert_eq!(mount.guide(Dir::North, 10, &CancelToken::new()), MoveResult::Error);
        assert_eq!(mount.check_connection().as_deref(), Some("INDI server disconnected"));
        mount.connect(&CancelToken::new()).unwrap();
        assert!(mount.is_connected());
        // The driver switches off.
        fake.push(&fake::set_switch(DEV, "CONNECTION", "Ok", &[("CONNECT", false), ("DISCONNECT", true)]));
        mount
            .client
            .wait(Duration::from_secs(2), None, |l| (l.device_connected(DEV) == Some(false)).then_some(()))
            .unwrap();
        assert_eq!(mount.check_connection().as_deref(), Some("INDI mount was disconnected"));
        // The device goes: no alert.
        mount.connect(&CancelToken::new()).unwrap();
        fake.push(&format!("<delProperty device='{DEV}'/>\n"));
        mount.client.wait(Duration::from_secs(2), None, |l| l.store.device(DEV).is_none().then_some(())).unwrap();
        assert!(!mount.is_connected());
        assert_eq!(mount.check_connection(), None);
        assert_eq!(mount.take_debug().last().unwrap(), "INDI Mount: serverDisconnected");
        mount.client.shutdown();
    }
}
