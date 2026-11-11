//! Application launcher — spawn and monitor KStars, PHD2 and junos-guider
//! processes on the server host, pushing status changes to all connected
//! browsers over `/ws`.
//!
//! ## Process lifecycle
//!
//! - **Launch**: `POST /api/apps/launch {"app": "kstars"|"phd2"|"junos-guider"}`
//!   — spawns the process if it is not already running and stores the `Child`
//!   handle. PHD2 and junos-guider both serve PHD2's event server on :4400, so
//!   either is refused while the other runs. junos-guider gets `--ws-addr`
//!   from the relay's `--guider-ws`, so the two meet whatever the address.
//! - **Stop**: `POST /api/apps/stop {"app": …}` — sends SIGTERM (junos-guider
//!   then closes its guide log and disconnects the gear), waits up to 5 s for
//!   a child of ours to exit, then kills it; clears the handle.
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
//! ## Processes we did not start
//!
//! Whenever an app has no live handle (each 2 s poll, each launch, each
//! status read), `/proc/*/comm` is walked for it: one already running when
//! the server started, or started later — a systemd unit, restarted under a
//! new PID. Such processes are tracked as "external" (no `Child` handle);
//! the Stop button still works for those via `libc::kill` rather than
//! `Child::kill`. junos-guider also counts as running while the relay is
//! attached to it (`guider.rs`), wherever it runs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, response::Json};
use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::guider::GuiderLink;
use crate::hub::Hub;
use crate::AppState;

// ── Public process names ─────────────────────────────────────────────────────

/// Canonical names we support.  These are also the executable names on PATH.
pub const APP_KSTARS: &str = "kstars";
pub const APP_PHD2:   &str = "phd2";
pub const APP_GUIDER: &str = "junos-guider";

/// The three, in the order of `running()`'s flags.
const APPS: [&str; 3] = [APP_KSTARS, APP_PHD2, APP_GUIDER];

/// How long Stop waits for a child to exit on SIGTERM before killing it.
const STOP_GRACE: Duration = Duration::from_secs(5);

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
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                }
                // Reaped either way, so we don't leave zombies.
                if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
                    child.start_kill()?;
                    let _ = child.wait().await;
                }
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
    /// The relay's link to junos-guider: attached means it runs.
    guider: GuiderLink,
    /// junos-guider's arguments when we launch it.
    guider_args: Vec<String>,
}

struct Inner {
    apps: HashMap<String, AppEntry>,
}

impl Inner {
    /// Whether kstars, phd2 and junos-guider run, in `APPS` order, looking
    /// in `/proc` for any without a live handle.
    fn running(&mut self) -> [bool; 3] {
        let running = APPS.map(|app| self.apps.get_mut(app).is_some_and(|e| e.is_running()));
        if !running.contains(&false) {
            return running;
        }
        for (app, pid) in scan_proc() {
            if let Some(entry) = self.apps.get_mut(app).filter(|e| e.handle.is_none()) {
                info!("apps: detected running {app} (pid {pid})");
                entry.handle = Some(ProcessHandle::External(pid));
            }
        }
        APPS.map(|app| self.apps.get(app).is_some_and(|e| e.handle.is_some()))
    }
}

/// The first PID `/proc/*/comm` shows for each of the three apps.
fn scan_proc() -> HashMap<&'static str, u32> {
    let mut found = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found; // not Linux
    };
    for entry in entries.flatten() {
        // Only numeric PID directories.
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else { continue };
        // Nix wrappers exec the real binary as `.kstars-wrapped` /
        // `.phd2-wrapped.bin`, so drop the leading dot before matching
        // (`comm` keeps 15 bytes: `junos-guider-w` still matches).
        let comm = comm.trim().trim_start_matches('.').to_lowercase();
        for app in APPS {
            if comm.starts_with(app) {
                found.entry(app).or_insert(pid);
            }
        }
    }
    found
}

/// `--ws-addr host:port` for a junos-guider we launch, from the relay's
/// `--guider-ws` URL (`ws://host:port`); none when it has another shape.
fn guider_args(guider_ws: &str) -> Vec<String> {
    let addr = guider_ws.strip_prefix("ws://").and_then(|rest| rest.split('/').next());
    match addr {
        Some(addr) if !addr.is_empty() => vec!["--ws-addr".to_string(), addr.to_string()],
        _ => Vec::new(),
    }
}

impl AppManager {
    pub fn new(hub: Hub, guider: GuiderLink, guider_ws: &str) -> Self {
        let apps = APPS.iter().map(|app| (app.to_string(), AppEntry::new_idle())).collect();
        Self {
            inner: Arc::new(Mutex::new(Inner { apps })),
            hub,
            guider,
            guider_args: guider_args(guider_ws),
        }
    }

    /// Whether kstars, phd2 and junos-guider run, in `APPS` order.
    fn running(&self, g: &mut Inner) -> [bool; 3] {
        let mut running = g.running();
        running[2] |= self.guider.attached();
        running
    }

    /// Start the background monitor task that polls process status and pushes
    /// `app_state` broadcasts on changes.
    pub fn start_monitor(self) {
        tokio::spawn(async move {
            let mut prev = [false; 3];
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                let cur = self.running(&mut *self.inner.lock().await);
                if cur != prev {
                    prev = cur;
                    self.hub.broadcast_text(build_app_state_msg(cur));
                }
            }
        });
    }

    /// Current status snapshot (non-mutating read).
    pub async fn status_json(&self) -> serde_json::Value {
        status(self.running(&mut *self.inner.lock().await))
    }

    /// The `app_state` message for the current status.
    pub async fn state_msg(&self) -> String {
        build_app_state_msg(self.running(&mut *self.inner.lock().await))
    }

    async fn launch_app(&self, app: &str) -> Result<(), String> {
        let mut g = self.inner.lock().await;
        let running = self.running(&mut g);
        let is_running = |a: &str| APPS.iter().position(|x| *x == a).is_some_and(|i| running[i]);
        if let Some((other, refusal)) = rival(app) {
            if is_running(other) {
                return Err(refusal.to_string());
            }
        }
        if is_running(app) {
            return Ok(()); // already up
        }
        let entry = g.apps.get_mut(app).ok_or_else(|| format!("unknown app: {app}"))?;
        let args: &[String] = if app == APP_GUIDER { &self.guider_args } else { &[] };
        let child = Command::new(app)
            .args(args)
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
    fn a_launched_junos_guider_listens_where_the_relay_dials() {
        assert_eq!(guider_args("ws://127.0.0.1:4410"), vec!["--ws-addr", "127.0.0.1:4410"]);
        assert_eq!(guider_args("ws://astro.local:5000/"), vec!["--ws-addr", "astro.local:5000"]);
        assert!(guider_args("wss://host:4410").is_empty());
        assert!(guider_args("ws://").is_empty());
    }

    #[test]
    fn app_state_keeps_the_old_keys_and_adds_junos_guider() {
        let v: serde_json::Value = serde_json::from_str(&build_app_state_msg([true, false, true])).unwrap();
        assert_eq!(v["type"], "app_state");
        assert_eq!(v["payload"], serde_json::json!({"kstars": "running", "phd2": "stopped", "junos_guider": "running"}));
    }
}
