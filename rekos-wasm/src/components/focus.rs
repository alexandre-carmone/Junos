//! Focus module UI — full-screen tab.
//!
//! Talks to KStars via Ekos Live:
//!   - Inbound: `new_focus_state` (status/hfr/pos/log), `focus_get_all_settings`
//!     (debounced settings snapshot), `new_preview_image` with `uuid: "+F"`
//!     (focus frames). See `ws.rs::apply_ekos_event` for the match arms.
//!   - Outbound: `focus_start`, `focus_stop`, `focus_capture`, `focus_loop`,
//!     `focus_reset`, `focus_in{steps}`, `focus_out{steps}`,
//!     `focus_set_all_settings{…}`, `focus_set_crosshair{x,y}`.
//!     Command list: `kstars/kstars/ekos/ekoslive/commands.h`,
//!     handlers: `kstars/kstars/ekos/ekoslive/message.cpp:709-739`.

use leptos::prelude::*;
use leptos::html;
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{HtmlCanvasElement, CanvasRenderingContext2d, MouseEvent};

use crate::compat::{CameraSnapshot, FocusSnapshot};
use crate::i18n::{Lang, t};
use crate::ws::SendCmd;
use crate::ws_helpers::{send_cmd, dispatch_setting as ws_dispatch_setting};

fn status_color(status: &str) -> &'static str {
    let s = status.to_lowercase();
    if s.contains("fail") || s.contains("abort") { "#ff6a6a" }
    else if s.contains("complete")                { "#7affa0" }
    else if s.contains("progress")                { "#88aaff" }
    else if s.contains("framing")                 { "#ffd060" }
    else if s.contains("changing")                { "#ffd060" }
    else if s.contains("user input")              { "#ffd060" }
    else                                           { "#808090" }
}

/// Subset of `focus_get_all_settings` keys this first cut knows how to render.
/// Unknown keys are ignored (no generic fallback in v1, per plan).
const KNOWN_SETTING_KEYS: &[&str] = &[
    "focusExposure",
    "focusBinning",
    "focusGain",
    "focusISO",
    "focusIterations",
    "focusStepSize",
    "focusMaxStep",
    "focusMaxTravel",
    "focusTolerance",
    "focusBacklash",
    "focusAlgorithm",
    "focusAutoStarEnabled",
    "focusSuspendGuiding",
    "focusUseFullField",
];

