//! Shared state between the inbound KStars handlers and the outbound browser handler.
//!
//! In the Ekos Live protocol, KStars is the *client*: it dials our server on
//! `/message/ekos` (JSON) and `/media/ekos` (binary). Browsers connect to `/ws`
//! and we fan-out messages between them and the currently connected KStars session.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Mutex};

/// Broadcast capacity: number of in-flight messages buffered for slow browsers
/// before they start getting lagged errors.
const BROADCAST_CAP: usize = 512;

/// Per-session command queue capacity (browser → KStars).
pub const CMD_CAP: usize = 256;

/// How far back the guide-metric history is kept for replay to a
/// (re)connecting browser.
const GUIDE_WINDOW: Duration = Duration::from_secs(300);

/// Hard cap on retained guide samples, so a misbehaving high-rate guider can't
/// grow the buffer unbounded even inside `GUIDE_WINDOW`.
const GUIDE_HISTORY_CAP: usize = 2000;

/// "Sticky state" event types: the last message of each kind is replayed to a
/// (re)connecting browser so it recovers module state without waiting for the
/// next KStars push. `new_mount_state` and the device-property replies are
/// handled specially (see `ReplayCache::capture`) and are intentionally absent.
const STICKY_TYPES: &[&str] = &[
    "new_camera_state",
    "new_focus_state",
    "new_align_state",
    "new_capture_state",
    "new_cap_state",
    "new_scheduler_state",
    "new_polar_state",
    "new_livestacker_state",
    "get_scopes",
    "train_get_all",
];

/// INDI properties whose last `device_property_{set,get}` reply is kept per
/// device so a reconnecting browser recovers the FOV inputs (sensor geometry
/// and mount coordinates) immediately, short-circuiting the client retry loop.
const STICKY_PROPS: &[&str] = &["CCD_INFO", "EQUATORIAL_EOD_COORD"];

/// Server-side snapshot of the most recent KStars state, replayed to every
/// browser on connect so a dropped/refreshed browser recovers the last preview
/// image, the recent guiding metrics, and the FOV geometry without waiting for
/// KStars to push again. Reset on each new KStars attach.
#[derive(Default)]
struct ReplayCache {
    /// Latest message per sticky type (see `STICKY_TYPES`), keyed by type.
    latest_by_type: HashMap<String, String>,
    /// Latest device-property reply per `"device\0property"` (see `STICKY_PROPS`).
    latest_props: HashMap<String, String>,
    /// Latest `new_mount_state` overall (may be status-only, partial payload).
    last_mount_state: Option<String>,
    /// Latest `new_mount_state` that actually carried RA/Dec. Kept apart from
    /// `last_mount_state` because status-only updates would otherwise clobber
    /// the coordinates (message.cpp emits `new_mount_state` from several sites
    /// with partial payloads).
    last_mount_coords: Option<String>,
    /// Latest `new_preview_image` (decoded from a media frame).
    last_preview: Option<String>,
    /// Recent `new_guide_state` messages within `GUIDE_WINDOW`, in arrival order.
    guide_history: VecDeque<(Instant, String)>,
}

impl ReplayCache {
    fn prune_guide(&mut self, now: Instant) {
        while let Some((t, _)) = self.guide_history.front() {
            if now.duration_since(*t) > GUIDE_WINDOW {
                self.guide_history.pop_front();
            } else {
                break;
            }
        }
        while self.guide_history.len() > GUIDE_HISTORY_CAP {
            self.guide_history.pop_front();
        }
    }

    fn capture(&mut self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let Some(t) = v.get("type").and_then(Value::as_str) else {
            return;
        };
        let payload = v.get("payload");
        match t {
            "new_guide_state" => {
                let now = Instant::now();
                self.guide_history.push_back((now, text.to_string()));
                self.prune_guide(now);
            }
            "new_mount_state" => {
                if payload.and_then(|p| p.get("ra")).is_some() {
                    self.last_mount_coords = Some(text.to_string());
                }
                self.last_mount_state = Some(text.to_string());
            }
            "device_property_set" | "device_property_get" => {
                let prop = payload.and_then(|p| p.get("property")).and_then(Value::as_str);
                let device = payload.and_then(|p| p.get("device")).and_then(Value::as_str);
                if let (Some(prop), Some(device)) = (prop, device) {
                    if STICKY_PROPS.contains(&prop) {
                        self.latest_props
                            .insert(format!("{device}\u{0}{prop}"), text.to_string());
                    }
                }
            }
            other if STICKY_TYPES.contains(&other) => {
                self.latest_by_type.insert(other.to_string(), text.to_string());
            }
            _ => {}
        }
    }

