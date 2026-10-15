//! Which INDIGO device plays which Ekos role, and the optical train / scope
//! replies that tell the browser so.
//!
//! Ekos keys everything on optical trains (`train_get_all`) and the browser
//! binds each module to its train's device (`ws/mod.rs`). INDIGO has no such
//! thing: the agents pick devices with `FILTER_*_LIST` switches. The roles
//! follow those lists — what the Imager Agent holds is the camera, what the
//! Mount Agent holds is the mount — and a list still at `NONE` gets a default
//! pick from the `INFO.DEVICE_INTERFACE` bits, which `agents.rs` then writes
//! into the list. Without agents the defaults are the roles.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::agents::{AgentSet, Kind};
use super::protocol::{interface, PropertyStore};

pub const TRAIN_NAME: &str = "INDIGO";
pub const TRAIN_ID: i64 = 1;
pub const SCOPE_NAME: &str = "INDIGO lens";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roles {
    pub mount: Option<String>,
    pub camera: Option<String>,
    /// The guide camera (what Ekos' train calls the guider).
    pub guider: Option<String>,
    /// The pulse-guiding output — an ST4 port or the mount's own.
    pub guider_port: Option<String>,
    pub focuser: Option<String>,
    pub wheel: Option<String>,
}

/// How much a device name looks like a given role: the simulator driver
/// ships "CCD Imager Simulator", "CCD Guider Simulator", "CCD Bahtinov Mask
/// Simulator", "CCD File Simulator" and "DSLR Simulator", all cameras.
fn camera_score(name: &str, guide: bool) -> i32 {
    let lower = name.to_ascii_lowercase();
    if lower.contains("bahtinov") || lower.contains("file simulator") {
        -3
    } else if lower.contains("guid") {
        if guide { 2 } else { -2 }
    } else if lower.contains("dslr") {
        -1
    } else if lower.contains("imager") {
        if guide { -1 } else { 1 }
    } else {
        0
    }
}

impl Roles {
    pub fn derive(store: &PropertyStore, agents: &AgentSet) -> Self {
        let held = agents.held(store);
        let devices: Vec<(&str, u32)> = store
            .device_names()
            .map(|d| (d, store.interface(d)))
            .filter(|(_, i)| i & interface::AGENT == 0)
            .filter(|(d, _)| !held.contains(*d))
            .collect();
        let best = |filter: &dyn Fn(&str, u32) -> bool, score: &dyn Fn(&str) -> i32| -> Option<String> {
            let mut candidates: Vec<(i32, usize, &str)> = devices
                .iter()
                .enumerate()
                .filter(|(_, (d, i))| filter(d, *i))
                .map(|(idx, (d, _))| (score(d), idx, *d))
                .collect();
            // Highest score, then definition order.
            candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            candidates.first().map(|(_, _, d)| d.to_string())
        };
        let chosen = |kind: Kind, list: &str| agents.get(kind).and_then(|a| AgentSet::selected(store, a, list));

        let mount = chosen(Kind::Mount, "FILTER_MOUNT_LIST")
            .or_else(|| best(&|_, i| i & interface::MOUNT != 0, &|_| 0));
        let camera = chosen(Kind::Imager, "FILTER_CCD_LIST")
            .or_else(|| best(&|_, i| i & interface::CCD != 0, &|d| camera_score(d, false)));
        let ccd_count = store
            .device_names()
            .filter(|d| store.interface(d) & interface::AGENT == 0 && store.interface(d) & interface::CCD != 0)
            .count();
        let guider = chosen(Kind::Guider, "FILTER_CCD_LIST").or_else(|| {
            // Never the main camera: two agents can't share one device.
            if ccd_count < 2 {
                return None;
            }
            best(&|d, i| i & interface::CCD != 0 && Some(d) != camera.as_deref(), &|d| camera_score(d, true))
        });
        let prefix_score = |prefer: &Option<String>, d: &str| -> i32 {
            prefer.as_deref().is_some_and(|p| d.starts_with(p)) as i32
        };
        let guider_port = chosen(Kind::Guider, "FILTER_GUIDER_LIST").or_else(|| {
            best(
                &|_, i| i & interface::GUIDER != 0 && i & (interface::CCD | interface::MOUNT) == 0,
                &|d| 2 * prefix_score(&guider, d) + prefix_score(&mount, d),
            )
        });
        let focuser = chosen(Kind::Imager, "FILTER_FOCUSER_LIST")
            .or_else(|| best(&|_, i| i & interface::FOCUSER != 0, &|d| prefix_score(&camera, d)));
        let wheel = chosen(Kind::Imager, "FILTER_WHEEL_LIST")
            .or_else(|| best(&|_, i| i & interface::WHEEL != 0, &|d| prefix_score(&camera, d)));
        Self { mount, camera, guider, guider_port, focuser, wheel }
    }

