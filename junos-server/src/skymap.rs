//! Offline sky map: a local HiPS mirror of the DSS2 colour survey, served
//! at `/api/skymap/*` for the planetarium's imagery, the Framing Assistant
//! and the Targets thumbnails.
//!
//! The directory (`--skymap-dir`, default `.cache/skymap`) is written by
//! `uv run scripts/fetch_skymap.py` and keeps CDS's exact HiPS layout:
//! `properties`, then one 512 px JPEG per HEALPix cell at
//! `Norder{k}/Dir{(n / 10000) * 10000}/Npix{n}.jpg`. The browser computes
//! which cells sit under its view at the order matching its zoom and asks for
//! those tiles only — never the whole mirror — so a tile is served with a
//! year-long `immutable` policy (the survey never changes) and a strong ETag,
//! while `info.json` (what the mirror holds, `max_order` above all) is
//! `no-cache` + weak ETag so a reload revalidates to a 304.
//!
//! The mirror is optional: without it `info` answers `max_order: null` and
//! the sky draws symbols over the Milky Way panorama alone. With
//! `--skymap-online` a tile the mirror lacks is fetched from the CDS servers
//! on demand and stored, so the map works before (or beyond) the download.

use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::AppState;

/// Validator bound: the HiPS standard allows up to 29, DSS2 colour stops at 9.
pub const MAX_ORDER: u32 = 12;
/// Orders the on-demand fetch will ask CDS for (`hips_order` of the survey).
const SURVEY_MAX_ORDER: u32 = 9;
const SURVEY_PATH: &str = "DSS/DSSColor";
const MIRRORS: [&str; 3] = [
    "https://alasky.cds.unistra.fr",
    "https://alaskybis.cds.unistra.fr",
    "https://alasky.u-strasbg.fr",
];
/// An order counts as complete from this fraction of its `12·4^k` tiles.
const COMPLETE_FRACTION: f64 = 0.99;
/// Below this a tile file is a truncated write, not a tile.
const MIN_TILE_BYTES: u64 = 1024;
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const A_DAY: &str = "public, max-age=86400";
const NO_CACHE: &str = "no-cache";
/// Concurrent CDS fetches in online mode — a phone panning across an
/// unmirrored region must not turn into hundreds of parallel requests.
const ONLINE_PARALLEL: usize = 4;
const ONLINE_TIMEOUT: Duration = Duration::from_secs(20);
const ONLINE_WARN_EVERY: Duration = Duration::from_secs(60);

pub fn ntiles(order: u32) -> u64 {
    12u64 << (2 * order)
}

// ── validator ────────────────────────────────────────────────────────────────

/// `("Norder7", "Dir190000", "Npix196607.jpg")` → `Some((7, 196607))`.
///
/// The three segments come straight off the wire; the on-disk path is
/// rebuilt from the parsed numbers, never from these strings, so neither `..`
/// nor an alias like `Npix01.jpg` (a different file name for the same tile)
/// can get through.
pub fn parse_tile(norder: &str, dir: &str, npix: &str) -> Option<(u32, u64)> {
    let order = canonical_digits(norder.strip_prefix("Norder")?)?;
    if order > MAX_ORDER as u64 {
        return None;
    }
    let order = order as u32;
    let npix = canonical_digits(npix.strip_prefix("Npix")?.strip_suffix(".jpg")?)?;
    if npix >= ntiles(order) {
        return None;
    }
    let dir = canonical_digits(dir.strip_prefix("Dir")?)?;
    if dir != (npix / 10000) * 10000 {
        return None;
    }
    Some((order, npix))
}

/// Plain decimal digits in their canonical spelling (no sign, no leading
/// zero, no `+`, nothing a parser would accept but a file name wouldn't).
fn canonical_digits(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 12 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: u64 = s.parse().ok()?;
    (v.to_string() == s).then_some(v)
}

pub fn tile_rel(order: u32, npix: u64) -> String {
    format!("Norder{order}/Dir{}/Npix{npix}.jpg", (npix / 10000) * 10000)
}

// ── what the mirror holds ────────────────────────────────────────────────────

