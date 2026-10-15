//! The Focus tab on the Imager Agent: autofocus (`AGENT_START_PROCESS
//! {FOCUSING}`), framing (`PREVIEW` / `PREVIEW_1`), manual moves on the
//! agent's focuser copy, and `new_focus_state` from the agent's stats.
//!
//! The agent forces CLIENT upload + RAW format while it runs and restores
//! them after; with `CCD_PREVIEW` ENABLED the camera also publishes a JPEG of
//! every frame on `CCD_PREVIEW_IMAGE`, which the agent republishes — that is
//! the "+F" frame the browser overlays its star detection on.

use serde_json::{json, Value};

use super::agents::AgentSet;
use super::commands::reply_msg;
use super::process::{Edge, Log, Process, Reaction};
use super::protocol::{new_vector_msg, Change, Kind, PropertyStore, State};

/// INDIGO's `HFD` is a half-flux *diameter*; Ekos charts the radius.
const HFD_TO_HFR: f64 = 0.5;

#[derive(Default)]
pub struct Focus {
    log: Log,
    process: Process,
    abort_requested: bool,
    last_frame: f64,
    /// Size of the last preview frame, for the crosshair mapping.
    last_size: Option<(u32, u32)>,
    /// The focuser position last pushed (the copy updates twice per move).
    last_pos: Option<i64>,
}

fn switch(device: &str, name: &str, item: &str) -> String {
    new_vector_msg(Kind::Switch, device, name, &[(item.to_string(), json!(true))])
}

fn number(device: &str, name: &str, items: &[(&str, f64)]) -> String {
    let items: Vec<(String, Value)> = items.iter().map(|(n, v)| (n.to_string(), json!(v))).collect();
    new_vector_msg(Kind::Number, device, name, &items)
}

/// `CCD_BIN` as Ekos' "NxN".
pub fn binning_string(store: &PropertyStore, device: &str) -> Option<String> {
    let p = store.get(device, "CCD_BIN")?;
    Some(format!("{}x{}", p.number("HORIZONTAL")? as i64, p.number("VERTICAL")? as i64))
}

/// A `CCD_BIN` write for Ekos' "NxN" (or a bare number), if it differs.
pub fn binning_write(store: &PropertyStore, device: &str, value: &Value) -> Option<String> {
    let n: f64 = match value {
        Value::String(s) => s.split('x').next()?.trim().parse().ok()?,
        v => v.as_f64()?,
    };
    if n < 1.0 || binning_string(store, device)? == format!("{}x{}", n as i64, n as i64) {
        return None;
    }
    Some(number(device, "CCD_BIN", &[("HORIZONTAL", n), ("VERTICAL", n)]))
}

impl Focus {
    fn state_msg(&self, mut payload: Value) -> String {
        payload["log"] = json!(self.log.text());
        reply_msg("new_focus_state", payload)
    }

    fn stats(&self, store: &PropertyStore, agent: &str) -> (Option<f64>, Option<f64>) {
        let s = store.get(agent, "AGENT_IMAGER_STATS");
        let hfr = s.and_then(|p| p.number("HFD")).filter(|h| *h > 0.0).map(|h| h * HFD_TO_HFR);
        let pos = s
            .and_then(|p| p.number("FOCUS_POSITION"))
            .or_else(|| store.get(agent, "FOCUSER_POSITION").and_then(|p| p.number("POSITION")));
        (hfr, pos)
    }

