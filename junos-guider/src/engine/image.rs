//! The guide frame: a `u16` raster with the statistics PHD2 keeps on it
//! (`usImage`) and the image math the capture pipeline runs on it — the 3×3
//! median, the 2×2 luminance mean, pixel squaring, dark subtraction and the
//! noise-reduction step of the worker thread.
//!
//! Every function here is a faithful port: same neighbourhoods, same integer
//! truncations, same quirks. The frame is row-major, `y * w + x`, with (0, 0)
//! the first pixel the camera sent.
//!
//! Ported from PHD2 src/usImage.h, src/usImage.cpp, src/image_math.h, src/image_math.cpp, src/worker_thread.cpp — BSD-3-Clause, see THIRD_PARTY.md

use chrono::{DateTime, Local};

use super::traits::Rect;

/// PHD2's `usImage`: the pixels plus everything the star finder, the dark
/// subtraction and the FITS writer read off the frame.
#[derive(Clone, Debug)]
pub struct UsImage {
    /// Frame width in pixels (`Size.x`).
    pub w: i32,
    /// Frame height in pixels (`Size.y`).
    pub h: i32,
    /// Row-major pixels, `w * h` of them (`ImageData`).
    pub data: Vec<u16>,
    /// Where the valid data is when the camera took a subframe (`Subframe`).
    /// `None`, or an empty rect, means the whole frame.
    pub subframe: Option<Rect>,
    /// Statistics from [`UsImage::calc_stats`], over the subframe when set.
    pub min_adu: u16,
    pub max_adu: u16,
    pub median_adu: u16,
    /// Min / max after a 3×3 median, from [`UsImage::calc_stats`].
    pub filt_min: u16,
    pub filt_max: u16,
    /// Capture start, local wall time (`ImgStartTime`); the daemon sets it
    /// from its clock when the exposure starts.
    pub img_start_time: DateTime<Local>,
    /// Exposure in milliseconds (`ImgExpDur`).
    pub img_exp_dur: i32,
    /// Frames stacked into this one (`ImgStackCnt`, 1 for a plain frame).
    pub img_stack_cnt: i32,
    /// Camera bit depth (`BitsPerPixel`), 0 when unknown.
    pub bits_per_pixel: u8,
    /// Dark-subtraction bias added to every pixel (`Pedestal`), see [`subtract`].
    pub pedestal: u16,
    /// Capture sequence number (`FrameNum`).
    pub frame_num: u32,
}

impl Default for UsImage {
    fn default() -> Self {
        Self {
            w: 0,
            h: 0,
            data: Vec::new(),
            subframe: None,
            min_adu: 0,
            max_adu: 0,
            median_adu: 0,
            filt_min: 0,
            filt_max: 0,
            img_start_time: DateTime::<Local>::default(),
            img_exp_dur: 0,
            img_stack_cnt: 1,
            bits_per_pixel: 0,
            pedestal: 0,
            frame_num: 0,
        }
    }
}

impl UsImage {
    /// A zeroed `w × h` frame with PHD2's constructor defaults.
    pub fn new(w: i32, h: i32) -> Self {
        let mut img = Self::default();
        img.init(w, h);
        img
    }

    /// `usImage::Init`: size the pixel buffer, drop the subframe and the
    /// min / max / median; every other field is kept. PHD2 leaves the new
    /// pixels uninitialised; they are zeroed here.
    pub fn init(&mut self, w: i32, h: i32) {
        let n = (w.max(0) as usize) * (h.max(0) as usize);
        if n != self.data.len() {
            self.data = vec![0; n];
        }
        self.w = w;
        self.h = h;
        self.subframe = None;
        self.min_adu = 0;
        self.max_adu = 0;
        self.median_adu = 0;
    }

    /// `NPixels`.
    pub fn npixels(&self) -> usize {
        self.data.len()
    }

    /// `Pixel(x, y)`.
    pub fn pixel(&self, x: i32, y: i32) -> u16 {
        self.data[(y * self.w + x) as usize]
    }

    pub fn pixel_mut(&mut self, x: i32, y: i32) -> &mut u16 {
        &mut self.data[(y * self.w + x) as usize]
    }

    /// `usImage::Clear`: every pixel to 0.
    pub fn clear(&mut self) {
        self.data.fill(0);
    }

