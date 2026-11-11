//! Application launcher — spawn and monitor KStars, PHD2 and junos-guider
//! processes on the server host, pushing status changes to all connected
//! browsers over `/ws`.
//!
//! ## Process lifecycle
//!
//! - **Launch**: `POST /api/apps/launch {"app": "kstars"|"phd2"|"junos-guider"}`
//!   — spawns the process if it is not already running and stores the `Child`
//!   handle. PHD2 and junos-guider both serve PHD2's event server on :4400, so
//!   either is refused while the other runs.
//! - **Stop**: `POST /api/apps/stop {"app": …}` — sends SIGTERM
//!   to the child; clears the handle once the process exits.
//! - **Monitor**: a background task polls every 2 s; when a process that was
//!   running exits (whether by our stop or externally), it broadcasts an
//!   `app_state` message to all browsers.
//! - **Host power**: `POST /api/apps/power {"action": "poweroff"|"reboot"}` —
//!   runs `systemctl <action>` as the server's own user, nothing more: logind
//!   and polkit decide (fine from a desktop session; a system unit needs a
//!   polkit rule and AF_UNIX, see `nix/module.nix` and `packaging/`). KStars
//!   is not stopped first — systemd takes the session down with the host.
//! - **External guider**: `POST /api/apps/guider {"action": "connect"|"disconnect"}`
//!   — calls KStars' D-Bus `connectGuider` / `disconnectGuider` on
//!   `/KStars/Ekos/Guide` through `dbus-send` (the Guide module's Connect /
//!   Disconnect buttons for PHD2 and LinGuider: Ekos Live has no command for
//!   them, and `option_set GuiderType` changes nothing live). Same host as
//!   KStars and its session bus, like the cover script in
//!   `sequence_editor/cover.rs`.
//!
//! ## Initial state
//!
//! On startup `AppManager::scan_existing()` walks `/proc/*/comm` to detect
//! already-running instances.  Such processes are tracked as "external" (no
//! `Child` handle); the Stop button still works for those via `libc::kill`
//! rather than `Child::kill`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{extract::State, response::Json};
use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::hub::Hub;
use crate::AppState;

// ── Public process names ─────────────────────────────────────────────────────

/// Canonical names we support.  These are also the executable names on PATH.
pub const APP_KSTARS: &str = "kstars";
pub const APP_PHD2:   &str = "phd2";
pub const APP_GUIDER: &str = "junos-guider";

/// The app that cannot run next to `app`, and the refusal to launch `app`
/// while it does: PHD2 and junos-guider both listen on PHD2's :4400.
fn rival(app: &str) -> Option<(&'static str, &'static str)> {
    match app {
        APP_GUIDER => Some((APP_PHD2, "PHD2 is running on :4400")),
        APP_PHD2 => Some((APP_GUIDER, "junos-guider is running on :4400")),
        _ => None,
    }
}

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
    /// Pushes `app_state` JSON to all browser WebSockets.
    hub: Hub,
}

struct Inner {
    apps: HashMap<String, AppEntry>,
}

impl Inner {
    /// Whether kstars, phd2 and junos-guider run, in that order.
    fn running(&mut self) -> [bool; 3] {
        [APP_KSTARS, APP_PHD2, APP_GUIDER].map(|app| self.apps.get_mut(app).is_some_and(|e| e.is_running()))
    }
}

impl AppManager {
    pub fn new(hub: Hub) -> Self {
        let mut apps = HashMap::new();
        apps.insert(APP_KSTARS.to_string(), AppEntry::new_idle());
        apps.insert(APP_PHD2.to_string(),   AppEntry::new_idle());
        apps.insert(APP_GUIDER.to_string(), AppEntry::new_idle());
        Self {
            inner: Arc::new(Mutex::new(Inner { apps })),
            hub,
        }
    }