    /// React to a change on the Imager Agent.
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
                r.to_browser.push(self.state_msg(json!({ "status": "Idle" })));
                return r;
            }
            _ => return r,
        };
        let Some(p) = store.get(agent, name) else { return r };
        let noted = p.message.as_deref().is_some_and(|m| self.log.push(m));
        match name {
            "AGENT_START_PROCESS" => match self.process.observe(p) {
                Edge::Started(item) => {
                    self.last_frame = 0.0;
                    let payload = match item.as_str() {
                        "FOCUSING" => {
                            self.abort_requested = false;
                            json!({ "status": "In Progress", "focusinitHFRPlot": true, "title": "Autofocus" })
                        }
                        "PREVIEW" | "PREVIEW_1" => json!({ "status": "Framing" }),
                        _ => return r,
                    };
                    r.to_browser.push(self.state_msg(payload));
                }
                Edge::Finished { ok, item } => {
                    let payload = match (item.as_str(), ok) {
                        ("FOCUSING", true) => {
                            let (hfr, pos) = self.stats(store, agent);
                            let title = match (hfr, pos) {
                                (Some(h), Some(p)) => format!("Autofocus complete: HFR {h:.2} at {}", p as i64),
                                _ => "Autofocus complete".to_string(),
                            };
                            let mut v = json!({ "status": "Complete", "title": title });
                            if let Some(h) = hfr {
                                v["hfr"] = json!(h);
                            }
                            if let Some(p) = pos {
                                v["pos"] = json!(p as i64);
                            }
                            v
                        }
                        ("FOCUSING", false) => {
                            let aborted = self.abort_requested
                                || self.log.latest().is_some_and(|l| l.to_ascii_lowercase().contains("abort"));
                            json!({ "status": if aborted { "Aborted" } else { "Failed" } })
                        }
                        _ => json!({ "status": "Idle" }),
                    };
                    self.abort_requested = false;
                    r.to_browser.push(self.state_msg(payload));
                }
                Edge::Still if noted => r.to_browser.push(self.state_msg(json!({}))),
                Edge::Still => {}
            },
            "AGENT_IMAGER_STATS" => {
                let frame = p.number("FRAME").unwrap_or(0.0);
                if self.process.is_running() && frame > self.last_frame {
                    self.last_frame = frame;
                    let (hfr, pos) = self.stats(store, agent);
                    if let Some(h) = hfr {
                        let mut v = json!({ "hfr": h });
                        if let Some(p) = pos {
                            v["pos"] = json!(p as i64);
                        }
                        r.to_browser.push(self.state_msg(v));
                    }
                }
            }
            "CCD_PREVIEW_IMAGE" => {
                if p.state == State::Ok && self.process.is_running() {
                    if let Some(url) = p.item("IMAGE").and_then(|i| i.blob_url()) {
                        r.fetch_preview = Some(url.to_string());
                    }
                }
            }
            "FOCUSER_POSITION" => {
                if let Some(pos) = p.number("POSITION").map(|v| v as i64).filter(|v| Some(*v) != self.last_pos) {
                    self.last_pos = Some(pos);
                    r.to_browser.push(self.state_msg(json!({ "pos": pos })));
                }
            }
            _ if noted => r.to_browser.push(self.state_msg(json!({}))),
            _ => {}
        }
        r
    }

    /// A fetched "+F" frame: remember its size for the crosshair.
    pub fn frame_delivered(&mut self, size: (u32, u32)) {
        self.last_size = Some(size);
    }

    /// `focus_get_all_settings`: the keys the sheet should show, straight
    /// from the agent (it renders only the keys present).
    pub fn settings_payload(&self, store: &PropertyStore, agent: &str) -> Value {
        let mut p = json!({});
        if let Some(e) = store.get(agent, "AGENT_IMAGER_BATCH").and_then(|b| b.number("EXPOSURE")) {
            p["focusExposure"] = json!(e);
        }
        if let Some(b) = binning_string(store, agent) {
            p["focusBinning"] = json!(b);
        }
        if let Some(g) = store.get(agent, "CCD_GAIN").and_then(|g| g.number("GAIN")) {
            p["focusGain"] = json!(g);
        }
        if let Some(f) = store.get(agent, "AGENT_IMAGER_FOCUS") {
            if let Some(s) = f.number("U_CURVE_STEP") {
                p["focusStepSize"] = json!(s);
            }
            if let Some(b) = f.number("BACKLASH") {
                p["focusBacklash"] = json!(b);
            }
        }
        p
    }

    /// `focus_set_all_settings {key: value}` → the INDIGO writes.
    pub fn apply_setting(&mut self, key: &str, value: &Value, store: &PropertyStore, agent: &str) -> Vec<String> {
        let num = value.as_f64();
        match (key, num) {
            ("focusExposure", Some(v)) if v > 0.0 => vec![number(agent, "AGENT_IMAGER_BATCH", &[("EXPOSURE", v)])],
            ("focusBinning", _) => binning_write(store, agent, value).into_iter().collect(),
            ("focusGain", Some(v)) => vec![number(agent, "CCD_GAIN", &[("GAIN", v)])],
            ("focusStepSize", Some(v)) if v >= 1.0 => vec![number(agent, "AGENT_IMAGER_FOCUS", &[("U_CURVE_STEP", v)])],
            ("focusBacklash", Some(v)) if v >= 0.0 => vec![number(agent, "AGENT_IMAGER_FOCUS", &[("BACKLASH", v)])],
            _ => Vec::new(),
        }
    }

    /// Writes that make the agent publish a JPEG of each frame.
    fn preview_on(store: &PropertyStore, agent: &str) -> Option<String> {
        store
            .get(agent, "CCD_PREVIEW")
            .filter(|p| !p.on("ENABLED") && !p.on("ENABLED_WITH_HISTOGRAM"))
            .map(|_| switch(agent, "CCD_PREVIEW", "ENABLED"))
    }

    /// `focus_start`: autofocus. `None` when the agent is busy.
    pub fn start(&mut self, store: &PropertyStore, agent: &str) -> Option<Vec<String>> {
        if AgentSet::busy(store, agent) {
            return None;
        }
        self.abort_requested = false;
        let mut out: Vec<String> = Self::preview_on(store, agent).into_iter().collect();
        out.push(switch(agent, "AGENT_START_PROCESS", "FOCUSING"));
        Some(out)
    }

    /// `focus_capture` (one frame) / `focus_loop`.
    pub fn preview(&mut self, store: &PropertyStore, agent: &str, looping: bool) -> Option<Vec<String>> {
        if AgentSet::busy(store, agent) {
            return None;
        }
        let mut out: Vec<String> = Self::preview_on(store, agent).into_iter().collect();
        out.push(switch(agent, "AGENT_START_PROCESS", if looping { "PREVIEW" } else { "PREVIEW_1" }));
        Some(out)
    }

    /// `focus_stop`.
    pub fn stop(&mut self, agent: &str) -> Vec<String> {
        self.abort_requested = true;
        vec![switch(agent, "AGENT_ABORT_PROCESS", "ABORT")]
    }

    /// `focus_reset`: forget the selected star (Ekos' "reset frame").
    pub fn reset(&self, store: &PropertyStore, agent: &str) -> Vec<String> {
        if AgentSet::busy(store, agent) {
            return Vec::new();
        }
        vec![switch(agent, "AGENT_START_PROCESS", "CLEAR_SELECTION")]
    }

    /// `focus_in` / `focus_out {steps}` on the agent's focuser copy.
    pub fn step(&self, store: &PropertyStore, agent: &str, inward: bool, steps: f64) -> Vec<String> {
        if steps <= 0.0 || store.get(agent, "FOCUSER_STEPS").is_none() {
            return Vec::new();
        }
        vec![
            switch(agent, "FOCUSER_DIRECTION", if inward { "MOVE_INWARD" } else { "MOVE_OUTWARD" }),
            number(agent, "FOCUSER_STEPS", &[("STEPS", steps)]),
        ]
    }

    /// `focus_set_crosshair {x, y}` (fractions of the frame) → the star to
    /// focus on, in the agent's (binned) frame pixels. 0,0 means "auto", so
    /// the corner is nudged.
    pub fn crosshair(&self, store: &PropertyStore, agent: &str, x: f64, y: f64) -> Vec<String> {
        let size = self.last_size.map(|(w, h)| (w as f64, h as f64)).or_else(|| {
            let f = store.get(agent, "CCD_FRAME")?;
            let b = store.get(agent, "CCD_BIN");
            let bx = b.and_then(|b| b.number("HORIZONTAL")).unwrap_or(1.0).max(1.0);
            let by = b.and_then(|b| b.number("VERTICAL")).unwrap_or(1.0).max(1.0);
            Some((f.number("WIDTH")? / bx, f.number("HEIGHT")? / by))
        });
        let Some((w, h)) = size else { return Vec::new() };
        if store.get(agent, "AGENT_IMAGER_SELECTION").is_none() {
            return Vec::new();
        }
        let px = (x.clamp(0.0, 1.0) * w).round().max(1.0);
        let py = (y.clamp(0.0, 1.0) * h).round().max(1.0);
        vec![number(agent, "AGENT_IMAGER_SELECTION", &[("X", px), ("Y", py)])]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    const A: &str = "Imager Agent";

    fn store() -> PropertyStore {
        let mut s = PropertyStore::default();
        for f in [
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","perm":"rw","state":"Ok","rule":"AtMostOne","items":[{"name":"PREVIEW_1","value":false},{"name":"PREVIEW","value":false},{"name":"FOCUSING","value":false},{"name":"CLEAR_SELECTION","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_ABORT_PROCESS","perm":"rw","state":"Ok","items":[{"name":"ABORT","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_BATCH","perm":"rw","state":"Ok","items":[{"name":"COUNT","value":1},{"name":"EXPOSURE","value":2.5}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_FOCUS","perm":"rw","state":"Ok","items":[{"name":"U_CURVE_STEP","value":20},{"name":"BACKLASH","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","perm":"ro","state":"Ok","items":[{"name":"FRAME","value":0},{"name":"FOCUS_POSITION","value":0},{"name":"HFD","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_SELECTION","perm":"rw","state":"Ok","items":[{"name":"X","value":0},{"name":"Y","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"CCD_FRAME","perm":"rw","state":"Ok","items":[{"name":"WIDTH","value":1600},{"name":"HEIGHT","value":1200}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"CCD_BIN","perm":"rw","state":"Ok","items":[{"name":"HORIZONTAL","value":2},{"name":"VERTICAL","value":2}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"CCD_GAIN","perm":"rw","state":"Ok","items":[{"name":"GAIN","value":100}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"CCD_PREVIEW","perm":"rw","state":"Ok","items":[{"name":"DISABLED","value":true},{"name":"ENABLED","value":false}]}}"#,
            r#"{"defBLOBVector":{"device":"Imager Agent","name":"CCD_PREVIEW_IMAGE","perm":"ro","state":"Idle","items":[{"name":"IMAGE"}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"FOCUSER_STEPS","perm":"rw","state":"Ok","items":[{"name":"STEPS","value":0}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"FOCUSER_DIRECTION","perm":"rw","state":"Ok","items":[{"name":"MOVE_INWARD","value":true},{"name":"MOVE_OUTWARD","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"FOCUSER_POSITION","perm":"rw","state":"Ok","items":[{"name":"POSITION","value":5000}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        s
    }

    fn set(s: &mut PropertyStore, frame: &str) -> Change {
        s.apply(parse_frame(frame).unwrap()).unwrap()
    }

    fn payloads(r: &Reaction) -> Vec<Value> {
        r.to_browser
            .iter()
            .map(|m| {
                let v: Value = serde_json::from_str(m).unwrap();
                assert_eq!(v["type"], "new_focus_state");
                v["payload"].clone()
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
    fn autofocus_walks_the_curve_and_completes() {
        let mut s = store();
        let mut f = Focus::default();
        let w = f.start(&s, A).unwrap();
        assert_eq!(kinds(&w), ["newSwitchVector:CCD_PREVIEW:ENABLED", "newSwitchVector:AGENT_START_PROCESS:FOCUSING"]);

        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","message":"Focusing started","items":[{"name":"FOCUSING","value":true}]}}"#);
        let p = payloads(&f.on_change(&c, &s, A));
        assert_eq!(p[0]["status"], "In Progress");
        assert_eq!(p[0]["focusinitHFRPlot"], true);
        assert_eq!(p[0]["log"], "Focusing started");
        // Busy: a second start is refused.
        assert!(f.start(&s, A).is_none());

        let c = set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","state":"Busy","items":[{"name":"FRAME","value":1},{"name":"FOCUS_POSITION","value":5020},{"name":"HFD","value":6.4}]}}"#);
        let p = payloads(&f.on_change(&c, &s, A));
        assert_eq!(p[0]["hfr"], 3.2);
        assert_eq!(p[0]["pos"], 5020);
        // Same frame again: nothing.
        let c = set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","state":"Busy","items":[{"name":"FRAME","value":1}]}}"#);
        assert!(f.on_change(&c, &s, A).to_browser.is_empty());

        let c = set(&mut s, r#"{"setBLOBVector":{"device":"Imager Agent","name":"CCD_PREVIEW_IMAGE","state":"Ok","items":[{"name":"IMAGE","value":"/blob/0x1.jpeg"}]}}"#);
        assert_eq!(f.on_change(&c, &s, A).fetch_preview.as_deref(), Some("/blob/0x1.jpeg"));

        set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","state":"Ok","items":[{"name":"FRAME","value":12},{"name":"FOCUS_POSITION","value":5100},{"name":"HFD","value":2.0}]}}"#);
        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Ok","message":"Focusing finished","items":[{"name":"FOCUSING","value":false}]}}"#);
        let p = payloads(&f.on_change(&c, &s, A));
        assert_eq!(p[0]["status"], "Complete");
        assert_eq!(p[0]["hfr"], 1.0);
        assert_eq!(p[0]["pos"], 5100);
        assert_eq!(p[0]["title"], "Autofocus complete: HFR 1.00 at 5100");
        assert_eq!(p[0]["log"], "Focusing finished\nFocusing started");

        // No process: the preview frame is nobody's.
        let c = set(&mut s, r#"{"setBLOBVector":{"device":"Imager Agent","name":"CCD_PREVIEW_IMAGE","state":"Ok","items":[{"name":"IMAGE","value":"/blob/0x2.jpeg"}]}}"#);
        assert!(f.on_change(&c, &s, A).fetch_preview.is_none());
    }

    #[test]
    fn failure_abort_and_framing() {
        let mut s = store();
        let mut f = Focus::default();
        f.start(&s, A);
        set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"FOCUSING","value":true}]}}"#);
        f.on_change(&Change::Updated { device: A.into(), name: "AGENT_START_PROCESS".into() }, &s, A);
        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Alert","message":"Focusing failed","items":[{"name":"FOCUSING","value":false}]}}"#);
        assert_eq!(payloads(&f.on_change(&c, &s, A))[0]["status"], "Failed");

        f.start(&s, A);
        set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"FOCUSING","value":true}]}}"#);
        f.on_change(&Change::Updated { device: A.into(), name: "AGENT_START_PROCESS".into() }, &s, A);
        assert_eq!(kinds(&f.stop(A)), ["newSwitchVector:AGENT_ABORT_PROCESS:ABORT"]);
        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Alert","message":"Focusing aborted","items":[{"name":"FOCUSING","value":false}]}}"#);
        assert_eq!(payloads(&f.on_change(&c, &s, A))[0]["status"], "Aborted");

        // Framing: PREVIEW ends Ok even when aborted → Idle.
        let w = f.preview(&s, A, true).unwrap();
        assert_eq!(kinds(&w).last().unwrap(), "newSwitchVector:AGENT_START_PROCESS:PREVIEW");
        assert_eq!(kinds(&f.preview(&s, A, false).unwrap()).last().unwrap(), "newSwitchVector:AGENT_START_PROCESS:PREVIEW_1");
        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"PREVIEW","value":true}]}}"#);
        assert_eq!(payloads(&f.on_change(&c, &s, A))[0]["status"], "Framing");
        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Ok","items":[{"name":"PREVIEW","value":false}]}}"#);
        assert_eq!(payloads(&f.on_change(&c, &s, A))[0]["status"], "Idle");

        // A message frame lands in the log, newest first.
        let r = f.on_change(&Change::Message { device: Some(A.into()), text: "Preview failed".into() }, &s, A);
        assert!(payloads(&r)[0]["log"].as_str().unwrap().starts_with("Preview failed\n"));
    }

    #[test]
    fn manual_moves_crosshair_and_settings() {
        let mut s = store();
        let mut f = Focus::default();
        assert_eq!(kinds(&f.step(&s, A, true, 100.0)), ["newSwitchVector:FOCUSER_DIRECTION:MOVE_INWARD", "newNumberVector:FOCUSER_STEPS:STEPS"]);
        assert_eq!(kinds(&f.step(&s, A, false, 10.0))[0], "newSwitchVector:FOCUSER_DIRECTION:MOVE_OUTWARD");
        assert!(f.step(&s, A, true, 0.0).is_empty());
        let c = set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"FOCUSER_POSITION","state":"Ok","items":[{"name":"POSITION","value":4900}]}}"#);
        assert_eq!(payloads(&f.on_change(&c, &s, A))[0]["pos"], 4900);

        // Crosshair: no frame seen yet → binned sensor size (1600/2 × 1200/2).
        let w = f.crosshair(&s, A, 0.5, 0.25);
        assert!(w[0].contains(r#"{"name":"X","value":400.0}"#) && w[0].contains(r#"{"name":"Y","value":150.0}"#), "{w:?}");
        f.frame_delivered((640, 480));
        let w = f.crosshair(&s, A, 0.0, 1.0);
        assert!(w[0].contains(r#"{"name":"X","value":1.0}"#) && w[0].contains(r#"{"name":"Y","value":480.0}"#), "{w:?}");
        assert_eq!(kinds(&f.reset(&s, A)), ["newSwitchVector:AGENT_START_PROCESS:CLEAR_SELECTION"]);

        let p = f.settings_payload(&s, A);
        assert_eq!(p["focusExposure"], 2.5);
        assert_eq!(p["focusBinning"], "2x2");
        assert_eq!(p["focusGain"], 100.0);
        assert_eq!(p["focusStepSize"], 20.0);
        assert_eq!(p["focusBacklash"], 0.0);
        assert_eq!(kinds(&f.apply_setting("focusExposure", &json!(3.0), &s, A)), ["newNumberVector:AGENT_IMAGER_BATCH:EXPOSURE"]);
        assert_eq!(kinds(&f.apply_setting("focusBinning", &json!("1x1"), &s, A)), ["newNumberVector:CCD_BIN:HORIZONTAL"]);
        assert!(f.apply_setting("focusBinning", &json!("2x2"), &s, A).is_empty(), "unchanged binning");
        assert_eq!(kinds(&f.apply_setting("focusStepSize", &json!(50), &s, A)), ["newNumberVector:AGENT_IMAGER_FOCUS:U_CURVE_STEP"]);
        assert!(f.apply_setting("focusAlgorithm", &json!("Linear"), &s, A).is_empty());
    }
}