    /// `usImage::CopyFrom`: [`UsImage::init`] to the source's size, then copy
    /// the pixels. The other fields keep their values, as in PHD2.
    pub fn copy_from(&mut self, src: &UsImage) {
        self.init(src.w, src.h);
        self.data.copy_from_slice(&src.data);
    }

    /// The subframe when one is set and not empty — PHD2's
    /// `!Subframe.IsEmpty()` branches.
    pub fn roi(&self) -> Option<Rect> {
        self.subframe.filter(|r| !r.is_empty())
    }

    /// `usImage::CalcStats`: min / max / median over the subframe (or the
    /// whole frame) by histogram, then `filt_min` / `filt_max` over the same
    /// pixels after a 3×3 median (so a hot pixel does not set the max).
    pub fn calc_stats(&mut self) {
        if self.data.is_empty() {
            return;
        }
        // usImage.cpp:152-155: the starting values before any pixel is seen.
        self.min_adu = 65535;
        self.max_adu = 0;
        self.filt_min = 65535;
        self.filt_max = 0;

        let (pixels, w, h): (std::borrow::Cow<[u16]>, i32, i32) = match self.roi() {
            None => (std::borrow::Cow::Borrowed(&self.data), self.w, self.h),
            Some(sf) => {
                // usImage.cpp:187-201: the subframe copied out as its own frame.
                let mut tmp = Vec::with_capacity((sf.w * sf.h) as usize);
                for y in 0..sf.h {
                    let start = (sf.x + (sf.y + y) * self.w) as usize;
                    tmp.extend_from_slice(&self.data[start..start + sf.w as usize]);
                }
                (std::borrow::Cow::Owned(tmp), sf.w, sf.h)
            }
        };

        let (min, max, median) = histogram_stats(&pixels);
        self.min_adu = min;
        self.max_adu = max;
        self.median_adu = median;

        let mut filtered = vec![0u16; pixels.len()];
        median3_rect(&mut filtered, &pixels, w, Rect::new(0, 0, w, h));
        for &d in &filtered {
            if d < self.filt_min {
                self.filt_min = d;
            }
            if d > self.filt_max {
                self.filt_max = d;
            }
        }
    }
}

