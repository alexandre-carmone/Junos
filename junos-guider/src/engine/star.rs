//! Star detection: the centroid / peak finder every guide frame goes through
//! (`Star::Find`), its half-flux diameter and its saturation tests, the
//! status strings the event server reports for a lost star, and the guide
//! star auto-selection (`GuideStar::AutoFind`).
//!
//! `find` is a step-for-step port: same search bounds, same 3×3 smoothing
//! kernel, same annulus background with its sigma clipping, same SNR
//! formula, same rejection order and the same quirks (the `snr = 2.9`
//! false-star guard, the single-pixel HFR of 0.25). `auto_find` keeps
//! PHD2's filters, `f32` arithmetic, pruning order and three-pass pick,
//! including its quirks (a peak scoring exactly like a listed one is
//! dropped, secondary offsets run from the primary's integer peak). Nothing
//! here looks at the configuration: every PHD2 setting is a parameter.
//!
//! Ported from PHD2 src/star.h, src/star.cpp, src/guider_multistar.cpp (the status strings) — BSD-3-Clause, see THIRD_PARTY.md

use super::image::{UsImage, median3};
use super::traits::{Point, Rect};

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
        // (unsigned short) of the double: g++ on x86-64 truncates it to a
        // 32-bit int (cvttsd2si) and keeps the low 16 bits, so a threshold
        // over 65535 (a background near saturation) wraps, e.g. 70000.3 →
        // 4464, and lets almost the whole aperture through. Kept: it changes
        // mass, SNR and HFD. The value stays far below 2^31, so the i32 step
        // never saturates.
        thresh = ((mean_bg + 3.0 * sigma_bg + 0.5) as i32) as u16;

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

/// PHD2's `GuideStar`: a star of the multi-star list with the state the
/// guider tracks it by.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GuideStar {
    pub star: Star,
    /// Where the star was when it joined the list; its offsets are measured
    /// from here.
    pub reference_point: Point,
    /// Frames where the star moved unlike the primary.
    pub miss_count: u32,
    /// Frames with an offset of exactly zero on an axis (a hot pixel).
    pub zero_count: u32,
    /// Offset from the primary star, set by `auto_find` and used to re-find
    /// the star after a dither.
    pub offset_from_primary: Point,
    pub was_lost: bool,
}

/// What `GuideStar::AutoFind` reads from the profile, the guider and the
/// camera.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoFindParams {
    /// The downsample factor (≥ 1), already resolved from
    /// `/guider/AutoSelDownsample` by [`auto_sel_downsample`].
    pub downsample: i32,
    /// `/guider/StarMinHFD`.
    pub min_hfd: f64,
    /// `/guider/StarMaxHFD`.
    pub max_hfd: f64,
    /// `/guider/StarMinSNR` (`GetAFMinStarSNR`, 6 by default).
    pub min_snr: f64,
    /// `/camera/SaturationByADU` (true by default).
    pub saturation_by_adu: bool,
    /// `/camera/SaturationADU`, 0 while unset.
    pub saturation_adu: u16,
}

/// What `GuideStar::AutoFind` gives back besides the star list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoFindResult {
    /// The chosen star, `None` when nothing was found (PHD2 returns false).
    /// It is the convolution peak mapped back onto the frame, in whole
    /// pixels, not a centroid: AutoSelect runs [`Star::find`] there next.
    pub position: Option<Point>,
    /// `Some(level)` when saturation is by ADU with no level set: PHD2 then
    /// sets the camera's level to `(1 << bpp) − 1` (star.cpp:964,
    /// `SetSaturationByADU(true, level)`), even when no star is found. The
    /// caller saves it to `/camera/SaturationADU`.
    pub saturation_adu_set: Option<u16>,
}

/// `/guider/AutoSelDownsample` resolved as AutoFind does (star.cpp:756):
/// 0 is "Auto", which downsamples by 2 when the pixel scale is 0.6″/px or
/// finer (an unknown scale is 1.0, so no downsampling).
pub fn auto_sel_downsample(setting: u32, pixel_scale: f64) -> i32 {
    const DOWNSAMPLE_SCALE_THRESH: f64 = 0.6;
    if setting == 0 {
        if pixel_scale > DOWNSAMPLE_SCALE_THRESH { 1 } else { 2 }
    } else {
        setting as i32
    }
}

impl GuideStar {
    /// `GuideStar(const Star&)`: the reference point starts at the star.
    pub fn from_star(star: Star) -> Self {
        GuideStar { reference_point: star.position(), star, ..Default::default() }
    }

