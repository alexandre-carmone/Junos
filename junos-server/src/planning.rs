//! Planning files — what the Scheduler writes — for the Files tab's Planning
//! sheet.
//!
//! Endpoints (mounted under `/api/planning`):
//!
//! - `GET    /list`                              — every group, see [`Kind`].
//! - `GET    /raw?kind=&name=[&download=1]`      — one file's text.
//! - `POST   /rename` body `{kind, name, new_name}` — schedules only.
//! - `DELETE /delete?kind=&name=`
//!
//! Four flat folders, each holding one file type:
//!
//! - `schedules` — `~/.junos-schedules/*.esl`, written by KStars on the
//!   Scheduler tab's Save schedule (`scheduler_save_file`).
//! - `sequences` — `~/.junos-sequences/*.esq`, one per job added from the
//!   Scheduler or Mosaic tab (`scheduler_save_sequence_file`, which KStars
//!   resolves against *its* `$HOME` — same host and user assumed, as for the
//!   folder `main.rs` creates).
//! - `queues` / `scripts` — the task-queue collections and their shell scripts
//!   (`taskqueue.rs`).
//!
//! Only schedules can be renamed: a sequence is named by absolute path in the
//! jobs and `.esl` files that use it, a queue in the Scheduler's slot settings,
//! a script in its queue — renaming those would break the reference silently.
//!
//! A name is a bare file name with the group's extension; the joined path is
//! canonicalized and must stay inside the canonical folder (as in `files.rs`),
//! which also covers a symlink pointing elsewhere.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::Config;
use crate::files::ApiErr;
use crate::taskqueue::RESERVED_QUEUES;
use crate::AppState;

/// Planning files are small text; this only bounds a stray huge file.
const MAX_BYTES: u64 = 1024 * 1024;

fn home_subdir(name: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(name))
}

/// Where the Scheduler tab saves `.esl` files.
pub fn schedules_dir() -> Option<PathBuf> {
    home_subdir(".junos-schedules")
}

/// Where the Scheduler and Mosaic tabs save each job's `.esq`.
pub fn sequences_dir() -> Option<PathBuf> {
    home_subdir(".junos-sequences")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Schedules,
    Sequences,
    Queues,
    Scripts,
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::Schedules, Kind::Sequences, Kind::Queues, Kind::Scripts];

    fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.key() == s)
    }

    fn key(self) -> &'static str {
        match self {
            Kind::Schedules => "schedules",
            Kind::Sequences => "sequences",
            Kind::Queues => "queues",
            Kind::Scripts => "scripts",
        }
    }

    fn ext(self) -> &'static str {
        match self {
            Kind::Schedules => "esl",
            Kind::Sequences => "esq",
            Kind::Queues => "json",
            Kind::Scripts => "sh",
        }
    }

    fn dir(self, config: &Config) -> Option<PathBuf> {
        match self {
            Kind::Schedules => schedules_dir(),
            Kind::Sequences => sequences_dir(),
            Kind::Queues => Some(config.resolved_taskqueue_dir().join("collections")),
            Kind::Scripts => Some(config.resolved_taskqueue_dir().join("scripts")),
        }
    }
}

fn err(status: StatusCode, reason: impl Into<String>) -> ApiErr {
    ApiErr { status, reason: reason.into() }
}

fn kind_of(s: &str) -> Result<Kind, ApiErr> {
    Kind::parse(s).ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("unknown kind \"{s}\"")))
}

/// A bare file name — no separator, not hidden — ending in `.<ext>`.
fn is_valid_name(kind: Kind, name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && name.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.trim().is_empty() && ext.eq_ignore_ascii_case(kind.ext())
        })
}

fn check_name(kind: Kind, name: &str) -> Result<(), ApiErr> {
    if is_valid_name(kind, name) {
        Ok(())
    } else {
        Err(err(StatusCode::BAD_REQUEST, format!("invalid {} file name \"{name}\"", kind.ext())))
    }
}

