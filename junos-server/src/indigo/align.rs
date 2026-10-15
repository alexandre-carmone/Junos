//! Plate solving and polar alignment on the Astrometry Agent.
//!
//! The agent has no device selectors: it takes images through the related
//! Imager Agent (`AGENT_IMAGER_CAPTURE`) and syncs / slews through the
//! related Mount Agent (`agents.rs` keeps those links). `AGENT_START_PROCESS`
//! runs one of `SOLVE`, `SYNC`, `CENTER` (Ekos' "Nothing" / "Sync" /
//! "Slew to target" solve actions) or `CALCULATE_PA_ERROR` /
//! `RECALCULATE_PA_ERROR` (the three-point polar alignment and its refresh).
//! `AGENT_PLATESOLVER_WCS` narrates the solve (`STATE`: 0 idle, 1 waiting
//! for the image, 2 solving, 3 syncing, 4 centering, 5 goto) and holds the
//! result; `AGENT_PLATESOLVER_PA_STATE` narrates the alignment (`STATE`: 0
//! idle, 2/3/4 the three reference points, 5 recalculating, 6 in progress =
//! adjusting) and holds the errors.
//!
//! The browser's `new_polar_state` is KStars' assistant: its stages are
//! derived here from the PA state, the WCS state and whether the mount is
//! slewing. Its error signs follow KStars' arrows ("←" for a positive
//! azimuth error, "↓" for a positive altitude error), so INDIGO's
//! `AZ_CORRECTION_CW` / `ALT_CORRECTION_UP` flags set the signs here — the
//! agent's own signed values are never copied.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::agents::AgentSet;
use super::astro;
use super::commands::reply_msg;
use super::focus::{binning_string, binning_write};
use super::mount;
use super::process::{Edge, Log, Process, Reaction};
use super::protocol::{new_vector_msg, Change, Kind, PropertyStore, State};

/// How long a solution waits for its "+A" frame before going out alone.
const FRAME_HOLD: Duration = Duration::from_secs(3);

const WCS_IDLE: f64 = 0.0;
const WCS_WAITING: f64 = 1.0;
const WCS_SOLVING: f64 = 2.0;
const WCS_SYNCING: f64 = 3.0;
const WCS_CENTERING: f64 = 4.0;
const WCS_GOTO: f64 = 5.0;

const PA_REFERENCE_1: f64 = 2.0;
const PA_REFERENCE_2: f64 = 3.0;
const PA_REFERENCE_3: f64 = 4.0;
const PA_RECALCULATE: f64 = 5.0;
const PA_IN_PROGRESS: f64 = 6.0;

/// Ekos' solve action (the `syncR` / `slewR` / `nothingR` radio).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Sync,
    Slew,
    Nothing,
}

impl Action {
    fn item(self) -> &'static str {
        match self {
            Self::Sync => "SYNC",
            Self::Slew => "CENTER",
            Self::Nothing => "SOLVE",
        }
    }
}

/// The devices a solve or an alignment needs besides the solver.
pub struct Rig<'a> {
    /// The Mount Agent holds a mount (SYNC / CENTER / polar alignment).
    pub mount_ready: bool,
    /// The Imager Agent holds a camera.
    pub camera_ready: bool,
    /// The mount device, for its slewing state.
    pub mount: Option<&'a str>,
}

pub struct Align {
    log: Log,
    process: Process,
    abort_requested: bool,
    status: String,
    last_wcs: Option<State>,
    frame_in_flight: bool,
    pending_solution: Option<(Value, Instant)>,
    last_size: Option<(u32, u32)>,
    stage: String,
    last_pa_state: f64,
    /// The WCS hasn't updated since the PA state last changed, so a
    /// "solving" there is the previous reference's.
    wcs_stale: bool,
    // Local-only settings (no counterpart in the agent), echoed back.
    action: Action,
    accuracy_arcsec: f64,
    hints_radius: f64,
    pah_mount_speed: String,
    pah_refresh_algorithm: String,
}

impl Default for Align {
    fn default() -> Self {
        Self {
            log: Log::default(),
            process: Process::default(),
            abort_requested: false,
            status: String::new(),
            last_wcs: None,
            frame_in_flight: false,
            pending_solution: None,
            last_size: None,
            stage: String::new(),
            last_pa_state: 0.0,
            wcs_stale: false,
            action: Action::Slew,
            accuracy_arcsec: 30.0,
            hints_radius: 30.0,
            pah_mount_speed: "Max".into(),
            pah_refresh_algorithm: "Plate Solve".into(),
        }
    }
}

fn switch(device: &str, name: &str, item: &str) -> String {
    new_vector_msg(Kind::Switch, device, name, &[(item.to_string(), json!(true))])
}

fn number(device: &str, name: &str, items: &[(&str, f64)]) -> String {
    let items: Vec<(String, Value)> = items.iter().map(|(n, v)| (n.to_string(), json!(v))).collect();
    new_vector_msg(Kind::Number, device, name, &items)
}

impl Align {
    fn align_msg(&self, mut payload: Value) -> String {
        payload["log"] = json!(self.log.text());
        reply_msg("new_align_state", payload)
    }

    fn push_status(&mut self, status: &str, r: &mut Reaction) {
        if status != self.status {
            self.status = status.to_string();
            r.to_browser.push(self.align_msg(json!({ "status": status })));
        }
    }

