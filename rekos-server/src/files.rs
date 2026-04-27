//! Captures-folder browser API.
//!
//! Endpoints (mounted under `/api/files`):
//!
//! - `GET /list?path=<rel>`   — directory listing.
//! - `GET /meta?path=<rel>`   — per-file metadata, including parsed FITS header.
//! - `GET /thumb?path=<rel>&size=N` — JPEG thumbnail (cached on disk).
//! - `GET /raw?path=<rel>[&as=preview]` — original bytes, or full-res JPEG
//!   for FITS when `as=preview`.
//!
//! All paths are resolved relative to `Config::resolved_captures_dir()`. After
//! joining we `canonicalize()` and reject any path that doesn't stay inside
//! the canonical root — covers `..` traversal and symlinks pointing outside.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::AppState;

// ── Query types ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PathQ {
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct ThumbQ {
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_thumb_size")]
    pub size: u32,
}

fn default_thumb_size() -> u32 { 256 }

#[derive(Debug, Deserialize)]
pub struct RawQ {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub r#as: Option<String>,
}

// ── Sandbox resolution ───────────────────────────────────────────────────────

/// Resolve a user-supplied relative path against the captures root.
/// Returns `(canonical_root, canonical_target)` on success. The caller decides
/// whether the target needs to exist (we run `canonicalize` only when it does).
fn resolve(state: &AppState, rel: &str) -> Result<(PathBuf, PathBuf), StatusCode> {
    let root = state.config.resolved_captures_dir();
    let root = root.canonicalize().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Strip leading slashes so absolute-looking inputs are treated as relative.
    let trimmed = rel.trim_start_matches(['/', '\\']);
    let joined = if trimmed.is_empty() { root.clone() } else { root.join(trimmed) };

    let canonical = joined.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    if !canonical.starts_with(&root) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok((root, canonical))
}

fn relative_to(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).map(|r| r.to_string_lossy().to_string()).unwrap_or_default()
}

fn parent_of(root: &Path, p: &Path) -> Option<String> {
    if p == root { return None; }
    p.parent().map(|par| relative_to(root, par))
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn extension(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).unwrap_or_default()
}

fn is_fits_ext(ext: &str) -> bool {
    matches!(ext, "fits" | "fit" | "fts")
}

// ── /list ────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct DirEntry {
    name: String,
    kind: &'static str,
    size: u64,
    mtime: u64,
    ext: String,
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<PathQ>,
) -> Result<Json<Value>, StatusCode> {
    let (root, target) = resolve(&state, &q.path)?;
    let read = std::fs::read_dir(&target).map_err(|_| StatusCode::NOT_FOUND)?;

    let mut dirs: Vec<DirEntry> = Vec::new();
    let mut files: Vec<DirEntry> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Hide our own thumb cache.
        if name.starts_with('.') { continue; }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            dirs.push(DirEntry {
                name,
                kind: "dir",
                size: 0,
                mtime: mtime_secs(&meta),
                ext: String::new(),
            });
        } else if meta.is_file() {
            files.push(DirEntry {
                name: name.clone(),
                kind: "file",
                size: meta.len(),
                mtime: mtime_secs(&meta),
                ext: extension(&path),
            });
        }
    }
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let mut entries = dirs;
    entries.extend(files);

    Ok(Json(json!({
        "path":    relative_to(&root, &target),
        "parent":  parent_of(&root, &target),
        "entries": entries,
    })))
}

// ── /meta ────────────────────────────────────────────────────────────────────

pub async fn meta(
    State(state): State<AppState>,
    Query(q): Query<PathQ>,
) -> Result<Json<Value>, StatusCode> {
    let (_root, target) = resolve(&state, &q.path)?;
    let meta = std::fs::metadata(&target).map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() { return Err(StatusCode::BAD_REQUEST); }

    let ext = extension(&target);
    let name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();

    let fits_json = if is_fits_ext(&ext) {
        match std::fs::read(&target) {
            Ok(bytes) => match parse_fits_header(&bytes) {
                Ok(rows) => {
                    let parsed = parse_fits_meta(&rows);
                    Some(json!({
                        "header": rows.iter().map(|r| json!({
                            "key":     r.key,
                            "value":   r.value,
                            "comment": r.comment,
                        })).collect::<Vec<_>>(),
                        "parsed": parsed,
                    }))
                }
                Err(e) => {
                    warn!("FITS header parse failed for {}: {e}", target.display());
                    None
                }
            },
            Err(_) => None,
        }
    } else {
        None
    };

    Ok(Json(json!({
        "name":  name,
        "size":  meta.len(),
        "mtime": mtime_secs(&meta),
        "ext":   ext,
        "fits":  fits_json,
    })))
}

// ── /thumb ───────────────────────────────────────────────────────────────────

