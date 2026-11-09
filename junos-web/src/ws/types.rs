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

/// Mirrors the camera fields KStars sends; not all are shown by the UI.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct CameraStatusData {
    pub device: String,
    pub connected: bool,
    pub pixel_size_um: Option<f64>,
    pub sensor_width: Option<u32>,
    pub sensor_height: Option<u32>,
    // Binning factor from INDI CCD_BINNING (HOR_BIN/VER_BIN). Needed to convert
    // a plate solve's per-binned-pixel scale back into an effective focal
    // length (see astro::effective_focal_mm). None until CCD_BINNING arrives.
    pub bin_x: Option<u32>,
    pub bin_y: Option<u32>,
    // Live imaging state — driven by new_camera_state / new_capture_state /
    // INDI CCD_TEMPERATURE / CCD_COOLER.
    pub temperature: Option<f64>,
    pub target_temperature: Option<f64>,
    pub cooler_on: Option<bool>,
    // Combo option lists. Sourced from INDI switch labels — fetched with
    // compact:false because compact mode strips labels.
    pub capture_format_options:  Vec<String>,  // CCD_CAPTURE_FORMAT
    /// Label of the CCD_CAPTURE_FORMAT switch that is on — what a sequence
    /// job's `<Format>` falls back to when the row leaves it unset.
    pub capture_format:          Option<String>,
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

/// One row of the Capture main camera's Filter Manager table — Ekos'
/// "Filter Settings" (`fm_get_data`, filtermanager.cpp:952-989). Every
/// column is kept because `fm_set_data` (:991) overwrites each column of
/// each row it receives: rows always round-trip whole.
#[allow(dead_code)] // wire shape — the last-AF columns are only round-tripped
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterRow {
    /// 0-based slot; never edited (an out-of-range index crashes KStars).
    pub index: usize,
    pub label: String,
    /// Default exposure for this filter, s (0.001–3600).
    pub exposure: f64,
    /// Focus offset relative to the lock / reference filter, steps (±500000).
    pub offset: i64,
    /// Run autofocus after switching to this filter.
    pub autofocus: bool,
    /// Filter to focus with instead; `"--"` = none.
    pub lock: String,
    pub lastafsolution: i64,
    pub lastaftemp: f64,
    pub lastafalt: f64,
    /// `yyyy-MM-dd hh:mm:ss`; a missing value is stored by KStars as the
    /// literal format string, so it is always sent back verbatim.
    pub lastafdt: String,
    pub tickspertemp: f64,
    pub ticksperalt: f64,
    pub wavelength: i64,
}

impl FilterRow {
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Some(Self {
            index: v["index"].as_u64()? as usize,
            label: s("label"),
            exposure: v["exposure"].as_f64().unwrap_or(1.0),
            offset: v["offset"].as_i64().unwrap_or(0),
            autofocus: v["autofocus"].as_bool().unwrap_or(false),
            lock: if v["lock"].as_str().is_some_and(|l| !l.is_empty()) { s("lock") } else { "--".into() },
            lastafsolution: v["lastafsolution"].as_i64().unwrap_or(0),
            lastaftemp: v["lastaftemp"].as_f64().unwrap_or(-100.0),
            lastafalt: v["lastafalt"].as_f64().unwrap_or(-100.0),
            lastafdt: s("lastafdt"),
            tickspertemp: v["tickspertemp"].as_f64().unwrap_or(0.0),
            ticksperalt: v["ticksperalt"].as_f64().unwrap_or(0.0),
            wavelength: v["wavelength"].as_i64().unwrap_or(500),
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "index": self.index,
            "label": self.label,
            "exposure": self.exposure,
            "offset": self.offset,
            "autofocus": self.autofocus,
            "lock": self.lock,
            "lastafsolution": self.lastafsolution,
            "lastaftemp": self.lastaftemp,
            "lastafalt": self.lastafalt,
            "lastafdt": self.lastafdt,
            "tickspertemp": self.tickspertemp,
            "ticksperalt": self.ticksperalt,
            "wavelength": self.wavelength,
        })
    }
}

