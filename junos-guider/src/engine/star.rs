//! Star detection: the centroid / peak finder every guide frame goes through
//! (`Star::Find`), its half-flux diameter and its saturation tests, and the
//! status strings the event server reports for a lost star.
//!
//! `find` is a step-for-step port: same search bounds, same 3×3 smoothing
//! kernel, same annulus background with its sigma clipping, same SNR
//! formula, same rejection order and the same quirks (the `snr = 2.9`
//! false-star guard, the single-pixel HFR of 0.25). Nothing here looks at the
//! configuration: every PHD2 setting is a parameter.
//!
//! Ported from PHD2 src/star.h, src/star.cpp, src/guider_multistar.cpp (the status strings) — BSD-3-Clause, see THIRD_PARTY.md

use super::image::UsImage;
use super::traits::Point;

/// `Star::FindMode`: a sub-pixel centroid, or the brightest raw pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindMode {
    Centroid,
    Peak,
}

/// `Star::FindResult`. The discriminants are PHD2's: the event server sends
/// them as `ErrorCode` in `StarLost` and `GuideStep`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FindResult {
    Ok = 0,
    Saturated = 1,
    LowSnr = 2,
    LowMass = 3,
    LowHfd = 4,
    HiHfd = 5,
    /// Never produced by `find`; the guider sets it when the star drifts
    /// too close to the frame edge.
    TooNearEdge = 6,
    /// Set by the guider's mass checker, not by `find`.
    MassChange = 7,
    #[default]
    Error = 8,
}

impl FindResult {
    /// `Star::WasFound(FindResult)`: a saturated star still counts as found.
    pub fn was_found(self) -> bool {
        matches!(self, FindResult::Ok | FindResult::Saturated)
    }

    /// The `ErrorCode` value of the event server (the enum's integer).
    pub fn error_code(self) -> i32 {
        self as i32
    }

    /// `StarStatusStr` (guider_multistar.cpp:400) for a valid star: the
    /// `Status` text of `StarLost` / `GuideStep` and the status bar.
    pub fn status_str(self) -> &'static str {
        match self {
            FindResult::LowSnr => "Star lost - low SNR",
            FindResult::LowMass => "Star lost - low mass",
            FindResult::LowHfd => "Star lost - low HFD",
            FindResult::TooNearEdge => "Star too near edge",
            FindResult::MassChange => "Star lost - mass changed",
            _ => "No star found",
        }
    }
}

/// PHD2's `Star`: a `PHD_Point` plus the measurements of the last `find`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Star {
    pub x: f64,
    pub y: f64,
    /// Background-subtracted flux of the pixels over the threshold (ADU).
    pub mass: f64,
    pub snr: f64,
    /// Half-flux diameter in pixels (`2 × hfr`).
    pub hfd: f64,
    /// Brightest raw pixel of the search region.
    pub peak_val: u16,
    pub result: FindResult,
    /// `PHD_Point::IsValid()`. A fresh star is invalid with `x = y = 0`;
    /// `find` always validates it, even on `Error` (star.cpp:437).
    pub valid: bool,
}

impl Default for Star {
    fn default() -> Self {
        Self::new()
    }
}

