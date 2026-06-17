//! Pure data types consumed by `compat.rs` / sky / tab components.
//!
//! No Leptos signals, no logic beyond `ProfileInfo::{from_json,to_json}`.

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
    /// `executeMeridianFlip` from `mount_get_all_settings`.
    pub meridian_flip_enabled:    Option<bool>,
    /// `meridianFlipOffsetDegrees` — hour-angle offset past meridian, in degrees.
    pub meridian_flip_offset_deg: Option<f64>,
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
    // Combo option lists. Sourced from INDI switch labels — fetched with
    // compact:false because compact mode strips labels.
    pub capture_format_options:  Vec<String>,  // CCD_CAPTURE_FORMAT
    pub transfer_format_options: Vec<String>,  // CCD_TRANSFER_FORMAT
    pub iso_options:             Vec<String>,  // CCD_ISO (DSLR only)
    pub frame_type_options:      Vec<String>,  // CCD_FRAME_TYPE (Light/Dark/Bias/Flat)
}

#[derive(Debug, Clone, Default)]
pub struct FilterWheelStatusData {
    pub device:        String,
    /// Filter labels from INDI `FILTER_NAME` text property — one per slot.
    pub filter_names:  Vec<String>,
    /// 1-based current slot index from `FILTER_SLOT` number property.
    pub current_slot:  Option<i32>,
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

#[derive(Debug, Clone)]
pub struct OpticalTrain {
    pub id: i64,
    pub name: String,
    pub mount: String,
    pub camera: String,
    pub scope: String,
    pub guider: String,
    pub focuser: String,
    pub filterwheel: String,
    pub rotator: String,
    pub dustcap: String,
    pub lightbox: String,
    /// Per-train focal reducer ratio (KStars `OpticalTrainManager` default 1.0).
    /// KStars' framing assistant multiplies focal length by this when computing
    /// the camera FOV (`framingassistantui.cpp:422`); we must do the same so
    /// the planetarium FOV preview matches the actual mosaic layout.
    pub reducer: f64,
}

impl Default for OpticalTrain {
    fn default() -> Self {
        Self {
            id: 0, name: String::new(), mount: String::new(), camera: String::new(),
            scope: String::new(), guider: String::new(), focuser: String::new(),
            filterwheel: String::new(), rotator: String::new(), dustcap: String::new(),
            lightbox: String::new(), reducer: 1.0,
        }
    }
}

impl OpticalTrain {
    pub(super) fn from_json(t: &serde_json::Value) -> Self {
        let s = |k: &str| t[k].as_str().unwrap_or("").to_string();
        Self {
            id: t["id"].as_i64().unwrap_or(0),
            name: s("name"),
            mount: s("mount"),
            camera: s("camera"),
            scope: s("scope"),
            guider: s("guider"),
            focuser: s("focuser"),
            filterwheel: s("filterwheel"),
            rotator: s("rotator"),
            dustcap: s("dustcap"),
            lightbox: s("lightbox"),
            // KStars stores reducer as a number; older trains may miss the
            // field entirely → default to 1.0 (no reducer).
            reducer: t["reducer"].as_f64().filter(|v| *v > 0.0).unwrap_or(1.0),
        }
    }

    /// Serialize for `train_add` (`include_id = false`, KStars auto-assigns id
    /// and stamps the active profile) / `train_update` (`include_id = true`).
    /// Empty device slots are sent as `"--"`, matching KStars' reset default.
    pub fn to_json(&self, include_id: bool) -> serde_json::Value {
        let dev = |v: &str| {
            let v = v.trim();
            if v.is_empty() { "--".to_string() } else { v.to_string() }
        };
        let mut o = serde_json::json!({
            "name":        self.name,
            "mount":       dev(&self.mount),
            "camera":      dev(&self.camera),
            "scope":       self.scope,
            "guider":      dev(&self.guider),
            "focuser":     dev(&self.focuser),
            "filterwheel": dev(&self.filterwheel),
            "rotator":     dev(&self.rotator),
            "dustcap":     dev(&self.dustcap),
            "lightbox":    dev(&self.lightbox),
            "reducer":     self.reducer,
        });
        if include_id {
            o["id"] = serde_json::json!(self.id);
        }
        o
    }
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
    /// OAL DB row id (string). Empty for a not-yet-saved scope. Required for
    /// `scope_update` / `scope_delete`.
    pub id: String,
    pub model: String,
    pub vendor: String,
    /// "Refractor", "Reflector", … (OAL `type`).
    pub type_: String,
    /// Derived display name `"<vendor> <model> <fl>@F/<ratio>"`, computed
    /// server-side; this is what a train's `scope` field matches against.
    pub name: String,
    pub focal_length_mm: f64,
    pub aperture_mm: f64,
}

impl ScopeInfo {
    pub(super) fn from_json(s: &serde_json::Value) -> Self {
        Self {
            id: s["id"].as_str().unwrap_or("").to_string(),
            model: s["model"].as_str().unwrap_or("").to_string(),
            vendor: s["vendor"].as_str().unwrap_or("").to_string(),
            type_: s["type"].as_str().unwrap_or("").to_string(),
            name: s["name"].as_str().unwrap_or("").to_string(),
            focal_length_mm: s["focal_length"].as_f64().unwrap_or(0.0),
            aperture_mm: s["aperture"].as_f64().unwrap_or(0.0),
        }
    }
}

/// Connected INDI device, from `get_devices` (message.cpp:342). `interface`
/// is the libindi driver-interface bitfield (basedevice.h) — used to filter
/// devices by role when building optical-train slots.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub name: String,
    pub connected: bool,
    pub interface: i64,
}

