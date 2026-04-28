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

use crate::ws_helpers::extract_indi_number;

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
    pub ha_deg: Option<f64>,
    /// INDI pier side (kstars/indi/indimount.h:39).
    /// -1 = PIER_UNKNOWN, 0 = PIER_WEST, 1 = PIER_EAST.
    pub pier_side: Option<i32>,
    pub az_deg:               Option<f64>,
    pub alt_deg:              Option<f64>,
    pub ra0_h:                Option<f64>,
    pub dec0_deg:             Option<f64>,
    pub slew_rate:            Option<i32>,
    pub target:               String,
    pub status_str:           String,
    pub meridian_flip_status: String,
    pub auto_park_countdown:  String,
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
    pub seq_remaining_time: String,
    pub overall_remaining_time: String,
    pub exposure_left: Option<f64>,
    pub exposure_total: Option<f64>,
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

/// Equipment profile mirrored from KStars `get_profiles` responses.
///
/// Wire shape: `kstars/profileinfo.cpp::toJson` (~line 135). We model the
/// scalar + driver-label fields the UI shows; the legacy `drivers`
/// object-of-arrays is ignored on parse — typed slots cover the same data.
#[derive(Debug, Clone, Default)]
pub struct ProfileInfo {
    pub name:                String,
    pub auto_connect:        bool,
    pub port_selector:       bool,
    pub mode:                String,    // "local" | "remote"
    pub remote_host:         String,
    pub remote_port:         u16,
    pub guiding:             i32,       // 0=Internal 1=PHD2 2=LinGuider 3=SEP
    pub remote_guiding_host: String,
    pub remote_guiding_port: u16,
    pub use_web_manager:     bool,
    pub mount:               String,
    pub ccd:                 String,
    pub guider:              String,
    pub focuser:             String,
    pub filter:              String,
    pub ao:                  String,
    pub dome:                String,
    pub weather:             String,
    pub aux1:                String,
    pub aux2:                String,
    pub aux3:                String,
    pub aux4:                String,
    pub remote:              String,    // CSV of remote drivers
    pub driver_source:       String,
}

impl ProfileInfo {
    fn from_json(v: &serde_json::Value) -> Self {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let b = |k: &str| v[k].as_bool().unwrap_or(false);
        let u = |k: &str| v[k].as_u64().unwrap_or(0) as u16;
        let i = |k: &str| v[k].as_i64().unwrap_or(0) as i32;
        let mode = v["mode"].as_str().unwrap_or("local").to_string();
        Self {
            name:                s("name"),
            auto_connect:        b("auto_connect"),
            port_selector:       b("port_selector"),
            mode:                if mode.is_empty() { "local".into() } else { mode },
            remote_host:         s("remote_host"),
            remote_port:         u("remote_port"),
            guiding:             i("guiding"),
            remote_guiding_host: s("remote_guiding_host"),
            remote_guiding_port: u("remote_guiding_port"),
            use_web_manager:     b("use_web_manager"),
            mount:               s("mount"),
            ccd:                 s("ccd"),
            guider:              s("guider"),
            focuser:             s("focuser"),
            filter:              s("filter"),
            ao:                  s("ao"),
            dome:                s("dome"),
            weather:             s("weather"),
            aux1:                s("aux1"),
            aux2:                s("aux2"),
            aux3:                s("aux3"),
            aux4:                s("aux4"),
            remote:              s("remote"),
            driver_source:       s("driver_source"),
        }
    }

    /// Serialize for `profile_add` / `profile_update`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "name":                self.name,
            "auto_connect":        self.auto_connect,
            "port_selector":       self.port_selector,
            "mode":                self.mode,
            "remote_host":         self.remote_host,
            "remote_port":         self.remote_port,
            "guiding":             self.guiding,
            "remote_guiding_host": self.remote_guiding_host,
            "remote_guiding_port": self.remote_guiding_port,
            "use_web_manager":     self.use_web_manager,
            "mount":               self.mount,
            "ccd":                 self.ccd,
            "guider":              self.guider,
            "focuser":             self.focuser,
            "filter":              self.filter,
            "ao":                  self.ao,
            "dome":                self.dome,
            "weather":             self.weather,
            "aux1":                self.aux1,
            "aux2":                self.aux2,
            "aux3":                self.aux3,
            "aux4":                self.aux4,
            "remote":              self.remote,
            "driver_source":       if self.driver_source.is_empty() { "system".to_string() } else { self.driver_source.clone() },
        })
    }
}

