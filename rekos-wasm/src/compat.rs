//! Snapshot types consumed by the planetarium (`components/sky/`).
//!
//! These are flat, derived projections of the `DeviceStore` — the sky
//! module imports them directly and does not touch `DeviceStore`.

use leptos::prelude::*;

use crate::ws::{
    DeviceStore, GuideDriftSample, GuideStateSample, HfrSample, PolarVectorData,
};

#[derive(Debug, Clone, Default)]
pub struct MountSnapshot {
    pub device_name: Option<String>,
    pub connected: bool,
    pub slewing: bool,
    pub tracking: bool,
    pub parked: bool,
    pub ra_h: Option<f64>,
    pub dec_deg: Option<f64>,
    pub ha_deg: Option<f64>,
    pub pier_side: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct CameraSnapshot {
    pub device: String,
    pub pixel_size_um: Option<f64>,
    pub sensor_width: Option<u32>,
    pub sensor_height: Option<u32>,
    pub temperature: Option<f64>,
    pub cooler_on: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct CaptureSnapshot {
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
    pub preview_url: Option<String>,
    pub settings: serde_json::Value,
    pub sequence: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct SiteSnapshot {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SolveSnapshot {
    pub rotation_deg:    Option<f64>,
    pub ra_jnow_deg:     Option<f64>,
    pub dec_jnow_deg:    Option<f64>,
    pub pixscale_arcsec: Option<f64>,
    pub solved_at_ms:    Option<f64>,
}

pub fn derive_solve(store: &DeviceStore) -> Signal<SolveSnapshot> {
    let sig = store.align_solution;
    Signal::derive(move || {
        let a = sig.get();
        SolveSnapshot {
            rotation_deg:    a.orientation_deg,
            ra_jnow_deg:     a.ra_jnow_deg,
            dec_jnow_deg:    a.dec_jnow_deg,
            pixscale_arcsec: a.pixscale_arcsec,
            solved_at_ms:    a.solved_at_ms,
        }
    })
}

#[derive(Debug, Clone, Default)]
pub struct FocusSnapshot {
    pub device: String,
    pub connected: bool,
    pub status: String,
    pub hfr: Option<f64>,
    pub position: Option<i64>,
    pub temperature: Option<f64>,
    pub log: String,
    pub preview_url: Option<String>,
    pub history: Vec<HfrSample>,
    pub settings: serde_json::Value,
}

pub fn derive_mount(store: &DeviceStore) -> Signal<MountSnapshot> {
    let mount_status = store.mount_status;
    Signal::derive(move || {
        match mount_status.get() {
            Some(ms) => MountSnapshot {
                device_name: Some(ms.device),
                connected: ms.connected,
                slewing: ms.slewing,
                tracking: ms.tracking,
                parked: ms.parked,
                ra_h: ms.ra_h,
                dec_deg: ms.dec_deg,
                ha_deg: ms.ha_deg,
                pier_side: ms.pier_side,
            },
            None => MountSnapshot::default(),
        }
    })
}

pub fn derive_focus(store: &DeviceStore) -> Signal<FocusSnapshot> {
    let focus_status      = store.focus_status;
    let focus_settings    = store.focus_settings;
    let focus_preview_url = store.focus_preview_url;
    let focus_hfr_history = store.focus_hfr_history;
    Signal::derive(move || {
        let (device, connected, status, hfr, position, temperature, log) =
            match focus_status.get() {
                Some(fs) => (fs.device, fs.connected, fs.status, fs.hfr, fs.position, fs.temperature, fs.log),
                None => (String::new(), false, String::new(), None, None, None, String::new()),
            };
        FocusSnapshot {
            device,
            connected,
            status,
            hfr,
            position,
            temperature,
            log,
            preview_url: focus_preview_url.get(),
            history: focus_hfr_history.get(),
            settings: focus_settings.get(),
        }
    })
}

pub fn derive_camera(store: &DeviceStore) -> Signal<CameraSnapshot> {
    let camera_status = store.camera_status;
    Signal::derive(move || {
        match camera_status.get() {
            Some(cs) => CameraSnapshot {
                device: cs.device,
                pixel_size_um: cs.pixel_size_um,
                sensor_width: cs.sensor_width,
                sensor_height: cs.sensor_height,
                temperature: cs.temperature,
                cooler_on: cs.cooler_on,
            },
            None => CameraSnapshot::default(),
        }
    })
}

pub fn derive_capture(store: &DeviceStore) -> Signal<CaptureSnapshot> {
    let status_sig   = store.capture_status;
    let settings_sig = store.capture_settings;
    let seq_sig      = store.capture_sequence;
    let preview_sig  = store.capture_preview_url;
    Signal::derive(move || {
        let s = status_sig.get();
        CaptureSnapshot {
            status: s.status,
            target: s.target,
            seq_total: s.seq_total,
            seq_current: s.seq_current,
            progress: s.progress,
            seq_remaining_time: s.seq_remaining_time,
            overall_remaining_time: s.overall_remaining_time,
            exposure_left: s.exposure_left,
            exposure_total: s.exposure_total,
            log: s.log,
            preview_url: preview_sig.get(),
            settings: settings_sig.get(),
            sequence: seq_sig.get(),
        }
    })
}

#[derive(Debug, Clone, Default)]
pub struct PolarAlignSnapshot {
    pub enabled:           bool,
    pub stage:             String,
    pub message:           String,
    pub vector:            Option<PolarVectorData>,
    pub updated_error:     Option<f64>,
    pub updated_az_error:  Option<f64>,
    pub updated_alt_error: Option<f64>,
    pub settings:          serde_json::Value,
    pub preview_url:       Option<String>,
}

pub fn derive_polar_align(store: &DeviceStore) -> Signal<PolarAlignSnapshot> {
    let state       = store.polar_state;
    let settings    = store.align_settings;
    let preview_sig = store.align_preview_url;
    Signal::derive(move || {
        let p = state.get();
        PolarAlignSnapshot {
            enabled:           p.enabled,
            stage:             p.stage,
            message:           p.message,
            vector:            p.vector,
            updated_error:     p.updated_error,
            updated_az_error:  p.updated_az_error,
            updated_alt_error: p.updated_alt_error,
            settings:          settings.get(),
            preview_url:       preview_sig.get(),
        }
    })
}

#[derive(Debug, Clone, Default)]
pub struct GuideSnapshot {
    pub connected:   bool,
    pub status:      String,
    pub history:     Vec<GuideStateSample>,
    pub drift:       Vec<GuideDriftSample>,
    pub ra_rms:      Option<f64>,
    pub de_rms:      Option<f64>,
    pub log:         String,
    pub preview_url: Option<String>,
    pub settings:    serde_json::Value,
    pub options:     serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct SchedulerSnapshot {
    pub status:   i64,
    pub log:      String,
    pub jobs:     Vec<serde_json::Value>,
    pub settings: serde_json::Value,
    pub home_dir: String,
}

pub fn derive_scheduler(store: &DeviceStore) -> Signal<SchedulerSnapshot> {
    let status_sig   = store.scheduler_status;
    let settings_sig = store.scheduler_settings;
    let jobs_sig     = store.scheduler_jobs;
    let home_sig     = store.home_dir;
    Signal::derive(move || {
        let s = status_sig.get();
        SchedulerSnapshot {
            status:   s.status,
            log:      s.log,
            jobs:     jobs_sig.get(),
            settings: settings_sig.get(),
            home_dir: home_sig.get(),
        }
    })
}

#[derive(Debug, Clone, Default)]
pub struct MosaicTileData {
    pub ra_deg:   f64,
    pub dec_deg:  f64,
    pub index:    u32,
    pub rotation: f64,
}

#[derive(Debug, Clone, Default)]
pub struct MosaicSnapshot {
    pub target_name:     Option<String>,
    pub center_ra_deg:   Option<f64>,
    pub center_dec_deg:  Option<f64>,
    pub grid_w:          Option<u32>,
    pub grid_h:          Option<u32>,
    pub overlap:         Option<f64>,
    pub camera_fov_w_deg: Option<f64>,
    pub camera_fov_h_deg: Option<f64>,
    pub pa:              Option<f64>,
    pub tiles:           Vec<MosaicTileData>,
}

pub fn derive_mosaic(store: &DeviceStore) -> Signal<MosaicSnapshot> {
    let mosaic_tiles = store.mosaic_tiles;
    Signal::derive(move || {
        let Some(v) = mosaic_tiles.get() else {
            return MosaicSnapshot::default();
        };
        let tiles = v["tiles"].as_array().map(|arr| {
            arr.iter().filter_map(|t| {
                let sc = &t["skyCenter"];
                let ra_deg  = sc["ra0"].as_f64()?;
                let dec_deg = sc["dec0"].as_f64()?;
                let index    = t["index"].as_u64().unwrap_or(0) as u32;
                let rotation = t["rotation"].as_f64().unwrap_or(0.0);
                Some(MosaicTileData { ra_deg, dec_deg, index, rotation })
            }).collect()
        }).unwrap_or_default();
        MosaicSnapshot {
            target_name:     v["targetName"].as_str().map(|s| s.to_string()),
            center_ra_deg:   v["ra0"].as_f64(),
            center_dec_deg:  v["dec0"].as_f64(),
            grid_w:          v["gridSize"]["width"].as_u64().map(|x| x as u32),
            grid_h:          v["gridSize"]["height"].as_u64().map(|x| x as u32),
            overlap:         v["overlap"].as_f64(),
            // cameraFOV is in arcmin → convert to degrees
            camera_fov_w_deg: v["cameraFOV"]["width"].as_f64().map(|x| x / 60.0),
            camera_fov_h_deg: v["cameraFOV"]["height"].as_f64().map(|x| x / 60.0),
            pa:              v["positionAngle"].as_f64(),
            tiles,
        }
    })
}

pub fn derive_guide(store: &DeviceStore) -> Signal<GuideSnapshot> {
    let status_sig   = store.guide_status;
    let settings_sig = store.guide_settings;
    let options_sig  = store.guide_options;
    let preview_sig  = store.guide_preview_url;
    Signal::derive(move || {
        match status_sig.get() {
            Some(gs) => GuideSnapshot {
                connected:   gs.connected,
                status:      gs.status,
                history:     gs.history,
                drift:       gs.drift,
                ra_rms:      gs.ra_rms,
                de_rms:      gs.de_rms,
                log:         gs.log,
                preview_url: preview_sig.get(),
                settings:    settings_sig.get(),
                options:     options_sig.get(),
            },
            None => GuideSnapshot {
                preview_url: preview_sig.get(),
                settings:    settings_sig.get(),
                options:     options_sig.get(),
                ..GuideSnapshot::default()
            },
        }
    })
}