impl DeviceInfo {
    pub(super) fn from_json(v: &serde_json::Value) -> Self {
        Self {
            name: v["name"].as_str().unwrap_or("").to_string(),
            connected: v["connected"].as_bool().unwrap_or(false),
            interface: v["interface"].as_i64().unwrap_or(0),
        }
    }
}

/// INDI driver descriptor mirrored from KStars `get_drivers` responses.
///
/// Wire shape: `kstars/indi/driverinfo.h::toJson` (~line 57). Profile slots
/// reference drivers by `label`, not `name` — see `profileeditor.cpp:531`.
#[derive(Debug, Clone, Default)]
pub struct DriverInfo {
    pub name:   String, // executable, e.g. indi_eqmod_telescope
    pub label:  String, // human, e.g. "EQMod Mount"
    pub family: String, // "Telescopes", "CCDs", ...
}

impl DriverInfo {
    pub(super) fn from_json(v: &serde_json::Value) -> Self {
        Self {
            name:   v["name"].as_str().unwrap_or("").to_string(),
            label:  v["label"].as_str().unwrap_or("").to_string(),
            family: v["family"].as_str().unwrap_or("").to_string(),
        }
    }
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
    pub(super) fn from_json(v: &serde_json::Value) -> Self {
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

// Dust cap + flat panel state. KStars declares `new_cap_state` in
// commands.h:32 and a `updateCapStatus` helper at message.cpp:2597, but
// nothing calls it as of the current snapshot — so we rely on the
// underlying INDI properties (`CAP_PARK` switch on dust caps,
// `FLAT_LIGHT_CONTROL` switch + `FLAT_LIGHT_INTENSITY` number on
// lightbox-capable drivers). Many drivers (e.g. FlipFlat) advertise both
// the DUSTCAP_INTERFACE and LIGHTBOX_INTERFACE bits on the same device.
//
// Park state derives from the active element in `CAP_PARK`:
//   PARK on, UNPARK off  → Parked (closed)
//   PARK off, UNPARK on  → Unparked (open)
//   `state == "Busy"`    → Moving
//   anything else        → Unknown
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DustCapParkState {
    #[default]
    Unknown,
    Parked,
    Unparked,
    Moving,
}

#[derive(Debug, Clone, Default)]
pub struct DustCapStatusData {
    /// INDI device name (e.g. "Flip Flat"). Empty until `get_devices` lands.
    pub device:           String,
    pub connected:        bool,
    /// True when the device advertises the LIGHTBOX_INTERFACE bit (1 << 10).
    pub has_light_panel:  bool,
    pub park_state:       DustCapParkState,
    /// `FLAT_LIGHT_ON` switch state — None when the device has no light box.
    pub light_on:         Option<bool>,
    /// Current `FLAT_LIGHT_INTENSITY_VALUE` from the INDI number property.
    pub brightness:       Option<f64>,
    /// `min` / `max` reported by the FLAT_LIGHT_INTENSITY number property
    /// — drivers vary (0..255 typical, but some report 0..65535).
    pub brightness_min:   Option<f64>,
    pub brightness_max:   Option<f64>,
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
//   {:427} {status: int}  — Ekos::SchedulerState (kstars/ekos/ekos.h:185):
//     0=IDLE 1=STARTUP 2=RUNNING 3=PAUSED 4=SHUTDOWN 5=ABORTED 6=LOADING
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
    pub device:    String,
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
    pub status:           Option<String>, // last new_align_state {status} string
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
