//! The Imaging tab's one-shot capture on an INDIGO camera: Preview and Loop.
//!
//! Ekos' Capture module keeps the one-shot settings (`capture_get_all_settings`
//! → `captureExposureN`, `captureTypeS`, …); here they live in `Settings`.
//! An exposure is driven straight on the camera driver — `CCD_UPLOAD_MODE`
//! LOCAL into the captures dir (so the Files tab lists the frames),
//! `CCD_PREVIEW` ENABLED so the driver also publishes a stretched JPEG
//! (`CCD_PREVIEW_IMAGE`), which becomes the browser's `new_preview_image`.
//! The capture queue (`capture_start`) is not covered: that is the Imager
//! agent's job, a later phase.

use serde_json::{json, Value};

use super::commands::reply_msg;
use super::protocol::{new_vector_msg, Change, Kind, PropertyStore, State};

/// Ekos' one-shot form values.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub exposure: f64,
    /// "Light" | "Bias" | "Dark" | "Flat" (Ekos' `captureTypeS`).
    pub frame_type: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { exposure: 1.0, frame_type: "Light".into() }
    }
}

#[derive(Default)]
pub struct Imaging {
    pub settings: Settings,
    /// Re-arm after each image until `capture_stop`.
    looping: bool,
    /// An exposure we started is in flight: the next preview is ours.
    armed: bool,
    last_exposure_state: Option<State>,
    previews: u64,
}

/// What a camera property change asks the session to do.
#[derive(Default)]
pub struct Reaction {
    pub to_browser: Vec<String>,
    pub to_indigo: Vec<String>,
    /// A preview JPEG to fetch (BLOB link as INDIGO gave it) and forward.
    pub fetch_preview: Option<String>,
}

impl Imaging {
    /// `capture_get_all_settings`: the one-shot values, where frames go, and
    /// the sequence limits the browser's editor expects to find
    /// (`sequence_editor/limits.rs::from_capture_settings` needs
    /// `enforceRefocusEveryN` to exist at all).
    pub fn settings_payload(&self, store: &PropertyStore, camera: Option<&str>, wheel: Option<&str>, captures_dir: &str) -> Value {
        let mut p = json!({
            "captureExposureN": self.settings.exposure,
            "captureTypeS": self.settings.frame_type,
            "captureISOS": "",
            "fileDirectoryT": captures_dir,
            "enforceRefocusEveryN": false,
            "refocusEveryN": 60,
            "enforceAutofocusOnTemperature": false,
            "maxFocusTemperatureDelta": 1.0,
            "refocusAfterMeridianFlip": false,
            "enforceAutofocusHFR": false,
            "hFRCheckAlgorithm": "Last Autofocus",
            "hFRThresholdPercentage": 10.0,
            "inSequenceCheckFrames": 1,
            "hFRDeviation": 0.1,
            "enforceGuideDeviation": false,
            "guideDeviation": 2.0,
            "enforceStartGuiderDrift": false,
            "startGuideDeviation": 2.0,
        });
        if let Some(gain) = camera.and_then(|c| store.get(c, "CCD_GAIN")).and_then(|g| g.number("GAIN")) {
            p["captureGainN"] = json!(gain);
        }
        if let Some(name) = wheel.and_then(|w| current_filter(store, w)) {
            p["FilterPosCombo"] = json!(name);
        }
        p
    }

