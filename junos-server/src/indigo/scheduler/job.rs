//! A scheduler job as KStars' `SchedulerJob`: the file fields, the runtime
//! state, its `toJson` for `scheduler_get_jobs`, the add-job form
//! (`fillJobFromUI`) and the module settings `scheduler_set_all_settings`
//! carries.

use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};

use super::constraints::{self, Rules, Site};
use crate::indigo::scansexa;

#[derive(Clone, Debug, PartialEq)]
pub enum StartCond {
    Asap,
    At(DateTime<Local>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Completion {
    Sequence,
    Repeat(u32),
    Loop,
    At(DateTime<Local>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Steps {
    pub track: bool,
    pub focus: bool,
    pub align: bool,
    pub guide: bool,
}

/// `SchedulerJobStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// KStars' `SchedulerJobStatus`, in full: the browser labels every value, this
// runner never evaluates asynchronously.
#[allow(dead_code)]
#[repr(i64)]
pub enum JobState {
    Idle = 0,
    Evaluation = 1,
    Scheduled = 2,
    Busy = 3,
    Error = 4,
    Aborted = 5,
    Invalid = 6,
    Complete = 7,
}

/// `SchedulerJobStage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum Stage {
    Idle = 0,
    Slewing = 1,
    SlewComplete = 2,
    Focusing = 3,
    FocusComplete = 4,
    Aligning = 5,
    AlignComplete = 6,
    Reslewing = 7,
    ReslewingComplete = 8,
    PostAlignFocusing = 9,
    PostAlignFocusingComplete = 10,
    Guiding = 11,
    GuidingComplete = 12,
    Capturing = 13,
    Complete = 14,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SchedulerJob {
    pub name: String,
    pub group: String,
    /// J2000, hours.
    pub ra0_h: f64,
    /// J2000, degrees.
    pub dec0_deg: f64,
    pub pa: f64,
    pub sequence: PathBuf,
    pub start: StartCond,
    pub completion: Completion,
    pub min_alt: Option<f64>,
    pub min_moon_sep: Option<f64>,
    pub max_moon_alt: Option<f64>,
    pub twilight: bool,
    pub horizon: bool,
    pub steps: Steps,
    // Filled by the runner from the .esq
    pub sequence_count: u32,
    pub duration_secs: f64,
    pub in_sequence_focus: bool,
    // Runtime
    pub state: JobState,
    pub stage: Stage,
    pub completed_count: u32,
    pub repeats_required: u32,
    pub repeats_remaining: u32,
    pub startup_time: Option<DateTime<Local>>,
    pub stop_time: Option<DateTime<Local>>,
    pub last_abort: Option<Instant>,
    pub last_error: Option<Instant>,
    pub last_abort_reason: String,
    pub state_time: Option<DateTime<Local>>,
}

impl SchedulerJob {
    pub fn new(name: impl Into<String>, ra0_h: f64, dec0_deg: f64, sequence: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            group: String::new(),
            ra0_h,
            dec0_deg,
            pa: 0.0,
            sequence: sequence.into(),
            start: StartCond::Asap,
            completion: Completion::Sequence,
            min_alt: None,
            min_moon_sep: None,
            max_moon_alt: None,
            twilight: false,
            horizon: false,
            steps: Steps::default(),
            sequence_count: 0,
            duration_secs: 0.0,
            in_sequence_focus: false,
            state: JobState::Idle,
            stage: Stage::Idle,
            completed_count: 0,
            repeats_required: 1,
            repeats_remaining: 1,
            startup_time: None,
            stop_time: None,
            last_abort: None,
            last_error: None,
            last_abort_reason: String::new(),
            state_time: None,
        }
    }

    /// Sets the completion condition and the repeat counters it implies.
    pub fn set_completion(&mut self, completion: Completion) {
        self.repeats_required = match completion {
            Completion::Repeat(n) => n.max(1),
            _ => 1,
        };
        self.repeats_remaining = self.repeats_required;
        self.completion = completion;
    }

    pub fn rules(&self) -> Rules {
        Rules {
            min_alt: self.min_alt,
            min_moon_sep: self.min_moon_sep,
            max_moon_alt: self.max_moon_alt,
            twilight: self.twilight,
        }
    }

    /// Back to the file state: nothing run, nothing scheduled.
    pub fn reset(&mut self) {
        self.state = JobState::Idle;
        self.stage = Stage::Idle;
        self.completed_count = 0;
        self.repeats_remaining = self.repeats_required;
        self.startup_time = None;
        self.stop_time = None;
        self.state_time = None;
        self.last_abort = None;
        self.last_error = None;
        self.last_abort_reason.clear();
    }

    /// `SchedulerJob::toJson`, for `scheduler_get_jobs`. `jd` is now; without
    /// a site the altitudes are 0 and the formatted cells empty.
    pub fn to_json(&self, jd: f64, site: Option<&Site>) -> Value {
        let arrow = |setting: bool| if setting { "↓" } else { "↑" };
        let (altitude, altitude_formatted) = site.map_or((0.0, String::new()), |site| {
            let s = constraints::snapshot(self.ra0_h, self.dec0_deg, jd, site);
            (s.alt, format!("{}{:.1}°", arrow(s.setting), s.alt))
        });
        // KStars' startup / completion cells: ⚠ when below the job's minimum.
        let cell = |t: Option<DateTime<Local>>| match (t, site) {
            (Some(t), Some(site)) => {
                let s = constraints::snapshot(self.ra0_h, self.dec0_deg, jd_of(&t), site);
                let warn = if self.min_alt.is_some_and(|min| s.alt < min) { "⚠" } else { "" };
                format!("{warn}{}{:.1}° {}", arrow(s.setting), s.alt, t.format("%H:%M"))
            }
            _ => String::new(),
        };
        let iso = |t: Option<DateTime<Local>>| t.map_or_else(|| "--".to_string(), |t| t.to_rfc3339());
        let startup = if self.state == JobState::Busy { self.state_time } else { self.startup_time };
        let completion = match self.completion {
            Completion::At(t) => Some(t),
            _ => None,
        };
        json!({
            "name": self.name,
            "pa": self.pa,
            "targetRA": self.ra0_h,
            "targetDEC": self.dec0_deg,
            "state": self.state as i64,
            "stage": self.stage as i64,
            "sequenceCount": self.sequence_count,
            "completedCount": self.completed_count,
            "minAltitude": self.min_alt.unwrap_or(0.0),
            "minMoonSeparation": self.min_moon_sep.unwrap_or(-1.0),
            "maxMoonAltitude": self.max_moon_alt.unwrap_or(90.0),
            "repeatsRequired": self.repeats_required,
            "repeatsRemaining": self.repeats_remaining,
            "inSequenceFocus": self.in_sequence_focus,
            "startupTime": iso(startup),
            "completionTime": iso(completion),
            "stopTime": iso(self.stop_time),
            "altitude": altitude,
            "altitudeFormatted": altitude_formatted,
            "startupFormatted": cell(startup),
            "endFormatted": cell(self.stop_time),
            "sequence": self.sequence.to_string_lossy(),
        })
    }
}

// ── Time helpers ─────────────────────────────────────────────────────────────

/// Julian Date of a local time.
pub fn jd_of(t: &DateTime<Local>) -> f64 {
    2_440_587.5 + (t.timestamp() as f64 + f64::from(t.timestamp_subsec_nanos()) * 1e-9) / 86_400.0
}

/// Local time of a Julian Date (to the second).
pub fn local_of_jd(jd: f64) -> DateTime<Local> {
    let secs = ((jd - 2_440_587.5) * 86_400.0).round() as i64;
    DateTime::from_timestamp(secs, 0).unwrap_or_default().with_timezone(&Local)
}

/// KStars' local ISO (`Qt::ISODate` without an offset, with or without the
/// seconds), or RFC 3339. The first reading of an ambiguous DST hour.
pub fn parse_local_time(s: &str) -> Option<DateTime<Local>> {
    let s = s.trim();
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            let local = Local.from_local_datetime(&naive);
            return local.single().or_else(|| local.earliest());
        }
    }
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Local))
}

