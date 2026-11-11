//! Static polar alignment: the mount is turned in RA about its polar axis
//! while a star near the pole is followed, so the star moves on a circle
//! round the centre of rotation (CoR, where the mount's axis points). With
//! the CoR known, a reference star's expected position (computed from its
//! catalogue RA/Dec) tells how far, and which way, to move the mount in
//! altitude and azimuth.
//!
//! Two modes. **Auto** (a mount that can slew): Rotate reads the first
//! position, then slews west in small steps (each within the search region,
//! so the guider keeps the star) until the arc is long enough to measure;
//! the measured arc refines the offset estimate, and the CoR comes from the
//! two positions and the RA turned. **Manual**: the user slews by hand and
//! reads three positions; the CoR is the circle through them.
//!
//! The engine drives it in the guider's Selected state through
//! [`StaticPa::update_state`], which reaches the guider through
//! [`StaticPaGear`] and the mount through the [`PointingSource`] it extends;
//! the overlay comes from [`StaticPa::calc_adjustments`] and the getters.
//! PHD2's window state that only drew things (the star-map panel's pan, the
//! instructions toggle, "Show Orbits", the notes box) stays in the UI.
//!
//! Ported from PHD2 `src/staticpa_tool.h`, `src/staticpa_toolwin.h`, `src/staticpa_toolwin.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;

use chrono::TimeZone;

use crate::engine::algo::MountClass;
use crate::engine::calibration::{ProfileAccess, degrees, keys as cal_keys, norm, norm_angle, pier_side_from, radians};
use crate::engine::log::cfmt::fmt_f;
use crate::engine::mount::PointingSource;
use crate::engine::traits::{PierSide, Point, c_int};

/// The tool's profile keys (`staticpa_toolwin.cpp:236-237`).
pub mod keys {
    /// Index into the hemisphere's [`super::NORTH_STARS`] / [`super::SOUTH_STARS`].
    pub const REF_STAR: &str = "/StaticPaTool/RefStar";
    /// 1 north, -1 south.
    pub const HEMISPHERE: &str = "/StaticPaTool/Hemisphere";
    /// The "Adjustment notes" box, saved as typed.
    pub const NOTES: &str = "/StaticPaTool/Notes";
}

/// A reference star near a pole: catalogue RA and Dec at J2000.0 in degrees,
/// V magnitude.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoleStar {
    pub name: &'static str,
    pub ra2000: f64,
    pub dec2000: f64,
    pub mag: f64,
}

const fn star(name: &'static str, ra2000: f64, dec2000: f64, mag: f64) -> PoleStar {
    PoleStar { name, ra2000, dec2000, mag }
}

/// `c_SthStars` (`staticpa_toolwin.cpp:209`), verbatim.
pub const SOUTH_STARS: [PoleStar; 8] = [
    star("A: sigma Oct", 317.19908, -88.9564, 4.3),
    star("B: HD99828", 165.91797, -89.2392, 7.5),
    star("C: HD125371", 241.45949, -89.3087, 7.8),
    star("D: HD92239", 142.27856, -89.3471, 8.0),
    star("E: HD90105", 130.52896, -89.4606, 7.2),
    star("F: BQ Oct", 218.86418, -89.7718, 6.8),
    star("G: HD99685", 149.13626, -89.7824, 7.8),
    star("H: HD98784", 134.64254, -89.8312, 8.9),
];

/// `c_NthStars` (`staticpa_toolwin.cpp:222`), verbatim.
pub const NORTH_STARS: [PoleStar; 8] = [
    star("A: HD5914", 23.48114, 89.0155, 6.45),
    star("B: HD14369", 55.20640, 89.1048, 8.05),
    star("C: Polaris", 37.96089, 89.2643, 1.95),
    star("D: HD211455", 309.69879, 89.4065, 8.9),
    star("E: TYC-4629-33-1", 75.97399, 89.4207, 9.25),
    star("F: HD21070", 146.59109, 89.5695, 9.0),
    star("G: HD1687", 9.92515, 89.4443, 8.1),
    star("H: TYC-4629-37-1", 70.70722, 89.6301, 9.15),
];

/// `c_autoInstr` (`staticpa_toolwin.cpp:253`), one step per line (PHD2
/// separates them with `<br/>` for its HTML box).
pub const AUTO_INSTRUCTIONS: [&str; 8] = [
    "Slew to near the Celestial Pole.",
    "Choose a Reference Star from the list.",
    "Use the Star Map to help identify a Reference Star.",
    "Select it as the guide star on the main display.",
    "Click Rotate to start the alignment.",
    "Wait for the adjustments to display.",
    "Adjust your mount's altitude and azimuth as displayed.",
    "Red=Altitude; Blue=Azimuth",
];

/// `c_manualInstr` (`staticpa_toolwin.cpp:261`), one step per line.
pub const MANUAL_INSTRUCTIONS: [&str; 11] = [
    "Slew to near the Celestial Pole.",
    "Choose a Reference Star from the list.",
    "Use the Star Map to help identify a Reference Star.",
    "Select it as the guide star on the main display.",
    "Click Get first position.",
    "Slew at least 0h20m west in RA.",
    "Ensure the Reference Star is still selected.",
    "Click Get second position.",
    "Repeat for the third position.",
    "Wait for the adjustments to display.",
    "Adjust your mount's altitude and azimuth to place three reference stars on their orbits",
];

/// Pointing-source coordinates as `GetCoordinates` gives them: RA hours,
/// Dec degrees, local sidereal time hours.
pub type Coords = (f64, f64, f64);

/// What the tool reaches of the guider (`pFrame->pGuider`) while it
/// rotates, on top of the mount (`pPointingSource`): `coordinates`,
/// `is_slewing`, `slew_to_async` and `abort_slew`.
pub trait StaticPaGear: PointingSource {
    /// `Guider::CurrentPosition`, `None` when invalid.
    fn current_position(&self) -> Option<Point>;
    /// `SetLockPosToStarAtPosition(CurrentPosition())`, run right after a
    /// step's slew starts; `true` on success.
    fn lock_on_current_star(&mut self) -> bool;
    /// `Guider::GetSearchRegion`, px.
    fn search_region(&self) -> i32;
    /// The guide frame's size in px. PHD2 takes the displayed bitmap's size
    /// over the display zoom, truncated to int.
    fn frame_size(&self) -> (i32, i32);
}

/// What PHD2 reads when the window opens.
#[derive(Clone, Copy, Debug)]
pub struct StaticPaOpen {
    /// `GetCameraPixelScale`, arc-seconds per pixel.
    pub pixel_scale: f64,
    /// The primary mount's calibrated camera angle (`xAngle`, radians) and
    /// class, when it is connected and calibrated (`pMount`: the AO when one
    /// is connected).
    pub mount: Option<(f64, MountClass)>,
    /// The pointing source's `SideOfPier` (read only with a calibrated mount).
    pub pier_side: PierSide,
    /// The pointing source's site latitude, when it knows one.
    pub site_lat: Option<f64>,
    /// `pPointingSource && pPointingSource->CanSlewAsync()`
    /// ([`PointingSource::can_slew_async`]).
    pub can_slew: bool,
    /// [`days_since_j2000`] now: the pole stars are precessed once, here.
    pub days: f64,
    /// `m_camWidth` (`staticpa_toolwin.cpp:184`): the camera's `FrameSize`
    /// width, or when that is 0 the displayed image's width over the zoom.
    /// The star map is drawn at `320 / cam_width` of the image's scale.
    pub cam_width: f64,
}

/// 2000-01-01 12:00:00 UTC, in seconds since the epoch.
const J2000_NOON_UTC: i64 = 946_728_000;

/// `mktime` of 2000-01-01 12:00:00 with `tm_isdst = 0`
/// (`staticpa_toolwin.cpp:843-851, 879-887`): noon in the local zone's
/// *standard* time, the `j2000_local` of [`days_since_j2000`]. It reads the
/// local time zone. Where daylight time was in force that day (a southern
/// summer) it is an hour after the naive local noon, which would turn the
/// no-coordinates sky of [`StaticPa::radec_to_px`] by about 15°.
pub fn j2000_local() -> i64 {
    j2000_standard_noon(|t| {
        chrono::Local.timestamp_opt(t, 0).single().map_or(0, |d| d.offset().local_minus_utc() as i64)
    })
}

/// [`j2000_local`] for a zone given by its offset east of UTC (seconds) at
/// a UTC time. Its standard offset is the smaller of January's and July's in
/// 2000, since daylight time adds to it. Deviation: a zone whose tz data
/// counts winter as the daylight time (Europe/Dublin's negative DST) gets
/// its winter offset here, where glibc's `mktime` takes the summer one.
fn j2000_standard_noon(offset_at: impl Fn(i64) -> i64) -> i64 {
    let july = J2000_NOON_UTC + 182 * 86_400;
    J2000_NOON_UTC - offset_at(J2000_NOON_UTC).min(offset_at(july))
}

/// `difftime(now, j2000) / 86400`, the time both `J2000Now` and `Radec2Px`
/// use: `now_utc` is `time(NULL)` and `j2000_local` is [`j2000_local`], noon
/// *local standard* time.
/// The result is off by the time zone's offset (PHD2 parity); in `Radec2Px`
/// that offset turns its Greenwich sidereal time into a rough local one.
pub fn days_since_j2000(now_utc: i64, j2000_local: i64) -> f64 {
    (now_utc - j2000_local) as f64 / 86400.0
}

