//! Typed epoch structs for RA/Dec coordinates (frontend / WASM).
//!
//! [`J2000`] — catalog epoch (ICRS), used by star and DSO catalogs.
//! [`JNow`]  — epoch-of-date, what INDI mounts report and accept.
//!
//! Uses the IAU 1976 Lieske precession model — sufficient for visual sky map rendering.

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// RA/Dec in J2000 epoch (ICRS-aligned, catalog standard).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct J2000 {
    pub ra_deg: f64,
    pub dec_deg: f64,
}

/// RA/Dec in current epoch-of-date (what INDI mounts report/accept).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JNow {
    pub ra_deg: f64,
    pub dec_deg: f64,
}

// ---------------------------------------------------------------------------
// J2000 impl
// ---------------------------------------------------------------------------

impl J2000 {
    pub fn new(ra_deg: f64, dec_deg: f64) -> Self {
        Self { ra_deg, dec_deg }
    }

    pub fn ra_hours(&self) -> f64 {
        self.ra_deg / 15.0
    }

    /// Precess from J2000 to epoch-of-date using the IAU 1976 Lieske precession model.
    pub fn to_jnow(&self, jd: f64) -> JNow {
        let (ra, dec) = precess_j2000_to_jnow(self.ra_deg, self.dec_deg, jd);
        JNow { ra_deg: ra, dec_deg: dec }
    }
}

// ---------------------------------------------------------------------------
// JNow impl
// ---------------------------------------------------------------------------

impl JNow {
    pub fn new(ra_deg: f64, dec_deg: f64) -> Self {
        Self { ra_deg, dec_deg }
    }

    pub fn ra_hours(&self) -> f64 {
        self.ra_deg / 15.0
    }

    /// Reverse-precess from epoch-of-date back to J2000.
    pub fn to_j2000(&self, jd: f64) -> J2000 {
        let (ra, dec) = precess_jnow_to_j2000(self.ra_deg, self.dec_deg, jd);
        J2000 { ra_deg: ra, dec_deg: dec }
    }
}

// ---------------------------------------------------------------------------
// IAU 1976 Lieske precession
// ---------------------------------------------------------------------------

/// Precess J2000 RA/Dec to epoch of date using the IAU 1976 Lieske precession model.
fn precess_j2000_to_jnow(ra_deg: f64, dec_deg: f64, jd: f64) -> (f64, f64) {
    let t = (jd - 2_451_545.0) / 36525.0;

    // IAU 1976 precession angles in arcseconds
    let zeta_a = (0.017998 * t + 0.30188) * t * t + 2306.2181 * t;
    let z_a = (0.018203 * t + 1.09468) * t * t + 2306.2181 * t;
    let theta_a = (-0.041833 * t - 0.42665) * t * t + 2004.3109 * t;

    let zeta = zeta_a.to_radians() / 3600.0;
    let z = z_a.to_radians() / 3600.0;
    let theta = theta_a.to_radians() / 3600.0;

    let ra0 = ra_deg.to_radians();
    let dec0 = dec_deg.to_radians();

    let cos_dec0 = dec0.cos();
    let sin_dec0 = dec0.sin();
    let cos_theta = theta.cos();
    let sin_theta = theta.sin();

    let a = cos_dec0 * (ra0 + zeta).sin();
    let b = cos_theta * cos_dec0 * (ra0 + zeta).cos() - sin_theta * sin_dec0;
    let c = sin_theta * cos_dec0 * (ra0 + zeta).cos() + cos_theta * sin_dec0;

    let mut ra_now = (a.atan2(b) + z).to_degrees();
    let dec_now = c.asin().to_degrees();

    ra_now = ra_now.rem_euclid(360.0);
    (ra_now, dec_now)
}

/// Inverse-precess from epoch-of-date back to J2000 (transpose rotation matrix).
fn precess_jnow_to_j2000(ra_deg: f64, dec_deg: f64, jd: f64) -> (f64, f64) {
    let t = (jd - 2_451_545.0) / 36525.0;

    let zeta_a = (0.017998 * t + 0.30188) * t * t + 2306.2181 * t;
    let z_a = (0.018203 * t + 1.09468) * t * t + 2306.2181 * t;
    let theta_a = (-0.041833 * t - 0.42665) * t * t + 2004.3109 * t;

    // Inverse: initial rotation -z, negate theta, final rotation -ζ
    let zeta = (-z_a).to_radians() / 3600.0;
    let z = (-zeta_a).to_radians() / 3600.0;
    let theta = (-theta_a).to_radians() / 3600.0;

    let ra0 = ra_deg.to_radians();
    let dec0 = dec_deg.to_radians();

    let cos_dec0 = dec0.cos();
    let sin_dec0 = dec0.sin();
    let cos_theta = theta.cos();
    let sin_theta = theta.sin();

    let a = cos_dec0 * (ra0 + zeta).sin();
    let b = cos_theta * cos_dec0 * (ra0 + zeta).cos() - sin_theta * sin_dec0;
    let c = sin_theta * cos_dec0 * (ra0 + zeta).cos() + cos_theta * sin_dec0;

    let ra_j2000 = (a.atan2(b) + z).to_degrees().rem_euclid(360.0);
    let dec_j2000 = c.asin().to_degrees();
    (ra_j2000, dec_j2000)
}
