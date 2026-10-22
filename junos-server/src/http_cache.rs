//! Browser-cache policy for the static frontend.
//!
//! The field use case is a phone on a weak wifi link, so a page reload must
//! not re-download the ~15 MB the app is made of. `ServeDir` sends no
//! `Cache-Control` at all, which leaves the browser guessing; this middleware
//! adds one keyed on the request path, and only where the handler set none.
//!
//! - Trunk hashes the wasm / js / css file names, so those are immutable.
//! - The three catalogs (`*.bin`) keep their names but the client fetches them
//!   with a `?v=N` query it bumps whenever a catalog is regenerated
//!   (`catalog.rs`, `star_tiles.rs`, `dso_catalog.rs`), so they are immutable
//!   per URL too.
//! - `index.html` must always be revalidated or a deploy would never show up.
//!
//! `/api/*` responses are left to their handlers (`skymap.rs`, `files.rs`
//! set their own policy).

use axum::{
    extract::Request,
    http::{header, HeaderValue},
    middleware::Next,
    response::Response,
};

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";

/// The `Cache-Control` value for a request path, or `None` to leave the
/// response as the handler built it.
pub fn cache_policy(path: &str) -> Option<&'static str> {
    if path.starts_with("/api/") {
        return None;
    }
    if path == "/" || path.ends_with("/index.html") {
        return Some(REVALIDATE);
    }
    let immutable = [".wasm", ".js", ".css", ".bin"]
        .iter()
        .any(|ext| path.ends_with(ext));
    immutable.then_some(IMMUTABLE)
}

/// Axum middleware: applies [`cache_policy`] when the response carries no
/// `Cache-Control` of its own.
pub async fn apply(req: Request, next: Next) -> Response {
    let policy = cache_policy(req.uri().path());
    let mut res = next.run(req).await;
    if let Some(policy) = policy {
        if !res.headers().contains_key(header::CACHE_CONTROL) {
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(policy));
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_bundle_and_catalogs_are_immutable() {
        for p in [
            "/junos-web-dba32136777594a3_bg.wasm",
            "/junos-web-dba32136777594a3.js",
            "/tailwind-c6ac4bd014007a73.css",
            "/junos.bin",
            "/tycho.bin",
            "/dso.bin",
        ] {
            assert_eq!(cache_policy(p), Some(IMMUTABLE), "{p}");
        }
    }

    #[test]
    fn the_page_itself_is_revalidated() {
        assert_eq!(cache_policy("/"), Some(REVALIDATE));
        assert_eq!(cache_policy("/index.html"), Some(REVALIDATE));
    }

    #[test]
    fn api_and_unknown_files_are_left_alone() {
        assert_eq!(cache_policy("/api/skymap/info"), None);
        assert_eq!(cache_policy("/api/skymap/Norder3/Dir0/Npix0.jpg"), None);
        assert_eq!(cache_policy("/api/files/raw"), None);
        assert_eq!(cache_policy("/favicon.ico"), None);
    }
}
