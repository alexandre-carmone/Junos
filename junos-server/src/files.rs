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

// ── Helpers for write operations ─────────────────────────────────────────────

/// Resolve a relative path for a write-operation target that may not exist
/// yet (e.g. the new name for a rename). Validates the *parent* is inside
/// the sandbox, and the joined target would also be inside.
fn resolve_new(state: &AppState, rel: &str) -> Result<(PathBuf, PathBuf), StatusCode> {
    let root = state.config.resolved_captures_dir();
    let root = root.canonicalize().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let trimmed = rel.trim_start_matches(['/', '\\']);
    if trimmed.is_empty() { return Err(StatusCode::BAD_REQUEST); }
    let joined = root.join(trimmed);
    // Canonicalize the parent (must exist) and ensure it's inside root.
    let parent = joined.parent().ok_or(StatusCode::BAD_REQUEST)?;
    let parent_canon = parent.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    if !parent_canon.starts_with(&root) {
        return Err(StatusCode::FORBIDDEN);
    }
    let file_name = joined.file_name().ok_or(StatusCode::BAD_REQUEST)?;
    Ok((root, parent_canon.join(file_name)))
}

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

#[derive(Debug, Deserialize)]
pub struct TiltQ {
    #[serde(default)]
    pub path: String,
    /// Mosaic tile side as a percent of image width (KStars default: 25).
    #[serde(default = "default_tile_pct")]
    pub tile_pct: f64,
    /// Minimum stars in a tile for its HFR to be reported (else `null`).
    #[serde(default = "default_min_stars")]
    pub min_stars: usize,
}

