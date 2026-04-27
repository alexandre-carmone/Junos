//! Ekos Scheduler tab — view the job queue, start/stop the scheduler,
//! remove jobs, and add new jobs with a visual sequence builder.
//!
//! Inbound:  `new_scheduler_state`, `scheduler_get_jobs`,
//!           `scheduler_get_all_settings`
//! Outbound: `scheduler_start_job`, `scheduler_remove_jobs`,
//!           `scheduler_set_all_settings` + `scheduler_add_jobs`,
//!           `scheduler_save_sequence_file`

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::compat::SchedulerSnapshot;
use crate::dso_catalog::DsoCatalogData;
use crate::i18n::{Lang, Translations, t};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;
use crate::SchedulerPrefillCtx;

const SCHED_CSS: &str = r#"
:root {
    --sched-blue:   #88aaff;
    --sched-green:  #44ee88;
    --sched-red:    #ee4444;
    --sched-bg:     #0a0a0f;
    --sched-border: #222;
}
.sched-root {
    position: absolute; inset: 0; background: var(--sched-bg); color: #c0c0d0;
    font-family: monospace; display: flex; flex-direction: column;
    overflow: hidden;
}
/* ── Header ────────────────────────────────────────────────────────────── */
.sched-header {
    display: flex; flex-direction: column;
    padding: 8px 20px 8px 80px;
    border-bottom: 1px solid var(--sched-border); background: rgba(6,6,15,0.9);
    flex-shrink: 0;
}
.sched-header-top {
    display: flex; flex-wrap: wrap; align-items: center;
    gap: 8px 14px; font-size: 13px; min-height: 36px;
}
.sched-title {
    color: var(--sched-blue); font-weight: 700; font-size: 13px;
    letter-spacing: 0.08em;
}
.sched-job-count {
    color: #556; font-size: 11px; font-weight: 400;
}
.sched-ctrl-group {
    display: flex; align-items: center; gap: 8px;
}
.sched-log-row {
    color: #556; font-size: 11px; font-style: italic;
    padding-top: 3px; min-height: 16px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
/* ── Status badges ─────────────────────────────────────────────────────── */
.sched-badge {
    display: inline-block; padding: 3px 10px; border-radius: 14px;
    font-size: 11px; font-weight: 600; letter-spacing: 0.06em;
}
.sched-badge-idle    { background: #1a1a2a; color: var(--sched-blue);  border: 1px solid #3a3a5a; }
.sched-badge-running { background: #0a2a1a; color: var(--sched-green); border: 1px solid #1a5a3a; }
.sched-badge-paused  { background: #2a2a0a; color: #ffcc44;            border: 1px solid #5a5a1a; }
/* ── Buttons ───────────────────────────────────────────────────────────── */
.sched-btn {
    padding: 4px 14px; border-radius: 6px; font-family: monospace;
    font-size: 11px; font-weight: 600; cursor: pointer;
    touch-action: manipulation; -webkit-tap-highlight-color: transparent;
    border: 1px solid #3a5a3a; background: #0a1a0a; color: var(--sched-green);
    transition: background 0.15s;
}
.sched-btn:hover { background: #0f2a0f; }
.sched-btn-stop  { border-color: #5a2a2a; background: #1a0a0a; color: var(--sched-red); }
.sched-btn-stop:hover { background: #2a0a0a; }
.sched-btn-apply {
    border: 1px solid #3a4a6a; background: #0a0f1a; color: var(--sched-blue);
    padding: 4px 12px; border-radius: 6px; font-family: monospace;
    font-size: 11px; font-weight: 600; cursor: pointer;
    touch-action: manipulation;
}
.sched-btn-apply:hover { background: #0f1a2a; }
.sched-btn-clear {
    border: 1px solid #3a2a2a; background: #120808; color: #aa6666;
    padding: 4px 12px; border-radius: 6px; font-family: monospace;
    font-size: 11px; font-weight: 600; cursor: pointer;
    touch-action: manipulation;
}
.sched-btn-clear:hover { background: #1e0a0a; }
.sched-btn-icon {
    background: none; border: 1px solid #2a2a4a; border-radius: 5px;
    color: var(--sched-blue); font-size: 12px; cursor: pointer; padding: 3px 8px;
    font-family: monospace; touch-action: manipulation;
}
.sched-btn-icon:hover { background: rgba(136,170,255,0.1); }
/* ── Body ──────────────────────────────────────────────────────────────── */
.sched-body {
    flex: 1; overflow-y: auto; -webkit-overflow-scrolling: touch;
    padding: 0 0 20px;
}
/* ── Section title bar ─────────────────────────────────────────────────── */
.sched-section-bar {
    display: flex; align-items: center; justify-content: space-between;
    padding: 8px 20px 4px;
    border-bottom: 1px solid #161620;
}
.sched-section-label {
    color: #445; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.07em; font-weight: 600;
}
/* ── Job table ─────────────────────────────────────────────────────────── */
.sched-table-wrap { overflow-x: auto; padding: 0 16px 12px; }
.sched-table {
    width: 100%; border-collapse: collapse; font-size: 12px;
    min-width: 520px;
}
.sched-table th {
    color: var(--sched-blue); font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.06em; padding: 8px 8px 4px;
    border-bottom: 1px solid #222; text-align: left; white-space: nowrap;
}
.sched-table td {
    padding: 6px 8px; border-bottom: 1px solid #111;
    vertical-align: middle;
}
.sched-table tr:hover td { background: rgba(136,170,255,0.04); }
/* State labels */
.sched-state-active   { color: var(--sched-green); }
.sched-state-error    { color: var(--sched-red); }
.sched-state-aborted  { color: #ffaa44; }
.sched-state-complete { color: #6688aa; }
.sched-state-queued   { color: #c0c0d0; }
.sched-stage-sub {
    display: block; color: #4a6a4a; font-size: 10px; margin-top: 1px;
}
/* Altitude cells */
.sched-alt-good { color: var(--sched-green); }
.sched-alt-warn { color: #ffaa44; }
.sched-alt-bad  { color: #884444; }
/* Progress bar */
.sched-progress-wrap {
    position: relative; height: 16px; background: #0f0f1a;
    border-radius: 3px; overflow: hidden; min-width: 60px;
}
.sched-progress-bar {
    position: absolute; inset: 0; background: rgba(68,238,136,0.25);
    transition: width 0.4s;
}
.sched-progress-text {
    position: relative; font-size: 10px; line-height: 16px;
    padding: 0 4px; color: #aaa; white-space: nowrap;
}
/* Remove button */
.sched-remove-btn {
    background: none; border: none; color: #664444; cursor: pointer;
    font-size: 13px; padding: 3px 8px; border-radius: 4px;
    touch-action: manipulation; display: block; width: 100%; text-align: center;
}
.sched-remove-btn:hover { background: rgba(200,50,50,0.15); color: var(--sched-red); }
.sched-empty { color: #444; font-size: 12px; padding: 24px 20px; }
/* ── Collapsible panels ─────────────────────────────────────────────────── */
.sched-add-section { padding: 0 16px 10px; }
.sched-add-details { border: 1px solid #1e1e30; border-radius: 8px; overflow: hidden; }
.sched-add-summary {
    padding: 9px 14px; font-size: 11px; color: var(--sched-blue); cursor: pointer;
    user-select: none; background: rgba(10,10,20,0.6);
    letter-spacing: 0.05em; text-transform: uppercase; font-weight: 600;
    list-style: none; display: flex; align-items: center; gap: 8px;
}
.sched-add-summary:hover { background: rgba(20,20,40,0.8); }
.sched-add-body {
    padding: 14px; display: flex; flex-direction: column; gap: 12px;
    background: rgba(6,6,15,0.5);
}
/* ── Form fields ────────────────────────────────────────────────────────── */
.sched-field-row {
    display: flex; flex-wrap: wrap; gap: 8px; align-items: center;
}
.sched-field-label {
    color: var(--sched-blue); font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.05em; min-width: 60px;
}
.sched-field-unit { color: #445; font-size: 11px; }
.sched-input {
    background: #0d0d18; border: 1px solid #2a2a4a; border-radius: 4px;
    color: #c0c0d0; font-family: monospace; font-size: 12px;
    padding: 5px 8px; outline: none;
}
.sched-input:focus { border-color: #4466aa; }
.sched-select {
    background: #0d0d18; border: 1px solid #2a2a4a; border-radius: 4px;
    color: #c0c0d0; font-family: monospace; font-size: 12px;
    padding: 5px 6px; outline: none; cursor: pointer;
}
.sched-select:focus { border-color: #4466aa; }
.sched-coords-hint {
    color: #4a5a6a; font-size: 10px; font-style: italic; padding: 2px 0 0 4px;
}
.sched-search-row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
.sched-search-result {
    color: var(--sched-green); font-size: 11px; padding: 3px 8px;
    background: #0a1a0a; border-radius: 4px; border: 1px solid #1a3a1a;
}
.sched-fieldset {
    border: 1px solid #1e1e30; border-radius: 6px;
    padding: 8px 12px 10px; margin: 0;
}
.sched-fieldset legend {
    color: #667; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.05em; padding: 0 6px;
}
.sched-toggle-label {
    display: flex; align-items: center; gap: 5px; cursor: pointer;
    color: #aab; user-select: none; font-size: 11px;
}
.sched-toggle-label input[type=checkbox] { accent-color: var(--sched-blue); cursor: pointer; }
/* ── Sequence builder ───────────────────────────────────────────────────── */
.sched-seq-section { display: flex; flex-direction: column; gap: 8px; }
.sched-seq-label {
    color: var(--sched-blue); font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.05em;
}
.sched-seq-table { border-collapse: collapse; font-size: 12px; width: 100%; }
.sched-seq-table th {
    color: #556; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.05em; padding: 4px 6px; text-align: left;
    border-bottom: 1px solid #1a1a2a;
}
.sched-seq-table td { padding: 4px 4px; }
.sched-seq-row-num { color: #334; font-size: 10px; text-align: center; }
.sched-seq-total {
    font-size: 10px; color: #4a5a6a; padding: 4px 6px;
    border-top: 1px solid #1a1a2a; text-align: right;
}
.sched-seq-add-btn {
    background: none; border: 1px dashed #3a3a5a; border-radius: 4px;
    color: var(--sched-blue); font-family: monospace; font-size: 11px;
    cursor: pointer; padding: 4px 12px; margin-top: 4px;
    touch-action: manipulation;
}
.sched-seq-add-btn:hover { background: rgba(136,170,255,0.08); }
/* ── Form footer ────────────────────────────────────────────────────────── */
.sched-form-error { color: var(--sched-red); font-size: 11px; }
.sched-form-btns  { display: flex; gap: 10px; align-items: center; }
.sched-add-btn {
    padding: 6px 20px; border-radius: 6px; font-family: monospace;
    font-size: 12px; font-weight: 600; cursor: pointer; touch-action: manipulation;
    border: 1px solid #3a5a3a; background: #0a1a0a; color: var(--sched-green);
}
.sched-add-btn:hover { background: #0f2a0f; }
/* ── Mobile (≤ 767 px) ──────────────────────────────────────────────── */
@media (max-width: 767px) {
    .sched-header {
        padding: 8px 16px 8px 16px;
    }
    .sched-table {
        min-width: 360px;
        font-size: 11px;
    }
    .sched-table th,
    .sched-table td {
        padding: 4px 5px;
    }
    .sched-col-start,
    .sched-col-end { display: none; }
    .sched-btn {
        padding: 6px 10px;
    }
    .sched-section-bar {
        padding: 8px 12px 4px;
    }
}
"#;

fn scheduler_status_label(tr: &'static Translations, status: i64) -> (&'static str, &'static str) {
    match status {
        0 => (tr.sched_status_idle,    "sched-badge-idle"),
        1 => (tr.sched_status_running, "sched-badge-running"),
        2 => (tr.sched_status_paused,  "sched-badge-paused"),
        _ => (tr.sched_status_unknown, "sched-badge-idle"),
    }
}

fn job_state_label(tr: &'static Translations, state: i64) -> (&'static str, &'static str) {
    match state {
        0 => (tr.sched_state_idle,       "sched-state-queued"),
        1 => (tr.sched_state_evaluating, "sched-state-queued"),
        2 => (tr.sched_state_scheduled,  "sched-state-queued"),
        3 => (tr.sched_state_active,     "sched-state-active"),
        4 => (tr.sched_state_error,      "sched-state-error"),
        5 => (tr.sched_state_aborted,    "sched-state-aborted"),
        6 => (tr.sched_state_invalid,    "sched-state-error"),
        7 => (tr.sched_state_complete,   "sched-state-complete"),
        _ => ("?",                       "sched-state-queued"),
    }
}

fn job_stage_label(tr: &'static Translations, stage: i64) -> &'static str {
    match stage {
        1  => tr.sched_stage_slewing,
        2  => tr.sched_stage_slew_done,
        3  => tr.sched_stage_focusing,
        4  => tr.sched_stage_focus_done,
        5  => tr.sched_stage_aligning,
        6  => tr.sched_stage_align_done,
        7  => tr.sched_stage_reslewing,
        8  => tr.sched_stage_reslew_done,
        9  => tr.sched_stage_post_focus,
        10 => tr.sched_stage_post_focus_done,
        11 => tr.sched_stage_guiding,
        12 => tr.sched_stage_guide_done,
        13 => tr.sched_stage_capturing,
        14 => tr.sched_stage_done,
        _  => "",
    }
}

fn ra_to_hms(ra_h: f64) -> String {
    let h = ra_h.floor() as i64;
    let rem = (ra_h - h as f64) * 60.0;
    let m = rem.floor() as i64;
    let s = ((rem - m as f64) * 60.0).round() as i64;
    format!("{:02}h {:02}m {:02}s", h, m, s)
}

fn dec_to_dms(dec_d: f64) -> String {
    let sign = if dec_d < 0.0 { "−" } else { "+" };
    let abs  = dec_d.abs();
    let d    = abs.floor() as i64;
    let rem  = (abs - d as f64) * 60.0;
    let m    = rem.floor() as i64;
    let s    = ((rem - m as f64) * 60.0).round() as i64;
    format!("{}{:02}° {:02}′ {:02}″", sign, d, m, s)
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Generate a minimal ESQ XML from a list of sequence frames.
pub(crate) fn build_esq_xml(job_name: &str, frames: &[SeqFrame]) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<SequenceQueue version='2.1'>\n");
    xml.push_str("<GuideDeviation enabled='false'>0</GuideDeviation>\n");
    xml.push_str("<GuideStartDeviation enabled='false'>0</GuideStartDeviation>\n");
    xml.push_str("<HFRCheck enabled='false'><HFRDeviation>0.1</HFRDeviation>\
<HFRCheckAlgorithm>0</HFRCheckAlgorithm><HFRCheckThreshold>0</HFRCheckThreshold>\
<HFRCheckFrames>1</HFRCheckFrames></HFRCheck>\n");
    xml.push_str("<RefocusOnTemperatureDelta enabled='false'>1</RefocusOnTemperatureDelta>\n");
    xml.push_str("<RefocusEveryN enabled='false'>60</RefocusEveryN>\n");
    xml.push_str("<RefocusOnMeridianFlip enabled='false'/>\n");
    for f in frames {
        xml.push_str("<Job>\n");
        xml.push_str(&format!("<Exposure>{}</Exposure>\n", f.exposure));
        xml.push_str("<Format>FITS</Format>\n<Encoding>FITS</Encoding>\n");
        xml.push_str("<Binning><X>1</X><Y>1</Y></Binning>\n");
        xml.push_str("<Frame><X>0</X><Y>0</Y><W>0</W><H>0</H></Frame>\n");
        if !f.filter.is_empty() {
            xml.push_str(&format!("<Filter>{}</Filter>\n", f.filter));
        }
        xml.push_str(&format!("<Type>{}</Type>\n", f.frame_type));
        xml.push_str(&format!("<Count>{}</Count>\n", f.count));
        xml.push_str("<Delay>0</Delay>\n");
        if !job_name.is_empty() {
            xml.push_str(&format!("<TargetName>{}</TargetName>\n", job_name));
        }
        xml.push_str("<GuideDitherPerJob>-1</GuideDitherPerJob>\n");
        xml.push_str("<FITSDirectory></FITSDirectory>\n");
        xml.push_str("<PlaceholderFormat>/%T/%F/Light/%T_%F_%e_secs_%04d</PlaceholderFormat>\n");
        xml.push_str("<PlaceholderSuffix>0</PlaceholderSuffix>\n");
        xml.push_str("<UploadMode>0</UploadMode>\n");
        xml.push_str("<Properties/>\n");
        xml.push_str("<Calibration><FlatSource><Type>Manual</Type></FlatSource>\
<FlatDuration><Type>ADU</Type><Value>0</Value><Tolerance>0</Tolerance></FlatDuration>\
<PreMountPark>false</PreMountPark><PreDomePark>false</PreDomePark></Calibration>\n");
        xml.push_str("</Job>\n");
    }
    xml.push_str("</SequenceQueue>\n");
    xml
}

/// One row in the sequence builder.
#[derive(Clone)]
pub(crate) struct SeqFrame {
    pub(crate) frame_type: String,
    pub(crate) filter:     String,
    pub(crate) exposure:   String,
    pub(crate) count:      String,
}

impl Default for SeqFrame {
    fn default() -> Self {
        Self {
            frame_type: "Light".into(),
            filter:     String::new(),
            exposure:   "120".into(),
            count:      "10".into(),
        }
    }
}

#[component]
pub fn SchedulerTab(
    #[prop(into)] scheduler: Signal<SchedulerSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let dso_catalog = use_context::<RwSignal<Option<std::sync::Arc<DsoCatalogData>>>>();

    // ── Observatory scripts ─────────────────────────────────────────────────
    let startup_enabled  = RwSignal::new(false);
    let pre_startup      = RwSignal::new(String::new());
    let post_startup     = RwSignal::new(String::new());
    let shutdown_enabled = RwSignal::new(false);
    let pre_shutdown     = RwSignal::new(String::new());
    let post_shutdown    = RwSignal::new(String::new());

    // ── Global scheduler settings ───────────────────────────────────────────
    let greedy         = RwSignal::new(false);
    let remember_prog  = RwSignal::new(true);
    let reschedule_err = RwSignal::new(false);

    // Pre-populate from KStars settings once they arrive
    let settings_populated = RwSignal::new(false);
    Effect::new(move |_| {
        if settings_populated.get_untracked() { return; }
        let s = scheduler.get().settings;
        if !s.is_object() { return; }
        if let Some(v) = s["schedulerStartupEnabled"].as_bool()           { startup_enabled.set(v); }
        if let Some(v) = s["schedulerPreStartupScript"].as_str()          { pre_startup.set(v.to_string()); }
        if let Some(v) = s["schedulerPostStartupScript"].as_str()         { post_startup.set(v.to_string()); }
        if let Some(v) = s["schedulerShutdownEnabled"].as_bool()          { shutdown_enabled.set(v); }
        if let Some(v) = s["schedulerPreShutdownScript"].as_str()         { pre_shutdown.set(v.to_string()); }
        if let Some(v) = s["schedulerPostShutdownScript"].as_str()        { post_shutdown.set(v.to_string()); }
        if let Some(v) = s["kcfg_GreedyScheduling"].as_bool()            { greedy.set(v); }
        if let Some(v) = s["kcfg_RememberJobProgress"].as_bool()         { remember_prog.set(v); }
        if let Some(v) = s["errorHandlingRescheduleErrorsCB"].as_bool()   { reschedule_err.set(v); }
        settings_populated.set(true);
    });

    // ── Add-job form state ──────────────────────────────────────────────────
    let f_target_name = RwSignal::new(String::new());
    let f_ra_h        = RwSignal::new(String::new());
    let f_dec_deg     = RwSignal::new(String::new());
    let f_min_alt     = RwSignal::new("30".to_string());
    let f_min_moon    = RwSignal::new("0".to_string());
    let f_pa          = RwSignal::new("0".to_string());
    let search_result = RwSignal::new(Option::<String>::None);
    let form_error    = RwSignal::new(Option::<String>::None);

    // Pre-fill from sky right-click "Add to Scheduler" action.
    let prefill_ctx = use_context::<SchedulerPrefillCtx>();
    Effect::new(move |_| {
        let Some(pctx) = prefill_ctx else { return };
        let Some((name, ra_deg, dec_deg)) = pctx.0.get() else { return };
        let ra_h = ra_deg / 15.0;
        f_target_name.set(name);
        f_ra_h.set(format!("{:.6}", ra_h));
        f_dec_deg.set(format!("{:.6}", dec_deg));
        pctx.0.set(None);  // consume
    });

    // Derived HMS/DMS hint shown after RA/Dec are populated
    let coords_hint = Signal::derive(move || {
        let ra  = f_ra_h.get().parse::<f64>().ok()?;
        let dec = f_dec_deg.get().parse::<f64>().ok()?;
        if ra < 0.0 || ra > 24.0 { return None; }
        if dec < -90.0 || dec > 90.0 { return None; }
        Some(format!("{} / {}", ra_to_hms(ra), dec_to_dms(dec)))
    });

    // Per-job step pipeline (default: Track + Guide)
    let step_track = RwSignal::new(true);
    let step_focus = RwSignal::new(false);
    let step_align = RwSignal::new(false);
    let step_guide = RwSignal::new(true);

    // Per-job startup condition: "asap" | "at"
    let startup_cond = RwSignal::new("asap".to_string());
    let startup_at   = RwSignal::new(String::new());

    // Per-job completion condition: "sequence" | "repeat" | "loop" | "at"
    let completion_cond  = RwSignal::new("sequence".to_string());
    let completion_count = RwSignal::new("1".to_string());
    let completion_at    = RwSignal::new(String::new());

    // Sequence frames — start with one default Light row
    let seq_frames: RwSignal<Vec<SeqFrame>> = RwSignal::new(vec![SeqFrame::default()]);

    // Derived: total exposure summary
    let seq_total_hint = Signal::derive(move || {
        let frames = seq_frames.get();
        let total_secs: f64 = frames.iter()
            .filter_map(|f| {
                let exp  = f.exposure.parse::<f64>().ok()?;
                let cnt  = f.count.parse::<f64>().ok()?;
                Some(exp * cnt)
            })
            .sum();
        if total_secs <= 0.0 { return String::new(); }
        if total_secs < 60.0 {
            format!("Total: {:.0} s", total_secs)
        } else if total_secs < 3600.0 {
            format!("Total: {:.1} min", total_secs / 60.0)
        } else {
            format!("Total: {:.2} h", total_secs / 3600.0)
        }
    });

    // ── Catalog lookup ──────────────────────────────────────────────────────
    let on_catalog_search = {
        let f_target_name2 = f_target_name;
        let f_ra_h2        = f_ra_h;
        let f_dec_deg2     = f_dec_deg;
        let search_result2 = search_result;
        move |_| {
            let name = f_target_name2.get_untracked().to_lowercase();
            let found = dso_catalog.and_then(|sig| {
                sig.get_untracked().and_then(|cat| {
                    cat.dsos.iter().find(|e| e.name.to_lowercase().contains(&name)).map(|e| {
                        let ra_h = e.ra_deg as f64 / 15.0;
                        let dec  = e.dec_deg as f64;
                        (e.name.clone(), ra_h, dec)
                    })
                })
            });
            match found {
                Some((name, ra_h, dec)) => {
                    f_ra_h2.set(format!("{:.4}", ra_h));
                    f_dec_deg2.set(format!("{:.4}", dec));
                    search_result2.set(Some(format!(
                        "{} → RA {:.3}h  Dec {:.2}°", name, ra_h, dec
                    )));
                }
                None => {
                    search_result2.set(Some(t(lang.get_untracked()).sched_not_found.to_string()));
                }
            }
        }
    };

    // ── Apply observatory scripts ───────────────────────────────────────────
    let send_for_scripts = Arc::clone(&send);
    let on_apply_scripts = move |_| {
        send_cmd(&send_for_scripts, "scheduler_set_all_settings", serde_json::json!({
            "schedulerStartupEnabled":    startup_enabled.get_untracked(),
            "schedulerPreStartupScript":  pre_startup.get_untracked(),
            "schedulerPostStartupScript": post_startup.get_untracked(),
            "schedulerShutdownEnabled":   shutdown_enabled.get_untracked(),
            "schedulerPreShutdownScript": pre_shutdown.get_untracked(),
            "schedulerPostShutdownScript":post_shutdown.get_untracked(),
        }));
    };

    // ── Clear form ──────────────────────────────────────────────────────────
    let on_clear_form = move |_| {
        f_target_name.set(String::new());
        f_ra_h.set(String::new());
        f_dec_deg.set(String::new());
        f_min_alt.set("30".to_string());
        f_min_moon.set("0".to_string());
        f_pa.set("0".to_string());
        search_result.set(None);
        form_error.set(None);
        step_track.set(true);
        step_focus.set(false);
        step_align.set(false);
        step_guide.set(true);
        startup_cond.set("asap".to_string());
        startup_at.set(String::new());
        completion_cond.set("sequence".to_string());
        completion_count.set("1".to_string());
        completion_at.set(String::new());
        seq_frames.set(vec![SeqFrame::default()]);
    };

    // ── Submit new job ──────────────────────────────────────────────────────
    let send_for_add = Arc::clone(&send);
    let on_add_job = move |_| {
        let name      = f_target_name.get_untracked();
        let home      = scheduler.get_untracked().home_dir;
        let frames_raw = seq_frames.get_untracked();

        // Validate RA/Dec
        let ra_f = match f_ra_h.get_untracked().parse::<f64>() {
            Ok(v) if (0.0..=24.0).contains(&v) => v,
            _ => {
                form_error.set(Some(t(lang.get_untracked()).sched_err_ra.to_string()));
                return;
            }
        };
        let dec_f = match f_dec_deg.get_untracked().parse::<f64>() {
            Ok(v) if (-90.0..=90.0).contains(&v) => v,
            _ => {
                form_error.set(Some(t(lang.get_untracked()).sched_err_dec.to_string()));
                return;
            }
        };

        let frames: Vec<SeqFrame> = frames_raw.iter().filter(|f| {
            f.exposure.parse::<f64>().is_ok() && f.count.parse::<u32>().is_ok()
        }).cloned().collect();

        if frames.is_empty() {
            form_error.set(Some(t(lang.get_untracked()).sched_err_frames.to_string()));
            return;
        }

        form_error.set(None);

        let xml = build_esq_xml(&name, &frames);
        let safe_name = sanitize_name(if name.is_empty() { "sequence" } else { &name });
        let rel_path  = format!(".rekos-sequences/{}.esq", safe_name);
        let abs_path  = if home.is_empty() {
            format!(".rekos-sequences/{}.esq", safe_name)
        } else {
            format!("{}/.rekos-sequences/{}.esq", home, safe_name)
        };

        // Resolve completion condition fields
        let cond = completion_cond.get_untracked();
        let (seq_r, rep_r, rep_lim, loop_r, until_r, until_val) = match cond.as_str() {
            "repeat" => (false, true,  completion_count.get_untracked().parse::<i64>().unwrap_or(1), false, false, String::new()),
            "loop"   => (false, false, 1,  true,  false, String::new()),
            "at"     => (false, false, 1,  false, true,  completion_at.get_untracked()),
            _        => (true,  false, 1,  false, false, String::new()),
        };

        // Resolve startup condition fields
        let sc = startup_cond.get_untracked();
        let (asap_r, start_time_r, start_time_val) = if sc == "at" {
            (false, true, startup_at.get_untracked())
        } else {
            (true, false, String::new())
        };

        // 1. Save the ESQ file to the KStars machine
        send_cmd(&send_for_add, "scheduler_save_sequence_file",
            serde_json::json!({"path": rel_path, "filedata": xml}));

        // 2. Pre-fill the scheduler form (KStars processes msgs in order)
        send_cmd(&send_for_add, "scheduler_set_all_settings", serde_json::json!({
            "nameEdit":          name,
            "raBox":             format!("{:.6}", ra_f),
            "decBox":            format!("{:.6}", dec_f),
            "sequenceEdit":      abs_path,
            "minAltitude":       f_min_alt.get_untracked().parse::<f64>().unwrap_or(30.0),
            "minMoonSeparation": f_min_moon.get_untracked().parse::<f64>().unwrap_or(0.0),
            "positionAngleSpin": f_pa.get_untracked().parse::<f64>().unwrap_or(0.0),
            "schedulerTrackStep": step_track.get_untracked(),
            "schedulerFocusStep": step_focus.get_untracked(),
            "schedulerAlignStep": step_align.get_untracked(),
            "schedulerGuideStep": step_guide.get_untracked(),
            "asapConditionR":        asap_r,
            "startupTimeConditionR": start_time_r,
            "startupTimeEdit":       start_time_val,
            "schedulerCompleteSequences":    seq_r,
            "schedulerRepeatSequences":      rep_r,
            "schedulerRepeatSequencesLimit": rep_lim,
            "schedulerUntilTerminated":      loop_r,
            "schedulerUntil":                until_r,
            "schedulerUntilValue":           until_val,
        }));

        // 3. Add job
        send_cmd(&send_for_add, "scheduler_add_jobs", serde_json::json!({}));

        // 4. Refresh job list
        let s = Arc::clone(&send_for_add);
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(800).await;
            send_cmd(&s, "scheduler_get_jobs", serde_json::json!({}));
        });
    };

    // ── Start/Stop ──────────────────────────────────────────────────────────
    let send_for_toggle = Arc::clone(&send);
    let on_toggle = move |_| {
        send_cmd(&send_for_toggle, "scheduler_start_job", serde_json::json!({}));
    };

    // Pre-clone for the job list reactive block so `send` isn't moved into it,
    // leaving it available for the settings checkbox on:change handlers below.
    let send_for_jobs = Arc::clone(&send);

    view! {
        <style>{SCHED_CSS}</style>
        <div class="sched-root">

            // ── Header ──────────────────────────────────────────────────────
            <div class="sched-header">
                <div class="sched-header-top">
                    <span class="sched-title">{move || tr().sched_title}</span>
                    <span class="sched-job-count">
                        {move || {
                            let n = scheduler.get().jobs.len();
                            let word = if n == 1 { tr().sched_job_singular } else { tr().sched_job_plural };
                            format!("({} {})", n, word)
                        }}
                    </span>
                    <div class="sched-ctrl-group">
                        {move || {
                            let snap = scheduler.get();
                            let (label, cls) = scheduler_status_label(tr(), snap.status);
                            view! {
                                <span class={format!("sched-badge {}", cls)}>{label}</span>
                            }
                        }}
                        <button
                            class=move || {
                                if scheduler.get().status == 1 { "sched-btn sched-btn-stop" }
                                else { "sched-btn" }
                            }
                            on:click=on_toggle.clone()>
                            {move || if scheduler.get().status == 1 { tr().sched_btn_stop } else { tr().sched_btn_start }}
                        </button>
                    </div>
                </div>
                <div class="sched-log-row">
                    {move || {
                        let log = scheduler.get().log;
                        if log.is_empty() { String::new() } else { log }
                    }}
                </div>
            </div>

            // ── Body ─────────────────────────────────────────────────────────
            <div class="sched-body">

                // ── Job list section ─────────────────────────────────────────
                {move || {
                    let snap = scheduler.get();
                    let job_count = snap.jobs.len();
                    let send_ref = Arc::clone(&send_for_jobs);
                    let send_ref2 = Arc::clone(&send_for_jobs);
                    view! {
                        <div>
                            <div class="sched-section-bar">
                                <span class="sched-section-label">
                                    {format!("{} ({})", tr().sched_jobs_section, job_count)}
                                </span>
                                <button
                                    class="sched-btn-icon"
                                    title=move || tr().sched_refresh_jobs
                                    on:click=move |_| {
                                        send_cmd(&send_ref, "scheduler_get_jobs", serde_json::json!({}));
                                    }>
                                    "↻"
                                </button>
                            </div>
                            <div class="sched-table-wrap">
                                {if snap.jobs.is_empty() {
                                    view! {
                                        <div class="sched-empty">
                                            {move || tr().sched_no_jobs}
                                        </div>
                                    }.into_any()
                                } else {
                                    view! {
                                        <table class="sched-table">
                                            <thead>
                                                <tr>
                                                    <th>{move || tr().sched_col_name}</th>
                                                    <th>{move || tr().sched_col_coords}</th>
                                                    <th>{move || tr().sched_col_state}</th>
                                                    <th>{move || tr().sched_col_alt}</th>
                                                    <th>{move || tr().sched_col_progress}</th>
                                                    <th class="sched-col-start">{move || tr().sched_col_start}</th>
                                                    <th class="sched-col-end">{move || tr().sched_col_end}</th>
                                                    <th></th>
                                                </tr>
                                            </thead>
                                            <tbody>
                                                {snap.jobs.into_iter().enumerate().map(|(i, job)| {
                                                    let name    = job["name"].as_str().unwrap_or("?").to_string();
                                                    let ra_h    = job["targetRA"].as_f64().unwrap_or(0.0);
                                                    let dec_d   = job["targetDEC"].as_f64().unwrap_or(0.0);
                                                    let state   = job["state"].as_i64().unwrap_or(0);
                                                    let stage   = job["stage"].as_i64().unwrap_or(0);
                                                    let alt_val = job["altitude"].as_f64().unwrap_or(0.0);
                                                    let alt_str = job["altitudeFormatted"].as_str()
                                                        .map(|s| s.to_string())
                                                        .unwrap_or_else(|| format!("{:.0}°", alt_val));
                                                    let done    = job["completedCount"].as_i64().unwrap_or(0);
                                                    let total   = job["sequenceCount"].as_i64().unwrap_or(0);
                                                    let start_s = job["startupFormatted"].as_str()
                                                        .or_else(|| job["startupTime"].as_str())
                                                        .unwrap_or("—").to_string();
                                                    let end_s   = job["endFormatted"].as_str()
                                                        .or_else(|| job["completionTime"].as_str())
                                                        .unwrap_or("—").to_string();
                                                    let (state_label, state_cls) = job_state_label(tr(), state);
                                                    let stage_label = if state == 3 { job_stage_label(tr(), stage) } else { "" };
                                                    let alt_cls = if alt_val >= 30.0 { "sched-alt-good" }
                                                                  else if alt_val >= 20.0 { "sched-alt-warn" }
                                                                  else { "sched-alt-bad" };
                                                    let pct = if total > 0 {
                                                        (done * 100 / total).min(100)
                                                    } else { 0 };
                                                    let send_rm = Arc::clone(&send_ref2);
                                                    view! {
                                                        <tr>
                                                            <td style="font-weight:600;">{name}</td>
                                                            <td style="color:#8899aa; font-size:11px;">
                                                                {format!("{:.2}h {}{:.1}°",
                                                                    ra_h,
                                                                    if dec_d < 0.0 { "" } else { "+" },
                                                                    dec_d)}
                                                            </td>
                                                            <td class={state_cls}>
                                                                {state_label}
                                                                {(!stage_label.is_empty()).then(|| view! {
                                                                    <span class="sched-stage-sub">
                                                                        {"∙ "}{stage_label}
                                                                    </span>
                                                                })}
                                                            </td>
                                                            <td class={alt_cls}>{alt_str}</td>
                                                            <td>
                                                                <div class="sched-progress-wrap">
                                                                    <div class="sched-progress-bar"
                                                                         style={format!("width:{}%", pct)}>
                                                                    </div>
                                                                    <span class="sched-progress-text">
                                                                        {format!("{}/{}", done, total)}
                                                                    </span>
                                                                </div>
                                                            </td>
                                                            <td class="sched-col-start" style="color:#667; font-size:11px;">{start_s}</td>
                                                            <td class="sched-col-end" style="color:#667; font-size:11px;">{end_s}</td>
                                                            <td style="width:36px;">
                                                                <button
                                                                    class="sched-remove-btn"
                                                                    title=move || tr().sched_remove_job
                                                                    on:click=move |_| {
                                                                        send_cmd(&send_rm, "scheduler_remove_jobs",
                                                                            serde_json::json!({"index": i}));
                                                                        let sr = Arc::clone(&send_rm);
                                                                        wasm_bindgen_futures::spawn_local(async move {
                                                                            gloo_timers::future::TimeoutFuture::new(400).await;
                                                                            send_cmd(&sr, "scheduler_get_jobs",
                                                                                serde_json::json!({}));
                                                                        });
                                                                    }>
                                                                    "×"
                                                                </button>
                                                            </td>
                                                        </tr>
                                                    }
                                                }).collect::<Vec<_>>()}
                                            </tbody>
                                        </table>
                                    }.into_any()
                                }}
                            </div>
                        </div>
                    }
                }}

                // ── Settings panel ───────────────────────────────────────────
                <div class="sched-add-section" style="padding-bottom:8px; padding-top:8px;">
                    <details class="sched-add-details">
                        <summary class="sched-add-summary">{move || tr().sched_settings_section}</summary>
                        <div class="sched-add-body">
                            <div style="display:flex; flex-wrap:wrap; gap:14px; align-items:center;">
                                <label class="sched-toggle-label">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || greedy.get()
                                        on:change={
                                            let s = Arc::clone(&send);
                                            move |ev| {
                                                let checked = ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                                greedy.set(checked);
                                                send_cmd(&s, "scheduler_set_all_settings",
                                                    serde_json::json!({"kcfg_GreedyScheduling": checked}));
                                            }
                                        }
                                    />
                                    {move || tr().sched_greedy}
                                </label>
                                <label class="sched-toggle-label">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || remember_prog.get()
                                        on:change={
                                            let s = Arc::clone(&send);
                                            move |ev| {
                                                let checked = ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                                remember_prog.set(checked);
                                                send_cmd(&s, "scheduler_set_all_settings",
                                                    serde_json::json!({"kcfg_RememberJobProgress": checked}));
                                            }
                                        }
                                    />
                                    {move || tr().sched_remember_progress}
                                </label>
                                <label class="sched-toggle-label">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || reschedule_err.get()
                                        on:change={
                                            let s = Arc::clone(&send);
                                            move |ev| {
                                                let checked = ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                                reschedule_err.set(checked);
                                                send_cmd(&s, "scheduler_set_all_settings",
                                                    serde_json::json!({"errorHandlingRescheduleErrorsCB": checked}));
                                            }
                                        }
                                    />
                                    {move || tr().sched_reschedule_error}
                                </label>
                            </div>
                        </div>
                    </details>
                </div>

                // ── Observatory scripts panel ────────────────────────────────
                <div class="sched-add-section" style="padding-bottom:8px;">
                    <details class="sched-add-details">
                        <summary class="sched-add-summary">{move || tr().sched_scripts_section}</summary>
                        <div class="sched-add-body">
                            <fieldset class="sched-fieldset">
                                <legend>{move || tr().sched_startup_legend}</legend>
                                <div class="sched-field-row" style="margin-bottom:8px;">
                                    <label class="sched-toggle-label">
                                        <input
                                            type="checkbox"
                                            prop:checked=move || startup_enabled.get()
                                            on:change=move |ev| {
                                                startup_enabled.set(ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked());
                                            }
                                        />
                                        {move || tr().sched_enable_startup}
                                    </label>
                                </div>
                                <div class="sched-field-row">
                                    <span class="sched-field-label">{move || tr().sched_pre_script}</span>
                                    <input
                                        class="sched-input"
                                        style="flex:1; min-width:200px;"
                                        placeholder="/path/to/pre_startup.sh"
                                        prop:value=move || pre_startup.get()
                                        on:input=move |ev| {
                                            pre_startup.set(ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>().value());
                                        }
                                    />
                                </div>
                                <div class="sched-field-row" style="margin-top:6px;">
                                    <span class="sched-field-label">{move || tr().sched_post_script}</span>
                                    <input
                                        class="sched-input"
                                        style="flex:1; min-width:200px;"
                                        placeholder="/path/to/post_startup.sh"
                                        prop:value=move || post_startup.get()
                                        on:input=move |ev| {
                                            post_startup.set(ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>().value());
                                        }
                                    />
                                </div>
                            </fieldset>

                            <fieldset class="sched-fieldset">
                                <legend>{move || tr().sched_shutdown_legend}</legend>
                                <div class="sched-field-row" style="margin-bottom:8px;">
                                    <label class="sched-toggle-label">
                                        <input
                                            type="checkbox"
                                            prop:checked=move || shutdown_enabled.get()
                                            on:change=move |ev| {
                                                shutdown_enabled.set(ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked());
                                            }
                                        />
                                        {move || tr().sched_enable_shutdown}
                                    </label>
                                </div>
                                <div class="sched-field-row">
                                    <span class="sched-field-label">{move || tr().sched_pre_script}</span>
                                    <input
                                        class="sched-input"
                                        style="flex:1; min-width:200px;"
                                        placeholder="/path/to/pre_shutdown.sh"
                                        prop:value=move || pre_shutdown.get()
                                        on:input=move |ev| {
                                            pre_shutdown.set(ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>().value());
                                        }
                                    />
                                </div>
                                <div class="sched-field-row" style="margin-top:6px;">
                                    <span class="sched-field-label">{move || tr().sched_post_script}</span>
                                    <input
                                        class="sched-input"
                                        style="flex:1; min-width:200px;"
                                        placeholder="/path/to/post_shutdown.sh"
                                        prop:value=move || post_shutdown.get()
                                        on:input=move |ev| {
                                            post_shutdown.set(ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>().value());
                                        }
                                    />
                                </div>
                            </fieldset>

                            <button class="sched-btn-apply" on:click=on_apply_scripts.clone()>
                                {move || tr().sched_apply_scripts}
                            </button>
                        </div>
                    </details>
                </div>

                // ── Add Job form ──────────────────────────────────────────────
                <div class="sched-add-section">
                    <details class="sched-add-details">
                        <summary class="sched-add-summary">{move || tr().sched_add_job_section}</summary>
                        <div class="sched-add-body">

                            // Target name + catalog search
                            <div class="sched-field-row">
                                <span class="sched-field-label">{move || tr().sched_target_label}</span>
                                <div class="sched-search-row">
                                    <input
                                        class="sched-input"
                                        style="width:180px;"
                                        placeholder=move || tr().sched_target_placeholder
                                        prop:value=move || f_target_name.get()
                                        on:input=move |ev| {
                                            f_target_name.set(
                                                ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>()
                                                    .value()
                                            );
                                        }
                                    />
                                    <button
                                        class="sched-btn"
                                        style="padding:4px 10px;"
                                        on:click=on_catalog_search.clone()>
                                        {move || tr().sched_search_catalog}
                                    </button>
                                    {move || search_result.get().map(|r| view! {
                                        <span class="sched-search-result">{r}</span>
                                    })}
                                </div>
                            </div>

                            // RA / Dec
                            <div class="sched-field-row">
                                <span class="sched-field-label">{move || tr().sched_ra_label}</span>
                                <input
                                    class="sched-input"
                                    style="width:90px;"
                                    placeholder="5.5882"
                                    prop:value=move || f_ra_h.get()
                                    on:input=move |ev| {
                                        f_ra_h.set(
                                            ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .value()
                                        );
                                    }
                                />
                                <span class="sched-field-unit">"h"</span>
                                <span class="sched-field-label" style="margin-left:8px;">{move || tr().sched_dec_label}</span>
                                <input
                                    class="sched-input"
                                    style="width:90px;"
                                    placeholder="-5.3911"
                                    prop:value=move || f_dec_deg.get()
                                    on:input=move |ev| {
                                        f_dec_deg.set(
                                            ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .value()
                                        );
                                    }
                                />
                                <span class="sched-field-unit">"°"</span>
                            </div>
                            {move || coords_hint.get().map(|h| view! {
                                <div class="sched-coords-hint">{h}</div>
                            })}

                            // Constraints
                            <div class="sched-field-row">
                                <span class="sched-field-label">{move || tr().sched_min_alt}</span>
                                <input
                                    class="sched-input"
                                    style="width:60px;"
                                    prop:value=move || f_min_alt.get()
                                    on:input=move |ev| {
                                        f_min_alt.set(
                                            ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .value()
                                        );
                                    }
                                />
                                <span class="sched-field-unit">"°"</span>
                                <span class="sched-field-label" style="margin-left:8px;">{move || tr().sched_moon_sep}</span>
                                <input
                                    class="sched-input"
                                    style="width:60px;"
                                    prop:value=move || f_min_moon.get()
                                    on:input=move |ev| {
                                        f_min_moon.set(
                                            ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .value()
                                        );
                                    }
                                />
                                <span class="sched-field-unit">"°"</span>
                                <span class="sched-field-label" style="margin-left:8px;">{move || tr().sched_pa_label}</span>
                                <input
                                    class="sched-input"
                                    style="width:60px;"
                                    prop:value=move || f_pa.get()
                                    on:input=move |ev| {
                                        f_pa.set(
                                            ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .value()
                                        );
                                    }
                                />
                                <span class="sched-field-unit">"°"</span>
                            </div>

                            // ── Step pipeline ────────────────────────────────
                            <fieldset class="sched-fieldset">
                                <legend>{move || tr().sched_steps_legend}</legend>
                                <div class="sched-field-row" style="gap:16px;">
                                    <label class="sched-toggle-label">
                                        <input type="checkbox"
                                            prop:checked=move || step_track.get()
                                            on:change=move |ev| {
                                                step_track.set(ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked());
                                            }
                                        />
                                        {move || tr().sched_step_track}
                                    </label>
                                    <label class="sched-toggle-label">
                                        <input type="checkbox"
                                            prop:checked=move || step_focus.get()
                                            on:change=move |ev| {
                                                step_focus.set(ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked());
                                            }
                                        />
                                        {move || tr().sched_step_focus}
                                    </label>
                                    <label class="sched-toggle-label">
                                        <input type="checkbox"
                                            prop:checked=move || step_align.get()
                                            on:change=move |ev| {
                                                step_align.set(ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked());
                                            }
                                        />
                                        {move || tr().sched_step_align}
                                    </label>
                                    <label class="sched-toggle-label">
                                        <input type="checkbox"
                                            prop:checked=move || step_guide.get()
                                            on:change=move |ev| {
                                                step_guide.set(ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().checked());
                                            }
                                        />
                                        {move || tr().sched_step_guide}
                                    </label>
                                </div>
                            </fieldset>

                            // ── Startup condition ────────────────────────────
                            <fieldset class="sched-fieldset">
                                <legend>{move || tr().sched_start_when}</legend>
                                <div class="sched-field-row">
                                    <select
                                        class="sched-select"
                                        on:change=move |ev| {
                                            startup_cond.set(ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlSelectElement>().value());
                                        }>
                                        <option value="asap">{move || tr().sched_cond_asap}</option>
                                        <option value="at">{move || tr().sched_cond_at_time}</option>
                                    </select>
                                    {move || (startup_cond.get() == "at").then(|| view! {
                                        <input
                                            type="datetime-local"
                                            class="sched-input"
                                            prop:value=move || startup_at.get()
                                            on:input=move |ev| {
                                                startup_at.set(ev.target().unwrap()
                                                    .unchecked_into::<web_sys::HtmlInputElement>().value());
                                            }
                                        />
                                    })}
                                </div>
                            </fieldset>

                            // ── Completion condition ─────────────────────────
                            <fieldset class="sched-fieldset">
                                <legend>{move || tr().sched_complete_when}</legend>
                                <div class="sched-field-row">
                                    <select
                                        class="sched-select"
                                        on:change=move |ev| {
                                            completion_cond.set(ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlSelectElement>().value());
                                        }>
                                        <option value="sequence">{move || tr().sched_cond_seq}</option>
                                        <option value="repeat">{move || tr().sched_cond_repeat}</option>
                                        <option value="loop">{move || tr().sched_cond_loop}</option>
                                        <option value="at">{move || tr().sched_cond_finish_at}</option>
                                    </select>
                                    {move || match completion_cond.get().as_str() {
                                        "repeat" => view! {
                                            <input
                                                type="number"
                                                class="sched-input"
                                                style="width:60px;"
                                                min="1"
                                                prop:value=move || completion_count.get()
                                                on:input=move |ev| {
                                                    completion_count.set(ev.target().unwrap()
                                                        .unchecked_into::<web_sys::HtmlInputElement>().value());
                                                }
                                            />
                                            <span class="sched-field-unit">{move || tr().sched_times_unit}</span>
                                        }.into_any(),
                                        "at" => view! {
                                            <input
                                                type="datetime-local"
                                                class="sched-input"
                                                prop:value=move || completion_at.get()
                                                on:input=move |ev| {
                                                    completion_at.set(ev.target().unwrap()
                                                        .unchecked_into::<web_sys::HtmlInputElement>().value());
                                                }
                                            />
                                        }.into_any(),
                                        _ => view! { <span></span> }.into_any(),
                                    }}
                                </div>
                            </fieldset>

                            // ── Sequence builder ────────────────────────────
                            <div class="sched-seq-section">
                                <span class="sched-seq-label">{move || tr().sched_seq_label}</span>
                                <table class="sched-seq-table">
                                    <thead>
                                        <tr>
                                            <th style="width:24px;">"#"</th>
                                            <th>{move || tr().sched_seq_col_type}</th>
                                            <th>{move || tr().sched_seq_col_filter}</th>
                                            <th>{move || tr().sched_seq_col_exp}</th>
                                            <th>{move || tr().sched_seq_col_count}</th>
                                            <th></th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        {move || {
                                            seq_frames.get().into_iter().enumerate().map(|(idx, frame)| {
                                                let ft = frame.frame_type.clone();
                                                let fi = frame.filter.clone();
                                                let ex = frame.exposure.clone();
                                                let co = frame.count.clone();
                                                view! {
                                                    <tr>
                                                        <td class="sched-seq-row-num">{idx + 1}</td>
                                                        <td>
                                                            <select
                                                                class="sched-select"
                                                                prop:value=ft.clone()
                                                                on:change=move |ev| {
                                                                    let v = ev.target().unwrap()
                                                                        .unchecked_into::<web_sys::HtmlSelectElement>()
                                                                        .value();
                                                                    seq_frames.update(|fs| {
                                                                        if let Some(f) = fs.get_mut(idx) { f.frame_type = v; }
                                                                    });
                                                                }>
                                                                <option value="Light" selected={ft == "Light"}>"Light"</option>
                                                                <option value="Dark"  selected={ft == "Dark"}>"Dark"</option>
                                                                <option value="Bias"  selected={ft == "Bias"}>"Bias"</option>
                                                                <option value="Flat"  selected={ft == "Flat"}>"Flat"</option>
                                                            </select>
                                                        </td>
                                                        <td>
                                                            <input
                                                                class="sched-input"
                                                                style="width:70px;"
                                                                placeholder="Ha"
                                                                prop:value=fi
                                                                on:input=move |ev| {
                                                                    let v = ev.target().unwrap()
                                                                        .unchecked_into::<web_sys::HtmlInputElement>()
                                                                        .value();
                                                                    seq_frames.update(|fs| {
                                                                        if let Some(f) = fs.get_mut(idx) { f.filter = v; }
                                                                    });
                                                                }
                                                            />
                                                        </td>
                                                        <td>
                                                            <input
                                                                class="sched-input"
                                                                style="width:70px;"
                                                                prop:value=ex
                                                                on:input=move |ev| {
                                                                    let v = ev.target().unwrap()
                                                                        .unchecked_into::<web_sys::HtmlInputElement>()
                                                                        .value();
                                                                    seq_frames.update(|fs| {
                                                                        if let Some(f) = fs.get_mut(idx) { f.exposure = v; }
                                                                    });
                                                                }
                                                            />
                                                        </td>
                                                        <td>
                                                            <input
                                                                class="sched-input"
                                                                style="width:60px;"
                                                                prop:value=co
                                                                on:input=move |ev| {
                                                                    let v = ev.target().unwrap()
                                                                        .unchecked_into::<web_sys::HtmlInputElement>()
                                                                        .value();
                                                                    seq_frames.update(|fs| {
                                                                        if let Some(f) = fs.get_mut(idx) { f.count = v; }
                                                                    });
                                                                }
                                                            />
                                                        </td>
                                                        <td>
                                                            <button
                                                                class="sched-remove-btn"
                                                                on:click=move |_| {
                                                                    seq_frames.update(|fs| {
                                                                        if fs.len() > 1 { fs.remove(idx); }
                                                                    });
                                                                }>
                                                                "×"
                                                            </button>
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect::<Vec<_>>()
                                        }}
                                    </tbody>
                                </table>
                                {move || {
                                    let hint = seq_total_hint.get();
                                    (!hint.is_empty()).then(|| view! {
                                        <div class="sched-seq-total">{hint}</div>
                                    })
                                }}
                                <button
                                    class="sched-seq-add-btn"
                                    on:click=move |_| {
                                        seq_frames.update(|fs| fs.push(SeqFrame::default()));
                                    }>
                                    {move || tr().sched_add_frame}
                                </button>
                            </div>

                            // Form error + action buttons
                            {move || form_error.get().map(|e| view! {
                                <div class="sched-form-error">{e}</div>
                            })}
                            <div class="sched-form-btns">
                                <button class="sched-add-btn" on:click=on_add_job.clone()>
                                    {move || tr().sched_add_job_btn}
                                </button>
                                <button class="sched-btn-clear" on:click=on_clear_form.clone()>
                                    {move || tr().sched_clear_btn}
                                </button>
                            </div>
                        </div>
                    </details>
                </div>

            </div>
        </div>
    }
}