#[component]
pub fn FocusTab(
    #[prop(into)] focus:  Signal<FocusSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] send:   SendCmd,
) -> impl IntoView {
    let _ = camera; // Reserved for future use (CCD_INFO-driven frame sizing).

    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let step_size = RwSignal::new(100_i64);

    // ── Action dispatchers ────────────────────────────────────────────────
    let send1 = send.clone();
    let on_start = move |_| send_cmd(&send1, "focus_start", serde_json::json!({}));
    let send2 = send.clone();
    let on_stop = move |_| send_cmd(&send2, "focus_stop", serde_json::json!({}));
    let send3 = send.clone();
    let on_capture = move |_| send_cmd(&send3, "focus_capture", serde_json::json!({}));
    let send4 = send.clone();
    let on_loop = move |_| send_cmd(&send4, "focus_loop", serde_json::json!({}));
    let send5 = send.clone();
    let on_reset = move |_| send_cmd(&send5, "focus_reset", serde_json::json!({}));

    let send_in = send.clone();
    let on_in = move |_| {
        send_cmd(&send_in, "focus_in", serde_json::json!({ "steps": step_size.get() }));
    };
    let send_out = send.clone();
    let on_out = move |_| {
        send_cmd(&send_out, "focus_out", serde_json::json!({ "steps": step_size.get() }));
    };

    // ── Preview click → focus_set_crosshair ───────────────────────────────
    let send_xh = send.clone();
    let on_preview_click = move |ev: MouseEvent| {
        let target = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok());
        let Some(el) = target else { return };
        let rect = el.get_bounding_client_rect();
        let w = rect.width();
        let h = rect.height();
        if w <= 0.0 || h <= 0.0 { return; }
        let x = (ev.client_x() as f64 - rect.left()) / w;
        let y = (ev.client_y() as f64 - rect.top())  / h;
        let x = x.clamp(0.0, 1.0);
        let y = y.clamp(0.0, 1.0);
        send_cmd(&send_xh, "focus_set_crosshair", serde_json::json!({ "x": x, "y": y }));
    };

    // ── HFR history mini-plot ─────────────────────────────────────────────
    let canvas_ref = NodeRef::<html::Canvas>::new();
    Effect::new(move |_| {
        let history = focus.with(|f| f.history.clone());
        let Some(canvas) = canvas_ref.get() else { return };
        let canvas: HtmlCanvasElement = canvas.unchecked_into();
        let w = canvas.width() as f64;
        let h = canvas.height() as f64;
        let Ok(Some(ctx)) = canvas.get_context("2d") else { return };
        let ctx: CanvasRenderingContext2d = ctx.unchecked_into();
        ctx.set_fill_style_str("#0a0a0f");
        ctx.fill_rect(0.0, 0.0, w, h);
        ctx.set_stroke_style_str("#222");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(0.0, h - 0.5);
        ctx.line_to(w, h - 0.5);
        let _ = ctx.stroke();
        if history.len() < 2 { return; }
        let min_hfr = history.iter().map(|s| s.hfr).fold(f64::INFINITY, f64::min);
        let max_hfr = history.iter().map(|s| s.hfr).fold(f64::NEG_INFINITY, f64::max);
        let span = (max_hfr - min_hfr).max(0.1);
        let pad = 6.0;
        ctx.set_stroke_style_str("#88aaff");
        ctx.set_line_width(1.5);
        ctx.begin_path();
        for (i, s) in history.iter().enumerate() {
            let x = pad + (i as f64) * (w - 2.0 * pad) / ((history.len() - 1) as f64);
            let y = pad + (1.0 - (s.hfr - min_hfr) / span) * (h - 2.0 * pad);
            if i == 0 { ctx.move_to(x, y); } else { ctx.line_to(x, y); }
        }
        let _ = ctx.stroke();
        ctx.set_fill_style_str("#cfe0ff");
        let label = format!("HFR  min {:.2}   max {:.2}", min_hfr, max_hfr);
        let _ = ctx.set_font("10px monospace");
        let _ = ctx.fill_text(&label, 6.0, 12.0);
    });

    // ── Settings grid ─────────────────────────────────────────────────────
    let send_set_all = send.clone();
    let dispatch_setting = move |key: &'static str, value: serde_json::Value| {
        ws_dispatch_setting(&send_set_all, "focus_set_all_settings", None, key, value);
    };

    let settings_rows = move || {
        let settings = focus.with(|f| f.settings.clone());
        let obj = match settings.as_object() {
            Some(o) => o.clone(),
            None => return Vec::new(),
        };
        let mut rows: Vec<(String, String, serde_json::Value)> = Vec::new();
        for key in KNOWN_SETTING_KEYS {
            if let Some(v) = obj.get(*key) {
                let kind = if v.is_boolean() { "bool" }
                           else if v.is_number() { "number" }
                           else { "string" };
                rows.push((key.to_string(), kind.to_string(), v.clone()));
            }
        }
        rows
    };

    let btn_base = "py-sp-2 px-sp-3 bg-[rgba(12,14,24,0.9)] border border-border-base text-text font-mono text-sm cursor-pointer";
    let btn_action = format!("{btn_base} !border-text-blue !text-text-blue");
    let btn_action_clone = btn_action.clone();
    let fieldset_cls = "border border-border-base py-sp-3 px-3";
    let legend_cls = "text-text-blue px-sp-2 text-sm";
    let header_label = "text-text-blue";

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono grid grid-rows-[56px_1fr] overflow-hidden">
            // Header
            <div class="flex items-center gap-sp-5 pr-5 pl-20 border-b border-border-base bg-[rgba(6,6,15,0.85)] text-md">
                <span
                    class="inline-block py-sp-1 px-sp-3 rounded-[14px] border border-current text-sm"
                    style=move || format!(
                        "color:{};",
                        status_color(&focus.with(|f| f.status.clone()))
                    )
                >
                    {move || {
                        let s = focus.with(|f| f.status.clone());
                        if s.is_empty() { tr().idle.to_string() } else { s }
                    }}
                </span>
                <span class=header_label>{move || tr().focus_header_focuser}</span>
                <span>{move || {
                    let d = focus.with(|f| f.device.clone());
                    if d.is_empty() { "—".to_string() } else { d }
                }}</span>
                <span class=header_label>{move || tr().focus_header_hfr}</span>
                <span>{move || focus.with(|f| f.hfr
                    .map(|v| format!("{:.3}", v))
                    .unwrap_or_else(|| "—".into()))}</span>
                <span class=header_label>{move || tr().focus_header_position}</span>
                <span>{move || focus.with(|f| f.position
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "—".into()))}</span>
                <span class=header_label>{move || tr().focus_header_temperature}</span>
                <span>{move || focus.with(|f| f.temperature
                    .map(|v| format!("{:.1}°C", v))
                    .unwrap_or_else(|| "—".into()))}</span>
            </div>

            // Body — 1fr | 320 px on desktop, stacked on tablet
            <div class="grid grid-cols-[1fr_320px] md:max-lg:grid-cols-[3fr_2fr] min-h-0">
                // Left — preview + HFR plot
                <div class="grid grid-rows-[1fr_110px] md:max-lg:grid-rows-[1fr_90px] min-h-0 border-r border-border-base">
                    <div class="relative min-h-0 overflow-hidden flex items-center justify-center bg-bg-input-deep">
                        {move || match focus.with(|f| f.preview_url.clone()) {
                            Some(url) => view! {
                                <img
                                    src=url
                                    class="max-w-full max-h-full object-contain cursor-crosshair [image-rendering:pixelated]"
                                    on:click=on_preview_click.clone()
                                />
                            }.into_any(),
                            None => view! {
                                <div class="text-[#444] text-sm text-center px-3">
                                    {move || tr().focus_no_frame}
                                </div>
                            }.into_any(),
                        }}
                    </div>
                    <div class="border-t border-border-base p-sp-2 bg-bg-input-deep">
                        <canvas
                            node_ref=canvas_ref
                            class="w-full h-[94px] md:max-lg:!h-[76px] block"
                            width="640"
                            height="90"
                        ></canvas>
                    </div>
                </div>

                // Right — controls
                <div class="flex flex-col min-h-0 overflow-y-auto py-sp-4 px-4 gap-4">

                    // Actions
                    <fieldset class=fieldset_cls>
                        <legend class=legend_cls>{move || tr().focus_actions_section}</legend>
                        <div class="grid grid-cols-2 gap-sp-2">
                            <button on:click=on_start class=format!("{btn_base} !border-accent-green !text-accent-green")>{move || tr().start}</button>
                            <button on:click=on_stop  class=format!("{btn_base} !border-[#ff6a6a] !text-[#ff6a6a]")>{move || tr().stop}</button>
                            <button on:click=on_capture class=btn_action.clone()>{move || tr().focus_capture_btn}</button>
                            <button on:click=on_loop    class=btn_action.clone()>{move || tr().focus_loop_btn}</button>
                            <button on:click=on_reset class=format!("{btn_base} col-span-2")>
                                {move || tr().focus_reset_frame}
                            </button>
                        </div>
                    </fieldset>

                    // Manual
                    <fieldset class=fieldset_cls>
                        <legend class=legend_cls>{move || tr().focus_manual_section}</legend>
                        <div class="flex items-center gap-sp-2 mb-sp-2">
                            <span class="text-sm text-text-blue">{move || tr().focus_step_label}</span>
                            <input
                                type="number"
                                min="1"
                                value=move || step_size.get().to_string()
                                on:input=move |ev| {
                                    let v: i64 = event_target_value(&ev).parse().unwrap_or(100);
                                    step_size.set(v.max(1));
                                }
                                class="flex-1 bg-bg-input-deep text-text-dim border border-border-base py-sp-1 px-sp-2 font-mono text-sm"
                            />
                        </div>
                        <div class="grid grid-cols-2 gap-sp-2">
                            <button on:click=on_in  class=btn_action_clone.clone()>{move || tr().focus_in_btn}</button>
                            <button on:click=on_out class=btn_action_clone>{move || tr().focus_out_btn}</button>
                        </div>
                    </fieldset>

                    // Settings
                    <fieldset class=fieldset_cls>
                        <legend class=legend_cls>{move || tr().focus_settings_section}</legend>
                        <div class="flex flex-col gap-sp-2">
                            {move || {
                                let rows = settings_rows();
                                if rows.is_empty() {
                                    return view! {
                                        <div class="text-[#555] text-sm">
                                            {tr().focus_settings_not_loaded}
                                        </div>
                                    }.into_any();
                                }
                                rows.into_iter().map(|(key, kind, val)| {
                                    let dispatch = dispatch_setting.clone();
                                    render_setting_row(key, kind, val, dispatch)
                                }).collect::<Vec<_>>().into_any()
                            }}
                        </div>
                    </fieldset>
                </div>
            </div>
        </div>
    }
}

