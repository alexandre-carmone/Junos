//! Shared state between the inbound KStars handlers and the outbound browser handler.
//!
//! In the Ekos Live protocol, KStars is the *client*: it dials our server on
//! `/message/ekos` (JSON) and `/media/ekos` (binary). Browsers connect to `/ws`
//! and we fan-out messages between them and the currently connected KStars session.
//! junos-guider's `guider_*` traffic (`guider.rs`) shares the same fan-out and
//! replay cache, but outlives a KStars attach.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Mutex};

/// Broadcast capacity: number of in-flight messages buffered for slow browsers
/// before they start getting lagged errors.
const BROADCAST_CAP: usize = 512;

/// Which Ekos view a preview frame belongs to, from the media uuid prefix
/// (`kstars/ekos/ekoslive/media.cpp:752-753`, `:587`): `+F` focus, `+A`
/// align / polar align, `+G` guide, anything else the capture preview — plus
/// `+S`, the live stack the relay itself publishes (`stack.rs`), and `+J`,
/// junos-guider's frames (`guider.rs`). The browser subscribes per kind
/// (`proxy.rs`, `junos_media_filter`) so a phone on the Profiles tab is not
/// sent every guide frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrameKind {
    Focus,
    Align,
    Guide,
    Capture,
    /// The live stack, read from KStars' output folder by `stack.rs` — never
    /// a KStars media frame.
    Stack,
    /// junos-guider's guide frame with its overlay (`guider.rs`) — never a
    /// KStars media frame either.
    Guider,
}

impl FrameKind {
    pub const ALL: [FrameKind; 6] =
        [Self::Focus, Self::Align, Self::Guide, Self::Capture, Self::Stack, Self::Guider];

    pub fn from_uuid(uuid: &str) -> Self {
        if uuid.starts_with("+F") {
            Self::Focus
        } else if uuid.starts_with("+A") {
            Self::Align
        } else if uuid.starts_with("+G") {
            Self::Guide
        } else if uuid.starts_with("+S") {
            Self::Stack
        } else if uuid.starts_with("+J") {
            Self::Guider
        } else {
            Self::Capture
        }
    }

    /// The wire spelling the browser uses in `junos_media_filter.kinds`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "+F" => Some(Self::Focus),
            "+A" => Some(Self::Align),
            "+G" => Some(Self::Guide),
            "+S" => Some(Self::Stack),
            "+J" => Some(Self::Guider),
            "capture" => Some(Self::Capture),
            _ => None,
        }
    }
}

/// One frame on the KStars → browsers broadcast. Previews carry their kind so
/// a browser task can filter them without re-parsing the JSON, and the lite
/// variant (`kstars_ws::lite_jpeg`, built only while some browser asked for
/// it) next to the full one.
#[derive(Debug)]
pub enum Outbound {
    Text(String),
    Preview {
        kind: FrameKind,
        full: String,
        lite: Option<String>,
    },
    /// junos-guider's answer to one browser's `guider_rpc`, for the browser
    /// task whose `GuiderSlot` id is `to` (`guider.rs`); the others skip it.
    Reply { to: u64, text: String },
}

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
    // Ekos' Filter Settings table: a reply to the browser's own request
    // (message.cpp:1818 answers under the request's type), never a push, so
    // a reconnecting browser would otherwise wait for its bootstrap.
    "fm_get_data",
    // The Focus module's Aberration Inspector (a KStars fork, see CLAUDE.md):
    // its status and the last run's result are pushed once per run, so a
    // reconnecting browser would otherwise wait for the next run.
    "focus_abins_status",
    "focus_abins_result",
    // junos-guider (`guider.rs`): its snapshots, each sent whole on every
    // change. They survive a KStars attach (`reset_kstars`) and go when the
    // daemon link drops (`forget_guider`). `guider_history` is not one: the
    // daemon sends it only when the link opens, so it is rebuilt from the
    // events (`GuiderHistory`).
    "guider_state",
    "guider_settings",
    "guider_calibration",
    "guider_profiles",
    "guider_devices",
    "guider_darks",
    "guider_ga",
    "guider_pa",
];

/// Message types that belong to junos-guider rather than KStars.
pub const GUIDER_PREFIX: &str = "guider_";

