//! Files › Planning: typed wrappers around the server's `/api/planning/*`.

use gloo_net::http::Request;
use serde::Deserialize;
use serde_json::json;

use super::super::api::checked;
use super::super::utils::url_encode;
use super::parse::Kind;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct PlanEntry {
    pub name: String,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct PlanGroup {
    pub kind: String,
    /// Absolute folder on the host; "" when the server has no `$HOME`.
    pub dir: String,
    pub entries: Vec<PlanEntry>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct PlanList {
    pub groups: Vec<PlanGroup>,
}

impl PlanList {
    pub(crate) fn group(&self, kind: Kind) -> Option<&PlanGroup> {
        self.groups.iter().find(|g| g.kind == kind.key())
    }
}

pub(crate) async fn fetch_list() -> Result<PlanList, String> {
    checked(Request::get("/api/planning/list").send().await).await?.json().await.map_err(|e| e.to_string())
}

fn query(kind: Kind, name: &str) -> String {
    format!("kind={}&name={}", kind.key(), url_encode(name))
}

pub(crate) fn raw_url(kind: Kind, name: &str) -> String {
    format!("/api/planning/raw?{}", query(kind, name))
}

pub(crate) fn download_url(kind: Kind, name: &str) -> String {
    format!("{}&download=1", raw_url(kind, name))
}

/// Any text file the server hands out (`/api/planning/raw`, `/api/files/raw`).
pub(crate) async fn fetch_text(url: &str) -> Result<String, String> {
    checked(Request::get(url).send().await).await?.text().await.map_err(|e| e.to_string())
}

/// Rename a schedule; the server adds `.esl` when missing. Returns the name
/// it ended up with.
pub(crate) async fn rename(kind: Kind, name: &str, new_name: &str) -> Result<String, String> {
    let req = Request::post("/api/planning/rename")
        .json(&json!({ "kind": kind.key(), "name": name, "new_name": new_name }))
        .map_err(|e| e.to_string())?;
    let reply: serde_json::Value = checked(req.send().await).await?.json().await.map_err(|e| e.to_string())?;
    Ok(reply["name"].as_str().unwrap_or(new_name).to_string())
}

pub(crate) async fn delete(kind: Kind, name: &str) -> Result<(), String> {
    let url = format!("/api/planning/delete?{}", query(kind, name));
    checked(Request::delete(&url).send().await).await.map(drop)
}