/// The Filter Manager table of the Capture main camera's wheel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterManagerData {
    /// INDI name of the wheel; `fm_set_data` is ignored unless it matches.
    pub device: String,
    pub filters: Vec<FilterRow>,
}

impl FilterManagerData {
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let device = v["device"].as_str()?.to_string();
        let filters = v["filters"].as_array()?.iter().filter_map(FilterRow::from_json).collect();
        Some(Self { device, filters })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "device": self.device,
            "filters": self.filters.iter().map(FilterRow::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn row(&self, label: &str) -> Option<&FilterRow> {
        self.filters.iter().find(|f| f.label == label)
    }
}

/// One filter of the Build Filter Offsets table
/// (`filter_offset_get_all_settings`, buildfilteroffsets.cpp:1085-1152).
#[allow(dead_code)] // wire shape — `lock` and `save` are shown, never edited
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterOffsetRow {
    pub filter: String,
    pub selected: bool,
    pub offset: i64,
    pub lock: String,
    pub num_focus_runs: u32,
    /// Focuser position found by each autofocus run so far.
    pub af_runs: Vec<i64>,
    pub average: i64,
    pub new_offset: i64,
    pub save: bool,
}

impl FilterOffsetRow {
    pub fn from_json(v: &serde_json::Value) -> Self {
        Self {
            filter: v["filter"].as_str().unwrap_or("").to_string(),
            selected: v["selected"].as_bool().unwrap_or(false),
            offset: v["offset"].as_i64().unwrap_or(0),
            lock: v["lock"].as_str().unwrap_or("--").to_string(),
            num_focus_runs: v["numFocusRuns"].as_u64().unwrap_or(0) as u32,
            af_runs: v["afRuns"].as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default(),
            average: v["average"].as_i64().unwrap_or(0),
            new_offset: v["newOffset"].as_i64().unwrap_or(0),
            save: v["save"].as_i64().unwrap_or(0) != 0,
        }
    }
}

/// The Build Filter Offsets wizard's state as KStars reports it.
#[allow(dead_code)] // wire shape — `in_build_offsets` is kept for completeness
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterOffsetsData {
    pub filters: Vec<FilterOffsetRow>,
    pub ref_filter: String,
    pub adapt_focus: bool,
    /// Set when a run starts and never cleared (buildfilteroffsets.cpp:505) —
    /// not "running". `table_in_edit_mode` marks a finished run.
    pub in_build_offsets: bool,
    pub table_in_edit_mode: bool,
    pub progress: i64,
    pub progress_max: i64,
    /// Localised status text — display, never match.
    pub status: String,
}

impl FilterOffsetsData {
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let filters = v["filters"].as_array()?.iter().map(FilterOffsetRow::from_json).collect();
        Some(Self {
            filters,
            ref_filter: v["refFilter"].as_str().unwrap_or("").to_string(),
            adapt_focus: v["adaptFocus"].as_bool().unwrap_or(false),
            in_build_offsets: v["inBuildOffsets"].as_bool().unwrap_or(false),
            table_in_edit_mode: v["tableInEditMode"].as_bool().unwrap_or(false),
            progress: v["progress"].as_i64().unwrap_or(0),
            progress_max: v["progressMax"].as_i64().unwrap_or(0),
            status: v["status"].as_str().unwrap_or("").to_string(),
        })
    }

    /// Whether a run already happened in this KStars session: its results
    /// stay until the desktop dialog re-initialises the table.
    pub fn has_results(&self) -> bool {
        self.table_in_edit_mode || self.filters.iter().any(|f| !f.af_runs.is_empty())
    }
}

/// `filter_offset_progress {current, total, status}` (message.cpp:6086).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterOffsetsProgress {
    pub current: i64,
    pub total: i64,
    pub status: String,
}

/// Where the Build Filter Offsets run stands, derived in the store from the
/// replies and pushes (KStars has no single "running" flag on the wire).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BfoPhase {
    #[default]
    Idle,
    Running,
    Stopped,
    Done,
}

