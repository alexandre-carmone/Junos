//! Low-precision ephemeris for Sun, Moon, and the major planets.
//!
//! Adapted from Paul Schlyter's "How to compute planetary positions"
//! (<http://www.stjarnhimlen.se/comp/ppcomp.html>). Accuracy:
//!  - Sun       ~1 arcmin
//!  - Moon      ~2 arcmin
//!  - Planets   ~1–5 arcmin
//!
//! Output RA/Dec is in mean equinox-of-date (effectively JNow), so no extra
//! precession is needed before feeding it into `astro::eq_to_altaz`.
//!
//! Pure f64, no dependencies. All angles in degrees unless noted.
#![allow(clippy::too_many_lines)]

use std::f64::consts::PI;

use crate::coords::JNow;

const DEG: f64 = PI / 180.0;
const RAD: f64 = 180.0 / PI;

/// Apparent position of a celestial body at a given Julian Date.
#[derive(Debug, Clone, Copy)]
pub struct BodyPos {
    pub jnow: JNow,
    pub mag: f32,
    pub phase: Option<f64>,                   // illuminated fraction 0..1
    pub angular_diameter_arcmin: Option<f64>, // Sun, Moon
    pub distance_au: f64,                     // geocentric distance (AU)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planet {
    Mercury, Venus, Mars, Jupiter, Saturn, Uranus, Neptune,
}

impl Planet {
    pub fn name(self) -> &'static str {
        match self {
            Planet::Mercury => "Mercury",
            Planet::Venus   => "Venus",
            Planet::Mars    => "Mars",
            Planet::Jupiter => "Jupiter",
            Planet::Saturn  => "Saturn",
            Planet::Uranus  => "Uranus",
            Planet::Neptune => "Neptune",
        }
    }
}

