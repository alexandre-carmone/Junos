//! The Imaging tab's capture queue on the INDIGO Imager Agent.
//!
//! Ekos' Capture module runs a list of sequence jobs (`.esq` rows). The
//! Imager Agent (3.0-8) has no sequencer, only one batch at a time:
//! `AGENT_IMAGER_BATCH {COUNT, EXPOSURE, DELAY, FRAMES_TO_SKIP_BEFORE_DITHER}`
//! then `AGENT_START_PROCESS EXPOSURE`. So this module *is* the sequencer:
//! one job = one batch, prepared on the agent's camera mirrors (`CCD_*` on
//! the agent device, forwarded to the camera — the filter through
//! `AGENT_WHEEL_FILTER`, the files through `CCD_UPLOAD_MODE` + `CCD_LOCAL_MODE`,
//! see `paths.rs`), run, then the next. Progress comes from
//! `AGENT_IMAGER_STATS` (phase, seconds left) and `CCD_IMAGE_FILE` (one
//! `Ok` per saved frame — the frame counter `FRAME` is bumped when an
//! exposure *starts*, so the saved files are the primary count and
//! `FRAME − 1` only a floor).
//!
//! What the browser sees is Ekos': `capture_get_sequences` (the jobs, bare
//! array) and `new_capture_state` (status label, `seqv`/`seqr`, `expv`/`expr`,
//! `ovp`/`ovt`, newest-first log).
//!
//! Limits (`.esq` head): refocus every N minutes and on a temperature delta
//! are honoured at job boundaries (a `FOCUSING` run before the job); the HFR
//! check, the guide-deviation limits and the flip refocus are stored and
//! echoed but not enforced. A `<PostJobScript>` runs after the row's last
//! frame with KStars' JSON on stdin.

pub mod esq;
pub mod paths;

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::oneshot;
use tracing::warn;

use super::agents::AgentSet;
use super::commands::reply_msg;
use super::process::{Edge, Log, Process, Reaction};
use super::protocol::{new_vector_msg, Change, Kind, PropertyStore, State};

const FILTER_TIMEOUT: Duration = Duration::from_secs(60);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    Light,
    Bias,
    Dark,
    Flat,
    DarkFlat,
}

impl FrameType {
    pub fn from_label(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "bias" => Self::Bias,
            "dark" => Self::Dark,
            "flat" => Self::Flat,
            "dark flat" | "darkflat" => Self::DarkFlat,
            _ => Self::Light,
        }
    }

    /// Ekos' label (`<Type>`, the `Type` job key).
    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Bias => "Bias",
            Self::Dark => "Dark",
            Self::Flat => "Flat",
            Self::DarkFlat => "Dark Flat",
        }
    }

    /// INDIGO's `CCD_FRAME_TYPE` item (`DARKFLAT` only on newer drivers).
    fn item(self) -> &'static str {
        match self {
            Self::Light => "LIGHT",
            Self::Bias => "BIAS",
            Self::Dark => "DARK",
            Self::Flat => "FLAT",
            Self::DarkFlat => "DARKFLAT",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Idle,
    InProgress,
    Aborted,
    Complete,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::InProgress => "In Progress",
            Self::Aborted => "Aborted",
            Self::Complete => "Complete",
        }
    }
}

/// One `.esq` row.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub frame_type: FrameType,
    /// `<Filter>`; `None` leaves the wheel where it is.
    pub filter: Option<String>,
    pub exposure: f64,
    pub count: u32,
    pub delay_s: f64,
    pub bin: (u32, u32),
    pub gain: Option<f64>,
    pub offset: Option<f64>,
    pub iso_index: Option<u32>,
    pub format: String,
    pub encoding: String,
    pub temperature: Option<f64>,
    pub target: String,
    pub fits_dir: String,
    pub placeholder_format: String,
    pub placeholder_suffix: u32,
    pub upload_mode: u8,
    /// `<GuideDitherPerJob>`: 0 the Guide module's, N every N frames, −1 never.
    pub dither: i32,
    pub post_job_script: String,
    pub completed: u32,
    pub status: JobStatus,
}

impl Job {
    pub fn new(frame_type: FrameType, exposure: f64, count: u32) -> Self {
        Self {
            frame_type,
            filter: None,
            exposure,
            count,
            delay_s: 0.0,
            bin: (1, 1),
            gain: None,
            offset: None,
            iso_index: None,
            format: String::new(),
            encoding: "FITS".into(),
            temperature: None,
            target: String::new(),
            fits_dir: String::new(),
            placeholder_format: esq::DEFAULT_PLACEHOLDER.into(),
            placeholder_suffix: esq::DEFAULT_SUFFIX,
            upload_mode: 0,
            dither: 0,
            post_job_script: String::new(),
            completed: 0,
            status: JobStatus::Idle,
        }
    }

    fn remaining(&self) -> u32 {
        self.count.saturating_sub(self.completed)
    }

    /// `Camera::createJsonJob` (`camera_jobs.cpp:877`).
    fn to_json(&self) -> Value {
        let exp = format!("{:.3}", self.exposure);
        let exp = exp.trim_end_matches('0').trim_end_matches('.').to_string();
        let num = |v: Option<f64>| v.map_or_else(|| "--".to_string(), |v| format!("{v:.1}"));
        json!({
            "Status": self.status.label(),
            "Type": self.frame_type.label(),
            "Exp": exp,
            "Filter": self.filter.clone().unwrap_or_default(),
            "Count": format!("{}/{}", self.completed, self.count),
            "Bin": format!("{}x{}", self.bin.0, self.bin.1),
            "ISO/Gain": num(self.gain),
            "Offset": num(self.offset),
            "Encoding": self.encoding,
            "Format": self.format,
            "Temperature": self.temperature.map_or(json!("--"), |t| json!(t)),
            "DitherPerJobEnabled": self.dither >= 0,
            "DitherPerJobFrequency": self.dither.max(0),
        })
    }
}

/// The `.esq` head — exactly the keys the browser's limits panel reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    pub refocus_every: bool,
    pub refocus_every_min: u32,
    pub refocus_temp: bool,
    pub refocus_temp_delta: f64,
    pub refocus_flip: bool,
    pub hfr_check: bool,
    pub hfr_deviation: f64,
    /// 0 last autofocus, 1 fixed, 2 median measure.
    pub hfr_algorithm: u8,
    pub hfr_threshold: f64,
    pub hfr_frames: u32,
    pub guide_abort: bool,
    pub guide_abort_arcsec: f64,
    pub guide_start: bool,
    pub guide_start_arcsec: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            refocus_every: false,
            refocus_every_min: 60,
            refocus_temp: false,
            refocus_temp_delta: 1.0,
            refocus_flip: false,
            hfr_check: false,
            hfr_deviation: 0.5,
            hfr_algorithm: 0,
            hfr_threshold: 10.0,
            hfr_frames: 1,
            guide_abort: false,
            guide_abort_arcsec: 2.0,
            guide_start: false,
            guide_start_arcsec: 2.0,
        }
    }
}

impl Limits {
    fn algorithm_label(&self) -> &'static str {
        match self.hfr_algorithm {
            1 => "Fixed",
            2 => "Median Measure",
            _ => "Last Autofocus",
        }
    }

    pub fn payload(&self) -> Value {
        json!({
            "enforceRefocusEveryN": self.refocus_every,
            "refocusEveryN": self.refocus_every_min,
            "enforceAutofocusOnTemperature": self.refocus_temp,
            "maxFocusTemperatureDelta": self.refocus_temp_delta,
            "refocusAfterMeridianFlip": self.refocus_flip,
            "enforceAutofocusHFR": self.hfr_check,
            "hFRCheckAlgorithm": self.algorithm_label(),
            "hFRThresholdPercentage": self.hfr_threshold,
            "inSequenceCheckFrames": self.hfr_frames,
            "hFRDeviation": self.hfr_deviation,
            "enforceGuideDeviation": self.guide_abort,
            "guideDeviation": self.guide_abort_arcsec,
            "enforceStartGuiderDrift": self.guide_start,
            "startGuideDeviation": self.guide_start_arcsec,
        })
    }

    /// One `capture_set_all_settings` key; `true` when it was a limit.
    pub fn apply(&mut self, key: &str, v: &Value) -> bool {
        let on = v.as_bool().or_else(|| v.as_f64().map(|n| n != 0.0)).unwrap_or(false);
        let num = v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()));
        match key {
            "enforceRefocusEveryN" => self.refocus_every = on,
            "refocusEveryN" => self.refocus_every_min = num.map(|n| n.max(1.0) as u32).unwrap_or(self.refocus_every_min),
            "enforceAutofocusOnTemperature" => self.refocus_temp = on,
            "maxFocusTemperatureDelta" => self.refocus_temp_delta = num.unwrap_or(self.refocus_temp_delta),
            "refocusAfterMeridianFlip" => self.refocus_flip = on,
            "enforceAutofocusHFR" => self.hfr_check = on,
            "hFRCheckAlgorithm" => {
                self.hfr_algorithm = match v.as_str() {
                    Some("Fixed") => 1,
                    Some("Median Measure") => 2,
                    Some(_) => 0,
                    None => num.map(|n| n as u8).unwrap_or(0).min(2),
                }
            }
            "hFRThresholdPercentage" => self.hfr_threshold = num.unwrap_or(self.hfr_threshold),
            "inSequenceCheckFrames" => self.hfr_frames = num.map(|n| n.max(1.0) as u32).unwrap_or(self.hfr_frames),
            "hFRDeviation" => self.hfr_deviation = num.unwrap_or(self.hfr_deviation),
            "enforceGuideDeviation" => self.guide_abort = on,
            "guideDeviation" => self.guide_abort_arcsec = num.unwrap_or(self.guide_abort_arcsec),
            "enforceStartGuiderDrift" => self.guide_start = on,
            "startGuideDeviation" => self.guide_start_arcsec = num.unwrap_or(self.guide_start_arcsec),
            _ => return false,
        }
        true
    }
}

