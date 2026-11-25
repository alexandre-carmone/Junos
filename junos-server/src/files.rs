//! Captures-folder browser API.
//!
//! Endpoints (mounted under `/api/files`):
//!
//! - `GET /list?path=<rel>`   — directory listing.
//! - `GET /meta?path=<rel>`   — per-file metadata, including parsed FITS header.
//! - `GET /thumb?path=<rel>&size=N[&v=mtime]` — JPEG q90 thumbnail, longest
//!   side ≤ N (64–4096), FITS auto-stretched as KStars does (cached on disk).
//! - `GET /raw?path=<rel>[&as=preview][&v=mtime]` — original bytes, or for a
//!   FITS with `as=preview` the same render at full resolution.
//! - With `v` (the file's mtime), thumb and raw are cached by the browser for
//!   good: the URL names one version of the file.
//! - `GET /download`, `POST /rename`, `DELETE /delete`, `GET /resolve?abs=`,
//!   `POST /mkdir {path}` — see each handler.
//!
//! All paths are resolved relative to `Config::resolved_captures_dir()`. After
//! joining we `canonicalize()` and reject any path that doesn't stay inside
//! the canonical root — covers `..` traversal and symlinks pointing outside.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::UNIX_EPOCH;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tracing::warn;

use crate::AppState;

// ── Captures root ────────────────────────────────────────────────────────────

/// An error carrying both a status and a human-readable reason, so the Files
/// tab can show *why* a listing failed instead of a bare `HTTP 500`.
pub struct ApiErr {
    pub status: StatusCode,
    pub reason: String,
}

impl From<StatusCode> for ApiErr {
    fn from(status: StatusCode) -> Self {
        let reason = status.canonical_reason().unwrap_or("error").to_string();
        ApiErr { status, reason }
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.reason }))).into_response()
    }
}

/// Canonical captures root, or a described error.
///
/// This is the single place the sandbox root is resolved. A missing or
/// unreadable root used to surface as a silent `500` with no log line and no
/// body — the Files tab just rendered empty. Now the reason (path + io error)
/// is logged and handed back to the caller.
fn captures_root(state: &AppState) -> Result<PathBuf, ApiErr> {
    let root = state.config.resolved_captures_dir();
    root.canonicalize().map_err(|e| {
        let reason = format!(
            "captures directory {} is not accessible: {e} \
             (set --captures-dir / CAPTURES_DIR to an existing readable folder)",
            root.display()
        );
        warn!("{reason}");
        ApiErr { status: StatusCode::INTERNAL_SERVER_ERROR, reason }
    })
}

// ── Helpers for write operations ─────────────────────────────────────────────

// ── Query types ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PathQ {
    #[serde(default)]
    pub path: String,
}

/// `v` is the file's mtime as the client knows it: with it, the URL names one
/// version of the file and the response is cached for good.
#[derive(Debug, Deserialize)]
pub struct ThumbQ {
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_thumb_size")]
    pub size: u32,
    #[serde(default)]
    pub v: Option<String>,
}

fn default_thumb_size() -> u32 { 256 }

#[derive(Debug, Deserialize)]
pub struct RawQ {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub r#as: Option<String>,
    #[serde(default)]
    pub v: Option<String>,
}

// ── Sandbox resolution ───────────────────────────────────────────────────────

