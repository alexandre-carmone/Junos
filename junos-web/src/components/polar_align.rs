//! Polar Alignment module UI — full-screen tab.
//!
//! Layout (phone-first, same as Focus): a header (stage · settings), the align
//! frame, then a step strip, KStars' message, the live error readout and one
//! primary button that follows the stage. On phones the frame stays pinned and
//! the rest scrolls beneath it; from `md` up the controls get the right-hand
//! column. Settings open as a bottom sheet on phones, a floating panel on md+.
//!
//! Wire protocol: see kstars/ekos/align/polaralignmentassistant.{h,cpp} and
//! the inbound handlers at kstars/ekos/ekoslive/message.cpp:1310-1383.
//!
//! Outbound (browser → KStars):
//!   - polar_start, polar_stop, polar_refresh {value: exposure},
//!     polar_slew_done, polar_reset_view
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
//! `polar_reset_view` (commands.h:477) is wired: KStars' align module
//! reacts to it even without us hosting a frame view (it emits the
//! `resetPolarView()` signal which the desktop Align widget consumes).
//!
//! Deliberately NOT wired: `polar_refreshing_done` (just `stopPAHProcess()`,
//! message.cpp:1369 — the same as `polar_stop`), `polar_set_algorithm`
//! (widget-name bug upstream, message.cpp:1331 — use align_set_all_settings
//! instead), `polar_set_crosshair` / `polar_set_zoom` (require a live frame
//! canvas in this tab; deferred), `NEW_ALIGN_FRAME` (declared in commands.h:35
//! but never emitted).

use leptos::prelude::*;
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::MouseEvent;

use crate::compat::{MountSnapshot, PolarAlignSnapshot};
use crate::components::tab_wheel_icons::tab_icon;
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{Lang, Translations, t};
use crate::ws::SendCmd;
use crate::ws_helpers::{send_cmd, dispatch_setting as ws_dispatch_setting};
use crate::Tab;

const CARD: &str = "panel p-3 flex flex-col gap-3";
const CHIP: &str = "chip h-9 md:h-7 px-4 justify-center cursor-pointer";
const INPUT: &str = "input input--sm font-mono w-[150px] shrink-0 max-md:h-9";

/// Wire values for the direction pills (sent to KStars as-is).
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

// KStars sends the stage untranslated (`PAHStages`,
// polaralignmentassistant.cpp:26-43).

fn stage_badge(stage: &str) -> &'static str {
    match stage {
        "First Capture" | "First Solve"
        | "Second Capture" | "Second Solve"
        | "Third Capture" | "Third Solve" => "badge badge--info",
        "First Rotation" | "Second Rotation"
        | "First Settle" | "Second Settle"
        | "Finding CP" | "Select Star" => "badge badge--warn",
        "Refreshing" | "Refresh Complete" => "badge badge--ok",
        _ => "badge",
    }
}

/// Where the run is: 0 idle, 1–3 the three captures (each including the
/// rotation that follows it), 4 adjusting the mount. `enabled` (PAHEnabled)
/// means "PAA is available (FOV wide enough)", not "running" — the stage alone
/// decides.
fn step_of(stage: &str) -> u8 {
    match stage {
        "First Capture" | "First Solve" | "Finding CP"
        | "First Rotation" | "First Settle" => 1,
        "Second Capture" | "Second Solve"
        | "Second Rotation" | "Second Settle" => 2,
        "Third Capture" | "Third Solve" => 3,
        "Select Star" | "Refreshing" | "Refresh Complete" => 4,
        _ => 0,
    }
}

fn is_rotation_stage(stage: &str) -> bool {
    stage == "First Rotation" || stage == "Second Rotation"
}

/// What the big button does at a given stage.
#[derive(Clone, Copy, PartialEq)]
enum Primary { Start, RotationDone, Refresh, Stop }