    /// Every device the roles name.
    #[allow(dead_code)]
    pub fn devices(&self) -> HashSet<&str> {
        [&self.mount, &self.camera, &self.guider, &self.guider_port, &self.focuser, &self.wheel]
            .into_iter()
            .filter_map(|d| d.as_deref())
            .collect()
    }

    /// `train_get_all`: one train. Empty slots are `"--"`, which the browser
    /// treats as unset (`ws/types.rs`).
    pub fn train_payload(&self) -> Value {
        let slot = |d: &Option<String>| d.clone().unwrap_or_else(|| "--".into());
        json!([{
            "id": TRAIN_ID,
            "name": TRAIN_NAME,
            "mount": slot(&self.mount),
            "camera": slot(&self.camera),
            "scope": SCOPE_NAME,
            "guider": slot(&self.guider),
            "focuser": slot(&self.focuser),
            "filterwheel": slot(&self.wheel),
            "rotator": "--",
            "dustcap": "--",
            "lightbox": "--",
            "reducer": 1.0,
        }])
    }

    /// `train_get_profiles`: every module on the one train (the keys are
    /// Ekos' module enum, Capture 1 … Align 5, plus 0 and 6).
    pub fn train_profiles_payload(&self) -> Value {
        let mut m = serde_json::Map::new();
        for k in 0..=6 {
            m.insert(k.to_string(), json!(TRAIN_ID));
        }
        Value::Object(m)
    }

