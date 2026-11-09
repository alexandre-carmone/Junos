//! Focus tab: the Aberration Inspector — a card (state, Start/Stop, the last
//! run's one-line verdict) and a sheet with the full result: the 3×3 tile
//! map, backfocus, sensor tilt, the tilt plate's screw turns, the per-tile
//! table and the settings.
//!
//! The wire is a KStars fork's `focus_abins_*` (see CLAUDE.md): the status
//! and result land in `FocusSnapshot` through the store; a setting is sent
//! as `focus_abins_set_all_settings {key: value}` and KStars echoes the
//! effective map under `focus_abins_get_all_settings` — an invalid value is
//! dropped silently, so the echo is what the sheet shows. Stop is the Focus
//! module's `focus_stop` (the run is an autofocus).

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{setting_row, sheet, CARD, CARD_TITLE, CHECK, LABEL, NUM, SELECT};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};
use crate::ws::{AbInsResult, AbInsStatus, SendCmd};
use crate::ws_helpers::send_cmd;

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

/// A screw's position on the plate, rear view (tiltcorrectioncalc.cpp).
fn position_text(pos: &str, tr: &'static Translations) -> &'static str {
    match pos {
        "right" => tr.abins_pos_right,
        "upperRight" => tr.abins_pos_upper_right,
        "top" => tr.abins_pos_top,
        "upperLeft" => tr.abins_pos_upper_left,
        "left" => tr.abins_pos_left,
        "lowerLeft" => tr.abins_pos_lower_left,
        "bottom" => tr.abins_pos_bottom,
        "lowerRight" => tr.abins_pos_lower_right,
        other => Box::leak(other.to_string().into_boxed_str()),
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
fn local_time(iso: &str) -> String {
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
fn delta_class(delta: f64, cfz: f64) -> &'static str {
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
    result: Memo<Option<AbInsResult>>,
    settings: Memo<Value>,
    send: SendCmd,
    open: RwSignal<bool>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let form = Form { settings, send, lang };

    view! {
        <Show when=move || open.get()>
            {sheet(move || tr().abins_title, move || open.set(false), view! {
                <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 \
                            pb-[max(0.75rem,env(safe-area-inset-bottom))] flex flex-col gap-3 text-sm">
                    {move || result.with(|r| match r {
                        Some(r) => result_cards(r, tr()).into_any(),
                        None => view! { <div class="text-text-faint py-2">{tr().abins_no_result}</div> }.into_any(),
                    })}
                    {form.card()}
                </div>
            })}
        </Show>
    }
}

/// A label · value line inside a card.
fn fact(label: &'static str, value: String) -> impl IntoView {
    view! {
        <div class="flex items-baseline justify-between gap-3 min-h-7">
            <span class=LABEL>{label}</span>
            <span class="font-mono text-text-blue-bright text-right">{value}</span>
        </div>
    }
}

fn result_cards(r: &AbInsResult, tr: &'static Translations) -> impl IntoView + use<> {
    let cfz = r.cfz_microns();

    // Header: run, train, time, fit, datapoints, CFZ, warnings.
    let warnings = r.warnings.iter().map(|w| view! {
        <span class="chip h-7 px-2 text-xs text-state-warn">{warning_text(w, tr)}</span>
    }).collect_view();
    let header = view! {
        <div class=CARD>
            <div class="flex items-center gap-2">
                <span class=format!("{CARD_TITLE} flex-1")>{format!("{} · {} {}", tr.abins_result_section, tr.abins_run, r.run)}</span>
                <span class=if r.results_ok { "badge badge--ok" } else { "badge badge--warn" }>
                    {if r.results_ok { tr.abins_results_ok } else { tr.abins_results_not_ok }}
                </span>
            </div>
            {fact(tr.abins_train, r.train.clone())}
            {fact(tr.abins_time, local_time(&r.timestamp))}
            {fact(tr.abins_curve_fit, r.curve_fit.clone())}
            {fact(tr.abins_datapoints, r.datapoints.to_string())}
            {fact(tr.abins_cfz, format!("{:.0} µm · {:.0} {}", cfz, r.cfz_steps, tr.abins_steps))}
            {(!r.warnings.is_empty()).then(|| view! { <div class="flex flex-wrap gap-1.5">{warnings}</div> })}
        </div>
    };

    // Tile map: 3×3 in `id` order (row * 3 + col), the centre is the reference.
    let mut tiles = r.tiles.clone();
    tiles.sort_by_key(|t| t.id);
    let cells = tiles.iter().map(|t| {
        let centre = t.id == 4;
        let cls = if !t.used { "opacity-40" } else if !t.fit { "line-through opacity-60" } else { "" };
        let delta = if centre { tr.abins_ref.to_string() } else { format!("{:+.0} µm", t.delta_microns) };
        let delta_cls = if centre || !t.fit { "text-text-muted" } else { delta_class(t.delta_microns, cfz) };
        view! {
            <div class=format!("rounded-lg bg-bg-elev-1 border border-border-base px-2 py-1.5 flex flex-col items-center {cls}")>
                <span class="text-xs uppercase tracking-[0.06em] text-text-muted">{t.name.clone()}</span>
                <span class="font-mono text-base leading-tight text-text-blue-bright">{t.solution.to_string()}</span>
                <span class=format!("font-mono text-xs {delta_cls}")>{delta}</span>
            </div>
        }
    }).collect_view();
    let tile_map = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_tile_map}</span>
            <div class="grid grid-cols-3 gap-2">{cells}</div>
            <div class="text-xs text-text-muted leading-tight">{tr.abins_tile_map_hint}</div>
        </div>
    };

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

    // Tilt plate: the screws to turn.
    let plate = view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_tilt_plate}</span>
            {match &r.tilt_plate {
                Some(p) => {
                    let screws = p.points.iter().map(|s| view! {
                        <tr>
                            <td class="py-1 pr-2 font-mono">{format!("#{}", s.index)}</td>
                            <td class="py-1 pr-2">{position_text(&s.position, tr)}</td>
                            <td class="py-1 pr-2 font-mono text-right">{format!("{:+.3}", s.adjustment_mm)}</td>
                            <td class="py-1 font-mono text-right">{format!("{:+.2}", s.turns)}</td>
                        </tr>
                    }).collect_view();
                    view! {
                        {fact(tr.abins_plate_type, format!("{} · {}",
                            if p.plate_type == 1 { tr.abins_plate_4 } else { tr.abins_plate_3 },
                            if p.mode == 1 { tr.abins_mode_push } else { tr.abins_mode_relative }))}
                        {fact(tr.abins_geometry, format!("r {:.0} mm · {:.2} mm/turn · {:.0}°", p.radius_mm, p.screw_pitch_mm, p.plate_angle_deg))}
                        {fact(tr.abins_total_tilt, format!("{:.0} µm", p.total_tilt_microns))}
                        {fact(tr.abins_max_adj, format!("{:.3} mm", p.max_adjustment_mm))}
                        <table class="w-full text-sm">
                            <thead class="text-xs uppercase tracking-[0.06em] text-text-muted">
                                <tr>
                                    <th class="text-left font-normal pr-2">{tr.abins_screw}</th>
                                    <th class="text-left font-normal pr-2">{tr.abins_position}</th>
                                    <th class="text-right font-normal pr-2">"mm"</th>
                                    <th class="text-right font-normal">{tr.abins_turns}</th>
                                </tr>
                            </thead>
                            <tbody>{screws}</tbody>
                        </table>
                        <div class="text-xs text-text-muted leading-tight">{tr.abins_plate_hint}</div>
                    }.into_any()
                }
                None => view! {
                    <div class="text-text-faint">
                        {if !r.tilt.ok { tr.abins_plate_no_tilt } else if !r.plate_present { tr.abins_no_plate } else { tr.abins_not_ok }}
                    </div>
                }.into_any(),
            }}
        </div>
    };

    // Per-tile table.
    let rows = tiles.iter().map(|t| view! {
        <tr class=if t.used { "" } else { "opacity-40" }>
            <td class="py-1 pr-2 font-mono">{t.name.clone()}</td>
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

    view! { {header} {tile_map} {backfocus} {tilt} {plate} {table} }
}

// ── Settings ─────────────────────────────────────────────────────────────

/// The settings card: KStars' map, each key sent on change and echoed whole.
#[derive(Clone)]
struct Form {
    settings: Memo<Value>,
    send: SendCmd,
    lang: RwSignal<Lang>,
}

impl Form {
    fn put(&self, payload: Value) {
        send_cmd(&self.send, "focus_abins_set_all_settings", payload);
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
