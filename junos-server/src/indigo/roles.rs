//! Which INDIGO device plays which Ekos role, and the optical train / scope
//! replies that tell the browser so.
//!
//! Ekos keys everything on optical trains (`train_get_all`) and the browser
//! binds each module to its train's device (`ws/mod.rs`). INDIGO has no such
//! thing: the agents pick devices with `FILTER_*_LIST` switches. Until that is
//! wired, one synthetic train is derived from the `INFO.DEVICE_INTERFACE`
//! bits — first mount, first camera, …

use serde_json::{json, Value};

use super::protocol::{interface, PropertyStore};

pub const TRAIN_NAME: &str = "INDIGO";
pub const TRAIN_ID: i64 = 1;
pub const SCOPE_NAME: &str = "INDIGO lens";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roles {
    pub mount: Option<String>,
    pub camera: Option<String>,
    pub guider: Option<String>,
    pub focuser: Option<String>,
    pub wheel: Option<String>,
}

impl Roles {
    pub fn derive(store: &PropertyStore) -> Self {
        let devices: Vec<(&str, u32)> = store
            .device_names()
            .map(|d| (d, store.interface(d)))
            .filter(|(_, i)| i & interface::AGENT == 0)
            .collect();
        let first = |bit: u32| {
            devices
                .iter()
                .find(|(_, i)| i & bit != 0)
                .map(|(d, _)| d.to_string())
        };
        let mount = first(interface::MOUNT);
        let camera = first(interface::CCD);
        // The guide camera: a second CCD, else a CCD that also guides.
        let guider = devices
            .iter()
            .filter(|(_, i)| i & interface::CCD != 0)
            .map(|(d, _)| d.to_string())
            .find(|d| Some(d) != camera.as_ref())
            .or_else(|| {
                devices
                    .iter()
                    .find(|(_, i)| i & interface::GUIDER != 0 && i & interface::CCD != 0)
                    .map(|(d, _)| d.to_string())
            });
        Self {
            mount,
            camera,
            guider,
            focuser: first(interface::FOCUSER),
            wheel: first(interface::WHEEL),
        }
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
        let r = Roles::derive(&store);
        assert_eq!(r.mount.as_deref(), Some("Mount"));
        assert_eq!(r.camera.as_deref(), Some("Main Cam"));
        assert_eq!(r.guider.as_deref(), Some("Guide Cam"));
        assert_eq!(r.wheel.as_deref(), Some("Wheel"));
        assert_eq!(r.focuser, None);
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
}
