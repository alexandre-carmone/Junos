//! Snapshot types consumed by the planetarium (`components/sky/`).
//!
//! These are flat, derived projections of the `DeviceStore` — the sky
//! module imports them directly and does not touch `DeviceStore`.

use leptos::prelude::*;

use crate::ws::{DeviceStore, HfrSample};

#[derive(Debug, Clone, Default)]
pub struct MountSnapshot {
    pub device_name: Option<String>,
    pub connected: bool,
    pub slewing: bool,
    pub tracking: bool,
    pub parked: bool,
    pub ra_h: Option<f64>,
    pub dec_deg: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct CameraSnapshot {
    pub pixel_size_um: Option<f64>,
    pub sensor_width: Option<u32>,
    pub sensor_height: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct SiteSnapshot {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SolveSnapshot {
    pub rotation_deg: Option<f64>,
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
                pixel_size_um: cs.pixel_size_um,
                sensor_width: cs.sensor_width,
                sensor_height: cs.sensor_height,
            },
            None => CameraSnapshot::default(),
        }
    })
}