// ── Aberration Inspector (Focus) ─────────────────────────────────────────
// A KStars fork's `focus_abins_*` messages (see CLAUDE.md). Field names are
// the camelCase wire keys in snake_case; every struct mirrors the wire shape
// whole, so some fields are kept for completeness rather than read.

fn f64_or(v: &serde_json::Value, key: &str, d: f64) -> f64 {
    v[key].as_f64().unwrap_or(d)
}

fn str_of(v: &serde_json::Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

/// `focus_abins_status`: pushed on start / after analysis / at end, sent in
/// `get_states`, and the reply to `focus_abins_start` (then with
/// `startAccepted` and, when refused, `error`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbInsStatus {
    pub can_start: bool,
    /// Blocker codes (focus.cpp `abInsBlockers`): `noFocuser`,
    /// `algorithmNotLinear1Pass`, `maskNotMosaic`, `inspectorRunning`, …
    pub reasons: Vec<String>,
    pub running: bool,
    pub last_run: i64,
    /// `none` | `complete` | `aborted`.
    pub last_run_state: String,
    pub has_result: bool,
    pub dialog_available: bool,
    /// The `error` of a refused `focus_abins_start` reply; cleared by the
    /// next plain status push.
    pub start_error: Option<String>,
}

impl AbInsStatus {
    pub fn from_json(v: &serde_json::Value) -> Self {
        let refused = v["startAccepted"].as_bool() == Some(false);
        Self {
            can_start: v["canStart"].as_bool().unwrap_or(false),
            reasons: v["reasons"].as_array().map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            running: v["running"].as_bool().unwrap_or(false),
            last_run: v["lastRun"].as_i64().unwrap_or(0),
            last_run_state: str_of(v, "lastRunState"),
            has_result: v["hasResult"].as_bool().unwrap_or(false),
            dialog_available: v["dialogAvailable"].as_bool().unwrap_or(false),
            start_error: refused.then(|| str_of(v, "error")),
        }
    }
}

#[allow(dead_code)] // wire shape
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbInsSensor {
    pub width_px: i64,
    pub height_px: i64,
    pub pixel_size_microns: f64,
    pub tile_width_px: i64,
    pub lr_span_microns: f64,
    pub tb_span_microns: f64,
}

