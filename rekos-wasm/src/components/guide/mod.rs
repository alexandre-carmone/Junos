//! Guiding module UI — fullscreen tab.
//!
//! Wire protocol: see kstars/ekos/ekoslive/message.cpp::processGuideCommands
//! (lines 649-692) for outbound commands, and `new_guide_state` emission at
//! message.cpp:2581-2583.
//!
//! Outbound (browser → KStars):
//!   - guide_start, guide_stop, guide_capture, guide_loop, guide_clear
//!   - guide_get_all_settings (primed + refreshed from ws.rs)
//!   - guide_set_all_settings {<widgetName>: <value>}  (no wrapper — the
//!     map is the payload root, unlike align_set_all_settings which wraps
//!     under {settings:{...}}; see message.cpp:673)
//!   - option_set / option_get for GuiderType and PHD2/LinGuider host+port
//!     (global `Options::` values, not inside guide_get_all_settings).
//!
//! Inbound (KStars → browser):
//!   - new_guide_state {status} — one of the 20 labels in ekos.h:20-40.
//!   - guide_get_all_settings — flat widget map.
//!   - option_get [{name, value}, ...] — reply to our option_get.
//!   - new_preview_image with uuid "+G*" — guide camera frame (Internal
//!     guider only, or PHD2 when its camera matches the Ekos guide camera).
//!
//! Deliberately NOT wired: guide_report (declared in commands.h but has no
//! handler branch in processGuideCommands — silently dropped), dither-now
//! / suspend / resume (not exposed over Ekos Live at all — they are DBUS/
//! Q_SCRIPTABLE only in KStars).

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::compat::GuideSnapshot;
use crate::ws::SendCmd;

mod timeline;
use timeline::drift_plot;

// ---------------------------------------------------------------------------
// Combo option lists (sourced from kstars/ekos/guide/*.ui)
// ---------------------------------------------------------------------------

const BINNING_OPTIONS:  &[&str] = &["1x1", "2x2", "3x3", "4x4"];
const SQUARE_OPTIONS:   &[&str] = &["8", "16", "32", "64", "128"];
const PULSE_ALGO_OPTS:  &[&str] = &["Standard", "Hysteresis", "Linear", "GPG"];
const GUIDE_ALGO_OPTS:  &[&str] = &[
    "Smart",
    "SEP",
    "Fast",
    "Auto Threshold",
    "No Threshold",
    "SEP Multi Star (recommended)",
];

// ---------------------------------------------------------------------------
// Low-level helpers — settings accessors + form-event extractors
// ---------------------------------------------------------------------------

fn settings_str(settings: &serde_json::Value, key: &str) -> Option<String> {
    settings.get(key).and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b)   => Some(b.to_string()),
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

// ---------------------------------------------------------------------------
// Style constants
// ---------------------------------------------------------------------------

fn action_btn(color: &str, enabled: bool) -> String {
    let (border, c, opacity) = if enabled {
        (color, color, "1")
    } else {
        ("#333", "#666", "0.55")
    };
    format!(
        "padding:8px 12px; background:rgba(12,14,24,0.9); \
         border:1px solid {border}; color:{c}; \
         cursor:{cursor}; font-family:monospace; font-size:12px; \
         opacity:{opacity};",
        border = border,
        c = c,
        cursor = if enabled { "pointer" } else { "not-allowed" },
        opacity = opacity,
    )
}

fn input_style() -> &'static str {
    "flex:1; min-width:0; background:#06060c; color:#cfe0ff; \
     border:1px solid #222; padding:4px 6px; \
     font-family:monospace; font-size:12px;"
}

fn field_label_style() -> &'static str {
    "flex:0 0 200px; color:#88aaff; font-size:11px;"
}

fn row_style() -> &'static str {
    "display:flex; align-items:center; gap:8px;"
}

fn fieldset_style() -> &'static str {
    "border:1px solid #222; padding:10px 14px; margin:0;"
}

