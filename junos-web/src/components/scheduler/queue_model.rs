//! Scheduler startup/shutdown queues — the KStars task-queue *collection*
//! format, mirrored as editable steps.
//!
//! KStars 3.8+ loads each of the Scheduler's four procedure slots as a JSON
//! collection (`QueueManager::loadCollectionFromJson`):
//! `{"name","description","version","tasks":[{"template_id","device","failure_action","parameters"}]}`.
//! Two things it does *silently* make this module strict:
//! - a task whose `template_id` it doesn't know is skipped;
//! - a task missing any template parameter, or with one outside its min..max,
//!   is dropped (`TaskTemplate::validateParameters`).
//!
//! So every step serialises every parameter of its template, validated against
//! [`TEMPLATES`] first. Tasks this editor doesn't model survive a load/save
//! round-trip verbatim as [`QueueStep::Unknown`].
//!
//! A collection can't hold a custom INDI step ([`QueueStep::Indi`]); a queue
//! that has one is written in KStars' queue format instead — see
//! [`super::queue_native`]. [`to_document`] / [`from_document`] pick the format.

use serde_json::{json, Map, Value};

use super::queue_native::{check_indi, from_queue, to_queue, IndiStep};

// ── Slots ────────────────────────────────────────────────────────────────────

/// The Scheduler's four procedure slots (`scheduler.ui` line edits).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueueSlot {
    PreStartup,
    PostStartup,
    PreShutdown,
    PostShutdown,
}

impl QueueSlot {
    /// `scheduler_set_all_settings` key holding the slot's collection path.
    pub fn setting_key(self) -> &'static str {
        match self {
            Self::PreStartup   => "schedulerPreStartupScript",
            Self::PostStartup  => "schedulerPostStartupScript",
            Self::PreShutdown  => "schedulerPreShutdownScript",
            Self::PostShutdown => "schedulerPostShutdownScript",
        }
    }

    /// Checkbox that gates the slot's procedure (shared by pre and post).
    pub fn enable_key(self) -> &'static str {
        match self {
            Self::PreStartup | Self::PostStartup   => "schedulerStartupEnabled",
            Self::PreShutdown | Self::PostShutdown => "schedulerShutdownEnabled",
        }
    }

    /// Pre-startup runs before Ekos/INDI start and post-shutdown after they
    /// stop. A device task there makes `QueueExecutor::start()` refuse to run,
    /// and the scheduler then waits forever in its `*_RUNNING` state
    /// (`queueexecutor.cpp`) — so only delays and scripts belong there.
    pub fn allows_devices(self) -> bool {
        matches!(self, Self::PostStartup | Self::PreShutdown)
    }

    pub fn default_name(self) -> &'static str {
        match self {
            Self::PreStartup   => "junos_pre_startup",
            Self::PostStartup  => "junos_post_startup",
            Self::PreShutdown  => "junos_pre_shutdown",
            Self::PostShutdown => "junos_post_shutdown",
        }
    }

    /// What a fresh queue for this slot starts from: the stock KStars
    /// procedure where the slot has one by default.
    pub fn default_preset(self) -> Preset {
        match self {
            Self::PostStartup => Preset::Startup,
            Self::PreShutdown => Preset::Shutdown,
            Self::PreStartup | Self::PostShutdown => Preset::Empty,
        }
    }
}

/// Starting points for a new queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    Empty,
    Startup,
    Shutdown,
}

impl Preset {
    pub fn key(self) -> &'static str {
        match self {
            Self::Empty    => "empty",
            Self::Startup  => "startup",
            Self::Shutdown => "shutdown",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "startup"  => Self::Startup,
            "shutdown" => Self::Shutdown,
            _          => Self::Empty,
        }
    }

    /// The tasks of KStars' stock `observatory_startup.json` /
    /// `observatory_shutdown.json` (`kstars/data/taskqueue/collections/`).
    pub fn steps(self) -> Vec<QueueStep> {
        let skip = |id: &'static str, wait: &str| QueueStep::Template {
            id,
            values: vec![wait.to_string()],
            on_missing_device: FAIL_SKIP,
        };
        match self {
            Self::Empty => Vec::new(),
            Self::Startup => vec![
                skip("dome_unpark", "120"),
                skip("mount_unpark", "60"),
                skip("dustcap_unpark", "30"),
            ],
            Self::Shutdown => vec![
                skip("dustcap_park", "30"),
                skip("mount_park", "60"),
                skip("dome_park", "120"),
            ],
        }
    }
}