/// One of the nine mosaic tiles, `id = row * 3 + col`, names TL T TR L C R
/// BL B BR. Deltas are tile − centre.
#[allow(dead_code)] // wire shape
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbInsTile {
    pub id: i64,
    pub name: String,
    pub used: bool,
    pub fit: bool,
    pub excluded: bool,
    pub solution: i64,
    pub delta_ticks: i64,
    pub delta_microns: f64,
    pub min_stars: i64,
    pub max_stars: i64,
    pub r2: f64,
    pub min_measure: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbInsBackfocus {
    pub ok: bool,
    /// Negative: move the sensor nearer the flattener.
    pub delta_microns: f64,
    /// `none` | `nearer` | `away`.
    pub advice: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbInsTilt {
    pub ok: bool,
    pub lr_microns: f64,
    pub tb_microns: f64,
    pub diagonal_microns: f64,
    pub lr_percent: f64,
    pub tb_percent: f64,
    pub diagonal_percent: f64,
}

/// One tilt-plate screw: where it sits (rear view, `right` … `lowerRight`)
/// and how far to turn it.
#[allow(dead_code)] // wire shape
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbInsScrew {
    pub index: i64,
    pub angle_deg: f64,
    pub position: String,
    pub adjustment_mm: f64,
    pub turns: f64,
}

/// `tiltPlate` when `valid` — the Tilt Correction Advisory's numbers.
#[allow(dead_code)] // wire shape
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbInsTiltPlate {
    /// 0 = 3-point, 1 = 4-point.
    pub plate_type: i64,
    /// 0 = relative, 1 = push-only.
    pub mode: i64,
    pub radius_mm: f64,
    pub screw_pitch_mm: f64,
    pub thread_index: i64,
    pub plate_angle_deg: f64,
    pub total_tilt_microns: f64,
    pub max_adjustment_mm: f64,
    pub points: Vec<AbInsScrew>,
}

/// `focus_abins_result` (aberrationinspectoranalysis.cpp `toJson` plus
/// `Focus::buildAbInsResult`). `None` from `{"available": false}`.
#[allow(dead_code)] // wire shape
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbInsResult {
    pub run: i64,
    pub train: String,
    /// ISO-8601 UTC.
    pub timestamp: String,
    pub curve_fit: String,
    pub use_weights: bool,
    pub datapoints: i64,
    pub y_axis_label: String,
    pub focuser_step_microns: f64,
    pub cfz_steps: f64,
    pub sensor: AbInsSensor,
    /// `all` | `outerCorners` | `innerDiamond`.
    pub tile_selection: String,
    pub opt_centres: bool,
    /// `insufficientDatapoints` | `datapointMismatch` | `tileOffsetsUnavailable`.
    pub warnings: Vec<String>,
    pub tiles: Vec<AbInsTile>,
    pub backfocus: AbInsBackfocus,
    pub tilt: AbInsTilt,
    pub results_ok: bool,
    pub plate_present: bool,
    pub eta_connected: bool,
    pub tilt_plate: Option<AbInsTiltPlate>,
}

impl AbInsResult {
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        if v["available"].as_bool() == Some(false) {
            return None;
        }
        let tiles = v["tiles"].as_array()?.iter().map(|t| AbInsTile {
            id: t["id"].as_i64().unwrap_or(0),
            name: str_of(t, "name"),
            used: t["used"].as_bool().unwrap_or(false),
            fit: t["fit"].as_bool().unwrap_or(false),
            excluded: t["excluded"].as_bool().unwrap_or(false),
            solution: t["solution"].as_i64().unwrap_or(0),
            delta_ticks: t["deltaTicks"].as_i64().unwrap_or(0),
            delta_microns: f64_or(t, "deltaMicrons", 0.0),
            min_stars: t["minStars"].as_i64().unwrap_or(0),
            max_stars: t["maxStars"].as_i64().unwrap_or(0),
            r2: f64_or(t, "r2", 0.0),
            min_measure: f64_or(t, "minMeasure", 0.0),
        }).collect();
        let s = &v["sensor"];
        let b = &v["backfocus"];
        let t = &v["tilt"];
        let p = &v["tiltPlate"];
        let tilt_plate = (p["valid"].as_bool() == Some(true)).then(|| AbInsTiltPlate {
            plate_type: p["plateType"].as_i64().unwrap_or(0),
            mode: p["mode"].as_i64().unwrap_or(0),
            radius_mm: f64_or(p, "radiusMm", 0.0),
            screw_pitch_mm: f64_or(p, "screwPitchMm", 0.0),
            thread_index: p["threadIndex"].as_i64().unwrap_or(0),
            plate_angle_deg: f64_or(p, "plateAngleDeg", 0.0),
            total_tilt_microns: f64_or(p, "totalTiltMicrons", 0.0),
            max_adjustment_mm: f64_or(p, "maxAdjustmentMm", 0.0),
            points: p["points"].as_array().map(|a| a.iter().map(|q| AbInsScrew {
                index: q["index"].as_i64().unwrap_or(0),
                angle_deg: f64_or(q, "angleDeg", 0.0),
                position: str_of(q, "position"),
                adjustment_mm: f64_or(q, "adjustmentMm", 0.0),
                turns: f64_or(q, "turns", 0.0),
            }).collect()).unwrap_or_default(),
        });
        Some(Self {
            run: v["run"].as_i64().unwrap_or(0),
            train: str_of(v, "train"),
            timestamp: str_of(v, "timestamp"),
            curve_fit: str_of(v, "curveFit"),
            use_weights: v["useWeights"].as_bool().unwrap_or(false),
            datapoints: v["datapoints"].as_i64().unwrap_or(0),
            y_axis_label: str_of(v, "yAxisLabel"),
            focuser_step_microns: f64_or(v, "focuserStepMicrons", 0.0),
            cfz_steps: f64_or(v, "cfzSteps", 0.0),
            sensor: AbInsSensor {
                width_px: s["widthPx"].as_i64().unwrap_or(0),
                height_px: s["heightPx"].as_i64().unwrap_or(0),
                pixel_size_microns: f64_or(s, "pixelSizeMicrons", 0.0),
                tile_width_px: s["tileWidthPx"].as_i64().unwrap_or(0),
                lr_span_microns: f64_or(s, "lrSpanMicrons", 0.0),
                tb_span_microns: f64_or(s, "tbSpanMicrons", 0.0),
            },
            tile_selection: str_of(v, "tileSelection"),
            opt_centres: v["optCentres"].as_bool().unwrap_or(false),
            warnings: v["warnings"].as_array().map(|a| a.iter().filter_map(|w| w.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            tiles,
            backfocus: AbInsBackfocus {
                ok: b["ok"].as_bool().unwrap_or(false),
                delta_microns: f64_or(b, "deltaMicrons", 0.0),
                advice: str_of(b, "advice"),
            },
            tilt: AbInsTilt {
                ok: t["ok"].as_bool().unwrap_or(false),
                lr_microns: f64_or(t, "lrMicrons", 0.0),
                tb_microns: f64_or(t, "tbMicrons", 0.0),
                diagonal_microns: f64_or(t, "diagonalMicrons", 0.0),
                lr_percent: f64_or(t, "lrPercent", 0.0),
                tb_percent: f64_or(t, "tbPercent", 0.0),
                diagonal_percent: f64_or(t, "diagonalPercent", 0.0),
            },
            results_ok: v["resultsOK"].as_bool().unwrap_or(false),
            plate_present: p["present"].as_bool().unwrap_or(false),
            eta_connected: p["etaConnected"].as_bool().unwrap_or(false),
            tilt_plate,
        })
    }
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
    /// Human-readable summary KStars draws above its own V-curve
    /// (`new_focus_state {title}`, manager.cpp:2597). Empty when unset.
    pub plot_title: String,
}

/// One HFR reading. `t_ms` is kept so a future plot can use a time axis.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct HfrSample {
    pub t_ms: f64,
    pub hfr: f64,
    pub position: Option<i64>,
}

/// A single star detected server-side in a focus frame (coordinates *and* HFR
/// in the focus JPEG's pixel space — the same image shown in the preview).
/// Multiply `hfr` by [`FocusStars::sensor_scale`] to compare it with the HFR
/// KStars reports, which is in sensor pixels.
#[derive(Debug, Clone)]
pub struct FocusStar {
    pub x: f64,
    pub y: f64,
    pub hfr: f64,
}

/// Detected stars for the current focus frame, plus the JPEG dimensions the
/// coordinates are relative to (needed to map onto the letterboxed preview).
#[derive(Debug, Clone)]
pub struct FocusStars {
    pub img_w: f64,
    pub img_h: f64,
    pub stars: Vec<FocusStar>,
    /// Sensor pixels per JPEG pixel — `metadata.resolution.w / img_w`. KStars
    /// renders the focus view at a zoom-dependent scale and then caps it at
    /// 960 px wide (media.cpp:454), so this is an arbitrary positive real, not
    /// 1 or 2, and can be < 1. `None` when the frame geometry can't be trusted:
    /// missing metadata, or the Aberration Inspector's 3×3 mosaic view where
    /// `displayPixmap` is a tile montage rather than the frame
    /// (fitsview.cpp:1105) — there the star *coordinates* are wrong too.
    pub sensor_scale: Option<f64>,
    /// KStars' own aggregate HFR for this exact frame (`metadata.hfr`,
    /// media.cpp:443), in sensor pixels. `None` when unmeasured.
    ///
    /// Note this is not the same estimator as ours: `starfind.rs` computes the
    /// flux-weighted *mean* radius, KStars/SEP the *half-flux* radius. For a
    /// Gaussian those differ by ~6% (1.253σ vs 1.177σ), so the two values stay
    /// close but never identical — the UI labels them separately.
    pub kstars_hfr: Option<f64>,
}

/// KStars' statistics of the raw frame behind a capture preview, in ADU
/// (`metadata`, media.cpp:446). The JPEG itself is auto-stretched, so these
/// are the frame's real levels. `bpp` is the sample size in bits (8, 16, 32
/// or 64, `FITSData::bpp`). `shadows` / `midtones` / `highlights` are the
/// stretch that made the JPEG (0–1 of the full scale, red channel only;
/// 0 / 0.5 / 1 is no stretch).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameStats {
    pub median: f64,
    pub mean: f64,
    pub stddev: f64,
    pub min: f64,
    pub max: f64,
    pub bpp: u32,
    pub shadows: f64,
    pub midtones: f64,
    pub highlights: f64,
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

/// KStars' observer geographic location, from `astro_get_location`
/// (message.cpp:1848). This is the authoritative site the planetarium must
/// use so its horizontal-coordinate math matches KStars; the browser's
/// hardcoded default (Paris) is only a fallback before this arrives.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct SiteInfo {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Metres above sea level. Not yet used by the sky math, kept for parity.
    pub elevation: f64,
}

impl SiteInfo {
    pub(super) fn from_json(v: &serde_json::Value) -> Self {
        Self {
            name: v["name"].as_str().unwrap_or("").to_string(),
            latitude: v["latitude"].as_f64().unwrap_or(0.0),
            longitude: v["longitude"].as_f64().unwrap_or(0.0),
            elevation: v["elevation"].as_f64().unwrap_or(0.0),
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
#[allow(dead_code)]
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

// Polar alignment state (PAA). See kstars/ekos/align/polaralignmentassistant.*
// and the `new_polar_state` arms in message.cpp:1157-1263.
/// Mirrors the `new_polar_state` vector payload; the UI reads the errors only.
#[allow(dead_code)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct GuideStateSample {
    pub t_ms:   f64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
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

/// One entry in the plate-solve process timeline — a status transition
/// stamped with the wall-clock time it was received. Mirrors the
/// `GuideStateSample` pattern.
#[derive(Debug, Clone)]
pub struct AlignStateSample {
    pub t_ms:   f64,
    pub status: String,
}

/// One Align capture as sent on the media socket (uuid `+A`, `Media::upload`,
/// media.cpp:446-471). `width`/`height` are the solver's (binned) frame from
/// `metadata.resolution`, not the JPEG's — KStars downscales the JPEG to 960 px
/// wide but keeps the aspect ratio.
#[derive(Debug, Clone, PartialEq)]
pub struct AlignFrame {
    pub url:    String,
    pub width:  u32,
    pub height: u32,
}

/// Latest plate-solve result. Populated from the `solution` payload of
/// `new_align_state` (kstars/ekos/align/align.cpp:2351). All RA/Dec values are
/// JNow — KStars uses `m_AlignCoord` which is epoch-of-date.
#[derive(Debug, Clone, Default)]
pub struct AlignSolutionData {
    pub ra_jnow_deg:      Option<f64>,
    pub dec_jnow_deg:     Option<f64>,
    pub orientation_deg:  Option<f64>, // PA from align.cpp:2364
    pub pixscale_arcsec:  Option<f64>, // pix from align.cpp:2363 — arcsec per *binned* px
    // `fov` "W' x H'" (align_solver.cpp:988 ← FOVOut, align_fov.cpp:210): KStars'
    // effective FOV in arcmin, already binning-corrected — unlike `pix`.
    pub fov_arcmin:       Option<(f64, f64)>,
    pub solved_at_ms:     Option<f64>, // js_sys::Date::now() at receipt
    /// The Align frame this solution was computed on, or `None` when the solve
    /// came without one (remote solver, Load & Slew — see `align_frame_pending`).
    pub image:            Option<AlignFrame>,
    pub status:           Option<String>, // last new_align_state {status} string
    // Progress overlay inputs — all from `new_align_state` (manager.cpp:2500).
    pub log:              String,           // full accumulated solver log text
    pub download_progress: Option<String>,  // remote-solver download info
    pub history:          Vec<AlignStateSample>, // status transition timeline
}

// ---------------------------------------------------------------------------
// Generic INDI property mirror — Devices tab (INDI control panel).
//
// Wire shapes: kstars/indi/indistd.cpp:1696-1809 ({switch,number,text,light}ToJson).
// Non-compact adds top-level label/group/perm (+rule for switches) and
// per-element label/min/max/step/format; compact carries only element
// name + value/state. `state`/`perm`/`rule` and switch/light element
// `state` are transmitted as INDI enum ints — but older paths in this
// codebase have seen string forms ("Busy", "On"), so parse both.
// ---------------------------------------------------------------------------

/// INDI `IPState` — property (and light element) state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndiState {
    #[default]
    Idle,  // 0
    Ok,    // 1
    Busy,  // 2
    Alert, // 3
}

impl IndiState {
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v.as_i64() {
            Some(0) => Self::Idle,
            Some(1) => Self::Ok,
            Some(2) => Self::Busy,
            Some(3) => Self::Alert,
            _ => match v.as_str() {
                Some("Ok") => Self::Ok,
                Some("Busy") => Self::Busy,
                Some("Alert") => Self::Alert,
                _ => Self::Idle,
            },
        }
    }
}

/// INDI `IPerm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndiPerm {
    Ro, // 0
    Wo, // 1
    #[default]
    Rw, // 2
}

impl IndiPerm {
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v.as_i64() {
            Some(0) => Self::Ro,
            Some(1) => Self::Wo,
            Some(2) => Self::Rw,
            _ => match v.as_str() {
                Some("ro") => Self::Ro,
                Some("wo") => Self::Wo,
                _ => Self::Rw,
            },
        }
    }

    pub fn writable(self) -> bool {
        !matches!(self, Self::Ro)
    }
}