    /// The solved plate in KStars' `solution` shape (JNow).
    fn solution(store: &PropertyStore, agent: &str) -> Option<Value> {
        let w = store.get(agent, "AGENT_PLATESOLVER_WCS")?;
        let mut ra = w.number("RA")? * 15.0;
        let mut dec = w.number("DEC")?;
        // The agent calls any non-zero epoch J2000 (indigo_platesolver.c).
        if w.number("EPOCH").unwrap_or(0.0) != 0.0 {
            (ra, dec) = astro::j2000_to_jnow(ra, dec, astro::now_jd());
        }
        Some(json!({
            "ra.Hours": ra / 15.0,
            "de.Degrees": dec,
            "PA": w.number("ANGLE").unwrap_or(0.0),
            "pix": w.number("SCALE").unwrap_or(0.0) * 3600.0,
            "fov": format!("{:.1}' x {:.1}'", w.number("WIDTH").unwrap_or(0.0) * 60.0, w.number("HEIGHT").unwrap_or(0.0) * 60.0),
        }))
    }

    /// React to a change on the solver (or the mount's coordinates, which
    /// split a polar-alignment rotation from its settle).
    pub fn on_change(&mut self, change: &Change, store: &PropertyStore, agent: &str, rig: &Rig, now: Instant) -> Reaction {
        let mut r = Reaction::default();
        let name = match change {
            Change::Defined { device, name } | Change::Updated { device, name } if device == agent => name.as_str(),
            Change::Updated { device, name } if Some(device.as_str()) == rig.mount && name == "MOUNT_EQUATORIAL_COORDINATES" => {
                self.polar_update(store, agent, rig, &mut r);
                return r;
            }
            Change::Message { device: Some(d), text } if d == agent => {
                if self.log.push(text) {
                    r.to_browser.push(self.align_msg(json!({})));
                }
                return r;
            }
            Change::Deleted { device, name: None } if device == agent => {
                self.process = Process::default();
                self.last_wcs = None;
                self.push_status("Idle", &mut r);
                self.push_stage("Idle", json!({}), &mut r);
                return r;
            }
            _ => return r,
        };
        let Some(p) = store.get(agent, name) else { return r };
        let noted = p.message.as_deref().is_some_and(|m| self.log.push(m));
        match name {
            "AGENT_START_PROCESS" => match self.process.observe(p) {
                Edge::Started(_) => self.abort_requested = false,
                Edge::Finished { ok: false, .. } => {
                    // The WCS may not report the failure itself ("Image
                    // capture failed", "Process aborted").
                    let aborted = self.abort_requested
                        || self.log.latest().is_some_and(|l| l.to_ascii_lowercase().contains("abort"));
                    self.abort_requested = false;
                    self.pending_solution = None;
                    self.push_status(if aborted { "Aborted" } else { "Failed" }, &mut r);
                    self.polar_update(store, agent, rig, &mut r);
                }
                Edge::Finished { ok: true, .. } => {
                    self.abort_requested = false;
                    // The adjusting stage waits for the process to end.
                    self.polar_update(store, agent, rig, &mut r);
                }
                Edge::Still if noted => r.to_browser.push(self.align_msg(json!({}))),
                Edge::Still => {}
            },
            "AGENT_PLATESOLVER_WCS" => {
                self.wcs_stale = false;
                let was = self.last_wcs.replace(p.state);
                let st = p.number("STATE").unwrap_or(WCS_IDLE);
                match p.state {
                    State::Busy => {
                        let status = if st == WCS_SYNCING {
                            "Syncing"
                        } else if st == WCS_CENTERING || st == WCS_GOTO {
                            "Slewing"
                        } else {
                            "In Progress"
                        };
                        self.push_status(status, &mut r);
                    }
                    State::Ok if was == Some(State::Busy) => {
                        self.push_status("Successful", &mut r);
                        if let Some(solution) = Self::solution(store, agent) {
                            let msg = json!({ "status": "Successful", "solution": solution });
                            if self.frame_in_flight {
                                self.pending_solution = Some((msg, now));
                            } else {
                                r.to_browser.push(self.align_msg(msg));
                            }
                        }
                    }
                    State::Alert if was == Some(State::Busy) => {
                        let aborted = self.abort_requested
                            || self.log.latest().is_some_and(|l| l.to_ascii_lowercase().contains("abort"));
                        self.pending_solution = None;
                        self.push_status(if aborted { "Aborted" } else { "Failed" }, &mut r);
                    }
                    _ if noted => r.to_browser.push(self.align_msg(json!({}))),
                    _ => {}
                }
                self.polar_update(store, agent, rig, &mut r);
            }
            "AGENT_PLATESOLVER_PA_STATE" => self.polar_update(store, agent, rig, &mut r),
            "CCD_PREVIEW_IMAGE" => {
                if p.state == State::Ok {
                    if let Some(url) = p.item("IMAGE").and_then(|i| i.blob_url()) {
                        self.frame_in_flight = true;
                        r.fetch_preview = Some(url.to_string());
                    }
                }
            }
            "AGENT_PLATESOLVER_HINTS" => {
                if let Some(radius) = p.number("RADIUS").filter(|v| *v > 0.0) {
                    self.hints_radius = radius;
                }
            }
            _ if noted => r.to_browser.push(self.align_msg(json!({}))),
            _ => {}
        }
        r
    }

    /// The "+A" frame reached the browser: the solution it was holding back
    /// can follow (the sky pairs a solution with the last frame it received).
    pub fn frame_delivered(&mut self, size: (u32, u32)) -> Vec<String> {
        self.frame_in_flight = false;
        self.last_size = Some(size);
        self.pending_solution.take().map(|(m, _)| vec![self.align_msg(m)]).unwrap_or_default()
    }