/// `Qt::ISODate` of a local `QDateTime`: no offset.
pub fn iso_local(t: &DateTime<Local>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S").to_string()
}

// ── The add-job form ─────────────────────────────────────────────────────────

/// Lenient readers: the browser sends JSON booleans and numbers, but KStars'
/// `QVariantMap` round trip can turn them into 0/1 or strings.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "on" | "yes"),
        _ => false,
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The add-job form, the union of the `scheduler_set_all_settings` maps the
/// browser sends before `scheduler_add_jobs`.
#[derive(Clone, Debug, Default)]
pub struct Form(pub Map<String, Value>);

impl Form {
    pub fn apply(&mut self, map: &Map<String, Value>) {
        for (k, v) in map {
            self.0.insert(k.clone(), v.clone());
        }
    }

    fn get(&self, key: &str) -> &Value {
        self.0.get(key).unwrap_or(&Value::Null)
    }

    fn flag(&self, key: &str) -> bool {
        truthy(self.get(key))
    }

    fn num(&self, key: &str) -> Option<f64> {
        number(self.get(key))
    }

    /// `Scheduler::fillJobFromUI`.
    pub fn to_job(&self) -> Result<SchedulerJob, String> {
        let name = string(self.get("nameEdit"));
        if name.is_empty() {
            return Err("job name is empty".to_string());
        }
        let sequence = string(self.get("sequenceEdit"));
        if sequence.is_empty() {
            return Err("no sequence file".to_string());
        }
        let coord = |key: &str| {
            let s = string(self.get(key));
            scansexa::parse(&s).ok_or_else(|| format!("invalid {key} \"{s}\""))
        };
        let ra0_h = coord("raBox")?;
        let dec0_deg = coord("decBox")?;

        let mut job = SchedulerJob::new(name, ra0_h, dec0_deg, sequence);
        job.pa = self.num("positionAngleSpin").unwrap_or(0.0);

        job.start = if self.flag("asapConditionR") || !self.flag("startupTimeConditionR") {
            StartCond::Asap
        } else {
            let s = string(self.get("startupTimeEdit"));
            StartCond::At(parse_local_time(&s).ok_or_else(|| format!("invalid startup time \"{s}\""))?)
        };

        job.min_alt = self.flag("schedulerAltitude").then(|| self.num("schedulerAltitudeValue")).flatten();
        job.min_moon_sep = self.flag("schedulerMoonSeparation").then(|| self.num("schedulerMoonSeparationValue")).flatten();
        job.max_moon_alt = self.flag("schedulerMoonAltitude").then(|| self.num("schedulerMoonAltitudeMaxValue")).flatten();
        job.twilight = self.flag("schedulerTwilight");
        job.horizon = self.flag("schedulerHorizon");
        job.steps = Steps {
            track: self.flag("schedulerTrackStep"),
            focus: self.flag("schedulerFocusStep"),
            align: self.flag("schedulerAlignStep"),
            guide: self.flag("schedulerGuideStep"),
        };

        let completion = if self.flag("schedulerCompleteSequences") {
            Completion::Sequence
        } else if self.flag("schedulerRepeatSequences") {
            Completion::Repeat(self.num("schedulerRepeatSequencesLimit").map_or(1, |n| n.max(1.0) as u32))
        } else if self.flag("schedulerUntilTerminated") {
            Completion::Loop
        } else if self.flag("schedulerUntil") {
            let s = string(self.get("schedulerUntilValue"));
            Completion::At(parse_local_time(&s).ok_or_else(|| format!("invalid completion time \"{s}\""))?)
        } else {
            Completion::Sequence
        };
        job.set_completion(completion);
        Ok(job)
    }
}