/// What a sequence file amounts to, for the Scheduler's estimates.
#[derive(Clone, Debug, PartialEq)]
pub struct SequencePlan {
    pub frames: u32,
    pub duration_secs: f64,
    pub in_sequence_focus: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Queue {
    pub jobs: Vec<Job>,
    pub limits: Limits,
    /// The file it was loaded from, when it was.
    pub path: Option<String>,
}

/// Ekos' per-frame overhead estimate (download, settle).
const FRAME_OVERHEAD_S: f64 = 2.0;

impl Queue {
    pub fn plan(&self) -> SequencePlan {
        let frames = self.jobs.iter().map(|j| j.count).sum();
        let duration_secs = self.jobs.iter().map(|j| f64::from(j.count) * (j.exposure + j.delay_s + FRAME_OVERHEAD_S)).sum();
        SequencePlan {
            frames,
            duration_secs,
            in_sequence_focus: self.limits.refocus_every || self.limits.hfr_check || self.limits.refocus_temp,
        }
    }
}

/// What the queue tells the Scheduler.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    JobStarted(usize),
    ImageReceived,
    JobComplete(usize),
    Complete,
    Aborted(String),
}

enum Phase {
    Idle,
    /// `FOCUSING` written for a limit; waiting for its edge.
    Refocus,
    /// `AGENT_WHEEL_FILTER` written; waiting for Ok with the item on.
    PrepareFilter { since: Instant },
    /// Camera mirrors written; waiting for them to reflect the job.
    PrepareCamera { since: Instant },
    /// `AGENT_IMAGER_BATCH` + `EXPOSURE` written; waiting for Busy.
    AgentStart { since: Instant, retried: bool },
    Running {
        /// `completed` when the batch started (a resumed job).
        base: u32,
        /// Saved frames seen through `CCD_IMAGE_FILE`.
        files: u32,
        /// `AGENT_IMAGER_STATS.FRAME − 1`, the floor when no file lands.
        frame_floor: u32,
        last_file: Option<String>,
    },
    Script { done: oneshot::Receiver<Option<i32>>, since: Instant },
    Complete,
    Aborted,
}

pub struct Capture {
    pub queue: Queue,
    phase: Phase,
    current: Option<usize>,
    process: Process,
    log: Log,
    log_dirty: bool,
    last_status: String,
    /// The last `expv` seen, for the overall progress.
    expv: f64,
    paused: bool,
    captures_dir: String,
    /// When the last successful autofocus ended (any `FOCUSING` on the agent).
    last_focus_at: Option<Instant>,
    temp_at_focus: Option<f64>,
    /// The agent's `FRAMES_TO_SKIP_BEFORE_DITHER` before a row changed it.
    saved_dither: Option<f64>,
    events: Vec<CaptureEvent>,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            queue: Queue::default(),
            phase: Phase::Idle,
            current: None,
            process: Process::default(),
            log: Log::default(),
            log_dirty: false,
            last_status: "Idle".into(),
            expv: 0.0,
            paused: false,
            captures_dir: String::new(),
            last_focus_at: None,
            temp_at_focus: None,
            saved_dither: None,
            events: Vec::new(),
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

fn text(device: &str, name: &str, items: &[(&str, &str)]) -> String {
    let items: Vec<(String, Value)> = items.iter().map(|(n, v)| (n.to_string(), json!(v))).collect();
    new_vector_msg(Kind::Text, device, name, &items)
}

fn hms(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// KStars' `generateScriptInput` for a post-job script.
pub fn script_input(queue: &Queue, idx: usize) -> Value {
    let job = &queue.jobs[idx];
    json!({
        "script_type": "POST_JOB",
        "job": {
            "filter": job.filter.clone().unwrap_or_default(),
            "exposure": job.exposure,
            "type": job.frame_type.label(),
            "binning_x": job.bin.0,
            "binning_y": job.bin.1,
            "target_name": job.target,
        },
        "progress": {
            "completed": queue.jobs.iter().filter(|j| j.status == JobStatus::Complete).count(),
            "total": queue.jobs.len(),
        },
    })
}

impl Capture {
    // ── Queue edits ──────────────────────────────────────────────────────

    pub fn is_running(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::Complete | Phase::Aborted)
    }

    pub fn is_complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }

    fn refuse_if_running(&self) -> Result<(), String> {
        if self.is_running() {
            Err("the capture queue is running".into())
        } else {
            Ok(())
        }
    }

    /// `capture_load_sequence_file {filedata}`: replace the queue. `target`
    /// overrides every row's `<TargetName>` (the Scheduler's job name).
    pub fn load_xml(&mut self, xml: &str, target: Option<&str>) -> Result<(), String> {
        self.refuse_if_running()?;
        let mut q = esq::parse(xml)?;
        if let Some(t) = target {
            for j in &mut q.jobs {
                j.target = t.to_string();
            }
        }
        self.queue = q;
        self.phase = Phase::Idle;
        self.current = None;
        self.last_status = "Idle".into();
        Ok(())
    }

    pub fn load_file(&mut self, path: &str, target: Option<&str>) -> Result<(), String> {
        let xml = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        self.load_xml(&xml, target)?;
        self.queue.path = Some(path.to_string());
        Ok(())
    }

    pub fn to_esq(&self) -> String {
        esq::write(&self.queue)
    }

    /// `capture_save_sequence_file {filepath}`: write and return the text.
    pub fn save_file(&self, path: &str) -> Result<String, String> {
        if let Some(parent) = std::path::Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let xml = self.to_esq();
        std::fs::write(path, &xml).map_err(|e| format!("cannot write {path}: {e}"))?;
        Ok(xml)
    }

    pub fn add(&mut self, job: Job) -> Result<(), String> {
        self.refuse_if_running()?;
        self.queue.jobs.push(job);
        Ok(())
    }

    /// `capture_remove_sequence {index}` — KStars refuses while capturing.
    pub fn remove(&mut self, index: usize) -> Result<(), String> {
        self.refuse_if_running()?;
        if index >= self.queue.jobs.len() {
            return Err(format!("no job #{}", index + 1));
        }
        self.queue.jobs.remove(index);
        Ok(())
    }

    /// `capture_clear_sequences`: stops a running queue first.
    pub fn clear(&mut self, agent: &str) -> Reaction {
        let mut r = Reaction::default();
        if self.is_running() {
            r = self.stop(agent);
        }
        self.queue = Queue::default();
        self.current = None;
        self.phase = Phase::Idle;
        self.push_sequences(&mut r);
        r.to_browser.push(self.state_msg(Some("Idle")));
        r
    }

    /// Every job back to the start (a Scheduler repeat, or `capture_start`
    /// on a finished queue).
    pub fn reset_progress(&mut self) {
        for j in &mut self.queue.jobs {
            j.completed = 0;
            j.status = JobStatus::Idle;
        }
        if !self.is_running() {
            self.phase = Phase::Idle;
            self.current = None;
        }
    }

    pub fn drain_events(&mut self) -> Vec<CaptureEvent> {
        std::mem::take(&mut self.events)
    }

    // ── Browser payloads ─────────────────────────────────────────────────

    pub fn sequences_msg(&self) -> String {
        reply_msg("capture_get_sequences", Value::Array(self.queue.jobs.iter().map(Job::to_json).collect()))
    }

    fn push_sequences(&self, r: &mut Reaction) {
        r.to_browser.push(self.sequences_msg());
    }