/// `J2000Now` (`staticpa_toolwin.cpp:877`): precess J2000 RA/Dec (degrees)
/// to `days` after J2000 (IAU 2006 angles, Capitaine et al. 2005, to t³).
pub fn j2000_now(ra: f64, dec: f64, days: f64) -> (f64, f64) {
    let tnow = days / 36525.0;
    // `pow`, not `powi`: the libm call rounds t³ once, like PHD2.
    let t2 = tnow.powf(2.0);
    let t3 = tnow.powf(3.0);
    let zeta = 2.5976176 + 2306.0809506 * tnow + 0.3019015 * t2 + 0.0179663 * t3;
    let zetarad = radians(zeta / 3600.0);
    let zed = -2.5976176 + 2306.0803226 * tnow + 1.0947790 * t2 + 0.0182273 * t3;
    let zedrad = radians(zed / 3600.0);
    let theta = 2004.1917476 * tnow - 0.4269353 * t2 - 0.0418251 * t3;
    let thetarad = radians(theta / 3600.0);

    let xx = zedrad.cos() * thetarad.cos() * zetarad.cos() - zedrad.sin() * zetarad.sin();
    let yx = -zedrad.cos() * thetarad.cos() * zetarad.sin() - zedrad.sin() * zetarad.cos();
    let zx = -zedrad.cos() * thetarad.sin();
    let xy = zedrad.sin() * thetarad.cos() * zetarad.cos() + zedrad.cos() * zetarad.sin();
    let yy = -zedrad.sin() * thetarad.cos() * zetarad.sin() + zedrad.cos() * zetarad.cos();
    let zy = -zedrad.sin() * thetarad.sin();
    let xz = thetarad.sin() * zetarad.cos();
    let yz = -thetarad.sin() * zetarad.sin();
    let zz = thetarad.cos();

    let x0 = radians(dec).cos() * radians(ra).cos();
    let y0 = radians(dec).cos() * radians(ra).sin();
    let z0 = radians(dec).sin();
    let x = xx * x0 + yx * y0 + zx * z0;
    let y = xy * x0 + yy * y0 + zy * z0;
    let z = xz * x0 + yz * y0 + zz * z0;
    (norm(degrees(y.atan2(x)), 0.0, 360.0), degrees(z.atan2((1.0 - z * z).sqrt())))
}

/// The circle through three points (manual mode, `CalcRotationCentre`,
/// `staticpa_toolwin.cpp:701-716`): centre and radius from the 3×3
/// determinants of `[x² + y², x, y, 1]`.
pub fn centre_from_three(p1: Point, p2: Point, p3: Point) -> (Point, f64) {
    let (x1, y1, x2, y2, x3, y3) = (p1.x, p1.y, p2.x, p2.y, p3.x, p3.y);
    let a = x1 * x1 + y1 * y1;
    let b = x1;
    let c = y1;
    let e = x2 * x2 + y2 * y2;
    let f = x2;
    let g = y2;
    let i = x3 * x3 + y3 * y3;
    let j = x3;
    let k = y3;
    let m11 = b * g + c * j + f * k - g * j - c * f - b * k;
    let m12 = a * g + c * i + e * k - g * i - c * e - a * k;
    let m13 = a * f + b * i + e * j - f * i - b * e - a * j;
    let m14 = a * f * k + b * g * i + c * e * j - c * f * i - b * e * k - a * g * j;
    let cx = (1. / 2.) * m12 / m11;
    let cy = (-1. / 2.) * m13 / m11;
    let cr = (cx * cx + cy * cy + m14 / m11).sqrt();
    (Point::new(cx, cy), cr)
}

/// The CoR from two points and the RA turned between them (auto mode,
/// `CalcRotationCentre`): `r = chord / 2 / sin(Δ/2)`, the centre on the
/// chord's perpendicular bisector, on the side the star turns about (clockwise
/// round the north pole, anticlockwise round the south one). Also returns the
/// rotation `radiff` and the base's slope (radians) for the debug log.
pub fn centre_from_two(p1: Point, p2: Point, ra1_hours: f64, ra2_hours: f64, hemi: i32) -> (Point, f64, f64, f64) {
    let (x1, y1, x2, y2) = (p1.x, p1.y, p2.x, p2.y);
    // RA decreases going west; the image turns the other way, in a
    // right-handed sense for the hemisphere.
    let radiff = norm_angle(radians((ra1_hours - ra2_hours) * 15.0 * hemi as f64));
    let theta2 = radiff / 2.0;
    let lenchord = (x1 - x2).hypot(y1 - y2);
    let cr = (lenchord / 2.0 / theta2.sin()).abs();
    let lenbase = (cr * theta2.cos()).abs();
    // Pixels run y-down, hence the inverted y.
    let slopebase = (y1 - y2).atan2(x2 - x1) - hemi as f64 * PI / 2.0;
    let cx = (x1 + x2) / 2.0 + lenbase * slopebase.cos();
    let cy = (y1 + y2) / 2.0 - lenbase * slopebase.sin();
    (Point::new(cx, cy), cr, radiff, slopebase)
}

/// One pole star as the tool shows it: position precessed to the date the
/// window opened, and where it sits relative to the pole in the image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapStar {
    pub name: &'static str,
    pub ra: f64,
    pub dec: f64,
    pub mag: f64,
    /// `Radec2Px`: px from the CoR.
    pub px: Point,
    /// The star-map dot's radius, px (`CreateStarTemplate`:
    /// `356·exp(−0.3·mag) / scale`).
    pub radius_px: f64,
}

/// `CalcAdjustments`: the reference star's measured and expected positions
/// and the move between them split along the altitude and azimuth axes, as
/// image vectors (PHD2 draws red altitude then blue azimuth from `measured`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Adjustment {
    pub measured: Point,
    pub target: Point,
    pub alt_corr: Point,
    pub az_corr: Point,
    /// Signed lengths, px.
    pub alt_px: f64,
    pub az_px: f64,
    pub total_px: f64,
    /// `|px| · scale / 60`, as in the status line.
    pub alt_arcmin: f64,
    pub az_arcmin: f64,
    pub total_arcmin: f64,
}

/// The tool's state (`StaticPaToolWin`).
#[derive(Clone, Debug)]
pub struct StaticPa {
    can_slew: bool,
    pixel_scale: f64,
    cam_width: f64,
    /// Camera angle to RA, degrees.
    cam_angle: f64,
    /// Pixels the star must move for an arc to be measurable.
    dev_px: f64,
    ref_star: usize,
    auto: bool,
    hemi: i32,
    /// Manual hour angle, degrees.
    ha: f64,
    flip: bool,
    aligning: bool,
    /// Bit 0: CoR computed; bits 1..=3: position 1..=3 read.
    state: u32,
    num_pos: i32,
    req_rot: f64,
    req_step: i32,
    tot_rot: f64,
    n_step: i32,
    ra_pos: [f64; 3],
    px_pos: [Point; 3],
    px_centre: Point,
    radius: f64,
    disp_size: (i32, i32),
    north_now: [(f64, f64); 8],
    south_now: [(f64, f64); 8],
    status: String,
    log: Vec<String>,
}

impl StaticPa {
    /// Open the tool (`StaticPaToolWin()`). PHD2 also starts looping when
    /// the camera is idle (the engine's job) and centres `m_pxCentre` on the
    /// frame, which nothing reads before the CoR is computed.
    pub fn new(p: &impl ProfileAccess, open: &StaticPaOpen) -> Self {
        let mut log = Vec::new();
        let mut cam_angle = 0.0;
        let mut flip = false;
        if let Some((x_angle, class)) = open.mount {
            log.push(format!("StaticPA: Camera angle {}", fmt_f(degrees(x_angle), 1)));
            let key = format!("{}{}", cal_keys::prefix(class.class_name()), cal_keys::PIER_SIDE);
            let cal_pier_side = pier_side_from(p.get_i32(&key, PierSide::Unknown as i32));
            log.push(format!(
                "StaticPA: calPierSide {}; currPierSide {}",
                cal_pier_side.label(),
                open.pier_side.label()
            ));
            if open.pier_side != cal_pier_side && open.pier_side != PierSide::Unknown {
                flip = true;
                log.push("StaticPA: Flipped Camera angle".to_string());
            }
            cam_angle = degrees(x_angle);
        }
        let now = |s: &PoleStar| j2000_now(s.ra2000, s.dec2000, open.days);
        let mut hemi = p.get_i32(keys::HEMISPHERE, 1);
        if let Some(lat) = open.site_lat {
            hemi = if lat >= 0.0 { 1 } else { -1 };
        }
        // PHD2 would throw from `vector::at` on a stored index past the list.
        let ref_star = usize::try_from(p.get_i32(keys::REF_STAR, 0)).ok().filter(|&i| i < 8).unwrap_or(0);
        Self {
            can_slew: open.can_slew,
            pixel_scale: open.pixel_scale,
            cam_width: open.cam_width,
            cam_angle,
            dev_px: 5.0,
            ref_star,
            auto: open.can_slew,
            hemi,
            ha: if open.can_slew { 0.0 } else { 270.0 },
            flip,
            aligning: false,
            state: 0,
            num_pos: 0,
            req_rot: 0.0,
            req_step: 0,
            tot_rot: 0.0,
            n_step: 0,
            ra_pos: [0.0; 3],
            px_pos: [Point::default(); 3],
            px_centre: Point::default(),
            radius: 0.0,
            disp_size: (0, 0),
            north_now: NORTH_STARS.map(|s| now(&s)),
            south_now: SOUTH_STARS.map(|s| now(&s)),
            status: String::new(),
            log,
        }
    }

    // ------------------------------------------------------------ settings

    /// `OnManual` (the box is hidden when the mount cannot slew).
    pub fn set_manual(&mut self, manual: bool) {
        if self.can_slew {
            self.auto = !manual;
        }
    }

