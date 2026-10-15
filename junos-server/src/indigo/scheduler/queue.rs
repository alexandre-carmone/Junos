//! The Scheduler's startup / shutdown procedures: KStars task queues, run
//! against INDIGO devices.
//!
//! KStars 3.8 keeps each of the Scheduler's four procedure slots in a JSON
//! file that `QueueManager::loadQueue` reads in one of two shapes — a
//! *collection* naming system templates (`{"tasks":[{template_id, device,
//! failure_action, parameters}]}`) or a *queue* with the actions spelled out
//! (`{"items":[{"task":{…, "actions":[…]}}]}`) — and runs it through
//! `QueueExecutor` (`kstars/ekos/scheduler/taskqueue/`). This module is that
//! executor for the INDIGO backend: [`parse`] expands the templates (embedded
//! verbatim from `kstars/data/taskqueue/templates/system/`) into [`Step`]s,
//! and a [`Procedure`] drives them one [`Procedure::advance`] at a time — the
//! session calls it on its 1 s tick with the property mirror and gets back the
//! INDIGO writes to send, the scripts to spawn and the log lines.
//!
//! The files carry INDI property names (`TELESCOPE_PARK.PARK`) while the
//! devices are INDIGO's: [`indigo_target`] renames what the templates write
//! and what `alias.rs` already knows, and lets anything else through — a
//! custom step built from the Devices tab's listing already uses INDIGO names.
//!
//! Semantics follow KStars' actions (`taskqueue/actions/*.cpp`): a `SET`
//! completes once the property is seen in any state but Alert (the value is
//! the following `EVALUATE`'s business — INDIGO's `CCD_TEMPERATURE` stays
//! Busy for a whole cool-down), an `EVALUATE` polls until its condition holds,
//! each with `retries` more attempts after a timeout, then the action's
//! `failure_action` decides: abort the queue, continue, or skip the task.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::super::alias;
use super::super::protocol::{interface, new_vector_msg, ItemValue, Kind, PropertyStore, State};
use super::super::roles::Roles;

// ── Slots and steps ──────────────────────────────────────────────────────────

/// The Scheduler's four procedure slots, in the order they run over a night.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    PreStartup,
    PostStartup,
    PreShutdown,
    PostShutdown,
}

impl Slot {
    pub fn label(self) -> &'static str {
        match self {
            Self::PreStartup => "pre-startup",
            Self::PostStartup => "post-startup",
            Self::PreShutdown => "pre-shutdown",
            Self::PostShutdown => "post-shutdown",
        }
    }

    /// The log prefix: the two startup slots never run at the same time, nor
    /// the two shutdown ones.
    fn prefix(self) -> &'static str {
        match self {
            Self::PreStartup | Self::PostStartup => "Startup",
            Self::PreShutdown | Self::PostShutdown => "Shutdown",
        }
    }
}

/// `TaskAction::FailureAction`.
pub const FAIL_ABORT: u8 = 0;
pub const FAIL_CONTINUE: u8 = 1;
pub const FAIL_SKIP: u8 = 2;

/// One thing the procedure does. KStars nests `DELAY` and `SCRIPT` actions
/// inside tasks; here they are steps of their own, so a task is only ever
/// property work on one device.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Script { path: PathBuf, timeout: Duration, retries: u8, failure_action: u8 },
    Delay(Duration),
    Task {
        name: String,
        /// Empty: resolved at run time from the first action's property.
        device: String,
        /// The template's `supported_interfaces`, OR'ed (INDI bits) — informative.
        interfaces: u32,
        /// What to do when no device can be found for the task.
        failure_action: u8,
        actions: Vec<Action>,
    },
}

/// `SetAction` / `EvaluateAction`, with the file's (INDI) property names.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Set {
        property: String,
        element: String,
        /// `bool` for a switch, a number, or text.
        value: Value,
        timeout: Duration,
        retries: u8,
        failure_action: u8,
        /// `wait_for_completion`.
        wait: bool,
    },
    Evaluate {
        property: String,
        element: String,
        /// `EvaluateAction::PropertyType`: 0 number, 1 text, 2 switch, 3 light, 4 state.
        kind: u8,
        /// `EvaluateAction::ConditionType`: 0 =, 1 ≠, 2 >, 3 <, 4 ≥, 5 ≤, 6 within, 7 contains, 8 starts with.
        cond: u8,
        target: Value,
        margin: Option<f64>,
        timeout: Duration,
        retries: u8,
        failure_action: u8,
    },
}

impl Action {
    fn property(&self) -> &str {
        match self {
            Self::Set { property, .. } | Self::Evaluate { property, .. } => property,
        }
    }

    fn element(&self) -> &str {
        match self {
            Self::Set { element, .. } | Self::Evaluate { element, .. } => element,
        }
    }

    fn failure_action(&self) -> u8 {
        match self {
            Self::Set { failure_action, .. } | Self::Evaluate { failure_action, .. } => *failure_action,
        }
    }
}

// ── Templates ────────────────────────────────────────────────────────────────

/// `kstars/kstars/data/taskqueue/templates/system/*.json`, verbatim.
const TEMPLATE_FILES: &[&str] = &[
    include_str!("templates/camera.json"),
    include_str!("templates/delay.json"),
    include_str!("templates/dome.json"),
    include_str!("templates/dustcap.json"),
    include_str!("templates/mount.json"),
    include_str!("templates/script.json"),
];

struct Param {
    name: String,
    ty: String,
    min: Option<f64>,
    max: Option<f64>,
}

struct Template {
    id: String,
    interfaces: u32,
    params: Vec<Param>,
    actions: Vec<Value>,
}

fn templates() -> &'static [Template] {
    static ALL: OnceLock<Vec<Template>> = OnceLock::new();
    ALL.get_or_init(|| {
        TEMPLATE_FILES
            .iter()
            .flat_map(|text| {
                let file: Value = serde_json::from_str(text).expect("embedded KStars template");
                file["templates"].as_array().cloned().unwrap_or_default()
            })
            .map(|t| Template {
                id: t["id"].as_str().unwrap_or_default().to_string(),
                interfaces: t["supported_interfaces"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_u64).fold(0, |acc, i| acc | i as u32))
                    .unwrap_or(0),
                params: t["parameters"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|p| Param {
                                name: p["name"].as_str().unwrap_or_default().to_string(),
                                ty: p["type"].as_str().unwrap_or("number").to_string(),
                                min: p["min"].as_f64(),
                                max: p["max"].as_f64(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                actions: t["actions"].as_array().cloned().unwrap_or_default(),
            })
            .collect()
    })
}

fn template(id: &str) -> Option<&'static Template> {
    templates().iter().find(|t| t.id == id)
}

/// A number as the files carry it: a JSON number, or a numeric string left by
/// a `${param}` substitution.
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A switch value as `QVariant::toBool` reads it.
fn as_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "on" | "yes"),
        _ => false,
    }
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A value for the log: `On` / `Off` for a switch.
fn show(v: &Value) -> String {
    match v {
        Value::Bool(true) => "On".into(),
        Value::Bool(false) => "Off".into(),
        other => as_text(other),
    }
}

