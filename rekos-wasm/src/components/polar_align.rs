//! Polar Alignment module UI — fullscreen tab.
//!
//! Wire protocol: see kstars/ekos/align/polaralignmentassistant.{h,cpp} and
//! the inbound handlers at kstars/ekos/ekoslive/message.cpp:1079-1263.
//!
//! Outbound (browser → KStars):
//!   - polar_start, polar_stop, polar_refresh {value: exposure},
//!     polar_refreshing_done, polar_slew_done
//!   - align_set_all_settings {settings: {pAHDirection, pAHRotation,
//!     pAHMountSpeed, pAHManualSlew, pAHExposure, pAHRefreshAlgorithm}}
//!   - align_get_all_settings (primed + refreshed from ws.rs)
//!
//! Inbound (KStars → browser): `new_polar_state` (partial: stage, message,
//! enabled, vector, updatedError*) and `align_get_all_settings` (settings map).
//!
//! Deliberately NOT wired: `polar_set_algorithm` (widget-name bug upstream,
//! message.cpp:1102 — use align_set_all_settings instead),
//! `polar_set_crosshair` / `polar_set_zoom` / `polar_reset_view` (no frame
//! view), `NEW_ALIGN_FRAME` (declared in commands.h:35 but never emitted).

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::compat::{MountSnapshot, PolarAlignSnapshot};
use crate::i18n::{Lang, Translations, t};
use crate::ws::{PolarVectorData, SendCmd};
use crate::ws_helpers::{send_cmd, dispatch_setting as ws_dispatch_setting};

/// Wire values for the direction dropdown (sent to KStars as-is).
const DIRECTION_WIRE: &[&str] = &["West", "East"];
fn direction_label(wire: &str, tr: &Translations) -> &'static str {
    match wire {
        "East" => tr.pa_east,
        _      => tr.pa_west,
    }
}

/// Wire values for the refresh-algorithm dropdown.
const ALGORITHM_WIRE: &[&str] = &[
    "Plate Solve",
    "Move Star",
    "Move Star & Calc Error",
];
fn algorithm_label(wire: &str, tr: &Translations) -> &'static str {
    match wire {
        "Move Star"              => tr.pa_algo_move_star,
        "Move Star & Calc Error" => tr.pa_algo_move_star_calc,
        _                        => tr.pa_algo_plate_solve,
    }
}

const DEFAULT_SPEED_OPTIONS: &[&str] = &[
    "1x", "2x", "4x", "8x", "16x", "32x", "64x", "128x", "256x", "Max",
];
fn speed_label(wire: &str, tr: &Translations) -> String {
    if wire == "Max" { tr.pa_speed_max.to_string() } else { wire.to_string() }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn dispatch_align_setting(send: &SendCmd, key: &str, value: serde_json::Value) {
    ws_dispatch_setting(send, "align_set_all_settings", Some("settings"), key, value);
}

fn stage_color(stage: &str) -> &'static str {
    match stage {
        "" | "Idle" => "#808090",
        "First Capture" | "First Solve"
        | "Second Capture" | "Second Solve"
        | "Third Capture" | "Third Solve" => "#88aaff",
        "First Rotation" | "Second Rotation"
        | "First Settle" | "Second Settle"
        | "Finding CP" | "Select Star" => "#ffd060",
        "Refreshing" | "Refresh Complete" => "#7affa0",
        _ => "#c0c0d0",
    }
}

// `enabled` from PAHEnabled means "PAA is available (FOV wide enough)", not
// "PAA is running". Stage alone decides whether we show the intro panel.
fn is_intro_stage(stage: &str) -> bool {
    stage.is_empty() || stage == "Idle"
}

fn is_progress_stage(stage: &str) -> bool {
    matches!(
        stage,
        "First Capture" | "First Solve" | "First Settle"
        | "Finding CP"
        | "Second Capture" | "Second Solve" | "Second Settle"
        | "Third Capture" | "Third Solve"
    )
}

fn is_rotation_stage(stage: &str) -> bool {
    stage == "First Rotation" || stage == "Second Rotation"
}

fn is_refresh_stage(stage: &str) -> bool {
    matches!(stage, "Select Star" | "Refreshing" | "Refresh Complete")
}