/// PHD2's `HistogramBuilder` (usImage.cpp:40): min, max and the median as
/// the smallest value whose cumulative count exceeds `n / 2` — the element
/// at index `n / 2` of the sorted pixels (`MaxADU` when the loop runs out,
/// which only happens for that same element).
fn histogram_stats(pixels: &[u16]) -> (u16, u16, u16) {
    let mut histo = vec![0u32; 65536];
    let mut min = pixels[0];
    let mut max = pixels[0];
    for &v in pixels {
        histo[v as usize] += 1;
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    let mut left = (pixels.len() / 2) as u32;
    for v in min..max {
        if histo[v as usize] > left {
            return (min, max, v);
        }
        left -= histo[v as usize];
    }
    (min, max, max)
}

/// The median of a 3 to 9 element window. PHD2's `median3..median9` are
/// hand-unrolled sorting networks (image_math.cpp:182-394) that return the
/// middle element for an odd count and the truncated mean of the two
/// middle elements for an even one; a sort gives the same numbers.
fn window_median(win: &mut [u16]) -> u16 {
    win.sort_unstable();
    let n = win.len();
    if n % 2 == 1 {
        win[n / 2]
    } else {
        ((win[n / 2 - 1] as u32 + win[n / 2] as u32) / 2) as u16
    }
}

/// `Median3(dst, src, size, rect)` (image_math.cpp:396): a 3×3 median of
/// `src` (a `w`-wide frame) written into `dst` inside `rect` only — the
/// neighbourhood is clipped to the rect, so its edges use 6 pixels and its
/// corners 4. Pixels of `dst` outside the rect are left as they are; that is
/// how AutoFind blanks everything outside its ROI (star.cpp:738). A rect
/// smaller than 2×2 is copied as-is (PHD2 reads out of bounds there).
pub fn median3_rect(dst: &mut [u16], src: &[u16], w: i32, rect: Rect) {
    let (rx, ry, rw, rh) = (rect.x, rect.y, rect.w, rect.h);
    let ix = |x: i32, y: i32| ((ry + y) * w + rx + x) as usize;
    if rw < 2 || rh < 2 {
        for y in 0..rh.max(0) {
            for x in 0..rw.max(0) {
                dst[ix(x, y)] = src[ix(x, y)];
            }
        }
        return;
    }
    let mut win = [0u16; 9];
    for y in 0..rh {
        for x in 0..rw {
            let mut n = 0;
            for dy in -1..=1 {
                let yy = y + dy;
                if yy < 0 || yy >= rh {
                    continue;
                }
                for dx in -1..=1 {
                    let xx = x + dx;
                    if xx < 0 || xx >= rw {
                        continue;
                    }
                    win[n] = src[ix(xx, yy)];
                    n += 1;
                }
            }
            dst[ix(x, y)] = window_median(&mut win[..n]);
        }
    }
}

/// `Median3(usImage&)` (image_math.cpp:150): the 3×3 median in place. With
/// a subframe, only the subframe is filtered and the rest of the frame is
/// blanked to 0.
pub fn median3(img: &mut UsImage) {
    let mut tmp = vec![0u16; img.data.len()];
    let rect = img.roi().unwrap_or(Rect::new(0, 0, img.w, img.h));
    median3_rect(&mut tmp, &img.data, img.w, rect);
    img.data = tmp;
}

/// `QuickLRecon` (image_math.cpp:79): a luminance-only debayer, the mean of
/// each sliding 2×2 window (`>> 2`); the last column and row average the two
/// pixels left (`>> 1`), the bottom-right pixel is copied. With a subframe,
/// only the subframe is processed and the rest of the frame is blanked.
pub fn quick_l_recon(img: &mut UsImage) {
    let mut tmp = vec![0u16; img.data.len()];
    let rect = img.roi().unwrap_or(Rect::new(0, 0, img.w, img.h));
    let (rx, ry, rw, rh) = (rect.x, rect.y, rect.w, rect.h);
    if rw < 1 || rh < 1 {
        return;
    }
    let w = img.w;
    let src = &img.data;
    let ix = |x: i32, y: i32| ((ry + y) * w + rx + x) as usize;
    for y in 0..rh - 1 {
        for x in 0..rw - 1 {
            let t = src[ix(x, y)] as u32 + src[ix(x + 1, y)] as u32 + src[ix(x, y + 1)] as u32 + src[ix(x + 1, y + 1)] as u32;
            tmp[ix(x, y)] = (t >> 2) as u16;
        }
        let t = src[ix(rw - 1, y)] as u32 + src[ix(rw - 1, y + 1)] as u32;
        tmp[ix(rw - 1, y)] = (t >> 1) as u16;
    }
    for x in 0..rw - 1 {
        let t = src[ix(x, rh - 1)] as u32 + src[ix(x + 1, rh - 1)] as u32;
        tmp[ix(x, rh - 1)] = (t >> 1) as u16;
    }
    tmp[ix(rw - 1, rh - 1)] = src[ix(rw - 1, rh - 1)];
    img.data = tmp;
}

/// `SquarePixels` (image_math.cpp:607): when the pixels are wider than
/// tall (`xsize > ysize`, microns), stretch the frame in X to
/// `round(w * xsize / ysize)` columns so the image is geometrically square.
/// Nothing happens otherwise. Quirk kept: PHD2 computes the interpolation
/// weight but reads the left neighbour twice (`ind1` for both terms,
/// image_math.cpp:641), so each output pixel is the source pixel at
/// `floor(x * ratio)` — the arithmetic is reproduced in case the two
/// products round differently. The subframe is dropped (`Init`).
pub fn square_pixels(img: &mut UsImage, xsize: f32, ysize: f32) {
    if img.data.is_empty() || xsize <= ysize {
        return;
    }
    // `double ratio = ysize / xsize` — a float division widened to double.
    let ratio = (ysize / xsize) as f64;
    let (old_w, old_h) = (img.w, img.h);
    let old = std::mem::take(&mut img.data);
    let newsize = (old_w as f64 / ratio + 0.5).floor() as i32; // ROUND()
    img.init(newsize, old_h);
    for y in 0..old_h {
        let line = &old[(y * old_w) as usize..((y + 1) * old_w) as usize];
        for x in 0..newsize {
            let oldposition = x as f64 * ratio;
            let ind1 = oldposition.floor() as usize;
            let weight = oldposition.ceil() - oldposition;
            let v = line[ind1] as f32 as f64;
            img.data[(y * newsize + x) as usize] = (v * weight + v * (1.0 - weight)) as u16;
        }
    }
}

/// `Subtract` (image_math.cpp:652), the dark subtraction:
/// `pedestal = median(dark) - light.median_adu` when the dark is brighter
/// (otherwise the pedestal is left as it was — 0 on a fresh frame), the
/// dark's median over the light's subframe when one is set (recomputed
/// there), then `px = clamp(light + pedestal - dark, 0, 65535)` over that
/// region. Errors when either frame is empty or the sizes differ, in which
/// case nothing is changed.
///
/// PHD2 calls it (`GuideCamera::SubtractDark`, camera.cpp:1496) on the raw
/// frame straight from the camera, before [`UsImage::calc_stats`]: the
/// light's `median_adu` is still 0 from `Init`, so the pedestal is the
/// dark's median (over the subframe when set). Call it the same way.
pub fn subtract(light: &mut UsImage, dark: &UsImage) -> Result<(), &'static str> {
    if light.data.is_empty() || dark.data.is_empty() {
        return Err("no image data");
    }
    if light.w != dark.w || light.h != dark.h {
        return Err("size mismatch");
    }
    let w = light.w;
    let median_light = light.median_adu;
    let (region, median_dark) = match light.roi() {
        Some(sf) => {
            // image_math.cpp:671-682: the dark's median inside the subframe,
            // `nth_element` at pixcnt / 2.
            let mut tmp = Vec::with_capacity((sf.w * sf.h) as usize);
            for y in 0..sf.h {
                let start = (sf.x + (sf.y + y) * w) as usize;
                tmp.extend_from_slice(&dark.data[start..start + sf.w as usize]);
            }
            tmp.sort_unstable();
            let m = tmp[tmp.len() / 2];
            (sf, m)
        }
        None => (Rect::new(0, 0, light.w, light.h), dark.median_adu),
    };
    if median_dark > median_light {
        // Needed for the saturation test in Star::Find.
        light.pedestal = median_dark - median_light;
    }
    let pedestal = light.pedestal as i32;
    for y in region.y..region.y + region.h {
        for x in region.x..region.x + region.w {
            let i = (y * w + x) as usize;
            let newval = light.data[i] as i32 + pedestal - dark.data[i] as i32;
            // < 0: a hot pixel in the dark that is not in the light.
            light.data[i] = newval.clamp(0, 65535) as u16;
        }
    }
    Ok(())
}

