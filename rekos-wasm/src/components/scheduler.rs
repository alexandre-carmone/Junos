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
use crate::i18n::{Lang, t};
use crate::ws::SendCmd;

const SCHED_CSS: &str = r#"
.sched-root {
    position: absolute; inset: 0; background: #0a0a0f; color: #c0c0d0;
    font-family: monospace; display: flex; flex-direction: column;
    overflow: hidden;
}
.sched-header {
    display: flex; flex-wrap: wrap; align-items: center; gap: 10px 18px;
    padding: 10px 20px 10px 80px;
    border-bottom: 1px solid #222; background: rgba(6,6,15,0.85);
    font-size: 13px; min-height: 44px; flex-shrink: 0;
}
.sched-badge {
    display: inline-block; padding: 3px 10px; border-radius: 14px;
    font-size: 11px; font-weight: 600; letter-spacing: 0.06em;
}
.sched-badge-idle    { background: #1a1a2a; color: #88aaff; border: 1px solid #3a3a5a; }
.sched-badge-running { background: #0a2a1a; color: #44ee88; border: 1px solid #1a5a3a; }
.sched-badge-paused  { background: #2a2a0a; color: #ffcc44; border: 1px solid #5a5a1a; }
.sched-log {
    color: #667; font-size: 11px; flex: 1; overflow: hidden;
    text-overflow: ellipsis; white-space: nowrap;
}
.sched-btn {
    padding: 4px 14px; border-radius: 6px; font-family: monospace;
    font-size: 11px; font-weight: 600; cursor: pointer;
    touch-action: manipulation; -webkit-tap-highlight-color: transparent;
    border: 1px solid #3a5a3a; background: #0a1a0a; color: #44ee88;
    transition: background 0.15s;
}
.sched-btn:hover { background: #0f2a0f; }
.sched-btn-stop {
    border-color: #5a2a2a; background: #1a0a0a; color: #ee4444;
}
.sched-btn-stop:hover { background: #2a0a0a; }
.sched-body {
    flex: 1; overflow-y: auto; -webkit-overflow-scrolling: touch;
    padding: 0 0 80px;
}
.sched-table-wrap { overflow-x: auto; padding: 0 16px 16px; }
.sched-table {
    width: 100%; border-collapse: collapse; font-size: 12px;
    min-width: 600px;
}
.sched-table th {
    color: #88aaff; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.06em; padding: 8px 8px 4px;
    border-bottom: 1px solid #222; text-align: left; white-space: nowrap;
}
.sched-table td {
    padding: 7px 8px; border-bottom: 1px solid #111;
    vertical-align: middle;
}
.sched-table tr:hover td { background: rgba(136,170,255,0.04); }
.sched-state-active   { color: #44ee88; }
.sched-state-error    { color: #ee4444; }
.sched-state-aborted  { color: #ffaa44; }
.sched-state-complete { color: #6688aa; }
.sched-state-queued   { color: #c0c0d0; }
.sched-remove-btn {
    background: none; border: none; color: #884444; cursor: pointer;
    font-size: 14px; padding: 2px 6px; border-radius: 4px;
    touch-action: manipulation;
}
.sched-remove-btn:hover { background: rgba(200,50,50,0.15); color: #ee4444; }
.sched-empty { color: #444; font-size: 12px; padding: 24px 20px; }
.sched-add-section {
    padding: 0 16px 16px;
}
.sched-add-details {
    border: 1px solid #222; border-radius: 8px; overflow: hidden;
}
.sched-add-summary {
    padding: 10px 14px; font-size: 11px; color: #88aaff; cursor: pointer;
    user-select: none; background: rgba(10,10,20,0.6);
    letter-spacing: 0.05em; text-transform: uppercase; font-weight: 600;
    list-style: none; display: flex; align-items: center; gap: 8px;
}
.sched-add-summary:hover { background: rgba(20,20,40,0.8); }
.sched-add-body {
    padding: 14px; display: flex; flex-direction: column; gap: 12px;
    background: rgba(6,6,15,0.5);
}
.sched-field-row {
    display: flex; flex-wrap: wrap; gap: 10px; align-items: center;
}
.sched-field-label {
    color: #88aaff; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.05em; min-width: 60px;
}
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
.sched-search-row {
    display: flex; gap: 8px; align-items: center; flex-wrap: wrap;
}
.sched-search-result {
    color: #44ee88; font-size: 11px; padding: 3px 8px;
    background: #0a1a0a; border-radius: 4px; border: 1px solid #1a3a1a;
}
/* Sequence builder */
.sched-seq-section {
    display: flex; flex-direction: column; gap: 8px;
}
.sched-seq-label {
    color: #88aaff; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.05em;
}
.sched-seq-table {
    border-collapse: collapse; font-size: 12px; width: 100%;
}
.sched-seq-table th {
    color: #556; font-size: 10px; text-transform: uppercase;
    letter-spacing: 0.05em; padding: 4px 6px; text-align: left;
    border-bottom: 1px solid #1a1a2a;
}
.sched-seq-table td { padding: 4px 4px; }
.sched-seq-add-btn {
    background: none; border: 1px dashed #3a3a5a; border-radius: 4px;
    color: #88aaff; font-family: monospace; font-size: 11px;
    cursor: pointer; padding: 4px 12px; margin-top: 4px;
    touch-action: manipulation;
}
.sched-seq-add-btn:hover { background: rgba(136,170,255,0.08); }
.sched-add-btn {
    align-self: flex-end; padding: 6px 20px; border-radius: 6px;
    font-family: monospace; font-size: 12px; font-weight: 600;
    cursor: pointer; touch-action: manipulation;
    border: 1px solid #3a5a3a; background: #0a1a0a; color: #44ee88;
}
.sched-add-btn:hover { background: #0f2a0f; }
"#;

fn scheduler_status_label(status: i64) -> (&'static str, &'static str) {
    match status {
        0 => ("Idle",    "sched-badge-idle"),
        1 => ("Running", "sched-badge-running"),
        2 => ("Paused",  "sched-badge-paused"),
        _ => ("Unknown", "sched-badge-idle"),
    }
}

fn job_state_label(state: i64) -> (&'static str, &'static str) {
    match state {
        0 => ("Idle",       "sched-state-queued"),
        1 => ("Evaluating", "sched-state-queued"),
        2 => ("Scheduled",  "sched-state-queued"),
        3 => ("Active",     "sched-state-active"),
        4 => ("Error",      "sched-state-error"),
        5 => ("Aborted",    "sched-state-aborted"),
        6 => ("Invalid",    "sched-state-error"),
        7 => ("Complete",   "sched-state-complete"),
        _ => ("?",          "sched-state-queued"),
    }
}

fn send_cmd(send: &SendCmd, type_str: &str, payload: serde_json::Value) {
    let msg = serde_json::json!({"type": type_str, "payload": payload}).to_string();
    send(msg);
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Generate a minimal ESQ XML from a list of sequence frames.
fn build_esq_xml(job_name: &str, frames: &[SeqFrame]) -> String {
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
struct SeqFrame {
    frame_type: String,
    filter:     String,
    exposure:   String,
    count:      String,
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
    let _tr = move || t(lang.get());

    let dso_catalog = use_context::<RwSignal<Option<std::sync::Arc<DsoCatalogData>>>>();

    // ── Add-job form state ──────────────────────────────────────────────────
    let f_target_name = RwSignal::new(String::new());
    let f_ra_h        = RwSignal::new(String::new());
    let f_dec_deg     = RwSignal::new(String::new());
    let f_min_alt     = RwSignal::new("30".to_string());
    let f_min_moon    = RwSignal::new("0".to_string());
    let f_repeats     = RwSignal::new("1".to_string());
    let f_pa          = RwSignal::new("0".to_string());
    let search_result = RwSignal::new(Option::<String>::None);

    // Sequence frames — start with one default Light row
    let seq_frames: RwSignal<Vec<SeqFrame>> = RwSignal::new(vec![SeqFrame::default()]);

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
                    search_result2.set(Some("Not found in catalog".to_string()));
                }
            }
        }
    };

    // ── Submit new job ──────────────────────────────────────────────────────
    let send_for_add = Arc::clone(&send);
    let on_add_job = move |_| {
        let name      = f_target_name.get_untracked();
        let home      = scheduler.get_untracked().home_dir;
        let frames_raw = seq_frames.get_untracked();

        // Convert raw string fields to typed values for XML generation
        let frames: Vec<SeqFrame> = frames_raw.iter().filter(|f| {
            f.exposure.parse::<f64>().is_ok() && f.count.parse::<u32>().is_ok()
        }).cloned().collect();

        if frames.is_empty() { return; }

        let xml = build_esq_xml(&name, &frames);
        let safe_name = sanitize_name(if name.is_empty() { "sequence" } else { &name });
        let rel_path  = format!(".rekos-sequences/{}.esq", safe_name);
        let abs_path  = if home.is_empty() {
            format!(".rekos-sequences/{}.esq", safe_name)
        } else {
            format!("{}/.rekos-sequences/{}.esq", home, safe_name)
        };

        let ra_f  = f_ra_h.get_untracked().parse::<f64>().unwrap_or(0.0);
        let dec_f = f_dec_deg.get_untracked().parse::<f64>().unwrap_or(0.0);

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
            "repeatsSpin":       f_repeats.get_untracked().parse::<i64>().unwrap_or(1),
            "positionAngleSpin": f_pa.get_untracked().parse::<f64>().unwrap_or(0.0),
        }));

        // 3. Add job (file is written before this arrives at KStars)
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

    view! {
        <style>{SCHED_CSS}</style>
        <div class="sched-root">
            // ── Header ──────────────────────────────────────────────────────
            <div class="sched-header">
                <span style="color:#88aaff; font-weight:700; font-size:13px; letter-spacing:0.08em;">
                    "SCHEDULER"
                </span>
                {move || {
                    let snap = scheduler.get();
                    let (label, cls) = scheduler_status_label(snap.status);
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
                    {move || if scheduler.get().status == 1 { "■ Stop" } else { "▶ Start" }}
                </button>
                <span class="sched-log">
                    {move || {
                        let log = scheduler.get().log;
                        if log.is_empty() { "—".to_string() } else { log }
                    }}
                </span>
            </div>

            // ── Body ─────────────────────────────────────────────────────────
            <div class="sched-body">
                // ── Job list ─────────────────────────────────────────────────
                <div class="sched-table-wrap">
                    {move || {
                        let snap = scheduler.get();
                        if snap.jobs.is_empty() {
                            view! {
                                <div class="sched-empty">
                                    "No scheduled jobs. Add one below or load a schedule in KStars."
                                </div>
                            }.into_any()
                        } else {
                            let send_for_remove = Arc::clone(&send);
                            view! {
                                <table class="sched-table">
                                    <thead>
                                        <tr>
                                            <th>"#"</th>
                                            <th>"Name"</th>
                                            <th>"RA / Dec"</th>
                                            <th>"State"</th>
                                            <th>"Alt"</th>
                                            <th>"Progress"</th>
                                            <th>"Start"</th>
                                            <th>"End"</th>
                                            <th></th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        {snap.jobs.into_iter().enumerate().map(|(i, job)| {
                                            let name    = job["name"].as_str().unwrap_or("?").to_string();
                                            let ra_h    = job["targetRA"].as_f64().unwrap_or(0.0);
                                            let dec_d   = job["targetDEC"].as_f64().unwrap_or(0.0);
                                            let state   = job["state"].as_i64().unwrap_or(0);
                                            let alt     = job["altitudeFormatted"].as_str()
                                                .map(|s| s.to_string())
                                                .unwrap_or_else(|| {
                                                    format!("{:.0}°", job["altitude"].as_f64().unwrap_or(0.0))
                                                });
                                            let done    = job["completedCount"].as_i64().unwrap_or(0);
                                            let total   = job["sequenceCount"].as_i64().unwrap_or(0);
                                            let start_s = job["startupFormatted"].as_str()
                                                .or_else(|| job["startupTime"].as_str())
                                                .unwrap_or("—").to_string();
                                            let end_s   = job["endFormatted"].as_str()
                                                .or_else(|| job["completionTime"].as_str())
                                                .unwrap_or("—").to_string();
                                            let (state_label, state_cls) = job_state_label(state);
                                            let send_rm = Arc::clone(&send_for_remove);
                                            view! {
                                                <tr>
                                                    <td style="color:#445;">{i + 1}</td>
                                                    <td style="font-weight:600;">{name}</td>
                                                    <td style="color:#aaa;">
                                                        {format!("{:.2}h / {:.1}°", ra_h, dec_d)}
                                                    </td>
                                                    <td class={state_cls}>{state_label}</td>
                                                    <td style="color:#88aaff;">{alt}</td>
                                                    <td>{format!("{}/{}", done, total)}</td>
                                                    <td style="color:#667; font-size:11px;">{start_s}</td>
                                                    <td style="color:#667; font-size:11px;">{end_s}</td>
                                                    <td>
                                                        <button
                                                            class="sched-remove-btn"
                                                            title="Remove job"
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
                                                            "✕"
                                                        </button>
                                                    </td>
                                                </tr>
                                            }
                                        }).collect::<Vec<_>>()}
                                    </tbody>
                                </table>
                            }.into_any()
                        }
                    }}
                </div>

                // ── Add Job form ──────────────────────────────────────────────
                <div class="sched-add-section">
                    <details class="sched-add-details">
                        <summary class="sched-add-summary">"▸ Add Job"</summary>
                        <div class="sched-add-body">
                            // Target name + catalog search
                            <div class="sched-field-row">
                                <span class="sched-field-label">"Target"</span>
                                <div class="sched-search-row">
                                    <input
                                        class="sched-input"
                                        style="width:180px;"
                                        placeholder="M42 / NGC 1234 / …"
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
                                        "Search catalog"
                                    </button>
                                    {move || search_result.get().map(|r| view! {
                                        <span class="sched-search-result">{r}</span>
                                    })}
                                </div>
                            </div>

                            // RA / Dec
                            <div class="sched-field-row">
                                <span class="sched-field-label">"RA (h)"</span>
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
                                <span class="sched-field-label" style="margin-left:8px;">"Dec (°)"</span>
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
                            </div>

                            // Constraints
                            <div class="sched-field-row">
                                <span class="sched-field-label">"Min alt"</span>
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
                                <span style="color:#445; font-size:11px;">"°"</span>
                                <span class="sched-field-label" style="margin-left:8px;">"Min moon"</span>
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
                                <span style="color:#445; font-size:11px;">"°"</span>
                                <span class="sched-field-label" style="margin-left:8px;">"Repeats"</span>
                                <input
                                    class="sched-input"
                                    style="width:52px;"
                                    prop:value=move || f_repeats.get()
                                    on:input=move |ev| {
                                        f_repeats.set(
                                            ev.target().unwrap()
                                                .unchecked_into::<web_sys::HtmlInputElement>()
                                                .value()
                                        );
                                    }
                                />
                                <span class="sched-field-label" style="margin-left:8px;">"PA (°)"</span>
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
                            </div>

                            // ── Sequence builder ────────────────────────────
                            <div class="sched-seq-section">
                                <span class="sched-seq-label">"Sequence"</span>
                                <table class="sched-seq-table">
                                    <thead>
                                        <tr>
                                            <th>"Type"</th>
                                            <th>"Filter"</th>
                                            <th>"Exp (s)"</th>
                                            <th>"Count"</th>
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
                                                                "✕"
                                                            </button>
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect::<Vec<_>>()
                                        }}
                                    </tbody>
                                </table>
                                <button
                                    class="sched-seq-add-btn"
                                    on:click=move |_| {
                                        seq_frames.update(|fs| fs.push(SeqFrame::default()));
                                    }>
                                    "+ Add frame type"
                                </button>
                            </div>

                            <button class="sched-add-btn" on:click=on_add_job.clone()>
                                "Add Job"
                            </button>
                        </div>
                    </details>
                </div>
            </div>
        </div>
    }
}
