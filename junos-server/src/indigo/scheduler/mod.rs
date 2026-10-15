//! The Ekos Scheduler, run by the relay.
//!
//! INDIGO has no scheduler, so this module plays KStars' `SchedulerProcess`
//! for the browser: a job list (`job.rs`, filled from the browser's form map
//! or an `.esl` file, `esl.rs`), a simplified greedy selection on each job's
//! start condition and constraints (`constraints.rs`: altitude, Moon,
//! twilight), the per-job stage pipeline — slew (the mount device), focus
//! (the Imager Agent), align (the Astrometry Agent's CENTER), guide (the
//! Guider Agent), capture (`capture/`, the job's `.esq`) — with KStars'
//! retries, and the startup / shutdown procedures (`queue.rs`, the
//! task-queue files) around the night.
//!
//! The modules' outcomes are observed from the `new_*_state` messages they
//! emit (`observe`), which are exact transition events, the way
//! `SchedulerProcess` is driven by the modules' status signals; the mount is
//! read from the property store. Everything is pushed to the browser as
//! `new_scheduler_state {status | log}` and `scheduler_get_jobs`.
//!
//! Not covered: meridian flips, the artificial horizon, remembering job
//! progress across restarts, lead/follower jobs, per-job optical trains,
//! FITS-file targets, pausing, pre-emption of a running job.

pub mod constraints;
pub mod esl;
pub mod job;
pub mod queue;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use serde_json::{json, Map, Value};

use super::agents::AgentSet;
use super::align::{Action, Align};
use super::capture::{esq, Capture, CaptureEvent};
use super::commands::{mount_command, reply_msg, rig};
use super::focus::Focus;
use super::guide::Guide;
use super::mount;
use super::process::Log;
use super::protocol::{PropertyStore, State};
use super::roles::Roles;
use constraints::Site;
use job::{jd_of, local_of_jd, Completion, Form, JobState, SchedulerJob, Settings, Stage, StartCond};
use queue::{Procedure, ScriptReq, Slot};

/// A script the runner spawned for a procedure has ended: its exit code,
/// or why it didn't run ("timed out", a spawn error).
#[derive(Debug)]
pub struct ScriptDone {
    pub id: u64,
    pub outcome: Result<i32, String>,
}

/// Ekos' `SchedulerState`.
pub const STATUS_IDLE: i64 = 0;
pub const STATUS_STARTUP: i64 = 1;
pub const STATUS_RUNNING: i64 = 2;
pub const STATUS_SHUTDOWN: i64 = 4;
pub const STATUS_ABORTED: i64 = 5;
pub const STATUS_LOADING: i64 = 6;

const SLEW_TIMEOUT: Duration = Duration::from_secs(600);
const SLEW_RESTART: Duration = Duration::from_secs(10);
const FOCUS_TIMEOUT: Duration = Duration::from_secs(900);
const ALIGN_TIMEOUT: Duration = Duration::from_secs(600);
const GUIDE_TIMEOUT: Duration = Duration::from_secs(600);
const RESLEW_TIMEOUT: Duration = Duration::from_secs(300);
const BUSY_RETRY: Duration = Duration::from_secs(30);
const GUIDE_RETRY_BASE: Duration = Duration::from_secs(5);
/// KStars' `MAX_FAILURE_ATTEMPTS`.
const MAX_FAILURES: u8 = 5;
/// An `At` start this far in the past is missed (KStars' 500 s).
const MISSED_START: Duration = Duration::from_secs(500);
/// How far ahead a job's window is searched.
const HORIZON_DAYS: f64 = 1.0;

/// What the runner reads besides the modules.
pub struct SchedCtx<'a> {
    pub store: &'a PropertyStore,
    pub roles: &'a Roles,
    pub set: &'a AgentSet,
    pub captures_dir: &'a str,
    pub now: Instant,
    pub jd: f64,
    pub local: DateTime<Local>,
}

/// The modules the runner drives.
pub struct Drivers<'a> {
    pub capture: &'a mut Capture,
    pub focus: &'a mut Focus,
    pub guide: &'a mut Guide,
    pub align: &'a mut Align,
}

#[derive(Debug, Default)]
pub struct SchedReaction {
    pub to_browser: Vec<String>,
    pub to_indigo: Vec<String>,
    pub scripts: Vec<ScriptReq>,
}

/// A module transition the runner waits on.
#[derive(Clone, Debug, PartialEq)]
enum Signal {
    Focus(String),
    Align(String),
    Guide(String),
    Capture(CaptureEvent),
}

/// Why the runner stopped the capture itself.
#[derive(Clone, Debug, PartialEq)]
enum StopReason {
    /// A constraint failed: the job goes back to the pool.
    Constraint(String),
    /// `Completion::At` reached.
    Finish,
    /// Guiding was lost: restart it, then resume.
    GuideLost,
}

#[derive(Default)]
struct Run {
    /// The active job, if one runs.
    job: Option<usize>,
    stage_since: Option<Instant>,
    /// The stage couldn't start (a busy agent): retry since.
    pending_since: Option<Instant>,
    focus_fail: u8,
    align_fail: u8,
    guide_fail: u8,
    capture_fail: u8,
    guide_retry_at: Option<Instant>,
    tracking_ticks: u8,
    sleeping_until: Option<f64>,
    stopping: Option<StopReason>,
}

/// The procedure slots still to run, with the one in progress.
struct Procedures {
    current: Option<Procedure>,
    remaining: Vec<(Slot, PathBuf)>,
}

enum RunState {
    Idle,
    Startup(Procedures),
    Running(Run),
    Shutdown(Procedures),
    Aborted,
}

pub struct Scheduler {
    pub jobs: Vec<SchedulerJob>,
    pub form: Form,
    pub settings: Settings,
    state: RunState,
    loading: bool,
    log: Log,
    jobs_dirty: bool,
    pending: Vec<Signal>,
    last_guide_status: String,
    next_script_id: u64,
    /// The user's solve action, put back after a scheduler solve.
    saved_action: Option<Action>,
    /// Avoid repeating the sleep line every tick.
    sleep_logged: Option<f64>,
    no_site_logged: bool,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self {
            jobs: Vec::new(),
            form: Form::default(),
            settings: Settings::default(),
            state: RunState::Idle,
            loading: false,
            log: Log::default(),
            jobs_dirty: false,
            pending: Vec::new(),
            last_guide_status: String::new(),
            next_script_id: 1,
            saved_action: None,
            sleep_logged: None,
            no_site_logged: false,
        }
    }
}

fn site_of(ctx: &SchedCtx) -> Option<Site> {
    mount::site_latlon(ctx.store, ctx.roles.mount.as_deref()).map(|(lat, lon_east)| Site { lat, lon_east })
}

/// `sequence_count` / `duration_secs` / `in_sequence_focus` from a job's
/// `.esq`, times its repeats.
fn plan_job(job: &mut SchedulerJob) -> Result<(), String> {
    let xml = std::fs::read_to_string(&job.sequence).map_err(|e| format!("cannot read {}: {e}", job.sequence.display()))?;
    let plan = esq::parse(&xml)?.plan();
    let repeats = f64::from(job.repeats_required.max(1));
    job.sequence_count = plan.frames * job.repeats_required.max(1);
    job.duration_secs = plan.duration_secs * repeats;
    job.in_sequence_focus = plan.in_sequence_focus;
    Ok(())
}

impl Scheduler {
    pub fn status(&self) -> i64 {
        if self.loading {
            return STATUS_LOADING;
        }
        match self.state {
            RunState::Idle => STATUS_IDLE,
            RunState::Startup(_) => STATUS_STARTUP,
            RunState::Running(_) => STATUS_RUNNING,
            RunState::Shutdown(_) => STATUS_SHUTDOWN,
            RunState::Aborted => STATUS_ABORTED,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, RunState::Startup(_) | RunState::Running(_) | RunState::Shutdown(_))
    }

    fn status_msg(&self) -> String {
        reply_msg("new_scheduler_state", json!({ "status": self.status() }))
    }

    fn note(&mut self, line: impl AsRef<str>, r: &mut SchedReaction) {
        if self.log.push(line.as_ref()) {
            r.to_browser.push(reply_msg("new_scheduler_state", json!({ "log": self.log.text() })));
        }
    }

    pub fn jobs_msg(&self, ctx: &SchedCtx) -> String {
        let site = site_of(ctx);
        let jobs: Vec<Value> = self.jobs.iter().map(|j| j.to_json(ctx.jd, site.as_ref())).collect();
        reply_msg("scheduler_get_jobs", json!({ "jobs": jobs }))
    }

    fn flush_jobs(&mut self, ctx: &SchedCtx, r: &mut SchedReaction) {
        if self.jobs_dirty {
            self.jobs_dirty = false;
            r.to_browser.push(self.jobs_msg(ctx));
        }
    }

    // ── The job list ─────────────────────────────────────────────────────

    /// `scheduler_set_all_settings`: the settings keys, the rest into the
    /// job form.
    pub fn apply_settings(&mut self, map: &Map<String, Value>) {
        let mut rest = Map::new();
        for (k, v) in map {
            if !self.settings.apply(k, v) {
                rest.insert(k.clone(), v.clone());
            }
        }
        self.form.apply(&rest);
    }