/// PHD2's `NOISE_REDUCTION_METHOD` (myframe.h:61), profile key
/// `/NoiseReductionMethod`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NoiseReduction {
    #[default]
    None = 0,
    Mean2x2 = 1,
    Median3 = 2,
}

impl NoiseReduction {
    /// From the profile value; anything else is the default, as
    /// `MyFrame::SetNoiseReductionMethod` (myframe.cpp:2193) falls back.
    /// PHD2 also writes the value it kept back to `/NoiseReductionMethod`
    /// (myframe.cpp:2218); the profile layer must persist the fallback.
    pub fn from_config(v: i64) -> NoiseReduction {
        match v {
            1 => NoiseReduction::Mean2x2,
            2 => NoiseReduction::Median3,
            _ => NoiseReduction::None,
        }
    }

    /// The wording of the guide-log settings summary (myframe.cpp:2822).
    pub fn label(self) -> &'static str {
        match self {
            NoiseReduction::None => "none",
            NoiseReduction::Mean2x2 => "2x2 mean",
            NoiseReduction::Median3 => "3x3 median",
        }
    }
}

/// The worker thread's noise-reduction step (worker_thread.cpp:224), run
/// on the captured frame after the dark / defect-map subtraction and before
/// `calc_stats`.
pub fn apply_noise_reduction(img: &mut UsImage, nr: NoiseReduction) {
    match nr {
        NoiseReduction::None => {}
        NoiseReduction::Mean2x2 => quick_l_recon(img),
        NoiseReduction::Median3 => median3(img),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: i32, h: i32, px: &[u16]) -> UsImage {
        let mut img = UsImage::new(w, h);
        img.data.copy_from_slice(px);
        img
    }

    // A 5×5 gradient with a hot pixel in the middle and one in a corner.
    const A: [u16; 25] = [
        900, 10, 20, 30, 40, //
        20, 30, 40, 50, 60, //
        40, 50, 5000, 70, 80, //
        60, 70, 80, 90, 100, //
        80, 90, 100, 110, 120,
    ];

    #[test]
    fn median3_removes_hot_pixels_and_clips_at_the_edges() {
        let mut img = image(5, 5, &A);
        median3(&mut img);
        // Corners: the truncated mean of the two middle values of 4; edges:
        // of 6; interior: the middle of 9.
        let expected: [u16; 25] = [
            25, 25, 30, 40, 45, //
            35, 40, 40, 50, 55, //
            45, 50, 70, 80, 75, //
            65, 80, 90, 100, 95, //
            75, 80, 90, 100, 105,
        ];
        assert_eq!(img.data, expected);
    }

    #[test]
    fn median3_with_a_subframe_blanks_the_rest() {
        let mut px = [100u16; 36];
        px[3 * 6 + 3] = 4000;
        px[0] = 7;
        let mut img = image(6, 6, &px);
        img.subframe = Some(Rect::new(1, 1, 4, 4));
        median3(&mut img);
        for y in 0..6 {
            for x in 0..6 {
                let inside = (1..5).contains(&x) && (1..5).contains(&y);
                assert_eq!(img.pixel(x, y), if inside { 100 } else { 0 }, "({x},{y})");
            }
        }
    }

    #[test]
    fn median3_rect_only_touches_the_rect() {
        let src = [5u16; 16];
        let mut dst = [9u16; 16];
        median3_rect(&mut dst, &src, 4, Rect::new(1, 1, 2, 2));
        assert_eq!(dst[5], 5);
        assert_eq!(dst[10], 5);
        assert_eq!(dst[0], 9);
        assert_eq!(dst[15], 9);
    }

    #[test]
    fn calc_stats_histogram_median_and_filtered_range() {
        let mut img = image(5, 5, &A);
        img.calc_stats();
        // 25 pixels: the median is the 13th smallest; the filtered range
        // comes from the median3 output above.
        assert_eq!((img.min_adu, img.max_adu, img.median_adu), (10, 5000, 70));
        assert_eq!((img.filt_min, img.filt_max), (25, 105));

        img.subframe = Some(Rect::new(1, 1, 3, 3));
        img.calc_stats();
        assert_eq!((img.min_adu, img.max_adu, img.median_adu), (30, 5000, 70));
        assert_eq!((img.filt_min, img.filt_max), (45, 85));

        // Even count: index n / 2 of the sorted pixels, i.e. the 9th of 16.
        let mut img = image(4, 4, &(1..=16).collect::<Vec<u16>>());
        img.calc_stats();
        assert_eq!(img.median_adu, 9);
        // A flat frame: the loop runs out and returns MaxADU.
        let mut img = image(2, 2, &[7, 7, 7, 7]);
        img.calc_stats();
        assert_eq!((img.min_adu, img.max_adu, img.median_adu), (7, 7, 7));
    }

    #[test]
    fn quick_l_recon_4x4() {
        let mut img = image(4, 4, &(0..16).collect::<Vec<u16>>());
        quick_l_recon(&mut img);
        assert_eq!(img.data, [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15]);

        let mut img = image(4, 4, &(0..16).map(|v| v * 100).collect::<Vec<u16>>());
        img.subframe = Some(Rect::new(1, 1, 2, 2));
        quick_l_recon(&mut img);
        assert_eq!(img.data, [0, 0, 0, 0, 0, 750, 800, 0, 0, 950, 1000, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn subtract_pedestal_rule() {
        // The capture pipeline's shape: a fresh light without stats
        // (median_adu 0), so the pedestal is the dark's median, 120.
        let mut light = image(3, 3, &[90, 100, 110, 100, 100, 100, 120, 100, 95]);
        let mut dark = image(3, 3, &[120, 120, 120, 130, 110, 115, 100, 200, 120]);
        dark.calc_stats();
        subtract(&mut light, &dark).unwrap();
        assert_eq!(light.pedestal, dark.median_adu);
        assert_eq!(light.pedestal, 120);
        assert_eq!(light.data, [90, 100, 110, 90, 110, 105, 140, 20, 95]);

        // With the light's stats (which PHD2 never has at this point) the
        // pedestal is the difference of the medians: 120 - 100.
        let mut light = image(3, 3, &[90, 100, 110, 100, 100, 100, 120, 100, 95]);
        light.calc_stats();
        subtract(&mut light, &dark).unwrap();
        assert_eq!(light.pedestal, 20);
        assert_eq!(light.data, [0, 0, 10, 0, 10, 5, 40, 0, 0]);

        // A dark darker than the light leaves the pedestal alone.
        let mut light = image(2, 1, &[500, 65535]);
        light.calc_stats();
        let mut dark = image(2, 1, &[10, 0]);
        dark.calc_stats();
        subtract(&mut light, &dark).unwrap();
        assert_eq!(light.pedestal, 0);
        assert_eq!(light.data, [490, 65535]);

        // Size mismatch: refused, untouched.
        let mut light = image(2, 1, &[500, 600]);
        let dark = image(1, 2, &[1, 1]);
        assert_eq!(subtract(&mut light, &dark), Err("size mismatch"));
        assert_eq!(light.data, [500, 600]);
    }

    #[test]
    fn subtract_uses_the_subframe_medians() {
        // Dark: 300 everywhere except 10 inside the 2×2 centre. The light is
        // fresh (no stats) with its subframe on that centre, so the pedestal
        // is the dark's median there, 10, not the full-frame 300.
        let mut dark_px = [300u16; 16];
        for y in 1..3 {
            for x in 1..3 {
                dark_px[y * 4 + x] = 10;
            }
        }
        let mut dark = image(4, 4, &dark_px);
        dark.calc_stats();
        assert_eq!(dark.median_adu, 300);
        let mut light = image(4, 4, &[50; 16]);
        light.subframe = Some(Rect::new(1, 1, 2, 2));
        subtract(&mut light, &dark).unwrap();
        assert_eq!(light.pedestal, 10);
        // Inside: 50 + 10 - 10; outside: untouched.
        assert!(light.data.iter().all(|&v| v == 50));

        // With the light's stats (median 50 > 10) the pedestal stays 0.
        let mut light = image(4, 4, &[50; 16]);
        light.subframe = Some(Rect::new(1, 1, 2, 2));
        light.calc_stats();
        subtract(&mut light, &dark).unwrap();
        assert_eq!(light.pedestal, 0);
        for y in 0..4 {
            for x in 0..4 {
                let inside = (1..3).contains(&x) && (1..3).contains(&y);
                assert_eq!(light.pixel(x, y), if inside { 40 } else { 50 });
            }
        }
    }

    #[test]
    fn square_pixels_duplicates_the_left_neighbour() {
        let mut img = image(4, 2, &[1000, 2000, 3000, 4000, 5, 6, 7, 8]);
        img.subframe = Some(Rect::new(0, 0, 2, 2));
        square_pixels(&mut img, 2.0, 1.0);
        assert_eq!((img.w, img.h), (8, 2));
        // A true interpolation would give 1500 at x = 1; PHD2 gives 1000.
        assert_eq!(img.data, [1000, 1000, 2000, 2000, 3000, 3000, 4000, 4000, 5, 5, 6, 6, 7, 7, 8, 8]);
        assert_eq!(img.subframe, None);

        let mut img = image(2, 1, &[1, 2]);
        square_pixels(&mut img, 1.0, 1.0);
        assert_eq!(img.data, [1, 2]);
    }

    #[test]
    fn noise_reduction_config_and_dispatch() {
        assert_eq!(NoiseReduction::from_config(2), NoiseReduction::Median3);
        assert_eq!(NoiseReduction::from_config(7), NoiseReduction::None);
        assert_eq!(NoiseReduction::Mean2x2.label(), "2x2 mean");
        let mut img = image(5, 5, &A);
        apply_noise_reduction(&mut img, NoiseReduction::None);
        assert_eq!(img.data, A);
        apply_noise_reduction(&mut img, NoiseReduction::Median3);
        assert_eq!(img.pixel(2, 2), 70);
    }

    #[test]
    fn init_keeps_the_frame_metadata() {
        let mut img = UsImage::new(2, 2);
        img.img_exp_dur = 1500;
        img.pedestal = 3;
        img.subframe = Some(Rect::new(0, 0, 1, 1));
        img.median_adu = 9;
        img.init(3, 1);
        assert_eq!(img.data.len(), 3);
        assert_eq!(img.img_exp_dur, 1500);
        assert_eq!(img.pedestal, 3);
        assert_eq!(img.subframe, None);
        assert_eq!(img.median_adu, 0);
        assert_eq!(img.img_stack_cnt, 1);
    }
}