/// Format a degrees value as DMS-ish. PAA errors are typically small
/// (arcminutes), so switch to arcmin/arcsec below 1°. `-1` is the
/// solver-failure sentinel from `updatedErrorsChanged`.
fn format_deg_as_dms_small(deg: f64) -> String {
    if !deg.is_finite() || (deg - -1.0).abs() < 1e-9 {
        return "—".into();
    }
    let sign = if deg < 0.0 { "-" } else { "" };
    let abs = deg.abs();
    if abs >= 1.0 {
        let d = abs.trunc();
        let m = (abs - d) * 60.0;
        format!("{sign}{d:.0}°{m:04.1}'")
    } else if abs * 60.0 >= 1.0 {
        let m = (abs * 60.0).trunc();
        let s = ((abs * 60.0) - m) * 60.0;
        format!("{sign}{m:.0}'{s:04.1}\"")
    } else {
        format!("{sign}{:.2}\"", abs * 3600.0)
    }
}

fn settings_str(settings: &serde_json::Value, key: &str) -> Option<String> {
    settings.get(key).and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

fn settings_i64(settings: &serde_json::Value, key: &str) -> Option<i64> {
    settings
        .get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|x| x as i64)))
}

fn settings_f64(settings: &serde_json::Value, key: &str) -> Option<f64> {
    settings.get(key).and_then(|v| v.as_f64())
}

fn settings_bool(settings: &serde_json::Value, key: &str) -> Option<bool> {
    settings.get(key).and_then(|v| v.as_bool())
}

fn event_target_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

