//! Polar Alignment module UI — fullscreen tab.
//!
//! Wire protocol: see kstars/ekos/align/polaralignmentassistant.{h,cpp} and
//! the inbound handlers at kstars/ekos/ekoslive/message.cpp:1079-1263.
//!
//! Outbound (browser → KStars):
//!   - polar_start, polar_stop, polar_refresh {value: exposure},
//!     polar_refreshing_done, polar_slew_done
//!   - align_set_all_settings {pAHDirection, pAHRotation, pAHMountSpeed,
//!     pAHManualSlew, pAHExposure, pAHRefreshAlgorithm} — keys live at the top
//!     level of the payload (kstars/ekos/ekoslive/message.cpp:871-874 calls
//!     `payload.toVariantMap()` and feeds it straight to `Align::setAllSettings`,
//!     which looks each key up against the dialog's child widgets).
//!   - align_get_all_settings (primed + refreshed from ws.rs)
//!
//! Inbound (KStars → browser): `new_polar_state` (partial: stage, message,
//! enabled, vector, updatedError*) and `align_get_all_settings` (settings map).
//!
//! `polar_reset_view` (commands.h:419) is wired: KStars' align module
//! reacts to it even without us hosting a frame view (it emits the
//! `resetPolarView()` signal which the desktop Align widget consumes).
//!
//! Deliberately NOT wired: `polar_set_algorithm` (widget-name bug upstream,
//! message.cpp:1102 — use align_set_all_settings instead),
//! `polar_set_crosshair` / `polar_set_zoom` (require a live frame canvas in
//! this tab; deferred), `NEW_ALIGN_FRAME` (declared in commands.h:35 but
//! never emitted).

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::components::branding::{POLAR_LOGO_SVG, junos_header, section_card};
use crate::compat::{MountSnapshot, PolarAlignSnapshot};
use crate::i18n::{Lang, Translations, t};
use crate::ws::{PolarVectorData, SendCmd};
use crate::ws_helpers::{send_cmd, dispatch_setting as ws_dispatch_setting};

// Section header glyphs. 24×24 viewBox, `currentColor` so each inherits the
// accent color of its card header. Style matches `tab_wheel_icons.rs`.
/// Sliders — pre-start setup parameters.
const ICON_SETUP: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M4 8 L20 8 M4 16 L20 16"/><circle cx="9" cy="8" r="2.6"/><circle cx="15" cy="16" r="2.6"/></svg>"##;
/// Camera — capture & solve in progress.
const ICON_CAPTURE: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 8 L8 8 L9.5 5.5 L14.5 5.5 L16 8 L20 8 A1 1 0 0 1 21 9 L21 18 A1 1 0 0 1 20 19 L4 19 A1 1 0 0 1 3 18 L3 9 A1 1 0 0 1 4 8 Z"/><circle cx="12" cy="13" r="4"/></svg>"##;
/// Circular arrow — manual mount rotation step.
const ICON_ROTATE: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 L20 11 L15 11"/><path d="M20 11 A8 8 0 1 0 18.5 15"/></svg>"##;
/// Crosshair — refresh & correct to center.
const ICON_ADJUST: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><circle cx="12" cy="12" r="8"/><path d="M12 2 L12 5 M12 19 L12 22 M2 12 L5 12 M19 12 L22 12"/><circle cx="12" cy="12" r="2.4" fill="currentColor" stroke="none"/></svg>"##;

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

/// Send a single-key update to KStars' align module. The payload is flat — see
/// the module docstring for the wire shape and KStars handler reference.
fn dispatch_align_setting(send: &SendCmd, key: &str, value: serde_json::Value) {
    ws_dispatch_setting(send, "align_set_all_settings", None, key, value);
}