pub async fn thumb(
    State(state): State<AppState>,
    Query(q): Query<ThumbQ>,
) -> Result<Response, StatusCode> {
    let (root, target) = resolve(&state, &q.path)?;
    let meta = std::fs::metadata(&target).map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() { return Err(StatusCode::BAD_REQUEST); }
    let size = q.size.clamp(64, 1024);

    let cache_path = thumb_cache_path(&root, &target, size, mtime_secs(&meta));
    if let Ok(bytes) = std::fs::read(&cache_path) {
        return Ok(jpeg_response(bytes));
    }

    let ext = extension(&target);
    let bytes = std::fs::read(&target).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let jpeg = if is_fits_ext(&ext) {
        fits_to_jpeg(&bytes, Some(size)).map_err(|e| {
            warn!("FITS thumb failed for {}: {e}", target.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    } else {
        image_resize_jpeg(&bytes, size).map_err(|e| {
            warn!("Image thumb failed for {}: {e}", target.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    };
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&cache_path, &jpeg);
    Ok(jpeg_response(jpeg))
}

fn thumb_cache_path(root: &Path, target: &Path, size: u32, mtime: u64) -> PathBuf {
    let mut h = DefaultHasher::new();
    target.hash(&mut h);
    mtime.hash(&mut h);
    let key = format!("{:016x}_{}.jpg", h.finish(), size);
    root.join(".rekos-thumbs").join(key)
}

fn jpeg_response(bytes: Vec<u8>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "private, max-age=300".parse().unwrap());
    (StatusCode::OK, headers, Bytes::from(bytes)).into_response()
}

// ── /raw ─────────────────────────────────────────────────────────────────────

pub async fn raw(
    State(state): State<AppState>,
    Query(q): Query<RawQ>,
) -> Result<Response, StatusCode> {
    let (_root, target) = resolve(&state, &q.path)?;
    let meta = std::fs::metadata(&target).map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() { return Err(StatusCode::BAD_REQUEST); }

    let ext = extension(&target);
    let want_preview = q.r#as.as_deref() == Some("preview");

    if want_preview && is_fits_ext(&ext) {
        let bytes = std::fs::read(&target).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let jpeg = fits_to_jpeg(&bytes, None).map_err(|e| {
            warn!("FITS preview failed for {}: {e}", target.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        return Ok(jpeg_response(jpeg));
    }

    let bytes = std::fs::read(&target).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mime = mime_guess::from_path(&target)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "private, max-age=60".parse().unwrap());
    Ok((StatusCode::OK, headers, Bytes::from(bytes)).into_response())
}

// ── Image (JPEG/PNG) thumbnail ───────────────────────────────────────────────

fn image_resize_jpeg(bytes: &[u8], max_side: u32) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let resized = img.thumbnail(max_side, max_side);
    let mut out = Vec::new();
    resized
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

// ── FITS header parser ──────────────────────────────────────────────────────
//
// FITS headers are 80-character ASCII records grouped in 2880-byte blocks.
// Records before the data unit either look like
//     KEY     = VALUE / COMMENT
// (with VALUE quoted for strings) or are commentary cards (HISTORY, COMMENT,
// blank key) which we keep for completeness. The header ends at a record whose
// first 8 bytes are `END     ` (rest spaces). Data starts at the next 2880
// boundary.

const FITS_BLOCK: usize = 2880;
const FITS_RECORD: usize = 80;

#[derive(Debug, Clone)]
struct FitsCard {
    key: String,
    value: String,
    comment: String,
}

fn parse_fits_header(bytes: &[u8]) -> Result<Vec<FitsCard>, String> {
    if bytes.len() < FITS_RECORD { return Err("file too short".into()); }
    let mut rows = Vec::new();
    let mut i = 0;
    while i + FITS_RECORD <= bytes.len() {
        let rec = &bytes[i..i + FITS_RECORD];
        let key = std::str::from_utf8(&rec[0..8]).map_err(|e| e.to_string())?.trim().to_string();
        if key == "END" { return Ok(rows); }

        // Commentary cards (HISTORY, COMMENT, blank) — keep raw text.
        if key.is_empty() || key == "HISTORY" || key == "COMMENT" {
            let text = std::str::from_utf8(&rec[8..]).unwrap_or("").trim_end().to_string();
            rows.push(FitsCard { key, value: text, comment: String::new() });
            i += FITS_RECORD;
            continue;
        }

        // Value/comment cards have `= ` at columns 9-10.
        let body = std::str::from_utf8(&rec[10..]).unwrap_or("").trim_end();
        let (value, comment) = split_value_comment(body);
        rows.push(FitsCard {
            key,
            value: value.trim().trim_matches('\'').trim().to_string(),
            comment: comment.trim_start_matches('/').trim().to_string(),
        });
        i += FITS_RECORD;

        // Bound the search — FITS headers are usually < 100 cards but pad up.
        // We stop when END is seen; if a file is malformed and never has END
        // we cap at ~64 KiB to avoid huge memory use.
        if rows.len() > 1024 { return Err("header too long, no END".into()); }
    }
    Err("no END card".into())
}

fn split_value_comment(s: &str) -> (&str, &str) {
    // String values are single-quoted and may contain `/`. Skip past the
    // closing quote first.
    let s = s.trim_start();
    if s.starts_with('\'') {
        let mut iter = s.char_indices();
        iter.next(); // opening quote
        let mut end = None;
        while let Some((idx, c)) = iter.next() {
            if c == '\'' {
                // FITS escapes embedded quote as ''.
                if let Some((_, '\'')) = s[idx + 1..].char_indices().next() {
                    iter.next();
                    continue;
                }
                end = Some(idx + 1);
                break;
            }
        }
        if let Some(e) = end {
            let value = &s[..e];
            let rest = &s[e..];
            if let Some(slash) = rest.find('/') {
                return (value, &rest[slash..]);
            }
            return (value, "");
        }
    }
    if let Some(slash) = s.find('/') {
        (&s[..slash], &s[slash..])
    } else {
        (s, "")
    }
}

fn header_get<'a>(rows: &'a [FitsCard], key: &str) -> Option<&'a str> {
    rows.iter().find(|c| c.key == key).map(|c| c.value.as_str())
}

fn header_f64(rows: &[FitsCard], key: &str) -> Option<f64> {
    header_get(rows, key)?.trim().parse::<f64>().ok()
}

fn header_i64(rows: &[FitsCard], key: &str) -> Option<i64> {
    header_get(rows, key)?.trim().parse::<i64>().ok()
}

fn parse_fits_meta(rows: &[FitsCard]) -> Value {
    let exposure   = header_f64(rows, "EXPTIME").or_else(|| header_f64(rows, "EXPOSURE"));
    let gain       = header_f64(rows, "GAIN");
    let xbin       = header_i64(rows, "XBINNING");
    let ybin       = header_i64(rows, "YBINNING");
    let frame_type = header_get(rows, "FRAME").or_else(|| header_get(rows, "IMAGETYP"))
        .map(|s| s.to_string());
    let filter     = header_get(rows, "FILTER").map(|s| s.to_string());
    let target     = header_get(rows, "OBJECT").map(|s| s.to_string());
    let focal      = header_f64(rows, "FOCALLEN");
    let pixel      = header_f64(rows, "XPIXSZ").or_else(|| header_f64(rows, "PIXSIZE1"));
    let ccd_temp   = header_f64(rows, "CCD-TEMP").or_else(|| header_f64(rows, "CCDTEMP"));
    let ra         = header_f64(rows, "OBJCTRA")
        .or_else(|| header_f64(rows, "RA"))
        .or_else(|| header_f64(rows, "CRVAL1"));
    let dec        = header_f64(rows, "OBJCTDEC")
        .or_else(|| header_f64(rows, "DEC"))
        .or_else(|| header_f64(rows, "CRVAL2"));
    let rotation   = header_f64(rows, "ROTANG").or_else(|| header_f64(rows, "OBJCTROT"));
    let plate_solved = rows.iter().any(|c| c.key == "CD1_1" || c.key == "PC1_1" || c.key == "CRVAL1");

    let binning = match (xbin, ybin) {
        (Some(x), Some(y)) => Some(format!("{}x{}", x, y)),
        (Some(x), None) => Some(format!("{}x{}", x, x)),
        _ => None,
    };

    let nx = header_i64(rows, "NAXIS1");
    let ny = header_i64(rows, "NAXIS2");
    let fov_arcmin = match (focal, pixel, nx, ny) {
        (Some(fl), Some(px), Some(w), Some(h)) if fl > 0.0 => {
            // arcmin per pixel ≈ 206265 * pixel_um*1e-3 / focal_mm / 60
            let arcsec_per_px = 206_265.0 * (px / 1000.0) / fl;
            let arcmin_w = arcsec_per_px * (w as f64) / 60.0;
            let arcmin_h = arcsec_per_px * (h as f64) / 60.0;
            Some(json!({ "w": arcmin_w, "h": arcmin_h }))
        }
        _ => None,
    };

    json!({
        "exposure":     exposure,
        "gain":         gain,
        "binning":      binning,
        "frame_type":   frame_type,
        "filter":       filter,
        "target":       target,
        "focal_length": focal,
        "pixel_size":   pixel,
        "ccd_temp":     ccd_temp,
        "ra":           ra,
        "dec":          dec,
        "fov_arcmin":   fov_arcmin,
        "rotation":     rotation,
        "plate_solved": plate_solved,
        "naxis1":       nx,
        "naxis2":       ny,
    })
}

// ── FITS pixel decode → JPEG (auto-stretched) ───────────────────────────────

fn fits_to_jpeg(bytes: &[u8], max_side: Option<u32>) -> Result<Vec<u8>, String> {
    let rows = parse_fits_header(bytes)?;

    let bitpix = header_i64(&rows, "BITPIX").ok_or("missing BITPIX")?;
    let naxis  = header_i64(&rows, "NAXIS").unwrap_or(0);
    if naxis < 2 { return Err("not a 2D image".into()); }
    let w = header_i64(&rows, "NAXIS1").ok_or("missing NAXIS1")? as usize;
    let h = header_i64(&rows, "NAXIS2").ok_or("missing NAXIS2")? as usize;
    let bzero  = header_f64(&rows, "BZERO").unwrap_or(0.0);
    let bscale = header_f64(&rows, "BSCALE").unwrap_or(1.0);

    // Find the data offset: end of the header in 2880 blocks past the END card.
    let header_end = find_header_end(bytes)?;
    let data_start = ((header_end + FITS_BLOCK - 1) / FITS_BLOCK) * FITS_BLOCK;

    let bytes_per_px = (bitpix.unsigned_abs() as usize) / 8;
    let plane_pixels = w * h;
    let need = data_start + plane_pixels * bytes_per_px;
    if bytes.len() < need {
        return Err(format!("truncated: need {need} have {}", bytes.len()));
    }
    let raw = &bytes[data_start..need];

    // Convert into f32 with BZERO/BSCALE applied.
    let mut floats: Vec<f32> = Vec::with_capacity(plane_pixels);
    match bitpix {
        8 => {
            for &b in raw {
                floats.push((b as f64 * bscale + bzero) as f32);
            }
        }
        16 => {
            for chunk in raw.chunks_exact(2) {
                let v = i16::from_be_bytes([chunk[0], chunk[1]]) as f64;
                floats.push((v * bscale + bzero) as f32);
            }
        }
        32 => {
            for chunk in raw.chunks_exact(4) {
                let v = i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64;
                floats.push((v * bscale + bzero) as f32);
            }
        }
        -32 => {
            for chunk in raw.chunks_exact(4) {
                let v = f32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64;
                floats.push((v * bscale + bzero) as f32);
            }
        }
        -64 => {
            for chunk in raw.chunks_exact(8) {
                let v = f64::from_be_bytes([
                    chunk[0], chunk[1], chunk[2], chunk[3],
                    chunk[4], chunk[5], chunk[6], chunk[7],
                ]);
                floats.push((v * bscale + bzero) as f32);
            }
        }
        other => return Err(format!("unsupported BITPIX {other}")),
    }

    // Auto-stretch via percentile clip on a downsampled view.
    let (lo, hi) = percentile_range(&floats, 0.02, 0.995);
    let span = (hi - lo).max(1e-6);
    let mut u8buf = vec![0u8; plane_pixels];
    for (dst, &v) in u8buf.iter_mut().zip(floats.iter()) {
        let n = ((v - lo) / span).clamp(0.0, 1.0);
        *dst = (n * 255.0) as u8;
    }

    let img = image::GrayImage::from_raw(w as u32, h as u32, u8buf)
        .ok_or("image build failed")?;
    let dyn_img = image::DynamicImage::ImageLuma8(img);
    let final_img = match max_side {
        Some(m) => dyn_img.thumbnail(m, m),
        None => dyn_img,
    };
    let mut out = Vec::new();
    final_img
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn find_header_end(bytes: &[u8]) -> Result<usize, String> {
    let mut i = 0;
    while i + FITS_RECORD <= bytes.len() {
        if &bytes[i..i + 3] == b"END" && bytes[i + 3..i + 8].iter().all(|&b| b == b' ') {
            return Ok(i + FITS_RECORD);
        }
        i += FITS_RECORD;
        if i > 64 * FITS_BLOCK { return Err("no END".into()); }
    }
    Err("no END".into())
}

/// Approximate percentile clip using a downsampled stride to keep this O(N).
fn percentile_range(samples: &[f32], lo_p: f64, hi_p: f64) -> (f32, f32) {
    let stride = (samples.len() / 100_000).max(1);
    let mut sub: Vec<f32> = samples.iter().step_by(stride).copied().collect();
    if sub.is_empty() { return (0.0, 1.0); }
    sub.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lo_idx = ((sub.len() as f64) * lo_p).floor() as usize;
    let hi_idx = (((sub.len() as f64) * hi_p).floor() as usize).min(sub.len() - 1);
    (sub[lo_idx], sub[hi_idx])
}