/// `name` inside `dir`, canonicalized; must exist and stay inside `dir`.
fn resolve_in(dir: &Path, kind: Kind, name: &str) -> Result<PathBuf, ApiErr> {
    check_name(kind, name)?;
    let root = dir
        .canonicalize()
        .map_err(|e| err(StatusCode::NOT_FOUND, format!("{}: {e}", dir.display())))?;
    let target = root
        .join(name)
        .canonicalize()
        .map_err(|e| err(StatusCode::NOT_FOUND, format!("{name}: {e}")))?;
    if !target.starts_with(&root) {
        return Err(err(StatusCode::FORBIDDEN, format!("{name} points outside {}", root.display())));
    }
    if !target.is_file() {
        return Err(err(StatusCode::BAD_REQUEST, format!("{name} is not a file")));
    }
    Ok(target)
}

fn resolve(config: &Config, kind: Kind, name: &str) -> Result<PathBuf, ApiErr> {
    let dir = kind
        .dir(config)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "HOME is not set on the server"))?;
    resolve_in(&dir, kind, name)
}

// ── /list ────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct Entry {
    name: String,
    size: u64,
    mtime: u64,
}

/// The group's files, by name; a missing folder is just empty.
fn list_dir(dir: &Path, kind: Kind) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Entry> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !is_valid_name(kind, &name) {
                return None;
            }
            let meta = e.metadata().ok().filter(std::fs::Metadata::is_file)?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            Some(Entry { name, size: meta.len(), mtime })
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

pub async fn list(State(state): State<AppState>) -> Json<Value> {
    let groups: Vec<Value> = Kind::ALL
        .into_iter()
        .map(|kind| {
            let dir = kind.dir(&state.config);
            let entries = dir.as_deref().map(|d| list_dir(d, kind)).unwrap_or_default();
            let dir = dir.map(|d| d.canonicalize().unwrap_or(d).to_string_lossy().into_owned());
            json!({ "kind": kind.key(), "dir": dir.unwrap_or_default(), "entries": entries })
        })
        .collect();
    Json(json!({ "groups": groups }))
}

// ── /raw ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct FileQ {
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub download: Option<String>,
}

pub async fn raw(State(state): State<AppState>, Query(q): Query<FileQ>) -> Result<Response, ApiErr> {
    let kind = kind_of(&q.kind)?;
    let path = resolve(&state.config, kind, &q.name)?;
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if len > MAX_BYTES {
        return Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("{} exceeds {} KB", q.name, MAX_BYTES / 1024),
        ));
    }
    let bytes = std::fs::read(&path)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}: {e}", path.display())))?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "text/plain; charset=utf-8".parse().unwrap());
    // Files change under the same name (a re-saved schedule); always revalidate.
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    if q.download.as_deref().is_some_and(|v| v == "1" || v == "true") {
        let safe: String = q.name
            .chars()
            .map(|c| if matches!(c, '"' | ';' | '\r' | '\n') { '_' } else { c })
            .collect();
        if let Ok(v) = format!(r#"attachment; filename="{safe}""#).parse() {
            headers.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    Ok((StatusCode::OK, headers, bytes).into_response())
}

// ── /rename ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RenameBody {
    pub kind: String,
    pub name: String,
    pub new_name: String,
}

/// The typed name with the group's extension added when it's missing.
fn with_ext(kind: Kind, name: &str) -> String {
    let name = name.trim();
    let has_ext = name
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case(kind.ext()));
    if has_ext { name.to_string() } else { format!("{name}.{}", kind.ext()) }
}

