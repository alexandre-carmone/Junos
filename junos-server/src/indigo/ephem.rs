//! Sun and Moon positions, for the scheduler's constraints (twilight, Moon
//! separation and altitude). Ported from `junos-web/src/ephemeris.rs` — Paul
//! Schlyter's low-precision method (<http://www.stjarnhimlen.se/comp/ppcomp.html>,
//! ~1′ for the Sun, ~2′ for the Moon) reduced to the two bodies — keep in
//! step. Output is the mean equinox of date, so it goes straight to
//! `astro::eq_to_altaz`; no precession needed.

use std::f64::consts::PI;

const DEG: f64 = PI / 180.0;
const RAD: f64 = 180.0 / PI;

/// Days since Schlyter's epoch (1999 Dec 31, 0h UT = JD 2451543.5).
fn days(jd: f64) -> f64 {
    jd - 2_451_543.5
}

fn norm360(x: f64) -> f64 {
    x.rem_euclid(360.0)
}

/// Solve Kepler's equation E − e·sin E = M (E, M in degrees).
fn kepler(m_deg: f64, e: f64) -> f64 {
    let m = m_deg * DEG;
    let mut ecc = m + e * m.sin() * (1.0 + e * m.cos());
    for _ in 0..8 {
        let dm = ecc - e * ecc.sin() - m;
        let de = dm / (1.0 - e * ecc.cos());
        ecc -= de;
        if de.abs() < 1e-10 {
            break;
        }
    }
    ecc * RAD
}

fn obliquity(d: f64) -> f64 {
    23.4393 - 3.563e-7 * d
}

/// Ecliptic (lon, lat in degrees) → equatorial (ra_deg, dec_deg).
fn ecl_to_eq(lon: f64, lat: f64, ecl: f64) -> (f64, f64) {
    let l = lon * DEG;
    let b = lat * DEG;
    let e = ecl * DEG;
    let xe = b.cos() * l.cos();
    let ye = b.cos() * l.sin() * e.cos() - b.sin() * e.sin();
    let ze = b.cos() * l.sin() * e.sin() + b.sin() * e.cos();
    let ra = norm360(ye.atan2(xe) * RAD);
    let dec = ze.atan2((xe * xe + ye * ye).sqrt()) * RAD;
    (ra, dec)
}

/// The Sun's geocentric ecliptic longitude (deg) and its mean anomaly (deg).
fn sun_lon(d: f64) -> (f64, f64) {
    let w = 282.9404 + 4.70935e-5 * d;
    let e = 0.016709 - 1.151e-9 * d;
    let m_s = norm360(356.0470 + 0.9856002585 * d);
    let ecc = kepler(m_s, e);
    let xv = ecc.to_radians().cos() - e;
    let yv = (1.0 - e * e).sqrt() * ecc.to_radians().sin();
    let v = yv.atan2(xv) * RAD;
    (norm360(v + w), m_s)
}

/// The Sun: (ra_deg, dec_deg), equinox of date.
pub fn sun(jd: f64) -> (f64, f64) {
    let d = days(jd);
    let (lon, _) = sun_lon(d);
    ecl_to_eq(lon, 0.0, obliquity(d))
}