/// `Task::substituteValue`: a string that is exactly `${name}` becomes that
/// parameter with its type; one embedding placeholders is spelled out, and
/// becomes a number when the result reads as one. Objects and arrays recurse.
fn substitute(v: &Value, params: &Map<String, Value>) -> Value {
    match v {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
                if let Some(p) = params.get(name) {
                    return p.clone();
                }
            }
            let mut out = s.clone();
            for (name, p) in params {
                out = out.replace(&format!("${{{name}}}"), &as_text(p));
            }
            if out != *s {
                if let Ok(n) = out.trim().parse::<f64>() {
                    return json!(n);
                }
            }
            Value::String(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|x| substitute(x, params)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), substitute(x, params))).collect()),
        other => other.clone(),
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────────

/// Either file shape into steps, plus one warning per task or action KStars
/// would have dropped silently.
pub fn parse(json: &str) -> Result<(Vec<Step>, Vec<String>), String> {
    let doc: Value = serde_json::from_str(json).map_err(|e| format!("not JSON: {e}"))?;
    let mut warnings = Vec::new();
    let mut steps = Vec::new();
    if let Some(tasks) = doc.get("tasks").and_then(Value::as_array) {
        for (i, task) in tasks.iter().enumerate() {
            steps.extend(collection_task(i, task, &mut warnings));
        }
    } else if let Some(items) = doc.get("items").and_then(Value::as_array) {
        for (i, item) in items.iter().enumerate() {
            steps.extend(native_task(i, &item["task"], &mut warnings));
        }
    } else {
        return Err("neither a task collection (\"tasks\") nor a task queue (\"items\")".into());
    }
    Ok((steps, warnings))
}

pub fn load(path: &Path) -> Result<(Vec<Step>, Vec<String>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text)
}

fn failure_action_of(v: &Value) -> u8 {
    v.as_u64().filter(|f| *f <= u64::from(FAIL_SKIP)).map(|f| f as u8).unwrap_or(FAIL_ABORT)
}

/// A collection task: `QueueManager::loadCollectionFromJson` +
/// `Task::instantiateFromTemplate`, warnings where KStars just `continue`s.
fn collection_task(i: usize, task: &Value, warnings: &mut Vec<String>) -> Vec<Step> {
    let id = task["template_id"].as_str().unwrap_or_default();
    let Some(tmpl) = template(id) else {
        warnings.push(format!("task {}: unknown template_id \"{id}\", skipped", i + 1));
        return Vec::new();
    };
    let params = task["parameters"].as_object().cloned().unwrap_or_default();
    // `TaskTemplate::validateParameters`, same wording.
    for p in &tmpl.params {
        let Some(v) = params.get(&p.name) else {
            warnings.push(format!("task {} ({id}) dropped: Missing required parameter: {}", i + 1, p.name));
            return Vec::new();
        };
        if p.ty == "number" {
            let Some(n) = num(v) else {
                warnings.push(format!("task {} ({id}) dropped: Parameter {} must be a number", i + 1, p.name));
                return Vec::new();
            };
            if p.min.is_some_and(|min| n < min) {
                warnings.push(format!("task {} ({id}) dropped: Parameter {} must be >= {}", i + 1, p.name, p.min.unwrap_or(0.0)));
                return Vec::new();
            }
            if p.max.is_some_and(|max| n > max) {
                warnings.push(format!("task {} ({id}) dropped: Parameter {} must be <= {}", i + 1, p.name, p.max.unwrap_or(0.0)));
                return Vec::new();
            }
        }
    }
    let actions: Vec<Value> = tmpl.actions.iter().map(|a| substitute(a, &params)).collect();
    steps_of(
        i,
        id.to_string(),
        task["device"].as_str().unwrap_or_default().to_string(),
        tmpl.interfaces,
        failure_action_of(&task["failure_action"]),
        &actions,
        warnings,
    )
}

/// A queue item's task: `Task::loadFromJson`, the actions as written.
fn native_task(i: usize, task: &Value, warnings: &mut Vec<String>) -> Vec<Step> {
    let template_id = task["template_id"].as_str().unwrap_or_default();
    let name = task["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or(Some(template_id).filter(|s| !s.is_empty()))
        .unwrap_or("task")
        .to_string();
    let Some(actions) = task["actions"].as_array().filter(|a| !a.is_empty()) else {
        warnings.push(format!("task {} ({name}) dropped: no actions", i + 1));
        return Vec::new();
    };
    // KStars never substitutes in this shape; a file that still carries
    // placeholders next to its parameters is only helped by it.
    let params = task["parameters"].as_object().cloned().unwrap_or_default();
    let actions: Vec<Value> = actions.iter().map(|a| substitute(a, &params)).collect();
    let interfaces = task["device_interface"]
        .as_u64()
        .map(|v| v as u32)
        .or_else(|| template(template_id).map(|t| t.interfaces))
        .unwrap_or(0);
    steps_of(
        i,
        name,
        task["device"].as_str().unwrap_or_default().to_string(),
        interfaces,
        failure_action_of(&task["failure_action"]),
        &actions,
        warnings,
    )
}

/// A task's resolved action objects into steps: the `SET` / `EVALUATE` runs
/// stay one task, each `DELAY` / `SCRIPT` cuts it.
fn steps_of(
    i: usize,
    name: String,
    device: String,
    interfaces: u32,
    failure_action: u8,
    actions: &[Value],
    warnings: &mut Vec<String>,
) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut pending: Vec<Action> = Vec::new();
    let flush = |pending: &mut Vec<Action>, steps: &mut Vec<Step>| {
        if !pending.is_empty() {
            steps.push(Step::Task {
                name: name.clone(),
                device: device.clone(),
                interfaces,
                failure_action,
                actions: std::mem::take(pending),
            });
        }
    };
    let secs = |v: &Value, default: f64| Duration::from_secs_f64(num(v).unwrap_or(default).max(0.0));
    let retries = |v: &Value| num(v).map(|n| n.clamp(0.0, 255.0) as u8).unwrap_or(2);
    for a in actions {
        let ty = a["type"].as_str().unwrap_or_default();
        let property = a["property"].as_str().unwrap_or_default().to_string();
        let element = a["element"].as_str().unwrap_or_default().to_string();
        match ty {
            "SET" | "EVALUATE" if property.is_empty() => {
                warnings.push(format!("task {} ({name}): {ty} without a property, skipped", i + 1));
            }
            "SET" => pending.push(Action::Set {
                property,
                element,
                value: a["value"].clone(),
                timeout: secs(&a["timeout"], 30.0),
                retries: retries(&a["retries"]),
                failure_action: failure_action_of(&a["failure_action"]),
                wait: a["wait_for_completion"].as_bool().unwrap_or(true),
            }),
            "EVALUATE" => pending.push(Action::Evaluate {
                property,
                element,
                kind: num(&a["property_type"]).unwrap_or(0.0) as u8,
                cond: num(&a["condition"]).unwrap_or(0.0) as u8,
                target: a["target"].clone(),
                margin: num(&a["margin"]),
                timeout: secs(&a["timeout"], 30.0),
                retries: retries(&a["retries"]),
                failure_action: failure_action_of(&a["failure_action"]),
            }),
            "DELAY" => {
                flush(&mut pending, &mut steps);
                let unit = match num(&a["unit"]).unwrap_or(0.0) as u8 {
                    1 => 60.0,
                    2 => 3600.0,
                    _ => 1.0,
                };
                steps.push(Step::Delay(Duration::from_secs_f64((num(&a["duration"]).unwrap_or(0.0) * unit).max(0.0))));
            }
            "SCRIPT" => {
                flush(&mut pending, &mut steps);
                steps.push(Step::Script {
                    path: PathBuf::from(a["script_path"].as_str().unwrap_or_default()),
                    timeout: secs(&a["timeout"], 300.0),
                    retries: retries(&a["retries"]),
                    failure_action: failure_action_of(&a["failure_action"]),
                });
            }
            other => warnings.push(format!("task {} ({name}): action type \"{other}\" not supported, skipped", i + 1)),
        }
    }
    flush(&mut pending, &mut steps);
    steps
}