    /// `OnHemi`: a change resets the reference star to the first of the
    /// list, without saving it. PHD2 enables the choice in manual mode only,
    /// so in auto mode nothing happens.
    pub fn set_hemisphere(&mut self, p: &mut impl ProfileAccess, north: bool) {
        if self.auto {
            return;
        }
        let hemi = if north { 1 } else { -1 };
        p.set_i32(keys::HEMISPHERE, hemi);
        if hemi != self.hemi {
            self.ref_star = 0;
            self.hemi = hemi;
        }
    }

    /// `OnHa`: the hour angle spinner, 0..24 h.
    pub fn set_hour_angle(&mut self, hours: f64) {
        self.ha = hours * 15.0;
    }

    /// `OnFlip`: invert the camera angle (180°) in `Radec2Px`.
    pub fn set_flip(&mut self, flip: bool) {
        self.flip = flip;
    }

    /// `OnRefStar`; an index past the list is ignored.
    pub fn set_ref_star(&mut self, p: &mut impl ProfileAccess, index: usize) {
        if index < 8 {
            p.set_i32(keys::REF_STAR, index as i32);
            self.ref_star = index;
        }
    }

    // ------------------------------------------------------------- buttons

    /// Rotate, or "Get first position" in manual mode (`OnRotate`). In auto
    /// mode a press while aligning stops: the slew is aborted and everything
    /// measured is dropped. The refusals and the stop leave their text in
    /// [`status`](Self::status).
    pub fn rotate(&mut self, calibrating_or_guiding: bool, locked: bool, gear: &mut impl StaticPaGear) {
        if self.aligning && self.auto {
            gear.abort_slew();
            self.aligning = false;
            self.num_pos = 0;
            self.state = 0;
            self.status = "Static alignment stopped".to_string();
            self.log.push("Static alignment stopped".to_string());
            return;
        }
        if calibrating_or_guiding {
            self.status = "Please wait till Calibration is done and/or stop guiding".to_string();
            return;
        }
        if !locked {
            self.status = "Please select a star".to_string();
            return;
        }
        self.num_pos = 1;
        if self.auto {
            self.state = 0;
        }
        self.aligning = true;
    }

    /// "Get second position" (`OnStar2`); read at the next frame. PHD2 shows
    /// the button in manual mode only: in auto mode nothing happens (it
    /// would run the auto Pos#2 branch with no rotation sized and loop).
    pub fn second_position(&mut self) {
        if !self.auto {
            self.num_pos = 2;
            self.aligning = true;
        }
    }

    /// "Get third position" (`OnStar3`), manual mode only like the second.
    pub fn third_position(&mut self) {
        if !self.auto {
            self.num_pos = 3;
            self.aligning = true;
        }
    }

    /// `OnClear`: only once a CoR is shown.
    pub fn clear(&mut self) {
        if self.is_calced() {
            self.aligning = false;
            self.num_pos = 0;
            self.state = 0;
            self.status = "Static Polar alignment display cleared".to_string();
            self.log.push("Static PA display cleared".to_string());
        }
    }

    /// `OnClose`: stops reading positions; a slew under way is left to run.
    pub fn close(&mut self) {
        self.aligning = false;
        self.log.push("Close StaticPaTool".to_string());
    }

    // -------------------------------------------------------------- frames

    /// One frame in the guider's Selected state (`StaticPaTool::UpdateState`).
    /// `false` when the rotation failed: the guider then drops the star
    /// (`STATE_UNINITIALIZED`, "Static PA rotation failed").
    pub fn update_state(&mut self, gear: &mut impl StaticPaGear) -> bool {
        if self.aligning { self.rotate_mount(gear) } else { true }
    }

    /// The star was lost in the Selected state (`NotifyStarLost`).
    pub fn notify_star_lost(&mut self, gear: &mut impl StaticPaGear) {
        if self.aligning {
            self.rotate_fail("Static PA rotation failed - star lost".to_string(), gear);
        }
    }

    /// `RotateMount`: read a position, or take one more westward step.
    fn rotate_mount(&mut self, gear: &mut impl StaticPaGear) -> bool {
        self.status = format!("Reading Star Position #{}", self.num_pos);
        self.log.push(format!("StaticPA: Reading Star Pos#{}", self.num_pos));
        if self.num_pos == 1 {
            // Assume a 5° offset of the star from the CoR to begin with.
            if !self.set_params(5.0, gear) {
                self.log.push("StaticPA: Error from SetParams".to_string());
                return self.rotate_fail("Error setting rotation parameters: Stopping".to_string(), gear);
            }
            self.log.push(format!(
                "StaticPA: Pos#1 m_reqRot={} m_reqStep={}",
                fmt_f(self.req_rot, 1),
                self.req_step
            ));
            if !self.set_star(self.num_pos, gear) {
                return self.set_star_failed(gear);
            }
            self.num_pos += 1;
            if !self.auto {
                self.aligning = false;
                if self.is_aligned() {
                    self.calc_rotation_centre(gear);
                }
            }
            self.tot_rot = 0.0;
            self.n_step = 0;
            return true;
        }
        if self.num_pos == 2 {
            let theta = self.req_rot - self.tot_rot;
            if !self.auto {
                return self.read_manual_position(gear);
            }
            self.status = format!(
                "Star Pos#2 Step={} / {} Rotated={} / {} deg",
                self.n_step,
                self.req_step,
                fmt_f(self.tot_rot, 1),
                fmt_f(self.req_rot, 1)
            );
            self.log.push(format!(
                "StaticPA: Star Pos#2 m_nStep={} / {} m_totRot={} / {} deg",
                self.n_step,
                self.req_step,
                fmt_f(self.tot_rot, 1),
                fmt_f(self.req_rot, 1)
            ));
            if gear.is_slewing() {
                return true;
            }
            if self.tot_rot < self.req_rot {
                // `theta / (m_reqStep - m_nStep)`: an int difference, which
                // is 0 (→ inf) if rounding leaves the total a hair short.
                let newtheta = theta / self.req_step.wrapping_sub(self.n_step) as f64;
                if !self.move_west_by(newtheta, gear) {
                    self.log.push(format!("StaticPA: Error from MoveWestBy at step {}", self.n_step));
                    return self.rotate_fail(format!("Error moving west step {}: Stopping", self.n_step), gear);
                }
                self.tot_rot += newtheta;
            } else {
                if !self.set_star(self.num_pos, gear) {
                    return self.set_star_failed(gear);
                }
                // The arc actually covered for the rotation asked gives the
                // star's real distance from the CoR (PHD2 warns this can loop
                // forever).
                let actpix = (self.px_pos[1].x - self.px_pos[0].x).hypot(self.px_pos[1].y - self.px_pos[0].y);
                let actsec = actpix * self.pixel_scale;
                let actoffsetdeg = 90.0 - degrees((actsec / 3600.0 / self.req_rot).acos());
                self.log.push(format!(
                    "StaticPA: Star Pos#2 actpix={} actsec={} m_pxScale={}",
                    fmt_f(actpix, 1),
                    fmt_f(actsec, 1),
                    fmt_f(self.pixel_scale, 1)
                ));
                if actoffsetdeg == 0.0 {
                    self.log.push(format!(
                        "StaticPA: Star Pos#2 Mount did not move actoffsetdeg={}",
                        fmt_f(actoffsetdeg, 1)
                    ));
                    return self.rotate_fail(
                        format!("Star Pos#2 Mount did not move. Calculated polar offset={} deg", fmt_f(actoffsetdeg, 1)),
                        gear,
                    );
                }
                let prev_rotdg = self.req_rot;
                if !self.set_params(actoffsetdeg, gear) {
                    self.log.push("StaticPA: Error from SetParams".to_string());
                    return self.rotate_fail("Error setting rotation parameters: Stopping".to_string(), gear);
                }
                if self.req_rot <= prev_rotdg {
                    // Moved far enough: show the adjustments.
                    self.num_pos += 1;
                    self.n_step = 0;
                    self.tot_rot = 0.0;
                    self.aligning = false;
                    self.calc_rotation_centre(gear);
                } else if self.req_rot > 45.0 {
                    self.log.push(format!(
                        "StaticPA: Pos#2 Too close to CoR actoffsetdeg={} m_reqRot={}",
                        fmt_f(actoffsetdeg, 1),
                        fmt_f(self.req_rot, 1)
                    ));
                    return self.rotate_fail(
                        format!("Star is too close to CoR ({} deg) - try another reference star", fmt_f(actoffsetdeg, 1)),
                        gear,
                    );
                } else {
                    self.n_step = c_int(self.req_step as f64 * self.tot_rot / self.req_rot);
                    self.log.push(format!(
                        "StaticPA: Star Pos#2 m_nStep={} / {} m_totRot={} / {}",
                        self.n_step,
                        self.req_step,
                        fmt_f(self.tot_rot, 1),
                        fmt_f(self.req_rot, 1)
                    ));
                }
            }
            return true;
        }
        if self.num_pos == 3 {
            if !self.auto {
                return self.read_manual_position(gear);
            }
            self.num_pos += 1;
            return true;
        }
        true
    }

    /// Manual mode, positions 2 and 3: read it, and compute the CoR once all
    /// three are in.
    fn read_manual_position(&mut self, gear: &mut impl StaticPaGear) -> bool {
        if !self.set_star(self.num_pos, gear) {
            return self.set_star_failed(gear);
        }
        self.num_pos += 1;
        self.aligning = false;
        if self.is_aligned() {
            self.calc_rotation_centre(gear);
        }
        true
    }

