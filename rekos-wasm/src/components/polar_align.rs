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
use crate::ws::{PolarVectorData, SendCmd};

const DIRECTION_OPTIONS: &[&str] = &["West", "East"];
const ALGORITHM_OPTIONS: &[&str] = &[
    "Plate Solve",
    "Move Star",
    "Move Star & Calc Error",
];
const DEFAULT_SPEED_OPTIONS: &[&str] = &[
    "1x", "2x", "4x", "8x", "16x", "32x", "64x", "128x", "256x", "Max",
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_cmd(send: &SendCmd, t: &str, payload: serde_json::Value) {
    let msg = serde_json::json!({ "type": t, "payload": payload }).to_string();
    send(msg);
}

fn dispatch_align_setting(send: &SendCmd, key: &str, value: serde_json::Value) {
    let mut inner = serde_json::Map::new();
    inner.insert(key.to_string(), value);
    send_cmd(
        send,
        "align_set_all_settings",
        serde_json::json!({ "settings": serde_json::Value::Object(inner) }),
    );
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

fn action_btn(color: &str) -> String {
    format!(
        "padding:8px 12px; background:rgba(12,14,24,0.9); \
         border:1px solid {c}; color:{c}; cursor:pointer; \
         font-family:monospace; font-size:12px;",
        c = color
    )
}

fn input_style() -> &'static str {
    "flex:1; background:#06060c; color:#cfe0ff; border:1px solid #222; \
     padding:4px 6px; font-family:monospace; font-size:12px;"
}

fn field_label_style() -> &'static str {
    "flex:0 0 120px; color:#88aaff; font-size:11px;"
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

    view! {
        <div class="polar-tab-root"
             style="position:absolute; inset:0; background:#0a0a0f; color:#c0c0d0; \
                    font-family:monospace; display:grid; \
                    grid-template-rows:56px 1fr; overflow:hidden;">

            // ── Header ────────────────────────────────────────────────
            <div class="polar-header"
                 style="display:flex; align-items:center; gap:14px; \
                        padding:0 20px 0 80px; border-bottom:1px solid #222; \
                        background:rgba(6,6,15,0.85); font-size:13px;">
                <span style=move || format!(
                    "display:inline-block; padding:4px 10px; border-radius:14px; \
                     border:1px solid {c}; color:{c}; font-size:11px;",
                    c = stage_color(&polar.with(|p| p.stage.clone()))
                )>
                    {move || {
                        let s = polar.with(|p| p.stage.clone());
                        if s.is_empty() { "Idle".to_string() } else { s }
                    }}
                </span>
                <span style="color:#88aaff;">"Enabled:"</span>
                <span>{move || if polar.with(|p| p.enabled) { "yes" } else { "no" }}</span>
                <span style="flex:1; color:#c0c0d0; overflow:hidden; \
                             white-space:nowrap; text-overflow:ellipsis;"
                      title=move || polar.with(|p| p.message.clone())>
                    {move || polar.with(|p| p.message.clone())}
                </span>
            </div>

            // ── Body ──────────────────────────────────────────────────
            <div class="polar-body"
                 style="overflow-y:auto; padding:16px 20px; \
                        display:flex; flex-direction:column; gap:14px;">

                // Live frame from KStars align module (uuid "+A" — every
                // capture, solve, and refresh iteration streams a JPEG).
                <Show when=move || polar.with(|p| p.preview_url.is_some())>
                    <div style="display:flex; justify-content:center; \
                                align-items:center; background:#06060c; \
                                border:1px solid #222; padding:8px; \
                                min-height:220px; max-height:440px;">
                        <img
                            src=move || polar.with(|p|
                                p.preview_url.clone().unwrap_or_default())
                            alt="align frame"
                            style="max-width:100%; max-height:424px; \
                                   object-fit:contain; display:block; \
                                   image-rendering:pixelated;"
                        />
                    </div>
                </Show>

                // Intro section
                <Show when=intro_visible>
                    <fieldset style="border:1px solid #222; padding:12px 14px;">
                        <legend style="color:#88aaff; padding:0 6px; font-size:11px;">
                            "Pre-start options"
                        </legend>
                        <div style="display:flex; flex-direction:column; gap:8px;">

                            // Direction
                            <div style="display:flex; align-items:center; gap:8px;">
                                <span style=field_label_style()>"Direction"</span>
                                <select
                                    on:change=on_direction_change.clone()
                                    style=input_style()
                                    prop:value=move || polar.with(|p|
                                        settings_str(&p.settings, "pAHDirection")
                                            .unwrap_or_else(|| "West".into()))
                                >
                                    {DIRECTION_OPTIONS.iter().map(|o| view! {
                                        <option value=*o>{*o}</option>
                                    }).collect::<Vec<_>>()}
                                </select>
                            </div>

                            // Rotation
                            <div style="display:flex; align-items:center; gap:8px;">
                                <span style=field_label_style()>"Rotation °"</span>
                                <input
                                    type="number"
                                    min="15"
                                    max="60"
                                    step="1"
                                    on:change=on_rotation_change.clone()
                                    prop:value=move || polar.with(|p|
                                        settings_i64(&p.settings, "pAHRotation")
                                            .unwrap_or(30).to_string())
                                    style=input_style()
                                />
                            </div>

                            // Mount speed
                            <div style="display:flex; align-items:center; gap:8px;">
                                <span style=field_label_style()>"Mount speed"</span>
                                <select
                                    on:change=on_speed_change.clone()
                                    style=input_style()
                                    prop:value=move || polar.with(|p|
                                        settings_str(&p.settings, "pAHMountSpeed")
                                            .unwrap_or_default())
                                >
                                    {move || {
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
                                        opts.into_iter().map(|o| {
                                            let label = o.clone();
                                            view! { <option value=o>{label}</option> }
                                        }).collect::<Vec<_>>()
                                    }}
                                </select>
                            </div>

                            // Manual slew
                            <div style="display:flex; align-items:center; gap:8px;">
                                <span style=field_label_style()>"Manual slew"</span>
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
                                <div style="margin-top:4px; padding:8px 10px; \
                                            border:1px solid #ff9a3a; \
                                            background:rgba(80,40,10,0.4); \
                                            color:#ffb070; font-size:11px; \
                                            line-height:1.4;">
                                    {move || meridian_warning().unwrap_or_default()}
                                </div>
                            </Show>

                            <div style="margin-top:6px;">
                                <button on:click=on_start.clone() style=action_btn("#7affa0")>
                                    "Start polar alignment"
                                </button>
                            </div>
                        </div>
                    </fieldset>
                </Show>

                // Progress section
                <Show when=progress_visible>
                    <fieldset style="border:1px solid #222; padding:12px 14px;">
                        <legend style="color:#88aaff; padding:0 6px; font-size:11px;">
                            "Capture / Solve"
                        </legend>
                        <div style="padding:8px 0; color:#cfe0ff;">
                            {move || {
                                let m = polar.with(|p| p.message.clone());
                                if m.is_empty() { "Running…".to_string() } else { m }
                            }}
                        </div>
                        <div>
                            <button on:click=on_stop_abort.clone() style=action_btn("#ff6a6a")>
                                "Abort"
                            </button>
                        </div>
                    </fieldset>
                </Show>

                // Manual rotation section
                <Show when=rotation_visible>
                    <fieldset style="border:1px solid #222; padding:12px 14px;">
                        <legend style="color:#ffd060; padding:0 6px; font-size:11px;">
                            "Manual rotation"
                        </legend>
                        <div style="padding:4px 0 10px; color:#cfe0ff;">
                            "Rotate the mount by the configured angle, then click Done."
                        </div>
                        <div>
                            <button on:click=on_slew_done.clone() style=action_btn("#ffd060")>
                                "Rotation done"
                            </button>
                        </div>
                    </fieldset>
                </Show>

                // Refresh section
                <Show when=refresh_visible>
                    <fieldset style="border:1px solid #222; padding:12px 14px;">
                        <legend style="color:#7affa0; padding:0 6px; font-size:11px;">
                            "Refresh / Correct"
                        </legend>
                        <div style="display:grid; grid-template-columns:1fr 140px; gap:14px;">
                            <div style="display:flex; flex-direction:column; gap:8px;">

                                // Exposure
                                <div style="display:flex; align-items:center; gap:8px;">
                                    <span style=field_label_style()>"Exposure s"</span>
                                    <input
                                        type="number"
                                        min="0.1"
                                        max="60"
                                        step="0.1"
                                        on:change=on_exposure_change.clone()
                                        prop:value=move || exposure.get().to_string()
                                        style=input_style()
                                    />
                                </div>

                                // Algorithm
                                <div style="display:flex; align-items:center; gap:8px;">
                                    <span style=field_label_style()>"Algorithm"</span>
                                    <select
                                        on:change=on_algo_change.clone()
                                        style=input_style()
                                        prop:value=move || polar.with(|p|
                                            settings_str(&p.settings, "pAHRefreshAlgorithm")
                                                .unwrap_or_else(|| "Plate Solve".into()))
                                    >
                                        {ALGORITHM_OPTIONS.iter().map(|o| view! {
                                            <option value=*o>{*o}</option>
                                        }).collect::<Vec<_>>()}
                                    </select>
                                </div>

                                // Error readouts
                                <div style="display:grid; grid-template-columns:1fr 1fr; \
                                            gap:8px; margin-top:6px;">
                                    <div style="border:1px solid #222; padding:6px 8px;">
                                        <div style="color:#88aaff; font-size:11px;">"Original"</div>
                                        {move || {
                                            let v = polar.with(|p| p.vector.clone());
                                            let (err, az, alt) = match v {
                                                Some(v) => (v.error, v.az_error, v.alt_error),
                                                None => (f64::NAN, f64::NAN, f64::NAN),
                                            };
                                            view! {
                                                <div style="font-size:12px;">
                                                    "Total: " {format_deg_as_dms_small(err)}
                                                </div>
                                                <div style="font-size:12px;">
                                                    "Az:    " {format_deg_as_dms_small(az)}
                                                </div>
                                                <div style="font-size:12px;">
                                                    "Alt:   " {format_deg_as_dms_small(alt)}
                                                </div>
                                            }
                                        }}
                                    </div>
                                    <div style="border:1px solid #222; padding:6px 8px;">
                                        <div style="color:#7affa0; font-size:11px;">"Updated"</div>
                                        {move || {
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
                                                <div style="font-size:12px;">"Total: " {fmt(e)}</div>
                                                <div style="font-size:12px;">"Az:    " {fmt(az)}</div>
                                                <div style="font-size:12px;">"Alt:   " {fmt(al)}</div>
                                            }
                                        }}
                                    </div>
                                </div>

                                // Refresh controls
                                <div style="display:flex; gap:8px; margin-top:8px;">
                                    <button on:click=on_start_refresh.clone() style=action_btn("#7affa0")>
                                        "Start refresh"
                                    </button>
                                    <button on:click=on_stop_refresh.clone() style=action_btn("#ffd060")>
                                        "Stop refresh"
                                    </button>
                                </div>
                            </div>

                            // Correction vector preview
                            <div>
                                {move || correction_svg(polar.with(|p| p.vector.clone()))}
                            </div>
                        </div>
                    </fieldset>
                </Show>

                // Footer — always-visible global Stop
                <div style="display:flex; gap:8px;">
                    <button on:click=on_stop_footer style=action_btn("#ff6a6a")>
                        "Stop polar alignment"
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
        <div style="display:flex; flex-direction:column; align-items:center; gap:6px;">
            <svg width="120" height="120" viewBox="-60 -60 120 120"
                 style="background:#06060c; border:1px solid #222;">
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
            <div style="font-size:10px; color:#c0c0d0; text-align:center; line-height:1.4;">
                <div>"Err "  {format_deg_as_dms_small(err)}</div>
                <div>"Az "   {format_deg_as_dms_small(az)}</div>
                <div>"Alt "  {format_deg_as_dms_small(alt)}</div>
            </div>
        </div>
    }
}