// ── Templates ────────────────────────────────────────────────────────────────

/// `failure_action` values (`TaskAction::FailureAction`). On a collection
/// task it only decides what happens when no connected device matches.
pub const FAIL_ABORT: u8 = 0;
pub const FAIL_CONTINUE: u8 = 1;
pub const FAIL_SKIP: u8 = 2;

pub struct ParamSpec {
    pub name: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    pub unit: &'static str,
}

pub(super) const fn p(name: &'static str, default: f64, min: f64, max: f64, unit: &'static str) -> ParamSpec {
    ParamSpec { name, default, min, max, unit }
}

pub struct TemplateSpec {
    pub id: &'static str,
    /// Template declares `supported_interfaces` — KStars binds it to the first
    /// connected INDI device with those interfaces.
    pub needs_device: bool,
    pub params: &'static [ParamSpec],
}

/// Mirror of `kstars/kstars/data/taskqueue/templates/system/*.json` (ids,
/// parameter names, defaults and ranges must match exactly — see the module
/// doc). `script_execute` is handled separately as [`QueueStep::Script`].
pub const TEMPLATES: &[TemplateSpec] = &[
    TemplateSpec { id: "dome_unpark",    needs_device: true, params: &[p("wait_timeout", 120.0, 60.0, 600.0, "s")] },
    TemplateSpec { id: "dome_park",      needs_device: true, params: &[p("wait_timeout", 120.0, 60.0, 600.0, "s")] },
    TemplateSpec { id: "mount_unpark",   needs_device: true, params: &[p("wait_timeout", 5.0, 5.0, 300.0, "s")] },
    TemplateSpec { id: "mount_park",     needs_device: true, params: &[p("wait_timeout", 60.0, 30.0, 300.0, "s")] },
    TemplateSpec { id: "dustcap_unpark", needs_device: true, params: &[p("wait_timeout", 30.0, 10.0, 120.0, "s")] },
    TemplateSpec { id: "dustcap_park",   needs_device: true, params: &[p("wait_timeout", 30.0, 10.0, 120.0, "s")] },
    TemplateSpec { id: "camera_cool", needs_device: true, params: &[
        p("target_temperature", -10.0, -50.0, 50.0, "°C"),
        p("tolerance",          0.5,   0.1,   5.0, "°C"),
        p("ramp_slope",         10.0,  0.0,   30.0, "°C/min"),
        p("ramp_threshold",     0.2,   0.1,   2.0, "°C"),
        p("max_wait_time",      600.0, 60.0,  1800.0, "s"),
    ] },
    TemplateSpec { id: "camera_warm", needs_device: true, params: &[
        p("target_temperature", 20.0,  -10.0, 50.0, "°C"),
        p("tolerance",          2.0,   0.5,   5.0, "°C"),
        p("ramp_slope",         15.0,  0.0,   30.0, "°C/min"),
        p("ramp_threshold",     0.5,   0.1,   2.0, "°C"),
        p("max_wait_time",      600.0, 60.0,  1800.0, "s"),
    ] },
    TemplateSpec { id: "camera_warm_passive", needs_device: true, params: &[
        p("target_temperature", 20.0,  -10.0, 50.0, "°C"),
        p("max_wait_time",      600.0, 60.0,  1800.0, "s"),
    ] },
    TemplateSpec { id: "delay", needs_device: false, params: &[p("delay_seconds", 60.0, 5.0, 21600.0, "s")] },
];