pub const ALL_PLANETS: [Planet; 7] = [
    Planet::Mercury, Planet::Venus, Planet::Mars,
    Planet::Jupiter, Planet::Saturn, Planet::Uranus, Planet::Neptune,
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Days since Schlyter's epoch (1999 Dec 31, 0h UT = JD 2451543.5).
fn days(jd: f64) -> f64 { jd - 2451543.5 }

fn norm360(x: f64) -> f64 {
    let r = x.rem_euclid(360.0);
    if r < 0.0 { r + 360.0 } else { r }
}

/// Solve Kepler's equation E - e*sin(E) = M (E, M in degrees).
fn kepler(m_deg: f64, e: f64) -> f64 {
    let m = m_deg * DEG;
    let mut ecc = m + e * m.sin() * (1.0 + e * m.cos());
    for _ in 0..8 {
        let dm = ecc - e * ecc.sin() - m;
        let de = dm / (1.0 - e * ecc.cos());
        ecc -= de;
        if de.abs() < 1e-10 { break; }
    }
    ecc * RAD
}

fn obliquity(d: f64) -> f64 {
    23.4393 - 3.563e-7 * d
}

/// Convert ecliptic (lon, lat in degrees, distance) to equatorial (ra_deg, dec_deg).
fn ecl_to_eq(lon: f64, lat: f64, ecl: f64) -> (f64, f64) {
    let l = lon * DEG;
    let b = lat * DEG;
    let e = ecl * DEG;
    let xe = b.cos() * l.cos();
    let ye = b.cos() * l.sin() * e.cos() - b.sin() * e.sin();
    let ze = b.cos() * l.sin() * e.sin() + b.sin() * e.cos();
    let ra  = norm360(ye.atan2(xe) * RAD);
    let dec = ze.atan2((xe * xe + ye * ye).sqrt()) * RAD;
    (ra, dec)
}

// ---------------------------------------------------------------------------
// Sun
// ---------------------------------------------------------------------------

/// Returns (geocentric ecliptic xs, ys in AU, lon_deg, distance_r AU, M_sun_deg).
fn sun_helio(d: f64) -> (f64, f64, f64, f64, f64) {
    let w = 282.9404 + 4.70935e-5 * d;
    let e = 0.016709 - 1.151e-9 * d;
    let m_s = norm360(356.0470 + 0.9856002585 * d);
    let ecc = kepler(m_s, e);
    let xv = ecc.to_radians().cos() - e;
    let yv = (1.0 - e * e).sqrt() * ecc.to_radians().sin();
    let v  = yv.atan2(xv) * RAD;
    let r  = (xv * xv + yv * yv).sqrt();
    let lon = norm360(v + w);
    let xs = r * lon.to_radians().cos();
    let ys = r * lon.to_radians().sin();
    (xs, ys, lon, r, m_s)
}

pub fn sun(jd: f64) -> BodyPos {
    let d = days(jd);
    let (_xs, _ys, lon, r, _ms) = sun_helio(d);
    let (ra, dec) = ecl_to_eq(lon, 0.0, obliquity(d));
    let ang_dia = 1919.26 / r / 60.0; // arcmin
    BodyPos {
        jnow: JNow::new(ra, dec),
        mag: -26.74,
        phase: None,
        angular_diameter_arcmin: Some(ang_dia),
        distance_au: r,
    }
}

// ---------------------------------------------------------------------------
// Moon (with main perturbations)
// ---------------------------------------------------------------------------

pub fn moon(jd: f64) -> BodyPos {
    let d = days(jd);

    // Orbital elements (mean)
    let n = norm360(125.1228 - 0.0529538083 * d);
    let i = 5.1454_f64;
    let w = norm360(318.0634 + 0.1643573223 * d);
    let a = 60.2666_f64; // Earth radii
    let e = 0.054900_f64;
    let mm = norm360(115.3654 + 13.0649929509 * d);

    let ecc = kepler(mm, e);
    let xv = a * (ecc.to_radians().cos() - e);
    let yv = a * (1.0 - e * e).sqrt() * ecc.to_radians().sin();
    let v  = yv.atan2(xv) * RAD;
    let r  = (xv * xv + yv * yv).sqrt(); // Earth radii

    let vw = (v + w) * DEG;
    let nr = n * DEG;
    let ir = i * DEG;
    let xh = r * (nr.cos() * vw.cos() - nr.sin() * vw.sin() * ir.cos());
    let yh = r * (nr.sin() * vw.cos() + nr.cos() * vw.sin() * ir.cos());
    let zh = r * vw.sin() * ir.sin();

    let mut lon = norm360(yh.atan2(xh) * RAD);
    let mut lat = zh.atan2((xh * xh + yh * yh).sqrt()) * RAD;

    // Sun's mean anomaly + mean longitude (for perturbations)
    let (_, _, sun_lon, _, ms) = sun_helio(d);
    let lm = lon;            // Moon mean longitude (using current lon)
    let ls = sun_lon;        // Sun mean longitude
    let dd = lm - ls;        // Mean elongation
    let f  = lm - n;         // Argument of latitude

    let mm_r = mm.to_radians();
    let ms_r = ms.to_radians();
    let dd_r = dd.to_radians();
    let f_r  = f.to_radians();

    // Main lunar perturbations (Schlyter's truncated set; arcmin-class accuracy)
    lon += -1.274 * (mm_r - 2.0 * dd_r).sin();
    lon +=  0.658 * (2.0 * dd_r).sin();
    lon += -0.186 * ms_r.sin();
    lon += -0.059 * (2.0 * mm_r - 2.0 * dd_r).sin();
    lon += -0.057 * (mm_r - 2.0 * dd_r + ms_r).sin();
    lon +=  0.053 * (mm_r + 2.0 * dd_r).sin();
    lon +=  0.046 * (2.0 * dd_r - ms_r).sin();
    lon +=  0.041 * (mm_r - ms_r).sin();
    lon += -0.035 * dd_r.sin();
    lon += -0.031 * (mm_r + ms_r).sin();
    lon += -0.015 * (2.0 * f_r - 2.0 * dd_r).sin();
    lon +=  0.011 * (mm_r - 4.0 * dd_r).sin();

    lat += -0.173 * (f_r - 2.0 * dd_r).sin();
    lat += -0.055 * (mm_r - f_r - 2.0 * dd_r).sin();
    lat += -0.046 * (mm_r + f_r - 2.0 * dd_r).sin();
    lat +=  0.033 * (f_r + 2.0 * dd_r).sin();
    lat +=  0.017 * (2.0 * mm_r + f_r).sin();

    lon = norm360(lon);

    let (ra, dec) = ecl_to_eq(lon, lat, obliquity(d));

    // Phase: elongation Sun↔Moon
    let elong = ((sun_lon - lon).to_radians().cos()
        * lat.to_radians().cos())
        .acos() * RAD;
    let phase_angle = 180.0 - elong; // 0 = full, 180 = new
    let illum = (1.0 + phase_angle.to_radians().cos()) / 2.0;

    // Magnitude (very rough): scales with phase.
    let mag = -12.7 + 0.026 * phase_angle.abs() + 4.0e-9 * phase_angle.powi(4);

    // Angular diameter: Moon mean radius 1737.4 km / earth radius 6378.14 km
    // r in Earth radii, distance in km = r * 6378.14
    // ang_diameter_rad = 2 * 1737.4 / (r * 6378.14)
    let ang_dia = (2.0 * 1737.4 / (r * 6378.14)).to_degrees() * 60.0;

    BodyPos {
        jnow: JNow::new(ra, dec),
        mag: mag as f32,
        phase: Some(illum),
        angular_diameter_arcmin: Some(ang_dia),
        distance_au: r * 6378.14 / 149_597_870.7,
    }
}

// ---------------------------------------------------------------------------
// Planets
// ---------------------------------------------------------------------------

struct Elements {
    n: f64, i: f64, w: f64, a: f64, e: f64, m: f64,
}

fn elements(p: Planet, d: f64) -> Elements {
    match p {
        Planet::Mercury => Elements {
            n:  48.3313 + 3.24587e-5 * d,
            i:   7.0047 + 5.00e-8 * d,
            w:  29.1241 + 1.01444e-5 * d,
            a:   0.387098,
            e:   0.205635 + 5.59e-10 * d,
            m: 168.6562 + 4.0923344368 * d,
        },
        Planet::Venus => Elements {
            n:  76.6799 + 2.46590e-5 * d,
            i:   3.3946 + 2.75e-8 * d,
            w:  54.8910 + 1.38374e-5 * d,
            a:   0.723330,
            e:   0.006773 - 1.302e-9 * d,
            m:  48.0052 + 1.6021302244 * d,
        },
        Planet::Mars => Elements {
            n:  49.5574 + 2.11081e-5 * d,
            i:   1.8497 - 1.78e-8 * d,
            w: 286.5016 + 2.92961e-5 * d,
            a:   1.523688,
            e:   0.093405 + 2.516e-9 * d,
            m:  18.6021 + 0.5240207766 * d,
        },
        Planet::Jupiter => Elements {
            n: 100.4542 + 2.76854e-5 * d,
            i:   1.3030 - 1.557e-7 * d,
            w: 273.8777 + 1.64505e-5 * d,
            a:   5.20256,
            e:   0.048498 + 4.469e-9 * d,
            m:  19.8950 + 0.0830853001 * d,
        },
        Planet::Saturn => Elements {
            n: 113.6634 + 2.38980e-5 * d,
            i:   2.4886 - 1.081e-7 * d,
            w: 339.3939 + 2.97661e-5 * d,
            a:   9.55475,
            e:   0.055546 - 9.499e-9 * d,
            m: 316.9670 + 0.0334442282 * d,
        },
        Planet::Uranus => Elements {
            n:  74.0005 + 1.3978e-5 * d,
            i:   0.7733 + 1.9e-8 * d,
            w:  96.6612 + 3.0565e-5 * d,
            a:  19.18171 - 1.55e-8 * d,
            e:   0.047318 + 7.45e-9 * d,
            m: 142.5905 + 0.011725806 * d,
        },
        Planet::Neptune => Elements {
            n: 131.7806 + 3.0173e-5 * d,
            i:   1.7700 - 2.55e-7 * d,
            w: 272.8461 - 6.027e-6 * d,
            a:  30.05826 + 3.313e-8 * d,
            e:   0.008606 + 2.15e-9 * d,
            m: 260.2471 + 0.005995147 * d,
        },
    }
}

/// Heliocentric ecliptic position (xh, yh, zh in AU) and orbital radius r.
fn helio(p: Planet, d: f64) -> (f64, f64, f64, f64) {
    let el = elements(p, d);
    let m  = norm360(el.m);
    let ecc = kepler(m, el.e);
    let xv = el.a * (ecc.to_radians().cos() - el.e);
    let yv = el.a * (1.0 - el.e * el.e).sqrt() * ecc.to_radians().sin();
    let v = yv.atan2(xv) * RAD;
    let r = (xv * xv + yv * yv).sqrt();

    let vw = (v + el.w) * DEG;
    let nr = el.n * DEG;
    let ir = el.i * DEG;
    let xh = r * (nr.cos() * vw.cos() - nr.sin() * vw.sin() * ir.cos());
    let yh = r * (nr.sin() * vw.cos() + nr.cos() * vw.sin() * ir.cos());
    let zh = r * vw.sin() * ir.sin();
    (xh, yh, zh, r)
}

pub fn planet(p: Planet, jd: f64) -> BodyPos {
    let d = days(jd);
    let (xh, yh, zh, r_helio) = helio(p, d);
    let (xs, ys, _slon, _sr, _sm) = sun_helio(d);

    // Geocentric ecliptic = heliocentric_planet + sun_geocentric
    let xg = xh + xs;
    let yg = yh + ys;
    let zg = zh;

    let dist = (xg * xg + yg * yg + zg * zg).sqrt();
    let lon  = norm360(yg.atan2(xg) * RAD);
    let lat  = (zg / dist).asin() * RAD;
    let (ra, dec) = ecl_to_eq(lon, lat, obliquity(d));

    // Phase angle (planet, geocentric vs heliocentric)
    let r = r_helio;
    let r_sun_earth = (xs * xs + ys * ys).sqrt();
    let cos_fv = (r * r + dist * dist - r_sun_earth * r_sun_earth)
        / (2.0 * r * dist);
    let fv = cos_fv.clamp(-1.0, 1.0).acos() * RAD;

    let log_rd = (r * dist).log10();
    let mag = match p {
        Planet::Mercury => -0.36 + 5.0 * log_rd + 0.027 * fv + 2.2e-13 * fv.powi(6),
        Planet::Venus   => -4.34 + 5.0 * log_rd + 0.013 * fv + 4.2e-7  * fv.powi(3),
        Planet::Mars    => -1.51 + 5.0 * log_rd + 0.016 * fv,
        Planet::Jupiter => -9.25 + 5.0 * log_rd + 0.014 * fv,
        Planet::Saturn  => -9.0  + 5.0 * log_rd + 0.044 * fv, // rings ignored
        Planet::Uranus  => -7.15 + 5.0 * log_rd + 0.001 * fv,
        Planet::Neptune => -6.90 + 5.0 * log_rd + 0.001 * fv,
    };

    let illum = (1.0 + fv.to_radians().cos()) / 2.0;

    BodyPos {
        jnow: JNow::new(ra, dec),
        mag: mag as f32,
        phase: Some(illum),
        angular_diameter_arcmin: None,
        distance_au: dist,
    }
}

pub fn all_planets(jd: f64) -> [(Planet, BodyPos); 7] {
    [
        (Planet::Mercury, planet(Planet::Mercury, jd)),
        (Planet::Venus,   planet(Planet::Venus,   jd)),
        (Planet::Mars,    planet(Planet::Mars,    jd)),
        (Planet::Jupiter, planet(Planet::Jupiter, jd)),
        (Planet::Saturn,  planet(Planet::Saturn,  jd)),
        (Planet::Uranus,  planet(Planet::Uranus,  jd)),
        (Planet::Neptune, planet(Planet::Neptune, jd)),
    ]
}