    fn set_star_failed(&mut self, gear: &mut impl StaticPaGear) -> bool {
        self.log.push(format!("StaticPA: Error from SetStar {}", self.num_pos));
        self.rotate_fail(format!("Error reading star position #{}: Stopping", self.num_pos), gear)
    }

    /// `RotateFail`: always `false`.
    fn rotate_fail(&mut self, msg: String, gear: &mut impl StaticPaGear) -> bool {
        self.status = msg;
        self.aligning = false;
        if self.auto {
            gear.abort_slew();
            self.num_pos = 0;
            self.state = 0;
        }
        false
    }

    /// `SetStar`: record position `npos` (1..=3) and, in auto mode, the RA.
    fn set_star(&mut self, npos: i32, gear: &impl StaticPaGear) -> bool {
        let idx = (npos - 1) as usize;
        self.unset_state(npos);
        if self.auto {
            match gear.coordinates() {
                Some((ra, _, _)) => self.ra_pos[idx] = ra,
                None => {
                    self.log.push("StaticPA: SetStar failed to get scope coordinates".to_string());
                    return false;
                }
            }
        }
        self.px_pos[idx] = Point::new(-1.0, -1.0);
        let Some(star) = gear.current_position() else {
            return false;
        };
        self.px_pos[idx] = star;
        self.set_state(npos);
        self.log.push(format!(
            "StaticPA: Setstar #{} {}, {}",
            npos,
            fmt_f(star.x, 0),
            fmt_f(star.y, 0)
        ));
        self.status = format!("Read Position #{}: {}, {}", npos, fmt_f(star.x, 0), fmt_f(star.y, 0));
        true
    }

    /// `SetParams`: the rotation that moves a star `newoffset` degrees from
    /// the CoR by `dev_px` along its arc, and the number of slews keeping
    /// each within the search region.
    fn set_params(&mut self, newoffset: f64, gear: &impl StaticPaGear) -> bool {
        let offsetdeg = newoffset;
        let offsetpx = offsetdeg * 3600.0 / self.pixel_scale;
        self.log.push(format!(
            "StaticPA:SetParams(newoffset={}) m_pxScale={} m_offsetpx={} m_devpx={}",
            fmt_f(newoffset, 1),
            fmt_f(self.pixel_scale, 1),
            fmt_f(offsetpx, 1),
            fmt_f(self.dev_px, 1)
        ));
        if offsetpx < self.dev_px {
            self.log.push(format!(
                "StaticPA: SetParams() Too close to CoR: m_offsetpx={} m_devpx={}",
                fmt_f(offsetpx, 1),
                fmt_f(self.dev_px, 1)
            ));
            return false;
        }
        self.req_rot = degrees((1.0 - self.dev_px / offsetpx).acos());
        let rotpx = self.req_rot * 3600.0 / self.pixel_scale * radians(offsetdeg).sin();
        let region = gear.search_region();
        self.req_step = 1;
        if rotpx > region as f64 {
            self.req_step = c_int((rotpx / region as f64).ceil());
        }
        self.log.push(format!(
            "StaticPA: SetParams() m_reqRot={} m_rotpx={} m_reqStep={} region={}",
            fmt_f(self.req_rot, 1),
            fmt_f(rotpx, 1),
            self.req_step,
            region
        ));
        true
    }

    /// `MoveWestBy`: slew `thetadeg` west and re-lock on the star.
    fn move_west_by(&mut self, thetadeg: f64, gear: &mut impl StaticPaGear) -> bool {
        let Some((cur_ra, cur_dec, _)) = gear.coordinates() else {
            self.log.push("StaticPA: MoveWestBy failed to get scope coordinates".to_string());
            return false;
        };
        let slew_ra = norm(cur_ra - thetadeg * 24.0 / 360.0, 0.0, 24.0);
        self.log.push(format!(
            "StaticPA: Slewing from RA hrs: {} to:{}",
            fmt_f(cur_ra, 3),
            fmt_f(slew_ra, 3)
        ));
        if !gear.slew_to_async(slew_ra, cur_dec) {
            self.log.push("StaticPA: MoveWestBy: async slew failed".to_string());
            return false;
        }
        self.n_step += 1;
        if !gear.lock_on_current_star() {
            self.log.push("StaticPA: MoveWestBy: Failed to lock star position".to_string());
            return false;
        }
        true
    }

    /// `CalcRotationCentre`. PHD2 also splits the CoR's offset from the
    /// frame centre into cone and Dec parts for an overlay it never draws
    /// (`drawCone = false`); that is left out.
    fn calc_rotation_centre(&mut self, gear: &impl StaticPaGear) {
        let p1 = self.px_pos[0];
        let p2 = self.px_pos[1];
        self.unset_state(0);
        let (centre, cr) = if !self.auto {
            let p3 = self.px_pos[2];
            self.log.push(format!(
                "StaticPA: Manual CalcCoR: P1({},{}); P2({},{}); P3({},{})",
                fmt_f(p1.x, 1),
                fmt_f(p1.y, 1),
                fmt_f(p2.x, 1),
                fmt_f(p2.y, 1),
                fmt_f(p3.x, 1),
                fmt_f(p3.y, 1)
            ));
            centre_from_three(p1, p2, p3)
        } else {
            self.log.push(format!(
                "StaticPA Auto CalcCoR: P1({},{}); P2({},{}); RA: {} {}",
                fmt_f(p1.x, 1),
                fmt_f(p1.y, 1),
                fmt_f(p2.x, 1),
                fmt_f(p2.y, 1),
                fmt_f(self.ra_pos[0] * 15., 1),
                fmt_f(self.ra_pos[1] * 15., 1)
            ));
            let (centre, cr, radiff, slopebase) =
                centre_from_two(p1, p2, self.ra_pos[0], self.ra_pos[1], self.hemi);
            self.log.push(format!(
                "StaticPA CalcCoR: radiff(deg): {}; cr: {}; slopebase(deg) {}",
                fmt_f(degrees(radiff), 1),
                fmt_f(cr, 1),
                fmt_f(degrees(slopebase), 1)
            ));
            (centre, cr)
        };
        self.px_centre = centre;
        self.radius = cr;
        self.disp_size = gear.frame_size();
        // PHD2 prints the display zoom here; a headless engine has none.
        self.log.push(format!(
            "StaticPA CalcCoR: W:H:scale:angle {}: {}: {} {}",
            self.disp_size.0,
            self.disp_size.1,
            fmt_f(1.0, 1),
            fmt_f(self.cam_angle, 1)
        ));
        self.set_state(0);
    }

    // ---------------------------------------------------------- the overlay

    /// `Radec2Px`: where a star at `ra`/`dec` (degrees, of date) sits
    /// relative to the CoR, in px. The image's rotation follows the mount's
    /// RA plus the manual hour angle, or without coordinates the sidereal
    /// time from `days` (see [`days_since_j2000`]) minus that hour angle.
    pub fn radec_to_px(&self, ra: f64, dec: f64, coords: Option<Coords>, days: f64) -> Point {
        let r = (90.0 - dec.abs()) * 3600.0 / self.pixel_scale;
        let ra_deg = match coords {
            Some((ra_hrs, _, _)) => norm(ra_hrs * 15.0 + self.ha, 0.0, 360.0),
            None => norm(280.46061837 + 360.98564736629 * days - self.ha, 0.0, 360.0),
        };
        let a1 = norm(ra - (ra_deg - 90.0), 0.0, 360.0);
        let cam_angle = norm(if self.flip { self.cam_angle + 180.0 } else { self.cam_angle }, 0.0, 360.0);
        let a = cam_angle - a1 * self.hemi as f64;
        Point::new(r * radians(a).cos(), -r * radians(a).sin())
    }

    /// `CalcAdjustments`, run by PHD2 on every repaint once the CoR is known:
    /// the reference star's target (CoR + `Radec2Px`) against its last
    /// measured position, split along the altitude axis (at HA + 90°, turned
    /// by the camera angle) and the azimuth axis. `None` before the CoR.
    pub fn calc_adjustments(&mut self, coords: Option<Coords>, days: f64) -> Option<Adjustment> {
        if !self.is_calced() {
            return None;
        }
        let (ra, dec) = self.pole_now()[self.ref_star];
        let starpx = self.radec_to_px(ra, dec, coords, days);
        let xt = starpx.x + self.px_centre.x;
        let yt = starpx.y + self.px_centre.y;
        let measured = self.px_pos[if self.auto { 1 } else { 2 }];
        let (xs, ys) = (measured.x, measured.y);

        let hcor_r = (xt - xs).hypot(yt - ys);
        let hcor_a = degrees((yt - ys).atan2(xt - xs));
        let ha_deg = match coords {
            Some((ra_hrs, _, st_hrs)) => norm((st_hrs - ra_hrs) * 15.0 + self.ha, 0.0, 360.0),
            None => self.ha,
        };
        let rarot = -self.cam_angle;
        let harot = norm(rarot - (90.0 + ha_deg), 0.0, 360.0);
        let hrot = norm(hcor_a - harot, 0.0, 360.0);
        let az_r = hcor_r * radians(hrot).sin();
        let alt_r = hcor_r * radians(hrot).cos();
        let az_corr = Point::new(-az_r * radians(harot).sin(), az_r * radians(harot).cos());
        let alt_corr = Point::new(alt_r * radians(harot).cos(), alt_r * radians(harot).sin());
        self.log.push(format!(
            "StaticPA CalcAdjust: Angles: rarot {}; ha_deg {}; m_ha {}; hcor_a {}; harot: {}",
            fmt_f(rarot, 1),
            fmt_f(ha_deg, 1),
            fmt_f(self.ha, 1),
            fmt_f(hcor_a, 1),
            fmt_f(harot, 1)
        ));
        self.log.push(format!(
            "StaticPA CalcAdjust: Errors(px): alt {}; az {}; tot {}",
            fmt_f(alt_r, 1),
            fmt_f(az_r, 1),
            fmt_f(hcor_r, 1)
        ));
        let arcmin = |px: f64| px.abs() * self.pixel_scale / 60.0;
        let adj = Adjustment {
            measured,
            target: Point::new(xt, yt),
            alt_corr,
            az_corr,
            alt_px: alt_r,
            az_px: az_r,
            total_px: hcor_r,
            alt_arcmin: arcmin(alt_r),
            az_arcmin: arcmin(az_r),
            total_arcmin: arcmin(hcor_r),
        };
        self.status = format!(
            "Polar Alignment Error (arcmin): Alt {}; Az {} Tot {}",
            fmt_f(adj.alt_arcmin, 1),
            fmt_f(adj.az_arcmin, 1),
            fmt_f(adj.total_arcmin, 1)
        );
        Some(adj)
    }

