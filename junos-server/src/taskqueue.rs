//! KStars Scheduler task-queue files, for the browser's startup/shutdown
//! queue editor.
//!
//! Endpoints:
//!
//! - `GET    /api/taskqueue/list`         — managed queues + scripts.
//! - `GET    /api/taskqueue/queue/:name`  — one queue's JSON.
//! - `PUT    /api/taskqueue/queue/:name`  — body `{content, overwrite}`.
//! - `DELETE /api/taskqueue/queue/:name`
//! - `GET    /api/taskqueue/script/:name` — one shell script's text.
//! - `PUT    /api/taskqueue/script/:name` — body `{body}`.
//!
//! KStars 3.8+ points each of the Scheduler's four procedure slots (pre/post
//! startup, pre/post shutdown) at a JSON task *collection*
//! (`schedulerprocess.cpp` → `QueueManager::loadQueue`); a shell script only
//! runs as a `script_execute` task inside one. So the editor writes both: the
//! script — mode 0755, since KStars' `ScriptAction::validateScript` refuses
//! anything not executable — and the collection that names it by absolute path.
//!
//! Files live under `Config::resolved_taskqueue_dir()`: `collections/<name>.json`
//! (the directory KStars' own Collections dialog lists) and `scripts/<name>.sh`.
//! A queue that holds custom INDI steps is written in KStars' own queue format
//! instead (an `items` array, actions spelled out — `QueueManager::fromJson`);
//! `loadQueue` reads both. As with the WS relay, the Ekos semantics (templates,
//! parameter ranges) live in the client; the server only checks for one of the
//! two arrays.
//!
//! Names come straight off the wire and are joined onto real paths, so only a
//! slug shape is accepted and the server appends the extension itself — no
//! canonicalise-and-compare needed.

use std::path::{Path, PathBuf};

use axum::extract::{Path as UrlPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::files::ApiErr;
use crate::AppState;

/// Upper bound for one collection or script. Both are hand-edited text; this
/// only stops a runaway request from filling the disk.
const MAX_BYTES: usize = 256 * 1024;

/// KStars resolves its default slot values by *name* through `KSPaths::locate`
/// (`scheduler.cpp`), which searches the user data dir first — a file of ours
/// with one of these names would silently replace the stock procedure.
pub(crate) const RESERVED_QUEUES: [&str; 2] = ["observatory_startup", "observatory_shutdown"];

fn collections_dir(state: &AppState) -> PathBuf {
    state.config.resolved_taskqueue_dir().join("collections")
}

fn scripts_dir(state: &AppState) -> PathBuf {
    state.config.resolved_taskqueue_dir().join("scripts")
}

fn err(status: StatusCode, reason: impl Into<String>) -> ApiErr {
    ApiErr { status, reason: reason.into() }
}

// ── Validation ───────────────────────────────────────────────────────────────

/// `[A-Za-z0-9_-]{1,64}`, not starting with `-`.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn check_name(name: &str) -> Result<(), ApiErr> {
    if is_safe_name(name) {
        Ok(())
    } else {
        Err(err(
            StatusCode::BAD_REQUEST,
            "name must be 1-64 characters of A-Z, a-z, 0-9, '_' or '-'",
        ))
    }
}

/// What `QueueManager::loadQueue` can run: an object carrying a `tasks` array
/// (a collection) or an `items` array (its own queue format).
fn validate_queue(content: &Value) -> Result<(), String> {
    content.as_object().ok_or("queue must be a JSON object")?;
    step_count(content)
        .map(|_| ())
        .ok_or_else(|| "queue must have a \"tasks\" or \"items\" array".to_string())
}

/// Number of steps in either format.
fn step_count(doc: &Value) -> Option<usize> {
    doc["tasks"].as_array().or_else(|| doc["items"].as_array()).map(Vec::len)
}

