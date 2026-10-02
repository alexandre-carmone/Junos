//! The capture queue: one card per `capture_get_sequences` job, and the job
//! details.
//!
//! Job keys come from `Camera::createJsonJob` (camera_jobs.cpp:877): Status
//! (Idle | In Progress | Aborted | Complete), Type, Exp, Filter, Count
//! ("done/total"), Bin, "ISO/Gain", Offset, Encoding, Format, Temperature.

use leptos::prelude::*;
use serde_json::Value;

use crate::components::frame_type::frame_type_visual;
use crate::i18n::Translations;

use super::fields::text;

fn status_cls(status: &str) -> &'static str {
    match status {
        "Complete" => "text-state-ok",
        "In Progress" => "text-state-info",
        "Aborted" => "text-state-err",
        _ => "text-text-muted",
    }
}

/// One job card; a tap opens its details. The running job shows the live
/// frame count (`seqv` / `seqr` of `new_capture_state`).
pub(super) fn job_card(
    i: usize,
    job: &Value,
    live: (Option<i64>, Option<i64>),
    tr: &'static Translations,
    open: Callback<usize>,
    remove: Callback<usize>,
) -> impl IntoView + use<> {
    let status = job["Status"].as_str().unwrap_or("Idle").to_string();
    let running = status == "In Progress";
    let status_cls = format!("shrink-0 text-xs {}", status_cls(&status));
    let (mut done, mut total) = job["Count"]
        .as_str()
        .and_then(|c| c.split_once('/'))
        .map(|(d, t)| (d.trim().to_string(), t.trim().to_string()))
        .unwrap_or_default();
    if running {
        if let Some(v) = live.0 { done = v.to_string(); }
        if let Some(v) = live.1 { total = v.to_string(); }
    }
    let pct = match (done.parse::<f64>(), total.parse::<f64>()) {
        (Ok(d), Ok(t)) if t > 0.0 => (d / t * 100.0).clamp(0.0, 100.0),
        _ => 0.0,
    };
    let kind = text(&job["Type"]);
    let (icon, color) = frame_type_visual(&kind);
    let summary = [format!("{} s", text(&job["Exp"])), text(&job["Filter"])]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{00b7} ");

    view! {
        <div class=if running {
                 "rounded-lg border border-accent-cyan-dim bg-bg-elev-2 p-3 flex flex-col gap-1.5 cursor-pointer"
             } else {
                 "rounded-lg border border-border-base bg-bg-elev-2 p-3 flex flex-col gap-1.5 cursor-pointer"
             }
             on:click=move |_| open.run(i)>
            <div class="flex items-center gap-2 min-w-0">
                <span class="shrink-0 font-mono text-xs text-text-faint">{format!("#{}", i + 1)}</span>
                <span class="inline-flex shrink-0" style:color=color inner_html=icon></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{kind}</span>
                <span class="flex-1 min-w-0 truncate font-mono text-sm text-text-dim">{summary}</span>
                <button class="btn-icon shrink-0 -mr-1 text-text-muted" title=tr.imaging_remove_job
                        on:click=move |ev| { ev.stop_propagation(); remove.run(i); }>
                    "\u{2716}"
                </button>
            </div>
            <div class="flex items-center gap-2">
                <span class=status_cls>{status}</span>
                <div class="flex-1 h-1 rounded-full bg-bg-elev-3 overflow-hidden">
                    <div class="h-full bg-accent-cyan" style:width=format!("{pct:.0}%")></div>
                </div>
                <span class="shrink-0 font-mono text-xs tabular-nums text-text-muted">{format!("{done} / {total}")}</span>
            </div>
        </div>
    }
}

/// Every field of a job as label / value tiles.
pub(super) fn job_details(job: &Value, tr: &'static Translations) -> impl IntoView + use<> {
    let rows = [
        (tr.status, "Status"),
        (tr.field_frame_type, "Type"),
        (tr.field_exposure_s, "Exp"),
        (tr.field_count, "Count"),
        (tr.field_filter, "Filter"),
        (tr.seq_binning, "Bin"),
        (tr.imaging_gain_iso, "ISO/Gain"),
        (tr.field_offset, "Offset"),
        (tr.field_encoding, "Encoding"),
        (tr.field_format, "Format"),
        (tr.field_job_temp_c, "Temperature"),
    ];
    view! {
        <div class="grid grid-cols-2 md:grid-cols-3 gap-2">
            {rows.map(|(label, key)| {
                let v = text(&job[key]);
                view! {
                    <div class="min-w-0 rounded-lg bg-bg-elev-2 border border-border-base px-2.5 py-1.5 flex flex-col">
                        <span class="text-xs uppercase tracking-[0.06em] text-text-muted truncate">{label}</span>
                        <span class="font-mono text-sm text-text-dim break-words">
                            {if v.is_empty() { "\u{2014}".to_string() } else { v }}
                        </span>
                    </div>
                }
            }).to_vec()}
        </div>
    }
}