    /// `GuideStar::AutoFind` (star.cpp:718): pick the best guide star of a
    /// full frame and, when `max_stars > 1`, the list of stars for
    /// multi-star guiding.
    ///
    /// The frame is median-filtered (outside `roi` blanked), downsampled,
    /// convolved with a 9×9 star template; its local maxima are scored
    /// against the global noise, the 100 best kept, then pruned: peaks under
    /// 5 px apart merge, two peaks in one search box both go unless one is
    /// 5× the other, peaks within `search_region + extra_edge_allowance` of
    /// the edge go. Each survivor is measured with [`Star::find`], brightest
    /// first, in three passes: under 90 % of saturation with the minimum
    /// SNR, then unsaturated with the minimum SNR, then anything found.
    ///
    /// `found_stars` is PHD2's out-parameter: left untouched when the frame
    /// is subframed or the ROI is smaller than the search region, cleared
    /// otherwise, then filled with the primary first and the secondaries
    /// (SNR ≥ `min_snr`, 25 px apart, carrying `offset_from_primary`),
    /// at most `max_stars` of them.
    pub fn auto_find(
        image: &UsImage,
        extra_edge_allowance: i32,
        search_region: i32,
        roi: Option<Rect>,
        found_stars: &mut Vec<GuideStar>,
        max_stars: usize,
        p: &AutoFindParams,
    ) -> AutoFindResult {
        let mut out = AutoFindResult { position: None, saturation_adu_set: None };

        if image.roi().is_some() {
            // AutoFind called on subframe
            return out;
        }

        // run a 3x3 median first to eliminate hot pixels; with an ROI the
        // subframe makes Median3 blank everything outside it
        let mut smoothed = image.clone();
        if let Some(roi) = roi.filter(|r| !r.is_empty()) {
            let r = roi.intersect(&Rect::new(0, 0, image.w, image.h));
            if r.w < search_region || r.h < search_region {
                return out; // bad ROI
            }
            smoothed.subframe = Some(r);
        }
        median3(&mut smoothed);

        // convert to floating point, downsample, run the PSF convolution
        let mut conv = FloatImg {
            px: smoothed.data.iter().map(|&v| v as f32).collect(),
            w: smoothed.w,
            h: smoothed.h,
        };
        let downsample = p.downsample;
        if downsample > 1 {
            conv = downsample_img(&conv, downsample);
        }
        conv = psf_conv(&conv);

        const CONV_RADIUS: i32 = 4;
        let dw = conv.w; // width of the downsampled image
        let dh = conv.h; // height of the downsampled image
        // region containing valid data
        let conv_rect = Rect::new(CONV_RADIUS, CONV_RADIUS, dw - 2 * CONV_RADIUS, dh - 2 * CONV_RADIUS);

        const TOP_N: usize = 100; // keep track of the brightest stars
        let mut stars: Vec<Peak> = Vec::new(); // ascending intensity, as std::set<Peak>

        let (_, global_stdev) = get_stats(&conv, conv_rect);

        const THRESHOLD: f64 = 0.1;

        // find each local maximum
        const SRCH: i32 = 4;
        for y in conv_rect.y + SRCH..=conv_rect.bottom() - SRCH {
            for x in conv_rect.x + SRCH..=conv_rect.right() - SRCH {
                let val = conv.at(x, y);
                if !(val > 0.0) {
                    continue;
                }
                let mut ismax = true;
                'window: for j in -SRCH..=SRCH {
                    for i in -SRCH..=SRCH {
                        if i == 0 && j == 0 {
                            continue;
                        }
                        if conv.at(x + i, y + j) > val {
                            ismax = false;
                            break 'window;
                        }
                    }
                }
                if !ismax {
                    continue;
                }

                // compare local maximum to mean value of surrounding pixels
                const LOCAL: i32 = 7;
                let local_rect = Rect::new(x - LOCAL, y - LOCAL, 2 * LOCAL + 1, 2 * LOCAL + 1).intersect(&conv_rect);
                let (local_mean, _) = get_stats(&conv, local_rect);

                // this is our measure of star intensity
                let h = (val as f64 - local_mean) / global_stdev;
                if h < THRESHOLD {
                    continue;
                }

                // coordinates on the original image
                let imgx = x * downsample + downsample / 2;
                let imgy = y * downsample + downsample / 2;

                insert_peak(&mut stars, Peak { x: imgx, y: imgy, val: h as f32 });
                if stars.len() > TOP_N {
                    stars.remove(0);
                }
            }
        }