/// The `guider_event`s junos-guider keeps in its `guider_history`
/// (`junos-guider/src/ws.rs`, `on_event`), by their PHD2 `Event` name.
const GUIDER_HISTORY_EVENTS: &[&str] = &[
    "GuideStep",
    "Calibrating",
    "StartCalibration",
    "CalibrationComplete",
    "CalibrationFailed",
    "StartGuiding",
    "GuidingStopped",
    "Paused",
    "Resumed",
    "GuidingDithered",
    "SettleDone",
    "StarLost",
    "LockPositionLost",
    "LoopingExposuresStopped",
];

/// junos-guider's `guider_history {events, states}`: seeded from the one
/// the daemon sends when the link opens, then kept current from the
/// `guider_event`s and the `guider_state` app-state changes, over the same
/// window as the daemon (300 s, at most 2000 of each, counted back from the
/// newest sample's daemon `Timestamp`, never from the relay's clock).
#[derive(Default)]
struct GuiderHistory {
    /// `(Timestamp, event payload text)`.
    events: VecDeque<(f64, String)>,
    /// `(Timestamp, app_state)` at each change.
    states: VecDeque<(f64, String)>,
}

impl GuiderHistory {
    fn from_payload(payload: Option<&Value>) -> Self {
        let list = |key: &str| payload.and_then(|p| p.get(key)).and_then(Value::as_array).cloned().unwrap_or_default();
        let stamp = |v: &Value| v.get("Timestamp").and_then(Value::as_f64);
        Self {
            events: list("events").iter().filter_map(|e| Some((stamp(e)?, e.to_string()))).collect(),
            states: list("states")
                .iter()
                .filter_map(|s| Some((stamp(s)?, s.get("app_state")?.as_str()?.to_string())))
                .collect(),
        }
    }

    fn push(q: &mut VecDeque<(f64, String)>, t: f64, item: String) {
        q.push_back((t, item));
        let oldest = t - GUIDE_WINDOW.as_secs_f64();
        while q.front().is_some_and(|(f, _)| *f < oldest) || q.len() > GUIDE_HISTORY_CAP {
            q.pop_front();
        }
    }

    /// A `guider_event` payload, kept when the daemon would keep it.
    fn event(&mut self, payload: &Value) {
        let name = payload.get("Event").and_then(Value::as_str).unwrap_or_default();
        if let (true, Some(t)) = (GUIDER_HISTORY_EVENTS.contains(&name), payload.get("Timestamp").and_then(Value::as_f64)) {
            Self::push(&mut self.events, t, payload.to_string());
        }
    }

    /// A `guider_state` payload: its app state, when it changed.
    fn state(&mut self, payload: &Value) {
        let (Some(s), Some(t)) =
            (payload.get("app_state").and_then(Value::as_str), payload.get("Timestamp").and_then(Value::as_f64))
        else {
            return;
        };
        if self.states.back().map(|(_, last)| last.as_str()) != Some(s) {
            Self::push(&mut self.states, t, s.to_string());
        }
    }

    fn message(&self) -> String {
        let events: Vec<&str> = self.events.iter().map(|(_, e)| e.as_str()).collect();
        let states: Vec<Value> =
            self.states.iter().map(|(t, s)| serde_json::json!({"Timestamp": t, "app_state": s})).collect();
        format!(
            r#"{{"type":"guider_history","payload":{{"events":[{}],"states":{}}}}}"#,
            events.join(","),
            Value::from(states)
        )
    }
}

/// INDI properties whose last `device_property_{set,get}` reply is kept per
/// device so a reconnecting browser recovers the FOV inputs (sensor geometry
/// and mount coordinates) immediately, short-circuiting the client retry loop.
///
/// Matched against the payload's `name` field — that is the key KStars uses for
/// number/switch/text properties (`indistd.cpp::{numberToJson, switchToJson,
/// textToJson}` all build `{device, name, state, …}`), and the key the client
/// reads in `ws/store.rs::apply_ekos_event`. Only BLOB payloads carry a
/// `property` key (`indistd.cpp:1058`), and no BLOB is listed here.
const STICKY_PROPS: &[&str] = &["CCD_INFO", "EQUATORIAL_EOD_COORD", "FILTER_SLOT", "FILTER_NAME"];

