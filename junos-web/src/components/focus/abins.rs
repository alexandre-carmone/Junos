//! Focus tab: the Aberration Inspector — a card (state, Start/Stop, the last
//! run's one-line verdict) and a sheet with the full result: the 3×3 tile
//! map, backfocus, sensor tilt, the tilt plate's screw turns (drawn in
//! `abins_plate.rs`), the per-tile table and the settings.
//!
//! The wire is a KStars fork's `focus_abins_*` (see CLAUDE.md): the status
//! and result land in `FocusSnapshot` through the store; a setting is sent
//! as `focus_abins_set_all_settings {key: value}` and KStars echoes the
//! effective map under `focus_abins_get_all_settings` — an invalid value is
//! dropped silently, so the echo is what the sheet shows. Stop is the Focus
//! module's `focus_stop` (the run is an autofocus).
//!
//! The tilt plate card is recomputed from the settings echo on every change
//! (`tilt_calc.rs`), like the desktop Tilt Correction Advisory; the result's
//! own `tiltPlate` is only the fallback before the settings have loaded.
//!
//! Once a run has finished (`canReanalyse`), `focus_abins_reanalyse` redoes
//! its analysis with other tiles excluded, another tile selection or
//! optCentres — KStars pushes the new result, same run, `reanalysed: true`.
//! A tap on the tile map selects a tile (shared with the other cards), the
//! Exclude / Include button toggles it, and a tile selection or optCentres
//! change in the settings re-analyses too.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{setting_row, sheet, CARD, CARD_TITLE, CHECK, LABEL, NUM, SELECT};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};
use crate::ws::{AbInsResult, AbInsStatus, SendCmd};
use crate::ws_helpers::send_cmd;

use super::{abins_curves, abins_plate, abins_sensor, tilt_calc};