fn legend_style() -> &'static str {
    "color:#88aaff; padding:0 6px; font-size:11px; cursor:pointer;"
}

fn stage_color(status: &str) -> &'static str {
    match status {
        "" | "Idle" | "Aborted" | "Disconnected"        => "#808090",
        "Calibrating" | "Selecting star" | "Looping"
        | "Capturing" | "Subtracting" | "Subframing"
        | "Reacquiring"                                 => "#ffd060",
        "Calibrated" | "Connected"                      => "#88aaff",
        "Guiding"                                       => "#7affa0",
        "Dithering" | "Dithering successful"
        | "Manual Dithering" | "Settling"               => "#66e0e0",
        "Calibration error" | "Dithering error"
        | "Suspended"                                   => "#ff6a6a",
        _                                               => "#c0c0d0",
    }
}

// ---------------------------------------------------------------------------
// Command dispatchers
// ---------------------------------------------------------------------------

fn send_cmd(send: &SendCmd, t: &str, payload: serde_json::Value) {
    let msg = serde_json::json!({ "type": t, "payload": payload }).to_string();
    send(msg);
}

/// `guide_set_all_settings` payload is the widget map directly at payload
/// root — see message.cpp:673 (`auto settings = payload.toVariantMap()`).
/// This differs from `align_set_all_settings` which expects `{settings:{...}}`.
fn dispatch_guide_setting(send: &SendCmd, key: &str, value: serde_json::Value) {
    let mut map = serde_json::Map::new();
    map.insert(key.to_string(), value);
    send_cmd(send, "guide_set_all_settings", serde_json::Value::Object(map));
}

/// Set one KStars `Options::` entry (GuiderType, PHD2Host, etc), then
/// immediately re-read the 5 guide-relevant options so the UI reflects
/// the confirmed value (KStars does not echo option_set back).
fn dispatch_option(send: &SendCmd, name: &str, value: serde_json::Value) {
    let set = serde_json::json!({
        "type": "option_set",
        "payload": { "options": [ { "name": name, "value": value } ] }
    }).to_string();
    send(set);
    refresh_guide_options(send);
}

fn refresh_guide_options(send: &SendCmd) {
    let get = r#"{"type":"option_get","payload":{"options":[{"name":"GuiderType"},{"name":"PHD2Host"},{"name":"PHD2Port"},{"name":"LinGuiderHost"},{"name":"LinGuiderPort"}]}}"#;
    send(get.to_string());
}

// ---------------------------------------------------------------------------
// Button-gating rules (mirror guide.cpp::isGuiderActive() + processGuideCommands)
// ---------------------------------------------------------------------------

fn is_idle(status: &str) -> bool {
    matches!(
        status,
        "" | "Idle" | "Aborted" | "Connected"
        | "Calibrated" | "Calibration error" | "Disconnected"
    )
}

fn can_start(status: &str) -> bool { is_idle(status) }

fn can_stop(status: &str) -> bool {
    !matches!(status, "" | "Idle" | "Aborted" | "Disconnected")
}

fn can_capture_or_loop(status: &str, is_internal: bool) -> bool {
    is_internal && is_idle(status)
}

fn can_clear(status: &str) -> bool {
    !matches!(status, "Calibrating" | "Guiding" | "Dithering")
}

fn guider_type_label(v: i64) -> &'static str {
    match v { 1 => "PHD2", 2 => "LinGuider", _ => "Internal" }
}

// ---------------------------------------------------------------------------
// Row-rendering helpers
// ---------------------------------------------------------------------------

/// Checkbox row bound to `guide.settings[key]`.
fn bool_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    key: &'static str,
    label: &'static str,
) -> impl IntoView {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        dispatch_guide_setting(&s, key, serde_json::Value::Bool(event_target_checked(&ev)));
    };
    view! {
        <div style=row_style()>
            <span style=field_label_style()>{label}</span>
            <input type="checkbox"
                   on:change=on_change
                   prop:checked=move || guide.with(|g|
                       settings_bool(&g.settings, key).unwrap_or(false)) />
        </div>
    }
}