        // merge stars that are very close into a single star: erase the
        // dimmer one and start over
        const MIN_LIMIT_SQ: i32 = 5 * 5;
        'repeat: loop {
            for a in 0..stars.len() {
                for b in a + 1..stars.len() {
                    let dx = stars[a].x - stars[b].x;
                    let dy = stars[a].y - stars[b].y;
                    if dx * dx + dy * dy < MIN_LIMIT_SQ {
                        stars.remove(a);
                        continue 'repeat;
                    }
                }
            }
            break;
        }

        // exclude stars that would fit within a single searchRegion box,
        // both of them, but do not let a very dim star eliminate a very
        // bright one (b is the brighter: the list is ascending)
        {
            const EXTRA: i32 = 5; // extra safety margin
            let fullw = search_region + EXTRA;
            let mut to_erase = vec![false; stars.len()];
            for a in 0..stars.len() {
                for b in a + 1..stars.len() {
                    let dx = (stars[a].x - stars[b].x).abs();
                    let dy = (stars[a].y - stars[b].y).abs();
                    if dx <= fullw && dy <= fullw && !(stars[b].val / stars[a].val >= 5.0) {
                        to_erase[a] = true;
                        to_erase[b] = true;
                    }
                }
            }
            let mut i = 0;
            stars.retain(|_| {
                i += 1;
                !to_erase[i - 1]
            });
        }

        // exclude stars too close to the edge
        let edge_dist = search_region + extra_edge_allowance;
        stars.retain(|s| {
            !(s.x <= edge_dist || s.x >= image.w - edge_dist || s.y <= edge_dist || s.y >= image.h - edge_dist)
        });

        // the saturation level, pedestal included, and the ADU limit the
        // finds below get (`pCamera->GetSaturationADU()`, 0 when saturation
        // is not by ADU)
        let max_adu: u16;
        let sat_level: u32;
        if p.saturation_by_adu {
            let mut sat_adu = p.saturation_adu;
            if sat_adu == 0 {
                // for cameras that don't scale their data to the full 16
                // bits this is too high; the user will have to fix it
                sat_adu = ((1u32 << image.bits_per_pixel) - 1) as u16;
                out.saturation_adu_set = Some(sat_adu);
            }
            max_adu = sat_adu;
            sat_level = sat_adu as u32 + image.pedestal as u32;
        } else {
            max_adu = 0;
            // the peak pixel overall, then look for a star with a flat top
            // at that level (a flat top below it is a false positive)
            let max_val = image.data.iter().copied().max().unwrap_or(0);
            let found_saturated = stars.iter().rev().any(|s| {
                let tmp = Star::find(image, search_region, s.x as f64, s.y as f64, FindMode::Centroid, p.min_hfd, p.max_hfd, 0);
                tmp.was_found() && tmp.result == FindResult::Saturated && (max_val - tmp.peak_val) as u32 * 255 <= max_val as u32
            });
            sat_level = if found_saturated {
                max_val as u32
            } else {
                // can't tell whether the max is saturated
                ((1u32 << image.bits_per_pixel) - 1) + image.pedestal as u32
            };
        }

        let range = sat_level.saturating_sub(image.pedestal as u32);
        // "near-saturation" threshold at 90% saturation
        let sat_thresh = (image.pedestal as u32 + 9 * range / 10).min(65535) as u16;

        let find = |s: &Peak| {
            Star::find(image, search_region, s.x as f64, s.y as f64, FindMode::Centroid, p.min_hfd, p.max_hfd, max_adu as u32)
        };

        // before sifting for the best star, collect all the viable
        // candidates; a repeated find can land on the same star, so drop
        // the ones within 25 px of a listed one
        found_stars.clear();
        if max_stars > 1 {
            for s in stars.iter().rev() {
                let tmp = find(s);
                if tmp.was_found() && tmp.snr >= p.min_snr {
                    let duplicate = found_stars.iter().any(|o| o.star.position().distance(&tmp.position()) < 25.0);
                    if !duplicate {
                        found_stars.push(GuideStar::from_star(tmp));
                    }
                }
            }
        }

        // final star selection: the only star, or the primary in multi-star
        // mode. Pass 1 takes the brightest star under 90 % saturation with
        // SNR ≥ min, pass 2 the brightest unsaturated one with SNR ≥ min,
        // pass 3 the brightest star found at all.
        for pass in 1..=3 {
            for s in stars.iter().rev() {
                let tmp = find(s);
                if !tmp.was_found() {
                    continue;
                }
                let saturated_or_dim = tmp.result == FindResult::Saturated || tmp.snr < p.min_snr;
                if (pass == 1 && (tmp.peak_val > sat_thresh || saturated_or_dim)) || (pass == 2 && saturated_or_dim) {
                    continue;
                }

                // star accepted, at the convolution peak
                let primary_ref = Point::new(s.x as f64, s.y as f64);
                out.position = Some(primary_ref);

                if max_stars > 1 {
                    // find the chosen star in the list (same centroid) and
                    // set every other star's offset — from the peak, not
                    // from the primary's centroid, as PHD2 does
                    let mut primary_loc = None;
                    for (i, gs) in found_stars.iter_mut().enumerate() {
                        if gs.star.x == tmp.x && gs.star.y == tmp.y {
                            primary_loc = Some(i);
                        } else {
                            gs.offset_from_primary = gs.reference_point - primary_ref;
                        }
                    }
                    match primary_loc {
                        Some(i) => {
                            // the stars ahead of it are likely saturated or
                            // otherwise flawed
                            found_stars.drain(..i);
                            found_stars.truncate(max_stars);
                        }
                        None => {
                            // the secondaries are presumably degraded: the
                            // primary alone
                            found_stars.clear();
                            found_stars.push(GuideStar::from_star(tmp));
                        }
                    }
                } else {
                    found_stars.push(GuideStar::from_star(tmp));
                }
                return out;
            }
        }

        out
    }
}

/// star.cpp's `FloatImg`: the frame as floats for the AutoFind filters.
struct FloatImg {
    px: Vec<f32>,
    w: i32,
    h: i32,
}

impl FloatImg {
    fn at(&self, x: i32, y: i32) -> f32 {
        self.px[(y * self.w + x) as usize]
    }
}