type Label = fn(&'static Translations) -> &'static str;

/// KStars' thread presets (tiltcorrectionwidget.cpp:320), by `tiltThreadIndex`.
/// The headless setter does not couple the index to the pitch, so a preset
/// pick sends both; Custom (9) sends the index alone.
const THREADS: [(&str, f64); 10] = [
    ("M2.5", 0.45), ("M3", 0.50), ("M3 fine", 0.35), ("M4", 0.70), ("M4 fine", 0.50),
    ("M5", 0.80), ("M5 fine", 0.50), ("M6", 1.00), ("Wanderer ETA", 0.50), ("Custom", 0.0),
];

fn reason_text(code: &str, tr: &'static Translations) -> &'static str {
    match code {
        "noFocuser" => tr.abins_reason_no_focuser,
        "focuserNotConnected" => tr.abins_reason_focuser_not_connected,
        "noCamera" => tr.abins_reason_no_camera,
        "cameraNotConnected" => tr.abins_reason_camera_not_connected,
        "focuserNotAbsolute" => tr.abins_reason_focuser_not_absolute,
        "algorithmNotLinear1Pass" => tr.abins_reason_not_linear1pass,
        "maskNotMosaic" => tr.abins_reason_mask_not_mosaic,
        "curveFitQuadratic" => tr.abins_reason_curve_quadratic,
        "inspectorRunning" => tr.abins_reason_inspector_running,
        "autofocusRunning" => tr.abins_reason_autofocus_running,
        "adjustFocusRunning" => tr.abins_reason_adjust_running,
        "adaptiveFocusRunning" => tr.abins_reason_adaptive_running,
        "focusLoopRunning" => tr.abins_reason_loop_running,
        "buildOffsetsRunning" => tr.abins_reason_bfo_running,
        _ => tr.abins_reason_unknown,
    }
}

fn warning_text(code: &str, tr: &'static Translations) -> &'static str {
    match code {
        "insufficientDatapoints" => tr.abins_warn_datapoints,
        "datapointMismatch" => tr.abins_warn_mismatch,
        "tileOffsetsUnavailable" => tr.abins_warn_offsets,
        _ => tr.abins_reason_unknown,
    }
}

fn advice_text(advice: &str, tr: &'static Translations) -> &'static str {
    match advice {
        "nearer" => tr.abins_advice_nearer,
        "away" => tr.abins_advice_away,
        _ => tr.abins_advice_none,
    }
}

/// State line and badge class from the status (`running` first, then the
/// last run's outcome).
fn state_of(status: &Option<AbInsStatus>, tr: &'static Translations) -> (String, &'static str) {
    let Some(s) = status else { return (tr.abins_state_idle.to_string(), "badge") };
    if s.running {
        return (format!("{} · {} {}", tr.abins_state_running, tr.abins_run, s.last_run), "badge badge--warn");
    }
    match s.last_run_state.as_str() {
        "complete" => (format!("{} · {} {}", tr.abins_state_complete, tr.abins_run, s.last_run), "badge badge--ok"),
        "aborted" => (format!("{} · {} {}", tr.abins_state_aborted, tr.abins_run, s.last_run), "badge badge--err"),
        _ => (tr.abins_state_idle.to_string(), "badge"),
    }
}

/// The ISO UTC timestamp in the browser's locale.
pub(super) fn local_time(iso: &str) -> String {
    if iso.is_empty() {
        return String::new();
    }
    let d = web_sys::js_sys::Date::new(&iso.into());
    if d.get_time().is_nan() {
        return iso.to_string();
    }
    d.to_locale_string("default", &wasm_bindgen::JsValue::UNDEFINED).into()
}

/// Colour of a tile's delta against the CFZ: within half of it, within it,
/// beyond. Neutral when the run has no CFZ.
pub(super) fn delta_class(delta: f64, cfz: f64) -> &'static str {
    if cfz <= 0.0 {
        "text-text-blue"
    } else if delta.abs() <= cfz / 2.0 {
        "text-state-ok"
    } else if delta.abs() <= cfz {
        "text-state-warn"
    } else {
        "text-state-err"
    }
}

/// Each tile's colour in the charts, by `id` (TL T TR / L C R / BL B BR):
/// accents only, the `state-*` colours keep their CFZ meaning.
const TILE_COLORS: [&str; 9] = [
    "var(--accent-cyan)", "var(--text-blue)", "var(--accent-amber)",
    "var(--accent-violet)", "var(--text-blue-bright)", "var(--accent-green)",
    "var(--band-sii)", "var(--accent-cyan-dim)", "var(--accent-green-soft)",
];

pub(super) fn tile_color(id: i64) -> &'static str {
    usize::try_from(id).ok().and_then(|i| TILE_COLORS.get(i)).copied().unwrap_or("var(--text-muted)")
}

/// The re-analysis refusal codes (focus.cpp `reanalyseAbIns`); any other
/// text is shown as it is.
fn reanalyse_error_text(code: &str, tr: &'static Translations) -> String {
    match code {
        "running" => tr.abins_rerr_running.to_string(),
        "noResult" => tr.abins_rerr_no_result.to_string(),
        other => format!("{} {other}", tr.abins_rerr_refused),
    }
}

/// Redo the last run's analysis with these settings (KStars' `Options` stay
/// as they are); the reply is the pushed result, or a refused status.
fn reanalyse(send: &SendCmd, exclude: Vec<i64>, tile_selection: &str, opt_centres: bool) {
    send_cmd(send, "focus_abins_reanalyse", json!({
        "exclude": exclude,
        "tileSelection": tile_selection,
        "optCentres": opt_centres,
    }));
}

fn excluded_ids(r: &AbInsResult) -> Vec<i64> {
    r.tiles.iter().filter(|t| t.excluded).map(|t| t.id).collect()
}

// ── Card ─────────────────────────────────────────────────────────────────

/// The Focus tab's card: state badge, Start (disabled with the blockers as a
/// hint) or Stop, the refusal text, the last result's verdict and Details.
pub fn abins_card(
    status: Memo<Option<AbInsStatus>>,
    result: Memo<Option<AbInsResult>>,
    send: SendCmd,
    open: RwSignal<bool>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let running = move || status.with(|s| s.as_ref().is_some_and(|s| s.running));
    let can_start = move || status.with(|s| s.as_ref().is_some_and(|s| s.can_start));
    let blockers = move || status.with(|s| {
        let tr = tr();
        let r: Vec<&str> = s.as_ref().map(|s| s.reasons.iter().map(|c| reason_text(c, tr)).collect()).unwrap_or_default();
        if r.is_empty() { String::new() } else { format!("{} {}", tr.abins_cannot_start, r.join(", ")) }
    });
    let on_click = move |_| {
        let ty = if running() { "focus_stop" } else { "focus_abins_start" };
        send_cmd(&send, ty, json!({}));
    };
    let summary = move || result.with(|r| {
        let tr = tr();
        let Some(r) = r else { return tr.abins_no_result.to_string() };
        let mut parts = vec![format!("{} {}", tr.abins_run, r.run)];
        if r.backfocus.ok {
            parts.push(format!("{} {:+.0} µm", tr.abins_backfocus, r.backfocus.delta_microns));
        }
        if r.tilt.ok {
            parts.push(format!("{} {:.2} %", tr.abins_tilt, r.tilt.diagonal_percent));
        }
        parts.join(" · ")
    });

    view! {
        <div class=CARD>
            <div class="flex items-center gap-2">
                <span class=format!("{CARD_TITLE} flex-1 min-w-0 truncate")>{move || tr().abins_title}</span>
                <span class=move || status.with(|s| state_of(s, tr()).1)>
                    {move || status.with(|s| state_of(s, tr()).0)}
                </span>
            </div>
            <button
                class=move || if running() { "btn btn-danger w-full h-11" } else { "btn btn-primary w-full h-11" }
                disabled=move || !running() && !can_start()
                title=blockers
                on:click=on_click
            >
                {move || if running() { format!("\u{25A0} {}", tr().stop) } else { tr().abins_start.to_string() }}
            </button>
            <Show when=move || !running() && !can_start()>
                <div class="text-xs text-text-muted leading-tight">{blockers}</div>
            </Show>
            {move || status.with(|s| s.as_ref().and_then(|s| s.start_error.clone())).map(|e| view! {
                <div class="text-xs text-state-err leading-tight">{e}</div>
            })}
            <div class="flex items-center gap-2">
                <span class="flex-1 min-w-0 text-sm text-text-blue leading-tight">{summary}</span>
                <button class="btn btn-ghost h-9 md:h-7 px-3 shrink-0"
                        disabled=move || result.with(|r| r.is_none())
                        on:click=move |_| open.set(true)>
                    {move || tr().abins_details}
                </button>
            </div>
        </div>
    }
}

// ── Sheet ────────────────────────────────────────────────────────────────

/// The sheet: the result's cards, then the settings.
pub fn abins_sheet(
    status: Memo<Option<AbInsStatus>>,
    result: Memo<Option<AbInsResult>>,
    settings: Memo<Value>,
    send: SendCmd,
    open: RwSignal<bool>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    // Out here, not in `result_cards`: those are rebuilt on every result or
    // settings echo, and the selected tile must outlive them.
    let selected = RwSignal::new(None::<i64>);
    let form = Form { settings, status, result, send: send.clone(), lang };
    let send_sv = StoredValue::new(send);

    view! {
        <Show when=move || open.get()>
            {sheet(move || tr().abins_title, move || open.set(false), view! {
                <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 \
                            pb-[max(0.75rem,env(safe-area-inset-bottom))] flex flex-col gap-3 text-sm">
                    {move || result.with(|r| match r {
                        Some(r) => result_cards(r, &settings.get(), tr(), status, selected, send_sv.get_value()).into_any(),
                        None => view! { <div class="text-text-faint py-2">{tr().abins_no_result}</div> }.into_any(),
                    })}
                    {form.card()}
                </div>
            })}
        </Show>
    }
}

/// A label · value line inside a card.
pub(super) fn fact(label: &'static str, value: String) -> impl IntoView {
    view! {
        <div class="flex items-baseline justify-between gap-3 min-h-7">
            <span class=LABEL>{label}</span>
            <span class="font-mono text-text-blue-bright text-right">{value}</span>
        </div>
    }
}

fn result_cards(
    r: &AbInsResult,
    settings: &Value,
    tr: &'static Translations,
    status: Memo<Option<AbInsStatus>>,
    selected: RwSignal<Option<i64>>,
    send: SendCmd,
) -> impl IntoView + use<> {
    let cfz = r.cfz_microns();

    // Header: run, train, time, fit, datapoints, CFZ, warnings.
    let warnings = r.warnings.iter().map(|w| view! {
        <span class="chip h-7 px-2 text-xs text-state-warn">{warning_text(w, tr)}</span>
    }).collect_view();
    let header = view! {
        <div class=CARD>
            <div class="flex items-center gap-2">
                <span class=format!("{CARD_TITLE} flex-1")>{format!("{} · {} {}", tr.abins_result_section, tr.abins_run, r.run)}</span>
                {r.reanalysed.then(|| view! {
                    <span class="badge" title=tr.abins_reanalysed_hint>{tr.abins_reanalysed}</span>
                })}
                <span class=if r.results_ok { "badge badge--ok" } else { "badge badge--warn" }>
                    {if r.results_ok { tr.abins_results_ok } else { tr.abins_results_not_ok }}
                </span>
            </div>
            {r.reanalysed.then(|| view! {
                <div class="text-xs text-text-muted leading-tight">{tr.abins_reanalysed_hint}</div>
            })}
            {fact(tr.abins_train, r.train.clone())}
            {fact(tr.abins_time, local_time(&r.timestamp))}
            {fact(tr.abins_curve_fit, r.curve_fit.clone())}
            {fact(tr.abins_datapoints, r.datapoints.to_string())}
            {fact(tr.abins_cfz, format!("{:.0} µm · {:.0} {}", cfz, r.cfz_steps, tr.abins_steps))}
            {(!r.warnings.is_empty()).then(|| view! { <div class="flex flex-wrap gap-1.5">{warnings}</div> })}
        </div>
    };

    // Tile map: 3×3 in `id` order (row * 3 + col), the centre is the reference.
    // A tap selects a tile (again: deselects); excluded tiles are dashed.
    let mut tiles = r.tiles.clone();
    tiles.sort_by_key(|t| t.id);
    let cells = tiles.iter().map(|t| {
        let id = t.id;
        let centre = id == 4;
        let cls = if !t.used { "opacity-40" } else if !t.fit || t.excluded { "opacity-60" } else { "" };
        let border = if t.excluded { "border-dashed" } else { "" };
        let name_cls = if !t.fit || t.excluded { "line-through" } else { "" };
        let delta = if centre { tr.abins_ref.to_string() } else { format!("{:+.0} µm", t.delta_microns) };
        let delta_cls = if centre || !t.fit { "text-text-muted" } else { delta_class(t.delta_microns, cfz) };
        let ring = move || if selected.get() == Some(id) { "border-accent-cyan ring-1 ring-accent-cyan" } else { "border-border-base" };
        view! {
            <button type="button"
                    class=move || format!("rounded-lg bg-bg-elev-1 border {} {border} px-2 py-1.5 flex flex-col items-center {cls}", ring())
                    on:click=move |_| selected.update(|s| *s = if *s == Some(id) { None } else { Some(id) })>
                <span class=format!("text-xs uppercase tracking-[0.06em] text-text-muted {name_cls}")>{t.name.clone()}</span>
                <span class="font-mono text-base leading-tight text-text-blue-bright">{t.solution.to_string()}</span>
                <span class=format!("font-mono text-xs {delta_cls}")>{delta}</span>
                {t.excluded.then(|| view! { <span class="text-[10px] text-state-warn">{tr.abins_excluded}</span> })}
            </button>
        }
    }).collect_view();

    // Exclude / Include the selected tile, Reset: re-analyse the same run.
    let can = move || status.with(|s| s.as_ref().is_some_and(|s| s.can_reanalyse));
    let excluded = excluded_ids(r);
    let names: Vec<(i64, String, bool)> = tiles.iter().map(|t| (t.id, t.name.clone(), t.excluded)).collect();
    let pick = move || selected.get().and_then(|id| names.iter().find(|n| n.0 == id).cloned());
    let toggle_label = {
        let pick = pick.clone();
        move || match pick() {
            Some((_, name, true)) => format!("{} {name}", tr.abins_include),
            Some((_, name, false)) => format!("{} {name}", tr.abins_exclude),
            None => tr.abins_exclude.to_string(),
        }
    };
    let on_toggle = {
        let (send, excluded, sel, opt) = (send.clone(), excluded.clone(), r.tile_selection.clone(), r.opt_centres);
        move |_| {
            let Some(id) = selected.get_untracked() else { return };
            let mut ex = excluded.clone();
            match ex.iter().position(|&e| e == id) {
                Some(i) => { ex.remove(i); }
                None => { ex.push(id); ex.sort(); }
            }
            reanalyse(&send, ex, &sel, opt);
        }
    };
    let on_reset = {
        let (send, sel, opt) = (send.clone(), r.tile_selection.clone(), r.opt_centres);
        move |_| reanalyse(&send, Vec::new(), &sel, opt)
    };
    let any_excluded = !excluded.is_empty();
    // The status is sticky: a refusal replayed after the run became
    // re-analysable again is stale, so it shows only while it still holds.
    let reanalyse_error = move || status.with(|s| {
        s.as_ref().filter(|s| s.running || !s.can_reanalyse).and_then(|s| s.reanalyse_error.as_deref()).map(|e| reanalyse_error_text(e, tr))
    });
    let tile_map = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_tile_map}</span>
            <div class="grid grid-cols-3 gap-2">{cells}</div>
            <div class="flex items-center gap-2">
                <button type="button" class="btn btn-ghost h-9 md:h-7 px-3 flex-1 min-w-0 truncate"
                        disabled=move || !can() || pick().is_none()
                        on:click=on_toggle>
                    {toggle_label}
                </button>
                <button type="button" class="btn btn-ghost h-9 md:h-7 px-3 shrink-0"
                        disabled=move || !can() || !any_excluded
                        on:click=on_reset>
                    {tr.reset}
                </button>
            </div>
            {move || reanalyse_error().map(|e| view! { <div class="text-xs text-state-err leading-tight">{e}</div> })}
            <div class="text-xs text-text-muted leading-tight">
                {tr.abins_tile_map_hint} " "
                {move || if can() { tr.abins_exclude_hint } else { tr.abins_reanalyse_off }}
            </div>
        </div>
    };

    let curves = abins_curves::curves_card(r, tr, selected);
    let sensor = abins_sensor::sensor_card(r, tr, selected);

    // Backfocus.
    let b = &r.backfocus;
    let backfocus = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_backfocus}</span>
            {if b.ok {
                view! {
                    <div class="flex items-baseline gap-3">
                        <span class=format!("font-mono text-2xl {}", delta_class(b.delta_microns, cfz))>{format!("{:+.0} µm", b.delta_microns)}</span>
                        <span class="text-text-blue">{advice_text(&b.advice, tr)}</span>
                    </div>
                }.into_any()
            } else {
                view! { <div class="text-text-faint">{tr.abins_not_ok}</div> }.into_any()
            }}
        </div>
    };

    // Tilt.
    let t_ = &r.tilt;
    let tilt_row = |label: &'static str, microns: f64, percent: f64| view! {
        <div class="flex items-baseline justify-between gap-3 min-h-7">
            <span class=LABEL>{label}</span>
            <span class="font-mono text-text-blue-bright">{format!("{microns:+.0} µm · {percent:.2} %")}</span>
        </div>
    };
    let tilt = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_tilt}</span>
            {if t_.ok {
                view! {
                    {tilt_row(tr.abins_tilt_lr, t_.lr_microns, t_.lr_percent)}
                    {tilt_row(tr.abins_tilt_tb, t_.tb_microns, t_.tb_percent)}
                    {tilt_row(tr.abins_tilt_diag, t_.diagonal_microns, t_.diagonal_percent)}
                }.into_any()
            } else {
                view! { <div class="text-text-faint">{tr.abins_not_ok}</div> }.into_any()
            }}
        </div>
    };

    // Tilt plate: the screws to turn, under the current settings (the
    // result's own advice only until the settings echo has arrived).
    let advice = tilt_calc::plate_advice(r, settings).or_else(|| r.tilt_plate.clone());
    let plate_present = settings["tiltPlatePresent"].as_bool().unwrap_or(r.plate_present);
    let plate = abins_plate::plate_card(advice.as_ref(), r.tilt.ok, plate_present, tr);

    // Per-tile table.
    let rows = tiles.iter().map(|t| view! {
        <tr class=if t.used && !t.excluded { "" } else { "opacity-40" }>
            <td class="py-1 pr-2 font-mono">
                <span class=if t.excluded { "line-through" } else { "" }>{t.name.clone()}</span>
                {t.excluded.then(|| view! { <span class="ml-1 text-[10px] text-state-warn">{tr.abins_excluded}</span> })}
            </td>
            <td class="py-1 pr-2 text-center">{if t.fit { "\u{2713}" } else { "\u{2014}" }}</td>
            <td class="py-1 pr-2 font-mono text-right">{t.solution.to_string()}</td>
            <td class="py-1 pr-2 font-mono text-right">{format!("{:+}", t.delta_ticks)}</td>
            <td class=format!("py-1 pr-2 font-mono text-right {}", delta_class(t.delta_microns, cfz))>{format!("{:+.0}", t.delta_microns)}</td>
            <td class="py-1 pr-2 font-mono text-right">{format!("{}–{}", t.min_stars, t.max_stars)}</td>
            <td class="py-1 pr-2 font-mono text-right">{format!("{:.3}", t.r2)}</td>
            <td class="py-1 font-mono text-right">{format!("{:.2}", t.min_measure)}</td>
        </tr>
    }).collect_view();
    let table = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_table}</span>
            <div class="overflow-x-auto">
                <table class="w-full text-sm whitespace-nowrap">
                    <thead class="text-xs uppercase tracking-[0.06em] text-text-muted">
                        <tr>
                            <th class="text-left font-normal pr-2">{tr.abins_col_tile}</th>
                            <th class="text-center font-normal pr-2">{tr.abins_col_fit}</th>
                            <th class="text-right font-normal pr-2">{tr.abins_col_solution}</th>
                            <th class="text-right font-normal pr-2">{tr.abins_col_dticks}</th>
                            <th class="text-right font-normal pr-2">"Δ µm"</th>
                            <th class="text-right font-normal pr-2">{tr.abins_col_stars}</th>
                            <th class="text-right font-normal pr-2">"R²"</th>
                            <th class="text-right font-normal">{r.y_axis_label.clone()}</th>
                        </tr>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>
            </div>
        </div>
    };

    view! { {header} {tile_map} {curves} {backfocus} {tilt} {sensor} {plate} {table} }
}