pub const SCRIPT_TEMPLATE_ID: &str = "script_execute";
pub(super) const DESCRIPTION: &str = "Created with Junos";
pub const SCRIPT_TIMEOUT: ParamSpec = p("timeout", 300.0, 30.0, 3600.0, "s");

pub fn template(id: &str) -> Option<&'static TemplateSpec> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// Body a new managed script starts with. KStars runs it with no shell and no
/// arguments (`QProcess::start(path)`), hence the mandatory interpreter line —
/// via `env`, since `/bin/bash` doesn't exist on NixOS.
pub const NEW_SCRIPT_BODY: &str = "#!/usr/bin/env bash\n\
# Runs on the KStars host, as the KStars user, with no arguments.\n\
# Exit 0 = success. Any other exit code fails this step: KStars retries it\n\
# twice, then aborts the startup/shutdown procedure.\n\
set -euo pipefail\n\n";

// ── Steps ────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum QueueStep {
    /// A [`TEMPLATES`] entry; `values[i]` is the text of `params[i]`.
    Template { id: &'static str, values: Vec<String>, on_missing_device: u8 },
    /// `script_execute`.
    Script { script: ScriptRef, timeout: String },
    /// One `SET` / `EVALUATE` on any INDI property — queue format only.
    Indi(IndiStep),
    /// A task this editor doesn't model, kept verbatim. `native`: read from a
    /// queue-format file, so it can only be written back into one.
    Unknown { task: Value, native: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScriptRef {
    /// `<scripts_dir>/<name>.sh`, written by junos-server from this editor.
    Managed { name: String, body: String },
    /// Any other executable on the KStars host, referenced as-is.
    External { path: String },
}

impl QueueStep {
    /// A fresh step of template `id` with every parameter at its default.
    pub fn new_template(id: &str) -> Option<Self> {
        let spec = template(id)?;
        Some(Self::Template {
            id: spec.id,
            values: spec.params.iter().map(|p| fmt_num(p.default)).collect(),
            on_missing_device: FAIL_SKIP,
        })
    }

    pub fn needs_device(&self) -> bool {
        match self {
            Self::Template { id, .. } => template(id).is_some_and(|t| t.needs_device),
            Self::Script { .. } => false,
            Self::Indi(_) => true,
            // Can't tell — `QueueExecutor` checks the template's interfaces.
            Self::Unknown { .. } => false,
        }
    }

    /// Only KStars' queue format can carry this step.
    pub fn needs_native(&self) -> bool {
        matches!(self, Self::Indi(_) | Self::Unknown { native: true, .. })
    }
}

/// `120.0` → `"120"`, `0.5` → `"0.5"`.
pub fn fmt_num(v: f64) -> String {
    format!("{v}")
}

/// Same shape junos-server accepts for queue and script names.
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// KStars resolves these by name to its stock collections; junos-server
/// refuses to write them.
pub fn is_reserved_name(name: &str) -> bool {
    matches!(name, "observatory_startup" | "observatory_shutdown")
}

pub fn managed_script_path(scripts_dir: &str, name: &str) -> String {
    format!("{}/{}.sh", scripts_dir.trim_end_matches('/'), name)
}

/// JSON numbers for the template parameters: integers where the value is
/// whole, so the files stay readable next to KStars' own.
pub(super) fn num_value(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

// ── Collection (de)serialisation ─────────────────────────────────────────────

/// Every parameter of template `id`, as JSON numbers. Values that don't
/// parse fall back to their defaults — call [`validate`] first.
pub(super) fn template_params(id: &str, values: &[String]) -> Map<String, Value> {
    let mut params = Map::new();
    for (i, p) in template(id).map(|s| s.params).unwrap_or(&[]).iter().enumerate() {
        let v = values.get(i).and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(p.default);
        params.insert(p.name.to_string(), num_value(v));
    }
    params
}

/// `script_execute`'s parameters.
pub(super) fn script_params(script: &ScriptRef, timeout: &str, scripts_dir: &str) -> Map<String, Value> {
    let path = match script {
        ScriptRef::Managed { name, .. } => managed_script_path(scripts_dir, name),
        ScriptRef::External { path } => path.trim().to_string(),
    };
    let timeout = timeout.trim().parse::<f64>().unwrap_or(SCRIPT_TIMEOUT.default);
    let mut params = Map::new();
    params.insert("script_path".into(), path.into());
    params.insert("timeout".into(), num_value(timeout));
    params
}

/// The text of each of `spec`'s parameters; a missing one takes its default,
/// which is what KStars' own editor would show.
pub(super) fn template_values(spec: &TemplateSpec, params: Option<&Value>) -> Vec<String> {
    spec.params
        .iter()
        .map(|p| fmt_num(params.and_then(|v| v.get(p.name)).and_then(Value::as_f64).unwrap_or(p.default)))
        .collect()
}

/// A `script_execute` task's step. A path under `scripts_dir` is a managed
/// script, its body left empty for the caller to fetch.
pub(super) fn script_step(params: Option<&Value>, scripts_dir: &str) -> QueueStep {
    let param = |name: &str| params.and_then(|p| p.get(name));
    let path = param("script_path").and_then(Value::as_str).unwrap_or_default().to_string();
    let timeout = param("timeout").and_then(Value::as_f64).unwrap_or(SCRIPT_TIMEOUT.default);
    let scripts_prefix = format!("{}/", scripts_dir.trim_end_matches('/'));
    let managed = path
        .strip_prefix(&scripts_prefix)
        .and_then(|rest| rest.strip_suffix(".sh"))
        .filter(|name| is_safe_name(name))
        .map(str::to_string);
    let script = match managed {
        Some(name) => ScriptRef::Managed { name, body: String::new() },
        None => ScriptRef::External { path },
    };
    QueueStep::Script { script, timeout: fmt_num(timeout) }
}

/// Build the collection for `steps`, which hold no [`QueueStep::needs_native`]
/// step. Call [`validate`] first.
pub fn to_collection(title: &str, steps: &[QueueStep], scripts_dir: &str) -> Value {
    let tasks: Vec<Value> = steps
        .iter()
        .map(|step| match step {
            // Only consulted when no device matches, so inert on a delay —
            // but KStars' stock collections carry it on every task.
            QueueStep::Template { id, values, on_missing_device } => json!({
                "template_id": id,
                "device": "",
                "failure_action": on_missing_device,
                "parameters": template_params(id, values),
            }),
            QueueStep::Script { script, timeout } => json!({
                "template_id": SCRIPT_TEMPLATE_ID,
                "device": "",
                "parameters": script_params(script, timeout, scripts_dir),
            }),
            QueueStep::Unknown { task, .. } => task.clone(),
            // Never here: `to_document` sends these to the queue format.
            QueueStep::Indi(_) => Value::Null,
        })
        .collect();

    json!({
        "name": title,
        "description": DESCRIPTION,
        "version": "1.0",
        "tasks": tasks,
    })
}

/// Parse a collection back into `(title, steps)`. Managed script bodies are
/// left empty — the caller fetches them.
pub fn from_collection(doc: &Value, scripts_dir: &str) -> Result<(String, Vec<QueueStep>), String> {
    let tasks = doc
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| "not a task collection (no \"tasks\" array)".to_string())?;
    let title = doc.get("name").and_then(Value::as_str).unwrap_or_default().to_string();

    let steps = tasks
        .iter()
        .map(|task| {
            let id = task.get("template_id").and_then(Value::as_str).unwrap_or_default();
            let params = task.get("parameters");
            if id == SCRIPT_TEMPLATE_ID {
                return script_step(params, scripts_dir);
            }
            let Some(spec) = template(id) else { return QueueStep::Unknown { task: task.clone(), native: false } };
            // Absent means ABORT_QUEUE (`Task::m_deviceMappingFailureAction`).
            let on_missing_device = task
                .get("failure_action")
                .and_then(Value::as_u64)
                .filter(|v| *v <= u64::from(FAIL_SKIP))
                .map(|v| v as u8)
                .unwrap_or(FAIL_ABORT);
            QueueStep::Template { id: spec.id, values: template_values(spec, params), on_missing_device }
        })
        .collect();

    Ok((title, steps))
}

/// The file for `steps`: a collection, or KStars' queue format once a step
/// needs it.
pub fn to_document(title: &str, steps: &[QueueStep], scripts_dir: &str) -> Value {
    if steps.iter().any(QueueStep::needs_native) {
        to_queue(title, steps, scripts_dir)
    } else {
        to_collection(title, steps, scripts_dir)
    }
}

/// Either format back into `(title, steps)`.
pub fn from_document(doc: &Value, scripts_dir: &str) -> Result<(String, Vec<QueueStep>), String> {
    if doc.get("items").is_some() {
        from_queue(doc, scripts_dir)
    } else {
        from_collection(doc, scripts_dir)
    }
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Why a queue can't be saved. `step` is 0-based.
#[derive(Clone, Debug, PartialEq)]
pub enum QueueError {
    BadName,
    ReservedName,
    NoSteps,
    DeviceStepInSlot { step: usize },
    BadParam { step: usize, param: &'static str, min: f64, max: f64 },
    BadScriptName { step: usize },
    DuplicateScriptName { step: usize },
    MissingShebang { step: usize },
    BadScriptPath { step: usize },
    /// A custom step without its device, property or element.
    IndiIncomplete { step: usize },
    /// A custom step's value doesn't fit its kind (or the kind can't be set).
    IndiBadValue { step: usize },
    /// A collection task this editor doesn't model, in a queue that must be
    /// written in the queue format (which can't express it).
    UnknownInQueue { step: usize },
}

pub fn validate(slot: QueueSlot, name: &str, steps: &[QueueStep]) -> Result<(), QueueError> {
    if !is_safe_name(name) {
        return Err(QueueError::BadName);
    }
    if is_reserved_name(name) {
        return Err(QueueError::ReservedName);
    }
    if steps.is_empty() {
        return Err(QueueError::NoSteps);
    }

    let native = steps.iter().any(QueueStep::needs_native);
    let mut script_names: Vec<&str> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        if step.needs_device() && !slot.allows_devices() {
            return Err(QueueError::DeviceStepInSlot { step: i });
        }
        match step {
            QueueStep::Template { id, values, .. } => {
                let Some(spec) = template(id) else { continue };
                for (j, p) in spec.params.iter().enumerate() {
                    check_param(i, p, values.get(j).map(String::as_str).unwrap_or_default())?;
                }
            }
            QueueStep::Script { script, timeout } => {
                check_param(i, &SCRIPT_TIMEOUT, timeout)?;
                match script {
                    ScriptRef::Managed { name, body } => {
                        if !is_safe_name(name) {
                            return Err(QueueError::BadScriptName { step: i });
                        }
                        if script_names.contains(&name.as_str()) {
                            return Err(QueueError::DuplicateScriptName { step: i });
                        }
                        script_names.push(name);
                        if !body.starts_with("#!") {
                            return Err(QueueError::MissingShebang { step: i });
                        }
                    }
                    ScriptRef::External { path } => {
                        if !path.trim().starts_with('/') {
                            return Err(QueueError::BadScriptPath { step: i });
                        }
                    }
                }
            }
            QueueStep::Indi(s) => check_indi(i, s)?,
            QueueStep::Unknown { native: false, .. } if native => return Err(QueueError::UnknownInQueue { step: i }),
            QueueStep::Unknown { .. } => {}
        }
    }
    Ok(())
}

pub(super) fn check_param(step: usize, p: &'static ParamSpec, text: &str) -> Result<(), QueueError> {
    match text.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && v >= p.min && v <= p.max => Ok(()),
        _ => Err(QueueError::BadParam { step, param: p.name, min: p.min, max: p.max }),
    }
}