// Polar alignment state (PAA). See kstars/ekos/align/polaralignmentassistant.*
// and the `new_polar_state` arms in message.cpp:1157-1263.
#[derive(Debug, Clone, Default)]
pub struct PolarVectorData {
    pub center_x:  f64,
    pub center_y:  f64,
    pub mag:       f64,
    pub pa:        f64,
    pub error:     f64,  // total polar error, degrees
    pub az_error:  f64,  // degrees
    pub alt_error: f64,  // degrees
}

#[derive(Debug, Clone, Default)]
pub struct PolarStateData {
    pub enabled:           bool,
    pub stage:             String,
    pub message:           String,
    pub vector:            Option<PolarVectorData>,
    pub updated_error:     Option<f64>,  // -1 on solver failure
    pub updated_az_error:  Option<f64>,
    pub updated_alt_error: Option<f64>,
}

// Scheduler module state. Two push shapes from manager.cpp:
//   {:417} {log: string}
//   {:427} {status: int}  — SchedulerState enum: 0=IDLE 1=RUNNING 2=PAUSED
#[derive(Debug, Clone, Default)]
pub struct SchedulerStatusData {
    pub status: i64,
    pub log: String,
}

// Guide module state. `new_guide_state` carries a *partial* payload per
// emission: either {status}, {drift_ra, drift_de}, {rarms, derms}, or
// {log}. See kstars/ekos/manager.cpp:2769-2786 for the four distinct
// senders. We merge them all into one struct.
#[derive(Debug, Clone)]
pub struct GuideStateSample {
    pub t_ms:   f64,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct GuideDriftSample {
    pub t_ms: f64,
    pub ra:   f64,  // arcsec drift on RA axis
    pub de:   f64,  // arcsec drift on DEC axis
}

#[derive(Debug, Clone, Default)]
pub struct GuideStatusData {
    pub connected: bool,
    pub status:    String,
    pub history:   Vec<GuideStateSample>,
    pub drift:     Vec<GuideDriftSample>,
    pub ra_rms:    Option<f64>,
    pub de_rms:    Option<f64>,
    pub log:       String,
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

/// Latest plate-solve result. Populated from the `solution` payload of
/// `new_align_state` (kstars/ekos/align/align.cpp:2351). All RA/Dec values are
/// JNow — KStars uses `m_AlignCoord` which is epoch-of-date.
#[derive(Debug, Clone, Default)]
pub struct AlignSolutionData {
    pub ra_jnow_deg:      Option<f64>,
    pub dec_jnow_deg:     Option<f64>,
    pub orientation_deg:  Option<f64>, // PA from align.cpp:2364
    pub pixscale_arcsec:  Option<f64>, // pix from align.cpp:2363
    pub solved_at_ms:     Option<f64>, // js_sys::Date::now() at receipt
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
    /// Server's $HOME, injected by rekos-server/proxy.rs on connect.
    /// Used to predict .esq file paths written via scheduler_save_sequence_file.
    pub home_dir:           RwSignal<String>,
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
    pub polar_state:        RwSignal<PolarStateData>,
    pub align_settings:     RwSignal<serde_json::Value>,
    pub align_solution:     RwSignal<AlignSolutionData>,
    pub align_preview_url:  RwSignal<Option<String>>,
    pub guide_status:       RwSignal<Option<GuideStatusData>>,
    pub guide_settings:     RwSignal<serde_json::Value>,
    /// Flattened `{name: value, ...}` map of global KStars `Options::`
    /// entries we care about (GuiderType, PHD2Host/Port, LinGuiderHost/Port).
    pub guide_options:      RwSignal<serde_json::Value>,
    pub guide_preview_url:  RwSignal<Option<String>>,
    pub scheduler_status:   RwSignal<SchedulerStatusData>,
    pub scheduler_settings: RwSignal<serde_json::Value>,
    pub scheduler_jobs:     RwSignal<Vec<serde_json::Value>>,
    pub mosaic_tiles:       RwSignal<Option<serde_json::Value>>,
    pub livestacker_state:    RwSignal<Option<LiveStackerState>>,
    pub livestacker_settings: RwSignal<serde_json::Value>,
    /// Equipment profile list, populated by `get_profiles` responses.
    /// Available before `online == true` — profile CRUD is dispatched
    /// before the Ekos-startup gate (message.cpp:249).
    pub profiles:             RwSignal<Vec<ProfileInfo>>,
    /// Name of the currently-selected profile in KStars (`selectedProfile`).
    pub selected_profile:     RwSignal<Option<String>>,
}

/// Aggregated state from `new_livestacker_state`. KStars sends two flavours:
/// state-only updates (`{state:"..."}`) and stacking updates with stats. We
/// merge both into this struct, preserving prior numeric fields when only the
/// state changes.
#[derive(Debug, Clone, Default)]
pub struct LiveStackerState {
    pub state:          String,
    pub ok:             bool,
    pub frames_stacked: u32,
    pub total_frames:   u32,
    pub mean_snr:       f64,
    pub min_snr:        f64,
    pub max_snr:        f64,
    pub message:        Option<String>,
}

impl DeviceStore {
    fn new() -> Self {
        Self {
            connected:          RwSignal::new(false),
            online:             RwSignal::new(false),
            home_dir:           RwSignal::new(String::new()),
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
            polar_state:        RwSignal::new(PolarStateData::default()),
            align_settings:     RwSignal::new(serde_json::Value::Null),
            align_solution:     RwSignal::new(AlignSolutionData::default()),
            align_preview_url:  RwSignal::new(None),
            guide_status:       RwSignal::new(None),
            guide_settings:     RwSignal::new(serde_json::Value::Null),
            guide_options:      RwSignal::new(serde_json::Value::Null),
            guide_preview_url:  RwSignal::new(None),
            scheduler_status:   RwSignal::new(SchedulerStatusData::default()),
            scheduler_settings: RwSignal::new(serde_json::Value::Null),
            scheduler_jobs:     RwSignal::new(Vec::new()),
            mosaic_tiles:       RwSignal::new(None),
            livestacker_state:    RwSignal::new(None),
            livestacker_settings: RwSignal::new(serde_json::Value::Null),
            profiles:             RwSignal::new(Vec::new()),
            selected_profile:     RwSignal::new(None),
        }
    }

