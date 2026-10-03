//! Scheduler tab: the job list — tonight's altitude of every job, then one
//! card per job.

use std::sync::Arc;

use leptos::prelude::*;
use serde_json::Value;

use crate::compat::SiteSnapshot;
use crate::components::form::{CARD, CARD_TITLE};
use crate::components::sky::{fmt_dec, fmt_ra};
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

use super::altitude::{altitude_chart, Night, Track};
use super::labels::{job_stage_label, job_state_label};

/// `jobs` is the `scheduler_get_jobs` list (`SchedulerJob::toJson`). The
/// cards are rebuilt only when it changes, not on every log line.
#[component]
pub fn SchedulerJobs(
    #[prop(into)] jobs: Signal<Vec<Value>>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    #[prop(into)] send: SendCmd,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let s_refresh = Arc::clone(&send);

    view! {
        <div class=format!("{CARD} md:min-h-0")>
            <div class="flex items-center gap-2">
                <span class=format!("{CARD_TITLE} flex-1")>
                    {move || format!("{} \u{00b7} {}", tr().sched_jobs_section, jobs.with(Vec::len))}
                </span>
                <button class="btn-icon text-text-muted" title=move || tr().sched_refresh_jobs
                        on:click=move |_| send_cmd(&s_refresh, "scheduler_get_jobs", serde_json::json!({}))>
                    "\u{21bb}"
                </button>
            </div>
            {move || {
                let list = jobs.get();
                (!list.is_empty()).then(|| {
                    let site = site.get();
                    let tracks = list.iter().enumerate().map(|(i, j)| job_track(i, j)).collect();
                    view! { <div class="shrink-0">{altitude_chart(Night::tonight(&site), site, tracks, None, tr())}</div> }
                })
            }}
            <div class="flex flex-col gap-2 md:flex-1 md:min-h-0 md:overflow-y-auto [overscroll-behavior:contain]">
                {move || {
                    let tr = tr();
                    let list = jobs.get();
                    if list.is_empty() {
                        return view! {
                            <div class="py-8 px-4 text-center text-sm text-text-faint">{tr.sched_no_jobs}</div>
                        }.into_any();
                    }
                    list.into_iter()
                        .enumerate()
                        .map(|(i, job)| job_card(i, job, tr, Arc::clone(&send)))
                        .collect::<Vec<_>>()
                        .into_any()
                }}
            </div>
        </div>
    }
}

/// First non-empty value among `keys` — KStars' formatted time, else ISO
/// ("--" when unset).
fn job_time(job: &Value, keys: [&str; 2]) -> Option<String> {
    keys.iter()
        .filter_map(|k| job[*k].as_str())
        .find(|s| !s.is_empty() && *s != "--")
        .map(str::to_string)
}

/// KStars' ISO time as Unix ms; `None` for "--".
fn job_ms(job: &Value, key: &str) -> Option<f64> {
    job[key].as_str().map(js_sys::Date::parse).filter(|t| t.is_finite())
}

/// The job's curve, with KStars' planned startup → stop as its window.
fn job_track(i: usize, job: &Value) -> Track {
    Track {
        ra_deg: job["targetRA"].as_f64().unwrap_or(0.0) * 15.0,
        dec_deg: job["targetDEC"].as_f64().unwrap_or(0.0),
        window: job_ms(job, "startupTime").zip(job_ms(job, "stopTime")),
        label: Some(format!("#{}", i + 1)),
    }
}

fn job_card(i: usize, job: Value, tr: &'static Translations, send: SendCmd) -> impl IntoView {
    let name = job["name"].as_str().unwrap_or("?").to_string();
    // targetRA / targetDEC are J2000 (ra0 / dec0), RA in hours.
    let coords = format!(
        "{} {}",
        fmt_ra(job["targetRA"].as_f64().unwrap_or(0.0) * 15.0),
        fmt_dec(job["targetDEC"].as_f64().unwrap_or(0.0)),
    );
    let state = job["state"].as_i64().unwrap_or(0);
    let (state_label, state_cls) = job_state_label(tr, state);
    let stage = if state == 3 { job_stage_label(tr, job["stage"].as_i64().unwrap_or(0)) } else { "" };
    let alt = job["altitude"].as_f64().unwrap_or(0.0);
    let alt_txt = job["altitudeFormatted"].as_str().filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{alt:.0}\u{00b0}"));
    let alt_cls = if alt >= 30.0 { "text-state-ok" } else if alt >= 20.0 { "text-state-warn" } else { "text-state-err" };
    let window = match (job_time(&job, ["startupFormatted", "startupTime"]), job_time(&job, ["endFormatted", "stopTime"])) {
        (None, None) => None,
        (a, b) => Some(format!("{} \u{2192} {}", a.as_deref().unwrap_or("\u{2014}"), b.as_deref().unwrap_or("\u{2014}"))),
    };
    let done = job["completedCount"].as_i64().unwrap_or(0);
    let total = job["sequenceCount"].as_i64().unwrap_or(0);
    let pct = if total > 0 { (done * 100 / total).clamp(0, 100) } else { 0 };

    let on_remove = move |_| {
        send_cmd(&send, "scheduler_remove_jobs", serde_json::json!({ "index": i }));
        let s = Arc::clone(&send);
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(400).await;
            send_cmd(&s, "scheduler_get_jobs", serde_json::json!({}));
        });
    };

    view! {
        <div class=if state == 3 {
                 "rounded-lg border border-accent-cyan-dim bg-bg-elev-2 p-3 flex flex-col gap-1.5"
             } else {
                 "rounded-lg border border-border-base bg-bg-elev-2 p-3 flex flex-col gap-1.5"
             }>
            <div class="flex items-center gap-2 min-w-0">
                <span class="shrink-0 font-mono text-xs text-text-faint">{format!("#{}", i + 1)}</span>
                <span class="flex-1 min-w-0 truncate font-semibold text-text-blue-bright">{name}</span>
                <span class=format!("shrink-0 text-sm {state_cls}")>
                    {state_label}
                    {(!stage.is_empty()).then(|| view! { <span class="text-text-muted">{format!(" \u{00b7} {stage}")}</span> })}
                </span>
                <button class="btn-icon shrink-0 -mr-1 text-text-muted" title=tr.sched_remove_job on:click=on_remove>
                    "\u{2716}"
                </button>
            </div>
            <div class="flex flex-wrap gap-x-3 gap-y-0.5 font-mono text-xs text-text-muted">
                <span>{coords}</span>
                <span class=alt_cls>{format!("Alt {alt_txt}")}</span>
                {window.map(|w| view! { <span>{w}</span> })}
            </div>
            <div class="flex items-center gap-2">
                <div class="flex-1 h-1.5 rounded-full bg-border-strong overflow-hidden">
                    <div class="h-full rounded-full bg-accent-cyan" style=format!("width:{pct}%")></div>
                </div>
                <span class="shrink-0 font-mono text-xs text-text-muted">{format!("{done}/{total}")}</span>
            </div>
        </div>
    }
}
