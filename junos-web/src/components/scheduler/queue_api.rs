//! Scheduler: typed wrappers around junos-server's `/api/taskqueue/*`
//! endpoints (startup/shutdown collections and their shell scripts).
//!
//! Names are [`super::queue_model::is_safe_name`] slugs — checked before any
//! call — so they go into the URL without encoding.

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct QueueList {
    pub collections_dir: String,
    pub scripts_dir: String,
    pub queues: Vec<QueueEntry>,
    pub scripts: Vec<ScriptEntry>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ScriptEntry {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct QueueEntry {
    pub name: String,
    /// Absolute path on the KStars host — the value a slot is set to.
    pub path: String,
    /// The collection's `name` field.
    pub title: Option<String>,
    /// `None` when the file isn't a `tasks` collection this editor can load.
    pub tasks: Option<usize>,
}

pub enum SaveErr {
    /// A queue of that name exists and `overwrite` was false.
    Exists,
    Failed(String),
}

/// `HTTP 400 — <reason>` from the server's `{"error": …}` body.
async fn failure(resp: gloo_net::http::Response) -> String {
    let status = resp.status();
    let detail = resp
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string));
    match detail {
        Some(d) => format!("HTTP {status} — {d}"),
        None => format!("HTTP {status}"),
    }
}

fn saved_path(v: &Value) -> Result<String, String> {
    v.get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "server reply has no path".to_string())
}

pub async fn fetch_list() -> Result<QueueList, String> {
    let resp = gloo_net::http::Request::get("/api/taskqueue/list")
        .send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(failure(resp).await); }
    resp.json::<QueueList>().await.map_err(|e| e.to_string())
}

pub async fn fetch_queue(name: &str) -> Result<Value, String> {
    let resp = gloo_net::http::Request::get(&format!("/api/taskqueue/queue/{name}"))
        .send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(failure(resp).await); }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

pub async fn fetch_script(name: &str) -> Result<String, String> {
    let resp = gloo_net::http::Request::get(&format!("/api/taskqueue/script/{name}"))
        .send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(failure(resp).await); }
    resp.text().await.map_err(|e| e.to_string())
}

/// Write `<collections>/<name>.json`; returns its absolute path.
pub async fn save_queue(name: &str, content: &Value, overwrite: bool) -> Result<String, SaveErr> {
    let resp = gloo_net::http::Request::put(&format!("/api/taskqueue/queue/{name}"))
        .json(&json!({ "content": content, "overwrite": overwrite }))
        .map_err(|e| SaveErr::Failed(e.to_string()))?
        .send().await.map_err(|e| SaveErr::Failed(e.to_string()))?;
    if resp.status() == 409 { return Err(SaveErr::Exists); }
    if !resp.ok() { return Err(SaveErr::Failed(failure(resp).await)); }
    let v = resp.json::<Value>().await.map_err(|e| SaveErr::Failed(e.to_string()))?;
    saved_path(&v).map_err(SaveErr::Failed)
}

/// Write `<scripts>/<name>.sh` (mode 0755); returns its absolute path.
pub async fn save_script(name: &str, body: &str) -> Result<String, String> {
    let resp = gloo_net::http::Request::put(&format!("/api/taskqueue/script/{name}"))
        .json(&json!({ "body": body })).map_err(|e| e.to_string())?
        .send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(failure(resp).await); }
    let v = resp.json::<Value>().await.map_err(|e| e.to_string())?;
    saved_path(&v)
}

pub async fn delete_queue(name: &str) -> Result<(), String> {
    let resp = gloo_net::http::Request::delete(&format!("/api/taskqueue/queue/{name}"))
        .send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(failure(resp).await); }
    Ok(())
}
