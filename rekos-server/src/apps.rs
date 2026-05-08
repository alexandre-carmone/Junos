//! Application launcher — spawn and monitor KStars and PHD2 processes on the
//! server host, pushing status changes to all connected browsers over `/ws`.
//!
//! ## Process lifecycle
//!
//! - **Launch**: `POST /api/apps/launch {"app": "kstars"|"phd2"}` — spawns the
//!   process if it is not already running and stores the `Child` handle.
//! - **Stop**: `POST /api/apps/stop {"app": "kstars"|"phd2"}` — sends SIGTERM
//!   to the child; clears the handle once the process exits.
//! - **Monitor**: a background task polls every 2 s; when a process that was
//!   running exits (whether by our stop or externally), it broadcasts an
//!   `app_state` message to all browsers.
//!
//! ## Initial state
//!
//! On startup `AppManager::scan_existing()` walks `/proc/*/comm` to detect
//! already-running instances.  Such processes are tracked as "external" (no
//! `Child` handle); the Stop button will still work via `/proc/<pid>/…` —
//! we use `nix::sys::signal` rather than `Child::kill` in that path.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{extract::State, response::Json};
use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

use crate::AppState;

// ── Public process names ─────────────────────────────────────────────────────

/// Canonical names we support.  These are also the executable names on PATH.
pub const APP_KSTARS: &str = "kstars";
pub const APP_PHD2:   &str = "phd2";

// ── Per-process state ────────────────────────────────────────────────────────

enum ProcessHandle {
    /// We own the child (we spawned it).
    Owned(Child),
    /// Process was already running when the server started; we know its PID
    /// but do not hold the `Child` handle.
    External(u32),
}

struct AppEntry {
    handle: Option<ProcessHandle>,
}

impl AppEntry {
    fn new_idle() -> Self {
        Self { handle: None }
    }

    /// Returns true if the process appears to be running right now.
    fn is_running(&mut self) -> bool {
        match &mut self.handle {
            None => false,
            Some(ProcessHandle::Owned(child)) => {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        // Exited
                        self.handle = None;
                        false
                    }
                    Ok(None) => true,   // still running
                    Err(_) => {
                        self.handle = None;
                        false
                    }
                }
            }
            Some(ProcessHandle::External(pid)) => {
                let pid = *pid;
                // Check /proc/<pid> existence — fast and allocation-free.
                if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                    true
                } else {
                    self.handle = None;
                    false
                }
            }
        }
    }

    /// Attempt to kill the process.  Returns Ok(()) if a signal was sent (or
    /// the process was already gone).
    async fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.handle {
            None => Ok(()),
            Some(ProcessHandle::Owned(child)) => {
                child.start_kill()?;
                // Wait to reap so we don't leave zombies.
                let _ = child.wait().await;
                self.handle = None;
                Ok(())
            }
            Some(ProcessHandle::External(pid)) => {
                let pid = *pid;
                // SIGTERM via kill(2).
                #[cfg(unix)]
                {
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                }
                self.handle = None;
                Ok(())
            }
        }
    }
}

// ── AppManager ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppManager {
    inner: Arc<Mutex<Inner>>,
    /// Sender used to push `app_state` JSON to all browser WebSockets.
    browser_tx: broadcast::Sender<String>,
}

struct Inner {
    apps: HashMap<String, AppEntry>,
}

impl AppManager {
    pub fn new(browser_tx: broadcast::Sender<String>) -> Self {
        let mut apps = HashMap::new();
        apps.insert(APP_KSTARS.to_string(), AppEntry::new_idle());
        apps.insert(APP_PHD2.to_string(),   AppEntry::new_idle());
        Self {
            inner: Arc::new(Mutex::new(Inner { apps })),
            browser_tx,
        }
    }

