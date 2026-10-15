//! Lightweight star detector for the focus frames' star overlay
//! (`annotate_focus_payload`, run on every "+F" preview by both backends).
//!
//! This is **not** a full SEP port — it is a pragmatic pipeline:
//!   background (median + MAD) → threshold → connected-component labelling →
//!   flux-weighted centroid → flux-weighted mean radius (HFR).
//!
//! HFR here uses the canonical autofocus definition `HFR = Σ(fᵢ·rᵢ) / Σ(fᵢ)`
//! (Weber & Brady), where `fᵢ` is background-subtracted pixel flux and `rᵢ` the
//! distance from the centroid.

use std::cmp::Ordering;

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Star {
    pub x: f64,
    pub y: f64,
    pub hfr: f64,
    /// Total background-subtracted flux — carried for future weighting/QA.
    #[allow(dead_code)]
    pub flux: f64,
    /// Blob pixel count — carried for future weighting/QA.
    #[allow(dead_code)]
    pub npix: usize,
}

#[derive(Debug, Clone)]
pub struct DetectParams {
    /// Threshold = median + `sigma_k` × noise, noise = 1.4826 × MAD.
    pub sigma_k: f64,
    /// Reject blobs smaller than this (hot pixels / cosmic rays).
    pub min_pixels: usize,
    /// Reject blobs larger than this (saturated cores, nebulosity).
    pub max_pixels: usize,
    /// Stop after this many stars (defensive cap on pathological frames).
    pub max_stars: usize,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self { sigma_k: 5.0, min_pixels: 4, max_pixels: 4000, max_stars: 5000 }
    }
}

/// Robust background estimate: `(median, MAD)` over a strided subsample so this
/// stays O(N) even on 24 MP frames.
fn median_mad(plane: &[f32]) -> (f32, f32) {
    let stride = (plane.len() / 100_000).max(1);
    let mut sub: Vec<f32> = plane.iter().step_by(stride).copied().collect();
    if sub.is_empty() { return (0.0, 1.0); }
    sub.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let median = sub[sub.len() / 2];
    let mut dev: Vec<f32> = sub.iter().map(|v| (v - median).abs()).collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let mad = dev[dev.len() / 2];
    (median, mad)
}

/// Detect stars in a mono `f32` image plane (row-major, `w`×`h`).
pub fn detect_stars(plane: &[f32], w: usize, h: usize, p: &DetectParams) -> Vec<Star> {
    let n = w * h;
    if plane.len() < n || n == 0 { return Vec::new(); }

    let (median, mad) = median_mad(plane);
    let noise = (1.4826 * mad).max(1e-6);
    let threshold = median + (p.sigma_k as f32) * noise;

    let mut visited = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut pix: Vec<usize> = Vec::new();
    let mut stars: Vec<Star> = Vec::new();

    for start in 0..n {
        if visited[start] || plane[start] <= threshold { continue; }

        // ── Flood-fill this connected component (8-neighbour, iterative). ──
        stack.clear();
        pix.clear();
        stack.push(start);
        visited[start] = true;
        let mut overflow = false;
        while let Some(idx) = stack.pop() {
            pix.push(idx);
            if pix.len() > p.max_pixels { overflow = true; break; }
            let x = (idx % w) as i64;
            let y = (idx / w) as i64;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if dx == 0 && dy == 0 { continue; }
                    let nx = x + dx;
                    let ny = y + dy;
                    if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 { continue; }
                    let nidx = ny as usize * w + nx as usize;
                    if !visited[nidx] && plane[nidx] > threshold {
                        visited[nidx] = true;
                        stack.push(nidx);
                    }
                }
            }
        }
        if overflow || pix.len() < p.min_pixels { continue; }

        // ── Reject edge-touching blobs (partial stars skew HFR/centroid). ──
        let (mut minx, mut miny, mut maxx, mut maxy) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for &idx in &pix {
            let x = idx % w;
            let y = idx / w;
            minx = minx.min(x); miny = miny.min(y);
            maxx = maxx.max(x); maxy = maxy.max(y);
        }
        if minx == 0 || miny == 0 || maxx == w - 1 || maxy == h - 1 { continue; }

        // ── Flux-weighted centroid (background-subtracted). ──
        let mut sf = 0f64;
        let mut sx = 0f64;
        let mut sy = 0f64;
        for &idx in &pix {
            let v = (plane[idx] - median) as f64;
            if v <= 0.0 { continue; }
            let x = (idx % w) as f64;
            let y = (idx / w) as f64;
            sf += v; sx += v * x; sy += v * y;
        }
        if sf <= 0.0 { continue; }
        let cx = sx / sf;
        let cy = sy / sf;

        // ── HFR = flux-weighted mean radius. ──
        let mut swr = 0f64;
        for &idx in &pix {
            let v = (plane[idx] - median) as f64;
            if v <= 0.0 { continue; }
            let x = (idx % w) as f64;
            let y = (idx / w) as f64;
            let r = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            swr += v * r;
        }
        let hfr = (swr / sf).max(0.5);

        stars.push(Star { x: cx, y: cy, hfr, flux: sf, npix: pix.len() });
        if stars.len() >= p.max_stars { break; }
    }

    stars
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paint a round Gaussian "star" of the given peak and sigma into a plane.
    fn add_star(plane: &mut [f32], w: usize, h: usize, cx: f64, cy: f64, peak: f32, sigma: f64) {
        let rad = (sigma * 4.0).ceil() as i64;
        let x0 = cx.round() as i64;
        let y0 = cy.round() as i64;
        for dy in -rad..=rad {
            for dx in -rad..=rad {
                let x = x0 + dx;
                let y = y0 + dy;
                if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 { continue; }
                let r2 = (x as f64 - cx).powi(2) + (y as f64 - cy).powi(2);
                let v = peak as f64 * (-r2 / (2.0 * sigma * sigma)).exp();
                let idx = y as usize * w + x as usize;
                plane[idx] += v as f32;
            }
        }
    }

    #[test]
    fn detects_injected_stars_at_known_positions() {
        let (w, h) = (200usize, 200usize);
        let mut plane = vec![100.0f32; w * h]; // flat bias, no noise
        let truth = [(50.0, 60.0), (150.0, 40.0), (120.0, 170.0)];
        for &(x, y) in &truth {
            add_star(&mut plane, w, h, x, y, 4000.0, 2.0);
        }
        let stars = detect_stars(&plane, w, h, &DetectParams::default());
        assert_eq!(stars.len(), truth.len(), "should find exactly the injected stars");
        for &(tx, ty) in &truth {
            let hit = stars.iter().any(|s| (s.x - tx).abs() < 1.0 && (s.y - ty).abs() < 1.0);
            assert!(hit, "no star detected near ({tx},{ty})");
        }
        // A σ=2 Gaussian should yield an HFR of a couple of pixels.
        for s in &stars {
            assert!(s.hfr > 0.5 && s.hfr < 6.0, "implausible HFR {}", s.hfr);
        }
    }

    #[test]
    fn defocus_increases_hfr() {
        let (w, h) = (120usize, 120usize);
        let mut tight = vec![50.0f32; w * h];
        let mut wide = vec![50.0f32; w * h];
        add_star(&mut tight, w, h, 60.0, 60.0, 5000.0, 1.5);
        add_star(&mut wide, w, h, 60.0, 60.0, 5000.0, 4.0);
        let a = detect_stars(&tight, w, h, &DetectParams::default());
        let b = detect_stars(&wide, w, h, &DetectParams::default());
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert!(b[0].hfr > a[0].hfr, "wider star must have larger HFR ({} vs {})", b[0].hfr, a[0].hfr);
    }

    #[test]
    fn rejects_hot_pixels() {
        let (w, h) = (64usize, 64usize);
        let mut plane = vec![10.0f32; w * h];
        plane[20 * w + 20] = 60000.0; // single hot pixel
        let stars = detect_stars(&plane, w, h, &DetectParams::default());
        assert!(stars.is_empty(), "single hot pixel must be rejected");
    }
}