// ── INDI → INDIGO ────────────────────────────────────────────────────────────

/// The INDIGO property and element for a file's INDI names: the ones KStars'
/// templates write, then `alias.rs`'s table, then the name itself. `None`
/// when INDIGO has no such thing (`CCD_TEMP_RAMP`: INDIGO cameras ramp on
/// their own).
pub fn indigo_target(property: &str, element: &str) -> Option<(String, String)> {
    const RENAMES: &[(&str, &str, &[(&str, &str)])] = &[
        ("TELESCOPE_PARK", "MOUNT_PARK", &[("PARK", "PARKED"), ("UNPARK", "UNPARKED")]),
        ("TELESCOPE_TRACK_STATE", "MOUNT_TRACKING", &[("TRACK_ON", "ON"), ("TRACK_OFF", "OFF")]),
        ("DOME_PARK", "DOME_PARK", &[("PARK", "PARKED"), ("UNPARK", "UNPARKED")]),
        ("CAP_PARK", "AUX_COVER", &[("PARK", "CLOSE"), ("UNPARK", "OPEN")]),
    ];
    if property == "CCD_TEMP_RAMP" {
        return None;
    }
    if let Some((_, indigo, elements)) = RENAMES.iter().find(|(indi, ..)| *indi == property) {
        let element = elements.iter().find(|(i, _)| *i == element).map(|(_, o)| *o).unwrap_or(element);
        return Some((indigo.to_string(), element.to_string()));
    }
    let (name, rename) = alias::write_target(property);
    let element = rename(element);
    Some((name, element))
}

/// The device a task runs on. Named: it must be there and connected. Empty:
/// `QueueExecutor` binds the first connected device with the template's
/// interfaces; here the first action's (INDIGO) property decides, through
/// the roles the agents hold.
fn resolve_device(store: &PropertyStore, roles: &Roles, device: &str, actions: &[Action]) -> Result<String, String> {
    if !device.is_empty() {
        if store.connected(device) {
            return Ok(device.to_string());
        }
        return Err(if store.device_names().any(|d| d == device) {
            format!("Device {device} not connected")
        } else {
            format!("Device {device} not found")
        });
    }
    let Some(property) = actions.iter().find_map(|a| indigo_target(a.property(), a.element()).map(|(p, _)| p)) else {
        return Err("No device to run the task on".into());
    };
    let first = |pred: &dyn Fn(&str) -> bool| store.device_names().find(|d| store.connected(d) && pred(d)).map(str::to_string);
    let role = if property.starts_with("MOUNT_") {
        roles.mount.clone()
    } else if property.starts_with("CCD_") {
        roles.camera.clone()
    } else if property.starts_with("DOME_") {
        first(&|d| store.interface(d) & interface::DOME != 0)
    } else if property == "AUX_COVER" {
        first(&|d| store.interface(d) & interface::DUSTCAP == interface::DUSTCAP || store.get(d, "AUX_COVER").is_some())
    } else {
        None
    };
    role.filter(|d| store.connected(d))
        .or_else(|| first(&|d| store.get(d, &property).is_some()))
        .ok_or_else(|| format!("No connected device found for {property}"))
}

// ── Execution ────────────────────────────────────────────────────────────────

/// A script for the runner to spawn — no arguments, nothing on stdin, like
/// `KSUtils::startProcess` — and report back through
/// [`Procedure::script_finished`], killed after `timeout`.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptReq {
    pub id: u64,
    pub path: PathBuf,
    pub timeout: Duration,
}

/// What one [`Procedure::advance`] produced.
#[derive(Debug, Default)]
pub struct Advance {
    pub to_indigo: Vec<String>,
    pub scripts: Vec<ScriptReq>,
    pub log: Vec<String>,
    /// Set on the call that ends the procedure, and repeated by every later one.
    pub finished: Option<Result<(), String>>,
}

/// What the current step is waiting on.
#[derive(Debug)]
enum Phase {
    /// Nothing in flight: the next `advance` starts the action at the cursor.
    Idle,
    /// A `SET` went out at `since` (attempt n): one poll interval later the
    /// property's state tells.
    SetWait { since: Instant, attempt: u8 },
    /// An `EVALUATE` polling since `since` (attempt n); `reported` is the
    /// last 10 s mark logged.
    EvalWait { since: Instant, attempt: u8, reported: u64 },
    Delay { until: Instant },
    /// Script `id` running (attempt n), its outcome once the runner reports it.
    Script { id: u64, attempt: u8, outcome: Option<Result<i32, String>> },
}

/// How an action call ended.
enum Flow {
    Wait,
    Next,
    Fail(String),
}

pub struct Procedure {
    slot: Slot,
    steps: Vec<Step>,
    cursor: usize,
    /// Within the current task.
    action: usize,
    /// The current task's device, resolved when it starts.
    device: Option<String>,
    phase: Phase,
    next_script_id: u64,
    finished: Option<Result<(), String>>,
}

/// `SetAction` polls once a second: the write has to reach the device and
/// its echo the mirror before the state means anything.
const SET_POLL: Duration = Duration::from_secs(1);

impl Procedure {
    pub fn new(slot: Slot, steps: Vec<Step>, first_script_id: u64) -> Self {
        Self {
            slot,
            steps,
            cursor: 0,
            action: 0,
            device: None,
            phase: Phase::Idle,
            next_script_id: first_script_id,
            finished: None,
        }
    }

    /// Drive the procedure: issue the next write / script / wait, poll the
    /// waits against the store. Called on every 1 s tick and right after
    /// [`Self::script_finished`]; idempotent while waiting.
    pub fn advance(&mut self, store: &PropertyStore, roles: &Roles, now: Instant) -> Advance {
        let mut out = Advance::default();
        loop {
            if let Some(f) = &self.finished {
                out.finished = Some(f.clone());
                return out;
            }
            let Some(step) = self.steps.get(self.cursor).cloned() else {
                self.finish(Ok(()), &mut out);
                return out;
            };
            let blocked = match step {
                Step::Delay(d) => self.run_delay(d, now, &mut out),
                Step::Script { path, timeout, retries, failure_action } => {
                    self.run_script(&path, timeout, retries, failure_action, &mut out)
                }
                Step::Task { name, device, failure_action, actions, .. } => {
                    self.run_task(&name, &device, failure_action, &actions, store, roles, now, &mut out)
                }
            };
            if blocked {
                return out;
            }
        }
    }

