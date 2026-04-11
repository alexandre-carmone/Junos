//! Coordinate math for the sky map.
//!
//! All angles in degrees unless suffixed otherwise.  Pure Rust, no deps.

use std::f64::consts::PI;

const DEG: f64 = PI / 180.0;

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// UTC calendar → Julian Date (valid for dates after 1582-10-15).
pub fn julian_date(y: i32, m: u32, d: u32, h: u32, min: u32, s: f64) -> f64 {
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let a = (y as f64 / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    let day_frac = (h as f64 + min as f64 / 60.0 + s / 3600.0) / 24.0;
    (365.25 * (y as f64 + 4716.0)).floor()
        + (30.6001 * (m as f64 + 1.0)).floor()
        + d as f64
        + day_frac
        + b
        - 1524.5
}

/// Greenwich Mean Sidereal Time in degrees from Julian Date.
pub fn gmst_deg(jd: f64) -> f64 {
    let t = (jd - 2451545.0) / 36525.0;
    let gmst = 280.46061837
        + 360.98564736629 * (jd - 2451545.0)
        + 0.000387933 * t * t
        - t * t * t / 38710000.0;
    gmst.rem_euclid(360.0)
}

/// Local Sidereal Time in degrees.
pub fn lst_deg(gmst: f64, lon_east: f64) -> f64 {
    (gmst + lon_east).rem_euclid(360.0)
}

// ---------------------------------------------------------------------------
// Coordinate transforms
// ---------------------------------------------------------------------------

/// Equatorial (RA/Dec degrees) → Horizontal (Alt, Az degrees).
/// `lst` = local sidereal time in degrees, `lat` = observer latitude in degrees.
pub fn eq_to_altaz(ra_deg: f64, dec_deg: f64, lst: f64, lat: f64) -> (f64, f64) {
    let ha = (lst - ra_deg).to_radians();
    let dec = dec_deg.to_radians();
    let phi = lat.to_radians();

    let sin_alt = dec.sin() * phi.sin() + dec.cos() * phi.cos() * ha.cos();
    let alt = sin_alt.asin();

    let cos_az_num = (dec.sin() - alt.sin() * phi.sin()) / (alt.cos() * phi.cos());
    let mut az = cos_az_num.clamp(-1.0, 1.0).acos();
    if ha.sin() > 0.0 {
        az = 2.0 * PI - az;
    }

    (alt.to_degrees(), az.to_degrees())
}

/// Horizontal (Alt, Az degrees) → Equatorial (RA, Dec degrees).
pub fn altaz_to_eq(alt_deg: f64, az_deg: f64, lst: f64, lat: f64) -> (f64, f64) {
    let alt = alt_deg.to_radians();
    let az = az_deg.to_radians();
    let phi = lat.to_radians();

    let sin_dec = alt.sin() * phi.sin() + alt.cos() * phi.cos() * az.cos();
    let dec = sin_dec.clamp(-1.0, 1.0).asin();

    let cos_ha = (alt.sin() - dec.sin() * phi.sin()) / (dec.cos() * phi.cos());
    let mut ha = cos_ha.clamp(-1.0, 1.0).acos();
    if az.sin() > 0.0 {
        ha = 2.0 * PI - ha;
    }

    let ra = (lst - ha.to_degrees()).rem_euclid(360.0);
    (ra, dec.to_degrees())
}

// ---------------------------------------------------------------------------
// Azimuthal equidistant projection
// ---------------------------------------------------------------------------

/// Project (alt, az) onto a unit disk centred on (center_alt, center_az).
///
/// Azimuthal equidistant: radial distance on the map is proportional to the
/// true angular distance from the centre — preserves distances along radii.
///
/// Returns `None` if the point is more than `fov_radius * 1.5` degrees from
/// the centre.  Output (x, y) is normalised so that a point exactly
/// `fov_radius` degrees away maps to ±1.
pub fn project(
    alt: f64,
    az: f64,
    center_alt: f64,
    center_az: f64,
    fov_radius: f64,
) -> Option<(f64, f64)> {
    let (a, b) = (alt * DEG, az * DEG);
    let (ca, cb) = (center_alt * DEG, center_az * DEG);

    // Angular separation
    let cos_c = a.sin() * ca.sin() + a.cos() * ca.cos() * (b - cb).cos();
    let c = cos_c.clamp(-1.0, 1.0).acos();

    if c.to_degrees() > fov_radius * 1.5 {
        return None;
    }

    // Azimuthal equidistant: r is simply proportional to c
    let r = c / (fov_radius * DEG);

    // Position angle from centre to point
    let sin_pa = a.cos() * (b - cb).sin();
    let cos_pa = a.sin() * ca.cos() - a.cos() * ca.sin() * (b - cb).cos();
    let pa = sin_pa.atan2(cos_pa);

    let x = r * pa.sin();
    let y = r * pa.cos();

    Some((x, y))
}

/// Like [`project`] but without the angular-distance cutoff.
///
/// Returns normalised screen coordinates even for points far from the view
/// centre; the browser canvas will clip anything outside its bounds.
pub fn project_unclamped(
    alt: f64,
    az: f64,
    center_alt: f64,
    center_az: f64,
    fov_radius: f64,
) -> (f64, f64) {
    let (a, b) = (alt * DEG, az * DEG);
    let (ca, cb) = (center_alt * DEG, center_az * DEG);
    let cos_c = a.sin() * ca.sin() + a.cos() * ca.cos() * (b - cb).cos();
    let c = cos_c.clamp(-1.0, 1.0).acos();
    let r = c / (fov_radius * DEG);
    let sin_pa = a.cos() * (b - cb).sin();
    let cos_pa = a.sin() * ca.cos() - a.cos() * ca.sin() * (b - cb).cos();
    let pa = sin_pa.atan2(cos_pa);
    (r * pa.sin(), r * pa.cos())
}

/// Inverse azimuthal equidistant: screen (x, y) → (alt, az) degrees.
///
/// Input is normalised so that ±1 corresponds to `fov_radius` degrees.
pub fn unproject(
    x: f64,
    y: f64,
    center_alt: f64,
    center_az: f64,
    fov_radius: f64,
) -> (f64, f64) {
    let r = (x * x + y * y).sqrt();
    let c = r * fov_radius * DEG; // angular distance in radians

    let ca = center_alt * DEG;
    let cb = center_az * DEG;

    if r < 1e-12 {
        return (center_alt, center_az);
    }

    let pa = x.atan2(y);

    let sin_alt = ca.sin() * c.cos() + ca.cos() * c.sin() * pa.cos();
    let alt = sin_alt.clamp(-1.0, 1.0).asin();

    let daz = (c.sin() * pa.sin()).atan2(ca.cos() * c.cos() - ca.sin() * c.sin() * pa.cos());
    let az = (cb + daz).to_degrees().rem_euclid(360.0);

    (alt.to_degrees(), az)
}

// ---------------------------------------------------------------------------
// Optics
// ---------------------------------------------------------------------------

/// Field of view in degrees from focal length (mm), sensor dimension (pixels),
/// and pixel size (micrometres).
pub fn fov_deg(focal_mm: f64, sensor_px: f64, pixel_um: f64) -> f64 {
    let sensor_mm = sensor_px * pixel_um / 1000.0;
    2.0 * (sensor_mm / (2.0 * focal_mm)).atan().to_degrees()
}