// ── Module settings ──────────────────────────────────────────────────────────

/// The scheduler's own settings among the `scheduler_set_all_settings` keys.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub greedy: bool,
    pub remember_progress: bool,
    pub reschedule_errors: bool,
    pub error_delay_secs: u32,
    pub startup_enabled: bool,
    pub shutdown_enabled: bool,
    pub pre_startup: String,
    pub post_startup: String,
    pub pre_shutdown: String,
    pub post_shutdown: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            greedy: true,
            remember_progress: false,
            reschedule_errors: false,
            error_delay_secs: 600,
            startup_enabled: false,
            shutdown_enabled: false,
            pre_startup: String::new(),
            post_startup: String::new(),
            pre_shutdown: String::new(),
            post_shutdown: String::new(),
        }
    }
}

impl Settings {
    /// Applies one key; `true` when it was a settings key (consumed).
    pub fn apply(&mut self, key: &str, v: &Value) -> bool {
        match key {
            "kcfg_GreedyScheduling" => self.greedy = truthy(v),
            "kcfg_RememberJobProgress" => self.remember_progress = truthy(v),
            "errorHandlingRescheduleErrorsCB" => self.reschedule_errors = truthy(v),
            "errorHandlingStrategyDelay" => {
                if let Some(n) = number(v) {
                    self.error_delay_secs = n.max(0.0) as u32;
                }
            }
            "schedulerStartupEnabled" => self.startup_enabled = truthy(v),
            "schedulerShutdownEnabled" => self.shutdown_enabled = truthy(v),
            "schedulerPreStartupScript" => self.pre_startup = string(v),
            "schedulerPostStartupScript" => self.post_startup = string(v),
            "schedulerPreShutdownScript" => self.pre_shutdown = string(v),
            "schedulerPostShutdownScript" => self.post_shutdown = string(v),
            _ => return false,
        }
        true
    }

