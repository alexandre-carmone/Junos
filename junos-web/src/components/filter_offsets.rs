//! Build Filter Offsets — Ekos' wizard that autofocuses each filter a few
//! times against a reference filter and derives the focus offsets of the
//! Filter Settings table. Shown inside the Filter settings sheet.
//!
//! Driven entirely over Ekos Live: KStars keeps a headless
//! `BuildFilterOffsets` per wheel (filtermanager.cpp:339), its table comes
//! from `filter_offset_get_all_settings` (lazily built on the first call,
//! buildfilteroffsets.cpp:1085), edits go through
//! `filter_offset_set_all_settings` (:1154 — `adaptFocus`, `refFilter` by
//! name, `filters[i].numFocusRuns` by position; nothing else is settable),
//! and Run / Stop are `invoke_method` calls on `BuildFilterOffsets:<wheel>`
//! (`buildTheOffsets`, `stopProcessing`, Q_INVOKABLE in the header). No call
//! is answered: the progress pushes (`filter_offset_progress`,
//! `filter_offset_calculated`) and the 3 s poll in `ws/retry.rs` keep the
//! store current while a run is on.
//!
//! Three things stay on the KStars desktop, and the view says so: a failed
//! autofocus opens a `KMessageBox` there (not relayed, :559); the table is
//! only re-initialised by the desktop dialog, so one clean run per session
//! (:113, :728); and the handler takes the first wheel by device name
//! (manager.cpp:4420), not necessarily the capture wheel.
//!
//! Saving applies the new offsets to the Filter Settings table ourselves
//! (`fm_set_data`), so a filter can be left out — KStars' `saveTheOffsets`
//! would save every computed one.

use std::collections::HashSet;

use gloo_timers::callback::Timeout;
use leptos::prelude::*;
use serde_json::json;

use crate::components::form::{setting_row, CARD, CARD_TITLE, CHECK, CHIP, FOOTER, LABEL, ROW, SELECT};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang};
use crate::ws::{BfoPhase, FilterOffsetsData, SendCmd};
use crate::ws_helpers::send_cmd;
use crate::FilterCtx;

/// libindi `BaseDevice::FILTER_INTERFACE`.
const FILTER_INTERFACE: i64 = 1 << 4;
/// KStars' default runs per filter (buildfilteroffsets.cpp:308).
const DEFAULT_RUNS: u32 = 5;

/// The wheel `filter_offset_*` addresses: KStars takes the first
/// FilterManager by device name (`QMap`, manager.cpp:4420), so the
/// alphabetically first wheel; the capture wheel until the device list is in.
fn bfo_wheel(ctx: &FilterCtx) -> String {
    ctx.devices
        .with(|d| d.iter().filter(|d| d.interface & FILTER_INTERFACE != 0).map(|d| d.name.clone()).min())
        .unwrap_or_else(|| ctx.wheel.with(|w| w.device.clone()))
}

fn signed(v: i64) -> String {
    if v > 0 { format!("+{v}") } else if v < 0 { format!("\u{2212}{}", -v) } else { "0".to_string() }
}