    /// The runner's report on a [`ScriptReq`]: the exit code, or why it
    /// couldn't run ("timed out", a spawn error). A stale id is ignored.
    pub fn script_finished(&mut self, id: u64, outcome: Result<i32, String>) {
        if let Phase::Script { id: running, outcome: slot, .. } = &mut self.phase {
            if *running == id && slot.is_none() {
                *slot = Some(outcome);
            }
        }
    }

    /// Stop here. Nothing in flight needs a write to stop — a park goes on,
    /// a running script is the runner's to kill — so this returns none.
    pub fn abort(&mut self) -> Vec<String> {
        if self.finished.is_none() {
            self.finished = Some(Err("aborted".into()));
        }
        Vec::new()
    }

    fn log(&self, out: &mut Advance, text: impl AsRef<str>) {
        out.log.push(format!("{}: {}", self.slot.prefix(), text.as_ref()));
    }

    fn finish(&mut self, result: Result<(), String>, out: &mut Advance) {
        match &result {
            Ok(()) => self.log(out, format!("{} procedure complete", self.slot.label())),
            Err(e) => self.log(out, format!("{} procedure failed: {e}", self.slot.label())),
        }
        self.finished = Some(result.clone());
        out.finished = Some(result);
    }

    fn next_step(&mut self) {
        self.cursor += 1;
        self.action = 0;
        self.device = None;
        self.phase = Phase::Idle;
    }

    /// `true` while the delay runs.
    fn run_delay(&mut self, d: Duration, now: Instant, out: &mut Advance) -> bool {
        match self.phase {
            Phase::Delay { until } if now >= until => {
                self.next_step();
                false
            }
            Phase::Delay { .. } => true,
            _ => {
                self.log(out, format!("waiting {} s", d.as_secs()));
                self.phase = Phase::Delay { until: now + d };
                true
            }
        }
    }

    /// `true` while the script runs.
    fn run_script(&mut self, path: &Path, timeout: Duration, retries: u8, failure_action: u8, out: &mut Advance) -> bool {
        let outcome = match &mut self.phase {
            Phase::Script { outcome: None, .. } => return true,
            Phase::Script { outcome, attempt, .. } => Some((outcome.take(), *attempt)),
            _ => None,
        };
        let (attempt, error) = match outcome {
            None => (0, None),
            Some((Some(Ok(0)), _)) => {
                self.log(out, format!("script {} exited with 0", path.display()));
                self.next_step();
                return false;
            }
            Some((Some(Ok(code)), attempt)) => {
                (attempt + 1, Some(format!("Script failed with exit code {code}: {}", path.display())))
            }
            Some((Some(Err(e)), attempt)) => (attempt + 1, Some(format!("Script {e}: {}", path.display()))),
            Some((None, _)) => unreachable!("taken above"),
        };
        if let Some(error) = error {
            if attempt > retries {
                return self.step_failed(error, failure_action, out);
            }
            self.log(out, format!("{error}, retry {attempt}/{retries}"));
        }
        if let Err(e) = validate_script(path) {
            // KStars retries a validation failure too, but nothing changes
            // between two looks at the same file.
            return self.step_failed(e, failure_action, out);
        }
        let id = self.next_script_id;
        self.next_script_id += 1;
        out.scripts.push(ScriptReq { id, path: path.to_path_buf(), timeout });
        self.log(out, format!("running script {}", path.display()));
        self.phase = Phase::Script { id, attempt, outcome: None };
        true
    }

    /// A script or task step failed: the queue, or just the step. `true`
    /// when the procedure is over.
    fn step_failed(&mut self, error: String, failure_action: u8, out: &mut Advance) -> bool {
        if failure_action == FAIL_ABORT {
            self.finish(Err(error), out);
            true
        } else {
            self.log(out, format!("{error} — continuing"));
            self.next_step();
            false
        }
    }

    /// `true` while an action waits (or the procedure ended).
    #[allow(clippy::too_many_arguments)]
    fn run_task(
        &mut self,
        name: &str,
        device: &str,
        failure_action: u8,
        actions: &[Action],
        store: &PropertyStore,
        roles: &Roles,
        now: Instant,
        out: &mut Advance,
    ) -> bool {
        if self.device.is_none() {
            match resolve_device(store, roles, device, actions) {
                Ok(d) => {
                    self.log(out, format!("{name} on {d}"));
                    self.device = Some(d);
                    self.action = 0;
                    self.phase = Phase::Idle;
                }
                Err(e) => return self.step_failed(format!("{name}: {e}"), failure_action, out),
            }
        }
        let device = self.device.clone().unwrap_or_default();
        loop {
            let Some(action) = actions.get(self.action) else {
                self.next_step();
                return false;
            };
            let flow = match action {
                Action::Set { .. } => self.run_set(&device, action, store, now, out),
                Action::Evaluate { .. } => self.run_eval(&device, action, store, now, out),
            };
            match flow {
                Flow::Wait => return true,
                Flow::Next => {
                    self.action += 1;
                    self.phase = Phase::Idle;
                }
                Flow::Fail(reason) => match action.failure_action() {
                    FAIL_CONTINUE => {
                        self.log(out, format!("{reason} — continuing"));
                        self.action += 1;
                        self.phase = Phase::Idle;
                    }
                    FAIL_SKIP => {
                        self.log(out, format!("{reason} — skipping {name}"));
                        self.next_step();
                        return false;
                    }
                    _ => {
                        self.finish(Err(format!("{name}: {reason}")), out);
                        return true;
                    }
                },
            }
        }
    }

