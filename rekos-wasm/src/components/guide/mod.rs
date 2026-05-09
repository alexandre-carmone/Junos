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
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::compat::GuideSnapshot;
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;
use crate::ws_helpers::{dispatch_setting as ws_dispatch_setting, send_cmd};

type LabelFn = fn(&Translations) -> &'static str;

// ── Shared Tailwind class fragments ───────────────────────────────────────────
const GUIDE_INPUT: &str = "input input--sm flex-1 min-w-0 font-mono";
const GUIDE_FIELD_LABEL: &str =
    "basis-[clamp(100px,25%,200px)] grow-0 shrink-0 text-text-blue text-sm";
const GUIDE_SECTION: &str = "fieldset m-0";
const GUIDE_DETAILS: &str = "fieldset !p-0";
const GUIDE_LEGEND: &str = "fieldset__legend cursor-pointer";
const GUIDE_DETAILS_BODY: &str = "py-sp-3 px-sp-4 flex flex-col gap-sp-2";
const GUIDE_BTN_BASE: &str = "btn";

mod timeline;
use timeline::drift_plot;

// ---------------------------------------------------------------------------
// Combo option lists (sourced from kstars/ekos/guide/*.ui)
// ---------------------------------------------------------------------------

const BINNING_OPTIONS: &[&str] = &["1x1", "2x2", "3x3", "4x4"];
const SQUARE_OPTIONS: &[&str] = &["8", "16", "32", "64", "128"];
const PULSE_ALGO_OPTS: &[&str] = &["Standard", "Hysteresis", "Linear", "GPG"];
const GUIDE_ALGO_OPTS: &[&str] = &[
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

// ---------------------------------------------------------------------------
// Style constants
// ---------------------------------------------------------------------------

fn stage_color(status: &str) -> &'static str {
    match status {
        "" | "Idle" | "Aborted" | "Disconnected" => "var(--text-muted)",
        "Calibrating" | "Selecting star" | "Looping" | "Capturing" | "Subtracting"
        | "Subframing" | "Reacquiring" => "var(--state-warn)",
        "Calibrated" | "Connected" => "var(--state-info)",
        "Guiding" => "var(--state-ok)",
        "Dithering" | "Dithering successful" | "Manual Dithering" | "Settling" => {
            "var(--accent-cyan-dim)"
        }
        "Calibration error" | "Dithering error" | "Suspended" => "var(--state-err)",
        _ => "var(--text)",
    }
}

// ---------------------------------------------------------------------------
// Command dispatchers
// ---------------------------------------------------------------------------

/// `guide_set_all_settings` payload is the widget map directly at payload
/// root — see message.cpp:673 (`auto settings = payload.toVariantMap()`).
/// This differs from `align_set_all_settings` which expects `{settings:{...}}`.
fn dispatch_guide_setting(send: &SendCmd, key: &str, value: serde_json::Value) {
    ws_dispatch_setting(send, "guide_set_all_settings", None, key, value);
}

/// Set one KStars `Options::` entry (GuiderType, PHD2Host, etc), then
/// immediately re-read the 5 guide-relevant options so the UI reflects
/// the confirmed value (KStars does not echo option_set back).
fn dispatch_option(send: &SendCmd, name: &str, value: serde_json::Value) {
    let set = serde_json::json!({
        "type": "option_set",
        "payload": { "options": [ { "name": name, "value": value } ] }
    })
    .to_string();
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
        "" | "Idle" | "Aborted" | "Connected" | "Calibrated" | "Calibration error" | "Disconnected"
    )
}

fn can_start(status: &str) -> bool {
    is_idle(status)
}

fn can_stop(status: &str) -> bool {
    !matches!(status, "" | "Idle" | "Aborted" | "Disconnected")
}

fn can_capture_or_loop(status: &str, is_internal: bool) -> bool {
    is_internal && is_idle(status)
}