// ── Settings ─────────────────────────────────────────────────────────────

/// The settings card: KStars' map, each key sent on change and echoed whole.
#[derive(Clone)]
struct Form {
    settings: Memo<Value>,
    status: Memo<Option<AbInsStatus>>,
    result: Memo<Option<AbInsResult>>,
    send: SendCmd,
    lang: RwSignal<Lang>,
}

impl Form {
    /// Send a setting; a tile selection or optCentres change also
    /// re-analyses the last run, keeping its exclusions, when KStars can.
    fn put(&self, payload: Value) {
        send_cmd(&self.send, "focus_abins_set_all_settings", payload.clone());
        if payload.get("abInsTileSelection").is_none() && payload.get("abInsOptCentres").is_none() {
            return;
        }
        if !self.status.with_untracked(|s| s.as_ref().is_some_and(|s| s.can_reanalyse)) {
            return;
        }
        let Some((exclude, sel, opt)) = self.result.with_untracked(|r| {
            r.as_ref().map(|r| (excluded_ids(r), r.tile_selection.clone(), r.opt_centres))
        }) else { return };
        let (sel, opt) = self.settings.with_untracked(|v| (
            v["abInsTileSelection"].as_str().map(str::to_string).unwrap_or(sel),
            v["abInsOptCentres"].as_bool().unwrap_or(opt),
        ));
        let sel = payload["abInsTileSelection"].as_str().map(str::to_string).unwrap_or(sel);
        let opt = payload["abInsOptCentres"].as_bool().unwrap_or(opt);
        reanalyse(&self.send, exclude, &sel, opt);
    }