    fn run_set(&mut self, device: &str, action: &Action, store: &PropertyStore, now: Instant, out: &mut Advance) -> Flow {
        let Action::Set { property, element, value, retries, wait, .. } = action else { return Flow::Next };
        let Some((prop, elem)) = indigo_target(property, element) else {
            self.log(out, format!("{property} has no INDIGO equivalent, skipped"));
            return Flow::Next;
        };
        let attempt = match self.phase {
            Phase::SetWait { since, .. } if now.duration_since(since) < SET_POLL => return Flow::Wait,
            Phase::SetWait { attempt, .. } => {
                let Some(p) = store.get(device, &prop) else {
                    return Flow::Fail(format!("Property {prop} not found on device {device}"));
                };
                // `SetAction::checkCompletion`: done unless the device refused.
                if p.state != State::Alert {
                    return Flow::Next;
                }
                if attempt >= *retries {
                    return Flow::Fail(format!("{prop} entered Alert state"));
                }
                self.log(out, format!("{prop} in Alert state, retry {}/{retries}", attempt + 1));
                attempt + 1
            }
            _ => 0,
        };
        let Some(p) = store.get(device, &prop) else {
            return Flow::Fail(format!("Property {prop} not found on device {device}"));
        };
        let Some(item) = p.item(&elem) else {
            return Flow::Fail(format!("Element {elem} not found in property {prop}"));
        };
        // `SetAction::isAlreadyDone`.
        let already = match &item.value {
            ItemValue::Switch(on) => *on == as_bool(value),
            ItemValue::Number(n) => num(value).is_some_and(|v| (n - v).abs() < 0.01),
            ItemValue::Text(t) => *t == as_text(value),
            _ => false,
        };
        if already {
            self.log(out, format!("{prop}.{elem} already at {}, skipping", show(value)));
            return Flow::Next;
        }
        let wire = match p.kind {
            Kind::Switch => json!(as_bool(value)),
            Kind::Number => match num(value) {
                Some(n) => json!(n),
                None => return Flow::Fail(format!("{} is not a number for {prop}.{elem}", show(value))),
            },
            Kind::Text => json!(as_text(value)),
            Kind::Light | Kind::Blob => return Flow::Fail(format!("Unsupported property type for {prop}")),
        };
        out.to_indigo.push(new_vector_msg(p.kind, device, &prop, &[(elem.clone(), wire)]));
        self.log(out, format!("{prop}.{elem} = {}", show(value)));
        if *wait {
            self.phase = Phase::SetWait { since: now, attempt };
            Flow::Wait
        } else {
            Flow::Next
        }
    }

    fn run_eval(&mut self, device: &str, action: &Action, store: &PropertyStore, now: Instant, out: &mut Advance) -> Flow {
        let Action::Evaluate { property, element, kind, cond, target, margin, timeout, retries, .. } = action else {
            return Flow::Next;
        };
        let Some((prop, elem)) = indigo_target(property, element) else {
            self.log(out, format!("{property} has no INDIGO equivalent, skipped"));
            return Flow::Next;
        };
        let desc = if *kind == 4 {
            format!("{prop} state {} {}", cond_symbol(*cond), as_text(target).to_uppercase())
        } else {
            format!("{prop}.{elem} {} {}", cond_symbol(*cond), show(target))
        };
        let (since, attempt, reported) = match self.phase {
            Phase::EvalWait { since, attempt, reported } => (since, attempt, reported),
            _ => {
                self.log(out, format!("waiting for {desc}"));
                self.phase = Phase::EvalWait { since: now, attempt: 0, reported: 0 };
                (now, 0, 0)
            }
        };
        if condition_holds(store, device, &prop, &elem, *kind, *cond, target, *margin) {
            self.log(out, format!("{desc} met"));
            return Flow::Next;
        }
        let elapsed = now.duration_since(since);
        if elapsed >= *timeout {
            if attempt >= *retries {
                return Flow::Fail(format!("Timeout evaluating {prop}.{elem}"));
            }
            self.log(out, format!("{desc}: timeout, retry {}/{retries}", attempt + 1));
            self.phase = Phase::EvalWait { since: now, attempt: attempt + 1, reported: 0 };
            return Flow::Wait;
        }
        let mark = elapsed.as_secs() / 10;
        if mark > reported {
            self.log(out, format!("waiting for {desc} ({} s)", elapsed.as_secs()));
            self.phase = Phase::EvalWait { since, attempt, reported: mark };
        }
        Flow::Wait
    }
}

fn cond_symbol(cond: u8) -> &'static str {
    match cond {
        0 => "=",
        1 => "≠",
        2 => ">",
        3 => "<",
        4 => "≥",
        5 => "≤",
        6 => "±",
        7 => "contains",
        8 => "starts with",
        _ => "?",
    }
}

/// `EvaluateAction::evaluateCondition` against the mirror. A property or
/// element that isn't there, or isn't of the asked kind, is "not yet".
#[allow(clippy::too_many_arguments)]
fn condition_holds(
    store: &PropertyStore,
    device: &str,
    prop: &str,
    elem: &str,
    kind: u8,
    cond: u8,
    target: &Value,
    margin: Option<f64>,
) -> bool {
    let Some(p) = store.get(device, prop) else { return false };
    match kind {
        // STATE
        4 => {
            let current = match p.state {
                State::Idle => "Idle",
                State::Ok => "OK",
                State::Busy => "Busy",
                State::Alert => "Alert",
            };
            let equal = current.eq_ignore_ascii_case(as_text(target).trim());
            match cond {
                0 => equal,
                1 => !equal,
                _ => false,
            }
        }
        // NUMBER
        0 => {
            let (Some(current), Some(t)) = (p.number(elem), num(target)) else { return false };
            match cond {
                0 => (current - t).abs() < 0.001,
                1 => (current - t).abs() >= 0.001,
                2 => current > t,
                3 => current < t,
                4 => current >= t,
                5 => current <= t,
                6 => (current - t).abs() <= margin.unwrap_or(0.0),
                _ => false,
            }
        }
        // TEXT
        1 => {
            let Some(current) = p.text(elem) else { return false };
            let t = as_text(target);
            match cond {
                0 => current == t,
                1 => current != t,
                7 => current.to_lowercase().contains(&t.to_lowercase()),
                8 => current.to_lowercase().starts_with(&t.to_lowercase()),
                _ => false,
            }
        }
        // SWITCH, LIGHT
        2 | 3 => {
            let current = match p.item(elem).map(|i| &i.value) {
                Some(ItemValue::Switch(on)) => *on,
                Some(ItemValue::Light(s)) => matches!(s, State::Ok | State::Busy),
                _ => return false,
            };
            match cond {
                0 => current == as_bool(target),
                1 => current != as_bool(target),
                _ => false,
            }
        }
        _ => false,
    }
}