fn event_target_select(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlSelectElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

fn event_target_checked(ev: &web_sys::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}

/// Mirrors `PolarAlignmentAssistant::checkPAHForMeridianCrossing()`
/// (kstars/ekos/align/polaralignmentassistant.cpp:694-727). Returns true when
/// the selected direction+rotation combined with the current mount HA and
/// pier side would cause the three-capture sequence to traverse the meridian,
/// which can jam a GEM or force a mid-PAA flip.
fn would_cross_meridian(
    ha_deg: f64,
    dec_deg: f64,
    pier_side: Option<i32>,
    rotation_deg: i64,
    going_west: bool,
) -> bool {
    // Skip check near the pole (the meridian isn't meaningful there).
    if dec_deg.abs() > 88.0 {
        return false;
    }
    let mut ha = ha_deg;
    while ha < -180.0 { ha += 360.0; }
    while ha >  180.0 { ha -= 360.0; }
    let close_to_meridian = ha.abs() < 2.0 * rotation_deg as f64;
    if !close_to_meridian {
        return false;
    }
    match pier_side {
        Some(1)  => !going_west, // PIER_EAST → warn if slewing east
        Some(0)  =>  going_west, // PIER_WEST → warn if slewing west
        _        => true,        // PIER_UNKNOWN → warn whenever close
    }
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

#[component]
pub fn PolarAlignTab(
    #[prop(into)] polar: Signal<PolarAlignSnapshot>,
    #[prop(into)] mount: Signal<MountSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Local exposure field, seeded from `pAHExposure` when settings first
    // arrive. Kept local so user edits aren't thrashed by the 5 s refresh.
    let exposure = RwSignal::new(2.0_f64);
    let exposure_seeded = RwSignal::new(false);
    Effect::new(move |_| {
        if exposure_seeded.get_untracked() {
            return;
        }
        let snap = polar.get();
        if let Some(v) = settings_f64(&snap.settings, "pAHExposure") {
            if v.is_finite() && v > 0.0 {
                exposure.set(v);
                exposure_seeded.set(true);
            }
        } else if !snap.settings.is_null() {
            exposure_seeded.set(true);
        }
    });

    // ── Dispatchers (one Arc clone each) ─────────────────────────────────
    let s_start = send.clone();
    let on_start = move |_| send_cmd(&s_start, "polar_start", serde_json::json!({}));

    let s_stop_a = send.clone();
    let on_stop_abort = move |_| send_cmd(&s_stop_a, "polar_stop", serde_json::json!({}));

    let s_stop_b = send.clone();
    let on_stop_footer = move |_| send_cmd(&s_stop_b, "polar_stop", serde_json::json!({}));

    let s_slew_done = send.clone();
    let on_slew_done = move |_| send_cmd(&s_slew_done, "polar_slew_done", serde_json::json!({}));

    let s_refresh = send.clone();
    let on_start_refresh = move |_| {
        send_cmd(
            &s_refresh,
            "polar_refresh",
            serde_json::json!({ "value": exposure.get() }),
        );
    };

    let s_refresh_done = send.clone();
    let on_stop_refresh = move |_| {
        send_cmd(&s_refresh_done, "polar_refreshing_done", serde_json::json!({}));
    };

    let s_dir = send.clone();
    let on_direction_change = move |ev: web_sys::Event| {
        let v = event_target_select(&ev);
        dispatch_align_setting(&s_dir, "pAHDirection", serde_json::Value::String(v));
    };

    let s_rot = send.clone();
    let on_rotation_change = move |ev: web_sys::Event| {
        let s = event_target_value(&ev);
        if let Ok(n) = s.parse::<i64>() {
            let n = n.clamp(15, 60);
            dispatch_align_setting(
                &s_rot,
                "pAHRotation",
                serde_json::Value::Number(n.into()),
            );
        }
    };

    let s_speed = send.clone();
    let on_speed_change = move |ev: web_sys::Event| {
        let v = event_target_select(&ev);
        dispatch_align_setting(&s_speed, "pAHMountSpeed", serde_json::Value::String(v));
    };

    let s_manual = send.clone();
    let on_manual_change = move |ev: web_sys::Event| {
        let on = event_target_checked(&ev);
        dispatch_align_setting(&s_manual, "pAHManualSlew", serde_json::Value::Bool(on));
    };

    let s_algo = send.clone();
    let on_algo_change = move |ev: web_sys::Event| {
        let v = event_target_select(&ev);
        dispatch_align_setting(
            &s_algo,
            "pAHRefreshAlgorithm",
            serde_json::Value::String(v),
        );
    };

    let s_exp = send.clone();
    let on_exposure_change = move |ev: web_sys::Event| {
        let s = event_target_value(&ev);
        if let Ok(v) = s.parse::<f64>() {
            let v = v.clamp(0.1, 60.0);
            exposure.set(v);
            if let Some(num) = serde_json::Number::from_f64(v) {
                dispatch_align_setting(&s_exp, "pAHExposure", serde_json::Value::Number(num));
            }
        }
    };

    // ── Meridian-crossing warning (intro only) ───────────────────────────
    let meridian_warning = move || -> Option<String> {
        let ms = mount.get();
        let (Some(ha), Some(dec)) = (ms.ha_deg, ms.dec_deg) else { return None };
        let (going_west, rotation) = polar.with(|p| {
            let west = settings_str(&p.settings, "pAHDirection")
                .map(|s| s == "West")
                .unwrap_or(true);
            let rot = settings_i64(&p.settings, "pAHRotation").unwrap_or(30);
            (west, rot)
        });
        if !would_cross_meridian(ha, dec, ms.pier_side, rotation, going_west) {
            return None;
        }
        let side = match ms.pier_side {
            Some(0) => "west",
            Some(1) => "east",
            _ => "unknown",
        };
        Some(format!(
            "⚠ Meridian crossing likely: mount HA {ha:+.1}° on pier {side}, \
             slewing {dir} by {rotation}°. Consider reversing direction, \
             reducing rotation, or pointing further from the meridian.",
            dir = if going_west { "west" } else { "east" },
        ))
    };

    // ── Section visibility closures ──────────────────────────────────────
    let intro_visible = move || polar.with(|p| is_intro_stage(&p.stage));
    let progress_visible = move || polar.with(|p| is_progress_stage(&p.stage));
    let rotation_visible = move || {
        polar.with(|p| {
            is_rotation_stage(&p.stage)
                && settings_bool(&p.settings, "pAHManualSlew").unwrap_or(false)
        })
    };
    let refresh_visible = move || polar.with(|p| is_refresh_stage(&p.stage));

    const POLAR_INPUT: &str = "flex-1 bg-bg-input-deep text-text-dim border border-border-base py-1 px-[6px] font-mono text-sm";
    const FIELD_LABEL: &str = "basis-[clamp(80px,22%,120px)] grow-0 shrink-0 text-text-blue text-sm";
    const SECTION_CLS: &str = "border border-border-base p-sp-4";
    const BTN_START: &str = "py-sp-2 px-sp-4 bg-[rgba(12,14,24,0.9)] border border-accent-green text-accent-green cursor-pointer font-mono text-sm";
    const BTN_STOP: &str = "py-sp-2 px-sp-4 bg-[rgba(12,14,24,0.9)] border border-[#ff6a6a] text-[#ff6a6a] cursor-pointer font-mono text-sm";
    const BTN_ROTATION: &str = "py-sp-2 px-sp-4 bg-[rgba(12,14,24,0.9)] border border-accent-amber text-accent-amber cursor-pointer font-mono text-sm";
    const HEADER_LABEL: &str = "text-text-blue";

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono grid grid-rows-[auto_1fr] overflow-hidden">

            // ── Header ────────────────────────────────────────────────
            <div class="flex items-center gap-y-sp-2 gap-x-sp-4 flex-wrap min-h-[48px] py-sp-2 pr-5 pl-20 border-b border-border-base bg-[rgba(6,6,15,0.85)] text-md max-[374px]:pl-12 max-[374px]:text-sm">
                <span class="inline-block py-1 px-sp-3 rounded-[14px] text-sm border border-current max-[374px]:text-[10px] max-[374px]:py-[3px] max-[374px]:px-2"
                      style=move || format!(
                    "color:{};",
                    stage_color(&polar.with(|p| p.stage.clone()))
                )>
                    {move || {
                        let s = polar.with(|p| p.stage.clone());
                        if s.is_empty() { tr().pa_idle.to_string() } else { s }
                    }}
                </span>
                <span class=HEADER_LABEL>{move || tr().pa_enabled_label}</span>
                <span>{move || if polar.with(|p| p.enabled) { tr().yes } else { tr().no }}</span>
                <span class="flex-1 text-text overflow-hidden whitespace-nowrap text-ellipsis"
                      title=move || polar.with(|p| p.message.clone())>
                    {move || polar.with(|p| p.message.clone())}
                </span>
            </div>

            // ── Body ──────────────────────────────────────────────────
            <div class="overflow-y-auto py-4 px-5 flex flex-col gap-sp-4">

                // Live frame from KStars align module (uuid "+A" — every
                // capture, solve, and refresh iteration streams a JPEG).
                <Show when=move || polar.with(|p| p.preview_url.is_some())>
                    <div class="flex justify-center items-center bg-bg-input-deep border border-border-base p-sp-2 min-h-[220px] max-h-[440px]">
                        <img
                            src=move || polar.with(|p|
                                p.preview_url.clone().unwrap_or_default())
                            alt="align frame"
                            class="max-w-full max-h-[424px] object-contain block [image-rendering:pixelated]"
                        />
                    </div>
                </Show>

                // Intro section
                <Show when=intro_visible>
                    <fieldset class=SECTION_CLS>
                        <legend class="px-sp-1 text-sm text-text-blue">
                            {move || tr().pa_pre_start}
                        </legend>
                        <div class="flex flex-col gap-sp-2">

                            // Direction
                            <div class="flex items-center gap-sp-2">
                                <span class=FIELD_LABEL>{move || tr().pa_direction}</span>
                                <select
                                    on:change=on_direction_change.clone()
                                    class=POLAR_INPUT
                                    prop:value=move || polar.with(|p|
                                        settings_str(&p.settings, "pAHDirection")
                                            .unwrap_or_else(|| "West".into()))
                                >
                                    {move || {
                                        let tr_ = tr();
                                        DIRECTION_WIRE.iter().map(|wire| {
                                            let label = direction_label(wire, tr_);
                                            view! { <option value=*wire>{label}</option> }
                                        }).collect::<Vec<_>>()
                                    }}
                                </select>
                            </div>

                            // Rotation
                            <div class="flex items-center gap-sp-2">
                                <span class=FIELD_LABEL>{move || tr().pa_rotation_deg_label}</span>
                                <input
                                    type="number"
                                    min="15"
                                    max="60"
                                    step="1"
                                    on:change=on_rotation_change.clone()
                                    prop:value=move || polar.with(|p|
                                        settings_i64(&p.settings, "pAHRotation")
                                            .unwrap_or(30).to_string())
                                    class=POLAR_INPUT
                                />
                            </div>

                            // Mount speed
                            <div class="flex items-center gap-sp-2">
                                <span class=FIELD_LABEL>{move || tr().pa_mount_speed_label}</span>
                                <select
                                    on:change=on_speed_change.clone()
                                    class=POLAR_INPUT
                                    prop:value=move || polar.with(|p|
                                        settings_str(&p.settings, "pAHMountSpeed")
                                            .unwrap_or_default())
                                >
                                    {move || {
                                        let tr_ = tr();
                                        let current = polar.with(|p|
                                            settings_str(&p.settings, "pAHMountSpeed")
                                                .unwrap_or_default());
                                        let mut opts: Vec<String> = DEFAULT_SPEED_OPTIONS
                                            .iter().map(|s| s.to_string()).collect();
                                        if !current.is_empty()
                                            && !opts.iter().any(|s| s == &current)
                                        {
                                            opts.insert(0, current);
                                        }
                                        opts.into_iter().map(|wire| {
                                            let label = speed_label(&wire, tr_);
                                            view! { <option value=wire>{label}</option> }
                                        }).collect::<Vec<_>>()
                                    }}
                                </select>
                            </div>

                            // Manual slew
                            <div class="flex items-center gap-sp-2">
                                <span class=FIELD_LABEL>{move || tr().pa_manual_slew_label}</span>
                                <input
                                    type="checkbox"
                                    on:change=on_manual_change.clone()
                                    prop:checked=move || polar.with(|p|
                                        settings_bool(&p.settings, "pAHManualSlew")
                                            .unwrap_or(false))
                                />
                            </div>

                            // Meridian-crossing warning — mirrors
                            // PolarAlignmentAssistant::checkPAHForMeridianCrossing().
                            <Show when=move || meridian_warning().is_some()>
                                <div class="mt-sp-1 py-sp-2 px-sp-3 border border-[#ff9a3a] bg-[rgba(80,40,10,0.4)] text-[#ffb070] text-sm leading-[1.4]">
                                    {move || meridian_warning().unwrap_or_default()}
                                </div>
                            </Show>

                            <div class="mt-[6px]">
                                <button on:click=on_start.clone() class=BTN_START>
                                    {move || tr().pa_start_btn_long}
                                </button>
                            </div>
                        </div>
                    </fieldset>
                </Show>

                // Progress section
                <Show when=progress_visible>
                    <fieldset class=SECTION_CLS>
                        <legend class="px-sp-1 text-sm text-text-blue">
                            {move || tr().pa_capture_solve}
                        </legend>
                        <div class="text-text-dim py-2">
                            {move || {
                                let m = polar.with(|p| p.message.clone());
                                if m.is_empty() { tr().pa_running.to_string() } else { m }
                            }}
                        </div>
                        <div>
                            <button on:click=on_stop_abort.clone() class=BTN_STOP>
                                {move || tr().pa_abort_short}
                            </button>
                        </div>
                    </fieldset>
                </Show>

                // Manual rotation section
                <Show when=rotation_visible>
                    <fieldset class=SECTION_CLS>
                        <legend class="px-sp-1 text-sm text-accent-amber">
                            {move || tr().pa_manual_rotation_section}
                        </legend>
                        <div class="text-text-dim pt-1 pb-[10px]">
                            {move || tr().pa_manual_rotate_instr}
                        </div>
                        <div>
                            <button on:click=on_slew_done.clone() class=BTN_ROTATION>
                                {move || tr().pa_rotation_done}
                            </button>
                        </div>
                    </fieldset>
                </Show>

                // Refresh section
                <Show when=refresh_visible>
                    <fieldset class=SECTION_CLS>
                        <legend class="px-sp-1 text-sm text-accent-green">
                            {move || tr().pa_refresh_correct}
                        </legend>
                        <div class="flex flex-wrap gap-sp-4 items-start">
                            <div class="flex flex-col gap-sp-2 flex-[1_1_260px] min-w-0">

                                // Exposure
                                <div class="flex items-center gap-sp-2">
                                    <span class=FIELD_LABEL>{move || tr().pa_exposure_s_label}</span>
                                    <input
                                        type="number"
                                        min="0.1"
                                        max="60"
                                        step="0.1"
                                        on:change=on_exposure_change.clone()
                                        prop:value=move || exposure.get().to_string()
                                        class=POLAR_INPUT
                                    />
                                </div>

                                // Algorithm
                                <div class="flex items-center gap-sp-2">
                                    <span class=FIELD_LABEL>{move || tr().pa_algorithm_label}</span>
                                    <select
                                        on:change=on_algo_change.clone()
                                        class=POLAR_INPUT
                                        prop:value=move || polar.with(|p|
                                            settings_str(&p.settings, "pAHRefreshAlgorithm")
                                                .unwrap_or_else(|| "Plate Solve".into()))
                                    >
                                        {move || {
                                            let tr_ = tr();
                                            ALGORITHM_WIRE.iter().map(|wire| {
                                                let label = algorithm_label(wire, tr_);
                                                view! { <option value=*wire>{label}</option> }
                                            }).collect::<Vec<_>>()
                                        }}
                                    </select>
                                </div>

                                // Error readouts
                                <div class="grid grid-cols-2 gap-sp-2 mt-[6px]">
                                    <div class="border border-border-base py-[6px] px-sp-2">
                                        <div class="text-sm text-text-blue">{move || tr().pa_original}</div>
                                        {move || {
                                            let tr_ = tr();
                                            let v = polar.with(|p| p.vector.clone());
                                            let (err, az, alt) = match v {
                                                Some(v) => (v.error, v.az_error, v.alt_error),
                                                None => (f64::NAN, f64::NAN, f64::NAN),
                                            };
                                            view! {
                                                <div class="text-sm">
                                                    {tr_.pa_total_label} {format_deg_as_dms_small(err)}
                                                </div>
                                                <div class="text-sm">
                                                    {tr_.pa_az_label_long} {format_deg_as_dms_small(az)}
                                                </div>
                                                <div class="text-sm">
                                                    {tr_.pa_alt_label_long} {format_deg_as_dms_small(alt)}
                                                </div>
                                            }
                                        }}
                                    </div>
                                    <div class="border border-border-base py-[6px] px-sp-2">
                                        <div class="text-sm text-accent-green">{move || tr().pa_updated}</div>
                                        {move || {
                                            let tr_ = tr();
                                            let (e, az, al) = polar.with(|p| (
                                                p.updated_error,
                                                p.updated_az_error,
                                                p.updated_alt_error,
                                            ));
                                            let fmt = |o: Option<f64>| match o {
                                                Some(v) => format_deg_as_dms_small(v),
                                                None => "—".to_string(),
                                            };
                                            view! {
                                                <div class="text-sm">{tr_.pa_total_label} {fmt(e)}</div>
                                                <div class="text-sm">{tr_.pa_az_label_long} {fmt(az)}</div>
                                                <div class="text-sm">{tr_.pa_alt_label_long} {fmt(al)}</div>
                                            }
                                        }}
                                    </div>
                                </div>

                                // Refresh controls
                                <div class="flex gap-sp-2 mt-sp-2">
                                    <button on:click=on_start_refresh.clone() class=BTN_START>
                                        {move || tr().pa_start_refresh}
                                    </button>
                                    <button on:click=on_stop_refresh.clone() class=BTN_ROTATION>
                                        {move || tr().pa_stop_refresh}
                                    </button>
                                </div>
                            </div>

                            // Correction vector preview
                            <div class="flex-[0_0_140px]">
                                {move || correction_svg(polar.with(|p| p.vector.clone()))}
                            </div>
                        </div>
                    </fieldset>
                </Show>

                // Footer — always-visible global Stop
                <div class="flex gap-sp-2">
                    <button on:click=on_stop_footer class=BTN_STOP>
                        {move || tr().pa_stop_btn_long}
                    </button>
                </div>
            </div>
        </div>
    }
}

fn correction_svg(vector: Option<PolarVectorData>) -> impl IntoView {
    let (mag, pa) = match vector.as_ref() {
        Some(v) => (v.mag, v.pa),
        None => (0.0, 0.0),
    };
    let arrow_len = (mag * 500.0).clamp(0.0, 55.0);
    let arrow_tail = (arrow_len - 6.0).max(0.0);
    let (err, az, alt) = match vector.as_ref() {
        Some(v) => (v.error, v.az_error, v.alt_error),
        None => (f64::NAN, f64::NAN, f64::NAN),
    };
    view! {
        <div class="flex flex-col items-center gap-[6px]">
            <svg width="120" height="120" viewBox="-60 -60 120 120"
                 class="bg-bg-input-deep border border-border-base">
                <circle cx="0" cy="0" r="55" fill="none" stroke="#333" stroke-width="0.5"/>
                <circle cx="0" cy="0" r="28" fill="none" stroke="#222" stroke-width="0.5"/>
                <line x1="-55" y1="0" x2="55" y2="0" stroke="#222" stroke-width="0.5"/>
                <line x1="0" y1="-55" x2="0" y2="55" stroke="#222" stroke-width="0.5"/>
                <g transform=format!("rotate({:.2})", -pa)>
                    <line x1="0" y1="0"
                          x2=format!("{:.2}", arrow_len) y2="0"
                          stroke="#88aaff" stroke-width="2"/>
                    <polygon
                        points=format!("{:.2},0 {:.2},-3 {:.2},3",
                            arrow_len, arrow_tail, arrow_tail)
                        fill="#88aaff"/>
                </g>
                <circle cx="0" cy="0" r="2" fill="#cfe0ff"/>
            </svg>
            <div class="text-xs text-text text-center leading-[1.4]">
                <div>"Err "  {format_deg_as_dms_small(err)}</div>
                <div>"Az "   {format_deg_as_dms_small(az)}</div>
                <div>"Alt "  {format_deg_as_dms_small(alt)}</div>
            </div>
        </div>
    }
}