    fn label(&self, label: Label) -> impl Fn() -> &'static str + Copy + Send + 'static + use<> {
        let lang = self.lang;
        move || label(t(lang.get()))
    }

    /// Radius, pitch and thread are KStars' fixed ETA geometry while a
    /// Wanderer ETA is connected on a 3-point plate (rejected if sent).
    fn eta_locked(&self) -> impl Fn() -> bool + Copy + use<> {
        let settings = self.settings;
        move || settings.with(|v| v["etaConnected"].as_bool() == Some(true) && v["tiltPlateType"].as_i64().unwrap_or(0) == 0)
    }

    fn check(&self, key: &'static str, label: Label) -> impl IntoView + use<> {
        let (f, settings) = (self.clone(), self.settings);
        view! {
            <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                <input type="checkbox" class=CHECK
                       prop:checked=move || settings.with(|v| v[key].as_bool().unwrap_or(false))
                       on:change=move |ev| f.put(json!({ key: event_target_checked(&ev) })) />
                <span class=LABEL>{self.label(label)}</span>
            </label>
        }
    }

    fn select_int(&self, key: &'static str, label: Label, options: Vec<(i64, Label)>) -> impl IntoView + use<> {
        let (f, settings, lang) = (self.clone(), self.settings, self.lang);
        let cur = move || settings.with(|v| v[key].as_i64().unwrap_or(0));
        setting_row(self.label(label), view! {
            <select class=SELECT on:change=move |ev| {
                if let Ok(n) = event_target_value(&ev).parse::<i64>() { f.put(json!({ key: n })) }
            }>
                {options.into_iter().map(|(n, l)| view! {
                    <option value=n.to_string() prop:selected=move || cur() == n>{move || l(t(lang.get()))}</option>
                }).collect_view()}
            </select>
        })
    }