    /// Once a second: a solution whose frame never came goes out alone.
    pub fn tick(&mut self, now: Instant) -> Vec<String> {
        match &self.pending_solution {
            Some((_, since)) if now.duration_since(*since) >= FRAME_HOLD => {
                self.frame_in_flight = false;
                self.pending_solution.take().map(|(m, _)| vec![self.align_msg(m)]).unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    // ── Polar alignment ────────────────────────────────────────────────

    fn push_stage(&mut self, stage: &str, mut payload: Value, r: &mut Reaction) {
        if stage == self.stage {
            return;
        }
        self.stage = stage.to_string();
        payload["stage"] = json!(stage);
        payload["enabled"] = json!(stage != "Idle");
        if payload.get("message").is_none() {
            payload["message"] = json!(self.log.latest().unwrap_or(""));
        }
        r.to_browser.push(reply_msg("new_polar_state", payload));
    }

    /// KStars' signed errors (degrees) from the agent's magnitudes and
    /// correction flags: "←" (positive azimuth) is counter-clockwise, "↓"
    /// (positive altitude) is down.
    fn errors(store: &PropertyStore, agent: &str) -> Option<(f64, f64, f64)> {
        let p = store.get(agent, "AGENT_PLATESOLVER_PA_STATE")?;
        let az = p.number("AZ_POLAR_ERROR")?.abs() * if p.number("AZ_CORRECTION_CW").unwrap_or(0.0) > 0.5 { -1.0 } else { 1.0 };
        let alt = p.number("ALT_POLAR_ERROR")?.abs() * if p.number("ALT_CORRECTION_UP").unwrap_or(0.0) > 0.5 { -1.0 } else { 1.0 };
        Some((p.number("POLAR_ERROR")?, az, alt))
    }

    fn advice(store: &PropertyStore, agent: &str) -> String {
        let Some(p) = store.get(agent, "AGENT_PLATESOLVER_PA_STATE") else { return String::new() };
        let cw = p.number("AZ_CORRECTION_CW").unwrap_or(0.0) > 0.5;
        let up = p.number("ALT_CORRECTION_UP").unwrap_or(0.0) > 0.5;
        let az = p.number("AZ_POLAR_ERROR").unwrap_or(0.0).abs() * 60.0;
        let alt = p.number("ALT_POLAR_ERROR").unwrap_or(0.0).abs() * 60.0;
        let warning = if p.number("ACCURACY_WARNING").unwrap_or(0.0) > 0.5 {
            " — initial error too large, align within 1–2° and restart"
        } else {
            ""
        };
        format!(
            "Azimuth: move {} by {az:.1}′ · Altitude: move {} by {alt:.1}′{warning}",
            if cw { "clockwise" } else { "counter-clockwise" },
            if up { "up" } else { "down" }
        )
    }

    fn polar_update(&mut self, store: &PropertyStore, agent: &str, rig: &Rig, r: &mut Reaction) {
        let Some(pa) = store.get(agent, "AGENT_PLATESOLVER_PA_STATE") else { return };
        let st = pa.number("STATE").unwrap_or(0.0);
        let mut wcs = store.get(agent, "AGENT_PLATESOLVER_WCS").and_then(|w| w.number("STATE")).unwrap_or(WCS_IDLE);
        // A new reference point starts with its capture: a WCS still saying
        // "solving" is the previous image's until the WCS updates again.
        if st != self.last_pa_state {
            self.last_pa_state = st;
            self.wcs_stale = true;
        }
        if self.wcs_stale && wcs == WCS_SOLVING {
            wcs = WCS_WAITING;
        }
        let slewing = rig.mount.is_some_and(|m| mount::status(store, m) == "Slewing");
        let reference = |capture: &'static str, solve: &'static str, rotation: &'static str, settle: &'static str| {
            if wcs == WCS_SOLVING {
                solve
            } else if wcs == WCS_CENTERING || wcs == WCS_GOTO {
                if slewing { rotation } else { settle }
            } else {
                capture
            }
        };
        // The agent reports the errors a moment before its process ends; a
        // Refresh sent then would be ignored, so "Select Star" (the adjusting
        // stage, with the Refresh button) waits for the process.
        if st == PA_IN_PROGRESS && pa.state != State::Alert && AgentSet::busy(store, agent) {
            return;
        }
        let stage = if st == PA_REFERENCE_1 {
            if wcs == WCS_SOLVING { "First Solve" } else { "First Capture" }
        } else if st == PA_REFERENCE_2 {
            reference("Second Capture", "Second Solve", "First Rotation", "First Settle")
        } else if st == PA_REFERENCE_3 {
            reference("Third Capture", "Third Solve", "Second Rotation", "Second Settle")
        } else if st == PA_RECALCULATE {
            "Refreshing"
        } else if st == PA_IN_PROGRESS {
            if pa.state == State::Alert || self.stage == "Refreshing" || self.stage == "Refresh Complete" {
                "Refresh Complete"
            } else {
                "Select Star"
            }
        } else {
            "Idle"
        };
        let mut payload = json!({});
        match stage {
            "Select Star" => {
                if let Some((error, az, alt)) = Self::errors(store, agent) {
                    let (w, h) = self.last_size.map(|(w, h)| (w as f64 / 2.0, h as f64 / 2.0)).unwrap_or((0.0, 0.0));
                    payload["vector"] = json!({
                        "center_x": w, "center_y": h, "mag": error, "pa": 0.0,
                        "error": error, "azError": az, "altError": alt,
                    });
                    payload["message"] = json!(Self::advice(store, agent));
                }
            }
            "Refresh Complete" => {
                if pa.state == State::Alert {
                    payload["updatedError"] = json!(-1.0);
                    payload["updatedAZError"] = json!(-1.0);
                    payload["updatedALTError"] = json!(-1.0);
                    payload["message"] = json!(pa.message.clone().or_else(|| self.log.latest().map(str::to_string)).unwrap_or_else(|| "Refresh solve failed".into()));
                } else if let Some((error, az, alt)) = Self::errors(store, agent) {
                    payload["updatedError"] = json!(error);
                    payload["updatedAZError"] = json!(az);
                    payload["updatedALTError"] = json!(alt);
                    payload["message"] = json!(Self::advice(store, agent));
                }
                // A second refresh must re-announce its result.
                if self.stage == "Refresh Complete" {
                    self.stage.clear();
                }
            }
            "Idle" => {
                if pa.state == State::Alert {
                    payload["message"] = json!(pa.message.clone().unwrap_or_else(|| "Polar alignment failed".into()));
                }
            }
            _ => {}
        }
        self.push_stage(stage, payload, r);
    }

    // ── Commands ───────────────────────────────────────────────────────

    fn preview_on(store: &PropertyStore, agent: &str) -> Option<String> {
        store
            .get(agent, "CCD_PREVIEW")
            .filter(|p| !p.on("ENABLED") && !p.on("ENABLED_WITH_HISTOGRAM"))
            .map(|_| switch(agent, "CCD_PREVIEW", "ENABLED"))
    }

    fn solver_busy(store: &PropertyStore, agent: &str) -> bool {
        AgentSet::busy(store, agent) || store.get(agent, "AGENT_PLATESOLVER_WCS").is_some_and(|w| w.state == State::Busy)
    }

    fn prepare(store: &PropertyStore, agent: &str) -> Vec<String> {
        let mut out: Vec<String> = Self::preview_on(store, agent).into_iter().collect();
        if store.get(agent, "AGENT_PLATESOLVER_SOLVE_IMAGES").is_some_and(|p| !p.on("DISABLED")) {
            out.push(switch(agent, "AGENT_PLATESOLVER_SOLVE_IMAGES", "DISABLED"));
        }
        out
    }

    /// `align_solve`: capture and solve, then sync / center per the solve
    /// action. `Err` explains why nothing was started.
    pub fn solve(&mut self, store: &PropertyStore, agent: &str, rig: &Rig) -> Result<(Vec<String>, Option<String>), String> {
        if !rig.camera_ready {
            return Err("The Imager Agent has no camera to solve with".into());
        }
        if Self::solver_busy(store, agent) {
            return Err("The solver is busy".into());
        }
        self.abort_requested = false;
        self.pending_solution = None;
        let mut out = Self::prepare(store, agent);
        let mut warning = None;
        let mut action = self.action;
        if action != Action::Nothing && !rig.mount_ready {
            warning = Some("The Mount Agent has no mount: solving without sync".to_string());
            action = Action::Nothing;
        }
        out.push(switch(agent, "AGENT_START_PROCESS", action.item()));
        Ok((out, warning))
    }

    /// `align_stop` / `polar_stop`.
    pub fn stop(&mut self, agent: &str) -> Vec<String> {
        self.abort_requested = true;
        vec![switch(agent, "AGENT_ABORT_PROCESS", "ABORT")]
    }

    /// `polar_start`: the three-point measurement.
    pub fn polar_start(&mut self, store: &PropertyStore, agent: &str, rig: &Rig) -> Result<Vec<String>, String> {
        if !rig.camera_ready {
            return Err("The Imager Agent has no camera to solve with".into());
        }
        if !rig.mount_ready {
            return Err("The Mount Agent has no mount to rotate".into());
        }
        if Self::solver_busy(store, agent) {
            return Err("The solver is busy".into());
        }
        self.abort_requested = false;
        self.stage.clear();
        let mut out = Self::prepare(store, agent);
        out.push(switch(agent, "AGENT_START_PROCESS", "CALCULATE_PA_ERROR"));
        Ok(out)
    }

    /// `polar_refresh {value}`: one more solve against the target position.
    pub fn polar_refresh(&mut self, store: &PropertyStore, agent: &str, exposure: Option<f64>) -> Result<Vec<String>, String> {
        let in_progress = store
            .get(agent, "AGENT_PLATESOLVER_PA_STATE")
            .and_then(|p| p.number("STATE"))
            .is_some_and(|s| s == PA_IN_PROGRESS);
        if !in_progress {
            return Err("Polar alignment is not in progress".into());
        }
        if Self::solver_busy(store, agent) {
            return Err("The solver is busy".into());
        }
        let mut out = Vec::new();
        if let Some(e) = exposure.filter(|e| *e > 0.0) {
            out.push(number(agent, "AGENT_PLATESOLVER_PA_SETTINGS", &[("EXPOSURE", e)]));
        }
        out.push(switch(agent, "AGENT_START_PROCESS", "RECALCULATE_PA_ERROR"));
        Ok(out)
    }

    // ── Settings ───────────────────────────────────────────────────────

    /// `align_get_all_settings`: the solve and polar keys in one map, as
    /// KStars sends them (the browser replaces the whole map).
    pub fn settings_payload(&self, store: &PropertyStore, agent: &str, imager: Option<&str>) -> Value {
        let mut p = json!({
            "syncR": self.action == Action::Sync,
            "slewR": self.action == Action::Slew,
            "nothingR": self.action == Action::Nothing,
            "localSolverR": true,
            "remoteSolverR": false,
            "alignAccuracyThreshold": self.accuracy_arcsec,
            "pAHMountSpeed": self.pah_mount_speed,
            "pAHManualSlew": false,
            "pAHRefreshAlgorithm": self.pah_refresh_algorithm,
        });
        if let Some(e) = store.get(agent, "AGENT_PLATESOLVER_EXPOSURE").and_then(|x| x.number("EXPOSURE")) {
            p["alignExposure"] = json!(e);
        }
        if let Some(s) = store.get(agent, "AGENT_PLATESOLVER_MOUNT_SETTLE_TIME").and_then(|x| x.number("SETTLE_TIME")) {
            p["alignSettlingTime"] = json!((s * 1000.0).round());
        }
        if let Some(i) = imager {
            if let Some(b) = binning_string(store, i) {
                p["alignBinning"] = json!(b);
            }
            if let Some(g) = store.get(i, "CCD_GAIN").and_then(|g| g.number("GAIN")) {
                p["alignGain"] = json!(g);
            }
        }
        if let Some(pa) = store.get(agent, "AGENT_PLATESOLVER_PA_SETTINGS") {
            if let Some(h) = pa.number("HA_MOVE") {
                p["pAHDirection"] = json!(if h >= 0.0 { "West" } else { "East" });
                p["pAHRotation"] = json!(h.abs());
            }
            if let Some(e) = pa.number("EXPOSURE") {
                p["pAHExposure"] = json!(e);
            }
        }
        p
    }

    /// `align_set_all_settings {key: value}` → the INDIGO writes.
    pub fn apply_setting(&mut self, key: &str, value: &Value, store: &PropertyStore, agent: &str, imager: Option<&str>) -> Vec<String> {
        let num = value.as_f64();
        let ha_move = store.get(agent, "AGENT_PLATESOLVER_PA_SETTINGS").and_then(|p| p.number("HA_MOVE")).unwrap_or(20.0);
        match (key, num) {
            ("alignExposure", Some(v)) if v > 0.0 => vec![number(agent, "AGENT_PLATESOLVER_EXPOSURE", &[("EXPOSURE", v)])],
            ("alignSettlingTime", Some(v)) if v >= 0.0 => {
                vec![number(agent, "AGENT_PLATESOLVER_MOUNT_SETTLE_TIME", &[("SETTLE_TIME", v / 1000.0)])]
            }
            ("alignBinning", _) => imager.and_then(|i| binning_write(store, i, value)).into_iter().collect(),
            ("alignGain", Some(v)) => imager.map(|i| number(i, "CCD_GAIN", &[("GAIN", v)])).into_iter().collect(),
            ("alignAccuracyThreshold", Some(v)) => {
                self.accuracy_arcsec = v;
                Vec::new()
            }
            ("syncR", _) | ("slewR", _) | ("nothingR", _) => {
                if value.as_bool() == Some(true) {
                    self.action = match key {
                        "syncR" => Action::Sync,
                        "slewR" => Action::Slew,
                        _ => Action::Nothing,
                    };
                }
                Vec::new()
            }
            ("pAHDirection", _) => {
                let west = value.as_str().is_some_and(|d| d.eq_ignore_ascii_case("west"));
                let v = ha_move.abs() * if west { 1.0 } else { -1.0 };
                vec![number(agent, "AGENT_PLATESOLVER_PA_SETTINGS", &[("HA_MOVE", v)])]
            }
            ("pAHRotation", Some(v)) if v > 0.0 => {
                let v = v * if ha_move >= 0.0 { 1.0 } else { -1.0 };
                vec![number(agent, "AGENT_PLATESOLVER_PA_SETTINGS", &[("HA_MOVE", v)])]
            }
            ("pAHExposure", Some(v)) if v > 0.0 => vec![number(agent, "AGENT_PLATESOLVER_PA_SETTINGS", &[("EXPOSURE", v)])],
            ("pAHMountSpeed", _) => {
                if let Some(s) = value.as_str() {
                    self.pah_mount_speed = s.to_string();
                }
                Vec::new()
            }
            ("pAHRefreshAlgorithm", _) => {
                if let Some(s) = value.as_str() {
                    self.pah_refresh_algorithm = s.to_string();
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// `align_set_astrometry_settings {position, …}`: "use position" is the
    /// search radius around the mount's pointing; off means a blind solve.
    pub fn apply_astrometry(&mut self, payload: &Value, store: &PropertyStore, agent: &str) -> Vec<String> {
        let Some(use_position) = payload.get("position").and_then(Value::as_bool) else { return Vec::new() };
        let Some(h) = store.get(agent, "AGENT_PLATESOLVER_HINTS") else { return Vec::new() };
        let current = h.number("RADIUS").unwrap_or(0.0);
        let radius = if use_position { self.hints_radius } else { 0.0 };
        if (current - radius).abs() < 1e-9 {
            return Vec::new();
        }
        vec![number(agent, "AGENT_PLATESOLVER_HINTS", &[("RADIUS", radius)])]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    const A: &str = "Astrometry Agent";

    fn store() -> PropertyStore {
        let mut s = PropertyStore::default();
        for f in [
            r#"{"defSwitchVector":{"device":"Astrometry Agent","name":"AGENT_START_PROCESS","perm":"rw","state":"Ok","rule":"AtMostOne","items":[{"name":"SOLVE","value":false},{"name":"SYNC","value":false},{"name":"CENTER","value":false},{"name":"CALCULATE_PA_ERROR","value":false},{"name":"RECALCULATE_PA_ERROR","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Astrometry Agent","name":"AGENT_ABORT_PROCESS","perm":"rw","state":"Ok","items":[{"name":"ABORT","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_WCS","perm":"ro","state":"Ok","items":[{"name":"STATE","value":0},{"name":"RA","value":0},{"name":"DEC","value":0},{"name":"EPOCH","value":2000},{"name":"ANGLE","value":0},{"name":"WIDTH","value":0},{"name":"HEIGHT","value":0},{"name":"SCALE","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_PA_STATE","perm":"ro","state":"Ok","items":[{"name":"STATE","value":0},{"name":"ALT_POLAR_ERROR","value":0},{"name":"AZ_POLAR_ERROR","value":0},{"name":"POLAR_ERROR","value":0},{"name":"ALT_CORRECTION_UP","value":0},{"name":"AZ_CORRECTION_CW","value":0},{"name":"ACCURACY_WARNING","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_PA_SETTINGS","perm":"rw","state":"Ok","items":[{"name":"EXPOSURE","value":1},{"name":"HA_MOVE","value":20},{"name":"COMPENSATE_REFRACTION","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_EXPOSURE","perm":"rw","state":"Ok","items":[{"name":"EXPOSURE","value":1}]}}"#,
            r#"{"defNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_MOUNT_SETTLE_TIME","perm":"rw","state":"Ok","items":[{"name":"SETTLE_TIME","value":3}]}}"#,
            r#"{"defNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_HINTS","perm":"rw","state":"Ok","items":[{"name":"RADIUS","value":30},{"name":"RA","value":0},{"name":"DEC","value":0}]}}"#,
            r#"{"defSwitchVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_SOLVE_IMAGES","perm":"rw","state":"Ok","items":[{"name":"ENABLED","value":false},{"name":"DISABLED","value":true}]}}"#,
            r#"{"defSwitchVector":{"device":"Astrometry Agent","name":"CCD_PREVIEW","perm":"rw","state":"Ok","items":[{"name":"DISABLED","value":false},{"name":"ENABLED","value":true}]}}"#,
            r#"{"defBLOBVector":{"device":"Astrometry Agent","name":"CCD_PREVIEW_IMAGE","perm":"ro","state":"Idle","items":[{"name":"IMAGE"}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"CCD_BIN","perm":"rw","state":"Ok","items":[{"name":"HORIZONTAL","value":1},{"name":"VERTICAL","value":1}]}}"#,
            r#"{"defNumberVector":{"device":"Mount","name":"MOUNT_EQUATORIAL_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"RA","value":1},{"name":"DEC","value":2}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        s
    }

    fn set(s: &mut PropertyStore, frame: &str) -> Change {
        s.apply(parse_frame(frame).unwrap()).unwrap()
    }

    fn rig() -> Rig<'static> {
        Rig { mount_ready: true, camera_ready: true, mount: Some("Mount") }
    }

    fn wcs(s: &mut PropertyStore, state: &str, st: f64) -> Change {
        set(s, &format!(r#"{{"setNumberVector":{{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_WCS","state":"{state}","items":[{{"name":"STATE","value":{st}}},{{"name":"RA","value":5.5}},{{"name":"DEC","value":-5.4}},{{"name":"ANGLE","value":12.5}},{{"name":"WIDTH","value":2.0}},{{"name":"HEIGHT","value":1.5}},{{"name":"SCALE","value":0.001}}]}}}}"#))
    }

    fn pa(s: &mut PropertyStore, state: &str, st: f64) -> Change {
        set(s, &format!(r#"{{"setNumberVector":{{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_PA_STATE","state":"{state}","items":[{{"name":"STATE","value":{st}}},{{"name":"ALT_POLAR_ERROR","value":0.5}},{{"name":"AZ_POLAR_ERROR","value":-0.3}},{{"name":"POLAR_ERROR","value":0.583}},{{"name":"ALT_CORRECTION_UP","value":1}},{{"name":"AZ_CORRECTION_CW","value":0}}]}}}}"#))
    }

    fn mount_busy(s: &mut PropertyStore, busy: bool) -> Change {
        set(s, &format!(r#"{{"setNumberVector":{{"device":"Mount","name":"MOUNT_EQUATORIAL_COORDINATES","state":"{}","items":[]}}}}"#, if busy { "Busy" } else { "Ok" }))
    }

    fn msgs(r: &Reaction, ty: &str) -> Vec<Value> {
        r.to_browser
            .iter()
            .filter_map(|m| {
                let v: Value = serde_json::from_str(m).unwrap();
                (v["type"] == ty).then(|| v["payload"].clone())
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
    fn solve_centers_and_pairs_the_frame_with_the_solution() {
        let mut s = store();
        let r = rig();
        let mut a = Align::default();
        let (w, warn) = a.solve(&s, A, &r).unwrap();
        assert!(warn.is_none());
        assert_eq!(kinds(&w), ["newSwitchVector:AGENT_START_PROCESS:CENTER"]);
        let t0 = Instant::now();
        let c = wcs(&mut s, "Busy", WCS_WAITING);
        assert_eq!(msgs(&a.on_change(&c, &s, A, &r, t0), "new_align_state")[0]["status"], "In Progress");
        let c = set(&mut s, r#"{"setBLOBVector":{"device":"Astrometry Agent","name":"CCD_PREVIEW_IMAGE","state":"Ok","items":[{"name":"IMAGE","value":"/blob/0x5.jpeg"}]}}"#);
        assert_eq!(a.on_change(&c, &s, A, &r, t0).fetch_preview.as_deref(), Some("/blob/0x5.jpeg"));
        let c = wcs(&mut s, "Busy", WCS_SOLVING);
        assert!(msgs(&a.on_change(&c, &s, A, &r, t0), "new_align_state").is_empty(), "same status");
        let c = wcs(&mut s, "Busy", WCS_SYNCING);
        assert_eq!(msgs(&a.on_change(&c, &s, A, &r, t0), "new_align_state")[0]["status"], "Syncing");
        let c = wcs(&mut s, "Busy", WCS_CENTERING);
        assert_eq!(msgs(&a.on_change(&c, &s, A, &r, t0), "new_align_state")[0]["status"], "Slewing");
        // Done before the frame landed: the solution waits for it.
        let c = wcs(&mut s, "Ok", WCS_IDLE);
        let m = msgs(&a.on_change(&c, &s, A, &r, t0), "new_align_state");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["status"], "Successful");
        assert!(m[0].get("solution").is_none());
        let out = a.frame_delivered((800, 600));
        assert_eq!(out.len(), 1);
        let v: Value = serde_json::from_str(&out[0]).unwrap();
        let sol = &v["payload"]["solution"];
        // J2000 → JNow moves it a little; PA, scale and FOV pass straight.
        assert!((sol["ra.Hours"].as_f64().unwrap() - 5.5).abs() < 0.05);
        assert!((sol["de.Degrees"].as_f64().unwrap() + 5.4).abs() < 0.5);
        assert_eq!(sol["PA"], 12.5);
        assert!((sol["pix"].as_f64().unwrap() - 3.6).abs() < 1e-9);
        assert_eq!(sol["fov"], "120.0' x 90.0'");
        assert!(a.tick(t0 + FRAME_HOLD).is_empty());

        // Frame already delivered: the next solution goes out at once.
        let c = wcs(&mut s, "Busy", WCS_SOLVING);
        a.on_change(&c, &s, A, &r, t0);
        let c = wcs(&mut s, "Ok", WCS_IDLE);
        let m = msgs(&a.on_change(&c, &s, A, &r, t0), "new_align_state");
        assert!(m.iter().any(|p| p.get("solution").is_some()));

        // Held solution whose frame never comes: released by the tick.
        let c = set(&mut s, r#"{"setBLOBVector":{"device":"Astrometry Agent","name":"CCD_PREVIEW_IMAGE","state":"Ok","items":[{"name":"IMAGE","value":"/blob/0x6.jpeg"}]}}"#);
        a.on_change(&c, &s, A, &r, t0);
        let c = wcs(&mut s, "Busy", WCS_SOLVING);
        a.on_change(&c, &s, A, &r, t0);
        let c = wcs(&mut s, "Ok", WCS_IDLE);
        a.on_change(&c, &s, A, &r, t0);
        assert!(a.tick(t0 + Duration::from_secs(1)).is_empty());
        assert_eq!(a.tick(t0 + FRAME_HOLD).len(), 1);
    }

    #[test]
    fn failure_abort_and_fallbacks() {
        let mut s = store();
        let r = rig();
        let mut a = Align::default();
        let t0 = Instant::now();
        a.solve(&s, A, &r).unwrap();
        let c = wcs(&mut s, "Busy", WCS_SOLVING);
        a.on_change(&c, &s, A, &r, t0);
        let c = set(&mut s, r#"{"setNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_WCS","state":"Alert","message":"No solution found","items":[]}}"#);
        let m = msgs(&a.on_change(&c, &s, A, &r, t0), "new_align_state");
        assert_eq!(m[0]["status"], "Failed");
        assert_eq!(m[0]["log"], "No solution found");
        a.solve(&s, A, &r).unwrap();
        let c = wcs(&mut s, "Busy", WCS_SOLVING);
        a.on_change(&c, &s, A, &r, t0);
        assert_eq!(kinds(&a.stop(A)), ["newSwitchVector:AGENT_ABORT_PROCESS:ABORT"]);
        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Astrometry Agent","name":"AGENT_START_PROCESS","state":"Alert","message":"Process aborted","items":[]}}"#);
        // The process was never seen Busy here, so the WCS carries the edge.
        a.on_change(&c, &s, A, &r, t0);
        let c = set(&mut s, r#"{"setNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_WCS","state":"Alert","items":[]}}"#);
        assert_eq!(msgs(&a.on_change(&c, &s, A, &r, t0), "new_align_state")[0]["status"], "Aborted");

        // No mount in the Mount Agent: solve only, with a warning.
        set(&mut s, r#"{"setNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_WCS","state":"Ok","items":[]}}"#);
        let no_mount = Rig { mount_ready: false, camera_ready: true, mount: None };
        let (w, warn) = a.solve(&s, A, &no_mount).unwrap();
        assert_eq!(kinds(&w), ["newSwitchVector:AGENT_START_PROCESS:SOLVE"]);
        assert!(warn.is_some());
        assert!(a.polar_start(&s, A, &no_mount).is_err());
        let no_cam = Rig { mount_ready: true, camera_ready: false, mount: None };
        assert!(a.solve(&s, A, &no_cam).is_err());
    }

    #[test]
    fn polar_alignment_stages_and_signs() {
        let mut s = store();
        let r = rig();
        let mut a = Align::default();
        let t0 = Instant::now();
        assert_eq!(kinds(&a.polar_start(&s, A, &r).unwrap()), ["newSwitchVector:AGENT_START_PROCESS:CALCULATE_PA_ERROR"]);
        let mut stages: Vec<(String, Value)> = Vec::new();
        fn observe(a: &mut Align, s: &PropertyStore, c: Change, r: &Rig, t0: Instant, stages: &mut Vec<(String, Value)>) {
            for p in msgs(&a.on_change(&c, s, A, r, t0), "new_polar_state") {
                stages.push((p["stage"].as_str().unwrap().to_string(), p));
            }
        }
        let step = |a: &mut Align, s: &mut PropertyStore, c: Change, stages: &mut Vec<(String, Value)>| observe(a, s, c, &r, t0, stages);
        let c = pa(&mut s, "Busy", PA_REFERENCE_1);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Busy", WCS_WAITING);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Busy", WCS_SOLVING);
        step(&mut a, &mut s, c, &mut stages);
        let c = pa(&mut s, "Busy", PA_REFERENCE_2);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Busy", WCS_CENTERING);
        step(&mut a, &mut s, c, &mut stages);
        let c = mount_busy(&mut s, true);
        step(&mut a, &mut s, c, &mut stages);
        let c = mount_busy(&mut s, false);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Busy", WCS_WAITING);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Busy", WCS_SOLVING);
        step(&mut a, &mut s, c, &mut stages);
        let c = pa(&mut s, "Busy", PA_REFERENCE_3);
        step(&mut a, &mut s, c, &mut stages);
        let c = mount_busy(&mut s, true);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Busy", WCS_CENTERING);
        step(&mut a, &mut s, c, &mut stages);
        let c = mount_busy(&mut s, false);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Busy", WCS_WAITING);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Busy", WCS_SOLVING);
        step(&mut a, &mut s, c, &mut stages);
        // The errors land while the process is still Busy: no stage yet.
        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Astrometry Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"CALCULATE_PA_ERROR","value":true}]}}"#);
        step(&mut a, &mut s, c, &mut stages);
        let c = pa(&mut s, "Ok", PA_IN_PROGRESS);
        step(&mut a, &mut s, c, &mut stages);
        let c = wcs(&mut s, "Ok", WCS_IDLE);
        step(&mut a, &mut s, c, &mut stages);
        assert_ne!(stages.last().unwrap().0, "Select Star");
        let c = set(&mut s, r#"{"setSwitchVector":{"device":"Astrometry Agent","name":"AGENT_START_PROCESS","state":"Ok","items":[{"name":"CALCULATE_PA_ERROR","value":false}]}}"#);
        step(&mut a, &mut s, c, &mut stages);
        let names: Vec<&str> = stages.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "First Capture", "First Solve", "Second Capture", "First Settle", "First Rotation", "First Settle",
                "Second Capture", "Second Solve", "Third Capture", "Second Rotation", "Second Settle",
                "Third Capture", "Third Solve", "Select Star"
            ],
            "{names:?}"
        );
        let select = &stages.last().unwrap().1;
        let v = &select["vector"];
        assert!((v["error"].as_f64().unwrap() - 0.583).abs() < 1e-9);
        // CW correction → negative azimuth (arrow "→"); not CW → positive ("←").
        assert!((v["azError"].as_f64().unwrap() - 0.3).abs() < 1e-9);
        // "Up" correction → negative altitude (arrow "↑").
        assert!((v["altError"].as_f64().unwrap() + 0.5).abs() < 1e-9);
        assert_eq!(select["enabled"], true);
        assert!(select["message"].as_str().unwrap().contains("counter-clockwise"));

        // Refresh: Refreshing → Refresh Complete with the updated errors.
        assert_eq!(kinds(&a.polar_refresh(&s, A, Some(2.0)).unwrap()), ["newNumberVector:AGENT_PLATESOLVER_PA_SETTINGS:EXPOSURE", "newSwitchVector:AGENT_START_PROCESS:RECALCULATE_PA_ERROR"]);
        let mut refreshed = Vec::new();
        let c = pa(&mut s, "Busy", PA_RECALCULATE);
        refreshed.extend(msgs(&a.on_change(&c, &s, A, &r, t0), "new_polar_state"));
        let c = pa(&mut s, "Ok", PA_IN_PROGRESS);
        refreshed.extend(msgs(&a.on_change(&c, &s, A, &r, t0), "new_polar_state"));
        assert_eq!(refreshed[0]["stage"], "Refreshing");
        assert_eq!(refreshed[1]["stage"], "Refresh Complete");
        assert!((refreshed[1]["updatedALTError"].as_f64().unwrap() + 0.5).abs() < 1e-9);
        // A failed refresh reports −1.
        let c = pa(&mut s, "Busy", PA_RECALCULATE);
        a.on_change(&c, &s, A, &r, t0);
        let c = set(&mut s, r#"{"setNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_PA_STATE","state":"Alert","message":"Solving failed","items":[{"name":"STATE","value":6}]}}"#);
        let m = msgs(&a.on_change(&c, &s, A, &r, t0), "new_polar_state");
        assert_eq!(m[0]["stage"], "Refresh Complete");
        assert_eq!(m[0]["updatedError"], -1.0);
        // The mount moved: the agent drops to IDLE.
        let c = set(&mut s, r#"{"setNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_PA_STATE","state":"Alert","message":"Polar alignment reset","items":[{"name":"STATE","value":0}]}}"#);
        let m = msgs(&a.on_change(&c, &s, A, &r, t0), "new_polar_state");
        assert_eq!(m[0]["stage"], "Idle");
        assert_eq!(m[0]["enabled"], false);
        assert!(a.polar_refresh(&s, A, None).is_err());
    }

    #[test]
    fn settings_round_trip() {
        let mut s = store();
        let mut a = Align::default();
        let p = a.settings_payload(&s, A, Some("Imager Agent"));
        assert_eq!(p["slewR"], true);
        assert_eq!(p["alignExposure"], 1.0);
        assert_eq!(p["alignSettlingTime"], 3000.0);
        assert_eq!(p["alignBinning"], "1x1");
        assert_eq!(p["pAHDirection"], "West");
        assert_eq!(p["pAHRotation"], 20.0);
        assert_eq!(p["pAHExposure"], 1.0);
        assert_eq!(p["pAHManualSlew"], false);
        assert_eq!(p["localSolverR"], true);
        a.apply_setting("syncR", &json!(true), &s, A, None);
        a.apply_setting("slewR", &json!(false), &s, A, None);
        assert_eq!(a.action, Action::Sync);
        assert_eq!(kinds(&a.apply_setting("alignSettlingTime", &json!(1500), &s, A, None)), ["newNumberVector:AGENT_PLATESOLVER_MOUNT_SETTLE_TIME:SETTLE_TIME"]);
        let w = a.apply_setting("pAHDirection", &json!("East"), &s, A, None);
        assert!(w[0].contains(r#""value":-20.0"#), "{w:?}");
        set(&mut s, r#"{"setNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_PA_SETTINGS","state":"Ok","items":[{"name":"HA_MOVE","value":-20}]}}"#);
        let w = a.apply_setting("pAHRotation", &json!(30), &s, A, None);
        assert!(w[0].contains(r#""value":-30.0"#), "{w:?}");
        assert_eq!(a.settings_payload(&s, A, None)["pAHDirection"], "East");
        assert_eq!(kinds(&a.apply_setting("alignBinning", &json!("2x2"), &s, A, Some("Imager Agent"))), ["newNumberVector:CCD_BIN:HORIZONTAL"]);
        // Blind solve on/off through the hints radius.
        let w = a.apply_astrometry(&json!({"position": false, "scale": true}), &s, A);
        assert!(w[0].contains(r#""value":0.0"#));
        set(&mut s, r#"{"setNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_HINTS","state":"Ok","items":[{"name":"RADIUS","value":0}]}}"#);
        let w = a.apply_astrometry(&json!({"position": true}), &s, A);
        assert!(w[0].contains(r#""value":30.0"#));
    }
}