    /// Scan `/proc/*/comm` for already-running instances of the three apps.
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
            // Nix wrappers exec the real binary as `.kstars-wrapped` /
            // `.phd2-wrapped.bin`, so drop the leading dot before matching
            // (`comm` keeps 15 bytes: `junos-guider-w` still matches).
            let comm = comm.trim().trim_start_matches('.').to_lowercase();
            for app in [APP_KSTARS, APP_PHD2, APP_GUIDER] {
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
            let mut prev = [false; 3];
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                let cur = self.inner.lock().await.running();
                if cur != prev {
                    prev = cur;
                    self.hub.broadcast_text(build_app_state_msg(cur));
                }
            }
        });
    }

    /// Current status snapshot (non-mutating read).
    pub async fn status_json(&self) -> serde_json::Value {
        status(self.inner.lock().await.running())
    }

    /// The `app_state` message for the current status.
    pub async fn state_msg(&self) -> String {
        build_app_state_msg(self.inner.lock().await.running())
    }

    async fn launch_app(&self, app: &str) -> Result<(), String> {
        let mut g = self.inner.lock().await;
        if let Some((other, refusal)) = rival(app) {
            if g.apps.get_mut(other).is_some_and(|e| e.is_running()) {
                return Err(refusal.to_string());
            }
        }
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

/// `{kstars, phd2, junos_guider}`, each `"running"` or `"stopped"`.
fn status([kstars, phd2, guider]: [bool; 3]) -> serde_json::Value {
    serde_json::json!({
        "kstars":       state_str(kstars),
        "phd2":         state_str(phd2),
        "junos_guider": state_str(guider),
    })
}

fn build_app_state_msg(running: [bool; 3]) -> String {
    serde_json::json!({ "type": "app_state", "payload": status(running) }).to_string()
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

#[derive(Deserialize)]
pub struct PowerRequest {
    pub action: String,
}

/// `POST /api/apps/power {"action": "poweroff"|"reboot"}` — hands the action
/// to `systemctl`. Always HTTP 200 with `{ok, error}`, like the other two
/// handlers; a polkit refusal ("Interactive authentication required") comes
/// back in `error` so the Profiles tab can show it.
pub async fn power_handler(
    State(_state): State<AppState>,
    Json(body): Json<PowerRequest>,
) -> Json<serde_json::Value> {
    let action = body.action.to_lowercase();
    match host_power(&action).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })),
        Err(e) => {
            warn!("apps/power error: {e}");
            Json(serde_json::json!({ "ok": false, "error": e }))
        }
    }
}

async fn host_power(action: &str) -> Result<(), String> {
    if !matches!(action, "poweroff" | "reboot") {
        return Err(format!("unknown action: {action}"));
    }
    let output = Command::new("systemctl")
        .arg(action)
        .output()
        .await
        .map_err(|e| format!("failed to run systemctl: {e}"))?;
    if output.status.success() {
        info!("apps: host {action} requested");
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("systemctl {action} exited with {}", output.status)
    } else {
        stderr
    })
}

#[derive(Deserialize)]
pub struct GuiderRequest {
    pub action: String,
}

/// `POST /api/apps/guider {"action": "connect"|"disconnect"}` — KStars'
/// `Guide::connectGuider` / `disconnectGuider` over the session D-Bus. Always
/// HTTP 200 with `{ok, accepted, error}`: `accepted` is the method's return —
/// `PHD2::Connect` answers false when it is already connected or still
/// disconnecting (guide.cpp:3429, phd2.cpp:195). A missing KStars, a Guide
/// module not yet created (Ekos not started) or no session bus come back in
/// `error` as dbus-send reports them.
pub async fn guider_handler(
    State(_state): State<AppState>,
    Json(body): Json<GuiderRequest>,
) -> Json<serde_json::Value> {
    let action = body.action.to_lowercase();
    match guider_dbus(&action).await {
        Ok(accepted) => Json(serde_json::json!({ "ok": true, "accepted": accepted })),
        Err(e) => {
            warn!("apps/guider error: {e}");
            Json(serde_json::json!({ "ok": false, "error": e }))
        }
    }
}

async fn guider_dbus(action: &str) -> Result<bool, String> {
    let method = match action {
        "connect" => "connectGuider",
        "disconnect" => "disconnectGuider",
        _ => return Err(format!("unknown action: {action}")),
    };
    let output = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--reply-timeout=10000",
            "--dest=org.kde.kstars",
            "/KStars/Ekos/Guide",
            &format!("org.kde.kstars.Ekos.Guide.{method}"),
        ])
        .output()
        .await
        .map_err(|e| format!("failed to run dbus-send: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // The two expected refusals, in plain words; anything else verbatim.
        return Err(if stderr.contains("ServiceUnknown") {
            "KStars is not running".to_string()
        } else if stderr.contains("UnknownObject") {
            "Ekos is not started".to_string()
        } else if stderr.is_empty() {
            format!("dbus-send exited with {}", output.status)
        } else {
            stderr
        });
    }
    // Reply body: `   boolean true` on its own line after the header.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let accepted = stdout.lines().any(|l| l.trim() == "boolean true");
    info!("apps: guider {action} requested (accepted: {accepted})");
    Ok(accepted)
}

async fn push_status(state: &AppState) {
    state.hub.broadcast_text(state.app_manager.state_msg().await);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phd2_and_junos_guider_exclude_each_other() {
        assert_eq!(rival(APP_GUIDER), Some((APP_PHD2, "PHD2 is running on :4400")));
        assert_eq!(rival(APP_PHD2), Some((APP_GUIDER, "junos-guider is running on :4400")));
        assert_eq!(rival(APP_KSTARS), None);
    }

    #[test]
    fn app_state_keeps_the_old_keys_and_adds_junos_guider() {
        let v: serde_json::Value = serde_json::from_str(&build_app_state_msg([true, false, true])).unwrap();
        assert_eq!(v["type"], "app_state");
        assert_eq!(v["payload"], serde_json::json!({"kstars": "running", "phd2": "stopped", "junos_guider": "running"}));
    }
}