/// Server-side snapshot of the most recent KStars state, replayed to every
/// browser on connect so a dropped/refreshed browser recovers the last preview
/// image, the recent guiding metrics, and the FOV geometry without waiting for
/// KStars to push again. KStars' part is reset on each new KStars attach;
/// junos-guider's (the `guider_*` stickies, the `+J` frame) is kept.
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
    /// Latest preview per kind, so a browser gets the current frame of a
    /// view the moment it subscribes to it (tab switch or reconnect).
    last_preview: HashMap<FrameKind, Arc<Outbound>>,
    /// Recent `new_guide_state` messages within `GUIDE_WINDOW`, in arrival order.
    guide_history: VecDeque<(Instant, String)>,
    /// junos-guider's history, while its link is up.
    guider_history: Option<GuiderHistory>,
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
                let prop = payload.and_then(|p| p.get("name")).and_then(Value::as_str);
                let device = payload.and_then(|p| p.get("device")).and_then(Value::as_str);
                if let (Some(prop), Some(device)) = (prop, device) {
                    if STICKY_PROPS.contains(&prop) {
                        self.latest_props
                            .insert(format!("{device}\u{0}{prop}"), text.to_string());
                    }
                }
            }
            "guider_history" => self.guider_history = Some(GuiderHistory::from_payload(payload)),
            "guider_event" => {
                if let Some(p) = payload {
                    self.guider_history.get_or_insert_with(GuiderHistory::default).event(p);
                }
            }
            "guider_state" => {
                if let Some(p) = payload {
                    if let Some(h) = &mut self.guider_history {
                        h.state(p);
                    }
                }
                self.latest_by_type.insert(t.to_string(), text.to_string());
            }
            other if STICKY_TYPES.contains(&other) => {
                self.latest_by_type.insert(other.to_string(), text.to_string());
            }
            _ => {}
        }
    }

    /// A new KStars session: forget everything but junos-guider's stickies,
    /// history and frame, which a KStars attach says nothing about.
    fn reset_kstars(&mut self) {
        let mut latest_by_type = std::mem::take(&mut self.latest_by_type);
        latest_by_type.retain(|t, _| t.starts_with(GUIDER_PREFIX));
        let mut last_preview = std::mem::take(&mut self.last_preview);
        last_preview.retain(|k, _| *k == FrameKind::Guider);
        let guider_history = self.guider_history.take();
        *self = Self { latest_by_type, last_preview, guider_history, ..Self::default() };
    }

    /// The daemon link dropped: forget junos-guider's stickies and history.
    fn forget_guider(&mut self) {
        self.latest_by_type.retain(|t, _| !t.starts_with(GUIDER_PREFIX));
        self.guider_history = None;
    }

    /// junos-guider's part of the replay, in the daemon's own order:
    /// `guider_state` first (a browser resets its guider view on the
    /// `attached` edge), the other snapshots, then the history.
    fn guider_snapshot(&self) -> Vec<String> {
        let mut out: Vec<String> = self.latest_by_type.get("guider_state").cloned().into_iter().collect();
        out.extend(
            self.latest_by_type
                .iter()
                .filter(|(t, _)| t.starts_with(GUIDER_PREFIX) && *t != "guider_state")
                .map(|(_, m)| m.clone()),
        );
        out.extend(self.guider_history.as_ref().map(GuiderHistory::message));
        out
    }

    /// Ordered messages to replay to a newly-connected browser. Coord-bearing
    /// mount state comes after the status-only one so the client (which merges
    /// partial payloads) ends up with RA/Dec; guide history is chronological.
    /// Previews are not here: they go out per kind once the browser has said
    /// which views it shows (`Hub::last_preview`).
    fn snapshot(&mut self) -> Vec<String> {
        self.prune_guide(Instant::now());
        let mut out = self.guider_snapshot();
        out.extend(self.latest_by_type.iter().filter(|(t, _)| !t.starts_with(GUIDER_PREFIX)).map(|(_, m)| m.clone()));
        out.extend(self.latest_props.values().cloned());
        if let Some(m) = &self.last_mount_state {
            out.push(m.clone());
        }
        if let Some(m) = &self.last_mount_coords {
            out.push(m.clone());
        }
        out.extend(self.guide_history.iter().map(|(_, m)| m.clone()));
        out
    }
}

#[derive(Clone)]
pub struct Hub {
    /// Frames from KStars (message channel + decoded media frames) fanned
    /// out to all connected browsers. Previews are filtered per browser in
    /// `proxy.rs`.
    pub browser_tx: broadcast::Sender<Arc<Outbound>>,