/// A local maximum of the convolved image: its position on the original
/// frame and its score.
#[derive(Clone, Copy, Debug)]
struct Peak {
    x: i32,
    y: i32,
    val: f32,
}

/// `std::set<Peak>::insert` with `Peak::operator<` comparing `val` only:
/// the list stays sorted ascending, and a peak whose score equals one
/// already listed is dropped, as the set does.
fn insert_peak(stars: &mut Vec<Peak>, p: Peak) {
    match stars.binary_search_by(|q| q.val.partial_cmp(&p.val).unwrap_or(std::cmp::Ordering::Equal)) {
        Ok(_) => {}
        Err(i) => stars.insert(i, p),
    }
}

/// `GetStats` (star.cpp:515): mean and population standard deviation of the
/// pixels in `win` (Welford). An empty window gives NaN, as in PHD2.
fn get_stats(img: &FloatImg, win: Rect) -> (f64, f64) {
    let (mut sum, mut a, mut q, mut k, mut km1) = (0.0, 0.0, 0.0, 1.0, 0.0);
    for y in 0..win.h {
        for x in 0..win.w {
            let v = img.at(win.x + x, win.y + y) as f64;
            sum += v;
            let a0 = a;
            a += (v - a) / k;
            q += (v - a0) * (v - a);
            km1 = k;
            k += 1.0;
        }
    }
    (sum / km1, (q / km1).sqrt())
}

/// `psf_conv` (star.cpp:574): correlate with a 9×9 star template, each ring
/// of the template taken relative to the 9×9 mean. The rings are summed in
/// `f32` in PHD2's order; the 4 px border stays 0.
///
/// ```text
/// D3 D3 D3 D3 D3 D3 D3 D3 D3
/// D3 D3 D3 D2 D1 D2 D3 D3 D3
/// D3 D3 C3 C2 C1 C2 C3 D3 D3
/// D3 D2 C2 B2 B1 B2 C2 D2 D3
/// D3 D1 C1 B1 A  B1 C1 D1 D3      1 A; 4 B1, B2, C1, C3, D1;
/// D3 D2 C2 B2 B1 B2 C2 D2 D3      8 C2, D2; 44 D3
/// D3 D3 C3 C2 C1 C2 C3 D3 D3
/// D3 D3 D3 D2 D1 D2 D3 D3 D3
/// D3 D3 D3 D3 D3 D3 D3 D3 D3
/// ```
fn psf_conv(src: &FloatImg) -> FloatImg {
    //                A      B1     B2     C1    C2    C3     D1     D2     D3
    const PSF: [f64; 9] = [0.906, 0.584, 0.365, 0.117, 0.049, -0.05, -0.064, -0.074, -0.094];
    const PSF_SIZE: i32 = 4;

    let (width, height) = (src.w, src.h);
    let mut dst = FloatImg { px: vec![0.0; src.px.len()], w: width, h: height };

    for y in PSF_SIZE..height - PSF_SIZE {
        for x in PSF_SIZE..width - PSF_SIZE {
            let px = |dx: i32, dy: i32| src.at(x + dx, y + dy);

            let a = px(0, 0);
            let b1 = px(0, -1) + px(0, 1) + px(1, 0) + px(-1, 0);
            let b2 = px(-1, -1) + px(1, -1) + px(-1, 1) + px(1, 1);
            let c1 = px(0, -2) + px(-2, 0) + px(2, 0) + px(0, 2);
            let c2 = px(-1, -2) + px(1, -2) + px(-2, -1) + px(2, -1) + px(-2, 1) + px(2, 1) + px(-1, 2) + px(1, 2);
            let c3 = px(-2, -2) + px(2, -2) + px(-2, 2) + px(2, 2);
            let d1 = px(0, -3) + px(-3, 0) + px(3, 0) + px(0, 3);
            let d2 = px(-1, -3) + px(1, -3) + px(-3, -1) + px(3, -1) + px(-3, 1) + px(3, 1) + px(-1, 3) + px(1, 3);
            let mut d3 = px(-4, -2)
                + px(-3, -2)
                + px(3, -2)
                + px(4, -2)
                + px(-4, -1)
                + px(4, -1)
                + px(-4, 0)
                + px(4, 0)
                + px(-4, 1)
                + px(4, 1)
                + px(-4, 2)
                + px(-3, 2)
                + px(3, 2)
                + px(4, 2);
            // rows −4 and +4 whole, rows −3 and +3 but their middle three
            for i in -4..=4 {
                d3 += px(i, -4);
            }
            for i in [-4, -3, -2, 2, 3, 4] {
                d3 += px(i, -3);
            }
            for i in [-4, -3, -2, 2, 3, 4] {
                d3 += px(i, 3);
            }
            for i in -4..=4 {
                d3 += px(i, 4);
            }

            let mean = (a + b1 + b2 + c1 + c2 + c3 + d1 + d2 + d3) as f64 / 81.0;
            let psf_fit = PSF[0] * (a as f64 - mean)
                + PSF[1] * (b1 as f64 - 4.0 * mean)
                + PSF[2] * (b2 as f64 - 4.0 * mean)
                + PSF[3] * (c1 as f64 - 4.0 * mean)
                + PSF[4] * (c2 as f64 - 8.0 * mean)
                + PSF[5] * (c3 as f64 - 4.0 * mean)
                + PSF[6] * (d1 as f64 - 4.0 * mean)
                + PSF[7] * (d2 as f64 - 8.0 * mean)
                + PSF[8] * (d3 as f64 - 44.0 * mean);

            dst.px[(width * y + x) as usize] = psf_fit as f32;
        }
    }
    dst
}

