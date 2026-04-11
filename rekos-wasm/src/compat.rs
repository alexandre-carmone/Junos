//! Snapshot types consumed by the planetarium (`components/sky/`).
//!
//! These are flat, derived projections of the `DeviceStore` — the sky
//! module imports them directly and does not touch `DeviceStore`.

use leptos::prelude::*;

use crate::ws::DeviceStore;

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
