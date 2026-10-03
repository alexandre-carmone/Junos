//! Scheduler queues in KStars' own *queue* format, and the custom INDI steps
//! that need it.
//!
//! A collection (`queue_model::to_collection`) can only name templates. The
//! format `QueueManager::saveQueue` writes spells every action out instead —
//! `{"items":[{"id","task":{name, template_id, device, parameters, actions}}]}`
//! — and `loadQueue` reads both, so a queue switches format only once it holds
//! a custom step. Built-in steps then keep their `template_id` and an empty
//! `device`, so `QueueExecutor` still binds the first connected device of the
//! right type; but this format has no "if no device" choice
//! (`Task::loadFromJson` never reads it): a missing device stops the queue.
//!
//! A custom step is one `SET` or `EVALUATE` action (`actions/setaction.cpp`,
//! `actions/evaluateaction.cpp`) on any device, property and element.

use serde_json::{json, Map, Value};

use super::queue_model::{
    check_param, fmt_num, num_value, p, script_params, script_step, template, template_params, template_values,
    ParamSpec, QueueError, QueueStep, DESCRIPTION, FAIL_ABORT, FAIL_CONTINUE, SCRIPT_TEMPLATE_ID,
};

// ── Custom INDI steps ────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndiOp {
    /// `SET` — write one element.
    Set,
    /// `EVALUATE` — poll until a condition holds.
    Wait,
}

/// `EvaluateAction::PropertyType`, in its order: the wire value is the index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndiKind {
    Number,
    Text,
    Switch,
    Light,
    /// The property's own state (Idle / OK / Busy / Alert), not an element.
    State,
}

pub const KINDS: [IndiKind; 5] = [IndiKind::Number, IndiKind::Text, IndiKind::Switch, IndiKind::Light, IndiKind::State];

/// `EvaluateAction::ConditionType`, in its order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cond {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    /// |value − target| ≤ margin.
    Within,
    Contains,
    StartsWith,
}

pub const CONDS: [Cond; 9] =
    [Cond::Eq, Cond::Ne, Cond::Gt, Cond::Lt, Cond::Ge, Cond::Le, Cond::Within, Cond::Contains, Cond::StartsWith];

impl IndiKind {
    /// `SetAction` writes numbers, texts and switches only.
    pub fn settable(self) -> bool {
        matches!(self, Self::Number | Self::Text | Self::Switch)
    }

    /// The conditions `EvaluateAction` implements for this kind.
    pub fn conds(self) -> &'static [Cond] {
        match self {
            Self::Number => &[Cond::Eq, Cond::Ne, Cond::Gt, Cond::Lt, Cond::Ge, Cond::Le, Cond::Within],
            Self::Text => &[Cond::Eq, Cond::Ne, Cond::Contains, Cond::StartsWith],
            Self::Switch | Self::Light | Self::State => &[Cond::Eq, Cond::Ne],
        }
    }

    /// The only values a switch, light or state can take; `None` = free text.
    pub fn choices(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Switch | Self::Light => Some(&["On", "Off"]),
            Self::State => Some(&["Idle", "OK", "Busy", "Alert"]),
            Self::Number | Self::Text => None,
        }
    }
}

impl Cond {
    fn symbol(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "≠",
            Self::Gt => ">",
            Self::Lt => "<",
            Self::Ge => "≥",
            Self::Le => "≤",
            Self::Within => "±",
            Self::Contains => "contains",
            Self::StartsWith => "starts with",
        }
    }
}

pub const INDI_TIMEOUT: ParamSpec = p("timeout", 30.0, 1.0, 86400.0, "s");
pub const INDI_RETRIES: ParamSpec = p("retries", 2.0, 0.0, 10.0, "");
pub const INDI_MARGIN: ParamSpec = p("margin", 0.5, 0.0, 1e9, "");