/// `Downsample` (star.cpp:657): the mean of each `ds × ds` block, summed in
/// `f32`; a partial block at the right or bottom edge is dropped.
fn downsample_img(src: &FloatImg, ds: i32) -> FloatImg {
    let dw = src.w / ds;
    let dh = src.h / ds;
    let d2 = (ds * ds) as f32;
    let mut px = vec![0.0f32; (dw * dh) as usize];
    for yy in 0..dh {
        for xx in 0..dw {
            let mut sum = 0.0f32;
            for j in 0..ds {
                for i in 0..ds {
                    sum += src.at(xx * ds + i, yy * ds + j);
                }
            }
            px[(yy * dw + xx) as usize] = sum / d2;
        }
    }
    FloatImg { px, w: dw, h: dh }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::rng::Rng;

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

    /// A star's (x, y, mass, snr, hfd, peak_val, result), to compare with
    /// PHD2's C++ output bit for bit.
    type Measured = (f64, f64, f64, f64, f64, u16, FindResult);

    fn measured(s: &Star) -> Measured {
        (s.x, s.y, s.mass, s.snr, s.hfd, s.peak_val, s.result)
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
        // and bit for bit what PHD2's C++ (star.cpp built with g++ on
        // x86-64) measures on this frame
        let phd2 = (31.288408279461297, 28.700901793505544, 28187.633093525128, 117.33271568951893, 3.4709251644221015, 2015, FindResult::Ok);
        assert_eq!(measured(&s), phd2);
    }

    #[test]
    fn peak_mode_takes_the_brightest_raw_pixel() {
        let img = one_star(2000.0);
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Peak, 1.5, 10.0, 0);
        // the peak pixel is the one nearest the true centre, mass = raw peak,
        // and the empty HFR list gives the 0.25 fallback (hfd 0.5); the
        // values are PHD2's C++ output on this frame
        assert_eq!(s.mass, img.pixel(31, 29) as f64);
        assert_eq!(measured(&s), (31.0, 29.0, 2015.0, 31.672907574681986, 0.5, 2015, FindResult::Ok));
        // peak mode never tracks max3, so it is never saturated
        let sat = one_star(300000.0);
        let s = Star::find(&sat, 15, 32.0, 28.0, FindMode::Peak, 1.5, 10.0, 0);
        assert_eq!(s.result, FindResult::Ok);
    }

    #[test]
    fn flat_top_is_saturated_without_a_known_max_adu() {
        let img = one_star(300000.0);
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 20.0, 0);
        assert!(s.was_found());
        // measurements are kept on a saturated star (PHD2's C++ output)
        let phd2 = (31.29664428944004, 28.702552138539144, 2337304.614814811, 1080.7264630398697, 4.779520762775285, 65535, FindResult::Saturated);
        assert_eq!(measured(&s), phd2);
        // an 8-bit frame uses the 1/191 rule: the same flat top is saturated
        let mut img8 = img.clone();
        img8.bits_per_pixel = 8;
        assert_eq!(Star::find(&img8, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 20.0, 0).result, FindResult::Saturated);
    }

    #[test]
    fn known_max_adu_rule_and_pedestal() {
        let img = one_star(2000.0);
        // peak 2015: saturated once max_adu is at or below it, with the Ok
        // measurements kept (PHD2's C++ output)
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 2000);
        let phd2 = (31.288408279461297, 28.700901793505544, 28187.633093525128, 117.33271568951893, 3.4709251644221015, 2015, FindResult::Saturated);
        assert_eq!(measured(&s), phd2);
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 60000);
        assert_eq!(s.result, FindResult::Ok);
        // the pedestal is removed before the comparison
        let mut ped = img.clone();
        ped.pedestal = 200;
        let s = Star::find(&ped, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 10.0, 2000);
        assert_eq!(s.result, FindResult::Ok);
    }

    #[test]
    fn threshold_wraps_near_saturation() {
        // a sky at 60000 ± 3000: mean + 3σ + 0.5 is over 65535, and the
        // wrapped threshold lets the whole aperture in. The values are PHD2's
        // C++ (star.cpp built with g++ on x86-64) on this frame; a threshold
        // capped at 65535 would give mass 53037 and the 2.9 SNR guard.
        let img = field(64, 64, 60000.0, 3000.0, 13, &[Gauss { x: 31.3, y: 28.7, sigma: 1.5, peak: 5000.0 }]);
        let s = Star::find(&img, 15, 32.0, 28.0, FindMode::Centroid, 1.5, 20.0, 0);
        assert_eq!(measured(&s), (32.0, 28.0, 18760.978417266655, 0.5725783353898136, 0.0, 65535, FindResult::LowSnr));
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

    #[test]
    fn auto_sel_downsample_resolution() {
        // "Auto" downsamples by 2 at 0.6"/px or finer; an unknown scale is 1.0
        assert_eq!(auto_sel_downsample(0, 1.0), 1);
        assert_eq!(auto_sel_downsample(0, 0.61), 1);
        assert_eq!(auto_sel_downsample(0, 0.6), 2);
        assert_eq!(auto_sel_downsample(0, 0.3), 2);
        assert_eq!(auto_sel_downsample(3, 0.3), 3);
        assert_eq!(auto_sel_downsample(1, 0.3), 1);
    }

    fn float_img(w: i32, h: i32, f: impl Fn(i32, i32) -> f32) -> FloatImg {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.push(f(x, y));
            }
        }
        FloatImg { px, w, h }
    }

    #[test]
    fn psf_template_coefficients() {
        // the template weights sum to 0.378 (1·0.906 + 4·0.584 + 4·0.365 +
        // 4·0.117 + 8·0.049 − 4·0.05 − 4·0.064 − 8·0.074 − 44·0.094); a unit
        // impulse adds 1/81 to the mean, so each response is the weight of
        // the impulse's ring minus 0.378/81
        let delta = float_img(17, 17, |x, y| if (x, y) == (8, 8) { 1.0 } else { 0.0 });
        let c = psf_conv(&delta);
        let k = 0.378 / 81.0;
        let at = |x: i32, y: i32| c.at(x, y) as f64;
        assert!((at(8, 8) - (0.906 - k)).abs() < 1e-6, "A {}", at(8, 8));
        assert!((at(9, 8) - (0.584 - k)).abs() < 1e-6, "B1 {}", at(9, 8));
        assert!((at(9, 9) - (0.365 - k)).abs() < 1e-6, "B2 {}", at(9, 9));
        assert!((at(8, 10) - (0.117 - k)).abs() < 1e-6, "C1 {}", at(8, 10));
        assert!((at(9, 10) - (0.049 - k)).abs() < 1e-6, "C2 {}", at(9, 10));
        assert!((at(10, 10) - (-0.05 - k)).abs() < 1e-6, "C3 {}", at(10, 10));
        assert!((at(5, 8) - (-0.064 - k)).abs() < 1e-6, "D1 {}", at(5, 8));
        assert!((at(7, 11) - (-0.074 - k)).abs() < 1e-6, "D2 {}", at(7, 11));
        // D3: the corners, the rows ±4 and the ends of the rows ±3
        for (x, y) in [(4, 4), (12, 12), (12, 8), (6, 11), (4, 5)] {
            assert!((at(x, y) - (-0.094 - k)).abs() < 1e-6, "D3 ({x},{y}) {}", at(x, y));
        }
        // the 4 px border is left at 0
        assert_eq!((at(3, 8), at(13, 8), at(8, 3), at(8, 13)), (0.0, 0.0, 0.0, 0.0));
        // a flat background vanishes: each ring minus its share of the mean
        let flat = psf_conv(&float_img(12, 12, |_, _| 100.0));
        assert!(flat.px.iter().all(|&v| v == 0.0));
        // too small for the template: all 0, no panic
        assert!(psf_conv(&float_img(8, 8, |x, _| x as f32)).px.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn downsample_and_stats() {
        // 5×4 ramp v = x + 5y by 2: the 2×2 block means, the odd column dropped
        let d = downsample_img(&float_img(5, 4, |x, y| (x + 5 * y) as f32), 2);
        assert_eq!((d.w, d.h), (2, 2));
        assert_eq!(d.px, vec![3.0, 5.0, 13.0, 15.0]);
        // mean and population sigma (q / n), over a window only
        let img = float_img(3, 3, |x, y| if x < 2 && y < 2 { (1 + x + 2 * y) as f32 } else { 50.0 });
        let (mean, sd) = get_stats(&img, Rect::new(0, 0, 2, 2));
        assert_eq!(mean, 2.5);
        assert!((sd - 1.25f64.sqrt()).abs() < 1e-12);
        let (mean, sd) = get_stats(&img, Rect::new(0, 0, 0, 0));
        assert!(mean.is_nan() && sd.is_nan());
    }

    #[test]
    fn peak_set_semantics() {
        let mut v = Vec::new();
        for (x, val) in [(1, 0.5), (2, 0.2), (3, 0.5), (4, 0.9)] {
            insert_peak(&mut v, Peak { x, y: 0, val });
        }
        // ascending by score; the second 0.5 is the set's duplicate key
        assert_eq!(v.iter().map(|p| (p.x, p.val)).collect::<Vec<_>>(), vec![(2, 0.2), (1, 0.5), (4, 0.9)]);
    }

    /// The AutoFind test field: a saturated star, the brightest clean star,
    /// a fainter one, a close pair (12 × 6 px apart, similar) and a star
    /// 13 px from the right edge.
    fn six_stars() -> UsImage {
        field(
            256,
            192,
            100.0,
            5.0,
            21,
            &[
                Gauss { x: 60.4, y: 50.2, sigma: 2.0, peak: 100000.0 },
                Gauss { x: 180.3, y: 60.7, sigma: 1.5, peak: 20000.0 },
                Gauss { x: 130.6, y: 120.2, sigma: 1.5, peak: 8000.0 },
                Gauss { x: 40.0, y: 140.0, sigma: 1.5, peak: 3000.0 },
                Gauss { x: 52.0, y: 146.0, sigma: 1.5, peak: 2500.0 },
                Gauss { x: 243.0, y: 120.0, sigma: 1.5, peak: 6000.0 },
            ],
        )
    }

    fn af_params() -> AutoFindParams {
        AutoFindParams {
            downsample: 1,
            min_hfd: 1.5,
            max_hfd: 20.0,
            min_snr: 6.0,
            saturation_by_adu: true,
            saturation_adu: 0,
        }
    }

    // The AutoFind expectations below (positions, lists, offsets and these
    // measurements) are PHD2's C++ output on the same frames: star.cpp and
    // image_math.cpp's Median3 built with g++ on x86-64, wx stubbed.
    /// The 20000 star of `six_stars`.
    const BRIGHT: Measured =
        (180.29885515345154, 60.70083738570846, 282488.054744526, 375.25066047723726, 3.4544032305703456, 19309, FindResult::Ok);
    /// The 8000 star.
    const MID: Measured =
        (130.60223688251548, 120.20201097114779, 112904.58214285717, 236.62630126809998, 3.5237816302385805, 7746, FindResult::Ok);
    /// The brighter star of the close pair.
    const PAIR: Measured =
        (40.001996758632174, 139.99983395141786, 42156.33707865167, 143.81639977675545, 3.5366925202775112, 3111, FindResult::Ok);
    /// The saturated star.
    const SATURATED: Measured =
        (60.3940981285546, 50.19990753273377, 2337265.6028037383, 1080.590399087413, 4.846224842909718, 65535, FindResult::Saturated);

    /// `auto_find`'s list as (star, offset from the primary); every star's
    /// reference point is its position.
    fn listed(list: &[GuideStar]) -> Vec<(Measured, Point)> {
        list.iter()
            .map(|g| {
                assert_eq!(g.reference_point, g.star.position());
                (measured(&g.star), g.offset_from_primary)
            })
            .collect()
    }

    fn pair(dim: f64) -> UsImage {
        field(
            128,
            128,
            100.0,
            5.0,
            9,
            &[Gauss { x: 60.3, y: 60.7, sigma: 1.5, peak: 20000.0 }, Gauss { x: 72.0, y: 66.0, sigma: 1.5, peak: dim }],
        )
    }

    #[test]
    fn auto_find_on_a_field() {
        let img = six_stars();
        for by_adu in [true, false] {
            let p = AutoFindParams { saturation_by_adu: by_adu, ..af_params() };
            let mut list = Vec::new();
            let r = GuideStar::auto_find(&img, 0, 15, None, &mut list, 12, &p);
            // the primary is the 20000 star, at its convolution peak in whole
            // pixels, not the brighter saturated one, which is listed but
            // erased with the stars ahead of the primary
            assert_eq!(r.position, Some(Point::new(180.0, 61.0)));
            // with saturation by ADU and no level, the level becomes 2^16 − 1
            assert_eq!(r.saturation_adu_set, if by_adu { Some(65535) } else { None });
            // the pair is pruned (both), the edge star too (243 ≥ 256 − 15):
            // the primary and the 8000 star remain, its offset running from
            // the primary's peak (180, 61), not its centroid
            let from_peak = Point::new(-49.397763117484516, 59.202010971147786);
            assert_eq!(listed(&list), vec![(BRIGHT, Point::default()), (MID, from_peak)]);
        }
    }

    #[test]
    fn auto_find_passes() {
        let img = six_stars();
        // pass 1 skips a star over 90 % of the saturation level: at 21000
        // ADU the threshold is 18900 and the 20000 star peaks at 19309
        // (BRIGHT); the two stars listed ahead of the 8000 one are erased
        let p = AutoFindParams { saturation_adu: 21000, ..af_params() };
        let mut list = Vec::new();
        let r = GuideStar::auto_find(&img, 0, 15, None, &mut list, 12, &p);
        assert_eq!(r, AutoFindResult { position: Some(Point::new(131.0, 120.0)), saturation_adu_set: None });
        assert_eq!(listed(&list), vec![(MID, Point::default())]);
        // no star reaches the SNR floor: pass 3 takes the brightest one
        // found, saturated or not, and the list is that star alone
        let p = AutoFindParams { min_snr: 1e6, ..af_params() };
        let r = GuideStar::auto_find(&img, 0, 15, None, &mut list, 12, &p);
        assert_eq!(r.position, Some(Point::new(60.0, 50.0)));
        assert_eq!(listed(&list), vec![(SATURATED, Point::default())]);
    }

    #[test]
    fn auto_find_edge_roi_downsample_and_list_size() {
        let img = six_stars();
        let mut list = Vec::new();
        // an edge allowance of 50 (calibration distance) prunes within 65 px
        // of the edges: only the 8000 star is left
        let r = GuideStar::auto_find(&img, 50, 15, None, &mut list, 12, &af_params());
        assert_eq!(r.position, Some(Point::new(131.0, 120.0)));
        assert_eq!(listed(&list), vec![(MID, Point::default())]);
        // an ROI excludes the 20000 star (Median3 blanks the rest)
        let roi = Some(Rect::new(100, 80, 156, 112));
        let r = GuideStar::auto_find(&img, 0, 15, roi, &mut list, 12, &af_params());
        assert_eq!(r.position, Some(Point::new(131.0, 120.0)));
        assert_eq!(listed(&list), vec![(MID, Point::default())]);
        // downsampling by 2 maps peak 90 back to 90·2 + 1, and the offsets
        // run from there
        let p = AutoFindParams { downsample: 2, ..af_params() };
        let r = GuideStar::auto_find(&img, 0, 15, None, &mut list, 12, &p);
        assert_eq!(r.position, Some(Point::new(181.0, 61.0)));
        let mid_from_peak = Point::new(-50.397763117484516, 59.202010971147786);
        assert_eq!(listed(&list), vec![(BRIGHT, Point::default()), (MID, mid_from_peak)]);
        // by 3 the pair is 4 px apart: one local maximum, no pair to prune,
        // so three stars are listed; max_stars cuts the list
        let p = AutoFindParams { downsample: 3, ..af_params() };
        let r = GuideStar::auto_find(&img, 0, 15, None, &mut list, 12, &p);
        assert_eq!(r.position, Some(Point::new(181.0, 61.0)));
        let pair_from_peak = Point::new(-140.99800324136783, 78.99983395141786);
        assert_eq!(listed(&list), vec![(BRIGHT, Point::default()), (MID, mid_from_peak), (PAIR, pair_from_peak)]);
        GuideStar::auto_find(&img, 0, 15, None, &mut list, 2, &p);
        assert_eq!(listed(&list), vec![(BRIGHT, Point::default()), (MID, mid_from_peak)]);
        // single-star mode: the primary alone, no secondaries collected
        let r = GuideStar::auto_find(&img, 0, 15, None, &mut list, 1, &af_params());
        assert_eq!(r.position, Some(Point::new(180.0, 61.0)));
        assert_eq!(listed(&list), vec![(BRIGHT, Point::default())]);
    }

    #[test]
    fn auto_find_close_pair_ratio() {
        // two stars 12 px apart in one search box: both dropped...
        let mut list = Vec::new();
        let r = GuideStar::auto_find(&pair(8000.0), 0, 15, None, &mut list, 12, &af_params());
        assert_eq!(r, AutoFindResult { position: None, saturation_adu_set: Some(65535) });
        assert!(list.is_empty());
        // ...unless the brighter scores 5× the dimmer: a very dim star does
        // not eliminate a very bright one (the dim one is a duplicate of the
        // bright one within 25 px, so the list holds one star)
        let r = GuideStar::auto_find(&pair(1500.0), 0, 15, None, &mut list, 12, &af_params());
        assert_eq!(r.position, Some(Point::new(60.0, 61.0)));
        let bright = (60.30138715925526, 60.70039132977992, 282642.046875, 375.29590691143716, 3.453256310253279, 19314, FindResult::Ok);
        assert_eq!(listed(&list), vec![(bright, Point::default())]);
    }

    #[test]
    fn auto_find_preconditions() {
        let img = six_stars();
        let keep = vec![GuideStar::from_star(Star::new())];
        // a subframed image: refused, the list untouched
        let mut sub = img.clone();
        sub.subframe = Some(Rect::new(0, 0, 100, 100));
        let mut list = keep.clone();
        let r = GuideStar::auto_find(&sub, 0, 15, None, &mut list, 12, &af_params());
        assert_eq!(r, AutoFindResult { position: None, saturation_adu_set: None });
        assert_eq!(list, keep);
        // an ROI narrower than the search region (clipped to the frame: 6 px)
        let r = GuideStar::auto_find(&img, 0, 15, Some(Rect::new(250, 0, 40, 40)), &mut list, 12, &af_params());
        assert_eq!(r.position, None);
        assert_eq!(list, keep);
        // an empty ROI means none
        let r = GuideStar::auto_find(&img, 0, 15, Some(Rect::default()), &mut list, 12, &af_params());
        assert_eq!(r.position, Some(Point::new(180.0, 61.0)));
        // a starless frame: nothing found, the list cleared, and the
        // saturation level still set (8-bit: 255)
        let mut blank = field(64, 64, 10.0, 2.0, 4, &[]);
        blank.bits_per_pixel = 8;
        let mut list = keep.clone();
        let r = GuideStar::auto_find(&blank, 0, 15, None, &mut list, 12, &af_params());
        assert_eq!(r, AutoFindResult { position: None, saturation_adu_set: Some(255) });
        assert!(list.is_empty());
    }
}