fn default_tile_pct() -> f64 { 25.0 }
fn default_min_stars() -> usize { 3 }

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

    // Le cache est indexé par contenu ; l'extension distingue PNG (couleur
    // débayerisée) et JPEG (mono). On sonde les deux avant de décoder.
    let cache_base = thumb_cache_base(&root, &target, size, mtime_secs(&meta));
    let png_cache = cache_base.with_extension("png");
    let jpg_cache = cache_base.with_extension("jpg");
    if let Ok(bytes) = std::fs::read(&png_cache) {
        return Ok(image_response(bytes, true));
    }
    if let Ok(bytes) = std::fs::read(&jpg_cache) {
        return Ok(image_response(bytes, false));
    }

    let ext = extension(&target);
    let bytes = std::fs::read(&target).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (img, is_png) = if is_fits_ext(&ext) {
        fits_to_image(&bytes, Some(size)).map_err(|e| {
            warn!("FITS thumb failed for {}: {e}", target.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    } else {
        (image_resize_jpeg(&bytes, size).map_err(|e| {
            warn!("Image thumb failed for {}: {e}", target.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?, false)
    };
    let cache_path = if is_png { &png_cache } else { &jpg_cache };
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(cache_path, &img);
    Ok(image_response(img, is_png))
}

/// Chemin de cache sans extension : l'appelant ajoute `.png`/`.jpg` selon le
/// format effectivement produit par le décodage.
fn thumb_cache_base(root: &Path, target: &Path, size: u32, mtime: u64) -> PathBuf {
    let mut h = DefaultHasher::new();
    target.hash(&mut h);
    mtime.hash(&mut h);
    let key = format!("{:016x}_{}", h.finish(), size);
    root.join(".junos-thumbs").join(key)
}

fn image_response(bytes: Vec<u8>, is_png: bool) -> Response {
    let ctype = if is_png { "image/png" } else { "image/jpeg" };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, ctype.parse().unwrap());
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
        let (img, is_png) = fits_to_image(&bytes, None).map_err(|e| {
            warn!("FITS preview failed for {}: {e}", target.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        return Ok(image_response(img, is_png));
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

// ── FITS pixel decode → image (auto-stretched) ──────────────────────────────
//
// Renvoie `(octets, is_png)`. Les FITS mono sont auto-étirés en niveaux de gris
// et encodés en JPEG (comportement historique). Les FITS OSC (mosaïque de Bayer,
// carte `BAYERPAT` présente) sont débayerisés en couleur et encodés en PNG pour
// éviter les artefacts JPEG sur des données fortement étirées.

/// Decoded first image plane of a FITS: pixel values as `f32` (BZERO/BSCALE
/// applied), the plane dimensions, the parsed header, and NAXIS. Shared by the
/// preview renderer (`fits_to_image`) and the tilt analyzer (`analyze_tilt`).
struct FitsPlane {
    floats: Vec<f32>,
    w: usize,
    h: usize,
    rows: Vec<FitsCard>,
    naxis: i64,
}

/// Parse the header and decode the first 2D image plane into `f32`. For NAXIS≥3
/// cubes this reads only the first plane (`w*h` samples). Bayer data is left as
/// the raw mosaic — callers that want colour debayer it themselves.
fn decode_fits_plane(bytes: &[u8]) -> Result<FitsPlane, String> {
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

    Ok(FitsPlane { floats, w, h, rows, naxis })
}

fn fits_to_image(bytes: &[u8], max_side: Option<u32>) -> Result<(Vec<u8>, bool), String> {
    let FitsPlane { floats, w, h, rows, naxis } = decode_fits_plane(bytes)?;
    let plane_pixels = w * h;

    // Motif de Bayer : présent ⇒ image couleur OSC à débayeriser. On ne tente
    // rien sur les cubes déjà planaires (NAXIS >= 3).
    let pattern = if naxis >= 3 {
        None
    } else {
        header_get(&rows, "BAYERPAT")
            .map(|p| p.trim().to_ascii_uppercase())
            .filter(|p| matches!(p.as_str(), "RGGB" | "BGGR" | "GRBG" | "GBRG"))
    };

    if let Some(pat) = pattern {
        // ── Couleur : débayerisation super-pixel 2×2 → RGB (w/2 × h/2). ──
        let (rgb, cw, ch) = debayer_superpixel(&floats, w, h, &pat);
        let npx = cw * ch;

        // Auto-étirement par canal : approxime une balance des blancs, sinon
        // les données OSC (vert dominant) tirent vers le vert.
        let mut chan: [Vec<f32>; 3] = [
            Vec::with_capacity(npx),
            Vec::with_capacity(npx),
            Vec::with_capacity(npx),
        ];
        for px in rgb.chunks_exact(3) {
            chan[0].push(px[0]);
            chan[1].push(px[1]);
            chan[2].push(px[2]);
        }
        let ranges: Vec<(f32, f32)> = chan
            .iter()
            .map(|c| {
                let (lo, hi) = percentile_range(c, 0.02, 0.995);
                (lo, (hi - lo).max(1e-6))
            })
            .collect();

        let mut u8buf = vec![0u8; npx * 3];
        for (dst, src) in u8buf.chunks_exact_mut(3).zip(rgb.chunks_exact(3)) {
            for c in 0..3 {
                let (lo, span) = ranges[c];
                let n = ((src[c] - lo) / span).clamp(0.0, 1.0);
                dst[c] = (n * 255.0) as u8;
            }
        }

        let img = image::RgbImage::from_raw(cw as u32, ch as u32, u8buf)
            .ok_or("image build failed")?;
        let dyn_img = image::DynamicImage::ImageRgb8(img);
        let final_img = match max_side {
            Some(m) => dyn_img.thumbnail(m, m),
            None => dyn_img,
        };
        let mut out = Vec::new();
        final_img
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        return Ok((out, true));
    }

    // ── Mono : auto-stretch via percentile clip sur une vue sous-échantillonnée.
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
    Ok((out, false))
}

/// Débayerisation « super-pixel » : chaque cellule de Bayer 2×2 donne un pixel
/// RGB (R, moyenne des deux verts, B). Rapide et sans artefact — idéal pour des
/// aperçus/vignettes. Décalage de Bayer supposé nul (XBAYROFF/YBAYROFF ignorés).
/// Renvoie `(rgb, largeur/2, hauteur/2)` en f32 entrelacé.
fn debayer_superpixel(floats: &[f32], w: usize, h: usize, pattern: &str) -> (Vec<f32>, usize, usize) {
    let cw = w / 2;
    let ch = h / 2;
    // Position (ligne, colonne) de R, G1, G2, B dans la cellule 2×2 :
    //   idx 0 = (0,0), 1 = (0,1), 2 = (1,0), 3 = (1,1)
    let (r, g1, g2, b) = match pattern {
        "RGGB" => (0usize, 1usize, 2usize, 3usize),
        "BGGR" => (3, 1, 2, 0),
        "GRBG" => (1, 0, 3, 2),
        "GBRG" => (2, 0, 3, 1),
        _ => (0, 1, 2, 3),
    };
    let cell = |base: usize, idx: usize| -> f32 {
        let dr = idx / 2;
        let dc = idx % 2;
        floats[base + dr * w + dc]
    };
    let mut rgb = vec![0f32; cw * ch * 3];
    for y in 0..ch {
        for x in 0..cw {
            let base = (y * 2) * w + (x * 2);
            let out = (y * cw + x) * 3;
            rgb[out] = cell(base, r);
            rgb[out + 1] = 0.5 * (cell(base, g1) + cell(base, g2));
            rgb[out + 2] = cell(base, b);
        }
    }
    (rgb, cw, ch)
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

// ── /tilt ────────────────────────────────────────────────────────────────────
//
// Per-tile HFR for the aberration/tilt inspector. Decodes a FITS to its first
// image plane, detects stars, buckets them into the same 3×3 mosaic grid Ekos
// uses (`ImageMosaicMask`), and returns a 2σ-clipped mean HFR per tile. The
// client runs one of these per focuser position, fits a V-curve per tile, and
// derives tilt/backfocus from the per-tile best-focus positions.

/// The 9 mosaic tiles as `(x, y, side)` in pixels, replicating KStars'
/// `ImageMosaicMask::refresh()` layout (index order TL,TM,TR,CL,CM,CR,BL,BM,BR).
fn mosaic_tiles(w: usize, h: usize, tile_pct: f64) -> (usize, [(usize, usize, usize); 9]) {
    let side = ((w as f64) * tile_pct / 100.0).round() as usize;
    let side = side.clamp(1, w.min(h));
    // Middle row/column origin: integer `(dim - side) / 2`, matching KStars'
    // `std::lround((width - tileWidth) / 2)` (integer division truncates).
    let xs = [0usize, w.saturating_sub(side) / 2, w.saturating_sub(side + 1)];
    let ys = [0usize, h.saturating_sub(side) / 2, h.saturating_sub(side + 1)];
    let mut tiles = [(0usize, 0usize, 0usize); 9];
    for row in 0..3 {
        for col in 0..3 {
            tiles[row * 3 + col] = (xs[col], ys[row], side);
        }
    }
    (side, tiles)
}

/// One iterative 2σ-clip pass set; returns the robust mean, or `None` if empty.
fn sigma_clipped_mean(vals: &[f64], sigma: f64) -> Option<f64> {
    if vals.is_empty() { return None; }
    let mut kept: Vec<f64> = vals.to_vec();
    for _ in 0..3 {
        let n = kept.len() as f64;
        if n < 3.0 { break; }
        let mean = kept.iter().sum::<f64>() / n;
        let var = kept.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let sd = var.sqrt();
        if sd <= f64::EPSILON { break; }
        let before = kept.len();
        kept.retain(|v| (v - mean).abs() <= sigma * sd);
        if kept.is_empty() { kept = vals.to_vec(); break; }
        if kept.len() == before { break; }
    }
    Some(kept.iter().sum::<f64>() / kept.len() as f64)
}

/// Analyze a decoded plane into per-tile HFR. Pure (no I/O) so it is unit-tested.
fn analyze_tilt(plane: &[f32], w: usize, h: usize, tile_pct: f64, min_stars: usize) -> Value {
    let stars = crate::starfind::detect_stars(plane, w, h, &crate::starfind::DetectParams::default());
    let (side, tiles) = mosaic_tiles(w, h, tile_pct);

    let mut buckets: [Vec<f64>; 9] = Default::default();
    for s in &stars {
        // First tile (index order) whose rect contains the star centre.
        for (idx, &(tx, ty, ts)) in tiles.iter().enumerate() {
            let sx = s.x;
            let sy = s.y;
            if sx >= tx as f64 && sx < (tx + ts) as f64
                && sy >= ty as f64 && sy < (ty + ts) as f64
            {
                buckets[idx].push(s.hfr);
                break;
            }
        }
    }

    let tiles_json: Vec<Value> = (0..9)
        .map(|idx| {
            let (tx, ty, ts) = tiles[idx];
            let n = buckets[idx].len();
            let hfr = if n >= min_stars {
                sigma_clipped_mean(&buckets[idx], 2.0)
            } else {
                None
            };
            json!({
                "idx": idx,
                "n_stars": n,
                "hfr": hfr,
                "cx": tx as f64 + ts as f64 / 2.0,
                "cy": ty as f64 + ts as f64 / 2.0,
            })
        })
        .collect();

    let all_hfr: Vec<f64> = stars.iter().map(|s| s.hfr).collect();
    json!({
        "naxis1": w,
        "naxis2": h,
        "tile_side_px": side,
        "n_stars_total": stars.len(),
        "overall_hfr": sigma_clipped_mean(&all_hfr, 2.0),
        "tiles": tiles_json,
    })
}

pub async fn tilt(
    State(state): State<AppState>,
    Query(q): Query<TiltQ>,
) -> Result<Json<Value>, StatusCode> {
    let (_root, target) = resolve(&state, &q.path)?;
    let meta = std::fs::metadata(&target).map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() { return Err(StatusCode::BAD_REQUEST); }
    if !is_fits_ext(&extension(&target)) { return Err(StatusCode::BAD_REQUEST); }

    let bytes = std::fs::read(&target).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let plane = decode_fits_plane(&bytes).map_err(|e| {
        warn!("FITS tilt decode failed for {}: {e}", target.display());
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let tile_pct = q.tile_pct.clamp(5.0, 33.0);
    Ok(Json(analyze_tilt(&plane.floats, plane.w, plane.h, tile_pct, q.min_stars.max(1))))
}

// ── /download ────────────────────────────────────────────────────────────────
//
// Streams the raw file bytes with a Content-Disposition: attachment header so
// browsers trigger a download dialog rather than rendering inline.

pub async fn download(
    State(state): State<AppState>,
    Query(q): Query<PathQ>,
) -> Result<Response, StatusCode> {
    let (_root, target) = resolve(&state, &q.path)?;
    let meta = std::fs::metadata(&target).map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() { return Err(StatusCode::BAD_REQUEST); }

    let name = target.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download.bin".to_string());
    // Sanitize: no CR/LF/double-quote/semicolon allowed in the filename header.
    let safe_name: String = name.chars()
        .map(|c| if matches!(c, '"' | ';' | '\r' | '\n') { '_' } else { c })
        .collect();

    let bytes = std::fs::read(&target).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mime = mime_guess::from_path(&target)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime.parse().unwrap_or(
        "application/octet-stream".parse().unwrap()));
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!(r#"attachment; filename="{}""#, safe_name)
            .parse()
            .unwrap_or("attachment".parse().unwrap()),
    );
    Ok((StatusCode::OK, headers, Bytes::from(bytes)).into_response())
}

// ── /rename ──────────────────────────────────────────────────────────────────
//
// Same-directory rename only. Body: { path, new_name }. `new_name` must not
// contain any path separator — rename across directories is refused to keep
// the surface area small.

#[derive(Debug, Deserialize)]
pub struct RenameBody {
    pub path: String,
    pub new_name: String,
}

pub async fn rename(
    State(state): State<AppState>,
    Json(body): Json<RenameBody>,
) -> Result<Json<Value>, StatusCode> {
    if body.new_name.is_empty()
        || body.new_name.contains('/')
        || body.new_name.contains('\\')
        || body.new_name == "."
        || body.new_name == ".."
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let (root, source) = resolve(&state, &body.path)?;
    let parent = source.parent().ok_or(StatusCode::BAD_REQUEST)?;
    let destination = parent.join(&body.new_name);

    // Safety: destination's parent must still be inside the sandbox
    // (canonicalize already confirmed that for source; parent == source.parent).
    if !parent.starts_with(&root) {
        return Err(StatusCode::FORBIDDEN);
    }

    if destination.exists() {
        return Err(StatusCode::CONFLICT);
    }

    std::fs::rename(&source, &destination).map_err(|e| {
        warn!("rename {} → {} failed: {e}", source.display(), destination.display());
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(json!({
        "ok": true,
        "path": relative_to(&root, &destination),
    })))
}

// ── /delete ──────────────────────────────────────────────────────────────────
//
// Deletes a single file, or an empty directory. Recursive deletion is
// refused to keep the damage surface small.

pub async fn delete(
    State(state): State<AppState>,
    Query(q): Query<PathQ>,
) -> Result<Json<Value>, StatusCode> {
    let (_root, target) = resolve(&state, &q.path)?;
    let meta = std::fs::metadata(&target).map_err(|_| StatusCode::NOT_FOUND)?;

    if meta.is_file() {
        std::fs::remove_file(&target).map_err(|e| {
            warn!("delete file {} failed: {e}", target.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    } else if meta.is_dir() {
        // Empty-directory-only. `remove_dir` fails with ENOTEMPTY otherwise.
        std::fs::remove_dir(&target).map_err(|e| {
            warn!("delete dir {} failed: {e}", target.display());
            // If not empty, surface a conflict rather than 500.
            if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                || e.raw_os_error() == Some(39 /* ENOTEMPTY */)
                || e.raw_os_error() == Some(66 /* BSD ENOTEMPTY */)
            {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;
    } else {
        return Err(StatusCode::BAD_REQUEST);
    }

    Ok(Json(json!({ "ok": true })))
}

// ── /resolve ─────────────────────────────────────────────────────────────────
//
// Given an absolute filesystem path, return its sandbox-relative form if it
// falls inside the captures root, or `{in_sandbox:false}` otherwise. Used by
// the Imaging tab's "Reveal in Files" bridge — it only has absolute paths
// reported by KStars and needs the relative form to navigate the browser.

#[derive(Debug, Deserialize)]
pub struct ResolveQ {
    #[serde(default)]
    pub abs: String,
}

pub async fn resolve_abs(
    State(state): State<AppState>,
    Query(q): Query<ResolveQ>,
) -> Json<Value> {
    let root = match state.config.resolved_captures_dir().canonicalize() {
        Ok(r) => r,
        Err(_) => return Json(json!({ "in_sandbox": false })),
    };

    let abs = PathBuf::from(&q.abs);
    // Canonicalize if it exists; otherwise try to canonicalize its parent
    // and re-attach the file name so we can still answer for files that
    // will soon exist (e.g. the next capture output).
    let canonical = abs.canonicalize().or_else(|_| {
        if let (Some(par), Some(fname)) = (abs.parent(), abs.file_name()) {
            par.canonicalize().map(|p| p.join(fname))
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no parent"))
        }
    });

    match canonical {
        Ok(c) if c.starts_with(&root) => {
            let rel = relative_to(&root, &c);
            let parent = c.parent().map(|p| relative_to(&root, p)).unwrap_or_default();
            Json(json!({
                "in_sandbox": true,
                "relative":   rel,
                "parent":     parent,
            }))
        }
        _ => Json(json!({ "in_sandbox": false })),
    }
}

// Keep clippy quiet about the unused helper during development.
#[allow(dead_code)]
fn _ensure_resolve_new_used(state: &AppState, rel: &str) -> Result<(PathBuf, PathBuf), StatusCode> {
    resolve_new(state, rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 8-bit 2D FITS with the given extra cards.
    fn make_fits(w: usize, h: usize, extra: &[(&str, &str)]) -> Vec<u8> {
        fn card(key: &str, value: &str) -> Vec<u8> {
            let text = format!("{key:<8}= {value}");
            let mut rec = text.into_bytes();
            rec.resize(FITS_RECORD, b' ');
            rec
        }
        let mut hdr = Vec::new();
        hdr.extend(card("SIMPLE", "T"));
        hdr.extend(card("BITPIX", "8"));
        hdr.extend(card("NAXIS", "2"));
        hdr.extend(card("NAXIS1", &w.to_string()));
        hdr.extend(card("NAXIS2", &h.to_string()));
        for (k, v) in extra {
            hdr.extend(card(k, v));
        }
        // END card, then pad the header to a 2880 block.
        let mut end = b"END".to_vec();
        end.resize(FITS_RECORD, b' ');
        hdr.extend(end);
        let pad = (hdr.len() + FITS_BLOCK - 1) / FITS_BLOCK * FITS_BLOCK - hdr.len();
        hdr.extend(std::iter::repeat(b' ').take(pad));
        // Ramp pixel data, padded to a full block.
        let mut data: Vec<u8> = (0..w * h).map(|i| (i % 256) as u8).collect();
        let dpad = (data.len() + FITS_BLOCK - 1) / FITS_BLOCK * FITS_BLOCK - data.len();
        data.extend(std::iter::repeat(0u8).take(dpad));
        hdr.extend(data);
        hdr
    }

    #[test]
    fn mono_fits_renders_jpeg() {
        let fits = make_fits(8, 8, &[]);
        let (bytes, is_png) = fits_to_image(&fits, None).unwrap();
        assert!(!is_png);
        assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF])); // JPEG SOI
    }

    #[test]
    fn bayer_fits_debayers_to_png() {
        let fits = make_fits(8, 8, &[("BAYERPAT", "'RGGB    '")]);
        let (bytes, is_png) = fits_to_image(&fits, None).unwrap();
        assert!(is_png);
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G'])); // PNG signature
    }

    #[test]
    fn mosaic_tile_layout_matches_kstars() {
        // 100-wide, 25% ⇒ side 25, corners flush, right col at w-side-1.
        let (side, tiles) = mosaic_tiles(100, 80, 25.0);
        assert_eq!(side, 25);
        assert_eq!(tiles[0], (0, 0, 25));        // TL
        assert_eq!(tiles[2].0, 100 - 25 - 1);    // TR x = w-side-1
        assert_eq!(tiles[6].1, 80 - 25 - 1);     // BL y = h-side-1
        assert_eq!(tiles[4], (37, 27, 25));      // CM: (100-25)/2=37, (80-25)/2=27 (int div)
    }

    #[test]
    fn analyze_tilt_buckets_stars_into_correct_tiles() {
        // Paint one bright star into the top-left and one into the bottom-right
        // tile of a 300×300 frame (25% ⇒ side 75) and check bucketing.
        let (w, h) = (300usize, 300usize);
        let mut plane = vec![100.0f32; w * h];
        let mut paint = |cx: i64, cy: i64| {
            for dy in -3..=3 {
                for dx in -3..=3 {
                    let x = cx + dx;
                    let y = cy + dy;
                    if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 { continue; }
                    let r2 = (dx * dx + dy * dy) as f64;
                    plane[y as usize * w + x as usize] += (3000.0 * (-r2 / 8.0).exp()) as f32;
                }
            }
        };
        // Several well-separated stars per tile so they clear the min_stars
        // gate without merging into one connected blob. TL tile is x,y∈[0,75);
        // BR tile is x,y∈[224,299].
        for &(x, y) in &[(15, 15), (15, 45), (45, 15), (45, 45)] { paint(x, y); }   // TL (idx 0)
        for &(x, y) in &[(240, 240), (240, 270), (270, 240), (270, 270)] { paint(x, y); } // BR (idx 8)
        drop(paint);

        let v = analyze_tilt(&plane, w, h, 25.0, 3);
        let tiles = v["tiles"].as_array().unwrap();
        assert!(tiles[0]["hfr"].is_f64(), "TL tile should have an HFR");
        assert!(tiles[8]["hfr"].is_f64(), "BR tile should have an HFR");
        assert!(tiles[4]["hfr"].is_null(), "empty centre tile should be null");
        assert_eq!(v["tile_side_px"], 75);
    }
}
