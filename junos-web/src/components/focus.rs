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
use crate::i18n::{Lang, Translations, t};
use crate::ws::SendCmd;
use crate::ws_helpers::{send_cmd, dispatch_setting as ws_dispatch_setting};

mod abmath;
mod aberration;
use aberration::AberrationInspector;

fn status_color(status: &str) -> &'static str {
    let s = status.to_lowercase();
    if s.contains("fail") || s.contains("abort") { "var(--state-err)" }
    else if s.contains("complete")                { "var(--state-ok)" }
    else if s.contains("progress")                { "var(--state-info)" }
    else if s.contains("framing")                 { "var(--state-warn)" }
    else if s.contains("changing")                { "var(--state-warn)" }
    else if s.contains("user input")              { "var(--state-warn)" }
    else                                           { "var(--text-muted)" }
}

/// Combo lists for keys that KStars exposes as `currentText` of a QComboBox.
/// Sourced from `kstars/ekos/focus/opsfocusprocess.ui` and the focus widgets.
/// When a key appears here, the settings overlay renders a `<select>` instead
/// of a free-text input.
const FOCUS_ALGORITHM_OPTS: &[&str] =
    &["Iterative", "Polynomial", "Linear", "Linear 1 Pass"];
const FOCUS_BINNING_OPTS: &[&str] = &["1x1", "2x2", "3x3", "4x4"];

fn param_label(key: &str, tr: &Translations) -> &'static str {
    match key {
        "focusExposure"        => tr.focus_param_exposure,
        "focusBinning"         => tr.focus_param_binning,
        "focusGain"            => tr.gain,
        "focusISO"             => tr.focus_param_iso,
        "focusIterations"      => tr.focus_param_iterations,
        "focusStepSize"        => tr.focus_step_size,
        "focusMaxStep"         => tr.focus_param_max_step,
        "focusMaxTravel"       => tr.focus_param_max_travel,
        "focusTolerance"       => tr.focus_tolerance,
        "focusBacklash"        => tr.focus_backlash,
        "focusAlgorithm"       => tr.focus_algorithm,
        "focusAutoStarEnabled" => tr.focus_param_auto_star,
        "focusSuspendGuiding"  => tr.focus_param_suspend_guiding,
        "focusUseFullField"    => tr.focus_param_use_full_field,
        _ => "",
    }
}