fn primary_for(stage: &str, manual_slew: bool) -> Primary {
    match step_of(stage) {
        0 => Primary::Start,
        _ if manual_slew && is_rotation_stage(stage) => Primary::RotationDone,
        4 if stage != "Refreshing" => Primary::Refresh,
        _ => Primary::Stop,
    }
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
/// tolerance or not finite.
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

/// Colour for a total error. A rule of thumb, not a KStars threshold: under 1′
/// is excellent, under 5′ is fine for guided imaging.
fn quality_cls(deg: f64) -> &'static str {
    if !deg.is_finite() || deg < 0.0 {
        "text-text-muted"
    } else if deg * 60.0 <= 1.0 {
        "text-state-ok"
    } else if deg * 60.0 <= 5.0 {
        "text-state-warn"
    } else {
        "text-state-err"
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

    let settings_open = RwSignal::new(false);
    let stage = Memo::new(move |_| polar.with(|p| p.stage.clone()));
    let step = Memo::new(move |_| step_of(&stage.get()));
    let primary = Memo::new(move |_| primary_for(&stage.get(), manual_local.get()));
    let has_frame = move || polar.with(|p| p.preview_url.is_some());

    // Escape closes the settings sheet. forget() the closure (one persistent
    // listener per mount); calls into a disposed RwSignal are a no-op in
    // leptos 0.7, so leftover listeners after a tab switch are harmless.
    {
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            move |e: web_sys::KeyboardEvent| {
                if e.key() == "Escape" && settings_open.get_untracked() {
                    settings_open.set(false);
                }
            },
        );
        if let Some(win) = web_sys::window() {
            let _ = win
                .add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        }
        cb.forget();
    }

    // ── Actions ──────────────────────────────────────────────────────────
    let s_primary = send.clone();
    let on_primary = move |_: MouseEvent| {
        let (ty, payload) = match primary.get_untracked() {
            Primary::Start => ("polar_start", serde_json::json!({})),
            Primary::RotationDone => ("polar_slew_done", serde_json::json!({})),
            Primary::Refresh => (
                "polar_refresh",
                serde_json::json!({ "value": exposure.get_untracked() }),
            ),
            Primary::Stop => ("polar_stop", serde_json::json!({})),
        };
        send_cmd(&s_primary, ty, payload);
    };

    let s_stop = send.clone();
    let on_stop = move |_: MouseEvent| send_cmd(&s_stop, "polar_stop", serde_json::json!({}));

    let s_reset_view = send.clone();
    let on_reset_view = move |_: MouseEvent| {
        send_cmd(&s_reset_view, "polar_reset_view", serde_json::json!({}));
    };

    // ── Settings dispatchers ─────────────────────────────────────────────
    let s_dir = send.clone();
    let set_direction = move |wire: &'static str| {
        direction_local.set(wire.to_string());
        mark_dirty();
        dispatch_align_setting(&s_dir, "pAHDirection", serde_json::Value::String(wire.into()));
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
        let v = event_target_value(&ev);
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
        let v = event_target_value(&ev);
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

    // ── Idle: settings summary + meridian-crossing warning ───────────────
    let summary = move || {
        let tr = tr();
        let mut parts = vec![
            direction_label(&direction_local.get(), tr).to_string(),
            format!("{}°", rotation_local.get()),
        ];
        let speed = speed_local.get();
        if !speed.is_empty() {
            parts.push(speed_label(&speed, tr));
        }
        if manual_local.get() {
            parts.push(tr.pa_manual_slew_label.to_string());
        }
        parts.join(" · ")
    };

    let meridian_warning = move || -> Option<String> {
        let ms = mount.get();
        let (Some(ha), Some(dec)) = (ms.ha_deg, ms.dec_deg) else { return None };
        let going_west = direction_local.with(|s| s == "West");
        if !would_cross_meridian(ha, dec, ms.pier_side, rotation_local.get(), going_west) {
            return None;
        }
        let tr = tr();
        let side = match ms.pier_side {
            Some(0) => tr.mount_pier_west,
            Some(1) => tr.mount_pier_east,
            _ => tr.mount_pier_unknown,
        };
        Some(format!(
            "\u{26A0} {} (HA {ha:+.1}° · {} {side})",
            tr.pa_meridian_warn, tr.mount_pier_side,
        ))
    };

    // ── Adjust: live errors ──────────────────────────────────────────────
    // The refresh errors once a valid one has come in, else the original
    // three-point solve. KStars sends only the scalars; the ↑↓←→ mapping lives
    // in its desktop widget (polaralignmentassistant.cpp:307-351).
    let readout = move || {
        let tr = tr();
        let (total, az, alt, original) = polar.with(|p| {
            let v = p.vector.as_ref();
            match p.updated_error.filter(|e| e.is_finite() && *e >= 0.0) {
                Some(e) => (Some(e), p.updated_az_error, p.updated_alt_error, v.map(|v| v.error)),
                None => (v.map(|v| v.error), v.map(|v| v.az_error), v.map(|v| v.alt_error), None),
            }
        });
        let total = total.filter(|t| t.is_finite() && *t >= 0.0);
        let (az, alt) = match total {
            Some(_) => (az.unwrap_or(f64::NAN), alt.unwrap_or(f64::NAN)),
            None => (f64::NAN, f64::NAN),
        };
        let axis = |err: f64, pos, neg| -> (String, &'static str) {
            if !err.is_finite() {
                return ("—".into(), "text-text-muted");
            }
            let v = format_deg_as_dms_small(err.abs());
            match axis_arrow(err, pos, neg) {
                Some(g) => (format!("{g} {v}"), "text-accent-cyan"),
                None => (format!("\u{2713} {v}"), "text-state-ok"),
            }
        };
        let (az_txt, az_cls) = axis(az, "←", "→");
        let (alt_txt, alt_cls) = axis(alt, "↓", "↑");
        let total_cls = quality_cls(total.unwrap_or(f64::NAN));
        let total_txt = total.map(format_deg_as_dms_small).unwrap_or_else(|| "—".into());
        view! {
            <div class="grid grid-cols-[minmax(0,1fr)_auto] gap-3 items-center">
                <div class="flex flex-col gap-2 min-w-0">
                    {tile(tr.pa_total, total_txt, total_cls)}
                    {tile(tr.pa_az_error, az_txt, az_cls)}
                    {tile(tr.pa_alt_error, alt_txt, alt_cls)}
                </div>
                {bullseye(az, alt, total_cls)}
            </div>
            {original.map(|o| view! {
                <div class="text-xs text-text-muted font-mono">
                    {format!("{} {}", tr.pa_original, format_deg_as_dms_small(o))}
                </div>
            })}
        }
    };

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::PolarAlign)></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{move || tr().tab_polar_align}</span>
                <span class=move || format!("{} ml-auto min-w-0 truncate", stage_badge(&stage.get()))>
                    {move || {
                        let s = stage.get();
                        if s.is_empty() { tr().idle.to_string() } else { s }
                    }}
                </span>
                <button class="btn-icon shrink-0 text-text-muted"
                        title=move || tr().pa_settings
                        on:click=move |_| settings_open.set(true)>
                    <span class="inline-block w-5 h-5" inner_html=tab_icon(Tab::Profiles)></span>
                </button>
            </div>

            // Body — a column on phones, frame | controls on md+.
            <div class="flex-1 min-h-0 flex flex-col \
                        md:grid md:grid-cols-[minmax(0,1fr)_300px] lg:grid-cols-[minmax(0,1fr)_340px] \
                        md:grid-rows-[minmax(0,1fr)] md:gap-3 md:p-3 md:pr-6">
                // Live frame from KStars' align module (uuid "+A" — every
                // capture, solve and refresh iteration streams a JPEG). Pinned
                // on phones; just a strip until the first frame arrives.
                <div class=move || format!(
                    "relative shrink-0 overflow-hidden flex items-center justify-center \
                     bg-bg-input-deep border-b border-border-base \
                     md:h-auto md:min-h-0 md:border md:rounded-lg {}",
                    if has_frame() { "h-[40dvh] min-h-[200px]" } else { "h-24" })>
                    {move || match polar.with(|p| p.preview_url.clone()) {
                        Some(url) => view! {
                            <img src=url alt="align frame"
                                 class="max-w-full max-h-full object-contain [image-rendering:pixelated]" />
                        }.into_any(),
                        None => view! {
                            <div class="text-text-faint text-sm text-center px-6">{move || tr().pa_no_frame}</div>
                        }.into_any(),
                    }}
                </div>

                // Controls — scroll under the frame on phones.
                <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] flex flex-col gap-3 \
                            p-3 pb-[max(0.75rem,env(safe-area-inset-bottom))] md:p-0">
                    <div class=CARD>
                        // Step strip: three captures, then adjust.
                        <div class="grid grid-cols-4 gap-1.5">
                            {(1..=4u8).map(|n| view! {
                                <div class="flex flex-col gap-1 min-w-0">
                                    <div class=move || {
                                        let s = step.get();
                                        if s > n { "h-1 rounded-full bg-accent-cyan" }
                                        else if s == n { "h-1 rounded-full bg-accent-cyan animate-pulse" }
                                        else { "h-1 rounded-full bg-border-strong" }
                                    }></div>
                                    <span class=move || {
                                        if step.get() >= n { "text-xs truncate text-text-dim" }
                                        else { "text-xs truncate text-text-faint" }
                                    }>
                                        {move || if n == 4 {
                                            tr().pa_step_adjust.to_string()
                                        } else {
                                            format!("{} {n}", tr().pa_step)
                                        }}
                                    </span>
                                </div>
                            }).collect::<Vec<_>>()}
                        </div>

                        // KStars' own instruction for the current stage.
                        <div class="text-sm text-text-dim leading-snug line-clamp-3"
                             title=move || polar.with(|p| p.message.clone())>
                            {move || {
                                let m = polar.with(|p| p.message.clone());
                                match (m.is_empty(), step.get()) {
                                    (false, _) => m,
                                    (true, 0) => tr().pa_idle.to_string(),
                                    (true, _) => tr().pa_running.to_string(),
                                }
                            }}
                        </div>

                        <Show when=move || step.get() == 0>
                            <button class="btn btn-ghost w-full justify-between h-auto min-h-[44px] py-2"
                                    on:click=move |_| settings_open.set(true)>
                                <span class="min-w-0 truncate font-mono text-sm">{summary}</span>
                                <span class="inline-block w-4 h-4 shrink-0 text-text-muted"
                                      inner_html=tab_icon(Tab::Profiles)></span>
                            </button>
                            {move || meridian_warning().map(|w| view! {
                                <div class="py-2 px-3 rounded-md border border-state-warn/60 bg-state-warn/10 \
                                            text-state-warn text-sm leading-snug">{w}</div>
                            })}
                        </Show>

                        <Show when=move || step.get() == 4>{readout}</Show>

                        <button
                            class=move || match primary.get() {
                                Primary::Stop => "btn btn-danger w-full h-12 text-base",
                                Primary::RotationDone =>
                                    "btn btn-ghost w-full h-12 text-base text-accent-amber !border-accent-amber",
                                _ => "btn btn-primary w-full h-12 text-base",
                            }
                            on:click=on_primary
                        >
                            {move || {
                                let tr = tr();
                                match primary.get() {
                                    Primary::Start => format!("\u{25B6}\u{FE0E} {}", tr.pa_start_btn_long),
                                    Primary::RotationDone => format!("\u{2713} {}", tr.pa_rotation_done),
                                    Primary::Refresh => format!("\u{25B6}\u{FE0E} {}", tr.pa_start_refresh),
                                    Primary::Stop => format!("\u{25A0} {}", tr.stop),
                                }
                            }}
                        </button>

                        // Stop stays reachable when the big button does
                        // something else; Reset view only while adjusting.
                        <div class="flex gap-2"
                             class:hidden=move || { step.get() == 0 || (primary.get() == Primary::Stop && step.get() != 4) }>
                            <Show when=move || primary.get() != Primary::Stop>
                                <button class="btn btn-danger flex-1" on:click=on_stop.clone()>
                                    {move || format!("\u{25A0} {}", tr().stop)}
                                </button>
                            </Show>
                            <Show when=move || step.get() == 4>
                                <button class="btn btn-ghost flex-1" on:click=on_reset_view.clone()>
                                    {move || tr().pa_reset_view}
                                </button>
                            </Show>
                        </div>
                    </div>
                </div>
            </div>

            // Settings — bottom sheet on phones, floating panel on md+.
            <Show when=move || settings_open.get()>
                <div class="absolute inset-0 z-[70] bg-[rgba(2,4,10,0.6)]"
                     on:click=move |_| settings_open.set(false)></div>
                <div class="panel absolute z-[80] inset-x-0 bottom-0 max-h-[80dvh] rounded-b-none \
                            pb-[max(0.75rem,env(safe-area-inset-bottom))] \
                            md:inset-x-auto md:bottom-auto md:top-14 md:right-6 md:w-[380px] \
                            md:max-h-[calc(100%-4.5rem)] md:rounded-lg md:pb-3 \
                            overflow-y-auto [overscroll-behavior:contain] px-3 pt-2 flex flex-col gap-1 text-sm">
                    <div class="flex items-center justify-between">
                        <span class="font-semibold text-text-blue">{move || tr().pa_settings}</span>
                        <button class="btn-icon" title=move || tr().info_close
                                on:click=move |_| settings_open.set(false)>"\u{2716}"</button>
                    </div>

                    {setting_row(move || tr().pa_direction, view! {
                        <div class="flex gap-1.5">
                            {DIRECTION_WIRE.iter().map(|&wire| {
                                let set = set_direction.clone();
                                view! {
                                    <button
                                        class=move || if direction_local.with(|d| d == wire) {
                                            format!("{CHIP} btn--active")
                                        } else {
                                            CHIP.to_string()
                                        }
                                        on:click=move |_| set(wire)
                                    >
                                        {move || direction_label(wire, tr())}
                                    </button>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    })}

                    {setting_row(move || tr().pa_rotation_deg_label, view! {
                        <input type="number" min="15" max="60" step="1" inputmode="numeric" class=INPUT
                               prop:value=move || rotation_local.get().to_string()
                               on:change=on_rotation_change.clone() />
                    })}

                    {setting_row(move || tr().pa_mount_speed_label, view! {
                        <select class=INPUT on:change=on_speed_change.clone()>
                            {move || {
                                let tr_ = tr();
                                let current = speed_local.get();
                                let mut opts: Vec<String> = DEFAULT_SPEED_OPTIONS
                                    .iter().map(|s| s.to_string()).collect();
                                // Keep an unexpected current value selectable.
                                if !current.is_empty() && !opts.contains(&current) {
                                    opts.insert(0, current.clone());
                                }
                                opts.into_iter().map(|wire| {
                                    let label = speed_label(&wire, tr_);
                                    let sel = wire == current;
                                    view! { <option value=wire selected=sel>{label}</option> }
                                }).collect::<Vec<_>>()
                            }}
                        </select>
                    })}

                    {setting_row(move || tr().pa_manual_slew_label, view! {
                        <input type="checkbox" class="w-5 h-5 min-h-0 shrink-0 accent-accent-cyan"
                               prop:checked=move || manual_local.get()
                               on:change=on_manual_change.clone() />
                    })}

                    {setting_row(move || tr().pa_exposure_s_label, view! {
                        <input type="number" min="0.1" max="60" step="0.1" inputmode="decimal" class=INPUT
                               prop:value=move || exposure.get().to_string()
                               on:change=on_exposure_change.clone() />
                    })}

                    {setting_row(move || tr().pa_algorithm_label, view! {
                        <select class=INPUT on:change=on_algo_change.clone()>
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
                    })}
                </div>
            </Show>
        </div>
    }
}

/// One settings row: label left, control right. A `<div>`, not a `<label>`:
/// the direction row holds two buttons, and a label would forward clicks on
/// its text to the first one.
fn setting_row(
    label: impl Fn() -> &'static str + Send + 'static,
    control: impl IntoView,
) -> impl IntoView {
    view! {
        <div class="flex items-center justify-between gap-3 min-h-[44px]">
            <span class="min-w-0 truncate text-text-blue">{move || label()}</span>
            {control}
        </div>
    }
}

/// One error readout: a small uppercase label left, the mono value right.
fn tile(label: &'static str, value: String, value_cls: &'static str) -> impl IntoView {
    view! {
        <div class="min-w-0 rounded-lg bg-bg-elev-1 border border-border-base px-2 py-1.5 \
                    flex items-baseline justify-between gap-2">
            <span class="text-xs uppercase tracking-[0.06em] text-text-muted truncate">{label}</span>
            <span class=format!("font-mono text-lg leading-tight whitespace-nowrap {value_cls}")>{value}</span>
        </div>
    }
}

/// Error bullseye. The dot sits at (az, −alt), so following the arrows walks
/// it to the center. Square-root radius so both 1′ and 15′ stay readable:
/// rings at 1′, 5′ and 15′, the rim is 30′ (larger errors pin to it).
fn bullseye(az_deg: f64, alt_deg: f64, dot_cls: &'static str) -> impl IntoView {
    const R: f64 = 54.0;
    let radius = |arcmin: f64| R * (arcmin / 30.0).sqrt();
    let e = az_deg.hypot(alt_deg) * 60.0; // arcmin
    let dot = e.is_finite().then(|| {
        if e <= 0.0 { return (0.0, 0.0); }
        let r = radius(e.min(30.0)) / e;
        (r * az_deg * 60.0, -r * alt_deg * 60.0)
    });
    view! {
        <svg viewBox="-60 -60 120 120" class="w-[120px] h-[120px] shrink-0">
            {[1.0, 5.0, 15.0, 30.0].map(|m| view! {
                <circle r=format!("{:.1}", radius(m)) fill="none"
                        stroke="var(--border-strong)" stroke-width="1"/>
            })}
            <line x1="-58" y1="0" x2="58" y2="0" stroke="var(--border-strong)" stroke-width="0.6"/>
            <line x1="0" y1="-58" x2="0" y2="58" stroke="var(--border-strong)" stroke-width="0.6"/>
            <circle r="1.6" fill="var(--text-muted)"/>
            {dot.map(|(x, y)| view! {
                <circle cx=format!("{x:.1}") cy=format!("{y:.1}") r="4.5"
                        fill="currentColor" class=dot_cls/>
            })}
        </svg>
    }
}
