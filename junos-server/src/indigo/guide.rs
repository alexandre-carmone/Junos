//! The Guide tab on the Guider Agent: calibration and guiding
//! (`AGENT_START_PROCESS`), framing (`PREVIEW` / `PREVIEW_1`), the drift and
//! RMS from `AGENT_GUIDER_STATS`, and the settings the sheet can honestly
//! map onto `AGENT_GUIDER_SETTINGS` and its switches.
//!
//! KStars' guide statuses (`ekos.h::GuideState`) come out of three things:
//! which process item runs, `AGENT_GUIDER_STATS.PHASE` (−1 previewing, 0
//! guiding, 1–7 the calibration moves, 8 failed, 9 done) and
//! `AGENT_GUIDER_STATS.DITHERING` (non-zero while a dither settles).

use serde_json::{json, Value};

use super::agents::AgentSet;
use super::commands::reply_msg;
use super::focus::{binning_string, binning_write};
use super::process::{Edge, Log, Process, Reaction};
use super::protocol::{new_vector_msg, Change, Kind, PropertyStore, State};

const PHASE_GUIDING: f64 = 0.0;

#[derive(Default)]
pub struct Guide {
    log: Log,
    process: Process,
    abort_requested: bool,
    status: String,
    last_frame: f64,
    /// Did the current process reach the guiding phase?
    guided: bool,
    /// Ekos' "reuse calibration" (local): guide without recalibrating when
    /// the agent still holds a calibration.
    reuse_calibration: bool,
}

fn switch(device: &str, name: &str, item: &str) -> String {
    new_vector_msg(Kind::Switch, device, name, &[(item.to_string(), json!(true))])
}

fn number(device: &str, name: &str, items: &[(&str, f64)]) -> String {
    let items: Vec<(String, Value)> = items.iter().map(|(n, v)| (n.to_string(), json!(v))).collect();
    new_vector_msg(Kind::Number, device, name, &items)
}

impl Guide {
    pub fn new() -> Self {
        Self { reuse_calibration: true, ..Self::default() }
    }

    fn state_msg(&self, mut payload: Value) -> String {
        payload["log"] = json!(self.log.text());
        reply_msg("new_guide_state", payload)
    }

    fn phase(store: &PropertyStore, agent: &str) -> f64 {
        store.get(agent, "AGENT_GUIDER_STATS").and_then(|p| p.number("PHASE")).unwrap_or(9.0)
    }

    fn dithering(store: &PropertyStore, agent: &str) -> bool {
        store.get(agent, "AGENT_GUIDER_STATS").and_then(|p| p.number("DITHERING")).unwrap_or(0.0) != 0.0
    }