    pub fn limits_payload(&self) -> Value {
        self.queue.limits.payload()
    }

    pub fn apply_setting(&mut self, key: &str, value: &Value) -> bool {
        self.queue.limits.apply(key, value)
    }

    fn note(&mut self, line: impl AsRef<str>) {
        if self.log.push(line.as_ref()) {
            self.log_dirty = true;
        }
    }

    /// `new_capture_state`: the status (only when it changed), the running
    /// job's counts, the exposure, the overall progress and the log (only
    /// when it changed — the agent's stats tick every 200 ms).
    fn state_msg(&mut self, status: Option<&str>) -> String {
        let mut p = json!({});
        if let Some(s) = status {
            if s != self.last_status {
                self.last_status = s.to_string();
                p["status"] = json!(s);
            }
        }
        let (seqv, seqr, expr, delay) = self
            .current
            .and_then(|i| self.queue.jobs.get(i))
            .map(|j| (j.completed, j.count, j.exposure, j.delay_s))
            .unwrap_or((0, 0, 0.0, 0.0));
        let capturing = matches!(self.phase, Phase::Running { .. });
        let expv = if capturing { self.expv } else { 0.0 };
        let elapsed = if capturing { (expr - expv).max(0.0) } else { 0.0 };
        let total: f64 = self.queue.jobs.iter().map(|j| f64::from(j.count) * j.exposure).sum();
        let done: f64 = self.queue.jobs.iter().map(|j| f64::from(j.completed.min(j.count)) * j.exposure).sum::<f64>() + elapsed;
        let remaining: f64 = self
            .queue
            .jobs
            .iter()
            .map(|j| f64::from(j.remaining()) * (j.exposure + j.delay_s))
            .sum::<f64>()
            - elapsed;
        let seqt = f64::from(seqr.saturating_sub(seqv)) * (expr + delay) - elapsed;
        p["seqv"] = json!(seqv);
        p["seqr"] = json!(seqr);
        p["expv"] = json!(expv);
        p["expr"] = json!(expr);
        p["ovp"] = json!(if total > 0.0 { (100.0 * done / total).clamp(0.0, 100.0).round() as i64 } else { 0 });
        p["ovt"] = json!(hms(remaining));
        p["seqt"] = json!(hms(seqt));
        if self.log_dirty {
            self.log_dirty = false;
            p["log"] = json!(self.log.text());
        }
        reply_msg("new_capture_state", p)
    }

    // ── Running ──────────────────────────────────────────────────────────

    /// `capture_start`: run the next pending job (resuming a partial one
    /// with its remaining count). `Err` explains a refusal.
    pub fn start(&mut self, store: &PropertyStore, agent: &str, captures_dir: &str, now: Instant) -> Result<Reaction, String> {
        if self.is_running() {
            return Err("the capture queue is already running".into());
        }
        if self.queue.jobs.is_empty() {
            return Err("the capture queue is empty".into());
        }
        if AgentSet::busy(store, agent) {
            return Err("the Imager Agent is busy".into());
        }
        if AgentSet::selected(store, agent, "FILTER_CCD_LIST").is_none() {
            return Err("the Imager Agent has no camera selected".into());
        }
        if self.queue.jobs.iter().all(|j| j.status == JobStatus::Complete) {
            self.reset_progress();
        }
        for j in &mut self.queue.jobs {
            if j.count == 0 {
                j.status = JobStatus::Complete;
            }
        }
        let Some(next) = self.queue.jobs.iter().position(|j| j.status != JobStatus::Complete && j.completed < j.count) else {
            return Err("every job is complete".into());
        };
        self.captures_dir = captures_dir.to_string();
        self.last_focus_at.get_or_insert(now);
        if self.saved_dither.is_none() {
            self.saved_dither = store.get(agent, "AGENT_IMAGER_BATCH").and_then(|b| b.number("FRAMES_TO_SKIP_BEFORE_DITHER"));
        }
        let mut r = Reaction::default();
        self.enter_job(next, store, agent, now, &mut r);
        Ok(r)
    }

    fn enter_job(&mut self, idx: usize, store: &PropertyStore, agent: &str, now: Instant, r: &mut Reaction) {
        self.current = Some(idx);
        self.queue.jobs[idx].status = JobStatus::InProgress;
        self.expv = 0.0;
        self.paused = false;
        self.note(format!("Job {} started: {}", idx + 1, self.describe(idx)));
        self.events.push(CaptureEvent::JobStarted(idx));
        self.push_sequences(r);
        r.to_browser.push(self.state_msg(Some("In Progress")));
        self.begin_job(store, agent, now, r);
    }

    fn describe(&self, idx: usize) -> String {
        let j = &self.queue.jobs[idx];
        let mut s = format!("{} × {} s {}", j.remaining(), j.exposure, j.frame_type.label());
        if let Some(f) = &j.filter {
            s.push_str(&format!(" ({f})"));
        }
        s
    }

    /// Step 1 — refocus if a limit says so.
    fn begin_job(&mut self, store: &PropertyStore, agent: &str, now: Instant, r: &mut Reaction) {
        let l = &self.queue.limits;
        let mut why = None;
        if l.refocus_every {
            let since = self.last_focus_at.map(|t| now.duration_since(t)).unwrap_or(Duration::ZERO);
            if since >= Duration::from_secs(u64::from(l.refocus_every_min) * 60) {
                why = Some(format!("{} min since the last autofocus", since.as_secs() / 60));
            }
        }
        if why.is_none() && l.refocus_temp {
            let temp = store.get(agent, "FOCUSER_TEMPERATURE").and_then(|p| p.number("TEMPERATURE"));
            match (temp, self.temp_at_focus) {
                (Some(t), Some(t0)) if (t - t0).abs() >= l.refocus_temp_delta => {
                    why = Some(format!("temperature moved {:.1} °C", t - t0));
                }
                (Some(t), None) => self.temp_at_focus = Some(t),
                _ => {}
            }
        }
        let focusing_possible = store.get(agent, "AGENT_START_PROCESS").is_some_and(|p| p.item("FOCUSING").is_some())
            && AgentSet::selected(store, agent, "FILTER_FOCUSER_LIST").is_some();
        match why {
            Some(why) if focusing_possible => {
                self.note(format!("Refocusing before job {} ({why})", self.current.unwrap_or(0) + 1));
                if let Some(w) = preview_on(store, agent) {
                    r.to_indigo.push(w);
                }
                r.to_indigo.push(switch(agent, "AGENT_START_PROCESS", "FOCUSING"));
                self.phase = Phase::Refocus;
                r.to_browser.push(self.state_msg(Some("Focusing")));
            }
            Some(why) => {
                self.note(format!("Refocus skipped ({why}): no focuser on the Imager Agent"));
                self.after_refocus(store, agent, now, r);
            }
            None => self.after_refocus(store, agent, now, r),
        }
    }

    /// Step 2 — the filter.
    fn after_refocus(&mut self, store: &PropertyStore, agent: &str, now: Instant, r: &mut Reaction) {
        let Some(idx) = self.current else { return };
        let wanted = self.queue.jobs[idx].filter.clone().filter(|f| !f.is_empty());
        if let Some(name) = wanted {
            match store.get(agent, "AGENT_WHEEL_FILTER") {
                Some(p) if !p.items.is_empty() => {
                    match p.items.iter().find(|i| i.label == name || i.name == name) {
                        Some(item) if item.on() && p.state == State::Ok => {}
                        Some(item) => {
                            self.note(format!("Changing filter to {name}"));
                            r.to_indigo.push(switch(agent, "AGENT_WHEEL_FILTER", &item.name));
                            self.phase = Phase::PrepareFilter { since: now };
                            r.to_browser.push(self.state_msg(Some("Changing Filter")));
                            return;
                        }
                        None => self.note(format!("Filter '{name}' is not on the wheel, keeping the current one")),
                    }
                }
                _ => self.note(format!("No filter wheel on the Imager Agent, filter '{name}' ignored")),
            }
        }
        self.after_filter(store, agent, now, r);
    }

