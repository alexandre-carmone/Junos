//! Guiding Assistant sheet: PHD2's dialog (`guiding_assistant.cpp`) over
//! `guider_ga`. Opening the sheet opens the assistant (`ga_open`); closing
//! the sheet leaves it running (while it measures, with the guide output
//! off, the tab shows a banner with Open and Stop), Close assistant is PHD2's window close
//! (`ga_close`: ends a run, restores the guide outputs). Start needs
//! guiding, as PHD2's; a Stop before 2 min opens the countdown
//! (`window.wait`) with Cancel and Use samples now (`ga_end_wait`). The
//! numbers are PHD2's own strings (`window.details`), the backlash test is
//! the checkbox PHD2 reads at Stop, each recommendation has its Apply
//! (`ga_apply {kind}`), and Review previous loads a saved run (`ga_history`,
//! `ga_review`).

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{CARD, CARD_TITLE, CHECK, LABEL};
use crate::components::sky::persisted;
use crate::i18n::{t, Lang, Translations};
use crate::ws::GuiderStatusData;

use super::protocol::{call_error, Reply, Rpc};

type Label = fn(&'static Translations) -> &'static str;

/// PHD2's three result grids: (title, [(label, `details` key)]).
const GROUPS: [(Label, &[(Label, &str)]); 3] = [
    (|t| t.guider_ga_status, &[
        (|t| t.guider_ga_start_time, "timestamp"),
        (|t| t.guider_ga_exposure, "exposure_time"),
        (|t| t.guider_snr, "snr"),
        (|t| t.guider_ga_star_mass, "star_mass"),
        (|t| t.guider_ga_elapsed, "elapsed_time"),
        (|t| t.guider_ga_samples, "sample_count"),
    ]),
    (|t| t.guider_ga_hf_motion, &[
        (|t| t.guider_ga_ra_rms, "ra_hpf_rms"),
        (|t| t.guider_ga_dec_rms, "dec_hpf_rms"),
        (|t| t.guider_ga_total_rms, "total_hpf_rms"),
    ]),
    (|t| t.guider_ga_other_motion, &[
        (|t| t.guider_ga_ra_peak, "ra_peak"),
        (|t| t.guider_ga_dec_peak, "dec_peak"),
        (|t| t.guider_ga_ra_peak_peak, "ra_peak_peak"),
        (|t| t.guider_ga_ra_drift, "ra_drift_rate"),
        (|t| t.guider_ga_ra_max_drift, "ra_peak_drift_rate"),
        (|t| t.guider_ga_drift_exposure, "ra_drift_exposure"),
        (|t| t.guider_ga_dec_drift, "dec_drift_rate"),
        (|t| t.guider_ga_backlash, "backlash_info"),
        (|t| t.guider_ga_pa_error, "pa_error"),
    ]),
];

/// The backlash test's Dec positions, north leg then south leg.
fn blt_plot(north: Vec<f64>, south: Vec<f64>) -> impl IntoView {
    let (lo, hi) = north.iter().chain(&south).fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let n = (north.len() + south.len()).max(2) as f64 - 1.0;
    let span = (hi - lo).max(1e-9);
    let pts = |vals: &[f64], from: usize| {
        vals.iter().enumerate()
            .map(|(i, v)| format!("{:.1},{:.1}", 200.0 * (from + i) as f64 / n, 58.0 - 56.0 * (v - lo) / span))
            .collect::<Vec<_>>().join(" ")
    };
    // The south leg starts where the north one ends.
    let south_pts = pts(&south, north.len().saturating_sub(1));
    view! {
        <svg viewBox="0 0 200 60" class="w-full h-16" preserveAspectRatio="none">
            <polyline points=pts(&north, 0) fill="none" stroke="var(--state-err)" stroke-width="1.5"
                      vector-effect="non-scaling-stroke" />
            <polyline points=south_pts fill="none" stroke="var(--accent-cyan)" stroke-width="1.5"
                      vector-effect="non-scaling-stroke" />
        </svg>
    }
}

pub(super) fn assistant_sheet(
    state: RwSignal<Option<GuiderStatusData>>,
    ga: RwSignal<Value>,
    rpc: Rpc,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let rpc = StoredValue::new(rpc);
    let last = RwSignal::new(None::<Reply>);
    let call = move |method: &'static str, params: Value| last.set(Some(rpc.with_value(|r| r.call(method, params))));

    // PHD2 ticks the backlash test unless the mount has encoders; the box
    // is the browser's, sent on change and whenever the assistant opens.
    // The daemon runs each call apart: the box goes once `ga_open` answers.
    let measure_blt = persisted("guider_ga_backlash", true);
    let opening = RwSignal::new(None::<Reply>);
    Effect::new(move |_| {
        if let Some(Ok(_)) = opening.get().and_then(|r| r.get()) {
            opening.set(None);
            call("ga_set_measure_backlash", json!({"on": measure_blt.get_untracked()}));
        }
    });
    let open = move || {
        let r = rpc.with_value(|r| r.call("ga_open", json!({})));
        last.set(Some(r));
        opening.set(Some(r));
    };
    if !ga.with_untracked(|g| g["open"].as_bool().unwrap_or(false)) {
        open();
    }

    let is_open = move || ga.with(|g| g["open"].as_bool().unwrap_or(false));
    let win = Memo::new(move |_| ga.with(|g| g["window"].clone()));
    let flag = move |k: &str| win.with(|w| w[k].as_bool().unwrap_or(false));
    let measuring = move || flag("measuring") || flag("measuring_backlash");
    let wait = Memo::new(move |_| win.with(|w| w["wait"].clone()));
    let no_mount = move || state.with(|s| !s.as_ref().is_some_and(|s| s.equipment.mount.as_ref().is_some_and(|m| m.connected)));
    let detail = move |k: &str| win.with(|w| w["details"][k].as_str().unwrap_or("").trim().to_string());
    let history = RwSignal::new(None::<Reply>);

    let group = move |(title, rows): (Label, &'static [(Label, &'static str)])| view! {
        <div class=CARD>
            <span class=CARD_TITLE>{move || title(tr())}</span>
            {rows.iter().map(|&(label, key)| view! {
                <div class="flex items-baseline justify-between gap-3 min-h-7 flex-wrap">
                    <span class="text-sm text-text-blue">{move || label(tr())}</span>
                    <span class="ml-auto font-mono text-xs text-text whitespace-pre-wrap text-right">{move || detail(key)}</span>
                </div>
            }).collect_view()}
        </div>
    };
    let blt = move || {
        let pts = |k: &str| win.with(|w| w["details"][k].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()))
            .unwrap_or_default();
        let (north, south) = (pts("blt_north"), pts("blt_south"));
        (!north.is_empty() || !south.is_empty()).then(|| blt_plot(north, south))
    };
    let recommendations = move || {
        win.with(|w| w["recommendations"].as_array().cloned().unwrap_or_default()).into_iter().map(|r| {
            let kind = r["kind"].as_str().unwrap_or("").to_string();
            let applied = r["applied"].as_bool().unwrap_or(false);
            let can_apply = r["can_apply"].as_bool().unwrap_or(false);
            view! {
                <div class="flex items-center gap-2 min-h-[44px] md:min-h-9">
                    <span class="flex-1 min-w-0 text-sm text-text break-words">{r["text"].as_str().unwrap_or("").to_string()}</span>
                    {if applied {
                        view! { <span class="shrink-0 text-state-ok" title=move || tr().guider_ga_applied>"\u{2713}"</span> }.into_any()
                    } else if can_apply {
                        view! {
                            <button class="btn btn-ghost h-9 px-3 shrink-0"
                                    on:click=move |_| call("ga_apply", json!({"kind": kind}))>
                                {move || tr().guider_ga_apply}
                            </button>
                        }.into_any()
                    } else {
                        ().into_any()
                    }}
                </div>
            }
        }).collect_view()
    };
    let past_runs = move || {
        let list = history.get().and_then(|r| r.get()).and_then(Result::ok)?;
        let list: Vec<String> = list.as_array()?.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        Some(if list.is_empty() {
            view! { <span class="text-sm text-text-muted">{move || tr().guider_ga_no_history}</span> }.into_any()
        } else {
            view! {
                <div class="flex flex-wrap gap-1.5">
                    {list.into_iter().rev().map(|ts| {
                        let label = ts.clone();
                        view! {
                            <button class="chip h-9 md:h-7 px-3 font-mono cursor-pointer"
                                    on:click=move |_| call("ga_review", json!({"timestamp": ts}))>
                                {label}
                            </button>
                        }
                    }).collect_view()}
                </div>
            }.into_any()
        })
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 pb-[max(1rem,env(safe-area-inset-bottom))] \
                    grid grid-cols-1 md:grid-cols-2 gap-3 items-start">
            <div class=format!("{CARD} md:col-span-2")>
                <div class="text-sm text-text whitespace-pre-line">{move || win.with(|w| w["instructions"].as_str().unwrap_or("").to_string())}</div>
                <div class="text-xs text-text-muted">{move || win.with(|w| w["status"].as_str().unwrap_or("").to_string())}</div>
                {move || (!wait.with(Value::is_null)).then(|| view! {
                    <div class="panel p-3 flex flex-col gap-2 border-state-warn">
                        <span class="text-sm text-text">
                            {move || if wait.with(|w| w["backlash"].as_bool().unwrap_or(false)) { tr().guider_ga_wait_blt } else { tr().guider_ga_wait }}
                        </span>
                        <span class="font-mono text-sm">
                            {move || format!("{} {}", tr().guider_ga_seconds_left, wait.with(|w| w["seconds"].as_i64().unwrap_or(0)))}
                        </span>
                        <div class="flex gap-2 flex-wrap">
                            <button class="btn btn-ghost h-9 px-3" on:click=move |_| call("ga_end_wait", json!({"accept": false}))>
                                {move || tr().guider_ga_wait_cancel}
                            </button>
                            <button class="btn btn-ghost h-9 px-3" on:click=move |_| call("ga_end_wait", json!({"accept": true}))>
                                {move || tr().guider_ga_wait_accept}
                            </button>
                        </div>
                    </div>
                })}
                <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                    <input type="checkbox" class=CHECK prop:checked=move || measure_blt.get()
                           disabled=move || no_mount() || flag("measuring_backlash")
                           on:change=move |ev| {
                               let on = crate::dom::event_target_checked(&ev);
                               measure_blt.set(on);
                               call("ga_set_measure_backlash", json!({"on": on}));
                           } />
                    <span class=LABEL>{move || tr().guider_ga_measure_backlash}</span>
                </label>
                {blt}
                <div class="flex gap-2 flex-wrap">
                    <Show when=is_open fallback=move || view! {
                        <button class="btn btn-primary h-11 px-5" on:click=move |_| open()>{move || tr().guider_ga_open}</button>
                    }>
                        <Show when=measuring fallback=move || view! {
                            <button class="btn btn-primary h-11 px-5 font-semibold" on:click=move |_| call("ga_start", json!({}))>
                                {move || tr().guider_ga_start}
                            </button>
                        }>
                            <button class="btn btn-danger h-11 px-5 font-semibold" on:click=move |_| call("ga_stop", json!({}))>
                                {move || tr().guider_btn_stop}
                            </button>
                        </Show>
                        <button class="btn btn-ghost h-11 px-4" on:click=move |_| call("ga_close", json!({}))>
                            {move || tr().guider_ga_close}
                        </button>
                    </Show>
                    <button class="btn btn-ghost h-11 px-4"
                            on:click=move |_| history.set(Some(rpc.with_value(|r| r.call("ga_history", json!({})))))>
                        {move || tr().guider_ga_review}
                    </button>
                </div>
                {past_runs}
                {move || call_error(last).map(|e| view! { <div class="text-sm text-state-err break-words">{e}</div> })}
            </div>

            <div class=format!("{CARD} md:col-span-2")>
                <span class=CARD_TITLE>{move || tr().guider_ga_recommendations}</span>
                {recommendations}
            </div>
            {GROUPS.into_iter().map(group).collect_view()}
        </div>
    }
}