/// The sheet body: status strip, the run's parameters, one card per filter,
/// and a footer with Back · Run / Stop · Save.
pub fn build_offsets_view(
    ctx: FilterCtx,
    on_back: impl Fn() + Clone + 'static,
    send: SendCmd,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    // Stored so every handler below captures only Copy values; a timer that
    // outlives the view finds it disposed and does nothing.
    let send = StoredValue::new(send);
    let emit = move |ty: &str, payload: serde_json::Value| {
        send.try_with_value(|s| send_cmd(s, ty, payload));
    };
    let data = ctx.offsets;
    let phase = ctx.offsets_phase;
    let wheel = Memo::new(move |_| bfo_wheel(&ctx));
    let capture_wheel = Memo::new(move |_| ctx.wheel.with(|w| w.device.clone()));
    let running = Memo::new(move |_| phase.get() == BfoPhase::Running);
    let has_results = Memo::new(move |_| data.with(|d| d.as_ref().is_some_and(FilterOffsetsData::has_results)));
    let n = Memo::new(move |_| data.with(|d| d.as_ref().map_or(0, |d| d.filters.len())));
    let af_failed = Memo::new(move |_| running.get() && matches!(ctx.focus_state.get().as_str(), "Failed" | "Aborted"));

    // Open: ask for the table — this is what builds KStars' model. Also
    // 300 ms after each push, for the table behind it.
    let refetch = move || emit("filter_offset_get_all_settings", json!({}));
    refetch();
    Effect::new(move |_| {
        ctx.offsets_progress.track();
        Timeout::new(300, refetch).forget();
    });
    let waited = RwSignal::new(false);
    Timeout::new(3_000, move || { let _ = waited.try_set(true); }).forget();

    let invoke = move |name: &str| {
        emit("invoke_method", json!({
            "object": format!("BuildFilterOffsets:{}", wheel.get_untracked()),
            "name": name,
        }));
    };

    // Parameters: a local change, sent whole (positional rows), then the
    // table is re-read for KStars' own corrections.
    let send_settings = move |mutate: &dyn Fn(&mut FilterOffsetsData)| {
            let Some(mut d) = data.get_untracked() else { return };
            mutate(&mut d);
            let reference = d.ref_filter.clone();
            for f in &mut d.filters {
                // The reference filter runs at least once (buildfilteroffsets.cpp:906).
                if f.filter == reference && f.num_focus_runs == 0 {
                    f.num_focus_runs = 1;
                }
                f.selected = f.num_focus_runs > 0;
            }
            emit("filter_offset_set_all_settings", json!({
                "adaptFocus": d.adapt_focus,
                "refFilter": d.ref_filter,
                "filters": d.filters.iter().map(|f| json!({"numFocusRuns": f.num_focus_runs})).collect::<Vec<_>>(),
            }));
            data.set(Some(d));
            Timeout::new(300, refetch).forget();
    };

    // Pushes seen so far: Run is taken back if none follows within 10 s
    // (invoke_method answers nothing, so a wrong object name would otherwise
    // leave the view "Running" for good).
    let pushes = RwSignal::new(0u32);
    Effect::new(move |_| {
        ctx.offsets_progress.track();
        pushes.update(|n| *n += 1);
    });
    let run = move |_| {
        phase.set(BfoPhase::Running);
        invoke("buildTheOffsets");
        let before = pushes.get_untracked();
        Timeout::new(10_000, move || {
            if pushes.try_get_untracked() == Some(before) && phase.get_untracked() == BfoPhase::Running {
                phase.set(BfoPhase::Idle);
            }
        })
        .forget();
    };
    let stop = move |_| invoke("stopProcessing");

    // Save: the computed offsets not unticked go into the Filter Settings
    // table, whole rows, then the table is re-read.
    let skipped = RwSignal::new(HashSet::<String>::new());
    let saved_note = RwSignal::new(false);
    let can_save = Memo::new(move |_| {
        !running.get()
            && data.with(|d| {
                d.as_ref().is_some_and(|d| {
                    skipped.with(|sk| d.filters.iter().any(|f| f.save && !f.af_runs.is_empty() && !sk.contains(&f.filter)))
                })
            })
    });
    let save = move |_| {
            let Some(mut table) = ctx.table.get_untracked() else { return };
            let Some(d) = data.get_untracked() else { return };
            let skip = skipped.get_untracked();
            for f in d.filters.iter().filter(|f| f.save && !f.af_runs.is_empty() && !skip.contains(&f.filter)) {
                if let Some(row) = table.filters.iter_mut().find(|r| r.label == f.filter) {
                    row.offset = f.new_offset;
                }
            }
            emit("fm_set_data", table.to_json());
            saved_note.set(true);
            Timeout::new(500, move || emit("fm_get_data", json!({}))).forget();
            Timeout::new(3_000, move || { let _ = saved_note.try_set(false); }).forget();
    };

    let phase_label = move || match phase.get() {
        BfoPhase::Idle => tr().bfo_phase_idle,
        BfoPhase::Running => tr().bfo_phase_running,
        BfoPhase::Stopped => tr().bfo_phase_stopped,
        BfoPhase::Done => tr().bfo_phase_done,
    };
    let phase_badge = move || match phase.get() {
        BfoPhase::Idle => "badge shrink-0",
        BfoPhase::Running => "badge badge--info shrink-0",
        BfoPhase::Stopped => "badge badge--err shrink-0",
        BfoPhase::Done => "badge badge--ok shrink-0",
    };
    // Progress: the latest push, else the table's own bar.
    let progress = Memo::new(move |_| {
        ctx.offsets_progress.with(|p| p.as_ref().map(|p| (p.current, p.total, p.status.clone())))
            .or_else(|| data.with(|d| d.as_ref().map(|d| (d.progress, d.progress_max, d.status.clone()))))
            .unwrap_or_default()
    });
    let pct = move || {
        let (cur, total, _) = progress.get();
        if total > 0 { (cur as f64 / total as f64 * 100.0).clamp(0.0, 100.0) } else { 0.0 }
    };
    let can_run = Memo::new(move |_| {
        !running.get() && !has_results.get()
            && data.with(|d| d.as_ref().is_some_and(|d| d.filters.iter().any(|f| f.num_focus_runs > 0)))
    });
    let multi_wheel = Memo::new(move |_| {
        let (w, c) = (wheel.get(), capture_wheel.get());
        (!c.is_empty() && w != c).then_some((w, c))
    });
    let ref_filter = Memo::new(move |_| data.with(|d| d.as_ref().map(|d| d.ref_filter.clone()).unwrap_or_default()));
    let filter_names = Memo::new(move |_| data.with(|d| d.as_ref().map(|d| d.filters.iter().map(|f| f.filter.clone()).collect::<Vec<_>>()).unwrap_or_default()));

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            // ── Status ────────────────────────────────────────────────────
            <div class=CARD>
                <div class="flex items-center gap-2 min-w-0">
                    <span class=phase_badge>{phase_label}</span>
                    <span class="flex-1 min-w-0 truncate text-sm text-text-dim">{move || progress.get().2}</span>
                    <span class="shrink-0 font-mono text-xs tabular-nums text-text-muted">
                        {move || { let (c, t, _) = progress.get(); if t > 0 { format!("{c} / {t}") } else { String::new() } }}
                    </span>
                </div>
                <div class="h-1.5 rounded-full bg-bg-elev-3 overflow-hidden">
                    <div class="h-full bg-accent-cyan transition-[width]" style:width=move || format!("{:.0}%", pct())></div>
                </div>
                <Show when=move || af_failed.get()>
                    <div class="rounded-md border border-state-warn bg-bg-elev-2 px-2.5 py-1.5 text-sm text-state-warn">
                        {move || tr().bfo_af_failed_desktop}
                    </div>
                </Show>
                <Show when=move || has_results.get() && !running.get()>
                    <span class="text-xs text-text-muted">{move || tr().bfo_rerun_desktop}</span>
                </Show>
                <Show when=move || multi_wheel.get().is_some()>
                    <span class="text-xs text-state-warn">
                        {move || {
                            let (w, c) = multi_wheel.get().unwrap_or_default();
                            tr().bfo_multi_wheel.replace("{0}", &w).replace("{1}", &c)
                        }}
                    </span>
                </Show>
                <Show when=move || data.with(Option::is_none) && waited.get()>
                    <span class="text-sm text-text-faint">{move || tr().bfo_no_wheel}</span>
                </Show>
            </div>

            // ── Parameters ────────────────────────────────────────────────
            <Show when=move || { n.get() > 0 }>
                <div class=CARD>
                    <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                        <input type="checkbox" class=CHECK
                               prop:checked=move || data.with(|d| d.as_ref().is_some_and(|d| d.adapt_focus))
                               disabled=move || running.get()
                               on:change=move |ev| {
                                   let on = event_target_checked(&ev);
                                   send_settings(&|d| d.adapt_focus = on);
                               } />
                        <span class=LABEL>{move || tr().bfo_adapt_focus}</span>
                    </label>
                    {setting_row(move || tr().bfo_ref_filter, view! {
                        <select class=SELECT disabled=move || running.get() || has_results.get()
                                on:change=move |ev| {
                                    let v = event_target_value(&ev);
                                    send_settings(&|d| d.ref_filter = v.clone());
                                }>
                            {move || {
                                let cur = ref_filter.get();
                                filter_names.get().into_iter().map(|o| {
                                    let (selected, label) = (o == cur, o.clone());
                                    view! { <option value=o prop:selected=selected>{label}</option> }
                                }).collect::<Vec<_>>()
                            }}
                        </select>
                    })}
                    <span class="text-xs text-text-muted">{move || tr().bfo_ref_hint}</span>
                </div>

                // ── One card per filter ───────────────────────────────────
                {move || (0..n.get()).map(|i| filter_card(i, data, running, has_results, skipped, send_settings, lang)).collect::<Vec<_>>()}
            </Show>
        </div>

        // ── Footer ────────────────────────────────────────────────────────
        <div class=FOOTER>
            <button type="button" class="btn btn-ghost h-11 px-4" on:click=move |_| on_back()>
                {move || tr().bfo_back}
            </button>
            <span class="flex-1 min-w-0 truncate text-sm text-state-ok">
                {move || if saved_note.get() { tr().bfo_saved } else { "" }}
            </span>
            {move || if running.get() {
                view! {
                    <button type="button" class="btn btn-danger h-11 px-5 font-semibold" on:click=stop>
                        {move || format!("\u{25A0} {}", tr().bfo_stop)}
                    </button>
                }.into_any()
            } else {
                view! {
                    <button type="button" class="btn btn-primary h-11 px-5 font-semibold"
                            disabled=move || !can_run.get() on:click=run>
                        {move || format!("\u{25B6}\u{FE0E} {}", tr().bfo_run)}
                    </button>
                }.into_any()
            }}
            <button type="button" class="btn btn-primary h-11 px-4 font-semibold"
                    disabled=move || !can_save.get() on:click=save>
                {move || tr().bfo_save}
            </button>
        </div>
    }
}

