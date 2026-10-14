//! The little astronomy the mount emitter needs: sidereal time, IAU 1976
//! precession and equatorial → horizontal. Ported from `junos-web/src/{astro,
//! coords}.rs` — that crate is wasm-only, so the server can't depend on it.
//! Keep the two in step.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current Julian Date (UTC).
pub fn now_jd() -> f64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    2_440_587.5 + secs / 86_400.0
}

/// Greenwich Mean Sidereal Time in degrees.
pub fn gmst_deg(jd: f64) -> f64 {
    let t = (jd - 2_451_545.0) / 36525.0;
    let gmst = 280.46061837
        + 360.98564736629 * (jd - 2_451_545.0)
        + 0.000387933 * t * t
        - t * t * t / 38_710_000.0;
    gmst.rem_euclid(360.0)
}

/// Local Sidereal Time in degrees, east longitude positive.
pub fn lst_deg(jd: f64, lon_east: f64) -> f64 {
    (gmst_deg(jd) + lon_east).rem_euclid(360.0)
}

fn precession_angles(jd: f64) -> (f64, f64, f64) {
    let t = (jd - 2_451_545.0) / 36525.0;
    let zeta_a = (0.017998 * t + 0.30188) * t * t + 2306.2181 * t;
    let z_a = (0.018203 * t + 1.09468) * t * t + 2306.2181 * t;
    let theta_a = (-0.041833 * t - 0.42665) * t * t + 2004.3109 * t;
    (
        zeta_a.to_radians() / 3600.0,
        z_a.to_radians() / 3600.0,
        theta_a.to_radians() / 3600.0,
    )
}

fn rotate(ra_deg: f64, dec_deg: f64, zeta: f64, z: f64, theta: f64) -> (f64, f64) {
    let ra0 = ra_deg.to_radians();
    let dec0 = dec_deg.to_radians();
    let (cos_dec0, sin_dec0) = (dec0.cos(), dec0.sin());
    let (cos_theta, sin_theta) = (theta.cos(), theta.sin());
    let a = cos_dec0 * (ra0 + zeta).sin();
    let b = cos_theta * cos_dec0 * (ra0 + zeta).cos() - sin_theta * sin_dec0;
    let c = sin_theta * cos_dec0 * (ra0 + zeta).cos() + cos_theta * sin_dec0;
    ((a.atan2(b) + z).to_degrees().rem_euclid(360.0), c.asin().to_degrees())
}

/// J2000 → epoch of date, degrees in and out.
pub fn j2000_to_jnow(ra_deg: f64, dec_deg: f64, jd: f64) -> (f64, f64) {
    let (zeta, z, theta) = precession_angles(jd);
    rotate(ra_deg, dec_deg, zeta, z, theta)
}

/// Epoch of date → J2000 (the transposed rotation), degrees in and out.
pub fn jnow_to_j2000(ra_deg: f64, dec_deg: f64, jd: f64) -> (f64, f64) {
    let (zeta, z, theta) = precession_angles(jd);
    rotate(ra_deg, dec_deg, -z, -zeta, -theta)
}

/// Equatorial (degrees) → (alt, az) degrees, az from north through east.
pub fn eq_to_altaz(ra_deg: f64, dec_deg: f64, lst_deg: f64, lat_deg: f64) -> (f64, f64) {
    let ha = (lst_deg - ra_deg).to_radians();
    let dec = dec_deg.to_radians();
    let lat = lat_deg.to_radians();
    let sin_alt = dec.sin() * lat.sin() + dec.cos() * lat.cos() * ha.cos();
    let alt = sin_alt.asin();
    let y = -ha.sin() * dec.cos();
    let x = dec.sin() * lat.cos() - dec.cos() * lat.sin() * ha.cos();
    let az = y.atan2(x).to_degrees().rem_euclid(360.0);
    (alt.to_degrees(), az)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precession_round_trips_and_moves_the_right_way() {
        // Vega, J2000: 18h36m56.3s +38°47'01". ~25 years of precession
        // shifts RA by about +0.3° at this declination.
        let jd = 2_460_676.5; // 2025-01-01
        let (ra, dec) = (279.2347, 38.7837);
        let (ra_now, dec_now) = j2000_to_jnow(ra, dec, jd);
        assert!(ra_now > ra + 0.2 && ra_now < ra + 0.5, "{ra_now}");
        assert!((dec_now - dec).abs() < 0.1);
        let (ra_back, dec_back) = jnow_to_j2000(ra_now, dec_now, jd);
        assert!((ra_back - ra).abs() < 1e-9 && (dec_back - dec).abs() < 1e-9);
    }

    #[test]
    fn sidereal_time_at_j2000_epoch() {
        // GMST at 2000-01-01 12:00 UT is 18h41m50.5s = 280.46°.
        assert!((gmst_deg(2_451_545.0) - 280.4606).abs() < 1e-3);
        assert!((lst_deg(2_451_545.0, 100.0) - 20.4606).abs() < 1e-3);
    }

    #[test]
    fn zenith_and_horizon() {
        // An object on the meridian at the observer's declination is at zenith.
        let (alt, _) = eq_to_altaz(100.0, 45.0, 100.0, 45.0);
        assert!((alt - 90.0).abs() < 1e-9);
        // Due east, on the equator, from the equator: rising, az 90.
        let (alt, az) = eq_to_altaz(90.0, 0.0, 0.0, 0.0);
        assert!(alt.abs() < 1e-9 && (az - 90.0).abs() < 1e-9);
    }
}