    /// Browsers currently asking for lite previews. `kstars_ws.rs` only
    /// re-encodes a frame while this is non-zero.
    lite_subscribers: Arc<AtomicUsize>,

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
            lite_subscribers: Arc::new(AtomicUsize::new(0)),
            kstars_msg_tx: Arc::new(Mutex::new(None)),
            last_connection_state: Arc::new(Mutex::new(None)),
            last_site_location: Arc::new(Mutex::new(None)),
            replay: Arc::new(Mutex::new(ReplayCache::default())),
        }
    }

    /// Subscribe a new browser to the broadcast of KStars events.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Outbound>> {
        self.browser_tx.subscribe()
    }

    /// Fan a text frame out to every browser.
    pub fn broadcast_text(&self, text: String) {
        let _ = self.browser_tx.send(Arc::new(Outbound::Text(text)));
    }

    /// Whether any browser currently wants lite previews.
    pub fn lite_wanted(&self) -> bool {
        self.lite_subscribers.load(Ordering::Relaxed) > 0
    }

    /// The lite-subscriber counter, for `proxy.rs` to hold while a browser
    /// asks for lite previews.
    pub fn lite_subscribers(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.lite_subscribers)
    }

    /// Fold a KStars text frame into the replay snapshot. Cheap no-op for
    /// message types we don't retain. Called for every KStars → browser frame.
    pub async fn capture(&self, text: &str) {
        self.replay.lock().await.capture(text);
    }

    /// Record the latest decoded preview of its kind, for the next browser
    /// that subscribes to that view.
    pub async fn capture_preview(&self, frame: Arc<Outbound>) {
        if let Outbound::Preview { kind, .. } = frame.as_ref() {
            self.replay.lock().await.last_preview.insert(*kind, frame.clone());
        }
    }

    /// Forget the latest preview of `kind`: a new live stack starts from
    /// nothing, and a browser subscribing then must not get the old one.
    pub async fn clear_preview(&self, kind: FrameKind) {
        self.replay.lock().await.last_preview.remove(&kind);
    }

    /// The latest preview of `kind`, if KStars has sent one this session.
    pub async fn last_preview(&self, kind: FrameKind) -> Option<Arc<Outbound>> {
        self.replay.lock().await.last_preview.get(&kind).cloned()
    }

    /// Ordered messages to replay to a newly-connected browser (recent guide
    /// metrics, FOV geometry, module states). Empty when nothing is cached yet.
    pub async fn replay_snapshot(&self) -> Vec<String> {
        self.replay.lock().await.snapshot()
    }

    /// Drop KStars' part of the replay snapshot. Called when a new KStars
    /// session attaches so a reconnecting browser never sees stale state from
    /// a previous profile; junos-guider's state is not KStars' and stays.
    pub async fn reset_kstars(&self) {
        self.replay.lock().await.reset_kstars();
    }

    /// junos-guider's part of the replay snapshot alone, for a browser that
    /// lagged and lost some of its messages.
    pub async fn replay_guider(&self) -> Vec<String> {
        self.replay.lock().await.guider_snapshot()
    }

    /// Drop junos-guider's snapshots and history: the daemon link dropped.
    pub async fn forget_guider(&self) {
        self.replay.lock().await.forget_guider();
    }

    /// Send a command string to KStars. Returns `false` if no KStars session
    /// is currently connected or the queue is closed.
    ///
    /// The sender is cloned out and the registry lock released *before* the
    /// bounded send is awaited. Holding it across that await wedges the whole
    /// relay: if KStars stops reading its socket, the session loop blocks in
    /// `sink.send(...)`, stops draining `cmd_rx`, and the 256-slot queue fills
    /// — at which point every browser task piles up on this mutex and stops
    /// being polled, so no browser receives events either, and `detach` cannot
    /// take the lock to recover. With the lock released the stall is contained
    /// to the one browser whose command is queued; the rest keep flowing.
    pub async fn send_to_kstars(&self, cmd: String) -> bool {
        let tx = self.kstars_msg_tx.lock().await.clone();
        match tx {
            Some(tx) => tx.send(cmd).await.is_ok(),
            None => false,
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
        // Property payloads key the property name as `name`, exactly as KStars
        // emits them (indistd.cpp::numberToJson). Using `property` here is what
        // previously let a wrong-key lookup pass its own test.
        c.capture(r#"{"type":"device_property_set","payload":{"device":"CCD Simulator","name":"CCD_INFO","state":0,"numbers":[]}}"#);
        // Not on the allowlist — must be dropped.
        c.capture(r#"{"type":"device_property_set","payload":{"device":"CCD Simulator","name":"CCD_TEMPERATURE","state":0,"numbers":[]}}"#);
        c.capture(r#"{"type":"new_align_state","payload":{"status":"Complete"}}"#);
        c.capture(r#"{"type":"focus_abins_status","payload":{"canStart":true,"running":false}}"#);
        c.capture(r#"{"type":"focus_abins_result","payload":{"available":true,"run":1}}"#);

        let snap = types(&c.snapshot());
        assert!(snap.contains(&"focus_abins_status".to_string()));
        assert!(snap.contains(&"focus_abins_result".to_string()));
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

    /// Regression guard for the wire key. KStars serializes number/switch/text
    /// properties as `{device, name, state, …}` (indistd.cpp:1708,1736,1759) —
    /// `property` appears only on BLOBs (:1058). Reading the wrong key made
    /// `latest_props` silently always-empty, so a reconnecting browser never
    /// recovered CCD_INFO / EQUATORIAL_EOD_COORD and had to wait out the
    /// client-side retry loop.
    #[test]
    fn sticky_props_key_off_the_name_field_not_property() {
        let mut c = ReplayCache::default();
        c.capture(r#"{"type":"device_property_get","payload":{"device":"Telescope Simulator","name":"EQUATORIAL_EOD_COORD","state":0,"numbers":[{"name":"RA","value":5.5}]}}"#);
        let snap = c.snapshot();
        assert_eq!(
            snap.iter().filter(|m| m.contains("EQUATORIAL_EOD_COORD")).count(),
            1,
            "a `name`-keyed property reply must be retained"
        );

        // The filter wheel's slot is kept per device, so two wheels don't
        // collide.
        let mut c = ReplayCache::default();
        c.capture(r#"{"type":"device_property_get","payload":{"device":"Filter Simulator","name":"FILTER_SLOT","state":0,"numbers":[{"name":"FILTER_SLOT_VALUE","value":2}]}}"#);
        c.capture(r#"{"type":"device_property_get","payload":{"device":"Filter Wheel 2","name":"FILTER_SLOT","state":0,"numbers":[{"name":"FILTER_SLOT_VALUE","value":5}]}}"#);
        assert_eq!(c.snapshot().iter().filter(|m| m.contains("FILTER_SLOT")).count(), 2);

        // A `property`-keyed payload is a BLOB descriptor, not a property
        // value; it must not populate the replay cache.
        let mut c = ReplayCache::default();
        c.capture(r#"{"type":"device_property_get","payload":{"device":"CCD Simulator","property":"CCD_INFO"}}"#);
        assert!(
            c.snapshot().is_empty(),
            "`property` is the BLOB key and must not be treated as a property name"
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

    #[tokio::test]
    async fn previews_are_kept_per_kind_and_left_out_of_the_snapshot() {
        let hub = Hub::new();
        let frame = |kind, tag: &str| {
            Arc::new(Outbound::Preview { kind, full: tag.to_string(), lite: None })
        };
        hub.capture_preview(frame(FrameKind::Guide, "g1")).await;
        hub.capture_preview(frame(FrameKind::Guide, "g2")).await;
        hub.capture_preview(frame(FrameKind::Focus, "f1")).await;
        hub.capture(r#"{"type":"new_camera_state","payload":{}}"#).await;

        let Some(g) = hub.last_preview(FrameKind::Guide).await else { panic!("no guide frame") };
        assert!(matches!(g.as_ref(), Outbound::Preview { full, .. } if full == "g2"));
        assert!(hub.last_preview(FrameKind::Capture).await.is_none());
        assert_eq!(types(&hub.replay_snapshot().await), vec!["new_camera_state"]);
    }

    #[test]
    fn frame_kind_from_uuid_and_wire() {
        assert_eq!(FrameKind::from_uuid("+F-abc"), FrameKind::Focus);
        assert_eq!(FrameKind::from_uuid("+A"), FrameKind::Align);
        assert_eq!(FrameKind::from_uuid("+G"), FrameKind::Guide);
        assert_eq!(FrameKind::from_uuid("+S-0007"), FrameKind::Stack);
        assert_eq!(FrameKind::from_uuid("fb37-…"), FrameKind::Capture);
        assert_eq!(FrameKind::parse("capture"), Some(FrameKind::Capture));
        assert_eq!(FrameKind::parse("+S"), Some(FrameKind::Stack));
        assert_eq!(FrameKind::from_uuid("+J-12"), FrameKind::Guider);
        assert_eq!(FrameKind::parse("+J"), Some(FrameKind::Guider));
        assert!(FrameKind::ALL.contains(&FrameKind::Guider));
        assert_eq!(FrameKind::parse("+X"), None);
    }

    #[tokio::test]
    async fn reset_kstars_keeps_the_guider_state_and_frame() {
        let hub = Hub::new();
        hub.capture(r#"{"type":"guider_state","payload":{"attached":true}}"#).await;
        hub.capture(r#"{"type":"guider_history","payload":{"events":[]}}"#).await;
        hub.capture(r#"{"type":"new_focus_state","payload":{"status":"Idle"}}"#).await;
        hub.capture(r#"{"type":"new_guide_state","payload":{"drift_ra":1}}"#).await;
        hub.capture(r#"{"type":"new_mount_state","payload":{"ra":1.0,"de":2.0}}"#).await;
        hub.capture(r#"{"type":"device_property_get","payload":{"device":"CCD Simulator","name":"CCD_INFO","state":0,"numbers":[]}}"#).await;
        let frame = |kind| Arc::new(Outbound::Preview { kind, full: String::new(), lite: None });
        hub.capture_preview(frame(FrameKind::Guider)).await;
        hub.capture_preview(frame(FrameKind::Guide)).await;

        hub.reset_kstars().await;
        let mut snap = types(&hub.replay_snapshot().await);
        snap.sort();
        assert_eq!(snap, vec!["guider_history", "guider_state"]);
        assert!(hub.last_preview(FrameKind::Guider).await.is_some());
        assert!(hub.last_preview(FrameKind::Guide).await.is_none());

        // The daemon link drops: its stickies go, KStars' stay.
        hub.capture(r#"{"type":"new_focus_state","payload":{"status":"Idle"}}"#).await;
        hub.forget_guider().await;
        assert_eq!(types(&hub.replay_snapshot().await), vec!["new_focus_state"]);
    }

    #[test]
    fn guider_history_follows_the_events_and_replays_after_the_state() {
        let mut c = ReplayCache::default();
        c.capture(r#"{"type":"guider_settings","payload":{}}"#);
        c.capture(r#"{"type":"guider_state","payload":{"attached":true,"app_state":"Looping","Timestamp":100.0}}"#);
        c.capture(r#"{"type":"guider_history","payload":{"events":[{"Event":"GuideStep","Timestamp":90.0,"Frame":1}],"states":[{"Timestamp":80.0,"app_state":"Looping"}]}}"#);
        c.capture(r#"{"type":"guider_event","payload":{"Event":"GuideStep","Timestamp":101.0,"Frame":2}}"#);
        // Not in the daemon's history: left out of the relay's too.
        c.capture(r#"{"type":"guider_event","payload":{"Event":"StarSelected","Timestamp":101.5}}"#);
        // The same app state again (the 5 s re-push), then a change.
        c.capture(r#"{"type":"guider_state","payload":{"attached":true,"app_state":"Looping","Timestamp":105.0}}"#);
        c.capture(r#"{"type":"guider_state","payload":{"attached":true,"app_state":"Guiding","Timestamp":110.0}}"#);

        let snap = c.snapshot();
        assert_eq!(types(&snap), vec!["guider_state", "guider_settings", "guider_history"]);
        let h: Value = serde_json::from_str(&snap[2]).unwrap();
        let frames: Vec<_> = h["payload"]["events"].as_array().unwrap().iter().map(|e| e["Frame"].clone()).collect();
        assert_eq!(frames, vec![serde_json::json!(1), serde_json::json!(2)]);
        let states: Vec<_> = h["payload"]["states"].as_array().unwrap().iter().map(|s| s["app_state"].clone()).collect();
        assert_eq!(states, vec!["Looping", "Guiding"]);

        // 300 s past the first step, counted on the daemon's stamps.
        c.capture(r#"{"type":"guider_event","payload":{"Event":"GuideStep","Timestamp":395.0,"Frame":3}}"#);
        let h: Value = serde_json::from_str(&c.guider_snapshot()[2]).unwrap();
        assert_eq!(h["payload"]["events"].as_array().unwrap().len(), 2);

        // A KStars attach keeps it; a daemon drop does not.
        c.reset_kstars();
        assert_eq!(types(&c.guider_snapshot()), vec!["guider_state", "guider_settings", "guider_history"]);
        c.forget_guider();
        assert!(c.snapshot().is_empty());
    }
}