    /// Step 3 — the camera mirrors.
    fn after_filter(&mut self, store: &PropertyStore, agent: &str, now: Instant, r: &mut Reaction) {
        let Some(idx) = self.current else { return };
        let (dir, _) = self.local_mode(idx);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.abort_with(format!("Cannot create {dir}: {e}"), agent, r);
            return;
        }
        let writes = self.camera_writes(store, agent, idx);
        if writes.is_empty() {
            self.start_batch(store, agent, now, r);
        } else {
            r.to_indigo.extend(writes);
            self.phase = Phase::PrepareCamera { since: now };
            r.to_browser.push(self.state_msg(Some("In Progress")));
        }
    }

    fn local_mode(&self, idx: usize) -> (String, String) {
        let now = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
        paths::local_mode(&self.queue.jobs[idx], &self.captures_dir, &now)
    }

    /// The writes that bring the agent's camera mirrors to the job; empty
    /// when they already reflect it (so this doubles as the readiness check).
    fn camera_writes(&self, store: &PropertyStore, agent: &str, idx: usize) -> Vec<String> {
        let job = &self.queue.jobs[idx];
        let mut out = Vec::new();
        let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
        if let Some(p) = store.get(agent, "CCD_FRAME_TYPE") {
            let mut item = job.frame_type.item();
            if p.item(item).is_none() {
                item = if job.frame_type == FrameType::DarkFlat { "FLAT" } else { "LIGHT" };
            }
            if !p.on(item) {
                out.push(switch(agent, "CCD_FRAME_TYPE", item));
            }
        }
        if let Some(p) = store.get(agent, "CCD_BIN") {
            let (bx, by) = (f64::from(job.bin.0), f64::from(job.bin.1));
            if !(p.number("HORIZONTAL").is_some_and(|v| near(v, bx)) && p.number("VERTICAL").is_some_and(|v| near(v, by))) {
                out.push(number(agent, "CCD_BIN", &[("HORIZONTAL", bx), ("VERTICAL", by)]));
            }
        }
        if let (Some(g), Some(p)) = (job.gain, store.get(agent, "CCD_GAIN")) {
            if !p.number("GAIN").is_some_and(|v| near(v, g)) {
                out.push(number(agent, "CCD_GAIN", &[("GAIN", g)]));
            }
        }
        if let (Some(o), Some(p)) = (job.offset, store.get(agent, "CCD_OFFSET")) {
            if !p.number("OFFSET").is_some_and(|v| near(v, o)) {
                out.push(number(agent, "CCD_OFFSET", &[("OFFSET", o)]));
            }
        }
        if let Some(p) = store.get(agent, "CCD_IMAGE_FORMAT") {
            let item = match job.encoding.to_ascii_lowercase().as_str() {
                "xisf" => "XISF",
                "native" => "RAW",
                _ => "FITS",
            };
            if p.item(item).is_some() && !p.on(item) {
                out.push(switch(agent, "CCD_IMAGE_FORMAT", item));
            }
        }
        if let Some(p) = store.get(agent, "CCD_UPLOAD_MODE") {
            let item = if job.upload_mode == 2 { "BOTH" } else { "LOCAL" };
            if p.item(item).is_some() && !p.on(item) {
                out.push(switch(agent, "CCD_UPLOAD_MODE", item));
            }
        }
        if let Some(p) = store.get(agent, "CCD_LOCAL_MODE") {
            let (dir, prefix) = self.local_mode(idx);
            let object = paths::sanitize(&job.target);
            let mut items: Vec<(&str, &str)> = Vec::new();
            if p.text("DIR").map(|d| d.trim_end_matches('/')) != Some(dir.as_str()) {
                items.push(("DIR", &dir));
            }
            if p.text("PREFIX") != Some(prefix.as_str()) {
                items.push(("PREFIX", &prefix));
            }
            if p.item("OBJECT").is_some() && p.text("OBJECT") != Some(object.as_str()) {
                items.push(("OBJECT", &object));
            }
            if !items.is_empty() {
                out.push(text(agent, "CCD_LOCAL_MODE", &items));
            }
        }
        if let Some(w) = preview_on(store, agent) {
            out.push(w);
        }
        out
    }

    /// Step 4 — the batch.
    fn start_batch(&mut self, store: &PropertyStore, agent: &str, now: Instant, r: &mut Reaction) {
        let Some(idx) = self.current else { return };
        let job = &self.queue.jobs[idx];
        let mut items = vec![
            ("COUNT", f64::from(job.remaining())),
            ("EXPOSURE", job.exposure),
            ("DELAY", job.delay_s),
        ];
        let has_dither = store.get(agent, "AGENT_IMAGER_BATCH").is_some_and(|b| b.item("FRAMES_TO_SKIP_BEFORE_DITHER").is_some());
        if has_dither {
            let skip = match (job.frame_type, job.dither) {
                (FrameType::Light, 0) => None,
                (FrameType::Light, n) if n > 0 => Some(f64::from(n - 1)),
                _ => Some(-1.0),
            };
            if let Some(s) = skip {
                items.push(("FRAMES_TO_SKIP_BEFORE_DITHER", s));
            }
        }
        r.to_indigo.push(number(agent, "AGENT_IMAGER_BATCH", &items));
        r.to_indigo.push(switch(agent, "AGENT_START_PROCESS", "EXPOSURE"));
        self.phase = Phase::AgentStart { since: now, retried: false };
    }

    fn restore_dither(&mut self, agent: &str, r: &mut Reaction) {
        if let Some(d) = self.saved_dither.take() {
            r.to_indigo.push(number(agent, "AGENT_IMAGER_BATCH", &[("FRAMES_TO_SKIP_BEFORE_DITHER", d)]));
        }
    }

    fn abort_with(&mut self, why: String, agent: &str, r: &mut Reaction) {
        if let Some(idx) = self.current {
            self.queue.jobs[idx].status = JobStatus::Aborted;
        }
        self.note(&why);
        self.phase = Phase::Aborted;
        self.restore_dither(agent, r);
        self.events.push(CaptureEvent::Aborted(why));
        self.push_sequences(r);
        r.to_browser.push(self.state_msg(Some("Aborted")));
    }

    fn job_done(&mut self, store: &PropertyStore, agent: &str, now: Instant, r: &mut Reaction) {
        let Some(idx) = self.current else { return };
        {
            let job = &mut self.queue.jobs[idx];
            job.completed = job.count;
            job.status = JobStatus::Complete;
        }
        self.note(format!("Job {} complete", idx + 1));
        self.events.push(CaptureEvent::JobComplete(idx));
        self.push_sequences(r);
        let script = self.queue.jobs[idx].post_job_script.clone();
        if !script.is_empty() {
            match spawn_script(&script, script_input(&self.queue, idx)) {
                Ok(done) => {
                    self.note(format!("Executing capture script {script}"));
                    self.phase = Phase::Script { done, since: now };
                    r.to_browser.push(self.state_msg(Some("In Progress")));
                    return;
                }
                Err(e) => self.note(format!("Capture script {script} not started: {e}")),
            }
        }
        self.next_job(store, agent, now, r);
    }

    fn next_job(&mut self, store: &PropertyStore, agent: &str, now: Instant, r: &mut Reaction) {
        let next = self.queue.jobs.iter().position(|j| j.status != JobStatus::Complete && j.completed < j.count);
        match next {
            Some(idx) => self.enter_job(idx, store, agent, now, r),
            None => {
                for j in &mut self.queue.jobs {
                    if j.count == 0 {
                        j.status = JobStatus::Complete;
                    }
                }
                self.phase = Phase::Complete;
                self.note("Sequence complete");
                self.restore_dither(agent, r);
                self.events.push(CaptureEvent::Complete);
                self.push_sequences(r);
                r.to_browser.push(self.state_msg(Some("Complete")));
            }
        }
    }

    /// `capture_stop`.
    pub fn stop(&mut self, agent: &str) -> Reaction {
        let mut r = Reaction::default();
        match self.phase {
            Phase::Idle | Phase::Complete | Phase::Aborted => return r,
            Phase::Running { .. } | Phase::Refocus => {
                // The Finished edge does the bookkeeping.
                r.to_indigo.push(switch(agent, "AGENT_ABORT_PROCESS", "ABORT"));
                self.note("Capture aborted");
                return r;
            }
            Phase::AgentStart { .. } => {
                r.to_indigo.push(switch(agent, "AGENT_ABORT_PROCESS", "ABORT"));
            }
            Phase::PrepareFilter { .. } | Phase::PrepareCamera { .. } | Phase::Script { .. } => {}
        }
        self.abort_with("Capture aborted".into(), agent, &mut r);
        r
    }

    // ── Reactions ────────────────────────────────────────────────────────

    pub fn on_change(&mut self, change: &Change, store: &PropertyStore, agent: &str, now: Instant) -> Reaction {
        let mut r = Reaction::default();
        let name = match change {
            Change::Defined { device, name } | Change::Updated { device, name } if device == agent => name.as_str(),
            Change::Message { device: Some(d), text } if d == agent => {
                if self.is_running() {
                    self.note(text);
                    if self.log_dirty {
                        r.to_browser.push(self.state_msg(None));
                    }
                }
                return r;
            }
            Change::Deleted { device, name: None } if device == agent => {
                if self.is_running() {
                    self.process = Process::default();
                    self.abort_with("Imager Agent went away".into(), agent, &mut r);
                }
                return r;
            }
            _ => return r,
        };
        let Some(p) = store.get(agent, name) else { return r };
        if self.is_running() {
            if let Some(m) = p.message.as_deref() {
                self.note(m);
            }
        }
        match name {
            "AGENT_START_PROCESS" => {
                let edge = self.process.observe(p);
                // Any autofocus that ends well resets the refocus limits.
                if let Edge::Finished { ok: true, item } = &edge {
                    if item == "FOCUSING" {
                        self.last_focus_at = Some(now);
                        self.temp_at_focus = store.get(agent, "FOCUSER_TEMPERATURE").and_then(|p| p.number("TEMPERATURE"));
                    }
                }
                match (&self.phase, edge) {
                    (Phase::Refocus, Edge::Finished { ok: true, item }) if item == "FOCUSING" => {
                        self.note("Autofocus complete");
                        self.after_refocus(store, agent, now, &mut r);
                    }
                    (Phase::Refocus, Edge::Finished { ok: false, item }) if item == "FOCUSING" => {
                        self.abort_with("Autofocus failed, aborting the sequence".into(), agent, &mut r);
                    }
                    (Phase::AgentStart { .. }, Edge::Started(item)) if item == "EXPOSURE" => {
                        let base = self.current.map(|i| self.queue.jobs[i].completed).unwrap_or(0);
                        self.phase = Phase::Running { base, files: 0, frame_floor: 0, last_file: None };
                        r.to_browser.push(self.state_msg(Some("Capturing")));
                    }
                    (Phase::AgentStart { .. }, Edge::Started(item)) => {
                        self.abort_with(format!("The Imager Agent is running {item}"), agent, &mut r);
                    }
                    (Phase::Running { .. }, Edge::Finished { ok, item }) if item == "EXPOSURE" => {
                        if ok {
                            self.job_done(store, agent, now, &mut r);
                        } else {
                            let why = p.message.clone().unwrap_or_else(|| "Batch failed".into());
                            self.abort_with(why, agent, &mut r);
                        }
                    }
                    _ => {}
                }
            }
            "AGENT_IMAGER_STATS" => {
                if let Phase::Running { base, files, frame_floor, .. } = &mut self.phase {
                    let frame = p.number("FRAME").unwrap_or(0.0).max(0.0) as u32;
                    *frame_floor = frame.saturating_sub(1);
                    let progress = (*files).max(*frame_floor);
                    let base = *base;
                    if let Some(idx) = self.current {
                        let job = &mut self.queue.jobs[idx];
                        job.completed = (base + progress).min(job.count);
                    }
                    self.expv = p.number("EXPOSURE").unwrap_or(0.0).max(0.0);
                    let status = if self.paused {
                        "Paused"
                    } else {
                        match p.number("PHASE").unwrap_or(3.0) as i64 {
                            1 => "Changing Filter",
                            2 => "Focusing",
                            4 => "Dithering",
                            5 => "Waiting",
                            _ => "Capturing",
                        }
                    };
                    r.to_browser.push(self.state_msg(Some(status)));
                }
            }
            "AGENT_PAUSE_PROCESS" => {
                if matches!(self.phase, Phase::Running { .. }) {
                    self.paused = p.state == State::Busy;
                    if self.paused {
                        r.to_browser.push(self.state_msg(Some("Paused")));
                    }
                }
            }
            "AGENT_WHEEL_FILTER" => {
                if matches!(self.phase, Phase::PrepareFilter { .. }) {
                    match p.state {
                        State::Ok if p.selected().is_some() => self.after_filter(store, agent, now, &mut r),
                        State::Alert => {
                            let why = p.message.clone().unwrap_or_else(|| "Filter change failed".into());
                            self.abort_with(why, agent, &mut r);
                        }
                        _ => {}
                    }
                }
            }
            "CCD_IMAGE_FILE" => {
                if let Phase::Running { base, files, frame_floor, last_file } = &mut self.phase {
                    let file = p.text("FILE").map(str::trim).filter(|f| !f.is_empty()).map(str::to_string);
                    if p.state == State::Ok && file.is_some() && file != *last_file {
                        *last_file = file;
                        *files += 1;
                        let progress = (*files).max(*frame_floor);
                        let base = *base;
                        if let Some(idx) = self.current {
                            let job = &mut self.queue.jobs[idx];
                            job.completed = (base + progress).min(job.count);
                            let (done, total) = (job.completed, job.count);
                            self.note(format!("Received image {done}/{total}"));
                        }
                        self.events.push(CaptureEvent::ImageReceived);
                        self.push_sequences(&mut r);
                        r.to_browser.push(self.state_msg(Some("Image Received")));
                    }
                }
            }
            "CCD_PREVIEW_IMAGE" => {
                if matches!(self.phase, Phase::Running { .. }) && p.state == State::Ok {
                    if let Some(url) = p.item("IMAGE").and_then(|i| i.blob_url()) {
                        r.fetch_preview = Some(url.to_string());
                    }
                }
            }
            _ if name.starts_with("CCD_") && matches!(self.phase, Phase::PrepareCamera { .. }) => {
                if let Some(idx) = self.current {
                    if self.camera_writes(store, agent, idx).is_empty() {
                        self.start_batch(store, agent, now, &mut r);
                    }
                }
            }
            _ => {}
        }
        if self.log_dirty && self.is_running() {
            r.to_browser.push(self.state_msg(None));
        }
        r
    }

    /// Timeouts and the script's exit, once a second.
    pub fn tick(&mut self, store: &PropertyStore, agent: &str, now: Instant) -> Reaction {
        let mut r = Reaction::default();
        match &mut self.phase {
            Phase::PrepareFilter { since } => {
                if now.duration_since(*since) > FILTER_TIMEOUT {
                    self.abort_with("The filter wheel did not answer".into(), agent, &mut r);
                }
            }
            Phase::PrepareCamera { since } => {
                let waited = now.duration_since(*since);
                if let Some(idx) = self.current {
                    let pending = self.camera_writes(store, agent, idx);
                    if pending.is_empty() {
                        self.start_batch(store, agent, now, &mut r);
                    } else if waited > PREPARE_TIMEOUT {
                        self.note("The camera did not confirm every setting, starting anyway");
                        self.start_batch(store, agent, now, &mut r);
                    }
                }
            }
            Phase::AgentStart { since, retried } => {
                if now.duration_since(*since) > START_TIMEOUT {
                    if *retried {
                        self.abort_with("The Imager Agent did not start the batch".into(), agent, &mut r);
                    } else {
                        *retried = true;
                        *since = now;
                        self.note("The Imager Agent did not start, trying again");
                        r.to_indigo.push(switch(agent, "AGENT_START_PROCESS", "EXPOSURE"));
                    }
                }
            }
            Phase::Script { done, since } => {
                let outcome = match done.try_recv() {
                    Ok(code) => Some(code.map_or_else(|| "Capture script was killed".to_string(), |c| format!("Capture script finished with code {c}"))),
                    Err(oneshot::error::TryRecvError::Closed) => Some("Capture script ended without a status".to_string()),
                    Err(oneshot::error::TryRecvError::Empty) if now.duration_since(*since) > SCRIPT_TIMEOUT => {
                        Some("Capture script timed out".to_string())
                    }
                    Err(_) => None,
                };
                if let Some(line) = outcome {
                    self.note(line);
                    self.next_job(store, agent, now, &mut r);
                }
            }
            _ => {}
        }
        if self.log_dirty && self.is_running() {
            r.to_browser.push(self.state_msg(None));
        }
        r
    }
}