    fn number(&self, key: &'static str, label: Label, min: &'static str, max: &'static str, step: &'static str, locked: bool) -> impl IntoView + use<> {
        let (f, settings) = (self.clone(), self.settings);
        setting_row(self.label(label), view! {
            <input type="number" min=min max=max step=step inputmode="decimal" class=NUM disabled=locked
                   prop:value=move || settings.with(|v| v[key].as_f64().unwrap_or(0.0)).to_string()
                   on:change=move |ev| {
                       if let Ok(x) = event_target_value(&ev).trim().parse::<f64>() { f.put(json!({ key: x })) }
                   } />
        })
    }

    fn card(&self) -> impl IntoView + use<> {
        let (f, settings, lang) = (self.clone(), self.settings, self.lang);
        let tr = move || t(lang.get());
        let locked = self.eta_locked();
        let tile_sel = move || settings.with(|v| v["abInsTileSelection"].as_str().unwrap_or("outerCorners").to_string());
        let thread = move || settings.with(|v| v["tiltThreadIndex"].as_i64().unwrap_or(9));
        let f_sel = f.clone();
        let f_thread = f.clone();
        view! {
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().abins_settings_section}</span>
                {move || settings.with(|v| v.is_object()).then(|| view! {
                    {setting_row(f.label(|t| t.abins_tile_selection), view! {
                        <select class=SELECT on:change={let f = f_sel.clone(); move |ev| f.put(json!({ "abInsTileSelection": event_target_value(&ev) }))}>
                            {[("all", (|t: &'static Translations| t.abins_sel_all) as Label),
                              ("outerCorners", |t| t.abins_sel_outer),
                              ("innerDiamond", |t| t.abins_sel_inner)]
                                .into_iter().map(|(k, l)| view! {
                                    <option value=k prop:selected=move || tile_sel() == k>{move || l(t(lang.get()))}</option>
                                }).collect_view()}
                        </select>
                    })}
                    {f.check("abInsOptCentres", |t| t.abins_opt_centres)}
                    {f.check("tiltPlatePresent", |t| t.abins_plate_present)}
                    {f.select_int("tiltPlateType", |t| t.abins_plate_type, vec![(0, |t| t.abins_plate_3), (1, |t| t.abins_plate_4)])}
                    {f.select_int("tiltMode", |t| t.abins_mode, vec![(0, |t| t.abins_mode_relative), (1, |t| t.abins_mode_push)])}
                    {setting_row(f.label(|t| t.abins_thread), view! {
                        <select class=SELECT disabled=locked() on:change={let f = f_thread.clone(); move |ev| {
                            let Ok(i) = event_target_value(&ev).parse::<i64>() else { return };
                            let pitch = THREADS.get(i as usize).map(|t| t.1).unwrap_or(0.0);
                            if pitch > 0.0 {
                                f.put(json!({ "tiltThreadIndex": i, "tiltScrewPitch": pitch }));
                            } else {
                                f.put(json!({ "tiltThreadIndex": i }));
                            }
                        }}>
                            {THREADS.iter().enumerate().map(|(i, (name, pitch))| {
                                let text = if *pitch > 0.0 { format!("{name} · {pitch:.2} mm") } else { name.to_string() };
                                view! { <option value=i.to_string() prop:selected=move || thread() == i as i64>{text}</option> }
                            }).collect_view()}
                        </select>
                    })}
                    {f.number("tiltRadius", |t| t.abins_radius, "5", "100", "0.5", locked())}
                    {f.number("tiltScrewPitch", |t| t.abins_pitch, "0.1", "2", "0.05", locked())}
                    {f.number("tiltRotation", |t| t.abins_rotation, "-180", "180", "1", false)}
                    <Show when=locked>
                        <div class="text-xs text-text-muted leading-tight">{move || tr().abins_eta_locked}</div>
                    </Show>
                })}
                {move || (!settings.with(|v| v.is_object())).then(|| view! {
                    <div class="text-text-faint py-2">{move || tr().abins_settings_not_loaded}</div>
                })}
            </div>
        }
    }
}
