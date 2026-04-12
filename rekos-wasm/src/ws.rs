//! Minimal WebSocket + DeviceStore for milestone 1.
//!
//! Scope: attach to the rekos-server `/ws` endpoint and decode just the
//! Ekos Live messages the planetarium needs for the FOV reticle.
//!
//! # Where FOV inputs actually come from
//!
//! The FOV reticle needs focal length, aperture, pixel size and sensor
//! dimensions. These live in three different places in KStars:
//!
//! 1. **Scope focal length + aperture** — `get_scopes` returns the OAL
//!    scope DB as `[{name, focal_length, aperture, …}]`. We match the
//!    active train's `scope` field against this list by name.
//! 2. **Active train's scope/camera names** — `train_get_all` returns
//!    records `[{id, name, scope, camera, mount, …}]`. Take the first
//!    entry as the active train.
//! 3. **Camera pixel size + sensor dimensions** — these come from the
//!    INDI `CCD_INFO` number property. Fetch via
//!    `device_property_get {device: <camera name>, property: "CCD_INFO"}`.
//!    The reply comes back as a `device_property_get` event whose payload
//!    is `{device, name, state, numbers:[{name, value}, …]}` (compact
//!    form from `kstars/indi/indistd.cpp::numberToJson`). Relevant element
//!    names: `CCD_MAX_X`, `CCD_MAX_Y`, `CCD_PIXEL_SIZE_X`, `CCD_PIXEL_SIZE_Y`,
//!    or the fallback `CCD_PIXEL_SIZE`.
//!
//! Note: `train_settings_get` is NOT a source for FOV data — it returns
//! `OpticalTrainSettings`, a map of module-enum IDs (`"0"`, `"1"`, …) to
//! per-module configs (Capture/Focus/Guide/Align settings), not hardware.

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use gloo_net::websocket::{futures::WebSocket, Message};
use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

/// Type-erased command sink. Components dispatch raw Ekos Live JSON strings.
pub type SendCmd = Arc<dyn Fn(String) + Send + Sync>;