    /// Ordered messages to replay to a newly-connected browser. Coord-bearing
    /// mount state comes after the status-only one so the client (which merges
    /// partial payloads) ends up with RA/Dec; guide history is chronological.
    fn snapshot(&mut self) -> Vec<String> {
        self.prune_guide(Instant::now());
        let mut out = Vec::new();
        out.extend(self.latest_by_type.values().cloned());
        out.extend(self.latest_props.values().cloned());
        if let Some(m) = &self.last_mount_state {
            out.push(m.clone());
        }
        if let Some(m) = &self.last_mount_coords {
            out.push(m.clone());
        }
        out.extend(self.guide_history.iter().map(|(_, m)| m.clone()));
        if let Some(p) = &self.last_preview {
            out.push(p.clone());
        }
        out
    }
}

#[derive(Clone)]
pub struct Hub {
    /// Text frames from KStars (message channel + decoded media frames)
    /// fanned out to all connected browsers.
    pub browser_tx: broadcast::Sender<String>,

    /// Current outbound pipe to the KStars message channel.
    /// `None` while no KStars session is connected. Set by the `/message/ekos`
    /// handler on connect and cleared on disconnect.
    pub kstars_msg_tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,

    /// Last `new_connection_state` received from KStars, replayed to browsers
    /// on connect so a page refresh gets the full connected+online state.
    pub last_connection_state: Arc<Mutex<Option<String>>>,

    /// Last `astro_get_location` reply from KStars (the observer site), replayed
    /// to browsers on connect so a late-joining page gets KStars' real location
    /// immediately without having to re-query it. Primed once per KStars attach.
    pub last_site_location: Arc<Mutex<Option<String>>>,

    /// Rolling snapshot of recent KStars state (last preview image, recent
    /// guiding metrics, FOV geometry) replayed to a browser on connect so a
    /// dropped/refreshed page recovers without waiting for the next push.
    replay: Arc<Mutex<ReplayCache>>,
}

impl Hub {
    pub fn new() -> Self {
        let (browser_tx, _) = broadcast::channel(BROADCAST_CAP);
        Self {
            browser_tx,
            kstars_msg_tx: Arc::new(Mutex::new(None)),
            last_connection_state: Arc::new(Mutex::new(None)),
            last_site_location: Arc::new(Mutex::new(None)),
            replay: Arc::new(Mutex::new(ReplayCache::default())),
        }
    }