fn stage_color(stage: &str) -> &'static str {
    match stage {
        "" | "Idle" => "var(--text-muted)",
        "First Capture" | "First Solve"
        | "Second Capture" | "Second Solve"
        | "Third Capture" | "Third Solve" => "var(--state-info)",
        "First Rotation" | "Second Rotation"
        | "First Settle" | "Second Settle"
        | "Finding CP" | "Select Star" => "var(--state-warn)",
        "Refreshing" | "Refresh Complete" => "var(--state-ok)",
        _ => "var(--text)",
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

/// Tolerance below which an axis is considered aligned (no arrow). Mirrors
/// `minError` in KStars' `PolarAlignmentAssistant::drawArrows`
/// (kstars/ekos/align/polaralignmentassistant.cpp:307-351).
const PA_MIN_ERR_DEG: f64 = 20.0 / 3600.0; // 20 arcsec

/// Maps a signed axis error to the glyph telling the user which way to move
/// the mount to null it. Sign convention follows KStars' `drawArrows`:
/// for azimuth pass `("←", "→")` (err > 0 → move left), for altitude pass
/// `("↓", "↑")` (err > 0 → move down). Returns None when the error is within
/// tolerance or not a finite solve (the `-1` solver-failure sentinel is not
/// finite-positive here so callers gate on the total error too).
fn axis_arrow(
    err: f64,
    positive_glyph: &'static str,
    negative_glyph: &'static str,
) -> Option<&'static str> {
    if !err.is_finite() || err.abs() < PA_MIN_ERR_DEG {
        return None;
    }
    Some(if err > 0.0 { positive_glyph } else { negative_glyph })
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

    // Local edit buffers for form fields, seeded once from server settings.
    // Kept local so user edits aren't thrashed by the 5 s align_get_all_settings
    // refresh that replaces store.align_settings wholesale.
    let exposure = RwSignal::new(2.0_f64);
    let direction_local = RwSignal::new(String::from("West"));
    let rotation_local = RwSignal::new(30_i64);
    let speed_local = RwSignal::new(String::new());
    let manual_local = RwSignal::new(false);
    let algo_local = RwSignal::new(String::from("Plate Solve"));
    // Set on first successful seed from the server *and* whenever the user
    // touches a field. Either path locks the seeding Effect so a late-arriving
    // `align_get_all_settings` cannot clobber a value the user already picked.
    let form_seeded = RwSignal::new(false);
    let mark_dirty = move || form_seeded.set(true);
    Effect::new(move |_| {
        if form_seeded.get_untracked() {
            return;
        }
        let snap = polar.get();
        if snap.settings.is_null() {
            return;
        }
        if let Some(v) = settings_f64(&snap.settings, "pAHExposure") {
            if v.is_finite() && v > 0.0 {
                exposure.set(v);
            }
        }
        if let Some(s) = settings_str(&snap.settings, "pAHDirection") {
            direction_local.set(s);
        }
        if let Some(n) = settings_i64(&snap.settings, "pAHRotation") {
            rotation_local.set(n);
        }
        if let Some(s) = settings_str(&snap.settings, "pAHMountSpeed") {
            speed_local.set(s);
        }
        if let Some(b) = settings_bool(&snap.settings, "pAHManualSlew") {
            manual_local.set(b);
        }
        if let Some(s) = settings_str(&snap.settings, "pAHRefreshAlgorithm") {
            algo_local.set(s);
        }
        form_seeded.set(true);
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

    let s_reset_view = send.clone();
    let on_reset_view = move |_| {
        send_cmd(&s_reset_view, "polar_reset_view", serde_json::json!({}));
    };

    let s_dir = send.clone();
    let on_direction_change = move |ev: web_sys::Event| {
        let v = event_target_select(&ev);
        direction_local.set(v.clone());
        mark_dirty();
        dispatch_align_setting(&s_dir, "pAHDirection", serde_json::Value::String(v));
    };

    let s_rot = send.clone();
    let on_rotation_change = move |ev: web_sys::Event| {
        let Ok(n) = event_target_value(&ev).parse::<i64>() else { return };
        let n = n.clamp(15, 60);
        rotation_local.set(n);
        mark_dirty();
        dispatch_align_setting(&s_rot, "pAHRotation", serde_json::Value::Number(n.into()));
    };

    let s_speed = send.clone();
    let on_speed_change = move |ev: web_sys::Event| {
        let v = event_target_select(&ev);
        speed_local.set(v.clone());
        mark_dirty();
        dispatch_align_setting(&s_speed, "pAHMountSpeed", serde_json::Value::String(v));
    };

    let s_manual = send.clone();
    let on_manual_change = move |ev: web_sys::Event| {
        let on = event_target_checked(&ev);
        manual_local.set(on);
        mark_dirty();
        dispatch_align_setting(&s_manual, "pAHManualSlew", serde_json::Value::Bool(on));
    };

    let s_algo = send.clone();
    let on_algo_change = move |ev: web_sys::Event| {
        let v = event_target_select(&ev);
        algo_local.set(v.clone());
        mark_dirty();
        dispatch_align_setting(&s_algo, "pAHRefreshAlgorithm", serde_json::Value::String(v));
    };

    let s_exp = send.clone();
    let on_exposure_change = move |ev: web_sys::Event| {
        let Ok(v) = event_target_value(&ev).parse::<f64>() else { return };
        let v = v.clamp(0.1, 60.0);
        exposure.set(v);
        mark_dirty();
        if let Some(num) = serde_json::Number::from_f64(v) {
            dispatch_align_setting(&s_exp, "pAHExposure", serde_json::Value::Number(num));
        }
    };

    // ── Meridian-crossing warning (intro only) ───────────────────────────
    let meridian_warning = move || -> Option<String> {
        let ms = mount.get();
        let (Some(ha), Some(dec)) = (ms.ha_deg, ms.dec_deg) else { return None };
        let going_west = direction_local.with(|s| s == "West");
        let rotation = rotation_local.get();
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
        polar.with(|p| is_rotation_stage(&p.stage)) && manual_local.get()
    };
    let refresh_visible = move || polar.with(|p| is_refresh_stage(&p.stage));

    const POLAR_INPUT: &str = "input input--sm flex-1 min-w-0 font-mono";
    const FIELD_LABEL: &str = "basis-[clamp(80px,22%,120px)] grow-0 shrink-0 text-text-blue text-sm max-[759px]:basis-[110px] max-[420px]:basis-auto";
    const BTN_START: &str = "btn btn-primary";
    const BTN_STOP: &str = "btn btn-danger";
    const BTN_ROTATION: &str = "btn btn-ghost text-accent-amber !border-accent-amber";
    const HEADER_LABEL: &str = "text-text-blue";

    view! {
        // `grid-cols-[minmax(0,1fr)]` caps the implicit column at the container
        // width — without it an `auto` column sizes to the widest content (the
        // align-frame preview at its natural width) and overflows off-screen.
        <div class="absolute inset-0 bg-bg text-text font-mono grid grid-rows-[auto_1fr] grid-cols-[minmax(0,1fr)] overflow-hidden">

            // ── Header ────────────────────────────────────────────────
            <div class="flex items-center gap-x-sp-4 gap-y-sp-2 flex-wrap min-h-[48px] py-sp-2 pr-5 pl-20 border-b border-border-base bg-[rgba(6,6,15,0.85)] text-md max-[759px]:pl-16 max-[759px]:pr-3 max-[759px]:gap-x-3 max-[759px]:text-sm max-[374px]:pl-12">
                <div class="shrink-0">
                    {junos_header(POLAR_LOGO_SVG, move || tr().tab_polar_align.to_string())}
                </div>
                <span class="w-px self-stretch bg-border-strong my-1 max-[759px]:hidden"></span>

                // Status cluster — flows and truncates independently of the brand.
                <div class="flex items-center gap-x-sp-4 gap-y-[6px] flex-wrap flex-1 min-w-0 max-[759px]:gap-x-3 max-[759px]:basis-full">
                    <span class="inline-block py-1 px-sp-3 rounded-[14px] text-sm border border-current min-w-0 max-w-full max-[374px]:text-[10px] max-[374px]:py-[3px] max-[374px]:px-2"
                          style=move || format!(
                        "color:{};",
                        stage_color(&polar.with(|p| p.stage.clone()))
                    )>
                        {move || {
                            let s = polar.with(|p| p.stage.clone());
                            if s.is_empty() { tr().pa_idle.to_string() } else { s }
                        }}
                    </span>
                    <span class=format!("{HEADER_LABEL} shrink-0")>{move || tr().pa_enabled_label}</span>
                    <span class="shrink-0">{move || if polar.with(|p| p.enabled) { tr().yes } else { tr().no }}</span>
                    <span class="flex-1 min-w-0 basis-[120px] text-text overflow-hidden whitespace-nowrap text-ellipsis"
                          title=move || polar.with(|p| p.message.clone())>
                        {move || polar.with(|p| p.message.clone())}
                    </span>
                </div>
            </div>

            // ── Body ──────────────────────────────────────────────────
            // `min-h-0` is required: this is the `1fr` grid track, whose default
            // `min-height:auto` would otherwise let it grow to its content's
            // height and overflow the fixed-height root (clipped by the root's
            // `overflow-hidden`) instead of scrolling here.
            <div class="min-h-0 min-w-0 overflow-y-auto overflow-x-hidden py-4 px-5 flex flex-col gap-sp-4 max-[759px]:p-sp-3">

                // Live frame from KStars align module (uuid "+A" — every
                // capture, solve, and refresh iteration streams a JPEG).
                <Show when=move || polar.with(|p| p.preview_url.is_some())>
                    <div class="shrink-0 min-w-0 flex justify-center items-center bg-bg-input-deep border border-border-base p-sp-2 min-h-[220px] max-h-[440px]">
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
                    {section_card("text-accent-cyan", "bg-accent-cyan", ICON_SETUP,
                        move || tr().pa_pre_start, view! {
                        <div class="flex flex-col gap-sp-2">

                            // Direction
                            <div class="flex items-center gap-sp-2 min-w-0 max-[420px]:flex-col max-[420px]:items-stretch">
                                <span class=FIELD_LABEL>{move || tr().pa_direction}</span>
                                <select
                                    on:change=on_direction_change.clone()
                                    class=POLAR_INPUT
                                >
                                    {move || {
                                        let tr_ = tr();
                                        let cur = direction_local.get();
                                        DIRECTION_WIRE.iter().map(|wire| {
                                            let label = direction_label(wire, tr_);
                                            let sel = *wire == cur;
                                            view! { <option value=*wire selected=sel>{label}</option> }
                                        }).collect::<Vec<_>>()
                                    }}
                                </select>
                            </div>

                            // Rotation
                            <div class="flex items-center gap-sp-2 min-w-0 max-[420px]:flex-col max-[420px]:items-stretch">
                                <span class=FIELD_LABEL>{move || tr().pa_rotation_deg_label}</span>
                                <input
                                    type="number"
                                    min="15"
                                    max="60"
                                    step="1"
                                    on:change=on_rotation_change.clone()
                                    prop:value=move || rotation_local.get().to_string()
                                    class=POLAR_INPUT
                                />
                            </div>

                            // Mount speed
                            <div class="flex items-center gap-sp-2 min-w-0 max-[420px]:flex-col max-[420px]:items-stretch">
                                <span class=FIELD_LABEL>{move || tr().pa_mount_speed_label}</span>
                                <select
                                    on:change=on_speed_change.clone()
                                    class=POLAR_INPUT
                                >
                                    {move || {
                                        let tr_ = tr();
                                        let current = speed_local.get();
                                        let mut opts: Vec<String> = DEFAULT_SPEED_OPTIONS
                                            .iter().map(|s| s.to_string()).collect();
                                        if !current.is_empty()
                                            && !opts.iter().any(|s| s == &current)
                                        {
                                            opts.insert(0, current.clone());
                                        }
                                        opts.into_iter().map(|wire| {
                                            let label = speed_label(&wire, tr_);
                                            let sel = wire == current;
                                            view! { <option value=wire selected=sel>{label}</option> }
                                        }).collect::<Vec<_>>()
                                    }}
                                </select>
                            </div>

                            // Manual slew
                            <div class="flex items-center gap-sp-2 max-[420px]:flex-col max-[420px]:items-stretch">
                                <span class=FIELD_LABEL>{move || tr().pa_manual_slew_label}</span>
                                <input
                                    type="checkbox"
                                    on:change=on_manual_change.clone()
                                    prop:checked=move || manual_local.get()
                                />
                            </div>

                            // Meridian-crossing warning — mirrors
                            // PolarAlignmentAssistant::checkPAHForMeridianCrossing().
                            <Show when=move || meridian_warning().is_some()>
                                <div class="mt-sp-1 py-sp-2 px-sp-3 border border-state-warn/60 bg-state-warn/10 text-state-warn text-sm leading-[1.4]">
                                    {move || meridian_warning().unwrap_or_default()}
                                </div>
                            </Show>

                            <div class="mt-[6px]">
                                <button on:click=on_start.clone() class=BTN_START>
                                    {move || tr().pa_start_btn_long}
                                </button>
                            </div>
                        </div>
                    })}
                </Show>

                // Progress section
                <Show when=progress_visible>
                    {section_card("text-accent-violet", "bg-accent-violet", ICON_CAPTURE,
                        move || tr().pa_capture_solve, view! {
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
                    })}
                </Show>

                // Manual rotation section
                <Show when=rotation_visible>
                    {section_card("text-accent-amber", "bg-accent-amber", ICON_ROTATE,
                        move || tr().pa_manual_rotation_section, view! {
                        <div class="text-text-dim pt-1 pb-[10px]">
                            {move || tr().pa_manual_rotate_instr}
                        </div>
                        <div>
                            <button on:click=on_slew_done.clone() class=BTN_ROTATION>
                                {move || tr().pa_rotation_done}
                            </button>
                        </div>
                    })}
                </Show>

                // Refresh section
                <Show when=refresh_visible>
                    {section_card("text-accent-green", "bg-accent-green", ICON_ADJUST,
                        move || tr().pa_refresh_correct, view! {
                        <div class="flex flex-wrap gap-sp-4 items-start">
                            <div class="flex flex-col gap-sp-2 flex-[1_1_260px] min-w-0">

                                // Exposure
                                <div class="flex items-center gap-sp-2 max-[420px]:flex-col max-[420px]:items-stretch">
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
                                <div class="flex items-center gap-sp-2 max-[420px]:flex-col max-[420px]:items-stretch">
                                    <span class=FIELD_LABEL>{move || tr().pa_algorithm_label}</span>
                                    <select
                                        on:change=on_algo_change.clone()
                                        class=POLAR_INPUT
                                    >
                                        {move || {
                                            let tr_ = tr();
                                            let cur = algo_local.get();
                                            ALGORITHM_WIRE.iter().map(|wire| {
                                                let label = algorithm_label(wire, tr_);
                                                let sel = *wire == cur;
                                                view! { <option value=*wire selected=sel>{label}</option> }
                                            }).collect::<Vec<_>>()
                                        }}
                                    </select>
                                </div>

                                // Error readouts
                                <div class="grid grid-cols-2 gap-sp-2 mt-[6px] max-[479px]:grid-cols-1">
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

                                // Adjust-mount direction arrows. Derived
                                // client-side from the signed az/alt errors —
                                // KStars sends only the scalars, the ↑↓←→
                                // mapping lives in its desktop widget
                                // (polaralignmentassistant.cpp:307-351).
                                <div class="mt-sp-2 border border-border-base py-[6px] px-sp-2">
                                    <div class="text-sm text-text-blue">{move || tr().pa_adjust_mount}</div>
                                    {move || {
                                        let tr_ = tr();
                                        // Prefer the live refresh errors; fall
                                        // back to the original solve vector.
                                        let (total, az, alt) = polar.with(|p| {
                                            let v = p.vector.as_ref();
                                            (
                                                p.updated_error.filter(|x| x.is_finite() && *x >= 0.0)
                                                    .or_else(|| v.map(|v| v.error)),
                                                p.updated_az_error.filter(|x| x.is_finite())
                                                    .or_else(|| v.map(|v| v.az_error)),
                                                p.updated_alt_error.filter(|x| x.is_finite())
                                                    .or_else(|| v.map(|v| v.alt_error)),
                                            )
                                        });
                                        // Only guide when we have a valid solve.
                                        let solved = total.map(|t| t.is_finite() && t >= 0.0).unwrap_or(false);
                                        let az = az.unwrap_or(f64::NAN);
                                        let alt = alt.unwrap_or(f64::NAN);
                                        let az_arrow = if solved { axis_arrow(az, "←", "→") } else { None };
                                        let alt_arrow = if solved { axis_arrow(alt, "↓", "↑") } else { None };
                                        let axis_view = |label: &'static str, arrow: Option<&'static str>, err: f64| {
                                            match arrow {
                                                Some(g) => view! {
                                                    <div class="flex items-center gap-sp-2 text-sm">
                                                        <span class="basis-[40px] grow-0 shrink-0 text-text-blue">{label}</span>
                                                        <span class="text-lg text-accent-green leading-none">{g}</span>
                                                        <span class="font-mono text-accent-green">{format_deg_as_dms_small(err)}</span>
                                                    </div>
                                                }.into_any(),
                                                None => view! {
                                                    <div class="flex items-center gap-sp-2 text-sm">
                                                        <span class="basis-[40px] grow-0 shrink-0 text-text-blue">{label}</span>
                                                        <span class="text-text-muted">{if solved { "✓" } else { "—" }}</span>
                                                    </div>
                                                }.into_any(),
                                            }
                                        };
                                        view! {
                                            <div class="flex flex-col gap-[2px] mt-[4px]">
                                                {axis_view(tr_.pa_az_label_long, az_arrow, az)}
                                                {axis_view(tr_.pa_alt_label_long, alt_arrow, alt)}
                                            </div>
                                        }
                                    }}
                                </div>

                                // Refresh controls
                                <div class="flex gap-sp-2 mt-sp-2 flex-wrap">
                                    <button on:click=on_start_refresh.clone() class=BTN_START>
                                        {move || tr().pa_start_refresh}
                                    </button>
                                    <button on:click=on_stop_refresh.clone() class=BTN_ROTATION>
                                        {move || tr().pa_stop_refresh}
                                    </button>
                                    <button on:click=on_reset_view.clone() class="btn btn-ghost">
                                        {move || tr().pa_reset_view}
                                    </button>
                                </div>
                            </div>

                            // Correction vector preview
                            <div class="flex-[0_0_140px] max-[420px]:flex-[1_1_100%] max-[420px]:flex max-[420px]:justify-center">
                                {move || correction_svg(polar.with(|p| p.vector.clone()))}
                            </div>
                        </div>
                    })}
                </Show>

                // Footer — always-visible global Stop
                <div class="shrink-0 flex gap-sp-2">
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
