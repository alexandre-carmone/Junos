//! Files tab: typed wrappers around the server's `/api/files/*` endpoints.

use serde_json::json;

use super::types::{FileMeta, ListReply, ResolveReply};
use super::utils::{is_image_ext, url_encode};

pub(super) async fn fetch_list(path: &str) -> Result<ListReply, String> {
    let url = format!("/api/files/list?path={}", url_encode(path));
    let resp = gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        let status = resp.status();
        // The server sends `{"error": "..."}` on failure — show that reason
        // rather than a bare status the user can do nothing with.
        let detail = resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()));
        return Err(match detail {
            Some(d) => format!("HTTP {status} — {d}"),
            None => format!("HTTP {status}"),
        });
    }
    resp.json::<ListReply>().await.map_err(|e| e.to_string())
}

pub(super) async fn fetch_meta(path: &str) -> Result<FileMeta, String> {
    let url = format!("/api/files/meta?path={}", url_encode(path));
    let resp = gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    resp.json::<FileMeta>().await.map_err(|e| e.to_string())
}

pub(super) async fn rename_file(path: &str, new_name: &str) -> Result<(), String> {
    let resp = gloo_net::http::Request::post("/api/files/rename")
        .json(&json!({ "path": path, "new_name": new_name })).map_err(|e| e.to_string())?
        .send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    Ok(())
}

pub(super) async fn delete_file(path: &str) -> Result<(), String> {
    let url = format!("/api/files/delete?path={}", url_encode(path));
    let resp = gloo_net::http::Request::delete(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    Ok(())
}

pub(super) async fn resolve_abs(abs: &str) -> Result<ResolveReply, String> {
    let url = format!("/api/files/resolve?abs={}", url_encode(abs));
    let resp = gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    resp.json::<ResolveReply>().await.map_err(|e| e.to_string())
}

/// Absolute server-side path of a sandbox-relative `rel`. `/api/config`
/// reports the canonical captures root — the same root `/api/files/*`
/// resolves `rel` against — so joining the two names the file on the host
/// KStars runs on.
pub(super) async fn abs_path_of(rel: &str) -> Result<String, String> {
    let resp = gloo_net::http::Request::get("/api/config").send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    let v = resp.json::<serde_json::Value>().await.map_err(|e| e.to_string())?;
    let root = v.get("captures_dir").and_then(|d| d.as_str()).filter(|d| !d.is_empty())
        .ok_or_else(|| "captures_dir missing from /api/config".to_string())?;
    Ok(format!("{}/{}", root.trim_end_matches('/'), rel.trim_start_matches('/')))
}

pub(super) async fn newest_image_in_abs_dir(abs: &str) -> Result<Option<String>, String> {
    let resolved = resolve_abs(abs).await?;
    if !resolved.in_sandbox {
        return Err("Path is outside the captures sandbox — preview unavailable".to_string());
    }
    let path = if resolved.relative.is_empty() { resolved.parent } else { resolved.relative };
    let mut entries = fetch_list(&path).await?.entries;
    entries.retain(|e| e.kind == "file" && is_image_ext(&e.ext));
    entries.sort_by(|a, b| a.mtime.cmp(&b.mtime));
    Ok(entries.pop().map(|e| if path.is_empty() { e.name } else { format!("{}/{}", path, e.name) }))
}