// ---------------------------------------------------------------------------
// Data types consumed by compat.rs / sky components
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct MountStatusData {
    pub device: String,
    pub connected: bool,
    pub slewing: bool,
    pub tracking: bool,
    pub parked: bool,
    pub ra_h: Option<f64>,
    pub dec_deg: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct CameraStatusData {
    pub device: String,
    pub connected: bool,
    pub pixel_size_um: Option<f64>,
    pub sensor_width: Option<u32>,
    pub sensor_height: Option<u32>,
    // Live imaging state — driven by new_camera_state / new_capture_state /
    // INDI CCD_TEMPERATURE / CCD_COOLER.
    pub temperature: Option<f64>,
    pub target_temperature: Option<f64>,
    pub cooler_on: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct CaptureStatusData {
    pub status: String,
    pub target: String,
    pub seq_total: Option<i64>,
    pub seq_current: Option<i64>,
    pub progress: Option<f64>,
    pub overall_remaining: Option<f64>,
    pub log: String,
}

#[derive(Debug, Clone, Default)]
pub struct TelescopeSettingsData {
    pub focal_length_mm: Option<f64>,
    pub aperture_mm: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct OpticalTrain {
    pub id: i64,
    pub name: String,
    pub mount: String,
    pub camera: String,
    pub scope: String,
    pub guider: String,
    pub focuser: String,
}

#[derive(Debug, Clone, Default)]
pub struct FocusStatusData {
    pub device: String,
    pub connected: bool,
    pub status: String,
    pub hfr: Option<f64>,
    pub position: Option<i64>,
    pub temperature: Option<f64>,
    pub log: String,
}

#[derive(Debug, Clone)]
pub struct HfrSample {
    pub t_ms: f64,
    pub hfr: f64,
    pub position: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct ScopeInfo {
    pub name: String,
    pub focal_length_mm: f64,
    pub aperture_mm: f64,
}

// ---------------------------------------------------------------------------
// Stub types still referenced by sky/actions.rs via crate-level contexts.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum SolveRadius {
    #[default]
    Large,
    Medium,
    Small,
    Narrow,
    VeryNarrow,
}

#[derive(Debug, Clone, Default)]
pub struct AlignDefaultsData {
    pub exposure_s: Option<f64>,
    pub accuracy_arcsec: Option<f64>,
    pub max_iterations: Option<u32>,
    pub solve_radius: Option<SolveRadius>,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct DeviceStore {
    pub connected:          RwSignal<bool>,
    /// Ekos::Success, i.e. a profile is running and requests that gate on it
    /// will actually be handled. Driven by `new_connection_state.online`.
    pub online:             RwSignal<bool>,
    pub mount_status:       RwSignal<Option<MountStatusData>>,
    pub camera_status:      RwSignal<Option<CameraStatusData>>,
    pub telescope_settings: RwSignal<TelescopeSettingsData>,
    pub optical_trains:     RwSignal<Vec<OpticalTrain>>,
    pub scopes:             RwSignal<Vec<ScopeInfo>>,
    pub focus_status:       RwSignal<Option<FocusStatusData>>,
    pub focus_settings:     RwSignal<serde_json::Value>,
    pub focus_preview_url:  RwSignal<Option<String>>,
    pub focus_hfr_history:  RwSignal<Vec<HfrSample>>,
    pub capture_status:     RwSignal<CaptureStatusData>,
    pub capture_settings:   RwSignal<serde_json::Value>,
    pub capture_sequence:   RwSignal<serde_json::Value>,
    pub capture_preview_url:RwSignal<Option<String>>,
}

impl DeviceStore {
    fn new() -> Self {
        Self {
            connected:          RwSignal::new(false),
            online:             RwSignal::new(false),
            mount_status:       RwSignal::new(None),
            camera_status:      RwSignal::new(None),
            telescope_settings: RwSignal::new(TelescopeSettingsData::default()),
            optical_trains:     RwSignal::new(Vec::new()),
            scopes:             RwSignal::new(Vec::new()),
            focus_status:       RwSignal::new(None),
            focus_settings:     RwSignal::new(serde_json::Value::Null),
            focus_preview_url:  RwSignal::new(None),
            focus_hfr_history:  RwSignal::new(Vec::new()),
            capture_status:     RwSignal::new(CaptureStatusData::default()),
            capture_settings:   RwSignal::new(serde_json::Value::Null),
            capture_sequence:   RwSignal::new(serde_json::Value::Null),
            capture_preview_url:RwSignal::new(None),
        }
    }

    fn apply_ekos_event(&self, type_str: &str, payload: &serde_json::Value) {
        match type_str {
            "new_connection_state" => {
                let connected = payload["connected"].as_bool().unwrap_or(false);
                let online = payload["online"].as_bool().unwrap_or(false);
                self.connected.set(connected);
                self.online.set(connected && online);
                if !connected {
                    self.mount_status.set(None);
                    self.camera_status.set(None);
                    self.focus_status.set(None);
                    self.focus_preview_url.set(None);
                    self.focus_hfr_history.set(Vec::new());
                }
            }

            "new_mount_state" => {
                self.mount_status.update(|opt| {
                    let ms = opt.get_or_insert_with(MountStatusData::default);
                    if let Some(dev) = payload["device"].as_str() {
                        if !dev.is_empty() { ms.device = dev.to_string(); }
                    }
                    if let Some(status) = payload["status"].as_str() {
                        let sl = status.to_lowercase();
                        ms.slewing  = sl.contains("slewing");
                        ms.tracking = sl.contains("tracking");
                        ms.parked   = sl.contains("park");
                        ms.connected = true;
                    }
                    // KStars sends RA and Dec in degrees.
                    if let Some(ra_deg) = payload["ra"].as_f64() { ms.ra_h = Some(ra_deg / 15.0); }
                    if let Some(dec)    = payload["de"].as_f64() { ms.dec_deg = Some(dec); }
                });
            }

            "get_scopes" => {
                // OAL scope DB — full list, not just the active one.
                // Shape: [{ id, model, vendor, type, name, focal_length, aperture }].
                if let Some(arr) = payload.as_array() {
                    let scopes: Vec<ScopeInfo> = arr.iter().map(|s| ScopeInfo {
                        name:            s["name"].as_str().unwrap_or("").to_string(),
                        focal_length_mm: s["focal_length"].as_f64().unwrap_or(0.0),
                        aperture_mm:     s["aperture"].as_f64().unwrap_or(0.0),
                    }).collect();
                    self.scopes.set(scopes);
                }
            }

            "train_get_all" => {
                if let Some(arr) = payload.as_array() {
                    let trains: Vec<OpticalTrain> = arr.iter().map(|t| OpticalTrain {
                        id:     t["id"].as_i64().unwrap_or(0),
                        name:   t["name"].as_str().unwrap_or("").to_string(),
                        mount:  t["mount"].as_str().unwrap_or("").to_string(),
                        camera: t["camera"].as_str().unwrap_or("").to_string(),
                        scope:  t["scope"].as_str().unwrap_or("").to_string(),
                        guider: t["guider"].as_str().unwrap_or("").to_string(),
                        focuser: t["focuser"].as_str().unwrap_or("").to_string(),
                    }).collect();
                    for t in &trains {
                        leptos::logging::log!(
                            "[ws] train: name={:?} mount={:?} scope={:?} camera={:?}",
                            t.name, t.mount, t.scope, t.camera
                        );
                    }
                    // Carry the first train's camera name into camera_status so
                    // it's visible before CCD_INFO comes back.
                    if let Some(first) = trains.first() {
                        self.camera_status.update(|opt| {
                            let cs = opt.get_or_insert_with(CameraStatusData::default);
                            if !first.camera.is_empty() { cs.device = first.camera.clone(); }
                        });
                    }
                    self.optical_trains.set(trains);
                }
            }

            // INDI property reply — either a direct get response, or a
            // pushed update from a prior device_property_subscribe.
            // We only consume two properties: CCD_INFO (camera sensor specs)
            // and EQUATORIAL_EOD_COORD (mount RA/Dec fast path).
            "device_property_get" | "device_property_set" => {
                let prop = payload["name"].as_str().unwrap_or("");
                leptos::logging::log!(
                    "[ws] recv {} device={} prop={}",
                    type_str,
                    payload["device"].as_str().unwrap_or("?"),
                    prop
                );

                if prop == "CCD_INFO" {
                    let mut max_x: Option<f64> = None;
                    let mut max_y: Option<f64> = None;
                    let mut pix_x: Option<f64> = None;
                    let mut pix_y: Option<f64> = None;
                    let mut pix_any: Option<f64> = None;
                    if let Some(arr) = payload["numbers"].as_array() {
                        for el in arr {
                            let n = el["name"].as_str().unwrap_or("");
                            let v = el["value"].as_f64();
                            match n {
                                "CCD_MAX_X"        => max_x   = v,
                                "CCD_MAX_Y"        => max_y   = v,
                                "CCD_PIXEL_SIZE_X" => pix_x   = v,
                                "CCD_PIXEL_SIZE_Y" => pix_y   = v,
                                "CCD_PIXEL_SIZE"   => pix_any = v,
                                _ => {}
                            }
                        }
                    }
                    let pix = pix_x.or(pix_y).or(pix_any);
                    let sw  = max_x.map(|v| v as u32);
                    let sh  = max_y.map(|v| v as u32);
                    if pix.is_some() || sw.is_some() || sh.is_some() {
                        self.camera_status.update(|opt| {
                            let cs = opt.get_or_insert_with(CameraStatusData::default);
                            if pix.is_some() { cs.pixel_size_um = pix; }
                            if sw.is_some()  { cs.sensor_width  = sw; }
                            if sh.is_some()  { cs.sensor_height = sh; }
                        });
                    }
                } else if prop == "ABS_FOCUS_POSITION" {
                    let mut pos: Option<i64> = None;
                    if let Some(arr) = payload["numbers"].as_array() {
                        for el in arr {
                            if el["name"].as_str() == Some("FOCUS_ABSOLUTE_POSITION") {
                                pos = el["value"].as_f64().map(|v| v as i64);
                            }
                        }
                    }
                    if pos.is_some() {
                        self.focus_status.update(|opt| {
                            let fs = opt.get_or_insert_with(FocusStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() { fs.device = dev.to_string(); }
                            }
                            if pos.is_some() { fs.position = pos; }
                        });
                    }
                } else if prop == "FOCUS_TEMPERATURE" {
                    let mut temp: Option<f64> = None;
                    if let Some(arr) = payload["numbers"].as_array() {
                        for el in arr {
                            if el["name"].as_str() == Some("TEMPERATURE") {
                                temp = el["value"].as_f64();
                            }
                        }
                    }
                    if temp.is_some() {
                        self.focus_status.update(|opt| {
                            let fs = opt.get_or_insert_with(FocusStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() { fs.device = dev.to_string(); }
                            }
                            fs.temperature = temp;
                        });
                    }
                } else if prop == "CCD_TEMPERATURE" {
                    let mut t: Option<f64> = None;
                    if let Some(arr) = payload["numbers"].as_array() {
                        for el in arr {
                            if el["name"].as_str() == Some("CCD_TEMPERATURE_VALUE") {
                                t = el["value"].as_f64();
                            }
                        }
                    }
                    if t.is_some() {
                        self.camera_status.update(|opt| {
                            let cs = opt.get_or_insert_with(CameraStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() { cs.device = dev.to_string(); }
                            }
                            cs.temperature = t;
                        });
                    }
                } else if prop == "CCD_COOLER" {
                    let mut on: Option<bool> = None;
                    if let Some(arr) = payload["switches"].as_array() {
                        for el in arr {
                            let n = el["name"].as_str().unwrap_or("");
                            let v = el["value"].as_bool()
                                .or_else(|| el["state"].as_str().map(|s| s == "On"));
                            if n == "COOLER_ON" { on = v; }
                        }
                    }
                    if on.is_some() {
                        self.camera_status.update(|opt| {
                            let cs = opt.get_or_insert_with(CameraStatusData::default);
                            cs.cooler_on = on;
                        });
                    }
                } else if prop == "EQUATORIAL_EOD_COORD" {
                    // INDI mount coord property: RA in hours, DEC in degrees.
                    let mut ra_h: Option<f64> = None;
                    let mut de_d: Option<f64> = None;
                    if let Some(arr) = payload["numbers"].as_array() {
                        for el in arr {
                            let n = el["name"].as_str().unwrap_or("");
                            let v = el["value"].as_f64();
                            match n {
                                "RA"  => ra_h = v,
                                "DEC" => de_d = v,
                                _ => {}
                            }
                        }
                    }
                    if ra_h.is_some() || de_d.is_some() {
                        self.mount_status.update(|opt| {
                            let ms = opt.get_or_insert_with(MountStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() { ms.device = dev.to_string(); }
                            }
                            if ra_h.is_some() { ms.ra_h = ra_h; }
                            if de_d.is_some() { ms.dec_deg = de_d; }
                        });
                    }
                }
            }

            // Focus module state. Partial payloads: any subset of
            // {status, hfr, pos, log, focusAdvisorMessage, focusAdvisorStage,
            //  focusinitHFRPlot, title}. See kstars manager.cpp:2530+ for the
            // emission sites.
            "new_focus_state" => {
                let hfr = payload["hfr"].as_f64();
                let pos = payload["pos"].as_i64()
                    .or_else(|| payload["pos"].as_f64().map(|v| v as i64));
                self.focus_status.update(|opt| {
                    let fs = opt.get_or_insert_with(FocusStatusData::default);
                    fs.connected = true;
                    if let Some(s) = payload["status"].as_str() {
                        fs.status = s.to_string();
                    }
                    if let Some(h) = hfr { fs.hfr = Some(h); }
                    if let Some(p) = pos { fs.position = Some(p); }
                    if let Some(l) = payload["log"].as_str() {
                        if !l.is_empty() { fs.log = l.to_string(); }
                    }
                });
                if let Some(h) = hfr {
                    if h > 0.0 && h.is_finite() {
                        let t_ms = web_sys::js_sys::Date::now();
                        self.focus_hfr_history.update(|v| {
                            v.push(HfrSample { t_ms, hfr: h, position: pos });
                            if v.len() > 200 {
                                let drop = v.len() - 200;
                                v.drain(..drop);
                            }
                        });
                    }
                }
            }

            // Debounced settings snapshot reply (see message.cpp:734).
            "focus_get_all_settings" => {
                self.focus_settings.set(payload.clone());
            }

            // Camera state: temperature (see message.cpp:446 sendTemperature).
            // Payload is {name, temperature}.
            "new_camera_state" => {
                self.camera_status.update(|opt| {
                    let cs = opt.get_or_insert_with(CameraStatusData::default);
                    if let Some(dev) = payload["name"].as_str() {
                        if !dev.is_empty() { cs.device = dev.to_string(); }
                    }
                    if let Some(t) = payload["temperature"].as_f64() {
                        cs.temperature = Some(t);
                    }
                });
            }

            // Capture module status (message.cpp:2567). Partial payloads.
            "new_capture_state" => {
                self.capture_status.update(|c| {
                    if let Some(s) = payload["status"].as_str() {
                        c.status = s.to_string();
                    }
                    if let Some(t) = payload["target"].as_str() {
                        c.target = t.to_string();
                    }
                    if let Some(v) = payload["seqTotal"].as_i64() { c.seq_total = Some(v); }
                    if let Some(v) = payload["seqCurrent"].as_i64() { c.seq_current = Some(v); }
                    if let Some(v) = payload["progress"].as_f64() { c.progress = Some(v); }
                    if let Some(v) = payload["overallRemaining"].as_f64() {
                        c.overall_remaining = Some(v);
                    }
                    if let Some(l) = payload["log"].as_str() {
                        if !l.is_empty() { c.log = l.to_string(); }
                    }
                });
            }

            "capture_get_all_settings" => {
                self.capture_settings.set(payload.clone());
            }

            "capture_get_sequences" => {
                self.capture_sequence.set(payload.clone());
            }

            // Binary media frames come through as JSON after server-side
            // decoding (rekos-server/src/kstars_ws.rs:169-198). Metadata is the
            // parsed header; we only consume focus frames (uuid starts with
            // "+F", see kstars media.cpp:752).
            "new_preview_image" => {
                let uuid = payload["metadata"]["uuid"].as_str().unwrap_or("");
                if let Some(b64) = payload["data"].as_str() {
                    let ext = payload["metadata"]["ext"].as_str().unwrap_or("jpg");
                    let mime = if ext == "jpg" { "image/jpeg" } else { "image/png" };
                    let url = format!("data:{};base64,{}", mime, b64);
                    // Focus frames are uuid-prefixed "+F" (kstars media.cpp:752).
                    // Everything else → capture preview target.
                    if uuid.starts_with("+F") {
                        self.focus_preview_url.set(Some(url));
                    } else {
                        self.capture_preview_url.set(Some(url));
                    }
                }
            }

            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Public hook
// ---------------------------------------------------------------------------

pub fn use_rekos_ws() -> (DeviceStore, SendCmd) {
    let store = DeviceStore::new();

    let ws_url = {
        let window   = web_sys::window().unwrap();
        let location = window.location();
        let host     = location.host().unwrap_or_else(|_| "localhost:8080".into());
        let proto    = location.protocol().unwrap_or_else(|_| "http:".into());
        let ws_proto = if proto == "https:" { "wss:" } else { "ws:" };
        format!("{}//{}/ws", ws_proto, host)
    };

    let (cmd_tx, mut cmd_rx) = futures::channel::mpsc::unbounded::<String>();

    let send_fn: SendCmd = Arc::new(move |json: String| {
        let _ = cmd_tx.unbounded_send(json);
    });

    // Prime: once Ekos is online, fetch scope DB, active train list, and the
    // debounced focus settings snapshot. All bypass the Ekos::Success gate.
    let online_sig = store.online;
    {
        let prime_send = send_fn.clone();
        Effect::new(move |_| {
            if online_sig.get() {
                prime_send(r#"{"type":"get_scopes","payload":{}}"#.to_string());
                prime_send(r#"{"type":"train_get_all","payload":{}}"#.to_string());
                prime_send(r#"{"type":"focus_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"capture_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"capture_get_sequences","payload":{}}"#.to_string());
            }
        });
    }

    // Cross-reference: when both the scopes list and the active train are
    // known, match the train's scope name against the scope DB and write
    // focal length + aperture into telescope_settings.
    let trains_sig = store.optical_trains;
    let scopes_sig = store.scopes;
    let telescope_sig = store.telescope_settings;
    Effect::new(move |_| {
        let trains = trains_sig.get();
        let scopes = scopes_sig.get();
        let Some(train) = trains.first() else { return };
        if train.scope.is_empty() || scopes.is_empty() { return; }
        if let Some(s) = scopes.iter().find(|s| s.name == train.scope) {
            telescope_sig.update(|t| {
                t.focal_length_mm = Some(s.focal_length_mm);
                t.aperture_mm     = Some(s.aperture_mm);
            });
        }
    });

    // Subscribe + get the INDI properties we care about on the active train's
    // camera (CCD_INFO) and mount (EQUATORIAL_EOD_COORD).
    //
    // Pitfall: `processDeviceCommands` in kstars/ekos/ekoslive/message.cpp:1664
    // drops the command silently if `INDIListener::findDevice` fails — and the
    // INDI driver may not be registered yet right after profile start. So
    // fire-and-forget isn't enough: we retry on a timer until the property
    // actually comes back, then stop.
    {
        let trains_sig2 = store.optical_trains;
        let camera_sig  = store.camera_status;
        let mount_sig   = store.mount_status;
        let focus_sig   = store.focus_status;
        let last_cam     = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let last_mount   = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let last_focuser = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let send_for_effect = send_fn.clone();
        Effect::new(move |_| {
            let trains = trains_sig2.get();
            let Some(train) = trains.first() else { return };

            if !train.camera.is_empty() && train.camera != "--"
                && *last_cam.borrow() != train.camera
            {
                *last_cam.borrow_mut() = train.camera.clone();
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_INFO",
                    camera_sig,
                    |cs| cs.as_ref().and_then(|c| c.sensor_width).is_some(),
                );
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_TEMPERATURE",
                    camera_sig,
                    |cs| cs.as_ref().and_then(|c| c.temperature).is_some(),
                );
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_COOLER",
                    camera_sig,
                    |cs| cs.as_ref().and_then(|c| c.cooler_on).is_some(),
                );
            }

            if !train.mount.is_empty() && train.mount != "--"
                && *last_mount.borrow() != train.mount
            {
                *last_mount.borrow_mut() = train.mount.clone();
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.mount.clone(),
                    "EQUATORIAL_EOD_COORD",
                    mount_sig,
                    |ms| ms.as_ref().and_then(|m| m.ra_h).is_some(),
                );
            }

            if !train.focuser.is_empty() && train.focuser != "--"
                && *last_focuser.borrow() != train.focuser
            {
                *last_focuser.borrow_mut() = train.focuser.clone();
                // Seed focus_status with the focuser device name so UI has
                // something to show before the first new_focus_state lands.
                focus_sig.update(|opt| {
                    let fs = opt.get_or_insert_with(FocusStatusData::default);
                    fs.device = train.focuser.clone();
                });
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.focuser.clone(),
                    "ABS_FOCUS_POSITION",
                    focus_sig,
                    |fs| fs.as_ref().and_then(|f| f.position).is_some(),
                );
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.focuser.clone(),
                    "FOCUS_TEMPERATURE",
                    focus_sig,
                    |fs| fs.as_ref().and_then(|f| f.temperature).is_some(),
                );
            }
        });
    }

    // Long-lived refresh loop — keeps INDI properties current after bootstrap.
    spawn_refresh_loop(send_fn.clone(), store.clone());

    let store_for_ws = store.clone();
    spawn_local(async move {
        let ws = match WebSocket::open(&ws_url) {
            Ok(ws) => ws,
            Err(e) => {
                leptos::logging::error!("WS open failed: {:?}", e);
                return;
            }
        };
        let (mut writer, mut reader) = ws.split();

        spawn_local(async move {
            while let Some(msg) = cmd_rx.next().await {
                if writer.send(Message::Text(msg)).await.is_err() { break; }
            }
        });

        while let Some(msg) = reader.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                        let type_str = val["type"].as_str().unwrap_or("").to_string();
                        let payload  = val["payload"].clone();
                        leptos::logging::log!("[ws] recv type={}", type_str);
                        store_for_ws.apply_ekos_event(&type_str, &payload);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    leptos::logging::error!("WS error: {:?}", e);
                    store_for_ws.connected.set(false);
                    store_for_ws.online.set(false);
                    break;
                }
            }
        }
    });

    (store, send_fn)
}

/// Repeatedly subscribe+get an INDI property on a device until `done_pred`
/// is satisfied (i.e. the expected data has landed in the store), or we
/// give up after a reasonable timeout. Needed because `processDeviceCommands`
/// in KStars silently drops commands while the INDI driver isn't registered
/// yet (`message.cpp:1664`), and drivers may take seconds to come up.
fn spawn_retry_property<T, F>(
    send: SendCmd,
    device: String,
    property: &'static str,
    signal: RwSignal<T>,
    done_pred: F,
)
where
    T: Clone + Send + Sync + 'static,
    F: Fn(&T) -> bool + 'static,
{
    use gloo_timers::future::TimeoutFuture;
    spawn_local(async move {
        leptos::logging::log!("[ws] retry_property start device={} prop={}", device, property);
        // First shot — subscribe (persistent push) + get (fast path).
        let sub = serde_json::json!({
            "type":"device_property_subscribe",
            "payload":{ "device": device, "properties":[property] }
        }).to_string();
        let get = serde_json::json!({
            "type":"device_property_get",
            "payload":{ "device": device, "property": property, "compact": true }
        }).to_string();
        send(sub.clone());
        send(get.clone());
        leptos::logging::log!("[ws] retry_property sent subscribe+get for {}", property);

        // Retry budget: 60 attempts × 1s = 1 minute.
        for i in 0..60 {
            TimeoutFuture::new(1_000).await;
            if done_pred(&signal.get_untracked()) {
                leptos::logging::log!("[ws] retry_property done for {} after {}s", property, i + 1);
                return;
            }
            send(sub.clone());
            send(get.clone());
        }
        leptos::logging::log!("[ws] retry_property giving up on {} after 60s", property);
    });
}

/// Long-lived periodic refresh: re-requests key INDI properties every few
/// seconds so the UI stays current even if KStars drops a push subscription.
/// Complements `spawn_retry_property` (which handles fast bootstrap then stops).
fn spawn_refresh_loop(send: SendCmd, store: DeviceStore) {
    use gloo_timers::future::TimeoutFuture;

    let online   = store.online;
    let trains   = store.optical_trains;

    spawn_local(async move {
        loop {
            TimeoutFuture::new(5_000).await;

            if !online.get_untracked() {
                continue;
            }
            let train_list = trains.get_untracked();
            let Some(train) = train_list.first() else { continue };

            // ── Ekos module-level state ─────────────────────────────
            send(r#"{"type":"get_scopes","payload":{}}"#.to_string());
            send(r#"{"type":"train_get_all","payload":{}}"#.to_string());
            send(r#"{"type":"capture_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"capture_get_sequences","payload":{}}"#.to_string());
            send(r#"{"type":"focus_get_all_settings","payload":{}}"#.to_string());

            // ── Camera INDI properties ───────────────────────────────
            if !train.camera.is_empty() && train.camera != "--" {
                for prop in ["CCD_INFO", "CCD_TEMPERATURE", "CCD_COOLER"] {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": train.camera, "property": prop, "compact": true }
                    }).to_string());
                }
            }

            // ── Mount INDI properties ────────────────────────────────
            if !train.mount.is_empty() && train.mount != "--" {
                send(serde_json::json!({
                    "type": "device_property_get",
                    "payload": { "device": train.mount, "property": "EQUATORIAL_EOD_COORD", "compact": true }
                }).to_string());
            }

            // ── Focuser INDI properties ──────────────────────────────
            if !train.focuser.is_empty() && train.focuser != "--" {
                for prop in ["ABS_FOCUS_POSITION", "FOCUS_TEMPERATURE"] {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": train.focuser, "property": prop, "compact": true }
                    }).to_string());
                }
            }
        }
    });
}
