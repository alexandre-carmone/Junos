//! Files tab: typed wrappers around the server's `/api/files/*` endpoints.

use gloo_net::http::{Request, Response};
use serde_json::json;

use super::types::{FileMeta, ListReply, ResolveReply};
use super::utils::{is_image_ext, join, url_encode};

/// Pass a 2xx through; otherwise the status plus the server's
/// `{"error": "..."}` reason when it sends one.
pub(super) async fn checked(resp: Result<Response, gloo_net::Error>) -> Result<Response, String> {
    let resp = resp.map_err(|e| e.to_string())?;
    if resp.ok() {
        return Ok(resp);
    }
    let status = resp.status();
    let detail = resp.json::<serde_json::Value>().await.ok()
        .and_then(|v| v["error"].as_str().map(str::to_string));
    Err(match detail {
        Some(d) => format!("HTTP {status} \u{2014} {d}"),
        None => format!("HTTP {status}"),
    })
}

pub(super) async fn fetch_list(path: &str) -> Result<ListReply, String> {
    let url = format!("/api/files/list?path={}", url_encode(path));
    checked(Request::get(&url).send().await).await?.json().await.map_err(|e| e.to_string())
}

pub(super) async fn fetch_meta(path: &str) -> Result<FileMeta, String> {
    let url = format!("/api/files/meta?path={}", url_encode(path));
    checked(Request::get(&url).send().await).await?.json().await.map_err(|e| e.to_string())
}

pub(super) async fn rename_file(path: &str, new_name: &str) -> Result<(), String> {
    let req = Request::post("/api/files/rename")
        .json(&json!({ "path": path, "new_name": new_name }))
        .map_err(|e| e.to_string())?;
    checked(req.send().await).await.map(drop)
}

pub(super) async fn delete_file(path: &str) -> Result<(), String> {
    let url = format!("/api/files/delete?path={}", url_encode(path));
    checked(Request::delete(&url).send().await).await.map(drop)
}

pub(super) async fn resolve_abs(abs: &str) -> Result<ResolveReply, String> {
    let url = format!("/api/files/resolve?abs={}", url_encode(abs));
    checked(Request::get(&url).send().await).await?.json().await.map_err(|e| e.to_string())
}

/// The newest image in an absolute host folder (the Live Stack output), as a
/// sandbox-relative path with its mtime; `outside` is the error for a folder
/// out of reach.
pub(super) async fn newest_image_in_abs_dir(abs: &str, outside: &str) -> Result<Option<(String, u64)>, String> {
    let resolved = resolve_abs(abs).await?;
    if !resolved.in_sandbox {
        return Err(outside.to_string());
    }
    let reply = fetch_list(&resolved.relative).await?;
    Ok(reply.entries.iter()
        .filter(|e| e.kind == "file" && is_image_ext(&e.ext))
        .max_by_key(|e| e.mtime)
        .map(|e| (join(&reply.path, &e.name), e.mtime)))
}