    fn apply_ekos_event(&self, type_str: &str, payload: &serde_json::Value) {
        match type_str {
            "file_default_path" => {
                if let Some(s) = payload.as_str() {
                    if !s.is_empty() { self.home_dir.set(s.to_string()); }
                }
            }

            "new_connection_state" => {
                let connected = payload["connected"].as_bool().unwrap_or(false);
                let online = payload["online"].as_bool().unwrap_or(false);
                self.connected.set(connected);
                self.online.set(connected && online);
                if let Some(h) = payload["home_dir"].as_str() {
                    if !h.is_empty() { self.home_dir.set(h.to_string()); }
                }
                if !connected {
                    self.mount_status.set(None);
                    self.camera_status.set(None);
                    self.focus_status.set(None);
                    self.focus_preview_url.set(None);
                    self.focus_hfr_history.set(Vec::new());
                    self.polar_state.set(PolarStateData::default());
                    self.align_settings.set(serde_json::Value::Null);
                    self.align_solution.set(AlignSolutionData::default());
                    self.align_preview_url.set(None);
                    self.guide_status.set(None);
                    self.guide_preview_url.set(None);
                    // guide_settings / guide_options left intact so the
                    // Guide tab doesn't flicker between blank and populated
                    // on transient disconnects.
                    self.scheduler_status.set(SchedulerStatusData::default());
                    self.scheduler_jobs.set(Vec::new());
                    self.mosaic_tiles.set(None);
                    self.livestacker_state.set(None);
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
                    // HA in degrees (manager.cpp:3189). Sent with the coord
                    // payload and throttled to 1 s (message.cpp:2552).
                    if let Some(ha) = payload["ha"].as_f64() { ms.ha_deg = Some(ha); }
                    // Pier side: -1/0/1 per kstars/indi/indimount.h:39.
                    // Emitted standalone on pierSideChanged (manager.cpp:2698).
                    if let Some(p) = payload["pierSide"].as_i64() {
                        ms.pier_side = Some(p as i32);
                    }
                    // Az/Alt and J2000 coords come with the throttled coord payload.
                    if let Some(v) = payload["az"].as_f64()  { ms.az_deg   = Some(v); }
                    if let Some(v) = payload["at"].as_f64()  { ms.alt_deg  = Some(v); }
                    if let Some(v) = payload["ra0"].as_f64() { ms.ra0_h    = Some(v / 15.0); }
                    if let Some(v) = payload["de0"].as_f64() { ms.dec0_deg = Some(v); }
                    // Slew rate index, target name, and info banners.
                    if let Some(v) = payload["slewRate"].as_i64() { ms.slew_rate = Some(v as i32); }
                    if let Some(v) = payload["target"].as_str() { if !v.is_empty() { ms.target = v.to_string(); } }
                    if let Some(v) = payload["meridianFlipStatus"].as_str() { ms.meridian_flip_status = v.to_string(); }
                    if let Some(v) = payload["autoParkCountdown"].as_str()  { ms.auto_park_countdown  = v.to_string(); }
                    if let Some(s) = payload["status"].as_str() { ms.status_str = s.to_string(); }
                });
            }

            "get_profiles" => {
                // Equipment profile list. Wire shape from message.cpp:1322-1339:
                //   { selectedProfile: "<name>", profiles: [ProfileInfo.toJson, …] }
                if let Some(arr) = payload["profiles"].as_array() {
                    let list: Vec<ProfileInfo> = arr.iter().map(ProfileInfo::from_json).collect();
                    self.profiles.set(list);
                }
                if let Some(s) = payload["selectedProfile"].as_str() {
                    self.selected_profile.set(if s.is_empty() { None } else { Some(s.to_string()) });
                }
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
                    let max_x  = extract_indi_number(payload, "CCD_MAX_X");
                    let max_y  = extract_indi_number(payload, "CCD_MAX_Y");
                    let pix_x  = extract_indi_number(payload, "CCD_PIXEL_SIZE_X");
                    let pix_y  = extract_indi_number(payload, "CCD_PIXEL_SIZE_Y");
                    let pix_any = extract_indi_number(payload, "CCD_PIXEL_SIZE");
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
                    let pos = extract_indi_number(payload, "FOCUS_ABSOLUTE_POSITION")
                        .map(|v| v as i64);
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
                    let temp = extract_indi_number(payload, "TEMPERATURE");
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
                    let t = extract_indi_number(payload, "CCD_TEMPERATURE_VALUE");
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
                    let ra_h = extract_indi_number(payload, "RA");
                    let de_d = extract_indi_number(payload, "DEC");
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
                    if let Some(v) = payload["seqr"].as_i64() { c.seq_total = Some(v); }
                    if let Some(v) = payload["seqv"].as_i64() { c.seq_current = Some(v); }
                    if let Some(v) = payload["ovp"].as_f64() { c.progress = Some(v); }
                    if let Some(s) = payload["seqt"].as_str() {
                        c.seq_remaining_time = s.to_string();
                    }
                    if let Some(s) = payload["ovt"].as_str() {
                        c.overall_remaining_time = s.to_string();
                    }
                    if let Some(v) = payload["expv"].as_f64() { c.exposure_left = Some(v); }
                    if let Some(v) = payload["expr"].as_f64() { c.exposure_total = Some(v); }
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

            // Polar alignment state (PAA). Partial payloads: any subset of
            // {stage, message, enabled, vector, updatedError*}. See
            // kstars/ekos/ekoslive/message.cpp:1157-1263.
            "new_polar_state" => {
                self.polar_state.update(|p| {
                    if let Some(s) = payload["stage"].as_str()    { p.stage   = s.to_string(); }
                    if let Some(m) = payload["message"].as_str()  { p.message = m.to_string(); }
                    if let Some(e) = payload["enabled"].as_bool() { p.enabled = e; }
                    if let Some(obj) = payload.get("vector").and_then(|v| v.as_object()) {
                        p.vector = Some(PolarVectorData {
                            center_x:  obj.get("center_x").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            center_y:  obj.get("center_y").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            mag:       obj.get("mag").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            pa:        obj.get("pa").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            error:     obj.get("error").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            az_error:  obj.get("azError").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            alt_error: obj.get("altError").and_then(|x| x.as_f64()).unwrap_or(0.0),
                        });
                    }
                    if let Some(v) = payload.get("updatedError").and_then(|x| x.as_f64())    { p.updated_error = Some(v); }
                    if let Some(v) = payload.get("updatedAZError").and_then(|x| x.as_f64())  { p.updated_az_error = Some(v); }
                    if let Some(v) = payload.get("updatedALTError").and_then(|x| x.as_f64()) { p.updated_alt_error = Some(v); }
                });
            }

            // Align module settings snapshot (reply to align_get_all_settings
            // and echoed after align_set_all_settings). See message.cpp:576-580.
            // Payload is the settings map directly.
            "align_get_all_settings" => {
                self.align_settings.set(payload.clone());
            }

            // Align state. KStars emits this from two places:
            //   - setAlignStatus  → {status}
            //   - setAlignSolution → {solution: {ra.Hours, de.Degrees, PA, pix, fov, ...}}
            // (see message.cpp:927, 943; align.cpp:2351 for the solution map).
            // RA/Dec in the solution are JNow.
            "new_align_state" => {
                if let Some(sol) = payload.get("solution").and_then(|v| v.as_object()) {
                    leptos::logging::log!("[ws] new_align_state solution: {}",
                        serde_json::to_string(sol).unwrap_or_default());
                    let ra_h = sol.get("ra.Hours").and_then(|x| x.as_f64());
                    let de_d = sol.get("de.Degrees").and_then(|x| x.as_f64());
                    let pa   = sol.get("PA").and_then(|x| x.as_f64());
                    let pix  = sol.get("pix").and_then(|x| x.as_f64());
                    if ra_h.is_some() || de_d.is_some() || pa.is_some() || pix.is_some() {
                        self.align_solution.update(|a| {
                            if let Some(v) = ra_h { a.ra_jnow_deg = Some(v * 15.0); }
                            if let Some(v) = de_d { a.dec_jnow_deg = Some(v); }
                            if let Some(v) = pa   { a.orientation_deg = Some(v); }
                            if let Some(v) = pix  { a.pixscale_arcsec = Some(v); }
                            a.solved_at_ms = Some(web_sys::js_sys::Date::now());
                        });
                    }
                }
            }

            // Guide module status. KStars emits `new_guide_state` from
            // multiple sites (see manager.cpp:2769-2786) with **partial
            // payloads**. Only `setStatus`/`updateGuideStatus` include a
            // `status` field; the other emissions carry just {rarms,derms},
            // {drift_ra,drift_de}, or {log}. So we must treat every field
            // as optional and never reset state from a missing field.
            "new_guide_state" => {
                self.guide_status.update(|opt| {
                    let gs = opt.get_or_insert_with(GuideStatusData::default);
                    gs.connected = true;

                    if let Some(status) = payload["status"].as_str() {
                        if gs.status != status {
                            let t_ms = web_sys::js_sys::Date::now();
                            gs.history.push(GuideStateSample {
                                t_ms,
                                status: status.to_string(),
                            });
                            if gs.history.len() > 256 {
                                let drop = gs.history.len() - 256;
                                gs.history.drain(..drop);
                            }
                            gs.status = status.to_string();
                        }
                    }

                    // Drift + RMS samples — pushed per-frame from
                    // manager.cpp:2664 (updateSigmas) and the
                    // newAxisDelta lambda at :2772-2776.
                    let drift_ra = payload["drift_ra"].as_f64();
                    let drift_de = payload["drift_de"].as_f64();
                    if drift_ra.is_some() || drift_de.is_some() {
                        let t_ms = web_sys::js_sys::Date::now();
                        gs.drift.push(GuideDriftSample {
                            t_ms,
                            ra: drift_ra.unwrap_or(f64::NAN),
                            de: drift_de.unwrap_or(f64::NAN),
                        });
                        if gs.drift.len() > 600 {
                            let drop = gs.drift.len() - 600;
                            gs.drift.drain(..drop);
                        }
                    }
                    if let Some(v) = payload["rarms"].as_f64() { gs.ra_rms = Some(v); }
                    if let Some(v) = payload["derms"].as_f64() { gs.de_rms = Some(v); }

                    if let Some(l) = payload["log"].as_str() {
                        if !l.is_empty() { gs.log = l.to_string(); }
                    }
                });
            }

            // Guide settings snapshot — debounced reply to
            // guide_get_all_settings (message.cpp:585-590). Flat widget map.
            "guide_get_all_settings" => {
                self.guide_settings.set(payload.clone());
            }

            // Reply to option_get — array of {name, value}. Walk it and
            // cache any guide-relevant keys. Ignore other keys so we don't
            // stomp on future consumers that request their own options.
            "option_get" => {
                const GUIDE_KEYS: &[&str] = &[
                    "GuiderType",
                    "PHD2Host",
                    "PHD2Port",
                    "LinGuiderHost",
                    "LinGuiderPort",
                ];
                if let Some(arr) = payload.as_array() {
                    self.guide_options.update(|opt| {
                        let map = match opt {
                            serde_json::Value::Object(m) => m,
                            _ => {
                                *opt = serde_json::Value::Object(serde_json::Map::new());
                                opt.as_object_mut().unwrap()
                            }
                        };
                        for el in arr {
                            let Some(name) = el["name"].as_str() else { continue };
                            if !GUIDE_KEYS.contains(&name) { continue; }
                            if let Some(v) = el.get("value") {
                                map.insert(name.to_string(), v.clone());
                            }
                        }
                    });
                }
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
                    // Frame uuid prefixes come from kstars/ekos/ekoslive/media.cpp:
                    //   "+F" focus (line 752)
                    //   "+A" align / polar align (line 587, 640 — sendUpdatedFrame)
                    //   "+G" guide (line 753)
                    // Everything else → capture preview target.
                    if uuid.starts_with("+F") {
                        self.focus_preview_url.set(Some(url));
                    } else if uuid.starts_with("+A") {
                        self.align_preview_url.set(Some(url));
                    } else if uuid.starts_with("+G") {
                        self.guide_preview_url.set(Some(url));
                    } else {
                        self.capture_preview_url.set(Some(url));
                    }
                }
            }

            // Scheduler status push: {status: int} or {log: string}.
            // Both emitted from manager.cpp:417 and :427 via sendSchedulerStatus.
            "new_scheduler_state" => {
                self.scheduler_status.update(|s| {
                    if let Some(v) = payload["status"].as_i64() { s.status = v; }
                    if let Some(l) = payload["log"].as_str() {
                        if !l.is_empty() { s.log = l.to_string(); }
                    }
                });
            }

            // Reply to scheduler_get_jobs — {"jobs": [...]}
            "scheduler_get_jobs" => {
                if let Some(arr) = payload["jobs"].as_array() {
                    self.scheduler_jobs.set(arr.clone());
                }
            }

            // Debounced settings reply (message.cpp:623).
            "scheduler_get_all_settings" => {
                self.scheduler_settings.set(payload.clone());
            }

            // Mosaic tile grid pushed by KStars Framing Assistant.
            "new_mosaic_tiles" => {
                self.mosaic_tiles.set(Some(payload.clone()));
            }

            // LiveStacker push: either {state:"..."} or {state:"stacking", ok,
            // frames_stacked, total_frames, mean_snr, min_snr, max_snr}.
            // Merge — preserve previous numeric stats when only state changes.
            "new_livestacker_state" => {
                self.livestacker_state.update(|opt| {
                    let s = opt.get_or_insert_with(LiveStackerState::default);
                    if let Some(v) = payload["state"].as_str() { s.state = v.to_string(); }
                    if let Some(v) = payload["ok"].as_bool()    { s.ok = v; }
                    if let Some(v) = payload["frames_stacked"].as_u64() { s.frames_stacked = v as u32; }
                    if let Some(v) = payload["total_frames"].as_u64()   { s.total_frames   = v as u32; }
                    if let Some(v) = payload["mean_snr"].as_f64() { s.mean_snr = v; }
                    if let Some(v) = payload["min_snr"].as_f64()  { s.min_snr  = v; }
                    if let Some(v) = payload["max_snr"].as_f64()  { s.max_snr  = v; }
                    if let Some(v) = payload["message"].as_str()  { s.message  = Some(v.to_string()); }
                });
            }

            "livestacker_get_all_settings" => {
                self.livestacker_settings.set(payload.clone());
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
                prime_send(r#"{"type":"file_default_path","payload":{"type":8}}"#.to_string());
                prime_send(r#"{"type":"get_devices","payload":{}}"#.to_string());
                prime_send(r#"{"type":"get_states","payload":{}}"#.to_string());
                prime_send(r#"{"type":"get_scopes","payload":{}}"#.to_string());
                prime_send(r#"{"type":"train_get_all","payload":{}}"#.to_string());
                prime_send(r#"{"type":"focus_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"capture_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"capture_get_sequences","payload":{}}"#.to_string());
                prime_send(r#"{"type":"align_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"guide_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"scheduler_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"scheduler_get_jobs","payload":{}}"#.to_string());
                prime_send(r#"{"type":"livestacker_get_all_settings","payload":{}}"#.to_string());
                // Guider-backend settings live in global KStars Options::,
                // not in guide_get_all_settings. See message.cpp:1418.
                prime_send(r#"{"type":"option_get","payload":{"options":[{"name":"GuiderType"},{"name":"PHD2Host"},{"name":"PHD2Port"},{"name":"LinGuiderHost"},{"name":"LinGuiderPort"}]}}"#.to_string());
            }
        });
    }

    // Profile list: not gated on `online`. KStars dispatches profile_*
    // commands before the Ekos-startup gate (message.cpp:249), so the
    // browser can list/CRUD/start profiles while Ekos is offline.
    // KStars also pushes `get_profiles` unsolicited on connect
    // (message.cpp:93), but if the browser opens after KStars is already
    // up that initial push is missed — re-fetch on every WS connect.
    let connected_sig = store.connected;
    {
        let prime_send = send_fn.clone();
        Effect::new(move |_| {
            if connected_sig.get() {
                prime_send(r#"{"type":"get_profiles","payload":{}}"#.to_string());
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
            send(r#"{"type":"get_devices","payload":{}}"#.to_string());
            send(r#"{"type":"get_states","payload":{}}"#.to_string());
            send(r#"{"type":"get_scopes","payload":{}}"#.to_string());
            send(r#"{"type":"train_get_all","payload":{}}"#.to_string());
            send(r#"{"type":"capture_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"capture_get_sequences","payload":{}}"#.to_string());
            send(r#"{"type":"focus_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"align_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"guide_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"scheduler_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"scheduler_get_jobs","payload":{}}"#.to_string());
            send(r#"{"type":"option_get","payload":{"options":[{"name":"GuiderType"},{"name":"PHD2Host"},{"name":"PHD2Port"},{"name":"LinGuiderHost"},{"name":"LinGuiderPort"}]}}"#.to_string());

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