/// Resolve a user-supplied relative path against the captures root.
/// Returns `(canonical_root, canonical_target)` on success. The caller decides
/// whether the target needs to exist (we run `canonicalize` only when it does).
fn resolve(state: &AppState, rel: &str) -> Result<(PathBuf, PathBuf), StatusCode> {
    let root = captures_root(state).map_err(|e| e.status)?;

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
) -> Result<Json<Value>, ApiErr> {
    let root = captures_root(&state)?;
    let (root, target) = resolve(&state, &q.path).map_err(|s| {
        ApiErr {
            status: s,
            reason: format!("{} : {}", s.canonical_reason().unwrap_or("error"), q.path),
        }
    }).map(|(_, t)| (root, t))?;

    let read = std::fs::read_dir(&target).map_err(|e| ApiErr {
        status: StatusCode::NOT_FOUND,
        reason: format!("cannot read {}: {e}", target.display()),
    })?;

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

/// JPEG quality of every render the Files tab gets (thumbnails and previews).
const PREVIEW_QUALITY: u8 = 90;

/// Render version in the thumbnail cache key: bumped whenever the render
/// changes, so a thumbnail cached by an older one is never served.
const THUMB_RENDER_VERSION: &str = "stf1";

/// Largest thumbnail kept in the disk cache: the grid's sizes. The viewer's
/// larger renders are cached by the browser only (the URL carries `v`), so
/// `.junos-thumbs` doesn't grow by megabytes per file opened.
const THUMB_CACHE_MAX: u32 = 512;

/// Render slots: a FITS cube decodes to hundreds of MB and a grid of
/// thumbnails asks for dozens at once. Two for the grid's thumbnails, one for
/// the viewer's renders, so opening a file doesn't wait behind the grid.
fn render_permits(small: bool) -> Arc<Semaphore> {
    static SMALL: OnceLock<Arc<Semaphore>> = OnceLock::new();
    static LARGE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let slot = if small { &SMALL } else { &LARGE };
    slot.get_or_init(|| Arc::new(Semaphore::new(if small { 2 } else { 1 }))).clone()
}

/// Run a render off the runtime under a permit. The permit moves into the
/// blocking task, so a client that goes away doesn't free it early.
async fn render_blocking<F>(small: bool, f: F) -> Result<Vec<u8>, String>
where
    F: FnOnce() -> Result<Vec<u8>, String> + Send + 'static,
{
    let permit = render_permits(small).acquire_owned().await.map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn thumb(
    State(state): State<AppState>,
    Query(q): Query<ThumbQ>,
) -> Result<Response, StatusCode> {
    let (root, target) = resolve(&state, &q.path)?;
    let meta = std::fs::metadata(&target).map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() { return Err(StatusCode::BAD_REQUEST); }
    let size = q.size.clamp(64, 4096);
    let immutable = q.v.is_some();

    let small = size <= THUMB_CACHE_MAX;
    let cache_path = small.then(|| thumb_cache_path(&root, &target, size, mtime_secs(&meta), meta.len()));
    if let Some(bytes) = cache_path.as_ref().and_then(|p| std::fs::read(p).ok()) {
        // A whole JPEG only (written through a rename, but a disk may still
        // have kept a cut one): anything else is rendered again.
        if bytes.ends_with(&[0xFF, 0xD9]) {
            return Ok(image_response(bytes, immutable));
        }
    }

    let img = render_blocking(small, move || {
        let ext = extension(&target);
        let bytes = std::fs::read(&target).map_err(|e| e.to_string())?;
        let img = if is_fits_ext(&ext) {
            fits_to_image(&bytes, Some(size))
        } else {
            image_resize_jpeg(&bytes, size)
        }
        .map_err(|e| format!("thumb failed for {}: {e}", target.display()))?;
        if let Some(path) = cache_path {
            write_cache(&path, &img);
        }
        Ok(img)
    })
    .await
    .map_err(|e| {
        warn!("{e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(image_response(img, immutable))
}

/// Write a cache file through a temporary one and a rename, so a concurrent
/// read never sees it half written.
fn write_cache(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, path)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Cache file of a thumbnail: the file (path, mtime, length), the size and
/// the render version.
fn thumb_cache_path(root: &Path, target: &Path, size: u32, mtime: u64, len: u64) -> PathBuf {
    let mut h = DefaultHasher::new();
    target.hash(&mut h);
    mtime.hash(&mut h);
    len.hash(&mut h);
    let key = format!("{:016x}_{}_{THUMB_RENDER_VERSION}.jpg", h.finish(), size);
    root.join(".junos-thumbs").join(key)
}

/// `Cache-Control` of a file response: for good when the URL carries `v`.
fn cache_control(immutable: bool, default: &'static str) -> header::HeaderValue {
    header::HeaderValue::from_static(if immutable { "private, max-age=31536000, immutable" } else { default })
}

fn image_response(bytes: Vec<u8>, immutable: bool) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("image/jpeg"));
    headers.insert(header::CACHE_CONTROL, cache_control(immutable, "private, max-age=300"));
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
    let immutable = q.v.is_some();

    if want_preview && is_fits_ext(&ext) {
        // Full resolution (up to the JPEG limit), not cached on disk.
        let img = render_blocking(false, move || {
            let bytes = std::fs::read(&target).map_err(|e| e.to_string())?;
            fits_to_image(&bytes, Some(u16::MAX as u32))
                .map_err(|e| format!("FITS preview failed for {}: {e}", target.display()))
        })
        .await
        .map_err(|e| {
            warn!("{e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        return Ok(image_response(img, immutable));
    }

    let bytes = std::fs::read(&target).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mime = mime_guess::from_path(&target)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
    headers.insert(header::CACHE_CONTROL, cache_control(immutable, "private, max-age=60"));
    Ok((StatusCode::OK, headers, Bytes::from(bytes)).into_response())
}

// ── Image (JPEG/PNG) thumbnail ───────────────────────────────────────────────

fn image_resize_jpeg(bytes: &[u8], max_side: u32) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let img = if img.width().max(img.height()) > max_side { img.thumbnail(max_side, max_side) } else { img };
    // The JPEG encoder takes 8-bit grey or RGB only (no alpha, no 16-bit).
    let img = match img.color() {
        image::ColorType::L8 | image::ColorType::Rgb8 => img,
        image::ColorType::L16 | image::ColorType::La8 | image::ColorType::La16 => {
            image::DynamicImage::ImageLuma8(img.to_luma8())
        }
        _ => image::DynamicImage::ImageRgb8(img.to_rgb8()),
    };
    encode_jpeg(&img, PREVIEW_QUALITY)
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
// Les FITS sont étirés comme KStars les affiche : l'auto-stretch STF (preset
// 1 de `Stretch::computeParams`), par canal et non lié. Les FITS mono en
// niveaux de gris ; les FITS OSC (mosaïque de Bayer, carte `BAYERPAT`)
// débayerisés en super-pixel ; les cubes RGB (NAXIS3 = 3, les empilements
// couleur du Live Stacker) en couleur. Une pose de calibration (dark, bias /
// offset) reste en gris même avec `BAYERPAT` : débayerisée, elle ne serait
// que des confettis.

/// Decoded image planes of a FITS: pixel values as `f32` (BZERO/BSCALE
/// applied), plane after plane, the plane dimensions, the number of planes
/// read (1, or 3 for an RGB cube), the parsed header, NAXIS and BITPIX. Used
/// by the preview renderer (`fits_to_dyn_image`).
struct FitsPlane {
    floats: Vec<f32>,
    w: usize,
    h: usize,
    planes: usize,
    rows: Vec<FitsCard>,
    naxis: i64,
    bitpix: i64,
}

/// Parse the header and decode the image into `f32`: the first 2D plane, or
/// the three planes of a NAXIS≥3 cube with `NAXIS3 = 3` (an RGB image, as
/// KStars' Live Stacker writes its colour stacks). Bayer data is left as the
/// raw mosaic — callers that want colour debayer it themselves.
fn decode_fits_plane(bytes: &[u8]) -> Result<FitsPlane, String> {
    let rows = parse_fits_header(bytes)?;

    let bitpix = header_i64(&rows, "BITPIX").ok_or("missing BITPIX")?;
    let naxis  = header_i64(&rows, "NAXIS").unwrap_or(0);
    if naxis < 2 { return Err("not a 2D image".into()); }
    let w = header_i64(&rows, "NAXIS1").ok_or("missing NAXIS1")? as usize;
    let h = header_i64(&rows, "NAXIS2").ok_or("missing NAXIS2")? as usize;
    let planes = if naxis >= 3 && header_i64(&rows, "NAXIS3") == Some(3) { 3 } else { 1 };
    let bzero  = header_f64(&rows, "BZERO").unwrap_or(0.0);
    let bscale = header_f64(&rows, "BSCALE").unwrap_or(1.0);

    // Find the data offset: end of the header in 2880 blocks past the END card.
    let header_end = find_header_end(bytes)?;
    let data_start = ((header_end + FITS_BLOCK - 1) / FITS_BLOCK) * FITS_BLOCK;

    let bytes_per_px = (bitpix.unsigned_abs() as usize) / 8;
    let samples = w * h * planes;
    let need = data_start + samples * bytes_per_px;
    if bytes.len() < need {
        return Err(format!("truncated: need {need} have {}", bytes.len()));
    }
    let raw = &bytes[data_start..need];

    // Convert into f32 with BZERO/BSCALE applied.
    let mut floats: Vec<f32> = Vec::with_capacity(samples);
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

    Ok(FitsPlane { floats, w, h, planes, rows, naxis, bitpix })
}

/// The stretch's input range, KStars' `getRange` and `recalculateInputRange`
/// (stretch.cpp:288-310, 365-376): 256 for 8-bit, 65536 otherwise, 1.0 for a
/// float frame whose sampled max (every 1000th pixel) is ≤ 1.01.
///
/// Deliberate deviation for BITPIX 32: KStars never stretches it sensibly
/// (65536 whatever the data); a stack summed past 16 bits takes its sampled
/// max instead, so it isn't one white sheet.
fn stretch_input_range(bitpix: i64, data: &[f32]) -> f32 {
    // `sampledMax` (stretch.cpp:28-37): starts at 0, a NaN never wins.
    let sampled_max = || {
        data.iter().step_by(1000).copied().filter(|v| v.is_finite()).fold(0f32, f32::max)
    };
    match bitpix {
        8 => 256.0,
        -32 | -64 if sampled_max() <= 1.01 => 1.0,
        32 => sampled_max().max(65536.0),
        _ => 65536.0,
    }
}

/// One channel's STF: shadows, midtones and highlights on a 0–1 scale.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stf {
    shadows: f32,
    midtones: f32,
    highlights: f32,
}

/// KStars' preset 1 (`setupStretchPreset`): target background B, clipping C.
const STF_B: f32 = 0.25;
const STF_C: f32 = 2.8;

/// `computeParamsOneChannel` (stretch.cpp:227-282): median and MAD over at
/// most ~500k strided samples, then PixInsight's STF (XISF spec 8.5.7).
/// Non-finite samples are skipped (KStars would sort them in).
fn stf_params(plane: &[f32], range: f32) -> Stf {
    let n = plane.len();
    let step = if n < 500_000 { 1 } else { n / 500_000 };
    let mut s: Vec<f32> = plane
        .iter()
        .step_by(step)
        .take(n / step)
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    if s.is_empty() {
        return Stf { shadows: 0.0, midtones: 0.5, highlights: 1.0 };
    }
    // `median` (stretch.cpp:21-26): nth_element at size / 2.
    let mid = s.len() / 2;
    let median = *s.select_nth_unstable_by(mid, f32::total_cmp).1;
    for v in s.iter_mut() {
        *v = (*v - median).abs();
    }
    let mad = *s.select_nth_unstable_by(mid, f32::total_cmp).1;

    let nm = median / range;
    let madn = 1.4826 * mad / range;
    let upper = nm > 0.5;
    let shadows = if upper || madn == 0.0 { 0.0 } else { (nm - STF_C * madn).clamp(0.0, 1.0) };
    let highlights = if !upper || madn == 0.0 { 1.0 } else { (nm + STF_C * madn).clamp(0.0, 1.0) };
    let (x, m) = if !upper { (nm - shadows, STF_B) } else { (STF_B, highlights - nm) };
    let midtones = if x == 0.0 {
        0.0
    } else if x == m {
        0.5
    } else if x == 1.0 {
        1.0
    } else {
        ((m - 1.0) * x) / ((2.0 * m - 1.0) * x - m)
    };
    Stf { shadows, midtones, highlights }
}

/// An `Stf` turned into the per-pixel map of `stretchOneChannel`
/// (stretch.cpp:58-112): shadows and highlights in ADU on `range − 1`.
struct StfMap {
    lo: f32,
    hi: f32,
    inv_span: f32,
    m: f32,
}

impl StfMap {
    fn new(stf: Stf, range: f32) -> Self {
        let max_in = if range > 1.0 { range - 1.0 } else { range };
        let span = if stf.highlights == stf.shadows { 1.0 } else { stf.highlights - stf.shadows };
        StfMap {
            lo: stf.shadows * max_in,
            hi: stf.highlights * max_in,
            inv_span: 1.0 / (span * max_in),
            m: stf.midtones,
        }
    }

    /// One raw value → 0..=255: the midtones transfer function. A
    /// non-finite pixel, and the 0/0 of midtones = 0 at the shadows, give 0.
    fn map(&self, x: f32) -> u8 {
        if !x.is_finite() || x < self.lo {
            return 0;
        }
        if x >= self.hi {
            return 255;
        }
        let y = (x - self.lo) * self.inv_span;
        let m = self.m;
        let out = 255.0 * (m - 1.0) * y / ((2.0 * m - 1.0) * y - m);
        // Truncated, as KStars assigns the float to its `uint8_t`.
        if out.is_finite() { out.clamp(0.0, 255.0) as u8 } else { 0 }
    }
}

/// Three planar channels, each stretched on its own (KStars' unlinked
/// stretch), straight into the interleaved RGB image.
fn stretch_rgb(chans: [&[f32]; 3], w: usize, h: usize, range: f32) -> Result<image::RgbImage, String> {
    let mut u8buf = vec![0u8; w * h * 3];
    for (c, plane) in chans.iter().enumerate() {
        let map = StfMap::new(stf_params(plane, range), range);
        for (dst, &v) in u8buf.chunks_exact_mut(3).zip(plane.iter()) {
            dst[c] = map.map(v);
        }
    }
    image::RgbImage::from_raw(w as u32, h as u32, u8buf).ok_or_else(|| "image build failed".into())
}

/// A calibration frame (dark, bias / offset) by its `IMAGETYP` or `FRAME`.
fn is_calibration_frame(rows: &[FitsCard]) -> bool {
    ["IMAGETYP", "FRAME"].iter().any(|k| {
        header_get(rows, k).is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("dark") || v.contains("bias") || v.contains("offset")
        })
    })
}

/// The Bayer pattern of the top-left 2×2 cell as stored, from `BAYERPAT`
/// corrected as KStars' `checkDebayerFITS` (fitsdata.cpp:7797-7913):
/// `ROWORDER = BOTTOM-UP` on an even height swaps the rows, an odd
/// `XBAYROFF` the columns, an odd `YBAYROFF` the rows (once: KStars' OpenCV
/// path corrects it twice).
fn bayer_pattern(rows: &[FitsCard], h: usize) -> Option<String> {
    let mut pat = header_get(rows, "BAYERPAT")?
        .trim_matches(|c: char| c == '\'' || c.is_whitespace())
        .to_ascii_uppercase();
    if !matches!(pat.as_str(), "RGGB" | "BGGR" | "GRBG" | "GBRG") {
        return None;
    }
    let swap_rows = |p: &str| match p {
        "RGGB" => "GBRG", "GBRG" => "RGGB", "GRBG" => "BGGR", _ => "GRBG",
    }.to_string();
    let swap_cols = |p: &str| match p {
        "RGGB" => "GRBG", "GRBG" => "RGGB", "GBRG" => "BGGR", _ => "GBRG",
    }.to_string();
    let bottom_up = header_get(rows, "ROWORDER")
        .is_some_and(|o| o.trim().eq_ignore_ascii_case("BOTTOM-UP"));
    if bottom_up && h.is_multiple_of(2) {
        pat = swap_rows(&pat);
    }
    if header_i64(rows, "XBAYROFF").is_some_and(|x| x.rem_euclid(2) == 1) {
        pat = swap_cols(&pat);
    }
    if header_i64(rows, "YBAYROFF").is_some_and(|y| y.rem_euclid(2) == 1) {
        pat = swap_rows(&pat);
    }
    Some(pat)
}

/// The auto-stretched picture of a FITS, at most `max_side` px when given:
/// RGB for a Bayer mosaic (débayerisation super-pixel 2×2, w/2 × h/2) or a
/// three-plane cube, grey otherwise.
fn fits_to_dyn_image(bytes: &[u8], max_side: Option<u32>) -> Result<image::DynamicImage, String> {
    let FitsPlane { floats, w, h, planes, rows, naxis, bitpix } = decode_fits_plane(bytes)?;
    let n = w * h;
    // `recalculateInputRange` samples the first plane only.
    let range = stretch_input_range(bitpix, &floats[..n]);

    // Motif de Bayer : présent ⇒ image couleur OSC à débayeriser. Rien sur
    // les cubes déjà planaires (NAXIS >= 3) ni sur les darks / bias.
    let pattern = if naxis >= 3 || is_calibration_frame(&rows) {
        None
    } else {
        bayer_pattern(&rows, h)
    };

    let img = if planes == 3 {
        let (r, gb) = floats.split_at(n);
        let (g, b) = gb.split_at(n);
        image::DynamicImage::ImageRgb8(stretch_rgb([r, g, b], w, h, range)?)
    } else if let Some(pat) = pattern {
        let ([r, g, b], cw, ch) = debayer_superpixel(&floats, w, h, &pat);
        drop(floats);
        image::DynamicImage::ImageRgb8(stretch_rgb([&r, &g, &b], cw, ch, range)?)
    } else {
        let map = StfMap::new(stf_params(&floats, range), range);
        let u8buf: Vec<u8> = floats.iter().map(|&v| map.map(v)).collect();
        let img = image::GrayImage::from_raw(w as u32, h as u32, u8buf)
            .ok_or("image build failed")?;
        image::DynamicImage::ImageLuma8(img)
    };

    // `thumbnail` fits the box both ways; never upscale a small frame.
    Ok(match max_side {
        Some(m) if img.width().max(img.height()) > m => img.thumbnail(m, m),
        _ => img,
    })
}

/// JPEG of a grey or RGB 8-bit image.
fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    img.write_with_encoder(enc).map_err(|e| e.to_string())?;
    Ok(out)
}

/// A FITS as a JPEG at most `max_side` px, with its dimensions: the live
/// stack's preview (`stack.rs`). JPEG whatever the colour, since the lite
/// re-encode only handles JPEG (`kstars_ws::lite_jpeg`).
pub(crate) fn fits_to_jpeg(bytes: &[u8], max_side: u32, quality: u8) -> Result<(Vec<u8>, u32, u32), String> {
    let img = fits_to_dyn_image(bytes, Some(max_side))?;
    Ok((encode_jpeg(&img, quality)?, img.width(), img.height()))
}

/// The Files tab's render of a FITS: JPEG q90, at most `max_side` px when
/// given.
fn fits_to_image(bytes: &[u8], max_side: Option<u32>) -> Result<Vec<u8>, String> {
    encode_jpeg(&fits_to_dyn_image(bytes, max_side)?, PREVIEW_QUALITY)
}

/// Débayerisation « super-pixel » : chaque cellule de Bayer 2×2 donne un pixel
/// RGB (R, moyenne des deux verts, B). Rapide et sans artefact — idéal pour des
/// aperçus/vignettes. `pattern` est déjà corrigé des décalages
/// (`bayer_pattern`). Renvoie les trois plans `[r, g, b]` en f32, largeur/2 ×
/// hauteur/2.
fn debayer_superpixel(floats: &[f32], w: usize, h: usize, pattern: &str) -> ([Vec<f32>; 3], usize, usize) {
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
    let mut out = [vec![0f32; cw * ch], vec![0f32; cw * ch], vec![0f32; cw * ch]];
    for y in 0..ch {
        for x in 0..cw {
            let base = (y * 2) * w + (x * 2);
            let i = y * cw + x;
            out[0][i] = cell(base, r);
            out[1][i] = 0.5 * (cell(base, g1) + cell(base, g2));
            out[2][i] = cell(base, b);
        }
    }
    (out, cw, ch)
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

// ── /mkdir ───────────────────────────────────────────────────────────────────
//
// Creates a folder (and its parents) inside the captures root. The Imaging
// tab's live stack needs its stacking folder before `livestacker_start`:
// KStars' `FITSDirWatcher` fails silently on a missing one and only creates
// the *output* folder itself (`fitsview.cpp:809`). Idempotent.

#[derive(Debug, Deserialize)]
pub struct MkdirBody {
    pub path: String,
}

pub async fn mkdir(
    State(state): State<AppState>,
    Json(body): Json<MkdirBody>,
) -> Result<Json<Value>, StatusCode> {
    let root = captures_root(&state).map_err(|e| e.status)?;
    let rel = Path::new(body.path.trim_start_matches(['/', '\\']));
    if rel.as_os_str().is_empty()
        || !rel.components().all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let target = root.join(rel);
    std::fs::create_dir_all(&target).map_err(|e| {
        warn!("mkdir {} failed: {e}", target.display());
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // A symlink among the parents could have led outside; check the result.
    let canonical = target.canonicalize().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !canonical.starts_with(&root) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(json!({ "ok": true, "path": relative_to(&root, &canonical) })))
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
    let root = match captures_root(&state) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn card(key: &str, value: &str) -> Vec<u8> {
        let text = format!("{key:<8}= {value}");
        let mut rec = text.into_bytes();
        rec.resize(FITS_RECORD, b' ');
        rec
    }

    /// A minimal FITS, a 2D image or a cube of `planes`, with the given
    /// extra cards and big-endian pixel `data`.
    fn fits_bytes(bitpix: i64, w: usize, h: usize, planes: usize, extra: &[(&str, &str)], mut data: Vec<u8>) -> Vec<u8> {
        let mut hdr = Vec::new();
        hdr.extend(card("SIMPLE", "T"));
        hdr.extend(card("BITPIX", &bitpix.to_string()));
        hdr.extend(card("NAXIS", if planes > 1 { "3" } else { "2" }));
        hdr.extend(card("NAXIS1", &w.to_string()));
        hdr.extend(card("NAXIS2", &h.to_string()));
        if planes > 1 {
            hdr.extend(card("NAXIS3", &planes.to_string()));
        }
        for (k, v) in extra {
            hdr.extend(card(k, v));
        }
        // END card, then pad the header to a 2880 block.
        let mut end = b"END".to_vec();
        end.resize(FITS_RECORD, b' ');
        hdr.extend(end);
        let pad = (hdr.len() + FITS_BLOCK - 1) / FITS_BLOCK * FITS_BLOCK - hdr.len();
        hdr.extend(std::iter::repeat(b' ').take(pad));
        // Pixel data, padded to a full block.
        let dpad = (data.len() + FITS_BLOCK - 1) / FITS_BLOCK * FITS_BLOCK - data.len();
        data.extend(std::iter::repeat(0u8).take(dpad));
        hdr.extend(data);
        hdr
    }

    /// An 8-bit ramp image or cube.
    fn make_fits(w: usize, h: usize, planes: usize, extra: &[(&str, &str)]) -> Vec<u8> {
        let data = (0..w * h * planes).map(|i| (i % 256) as u8).collect();
        fits_bytes(8, w, h, planes, extra, data)
    }

    /// Deterministic noise of roughly unit sigma (Irwin–Hall, 12 uniforms).
    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f32 {
            let mut sum = 0.0;
            for _ in 0..12 {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                sum += (self.0 >> 40) as f32 / (1u64 << 24) as f32;
            }
            sum - 6.0
        }
    }

    /// Unsigned 16-bit pixels (BZERO = 32768) as FITS stores them.
    fn u16_data(values: impl Iterator<Item = f32>) -> Vec<u8> {
        values
            .flat_map(|v| ((v.round().clamp(0.0, 65535.0) as i32 - 32768) as i16).to_be_bytes())
            .collect()
    }

    fn median_u8(mut v: Vec<u8>) -> u8 {
        let mid = v.len() / 2;
        *v.select_nth_unstable(mid).1
    }

    #[test]
    fn mono_fits_renders_jpeg() {
        let fits = make_fits(8, 8, 1, &[]);
        let bytes = fits_to_image(&fits, None).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF])); // JPEG SOI
    }

    #[test]
    fn bayer_fits_debayers_to_jpeg() {
        let fits = make_fits(8, 8, 1, &[("BAYERPAT", "'RGGB    '")]);
        let bytes = fits_to_image(&fits, None).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
        let img = fits_to_dyn_image(&fits, None).unwrap();
        assert!(matches!(img, image::DynamicImage::ImageRgb8(_)));
        assert_eq!((img.width(), img.height()), (4, 4));
    }

    #[test]
    fn rgb_cube_fits_renders_colour() {
        let fits = make_fits(8, 8, 3, &[]);
        let img = fits_to_dyn_image(&fits, None).unwrap();
        assert!(matches!(img, image::DynamicImage::ImageRgb8(_)));
        assert_eq!((img.width(), img.height()), (8, 8));
        assert!(fits_to_image(&fits, None).unwrap().starts_with(&[0xFF, 0xD8, 0xFF]));
    }

    #[test]
    fn fits_to_jpeg_scales_and_reports_the_size() {
        let fits = make_fits(16, 8, 3, &[]);
        let (bytes, w, h) = fits_to_jpeg(&fits, 8, 80).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert_eq!((w, h), (8, 4));
        let (_, w, h) = fits_to_jpeg(&make_fits(8, 8, 1, &[]), 1920, 80).unwrap();
        assert_eq!((w, h), (8, 8));
    }

    #[test]
    fn stf_params_match_hand_computed_values() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        // Lower half, shadows clamped to 0: median 30, MAD 10 on a 100 range.
        let p = stf_params(&[10.0, 20.0, 30.0, 40.0, 50.0], 100.0);
        assert!(close(p.shadows, 0.0) && close(p.highlights, 1.0), "{p:?}");
        assert!(close(p.midtones, 0.5625), "{p:?}"); // (-0.75·0.3)/(-0.5·0.3 − 0.25)
        // Upper half: median 80, MAD 10 → X = B, M = 1 − 0.8.
        let p = stf_params(&[60.0, 70.0, 80.0, 90.0, 100.0], 100.0);
        assert!(close(p.shadows, 0.0) && close(p.highlights, 1.0), "{p:?}");
        assert!(close(p.midtones, 0.2 / 0.35), "{p:?}");
        // A sky background: median 1000, MAD 5 on 65536.
        let p = stf_params(&[1000.0, 1010.0, 990.0, 1005.0, f32::NAN, 995.0], 65536.0);
        assert!((p.shadows - 0.014_942_07).abs() < 1e-7, "{p:?}");
        assert!((p.midtones - 0.000_949_55).abs() < 1e-7, "{p:?}");
        assert_eq!(p.highlights, 1.0);
        // A flat plane (MAD 0): no clipping.
        let p = stf_params(&[5.0; 10], 256.0);
        assert_eq!((p.shadows, p.highlights), (0.0, 1.0));
    }

    #[test]
    fn mono_background_lands_on_a_quarter() {
        let (w, h) = (200, 200);
        let mut noise = Noise(1);
        let data = u16_data((0..w * h).map(|_| 1000.0 + 20.0 * noise.next()));
        let fits = fits_bytes(16, w, h, 1, &[("BZERO", "32768"), ("BSCALE", "1")], data);
        let img = fits_to_dyn_image(&fits, None).unwrap();
        let image::DynamicImage::ImageLuma8(g) = img else { panic!("not grey") };
        let med = median_u8(g.into_raw()) as i32;
        assert!((med - 64).abs() <= 15, "median {med}");
    }

    #[test]
    fn rgb_cube_background_is_neutral() {
        let (w, h) = (150, 100);
        let mut noise = Noise(7);
        let bgs = [(800.0, 15.0), (1500.0, 25.0), (3000.0, 40.0)];
        let values: Vec<f32> = bgs
            .iter()
            .flat_map(|&(bg, sigma)| (0..w * h).map(|_| bg + sigma * noise.next()).collect::<Vec<_>>())
            .collect();
        let fits = fits_bytes(16, w, h, 3, &[("BZERO", "32768")], u16_data(values.into_iter()));
        let image::DynamicImage::ImageRgb8(rgb) = fits_to_dyn_image(&fits, None).unwrap() else {
            panic!("not RGB")
        };
        let raw = rgb.into_raw();
        let meds: Vec<i32> = (0..3)
            .map(|c| median_u8(raw.iter().skip(c).step_by(3).copied().collect()) as i32)
            .collect();
        let spread = meds.iter().max().unwrap() - meds.iter().min().unwrap();
        assert!(spread <= 3, "channel medians {meds:?}");
    }

    #[test]
    fn bayer_dark_stays_grey_and_light_is_debayered() {
        let (w, h) = (64, 48);
        let mut noise = Noise(3);
        let data = u16_data((0..w * h).map(|_| 500.0 + 10.0 * noise.next()));
        let render = |imagetyp: &str| {
            let fits = fits_bytes(16, w, h, 1, &[
                ("BZERO", "32768"),
                ("BAYERPAT", "'GRBG'"),
                ("IMAGETYP", imagetyp),
            ], data.clone());
            fits_to_dyn_image(&fits, None).unwrap()
        };
        let dark = render("'Dark Frame'");
        assert!(matches!(dark, image::DynamicImage::ImageLuma8(_)));
        assert_eq!((dark.width(), dark.height()), (64, 48));
        let light = render("'Light Frame'");
        assert!(matches!(light, image::DynamicImage::ImageRgb8(_)));
        assert_eq!((light.width(), light.height()), (32, 24));
        let flat = render("'Flat Field'");
        assert!(matches!(flat, image::DynamicImage::ImageRgb8(_)));
    }

    #[test]
    fn bayer_pattern_follows_roworder_and_offsets() {
        let rows = |cards: &[(&str, &str)]| -> Vec<FitsCard> {
            let mut fits = Vec::new();
            for (k, v) in cards {
                fits.extend(card(k, v));
            }
            let mut end = b"END".to_vec();
            end.resize(FITS_RECORD, b' ');
            fits.extend(end);
            parse_fits_header(&fits).unwrap()
        };
        let pat = |cards: &[(&str, &str)], h| bayer_pattern(&rows(cards), h);
        assert_eq!(pat(&[("BAYERPAT", "' rggb '")], 10).as_deref(), Some("RGGB"));
        assert_eq!(pat(&[("BAYERPAT", "'RGGB'"), ("ROWORDER", "'BOTTOM-UP'")], 10).as_deref(), Some("GBRG"));
        assert_eq!(pat(&[("BAYERPAT", "'RGGB'"), ("ROWORDER", "'BOTTOM-UP'")], 11).as_deref(), Some("RGGB"));
        assert_eq!(pat(&[("BAYERPAT", "'GRBG'"), ("XBAYROFF", "1")], 10).as_deref(), Some("RGGB"));
        assert_eq!(pat(&[("BAYERPAT", "'BGGR'"), ("YBAYROFF", "1")], 10).as_deref(), Some("GRBG"));
        assert_eq!(pat(&[("BAYERPAT", "'XYZW'")], 10), None);
    }

    #[test]
    fn float_frame_with_nans_maps_them_to_zero() {
        let (w, h) = (40, 30);
        let mut noise = Noise(5);
        let values: Vec<f32> = (0..w * h)
            .map(|i| if i % 7 == 0 { f32::NAN } else { 0.1 + 0.01 * noise.next() })
            .collect();
        let data = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        let fits = fits_bytes(-32, w, h, 1, &[], data);
        let image::DynamicImage::ImageLuma8(g) = fits_to_dyn_image(&fits, None).unwrap() else {
            panic!("not grey")
        };
        let raw = g.into_raw();
        assert!(raw.iter().step_by(7).all(|&v| v == 0));
        let med = median_u8(raw.iter().enumerate().filter(|(i, _)| i % 7 != 0).map(|(_, &v)| v).collect()) as i32;
        assert!((med - 64).abs() <= 15, "median {med}");
        assert_eq!(stretch_input_range(-32, &values), 1.0);
    }
}