    /// `get_scopes`: the one scope the train names, from the camera's
    /// `CCD_LENS {FOCAL_LENGTH, APERTURE}` (what INDIGO's agents use for the
    /// FOV) — in centimetres there (`indigo_ccd_driver.c`), millimetres in
    /// Ekos' scope DB. Empty until the camera reports a focal length — the
    /// browser's FOV reticle then simply has no size.
    pub fn scopes_payload(&self, store: &PropertyStore) -> Value {
        let lens = self
            .camera
            .as_deref()
            .and_then(|c| store.get(c, "CCD_LENS"));
        let focal_mm = lens.and_then(|p| p.number("FOCAL_LENGTH")).unwrap_or(0.0) * 10.0;
        if focal_mm <= 0.0 {
            return json!([]);
        }
        json!([{
            "id": "1",
            "name": SCOPE_NAME,
            "model": SCOPE_NAME,
            "vendor": "INDIGO",
            "type": "Refractor",
            "focal_length": focal_mm,
            "aperture": lens.and_then(|p| p.number("APERTURE")).unwrap_or(0.0) * 10.0,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    fn info(device: &str, iface: u32) -> String {
        format!(r#"{{"defTextVector":{{"device":"{device}","name":"INFO","perm":"ro","state":"Ok","items":[{{"name":"DEVICE_INTERFACE","value":"{iface}"}}]}}}}"#)
    }

    #[test]
    fn picks_by_interface_and_skips_agents() {
        let mut store = PropertyStore::default();
        for f in [
            info("Imager Agent", interface::AGENT | interface::CCD),
            info("Main Cam", interface::CCD),
            info("Guide Cam", interface::CCD | interface::GUIDER),
            info("Mount", interface::MOUNT | interface::GUIDER),
            info("Wheel", interface::WHEEL),
        ] {
            store.apply(parse_frame(&f).unwrap());
        }
        let r = Roles::derive(&store, &AgentSet::default());
        assert_eq!(r.mount.as_deref(), Some("Mount"));
        assert_eq!(r.camera.as_deref(), Some("Main Cam"));
        assert_eq!(r.guider.as_deref(), Some("Guide Cam"));
        assert_eq!(r.wheel.as_deref(), Some("Wheel"));
        assert_eq!(r.focuser, None);
        assert_eq!(r.guider_port, None, "a GUIDER bit on a CCD or a mount is not a port");
        let t = r.train_payload();
        assert_eq!(t[0]["camera"], "Main Cam");
        assert_eq!(t[0]["focuser"], "--");
        assert_eq!(r.train_profiles_payload()["3"], TRAIN_ID);
        assert_eq!(r.scopes_payload(&store), json!([]));
        store.apply(parse_frame(r#"{"defNumberVector":{"device":"Main Cam","name":"CCD_LENS","perm":"rw","state":"Ok","items":[{"name":"APERTURE","value":8},{"name":"FOCAL_LENGTH","value":40}]}}"#).unwrap());
        let s = r.scopes_payload(&store);
        assert_eq!(s[0]["name"], SCOPE_NAME);
        assert_eq!(s[0]["focal_length"], 400.0);
        assert_eq!(s[0]["aperture"], 80.0);
    }

    #[test]
    fn simulator_roster_lands_on_the_right_devices() {
        let mut store = PropertyStore::default();
        for f in [
            info("CCD Imager Simulator", interface::CCD),
            info("CCD Guider Simulator", interface::CCD),
            info("CCD Bahtinov Mask Simulator", interface::CCD),
            info("DSLR Simulator", interface::CCD),
            info("CCD File Simulator", interface::CCD),
            info("CCD Imager Simulator (wheel)", interface::WHEEL),
            info("CCD Imager Simulator (focuser)", interface::FOCUSER),
            info("CCD Guider Simulator (guider)", interface::GUIDER),
            info("CCD Guider Simulator (AO)", interface::AO),
            info("Mount Simulator", interface::MOUNT | interface::GUIDER),
            info("Mount Simulator (guider)", interface::GUIDER),
        ] {
            store.apply(parse_frame(&f).unwrap());
        }
        let r = Roles::derive(&store, &AgentSet::default());
        assert_eq!(r.camera.as_deref(), Some("CCD Imager Simulator"));
        assert_eq!(r.guider.as_deref(), Some("CCD Guider Simulator"));
        assert_eq!(r.guider_port.as_deref(), Some("CCD Guider Simulator (guider)"));
        assert_eq!(r.focuser.as_deref(), Some("CCD Imager Simulator (focuser)"));
        assert_eq!(r.wheel.as_deref(), Some("CCD Imager Simulator (wheel)"));
        assert_eq!(r.mount.as_deref(), Some("Mount Simulator"));
    }

    #[test]
    fn lone_camera_has_no_guide_camera() {
        let mut store = PropertyStore::default();
        store.apply(parse_frame(&info("Cam", interface::CCD)).unwrap());
        let r = Roles::derive(&store, &AgentSet::default());
        assert_eq!(r.camera.as_deref(), Some("Cam"));
        assert_eq!(r.guider, None);
    }

    #[test]
    fn roles_follow_the_agents_selection() {
        let mut store = PropertyStore::default();
        for f in [
            info("CCD Imager Simulator", interface::CCD),
            info("CCD Guider Simulator", interface::CCD),
            info("Imager Agent", interface::AGENT | interface::CCD),
            r#"{"defNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_BATCH","perm":"rw","state":"Ok","items":[{"name":"EXPOSURE","value":1}]}}"#.to_string(),
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"FILTER_CCD_LIST","perm":"rw","state":"Ok","items":[{"name":"NONE","value":false},{"name":"CCD Imager Simulator","value":false},{"name":"CCD Guider Simulator","value":true}]}}"#.to_string(),
        ] {
            store.apply(parse_frame(&f).unwrap());
        }
        let agents = AgentSet::discover(&store);
        let r = Roles::derive(&store, &agents);
        // The user pointed the imager at the guider camera: the camera role
        // follows, and the guide camera falls back to the other CCD.
        assert_eq!(r.camera.as_deref(), Some("CCD Guider Simulator"));
        assert_eq!(r.guider.as_deref(), Some("CCD Imager Simulator"));
    }
}