fn enum_options_for(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "focusAlgorithm" => Some(FOCUS_ALGORITHM_OPTS),
        "focusBinning"   => Some(FOCUS_BINNING_OPTS),
        _ => None,
    }
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
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let step_size = RwSignal::new(100_i64);

    // Settings overlay open/closed (mirrors guide tab pattern).
    let settings_open = RwSignal::new(false);

    // Escape closes the overlay. forget() the closure (one persistent listener
    // per FocusTab mount); calls into a disposed RwSignal are a no-op in
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

        // Background.
        ctx.set_fill_style_str("#0a0a0f");
        ctx.fill_rect(0.0, 0.0, w, h);

        // Margined plot box — leave room for Y tick labels (left) and X tick
        // labels (bottom). Compact: tuned to the ~90 px backing height.
        let ml = 34.0; // left  — Y tick labels
        let mb = 14.0; // bottom — X tick labels
        let mt = 8.0;  // top
        let mr = 8.0;  // right
        let px0 = ml;
        let py0 = mt;
        let pw = (w - ml - mr).max(1.0);
        let ph = (h - mt - mb).max(1.0);
        let px1 = px0 + pw;
        let py1 = py0 + ph;

        // Plot frame (Y axis + X axis lines).
        ctx.set_stroke_style_str("#2a2c3a");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(px0 + 0.5, py0);
        ctx.line_to(px0 + 0.5, py1 + 0.5);
        ctx.line_to(px1, py1 + 0.5);
        let _ = ctx.stroke();

        let _ = ctx.set_font("9px monospace");

        // HFR (Y) value range, with a small headroom pad so the curve never
        // touches the frame. `span` floor keeps flat-HFR runs from collapsing.
        let raw_min = history.iter().map(|s| s.hfr).fold(f64::INFINITY, f64::min);
        let raw_max = history.iter().map(|s| s.hfr).fold(f64::NEG_INFINITY, f64::max);
        let (y_min, y_max) = if history.is_empty() || !raw_min.is_finite() {
            (0.0, 1.0)
        } else {
            let span = (raw_max - raw_min).max(0.1);
            let pad = span * 0.12;
            (raw_min - pad, raw_max + pad)
        };
        let y_span = (y_max - y_min).max(0.1);
        let y_of = |hfr: f64| py1 - (hfr - y_min) / y_span * ph;

        // Y gridlines + tick labels at min / mid / max of the padded range.
        ctx.set_text_align("right");
        ctx.set_text_baseline("middle");
        for k in 0..3 {
            let val = y_min + y_span * (k as f64) / 2.0;
            let y = y_of(val);
            if k > 0 && k < 2 {
                ctx.set_stroke_style_str("#1e2030");
                ctx.set_line_width(1.0);
                ctx.begin_path();
                ctx.move_to(px0 + 1.0, y + 0.5);
                ctx.line_to(px1, y + 0.5);
                let _ = ctx.stroke();
            }
            ctx.set_fill_style_str("#8896b5");
            let _ = ctx.fill_text(&format!("{:.2}", val), px0 - 4.0, y);
        }

        // Axis titles.
        ctx.set_fill_style_str("#6b7591");
        ctx.set_text_align("left");
        ctx.set_text_baseline("top");
        let _ = ctx.fill_text("HFR", 2.0, 1.0);
        ctx.set_text_align("right");
        let _ = ctx.fill_text("#", w - 2.0, py1 + 3.0);

        if history.len() < 2 {
            // Empty / single-sample: leave the labelled frame visible.
            return;
        }

        let n = history.len();
        let x_of = |i: usize| px0 + (i as f64) * pw / ((n - 1) as f64);

        // X (sample index) ticks — cap at ~6 labels, always include the last.
        ctx.set_text_align("center");
        ctx.set_text_baseline("top");
        let step = ((n - 1) / 5).max(1);
        let mut idx = 0usize;
        while idx < n {
            let x = x_of(idx);
            ctx.set_stroke_style_str("#2a2c3a");
            ctx.set_line_width(1.0);
            ctx.begin_path();
            ctx.move_to(x + 0.5, py1);
            ctx.line_to(x + 0.5, py1 + 2.5);
            let _ = ctx.stroke();
            ctx.set_fill_style_str("#8896b5");
            let _ = ctx.fill_text(&format!("{}", idx + 1), x, py1 + 3.0);
            idx += step;
        }
        // Always label the final sample.
        {
            let x = x_of(n - 1);
            ctx.set_stroke_style_str("#2a2c3a");
            ctx.set_line_width(1.0);
            ctx.begin_path();
            ctx.move_to(x + 0.5, py1);
            ctx.line_to(x + 0.5, py1 + 2.5);
            let _ = ctx.stroke();
            ctx.set_fill_style_str("#8896b5");
            let _ = ctx.fill_text(&format!("{}", n), x, py1 + 3.0);
        }

        // Series polyline.
        ctx.set_stroke_style_str("#88aaff");
        ctx.set_line_width(1.5);
        ctx.begin_path();
        for (i, s) in history.iter().enumerate() {
            let x = x_of(i);
            let y = y_of(s.hfr);
            if i == 0 { ctx.move_to(x, y); } else { ctx.line_to(x, y); }
        }
        let _ = ctx.stroke();

        // Per-sample dots, with the last (current) sample emphasised.
        for (i, s) in history.iter().enumerate() {
            let x = x_of(i);
            let y = y_of(s.hfr);
            let last = i == n - 1;
            ctx.set_fill_style_str(if last { "#ffffff" } else { "#cfe0ff" });
            ctx.begin_path();
            let _ = ctx.arc(x, y, if last { 2.4 } else { 1.6 }, 0.0,
                std::f64::consts::TAU);
            ctx.fill();
        }
    });

    // ── Settings grid ─────────────────────────────────────────────────────
    // Stash `send` in a StoredValue so the reactive closure that renders
    // settings rows (now nested inside <Show>) doesn't have to capture a
    // non-Copy SendCmd through two layers of Fn closures. We rebuild the
    // dispatcher fresh on each reactive evaluation.
    let send_sv = StoredValue::new(send.clone());
    let send_ab = send.clone();

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

    let btn_action = "btn btn-ghost text-text-blue !border-text-blue".to_string();
    let btn_action_clone = btn_action.clone();
    let fieldset_cls = "fieldset";
    let legend_cls = "fieldset__legend";
    let header_label = "text-text-blue";

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono grid grid-rows-[56px_1fr] overflow-hidden">
            // Header
            <div class="flex flex-wrap items-center gap-y-[10px] gap-x-[18px] py-[10px] pr-5 pl-20 border-b border-border-base bg-[rgba(6,6,15,0.85)] text-md min-h-[44px] max-[759px]:py-sp-2 max-[759px]:pl-16 max-[759px]:pr-3 max-[759px]:gap-x-3 max-[759px]:gap-y-[6px] max-[759px]:text-sm">
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
                <span class="inline-flex items-center gap-[6px]">
                    <span class=header_label>{move || tr().focus_header_focuser}</span>
                    <span>{move || {
                        let d = focus.with(|f| f.device.clone());
                        if d.is_empty() { "—".to_string() } else { d }
                    }}</span>
                </span>
                <span class="inline-flex items-center gap-[6px]">
                    <span class=header_label>{move || tr().focus_header_hfr}</span>
                    <span>{move || focus.with(|f| f.hfr
                        .map(|v| format!("{:.3}", v))
                        .unwrap_or_else(|| "—".into()))}</span>
                </span>
                <span class="inline-flex items-center gap-[6px]">
                    <span class=header_label>{move || tr().focus_header_position}</span>
                    <span>{move || focus.with(|f| f.position
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "—".into()))}</span>
                </span>
                <span class="inline-flex items-center gap-[6px]">
                    <span class=header_label>{move || tr().focus_header_temperature}</span>
                    <span>{move || focus.with(|f| f.temperature
                        .map(|v| format!("{:.1}°C", v))
                        .unwrap_or_else(|| "—".into()))}</span>
                </span>
            </div>

            // Body — 1fr | 320 px on desktop, narrower right column on tablet, stacked on mobile
            <div class="grid grid-cols-[1fr_320px] max-[1199px]:grid-cols-[minmax(0,1fr)_280px] max-[759px]:flex max-[759px]:flex-col min-h-0">
                // Left — preview + HFR plot
                <div class="grid grid-rows-[1fr_110px] min-h-0 border-r border-border-base max-[759px]:shrink-0 max-[759px]:min-h-[180px] max-[759px]:max-h-[38vh] max-[759px]:border-r-0 max-[759px]:border-b max-[759px]:border-border-base">
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
                            class="w-full h-[94px] max-[759px]:h-[72px] block"
                            width="640"
                            height="90"
                        ></canvas>
                    </div>
                </div>

                // Right — controls
                <div class="flex flex-col min-h-0 overflow-y-auto py-sp-4 px-4 gap-4 max-[759px]:p-sp-3 max-[759px]:gap-sp-3 max-[759px]:pb-sp-6">

                    // Actions
                    <fieldset class=fieldset_cls>
                        <legend class=legend_cls>{move || tr().focus_actions_section}</legend>
                        <div class="grid grid-cols-2 gap-sp-2">
                            <button on:click=on_start class="btn btn-primary">{move || tr().start}</button>
                            <button on:click=on_stop  class="btn btn-danger">{move || tr().stop}</button>
                            <button on:click=on_capture class=btn_action.clone()>{move || tr().focus_capture_btn}</button>
                            <button on:click=on_loop    class=btn_action.clone()>{move || tr().focus_loop_btn}</button>
                            <button on:click=on_reset class="btn btn-ghost col-span-2">
                                {move || tr().focus_reset_frame}
                            </button>
                            <button
                                class="btn btn-ghost col-span-2"
                                on:click=move |_| settings_open.set(true)>
                                {move || tr().guide_settings_button}
                            </button>
                            <AberrationInspector focus=focus camera=camera send=send_ab.clone() />
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
                                class="input input--sm flex-1 font-mono"
                            />
                        </div>
                        <div class="grid grid-cols-2 gap-sp-2">
                            <button on:click=on_in  class=btn_action_clone.clone()>{move || tr().focus_in_btn}</button>
                            <button on:click=on_out class=btn_action_clone>{move || tr().focus_out_btn}</button>
                        </div>
                    </fieldset>

                </div>
            </div>

            // Fullscreen settings overlay (mirrors guide tab).
            <Show when=move || settings_open.get()>
                <div
                    class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2"
                    on:click=move |_| settings_open.set(false)>
                    <div
                        class="w-full max-w-[980px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col"
                        on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                        <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                            <h2 class="text-text-blue text-sm uppercase tracking-[0.08em] m-0">
                                {move || tr().focus_settings_section}
                            </h2>
                            <button
                                class="btn btn-ghost"
                                on:click=move |_| settings_open.set(false)>
                                {move || tr().imaging_close}
                            </button>
                        </div>
                        <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 flex flex-col gap-sp-2">
                            {move || {
                                let send = send_sv.get_value();
                                let dispatch_setting = move |key: &'static str, value: serde_json::Value| {
                                    ws_dispatch_setting(&send, "focus_set_all_settings", None, key, value);
                                };
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
                                    let label = param_label(&key, tr());
                                    render_setting_row(key, kind, val, label, dispatch)
                                }).collect::<Vec<_>>().into_any()
                            }}
                        </div>
                    </div>
                </div>
            </Show>
        </div>
    }
}