    /// `scheduler_add_jobs`: a job from the form.
    pub fn add_job(&mut self, ctx: &SchedCtx, r: &mut SchedReaction) -> Result<(), String> {
        let mut job = self.form.to_job()?;
        if let Err(e) = plan_job(&mut job) {
            self.note(format!("Job '{}': {e} — marked invalid", job.name), r);
            job.state = JobState::Invalid;
        }
        if job.horizon {
            self.note(format!("Job '{}': the artificial horizon is not available with the INDIGO backend, ignored", job.name), r);
        }
        self.note(format!("Job '{}' added", job.name), r);
        self.jobs.push(job);
        self.jobs_dirty = true;
        if matches!(self.state, RunState::Running(_)) {
            self.evaluate(ctx, r);
        }
        self.flush_jobs(ctx, r);
        Ok(())
    }

    /// `scheduler_remove_jobs {index}`.
    pub fn remove_job(&mut self, index: usize, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) -> Result<(), String> {
        if index >= self.jobs.len() {
            return Err(format!("no job #{}", index + 1));
        }
        if let RunState::Running(run) = &mut self.state {
            match run.job {
                Some(i) if i == index => {
                    let name = self.jobs[index].name.clone();
                    self.stop_stage(ctx, d, r);
                    self.note(format!("Job '{name}' removed while running"), r);
                    if let RunState::Running(run) = &mut self.state {
                        *run = Run::default();
                    }
                }
                Some(i) if i > index => run.job = Some(i - 1),
                _ => {}
            }
        }
        self.jobs.remove(index);
        self.jobs_dirty = true;
        self.flush_jobs(ctx, r);
        Ok(())
    }

    /// `scheduler_load_file`: replace the list (and the file's settings).
    pub fn load_file(&mut self, path: &Path, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) -> Result<(), String> {
        let xml = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        self.load_xml(&xml, ctx, d, r)
    }

    pub fn load_xml(&mut self, xml: &str, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) -> Result<(), String> {
        let loaded = esl::read(xml)?;
        if self.is_running() {
            let mut rr = self.stop(ctx, d);
            r.to_browser.append(&mut rr.to_browser);
            r.to_indigo.append(&mut rr.to_indigo);
        }
        self.loading = true;
        r.to_browser.push(self.status_msg());
        self.jobs.clear();
        for w in loaded.warnings {
            self.note(w, r);
        }
        for mut job in loaded.jobs {
            if let Err(e) = plan_job(&mut job) {
                self.note(format!("Job '{}': {e} — marked invalid", job.name), r);
                job.state = JobState::Invalid;
            }
            self.jobs.push(job);
        }
        let f = loaded.settings;
        let s = &mut self.settings;
        if let Some(v) = f.reschedule_errors { s.reschedule_errors = v; }
        if let Some(v) = f.error_delay_secs { s.error_delay_secs = v; }
        if let Some(v) = f.startup_enabled { s.startup_enabled = v; }
        if let Some(v) = f.shutdown_enabled { s.shutdown_enabled = v; }
        if let Some(v) = f.pre_startup { s.pre_startup = v; }
        if let Some(v) = f.post_startup { s.post_startup = v; }
        if let Some(v) = f.pre_shutdown { s.pre_shutdown = v; }
        if let Some(v) = f.post_shutdown { s.post_shutdown = v; }
        self.loading = false;
        self.note(format!("Schedule loaded: {} job(s)", self.jobs.len()), r);
        r.to_browser.push(self.status_msg());
        self.jobs_dirty = true;
        self.flush_jobs(ctx, r);
        Ok(())
    }