fn can_clear(status: &str) -> bool {
    !matches!(status, "Calibrating" | "Guiding" | "Dithering")
}

fn guider_type_label(v: i64, tr: &Translations) -> &'static str {
    match v {
        1 => tr.guide_phd2_label,
        2 => tr.guide_linguider_label,
        _ => tr.guide_internal,
    }
}

// ---------------------------------------------------------------------------
// Row-rendering helpers
// ---------------------------------------------------------------------------

/// Checkbox row bound to `guide.settings[key]`.
fn bool_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    lang: RwSignal<Lang>,
    key: &'static str,
    label: LabelFn,
) -> impl IntoView + use<> {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        dispatch_guide_setting(&s, key, serde_json::Value::Bool(event_target_checked(&ev)));
    };
    view! {
        <div class="flex items-center gap-sp-2">
            <span class=GUIDE_FIELD_LABEL>{move || label(t(lang.get()))}</span>
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
    lang: RwSignal<Lang>,
    key: &'static str,
    label: LabelFn,
    min: i64,
    max: i64,
    step: i64,
) -> impl IntoView + use<> {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        let raw = event_target_value(&ev);
        if let Ok(n) = raw.parse::<i64>() {
            let n = n.clamp(min, max);
            dispatch_guide_setting(&s, key, serde_json::Value::Number(n.into()));
        }
    };
    view! {
        <div class="flex items-center gap-sp-2">
            <span class=GUIDE_FIELD_LABEL>{move || label(t(lang.get()))}</span>
            <input type="number"
                   min=min.to_string()
                   max=max.to_string()
                   step=step.to_string()
                   on:change=on_change
                   prop:value=move || guide.with(|g|
                       settings_i64(&g.settings, key).map(|v| v.to_string())
                           .unwrap_or_default())
                   class=GUIDE_INPUT />
        </div>
    }
}

/// Double spinbox row (QDoubleSpinBox-equivalent).
fn float_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    lang: RwSignal<Lang>,
    key: &'static str,
    label: LabelFn,
    min: f64,
    max: f64,
    step: f64,
) -> impl IntoView + use<> {
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
        <div class="flex items-center gap-sp-2">
            <span class=GUIDE_FIELD_LABEL>{move || label(t(lang.get()))}</span>
            <input type="number"
                   min=min.to_string()
                   max=max.to_string()
                   step=step.to_string()
                   on:change=on_change
                   prop:value=move || guide.with(|g|
                       settings_f64(&g.settings, key).map(|v| format!("{v}"))
                           .unwrap_or_default())
                   class=GUIDE_INPUT />
        </div>
    }
}

/// Combo-box row bound to a widget's `currentText`. Because KStars exposes
/// combos as strings (see Guide::getAllSettings), we send a string back.
fn select_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    lang: RwSignal<Lang>,
    key: &'static str,
    label: LabelFn,
    options: &'static [&'static str],
) -> impl IntoView + use<> {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        dispatch_guide_setting(&s, key, serde_json::Value::String(event_target_select(&ev)));
    };
    view! {
        <div class="flex items-center gap-sp-2">
            <span class=GUIDE_FIELD_LABEL>{move || label(t(lang.get()))}</span>
            <select on:change=on_change
                    class=GUIDE_INPUT
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
    lang: RwSignal<Lang>,
    option: &'static str,
    label: LabelFn,
) -> impl IntoView + use<> {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        dispatch_option(
            &s,
            option,
            serde_json::Value::String(event_target_value(&ev)),
        );
    };
    view! {
        <div class="flex items-center gap-sp-2">
            <span class=GUIDE_FIELD_LABEL>{move || label(t(lang.get()))}</span>
            <input type="text"
                   on:change=on_change
                   prop:value=move || guide.with(|g|
                       settings_str(&g.options, option).unwrap_or_default())
                   class=GUIDE_INPUT />
        </div>
    }
}