fn render_setting_row(
    key: String,
    kind: String,
    val: serde_json::Value,
    label: &'static str,
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

    // Enum-valued keys (combo boxes in KStars) render as <select>, regardless
    // of whether the current payload happened to type them as string/number.
    let field = if let Some(opts) = enum_options_for(static_key) {
        let d = dispatch.clone();
        let current = display.clone();
        let opts_vec: Vec<String> = {
            let mut v: Vec<String> = opts.iter().map(|s| s.to_string()).collect();
            if !current.is_empty() && !v.iter().any(|o| o == &current) {
                v.insert(0, current.clone());
            }
            v
        };
        view! {
            <select
                class="input input--sm flex-1 font-mono"
                prop:value=current.clone()
                on:change=move |ev| {
                    let s = ev.target()
                        .and_then(|t| t.dyn_into::<web_sys::HtmlSelectElement>().ok())
                        .map(|el| el.value())
                        .unwrap_or_default();
                    d(static_key, serde_json::Value::String(s));
                }
            >
                {opts_vec.into_iter().map(|o| {
                    let l = o.clone();
                    view! { <option value=o>{l}</option> }
                }).collect::<Vec<_>>()}
            </select>
        }.into_any()
    } else { match kind.as_str() {
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
                    class="input input--sm flex-1 font-mono"
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
                    class="input input--sm flex-1 font-mono"
                />
            }.into_any()
        }
    } };

    let title_key = key.clone();
    view! {
        <div class="flex items-center gap-sp-2 text-sm max-[420px]:flex-col max-[420px]:items-stretch">
            <span class="basis-[140px] grow-0 shrink-0 text-text-blue overflow-hidden text-ellipsis whitespace-nowrap max-[759px]:basis-[110px] max-[420px]:basis-auto" title=title_key>
                {if label.is_empty() { key } else { label.to_string() }}
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