/// The Moon: (ra_deg, dec_deg, illuminated fraction 0..1), equinox of date,
/// with Schlyter's main perturbations.
pub fn moon(jd: f64) -> (f64, f64, f64) {
    let d = days(jd);

    // Mean orbital elements
    let n = norm360(125.1228 - 0.0529538083 * d);
    let i = 5.1454_f64;
    let w = norm360(318.0634 + 0.1643573223 * d);
    let a = 60.2666_f64; // Earth radii
    let e = 0.054900_f64;
    let mm = norm360(115.3654 + 13.0649929509 * d);

    let ecc = kepler(mm, e);
    let xv = a * (ecc.to_radians().cos() - e);
    let yv = a * (1.0 - e * e).sqrt() * ecc.to_radians().sin();
    let v = yv.atan2(xv) * RAD;
    let r = (xv * xv + yv * yv).sqrt();

    let vw = (v + w) * DEG;
    let nr = n * DEG;
    let ir = i * DEG;
    let xh = r * (nr.cos() * vw.cos() - nr.sin() * vw.sin() * ir.cos());
    let yh = r * (nr.sin() * vw.cos() + nr.cos() * vw.sin() * ir.cos());
    let zh = r * vw.sin() * ir.sin();

    let mut lon = norm360(yh.atan2(xh) * RAD);
    let mut lat = zh.atan2((xh * xh + yh * yh).sqrt()) * RAD;

    // Sun's mean longitude and anomaly for the perturbations
    let (sun_lon, ms) = sun_lon(d);
    let dd = lon - sun_lon; // mean elongation
    let f = lon - n; // argument of latitude

    let mm_r = mm.to_radians();
    let ms_r = ms.to_radians();
    let dd_r = dd.to_radians();
    let f_r = f.to_radians();

    lon += -1.274 * (mm_r - 2.0 * dd_r).sin();
    lon += 0.658 * (2.0 * dd_r).sin();
    lon += -0.186 * ms_r.sin();
    lon += -0.059 * (2.0 * mm_r - 2.0 * dd_r).sin();
    lon += -0.057 * (mm_r - 2.0 * dd_r + ms_r).sin();
    lon += 0.053 * (mm_r + 2.0 * dd_r).sin();
    lon += 0.046 * (2.0 * dd_r - ms_r).sin();
    lon += 0.041 * (mm_r - ms_r).sin();
    lon += -0.035 * dd_r.sin();
    lon += -0.031 * (mm_r + ms_r).sin();
    lon += -0.015 * (2.0 * f_r - 2.0 * dd_r).sin();
    lon += 0.011 * (mm_r - 4.0 * dd_r).sin();

    lat += -0.173 * (f_r - 2.0 * dd_r).sin();
    lat += -0.055 * (mm_r - f_r - 2.0 * dd_r).sin();
    lat += -0.046 * (mm_r + f_r - 2.0 * dd_r).sin();
    lat += 0.033 * (f_r + 2.0 * dd_r).sin();
    lat += 0.017 * (2.0 * mm_r + f_r).sin();

    lon = norm360(lon);
    let (ra, dec) = ecl_to_eq(lon, lat, obliquity(d));

    // Phase from the Sun ↔ Moon elongation: 0 = new, 180 = full.
    let elong = ((sun_lon - lon).to_radians().cos() * lat.to_radians().cos()).acos() * RAD;
    let phase_angle = 180.0 - elong;
    let illum = (1.0 + phase_angle.to_radians().cos()) / 2.0;

    (ra, dec, illum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::astro::angular_sep_deg;

    #[test]
    fn sun_at_j2000() {
        // 2000-01-01 12:00 TT: RA 18h 44m 50s, Dec −23° 02′ (Astronomical Almanac).
        let (ra, dec) = sun(2_451_545.0);
        assert!((ra - 281.3).abs() < 0.5, "{ra}");
        assert!((dec + 23.0).abs() < 0.5, "{dec}");
    }

    #[test]
    fn moon_on_new_year_2025() {
        // 2025-01-01 00:00 UT, 25.5 h after the new moon of 2024-12-30 22:27 UT:
        // ~13° east of the Sun along the ecliptic, ~5° south of it (102° past
        // the descending node, then near 192°), which works out to RA ≈ 297°,
        // Dec ≈ −26° by hand.
        let jd = 2_460_676.5;
        let (ra, dec, illum) = moon(jd);
        assert!((ra - 297.0).abs() < 1.0, "{ra}");
        assert!((dec + 26.0).abs() < 1.0, "{dec}");
        assert!(illum < 0.1, "{illum}");
        let (sra, sdec) = sun(jd);
        let elong = angular_sep_deg(sra, sdec, ra, dec);
        assert!((11.0..16.0).contains(&elong), "{elong}");
        assert!(ra > sra, "the young moon sits east of the sun");
    }
}