/// Integer input bound to a global `Options::` entry (e.g. PHD2Port).
fn int_option_row(
    send: &SendCmd,
    guide: Signal<GuideSnapshot>,
    lang: RwSignal<Lang>,
    option: &'static str,
    label: LabelFn,
    min: i64,
    max: i64,
) -> impl IntoView + use<> {
    let s = send.clone();
    let on_change = move |ev: web_sys::Event| {
        let raw = event_target_value(&ev);
        if let Ok(n) = raw.parse::<i64>() {
            let n = n.clamp(min, max);
            dispatch_option(&s, option, serde_json::Value::Number(n.into()));
        }
    };
    view! {
        <div class="flex items-center gap-sp-2">
            <span class=GUIDE_FIELD_LABEL>{move || label(t(lang.get()))}</span>
            <input type="number"
                   min=min.to_string()
                   max=max.to_string()
                   step="1"
                   on:change=on_change
                   prop:value=move || guide.with(|g|
                       settings_i64(&g.options, option).map(|v| v.to_string())
                           .unwrap_or_default())
                   class=GUIDE_INPUT />
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
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

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
    let on_internal = move |_| {
        dispatch_option(
            &s_internal,
            "GuiderType",
            serde_json::Value::Number(0.into()),
        )
    };
    let s_phd2 = send.clone();
    let on_phd2 =
        move |_| dispatch_option(&s_phd2, "GuiderType", serde_json::Value::Number(1.into()));
    let s_linguider = send.clone();
    let on_linguider = move |_| {
        dispatch_option(
            &s_linguider,
            "GuiderType",
            serde_json::Value::Number(2.into()),
        )
    };

    // Derive guider type from options map; default 0 (Internal).
    let guider_type = move || guide.with(|g| settings_i64(&g.options, "GuiderType").unwrap_or(0));
    let is_internal = move || guider_type() == 0;

    // Settings overlay open/closed.
    let settings_open = RwSignal::new(false);

    // Escape closes the overlay. We forget() the closure (one persistent
    // listener per GuideTab mount); calls into a disposed RwSignal are a
    // no-op in leptos 0.7, so leftover listeners after a tab switch are
    // harmless. on_cleanup is unusable here because Closure<dyn FnMut(_)>
    // is !Send.
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

    // Button gating closures — refresh on each render via guide signal.
    let status = move || guide.with(|g| g.status.clone());
    let btn_start = move || can_start(&status());
    let btn_stop = move || can_stop(&status());
    let btn_capture = move || can_capture_or_loop(&status(), is_internal());
    let btn_loop = move || can_capture_or_loop(&status(), is_internal());
    let btn_clear = move || can_clear(&status());

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono grid grid-rows-[auto_1fr] overflow-hidden">

            // ── Header ───────────────────────────────────────────────────
            <div class="flex items-center gap-y-sp-2 gap-x-sp-4 flex-wrap min-h-[48px] py-sp-2 pr-5 pl-20 border-b border-border-base bg-[rgba(6,6,15,0.85)] text-md max-[759px]:py-sp-2 max-[759px]:pl-3 max-[759px]:pr-3 max-[759px]:gap-y-[6px] max-[759px]:gap-x-3 max-[759px]:text-sm">
                <span class="inline-block py-1 px-sp-3 rounded-[14px] text-sm border border-current"
                      style=move || format!("color:{}", stage_color(&status()))>
                    {move || {
                        let s = status();
                        if s.is_empty() { tr().idle.to_string() } else { s }
                    }}
                </span>
                <span class="text-text-blue max-[479px]:hidden">{move || tr().guide_guider_label}</span>
                <span>{move || guider_type_label(guider_type(), t(lang.get()))}</span>
                <span class="text-text-blue ml-sp-2 max-[479px]:hidden">{move || tr().guide_rms}</span>
                <span class="text-sm">
                    {move || {
                        let tr_ = tr();
                        guide.with(|g| {
                            let ra = g.ra_rms.map(|v| format!("{v:.2}\"")).unwrap_or_else(|| "—".into());
                            let de = g.de_rms.map(|v| format!("{v:.2}\"")).unwrap_or_else(|| "—".into());
                            format!("{} {ra}  DEC {de}", tr_.ra_label)
                        })
                    }}
                </span>
                <span class="flex-1 max-[759px]:hidden"></span>
                <span class="text-text-blue">{move || tr().guide_connected}</span>
                <span>{move || if guide.with(|g| g.connected) { tr().yes } else { tr().no }}</span>
            </div>

            // ── Body ─────────────────────────────────────────────────────
            <div class="overflow-y-auto py-4 px-5 flex flex-col gap-[14px]">

                // Preview frame (uuid "+G*" from kstars media.cpp:753)
                <Show when=move || guide.with(|g| g.preview_url.is_some())>
                    <div class="flex justify-center items-center bg-bg-input-deep border border-border-base p-sp-2 min-h-[180px] max-h-[400px] max-[759px]:min-h-0 max-[759px]:max-h-[40vh]">
                        <img
                            src=move || guide.with(|g|
                                g.preview_url.clone().unwrap_or_default())
                            alt="guide frame"
                            class="max-w-full max-h-[384px] object-contain block [image-rendering:pixelated] max-[759px]:max-h-[40vh]"
                        />
                    </div>
                </Show>

                // Drift plot + state ribbon. Drift samples come from
                // kstars/ekos/manager.cpp:2772-2776 via partial
                // `new_guide_state {drift_ra, drift_de}` events.
                {move || guide.with(|g| drift_plot(&g.drift, &g.history))}

                // ── Action row ──────────────────────────────────────────
                <fieldset class=GUIDE_SECTION>
                    <legend class=GUIDE_LEGEND>{move || tr().guide_actions}</legend>
                    <div class="flex flex-wrap gap-sp-2">
                        <button
                            on:click=on_start.clone()
                            disabled=move || !btn_start()
                            class="btn btn-primary">
                            {move || tr().guide_start}
                        </button>
                        <button
                            on:click=on_stop.clone()
                            disabled=move || !btn_stop()
                            class="btn btn-danger">
                            {move || tr().stop}
                        </button>
                        <button
                            on:click=on_capture.clone()
                            disabled=move || !btn_capture()
                            class="btn btn-ghost text-text-blue !border-text-blue">
                            {move || tr().guide_capture}
                        </button>
                        <button
                            on:click=on_loop.clone()
                            disabled=move || !btn_loop()
                            class="btn btn-ghost text-text-blue !border-text-blue">
                            {move || tr().guide_loop}
                        </button>
                        <button
                            on:click=on_clear.clone()
                            disabled=move || !btn_clear()
                            class="btn btn-ghost text-accent-amber !border-accent-amber">
                            {move || tr().guide_clear_cal}
                        </button>
                        <button
                            class="btn btn-ghost ml-auto"
                            on:click=move |_| settings_open.set(true)>
                            {move || tr().guide_settings_button}
                        </button>
                    </div>
                    <div class="mt-[6px] text-xs text-[#667]">
                        {move || tr().guide_capture_loop_note}
                    </div>
                </fieldset>

                // ── Settings overlay ────────────────────────────────────
                // All parameter sections live inside a full-screen modal
                // (toggled by the Settings button on the action row). The
                // main view stays focused on the live drift plot + preview
                // + Start/Stop/Capture/Loop/Clear.
                <Show when=move || settings_open.get()>
                    <div
                        class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2"
                        on:click=move |_| settings_open.set(false)>
                        <div
                            class="w-full max-w-[980px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col"
                            on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                            <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                                <h2 class="text-text-blue text-sm uppercase tracking-[0.08em] m-0">
                                    {move || tr().guide_settings_title}
                                </h2>
                                <button
                                    class="btn btn-ghost"
                                    on:click=move |_| settings_open.set(false)>
                                    {move || tr().imaging_close}
                                </button>
                            </div>
                            <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 flex flex-col gap-[14px]">

                // ── Essentials ──────────────────────────────────────────
                <fieldset class=GUIDE_SECTION>
                    <legend class=GUIDE_LEGEND>{move || tr().guide_essentials}</legend>
                    <div class="flex flex-col gap-sp-2">
                        {float_row (&send, guide, lang, "guideExposure",    |t| t.guide_f_exposure,     0.1, 60.0, 0.1)}
                        {float_row (&send, guide, lang, "guideDelay",       |t| t.guide_f_delay,        0.0, 60.0, 0.1)}
                        {float_row (&send, guide, lang, "guideGain",        |t| t.guide_f_gain,         0.0, 1000.0, 1.0)}
                        {select_row(&send, guide, lang, "guideBinning",     |t| t.guide_f_binning,      BINNING_OPTIONS)}
                        {select_row(&send, guide, lang, "guideSquareSize",  |t| t.guide_f_tracking_box, SQUARE_OPTIONS)}
                        {bool_row  (&send, guide, lang, "guideDarkFrame",   |t| t.guide_f_dark_frame)}
                        {bool_row  (&send, guide, lang, "guideSubframe",    |t| t.guide_f_subframe)}
                        {bool_row  (&send, guide, lang, "guideAutoStar",    |t| t.guide_f_auto_star)}
                        {bool_row  (&send, guide, lang, "guideStreamingEnabled", |t| t.guide_f_stream)}
                    </div>
                </fieldset>

                // ── RA/DEC enable ───────────────────────────────────────
                <fieldset class=GUIDE_SECTION>
                    <legend class=GUIDE_LEGEND>{move || tr().guide_ra_dec_corrections}</legend>
                    <div class="flex flex-col gap-sp-2">
                        {bool_row(&send, guide, lang, "rAGuideEnabled",      |t| t.guide_f_ra_guiding)}
                        {bool_row(&send, guide, lang, "eastRAGuideEnabled",  |t| t.guide_f_east_pulses)}
                        {bool_row(&send, guide, lang, "westRAGuideEnabled",  |t| t.guide_f_west_pulses)}
                        {bool_row(&send, guide, lang, "dECGuideEnabled",     |t| t.guide_f_dec_guiding)}
                        {bool_row(&send, guide, lang, "northDECGuideEnabled",|t| t.guide_f_north_pulses)}
                        {bool_row(&send, guide, lang, "southDECGuideEnabled",|t| t.guide_f_south_pulses)}
                    </div>
                </fieldset>

                // ── Calibration (collapsible) ───────────────────────────
                <details class=GUIDE_DETAILS>
                    <summary class=GUIDE_LEGEND>{move || tr().guide_calibration}</summary>
                    <div class=GUIDE_DETAILS_BODY>
                        {int_row  (&send, guide, lang, "kcfg_AutoModeIterations",         |t| t.guide_f_iterations,          1, 100, 1)}
                        {int_row  (&send, guide, lang, "kcfg_CalibrationPulseDuration",   |t| t.guide_f_pulse_duration,      100, 10000, 100)}
                        {int_row  (&send, guide, lang, "kcfg_CalibrationMaxMove",         |t| t.guide_f_max_move,            1, 200, 1)}
                        {bool_row (&send, guide, lang, "kcfg_TwoAxisEnabled",             |t| t.guide_f_two_axis)}
                        {bool_row (&send, guide, lang, "kcfg_GuideAutoSquareSizeEnabled", |t| t.guide_f_auto_box_size)}
                        {bool_row (&send, guide, lang, "kcfg_GuideCalibrationBacklash",   |t| t.guide_f_dec_backlash)}
                        {bool_row (&send, guide, lang, "kcfg_ResetGuideCalibration",      |t| t.guide_f_reset_each_start)}
                        {bool_row (&send, guide, lang, "kcfg_ReuseGuideCalibration",      |t| t.guide_f_reuse_cal)}
                        {bool_row (&send, guide, lang, "kcfg_ReverseDecOnPierSideChange", |t| t.guide_f_reverse_dec_flip)}
                    </div>
                </details>

                // ── Dither (collapsible) ────────────────────────────────
                <details class=GUIDE_DETAILS>
                    <summary class=GUIDE_LEGEND>{move || tr().guide_dither}</summary>
                    <div class=GUIDE_DETAILS_BODY>
                        <div class="text-xs text-[#667] mb-1">
                            {move || tr().guide_dither_note}
                        </div>
                        {bool_row (&send, guide, lang, "kcfg_DitherEnabled",             |t| t.guide_f_dither_enabled)}
                        {float_row(&send, guide, lang, "kcfg_DitherPixels",              |t| t.guide_f_dither_amount,         0.1, 30.0, 0.1)}
                        {int_row  (&send, guide, lang, "kcfg_DitherFrames",              |t| t.guide_f_dither_frames,         1, 100, 1)}
                        {float_row(&send, guide, lang, "kcfg_DitherThreshold",           |t| t.guide_f_dither_settle_thr,     0.1, 10.0, 0.1)}
                        {int_row  (&send, guide, lang, "kcfg_DitherSettle",              |t| t.guide_f_dither_settle_t,       0, 300, 1)}
                        {int_row  (&send, guide, lang, "kcfg_DitherTimeout",             |t| t.guide_f_dither_timeout,        1, 600, 1)}
                        {int_row  (&send, guide, lang, "kcfg_DitherMaxIterations",       |t| t.guide_f_dither_max_iter,       1, 100, 1)}
                        {bool_row (&send, guide, lang, "kcfg_DitherWithOnePulse",        |t| t.guide_f_dither_one_pulse)}
                        {bool_row (&send, guide, lang, "kcfg_DitherFailAbortsAutoGuide", |t| t.guide_f_dither_fail_abort)}
                        {bool_row (&send, guide, lang, "kcfg_DitherNoGuiding",           |t| t.guide_f_dither_no_guiding)}
                        {int_row  (&send, guide, lang, "kcfg_DitherNoGuidingPulse",      |t| t.guide_f_dither_no_guide_pulse, 100, 10000, 100)}
                    </div>
                </details>

                // ── Algorithms (collapsible) ────────────────────────────
                <details class=GUIDE_DETAILS>
                    <summary class=GUIDE_LEGEND>{move || tr().guide_algorithms}</summary>
                    <div class=GUIDE_DETAILS_BODY>
                        {select_row(&send, guide, lang, "kcfg_GuideAlgorithm",           |t| t.guide_f_detection,      GUIDE_ALGO_OPTS)}
                        {select_row(&send, guide, lang, "kcfg_RAGuidePulseAlgorithm",    |t| t.guide_f_ra_pulse_algo,  PULSE_ALGO_OPTS)}
                        {select_row(&send, guide, lang, "kcfg_DECGuidePulseAlgorithm",   |t| t.guide_f_dec_pulse_algo, PULSE_ALGO_OPTS)}
                        {float_row (&send, guide, lang, "kcfg_RAProportionalGain",       |t| t.guide_f_ra_kp,          0.0, 1.0, 0.01)}
                        {float_row (&send, guide, lang, "kcfg_RAIntegralGain",           |t| t.guide_f_ra_ki,          0.0, 1.0, 0.01)}
                        {float_row (&send, guide, lang, "kcfg_RAMinimumPulseArcSec",     |t| t.guide_f_ra_min_pulse,   0.0, 10.0, 0.01)}
                        {float_row (&send, guide, lang, "kcfg_RAMaximumPulseArcSec",     |t| t.guide_f_ra_max_pulse,   0.0, 30.0, 0.1)}
                        {float_row (&send, guide, lang, "kcfg_RAHysteresis",             |t| t.guide_f_ra_hysteresis,  0.0, 1.0, 0.01)}
                        {float_row (&send, guide, lang, "kcfg_DECProportionalGain",      |t| t.guide_f_dec_kp,         0.0, 1.0, 0.01)}
                        {float_row (&send, guide, lang, "kcfg_DECIntegralGain",          |t| t.guide_f_dec_ki,         0.0, 1.0, 0.01)}
                        {float_row (&send, guide, lang, "kcfg_DECMinimumPulseArcSec",    |t| t.guide_f_dec_min_pulse,  0.0, 10.0, 0.01)}
                        {float_row (&send, guide, lang, "kcfg_DECMaximumPulseArcSec",    |t| t.guide_f_dec_max_pulse,  0.0, 30.0, 0.1)}
                        {float_row (&send, guide, lang, "kcfg_DECHysteresis",            |t| t.guide_f_dec_hysteresis, 0.0, 1.0, 0.01)}
                        {float_row (&send, guide, lang, "kcfg_GuideMaxDeltaRMS",         |t| t.guide_f_max_drms,       0.0, 30.0, 0.1)}
                        {float_row (&send, guide, lang, "kcfg_GuideMaxHFR",              |t| t.guide_f_max_hfr,        0.0, 30.0, 0.1)}
                        {int_row   (&send, guide, lang, "kcfg_GuideLostStarTimeout",     |t| t.guide_f_lost_star_to,   1, 600, 1)}
                        {int_row   (&send, guide, lang, "kcfg_GuideCalibrationTimeout",  |t| t.guide_f_cal_timeout,    1, 600, 1)}
                        {int_row   (&send, guide, lang, "kcfg_MinDetectionsSEPMultistar",|t| t.guide_f_sep_min,        1, 200, 1)}
                        {int_row   (&send, guide, lang, "kcfg_MaxMultistarReferenceStars",|t| t.guide_f_sep_max_ref,   1, 200, 1)}
                    </div>
                </details>

                // ── Guider backend (default open) ───────────────────────
                <details open=true class=GUIDE_DETAILS>
                    <summary class=GUIDE_LEGEND>{move || tr().guide_backend}</summary>
                    <div class=GUIDE_DETAILS_BODY>
                        <div class="flex gap-sp-4 flex-wrap">
                            <label class="flex items-center gap-[6px] cursor-pointer">
                                <input type="radio" name="guider-type"
                                       on:change=on_internal.clone()
                                       prop:checked=move || guider_type() == 0 />
                                {move || tr().guide_internal}
                            </label>
                            <label class="flex items-center gap-[6px] cursor-pointer">
                                <input type="radio" name="guider-type"
                                       on:change=on_phd2.clone()
                                       prop:checked=move || guider_type() == 1 />
                                {move || tr().guide_phd2_label}
                            </label>
                            <label class="flex items-center gap-[6px] cursor-pointer">
                                <input type="radio" name="guider-type"
                                       on:change=on_linguider.clone()
                                       prop:checked=move || guider_type() == 2 />
                                {move || tr().guide_linguider_label}
                            </label>
                        </div>

                        // PHD2 host/port — always shown (they remain editable
                        // even when Internal is active, matching KStars UI).
                        <div class="border-t border-border-strong pt-sp-2">
                            <div class="text-sm text-text-blue mb-[6px]">{move || tr().guide_phd2}</div>
                            {text_option_row(&send, guide, lang, "PHD2Host", |t| t.guide_host)}
                            {int_option_row (&send, guide, lang, "PHD2Port", |t| t.guide_port, 1, 65535)}
                        </div>

                        <div class="border-t border-border-strong pt-sp-2">
                            <div class="text-sm text-text-blue mb-[6px]">{move || tr().guide_linguider}</div>
                            {text_option_row(&send, guide, lang, "LinGuiderHost", |t| t.guide_host)}
                            {int_option_row (&send, guide, lang, "LinGuiderPort", |t| t.guide_port, 1, 65535)}
                        </div>
                    </div>
                </details>

                // ── Advanced / GPG (collapsed by default) ───────────────
                <details class=GUIDE_DETAILS>
                    <summary class=GUIDE_LEGEND>{move || tr().guide_advanced}</summary>
                    <div class=GUIDE_DETAILS_BODY>
                        {bool_row (&send, guide, lang, "kcfg_SaveGuideLog",              |t| t.guide_f_save_log)}
                        {bool_row (&send, guide, lang, "kcfg_UseGuideHead",              |t| t.guide_f_use_guide_head)}
                        {bool_row (&send, guide, lang, "kcfg_AlwaysInventGuideStar",     |t| t.guide_f_invent_star)}
                        {bool_row (&send, guide, lang, "latestCheck",                    |t| t.guide_f_latest_checks)}
                        {float_row(&send, guide, lang, "guiderAccuracyThreshold",        |t| t.guide_f_accuracy_thr,     0.0, 10.0, 0.1)}

                        <div class="border-t border-border-strong pt-sp-2 mt-[6px]">
                            <div class="text-sm text-text-blue mb-[6px]">{move || tr().guide_gpg}</div>
                            {int_row  (&send, guide, lang, "kcfg_GPGPeriod",                    |t| t.guide_f_gpg_period,             1, 3600, 1)}
                            {bool_row (&send, guide, lang, "kcfg_GPGEstimatePeriod",            |t| t.guide_f_gpg_estimate_period)}
                            {bool_row (&send, guide, lang, "kcfg_GPGDarkGuiding",               |t| t.guide_f_gpg_dark)}
                            {int_row  (&send, guide, lang, "kcfg_GPGDarkGuidingInterval",       |t| t.guide_f_gpg_dark_interval,      1, 600, 1)}
                            {float_row(&send, guide, lang, "kcfg_GPGpWeight",                   |t| t.guide_f_gpg_p_weight,           0.0, 1.0, 0.01)}
                            {float_row(&send, guide, lang, "kcfg_GPGSE0KLengthScale",           |t| t.guide_f_gpg_se0_length,         0.0, 1000.0, 1.0)}
                            {float_row(&send, guide, lang, "kcfg_GPGSE0KSignalVariance",        |t| t.guide_f_gpg_se0_signal,         0.0, 10.0, 0.01)}
                            {float_row(&send, guide, lang, "kcfg_GPGPKLengthScale",             |t| t.guide_f_gpg_pk_length,          0.0, 1000.0, 1.0)}
                            {float_row(&send, guide, lang, "kcfg_GPGPKSignalVariance",          |t| t.guide_f_gpg_pk_signal,          0.0, 10.0, 0.01)}
                            {float_row(&send, guide, lang, "kcfg_GPGSE1KLengthScale",           |t| t.guide_f_gpg_se1_length,         0.0, 1000.0, 1.0)}
                            {float_row(&send, guide, lang, "kcfg_GPGSE1KSignalVariance",        |t| t.guide_f_gpg_se1_signal,         0.0, 10.0, 0.01)}
                            {int_row  (&send, guide, lang, "kcfg_GPGPointsForApproximation",    |t| t.guide_f_gpg_points_approx,      1, 10000, 10)}
                            {int_row  (&send, guide, lang, "kcfg_GPGMinPeriodsForInference",    |t| t.guide_f_gpg_min_periods_inf,    1, 100, 1)}
                            {int_row  (&send, guide, lang, "kcfg_GPGMinPeriodsForPeriodEstimate",|t| t.guide_f_gpg_min_periods_period, 1, 100, 1)}
                        </div>
                    </div>
                </details>

                            </div>
                        </div>
                    </div>
                </Show>
            </div>
        </div>
    }
}