/// The `info.json` shape — identical whether the script wrote the file or
/// the server derived it from a directory scan.
#[derive(Debug, Clone, Serialize)]
pub struct SkymapInfo {
    pub survey:           String,
    pub title:            String,
    pub frame:            String,
    pub tile_width:       u32,
    pub tile_format:      String,
    pub survey_max_order: u32,
    /// Highest order such that every order up to it is complete — the zoom
    /// the client may rely on everywhere. `None` when even order 0 is not.
    pub max_order:        Option<u32>,
    pub orders:           BTreeMap<String, u64>,
    pub bytes:            u64,
    pub allsky:           bool,
    pub attribution:      String,
    pub attribution_url:  String,
    pub updated:          String,
}

/// Highest `k` with orders `0..=k` all at least `COMPLETE_FRACTION` full.
pub fn compute_max_order(counts: &BTreeMap<u32, u64>) -> Option<u32> {
    let mut best = None;
    let mut k = 0u32;
    while let Some(&n) = counts.get(&k) {
        if (n as f64) < COMPLETE_FRACTION * ntiles(k) as f64 {
            break;
        }
        best = Some(k);
        k += 1;
    }
    best
}

/// Walk `Norder*/Dir*` once and build the fallback `info`. A 262k-file
/// order-7 tree takes a few hundred ms; it runs once at startup.
pub fn scan(dir: &FsPath) -> SkymapInfo {
    let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
    let mut bytes = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(order) = name.to_str().and_then(|n| n.strip_prefix("Norder")).and_then(|n| n.parse::<u32>().ok()) else { continue };
            let Ok(dirs) = std::fs::read_dir(e.path()) else { continue };
            let mut n = 0u64;
            for d in dirs.flatten() {
                if !d.file_name().to_str().is_some_and(|s| s.starts_with("Dir")) {
                    continue;
                }
                let Ok(tiles) = std::fs::read_dir(d.path()) else { continue };
                for t in tiles.flatten() {
                    let ok_name = t.file_name().to_str().is_some_and(|s| s.starts_with("Npix") && s.ends_with(".jpg"));
                    if !ok_name {
                        continue;
                    }
                    if let Ok(m) = t.metadata() {
                        if m.len() > MIN_TILE_BYTES {
                            n += 1;
                            bytes += m.len();
                        }
                    }
                }
            }
            counts.insert(order, n);
        }
    }
    let props = std::fs::read_to_string(dir.join("properties"))
        .map(|s| parse_properties(&s))
        .unwrap_or_default();
    let get = |k: &str, d: &str| props.get(k).cloned().unwrap_or_else(|| d.to_string());
    SkymapInfo {
        survey: get("creator_did", "ivo://CDS/P/DSS2/color").trim_start_matches("ivo://").to_string(),
        title: get("obs_title", "DSS colored"),
        frame: get("hips_frame", "equatorial"),
        tile_width: props.get("hips_tile_width").and_then(|v| v.parse().ok()).unwrap_or(512),
        tile_format: "jpg".into(),
        survey_max_order: props.get("hips_order").and_then(|v| v.parse().ok()).unwrap_or(SURVEY_MAX_ORDER),
        max_order: compute_max_order(&counts),
        orders: counts.iter().map(|(k, n)| (k.to_string(), *n)).collect(),
        bytes,
        allsky: dir.join("allsky_small.jpg").is_file(),
        attribution: get("obs_copyright", "Digitized Sky Survey - STScI/NASA, Colored & Healpixed by CDS")
            .trim_matches('"')
            .to_string(),
        attribution_url: get("obs_copyright_url", "http://archive.stsci.edu/dss/copyright.html"),
        updated: iso_now(),
    }
}