/// INDI `ISRule` — switch vector semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndiRule {
    #[default]
    OneOfMany, // 0
    AtMostOne, // 1
    NOfMany,   // 2
}

impl IndiRule {
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v.as_i64() {
            Some(1) => Self::AtMostOne,
            Some(2) => Self::NOfMany,
            _ => Self::OneOfMany,
        }
    }
}

/// Switch/light element state — tolerant of int (ISState, what
/// `switchToJson` sends), bool, and string ("On"/"Off") forms.
pub(super) fn switch_state_from_json(el: &serde_json::Value) -> bool {
    el["state"]
        .as_i64()
        .map(|v| v == 1)
        .or_else(|| el["state"].as_bool())
        .or_else(|| el["value"].as_bool())
        .or_else(|| el["state"].as_str().map(|s| s == "On"))
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq)]
pub enum IndiElementValue {
    Number { value: f64, min: f64, max: f64, step: f64, format: String },
    Text(String),
    Switch(bool),
    Light(IndiState),
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndiElement {
    pub name: String,
    pub label: String,
    pub value: IndiElementValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndiProperty {
    pub name: String,
    pub label: String,
    pub group: String,
    pub state: IndiState,
    pub perm: IndiPerm,
    /// Only meaningful for switch properties.
    pub rule: IndiRule,
    /// True when parsed from a non-compact payload (labels/group/perm and
    /// number min/max/step/format are real, not defaults). Compact-first
    /// records get upgraded when the `device_get` reply lands.
    pub full: bool,
    pub elements: Vec<IndiElement>,
}

impl IndiProperty {
    pub fn is_switch(&self) -> bool {
        matches!(
            self.elements.first().map(|e| &e.value),
            Some(IndiElementValue::Switch(_))
        )
    }

    /// Parse a full (compact or non-compact) property object. Kind is
    /// detected by which element array is present. Labels fall back to
    /// names, group to "Main Control" — compact pushes arriving before the
    /// non-compact `device_get` still render.
    pub fn from_json(p: &serde_json::Value) -> Option<Self> {
        let name = p["name"].as_str()?.to_string();
        let s = |el: &serde_json::Value, k: &str, fb: &str| {
            el[k].as_str().unwrap_or(fb).to_string()
        };
        let elem_common = |el: &serde_json::Value| -> (String, String) {
            let n = el["name"].as_str().unwrap_or("").to_string();
            let l = s(el, "label", &n);
            (n, l)
        };

        let elements: Vec<IndiElement> = if let Some(arr) = p["switches"].as_array() {
            arr.iter()
                .map(|el| {
                    let (n, l) = elem_common(el);
                    IndiElement { name: n, label: l, value: IndiElementValue::Switch(switch_state_from_json(el)) }
                })
                .collect()
        } else if let Some(arr) = p["numbers"].as_array() {
            arr.iter()
                .map(|el| {
                    let (n, l) = elem_common(el);
                    IndiElement {
                        name: n,
                        label: l,
                        value: IndiElementValue::Number {
                            value: el["value"].as_f64().unwrap_or(0.0),
                            min: el["min"].as_f64().unwrap_or(0.0),
                            max: el["max"].as_f64().unwrap_or(0.0),
                            step: el["step"].as_f64().unwrap_or(0.0),
                            format: s(el, "format", ""),
                        },
                    }
                })
                .collect()
        } else if let Some(arr) = p["texts"].as_array() {
            arr.iter()
                .map(|el| {
                    let (n, l) = elem_common(el);
                    IndiElement { name: n, label: l, value: IndiElementValue::Text(s(el, "text", "")) }
                })
                .collect()
        } else if let Some(arr) = p["lights"].as_array() {
            arr.iter()
                .map(|el| {
                    let (n, l) = elem_common(el);
                    IndiElement { name: n, label: l, value: IndiElementValue::Light(IndiState::from_json(&el["state"])) }
                })
                .collect()
        } else {
            return None; // BLOBs aren't serialized by KStars (indistd.cpp:1031)
        };

        let is_light = p["lights"].is_array();
        Some(Self {
            label: s(p, "label", &name),
            group: s(p, "group", "Main Control"),
            state: IndiState::from_json(&p["state"]),
            // Lights carry no perm on the wire — always read-only.
            perm: if is_light { IndiPerm::Ro } else { IndiPerm::from_json(&p["perm"]) },
            rule: IndiRule::from_json(&p["rule"]),
            full: p["label"].is_string(),
            elements,
            name,
        })
    }

    /// Merge a compact update (pushed `device_property_get`) into an existing
    /// full record: element values + property state only — labels, group,
    /// perm, rule and number min/max/step/format are preserved.
    pub fn merge_compact(&mut self, p: &serde_json::Value) {
        self.state = IndiState::from_json(&p["state"]);
        let arr = p["switches"]
            .as_array()
            .or_else(|| p["numbers"].as_array())
            .or_else(|| p["texts"].as_array())
            .or_else(|| p["lights"].as_array());
        let Some(arr) = arr else { return };
        for el in arr {
            let Some(n) = el["name"].as_str() else { continue };
            let Some(existing) = self.elements.iter_mut().find(|e| e.name == n) else { continue };
            match &mut existing.value {
                IndiElementValue::Number { value, .. } => {
                    if let Some(v) = el["value"].as_f64() {
                        *value = v;
                    }
                }
                IndiElementValue::Text(t) => {
                    if let Some(v) = el["text"].as_str() {
                        *t = v.to_string();
                    }
                }
                IndiElementValue::Switch(on) => *on = switch_state_from_json(el),
                IndiElementValue::Light(st) => *st = IndiState::from_json(&el["state"]),
            }
        }
    }
}

/// KStars' answer to a file command the browser sent: `scheduler_load_file`
/// and `capture_load_sequence_file` reply `{result, path}`; `scheduler_save_file`
/// replies with the saved file's text, and nothing at all when it fails.
#[derive(Debug, Clone)]
pub struct FileReply {
    /// The command answered.
    pub cmd: String,
    pub ok: bool,
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