pub async fn rename(
    State(state): State<AppState>,
    Json(body): Json<RenameBody>,
) -> Result<Json<Value>, ApiErr> {
    let kind = kind_of(&body.kind)?;
    if kind != Kind::Schedules {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "only schedules can be renamed — sequences, queues and scripts are referenced by path",
        ));
    }
    let source = resolve(&state.config, kind, &body.name)?;
    let new_name = with_ext(kind, &body.new_name);
    check_name(kind, &new_name)?;
    let dest = source.with_file_name(&new_name);
    if dest.exists() {
        return Err(err(StatusCode::CONFLICT, format!("{new_name} already exists")));
    }
    std::fs::rename(&source, &dest).map_err(|e| {
        warn!("planning: rename {} → {} failed: {e}", source.display(), dest.display());
        err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    info!("planning: renamed {} → {}", source.display(), dest.display());
    Ok(Json(json!({ "ok": true, "name": new_name })))
}

// ── /delete ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DeleteQ {
    pub kind: String,
    pub name: String,
}

pub async fn delete(State(state): State<AppState>, Query(q): Query<DeleteQ>) -> Result<Json<Value>, ApiErr> {
    let kind = kind_of(&q.kind)?;
    let stem = q.name.rsplit_once('.').map_or(q.name.as_str(), |(s, _)| s);
    if kind == Kind::Queues && RESERVED_QUEUES.contains(&stem) {
        return Err(err(StatusCode::BAD_REQUEST, format!("\"{stem}\" is a KStars built-in procedure")));
    }
    let path = resolve(&state.config, kind, &q.name)?;
    std::fs::remove_file(&path).map_err(|e| {
        warn!("planning: deleting {} failed: {e}", path.display());
        err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    info!("planning: deleted {}", path.display());
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("junos-planning-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn kinds_round_trip() {
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.key()), Some(k));
        }
        assert_eq!(Kind::parse("captures"), None);
    }

    #[test]
    fn names_need_the_kind_extension() {
        assert!(is_valid_name(Kind::Schedules, "M31 night.esl"));
        assert!(is_valid_name(Kind::Schedules, "M31.ESL"));
        assert!(is_valid_name(Kind::Sequences, "NGC_7000.esq"));
        assert!(is_valid_name(Kind::Queues, "junos_pre_startup.json"));
        assert!(is_valid_name(Kind::Scripts, "roof.sh"));
        for bad in ["", ".esl", ".hidden.esl", "../x.esl", "a/b.esl", "a\\b.esl", "x.esq", "esl", " .esl"] {
            assert!(!is_valid_name(Kind::Schedules, bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn rename_adds_the_extension() {
        assert_eq!(with_ext(Kind::Schedules, " night "), "night.esl");
        assert_eq!(with_ext(Kind::Schedules, "night.esl"), "night.esl");
        assert_eq!(with_ext(Kind::Schedules, "night.ESL"), "night.ESL");
        assert_eq!(with_ext(Kind::Schedules, "m31.v2"), "m31.v2.esl");
    }

    #[test]
    fn list_keeps_only_the_kind_files() {
        let dir = temp_dir("list");
        for f in ["b.esl", "A.esl", ".hidden.esl", "notes.txt", "seq.esq"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        std::fs::create_dir(dir.join("folder.esl")).unwrap();
        let names: Vec<String> = list_dir(&dir, Kind::Schedules).into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["A.esl", "b.esl"]);
        assert!(list_dir(&dir.join("missing"), Kind::Schedules).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_stays_inside_the_folder() {
        let dir = temp_dir("resolve");
        let outside = temp_dir("outside");
        std::fs::write(dir.join("ok.esl"), "x").unwrap();
        std::fs::write(outside.join("secret.esl"), "x").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.esl"), dir.join("link.esl")).unwrap();

        assert!(resolve_in(&dir, Kind::Schedules, "ok.esl").is_ok());
        assert_eq!(resolve_in(&dir, Kind::Schedules, "link.esl").err().unwrap().status, StatusCode::FORBIDDEN);
        assert_eq!(resolve_in(&dir, Kind::Schedules, "missing.esl").err().unwrap().status, StatusCode::NOT_FOUND);
        assert_eq!(resolve_in(&dir, Kind::Schedules, "../x.esl").err().unwrap().status, StatusCode::BAD_REQUEST);

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }
}