/// Integer spinbox row (QSpinBox-equivalent).
fn int_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    key: &'static str,
    label: &'static str,
    min: i64,
    max: i64,
    step: i64,
) -> impl IntoView {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        let raw = event_target_value(&ev);
        if let Ok(n) = raw.parse::<i64>() {
            let n = n.clamp(min, max);
            dispatch_guide_setting(&s, key, serde_json::Value::Number(n.into()));
        }
    };
    view! {
        <div style=row_style()>
            <span style=field_label_style()>{label}</span>
            <input type="number"
                   min=min.to_string()
                   max=max.to_string()
                   step=step.to_string()
                   on:change=on_change
                   prop:value=move || guide.with(|g|
                       settings_i64(&g.settings, key).map(|v| v.to_string())
                           .unwrap_or_default())
                   style=input_style() />
        </div>
    }
}

/// Double spinbox row (QDoubleSpinBox-equivalent).
fn float_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    key: &'static str,
    label: &'static str,
    min: f64,
    max: f64,
    step: f64,
) -> impl IntoView {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        let raw = event_target_value(&ev);
        if let Ok(v) = raw.parse::<f64>() {
            let v = v.clamp(min, max);
            if let Some(n) = serde_json::Number::from_f64(v) {
                dispatch_guide_setting(&s, key, serde_json::Value::Number(n));
            }
        }
    };
    view! {
        <div style=row_style()>
            <span style=field_label_style()>{label}</span>
            <input type="number"
                   min=min.to_string()
                   max=max.to_string()
                   step=step.to_string()
                   on:change=on_change
                   prop:value=move || guide.with(|g|
                       settings_f64(&g.settings, key).map(|v| format!("{v}"))
                           .unwrap_or_default())
                   style=input_style() />
        </div>
    }
}

/// Combo-box row bound to a widget's `currentText`. Because KStars exposes
/// combos as strings (see Guide::getAllSettings), we send a string back.
fn select_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    key: &'static str,
    label: &'static str,
    options: &'static [&'static str],
) -> impl IntoView {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        dispatch_guide_setting(
            &s,
            key,
            serde_json::Value::String(event_target_select(&ev)),
        );
    };
    view! {
        <div style=row_style()>
            <span style=field_label_style()>{label}</span>
            <select on:change=on_change
                    style=input_style()
                    prop:value=move || guide.with(|g|
                        settings_str(&g.settings, key).unwrap_or_default())>
                {move || {
                    let cur = guide.with(|g|
                        settings_str(&g.settings, key).unwrap_or_default());
                    let mut opts: Vec<String> = options.iter().map(|s| s.to_string()).collect();
                    if !cur.is_empty() && !opts.iter().any(|s| s == &cur) {
                        opts.insert(0, cur);
                    }
                    opts.into_iter().map(|o| {
                        let l = o.clone();
                        view! { <option value=o>{l}</option> }
                    }).collect::<Vec<_>>()
                }}
            </select>
        </div>
    }
}

/// Text input row bound to a global `Options::` entry (e.g. PHD2Host).
fn text_option_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    option: &'static str,
    label: &'static str,
) -> impl IntoView {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        dispatch_option(&s, option, serde_json::Value::String(event_target_value(&ev)));
    };
    view! {
        <div style=row_style()>
            <span style=field_label_style()>{label}</span>
            <input type="text"
                   on:change=on_change
                   prop:value=move || guide.with(|g|
                       settings_str(&g.options, option).unwrap_or_default())
                   style=input_style() />
        </div>
    }
}

