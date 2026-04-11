//! Minimal WebSocket + DeviceStore for milestone 1.
//!
//! Scope: attach to the rekos-server `/ws` endpoint and decode just the
//! Ekos Live messages the planetarium needs for the FOV reticle:
//!   - `new_connection_state`    → KStars attached flag
//!   - `new_mount_state`         → mount RA/Dec + slew/track/park flags
//!   - `get_scopes`              → primary telescope focal length
//!   - `train_get_all`           → list of optical trains
//!   - `train_settings_get`      → focal length + pixel size + sensor WxH
//!
//! Any other Ekos Live message is ignored. Device tabs and their state
//! come back in later milestones.

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
}

// ---------------------------------------------------------------------------
// Stub types still referenced by sky/actions.rs via crate-level contexts.
// Kept as defaults — actions.rs reads them but milestone 1 ignores the
// contents on the wire.
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
    pub mount_status:       RwSignal<Option<MountStatusData>>,
    pub camera_status:      RwSignal<Option<CameraStatusData>>,
    pub telescope_settings: RwSignal<TelescopeSettingsData>,
    pub optical_trains:     RwSignal<Vec<OpticalTrain>>,
}

impl DeviceStore {
    fn new() -> Self {
        Self {
            connected:          RwSignal::new(false),
            mount_status:       RwSignal::new(None),
            camera_status:      RwSignal::new(None),
            telescope_settings: RwSignal::new(TelescopeSettingsData::default()),
            optical_trains:     RwSignal::new(Vec::new()),
        }
    }

    fn apply_ekos_event(&self, type_str: &str, payload: &serde_json::Value) {
        match type_str {
            "new_connection_state" => {
                let connected = payload["connected"].as_bool().unwrap_or(false);
                self.connected.set(connected);
                if !connected {
                    self.mount_status.set(None);
                    self.camera_status.set(None);
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
                    // KStars sends RA in degrees (ra) and Dec in degrees (de).
                    if let Some(ra_deg) = payload["ra"].as_f64() { ms.ra_h = Some(ra_deg / 15.0); }
                    if let Some(dec)    = payload["de"].as_f64() { ms.dec_deg = Some(dec); }
                });
            }

            "get_scopes" => {
                // sendScopes(): array of { id, model, vendor, type, aperture, focal_length }.
                // First entry = primary scope.
                if let Some(arr) = payload.as_array() {
                    if let Some(s) = arr.first() {
                        let fl = s["focal_length"].as_f64();
                        let ap = s["aperture"].as_f64();
                        self.telescope_settings.update(|t| {
                            if fl.is_some() { t.focal_length_mm = fl; }
                            if ap.is_some() { t.aperture_mm = ap; }
                        });
                    }
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
                    }).collect();
                    self.optical_trains.set(trains);
                }
            }

            "train_settings_get" => {
                // Settings is a flat key/value map. We pull focal length + pixel
                // size + sensor dimensions and feed them into telescope_settings +
                // camera_status so the planetarium FOV reticle uses real values.
                let fl = payload["focalLength"].as_f64()
                    .or_else(|| payload["focal_length"].as_f64());
                let ap = payload["aperture"].as_f64();
                if fl.is_some() || ap.is_some() {
                    self.telescope_settings.update(|t| {
                        if fl.is_some() { t.focal_length_mm = fl; }
                        if ap.is_some() { t.aperture_mm = ap; }
                    });
                }
                let pix = payload["pixelSize"].as_f64()
                    .or_else(|| payload["pixel_size"].as_f64());
                let sw  = payload["width"].as_u64().map(|v| v as u32)
                    .or_else(|| payload["sensorWidth"].as_u64().map(|v| v as u32));
                let sh  = payload["height"].as_u64().map(|v| v as u32)
                    .or_else(|| payload["sensorHeight"].as_u64().map(|v| v as u32));
                if pix.is_some() || sw.is_some() || sh.is_some() {
                    self.camera_status.update(|opt| {
                        let cs = opt.get_or_insert_with(CameraStatusData::default);
                        if pix.is_some() { cs.pixel_size_um = pix; }
                        if sw.is_some()  { cs.sensor_width  = sw; }
                        if sh.is_some()  { cs.sensor_height = sh; }
                    });
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

    // Prime sequence: once KStars reports connected=true we ask for the data
    // that populates FOV inputs. These requests are silently dropped if Ekos
    // hasn't started a profile yet, so re-fire on every connect.
    let connected_sig = store.connected;
    let prime_send = send_fn.clone();
    Effect::new(move |_| {
        if connected_sig.get() {
            prime_send(r#"{"type":"train_get_all","payload":{}}"#.to_string());
            prime_send(r#"{"type":"train_settings_get","payload":{}}"#.to_string());
            prime_send(r#"{"type":"get_scopes","payload":{}}"#.to_string());
        }
    });

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
                        store_for_ws.apply_ekos_event(&type_str, &payload);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    leptos::logging::error!("WS error: {:?}", e);
                    store_for_ws.connected.set(false);
                    break;
                }
            }
        }
    });

    (store, send_fn)
}