/// One filter: include / runs while idle, the autofocus positions, average
/// and new offset once run, and whether to save that offset.
fn filter_card(
    i: usize,
    data: RwSignal<Option<FilterOffsetsData>>,
    running: Memo<bool>,
    has_results: Memo<bool>,
    skipped: RwSignal<HashSet<String>>,
    send_settings: impl Fn(&dyn Fn(&mut FilterOffsetsData)) + Copy + 'static,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let row = move || data.with(|d| d.as_ref().and_then(|d| d.filters.get(i).cloned()).unwrap_or_default());
    let is_ref = move || data.with(|d| d.as_ref().is_some_and(|d| d.filters.get(i).is_some_and(|f| f.filter == d.ref_filter)));
    let frozen = move || running.get() || has_results.get();
    let set_runs = move |runs: u32| send_settings(&|d| { if let Some(f) = d.filters.get_mut(i) { f.num_focus_runs = runs.min(10); } });
    let toggle_include = move |_| { if row().num_focus_runs > 0 { set_runs(0) } else { set_runs(DEFAULT_RUNS) } };
    let dec = move |_| set_runs(row().num_focus_runs.saturating_sub(1));
    let inc = move |_| set_runs(row().num_focus_runs + 1);
    let included = move || row().num_focus_runs > 0;
    let dec_off = move || frozen() || row().num_focus_runs <= u32::from(is_ref());
    let inc_off = move || frozen() || row().num_focus_runs >= 10;
    let saved = move || !skipped.with(|s| s.contains(&row().filter));
    let has_run = move || !row().af_runs.is_empty();

    view! {
        <div class=CARD>
            <div class="flex items-center gap-2 min-w-0">
                <span class="min-w-0 truncate font-semibold text-text-blue-bright">{move || row().filter}</span>
                <Show when=is_ref>
                    <span class="shrink-0 text-xs text-accent-cyan" title=move || tr().bfo_ref_filter>"\u{2605}"</span>
                </Show>
                <span class="ml-auto shrink-0 text-xs text-text-muted">{move || tr().bfo_current_offset}</span>
                <span class="shrink-0 font-mono text-sm text-text-dim">{move || signed(row().offset)}</span>
            </div>
            <div class="flex flex-wrap items-center gap-x-4 gap-y-1">
                <button type="button"
                        class=move || if included() { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                        disabled=move || frozen() || is_ref()
                        on:click=toggle_include>
                    {move || tr().bfo_selected}
                </button>
                <div class=format!("{ROW} flex-1 min-w-[200px]")>
                    <span class=format!("{LABEL} flex-1")>{move || tr().bfo_runs}</span>
                    <button type="button" class="btn-icon" disabled=dec_off on:click=dec>"\u{2212}"</button>
                    <span class="w-7 text-center font-mono text-base">{move || row().num_focus_runs}</span>
                    <button type="button" class="btn-icon" disabled=inc_off on:click=inc>"+"</button>
                </div>
            </div>
            <Show when=has_run>
                <div class="flex flex-col gap-1.5 pt-1 border-t border-border-base">
                    <div class="flex flex-wrap items-center gap-1.5">
                        <span class=CARD_TITLE>{move || tr().bfo_af_runs}</span>
                        {move || row().af_runs.into_iter().map(|p| view! {
                            <span class="chip h-7 px-2 font-mono text-xs">{p.to_string()}</span>
                        }).collect::<Vec<_>>()}
                    </div>
                    <div class="grid grid-cols-2 gap-2">
                        <div class="rounded-lg bg-bg-elev-2 border border-border-base px-2.5 py-1.5 flex flex-col">
                            <span class=CARD_TITLE>{move || tr().bfo_average}</span>
                            <span class="font-mono text-sm text-text-dim">{move || row().average.to_string()}</span>
                        </div>
                        <div class="rounded-lg bg-bg-elev-2 border border-border-base px-2.5 py-1.5 flex flex-col">
                            <span class=CARD_TITLE>{move || tr().bfo_new_offset}</span>
                            <span class="font-mono text-base font-semibold text-text-blue-bright">{move || signed(row().new_offset)}</span>
                        </div>
                    </div>
                    <Show when=move || row().save && !running.get()>
                        <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                            <input type="checkbox" class=CHECK
                                   prop:checked=saved
                                   on:change=move |ev| {
                                       let on = event_target_checked(&ev);
                                       let name = row().filter;
                                       skipped.update(|s| { if on { s.remove(&name); } else { s.insert(name); } });
                                   } />
                            <span class=LABEL>{move || tr().bfo_save_row}</span>
                        </label>
                    </Show>
                </div>
            </Show>
        </div>
    }
}