    /// Scan `/proc/*/comm` for already-running instances of kstars / phd2.
    /// Call once at server startup before starting the monitor task.
    pub async fn scan_existing(&self) {
        let entries = match std::fs::read_dir("/proc") {
            Ok(e) => e,
            Err(_) => return, // not Linux, skip
        };
        let mut found: HashMap<String, u32> = HashMap::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Only numeric PID directories.
            let Ok(pid) = name_str.parse::<u32>() else { continue };
            let comm_path = format!("/proc/{pid}/comm");
            let Ok(comm) = std::fs::read_to_string(&comm_path) else { continue };
            let comm = comm.trim().to_lowercase();
            for app in [APP_KSTARS, APP_PHD2] {
                if comm == app || comm.starts_with(app) {
                    found.entry(app.to_string()).or_insert(pid);
                }
            }
        }
        let mut g = self.inner.lock().await;
        for (app, pid) in found {
            info!("apps: detected existing {app} (pid {pid})");
            if let Some(entry) = g.apps.get_mut(&app) {
                entry.handle = Some(ProcessHandle::External(pid));
            }
        }
    }

    /// Start the background monitor task that polls process status and pushes
    /// `app_state` broadcasts on changes.
    pub fn start_monitor(self) {
        tokio::spawn(async move {
            let mut prev_kstars = false;
            let mut prev_phd2   = false;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                let (cur_kstars, cur_phd2) = {
                    let mut g = self.inner.lock().await;
                    let k = g.apps.get_mut(APP_KSTARS).map(|e| e.is_running()).unwrap_or(false);
                    let p = g.apps.get_mut(APP_PHD2).map(|e| e.is_running()).unwrap_or(false);
                    (k, p)
                };
                if cur_kstars != prev_kstars || cur_phd2 != prev_phd2 {
                    prev_kstars = cur_kstars;
                    prev_phd2   = cur_phd2;
                    let msg = build_app_state_msg(cur_kstars, cur_phd2);
                    let _ = self.browser_tx.send(msg);
                }
            }
        });
    }

    /// Current status snapshot (non-mutating read).
    pub async fn status_json(&self) -> serde_json::Value {
        let mut g = self.inner.lock().await;
        let k = g.apps.get_mut(APP_KSTARS).map(|e| e.is_running()).unwrap_or(false);
        let p = g.apps.get_mut(APP_PHD2).map(|e| e.is_running()).unwrap_or(false);
        serde_json::json!({ "kstars": state_str(k), "phd2": state_str(p) })
    }

    async fn launch_app(&self, app: &str) -> Result<(), String> {
        let mut g = self.inner.lock().await;
        let entry = g.apps.get_mut(app).ok_or_else(|| format!("unknown app: {app}"))?;
        if entry.is_running() {
            return Ok(()); // already up
        }
        let child = Command::new(app)
            .spawn()
            .map_err(|e| format!("failed to spawn {app}: {e}"))?;
        info!("apps: launched {app} (pid {:?})", child.id());
        entry.handle = Some(ProcessHandle::Owned(child));
        Ok(())
    }

    async fn stop_app(&self, app: &str) -> Result<(), String> {
        let mut g = self.inner.lock().await;
        let entry = g.apps.get_mut(app).ok_or_else(|| format!("unknown app: {app}"))?;
        entry.kill().await.map_err(|e| format!("kill {app}: {e}"))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn state_str(running: bool) -> &'static str {
    if running { "running" } else { "stopped" }
}

pub fn build_app_state_msg(kstars: bool, phd2: bool) -> String {
    serde_json::json!({
        "type": "app_state",
        "payload": {
            "kstars": state_str(kstars),
            "phd2":   state_str(phd2),
        }
    })
    .to_string()
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AppRequest {
    pub app: String,
}

pub async fn launch_handler(
    State(state): State<AppState>,
    Json(body): Json<AppRequest>,
) -> Json<serde_json::Value> {
    let app = body.app.to_lowercase();
    match state.app_manager.launch_app(&app).await {
        Ok(()) => {
            // Push immediate status update.
            push_status(&state).await;
            Json(serde_json::json!({ "ok": true }))
        }
        Err(e) => {
            warn!("apps/launch error: {e}");
            Json(serde_json::json!({ "ok": false, "error": e }))
        }
    }
}

pub async fn stop_handler(
    State(state): State<AppState>,
    Json(body): Json<AppRequest>,
) -> Json<serde_json::Value> {
    let app = body.app.to_lowercase();
    match state.app_manager.stop_app(&app).await {
        Ok(()) => {
            push_status(&state).await;
            Json(serde_json::json!({ "ok": true }))
        }
        Err(e) => {
            warn!("apps/stop error: {e}");
            Json(serde_json::json!({ "ok": false, "error": e }))
        }
    }
}

pub async fn state_handler(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    Json(state.app_manager.status_json().await)
}

async fn push_status(state: &AppState) {
    let status = state.app_manager.status_json().await;
    let kstars = status["kstars"].as_str() == Some("running");
    let phd2   = status["phd2"].as_str()   == Some("running");
    let msg = build_app_state_msg(kstars, phd2);
    let _ = state.hub.browser_tx.send(msg);
}