    /// Subscribe a new browser to the broadcast of KStars events.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.browser_tx.subscribe()
    }

    /// Fold a KStars text frame into the replay snapshot. Cheap no-op for
    /// message types we don't retain. Called for every KStars → browser frame.
    pub async fn capture(&self, text: &str) {
        self.replay.lock().await.capture(text);
    }

    /// Record the latest decoded preview image for replay on browser connect.
    pub async fn capture_preview(&self, msg: &str) {
        self.replay.lock().await.last_preview = Some(msg.to_string());
    }

    /// Ordered messages to replay to a newly-connected browser (last preview,
    /// recent guide metrics, FOV geometry). Empty when nothing is cached yet.
    pub async fn replay_snapshot(&self) -> Vec<String> {
        self.replay.lock().await.snapshot()
    }

    /// Drop the replay snapshot. Called when a new KStars session attaches so a
    /// reconnecting browser never sees stale state from a previous profile.
    pub async fn reset_replay(&self) {
        *self.replay.lock().await = ReplayCache::default();
    }

    /// Send a command string to KStars. Returns `false` if no KStars session
    /// is currently connected or the queue is closed.
    pub async fn send_to_kstars(&self, cmd: String) -> bool {
        let guard = self.kstars_msg_tx.lock().await;
        if let Some(tx) = guard.as_ref() {
            tx.send(cmd).await.is_ok()
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(msgs: &[String]) -> Vec<String> {
        msgs.iter()
            .filter_map(|m| {
                serde_json::from_str::<Value>(m)
                    .ok()?
                    .get("type")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn snapshot_retains_sticky_state_and_props() {
        let mut c = ReplayCache::default();
        c.capture(r#"{"type":"get_scopes","payload":[]}"#);
        c.capture(r#"{"type":"new_focus_state","payload":{"status":"Idle"}}"#);
        c.capture(r#"{"type":"device_property_set","payload":{"device":"CCD Simulator","property":"CCD_INFO"}}"#);
        // Not on the allowlist — must be dropped.
        c.capture(r#"{"type":"device_property_set","payload":{"device":"CCD Simulator","property":"CCD_TEMPERATURE"}}"#);
        c.capture(r#"{"type":"new_align_state","payload":{"status":"Complete"}}"#);

        let snap = types(&c.snapshot());
        assert!(snap.contains(&"get_scopes".to_string()));
        assert!(snap.contains(&"new_focus_state".to_string()));
        assert!(snap.contains(&"new_align_state".to_string()));
        assert!(snap.contains(&"device_property_set".to_string()));
        // Only the allowlisted CCD_INFO property was kept.
        assert_eq!(
            snap.iter().filter(|t| *t == "device_property_set").count(),
            1
        );
    }

    #[test]
    fn sticky_state_keeps_only_latest_per_type() {
        let mut c = ReplayCache::default();
        c.capture(r#"{"type":"new_focus_state","payload":{"status":"Framing"}}"#);
        c.capture(r#"{"type":"new_focus_state","payload":{"status":"Complete"}}"#);
        let snap = c.snapshot();
        assert_eq!(types(&snap), vec!["new_focus_state"]);
        assert!(snap[0].contains("Complete"));
    }

    #[test]
    fn mount_coords_replayed_after_status_only() {
        let mut c = ReplayCache::default();
        c.capture(r#"{"type":"new_mount_state","payload":{"ra":12.3,"de":45.6}}"#);
        // Status-only update must not clobber the retained coordinates.
        c.capture(r#"{"type":"new_mount_state","payload":{"status":"Tracking"}}"#);
        let snap = c.snapshot();
        let mount: Vec<&String> = snap
            .iter()
            .filter(|m| m.contains("new_mount_state"))
            .collect();
        // Two frames: last-any (status), then coord-bearing last so RA/Dec wins.
        assert_eq!(mount.len(), 2);
        assert!(mount[0].contains("Tracking"));
        assert!(mount[1].contains("\"ra\":12.3"));
    }

    #[test]
    fn guide_history_is_chronological_and_bounded() {
        let mut c = ReplayCache::default();
        for i in 0..(GUIDE_HISTORY_CAP + 50) {
            c.capture(&format!(
                r#"{{"type":"new_guide_state","payload":{{"drift_ra":{i}}}}}"#
            ));
        }
        let snap = c.snapshot();
        let guides: Vec<&String> = snap.iter().filter(|m| m.contains("new_guide_state")).collect();
        assert_eq!(guides.len(), GUIDE_HISTORY_CAP);
        // Oldest dropped, newest kept, order preserved.
        assert!(guides.last().unwrap().contains(&format!("{}", GUIDE_HISTORY_CAP + 49)));
    }

    #[test]
    fn preview_is_replayed_last() {
        let mut c = ReplayCache::default();
        c.last_preview = Some(r#"{"type":"new_preview_image","payload":{}}"#.to_string());
        c.capture(r#"{"type":"new_camera_state","payload":{}}"#);
        let snap = c.snapshot();
        assert_eq!(
            snap.last().and_then(|m| serde_json::from_str::<Value>(m).ok().and_then(|v| v["type"].as_str().map(str::to_string))),
            Some("new_preview_image".to_string())
        );
    }
}