/// Decode a JPEG and detect its stars. Returns `(stars, width, height)` in
/// JPEG pixel space, or `None` if the payload isn't a decodable JPEG.
pub fn detect_in_jpeg(jpeg: &[u8]) -> Option<(Vec<Star>, u32, u32)> {
    use image::ImageFormat;
    let img = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg).ok()?;
    let luma = img.to_luma8();
    let (w, h) = luma.dimensions();
    let plane: Vec<f32> = luma.iter().map(|&p| p as f32).collect();
    let stars = detect_stars(&plane, w as usize, h as usize, &DetectParams::default());
    Some((stars, w, h))
}

/// Width of the real (unscaled) frame, parsed from the `resolution` metadata
/// field (`"WxH"`, KStars media.cpp:403). `None` when absent or malformed.
fn frame_width(metadata: &Value) -> Option<f64> {
    metadata
        .get("resolution")
        .and_then(Value::as_str)
        .and_then(|r| r.split_once('x'))
        .and_then(|(w, _)| w.trim().parse::<f64>().ok())
}

/// Add the star overlay (`stars`, `star_w`, `star_h`, `star_scale`) to a
/// `new_preview_image` payload when it is a focus frame (`uuid` "+F", JPEG).
/// Detection runs on the exact JPEG that gets displayed, so the coordinates
/// map straight onto the preview image with no rescaling. Other frames are
/// left untouched.
pub fn annotate_focus_payload(payload: &mut Value, jpeg: &[u8], metadata: &Value) {
    let uuid = metadata.get("uuid").and_then(Value::as_str).unwrap_or("");
    let ext = metadata.get("ext").and_then(Value::as_str).unwrap_or("");
    if !(uuid.starts_with("+F") && ext == "jpg") {
        return;
    }
    let Some((stars, w, h)) = detect_in_jpeg(jpeg) else { return };
    let arr: Vec<Value> = stars
        .iter()
        .map(|s| json!({ "x": s.x, "y": s.y, "hfr": s.hfr }))
        .collect();
    payload["stars"] = Value::Array(arr);
    payload["star_w"] = json!(w);
    payload["star_h"] = json!(h);
    // KStars renders the focus view at a zoom-dependent scale and then caps
    // it at HB_IMAGE_WIDTH/2 (media.cpp:454), so the HFR measured above is in
    // preview pixels. This ratio converts it to the frame's own (binned
    // sensor) pixels — the unit KStars publishes its own HFR in. It is an
    // arbitrary positive real, not 1 or 2, and can be < 1; the client decides
    // whether to trust it.
    if let Some(scale) = frame_width(metadata)
        .filter(|fw| *fw > 0.0 && w > 0)
        .map(|fw| fw / w as f64)
    {
        payload["star_scale"] = json!(scale);
    }
}