/// `ScriptAction::validateScript`, same wording.
fn validate_script(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Script path is empty".into());
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return Err(format!("Script does not exist: {}", path.display()));
    };
    if !meta.is_file() {
        return Err(format!("Script path is not a file: {}", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!("Script is not executable: {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    fn info(device: &str, iface: u32) -> String {
        format!(r#"{{"defTextVector":{{"device":"{device}","name":"INFO","perm":"ro","state":"Ok","items":[{{"name":"DEVICE_INTERFACE","value":"{iface}"}}]}}}}"#)
    }

    fn connection(device: &str, connected: bool) -> String {
        format!(r#"{{"defSwitchVector":{{"device":"{device}","name":"CONNECTION","perm":"rw","state":"Ok","items":[{{"name":"CONNECTED","value":{connected}}},{{"name":"DISCONNECTED","value":{}}}]}}}}"#, !connected)
    }

    fn switches(verb: &str, device: &str, name: &str, state: &str, items: &[(&str, bool)]) -> String {
        let items: Vec<String> = items.iter().map(|(n, v)| format!(r#"{{"name":"{n}","value":{v}}}"#)).collect();
        format!(r#"{{"{verb}SwitchVector":{{"device":"{device}","name":"{name}","perm":"rw","state":"{state}","rule":"OneOfMany","items":[{}]}}}}"#, items.join(","))
    }

    fn numbers(verb: &str, device: &str, name: &str, state: &str, items: &[(&str, f64)]) -> String {
        let items: Vec<String> = items.iter().map(|(n, v)| format!(r#"{{"name":"{n}","value":{v}}}"#)).collect();
        format!(r#"{{"{verb}NumberVector":{{"device":"{device}","name":"{name}","perm":"rw","state":"{state}","items":[{}]}}}}"#, items.join(","))
    }

    fn apply(s: &mut PropertyStore, frames: &[String]) {
        for f in frames {
            s.apply(parse_frame(f).unwrap());
        }
    }

    /// `(verb, device, property, items)` of each write.
    fn writes(adv: &Advance) -> Vec<(String, String, String, Vec<(String, Value)>)> {
        adv.to_indigo
            .iter()
            .map(|m| {
                let v: Value = serde_json::from_str(m).unwrap();
                let (verb, b) = v.as_object().unwrap().iter().next().unwrap();
                let items = b["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| (i["name"].as_str().unwrap().to_string(), i["value"].clone()))
                    .collect();
                (verb.clone(), b["device"].as_str().unwrap().to_string(), b["name"].as_str().unwrap().to_string(), items)
            })
            .collect()
    }

    fn rig() -> (PropertyStore, Roles) {
        let mut s = PropertyStore::default();
        apply(&mut s, &[
            info("Mount", interface::MOUNT),
            connection("Mount", true),
            switches("def", "Mount", "MOUNT_PARK", "Ok", &[("PARKED", false), ("UNPARKED", true)]),
            switches("def", "Mount", "MOUNT_TRACKING", "Ok", &[("ON", false), ("OFF", true)]),
            numbers("def", "Mount", "MOUNT_EQUATORIAL_COORDINATES", "Ok", &[("RA", 0.0), ("DEC", 5.0)]),
            info("Cam", interface::CCD),
            connection("Cam", true),
            numbers("def", "Cam", "CCD_TEMPERATURE", "Ok", &[("TEMPERATURE", 20.0)]),
            switches("def", "Cam", "CCD_COOLER", "Ok", &[("ON", false), ("OFF", true)]),
        ]);
        let roles = Roles { mount: Some("Mount".into()), camera: Some("Cam".into()), ..Roles::default() };
        (s, roles)
    }

    fn collection(tasks: Value) -> String {
        json!({ "name": "t", "description": "", "version": "1.0", "tasks": tasks }).to_string()
    }

    fn native(tasks: Value) -> String {
        let items: Vec<Value> = tasks.as_array().unwrap().iter().map(|t| json!({ "task": t })).collect();
        json!({ "name": "t", "version": "1.0", "items": items }).to_string()
    }

    fn procedure(json: &str) -> Procedure {
        let (steps, warnings) = parse(json).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        Procedure::new(Slot::PostStartup, steps, 1)
    }

    fn has_log(adv: &Advance, needle: &str) -> bool {
        adv.log.iter().any(|l| l.contains(needle))
    }

    #[test]
    fn mount_park_collection_writes_and_waits_for_ok() {
        let (mut store, roles) = rig();
        let t0 = Instant::now();
        let mut p = procedure(&collection(json!([
            { "template_id": "mount_park", "device": "", "failure_action": 2, "parameters": { "wait_timeout": 60 } }
        ])));
        let a = p.advance(&store, &roles, t0);
        let w = writes(&a);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, "newSwitchVector");
        assert_eq!((w[0].1.as_str(), w[0].2.as_str()), ("Mount", "MOUNT_PARK"));
        assert_eq!(w[0].3, vec![("PARKED".to_string(), json!(true))]);
        assert!(has_log(&a, "mount_park on Mount"), "{:?}", a.log);
        assert!(a.finished.is_none());

        // Same second: nothing to see yet.
        let a = p.advance(&store, &roles, t0);
        assert!(a.to_indigo.is_empty() && a.finished.is_none());

        apply(&mut store, &[switches("set", "Mount", "MOUNT_PARK", "Busy", &[("PARKED", true), ("UNPARKED", false)])]);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(2));
        assert!(a.to_indigo.is_empty());
        assert!(has_log(&a, "waiting for MOUNT_PARK state = OK"), "{:?}", a.log);
        assert!(a.finished.is_none());

        // Progress line every 10 s of waiting.
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(14));
        assert!(has_log(&a, "(12 s)"), "{:?}", a.log);

        apply(&mut store, &[switches("set", "Mount", "MOUNT_PARK", "Ok", &[("PARKED", true)])]);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(15));
        assert_eq!(a.finished, Some(Ok(())));
        assert!(has_log(&a, "procedure complete"));
        // Repeated afterwards.
        assert_eq!(p.advance(&store, &roles, t0 + Duration::from_secs(16)).finished, Some(Ok(())));
    }

    #[test]
    fn camera_cool_skips_the_ramp_and_waits_for_the_temperature() {
        let (mut store, roles) = rig();
        let t0 = Instant::now();
        let mut p = procedure(&collection(json!([
            { "template_id": "camera_cool", "device": "", "failure_action": 0, "parameters": {
                "target_temperature": -10, "tolerance": 0.5, "ramp_slope": 10, "ramp_threshold": 0.2, "max_wait_time": 600 } }
        ])));
        let a = p.advance(&store, &roles, t0);
        assert_eq!(a.log.iter().filter(|l| l.contains("CCD_TEMP_RAMP has no INDIGO equivalent, skipped")).count(), 2);
        let w = writes(&a);
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].0.as_str(), w[0].1.as_str(), w[0].2.as_str()), ("newNumberVector", "Cam", "CCD_TEMPERATURE"));
        assert_eq!(w[0].3, vec![("TEMPERATURE".to_string(), json!(-10.0))]);

        // The SET completes on a non-Alert state; the EVALUATE then polls.
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(2));
        assert!(has_log(&a, "waiting for CCD_TEMPERATURE.TEMPERATURE ± -10"), "{:?}", a.log);
        assert!(a.finished.is_none());
        apply(&mut store, &[numbers("set", "Cam", "CCD_TEMPERATURE", "Busy", &[("TEMPERATURE", -9.2)])]);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(60));
        assert!(a.finished.is_none() && !has_log(&a, "met"));
        apply(&mut store, &[numbers("set", "Cam", "CCD_TEMPERATURE", "Busy", &[("TEMPERATURE", -9.8)])]);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(61));
        assert!(has_log(&a, "CCD_TEMPERATURE.TEMPERATURE ± -10 met"), "{:?}", a.log);
        assert!(has_log(&a, "waiting for CCD_TEMPERATURE state = OK"), "{:?}", a.log);
        assert!(a.finished.is_none());
        apply(&mut store, &[numbers("set", "Cam", "CCD_TEMPERATURE", "Ok", &[("TEMPERATURE", -10.0)])]);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(62));
        assert_eq!(a.finished, Some(Ok(())));
    }

    #[test]
    fn native_queue_with_indigo_names() {
        let (mut store, roles) = rig();
        let t0 = Instant::now();
        let mut p = procedure(&native(json!([
            { "name": "Track", "template_id": "", "device": "Mount", "failure_action": 0, "actions": [
                { "type": "SET", "property": "MOUNT_TRACKING", "element": "ON", "value": true, "timeout": 10, "retries": 0, "failure_action": 0, "wait_for_completion": true },
                { "type": "EVALUATE", "property": "MOUNT_EQUATORIAL_COORDINATES", "element": "DEC", "property_type": 0, "condition": 4, "target": 10, "timeout": 30, "retries": 0, "failure_action": 0 }
            ] }
        ])));
        let a = p.advance(&store, &roles, t0);
        let w = writes(&a);
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].1.as_str(), w[0].2.as_str()), ("Mount", "MOUNT_TRACKING"));
        assert_eq!(w[0].3, vec![("ON".to_string(), json!(true))]);
        assert!(has_log(&a, "Track on Mount"));
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(1));
        assert!(has_log(&a, "waiting for MOUNT_EQUATORIAL_COORDINATES.DEC ≥ 10"), "{:?}", a.log);
        assert!(a.finished.is_none());
        apply(&mut store, &[numbers("set", "Mount", "MOUNT_EQUATORIAL_COORDINATES", "Ok", &[("DEC", 12.0)])]);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(2));
        assert_eq!(a.finished, Some(Ok(())));
    }

    #[test]
    fn unknown_template_and_bad_parameters_are_dropped_with_warnings() {
        let (steps, warnings) = parse(&collection(json!([
            { "template_id": "roof_open", "device": "", "parameters": {} },
            { "template_id": "mount_park", "device": "", "parameters": { "wait_timeout": 10 } },
            { "template_id": "mount_park", "device": "", "parameters": {} },
            { "template_id": "mount_unpark", "device": "", "parameters": { "wait_timeout": 5 } }
        ])))
        .unwrap();
        assert_eq!(steps.len(), 1);
        assert!(matches!(&steps[0], Step::Task { name, interfaces: 1, actions, .. } if name == "mount_unpark" && actions.len() == 2));
        assert_eq!(warnings.len(), 3);
        assert!(warnings[0].contains("unknown template_id \"roof_open\""));
        assert!(warnings[1].contains("Parameter wait_timeout must be >= 30"));
        assert!(warnings[2].contains("Missing required parameter: wait_timeout"));
        assert!(parse("{\"foo\": 1}").is_err());
        assert!(parse("nope").is_err());
    }

    #[test]
    fn delay_and_script_templates_become_their_own_steps() {
        let (steps, _) = parse(&collection(json!([
            { "template_id": "delay", "device": "", "parameters": { "delay_seconds": 90 } },
            { "template_id": "script_execute", "device": "", "parameters": { "script_path": "/opt/roof.sh", "timeout": 120 } }
        ])))
        .unwrap();
        assert_eq!(steps[0], Step::Delay(Duration::from_secs(90)));
        assert_eq!(
            steps[1],
            Step::Script { path: "/opt/roof.sh".into(), timeout: Duration::from_secs(120), retries: 2, failure_action: 0 }
        );
    }

    #[test]
    fn scripts_retry_then_fail_or_succeed() {
        let (store, roles) = rig();
        let t0 = Instant::now();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("junos_queue_test_{}.sh", std::process::id()));
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut p = Procedure::new(
            Slot::PreShutdown,
            vec![Step::Script { path: path.clone(), timeout: Duration::from_secs(60), retries: 1, failure_action: 0 }],
            7,
        );
        let a = p.advance(&store, &roles, t0);
        assert_eq!(a.scripts, vec![ScriptReq { id: 7, path: path.clone(), timeout: Duration::from_secs(60) }]);
        assert!(a.finished.is_none());
        p.script_finished(7, Ok(1));
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(1));
        assert_eq!(a.scripts.len(), 1);
        assert_eq!(a.scripts[0].id, 8);
        assert!(has_log(&a, "Script failed with exit code 1"));
        assert!(has_log(&a, "retry 1/1"), "{:?}", a.log);
        p.script_finished(8, Ok(1));
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(2));
        assert!(a.scripts.is_empty());
        assert!(matches!(&a.finished, Some(Err(e)) if e.contains("exit code 1")), "{:?}", a.finished);

        // Through the template, exit 0 completes the procedure.
        let mut p = procedure(&collection(json!([
            { "template_id": "script_execute", "device": "", "parameters": { "script_path": path.to_str().unwrap(), "timeout": 300 } }
        ])));
        let a = p.advance(&store, &roles, t0);
        assert_eq!(a.scripts.len(), 1);
        p.script_finished(a.scripts[0].id, Ok(0));
        let a = p.advance(&store, &roles, t0);
        assert_eq!(a.finished, Some(Ok(())));
        assert!(has_log(&a, "exited with 0"));

        // A runner error ("timed out", spawn failure) counts like a bad exit.
        let mut p = Procedure::new(
            Slot::PreShutdown,
            vec![Step::Script { path: path.clone(), timeout: Duration::from_secs(60), retries: 0, failure_action: 1 }],
            1,
        );
        let a = p.advance(&store, &roles, t0);
        p.script_finished(a.scripts[0].id, Err("timed out".into()));
        let a = p.advance(&store, &roles, t0);
        assert!(has_log(&a, "Script timed out"), "{:?}", a.log);
        // failure_action 1: the procedure goes on (and ends, nothing follows).
        assert_eq!(a.finished, Some(Ok(())));

        std::fs::remove_file(&path).unwrap();

        // Missing file: failure at once, KStars' wording.
        let mut p = procedure(&collection(json!([
            { "template_id": "script_execute", "device": "", "parameters": { "script_path": "/nonexistent/junos.sh", "timeout": 300 } }
        ])));
        let a = p.advance(&store, &roles, t0);
        assert!(a.scripts.is_empty());
        assert!(matches!(&a.finished, Some(Err(e)) if e.starts_with("Script does not exist")), "{:?}", a.finished);
    }

    #[test]
    fn delay_waits_its_duration() {
        let (store, roles) = rig();
        let t0 = Instant::now();
        let mut p = procedure(&native(json!([
            { "name": "settle", "device": "", "actions": [ { "type": "DELAY", "duration": 2, "unit": 0 } ] }
        ])));
        let a = p.advance(&store, &roles, t0);
        assert!(has_log(&a, "waiting 2 s"));
        assert!(a.finished.is_none());
        assert!(p.advance(&store, &roles, t0 + Duration::from_secs(1)).finished.is_none());
        assert_eq!(p.advance(&store, &roles, t0 + Duration::from_secs(3)).finished, Some(Ok(())));
        // Minutes.
        let (steps, _) = parse(&native(json!([{ "name": "s", "device": "", "actions": [ { "type": "DELAY", "duration": 2, "unit": 1 } ] }]))).unwrap();
        assert_eq!(steps, vec![Step::Delay(Duration::from_secs(120))]);
    }

    #[test]
    fn continue_on_a_missing_property() {
        let (store, roles) = rig();
        let t0 = Instant::now();
        let mut p = procedure(&native(json!([
            { "name": "Odd", "device": "Mount", "actions": [
                { "type": "SET", "property": "NOPE", "element": "X", "value": 1, "failure_action": 1 },
                { "type": "EVALUATE", "property": "MOUNT_PARK", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "failure_action": 0 }
            ] }
        ])));
        let a = p.advance(&store, &roles, t0);
        assert!(a.to_indigo.is_empty());
        assert!(has_log(&a, "Property NOPE not found on device Mount — continuing"), "{:?}", a.log);
        assert_eq!(a.finished, Some(Ok(())));

        // The same with the queue-aborting default.
        let mut p = procedure(&native(json!([
            { "name": "Odd", "device": "Mount", "actions": [ { "type": "SET", "property": "NOPE", "element": "X", "value": 1 } ] }
        ])));
        let a = p.advance(&store, &roles, t0);
        assert!(matches!(&a.finished, Some(Err(e)) if e.contains("NOPE not found")));
    }

    #[test]
    fn missing_device_follows_the_task_failure_action() {
        let (store, roles) = rig();
        let t0 = Instant::now();
        let dome = |failure_action: u8| {
            collection(json!([
                { "template_id": "dome_park", "device": "", "failure_action": failure_action, "parameters": { "wait_timeout": 120 } },
                { "template_id": "mount_unpark", "device": "", "failure_action": 0, "parameters": { "wait_timeout": 5 } }
            ]))
        };
        let mut p = procedure(&dome(2));
        let a = p.advance(&store, &roles, t0);
        assert!(has_log(&a, "dome_park: No connected device found for DOME_PARK — continuing"), "{:?}", a.log);
        // mount_unpark: already unparked, so nothing to write; its EVALUATE holds.
        assert!(has_log(&a, "MOUNT_PARK.UNPARKED already at On, skipping"), "{:?}", a.log);
        assert!(a.to_indigo.is_empty());
        assert_eq!(a.finished, Some(Ok(())));

        let mut p = procedure(&dome(0));
        let a = p.advance(&store, &roles, t0);
        assert!(matches!(&a.finished, Some(Err(e)) if e.starts_with("dome_park: No connected device")));

        // A named device that isn't there.
        let mut p = procedure(&native(json!([
            { "name": "Roof", "device": "Roof", "failure_action": 0, "actions": [ { "type": "SET", "property": "X", "element": "Y", "value": true } ] }
        ])));
        assert!(matches!(&p.advance(&store, &roles, t0).finished, Some(Err(e)) if e.contains("Device Roof not found")));
    }

    #[test]
    fn dust_cap_resolves_by_property_and_set_retries_on_alert() {
        let (mut store, roles) = rig();
        apply(&mut store, &[
            info("Cap", interface::DUSTCAP),
            connection("Cap", true),
            switches("def", "Cap", "AUX_COVER", "Ok", &[("OPEN", true), ("CLOSE", false)]),
        ]);
        let t0 = Instant::now();
        let mut p = procedure(&collection(json!([
            { "template_id": "dustcap_park", "device": "", "failure_action": 0, "parameters": { "wait_timeout": 30 } }
        ])));
        let a = p.advance(&store, &roles, t0);
        let w = writes(&a);
        assert_eq!((w[0].1.as_str(), w[0].2.as_str()), ("Cap", "AUX_COVER"));
        assert_eq!(w[0].3, vec![("CLOSE".to_string(), json!(true))]);

        // The device refuses: re-sent twice (the template's retries), then the queue fails.
        apply(&mut store, &[switches("set", "Cap", "AUX_COVER", "Alert", &[])]);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(1));
        assert_eq!(writes(&a).len(), 1);
        assert!(has_log(&a, "retry 1/2"), "{:?}", a.log);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(2));
        assert_eq!(writes(&a).len(), 1);
        assert!(has_log(&a, "retry 2/2"));
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(3));
        assert!(a.to_indigo.is_empty());
        assert!(matches!(&a.finished, Some(Err(e)) if e.contains("AUX_COVER entered Alert state")), "{:?}", a.finished);
    }

    #[test]
    fn evaluate_times_out_then_retries() {
        let (store, roles) = rig();
        let t0 = Instant::now();
        let mut p = procedure(&native(json!([
            { "name": "Wait", "device": "Mount", "actions": [
                { "type": "EVALUATE", "property": "MOUNT_PARK", "element": "PARKED", "property_type": 2, "condition": 0, "target": true, "timeout": 10, "retries": 1, "failure_action": 2 }
            ] },
            { "name": "settle", "device": "", "actions": [ { "type": "DELAY", "duration": 1, "unit": 0 } ] }
        ])));
        assert!(p.advance(&store, &roles, t0).finished.is_none());
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(10));
        assert!(has_log(&a, "timeout, retry 1/1"), "{:?}", a.log);
        let a = p.advance(&store, &roles, t0 + Duration::from_secs(20));
        // failure_action 2: the task is skipped, the delay step starts.
        assert!(has_log(&a, "Timeout evaluating MOUNT_PARK.PARKED — skipping Wait"), "{:?}", a.log);
        assert!(has_log(&a, "waiting 1 s"));
        assert!(a.finished.is_none());
        assert_eq!(p.advance(&store, &roles, t0 + Duration::from_secs(22)).finished, Some(Ok(())));
    }

    #[test]
    fn indi_names_map_to_indigo() {
        assert_eq!(indigo_target("TELESCOPE_PARK", "PARK"), Some(("MOUNT_PARK".into(), "PARKED".into())));
        assert_eq!(indigo_target("TELESCOPE_TRACK_STATE", "TRACK_OFF"), Some(("MOUNT_TRACKING".into(), "OFF".into())));
        assert_eq!(indigo_target("CAP_PARK", "UNPARK"), Some(("AUX_COVER".into(), "OPEN".into())));
        assert_eq!(indigo_target("CCD_TEMPERATURE", "CCD_TEMPERATURE_VALUE"), Some(("CCD_TEMPERATURE".into(), "TEMPERATURE".into())));
        assert_eq!(indigo_target("CCD_COOLER", "COOLER_OFF"), Some(("CCD_COOLER".into(), "OFF".into())));
        assert_eq!(indigo_target("CCD_TEMP_RAMP", "RAMP_SLOPE"), None);
        assert_eq!(indigo_target("DOME_SHUTTER", "OPENED"), Some(("DOME_SHUTTER".into(), "OPENED".into())));
    }

    #[test]
    fn abort_ends_the_procedure() {
        let (store, roles) = rig();
        let mut p = procedure(&native(json!([
            { "name": "settle", "device": "", "actions": [ { "type": "DELAY", "duration": 60, "unit": 0 } ] }
        ])));
        let t0 = Instant::now();
        assert!(p.advance(&store, &roles, t0).finished.is_none());
        assert!(p.abort().is_empty());
        assert_eq!(p.advance(&store, &roles, t0 + Duration::from_secs(1)).finished, Some(Err("aborted".into())));
        assert_eq!(Slot::PreShutdown.label(), "pre-shutdown");
    }
}
