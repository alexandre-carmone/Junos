//! Offline DSO tile cache for the browser's Framing Assistant.
//!
//! `GET /api/dso_tiles/index.json` lists the pre-downloaded survey cutouts;
//! `GET /api/dso_tiles/<file>.jpg` serves one, and
//! `GET /api/dso_tiles/thumbs/<file>.jpg` its planetarium sprite (the same
//! cutout at 512 px, background-subtracted and vignetted). The directory is
//! populated ahead of time by `uv run scripts/prefetch_dso_tiles.py` and is
//! *not* part of the repo — it holds ~10 GB of JPEGs. The same route also
//! serves the all-sky Milky Way panorama (`allsky.jpg`, `allsky_small.jpg`).
//!
//! This is what lets framing work with no internet: the client stamps every
//! cached tile that overlaps the zone it wants to draw, and leaves the rest
//! black. A missing cache directory is therefore not an error — it just
//! serves an empty index.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AppState;

/// Serve `index.json`, or an empty index when the cache was never populated —
/// the client treats "no tiles" and "no cache dir" identically.
pub async fn index(State(state): State<AppState>) -> Response {
    let path = state.config.resolved_dso_tile_dir().join("index.json");
    let body = std::fs::read(&path).unwrap_or_else(|_| b"[]".to_vec());

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "private, max-age=300".parse().unwrap());
    (StatusCode::OK, headers, Bytes::from(body)).into_response()
}

/// Serve one cached cutout by file name.
pub async fn tile(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, StatusCode> {
    // The name comes straight off the wire and is joined onto a real path, so
    // accept only the shape the prefetch script emits (`<slug>.jpg`). This
    // rejects `..`, absolute paths, and separators without needing to
    // canonicalise and compare roots.
    if !is_safe_tile_name(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let path = state.config.resolved_dso_tile_dir().join(&name);
    let bytes = std::fs::read(&path).map_err(|_| StatusCode::NOT_FOUND)?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
    // Tiles are immutable once fetched — a slug always denotes the same sky.
    headers.insert(header::CACHE_CONTROL, "private, max-age=86400".parse().unwrap());
    Ok((StatusCode::OK, headers, Bytes::from(bytes)).into_response())
}

/// Serve one planetarium sprite (`thumbs/<slug>.jpg`) by file name.
pub async fn thumb(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, StatusCode> {
    if !is_safe_tile_name(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = state.config.resolved_dso_tile_dir().join("thumbs").join(&name);
    let bytes = std::fs::read(&path).map_err(|_| StatusCode::NOT_FOUND)?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "private, max-age=86400".parse().unwrap());
    Ok((StatusCode::OK, headers, Bytes::from(bytes)).into_response())
}

/// `<slug>.jpg` where slug is the `[a-z0-9]+` form written by
/// `scripts/prefetch_dso_tiles.py::slug()`, plus `_` for the script's two
/// panorama files. No separators or dots, so no traversal is possible.
fn is_safe_tile_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jpg") else { return false };
    !stem.is_empty()
        && stem.len() <= 64
        && stem.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::is_safe_tile_name;

    #[test]
    fn accepts_prefetch_slugs() {
        assert!(is_safe_tile_name("m31.jpg"));
        assert!(is_safe_tile_name("ngc7000.jpg"));
    }

    #[test]
    fn accepts_the_allsky_panoramas() {
        // Served through the tile route; the name check must let them through.
        assert!(is_safe_tile_name("allsky.jpg"));
        assert!(is_safe_tile_name("allsky_small.jpg"));
    }

    #[test]
    fn rejects_traversal_and_odd_shapes() {
        for bad in [
            "../../etc/passwd",
            "../secret.jpg",
            "a/b.jpg",
            "/abs/path.jpg",
            "M31.jpg",     // slugs are lowercase
            "m31.png",
            "m31",
            ".jpg",
            "m 31.jpg",
        ] {
            assert!(!is_safe_tile_name(bad), "should reject {bad:?}");
        }
    }
}