impl Star {
    /// `Star::Star()`: invalid, everything zero, result `Error`.
    pub fn new() -> Self {
        Star {
            x: 0.0,
            y: 0.0,
            mass: 0.0,
            snr: 0.0,
            hfd: 0.0,
            peak_val: 0,
            result: FindResult::Error,
            valid: false,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// `Star::WasFound()`: valid and `Ok` or `Saturated`.
    pub fn was_found(&self) -> bool {
        self.valid && self.result.was_found()
    }

    /// `Star::Invalidate`: zero the measurements, result `Error`, point
    /// invalid (the position itself is kept, PHD2 reads it afterwards).
    pub fn invalidate(&mut self) {
        self.mass = 0.0;
        self.snr = 0.0;
        self.hfd = 0.0;
        self.result = FindResult::Error;
        self.valid = false;
    }

    /// `Star::SetError`.
    pub fn set_error(&mut self, error: FindResult) {
        self.result = error;
    }

    /// `Star::SetXY`: move the star and make it valid.
    pub fn set_xy(&mut self, x: f64, y: f64) {
        self.x = x;
        self.y = y;
        self.valid = true;
    }

    pub fn position(&self) -> Point {
        Point::new(self.x, self.y)
    }

    /// `StarStatusStr` (guider_multistar.cpp:400): "No star selected" for an
    /// invalid star, else the result's text.
    pub fn status_str(&self) -> &'static str {
        if !self.valid {
            "No star selected"
        } else {
            self.result.status_str()
        }
    }

    /// `Star::Find` (star.cpp:126): locate the star within `search_region`
    /// pixels of (`base_x`, `base_y`), clipped to the subframe (or the
    /// frame). `max_adu` is the camera's saturation level (0 = unknown, use
    /// the flat-top heuristic). `min_hfd` / `max_hfd` bound the HFD in
    /// `Centroid` mode only. The result's `was_found()` is PHD2's return.
    ///
    /// The base position is truncated to integers like PHD2's `int` parameters
    /// (star.cpp:482 passes doubles to them).
    pub fn find(
        img: &UsImage,
        search_region: i32,
        base_x: f64,
        base_y: f64,
        mode: FindMode,
        min_hfd: f64,
        max_hfd: f64,
        max_adu: u32,
    ) -> Star {
        let base_x = base_x as i32;
        let base_y = base_y as i32;
        let mut star = Star::new();
        let (new_x, new_y, result) =
            find_body(&mut star, img, search_region, base_x, base_y, mode, min_hfd, max_hfd, max_adu);

        // star.cpp:437 `done:` — the point is set (and so valid) whatever
        // happened, then the measurements are zeroed on an error. PHD2 also
        // zeroes them when the point is invalid, which SetXY makes impossible.
        star.set_xy(new_x, new_y);
        star.result = result;
        if result == FindResult::Error {
            star.mass = 0.0;
            star.snr = 0.0;
            star.hfd = 0.0;
        }
        star
    }
}

/// A pixel of the centroid aperture: its position, its background-subtracted
/// value and, once the centroid is known, its squared distance from it.
#[derive(Clone, Copy, Debug)]
struct R2M {
    r2: f64,
    x: i32,
    y: i32,
    m: f64,
}

/// Half-flux radius (star.cpp:83): sort the aperture pixels by distance from
/// the centroid, walk the cumulative mass up to half the total and
/// interpolate linearly in radius between the last two pixels. A lone pixel
/// (a hot pixel) is 0.25, as is an empty list or a zero mass (the `m1 > m0`
/// fallback).
fn hfr(vec: &mut [R2M], cx: f64, cy: f64, mass: f64) -> f64 {
    if vec.len() == 1 {
        return 0.25;
    }

    for p in vec.iter_mut() {
        let dx = p.x as f64 - cx;
        let dy = p.y as f64 - cy;
        p.r2 = dx * dx + dy * dy;
    }
    // std::sort is unstable; the order of equal radii is implementation
    // defined in PHD2 too, so a stable sort (raster order) is as good.
    vec.sort_by(|a, b| a.r2.partial_cmp(&b.r2).unwrap_or(std::cmp::Ordering::Equal));

    let (mut r20, mut r21, mut m0, mut m1) = (0.0, 0.0, 0.0, 0.0);
    let halfm = 0.5 * mass;
    for p in vec.iter() {
        r20 = r21;
        m0 = m1;
        r21 = p.r2;
        m1 += p.m;
        if m1 > halfm {
            break;
        }
    }

    if m1 > m0 {
        let r0 = r20.sqrt();
        let r1 = r21.sqrt();
        let s = (r1 - r0) / (m1 - m0);
        r0 + s * (halfm - m0)
    } else {
        0.25
    }
}

/// The body of `Star::Find` up to its `done:` label: fills the star's
/// `peak_val`, `mass`, `snr` and `hfd` as PHD2 does along the way and returns
/// the new position and the result. Only the search-bounds check can fail
/// (PHD2's `throw ERROR_INFO`), which leaves the position at the base.
fn find_body(
    star: &mut Star,
    img: &UsImage,
    search_region: i32,
    base_x: i32,
    base_y: i32,
    mode: FindMode,
    min_hfd: f64,
    max_hfd: f64,
    max_adu: u32,
) -> (f64, f64, FindResult) {
    let base = (base_x as f64, base_y as f64);

    let (minx, miny, maxx, maxy) = match img.roi() {
        None => (0, 0, img.w - 1, img.h - 1),
        Some(r) => (r.x, r.y, r.right(), r.bottom()),
    };

    // search region bounds
    let mut start_x = (base_x - search_region).max(minx);
    let mut end_x = (base_x + search_region).min(maxx);
    let mut start_y = (base_y - search_region).max(miny);
    let mut end_y = (base_y + search_region).min(maxy);

    if end_x <= start_x || end_y <= start_y {
        return (base.0, base.1, FindResult::Error);
    }

    let rowsize = img.w;
    let px = |x: i32, y: i32| img.data[(y * rowsize + x) as usize];

    let mut peak_x = 0;
    let mut peak_y = 0;
    let mut peak_val: u32 = 0;
    let mut max3 = [0u16; 3];

    if mode == FindMode::Peak {
        for y in start_y..=end_y {
            for x in start_x..=end_x {
                let val = px(x, y) as u32;
                if val > peak_val {
                    peak_val = val;
                    peak_x = x;
                    peak_y = y;
                }
            }
        }
        star.peak_val = peak_val as u16;
    } else {
        // find the peak value within the search region using a smoothing
        // function (the [1 2 1; 2 4 2; 1 2 1] kernel over the interior);
        // max3 keeps the three brightest raw pixels for the saturation test
        for y in start_y + 1..=end_y - 1 {
            for x in start_x + 1..=end_x - 1 {
                let mut p = px(x, y);
                let val = 4 * p as u32
                    + px(x - 1, y - 1) as u32
                    + px(x + 1, y - 1) as u32
                    + px(x - 1, y + 1) as u32
                    + px(x + 1, y + 1) as u32
                    + 2 * px(x, y - 1) as u32
                    + 2 * px(x - 1, y) as u32
                    + 2 * px(x + 1, y) as u32
                    + 2 * px(x, y + 1) as u32;

                if val > peak_val {
                    peak_val = val;
                    peak_x = x;
                    peak_y = y;
                }

                // the three swaps bubble p down, the displaced value with it
                if p > max3[0] {
                    std::mem::swap(&mut p, &mut max3[0]);
                }
                if p > max3[1] {
                    std::mem::swap(&mut p, &mut max3[1]);
                }
                if p > max3[2] {
                    std::mem::swap(&mut p, &mut max3[2]);
                }
            }
        }

        star.peak_val = max3[0]; // raw peak val
        peak_val /= 16; // smoothed peak value
    }

    // measure noise in the annulus with inner radius A and outer radius B
    const A: i32 = 7;
    const B: i32 = 12;
    const A2: i32 = A * A;
    const B2: i32 = B * B;

    // center window around peak value
    start_x = (peak_x - B).max(minx);
    end_x = (peak_x + B).min(maxx);
    start_y = (peak_y - B).max(miny);
    end_y = (peak_y + B).min(maxy);

    // find the mean and stdev of the background: up to 9 passes of ±2σ
    // clipping (Welford), stopping when fewer than 10 points survive or the
    // mean settles within 0.5 ADU
    let mut nbg: u32 = 0;
    let mut mean_bg = 0.0;
    let mut sigma2_bg = 0.0;
    let mut sigma_bg = 0.0;

    for iter in 0..9 {
        let mut sum = 0.0;
        let mut a = 0.0;
        let mut q = 0.0;
        nbg = 0;

        for y in start_y..=end_y {
            let dy = y - peak_y;
            let dy2 = dy * dy;
            for x in start_x..=end_x {
                let dx = x - peak_x;
                let r2 = dx * dx + dy2;

                // exclude points not in annulus
                if r2 <= A2 || r2 > B2 {
                    continue;
                }

                let val = px(x, y) as f64;

                if iter > 0 && (val < mean_bg - 2.0 * sigma_bg || val > mean_bg + 2.0 * sigma_bg) {
                    continue;
                }

                sum += val;
                nbg += 1;
                let k = nbg as f64;
                let a0 = a;
                a += (val - a) / k;
                q += (val - a0) * (val - a);
            }
        }

        if nbg < 10 {
            // too few background points (possible on the first pass too, in
            // a tiny subframe: the mean and sigma then stay 0)
            break;
        }

        let prev_mean_bg = mean_bg;
        mean_bg = sum / nbg as f64;
        sigma2_bg = q / (nbg - 1) as f64;
        sigma_bg = sigma2_bg.sqrt();

        if iter > 0 && (mean_bg - prev_mean_bg).abs() < 0.5 {
            break;
        }
    }

    let thresh: u16;
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut mass = 0.0;
    let n: u32;
    let mut hfrvec: Vec<R2M> = Vec::new();

    if mode == FindMode::Peak {
        mass = peak_val as f64;
        n = 1;
        thresh = 0;
    } else {
        // (unsigned short) of the double: truncation; `as u16` also saturates
        // where PHD2 would overflow
        thresh = (mean_bg + 3.0 * sigma_bg + 0.5) as u16;

        // find pixels over threshold within aperture; compute mass and centroid
        start_x = (peak_x - A).max(minx);
        end_x = (peak_x + A).min(maxx);
        start_y = (peak_y - A).max(miny);
        end_y = (peak_y + A).min(maxy);

        let mut count = 0u32;
        for y in start_y..=end_y {
            let dy = y - peak_y;
            let dy2 = dy * dy;
            if dy2 > A2 {
                continue;
            }
            for x in start_x..=end_x {
                let dx = x - peak_x;

                // exclude points outside aperture
                if dx * dx + dy2 > A2 {
                    continue;
                }

                // exclude points below threshold
                let val = px(x, y);
                if val < thresh {
                    continue;
                }

                let d = val as f64 - mean_bg;

                cx += dx as f64 * d;
                cy += dy as f64 * d;
                mass += d;
                count += 1;

                hfrvec.push(R2M { r2: 0.0, x, y, m: d });
            }
        }
        n = count;
    }

    star.mass = mass;

    // SNR estimate from: Measuring the Signal-to-Noise Ratio S/N of the CCD
    // Image of a Star or Nebula, J.H. Simonetti, 2004 January 8
    let gain = 0.5; // electrons per ADU, nominal
    star.snr = if n > 0 {
        mass / (mass / gain + sigma2_bg * n as f64 * (1.0 + 1.0 / nbg as f64)).sqrt()
    } else {
        0.0
    };

    const LOW_SNR: f64 = 3.0;

    // a few scattered pixels over threshold can give a false positive;
    // avoid this by requiring the smoothed peak value to be above the
    // threshold (quirk: the star then reports snr = 2.9, not its real SNR)
    if peak_val <= thresh as u32 && star.snr >= LOW_SNR {
        star.snr = LOW_SNR - 0.1;
    }

    // LOWMASS is tested before LOWSNR; both leave the position at the base
    if mass < 10.0 {
        star.hfd = 0.0;
        return (base.0, base.1, FindResult::LowMass);
    }

    if star.snr < LOW_SNR {
        star.hfd = 0.0;
        return (base.0, base.1, FindResult::LowSnr);
    }

    let new_x = peak_x as f64 + cx / mass;
    let new_y = peak_y as f64 + cy / mass;

    star.hfd = 2.0 * hfr(&mut hfrvec, new_x, new_y, mass);
    // check for constraints on HFD value
    if mode != FindMode::Peak {
        if star.hfd < min_hfd {
            return (new_x, new_y, FindResult::LowHfd);
        }
        if star.hfd > max_hfd {
            return (new_x, new_y, FindResult::HiHfd);
        }
    }

    // check for saturation

    let mut mx = max3[0] as u32;

    // remove pedestal
    if mx >= img.pedestal as u32 {
        mx -= img.pedestal as u32;
    } else {
        mx = 0; // unlikely
    }

    if max_adu > 0 {
        // maxADU is known
        let result = if mx >= max_adu { FindResult::Saturated } else { FindResult::Ok };
        return (new_x, new_y, result);
    }

    // maxADU not known, use the "flat-top" heuristic: even at saturation the
    // max values may vary a bit due to noise. Call it saturated if the top
    // three values are within 32 parts per 65535 of max for 16-bit cameras,
    // or within 1 part per 191 for 8-bit cameras
    let d = (max3[0] - max3[2]) as u32;

    let saturated = if img.bits_per_pixel < 12 { d * 191 < mx } else { d * 65535 < 32 * mx };

    (new_x, new_y, if saturated { FindResult::Saturated } else { FindResult::Ok })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::rng::Rng;
    use crate::engine::traits::Rect;

    /// A 2-D Gaussian: (x, y, sigma, peak above the background).
    struct Gauss {
        x: f64,
        y: f64,
        sigma: f64,
        peak: f64,
    }

    /// `w × h` frame of `bg` plus Gaussian noise of `noise` ADU (seeded),
    /// plus the stars, clamped to 16 bits.
    fn field(w: i32, h: i32, bg: f64, noise: f64, seed: u64, stars: &[Gauss]) -> UsImage {
        let mut img = UsImage::new(w, h);
        img.bits_per_pixel = 16;
        let mut rng = Rng::new(seed);
        for y in 0..h {
            for x in 0..w {
                let mut v = bg + noise * rng.gaussian();
                for s in stars {
                    let dx = x as f64 - s.x;
                    let dy = y as f64 - s.y;
                    v += s.peak * (-(dx * dx + dy * dy) / (2.0 * s.sigma * s.sigma)).exp();
                }
                *img.pixel_mut(x, y) = v.round().clamp(0.0, 65535.0) as u16;
            }
        }
        img
    }

    fn one_star(peak: f64) -> UsImage {
        field(64, 64, 100.0, 5.0, 7, &[Gauss { x: 31.3, y: 28.7, sigma: 1.5, peak }])
    }

    #[test]
    fn centroid_snr_and_hfd_of_a_gaussian() {
        let img = one_star(2000.0);
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::Ok);
        assert!(s.was_found() && s.is_valid());
        assert!((s.x - 31.3).abs() < 0.05, "x = {}", s.x);
        assert!((s.y - 28.7).abs() < 0.05, "y = {}", s.y);
        assert!(s.snr > 50.0, "snr = {}", s.snr);
        assert!(s.peak_val > 1900 && s.peak_val <= 2110, "peak = {}", s.peak_val);
        // the half-flux radius of a Gaussian is σ·sqrt(2 ln 2), so the HFD
        // is 2 × 1.1774 × 1.5 = 3.532 px
        let hfd_ref = 2.0 * 1.5 * (2.0 * std::f64::consts::LN_2).sqrt();
        assert!((s.hfd - hfd_ref).abs() / hfd_ref < 0.05, "hfd = {} vs {}", s.hfd, hfd_ref);
        // a background-subtracted mass close to the Gaussian's 2πσ²·peak
        let mass_ref = 2.0 * std::f64::consts::PI * 1.5 * 1.5 * 2000.0;
        assert!((s.mass - mass_ref).abs() / mass_ref < 0.05, "mass = {}", s.mass);
    }