/// The write that makes the agent publish a JPEG of each frame, if needed.
fn preview_on(store: &PropertyStore, agent: &str) -> Option<String> {
    store
        .get(agent, "CCD_PREVIEW")
        .filter(|p| !p.on("ENABLED") && !p.on("ENABLED_WITH_HISTOGRAM"))
        .map(|_| switch(agent, "CCD_PREVIEW", "ENABLED"))
}

/// Run a post-job script with KStars' JSON on stdin; the exit code comes
/// back on the channel (`None` when killed by a signal).
fn spawn_script(path: &str, input: Value) -> Result<oneshot::Receiver<Option<i32>>, String> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| "no async runtime".to_string())?;
    let mut child = tokio::process::Command::new(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let (tx, rx) = oneshot::channel();
    handle.spawn(async move {
        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.to_string().as_bytes()).await;
            drop(stdin);
        }
        let code = match child.wait().await {
            Ok(s) => s.code(),
            Err(e) => {
                warn!("capture script: {e}");
                None
            }
        };
        let _ = tx.send(code);
    });
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    const A: &str = "Imager Agent";

    fn store() -> PropertyStore {
        let mut s = PropertyStore::default();
        for f in [
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","perm":"rw","state":"Ok","rule":"AtMostOne","items":[{"name":"PREVIEW_1","value":false},{"name":"PREVIEW","value":false},{"name":"EXPOSURE","value":false},{"name":"FOCUSING","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_ABORT_PROCESS","perm":"rw","state":"Ok","items":[{"name":"ABORT","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_PAUSE_PROCESS","perm":"rw","state":"Ok","items":[{"name":"PAUSE","value":false},{"name":"PAUSE_WAIT","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_BATCH","perm":"rw","state":"Ok","items":[{"name":"COUNT","value":1},{"name":"EXPOSURE","value":1},{"name":"DELAY","value":0},{"name":"FRAMES_TO_SKIP_BEFORE_DITHER","value":0}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","perm":"ro","state":"Ok","items":[{"name":"EXPOSURE","value":0},{"name":"DELAY","value":0},{"name":"FRAME","value":0},{"name":"FRAMES","value":0},{"name":"PHASE","value":0}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_WHEEL_FILTER","perm":"rw","state":"Ok","rule":"OneOfMany","items":[{"name":"1","label":"L","value":true},{"name":"2","label":"Ha","value":false},{"name":"3","label":"OIII","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"FILTER_CCD_LIST","perm":"rw","state":"Ok","rule":"OneOfMany","items":[{"name":"NONE","value":false},{"name":"Cam","value":true}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"FILTER_FOCUSER_LIST","perm":"rw","state":"Ok","rule":"OneOfMany","items":[{"name":"NONE","value":false},{"name":"Foc","value":true}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"CCD_FRAME_TYPE","perm":"rw","state":"Ok","items":[{"name":"LIGHT","value":true},{"name":"BIAS","value":false},{"name":"DARK","value":false},{"name":"FLAT","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"CCD_BIN","perm":"rw","state":"Ok","items":[{"name":"HORIZONTAL","value":1},{"name":"VERTICAL","value":1}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"CCD_GAIN","perm":"rw","state":"Ok","items":[{"name":"GAIN","value":100}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"CCD_UPLOAD_MODE","perm":"rw","state":"Ok","items":[{"name":"CLIENT","value":true},{"name":"LOCAL","value":false},{"name":"BOTH","value":false}]}}"#,
            r#"{"defTextVector":{"device":"Imager Agent","name":"CCD_LOCAL_MODE","perm":"rw","state":"Ok","items":[{"name":"DIR","value":"/home/x/indigo_image_cache/"},{"name":"PREFIX","value":"IMAGE_XXX"},{"name":"OBJECT","value":""}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"CCD_PREVIEW","perm":"rw","state":"Ok","items":[{"name":"DISABLED","value":true},{"name":"ENABLED","value":false}]}}"#,
            r#"{"defBLOBVector":{"device":"Imager Agent","name":"CCD_PREVIEW_IMAGE","perm":"ro","state":"Idle","items":[{"name":"IMAGE"}]}}"#,
            r#"{"defTextVector":{"device":"Imager Agent","name":"CCD_IMAGE_FILE","perm":"ro","state":"Idle","items":[{"name":"FILE","value":""}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        s
    }

    fn set(s: &mut PropertyStore, frame: &str) -> Change {
        s.apply(parse_frame(frame).unwrap()).unwrap()
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

    fn write<'a>(msgs: &'a [String], prop: &str) -> Option<Value> {
        msgs.iter().find_map(|m| {
            let v: Value = serde_json::from_str(m).ok()?;
            let (_, b) = v.as_object()?.iter().next()?;
            (b["name"] == prop).then(|| b.clone())
        })
    }

    fn item_value(write: &Value, item: &str) -> Value {
        write["items"].as_array().unwrap().iter().find(|i| i["name"] == item).map(|i| i["value"].clone()).unwrap_or(Value::Null)
    }

    fn states(r: &Reaction) -> Vec<Value> {
        r.to_browser
            .iter()
            .filter_map(|m| {
                let v: Value = serde_json::from_str(m).ok()?;
                (v["type"] == "new_capture_state").then(|| v["payload"].clone())
            })
            .collect()
    }

    fn statuses(r: &Reaction) -> Vec<String> {
        states(r).iter().filter_map(|p| p["status"].as_str().map(str::to_string)).collect()
    }

    fn sequences(r: &Reaction) -> Option<Vec<Value>> {
        r.to_browser.iter().rev().find_map(|m| {
            let v: Value = serde_json::from_str(m).ok()?;
            (v["type"] == "capture_get_sequences").then(|| v["payload"].as_array().unwrap().clone())
        })
    }

    fn dir() -> String {
        let d = std::env::temp_dir().join(format!("junos-capture-{}", std::process::id()));
        d.to_string_lossy().into_owned()
    }

    fn two_jobs() -> Capture {
        let mut c = Capture::default();
        c.load_xml(esq::tests::BROWSER_XML, None).unwrap();
        c.queue.jobs[0].fits_dir = String::new();
        c
    }

    fn running(c: &mut Capture, s: &mut PropertyStore, t0: Instant) {
        let r = c.start(s, A, &dir(), t0).unwrap();
        assert_eq!(kinds(&r.to_indigo), ["newSwitchVector:AGENT_WHEEL_FILTER:2"]);
        let ch = set(s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_WHEEL_FILTER","state":"Ok","items":[{"name":"1","value":false},{"name":"2","value":true}]}}"#);
        let r = c.on_change(&ch, s, A, t0);
        // Camera mirrors: binning 2x2, gain 120, upload LOCAL, local mode, preview.
        let k = kinds(&r.to_indigo);
        assert!(k.contains(&"newNumberVector:CCD_BIN:HORIZONTAL".to_string()), "{k:?}");
        assert!(k.contains(&"newNumberVector:CCD_GAIN:GAIN".to_string()));
        assert!(k.contains(&"newSwitchVector:CCD_UPLOAD_MODE:LOCAL".to_string()));
        assert!(k.contains(&"newSwitchVector:CCD_PREVIEW:ENABLED".to_string()));
        assert!(!k.iter().any(|k| k.contains("CCD_FRAME_TYPE")), "light already on");
        let lm = write(&r.to_indigo, "CCD_LOCAL_MODE").unwrap();
        assert_eq!(item_value(&lm, "DIR"), json!(format!("{}/M_31", dir())));
        assert_eq!(item_value(&lm, "PREFIX"), json!("%o_%C_%F_%E_secs_%-DT%-H_%4I"));
        assert_eq!(item_value(&lm, "OBJECT"), json!("M_31"));
        assert!(std::path::Path::new(&format!("{}/M_31", dir())).is_dir());
        // The mirrors reflect the job: the batch goes out.
        let dir_ = format!("{}/M_31/", dir());
        for f in [
            r#"{"setNumberVector":{"device":"Imager Agent","name":"CCD_BIN","state":"Ok","items":[{"name":"HORIZONTAL","value":2},{"name":"VERTICAL","value":2}]}}"#.to_string(),
            r#"{"setNumberVector":{"device":"Imager Agent","name":"CCD_GAIN","state":"Ok","items":[{"name":"GAIN","value":120}]}}"#.to_string(),
            r#"{"setSwitchVector":{"device":"Imager Agent","name":"CCD_UPLOAD_MODE","state":"Ok","items":[{"name":"CLIENT","value":false},{"name":"LOCAL","value":true}]}}"#.to_string(),
            r#"{"setSwitchVector":{"device":"Imager Agent","name":"CCD_PREVIEW","state":"Ok","items":[{"name":"DISABLED","value":false},{"name":"ENABLED","value":true}]}}"#.to_string(),
        ] {
            let ch = set(s, &f);
            assert!(c.on_change(&ch, s, A, t0).to_indigo.is_empty());
        }
        let ch = set(s, &format!(r#"{{"setTextVector":{{"device":"Imager Agent","name":"CCD_LOCAL_MODE","state":"Ok","items":[{{"name":"DIR","value":"{dir_}"}},{{"name":"PREFIX","value":"%o_%C_%F_%E_secs_%-DT%-H_%4I"}},{{"name":"OBJECT","value":"M_31"}}]}}}}"#));
        let r = c.on_change(&ch, s, A, t0);
        assert_eq!(kinds(&r.to_indigo), ["newNumberVector:AGENT_IMAGER_BATCH:COUNT", "newSwitchVector:AGENT_START_PROCESS:EXPOSURE"]);
        let b = write(&r.to_indigo, "AGENT_IMAGER_BATCH").unwrap();
        assert_eq!(item_value(&b, "COUNT"), json!(10.0));
        assert_eq!(item_value(&b, "EXPOSURE"), json!(120.0));
        assert_eq!(item_value(&b, "DELAY"), json!(5.0));
        assert_eq!(item_value(&b, "FRAMES_TO_SKIP_BEFORE_DITHER"), json!(2.0), "dither every 3 → skip 2");
        let ch = set(s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","message":"Batch started","items":[{"name":"EXPOSURE","value":true}]}}"#);
        let r = c.on_change(&ch, s, A, t0);
        assert_eq!(statuses(&r), ["Capturing"]);
        assert!(c.is_running());
    }

    #[test]
    fn load_start_prepare_and_run_to_completion() {
        let mut s = store();
        let mut c = two_jobs();
        let t0 = Instant::now();
        assert!(c.start(&s, A, &dir(), t0).is_ok());
        let seq = sequences(&c.clear(A)).unwrap();
        assert!(seq.is_empty());
        let mut c = two_jobs();
        running(&mut c, &mut s, t0);
        assert_eq!(c.drain_events(), [CaptureEvent::JobStarted(0)]);

        // Progress: stats, then a saved file.
        let ch = set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","state":"Busy","items":[{"name":"EXPOSURE","value":100},{"name":"FRAME","value":1},{"name":"PHASE","value":3}]}}"#);
        let r = c.on_change(&ch, &s, A, t0);
        let p = &states(&r)[0];
        assert!(p.get("status").is_none(), "unchanged status is not repeated");
        assert_eq!(p["expv"], 100.0);
        assert_eq!(p["expr"], 120.0);
        assert_eq!(p["seqv"], 0);
        assert_eq!(p["seqr"], 10);
        assert_eq!(p["ovp"], 2, "20 s of 1203 s");
        let ch = set(&mut s, r#"{"setTextVector":{"device":"Imager Agent","name":"CCD_IMAGE_FILE","state":"Ok","items":[{"name":"FILE","value":"/x/M_31/a_0001.fits"}]}}"#);
        let r = c.on_change(&ch, &s, A, t0);
        assert_eq!(statuses(&r), ["Image Received"]);
        assert_eq!(sequences(&r).unwrap()[0]["Count"], "1/10");
        assert_eq!(c.drain_events(), [CaptureEvent::ImageReceived]);
        // Same path again: nothing counted.
        let ch = set(&mut s, r#"{"setTextVector":{"device":"Imager Agent","name":"CCD_IMAGE_FILE","state":"Ok","items":[{"name":"FILE","value":"/x/M_31/a_0001.fits"}]}}"#);
        assert!(states(&c.on_change(&ch, &s, A, t0)).is_empty());
        let ch = set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","state":"Busy","items":[{"name":"PHASE","value":5},{"name":"EXPOSURE","value":0}]}}"#);
        assert_eq!(statuses(&c.on_change(&ch, &s, A, t0)), ["Waiting"]);
        let ch = set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","state":"Busy","items":[{"name":"PHASE","value":4}]}}"#);
        assert_eq!(statuses(&c.on_change(&ch, &s, A, t0)), ["Dithering"]);
        // A preview frame is the Imaging tab's.
        let ch = set(&mut s, r#"{"setBLOBVector":{"device":"Imager Agent","name":"CCD_PREVIEW_IMAGE","state":"Ok","items":[{"name":"IMAGE","value":"/blob/0x1.jpeg"}]}}"#);
        assert_eq!(c.on_change(&ch, &s, A, t0).fetch_preview.as_deref(), Some("/blob/0x1.jpeg"));

        // Batch finished: job 1 complete, job 2 (no filter, dark) goes straight to the camera.
        let ch = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Ok","message":"Batch finished","items":[{"name":"EXPOSURE","value":false}]}}"#);
        let r = c.on_change(&ch, &s, A, t0);
        let seq = sequences(&r).unwrap();
        assert_eq!(seq[0]["Status"], "Complete");
        assert_eq!(seq[0]["Count"], "10/10");
        assert_eq!(seq[1]["Status"], "In Progress");
        let k = kinds(&r.to_indigo);
        assert!(k.contains(&"newSwitchVector:CCD_FRAME_TYPE:DARK".to_string()), "{k:?}");
        assert!(!k.iter().any(|k| k.contains("AGENT_WHEEL_FILTER")));
        assert_eq!(c.drain_events(), [CaptureEvent::JobComplete(0), CaptureEvent::JobStarted(1)]);
        assert!(states(&r).iter().any(|p| p["log"].as_str().is_some_and(|l| l.starts_with("Job 2 started"))));
        // The frame type lands; binning (back to 1x1) and the dark's folder
        // are still pending, then the batch goes out with dither off (-1).
        let ch = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"CCD_FRAME_TYPE","state":"Ok","items":[{"name":"LIGHT","value":false},{"name":"DARK","value":true}]}}"#);
        assert!(c.on_change(&ch, &s, A, t0).to_indigo.is_empty());
        let ch = set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"CCD_BIN","state":"Ok","items":[{"name":"HORIZONTAL","value":1},{"name":"VERTICAL","value":1}]}}"#);
        assert!(c.on_change(&ch, &s, A, t0).to_indigo.is_empty());
        let dark_dir = format!("{}/Dark/", dir());
        let ch = set(&mut s, &format!(r#"{{"setTextVector":{{"device":"Imager Agent","name":"CCD_LOCAL_MODE","state":"Ok","items":[{{"name":"DIR","value":"{dark_dir}"}},{{"name":"PREFIX","value":"%F_%E_secs_%4I"}},{{"name":"OBJECT","value":""}}]}}}}"#));
        let r = c.on_change(&ch, &s, A, t0 + Duration::from_secs(1));
        let k = kinds(&r.to_indigo);
        assert!(k.iter().any(|k| k.starts_with("newNumberVector:AGENT_IMAGER_BATCH")), "{k:?}");
        let b = write(&r.to_indigo, "AGENT_IMAGER_BATCH").unwrap();
        assert_eq!(item_value(&b, "COUNT"), json!(2.0));
        assert_eq!(item_value(&b, "FRAMES_TO_SKIP_BEFORE_DITHER"), json!(-1.0));
        set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"EXPOSURE","value":true}]}}"#);
        c.on_change(&Change::Updated { device: A.into(), name: "AGENT_START_PROCESS".into() }, &s, A, t0);
        let ch = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Ok","items":[{"name":"EXPOSURE","value":false}]}}"#);
        let r = c.on_change(&ch, &s, A, t0);
        // No runtime in tests: the post-job script can't spawn, the queue completes.
        assert_eq!(statuses(&r), ["Complete"]);
        assert!(c.is_complete() && !c.is_running());
        assert!(c.queue.jobs.iter().all(|j| j.completed == j.count));
        assert_eq!(states(&r).last().unwrap()["ovp"], 100);
        // The agent's dither setting is restored at the end.
        assert_eq!(item_value(&write(&r.to_indigo, "AGENT_IMAGER_BATCH").unwrap(), "FRAMES_TO_SKIP_BEFORE_DITHER"), json!(0.0));
        assert_eq!(c.drain_events().last(), Some(&CaptureEvent::Complete));
        // Starting again restarts from scratch.
        let r = c.start(&s, A, &dir(), t0).unwrap();
        assert_eq!(c.queue.jobs[0].completed, 0);
        assert_eq!(sequences(&r).unwrap()[0]["Status"], "In Progress");
    }

    #[test]
    fn abort_reconciles_and_resumes_with_the_remaining_count() {
        let mut s = store();
        let mut c = two_jobs();
        let t0 = Instant::now();
        running(&mut c, &mut s, t0);
        for i in 1..=3 {
            set(&mut s, &format!(r#"{{"setTextVector":{{"device":"Imager Agent","name":"CCD_IMAGE_FILE","state":"Ok","items":[{{"name":"FILE","value":"/x/{i}.fits"}}]}}}}"#));
            c.on_change(&Change::Updated { device: A.into(), name: "CCD_IMAGE_FILE".into() }, &s, A, t0);
        }
        // FRAME says 4 started: the floor (3) agrees with the files.
        set(&mut s, r#"{"setNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_STATS","state":"Busy","items":[{"name":"FRAME","value":4}]}}"#);
        c.on_change(&Change::Updated { device: A.into(), name: "AGENT_IMAGER_STATS".into() }, &s, A, t0);
        let r = c.stop(A);
        assert_eq!(kinds(&r.to_indigo), ["newSwitchVector:AGENT_ABORT_PROCESS:ABORT"]);
        assert!(c.is_running(), "until the agent confirms");
        let ch = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Alert","message":"Batch aborted","items":[{"name":"EXPOSURE","value":false}]}}"#);
        let r = c.on_change(&ch, &s, A, t0);
        assert_eq!(statuses(&r), ["Aborted"]);
        assert!(!c.is_running());
        let seq = sequences(&r).unwrap();
        assert_eq!(seq[0]["Status"], "Aborted");
        assert_eq!(seq[0]["Count"], "3/10");
        assert!(matches!(c.drain_events().last(), Some(CaptureEvent::Aborted(m)) if m == "Batch aborted"));
        // A second stop is a no-op; preview/loop would be allowed again.
        assert!(c.stop(A).to_indigo.is_empty());
        // Resume: the wheel already holds Ha and the mirrors are set → straight to the batch with COUNT 7.
        let r = c.start(&s, A, &dir(), t0).unwrap();
        let b = write(&r.to_indigo, "AGENT_IMAGER_BATCH").unwrap();
        assert_eq!(item_value(&b, "COUNT"), json!(7.0));
        assert_eq!(states(&r)[0]["seqv"], 3);
    }

    #[test]
    fn prepare_timeouts_and_filter_failure() {
        let mut s = store();
        let mut c = two_jobs();
        let t0 = Instant::now();
        c.start(&s, A, &dir(), t0).unwrap();
        assert!(c.tick(&s, A, t0 + Duration::from_secs(30)).to_browser.is_empty());
        let r = c.tick(&s, A, t0 + Duration::from_secs(61));
        assert_eq!(statuses(&r), ["Aborted"]);
        assert!(c.queue.limits.refocus_every && c.queue.jobs[0].status == JobStatus::Aborted);

        // Alert from the wheel.
        let mut c = two_jobs();
        c.start(&s, A, &dir(), t0).unwrap();
        let ch = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_WHEEL_FILTER","state":"Alert","message":"Failed to set filter offset","items":[{"name":"1","value":true}]}}"#);
        let r = c.on_change(&ch, &s, A, t0);
        assert_eq!(statuses(&r), ["Aborted"]);
        assert!(states(&r).last().unwrap()["log"].as_str().unwrap().contains("Failed to set filter offset"));

        // Camera silent: after 10 s the batch goes out anyway; the agent silent: retry then abort.
        let mut s = store();
        set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_WHEEL_FILTER","state":"Ok","items":[{"name":"1","value":false},{"name":"2","value":true}]}}"#);
        let mut c = two_jobs();
        c.queue.limits.refocus_every = false;
        let r = c.start(&s, A, &dir(), t0).unwrap();
        assert!(kinds(&r.to_indigo).iter().any(|k| k.contains("CCD_BIN")));
        assert!(c.tick(&s, A, t0 + Duration::from_secs(5)).to_indigo.is_empty());
        let r = c.tick(&s, A, t0 + Duration::from_secs(11));
        assert!(kinds(&r.to_indigo).iter().any(|k| k.contains("AGENT_START_PROCESS")), "{:?}", kinds(&r.to_indigo));
        let t1 = t0 + Duration::from_secs(11);
        assert!(c.tick(&s, A, t1 + Duration::from_secs(5)).to_indigo.is_empty());
        assert_eq!(kinds(&c.tick(&s, A, t1 + Duration::from_secs(11)).to_indigo), ["newSwitchVector:AGENT_START_PROCESS:EXPOSURE"]);
        let r = c.tick(&s, A, t1 + Duration::from_secs(23));
        assert_eq!(statuses(&r), ["Aborted"]);
    }

    #[test]
    fn refocus_every_n_runs_focusing_first() {
        let mut s = store();
        let mut c = two_jobs();
        assert!(c.queue.limits.refocus_every && c.queue.limits.refocus_every_min == 45);
        let t0 = Instant::now();
        // The timer starts at the first start: no refocus yet.
        c.start(&s, A, &dir(), t0).unwrap();
        c.stop(A);
        let mut c = two_jobs();
        c.last_focus_at = Some(t0);
        let r = c.start(&s, A, &dir(), t0 + Duration::from_secs(46 * 60)).unwrap();
        assert_eq!(kinds(&r.to_indigo), ["newSwitchVector:CCD_PREVIEW:ENABLED", "newSwitchVector:AGENT_START_PROCESS:FOCUSING"]);
        assert_eq!(statuses(&r), ["In Progress", "Focusing"]);
        set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"FOCUSING","value":true}]}}"#);
        c.on_change(&Change::Updated { device: A.into(), name: "AGENT_START_PROCESS".into() }, &s, A, t0);
        let ch = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Ok","message":"Focusing finished","items":[{"name":"FOCUSING","value":false}]}}"#);
        let r = c.on_change(&ch, &s, A, t0 + Duration::from_secs(47 * 60));
        assert_eq!(kinds(&r.to_indigo), ["newSwitchVector:AGENT_WHEEL_FILTER:2"]);
        assert_eq!(statuses(&r), ["Changing Filter"]);
        assert!(c.last_focus_at.unwrap() > t0 + Duration::from_secs(46 * 60));

        // A failed autofocus aborts the sequence.
        let mut c = two_jobs();
        c.last_focus_at = Some(t0);
        c.start(&s, A, &dir(), t0 + Duration::from_secs(50 * 60)).unwrap();
        set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"FOCUSING","value":true}]}}"#);
        c.on_change(&Change::Updated { device: A.into(), name: "AGENT_START_PROCESS".into() }, &s, A, t0);
        let ch = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Alert","message":"Focusing failed","items":[{"name":"FOCUSING","value":false}]}}"#);
        assert_eq!(statuses(&c.on_change(&ch, &s, A, t0)), ["Aborted"]);
    }

    #[test]
    fn foreign_runs_and_edits_while_running() {
        let mut s = store();
        let mut c = Capture::default();
        let t0 = Instant::now();
        // Nothing loaded: start refused; a batch someone else runs is ignored.
        assert!(c.start(&s, A, &dir(), t0).unwrap_err().contains("empty"));
        let ch = set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"EXPOSURE","value":true}]}}"#);
        let r = c.on_change(&ch, &s, A, t0);
        assert!(r.to_browser.is_empty() && r.fetch_preview.is_none());
        c.load_xml(esq::tests::BROWSER_XML, Some("NGC 7000")).unwrap();
        assert_eq!(c.queue.jobs[1].target, "NGC 7000");
        assert!(c.start(&s, A, &dir(), t0).unwrap_err().contains("busy"));
        set(&mut s, r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Ok","items":[{"name":"EXPOSURE","value":false}]}}"#);
        c.on_change(&Change::Updated { device: A.into(), name: "AGENT_START_PROCESS".into() }, &s, A, t0);
        c.start(&s, A, &dir(), t0).unwrap();
        assert!(c.load_xml(esq::tests::BROWSER_XML, None).is_err());
        assert!(c.remove(0).is_err());
        assert!(c.add(Job::new(FrameType::Bias, 0.1, 5)).is_err());
        let r = c.clear(A);
        assert!(!c.is_running() && c.queue.jobs.is_empty());
        assert_eq!(statuses(&r), ["Aborted", "Idle"]);
        assert!(c.remove(0).is_err());
        c.add(Job::new(FrameType::Bias, 0.1, 5)).unwrap();
        c.remove(0).unwrap();
        // Limits round trip through the settings keys.
        assert!(c.apply_setting("enforceRefocusEveryN", &json!(true)));
        assert!(c.apply_setting("hFRCheckAlgorithm", &json!("Fixed")));
        assert!(!c.apply_setting("captureExposureN", &json!(2.0)));
        let p = c.limits_payload();
        assert_eq!(p["enforceRefocusEveryN"], true);
        assert_eq!(p["hFRCheckAlgorithm"], "Fixed");
        assert_eq!(p["refocusEveryN"], 60);
    }

    #[test]
    fn plan_script_input_and_file_round_trip() {
        let mut c = two_jobs();
        let plan = c.queue.plan();
        assert_eq!(plan.frames, 12);
        assert!((plan.duration_secs - (10.0 * 127.0 + 2.0 * 3.5)).abs() < 1e-9);
        assert!(plan.in_sequence_focus);
        c.queue.jobs[0].status = JobStatus::Complete;
        let v = script_input(&c.queue, 0);
        assert_eq!(v["script_type"], "POST_JOB");
        assert_eq!(v["job"]["filter"], "Ha");
        assert_eq!(v["job"]["binning_x"], 2);
        assert_eq!(v["progress"]["completed"], 1);
        assert_eq!(v["progress"]["total"], 2);
        let path = format!("{}/seq/test.esq", dir());
        let text = c.save_file(&path).unwrap();
        assert!(text.contains("<SequenceQueue"));
        let mut d = Capture::default();
        d.load_file(&path, None).unwrap();
        assert_eq!(d.queue.jobs.len(), 2);
        assert_eq!(d.queue.path.as_deref(), Some(path.as_str()));
        assert!(d.load_file("/nonexistent/x.esq", None).unwrap_err().contains("cannot read"));
        assert_eq!(hms(3725.0), "01:02:05");
    }
}