    /// `capture_set_all_settings {key: value}` — one key at a time, as the
    /// Imaging tab sends them. Returns the INDIGO writes it implies.
    pub fn apply_setting(&mut self, key: &str, value: &Value, store: &PropertyStore, camera: Option<&str>, wheel: Option<&str>) -> Vec<String> {
        match key {
            "captureExposureN" => {
                if let Some(v) = value.as_f64().filter(|v| *v > 0.0) {
                    self.settings.exposure = v;
                }
                Vec::new()
            }
            "captureTypeS" => {
                if let Some(t) = value.as_str() {
                    self.settings.frame_type = t.to_string();
                }
                Vec::new()
            }
            "captureGainN" => match (camera, value.as_f64()) {
                (Some(c), Some(g)) if store.get(c, "CCD_GAIN").is_some() => {
                    vec![new_vector_msg(Kind::Number, c, "CCD_GAIN", &[("GAIN".into(), json!(g))])]
                }
                _ => Vec::new(),
            },
            "FilterPosCombo" => match (wheel, value.as_str()) {
                (Some(w), Some(name)) => filter_slot(store, w, name)
                    .map(|slot| vec![new_vector_msg(Kind::Number, w, "WHEEL_SLOT", &[("SLOT".into(), json!(slot))])])
                    .unwrap_or_default(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// `capture_preview` / `capture_loop`: start one exposure, with the
    /// driver set up to save the frame and publish a JPEG of it.
    pub fn start(&mut self, looping: bool, camera: &str, store: &PropertyStore, captures_dir: &str) -> Vec<String> {
        self.looping = looping;
        self.armed = true;
        let mut out = Vec::new();
        let switch = |name: &str, item: &str| new_vector_msg(Kind::Switch, camera, name, &[(item.into(), json!(true))]);
        if store.get(camera, "CCD_UPLOAD_MODE").is_some_and(|p| !p.on("LOCAL")) {
            out.push(switch("CCD_UPLOAD_MODE", "LOCAL"));
        }
        if store.get(camera, "CCD_LOCAL_MODE").is_some_and(|p| p.text("DIR") != Some(captures_dir)) {
            out.push(new_vector_msg(Kind::Text, camera, "CCD_LOCAL_MODE", &[("DIR".into(), json!(captures_dir))]));
        }
        if store.get(camera, "CCD_PREVIEW").is_some_and(|p| !p.on("ENABLED")) {
            out.push(switch("CCD_PREVIEW", "ENABLED"));
        }
        let frame = frame_type_item(&self.settings.frame_type);
        if store.get(camera, "CCD_FRAME_TYPE").is_some_and(|p| !p.on(frame)) {
            out.push(switch("CCD_FRAME_TYPE", frame));
        }
        out.push(self.expose(camera));
        out
    }

    pub fn stop(&mut self, camera: &str) -> Vec<String> {
        self.looping = false;
        self.armed = false;
        vec![new_vector_msg(Kind::Switch, camera, "CCD_ABORT_EXPOSURE", &[("ABORT_EXPOSURE".into(), json!(true))])]
    }

    fn expose(&self, camera: &str) -> String {
        new_vector_msg(Kind::Number, camera, "CCD_EXPOSURE", &[("EXPOSURE".into(), json!(self.settings.exposure))])
    }

    /// React to a change on the camera: exposure progress → `new_capture_state`,
    /// temperature → `new_camera_state`, a fresh preview → fetch it.
    pub fn on_change(&mut self, change: &Change, store: &PropertyStore, camera: &str) -> Reaction {
        let mut r = Reaction::default();
        let name = match change {
            Change::Defined { device, name } | Change::Updated { device, name } if device == camera => name.as_str(),
            _ => return r,
        };
        match name {
            "CCD_TEMPERATURE" => {
                if let Some(t) = store.get(camera, name).and_then(|p| p.number("TEMPERATURE")) {
                    r.to_browser.push(reply_msg("new_camera_state", json!({ "name": camera, "temperature": t })));
                }
            }
            "CCD_EXPOSURE" => {
                let Some(p) = store.get(camera, name) else { return r };
                let was = self.last_exposure_state.replace(p.state);
                let status = match p.state {
                    State::Busy => "Capturing",
                    State::Ok if was == Some(State::Busy) => "Image Received",
                    State::Ok => "Idle",
                    State::Alert => "Aborted",
                    State::Idle => "Idle",
                };
                let mut payload = json!({
                    "status": status,
                    "expr": self.settings.exposure,
                    "seqv": 0,
                    "seqr": 0,
                    "ovp": 0.0,
                    "log": "",
                });
                if p.state == State::Busy {
                    payload["expv"] = json!(p.number("EXPOSURE").unwrap_or(0.0));
                } else {
                    payload["expv"] = json!(0.0);
                }
                r.to_browser.push(reply_msg("new_capture_state", payload));
                if p.state == State::Alert {
                    self.armed = false;
                    self.looping = false;
                }
            }
            "CCD_PREVIEW_IMAGE" => {
                let Some(p) = store.get(camera, name) else { return r };
                if p.state == State::Ok && self.armed {
                    if let Some(url) = p.item("IMAGE").and_then(|i| i.blob_url()) {
                        self.previews += 1;
                        r.fetch_preview = Some(url.to_string());
                        if self.looping {
                            r.to_indigo.push(self.expose(camera));
                        } else {
                            self.armed = false;
                        }
                    }
                }
            }
            _ => {}
        }
        r
    }

    /// The `new_preview_image` for a fetched JPEG, in the shape
    /// `kstars_ws.rs::decode_media_frame` produces (inline base64, KStars'
    /// media metadata). No raw statistics: the browser's histogram works on
    /// the display axis only.
    pub fn preview_msg(&self, jpeg: &[u8], (w, h): (u32, u32)) -> String {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        reply_msg(
            "new_preview_image",
            json!({
                "metadata": {
                    "uuid": format!("+P{}", self.previews),
                    "ext": "jpg",
                    "resolution": format!("{w}x{h}"),
                    "bpp": "8",
                },
                "data": BASE64.encode(jpeg),
            }),
        )
    }
}

fn frame_type_item(ekos: &str) -> &'static str {
    match ekos {
        "Bias" => "BIAS",
        "Dark" => "DARK",
        "Flat" => "FLAT",
        _ => "LIGHT",
    }
}

/// The wheel's current filter name (`WHEEL_SLOT.SLOT` → `WHEEL_SLOT_NAME.SLOT_NAME_n`).
pub fn current_filter(store: &PropertyStore, wheel: &str) -> Option<String> {
    let slot = store.get(wheel, "WHEEL_SLOT")?.number("SLOT")? as i64;
    store
        .get(wheel, "WHEEL_SLOT_NAME")?
        .text(&format!("SLOT_NAME_{slot}"))
        .map(str::to_string)
}

fn filter_slot(store: &PropertyStore, wheel: &str, name: &str) -> Option<i64> {
    store
        .get(wheel, "WHEEL_SLOT_NAME")?
        .items
        .iter()
        .find(|i| i.text() == Some(name))
        .and_then(|i| i.name.strip_prefix("SLOT_NAME_")?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    fn store() -> PropertyStore {
        let mut s = PropertyStore::default();
        for f in [
            r#"{"defNumberVector":{"device":"Cam","name":"CCD_EXPOSURE","perm":"rw","state":"Idle","items":[{"name":"EXPOSURE","value":0}]}}"#,
            r#"{"defSwitchVector":{"device":"Cam","name":"CCD_UPLOAD_MODE","perm":"rw","state":"Ok","items":[{"name":"CLIENT","value":true},{"name":"LOCAL","value":false}]}}"#,
            r#"{"defTextVector":{"device":"Cam","name":"CCD_LOCAL_MODE","perm":"rw","state":"Ok","items":[{"name":"DIR","value":"/home/x/indigo_image_cache"}]}}"#,
            r#"{"defSwitchVector":{"device":"Cam","name":"CCD_PREVIEW","perm":"rw","state":"Ok","items":[{"name":"DISABLED","value":true},{"name":"ENABLED","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Cam","name":"CCD_FRAME_TYPE","perm":"rw","state":"Ok","items":[{"name":"LIGHT","value":true},{"name":"DARK","value":false}]}}"#,
            r#"{"defBLOBVector":{"device":"Cam","name":"CCD_PREVIEW_IMAGE","perm":"ro","state":"Idle","items":[{"name":"IMAGE"}]}}"#,
            r#"{"defNumberVector":{"device":"Cam","name":"CCD_GAIN","perm":"rw","state":"Ok","items":[{"name":"GAIN","value":120}]}}"#,
            r#"{"defNumberVector":{"device":"Wheel","name":"WHEEL_SLOT","perm":"rw","state":"Ok","items":[{"name":"SLOT","value":2}]}}"#,
            r#"{"defTextVector":{"device":"Wheel","name":"WHEEL_SLOT_NAME","perm":"rw","state":"Ok","items":[{"name":"SLOT_NAME_1","value":"L"},{"name":"SLOT_NAME_2","value":"Ha"}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        s
    }

    fn kinds(msgs: &[String]) -> Vec<String> {
        msgs.iter()
            .map(|m| {
                let v: Value = serde_json::from_str(m).unwrap();
                let (k, b) = v.as_object().unwrap().iter().next().unwrap();
                format!("{k}:{}", b["name"].as_str().unwrap_or(""))
            })
            .collect()
    }

    #[test]
    fn start_prepares_the_driver_once() {
        let mut s = store();
        let mut im = Imaging::default();
        im.settings.frame_type = "Dark".into();
        let out = im.start(false, "Cam", &s, "/caps");
        assert_eq!(
            kinds(&out),
            ["newSwitchVector:CCD_UPLOAD_MODE", "newTextVector:CCD_LOCAL_MODE", "newSwitchVector:CCD_PREVIEW", "newSwitchVector:CCD_FRAME_TYPE", "newNumberVector:CCD_EXPOSURE"]
        );
        // Once the driver reports the set-up, only the exposure goes out.
        for f in [
            r#"{"setSwitchVector":{"device":"Cam","name":"CCD_UPLOAD_MODE","state":"Ok","items":[{"name":"CLIENT","value":false},{"name":"LOCAL","value":true}]}}"#,
            r#"{"setTextVector":{"device":"Cam","name":"CCD_LOCAL_MODE","state":"Ok","items":[{"name":"DIR","value":"/caps"}]}}"#,
            r#"{"setSwitchVector":{"device":"Cam","name":"CCD_PREVIEW","state":"Ok","items":[{"name":"DISABLED","value":false},{"name":"ENABLED","value":true}]}}"#,
            r#"{"setSwitchVector":{"device":"Cam","name":"CCD_FRAME_TYPE","state":"Ok","items":[{"name":"LIGHT","value":false},{"name":"DARK","value":true}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        assert_eq!(kinds(&im.start(false, "Cam", &s, "/caps")), ["newNumberVector:CCD_EXPOSURE"]);
    }

    #[test]
    fn exposure_states_and_preview_loop() {
        let mut s = store();
        let mut im = Imaging::default();
        im.settings.exposure = 2.0;
        im.start(true, "Cam", &s, "/caps");

        s.apply(parse_frame(r#"{"setNumberVector":{"device":"Cam","name":"CCD_EXPOSURE","state":"Busy","items":[{"name":"EXPOSURE","value":1.5}]}}"#).unwrap());
        let r = im.on_change(&Change::Updated { device: "Cam".into(), name: "CCD_EXPOSURE".into() }, &s, "Cam");
        let v: Value = serde_json::from_str(&r.to_browser[0]).unwrap();
        assert_eq!(v["type"], "new_capture_state");
        assert_eq!(v["payload"]["status"], "Capturing");
        assert_eq!(v["payload"]["expv"], 1.5);
        assert_eq!(v["payload"]["expr"], 2.0);

        s.apply(parse_frame(r#"{"setNumberVector":{"device":"Cam","name":"CCD_EXPOSURE","state":"Ok","items":[{"name":"EXPOSURE","value":0}]}}"#).unwrap());
        let r = im.on_change(&Change::Updated { device: "Cam".into(), name: "CCD_EXPOSURE".into() }, &s, "Cam");
        let v: Value = serde_json::from_str(&r.to_browser[0]).unwrap();
        assert_eq!(v["payload"]["status"], "Image Received");

        // The preview lands: fetched, and the loop re-arms the exposure.
        s.apply(parse_frame(r#"{"setBLOBVector":{"device":"Cam","name":"CCD_PREVIEW_IMAGE","state":"Ok","items":[{"name":"IMAGE","value":"/blob/0x1.jpeg"}]}}"#).unwrap());
        let r = im.on_change(&Change::Updated { device: "Cam".into(), name: "CCD_PREVIEW_IMAGE".into() }, &s, "Cam");
        assert_eq!(r.fetch_preview.as_deref(), Some("/blob/0x1.jpeg"));
        assert_eq!(kinds(&r.to_indigo), ["newNumberVector:CCD_EXPOSURE"]);

        // Stop: abort, and the next preview is nobody's.
        assert_eq!(kinds(&im.stop("Cam")), ["newSwitchVector:CCD_ABORT_EXPOSURE"]);
        let r = im.on_change(&Change::Updated { device: "Cam".into(), name: "CCD_PREVIEW_IMAGE".into() }, &s, "Cam");
        assert!(r.fetch_preview.is_none() && r.to_indigo.is_empty());

        let msg = im.preview_msg(b"\xff\xd8", (640, 480));
        let v: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["payload"]["metadata"]["resolution"], "640x480");
        assert_eq!(v["payload"]["metadata"]["ext"], "jpg");
        assert_eq!(v["payload"]["data"], "/9g=");
    }

    #[test]
    fn settings_round_trip() {
        let s = store();
        let mut im = Imaging::default();
        let p = im.settings_payload(&s, Some("Cam"), Some("Wheel"), "/caps");
        assert_eq!(p["captureExposureN"], 1.0);
        assert_eq!(p["captureGainN"], 120.0);
        assert_eq!(p["FilterPosCombo"], "Ha");
        assert_eq!(p["fileDirectoryT"], "/caps");
        assert_eq!(p["enforceRefocusEveryN"], false);
        assert!(im.apply_setting("captureExposureN", &json!(5.0), &s, Some("Cam"), Some("Wheel")).is_empty());
        assert_eq!(im.settings.exposure, 5.0);
        let w = im.apply_setting("FilterPosCombo", &json!("L"), &s, Some("Cam"), Some("Wheel"));
        assert_eq!(kinds(&w), ["newNumberVector:WHEEL_SLOT"]);
        assert!(w[0].contains("\"value\":1"));
        let g = im.apply_setting("captureGainN", &json!(50), &s, Some("Cam"), Some("Wheel"));
        assert_eq!(kinds(&g), ["newNumberVector:CCD_GAIN"]);
    }
}