    /// `scheduler_save_file`: the `.esl` text (always written).
    pub fn save_file(&mut self, path: &Path, profile: &str, r: &mut SchedReaction) -> Result<String, String> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let xml = esl::write(&self.jobs, &self.settings, profile);
        std::fs::write(path, &xml).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        self.note(format!("Scheduler list saved to {}", path.display()), r);
        Ok(xml)
    }

    // ── Start / stop ─────────────────────────────────────────────────────

    fn procedures(&self, startup: bool) -> Vec<(Slot, PathBuf)> {
        let s = &self.settings;
        let slots: [(Slot, &str, bool); 2] = if startup {
            [(Slot::PreStartup, &s.pre_startup, s.startup_enabled), (Slot::PostStartup, &s.post_startup, s.startup_enabled)]
        } else {
            [(Slot::PreShutdown, &s.pre_shutdown, s.shutdown_enabled), (Slot::PostShutdown, &s.post_shutdown, s.shutdown_enabled)]
        };
        slots
            .into_iter()
            .filter(|(_, p, enabled)| *enabled && !p.trim().is_empty())
            .map(|(slot, p, _)| (slot, PathBuf::from(p.trim())))
            .collect()
    }

    /// `scheduler_start_job` while not running.
    pub fn start(&mut self, ctx: &SchedCtx, d: &mut Drivers) -> SchedReaction {
        let mut r = SchedReaction::default();
        if self.is_running() {
            return r;
        }
        if self.jobs.is_empty() {
            self.note("No jobs to run", &mut r);
            return r;
        }
        for j in &mut self.jobs {
            j.reset();
        }
        for i in 0..self.jobs.len() {
            if let Err(e) = plan_job(&mut self.jobs[i]) {
                let name = self.jobs[i].name.clone();
                self.note(format!("Job '{name}': {e} — marked invalid"), &mut r);
                self.jobs[i].state = JobState::Invalid;
            }
        }
        self.jobs_dirty = true;
        self.sleep_logged = None;
        if self.jobs.iter().all(|j| matches!(j.state, JobState::Invalid | JobState::Complete)) {
            self.note("No executable jobs found", &mut r);
            self.flush_jobs(ctx, &mut r);
            return r;
        }
        self.note("Scheduler started", &mut r);
        // A preview loop and the queue can't share the camera.
        let _ = d;
        let startup = self.procedures(true);
        if startup.is_empty() {
            self.state = RunState::Running(Run::default());
            r.to_browser.push(self.status_msg());
            self.evaluate(ctx, &mut r);
        } else {
            self.state = RunState::Startup(Procedures { current: None, remaining: startup });
            r.to_browser.push(self.status_msg());
            self.note("Running the startup procedure", &mut r);
        }
        self.flush_jobs(ctx, &mut r);
        r
    }

    /// `scheduler_start_job` while running (KStars' toggle), `profile_stop`.
    pub fn stop(&mut self, ctx: &SchedCtx, d: &mut Drivers) -> SchedReaction {
        let mut r = SchedReaction::default();
        match &mut self.state {
            RunState::Idle | RunState::Aborted => return r,
            RunState::Startup(p) | RunState::Shutdown(p) => {
                if let Some(proc_) = &mut p.current {
                    r.to_indigo.extend(proc_.abort());
                }
            }
            RunState::Running(_) => self.stop_stage(ctx, d, &mut r),
        }
        for j in &mut self.jobs {
            if matches!(j.state, JobState::Idle | JobState::Evaluation | JobState::Scheduled | JobState::Busy) {
                j.state = JobState::Aborted;
                j.last_abort = Some(ctx.now);
            }
        }
        self.state = RunState::Idle;
        self.jobs_dirty = true;
        self.note("Scheduler stopped", &mut r);
        r.to_browser.push(self.status_msg());
        self.flush_jobs(ctx, &mut r);
        r
    }

    /// Abort whatever the active job's stage is doing.
    fn stop_stage(&mut self, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else { return };
        match self.jobs[idx].stage {
            Stage::Slewing | Stage::Reslewing => {
                if let Some(m) = ctx.roles.mount.as_deref() {
                    r.to_indigo.extend(mount_command("mount_abort", &Value::Null, m, ctx.store));
                }
            }
            Stage::Focusing | Stage::PostAlignFocusing => {
                if let Some(a) = ctx.set.imager.as_deref() {
                    r.to_indigo.extend(d.focus.stop(a));
                }
            }
            Stage::Aligning => {
                if let Some(a) = ctx.set.astrometry.as_deref() {
                    r.to_indigo.extend(d.align.stop(a));
                }
                self.restore_action(d);
            }
            Stage::Guiding => {
                if let Some(a) = ctx.set.guider.as_deref() {
                    r.to_indigo.extend(d.guide.stop(a));
                }
            }
            Stage::Capturing => {
                if let Some(a) = ctx.set.imager.as_deref() {
                    if d.capture.is_running() {
                        let cr = d.capture.stop(a);
                        r.to_indigo.extend(cr.to_indigo);
                        r.to_browser.extend(cr.to_browser);
                    }
                }
                if self.jobs[idx].steps.guide {
                    if let Some(a) = ctx.set.guider.as_deref() {
                        r.to_indigo.extend(d.guide.stop(a));
                    }
                }
            }
            _ => {}
        }
    }

    fn restore_action(&mut self, d: &mut Drivers) {
        if let Some(a) = self.saved_action.take() {
            d.align.set_action(a);
        }
    }

    // ── Observation ──────────────────────────────────────────────────────

    /// The messages the modules just produced, and the capture queue's
    /// events; `step` acts on them.
    pub fn observe(&mut self, msgs: &[String], events: Vec<CaptureEvent>) {
        for m in msgs {
            let Ok(v) = serde_json::from_str::<Value>(m) else { continue };
            let Some(status) = v["payload"]["status"].as_str() else { continue };
            let signal = match v["type"].as_str() {
                Some("new_focus_state") => Signal::Focus(status.to_string()),
                Some("new_align_state") => Signal::Align(status.to_string()),
                Some("new_guide_state") => {
                    self.last_guide_status = status.to_string();
                    Signal::Guide(status.to_string())
                }
                _ => continue,
            };
            if self.is_running() {
                self.pending.push(signal);
            }
        }
        if self.is_running() {
            self.pending.extend(events.into_iter().map(Signal::Capture));
        }
    }

    /// React to what `observe` recorded.
    pub fn step(&mut self, ctx: &SchedCtx, d: &mut Drivers) -> SchedReaction {
        let mut r = SchedReaction::default();
        let signals = std::mem::take(&mut self.pending);
        for s in signals {
            self.signal(s, ctx, d, &mut r);
        }
        self.flush_jobs(ctx, &mut r);
        r
    }

    fn signal(&mut self, s: Signal, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else { return };
        let stage = self.jobs[idx].stage;
        match (stage, s) {
            (Stage::Focusing | Stage::PostAlignFocusing, Signal::Focus(st)) => match st.as_str() {
                "Complete" => {
                    let done = if stage == Stage::Focusing { Stage::FocusComplete } else { Stage::PostAlignFocusingComplete };
                    self.set_stage(idx, done, ctx);
                    self.next_action(ctx, d, r);
                }
                "Failed" | "Aborted" => {
                    let n = self.bump(|run| &mut run.focus_fail);
                    if n < MAX_FAILURES {
                        self.note(format!("Autofocus failed, attempt {n}/{MAX_FAILURES}; retrying"), r);
                        self.enter_stage(stage, ctx, d, r);
                    } else {
                        self.abort_job(format!("autofocus failed {MAX_FAILURES} times"), ctx, d, r);
                    }
                }
                _ => {}
            },
            (Stage::Aligning, Signal::Align(st)) => match st.as_str() {
                "Successful" => {
                    self.restore_action(d);
                    self.set_stage(idx, Stage::AlignComplete, ctx);
                    self.next_action(ctx, d, r);
                }
                "Failed" | "Aborted" => {
                    let n = self.bump(|run| &mut run.align_fail);
                    if n < MAX_FAILURES {
                        self.note(format!("Alignment failed, attempt {n}/{MAX_FAILURES}; retrying"), r);
                        self.enter_stage(Stage::Aligning, ctx, d, r);
                    } else {
                        self.restore_action(d);
                        self.abort_job(format!("alignment failed {MAX_FAILURES} times"), ctx, d, r);
                    }
                }
                _ => {}
            },
            (Stage::Guiding, Signal::Guide(st)) => match st.as_str() {
                "Guiding" => {
                    self.set_stage(idx, Stage::GuidingComplete, ctx);
                    self.next_action(ctx, d, r);
                }
                "Calibration error" | "Aborted" | "Idle" => {
                    let n = self.bump(|run| &mut run.guide_fail);
                    if n < MAX_FAILURES {
                        let delay = GUIDE_RETRY_BASE * u32::from(n);
                        self.note(format!("Guiding failed ({st}), attempt {n}/{MAX_FAILURES}; retrying in {} s", delay.as_secs()), r);
                        if let RunState::Running(run) = &mut self.state {
                            run.guide_retry_at = Some(ctx.now + delay);
                        }
                    } else {
                        self.abort_job(format!("guiding failed {MAX_FAILURES} times"), ctx, d, r);
                    }
                }
                _ => {}
            },
            (Stage::Capturing, Signal::Guide(st)) => {
                if self.jobs[idx].steps.guide && matches!(st.as_str(), "Calibration error" | "Aborted" | "Idle") {
                    self.note(format!("Guiding lost ({st}) during capture: restarting it"), r);
                    self.stop_capture(StopReason::GuideLost, ctx, d, r);
                }
            }
            (Stage::Capturing, Signal::Capture(ev)) => match ev {
                CaptureEvent::ImageReceived => {
                    self.jobs[idx].completed_count += 1;
                    self.jobs_dirty = true;
                }
                CaptureEvent::Complete => self.job_complete(ctx, d, r),
                CaptureEvent::Aborted(why) => {
                    let stopping = match &self.state {
                        RunState::Running(run) => run.stopping.clone(),
                        _ => None,
                    };
                    match stopping {
                        Some(StopReason::Constraint(c)) => {
                            self.note(format!("Job '{}' interrupted: {c}; it will be rescheduled", self.jobs[idx].name), r);
                            self.release_job(JobState::Idle, ctx, d, r);
                        }
                        Some(StopReason::Finish) => {
                            self.note(format!("Job '{}' reached its finish time", self.jobs[idx].name), r);
                            self.release_job(JobState::Complete, ctx, d, r);
                        }
                        Some(StopReason::GuideLost) => {
                            if let RunState::Running(run) = &mut self.state {
                                run.stopping = None;
                            }
                            self.enter_stage(Stage::Guiding, ctx, d, r);
                        }
                        None => {
                            let n = self.bump(|run| &mut run.capture_fail);
                            if n < MAX_FAILURES {
                                self.note(format!("Capture aborted ({why}), attempt {n}/{MAX_FAILURES}; restarting"), r);
                                self.enter_stage(Stage::Capturing, ctx, d, r);
                            } else {
                                self.abort_job(format!("capture failed {MAX_FAILURES} times"), ctx, d, r);
                            }
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn bump(&mut self, field: impl FnOnce(&mut Run) -> &mut u8) -> u8 {
        match &mut self.state {
            RunState::Running(run) => {
                let f = field(run);
                *f += 1;
                *f
            }
            _ => 0,
        }
    }

    // ── Evaluation ───────────────────────────────────────────────────────

    /// Rank the pool: every job's next start, then run the first that can
    /// start now, or sleep until one can, or end the night.
    fn evaluate(&mut self, ctx: &SchedCtx, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        if run.job.is_some() {
            return;
        }
        let site = site_of(ctx);
        if site.is_none() && !self.no_site_logged {
            self.no_site_logged = true;
            self.note("No observer location from the mount: altitude, Moon and twilight constraints are not evaluated", r);
        }
        let delay = Duration::from_secs(u64::from(self.settings.error_delay_secs));
        let reschedule_errors = self.settings.reschedule_errors;
        let mut runnable: Option<usize> = None;
        let mut earliest: Option<f64> = None;
        let mut notes = Vec::new();
        for (i, job) in self.jobs.iter_mut().enumerate() {
            match job.state {
                JobState::Complete | JobState::Invalid => continue,
                JobState::Error if !(reschedule_errors && job.last_error.is_some_and(|t| ctx.now.duration_since(t) >= delay)) => continue,
                JobState::Aborted if !job.last_abort.is_some_and(|t| ctx.now.duration_since(t) >= delay) => continue,
                _ => {}
            }
            if let Completion::At(t) = &job.completion {
                if *t <= ctx.local {
                    job.state = JobState::Complete;
                    notes.push(format!("Job '{}': its finish time has passed, complete", job.name));
                    continue;
                }
            }
            let not_before = match &job.start {
                StartCond::Asap => None,
                StartCond::At(t) => {
                    if ctx.local.signed_duration_since(*t).to_std().is_ok_and(|d| d > MISSED_START) && job.state != JobState::Aborted {
                        job.state = JobState::Invalid;
                        notes.push(format!("Job '{}' missed its start time", job.name));
                        continue;
                    }
                    Some(jd_of(t))
                }
            };
            let start = match &site {
                Some(site) => constraints::next_start(&job.rules(), job.ra0_h, job.dec0_deg, ctx.jd, not_before, site, HORIZON_DAYS),
                None => Some(not_before.map_or(ctx.jd, |t| t.max(ctx.jd))),
            };
            job.state = JobState::Scheduled;
            job.stage = Stage::Idle;
            match start {
                None => {
                    job.startup_time = None;
                    job.stop_time = None;
                }
                Some(t) => {
                    job.startup_time = Some(local_of_jd(t));
                    let natural = t + job.duration_secs / 86_400.0;
                    let mut end = match job.completion {
                        Completion::Loop => None,
                        _ => Some(natural),
                    };
                    if let Some(site) = &site {
                        if let Some(e) = constraints::next_end(&job.rules(), job.ra0_h, job.dec0_deg, t, site, HORIZON_DAYS) {
                            end = Some(end.map_or(e, |x: f64| x.min(e)));
                        }
                    }
                    if let Completion::At(at) = &job.completion {
                        let at = jd_of(at);
                        end = Some(end.map_or(at, |x: f64| x.min(at)));
                    }
                    job.stop_time = end.map(local_of_jd);
                    // Startable now (within the evaluation minute).
                    if t <= ctx.jd + 30.0 / 86_400.0 {
                        if runnable.is_none() {
                            runnable = Some(i);
                        }
                    } else {
                        earliest = Some(earliest.map_or(t, |e: f64| e.min(t)));
                    }
                }
            }
        }
        for n in notes {
            self.note(n, r);
        }
        self.jobs_dirty = true;
        match (runnable, earliest) {
            (Some(i), _) => {
                if let RunState::Running(run) = &mut self.state {
                    run.sleeping_until = None;
                }
                self.sleep_logged = None;
                self.execute_job(i, ctx, r);
            }
            (None, Some(t)) => {
                if let RunState::Running(run) = &mut self.state {
                    run.sleeping_until = Some(t);
                }
                if self.sleep_logged != Some(t) {
                    self.sleep_logged = Some(t);
                    let when = local_of_jd(t);
                    self.note(format!("Sleeping until {}", when.format("%H:%M")), r);
                }
            }
            (None, None) => self.end_of_jobs(ctx, r),
        }
    }

    fn end_of_jobs(&mut self, ctx: &SchedCtx, r: &mut SchedReaction) {
        let _ = ctx;
        let shutdown = self.procedures(false);
        if shutdown.is_empty() {
            self.note("No jobs left in the scheduler queue", r);
            self.state = RunState::Idle;
        } else {
            self.note("No jobs left: running the shutdown procedure", r);
            self.state = RunState::Shutdown(Procedures { current: None, remaining: shutdown });
        }
        r.to_browser.push(self.status_msg());
    }

    fn execute_job(&mut self, idx: usize, ctx: &SchedCtx, r: &mut SchedReaction) {
        let job = &mut self.jobs[idx];
        job.state = JobState::Busy;
        job.state_time = Some(ctx.local);
        job.stage = Stage::Idle;
        let name = job.name.clone();
        self.state = RunState::Running(Run { job: Some(idx), stage_since: Some(ctx.now), ..Run::default() });
        self.note(format!("Executing job '{name}'"), r);
        self.jobs_dirty = true;
        // Entering the stages needs the drivers: the next tick / step does it
        // through `pending_since`.
        if let RunState::Running(run) = &mut self.state {
            run.pending_since = Some(ctx.now);
        }
    }

    fn set_stage(&mut self, idx: usize, stage: Stage, ctx: &SchedCtx) {
        self.jobs[idx].stage = stage;
        self.jobs_dirty = true;
        if let RunState::Running(run) = &mut self.state {
            run.stage_since = Some(ctx.now);
            run.pending_since = None;
            run.tracking_ticks = 0;
        }
    }

    /// KStars' `getNextAction`: the stage after the one just completed.
    fn next_action(&mut self, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else { return };
        let job = &self.jobs[idx];
        let s = job.steps;
        let next = match job.stage {
            Stage::Idle => {
                if s.track { Stage::Slewing } else if s.focus { Stage::Focusing } else if s.align { Stage::Aligning } else if s.guide { Stage::Guiding } else { Stage::Capturing }
            }
            Stage::SlewComplete => {
                if s.focus { Stage::Focusing } else if s.align { Stage::Aligning } else if s.guide { Stage::Guiding } else { Stage::Capturing }
            }
            Stage::FocusComplete => {
                if s.align { Stage::Aligning } else if s.guide { Stage::Guiding } else { Stage::Capturing }
            }
            Stage::AlignComplete => Stage::Reslewing,
            Stage::ReslewingComplete => {
                if s.focus && job.in_sequence_focus { Stage::PostAlignFocusing } else if s.guide { Stage::Guiding } else { Stage::Capturing }
            }
            Stage::PostAlignFocusingComplete => {
                if s.guide { Stage::Guiding } else { Stage::Capturing }
            }
            _ => Stage::Capturing,
        };
        self.enter_stage(next, ctx, d, r);
    }

    /// Start a stage. A busy module leaves `pending_since` set for a retry.
    fn enter_stage(&mut self, stage: Stage, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else { return };
        if self.jobs[idx].stage != stage {
            self.set_stage(idx, stage, ctx);
        } else if let RunState::Running(run) = &mut self.state {
            run.pending_since = None;
        }
        let name = self.jobs[idx].name.clone();
        let retry = |me: &mut Self, why: &str, r: &mut SchedReaction| {
            if let RunState::Running(run) = &mut me.state {
                if run.pending_since.is_none() {
                    run.pending_since = Some(ctx.now);
                    me.note(format!("{why}, waiting"), r);
                }
            }
        };
        match stage {
            Stage::Slewing => {
                let Some(m) = ctx.roles.mount.as_deref() else {
                    self.fail_job("no mount for the slew", ctx, d, r);
                    return;
                };
                let status = mount::status(ctx.store, m);
                if status == "Parked" {
                    self.note("Unparking the mount", r);
                    r.to_indigo.extend(mount_command("mount_unpark", &Value::Null, m, ctx.store));
                }
                let (ra, de) = (self.jobs[idx].ra0_h, self.jobs[idx].dec0_deg);
                self.note(format!("Slewing to '{name}'"), r);
                let payload = json!({ "ra": format!("{ra:.6}"), "de": format!("{de:.6}"), "isJ2000": true });
                r.to_indigo.extend(mount_command("mount_goto_rade", &payload, m, ctx.store));
            }
            Stage::Focusing | Stage::PostAlignFocusing => {
                let Some(a) = ctx.set.imager.clone() else {
                    self.fail_job("no Imager Agent to focus with", ctx, d, r);
                    return;
                };
                match d.focus.start(ctx.store, &a) {
                    Some(w) => {
                        self.note(format!("Focusing for '{name}'"), r);
                        r.to_indigo.extend(w);
                    }
                    None => retry(self, "The Imager Agent is busy", r),
                }
            }
            Stage::Aligning => {
                let Some(a) = ctx.set.astrometry.clone() else {
                    self.fail_job("no Astrometry Agent to align with", ctx, d, r);
                    return;
                };
                if self.saved_action.is_none() {
                    self.saved_action = Some(d.align.set_action(Action::Slew));
                }
                let rig = rig(ctx.set, ctx.store, ctx.roles);
                match d.align.solve(ctx.store, &a, &rig) {
                    Ok((w, warning)) => {
                        self.note(format!("Solving and centering '{name}'"), r);
                        if let Some(w) = warning {
                            self.note(w, r);
                        }
                        r.to_indigo.extend(w);
                    }
                    Err(why) if why.contains("busy") => retry(self, &why, r),
                    Err(why) => {
                        self.restore_action(d);
                        self.fail_job(&why, ctx, d, r);
                    }
                }
            }
            Stage::Reslewing => {
                self.note("Waiting for the mount to settle", r);
            }
            Stage::Guiding => {
                if self.last_guide_status == "Guiding" {
                    self.note("Guiding already running", r);
                    self.set_stage(idx, Stage::GuidingComplete, ctx);
                    self.next_action(ctx, d, r);
                    return;
                }
                let Some(a) = ctx.set.guider.clone() else {
                    self.fail_job("no Guider Agent to guide with", ctx, d, r);
                    return;
                };
                if ctx.roles.guider.is_none() {
                    self.fail_job("no guide camera on the Guider Agent", ctx, d, r);
                    return;
                }
                match d.guide.start(ctx.store, &a) {
                    Some(w) => {
                        self.note(format!("Starting guiding for '{name}'"), r);
                        r.to_indigo.extend(w);
                    }
                    None => retry(self, "The Guider Agent is busy", r),
                }
            }
            Stage::Capturing => {
                let Some(a) = ctx.set.imager.clone() else {
                    self.fail_job("no Imager Agent to capture with", ctx, d, r);
                    return;
                };
                let job = &self.jobs[idx];
                let path = job.sequence.to_string_lossy().into_owned();
                if !d.capture.is_running() && d.capture.queue.path.as_deref() != Some(path.as_str()) || d.capture.is_complete() {
                    if let Err(e) = d.capture.load_file(&path, Some(&name)) {
                        self.jobs[idx].state = JobState::Invalid;
                        self.note(format!("Job '{name}': {e} — marked invalid"), r);
                        self.release_job(JobState::Invalid, ctx, d, r);
                        return;
                    }
                    if d.capture.is_complete() {
                        d.capture.reset_progress();
                    }
                }
                match d.capture.start(ctx.store, &a, ctx.captures_dir, ctx.now) {
                    Ok(cr) => {
                        self.note(format!("Capturing '{name}'"), r);
                        r.to_indigo.extend(cr.to_indigo);
                        r.to_browser.extend(cr.to_browser);
                    }
                    Err(why) if why.contains("busy") => retry(self, &why, r),
                    Err(why) => self.fail_job(&why, ctx, d, r),
                }
            }
            _ => {}
        }
    }

    /// The job can't run on this rig (KStars: `SCHEDJOB_ERROR`).
    fn fail_job(&mut self, why: &str, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else { return };
        self.note(format!("Job '{}' error: {why}", self.jobs[idx].name), r);
        self.jobs[idx].last_error = Some(ctx.now);
        self.release_job(JobState::Error, ctx, d, r);
    }

    /// The job failed after KStars' retries.
    fn abort_job(&mut self, why: String, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else { return };
        self.note(format!("Job '{}' aborted: {why}", self.jobs[idx].name), r);
        self.stop_stage(ctx, d, r);
        self.jobs[idx].last_abort = Some(ctx.now);
        self.jobs[idx].last_abort_reason = why;
        self.release_job(JobState::Aborted, ctx, d, r);
    }

    /// Leave the active job in `state` and look for the next one.
    fn release_job(&mut self, state: JobState, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else { return };
        if self.jobs[idx].steps.guide && state != JobState::Idle {
            if let Some(a) = ctx.set.guider.as_deref() {
                if self.last_guide_status == "Guiding" || self.last_guide_status == "Calibrating" {
                    r.to_indigo.extend(d.guide.stop(a));
                }
            }
        }
        self.restore_action(d);
        let job = &mut self.jobs[idx];
        job.state = state;
        job.stage = if state == JobState::Complete { Stage::Complete } else { Stage::Idle };
        self.state = RunState::Running(Run::default());
        self.jobs_dirty = true;
        self.evaluate(ctx, r);
    }

    /// KStars' `findNextJob` on a finished sequence.
    fn job_complete(&mut self, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else { return };
        let name = self.jobs[idx].name.clone();
        let again = match self.jobs[idx].completion {
            Completion::Sequence => false,
            Completion::Repeat(_) => {
                let job = &mut self.jobs[idx];
                job.repeats_remaining = job.repeats_remaining.saturating_sub(1);
                job.repeats_remaining > 0
            }
            Completion::Loop | Completion::At(_) => true,
        };
        if !again {
            self.note(format!("Job '{name}' complete"), r);
            self.release_job(JobState::Complete, ctx, d, r);
            return;
        }
        let left = self.jobs[idx].repeats_remaining;
        self.note(format!("Job '{name}': sequence complete, repeating ({left} left)"), r);
        d.capture.reset_progress();
        self.jobs[idx].stage = Stage::Idle;
        self.jobs_dirty = true;
        // Guiding still running → straight back to capture; align without
        // guide → re-align; track only → re-slew.
        let s = self.jobs[idx].steps;
        let next = if s.guide && self.last_guide_status == "Guiding" {
            Stage::Capturing
        } else if s.align {
            Stage::Aligning
        } else if s.track {
            Stage::Slewing
        } else {
            Stage::Capturing
        };
        self.enter_stage(next, ctx, d, r);
    }

    fn stop_capture(&mut self, why: StopReason, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        if let RunState::Running(run) = &mut self.state {
            run.stopping = Some(why);
        }
        if let Some(a) = ctx.set.imager.as_deref() {
            if d.capture.is_running() {
                let cr = d.capture.stop(a);
                r.to_indigo.extend(cr.to_indigo);
                r.to_browser.extend(cr.to_browser);
                return;
            }
        }
        // Nothing to stop: settle at once.
        self.pending.push(Signal::Capture(CaptureEvent::Aborted("stopped".into())));
    }

    // ── Tick ─────────────────────────────────────────────────────────────

    pub fn script_finished(&mut self, done: ScriptDone) {
        if let RunState::Startup(p) | RunState::Shutdown(p) = &mut self.state {
            if let Some(proc_) = &mut p.current {
                proc_.script_finished(done.id, done.outcome);
            }
        }
    }

    /// Once a second: procedures, evaluation, stage timeouts, constraints.
    pub fn tick(&mut self, ctx: &SchedCtx, d: &mut Drivers) -> SchedReaction {
        let mut r = SchedReaction::default();
        match &mut self.state {
            RunState::Idle | RunState::Aborted => return r,
            RunState::Startup(_) | RunState::Shutdown(_) => self.tick_procedures(ctx, &mut r),
            RunState::Running(_) => self.tick_running(ctx, d, &mut r),
        }
        self.flush_jobs(ctx, &mut r);
        r
    }

    fn tick_procedures(&mut self, ctx: &SchedCtx, r: &mut SchedReaction) {
        let startup = matches!(self.state, RunState::Startup(_));
        let (RunState::Startup(p) | RunState::Shutdown(p)) = &mut self.state else { return };
        if p.current.is_none() {
            match p.remaining.first().cloned() {
                Some((slot, path)) => {
                    p.remaining.remove(0);
                    match queue::load(&path) {
                        Ok((steps, warnings)) => {
                            let proc_ = Procedure::new(slot, steps, self.next_script_id);
                            self.next_script_id += 1000;
                            p.current = Some(proc_);
                            let mut lines = vec![format!("Running the {} queue {}", slot.label(), path.display())];
                            lines.extend(warnings);
                            for l in lines {
                                self.note(l, r);
                            }
                        }
                        Err(e) => {
                            self.note(format!("Cannot load the {} queue {}: {e}", slot.label(), path.display()), r);
                            self.procedures_failed(r);
                        }
                    }
                    return;
                }
                None => {
                    self.procedures_done(startup, ctx, r);
                    return;
                }
            }
        }
        let Some(proc_) = p.current.as_mut() else { return };
        let adv = proc_.advance(ctx.store, ctx.roles, ctx.now);
        r.to_indigo.extend(adv.to_indigo);
        r.scripts.extend(adv.scripts);
        let finished = adv.finished;
        for l in adv.log {
            self.note(l, r);
        }
        match finished {
            Some(Ok(())) => {
                if let RunState::Startup(p) | RunState::Shutdown(p) = &mut self.state {
                    p.current = None;
                }
            }
            Some(Err(why)) => {
                self.note(format!("Procedure failed: {why}"), r);
                self.procedures_failed(r);
            }
            None => {}
        }
    }

    fn procedures_failed(&mut self, r: &mut SchedReaction) {
        for j in &mut self.jobs {
            if matches!(j.state, JobState::Idle | JobState::Evaluation | JobState::Scheduled | JobState::Busy) {
                j.state = JobState::Aborted;
            }
        }
        self.jobs_dirty = true;
        self.state = RunState::Aborted;
        r.to_browser.push(self.status_msg());
    }

    fn procedures_done(&mut self, startup: bool, ctx: &SchedCtx, r: &mut SchedReaction) {
        if startup {
            self.note("Startup procedure complete", r);
            self.state = RunState::Running(Run::default());
            r.to_browser.push(self.status_msg());
            self.evaluate(ctx, r);
        } else {
            self.note("Shutdown complete", r);
            self.state = RunState::Idle;
            r.to_browser.push(self.status_msg());
        }
    }

    fn tick_running(&mut self, ctx: &SchedCtx, d: &mut Drivers, r: &mut SchedReaction) {
        let RunState::Running(run) = &self.state else { return };
        let Some(idx) = run.job else {
            match run.sleeping_until {
                Some(t) if t > ctx.jd + 30.0 / 86_400.0 => {}
                _ => self.evaluate(ctx, r),
            }
            return;
        };
        let stage = self.jobs[idx].stage;
        let since = run.stage_since.unwrap_or(ctx.now);
        let waited = ctx.now.duration_since(since);
        let pending = run.pending_since;
        let guide_retry = run.guide_retry_at;

        // A stage that couldn't start yet.
        if let Some(p) = pending {
            if ctx.now.duration_since(p) > BUSY_RETRY {
                self.abort_job("the module stayed busy".into(), ctx, d, r);
                return;
            }
            let target = if stage == Stage::Idle { None } else { Some(stage) };
            match target {
                None => self.next_action(ctx, d, r),
                Some(s) => self.enter_stage(s, ctx, d, r),
            }
            return;
        }
        if let Some(t) = guide_retry {
            if ctx.now >= t {
                if let RunState::Running(run) = &mut self.state {
                    run.guide_retry_at = None;
                }
                self.enter_stage(Stage::Guiding, ctx, d, r);
                return;
            }
        }

        match stage {
            Stage::Slewing | Stage::Reslewing => {
                let Some(m) = ctx.roles.mount.as_deref() else { return };
                let status = mount::status(ctx.store, m);
                let coords_busy = ctx.store.get(m, "MOUNT_EQUATORIAL_COORDINATES").is_some_and(|p| p.state == State::Busy);
                if status == "Tracking" && !coords_busy {
                    let ticks = match &mut self.state {
                        RunState::Running(run) => {
                            run.tracking_ticks += 1;
                            run.tracking_ticks
                        }
                        _ => 0,
                    };
                    let needed = if stage == Stage::Reslewing { 2 } else { 1 };
                    if ticks >= needed {
                        let done = if stage == Stage::Slewing { Stage::SlewComplete } else { Stage::ReslewingComplete };
                        self.note(if stage == Stage::Slewing { "Slew complete" } else { "Mount settled" }, r);
                        self.set_stage(idx, done, ctx);
                        self.next_action(ctx, d, r);
                    }
                    return;
                }
                if status == "Parked" && stage == Stage::Slewing && waited > SLEW_RESTART {
                    self.abort_job("the mount is parked".into(), ctx, d, r);
                    return;
                }
                let timeout = if stage == Stage::Slewing { SLEW_TIMEOUT } else { RESLEW_TIMEOUT };
                if waited > timeout {
                    self.abort_job("the mount did not reach the target".into(), ctx, d, r);
                } else if stage == Stage::Slewing && status == "Idle" && waited > SLEW_RESTART {
                    self.note("Mount is not slewing, restarting the slew", r);
                    self.enter_stage(Stage::Slewing, ctx, d, r);
                }
            }
            Stage::Focusing | Stage::PostAlignFocusing if waited > FOCUS_TIMEOUT => {
                self.abort_job("autofocus timed out".into(), ctx, d, r);
            }
            Stage::Aligning if waited > ALIGN_TIMEOUT => {
                self.restore_action(d);
                self.abort_job("alignment timed out".into(), ctx, d, r);
            }
            Stage::Guiding if waited > GUIDE_TIMEOUT => {
                self.abort_job("guiding did not start".into(), ctx, d, r);
            }
            Stage::Capturing => {
                let stopping = match &self.state {
                    RunState::Running(run) => run.stopping.is_some(),
                    _ => false,
                };
                if stopping {
                    return;
                }
                let job = &self.jobs[idx];
                if let Completion::At(t) = &job.completion {
                    if *t <= ctx.local {
                        self.stop_capture(StopReason::Finish, ctx, d, r);
                        return;
                    }
                }
                if let Some(site) = site_of(ctx) {
                    let snap = constraints::snapshot(job.ra0_h, job.dec0_deg, ctx.jd, &site);
                    if let Some(why) = constraints::violated(&job.rules(), &snap) {
                        self.stop_capture(StopReason::Constraint(why), ctx, d, r);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;
    use chrono::TimeZone;

    const IMAGER: &str = "Imager Agent";
    const GUIDER: &str = "Guider Agent";
    const SOLVER: &str = "Astrometry Agent";

    struct Env {
        store: PropertyStore,
        roles: Roles,
        set: AgentSet,
        now: Instant,
        local: DateTime<Local>,
    }

    struct Mods {
        capture: Capture,
        focus: Focus,
        guide: Guide,
        align: Align,
    }

    struct TestRig {
        env: Env,
        mods: Mods,
    }

    fn apply(s: &mut PropertyStore, frames: &[&str]) {
        for f in frames {
            s.apply(parse_frame(f).unwrap());
        }
    }

    fn test_rig() -> TestRig {
        let mut s = PropertyStore::default();
        apply(&mut s, &[
            r#"{"defTextVector":{"device":"Mount","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"1"}]}}"#,
            r#"{"defSwitchVector":{"device":"Mount","name":"CONNECTION","perm":"rw","state":"Ok","items":[{"name":"CONNECTED","value":true},{"name":"DISCONNECTED","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Mount","name":"GEOGRAPHIC_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"LATITUDE","value":48.5},{"name":"LONGITUDE","value":357.7},{"name":"ELEVATION","value":120}]}}"#,
            r#"{"defNumberVector":{"device":"Mount","name":"MOUNT_EQUATORIAL_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"RA","value":1},{"name":"DEC","value":2}]}}"#,
            r#"{"defSwitchVector":{"device":"Mount","name":"MOUNT_TRACKING","perm":"rw","state":"Ok","items":[{"name":"ON","value":false},{"name":"OFF","value":true}]}}"#,
            r#"{"defSwitchVector":{"device":"Mount","name":"MOUNT_PARK","perm":"rw","state":"Ok","items":[{"name":"PARKED","value":false},{"name":"UNPARKED","value":true}]}}"#,
            r#"{"defTextVector":{"device":"Cam","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"2"}]}}"#,
            r#"{"defTextVector":{"device":"Guide Cam","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"2"}]}}"#,
            r#"{"defTextVector":{"device":"Imager Agent","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"16384"}]}}"#,
            r#"{"defNumberVector":{"device":"Imager Agent","name":"AGENT_IMAGER_BATCH","perm":"rw","state":"Ok","items":[{"name":"COUNT","value":1},{"name":"EXPOSURE","value":1},{"name":"DELAY","value":0}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","perm":"rw","state":"Ok","rule":"AtMostOne","items":[{"name":"PREVIEW","value":false},{"name":"EXPOSURE","value":false},{"name":"FOCUSING","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_ABORT_PROCESS","perm":"rw","state":"Ok","items":[{"name":"ABORT","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"FILTER_CCD_LIST","perm":"rw","state":"Ok","rule":"OneOfMany","items":[{"name":"NONE","value":false},{"name":"Cam","value":true}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"CCD_UPLOAD_MODE","perm":"rw","state":"Ok","items":[{"name":"CLIENT","value":false},{"name":"LOCAL","value":true}]}}"#,
            r#"{"defTextVector":{"device":"Guider Agent","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"16384"}]}}"#,
            r#"{"defNumberVector":{"device":"Guider Agent","name":"AGENT_GUIDER_SETTINGS","perm":"rw","state":"Ok","items":[{"name":"SPEED_RA","value":0}]}}"#,
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"AGENT_START_PROCESS","perm":"rw","state":"Ok","rule":"AtMostOne","items":[{"name":"GUIDING","value":false},{"name":"CALIBRATION_AND_GUIDING","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"AGENT_ABORT_PROCESS","perm":"rw","state":"Ok","items":[{"name":"ABORT","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Guider Agent","name":"FILTER_CCD_LIST","perm":"rw","state":"Ok","rule":"OneOfMany","items":[{"name":"NONE","value":false},{"name":"Guide Cam","value":true}]}}"#,
            r#"{"defTextVector":{"device":"Astrometry Agent","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"16384"}]}}"#,
            r#"{"defNumberVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_WCS","perm":"ro","state":"Ok","items":[{"name":"STATE","value":0}]}}"#,
            r#"{"defSwitchVector":{"device":"Astrometry Agent","name":"AGENT_START_PROCESS","perm":"rw","state":"Ok","rule":"AtMostOne","items":[{"name":"SOLVE","value":false},{"name":"SYNC","value":false},{"name":"CENTER","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"Astrometry Agent","name":"AGENT_ABORT_PROCESS","perm":"rw","state":"Ok","items":[{"name":"ABORT","value":false}]}}"#,
            r#"{"defTextVector":{"device":"Mount Agent","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"16385"}]}}"#,
            r#"{"defSwitchVector":{"device":"Mount Agent","name":"FILTER_MOUNT_LIST","perm":"rw","state":"Ok","rule":"OneOfMany","items":[{"name":"NONE","value":false},{"name":"Mount","value":true}]}}"#,
        ]);
        let set = AgentSet::discover(&s);
        assert_eq!(set.imager.as_deref(), Some(IMAGER));
        assert_eq!(set.guider.as_deref(), Some(GUIDER));
        assert_eq!(set.astrometry.as_deref(), Some(SOLVER));
        let roles = Roles::derive(&s, &set);
        assert_eq!(roles.mount.as_deref(), Some("Mount"));
        TestRig {
            env: Env {
                store: s,
                roles,
                set,
                now: Instant::now(),
                // 2025-10-15 22:00 local (CEST): astronomical night at 48.5 N.
                local: Local.with_ymd_and_hms(2025, 10, 15, 22, 0, 0).single().unwrap(),
            },
            mods: Mods { capture: Capture::default(), focus: Focus::default(), guide: Guide::new(), align: Align::default() },
        }
    }

    fn ctx(env: &Env) -> SchedCtx<'_> {
        SchedCtx {
            store: &env.store,
            roles: &env.roles,
            set: &env.set,
            captures_dir: "/tmp",
            now: env.now,
            jd: jd_of(&env.local),
            local: env.local,
        }
    }

    fn drivers(m: &mut Mods) -> Drivers<'_> {
        Drivers { capture: &mut m.capture, focus: &mut m.focus, guide: &mut m.guide, align: &mut m.align }
    }

    impl TestRig {
        fn advance(&mut self, secs: u64) {
            self.env.now += Duration::from_secs(secs);
            self.env.local += chrono::Duration::seconds(secs as i64);
        }
    }

    fn tick(sch: &mut Scheduler, rig: &mut TestRig) -> SchedReaction {
        sch.tick(&ctx(&rig.env), &mut drivers(&mut rig.mods))
    }

    fn step(sch: &mut Scheduler, rig: &mut TestRig) -> SchedReaction {
        sch.step(&ctx(&rig.env), &mut drivers(&mut rig.mods))
    }

    fn start(sch: &mut Scheduler, rig: &mut TestRig) -> SchedReaction {
        sch.start(&ctx(&rig.env), &mut drivers(&mut rig.mods))
    }

    fn stop(sch: &mut Scheduler, rig: &mut TestRig) -> SchedReaction {
        sch.stop(&ctx(&rig.env), &mut drivers(&mut rig.mods))
    }

    fn add(sch: &mut Scheduler, rig: &TestRig, r: &mut SchedReaction) {
        sch.add_job(&ctx(&rig.env), r).unwrap();
    }

    fn esq_file() -> PathBuf {
        // One folder per test: they run in parallel and the capture queue
        // creates the job folders in there.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("junos-sched-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("job.esq");
        let xml = crate::indigo::capture::esq::tests::BROWSER_XML.replace("/data/caps &amp; more", &dir.to_string_lossy());
        std::fs::write(&p, xml).unwrap();
        p
    }

    /// Vega (always far above 30° at 22:00 local in mid-October at 48.5 N? —
    /// it is at ~50° in the west), with every step.
    fn vega_form(seq: &Path) -> Map<String, Value> {
        let v = json!({
            "asapConditionR": true, "startupTimeConditionR": false, "startupTimeEdit": "",
            "schedulerAltitude": true, "schedulerAltitudeValue": 30.0,
            "schedulerMoonSeparation": false, "schedulerMoonSeparationValue": 0.0,
            "schedulerMoonAltitude": false, "schedulerMoonAltitudeMaxValue": 90.0,
            "schedulerTwilight": true, "schedulerHorizon": false,
            "nameEdit": "Vega", "raBox": "18.615650", "decBox": "38.783690",
            "sequenceEdit": seq.to_string_lossy(), "positionAngleSpin": 0.0,
            "schedulerTrackStep": true, "schedulerFocusStep": true, "schedulerAlignStep": true, "schedulerGuideStep": true,
            "schedulerCompleteSequences": true, "schedulerRepeatSequences": false, "schedulerRepeatSequencesLimit": 1,
            "schedulerUntilTerminated": false, "schedulerUntil": false, "schedulerUntilValue": "",
        });
        v.as_object().unwrap().clone()
    }

    fn statuses(r: &SchedReaction) -> Vec<i64> {
        r.to_browser
            .iter()
            .filter_map(|m| serde_json::from_str::<Value>(m).ok())
            .filter(|v| v["type"] == "new_scheduler_state")
            .filter_map(|v| v["payload"]["status"].as_i64())
            .collect()
    }

    fn last_jobs(r: &SchedReaction) -> Option<Vec<Value>> {
        r.to_browser
            .iter()
            .rev()
            .filter_map(|m| serde_json::from_str::<Value>(m).ok())
            .find(|v| v["type"] == "scheduler_get_jobs")
            .map(|v| v["payload"]["jobs"].as_array().unwrap().clone())
    }

    fn writes(r: &SchedReaction) -> Vec<String> {
        r.to_indigo
            .iter()
            .map(|m| {
                let v: Value = serde_json::from_str(m).unwrap();
                let (_, b) = v.as_object().unwrap().iter().next().unwrap();
                format!("{}.{}.{}", b["device"].as_str().unwrap(), b["name"].as_str().unwrap(), b["items"][0]["name"].as_str().unwrap())
            })
            .collect()
    }

    fn msg(ty: &str, status: &str) -> String {
        reply_msg(ty, json!({ "status": status }))
    }

    /// Drive the capture queue's running batch to its end on the agent (the
    /// Busy edge, then Ok or Alert), and hand back what it reported.
    fn batch_ends(rig: &mut TestRig, ok: bool) -> Vec<CaptureEvent> {
        let now = rig.env.now;
        let frames = [
            r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Busy","items":[{"name":"EXPOSURE","value":true}]}}"#,
            if ok {
                r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Ok","message":"Batch finished","items":[{"name":"EXPOSURE","value":false}]}}"#
            } else {
                r#"{"setSwitchVector":{"device":"Imager Agent","name":"AGENT_START_PROCESS","state":"Alert","message":"Batch failed","items":[{"name":"EXPOSURE","value":false}]}}"#
            },
        ];
        for f in frames {
            let c = rig.env.store.apply(parse_frame(f).unwrap()).unwrap();
            rig.mods.capture.on_change(&c, &rig.env.store, IMAGER, now);
        }
        rig.mods.capture.drain_events()
    }

    /// Every batch of the loaded queue ends well: the `Complete` event.
    fn sequence_completes(rig: &mut TestRig) -> Vec<CaptureEvent> {
        let mut all = Vec::new();
        for _ in 0..4 {
            let ev = batch_ends(rig, true);
            let done = ev.contains(&CaptureEvent::Complete);
            all.extend(ev);
            if done {
                break;
            }
        }
        assert!(all.contains(&CaptureEvent::Complete), "{all:?}");
        all
    }

    #[test]
    fn form_to_job_evaluation_and_the_full_pipeline() {
        let mut rig = test_rig();
        let seq = esq_file();
        let mut sch = Scheduler::default();
        let mut r = SchedReaction::default();
        sch.apply_settings(&vega_form(&seq));
        add(&mut sch, &rig, &mut r);
        assert_eq!(sch.jobs.len(), 1);
        assert_eq!(sch.jobs[0].sequence_count, 12);
        assert!(sch.jobs[0].duration_secs > 1200.0);
        let jobs = last_jobs(&r).unwrap();
        assert_eq!(jobs[0]["name"], "Vega");
        assert_eq!(jobs[0]["state"], 0);

        // Start: no procedures → Running, the job is startable → Busy.
        let r = start(&mut sch, &mut rig);
        assert_eq!(statuses(&r), [STATUS_RUNNING]);
        assert_eq!(sch.status(), STATUS_RUNNING);
        let jobs = last_jobs(&r).unwrap();
        assert_eq!(jobs[0]["state"], 3, "{jobs:?}");
        assert!(jobs[0]["startupTime"].as_str().unwrap() != "--");
        // The first tick enters the first stage: the slew.
        let r = tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Slewing);
        let w = writes(&r);
        assert!(w.contains(&"Mount.MOUNT_ON_COORDINATES_SET.TRACK".to_string()), "{w:?}");
        assert!(w.contains(&"Mount.MOUNT_EQUATORIAL_COORDINATES.RA".to_string()));
        // Tracking on → slew complete → focusing.
        apply(&mut rig.env.store, &[r#"{"setSwitchVector":{"device":"Mount","name":"MOUNT_TRACKING","state":"Ok","items":[{"name":"ON","value":true},{"name":"OFF","value":false}]}}"#]);
        let r = tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Focusing);
        assert_eq!(writes(&r).last().unwrap(), "Imager Agent.AGENT_START_PROCESS.FOCUSING");
        // Focus complete → aligning with CENTER.
        sch.observe(&[msg("new_focus_state", "In Progress"), msg("new_focus_state", "Complete")], Vec::new());
        let r = step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Aligning);
        assert!(writes(&r).contains(&"Astrometry Agent.AGENT_START_PROCESS.CENTER".to_string()), "{:?}", writes(&r));
        // A failed solve is retried.
        sch.observe(&[msg("new_align_state", "Failed")], Vec::new());
        let r = step(&mut sch, &mut rig);
        assert!(writes(&r).contains(&"Astrometry Agent.AGENT_START_PROCESS.CENTER".to_string()));
        sch.observe(&[msg("new_align_state", "Successful")], Vec::new());
        step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Reslewing);
        // Two ticks tracking → the post-align refocus (the .esq has in-sequence
        // focus limits) → guiding.
        tick(&mut sch, &mut rig);
        let r = tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::PostAlignFocusing);
        assert_eq!(writes(&r).last().unwrap(), "Imager Agent.AGENT_START_PROCESS.FOCUSING");
        sch.observe(&[msg("new_focus_state", "Complete")], Vec::new());
        let r = step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Guiding);
        assert!(writes(&r).iter().any(|w| w.starts_with("Guider Agent.AGENT_START_PROCESS")), "{:?}", writes(&r));
        // Calibration error → retry after 5 s.
        sch.observe(&[msg("new_guide_state", "Calibrating"), msg("new_guide_state", "Calibration error")], Vec::new());
        step(&mut sch, &mut rig);
        assert!(tick(&mut sch, &mut rig).to_indigo.is_empty());
        rig.advance(6);
        let r = tick(&mut sch, &mut rig);
        assert!(writes(&r).iter().any(|w| w.starts_with("Guider Agent.AGENT_START_PROCESS")), "{:?} // {}", writes(&r), sch.log.text());
        sch.observe(&[msg("new_guide_state", "Guiding")], Vec::new());
        let r = step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Capturing);
        // The capture queue got the job's .esq, targeted at the job name.
        assert_eq!(rig.mods.capture.queue.jobs.len(), 2);
        assert_eq!(rig.mods.capture.queue.jobs[0].target, "Vega");
        assert!(rig.mods.capture.is_running());
        assert!(writes(&r).iter().any(|w| w.starts_with("Imager Agent.")), "{:?}", writes(&r));
        // Frames and completion.
        sch.observe(&[], vec![CaptureEvent::ImageReceived, CaptureEvent::ImageReceived]);
        let r = step(&mut sch, &mut rig);
        assert_eq!(last_jobs(&r).unwrap()[0]["completedCount"], 2);
        let ev = sequence_completes(&mut rig);
        sch.observe(&[], ev);
        let r = step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].state, JobState::Complete);
        assert!(writes(&r).contains(&"Guider Agent.AGENT_ABORT_PROCESS.ABORT".to_string()), "guiding stopped at the end");
        assert_eq!(sch.status(), STATUS_IDLE);
        assert_eq!(statuses(&r), [STATUS_IDLE]);
        let log = sch.log.text();
        assert!(log.starts_with("No jobs left"), "{log}");
    }

    #[test]
    fn constraints_sleep_stop_and_abort_after_failures() {
        let mut rig = test_rig();
        let seq = esq_file();
        let mut sch = Scheduler::default();
        let mut r = SchedReaction::default();
        // A target below the horizon now (RA 6h at 22:00 in October is near the horizon… use Dec −60: never up at 48.5 N).
        let mut form = vega_form(&seq);
        form.insert("nameEdit".into(), json!("Never"));
        form.insert("decBox".into(), json!("-60.0"));
        sch.apply_settings(&form);
        add(&mut sch, &rig, &mut r);
        // One that rises later tonight: RA 6h Dec +20 (Orion's shoulder region) is up at 30° around 1–2 am local in October.
        let mut form = vega_form(&seq);
        form.insert("nameEdit".into(), json!("Later"));
        form.insert("raBox".into(), json!("6.0"));
        form.insert("decBox".into(), json!("20.0"));
        form.insert("schedulerFocusStep".into(), json!(false));
        form.insert("schedulerAlignStep".into(), json!(false));
        form.insert("schedulerGuideStep".into(), json!(false));
        sch.apply_settings(&form);
        add(&mut sch, &rig, &mut r);
        let r = start(&mut sch, &mut rig);
        assert_eq!(sch.status(), STATUS_RUNNING);
        let jobs = last_jobs(&r).unwrap();
        assert_eq!(jobs[0]["state"], 2);
        assert_eq!(jobs[0]["startupTime"], "--");
        assert_eq!(jobs[1]["state"], 2);
        assert!(jobs[1]["startupTime"].as_str().unwrap() != "--");
        assert!(sch.log.text().starts_with("Sleeping until"), "{}", sch.log.text());
        // Jump to the start time: the job runs (track only → slew, then capture).
        let start_at = sch.jobs[1].startup_time.unwrap();
        let secs = (start_at - rig.env.local).num_seconds().max(0) as u64 + 30;
        rig.advance(secs);
        let r = tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[1].state, JobState::Busy, "{:?}", sch.log.text());
        assert!(last_jobs(&r).is_some());
        tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[1].stage, Stage::Slewing);
        // The mount stays idle: the slew is re-issued after 10 s, parked → aborted.
        rig.advance(11);
        let r = tick(&mut sch, &mut rig);
        assert!(writes(&r).contains(&"Mount.MOUNT_ON_COORDINATES_SET.TRACK".to_string()));
        apply(&mut rig.env.store, &[r#"{"setSwitchVector":{"device":"Mount","name":"MOUNT_TRACKING","state":"Ok","items":[{"name":"ON","value":true},{"name":"OFF","value":false}]}}"#]);
        tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[1].stage, Stage::Capturing);
        assert!(rig.mods.capture.is_running(), "{}", sch.log.text());
        // Capture aborts on its own five times → the job is aborted.
        for _ in 0..4 {
            let ev = batch_ends(&mut rig, false);
            assert!(matches!(ev.last(), Some(CaptureEvent::Aborted(_))), "{ev:?}");
            sch.observe(&[], ev);
            let r = step(&mut sch, &mut rig);
            assert_eq!(sch.jobs[1].state, JobState::Busy, "{}", sch.log.text());
            assert!(writes(&r).iter().any(|w| w.starts_with("Imager Agent.")), "restarted: {:?}", writes(&r));
        }
        let ev = batch_ends(&mut rig, false);
        sch.observe(&[], ev);
        step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[1].state, JobState::Aborted);
        assert!(sch.jobs[1].last_abort_reason.contains("5 times"));
        // Nothing else runnable: back to Idle (no shutdown procedure).
        assert_eq!(sch.status(), STATUS_IDLE);

        // Stop while running marks the jobs aborted (a fresh evening: Vega up).
        let mut rig = test_rig();
        let mut sch = Scheduler::default();
        let mut r = SchedReaction::default();
        sch.apply_settings(&vega_form(&seq));
        add(&mut sch, &rig, &mut r);
        start(&mut sch, &mut rig);
        tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Slewing);
        let r = stop(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].state, JobState::Aborted);
        assert!(writes(&r).contains(&"Mount.MOUNT_ABORT_MOTION.ABORT_MOTION".to_string()));
        assert_eq!(statuses(&r), [STATUS_IDLE]);
        // Toggle semantics: start again resets the job.
        start(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].state, JobState::Busy);
    }

    #[test]
    fn constraint_violation_releases_the_job_and_at_completes_it() {
        let mut rig = test_rig();
        let seq = esq_file();
        let mut sch = Scheduler::default();
        let mut r = SchedReaction::default();
        let mut form = vega_form(&seq);
        form.insert("schedulerTrackStep".into(), json!(false));
        form.insert("schedulerFocusStep".into(), json!(false));
        form.insert("schedulerAlignStep".into(), json!(false));
        form.insert("schedulerGuideStep".into(), json!(false));
        form.insert("schedulerAltitudeValue".into(), json!(45.0));
        sch.apply_settings(&form);
        add(&mut sch, &rig, &mut r);
        start(&mut sch, &mut rig);
        tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Capturing, "{}", sch.log.text());
        assert!(rig.mods.capture.is_running(), "{} // {}", sch.log.text(), rig.mods.capture.sequences_msg());
        // Three hours later Vega is below 45°: the capture is stopped, the job
        // goes back to the pool and is rescheduled for the next evening.
        rig.advance(3 * 3600);
        let r = tick(&mut sch, &mut rig);
        assert!(writes(&r).contains(&"Imager Agent.AGENT_ABORT_PROCESS.ABORT".to_string()), "{:?}", writes(&r));
        let ev = batch_ends(&mut rig, false);
        sch.observe(&[], ev);
        step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].state, JobState::Scheduled);
        assert!(sch.log.text().contains("will be rescheduled"), "{}", sch.log.text());
        assert!(sch.jobs[0].startup_time.unwrap() > rig.env.local);

        // Finish at a time.
        let mut sch = Scheduler::default();
        let mut rig = test_rig();
        let mut r = SchedReaction::default();
        let mut form = vega_form(&seq);
        for k in ["schedulerTrackStep", "schedulerFocusStep", "schedulerAlignStep", "schedulerGuideStep", "schedulerCompleteSequences"] {
            form.insert(k.into(), json!(false));
        }
        form.insert("schedulerUntil".into(), json!(true));
        form.insert("schedulerUntilValue".into(), json!("2025-10-15T22:10"));
        sch.apply_settings(&form);
        add(&mut sch, &rig, &mut r);
        assert!(matches!(sch.jobs[0].completion, Completion::At(_)));
        start(&mut sch, &mut rig);
        tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Capturing);
        // A sequence end before the time repeats.
        let ev = sequence_completes(&mut rig);
        sch.observe(&[], ev);
        step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].state, JobState::Busy);
        assert!(rig.mods.capture.is_running(), "{}", sch.log.text());
        rig.advance(11 * 60);
        tick(&mut sch, &mut rig);
        let ev = batch_ends(&mut rig, false);
        sch.observe(&[], ev);
        step(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].state, JobState::Complete);
        assert_eq!(sch.status(), STATUS_IDLE);
    }

    #[test]
    fn procedures_run_around_the_jobs_and_files_round_trip() {
        let mut rig = test_rig();
        let seq = esq_file();
        let dir = seq.parent().unwrap().to_path_buf();
        let queue_path = dir.join("startup.json");
        std::fs::write(&queue_path, r#"{"name":"startup","description":"","version":"1","tasks":[{"template_id":"mount_unpark","device":"","failure_action":0,"parameters":{"wait_timeout":60}},{"template_id":"delay","device":"","failure_action":0,"parameters":{"delay_seconds":1}}]}"#).unwrap();
        let mut sch = Scheduler::default();
        let mut r = SchedReaction::default();
        sch.apply_settings(&json!({ "schedulerStartupEnabled": true, "schedulerPreStartupScript": queue_path.to_string_lossy() }).as_object().unwrap().clone());
        sch.apply_settings(&json!({ "schedulerShutdownEnabled": true, "schedulerPostShutdownScript": queue_path.to_string_lossy() }).as_object().unwrap().clone());
        assert_eq!(sch.settings.payload()["schedulerStartupEnabled"], true);
        let mut form = vega_form(&seq);
        for k in ["schedulerTrackStep", "schedulerFocusStep", "schedulerAlignStep", "schedulerGuideStep"] {
            form.insert(k.into(), json!(false));
        }
        sch.apply_settings(&form);
        add(&mut sch, &rig, &mut r);
        // The mount is parked: the queue unparks it.
        apply(&mut rig.env.store, &[r#"{"setSwitchVector":{"device":"Mount","name":"MOUNT_PARK","state":"Ok","items":[{"name":"PARKED","value":true},{"name":"UNPARKED","value":false}]}}"#]);
        let r = start(&mut sch, &mut rig);
        assert_eq!(statuses(&r), [STATUS_STARTUP]);
        // Tick 1 loads the queue, tick 2 writes the unpark.
        tick(&mut sch, &mut rig);
        let r = tick(&mut sch, &mut rig);
        let mut r2 = SchedReaction::default();
        assert_eq!(writes(&r), ["Mount.MOUNT_PARK.UNPARKED"]);
        apply(&mut rig.env.store, &[r#"{"setSwitchVector":{"device":"Mount","name":"MOUNT_PARK","state":"Ok","items":[{"name":"PARKED","value":false},{"name":"UNPARKED","value":true}]}}"#]);
        for _ in 0..6 {
            rig.advance(1);
            tick(&mut sch, &mut rig);
            if sch.status() == STATUS_RUNNING {
                break;
            }
        }
        assert_eq!(sch.status(), STATUS_RUNNING, "{}", sch.log.text());
        assert_eq!(sch.jobs[0].state, JobState::Busy);
        tick(&mut sch, &mut rig);
        assert_eq!(sch.jobs[0].stage, Stage::Capturing);
        let ev = sequence_completes(&mut rig);
        sch.observe(&[], ev);
        step(&mut sch, &mut rig);
        assert_eq!(sch.status(), STATUS_SHUTDOWN);
        for _ in 0..8 {
            rig.advance(1);
            tick(&mut sch, &mut rig);
            if sch.status() == STATUS_IDLE {
                break;
            }
        }
        assert_eq!(sch.status(), STATUS_IDLE, "{}", sch.log.text());
        assert!(sch.log.text().starts_with("Shutdown complete"));

        // Save and load the schedule.
        let esl = dir.join("night.esl");
        let text = sch.save_file(&esl, "INDIGO", &mut r2).unwrap();
        assert!(text.contains("<Name>Vega</Name>"));
        assert!(text.contains("<PreStartupQueue>"));
        let mut other = Scheduler::default();
        other.load_file(&esl, &ctx(&rig.env), &mut drivers(&mut rig.mods), &mut r2).unwrap();
        assert_eq!(other.jobs.len(), 1);
        assert_eq!(other.jobs[0].name, "Vega");
        assert_eq!(other.jobs[0].sequence_count, 12);
        assert!(other.settings.startup_enabled);
        assert_eq!(other.settings.pre_startup, queue_path.to_string_lossy());
        assert!(other.load_file(Path::new("/nonexistent.esl"), &ctx(&rig.env), &mut drivers(&mut rig.mods), &mut r2).is_err());
        // Remove.
        other.remove_job(0, &ctx(&rig.env), &mut drivers(&mut rig.mods), &mut r2).unwrap();
        assert!(other.jobs.is_empty());
        assert!(other.remove_job(0, &ctx(&rig.env), &mut drivers(&mut rig.mods), &mut r2).is_err());
        // A missing .esq makes the job invalid.
        let mut bad = vega_form(Path::new("/nonexistent.esq"));
        bad.insert("nameEdit".into(), json!("Bad"));
        other.apply_settings(&bad);
        add(&mut other, &rig, &mut r2);
        assert_eq!(other.jobs[0].state, JobState::Invalid);
        let r = start(&mut other, &mut rig);
        assert_eq!(other.status(), STATUS_IDLE);
        assert!(statuses(&r).is_empty(), "never left Idle");
        assert!(other.log.text().starts_with("No executable jobs"));
    }
}