    /// The nine keys `scheduler_get_all_settings` reports for them.
    pub fn payload(&self) -> Value {
        json!({
            "kcfg_GreedyScheduling": self.greedy,
            "kcfg_RememberJobProgress": self.remember_progress,
            "errorHandlingRescheduleErrorsCB": self.reschedule_errors,
            "schedulerStartupEnabled": self.startup_enabled,
            "schedulerShutdownEnabled": self.shutdown_enabled,
            "schedulerPreStartupScript": self.pre_startup,
            "schedulerPostStartupScript": self.post_startup,
            "schedulerPreShutdownScript": self.pre_shutdown,
            "schedulerPostShutdownScript": self.post_shutdown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn constraints_map() -> Map<String, Value> {
        map(json!({"asapConditionR":true,"startupTimeConditionR":false,"startupTimeEdit":"","schedulerAltitude":true,"schedulerAltitudeValue":30.0,"schedulerMoonSeparation":false,"schedulerMoonSeparationValue":0.0,"schedulerMoonAltitude":false,"schedulerMoonAltitudeMaxValue":90.0,"schedulerTwilight":true,"schedulerHorizon":false}))
    }

    fn job_map() -> Map<String, Value> {
        map(json!({"nameEdit":"M 31","raBox":"0.712300","decBox":"41.269000","sequenceEdit":"/home/x/.junos-sequences/M_31.esq","positionAngleSpin":0.0,"schedulerTrackStep":true,"schedulerFocusStep":false,"schedulerAlignStep":true,"schedulerGuideStep":true,"schedulerCompleteSequences":false,"schedulerRepeatSequences":true,"schedulerRepeatSequencesLimit":3,"schedulerUntilTerminated":false,"schedulerUntil":false,"schedulerUntilValue":""}))
    }

    #[test]
    fn form_to_job() {
        let mut form = Form::default();
        form.apply(&constraints_map());
        form.apply(&job_map());
        let job = form.to_job().unwrap();
        let mut expected = SchedulerJob::new("M 31", 0.7123, 41.269, "/home/x/.junos-sequences/M_31.esq");
        expected.min_alt = Some(30.0);
        expected.twilight = true;
        expected.steps = Steps { track: true, focus: false, align: true, guide: true };
        expected.set_completion(Completion::Repeat(3));
        assert_eq!(job, expected);
        assert_eq!(job.repeats_required, 3);
        assert_eq!(job.repeats_remaining, 3);
        assert_eq!(job.rules(), Rules { min_alt: Some(30.0), min_moon_sep: None, max_moon_alt: None, twilight: true });
    }

    #[test]
    fn form_at_variants_and_lenient_types() {
        let mut form = Form::default();
        form.apply(&constraints_map());
        form.apply(&job_map());
        // Sexagesimal coordinates, 0/1 and string booleans, a numeric string.
        form.apply(&map(json!({
            "asapConditionR": 0, "startupTimeConditionR": "true", "startupTimeEdit": "2025-07-15T22:30",
            "raBox": "00 42 44.3", "decBox": "+41 16 09",
            "schedulerRepeatSequences": false, "schedulerUntil": 1, "schedulerUntilValue": "2025-07-16T03:00:00",
            "schedulerMoonSeparation": "1", "schedulerMoonSeparationValue": "45",
            "schedulerAltitude": false,
        })));
        let job = form.to_job().unwrap();
        let start = parse_local_time("2025-07-15T22:30:00").unwrap();
        let until = parse_local_time("2025-07-16T03:00:00").unwrap();
        assert_eq!(job.start, StartCond::At(start));
        assert_eq!(job.completion, Completion::At(until));
        assert_eq!(job.repeats_required, 1);
        assert!((job.ra0_h - 0.712_305).abs() < 1e-5);
        assert!((job.dec0_deg - 41.269_166).abs() < 1e-5);
        assert_eq!(job.min_alt, None);
        assert_eq!(job.min_moon_sep, Some(45.0));
        assert_eq!(iso_local(&start), "2025-07-15T22:30:00");
    }

    #[test]
    fn form_errors() {
        let mut form = Form::default();
        form.apply(&job_map());
        form.0.insert("nameEdit".into(), json!(""));
        assert_eq!(form.to_job().unwrap_err(), "job name is empty");
        form.0.insert("nameEdit".into(), json!("M 31"));
        form.0.insert("sequenceEdit".into(), json!(""));
        assert!(form.to_job().is_err());
        form.0.insert("sequenceEdit".into(), json!("/x.esq"));
        form.0.insert("raBox".into(), json!("abc"));
        assert!(form.to_job().unwrap_err().contains("raBox"));
    }

    #[test]
    fn json_keys_and_fallbacks() {
        let mut job = SchedulerJob::new("M 31", 0.7123, 41.269, "/x/M_31.esq");
        let v = job.to_json(2_460_676.5, None);
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        for k in [
            "name", "pa", "targetRA", "targetDEC", "state", "stage", "sequenceCount", "completedCount", "minAltitude",
            "minMoonSeparation", "maxMoonAltitude", "repeatsRequired", "repeatsRemaining", "inSequenceFocus",
            "startupTime", "completionTime", "stopTime", "altitude", "altitudeFormatted", "startupFormatted",
            "endFormatted", "sequence",
        ] {
            assert!(keys.contains(&k), "{k} missing");
        }
        assert_eq!(keys.len(), 22);
        assert_eq!(v["state"], 0);
        assert_eq!(v["stage"], 0);
        assert_eq!(v["minAltitude"], 0.0);
        assert_eq!(v["minMoonSeparation"], -1.0);
        assert_eq!(v["maxMoonAltitude"], 90.0);
        assert_eq!(v["startupTime"], "--");
        assert_eq!(v["completionTime"], "--");
        assert_eq!(v["stopTime"], "--");
        assert_eq!(v["altitude"], 0.0);
        assert_eq!(v["altitudeFormatted"], "");
        assert_eq!(v["startupFormatted"], "");
        assert_eq!(v["sequence"], "/x/M_31.esq");

        // With a site and times: the cells fill in, Busy reports the state time.
        let site = Site { lat: 48.5, lon_east: -2.3 };
        let t0 = parse_local_time("2025-10-15T22:00:00").unwrap();
        let t1 = parse_local_time("2025-10-16T01:00:00").unwrap();
        job.min_alt = Some(89.0);
        job.startup_time = Some(t0);
        job.stop_time = Some(t1);
        job.state = JobState::Scheduled;
        job.set_completion(Completion::At(t1));
        let v = job.to_json(jd_of(&t0), Some(&site));
        assert_eq!(v["state"], 2);
        assert_eq!(v["startupTime"], t0.to_rfc3339());
        assert_eq!(v["completionTime"], t1.to_rfc3339());
        assert_eq!(v["stopTime"], t1.to_rfc3339());
        assert!(v["altitude"].as_f64().unwrap() > 0.0);
        let alt = v["altitudeFormatted"].as_str().unwrap();
        assert!(alt.starts_with('↑') || alt.starts_with('↓'));
        assert!(alt.ends_with('°'), "{alt}");
        let startup = v["startupFormatted"].as_str().unwrap();
        assert!(startup.starts_with('⚠') && startup.ends_with(" 22:00"), "{startup}");
        assert!(v["endFormatted"].as_str().unwrap().ends_with(" 01:00"));
        job.state = JobState::Busy;
        job.state_time = Some(t1);
        let v = job.to_json(jd_of(&t0), Some(&site));
        assert_eq!(v["startupTime"], t1.to_rfc3339());
        assert!(v["startupFormatted"].as_str().unwrap().ends_with(" 01:00"));
    }

    #[test]
    fn reset_clears_the_run() {
        let mut job = SchedulerJob::new("M 31", 0.7123, 41.269, "/x/M_31.esq");
        job.set_completion(Completion::Repeat(4));
        job.state = JobState::Busy;
        job.stage = Stage::Capturing;
        job.completed_count = 7;
        job.repeats_remaining = 1;
        job.startup_time = Some(Local::now());
        job.stop_time = Some(Local::now());
        job.state_time = Some(Local::now());
        job.last_abort = Some(Instant::now());
        job.last_abort_reason = "cloud".into();
        job.reset();
        let mut fresh = SchedulerJob::new("M 31", 0.7123, 41.269, "/x/M_31.esq");
        fresh.set_completion(Completion::Repeat(4));
        assert_eq!(job, fresh);
        assert_eq!(job.repeats_remaining, 4);
    }

    #[test]
    fn jd_round_trip() {
        let t = parse_local_time("2025-07-15T22:30:00").unwrap();
        assert_eq!(local_of_jd(jd_of(&t)), t);
        // 2025-01-01 00:00 UT
        let utc = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z").unwrap().with_timezone(&Local);
        assert!((jd_of(&utc) - 2_460_676.5).abs() < 1e-9);
        assert_eq!(parse_local_time("2025-01-01T00:00:00Z"), Some(utc));
        assert_eq!(parse_local_time("nope"), None);
    }

    #[test]
    fn settings_round_trip() {
        let mut s = Settings::default();
        assert!(s.greedy && s.error_delay_secs == 600 && !s.startup_enabled);
        let incoming = json!({
            "kcfg_GreedyScheduling": false, "kcfg_RememberJobProgress": 1, "errorHandlingRescheduleErrorsCB": "true",
            "errorHandlingStrategyDelay": "120", "schedulerStartupEnabled": true, "schedulerShutdownEnabled": true,
            "schedulerPreStartupScript": "/q/pre.json", "schedulerPostStartupScript": "/q/post.json",
            "schedulerPreShutdownScript": "/q/pre_sd.json", "schedulerPostShutdownScript": "/q/post_sd.json",
            "schedulerAltitude": true,
        });
        let mut consumed = Vec::new();
        for (k, v) in incoming.as_object().unwrap() {
            if s.apply(k, v) {
                consumed.push(k.as_str());
            }
        }
        assert_eq!(consumed.len(), 10);
        assert!(!consumed.contains(&"schedulerAltitude"));
        assert_eq!(
            s,
            Settings {
                greedy: false,
                remember_progress: true,
                reschedule_errors: true,
                error_delay_secs: 120,
                startup_enabled: true,
                shutdown_enabled: true,
                pre_startup: "/q/pre.json".into(),
                post_startup: "/q/post.json".into(),
                pre_shutdown: "/q/pre_sd.json".into(),
                post_shutdown: "/q/post_sd.json".into(),
            }
        );
        let p = s.payload();
        assert_eq!(p.as_object().unwrap().len(), 9);
        let mut back = Settings::default();
        back.error_delay_secs = 120;
        for (k, v) in p.as_object().unwrap() {
            assert!(back.apply(k, v));
        }
        assert_eq!(back, s);
    }
}