#[derive(Clone, Debug, PartialEq)]
pub struct IndiStep {
    pub op: IndiOp,
    pub device: String,
    pub property: String,
    /// Unused for [`IndiKind::State`] — written as `STATE`, like KStars' templates.
    pub element: String,
    pub kind: IndiKind,
    /// Wait only.
    pub cond: Cond,
    /// Set: the new value. Wait: the target. One of [`IndiKind::choices`] when
    /// the kind has them.
    pub value: String,
    /// Wait with [`Cond::Within`] only.
    pub margin: String,
    /// Set only: hold the step until the device reports the change done.
    pub wait_done: bool,
    pub timeout: String,
    pub retries: String,
    /// [`FAIL_ABORT`] or [`FAIL_CONTINUE`].
    pub on_fail: u8,
}

impl IndiStep {
    pub fn new(op: IndiOp) -> Self {
        let (kind, value, timeout, retries) = match op {
            IndiOp::Set => (IndiKind::Switch, "On", "30", "2"),
            IndiOp::Wait => (IndiKind::State, "OK", "120", "0"),
        };
        Self {
            op,
            device: String::new(),
            property: String::new(),
            element: String::new(),
            kind,
            cond: Cond::Eq,
            value: value.to_string(),
            margin: fmt_num(INDI_MARGIN.default),
            wait_done: true,
            timeout: timeout.to_string(),
            retries: retries.to_string(),
            on_fail: FAIL_ABORT,
        }
    }

    /// One line for KStars' queue viewer and Files › Planning.
    pub fn summary(&self) -> String {
        let target = match self.kind {
            IndiKind::State => format!("{} state", self.property.trim()),
            _ => format!("{}.{}", self.property.trim(), self.element.trim()),
        };
        match self.op {
            IndiOp::Set => format!("Set {} · {target} = {}", self.device.trim(), self.value),
            IndiOp::Wait => format!("Wait {} · {target} {} {}", self.device.trim(), self.cond.symbol(), self.value),
        }
    }
}

pub fn check_indi(step: usize, s: &IndiStep) -> Result<(), QueueError> {
    let needs_element = s.kind != IndiKind::State;
    if s.device.trim().is_empty() || s.property.trim().is_empty() || (needs_element && s.element.trim().is_empty()) {
        return Err(QueueError::IndiIncomplete { step });
    }
    let value_ok = match s.kind.choices() {
        Some(choices) => choices.contains(&s.value.trim()),
        None if s.kind == IndiKind::Number => s.value.trim().parse::<f64>().is_ok_and(f64::is_finite),
        None => true,
    };
    let op_ok = match s.op {
        IndiOp::Set => s.kind.settable(),
        IndiOp::Wait => s.kind.conds().contains(&s.cond),
    };
    if !value_ok || !op_ok {
        return Err(QueueError::IndiBadValue { step });
    }
    check_param(step, &INDI_TIMEOUT, &s.timeout)?;
    check_param(step, &INDI_RETRIES, &s.retries)?;
    if s.op == IndiOp::Wait && s.cond == Cond::Within {
        check_param(step, &INDI_MARGIN, &s.margin)?;
    }
    Ok(())
}

/// A value as `SetAction` / `EvaluateAction` read it: a number, a bool for a
/// switch or light, a string otherwise.
fn indi_value(kind: IndiKind, text: &str) -> Value {
    match kind {
        IndiKind::Number => num_value(text.trim().parse().unwrap_or(0.0)),
        IndiKind::Switch | IndiKind::Light => Value::Bool(text.trim() == "On"),
        IndiKind::State => Value::String(text.trim().to_string()),
        IndiKind::Text => Value::String(text.to_string()),
    }
}

fn indi_task(s: &IndiStep) -> Value {
    let device = s.device.trim();
    let element = if s.kind == IndiKind::State { "STATE" } else { s.element.trim() };
    let num = |text: &str, p: &ParamSpec| num_value(text.trim().parse().unwrap_or(p.default));
    let mut action = json!({
        "device": device,
        "property": s.property.trim(),
        "element": element,
        "timeout": num(&s.timeout, &INDI_TIMEOUT),
        "retries": num(&s.retries, &INDI_RETRIES),
        "failure_action": s.on_fail,
    });
    let extra = match s.op {
        IndiOp::Set => json!({
            "type": "SET",
            "value": indi_value(s.kind, &s.value),
            "wait_for_completion": s.wait_done,
        }),
        IndiOp::Wait => {
            let mut e = json!({
                "type": "EVALUATE",
                "property_type": s.kind as u8,
                "condition": s.cond as u8,
                "target": indi_value(s.kind, &s.value),
            });
            if s.cond == Cond::Within {
                e["margin"] = num(&s.margin, &INDI_MARGIN);
            }
            e
        }
    };
    if let (Some(a), Value::Object(e)) = (action.as_object_mut(), extra) {
        a.extend(e);
    }
    json!({
        "name": s.summary(),
        "template_id": "",
        "device": device,
        "category": "Custom",
        "parameters": {},
        "actions": [action],
    })
}