/// Integer input bound to a global `Options::` entry (e.g. PHD2Port).
fn int_option_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    option: &'static str,
    label: &'static str,
    min: i64,
    max: i64,
) -> impl IntoView {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        let raw = event_target_value(&ev);
        if let Ok(n) = raw.parse::<i64>() {
            let n = n.clamp(min, max);
            dispatch_option(&s, option, serde_json::Value::Number(n.into()));
        }
    };
    view! {
        <div style=row_style()>
            <span style=field_label_style()>{label}</span>
            <input type="number"
                   min=min.to_string()
                   max=max.to_string()
                   step="1"
                   on:change=on_change
                   prop:value=move || guide.with(|g|
                       settings_i64(&g.options, option).map(|v| v.to_string())
                           .unwrap_or_default())
                   style=input_style() />
        </div>
    }
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

#[component]
pub fn GuideTab(
    #[prop(into)] guide: Signal<GuideSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    // Action-button dispatchers (one Arc clone each — matches polar_align).
    let s_start = send.clone();
    let on_start = move |_| send_cmd(&s_start, "guide_start", serde_json::json!({}));

    let s_stop = send.clone();
    let on_stop = move |_| send_cmd(&s_stop, "guide_stop", serde_json::json!({}));

    let s_capture = send.clone();
    let on_capture = move |_| send_cmd(&s_capture, "guide_capture", serde_json::json!({}));

    let s_loop = send.clone();
    let on_loop = move |_| send_cmd(&s_loop, "guide_loop", serde_json::json!({}));

    let s_clear = send.clone();
    let on_clear = move |_| send_cmd(&s_clear, "guide_clear", serde_json::json!({}));

    // Guider-backend radio — dispatches Options::GuiderType (int).
    let s_internal = send.clone();
    let on_internal  = move |_| dispatch_option(&s_internal,  "GuiderType", serde_json::Value::Number(0.into()));
    let s_phd2 = send.clone();
    let on_phd2      = move |_| dispatch_option(&s_phd2,      "GuiderType", serde_json::Value::Number(1.into()));
    let s_linguider = send.clone();
    let on_linguider = move |_| dispatch_option(&s_linguider, "GuiderType", serde_json::Value::Number(2.into()));

    // Derive guider type from options map; default 0 (Internal).
    let guider_type = move || guide.with(|g|
        settings_i64(&g.options, "GuiderType").unwrap_or(0));
    let is_internal = move || guider_type() == 0;

    // Button gating closures — refresh on each render via guide signal.
    let status = move || guide.with(|g| g.status.clone());
    let btn_start    = move || can_start(&status());
    let btn_stop     = move || can_stop(&status());
    let btn_capture  = move || can_capture_or_loop(&status(), is_internal());
    let btn_loop     = move || can_capture_or_loop(&status(), is_internal());
    let btn_clear    = move || can_clear(&status());

    view! {
        <div class="guide-tab-root"
             style="position:absolute; inset:0; background:#0a0a0f; color:#c0c0d0; \
                    font-family:monospace; display:grid; \
                    grid-template-rows:56px 1fr; overflow:hidden;">

            // ── Header ───────────────────────────────────────────────────
            <div class="guide-header"
                 style="display:flex; align-items:center; gap:14px; \
                        padding:0 20px 0 80px; border-bottom:1px solid #222; \
                        background:rgba(6,6,15,0.85); font-size:13px;">
                <span style=move || format!(
                    "display:inline-block; padding:4px 10px; border-radius:14px; \
                     border:1px solid {c}; color:{c}; font-size:11px;",
                    c = stage_color(&status())
                )>
                    {move || {
                        let s = status();
                        if s.is_empty() { "Idle".to_string() } else { s }
                    }}
                </span>
                <span style="color:#88aaff;">"Guider:"</span>
                <span>{move || guider_type_label(guider_type())}</span>
                <span style="color:#88aaff; margin-left:8px;">"RMS:"</span>
                <span style="font-size:12px;">
                    {move || guide.with(|g| {
                        let ra = g.ra_rms.map(|v| format!("{v:.2}\"")).unwrap_or_else(|| "—".into());
                        let de = g.de_rms.map(|v| format!("{v:.2}\"")).unwrap_or_else(|| "—".into());
                        format!("RA {ra}  DEC {de}")
                    })}
                </span>
                <span style="flex:1;"></span>
                <span style="color:#88aaff;">"Connected:"</span>
                <span>{move || if guide.with(|g| g.connected) { "yes" } else { "no" }}</span>
            </div>

            // ── Body ─────────────────────────────────────────────────────
            <div class="guide-body"
                 style="overflow-y:auto; padding:16px 20px; \
                        display:flex; flex-direction:column; gap:14px;">

                // Preview frame (uuid "+G*" from kstars media.cpp:753)
                <Show when=move || guide.with(|g| g.preview_url.is_some())>
                    <div style="display:flex; justify-content:center; \
                                align-items:center; background:#06060c; \
                                border:1px solid #222; padding:8px; \
                                min-height:180px; max-height:400px;">
                        <img
                            src=move || guide.with(|g|
                                g.preview_url.clone().unwrap_or_default())
                            alt="guide frame"
                            style="max-width:100%; max-height:384px; \
                                   object-fit:contain; display:block; \
                                   image-rendering:pixelated;"
                        />
                    </div>
                </Show>

                // Drift plot + state ribbon. Drift samples come from
                // kstars/ekos/manager.cpp:2772-2776 via partial
                // `new_guide_state {drift_ra, drift_de}` events.
                {move || guide.with(|g| drift_plot(&g.drift, &g.history))}

                // ── Action row ──────────────────────────────────────────
                <fieldset style=fieldset_style()>
                    <legend style=legend_style()>"Actions"</legend>
                    <div style="display:flex; flex-wrap:wrap; gap:8px;">
                        <button
                            on:click=on_start.clone()
                            disabled=move || !btn_start()
                            style=move || action_btn("#7affa0", btn_start())>
                            "Start guiding"
                        </button>
                        <button
                            on:click=on_stop.clone()
                            disabled=move || !btn_stop()
                            style=move || action_btn("#ff6a6a", btn_stop())>
                            "Stop"
                        </button>
                        <button
                            on:click=on_capture.clone()
                            disabled=move || !btn_capture()
                            style=move || action_btn("#88aaff", btn_capture())>
                            "Capture"
                        </button>
                        <button
                            on:click=on_loop.clone()
                            disabled=move || !btn_loop()
                            style=move || action_btn("#88aaff", btn_loop())>
                            "Loop"
                        </button>
                        <button
                            on:click=on_clear.clone()
                            disabled=move || !btn_clear()
                            style=move || action_btn("#ffd060", btn_clear())>
                            "Clear calibration"
                        </button>
                    </div>
                    <div style="margin-top:6px; font-size:11px; color:#667;">
                        "Capture / Loop require the Internal guider. \
                         PHD2 handles framing itself."
                    </div>
                </fieldset>

                // ── Essentials ──────────────────────────────────────────
                <fieldset style=fieldset_style()>
                    <legend style=legend_style()>"Essentials"</legend>
                    <div style="display:flex; flex-direction:column; gap:8px;">
                        {float_row (&send, guide, "guideExposure",    "Exposure (s)",     0.1, 60.0, 0.1)}
                        {float_row (&send, guide, "guideDelay",       "Delay (s)",        0.0, 60.0, 0.1)}
                        {float_row (&send, guide, "guideGain",        "Gain",             0.0, 1000.0, 1.0)}
                        {select_row(&send, guide, "guideBinning",     "Binning",          BINNING_OPTIONS)}
                        {select_row(&send, guide, "guideSquareSize",  "Tracking box (px)", SQUARE_OPTIONS)}
                        {bool_row  (&send, guide, "guideDarkFrame",   "Dark frame")}
                        {bool_row  (&send, guide, "guideSubframe",    "Subframe")}
                        {bool_row  (&send, guide, "guideAutoStar",    "Auto select star")}
                        {bool_row  (&send, guide, "guideStreamingEnabled", "Stream guide frames")}
                    </div>
                </fieldset>

                // ── RA/DEC enable ───────────────────────────────────────
                <fieldset style=fieldset_style()>
                    <legend style=legend_style()>"RA / DEC corrections"</legend>
                    <div style="display:flex; flex-direction:column; gap:8px;">
                        {bool_row(&send, guide, "rAGuideEnabled",      "RA guiding")}
                        {bool_row(&send, guide, "eastRAGuideEnabled",  "  \u{2514} East pulses")}
                        {bool_row(&send, guide, "westRAGuideEnabled",  "  \u{2514} West pulses")}
                        {bool_row(&send, guide, "dECGuideEnabled",     "DEC guiding")}
                        {bool_row(&send, guide, "northDECGuideEnabled","  \u{2514} North pulses")}
                        {bool_row(&send, guide, "southDECGuideEnabled","  \u{2514} South pulses")}
                    </div>
                </fieldset>

                // ── Calibration (collapsible) ───────────────────────────
                <details style="border:1px solid #222;">
                    <summary style=legend_style()>"Calibration"</summary>
                    <div style="padding:10px 14px; display:flex; flex-direction:column; gap:8px;">
                        {int_row  (&send, guide, "kcfg_AutoModeIterations",         "Iterations",          1, 100, 1)}
                        {int_row  (&send, guide, "kcfg_CalibrationPulseDuration",   "Pulse duration (ms)", 100, 10000, 100)}
                        {int_row  (&send, guide, "kcfg_CalibrationMaxMove",         "Max move (px)",       1, 200, 1)}
                        {bool_row (&send, guide, "kcfg_TwoAxisEnabled",             "Two-axis")}
                        {bool_row (&send, guide, "kcfg_GuideAutoSquareSizeEnabled", "Auto tracking-box size")}
                        {bool_row (&send, guide, "kcfg_GuideCalibrationBacklash",   "Account for DEC backlash")}
                        {bool_row (&send, guide, "kcfg_ResetGuideCalibration",      "Reset calibration each start")}
                        {bool_row (&send, guide, "kcfg_ReuseGuideCalibration",      "Reuse calibration when possible")}
                        {bool_row (&send, guide, "kcfg_ReverseDecOnPierSideChange", "Reverse DEC on pier flip")}
                    </div>
                </details>

                // ── Dither (collapsible) ────────────────────────────────
                <details style="border:1px solid #222;">
                    <summary style=legend_style()>"Dither"</summary>
                    <div style="padding:10px 14px; display:flex; flex-direction:column; gap:8px;">
                        <div style="font-size:11px; color:#667; margin-bottom:4px;">
                            "No \"dither now\" button — the Ekos Live protocol \
                             only exposes dither as an auto-trigger during \
                             capture sequences via these settings."
                        </div>
                        {bool_row (&send, guide, "kcfg_DitherEnabled",             "Enable dithering")}
                        {float_row(&send, guide, "kcfg_DitherPixels",              "Amount (px)",        0.1, 30.0, 0.1)}
                        {int_row  (&send, guide, "kcfg_DitherFrames",              "Frames between",     1, 100, 1)}
                        {float_row(&send, guide, "kcfg_DitherThreshold",           "Settle threshold (px)", 0.1, 10.0, 0.1)}
                        {int_row  (&send, guide, "kcfg_DitherSettle",              "Settle time (s)",    0, 300, 1)}
                        {int_row  (&send, guide, "kcfg_DitherTimeout",             "Dither timeout (s)", 1, 600, 1)}
                        {int_row  (&send, guide, "kcfg_DitherMaxIterations",       "Max iterations",     1, 100, 1)}
                        {bool_row (&send, guide, "kcfg_DitherWithOnePulse",        "Dither with one pulse")}
                        {bool_row (&send, guide, "kcfg_DitherFailAbortsAutoGuide", "Failure aborts auto-guide")}
                        {bool_row (&send, guide, "kcfg_DitherNoGuiding",           "Dither without guiding")}
                        {int_row  (&send, guide, "kcfg_DitherNoGuidingPulse",      "No-guide dither pulse (ms)", 100, 10000, 100)}
                    </div>
                </details>

                // ── Algorithms (collapsible) ────────────────────────────
                <details style="border:1px solid #222;">
                    <summary style=legend_style()>"Guiding algorithms"</summary>
                    <div style="padding:10px 14px; display:flex; flex-direction:column; gap:8px;">
                        {select_row(&send, guide, "kcfg_GuideAlgorithm",           "Detection",       GUIDE_ALGO_OPTS)}
                        {select_row(&send, guide, "kcfg_RAGuidePulseAlgorithm",    "RA pulse algo",   PULSE_ALGO_OPTS)}
                        {select_row(&send, guide, "kcfg_DECGuidePulseAlgorithm",   "DEC pulse algo",  PULSE_ALGO_OPTS)}
                        {float_row (&send, guide, "kcfg_RAProportionalGain",       "RA Proportional gain", 0.0, 1.0, 0.01)}
                        {float_row (&send, guide, "kcfg_RAIntegralGain",           "RA Integral gain",     0.0, 1.0, 0.01)}
                        {float_row (&send, guide, "kcfg_RAMinimumPulseArcSec",     "RA min pulse (arcsec)", 0.0, 10.0, 0.01)}
                        {float_row (&send, guide, "kcfg_RAMaximumPulseArcSec",     "RA max pulse (arcsec)", 0.0, 30.0, 0.1)}
                        {float_row (&send, guide, "kcfg_RAHysteresis",             "RA hysteresis",    0.0, 1.0, 0.01)}
                        {float_row (&send, guide, "kcfg_DECProportionalGain",      "DEC Proportional gain", 0.0, 1.0, 0.01)}
                        {float_row (&send, guide, "kcfg_DECIntegralGain",          "DEC Integral gain",     0.0, 1.0, 0.01)}
                        {float_row (&send, guide, "kcfg_DECMinimumPulseArcSec",    "DEC min pulse (arcsec)", 0.0, 10.0, 0.01)}
                        {float_row (&send, guide, "kcfg_DECMaximumPulseArcSec",    "DEC max pulse (arcsec)", 0.0, 30.0, 0.1)}
                        {float_row (&send, guide, "kcfg_DECHysteresis",            "DEC hysteresis",   0.0, 1.0, 0.01)}
                        {float_row (&send, guide, "kcfg_GuideMaxDeltaRMS",         "Max ΔRMS (arcsec)",     0.0, 30.0, 0.1)}
                        {float_row (&send, guide, "kcfg_GuideMaxHFR",              "Max HFR",          0.0, 30.0, 0.1)}
                        {int_row   (&send, guide, "kcfg_GuideLostStarTimeout",     "Lost-star timeout (s)", 1, 600, 1)}
                        {int_row   (&send, guide, "kcfg_GuideCalibrationTimeout",  "Calibration timeout (s)", 1, 600, 1)}
                        {int_row   (&send, guide, "kcfg_MinDetectionsSEPMultistar","SEP min detections", 1, 200, 1)}
                        {int_row   (&send, guide, "kcfg_MaxMultistarReferenceStars","SEP max reference stars", 1, 200, 1)}
                    </div>
                </details>

                // ── Guider backend (default open) ───────────────────────
                <details open=true style="border:1px solid #222;">
                    <summary style=legend_style()>"Guider backend"</summary>
                    <div style="padding:10px 14px; display:flex; flex-direction:column; gap:10px;">
                        <div style="display:flex; gap:14px; flex-wrap:wrap;">
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="radio" name="guider-type"
                                       on:change=on_internal
                                       prop:checked=move || guider_type() == 0 />
                                "Internal"
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="radio" name="guider-type"
                                       on:change=on_phd2
                                       prop:checked=move || guider_type() == 1 />
                                "PHD2"
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="radio" name="guider-type"
                                       on:change=on_linguider
                                       prop:checked=move || guider_type() == 2 />
                                "LinGuider"
                            </label>
                        </div>

                        // PHD2 host/port — always shown (they remain editable
                        // even when Internal is active, matching KStars UI).
                        <div style="border-top:1px solid #1a1a20; padding-top:8px;">
                            <div style="font-size:11px; color:#88aaff; margin-bottom:6px;">"PHD2"</div>
                            {text_option_row(&send, guide, "PHD2Host", "Host")}
                            {int_option_row (&send, guide, "PHD2Port", "Port", 1, 65535)}
                        </div>

                        <div style="border-top:1px solid #1a1a20; padding-top:8px;">
                            <div style="font-size:11px; color:#88aaff; margin-bottom:6px;">"LinGuider"</div>
                            {text_option_row(&send, guide, "LinGuiderHost", "Host")}
                            {int_option_row (&send, guide, "LinGuiderPort", "Port", 1, 65535)}
                        </div>
                    </div>
                </details>

                // ── Advanced / GPG (collapsed by default) ───────────────
                <details style="border:1px solid #222;">
                    <summary style=legend_style()>"Advanced"</summary>
                    <div style="padding:10px 14px; display:flex; flex-direction:column; gap:8px;">
                        {bool_row (&send, guide, "kcfg_SaveGuideLog",              "Save guide log")}
                        {bool_row (&send, guide, "kcfg_UseGuideHead",              "Use guide head")}
                        {bool_row (&send, guide, "kcfg_AlwaysInventGuideStar",     "Always invent guide star")}
                        {bool_row (&send, guide, "latestCheck",                    "Show latest checks")}
                        {float_row(&send, guide, "guiderAccuracyThreshold",        "Accuracy threshold",   0.0, 10.0, 0.1)}

                        <div style="border-top:1px solid #1a1a20; padding-top:8px; margin-top:6px;">
                            <div style="font-size:11px; color:#88aaff; margin-bottom:6px;">"GPG guider"</div>
                            {int_row  (&send, guide, "kcfg_GPGPeriod",                    "Period (s)",              1, 3600, 1)}
                            {bool_row (&send, guide, "kcfg_GPGEstimatePeriod",            "Estimate period")}
                            {bool_row (&send, guide, "kcfg_GPGDarkGuiding",               "Dark guiding")}
                            {int_row  (&send, guide, "kcfg_GPGDarkGuidingInterval",       "Dark-guiding interval",   1, 600, 1)}
                            {float_row(&send, guide, "kcfg_GPGpWeight",                   "Prediction weight",       0.0, 1.0, 0.01)}
                            {float_row(&send, guide, "kcfg_GPGSE0KLengthScale",           "SE0 length scale",        0.0, 1000.0, 1.0)}
                            {float_row(&send, guide, "kcfg_GPGSE0KSignalVariance",        "SE0 signal variance",     0.0, 10.0, 0.01)}
                            {float_row(&send, guide, "kcfg_GPGPKLengthScale",             "PK length scale",         0.0, 1000.0, 1.0)}
                            {float_row(&send, guide, "kcfg_GPGPKSignalVariance",          "PK signal variance",      0.0, 10.0, 0.01)}
                            {float_row(&send, guide, "kcfg_GPGSE1KLengthScale",           "SE1 length scale",        0.0, 1000.0, 1.0)}
                            {float_row(&send, guide, "kcfg_GPGSE1KSignalVariance",        "SE1 signal variance",     0.0, 10.0, 0.01)}
                            {int_row  (&send, guide, "kcfg_GPGPointsForApproximation",    "Points for approximation", 1, 10000, 10)}
                            {int_row  (&send, guide, "kcfg_GPGMinPeriodsForInference",    "Min periods for inference", 1, 100, 1)}
                            {int_row  (&send, guide, "kcfg_GPGMinPeriodsForPeriodEstimate","Min periods for period estimate", 1, 100, 1)}
                        </div>
                    </div>
                </details>
            </div>
        </div>
    }
}