    /// The hemisphere's pole stars as the star map and the overlay show them
    /// (`CreateStarTemplate`, `PaintHelper`).
    pub fn star_map(&self, coords: Option<Coords>, days: f64) -> Vec<MapStar> {
        let cat = if self.hemi >= 0 { &NORTH_STARS } else { &SOUTH_STARS };
        cat.iter()
            .zip(self.pole_now())
            .map(|(s, &(ra, dec))| MapStar {
                name: s.name,
                ra,
                dec,
                mag: s.mag,
                px: self.radec_to_px(ra, dec, coords, days),
                radius_px: 356.0 * (-0.3 * s.mag).exp() / self.pixel_scale,
            })
            .collect()
    }

    /// `m_poleStars`: follows the hemisphere (`FillPanel`).
    fn pole_now(&self) -> &[(f64, f64); 8] {
        if self.hemi >= 0 { &self.north_now } else { &self.south_now }
    }

    // ------------------------------------------------------------- getters

    /// The positions read so far, by slot (PHD2 circles each one).
    pub fn positions(&self) -> [Option<Point>; 3] {
        std::array::from_fn(|i| self.has_state(i as i32 + 1).then_some(self.px_pos[i]))
    }

    /// The CoR and the reference star's orbit radius, once computed.
    pub fn centre(&self) -> Option<(Point, f64)> {
        self.is_calced().then_some((self.px_centre, self.radius))
    }

    /// The frame size read with the CoR (PHD2 marks its centre).
    pub fn frame_size(&self) -> (i32, i32) {
        self.disp_size
    }

    pub fn is_aligning(&self) -> bool {
        self.aligning
    }

    /// `IsCalced`: the CoR is known.
    pub fn is_calced(&self) -> bool {
        self.has_state(0)
    }

    pub fn is_auto(&self) -> bool {
        self.auto
    }

    pub fn can_slew(&self) -> bool {
        self.can_slew
    }

    /// 1 north, -1 south.
    pub fn hemisphere(&self) -> i32 {
        self.hemi
    }

    pub fn ref_star(&self) -> usize {
        self.ref_star
    }

    /// The manual hour angle, hours.
    pub fn hour_angle(&self) -> f64 {
        self.ha / 15.0
    }

    pub fn flip(&self) -> bool {
        self.flip
    }

    /// Degrees, as the "Camera Angle" box shows it.
    pub fn cam_angle(&self) -> f64 {
        self.cam_angle
    }

    pub fn pixel_scale(&self) -> f64 {
        self.pixel_scale
    }

    /// `m_camWidth`, px: the star map's scale is `320 / cam_width`.
    pub fn cam_width(&self) -> f64 {
        self.cam_width
    }

    /// The position being read next (1..=3). Once the CoR is computed it is
    /// 4 in manual mode but 3 in auto mode: PHD2's `numPos == 3` auto branch
    /// is never reached, `aligning` being false by then. 0 after a stop, a
    /// failure or a clear.
    pub fn num_pos(&self) -> i32 {
        self.num_pos
    }

    /// Auto mode's progress: steps taken / needed, degrees turned / needed.
    pub fn rotation(&self) -> (i32, i32, f64, f64) {
        (self.n_step, self.req_step, self.tot_rot, self.req_rot)
    }

    /// The window's status-bar text.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The debug-log lines written since the last call.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    // --------------------------------------------------------------- state

    /// `IsAligned`: auto needs positions 1 and 2, manual all three.
    fn is_aligned(&self) -> bool {
        if self.auto { (self.state >> 1) & 3 == 3 } else { (self.state >> 1) & 7 == 7 }
    }

    fn has_state(&self, ipos: i32) -> bool {
        self.state & (1 << ipos) > 0
    }

    fn set_state(&mut self, ipos: i32) {
        self.state |= 1 << ipos;
    }

