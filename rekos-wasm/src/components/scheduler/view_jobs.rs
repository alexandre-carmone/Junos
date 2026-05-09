use std::sync::Arc;

use leptos::prelude::*;

use crate::compat::SchedulerSnapshot;
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

use super::labels::{job_stage_label, job_state_label, scheduler_status_label};

#[component]
pub fn SchedulerHeader(
    #[prop(into)] scheduler: Signal<SchedulerSnapshot>,
    #[prop(into)] send: SendCmd,
    #[prop(into)] lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    let on_toggle = {
        let send_for_toggle = Arc::clone(&send);
        move |_| {
            send_cmd(
                &send_for_toggle,
                "scheduler_start_job",
                serde_json::json!({}),
            );
        }
    };

    view! {
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
    }
}

#[component]
pub fn SchedulerJobsSection(
    #[prop(into)] scheduler: Signal<SchedulerSnapshot>,
    #[prop(into)] send: SendCmd,
    #[prop(into)] lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    view! {
        {move || {
            let snap = scheduler.get();
            let job_count = snap.jobs.len();
            let send_ref = Arc::clone(&send);
            let send_ref2 = Arc::clone(&send);
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
                            }
                                .into_any()
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
                                        {snap.jobs
                                            .into_iter()
                                            .enumerate()
                                            .map(|(i, job)| {
                                                let name = job["name"].as_str().unwrap_or("?").to_string();
                                                let ra_h = job["targetRA"].as_f64().unwrap_or(0.0);
                                                let dec_d = job["targetDEC"].as_f64().unwrap_or(0.0);
                                                let state = job["state"].as_i64().unwrap_or(0);
                                                let stage = job["stage"].as_i64().unwrap_or(0);
                                                let alt_val = job["altitude"].as_f64().unwrap_or(0.0);
                                                let alt_str = job["altitudeFormatted"]
                                                    .as_str()
                                                    .map(|s| s.to_string())
                                                    .unwrap_or_else(|| format!("{:.0}°", alt_val));
                                                let done = job["completedCount"].as_i64().unwrap_or(0);
                                                let total = job["sequenceCount"].as_i64().unwrap_or(0);
                                                let start_s = job["startupFormatted"]
                                                    .as_str()
                                                    .or_else(|| job["startupTime"].as_str())
                                                    .unwrap_or("—")
                                                    .to_string();
                                                let end_s = job["endFormatted"]
                                                    .as_str()
                                                    .or_else(|| job["completionTime"].as_str())
                                                    .unwrap_or("—")
                                                    .to_string();
                                                let (state_label, state_cls) = job_state_label(tr(), state);
                                                let stage_label =
                                                    if state == 3 { job_stage_label(tr(), stage) } else { "" };
                                                let alt_cls = if alt_val >= 30.0 {
                                                    "sched-alt-good"
                                                } else if alt_val >= 20.0 {
                                                    "sched-alt-warn"
                                                } else {
                                                    "sched-alt-bad"
                                                };
                                                let pct = if total > 0 {
                                                    (done * 100 / total).min(100)
                                                } else {
                                                    0
                                                };
                                                let send_rm = Arc::clone(&send_ref2);
                                                view! {
                                                    <tr>
                                                        <td class="sched-cell-name">{name}</td>
                                                        <td class="sched-cell-coords">
                                                            {format!(
                                                                "{:.2}h {}{:.1}°",
                                                                ra_h,
                                                                if dec_d < 0.0 { "" } else { "+" },
                                                                dec_d
                                                            )}
                                                        </td>
                                                        <td class={state_cls}>
                                                            {state_label}
                                                            {(!stage_label.is_empty())
                                                                .then(|| view! {
                                                                    <span class="sched-stage-sub">{"∙ "}{stage_label}</span>
                                                                })}
                                                        </td>
                                                        <td class={alt_cls}>{alt_str}</td>
                                                        <td>
                                                            <div class="sched-progress-wrap">
                                                                <div
                                                                    class="sched-progress-bar"
                                                                    style={format!("width:{}%", pct)}
                                                                ></div>
                                                                <span class="sched-progress-text">
                                                                    {format!("{}/{}", done, total)}
                                                                </span>
                                                            </div>
                                                        </td>
                                                        <td class="sched-col-start sched-cell-time">{start_s}</td>
                                                        <td class="sched-col-end sched-cell-time">{end_s}</td>
                                                        <td class="sched-cell-remove">
                                                            <button
                                                                class="sched-remove-btn"
                                                                title=move || tr().sched_remove_job
                                                                on:click=move |_| {
                                                                    send_cmd(
                                                                        &send_rm,
                                                                        "scheduler_remove_jobs",
                                                                        serde_json::json!({ "index": i }),
                                                                    );
                                                                    let sr = Arc::clone(&send_rm);
                                                                    wasm_bindgen_futures::spawn_local(async move {
                                                                        gloo_timers::future::TimeoutFuture::new(400).await;
                                                                        send_cmd(
                                                                            &sr,
                                                                            "scheduler_get_jobs",
                                                                            serde_json::json!({}),
                                                                        );
                                                                    });
                                                                }>
                                                                "×"
                                                            </button>
                                                        </td>
                                                    </tr>
                                                }
                                            })
                                            .collect::<Vec<_>>()}
                                    </tbody>
                                </table>
                            }
                                .into_any()
                        }}
                    </div>
                </div>
            }
        }}
    }
}