fn render_setting_row(
    key: String,
    kind: String,
    val: serde_json::Value,
    dispatch: impl Fn(&'static str, serde_json::Value) + Clone + 'static,
) -> leptos::prelude::AnyView {
    // Find the static slice for the key so the dispatcher closure stays 'static.
    let static_key: &'static str = KNOWN_SETTING_KEYS
        .iter()
        .find(|k| **k == key.as_str())
        .copied()
        .unwrap_or("");

    let display = match &val {
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    let field = match kind.as_str() {
        "bool" => {
            let checked = val.as_bool().unwrap_or(false);
            let d = dispatch.clone();
            view! {
                <input
                    type="checkbox"
                    checked=checked
                    on:change=move |ev| {
                        let on = event_target_checked(&ev);
                        d(static_key, serde_json::Value::Bool(on));
                    }
                />
            }.into_any()
        }
        "number" => {
            let d = dispatch.clone();
            view! {
                <input
                    type="number"
                    value=display.clone()
                    on:change=move |ev| {
                        let s = event_target_value(&ev);
                        if let Ok(n) = s.parse::<f64>() {
                            if let Some(num) = serde_json::Number::from_f64(n) {
                                d(static_key, serde_json::Value::Number(num));
                            }
                        }
                    }
                    class="flex-1 bg-bg-input-deep text-text-dim border border-border-base py-sp-1 px-sp-2 font-mono text-sm"
                />
            }.into_any()
        }
        _ => {
            let d = dispatch.clone();
            view! {
                <input
                    type="text"
                    value=display.clone()
                    on:change=move |ev| {
                        let s = event_target_value(&ev);
                        d(static_key, serde_json::Value::String(s));
                    }
                    class="flex-1 bg-bg-input-deep text-text-dim border border-border-base py-sp-1 px-sp-2 font-mono text-sm"
                />
            }.into_any()
        }
    };

    let title_key = key.clone();
    view! {
        <div class="flex items-center gap-sp-2 text-sm">
            <span class="basis-[140px] grow-0 shrink-0 text-text-blue overflow-hidden text-ellipsis whitespace-nowrap" title=title_key>
                {key}
            </span>
            {field}
        </div>
    }.into_any()
}

fn event_target_checked(ev: &web_sys::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}

fn event_target_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

// Silence unused-import warnings if `Closure` ends up unused at a given rustc
// incremental state — the file used to need it for raf-based renders.
#[allow(dead_code)]
fn _keep_closure_imported() { let _: Option<Closure<dyn FnMut()>> = None; }