    fn unset_state(&mut self, ipos: i32) {
        self.state = self.state & !(1 << ipos) & 15;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tools::test_util::{Mem, fnv1a};
    use std::cell::Cell;

    // Every golden below comes from PHD2's C++ (staticpa_toolwin.cpp's
    // methods copied into a harness driving the same fakes).

    const DAYS: f64 = 9446.25;

    /// A mount at RA `ra` and a star turning round (cx, cy) at radius `r`:
    /// `a0` degrees at RA `ra0`, clockwise on screen going west in the north.
    struct Fake {
        ra: f64,
        dec: f64,
        st: f64,
        slew_left: Cell<i32>,
        slews: i32,
        aborts: i32,
        /// Slews are accepted but the mount stays put.
        frozen: bool,
        /// `coordinates` calls that succeed before it fails.
        coords_ok: Cell<i32>,
        slew_fails: bool,
        /// No valid star position; the lock move refused.
        invalid: bool,
        lock_fails: bool,
        cx: f64,
        cy: f64,
        r: f64,
        a0: f64,
        ra0: f64,
        hemi: i32,
    }

    impl Fake {
        fn new(cx: f64, cy: f64, r: f64, a0: f64, hemi: i32) -> Self {
            let dec = if hemi > 0 { 89.0 } else { -89.0 };
            Self {
                ra: 6.0,
                dec,
                st: 8.5,
                slew_left: Cell::new(0),
                slews: 0,
                aborts: 0,
                frozen: false,
                coords_ok: Cell::new(i32::MAX),
                slew_fails: false,
                invalid: false,
                lock_fails: false,
                cx,
                cy,
                r,
                a0,
                ra0: 6.0,
                hemi,
            }
        }
    }

    impl PointingSource for Fake {
        fn coordinates(&self) -> Option<Coords> {
            let n = self.coords_ok.get();
            self.coords_ok.set(n - 1);
            (n > 0).then_some((self.ra, self.dec, self.st))
        }
        fn pier_side(&self) -> PierSide {
            PierSide::Unknown
        }
        fn guide_rates(&self) -> Option<(f64, f64)> {
            None
        }
        /// Busy for one frame after each slew.
        fn is_slewing(&self) -> bool {
            let n = self.slew_left.get();
            self.slew_left.set((n - 1).max(0));
            n > 0
        }
        fn can_report_position(&self) -> bool {
            true
        }
        fn can_slew_async(&self) -> bool {
            true
        }
        fn slew_to_async(&mut self, ra_hours: f64, dec_deg: f64) -> bool {
            if self.slew_fails {
                return false;
            }
            if !self.frozen {
                self.ra = ra_hours;
                self.dec = dec_deg;
            }
            self.slew_left.set(1);
            self.slews += 1;
            true
        }
        fn abort_slew(&mut self) {
            self.aborts += 1;
        }
    }

    impl StaticPaGear for Fake {
        fn current_position(&self) -> Option<Point> {
            if self.invalid {
                return None;
            }
            let a = radians(self.a0 + (self.ra0 - self.ra) * 15.0 * self.hemi as f64);
            Some(Point::new(self.cx + self.r * a.cos(), self.cy + self.r * a.sin()))
        }
        fn lock_on_current_star(&mut self) -> bool {
            !self.lock_fails
        }
        fn search_region(&self) -> i32 {
            15
        }
        fn frame_size(&self) -> (i32, i32) {
            (752, 580)
        }
    }

    /// The harness's tool: reference star C, the camera angle set as is.
    fn tool(scale: f64, cam_angle: f64, hemi: i32, can_slew: bool) -> StaticPa {
        let mut p = Mem::default();
        p.set_i32(keys::REF_STAR, 2);
        p.set_i32(keys::HEMISPHERE, hemi);
        let open = StaticPaOpen {
            pixel_scale: scale,
            mount: None,
            pier_side: PierSide::Unknown,
            site_lat: None,
            can_slew,
            days: DAYS,
            cam_width: 752.0,
        };
        let mut t = StaticPa::new(&p, &open);
        t.cam_angle = cam_angle;
        t
    }

    /// Target, alt / az / total px, alt and az vectors.
    fn assert_adjustment(a: &Adjustment, golden: [f64; 9]) {
        let got = [
            a.target.x, a.target.y, a.alt_px, a.az_px, a.total_px, a.alt_corr.x, a.alt_corr.y, a.az_corr.x, a.az_corr.y,
        ];
        assert_eq!(got, golden);
    }

    /// Rotate, then feed frames until the tool stops aligning.
    fn run_auto(t: &mut StaticPa, g: &mut Fake) -> Vec<bool> {
        t.rotate(false, true, g);
        let mut results = Vec::new();
        while t.is_aligning() && results.len() < 500 {
            results.push(t.update_state(g));
        }
        results
    }

    #[test]
    fn pole_stars_precess_to_date() {
        let north = [
            (27.686957243759572, 89.145430463333255),
            (63.824017411713534, 89.178028807530779),
            (46.419225465716728, 89.37124038881889),
            (297.59699467834713, 89.48674712521246),
            (90.694765616082833, 89.437542492608202),
            (155.07499659336705, 89.443462463315043),
            (13.743822553158781, 89.585277767480505),
            (93.893146982670814, 89.649778268334728),
        ];
        let south = [
            (322.37877530300671, -88.846352188769231),
            (163.05848978814345, -89.377983271139868),
            (253.28593701719862, -89.36436972480756),
            (133.35905425096368, -89.454137527606704),
            (117.09376507339198, -89.541298511461292),
            (257.12022233154875, -89.852488006131566),
            (111.38689488152309, -89.880833485635137),
            (78.367963685221895, -89.877659018023877),
        ];
        for (s, g) in NORTH_STARS.iter().zip(north) {
            assert_eq!(j2000_now(s.ra2000, s.dec2000, DAYS), g, "{}", s.name);
        }
        for (s, g) in SOUTH_STARS.iter().zip(south) {
            assert_eq!(j2000_now(s.ra2000, s.dec2000, DAYS), g, "{}", s.name);
        }
        // The star map lists the hemisphere's stars at those positions.
        let map = tool(2.0, 15.0, 1, true).star_map(None, DAYS);
        assert_eq!(map[2].name, "C: Polaris");
        assert_eq!((map[2].ra, map[2].dec), north[2]);
        assert!((map[2].radius_px - 356.0 * (-0.3f64 * 1.95).exp() / 2.0).abs() < 1e-12);
        assert_eq!(tool(2.0, 15.0, -1, true).star_map(None, DAYS)[0].name, "A: sigma Oct");
        assert_eq!(days_since_j2000(946_728_000 + 86_400 * 3, 946_728_000), 3.0);
    }

    #[test]
    fn j2000_noon_in_standard_time() {
        // Sydney: +11 h in January (daylight time), +10 h in July. mktime's
        // tm_isdst = 0 takes the +10 h, 02:00 UTC, not the naive 01:00.
        let sydney = |t: i64| if t < J2000_NOON_UTC + 86_400 { 11 * 3600 } else { 10 * 3600 };
        assert_eq!(j2000_standard_noon(sydney), 946_692_000);
        // Paris: +1 h of standard time in January; UTC.
        assert_eq!(j2000_standard_noon(|t| if t < J2000_NOON_UTC + 86_400 { 3600 } else { 7200 }), 946_724_400);
        assert_eq!(j2000_standard_noon(|_| 0), J2000_NOON_UTC);
        // The machine's own zone: whole quarter hours off noon UTC.
        let off = J2000_NOON_UTC - j2000_local();
        assert!(off % 900 == 0 && off.abs() <= 14 * 3600);
    }

    #[test]
    fn radec_to_px_for_polaris() {
        let mut t = tool(2.0, 15.0, 1, true);
        let g = Fake::new(0.0, 0.0, 0.0, 0.0, 1);
        let (ra, dec) = t.pole_now()[2];
        // Mount at RA 6 h (LST 8.5 h): the image turns with the mount's RA.
        let p = t.radec_to_px(ra, dec, g.coordinates(), DAYS);
        assert_eq!((p.x, p.y), (965.82296737148795, 589.98577722875984));
        // No coordinates: the sidereal time from the day count.
        let p = t.radec_to_px(ra, dec, None, DAYS);
        assert_eq!((p.x, p.y), (-1065.4401411135045, 381.75191857870959));
        // Camera flipped, manual hour angle 2 h.
        t.set_flip(true);
        t.set_hour_angle(2.0);
        let p = t.radec_to_px(ra, dec, g.coordinates(), DAYS);
        assert_eq!((p.x, p.y), (-1131.4201139165575, -28.031187265869143));
        // South, no slewing (HA 270°), sigma Octantis.
        let t = tool(3.5, -40.0, -1, false);
        let (ra, dec) = t.pole_now()[0];
        let p = t.radec_to_px(ra, dec, None, DAYS);
        assert_eq!((p.x, p.y), (925.37560500921109, 742.77946203807574));
        // The radius is the polar distance, whatever the angle.
        assert!((p.x.hypot(p.y) - (90.0 - dec.abs()) * 3600.0 / 3.5).abs() < 1e-9);
    }

    #[test]
    fn centre_from_three_points_on_a_circle() {
        // 10°, 100°, 200° round (100, 50), radius 30.
        let (c, r) = centre_from_three(
            Point::new(129.54423259036625, 55.20944533000791),
            Point::new(94.79055466999209, 79.544232590366249),
            Point::new(71.809221376422755, 39.739395700229942),
        );
        assert_eq!((c.x, c.y, r), (100.00000000000011, 49.999999999999922, 30.000000000000362));
    }

    #[test]
    fn centre_from_two_points_and_the_ra_turned() {
        // Round (400, 300), radius 250, 10° of RA west: 30° → 40° on screen
        // in the north, 30° → 20° in the south.
        let p0 = Point::new(616.50635094610971, 425.0);
        let p1 = Point::new(591.51111077974451, 460.69690242163483);
        let (c, r, radiff, _) = centre_from_two(p0, p1, 6.0, 6.0 - 10.0 / 15.0, 1);
        assert_eq!((c.x, c.y, r), (400.0, 299.99999999999989, 250.00000000000009));
        assert!((degrees(radiff) - 10.0).abs() < 1e-9);
        let p1 = Point::new(634.92315519647707, 385.50503583141716);
        let (c, r, radiff, _) = centre_from_two(p0, p1, 6.0, 6.0 - 10.0 / 15.0, -1);
        assert_eq!((c.x, c.y, r), (400.0, 300.00000000000045, 249.99999999999989));
        assert!((degrees(radiff) + 10.0).abs() < 1e-9);
    }

    #[test]
    fn set_params_sizes_the_rotation() {
        let mut t = tool(2.0, 15.0, 1, true);
        let g = Fake::new(0.0, 0.0, 0.0, 0.0, 1);
        // 5° at 2"/px is 9000 px from the CoR: 1.91° of rotation moves the
        // star 299.6 px along its arc, 20 slews of the 15 px search region.
        assert!(t.set_params(5.0, &g));
        assert_eq!((t.req_rot, t.req_step), (1.9099477475697859, 20));
        // Under 5 px from the CoR: refused, the previous values stay.
        assert!(!t.set_params(0.002, &g));
        assert_eq!((t.req_rot, t.req_step), (1.9099477475697859, 20));
        assert_eq!(
            t.take_log(),
            [
                "StaticPA:SetParams(newoffset=5.0) m_pxScale=2.0 m_offsetpx=9000.0 m_devpx=5.0",
                "StaticPA: SetParams() m_reqRot=1.9 m_rotpx=299.6 m_reqStep=20 region=15",
                "StaticPA:SetParams(newoffset=0.0) m_pxScale=2.0 m_offsetpx=3.6 m_devpx=5.0",
                "StaticPA: SetParams() Too close to CoR: m_offsetpx=3.6 m_devpx=5.0",
            ]
        );
    }

    #[test]
    fn auto_north_finds_the_centre_of_rotation() {
        // A star 1.5° from the CoR at 2"/px: 2700 px round a CoR far off the
        // frame, the star at (300, 250) to begin with.
        let mut g = Fake::new(-623.45438697930581, -2287.1700761219527, 2700.0, 70.0, 1);
        let mut t = tool(2.0, 15.0, 1, true);
        let results = run_auto(&mut t, &mut g);
        assert_eq!((results.len(), g.slews, g.aborts), (62, 28, 0));
        assert!(results.iter().all(|&ok| ok));
        assert_eq!(t.num_pos(), 3);
        assert!(t.is_calced());
        assert_eq!(t.positions().map(|p| p.is_some()), [true, true, false]);
        assert_eq!(t.rotation(), (0, 11, 0.0, 3.4875184674860642));
        assert_eq!(g.ra, 5.7674987688342565);
        let (c, r) = t.centre().unwrap();
        assert_eq!((c.x, c.y, r), (-623.45438697930626, -2287.1700761219536, 2700.0000000000014));
        assert_eq!(t.frame_size(), (752, 580));
        assert_eq!(t.status(), "Read Position #2: 144, 301");

        let a = t.calc_adjustments(g.coordinates(), DAYS).unwrap();
        assert_adjustment(
            &a,
            [
                304.69046337000395,
                -1639.5248349562776,
                952.50498095702756,
                1698.8396191850129,
                1947.6451911117249,
                -789.5463674825553,
                -532.80603632377256,
                950.28584830947989,
                -1408.194893548266,
            ],
        );
        assert_eq!(a.measured, t.positions()[1].unwrap());
        assert_eq!(t.status(), "Polar Alignment Error (arcmin): Alt 31.8; Az 56.6 Tot 64.9");
        let a = t.calc_adjustments(None, DAYS).unwrap();
        assert_adjustment(
            &a,
            [
                -1688.8945280928108,
                -1905.418157543244,
                2606.0714792222821,
                -1199.2065512575,
                2868.7462256872286,
                -674.50093172122445,
                -2517.2717469361573,
                -1158.3445789146647,
                310.37749447715311,
            ],
        );
        assert_eq!(t.status(), "Polar Alignment Error (arcmin): Alt 86.9; Az 40.0 Tot 95.6");

        // The whole debug log, step by step: 186 lines hashed, a few spelled.
        let log = t.take_log();
        assert_eq!(log.len(), 186);
        assert_eq!(fnv1a(&log), 0x506483799ca27fcf);
        assert_eq!(
            &log[106..112],
            [
                "StaticPA: Star Pos#2 m_nStep=20 / 20 m_totRot=1.9 / 1.9 deg",
                "StaticPA: Setstar #2 215, 279",
                "StaticPA: Star Pos#2 actpix=90.0 actsec=180.0 m_pxScale=2.0",
                "StaticPA:SetParams(newoffset=1.5) m_pxScale=2.0 m_offsetpx=2700.2 m_devpx=5.0",
                "StaticPA: SetParams() m_reqRot=3.5 m_rotpx=164.3 m_reqStep=11 region=15",
                "StaticPA: Star Pos#2 m_nStep=6 / 11 m_totRot=1.9 / 3.5",
            ]
        );
        assert_eq!(
            &log[log.len() - 7..],
            [
                "StaticPA Auto CalcCoR: P1(300.0,250.0); P2(144.0,301.5); RA: 90.0 86.5",
                "StaticPA CalcCoR: radiff(deg): 3.5; cr: 2700.0; slopebase(deg) -251.7",
                "StaticPA CalcCoR: W:H:scale:angle 752: 580: 1.0 15.0",
                "StaticPA CalcAdjust: Angles: rarot -15.0; ha_deg 41.0; m_ha 0.0; hcor_a -85.3; harot: 214.0",
                "StaticPA CalcAdjust: Errors(px): alt 952.5; az 1698.8; tot 1947.6",
                "StaticPA CalcAdjust: Angles: rarot -15.0; ha_deg 0.0; m_ha 0.0; hcor_a -129.7; harot: 255.0",
                "StaticPA CalcAdjust: Errors(px): alt 2606.1; az -1199.2; tot 2868.7",
            ]
        );
    }

    #[test]
    fn auto_south_turns_the_other_way() {
        let mut g = Fake::new(699.99999999999977, 1166.0254037844388, 1000.0, -120.0, -1);
        let mut t = tool(5.0, -40.0, -1, true);
        let results = run_auto(&mut t, &mut g);
        assert_eq!((results.len(), g.slews, g.aborts), (49, 21, 0));
        assert_eq!(t.rotation(), (0, 7, 0.0, 5.7328839339182363));
        assert_eq!(g.ra, 5.6178077377387812);
        let (c, r) = t.centre().unwrap();
        assert_eq!((c.x, c.y, r), (699.9999999999992, 1166.025403784439, 999.99999999999989));
        let a = t.calc_adjustments(g.coordinates(), DAYS).unwrap();
        assert_adjustment(
            &a,
            [
                344.43082620682537,
                1454.1530865896268,
                -1111.0084334722162,
                166.04747452562489,
                1123.348344478562,
                62.654802641368285,
                1109.2403323682199,
                165.7832202551987,
                -9.3641699127217546,
            ],
        );
        let a = t.calc_adjustments(None, DAYS).unwrap();
        assert_adjustment(
            &a,
            [
                1135.6328895445713,
                1306.278690725223,
                -73.863649365427662,
                1393.024562005603,
                1394.9814583170921,
                -47.478638618327906,
                56.582838144874493,
                1067.1187248526405,
                895.41892844621987,
            ],
        );
        let log = t.take_log();
        assert_eq!(log.len(), 158);
        assert_eq!(fnv1a(&log), 0xc85401f3fa192a2a);
        assert_eq!(log[log.len() - 6], "StaticPA CalcCoR: radiff(deg): -5.7; cr: 1000.0; slopebase(deg) -57.1");
    }

    #[test]
    fn a_mount_that_does_not_move_fails_and_resets() {
        let mut g = Fake::new(0.0, 0.0, 2700.0, 70.0, 1);
        g.frozen = true;
        let mut t = tool(2.0, 15.0, 1, true);
        let results = run_auto(&mut t, &mut g);
        assert_eq!((results.len(), g.slews, g.aborts), (42, 20, 1));
        assert_eq!(results.iter().position(|&ok| !ok), Some(41));
        assert_eq!(t.status(), "Star Pos#2 Mount did not move. Calculated polar offset=0.0 deg");
        assert_eq!(t.num_pos(), 0);
        assert_eq!(t.positions(), [None; 3]);
        assert!(!t.is_calced());
        assert_eq!(t.rotation(), (20, 20, 1.9099477475697859, 1.9099477475697859));
        assert_eq!(t.calc_adjustments(None, DAYS), None);
        let log = t.take_log();
        assert_eq!(log.len(), 110);
        assert_eq!(fnv1a(&log), 0xebf7499c9f458cb0);
        assert_eq!(log[109], "StaticPA: Star Pos#2 Mount did not move actoffsetdeg=0.0");
    }

    #[test]
    fn auto_failures_stop_and_reset() {
        // From the harness: each failure's last log lines, status and
        // counters, on a star 2700 px round (0, 0).
        let check = |g: &mut Fake, frames: usize, status: &str, tail: &[&str]| {
            let mut t = tool(2.0, 15.0, 1, true);
            let results = run_auto(&mut t, g);
            assert_eq!(results.len(), frames, "{status}");
            assert!(!results[frames - 1] && results[..frames - 1].iter().all(|&ok| ok));
            assert_eq!(t.status(), status);
            assert_eq!((t.num_pos(), t.positions(), t.is_aligning()), (0, [None; 3], false));
            let log = t.take_log();
            assert_eq!(&log[log.len() - tail.len()..], tail);
            assert_eq!(g.aborts, 1);
            t
        };
        let pos1 = [
            "StaticPA: Reading Star Pos#1",
            "StaticPA:SetParams(newoffset=5.0) m_pxScale=2.0 m_offsetpx=9000.0 m_devpx=5.0",
            "StaticPA: SetParams() m_reqRot=1.9 m_rotpx=299.6 m_reqStep=20 region=15",
            "StaticPA: Pos#1 m_reqRot=1.9 m_reqStep=20",
        ];
        let fake = || Fake::new(0.0, 0.0, 2700.0, 70.0, 1);

        // No star at position #1.
        let mut g = fake();
        g.invalid = true;
        let tail = [&pos1[..], &["StaticPA: Error from SetStar 1"]].concat();
        let t = check(&mut g, 1, "Error reading star position #1: Stopping", &tail);
        assert_eq!(t.rotation(), (0, 20, 0.0, 1.9099477475697859));

        // No mount coordinates at position #1, then at the first step.
        let mut g = fake();
        g.coords_ok.set(0);
        let tail = ["StaticPA: SetStar failed to get scope coordinates", "StaticPA: Error from SetStar 1"];
        check(&mut g, 1, "Error reading star position #1: Stopping", &tail);
        let step0 = "StaticPA: Star Pos#2 m_nStep=0 / 20 m_totRot=0.0 / 1.9 deg";
        let mut g = fake();
        g.coords_ok.set(1);
        let tail =
            [step0, "StaticPA: MoveWestBy failed to get scope coordinates", "StaticPA: Error from MoveWestBy at step 0"];
        check(&mut g, 2, "Error moving west step 0: Stopping", &tail);
        assert_eq!(g.slews, 0);

        // The slew refused; the slew started but the lock move refused: the
        // step was already counted.
        let slewing = "StaticPA: Slewing from RA hrs: 6.000 to:5.994";
        let mut g = fake();
        g.slew_fails = true;
        let tail =
            [step0, slewing, "StaticPA: MoveWestBy: async slew failed", "StaticPA: Error from MoveWestBy at step 0"];
        check(&mut g, 2, "Error moving west step 0: Stopping", &tail);
        assert_eq!(g.slews, 0);
        let mut g = fake();
        g.lock_fails = true;
        let tail =
            [slewing, "StaticPA: MoveWestBy: Failed to lock star position", "StaticPA: Error from MoveWestBy at step 1"];
        let t = check(&mut g, 2, "Error moving west step 1: Stopping", &tail);
        assert_eq!((g.slews, t.rotation().0), (1, 1));

        // A star 9 px from the CoR: the rotation it needs is over 45°.
        let mut g = Fake::new(300.0 - 9.0 * radians(70.0).cos(), 250.0 - 9.0 * radians(70.0).sin(), 9.0, 70.0, 1);
        let tail = [
            "StaticPA: Star Pos#2 actpix=0.3 actsec=0.6 m_pxScale=2.0",
            "StaticPA:SetParams(newoffset=0.0) m_pxScale=2.0 m_offsetpx=9.0 m_devpx=5.0",
            "StaticPA: SetParams() m_reqRot=63.6 m_rotpx=10.0 m_reqStep=1 region=15",
            "StaticPA: Pos#2 Too close to CoR actoffsetdeg=0.0 m_reqRot=63.6",
        ];
        let t = check(&mut g, 42, "Star is too close to CoR (0.0 deg) - try another reference star", &tail);
        assert_eq!(g.slews, 20);
        assert_eq!(t.rotation(), (20, 1, 1.9099477475697859, 63.613845249416272));
    }

    #[test]
    fn an_arc_too_long_for_any_star_loops_on_nan() {
        // At 100"/px a star 3000 px round its CoR moves farther than acos
        // allows: the offset is NaN, SetParams takes it, m_nStep becomes
        // INT_MIN and every later frame reads position #2 again (PHD2 never
        // leaves this). The harness's first 12 frames.
        let (cx, cy) = (300.0 - 3000.0 * radians(70.0).cos(), 250.0 - 3000.0 * radians(70.0).sin());
        let mut g = Fake::new(cx, cy, 3000.0, 70.0, 1);
        let mut t = tool(100.0, 15.0, 1, true);
        t.rotate(false, true, &mut g);
        assert!((0..12).all(|_| t.update_state(&mut g)));
        assert!(t.is_aligning());
        assert_eq!((t.num_pos(), g.slews, g.aborts), (2, 3, 0));
        let (n_step, req_step, tot_rot, req_rot) = t.rotation();
        assert_eq!((n_step, req_step, tot_rot), (i32::MIN, 1, 13.536202737381158));
        assert!(req_rot.is_nan());
        let log = t.take_log();
        assert_eq!(log.len(), 55);
        assert_eq!(fnv1a(&log), 0x6ab00ead28f29591);
        assert_eq!(
            &log[50..],
            [
                "StaticPA: Setstar #2 -388, 412",
                "StaticPA: Star Pos#2 actpix=707.1 actsec=70710.7 m_pxScale=100.0",
                "StaticPA:SetParams(newoffset=nan) m_pxScale=100.0 m_offsetpx=nan m_devpx=5.0",
                "StaticPA: SetParams() m_reqRot=nan m_rotpx=nan m_reqStep=1 region=15",
                "StaticPA: Star Pos#2 m_nStep=-2147483648 / 1 m_totRot=13.5 / nan",
            ]
        );
    }

    #[test]
    fn manual_mode_reads_three_positions() {
        let mut g = Fake::new(350.0, 260.0, 180.0, 40.0, 1);
        let mut t = tool(2.0, 15.0, 1, false);
        assert!(!t.is_auto());
        assert_eq!(t.hour_angle(), 18.0);
        t.rotate(false, true, &mut g);
        assert!(t.update_state(&mut g));
        assert!(!t.is_aligning()); // waits for the next button
        assert_eq!(t.num_pos(), 2);
        g.ra -= 1.0; // an hour west by hand
        t.second_position();
        assert!(t.update_state(&mut g));
        assert!(!t.is_calced());
        g.ra -= 1.0;
        t.third_position();
        assert!(t.update_state(&mut g));
        assert_eq!(t.num_pos(), 4);
        assert!(t.is_calced() && !t.is_aligning());
        assert_eq!(t.positions().map(|p| p.is_some()), [true, true, true]);
        let (c, r) = t.centre().unwrap();
        assert_eq!((c.x, c.y, r), (349.99999999989569, 259.99999999992178, 179.9999999997514));
        assert_eq!(g.slews, 0);
        let a = t.calc_adjustments(g.coordinates(), DAYS).unwrap();
        assert_adjustment(
            &a,
            [
                -643.85415463746085,
                801.43433668771956,
                -506.86433933520357,
                -997.79498264759127,
                1119.1542726033883,
                -66.159072185556511,
                502.52804464751887,
                -989.25870825052471,
                -130.23837970126309,
            ],
        );
        assert_eq!(a.measured, t.positions()[2].unwrap());
        let a = t.calc_adjustments(None, DAYS).unwrap();
        assert_adjustment(
            &a,
            [
                731.75191857860545,
                1325.4401411134263,
                77.299803759026432,
                948.62577004258344,
                951.76998862648975,
                74.665876817920434,
                -20.006661395523459,
                245.52241596206483,
                916.30213076748612,
            ],
        );
        assert_eq!(t.status(), "Polar Alignment Error (arcmin): Alt 2.6; Az 31.6 Tot 31.7");
        assert_eq!(
            t.take_log(),
            [
                "StaticPA: Reading Star Pos#1",
                "StaticPA:SetParams(newoffset=5.0) m_pxScale=2.0 m_offsetpx=9000.0 m_devpx=5.0",
                "StaticPA: SetParams() m_reqRot=1.9 m_rotpx=299.6 m_reqStep=20 region=15",
                "StaticPA: Pos#1 m_reqRot=1.9 m_reqStep=20",
                "StaticPA: Setstar #1 488, 376",
                "StaticPA: Reading Star Pos#2",
                "StaticPA: Setstar #2 453, 407",
                "StaticPA: Reading Star Pos#3",
                "StaticPA: Setstar #3 412, 429",
                "StaticPA: Manual CalcCoR: P1(487.9,375.7); P2(453.2,407.4); P3(411.6,429.1)",
                "StaticPA CalcCoR: W:H:scale:angle 752: 580: 1.0 15.0",
                "StaticPA CalcAdjust: Angles: rarot -15.0; ha_deg 337.5; m_ha 270.0; hcor_a 160.6; harot: 277.5",
                "StaticPA CalcAdjust: Errors(px): alt -506.9; az -997.8; tot 1119.2",
                "StaticPA CalcAdjust: Angles: rarot -15.0; ha_deg 270.0; m_ha 270.0; hcor_a 70.3; harot: 345.0",
                "StaticPA CalcAdjust: Errors(px): alt 77.3; az 948.6; tot 951.8",
            ]
        );
        // Re-reading the first position recomputes the centre at once.
        t.rotate(false, true, &mut g);
        assert!(t.update_state(&mut g));
        assert!(t.is_calced());
        assert_eq!(t.positions()[0], g.current_position());
        // Clear drops everything.
        t.clear();
        assert!(!t.is_calced());
        assert_eq!(t.positions(), [None; 3]);
        assert_eq!(t.status(), "Static Polar alignment display cleared");
    }

    #[test]
    fn position_buttons_are_manual_only() {
        // Hidden in auto mode: a press reads nothing and starts nothing.
        let mut g = Fake::new(0.0, 0.0, 2700.0, 70.0, 1);
        let mut t = tool(2.0, 15.0, 1, true);
        t.second_position();
        t.third_position();
        assert_eq!((t.num_pos(), t.is_aligning()), (0, false));
        assert!(t.update_state(&mut g));
        assert!(t.take_log().is_empty());
    }

    #[test]
    fn rotate_refusals_stop_and_star_lost() {
        let mut g = Fake::new(0.0, 0.0, 100.0, 0.0, 1);
        let mut t = tool(2.0, 15.0, 1, true);
        t.rotate(true, true, &mut g);
        assert_eq!(t.status(), "Please wait till Calibration is done and/or stop guiding");
        t.rotate(false, false, &mut g);
        assert_eq!(t.status(), "Please select a star");
        assert!(!t.is_aligning());
        // A second press while aligning stops: slew aborted, all dropped.
        t.rotate(false, true, &mut g);
        assert!(t.update_state(&mut g) && t.update_state(&mut g));
        assert_eq!(g.slews, 1);
        t.rotate(false, true, &mut g);
        assert_eq!((g.aborts, t.num_pos(), t.is_aligning()), (1, 0, false));
        assert_eq!(t.positions(), [None; 3]);
        assert_eq!(t.status(), "Static alignment stopped");
        // A lost star while aligning fails the same way, and only then.
        t.rotate(false, true, &mut g);
        assert!(t.update_state(&mut g));
        t.notify_star_lost(&mut g);
        assert_eq!((g.aborts, t.num_pos(), t.is_aligning()), (2, 0, false));
        assert_eq!(t.status(), "Static PA rotation failed - star lost");
        t.notify_star_lost(&mut g);
        assert_eq!(g.aborts, 2);
        // Clear does nothing before a centre; close stops reading, no abort.
        t.clear();
        assert_eq!(t.status(), "Static PA rotation failed - star lost");
        t.rotate(false, true, &mut g);
        t.close();
        assert!(!t.is_aligning());
        assert_eq!(g.aborts, 2);
        assert_eq!(t.take_log().last().map(String::as_str), Some("Close StaticPaTool"));
    }

    #[test]
    fn open_reads_the_calibration_pier_side_and_the_site() {
        let mut p = Mem::default();
        p.set_i32("/scope/calibration/pierSide", PierSide::East as i32);
        p.set_i32(keys::HEMISPHERE, 1);
        p.set_i32(keys::REF_STAR, 9); // past the list: star A
        let mut open = StaticPaOpen {
            pixel_scale: 1.2,
            mount: Some((radians(30.0), MountClass::Scope)),
            pier_side: PierSide::West,
            site_lat: Some(-30.0),
            can_slew: true,
            days: DAYS,
            cam_width: 1280.0,
        };
        let mut t = StaticPa::new(&p, &open);
        assert!(t.flip());
        assert_eq!(t.cam_width(), 1280.0);
        assert_eq!((t.hemisphere(), t.ref_star()), (-1, 0));
        assert!((t.cam_angle() - 30.0).abs() < 1e-12);
        assert_eq!(
            t.take_log(),
            [
                "StaticPA: Camera angle 30.0",
                "StaticPA: calPierSide East; currPierSide West",
                "StaticPA: Flipped Camera angle",
            ]
        );
        // An unknown current side: no flip. No mount: angle 0, no log.
        open.pier_side = PierSide::Unknown;
        assert!(!StaticPa::new(&p, &open).flip());
        open.mount = None;
        open.site_lat = None;
        let mut t = StaticPa::new(&p, &open);
        assert_eq!((t.cam_angle(), t.hemisphere()), (0.0, 1));
        assert!(t.take_log().is_empty());
        // No slewing: manual mode, and it stays so.
        let mut t = StaticPa::new(&p, &StaticPaOpen { can_slew: false, ..open });
        t.set_manual(false);
        assert!(!t.is_auto());
        // A hemisphere change picks the first star again, unsaved; the
        // choice is greyed out in auto mode.
        let mut t = StaticPa::new(&p, &open);
        t.set_hemisphere(&mut p, false);
        assert_eq!((t.hemisphere(), p.get_i32(keys::HEMISPHERE, 0)), (1, 1));
        t.set_manual(true);
        t.set_ref_star(&mut p, 2);
        t.set_hemisphere(&mut p, true);
        assert_eq!(t.ref_star(), 2);
        t.set_hemisphere(&mut p, false);
        assert_eq!((t.ref_star(), t.hemisphere()), (0, -1));
        assert_eq!(p.get_i32(keys::REF_STAR, -5), 2);
    }
}