    /// The status while the process runs.
    fn running_status(&self, store: &PropertyStore, agent: &str) -> &'static str {
        match self.process.running.as_deref().unwrap_or("") {
            "PREVIEW_1" => "Capturing",
            "PREVIEW" => "Looping",
            "GUIDING" => {
                if Self::dithering(store, agent) { "Dithering" } else { "Guiding" }
            }
            "CALIBRATION" => "Calibrating",
            "CALIBRATION_AND_GUIDING" => {
                if Self::phase(store, agent) == PHASE_GUIDING {
                    if Self::dithering(store, agent) { "Dithering" } else { "Guiding" }
                } else {
                    "Calibrating"
                }
            }
            _ => "Idle",
        }
    }

    fn push_status(&mut self, status: &str, r: &mut Reaction) {
        if status != self.status {
            self.status = status.to_string();
            r.to_browser.push(self.state_msg(json!({ "status": status })));
        }
    }

    /// React to a change on the Guider Agent.
    pub fn on_change(&mut self, change: &Change, store: &PropertyStore, agent: &str) -> Reaction {
        let mut r = Reaction::default();
        let name = match change {
            Change::Defined { device, name } | Change::Updated { device, name } if device == agent => name.as_str(),
            Change::Message { device: Some(d), text } if d == agent => {
                if self.log.push(text) {
                    r.to_browser.push(self.state_msg(json!({})));
                }
                return r;
            }
            Change::Deleted { device, name: None } if device == agent => {
                self.process = Process::default();
                self.push_status("Idle", &mut r);
                return r;
            }
            _ => return r,
        };
        let Some(p) = store.get(agent, name) else { return r };
        let noted = p.message.as_deref().is_some_and(|m| self.log.push(m));
        match name {
            "AGENT_START_PROCESS" => match self.process.observe(p) {
                Edge::Started(item) => {
                    if !matches!(item.as_str(), "PREVIEW" | "PREVIEW_1" | "CALIBRATION" | "CALIBRATION_AND_GUIDING" | "GUIDING") {
                        return r;
                    }
                    if item != "GUIDING" || !self.guided {
                        self.abort_requested = false;
                        self.guided = false;
                        self.last_frame = 0.0;
                    }
                    let status = self.running_status(store, agent);
                    self.push_status(status, &mut r);
                }
                Edge::Finished { ok, item } => {
                    let aborted = self.abort_requested
                        || self.log.latest().is_some_and(|l| l.to_ascii_lowercase().contains("abort"));
                    let status = match (item.as_str(), ok) {
                        // CALIBRATION_AND_GUIDING ends Ok after the calibration
                        // and starts again as GUIDING.
                        ("CALIBRATION", true) => "Calibrated",
                        ("CALIBRATION_AND_GUIDING", true) if !self.guided => "Calibrated",
                        ("CALIBRATION", false) | ("CALIBRATION_AND_GUIDING", false) if !self.guided => {
                            if aborted { "Aborted" } else { "Calibration error" }
                        }
                        ("GUIDING", _) | ("CALIBRATION_AND_GUIDING", _) => {
                            if aborted || !ok { "Aborted" } else { "Idle" }
                        }
                        _ => "Idle",
                    };
                    self.abort_requested = false;
                    self.guided = false;
                    self.push_status(status, &mut r);
                }
                Edge::Still if noted => r.to_browser.push(self.state_msg(json!({}))),
                Edge::Still => {}
            },
            "AGENT_GUIDER_STATS" => {
                if !self.process.is_running() {
                    return r;
                }
                let guiding = Self::phase(store, agent) == PHASE_GUIDING;
                if guiding {
                    self.guided = true;
                }
                let status = self.running_status(store, agent);
                self.push_status(status, &mut r);
                let frame = p.number("FRAME").unwrap_or(0.0);
                if guiding && frame > self.last_frame {
                    self.last_frame = frame;
                    let n = |k: &str| p.number(k).unwrap_or(0.0);
                    r.to_browser.push(self.state_msg(json!({
                        "drift_ra": n("DRIFT_RA_S"),
                        "drift_de": n("DRIFT_DEC_S"),
                        "rarms": n("RMSE_RA_S"),
                        "derms": n("RMSE_DEC_S"),
                    })));
                }
            }
            "CCD_PREVIEW_IMAGE" => {
                if p.state == State::Ok && self.process.is_running() {
                    if let Some(url) = p.item("IMAGE").and_then(|i| i.blob_url()) {
                        r.fetch_preview = Some(url.to_string());
                    }
                }
            }
            _ if noted => r.to_browser.push(self.state_msg(json!({}))),
            _ => {}
        }
        r
    }

    /// Whether the agent holds a calibration (`SPEED_RA` is its "calibrated" test).
    fn calibrated(store: &PropertyStore, agent: &str) -> bool {
        store
            .get(agent, "AGENT_GUIDER_SETTINGS")
            .and_then(|p| p.number("SPEED_RA"))
            .is_some_and(|v| v != 0.0)
    }

    fn preview_on(store: &PropertyStore, agent: &str) -> Option<String> {
        store
            .get(agent, "CCD_PREVIEW")
            .filter(|p| !p.on("ENABLED") && !p.on("ENABLED_WITH_HISTOGRAM"))
            .map(|_| switch(agent, "CCD_PREVIEW", "ENABLED"))
    }

    /// `guide_start`: guide with the held calibration, else calibrate first.
    /// `None` when the agent is busy.
    pub fn start(&mut self, store: &PropertyStore, agent: &str) -> Option<Vec<String>> {
        if AgentSet::busy(store, agent) {
            return None;
        }
        self.abort_requested = false;
        let mut out: Vec<String> = Self::preview_on(store, agent).into_iter().collect();
        let item = if self.reuse_calibration && Self::calibrated(store, agent) { "GUIDING" } else { "CALIBRATION_AND_GUIDING" };
        out.push(switch(agent, "AGENT_START_PROCESS", item));
        Some(out)
    }

    /// `guide_capture` (one frame) / `guide_loop`.
    pub fn preview(&mut self, store: &PropertyStore, agent: &str, looping: bool) -> Option<Vec<String>> {
        if AgentSet::busy(store, agent) {
            return None;
        }
        let mut out: Vec<String> = Self::preview_on(store, agent).into_iter().collect();
        out.push(switch(agent, "AGENT_START_PROCESS", if looping { "PREVIEW" } else { "PREVIEW_1" }));
        Some(out)
    }

    /// `guide_stop`.
    pub fn stop(&mut self, agent: &str) -> Vec<String> {
        self.abort_requested = true;
        vec![switch(agent, "AGENT_ABORT_PROCESS", "ABORT")]
    }

    /// `guide_clear`: a partial write only touches the named items, and a
    /// zero `SPEED_RA` is what the agent calls "not calibrated".
    pub fn clear_calibration(&self, store: &PropertyStore, agent: &str) -> Vec<String> {
        if store.get(agent, "AGENT_GUIDER_SETTINGS").is_none() || AgentSet::busy(store, agent) {
            return Vec::new();
        }
        vec![number(agent, "AGENT_GUIDER_SETTINGS", &[("SPEED_RA", 0.0), ("SPEED_DEC", 0.0), ("ANGLE", 0.0)])]
    }

    /// `guide_set_calibration_settings {pulse, …}`: the calibration step is
    /// the one field with a counterpart (`STEP0`, seconds; 0 = automatic).
    pub fn apply_calibration(&mut self, payload: &Value, store: &PropertyStore, agent: &str) -> Vec<String> {
        if let Some(reuse) = payload.get("reuseCalibration").and_then(Value::as_bool) {
            self.reuse_calibration = reuse;
        }
        match (payload.get("pulse").and_then(Value::as_f64), store.get(agent, "AGENT_GUIDER_SETTINGS")) {
            (Some(ms), Some(_)) if ms >= 0.0 => vec![number(agent, "AGENT_GUIDER_SETTINGS", &[("STEP0", ms / 1000.0)])],
            _ => Vec::new(),
        }
    }

    /// `guide_get_all_settings`: the keys the sheet can honestly show.
    pub fn settings_payload(&self, store: &PropertyStore, agent: &str) -> Value {
        let mut p = json!({ "kcfg_ReuseGuideCalibration": self.reuse_calibration });
        if let Some(s) = store.get(agent, "AGENT_GUIDER_SETTINGS") {
            for (key, item) in [
                ("guideExposure", "EXPOSURE"),
                ("guideDelay", "DELAY"),
                ("guiderAccuracyThreshold", "MIN_ERROR"),
            ] {
                if let Some(v) = s.number(item) {
                    p[key] = json!(v);
                }
            }
            if let Some(v) = s.number("STEP0") {
                p["kcfg_CalibrationPulseDuration"] = json!((v * 1000.0).round());
            }
        }
        if let Some(b) = binning_string(store, agent) {
            p["guideBinning"] = json!(b);
        }
        if let Some(g) = store.get(agent, "CCD_GAIN").and_then(|g| g.number("GAIN")) {
            p["guideGain"] = json!(g);
        }
        if let Some(sel) = store.get(agent, "AGENT_GUIDER_SELECTION") {
            if let Some(r) = sel.number("RADIUS") {
                p["guideSquareSize"] = json!(2.0 * r + 1.0);
            }
            if let Some(sf) = sel.number("SUBFRAME") {
                p["guideSubframe"] = json!(sf > 0.0);
            }
        }
        if let Some(m) = store.get(agent, "AGENT_GUIDER_DEC_MODE").and_then(|m| m.selected()) {
            p["dECGuideEnabled"] = json!(m != "NONE");
            p["northDECGuideEnabled"] = json!(m == "BOTH" || m == "NORTH");
            p["southDECGuideEnabled"] = json!(m == "BOTH" || m == "SOUTH");
        }
        if let Some(b) = store.get(agent, "AGENT_GUIDER_APPLY_DEC_BACKLASH") {
            p["kcfg_GuideCalibrationBacklash"] = json!(b.on("ENABLED"));
        }
        if let Some(f) = store.get(agent, "AGENT_GUIDER_FLIP_REVERSES_DEC") {
            p["kcfg_ReverseDecOnPierSideChange"] = json!(f.on("ENABLED"));
        }
        p
    }

    /// `guide_set_all_settings {key: value}` → the INDIGO writes.
    pub fn apply_setting(&mut self, key: &str, value: &Value, store: &PropertyStore, agent: &str) -> Vec<String> {
        let num = value.as_f64();
        let on = value.as_bool();
        let settings = |item: &str, v: f64| vec![number(agent, "AGENT_GUIDER_SETTINGS", &[(item, v)])];
        let dec_mode = |north: bool, south: bool| {
            let item = match (north, south) {
                (true, true) => "BOTH",
                (true, false) => "NORTH",
                (false, true) => "SOUTH",
                (false, false) => "NONE",
            };
            vec![switch(agent, "AGENT_GUIDER_DEC_MODE", item)]
        };
        let current_dec = |which: &str| {
            store
                .get(agent, "AGENT_GUIDER_DEC_MODE")
                .and_then(|m| m.selected())
                .is_some_and(|m| m == "BOTH" || m == which)
        };
        match (key, num, on) {
            ("guideExposure", Some(v), _) if v > 0.0 => settings("EXPOSURE", v),
            ("guideDelay", Some(v), _) if v >= 0.0 => settings("DELAY", v),
            ("guiderAccuracyThreshold", Some(v), _) if v >= 0.0 => settings("MIN_ERROR", v),
            ("kcfg_CalibrationPulseDuration", Some(v), _) if v >= 0.0 => settings("STEP0", v / 1000.0),
            ("guideBinning", _, _) => binning_write(store, agent, value).into_iter().collect(),
            ("guideGain", Some(v), _) => vec![number(agent, "CCD_GAIN", &[("GAIN", v)])],
            ("guideSquareSize", Some(v), _) if v >= 3.0 => {
                vec![number(agent, "AGENT_GUIDER_SELECTION", &[("RADIUS", ((v - 1.0) / 2.0).round())])]
            }
            ("guideSubframe", _, Some(b)) => vec![number(agent, "AGENT_GUIDER_SELECTION", &[("SUBFRAME", if b { 10.0 } else { 0.0 })])],
            ("dECGuideEnabled", _, Some(b)) => dec_mode(b, b),
            ("northDECGuideEnabled", _, Some(b)) => dec_mode(b, current_dec("SOUTH")),
            ("southDECGuideEnabled", _, Some(b)) => dec_mode(current_dec("NORTH"), b),
            ("kcfg_GuideCalibrationBacklash", _, Some(b)) => {
                vec![switch(agent, "AGENT_GUIDER_APPLY_DEC_BACKLASH", if b { "ENABLED" } else { "DISABLED" })]
            }
            ("kcfg_ReverseDecOnPierSideChange", _, Some(b)) => {
                vec![switch(agent, "AGENT_GUIDER_FLIP_REVERSES_DEC", if b { "ENABLED" } else { "DISABLED" })]
            }
            ("kcfg_ReuseGuideCalibration", _, Some(b)) => {
                self.reuse_calibration = b;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    const A: &str = "Guider Agent";

    fn store() -> PropertyStore {
        let mut s = PropertyStore::default();
        for f in [
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"AGENT_START_PROCESS","perm":"rw","state":"Ok","rule":"AtMostOne","items":[{"name":"PREVIEW_1","value":false},{"name":"PREVIEW","value":false},{"name":"CALIBRATION","value":false},{"name":"CALIBRATION_AND_GUIDING","value":false},{"name":"GUIDING","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"AGENT_ABORT_PROCESS","perm":"rw","state":"Ok","items":[{"name":"ABORT","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Guider Agent","name":"AGENT_GUIDER_SETTINGS","perm":"rw","state":"Ok","items":[{"name":"EXPOSURE","value":1},{"name":"DELAY","value":0},{"name":"STEP0","value":0},{"name":"SPEED_RA","value":0},{"name":"SPEED_DEC","value":0},{"name":"ANGLE","value":0},{"name":"MIN_ERROR","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Guider Agent","name":"AGENT_GUIDER_STATS","perm":"ro","state":"Ok","items":[{"name":"PHASE","value":9},{"name":"FRAME","value":0},{"name":"DRIFT_RA_S","value":0},{"name":"DRIFT_DEC_S","value":0},{"name":"RMSE_RA_S","value":0},{"name":"RMSE_DEC_S","value":0},{"name":"DITHERING","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Guider Agent","name":"AGENT_GUIDER_SELECTION","perm":"rw","state":"Ok","items":[{"name":"RADIUS","value":8},{"name":"SUBFRAME","value":0}]}}"#,
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"AGENT_GUIDER_DEC_MODE","perm":"rw","state":"Ok","items":[{"name":"BOTH","value":true},{"name":"NORTH","value":false},{"name":"SOUTH","value":false},{"name":"NONE","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"AGENT_GUIDER_APPLY_DEC_BACKLASH","perm":"rw","state":"Ok","items":[{"name":"DISABLED","value":true},{"name":"ENABLED","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"AGENT_GUIDER_FLIP_REVERSES_DEC","perm":"rw","state":"Ok","items":[{"name":"ENABLED","value":true},{"name":"DISABLED","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Guider Agent","name":"CCD_BIN","perm":"rw","state":"Ok","items":[{"name":"HORIZONTAL","value":1},{"name":"VERTICAL","value":1}]}}"#,
            r#"{"defNumberVector":{"device":"Guider Agent","name":"CCD_GAIN","perm":"rw","state":"Ok","items":[{"name":"GAIN","value":100}]}}"#,
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"CCD_PREVIEW","perm":"rw","state":"Ok","items":[{"name":"DISABLED","value":false},{"name":"ENABLED","value":true}]}}"#,
            r#"{"defBLOBVector":{"device":"Guider Agent","name":"CCD_PREVIEW_IMAGE","perm":"ro","state":"Idle","items":[{"name":"IMAGE"}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        s
    }

    fn set(s: &mut PropertyStore, frame: &str) -> Change {
        s.apply(parse_frame(frame).unwrap()).unwrap()
    }

    /// INDIGO sends every item of a switch on each update.
    fn process(s: &mut PropertyStore, state: &str, item: &str, message: Option<&str>) -> Change {
        let msg = message.map(|m| format!(r#""message":"{m}","#)).unwrap_or_default();
        let items = ["PREVIEW_1", "PREVIEW", "CALIBRATION", "CALIBRATION_AND_GUIDING", "GUIDING"]
            .iter()
            .map(|i| format!(r#"{{"name":"{i}","value":{}}}"#, *i == item))
            .collect::<Vec<_>>()
            .join(",");
        set(s, &format!(r#"{{"setSwitchVector":{{"device":"Guider Agent","name":"AGENT_START_PROCESS","state":"{state}",{msg}"items":[{items}]}}}}"#))
    }

    fn stats(s: &mut PropertyStore, phase: f64, frame: f64, dithering: f64) -> Change {
        set(s, &format!(r#"{{"setNumberVector":{{"device":"Guider Agent","name":"AGENT_GUIDER_STATS","state":"Busy","items":[{{"name":"PHASE","value":{phase}}},{{"name":"FRAME","value":{frame}}},{{"name":"DRIFT_RA_S","value":0.4}},{{"name":"DRIFT_DEC_S","value":-0.2}},{{"name":"RMSE_RA_S","value":0.5}},{{"name":"RMSE_DEC_S","value":0.3}},{{"name":"DITHERING","value":{dithering}}}]}}}}"#))
    }

    fn statuses(r: &Reaction) -> Vec<String> {
        r.to_browser
            .iter()
            .filter_map(|m| {
                let v: Value = serde_json::from_str(m).unwrap();
                assert_eq!(v["type"], "new_guide_state");
                v["payload"]["status"].as_str().map(str::to_string)
            })
            .collect()
    }

    fn kinds(msgs: &[String]) -> Vec<String> {
        msgs.iter()
            .map(|m| {
                let v: Value = serde_json::from_str(m).unwrap();
                let (k, b) = v.as_object().unwrap().iter().next().unwrap();
                format!("{k}:{}:{}", b["name"].as_str().unwrap(), b["items"][0]["name"].as_str().unwrap())
            })
            .collect()
    }

    #[test]
    fn calibration_then_guiding_with_drift_and_dither() {
        let mut s = store();
        let mut g = Guide::new();
        // Not calibrated: calibrate and guide in one process.
        assert_eq!(kinds(&g.start(&s, A).unwrap()), ["newSwitchVector:AGENT_START_PROCESS:CALIBRATION_AND_GUIDING"]);
        let c = process(&mut s, "Busy", "CALIBRATION_AND_GUIDING", Some("Calibration started"));
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Calibrating"]);
        let c = stats(&mut s, 4.0, 1.0, 0.0);
        assert!(statuses(&g.on_change(&c, &s, A)).is_empty(), "still calibrating, no drift samples");
        // The agent ends the calibration Ok and starts again as GUIDING.
        let c = process(&mut s, "Ok", "", Some("Calibration complete"));
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Calibrated"]);
        let c = process(&mut s, "Busy", "GUIDING", Some("Guiding started"));
        let r = g.on_change(&c, &s, A);
        assert_eq!(statuses(&r), ["Guiding"]);
        let c = stats(&mut s, 0.0, 2.0, 0.0);
        let r = g.on_change(&c, &s, A);
        let v: Value = serde_json::from_str(r.to_browser.last().unwrap()).unwrap();
        assert_eq!(v["payload"]["drift_ra"], 0.4);
        assert_eq!(v["payload"]["drift_de"], -0.2);
        assert_eq!(v["payload"]["rarms"], 0.5);
        assert_eq!(v["payload"]["derms"], 0.3);
        // A dither settles.
        let c = stats(&mut s, 0.0, 3.0, 1.5);
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Dithering"]);
        let c = stats(&mut s, 0.0, 4.0, 0.0);
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Guiding"]);
        // Guide frame while running.
        let c = set(&mut s, r#"{"setBLOBVector":{"device":"Guider Agent","name":"CCD_PREVIEW_IMAGE","state":"Ok","items":[{"name":"IMAGE","value":"/blob/0x9.jpeg"}]}}"#);
        assert_eq!(g.on_change(&c, &s, A).fetch_preview.as_deref(), Some("/blob/0x9.jpeg"));
        // Stop: the agent ends guiding Ok with "Guiding aborted".
        assert_eq!(kinds(&g.stop(A)), ["newSwitchVector:AGENT_ABORT_PROCESS:ABORT"]);
        let c = process(&mut s, "Ok", "", Some("Guiding aborted"));
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Aborted"]);
        // Now calibrated: a plain GUIDING start.
        set(&mut s, r#"{"setNumberVector":{"device":"Guider Agent","name":"AGENT_GUIDER_SETTINGS","state":"Ok","items":[{"name":"SPEED_RA","value":3.2},{"name":"SPEED_DEC","value":3.1}]}}"#);
        assert_eq!(kinds(&g.start(&s, A).unwrap()), ["newSwitchVector:AGENT_START_PROCESS:GUIDING"]);
        assert_eq!(kinds(&g.clear_calibration(&s, A)), ["newNumberVector:AGENT_GUIDER_SETTINGS:SPEED_RA"]);
        g.apply_setting("kcfg_ReuseGuideCalibration", &json!(false), &s, A);
        assert_eq!(kinds(&g.start(&s, A).unwrap()), ["newSwitchVector:AGENT_START_PROCESS:CALIBRATION_AND_GUIDING"]);
    }

    #[test]
    fn calibration_outcomes_and_framing() {
        let mut s = store();
        let mut g = Guide::new();
        let c = process(&mut s, "Busy", "CALIBRATION", None);
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Calibrating"]);
        let c = process(&mut s, "Ok", "", Some("Calibration complete"));
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Calibrated"]);
        let c = process(&mut s, "Busy", "CALIBRATION", None);
        g.on_change(&c, &s, A);
        let c = process(&mut s, "Alert", "", Some("Calibration failed"));
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Calibration error"]);
        // Guiding that fails (star lost) → Aborted.
        let c = process(&mut s, "Busy", "GUIDING", None);
        g.on_change(&c, &s, A);
        let c = stats(&mut s, 0.0, 1.0, 0.0);
        g.on_change(&c, &s, A);
        let c = process(&mut s, "Alert", "", Some("Guiding failed"));
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Aborted"]);
        // Framing.
        assert_eq!(kinds(&g.preview(&s, A, false).unwrap()), ["newSwitchVector:AGENT_START_PROCESS:PREVIEW_1"]);
        let c = process(&mut s, "Busy", "PREVIEW_1", None);
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Capturing"]);
        let c = process(&mut s, "Ok", "", None);
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Idle"]);
        let c = process(&mut s, "Busy", "PREVIEW", None);
        assert_eq!(statuses(&g.on_change(&c, &s, A)), ["Looping"]);
        assert!(g.start(&s, A).is_none(), "busy");
    }

    #[test]
    fn settings_round_trip() {
        let mut s = store();
        let mut g = Guide::new();
        let p = g.settings_payload(&s, A);
        assert_eq!(p["guideExposure"], 1.0);
        assert_eq!(p["guideBinning"], "1x1");
        assert_eq!(p["guideSquareSize"], 17.0);
        assert_eq!(p["guideSubframe"], false);
        assert_eq!(p["dECGuideEnabled"], true);
        assert_eq!(p["northDECGuideEnabled"], true);
        assert_eq!(p["kcfg_GuideCalibrationBacklash"], false);
        assert_eq!(p["kcfg_ReverseDecOnPierSideChange"], true);
        assert_eq!(p["kcfg_ReuseGuideCalibration"], true);
        assert_eq!(p["kcfg_CalibrationPulseDuration"], 0.0);
        assert!(p.get("kcfg_GPGEnabled").is_none());
        assert_eq!(kinds(&g.apply_setting("guideExposure", &json!(2.0), &s, A)), ["newNumberVector:AGENT_GUIDER_SETTINGS:EXPOSURE"]);
        assert_eq!(kinds(&g.apply_setting("guideSquareSize", &json!(32), &s, A)), ["newNumberVector:AGENT_GUIDER_SELECTION:RADIUS"]);
        assert_eq!(kinds(&g.apply_setting("southDECGuideEnabled", &json!(false), &s, A)), ["newSwitchVector:AGENT_GUIDER_DEC_MODE:NORTH"]);
        set(&mut s, r#"{"setSwitchVector":{"device":"Guider Agent","name":"AGENT_GUIDER_DEC_MODE","state":"Ok","items":[{"name":"BOTH","value":false},{"name":"NORTH","value":true}]}}"#);
        assert_eq!(kinds(&g.apply_setting("northDECGuideEnabled", &json!(false), &s, A)), ["newSwitchVector:AGENT_GUIDER_DEC_MODE:NONE"]);
        assert_eq!(kinds(&g.apply_setting("dECGuideEnabled", &json!(true), &s, A)), ["newSwitchVector:AGENT_GUIDER_DEC_MODE:BOTH"]);
        let w = g.apply_calibration(&json!({"pulse": 1500, "reuseCalibration": false}), &s, A);
        assert_eq!(kinds(&w), ["newNumberVector:AGENT_GUIDER_SETTINGS:STEP0"]);
        assert!(w[0].contains("1.5"));
        assert!(!g.reuse_calibration);
        assert!(g.apply_setting("kcfg_GPGEnabled", &json!(true), &s, A).is_empty());
    }
}
