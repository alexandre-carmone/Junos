//! Fetching the big static catalogs through the browser's Cache API.
//!
//! `junos.bin` (7 MB), `tycho.bin` (13 MB) and `dso.bin` are downloaded over
//! the observatory's wifi, and the HTTP cache alone does not reliably keep
//! entries that size across reloads (Safari in particular). So they are
//! stored explicitly in a named Cache, keyed by their versioned URL: a
//! regenerated catalog gets a new `?v=` and simply misses, and the stale
//! version is evicted when the new one lands.
//!
//! Every Cache API failure (insecure context, private mode, quota) falls back
//! to a plain fetch — this helper must never be the reason a catalog is
//! missing.

use std::cell::Cell;

use js_sys::{Array, ArrayBuffer, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Cache, Request, Response};

const CACHE_NAME: &str = "junos-catalogs";

thread_local! {
    static PERSIST_ASKED: Cell<bool> = const { Cell::new(false) };
}

/// The bytes at `url`, from the catalog cache when present, else fetched and
/// stored. `None` only when the network fetch itself fails.
pub async fn cached_bytes(url: &str) -> Option<Vec<u8>> {
    ask_persistent_storage();
    match via_cache(url).await {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            debug_log!("[net] cache path failed for {url}: {e:?}; plain fetch");
            plain_fetch(url).await
        }
    }
}

async fn via_cache(url: &str) -> Result<Vec<u8>, JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let caches = window.caches()?;
    let cache: Cache = JsFuture::from(caches.open(CACHE_NAME)).await?.dyn_into()?;

    let hit = JsFuture::from(cache.match_with_str(url)).await?;
    if let Ok(resp) = hit.dyn_into::<Response>() {
        return body_bytes(&resp).await;
    }

    let resp: Response = JsFuture::from(window.fetch_with_str(url)).await?.dyn_into()?;
    if !resp.ok() {
        return Err(JsValue::from_str(&format!("HTTP {}", resp.status())));
    }
    // Store a clone and read the original; a body can only be consumed once.
    if let Ok(stored) = resp.clone() {
        if JsFuture::from(cache.put_with_str(url, &stored)).await.is_ok() {
            evict_other_versions(&cache, url).await;
        }
    }
    body_bytes(&resp).await
}

async fn body_bytes(resp: &Response) -> Result<Vec<u8>, JsValue> {
    let buf: ArrayBuffer = JsFuture::from(resp.array_buffer()?).await?.dyn_into()?;
    Ok(Uint8Array::new(&buf).to_vec())
}

/// Drop every cached entry for the same path with a different `?v=` than
/// `url`, so a bumped catalog doesn't leave a 7 MB stale copy behind.
async fn evict_other_versions(cache: &Cache, url: &str) {
    let path = url.split('?').next().unwrap_or(url);
    let Ok(keys) = JsFuture::from(cache.keys()).await else { return };
    let keys: Array = match keys.dyn_into() {
        Ok(a) => a,
        Err(_) => return,
    };
    for key in keys.iter() {
        let Ok(req) = key.dyn_into::<Request>() else { continue };
        let full = req.url();
        // `Request::url` is absolute; compare on the path + query part.
        let rel = full
            .find("//")
            .and_then(|i| full[i + 2..].find('/').map(|j| &full[i + 2 + j..]))
            .unwrap_or(&full);
        if rel != url && rel.split('?').next() == Some(path) {
            let _ = JsFuture::from(cache.delete_with_str(rel)).await;
        }
    }
}

/// Best-effort `navigator.storage.persist()`, once per page load, so the
/// browser is less eager to evict the catalogs under storage pressure.
fn ask_persistent_storage() {
    if PERSIST_ASKED.with(|c| c.replace(true)) {
        return;
    }
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().storage().persist();
    }
}

async fn plain_fetch(url: &str) -> Option<Vec<u8>> {
    gloo_net::http::Request::get(url)
        .send().await.ok()?
        .binary().await.ok()
}