/// KStars starts the script with `QProcess::start(path)` — no shell, no
/// arguments — so the kernel has to find an interpreter line. CRLF endings
/// (a browser on Windows, a pasted snippet) would make it look for `bash\r`.
fn normalize_script(body: &str) -> Result<String, String> {
    let mut text = body.replace("\r\n", "\n");
    if !text.starts_with("#!") {
        return Err("script must start with an interpreter line such as #!/usr/bin/env bash".to_string());
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if text.len() > MAX_BYTES {
        return Err(format!("script exceeds {} KB", MAX_BYTES / 1024));
    }
    Ok(text)
}

// ── File helpers ─────────────────────────────────────────────────────────────

/// Write through a temp file in the same directory and rename over the target,
/// so KStars never loads a half-written collection or runs a truncated script.
/// The mode is set before the rename, so a script is executable from the moment
/// it appears.
fn write_atomic(path: &Path, bytes: &[u8], executable: bool) -> std::io::Result<()> {
    let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::create_dir_all(dir)?;
    let file_name = path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
    let tmp = dir.join(format!(".{}.tmp", file_name.to_string_lossy()));
    let result = std::fs::write(&tmp, bytes)
        .and_then(|()| if executable { set_executable(&tmp) } else { Ok(()) })
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// `(stem, path)` of every `<safe-name>.<ext>` file in `dir`, sorted by stem.
/// Files the API couldn't address anyway (spaces, dots…) are left out; a
/// missing directory is simply empty.
fn list_stems(dir: &Path, ext: &str) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != ext {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_string();
            is_safe_name(&stem).then_some((stem, path))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn io_status(e: &std::io::Error) -> StatusCode {
    if e.kind() == std::io::ErrorKind::NotFound {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

// ── /list ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct QueueEntry {
    name: String,
    path: String,
    /// The collection's `name` field — what KStars' Collections dialog shows.
    title: Option<String>,
    /// Step count, or `None` when the file is neither a `tasks` collection
    /// nor an `items` queue.
    tasks: Option<usize>,
}

#[derive(Serialize)]
struct ScriptEntry {
    name: String,
    path: String,
}

pub async fn list(State(state): State<AppState>) -> Json<Value> {
    let cdir = collections_dir(&state);
    let sdir = scripts_dir(&state);

    let queues: Vec<QueueEntry> = list_stems(&cdir, "json")
        .into_iter()
        .map(|(name, path)| {
            let doc = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            QueueEntry {
                name,
                path: path.to_string_lossy().into_owned(),
                title: doc.as_ref().and_then(|d| d["name"].as_str()).map(str::to_string),
                tasks: doc.as_ref().and_then(step_count),
            }
        })
        .collect();

    let scripts: Vec<ScriptEntry> = list_stems(&sdir, "sh")
        .into_iter()
        .map(|(name, path)| ScriptEntry { name, path: path.to_string_lossy().into_owned() })
        .collect();

    Json(json!({
        "collections_dir": cdir.to_string_lossy(),
        "scripts_dir":     sdir.to_string_lossy(),
        "queues":          queues,
        "scripts":         scripts,
    }))
}

// ── /queue/:name ─────────────────────────────────────────────────────────────

pub async fn get_queue(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiErr> {
    check_name(&name)?;
    let path = collections_dir(&state).join(format!("{name}.json"));
    let bytes = std::fs::read(&path).map_err(|e| err(io_status(&e), e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}

#[derive(Debug, Deserialize)]
pub struct QueuePut {
    pub content: Value,
    #[serde(default)]
    pub overwrite: bool,
}

pub async fn put_queue(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<QueuePut>,
) -> Result<Json<Value>, ApiErr> {
    check_name(&name)?;
    if RESERVED_QUEUES.contains(&name.as_str()) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("\"{name}\" would shadow KStars' built-in procedure — pick another name"),
        ));
    }
    validate_queue(&body.content).map_err(|r| err(StatusCode::BAD_REQUEST, r))?;

    let mut text = serde_json::to_string_pretty(&body.content)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    text.push('\n');
    if text.len() > MAX_BYTES {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, format!("queue exceeds {} KB", MAX_BYTES / 1024)));
    }

    let path = collections_dir(&state).join(format!("{name}.json"));
    if path.exists() && !body.overwrite {
        return Err(err(StatusCode::CONFLICT, format!("{} already exists", path.display())));
    }
    write_atomic(&path, text.as_bytes(), false).map_err(|e| {
        warn!("taskqueue: writing {} failed: {e}", path.display());
        err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}: {e}", path.display()))
    })?;
    info!("taskqueue: wrote {}", path.display());

    Ok(Json(json!({ "ok": true, "path": path.to_string_lossy() })))
}

pub async fn delete_queue(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Value>, ApiErr> {
    check_name(&name)?;
    let path = collections_dir(&state).join(format!("{name}.json"));
    std::fs::remove_file(&path).map_err(|e| {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("taskqueue: deleting {} failed: {e}", path.display());
        }
        err(io_status(&e), e.to_string())
    })?;
    info!("taskqueue: deleted {}", path.display());
    Ok(Json(json!({ "ok": true })))
}

// ── /script/:name ────────────────────────────────────────────────────────────

pub async fn get_script(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiErr> {
    check_name(&name)?;
    let path = scripts_dir(&state).join(format!("{name}.sh"));
    let text = std::fs::read_to_string(&path).map_err(|e| err(io_status(&e), e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ScriptPut {
    pub body: String,
}

/// Always overwrites: scripts are owned by the queue that references them and
/// are only ever saved from that queue's editor.
pub async fn put_script(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<ScriptPut>,
) -> Result<Json<Value>, ApiErr> {
    check_name(&name)?;
    let text = normalize_script(&body.body).map_err(|r| err(StatusCode::BAD_REQUEST, r))?;

    let path = scripts_dir(&state).join(format!("{name}.sh"));
    write_atomic(&path, text.as_bytes(), true).map_err(|e| {
        warn!("taskqueue: writing {} failed: {e}", path.display());
        err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}: {e}", path.display()))
    })?;
    info!("taskqueue: wrote script {}", path.display());

    Ok(Json(json!({ "ok": true, "path": path.to_string_lossy() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_slug_names() {
        for good in ["roof_open", "junos-pre-startup", "A1", "x"] {
            assert!(is_safe_name(good), "should accept {good:?}");
        }
        assert!(is_safe_name(&"a".repeat(64)));
    }

    #[test]
    fn rejects_traversal_and_odd_names() {
        for bad in [
            "",
            "../x",
            "a/b",
            "/abs",
            "..",
            ".hidden",
            "-rf",
            "with space",
            "roof.json",
            "é",
        ] {
            assert!(!is_safe_name(bad), "should reject {bad:?}");
        }
        assert!(!is_safe_name(&"a".repeat(65)));
    }

    #[test]
    fn reserved_names_are_safe_but_reserved() {
        // They pass the shape check — the PUT handler refuses them separately,
        // so the stock files can still be read back if someone put them there.
        for name in RESERVED_QUEUES {
            assert!(is_safe_name(name));
        }
    }

    #[test]
    fn script_needs_shebang_and_gets_lf_endings() {
        assert_eq!(
            normalize_script("#!/usr/bin/env bash\r\necho hi\r\n").unwrap(),
            "#!/usr/bin/env bash\necho hi\n"
        );
        assert_eq!(normalize_script("#!/bin/sh\nexit 0").unwrap(), "#!/bin/sh\nexit 0\n");
        assert!(normalize_script("echo hi\n").is_err());
        assert!(normalize_script(" #!/bin/sh\n").is_err());
        assert!(normalize_script("").is_err());
        assert!(normalize_script(&format!("#!/bin/sh\n{}", "x".repeat(MAX_BYTES))).is_err());
    }

    #[test]
    fn queue_needs_tasks_or_items_array() {
        assert!(validate_queue(&json!({"name": "q", "tasks": []})).is_ok());
        assert!(validate_queue(&json!({"name": "q", "items": []})).is_ok());
        assert!(validate_queue(&json!({"name": "q"})).is_err());
        assert!(validate_queue(&json!({"tasks": {}})).is_err());
        assert!(validate_queue(&json!({"items": "x"})).is_err());
        assert!(validate_queue(&json!([])).is_err());
        assert_eq!(step_count(&json!({"items": [{}, {}]})), Some(2));
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_marks_scripts_executable() {
        use std::os::unix::fs::PermissionsExt;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("junos-taskqueue-test-{}-{nanos}", std::process::id()));
        let script = dir.join("scripts").join("t.sh");

        write_atomic(&script, b"#!/bin/sh\nexit 0\n", true).unwrap();
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);

        // Overwrite in place keeps the mode and leaves no temp file behind.
        write_atomic(&script, b"#!/bin/sh\nexit 1\n", true).unwrap();
        assert_eq!(std::fs::read_to_string(&script).unwrap(), "#!/bin/sh\nexit 1\n");
        let leftovers: Vec<_> = std::fs::read_dir(script.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("t.sh")]);

        let queue = dir.join("collections").join("q.json");
        write_atomic(&queue, b"{}", false).unwrap();
        let mode = std::fs::metadata(&queue).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "queue files must not be executable");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