/// A task holding one `SET` or `EVALUATE` and no template — what
/// [`indi_task`] writes. Anything else isn't a custom step.
fn indi_from_task(task: &Value) -> Option<IndiStep> {
    if !task["template_id"].as_str().unwrap_or_default().is_empty() {
        return None;
    }
    let [a] = task["actions"].as_array()?.as_slice() else { return None };
    let op = match a["type"].as_str()? {
        "SET" => IndiOp::Set,
        "EVALUATE" => IndiOp::Wait,
        _ => return None,
    };
    let text = |v: &Value| match v {
        Value::Bool(on) => (if *on { "On" } else { "Off" }).to_string(),
        Value::Number(n) => fmt_num(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let (kind, cond, value) = match op {
        IndiOp::Set => {
            let kind = match &a["value"] {
                Value::Bool(_) => IndiKind::Switch,
                Value::Number(_) => IndiKind::Number,
                _ => IndiKind::Text,
            };
            (kind, Cond::Eq, text(&a["value"]))
        }
        IndiOp::Wait => (
            *KINDS.get(a["property_type"].as_u64()? as usize)?,
            *CONDS.get(a["condition"].as_u64()? as usize)?,
            text(&a["target"]),
        ),
    };
    let str_of = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let num = |key: &str, default: f64| fmt_num(a[key].as_f64().unwrap_or(default));
    Some(IndiStep {
        op,
        device: a["device"].as_str().map(str::to_string).unwrap_or_else(|| str_of(&task["device"])),
        property: str_of(&a["property"]),
        element: if kind == IndiKind::State { String::new() } else { str_of(&a["element"]) },
        kind,
        cond,
        value,
        margin: num("margin", INDI_MARGIN.default),
        wait_done: a["wait_for_completion"].as_bool().unwrap_or(true),
        timeout: num("timeout", INDI_TIMEOUT.default),
        retries: num("retries", INDI_RETRIES.default),
        // Absent means ABORT_QUEUE; SKIP_TO_NEXT_TASK is CONTINUE for a
        // one-action task.
        on_fail: if a["failure_action"].as_u64().unwrap_or(0) == 0 { FAIL_ABORT } else { FAIL_CONTINUE },
    })
}

// ── Built-in steps, spelled out ──────────────────────────────────────────────

/// The `actions` of each system template, verbatim from
/// `kstars/kstars/data/taskqueue/templates/system/*.json`.
const BUILTIN_ACTIONS: &str = r#"{
"camera_cool": [
  {"type": "SET", "property": "CCD_TEMP_RAMP", "element": "RAMP_SLOPE", "value": "${ramp_slope}", "timeout": 30, "retries": 2, "failure_action": 1},
  {"type": "SET", "property": "CCD_TEMP_RAMP", "element": "RAMP_THRESHOLD", "value": "${ramp_threshold}", "timeout": 30, "retries": 2, "failure_action": 1},
  {"type": "SET", "property": "CCD_TEMPERATURE", "element": "CCD_TEMPERATURE_VALUE", "value": "${target_temperature}", "timeout": 30, "retries": 2, "failure_action": 0},
  {"type": "EVALUATE", "property": "CCD_TEMPERATURE", "element": "CCD_TEMPERATURE_VALUE", "property_type": 0, "condition": 6, "target": "${target_temperature}", "margin": "${tolerance}", "timeout": "${max_wait_time}", "retries": 0, "failure_action": 0},
  {"type": "EVALUATE", "property": "CCD_TEMPERATURE", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "timeout": 60, "retries": 2, "failure_action": 0}
],
"camera_warm": [
  {"type": "SET", "property": "CCD_TEMP_RAMP", "element": "RAMP_SLOPE", "value": "${ramp_slope}", "timeout": 30, "retries": 2, "failure_action": 1},
  {"type": "SET", "property": "CCD_TEMP_RAMP", "element": "RAMP_THRESHOLD", "value": "${ramp_threshold}", "timeout": 30, "retries": 2, "failure_action": 1},
  {"type": "SET", "property": "CCD_TEMPERATURE", "element": "CCD_TEMPERATURE_VALUE", "value": "${target_temperature}", "timeout": 30, "retries": 2, "failure_action": 0},
  {"type": "EVALUATE", "property": "CCD_TEMPERATURE", "element": "CCD_TEMPERATURE_VALUE", "property_type": 0, "condition": 6, "target": "${target_temperature}", "margin": "${tolerance}", "timeout": "${max_wait_time}", "retries": 0, "failure_action": 0},
  {"type": "EVALUATE", "property": "CCD_TEMPERATURE", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "timeout": 60, "retries": 2, "failure_action": 0}
],
"camera_warm_passive": [
  {"type": "SET", "property": "CCD_COOLER", "element": "COOLER_OFF", "value": true, "timeout": 30, "retries": 2, "failure_action": 1},
  {"type": "EVALUATE", "property": "CCD_TEMPERATURE", "element": "CCD_TEMPERATURE_VALUE", "property_type": 0, "condition": 4, "target": "${target_temperature}", "timeout": "${max_wait_time}", "retries": 0, "failure_action": 0}
],
"delay": [
  {"type": "DELAY", "duration": "${delay_seconds}", "unit": 0, "timeout": "${delay_seconds}", "retries": 0, "failure_action": 1}
],
"dome_park": [
  {"type": "SET", "property": "DOME_PARK", "element": "PARK", "value": true, "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0},
  {"type": "EVALUATE", "property": "DOME_PARK", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "timeout": "${wait_timeout}", "retries": 1, "failure_action": 0}
],
"dome_unpark": [
  {"type": "SET", "property": "DOME_PARK", "element": "UNPARK", "value": true, "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0},
  {"type": "EVALUATE", "property": "DOME_PARK", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "timeout": "${wait_timeout}", "retries": 1, "failure_action": 0}
],
"dustcap_park": [
  {"type": "SET", "property": "CAP_PARK", "element": "PARK", "value": true, "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0},
  {"type": "EVALUATE", "property": "CAP_PARK", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0}
],
"dustcap_unpark": [
  {"type": "SET", "property": "CAP_PARK", "element": "UNPARK", "value": true, "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0},
  {"type": "EVALUATE", "property": "CAP_PARK", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0}
],
"mount_park": [
  {"type": "SET", "property": "TELESCOPE_PARK", "element": "PARK", "value": true, "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0},
  {"type": "EVALUATE", "property": "TELESCOPE_PARK", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0}
],
"mount_unpark": [
  {"type": "SET", "property": "TELESCOPE_PARK", "element": "UNPARK", "value": true, "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0},
  {"type": "EVALUATE", "property": "TELESCOPE_PARK", "element": "STATE", "property_type": 4, "condition": 0, "target": "OK", "timeout": "${wait_timeout}", "retries": 2, "failure_action": 0}
],
"script_execute": [
  {"type": "SCRIPT", "script_path": "${script_path}", "timeout": "${timeout}", "retries": 2, "failure_action": 0}
]
}"#;

/// A value that is exactly `${name}` becomes that parameter, as
/// `Task::substituteValue` does (no template embeds one mid-string).
fn fill(v: &Value, params: &Map<String, Value>) -> Value {
    match v {
        Value::String(s) => s
            .strip_prefix("${")
            .and_then(|rest| rest.strip_suffix('}'))
            .and_then(|name| params.get(name))
            .cloned()
            .unwrap_or_else(|| v.clone()),
        Value::Array(items) => Value::Array(items.iter().map(|x| fill(x, params)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), fill(x, params))).collect()),
        other => other.clone(),
    }
}

/// What `Task::instantiateFromTemplate` would build, before KStars binds a device.
fn template_task(id: &str, params: Map<String, Value>) -> Value {
    let actions = serde_json::from_str::<Value>(BUILTIN_ACTIONS)
        .ok()
        .and_then(|all| all.get(id).map(|a| fill(a, &params)))
        .unwrap_or_else(|| json!([]));
    json!({
        "name": id,
        "template_id": id,
        "device": "",
        "category": "",
        "parameters": params,
        "actions": actions,
    })
}

// ── Queue (de)serialisation ──────────────────────────────────────────────────

/// Build the queue for `steps`. Call `queue_model::validate` first.
pub fn to_queue(title: &str, steps: &[QueueStep], scripts_dir: &str) -> Value {
    let items: Vec<Value> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let task = match step {
                QueueStep::Template { id, values, .. } => template_task(id, template_params(id, values)),
                QueueStep::Script { script, timeout } => {
                    template_task(SCRIPT_TEMPLATE_ID, script_params(script, timeout, scripts_dir))
                }
                QueueStep::Indi(s) => indi_task(s),
                QueueStep::Unknown { task, .. } => task.clone(),
            };
            json!({ "id": format!("junos-{}", i + 1), "task": task })
        })
        .collect();
    json!({
        "name": title,
        "description": DESCRIPTION,
        "version": "1.0",
        "items": items,
    })
}

/// Parse a queue back into `(title, steps)`. Like `from_collection`, managed
/// script bodies are left for the caller to fetch.
pub fn from_queue(doc: &Value, scripts_dir: &str) -> Result<(String, Vec<QueueStep>), String> {
    let items = doc
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "not a task queue (no \"items\" array)".to_string())?;
    let title = doc.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let steps = items
        .iter()
        .map(|item| {
            let task = &item["task"];
            let id = task["template_id"].as_str().unwrap_or_default();
            let params = task.get("parameters");
            if id == SCRIPT_TEMPLATE_ID {
                return script_step(params, scripts_dir);
            }
            if let Some(spec) = template(id) {
                // This format always aborts on a missing device.
                return QueueStep::Template { id: spec.id, values: template_values(spec, params), on_missing_device: FAIL_ABORT };
            }
            match indi_from_task(task) {
                Some(s) => QueueStep::Indi(s),
                None => QueueStep::Unknown { task: task.clone(), native: true },
            }
        })
        .collect();
    Ok((title, steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::queue_model::ScriptRef;

    #[test]
    fn round_trips_custom_and_builtin_steps() {
        let mut set = IndiStep::new(IndiOp::Set);
        set.device = "Dome Simulator".into();
        set.property = "DOME_SHUTTER".into();
        set.element = "SHUTTER_OPEN".into();
        let mut wait = IndiStep::new(IndiOp::Wait);
        wait.device = "CCD Simulator".into();
        wait.property = "CCD_TEMPERATURE".into();
        wait.element = "CCD_TEMPERATURE_VALUE".into();
        wait.kind = IndiKind::Number;
        wait.cond = Cond::Within;
        wait.value = "-10".into();
        let steps = vec![
            QueueStep::new_template("mount_unpark").unwrap(),
            QueueStep::Indi(set),
            QueueStep::Indi(wait),
            QueueStep::Script { script: ScriptRef::External { path: "/opt/roof.sh".into() }, timeout: "300".into() },
        ];

        let doc = to_queue("Post", &steps, "/q/scripts");
        let unpark = &doc["items"][0]["task"];
        assert_eq!(unpark["template_id"], "mount_unpark");
        assert_eq!(unpark["actions"][0]["timeout"], 5);
        assert_eq!(doc["items"][1]["task"]["actions"][0]["value"], true);
        assert_eq!(doc["items"][2]["task"]["actions"][0]["condition"], 6);
        assert_eq!(doc["items"][3]["task"]["actions"][0]["script_path"], "/opt/roof.sh");

        let (title, back) = from_queue(&doc, "/q/scripts").unwrap();
        assert_eq!(title, "Post");
        let QueueStep::Template { on_missing_device, .. } = &back[0] else { panic!("built-in step") };
        assert_eq!(*on_missing_device, FAIL_ABORT);
        assert_eq!(back[1..], steps[1..]);
    }
}