    #[test]
    fn peak_mode_takes_the_brightest_raw_pixel() {
        let img = one_star(2000.0);
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Peak, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::Ok);
        // the peak pixel is the one nearest the true centre, mass = raw peak,
        // and the empty HFR list gives the 0.25 fallback (hfd 0.5)
        assert_eq!((s.x, s.y), (31.0, 29.0));
        assert_eq!(s.mass, img.pixel(31, 29) as f64);
        assert_eq!(s.hfd, 0.5);
        // peak mode never tracks max3, so it is never saturated
        let sat = one_star(300000.0);
        let s = Star::find(&sat, 15, 32.0, 28.0, FindMode::Peak, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::Ok);
    }

    #[test]
    fn flat_top_is_saturated_without_a_known_max_adu() {
        let img = one_star(300000.0);
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 20.0, 0);
        assert_eq!(s.result, FindResult::Saturated);
        assert!(s.was_found());
        assert_eq!(s.peak_val, 65535);
        // measurements are kept on a saturated star
        assert!(s.mass > 0.0 && s.snr > 0.0 && s.hfd > 0.0);
        // an 8-bit frame uses the 1/191 rule: the same flat top is saturated
        let mut img8 = img.clone();
        img8.bits_per_pixel = 8;
        assert_eq!(Star::find(&img8, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 20.0, 0).result, FindResult::Saturated);
    }

    #[test]
    fn known_max_adu_rule_and_pedestal() {
        let img = one_star(2000.0);
        // peak ~2100: saturated once max_adu is at or below it
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 2000);
        assert_eq!(s.result, FindResult::Saturated);
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 60000);
        assert_eq!(s.result, FindResult::Ok);
        // the pedestal is removed before the comparison
        let mut ped = img.clone();
        ped.pedestal = 200;
        let s = Star::find(&ped, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 2000);
        assert_eq!(s.result, FindResult::Ok);
    }

    #[test]
    fn low_mass_before_low_snr() {
        // a noise-free background with a single pixel 5 ADU above it: the
        // threshold is the background itself, mass = 5
        let mut img = UsImage::new(64, 64);
        img.bits_per_pixel = 16;
        img.data.fill(100);
        *img.pixel_mut(30, 30) = 105;
        let s = Star::find(&img, 15, 30.0, 30.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::LowMass);
        assert_eq!(s.mass, 5.0);
        assert_eq!(s.hfd, 0.0);
        assert!(!s.was_found());
        // the position stays at the base, and the base is truncated like
        // PHD2's int parameters
        assert_eq!((s.x, s.y), (30.0, 30.0));
        let s = Star::find(&img, 15, 30.9, 30.9, FindMode::Centroid, 1.5, 10.0, 0);
        assert_eq!((s.x, s.y), (30.0, 30.0));
    }

    #[test]
    fn no_star_at_the_position() {
        let img = field(64, 64, 100.0, 5.0, 11, &[]);
        let s = Star::find(&img, 15, 32.0, 32.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert!(matches!(s.result, FindResult::LowSnr | FindResult::LowMass), "{:?}", s.result);
        assert!(!s.was_found());
        assert_eq!(s.hfd, 0.0);
        // a far-off base against a 64 px frame: nothing to search
        let s = Star::find(&img, 15, 200.0, 200.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::Error);
        assert!(s.is_valid());
        assert_eq!((s.x, s.y, s.mass, s.snr, s.hfd), (200.0, 200.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn hot_pixel_hfr_is_a_quarter_pixel() {
        let mut img = field(64, 64, 100.0, 5.0, 3, &[]);
        *img.pixel_mut(40, 20) = 2000;
        let s = Star::find(&img, 15, 40.0, 20.0, FindMode::Centroid, 1.5, 10.0, 0);
        // one pixel over the threshold: hfr 0.25, hfd 0.5, under the 1.5 floor
        assert_eq!(s.hfd, 0.5);
        assert_eq!(s.result, FindResult::LowHfd);
        assert_eq!((s.x, s.y), (40.0, 20.0));
        let s = Star::find(&img, 15, 40.0, 20.0, FindMode::Centroid, 0.1, 10.0, 0);
        assert_eq!(s.result, FindResult::Ok);
        // and a bloated star is rejected as HiHfd
        let big = field(64, 64, 100.0, 5.0, 3, &[Gauss { x: 32.0, y: 32.0, sigma: 3.0, peak: 2000.0 }]);
        let s = Star::find(&big, 15, 32.0, 32.0, FindMode::Centroid, 1.5, 5.0, 0);
        assert_eq!(s.result, FindResult::HiHfd);
        assert!(s.hfd > 5.0);
    }

    #[test]
    fn false_star_guard_reports_snr_2_9() {
        // a noise-free frame: sigma 0, so thresh = (100 + 0.5) as u16 = 100
        // and every aperture pixel is "over threshold". Four isolated dots of
        // 101 give mass 4: LowMass is tested first.
        let mut img = UsImage::new(64, 64);
        img.bits_per_pixel = 16;
        img.data.fill(100);
        for (x, y) in [(30, 30), (34, 30), (30, 34), (34, 34)] {
            *img.pixel_mut(x, y) = 101;
        }
        let s = Star::find(&img, 15, 32.0, 32.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::LowMass);
        assert_eq!(s.mass, 4.0);

        // dots of 130 (about 6σ over a noisy background) 3 px apart around
        // the smoothed peak: no 3×3 window holds two of them, so the smoothed
        // peak, about (4·130 + 12·100)/16 = 107, stays under the threshold
        // (about 115) while the scattered mass gives a real SNR over 3. The
        // guard pins the SNR at 2.9 and the star is lost as LowSnr.
        let mut img = field(64, 64, 100.0, 5.0, 5, &[]);
        let s0 = Star::find(&img, 15, 32.0, 32.0, FindMode::Centroid, 1.5, 10.0, 0);
        let (px, py) = (s0.x as i32, s0.y as i32);
        for dx in [-6, -3, 0, 3, 6] {
            for dy in [-6, -3, 0, 3, 6] {
                if dx * dx + dy * dy <= 49 {
                    *img.pixel_mut(px + dx, py + dy) = 130;
                }
            }
        }
        let s = Star::find(&img, 15, 32.0, 32.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert_eq!(s.snr, 2.9);
        assert_eq!(s.result, FindResult::LowSnr);
    }

    #[test]
    fn subframe_bounds_and_tiny_regions() {
        let mut img = one_star(2000.0);
        // a subframe that ends before the search box starts (x 17..=14):
        // nothing to search
        img.subframe = Some(Rect::new(0, 0, 15, 15));
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::Error);
        // one that clips the box to its corner (x 17..=19, y 13..=19) away
        // from the star: searched, nothing found
        img.subframe = Some(Rect::new(0, 0, 20, 20));
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert!(!s.was_found() && s.result != FindResult::Error, "{:?}", s.result);
        // a subframe around it works, with the annulus clipped
        img.subframe = Some(Rect::new(24, 22, 16, 14));
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::Ok);
        assert!((s.x - 31.3).abs() < 0.1 && (s.y - 28.7).abs() < 0.1);
        // a search region of 0 is an empty range
        img.subframe = None;
        assert_eq!(Star::find(&img, 0, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 0).result, FindResult::Error);
    }

    #[test]
    fn hfr_formula() {
        // four pixels of mass 1 at r = 1 around (0, 0) and a centre of 6:
        // mass 10, half 5; cumulative 6 at r 0 > 5 → m1 = 6, m0 = 0, r0 = r1 = 0
        let mut v = vec![
            R2M { r2: 0.0, x: 0, y: 0, m: 6.0 },
            R2M { r2: 0.0, x: 1, y: 0, m: 1.0 },
            R2M { r2: 0.0, x: -1, y: 0, m: 1.0 },
            R2M { r2: 0.0, x: 0, y: 1, m: 1.0 },
            R2M { r2: 0.0, x: 0, y: -1, m: 1.0 },
        ];
        assert_eq!(hfr(&mut v, 0.0, 0.0, 10.0), 0.0);
        // centre 4, ring 4×1: half 4; cumulative 4 at r 0 (not > 4), 5 at r 1
        // → interpolate between (0, 4) and (1, 5): 0 + 1·(4 − 4) = 0
        v[0].m = 4.0;
        assert_eq!(hfr(&mut v, 0.0, 0.0, 8.0), 0.0);
        // centre 2, ring 4×2: half 5; cumulative 2 at r 0, 4 at r 1, 6 at r 1
        // → m0 = 4, m1 = 6, r0 = r1 = 1 → 1
        v[0].m = 2.0;
        for p in &mut v[1..] {
            p.m = 2.0;
        }
        assert_eq!(hfr(&mut v, 0.0, 0.0, 10.0), 1.0);
        // one pixel: 0.25; none: the fallback 0.25
        assert_eq!(hfr(&mut v[..1], 0.0, 0.0, 2.0), 0.25);
        assert_eq!(hfr(&mut [], 0.0, 0.0, 0.0), 0.25);
    }

    #[test]
    fn status_strings_and_codes() {
        let mut s = Star::new();
        assert!(!s.is_valid());
        assert_eq!(s.status_str(), "No star selected");
        assert_eq!(s.result.error_code(), 8);
        s.set_xy(1.0, 2.0);
        assert_eq!(s.status_str(), "No star found");
        s.set_error(FindResult::LowSnr);
        assert_eq!(s.status_str(), "Star lost - low SNR");
        assert_eq!(FindResult::LowMass.status_str(), "Star lost - low mass");
        assert_eq!(FindResult::LowHfd.status_str(), "Star lost - low HFD");
        assert_eq!(FindResult::TooNearEdge.status_str(), "Star too near edge");
        assert_eq!(FindResult::MassChange.status_str(), "Star lost - mass changed");
        assert_eq!(FindResult::Saturated.status_str(), "No star found");
        assert_eq!(FindResult::Saturated.error_code(), 1);
        assert_eq!(FindResult::HiHfd.error_code(), 5);
        assert!(FindResult::Saturated.was_found() && !FindResult::LowHfd.was_found());
        s.invalidate();
        assert!(!s.is_valid() && s.result == FindResult::Error && (s.x, s.y) == (1.0, 2.0));
    }
}