fn parse_properties(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SSZ` for now, without pulling in a date crate
/// (days-to-civil from Howard Hinnant's algorithms).
fn iso_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", sod / 3600, (sod % 3600) / 60, sod % 60)
}

// ── shared state ─────────────────────────────────────────────────────────────

struct Online {
    client:    reqwest::Client,
    sem:       Semaphore,
    last_warn: Mutex<BTreeMap<u32, Instant>>,
    counter:   AtomicU64,
}

/// Built once at startup: the directory scan (served when `info.json` is
/// missing) and, with `--skymap-online`, the CDS fetch-through.
pub struct SkymapState {
    fallback: SkymapInfo,
    online:   Option<Online>,
}

impl SkymapState {
    /// Scan the mirror and log what it holds, so an empty or half-copied
    /// directory shows in the journal rather than as a sky without imagery.
    pub fn new(dir: &FsPath, online: bool) -> Self {
        let fallback = scan(dir);
        if fallback.orders.is_empty() {
            info!(
                "sky map dir: {} (empty — run `uv run scripts/fetch_skymap.py`){}",
                dir.display(),
                if online { "; missing tiles fetched from CDS on demand" } else { "" }
            );
        } else {
            let orders: Vec<String> = fallback
                .orders
                .iter()
                .map(|(k, n)| {
                    let want = k.parse::<u32>().map(ntiles).unwrap_or(0);
                    if *n >= want { k.clone() } else { format!("{k}: {n}/{want}") }
                })
                .collect();
            info!(
                "sky map dir: {} (orders {}, max_order {}, {}, Milky Way {}{})",
                dir.display(),
                orders.join(", "),
                fallback.max_order.map(|k| k.to_string()).unwrap_or_else(|| "none".into()),
                if fallback.bytes >= 1_000_000_000 {
                    format!("{:.1} GB", fallback.bytes as f64 / 1e9)
                } else {
                    format!("{:.0} MB", fallback.bytes as f64 / 1e6)
                },
                if fallback.allsky { "yes" } else { "no" },
                if online { ", online fill" } else { "" }
            );
        }
        let online = online.then(|| Online {
            client: reqwest::Client::builder()
                .timeout(ONLINE_TIMEOUT)
                .user_agent(concat!("junos-web/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("reqwest client"),
            sem: Semaphore::new(ONLINE_PARALLEL),
            last_warn: Mutex::new(BTreeMap::new()),
            counter: AtomicU64::new(0),
        });
        Self { fallback, online }
    }
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// `GET /api/skymap/info` — `info.json` as the script wrote it (its mtime
/// moves while a fetch runs), else the startup scan. Always 200.
pub async fn info(State(state): State<AppState>, req: HeaderMap) -> Response {
    let path = state.config.resolved_skymap_dir().join("info.json");
    match serve_file(&path, "application/json", NO_CACHE, true, &req).await {
        Some(r) => r,
        None => {
            let body = serde_json::to_vec(&state.skymap.fallback).unwrap_or_else(|_| b"{}".to_vec());
            (StatusCode::OK, headers("application/json", NO_CACHE, None), Bytes::from(body)).into_response()
        }
    }
}

/// `GET /api/skymap/properties` — CDS's survey description, so a generic
/// HiPS client pointed at `/api/skymap/` reads the mirror.
pub async fn properties(State(state): State<AppState>, req: HeaderMap) -> Response {
    let path = state.config.resolved_skymap_dir().join("properties");
    serve_file(&path, "text/plain; charset=utf-8", NO_CACHE, true, &req)
        .await
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// `GET /api/skymap/allsky.jpg` — the Milky Way panorama (regenerable, a day).
pub async fn allsky(State(state): State<AppState>, req: HeaderMap) -> Response {
    panorama(&state, "allsky.jpg", &req).await
}

/// `GET /api/skymap/allsky_small.jpg` — its 2048 px copy for phones / Lite.
pub async fn allsky_small(State(state): State<AppState>, req: HeaderMap) -> Response {
    panorama(&state, "allsky_small.jpg", &req).await
}

async fn panorama(state: &AppState, name: &str, req: &HeaderMap) -> Response {
    let path = state.config.resolved_skymap_dir().join(name);
    serve_file(&path, "image/jpeg", A_DAY, false, req)
        .await
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// `GET /api/skymap/:norder/:dir/:npix` — one tile.
pub async fn tile(
    State(state): State<AppState>,
    Path((norder, dir, npix)): Path<(String, String, String)>,
    req: HeaderMap,
) -> Response {
    let Some((order, n)) = parse_tile(&norder, &dir, &npix) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let path = state.config.resolved_skymap_dir().join(tile_rel(order, n));
    if let Some(r) = serve_file(&path, "image/jpeg", IMMUTABLE, false, &req).await {
        return r;
    }
    let Some(online) = state.skymap.online.as_ref().filter(|_| order <= SURVEY_MAX_ORDER) else {
        // Cacheable: the client probes above the complete order (a `--cone`
        // run's tiles), so a miss should cost one request a day, not one a pan.
        return (StatusCode::NOT_FOUND, headers("text/plain", "public, max-age=86400", None), "no such tile")
            .into_response();
    };
    match fetch_online(online, order, n, &path).await {
        Some(bytes) => (StatusCode::OK, headers("image/jpeg", IMMUTABLE, None), bytes).into_response(),
        None => (StatusCode::BAD_GATEWAY, headers("text/plain", "no-store", None), "sky map tile unavailable").into_response(),
    }
}

/// Fetch a tile from the first mirror that has it, store it in the tree
/// (`.part` then rename — two requests racing on one tile are harmless) and
/// return its bytes. A 404 everywhere means the survey has no such tile.
async fn fetch_online(online: &Online, order: u32, npix: u64, path: &PathBuf) -> Option<Bytes> {
    let _permit = online.sem.acquire().await.ok()?;
    let rel = tile_rel(order, npix);
    let mut last_err = String::new();
    for m in MIRRORS {
        let url = format!("{m}/{SURVEY_PATH}/{rel}");
        match online.client.get(&url).send().await {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => return None,
            Ok(resp) if resp.status().is_success() => {
                let jpeg = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|ct| ct.starts_with("image/jpeg"));
                match resp.bytes().await {
                    Ok(b) if jpeg && b.len() as u64 > MIN_TILE_BYTES => {
                        if let Some(parent) = path.parent() {
                            let _ = tokio::fs::create_dir_all(parent).await;
                        }
                        let tmp = path.with_extension(format!(
                            "jpg.{}-{}.part",
                            std::process::id(),
                            online.counter.fetch_add(1, Ordering::Relaxed)
                        ));
                        if tokio::fs::write(&tmp, &b).await.is_ok() && tokio::fs::rename(&tmp, path).await.is_err() {
                            let _ = tokio::fs::remove_file(&tmp).await;
                        }
                        return Some(b);
                    }
                    Ok(b) => last_err = format!("{url}: not a tile ({} bytes, jpeg {jpeg})", b.len()),
                    Err(e) => last_err = format!("{url}: {e}"),
                }
            }
            Ok(resp) => last_err = format!("{url}: HTTP {}", resp.status()),
            Err(e) => last_err = format!("{url}: {e}"),
        }
    }
    // One line a minute per order, not one per tile of a pan.
    let mut warned = online.last_warn.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    if warned.get(&order).is_none_or(|t| now.duration_since(*t) >= ONLINE_WARN_EVERY) {
        warned.insert(order, now);
        warn!("sky map online fetch failed (order {order}): {last_err}");
    }
    None
}

// ── file serving ─────────────────────────────────────────────────────────────

/// Serve a file with a `(mtime, len)` ETag and a 304 on `If-None-Match`;
/// `None` when it does not exist (the caller decides what that means).
async fn serve_file(
    path: &FsPath,
    content_type: &str,
    cache_control: &'static str,
    weak: bool,
    req: &HeaderMap,
) -> Option<Response> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let etag = etag_for(mtime, meta.len(), weak);
    if none_match(req, &etag) {
        return Some((StatusCode::NOT_MODIFIED, headers(content_type, cache_control, Some(&etag))).into_response());
    }
    let body = tokio::fs::read(path).await.ok()?;
    Some((StatusCode::OK, headers(content_type, cache_control, Some(&etag)), Bytes::from(body)).into_response())
}

/// `"<mtime-secs>-<len>"`, prefixed `W/` for a weak validator.
pub fn etag_for(mtime_secs: u64, len: u64, weak: bool) -> String {
    format!("{}\"{mtime_secs}-{len}\"", if weak { "W/" } else { "" })
}

/// `If-None-Match` lists the ETags the browser holds; `*` matches anything.
pub fn none_match(req: &HeaderMap, etag: &str) -> bool {
    req.get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
        .unwrap_or(false)
}

fn headers(content_type: &str, cache_control: &'static str, etag: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Ok(ct) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, ct);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
    if let Some(tag) = etag.and_then(|t| HeaderValue::from_str(t).ok()) {
        h.insert(header::ETAG, tag);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_hips_paths() {
        assert_eq!(parse_tile("Norder3", "Dir0", "Npix767.jpg"), Some((3, 767)));
        assert_eq!(parse_tile("Norder7", "Dir190000", "Npix196607.jpg"), Some((7, 196607)));
        assert_eq!(parse_tile("Norder0", "Dir0", "Npix0.jpg"), Some((0, 0)));
        assert_eq!(tile_rel(7, 196607), "Norder7/Dir190000/Npix196607.jpg");
    }

    #[test]
    fn rejects_out_of_range_and_odd_shapes() {
        for (o, d, n) in [
            ("Norder3", "Dir0", "Npix768.jpg"),        // 12·4^3 = 768 tiles: 0..=767
            ("Norder3", "Dir10000", "Npix0.jpg"),      // wrong Dir
            ("Norder7", "Dir190000", "Npix186607.jpg"), // Dir must be floor(n/1e4)·1e4
            ("Norder13", "Dir0", "Npix0.jpg"),         // above MAX_ORDER
            ("Norder3", "Dir0", "Npix1.png"),
            ("Norder3", "Dir0", "Npix01.jpg"),         // alias spelling
            ("Norder03", "Dir0", "Npix1.jpg"),
            ("Norder3", "Dir00", "Npix1.jpg"),
            ("norder3", "Dir0", "Npix1.jpg"),          // case
            ("Norder3", "Dir0", "Npix+1.jpg"),
            ("Norder-1", "Dir0", "Npix1.jpg"),
            ("Norder3", "..", "Npix1.jpg"),
            ("Norder3", "Dir0", "../Npix1.jpg"),
            ("", "", ""),
            ("Norder", "Dir", "Npix.jpg"),
        ] {
            assert_eq!(parse_tile(o, d, n), None, "should reject {o}/{d}/{n}");
        }
    }

    #[test]
    fn etag_format_and_matching() {
        assert_eq!(etag_for(1700000000, 31777, false), "\"1700000000-31777\"");
        assert_eq!(etag_for(1700000000, 31777, true), "W/\"1700000000-31777\"");
        let tag = etag_for(1, 2, false);
        let mut h = HeaderMap::new();
        assert!(!none_match(&h, &tag));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"9-9\", \"1-2\""));
        assert!(none_match(&h, &tag));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(none_match(&h, &tag));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"1-3\""));
        assert!(!none_match(&h, &tag));
    }

    fn counts(v: &[(u32, u64)]) -> BTreeMap<u32, u64> {
        v.iter().copied().collect()
    }

    #[test]
    fn max_order_is_the_contiguous_complete_order() {
        let full: Vec<(u32, u64)> = (0..=7).map(|k| (k, ntiles(k))).collect();
        assert_eq!(compute_max_order(&counts(&full)), Some(7));
        // 98 % at order 7 → 6.
        let mut v = full.clone();
        v[7].1 = (ntiles(7) as f64 * 0.98) as u64;
        assert_eq!(compute_max_order(&counts(&v)), Some(6));
        assert_eq!(compute_max_order(&counts(&[])), None);
        assert_eq!(compute_max_order(&counts(&[(0, 11)])), None);
        // A gap at 4 stops the count even if 5 is complete.
        assert_eq!(compute_max_order(&counts(&[(0, 12), (1, 48), (2, 192), (3, 768), (5, ntiles(5))])), Some(3));
    }

    #[test]
    fn properties_parse_and_dates_format() {
        let p = parse_properties("# comment\nhips_order = 9\nobs_title=DSS colored\n\nbroken line\n");
        assert_eq!(p.get("hips_order").map(String::as_str), Some("9"));
        assert_eq!(p.get("obs_title").map(String::as_str), Some("DSS colored"));
        assert_eq!(p.len(), 2);
        let s = iso_now();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z') && &s[4..5] == "-" && &s[10..11] == "T");
    }
}
