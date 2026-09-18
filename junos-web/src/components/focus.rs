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

/// A `js_sys::Array` of dash lengths for `CanvasRenderingContext2d::set_line_dash`.
/// An empty slice resets to a solid stroke.
fn dash_array(segments: &[f64]) -> wasm_bindgen::JsValue {
    let arr = web_sys::js_sys::Array::new();
    for v in segments {
        arr.push(&wasm_bindgen::JsValue::from_f64(*v));
    }
    arr.into()
}

/// Draw text with a dark outline behind it so it stays readable over a bright
/// starfield or a gridline. Uses the context's current font/alignment.
fn halo_text(ctx: &CanvasRenderingContext2d, text: &str, x: f64, y: f64, fill: &str) {
    ctx.set_line_width(2.5);
    ctx.set_line_join("round");
    ctx.set_stroke_style_str("rgba(0,0,0,0.85)");
    let _ = ctx.stroke_text(text, x, y);
    ctx.set_fill_style_str(fill);
    let _ = ctx.fill_text(text, x, y);
}

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

    // Detected-stars overlay: on by default. `resize_tick` is bumped whenever
    // the preview box can change size, so both canvas draw Effects re-run:
    // the window `resize` listener below, `<img on:load>`, and the settings
    // overlay opening/closing. (There is no ResizeObserver — `web-sys` isn't
    // built with that feature here.)
    let show_stars = RwSignal::new(true);
    let resize_tick = RwSignal::new(0u32);

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

    // ── HFR chart ─────────────────────────────────────────────────────────
    // A V-curve when the focuser reports absolute positions, otherwise a
    // scatter against sample order. Both modes get labelled, unit-carrying
    // axes — without them a wiggle could be 0.02 px or 2 px and the reader
    // has no way to tell.
    let canvas_ref = NodeRef::<html::Canvas>::new();
    Effect::new(move |_| {
        // Redraw on layout changes as well as data changes.
        resize_tick.track();
        let tr = tr();
        let history = focus.with(|f| f.history.clone());
        let Some(canvas) = canvas_ref.get() else { return };
        let canvas: HtmlCanvasElement = canvas.unchecked_into();

        // ── Crisp, HiDPI-aware sizing ─────────────────────────────────────
        // The canvas is CSS-sized (w-full h-full); size its backing store to
        // the displayed size × devicePixelRatio and draw in CSS-pixel space so
        // nothing is stretched. Mirrors components/sky/mod.rs.
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0)
            .clamp(1.0, 2.0);
        let cw = canvas.client_width() as f64;
        let ch = canvas.client_height() as f64;
        if cw <= 0.0 || ch <= 0.0 { return; }
        let bw = (cw * dpr).round() as u32;
        let bh = (ch * dpr).round() as u32;
        if canvas.width() != bw { canvas.set_width(bw); }
        if canvas.height() != bh { canvas.set_height(bh); }

        let Ok(Some(ctx)) = canvas.get_context("2d") else { return };
        let ctx: CanvasRenderingContext2d = ctx.unchecked_into();
        // Reset any prior transform, then scale so 1 unit = 1 CSS pixel.
        let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        ctx.clear_rect(0.0, 0.0, bw as f64, bh as f64);
        let _ = ctx.scale(dpr, dpr);

        // Palette (literals from styles/tokens.css — Canvas2D can't read var()).
        const CYAN: &str = "#5beaff";       // --accent-cyan
        const BRIGHT: &str = "#c1d2ff";      // --text-blue-bright
        const MUTED: &str = "#9aa3b8";       // --text-muted
        const BORDER: &str = "#1c1e2c";      // --border
        const OK: &str = "#3ee08a";          // --state-ok

        // Plot box — a left gutter for the Y tick labels and a bottom gutter
        // for the X ticks plus the axis title.
        let pad_l = 44.0;
        let pad_r = 12.0;
        let pad_t = 12.0;
        let pad_b = 30.0;
        let px0 = pad_l;
        let py0 = pad_t;
        let pw = (cw - pad_l - pad_r).max(1.0);
        let ph = (ch - pad_t - pad_b).max(1.0);
        let py1 = py0 + ph;

        let frame = || {
            ctx.set_stroke_style_str(BORDER);
            ctx.set_line_width(1.0);
            ctx.begin_path();
            ctx.move_to(px0 + 0.5, py0);
            ctx.line_to(px0 + 0.5, py1 + 0.5);
            ctx.line_to(px0 + pw, py1 + 0.5);
            let _ = ctx.stroke();
        };
        let y_axis_title = || {
            let _ = ctx.set_font("10px monospace");
            ctx.set_fill_style_str(MUTED);
            ctx.set_text_align("left");
            ctx.set_text_baseline("top");
            let _ = ctx.fill_text(tr.focus_axis_hfr_px, 2.0, 1.0);
        };

        // Empty / single-sample: axis chrome only, never blank or garbled.
        if history.len() < 2 {
            frame();
            y_axis_title();
            let _ = ctx.set_font("10px monospace");
            ctx.set_fill_style_str(MUTED);
            ctx.set_text_align("center");
            ctx.set_text_baseline("middle");
            let _ = ctx.fill_text(tr.focus_chart_waiting, px0 + pw / 2.0, py0 + ph / 2.0);
            return;
        }

        let n = history.len();

        // ── Y scale ────────────────────────────────────────────────────────
        // Tick count follows the available height (~32 px apart), so the axis
        // degrades to 2 gridlines on a short mobile chart instead of crowding.
        let y_target = ((ph / 32.0).round() as usize).clamp(2, 5);
        let raw_min = history.iter().map(|s| s.hfr).fold(f64::INFINITY, f64::min);
        let raw_max = history.iter().map(|s| s.hfr).fold(f64::NEG_INFINITY, f64::max);
        let (mut lo, mut hi) = (raw_min, raw_max);
        // Floor on a flat run, or a stable HFR would be plotted against a
        // meaningless 0.001-wide ladder.
        if hi - lo < 0.2 {
            let c = (lo + hi) * 0.5;
            lo = c - 0.1;
            hi = c + 0.1;
        }
        let m = (hi - lo) * 0.04; // nice-rounding below adds the rest
        let (y_min, y_max, y_step) = abmath::nice_axis(lo - m, hi + m, y_target);
        let y_min = y_min.max(0.0); // HFR is never negative
        let y_span = (y_max - y_min).max(1e-6);
        let y_of = |hfr: f64| py1 - (hfr - y_min) / y_span * ph;
        // Decimals follow the step, so a 0.05 ladder isn't labelled 3.1/3.1/3.2.
        let y_decimals = ((-y_step.log10().floor()).max(0.0) as usize).min(2);

        // ── X mode: a true V-curve against focuser position when every sample
        // carries one and they are not all identical; otherwise sample order.
        // `pos` is already normalised in ws/store.rs (KStars sends -1 for
        // relative focusers), so `Some` here always means a real position. ───
        let positions: Vec<f64> =
            history.iter().filter_map(|s| s.position).map(|p| p as f64).collect();
        let p_min = positions.iter().copied().fold(f64::INFINITY, f64::min);
        let p_max = positions.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let pos_mode = positions.len() == n && p_max > p_min;

        let x_of_pos = |p: f64| px0 + (p - p_min) / (p_max - p_min) * pw;
        let x_of_idx = |i: usize| px0 + (i as f64) * pw / ((n - 1) as f64);
        let x_of = |i: usize| {
            if pos_mode { x_of_pos(positions[i]) } else { x_of_idx(i) }
        };

        // ── Gridlines + Y tick labels ──────────────────────────────────────
        ctx.set_line_width(1.0);
        let _ = ctx.set_font("10px monospace");
        ctx.set_text_baseline("middle");
        let y_ticks = ((y_span / y_step).round() as i32).clamp(1, 12);
        for k in 0..=y_ticks {
            let v = y_min + y_step * k as f64;
            let y = y_of(v).round() + 0.5;
            ctx.set_stroke_style_str(BORDER);
            ctx.begin_path();
            ctx.move_to(px0, y);
            ctx.line_to(px0 + pw, y);
            let _ = ctx.stroke();
            ctx.set_fill_style_str(MUTED);
            ctx.set_text_align("right");
            let _ = ctx.fill_text(&format!("{:.*}", y_decimals, v), px0 - 5.0, y);
        }

        // ── X tick labels ──────────────────────────────────────────────────
        ctx.set_text_baseline("top");
        ctx.set_fill_style_str(MUTED);
        let label_x = |x: f64, s: &str, first: bool, last: bool| {
            ctx.set_text_align(if first { "left" } else if last { "right" } else { "center" });
            let _ = ctx.fill_text(s, x, py1 + 5.0);
        };
        if pos_mode {
            // Ticks on round position values inside the sampled span.
            let x_step = abmath::nice_step((p_max - p_min) / 4.0);
            let first_tick = (p_min / x_step).ceil() * x_step;
            let mut v = first_tick;
            let mut guard = 0;
            while v <= p_max + 1e-6 && guard < 24 {
                let x = x_of_pos(v);
                ctx.set_stroke_style_str(BORDER);
                ctx.begin_path();
                ctx.move_to(x.round() + 0.5, py1);
                ctx.line_to(x.round() + 0.5, py1 + 3.0);
                let _ = ctx.stroke();
                label_x(x, &format!("{}", v.round() as i64),
                        x < px0 + 14.0, x > px0 + pw - 14.0);
                v += x_step;
                guard += 1;
            }
        } else {
            // 5 evenly spaced sample indices, 1-based to match KStars' plot.
            let ticks = 4.min(n - 1);
            for k in 0..=ticks {
                let i = (k * (n - 1)) / ticks.max(1);
                let x = x_of_idx(i);
                ctx.set_stroke_style_str(BORDER);
                ctx.begin_path();
                ctx.move_to(x.round() + 0.5, py1);
                ctx.line_to(x.round() + 0.5, py1 + 3.0);
                let _ = ctx.stroke();
                label_x(x, &format!("#{}", i + 1), k == 0, k == ticks);
            }
        }

        // X axis title, centred under the ticks.
        ctx.set_fill_style_str(MUTED);
        ctx.set_text_align("center");
        ctx.set_text_baseline("bottom");
        let x_title = if pos_mode { tr.focus_axis_position } else { tr.focus_axis_sample };
        let _ = ctx.fill_text(x_title, px0 + pw / 2.0, ch - 1.0);

        frame();
        y_axis_title();

        // ── Fitted V-curve (position mode only) ────────────────────────────
        // Same parabola the aberration inspector fits, sampled at 20 points
        // across the range like KStars' focushfrvplot.cpp::drawPolynomial.
        // `a > 0` means it opens upward, i.e. it actually has a minimum.
        let mut vertex: Option<f64> = None;
        if pos_mode {
            let samples: Vec<abmath::Sample> = history
                .iter()
                .filter_map(|s| s.position.map(|p| abmath::Sample { pos: p as f64, hfr: s.hfr }))
                .collect();
            let distinct = {
                let mut v: Vec<i64> = samples.iter().map(|s| s.pos as i64).collect();
                v.sort_unstable();
                v.dedup();
                v.len()
            };
            if distinct >= 3 {
                if let Some((a, b, c)) =
                    abmath::fit_parabola(&samples).filter(|&(a, _, _)| a > 0.0)
                {
                    ctx.set_stroke_style_str(MUTED);
                    ctx.set_line_width(1.0);
                    let _ = ctx.set_line_dash(&dash_array(&[2.0, 3.0]));
                    ctx.begin_path();
                    for k in 0..=20 {
                        let p = p_min + (p_max - p_min) * (k as f64 / 20.0);
                        let y = y_of((a * p + b) * p + c).clamp(py0, py1);
                        let x = x_of_pos(p);
                        if k == 0 { ctx.move_to(x, y); } else { ctx.line_to(x, y); }
                    }
                    let _ = ctx.stroke();
                    let _ = ctx.set_line_dash(&dash_array(&[]));
                    let v = -b / (2.0 * a);
                    if v >= p_min && v <= p_max { vertex = Some(v); }
                }
            }
        }

        // ── HFR scatter points ─────────────────────────────────────────────
        // One dot per sample; no connecting line (point chart, not line chart).
        ctx.set_fill_style_str(CYAN);
        for (i, s) in history.iter().enumerate() {
            ctx.begin_path();
            let _ = ctx.arc(x_of(i), y_of(s.hfr).clamp(py0, py1), 2.2, 0.0, std::f64::consts::TAU);
            ctx.fill();
        }

        // ── Best-focus marker ──────────────────────────────────────────────
        // In position mode that is the fitted vertex (a real focuser position);
        // in index mode, fall back to the lowest sample seen so far.
        if let Some(v) = vertex {
            let vx = x_of_pos(v);
            ctx.set_stroke_style_str(OK);
            ctx.set_line_width(1.0);
            ctx.begin_path();
            ctx.move_to(vx.round() + 0.5, py0);
            ctx.line_to(vx.round() + 0.5, py1);
            let _ = ctx.stroke();
            let _ = ctx.set_font("10px monospace");
            ctx.set_text_baseline("top");
            let lbl = format!("{} {}", tr.focus_chart_best, v.round() as i64);
            let right = vx > px0 + pw * 0.6;
            ctx.set_text_align(if right { "right" } else { "left" });
            let lx = if right { vx - 4.0 } else { vx + 4.0 };
            halo_text(&ctx, &lbl, lx, py0 + 2.0, OK);
        } else if !pos_mode {
            let min_idx = history
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.hfr.partial_cmp(&b.1.hfr).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(n - 1);
            if min_idx != n - 1 {
                let mx = x_of(min_idx);
                let my = y_of(history[min_idx].hfr);
                ctx.set_fill_style_str(MUTED);
                ctx.begin_path();
                ctx.move_to(mx, my + 5.0);
                ctx.line_to(mx - 3.0, my + 10.0);
                ctx.line_to(mx + 3.0, my + 10.0);
                ctx.close_path();
                ctx.fill();
                let _ = ctx.set_font("10px monospace");
                ctx.set_text_baseline("top");
                // "min", not "best": this labels an HFR value, whereas the
                // position-mode marker labels a focuser position. Same word
                // for two different quantities is what we're fixing here.
                let lbl = format!("{} {:.2}", tr.focus_chart_min, history[min_idx].hfr);
                let right = mx > px0 + pw * 0.6;
                ctx.set_text_align(if right { "right" } else { "left" });
                let lx = if right { (mx - 5.0).min(px0 + pw) } else { (mx + 5.0).max(px0) };
                halo_text(&ctx, &lbl, lx, my + 11.0, MUTED);
            }
        }

        // ── Current point + value callout ─────────────────────────────────
        let last = &history[n - 1];
        let (lx, ly) = (x_of(n - 1), y_of(last.hfr).clamp(py0, py1));
        ctx.set_fill_style_str(CYAN);
        ctx.begin_path();
        let _ = ctx.arc(lx, ly, 3.4, 0.0, std::f64::consts::TAU);
        ctx.fill();
        ctx.set_fill_style_str("#ffffff");
        ctx.begin_path();
        let _ = ctx.arc(lx, ly, 1.6, 0.0, std::f64::consts::TAU);
        ctx.fill();

        let _ = ctx.set_font("11px monospace");
        ctx.set_text_baseline("middle");
        let val = format!("{:.2}", last.hfr);
        // Place the label left of the dot when it's near the right edge.
        let right = lx > px0 + pw * 0.72;
        ctx.set_text_align(if right { "right" } else { "left" });
        let tx = if right { lx - 6.0 } else { lx + 6.0 };
        halo_text(&ctx, &val, tx, ly.clamp(py0 + 6.0, py1 - 6.0), BRIGHT);

        // Which mode the X axis is in, so it never has to be guessed.
        let _ = ctx.set_font("10px monospace");
        ctx.set_fill_style_str(MUTED);
        ctx.set_text_align("right");
        ctx.set_text_baseline("top");
        let mode = if pos_mode { tr.focus_chart_mode_position } else { tr.focus_chart_mode_index };
        let _ = ctx.fill_text(&format!("{mode} · #{n}"), px0 + pw, 1.0);
    });

    // ── Detected-stars overlay ────────────────────────────────────────────
    // Canvas painted over the preview <img>. Star coords are in focus-JPEG
    // pixel space (server-side detection, kstars_ws.rs); we map them onto the
    // letterboxed `object-contain` image via a min-fit scale + centering offset.
    let preview_box_ref = NodeRef::<html::Div>::new();
    let stars_canvas_ref = NodeRef::<html::Canvas>::new();
    let img_ref = NodeRef::<html::Img>::new();

    // Re-run the draw Effect on window resize so the overlay tracks the
    // preview box as the layout reflows.
    {
        let cb = Closure::<dyn FnMut()>::new(move || {
            resize_tick.update(|n| *n = n.wrapping_add(1));
        });
        if let Some(win) = web_sys::window() {
            let _ = win
                .add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        }
        cb.forget();
    }

    // Opening or closing the settings overlay reflows the page without firing
    // a window `resize`, which would otherwise leave the overlay misaligned.
    Effect::new(move |_| {
        settings_open.track();
        resize_tick.update(|n| *n = n.wrapping_add(1));
    });

    Effect::new(move |_| {
        resize_tick.track();
        let tr = tr();
        let on = show_stars.get();
        let stars = focus.with(|f| f.stars.clone());
        let (Some(container), Some(canvas)) =
            (preview_box_ref.get(), stars_canvas_ref.get())
        else { return };
        let container: web_sys::HtmlElement = container.unchecked_into();
        let canvas: HtmlCanvasElement = canvas.unchecked_into();

        let cw = container.client_width().max(0) as f64;
        let ch = container.client_height().max(0) as f64;
        if cw <= 0.0 || ch <= 0.0 { return; }

        // HiDPI-aware sizing, same treatment as the chart canvas below: back
        // the canvas at device resolution and draw in CSS-pixel space, or the
        // labels come out soft on a retina display.
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0)
            .clamp(1.0, 2.0);
        let bw = (cw * dpr).round() as u32;
        let bh = (ch * dpr).round() as u32;
        if canvas.width() != bw { canvas.set_width(bw); }
        if canvas.height() != bh { canvas.set_height(bh); }

        let Ok(Some(ctx)) = canvas.get_context("2d") else { return };
        let ctx: CanvasRenderingContext2d = ctx.unchecked_into();
        let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        ctx.clear_rect(0.0, 0.0, bw as f64, bh as f64);
        let _ = ctx.scale(dpr, dpr);

        let Some(fs) = stars else { return };
        if !on || fs.img_w <= 0.0 || fs.img_h <= 0.0 || fs.stars.is_empty() { return; }

        // JPEG px → sensor px for *display only*. Star coordinates must stay in
        // JPEG space for the letterbox mapping below, so `FocusStar` keeps one
        // unit throughout and the conversion happens here, at the boundary.
        let hfr_k = fs.sensor_scale.unwrap_or(1.0);

        // Map JPEG pixel space onto the *actually rendered* image rectangle.
        // The <img> uses `max-w-full max-h-full object-contain`, so a frame
        // smaller than the container renders at natural size (never scaled up);
        // recomputing an object-contain fit from the container over-scales it.
        // Prefer the img element's real geometry; fall back to container-based
        // object-contain math when the img isn't laid out yet (offset_* == 0).
        let img_box = img_ref.get().and_then(|img| {
            let img: web_sys::HtmlElement = img.unchecked_into();
            let iw = img.offset_width() as f64;
            let ih = img.offset_height() as f64;
            if iw > 0.0 && ih > 0.0 { Some((iw, ih)) } else { None }
        });
        let (scale, ox, oy) = match img_box {
            Some((iw, ih)) => {
                // Aspect is preserved, so iw/img_w == ih/img_h. The flex
                // container centers the img, matching (cw - iw)/2 offsets.
                (iw / fs.img_w, (cw - iw) / 2.0, (ch - ih) / 2.0)
            }
            None => {
                let scale = (cw / fs.img_w).min(ch / fs.img_h);
                (scale, (cw - fs.img_w * scale) / 2.0, (ch - fs.img_h * scale) / 2.0)
            }
        };

        // KStars already burns its OWN red HFR text into this JPEG before
        // sending it: Focus::initView sets setStarsHFREnabled(true)
        // (focus.cpp:7782), and drawStarCentroid then draws the number — *not*
        // a ring — at (xc + w + 5, yc + w/2), i.e. right of the star at its
        // vertical centre (fitsview.cpp:1653-1657). That text is rendered at
        // full frame scale and then downscaled with the pixmap, so on a large
        // sensor it arrives as an illegible red smear we cannot remove.
        //
        // So: anchor our label UP-and-right at 45°, which clears KStars' text
        // vertically while still pointing back at the star, and keep the label
        // count low — a second dense layer on top of theirs is exactly what
        // made this unreadable.
        const MAX_LABELS: usize = 60;
        const ADV: f64 = 6.6;  // 11px monospace advance ≈ 0.6em
        const ASC: f64 = 9.0;  // ascent above the alphabetic baseline
        const DESC: f64 = 3.0; // descent below it

        ctx.set_font("11px monospace");
        ctx.set_text_baseline("alphabetic");

        // Rank softest-first: when labels must be dropped, the bloated stars
        // are the ones that matter for judging focus.
        let mut order: Vec<usize> = (0..fs.stars.len()).collect();
        order.sort_by(|&a, &b| {
            fs.stars[b].hfr
                .partial_cmp(&fs.stars[a].hfr)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Greedy AABB reject against already-placed labels. Linear scan is
        // fine: at most MAX_LABELS boxes × the detector's star cap.
        let mut placed: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(MAX_LABELS);
        for &i in &order {
            if placed.len() >= MAX_LABELS { break; }
            let st = &fs.stars[i];
            let sx = ox + st.x * scale;
            let sy = oy + st.y * scale;
            // Geometry uses the JPEG-space HFR and the screen scale; the
            // sensor-converted value is for the text only.
            let off = (st.hfr * scale * 1.6).clamp(7.0, 16.0);
            let txt = format!("{:.1}", st.hfr * hfr_k);
            let tw = txt.len() as f64 * ADV;

            let (mut lx, mut ly) = (sx + off, sy - off);
            let mut right_aligned = false;
            if lx + tw > cw - 4.0 { lx = sx - off; right_aligned = true; }
            if ly - ASC < 2.0 { ly = sy + off + ASC; }

            let (x0, x1) = if right_aligned { (lx - tw, lx) } else { (lx, lx + tw) };
            let (y0, y1) = (ly - ASC, ly + DESC);
            if x1 < 0.0 || y1 < 0.0 || x0 > cw || y0 > ch { continue; }
            // Inflate by 2 px so neighbouring labels keep a visible gap.
            let (bx0, by0, bx1, by1) = (x0 - 2.0, y0 - 2.0, x1 + 2.0, y1 + 2.0);
            if placed.iter().any(|b| bx0 < b.2 && bx1 > b.0 && by0 < b.3 && by1 > b.1) {
                continue;
            }
            placed.push((bx0, by0, bx1, by1));

            ctx.set_text_align(if right_aligned { "right" } else { "left" });
            // Near-white, not cyan: it has to out-contrast both the cyan-ish
            // auto-stretch and KStars' red text underneath.
            halo_text(&ctx, &txt, lx, ly, "#eaf6ff");
        }

        // ── Readout ───────────────────────────────────────────────────────
        // Median, not mean: one saturated blob drags the mean away from what
        // the labels show, and KStars' own aggregate is robust.
        //
        // Both aggregates are shown but labelled separately — they are not the
        // same estimator (ours is the flux-weighted mean radius, KStars/SEP the
        // half-flux radius, ~6% apart on a Gaussian), so presenting them as one
        // number would just move the confusion rather than remove it.
        let n = fs.stars.len();
        let median = {
            let mut v: Vec<f64> = fs.stars.iter().map(|s| s.hfr * hfr_k).collect();
            v.sort_by(f64::total_cmp);
            if v.len() % 2 == 1 {
                v[v.len() / 2]
            } else {
                (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
            }
        };
        let unit = if fs.sensor_scale.is_some() { tr.focus_unit_px } else { tr.focus_unit_preview_px };
        let mut lines: Vec<(String, &str, &str)> = Vec::new();
        let head = match fs.kstars_hfr.filter(|_| fs.sensor_scale.is_some()) {
            Some(k) => format!(
                "{n} {} · {} {:.2} · {} {:.2} {unit}",
                tr.focus_overlay_stars, tr.focus_overlay_kstars_hfr, k,
                tr.focus_overlay_median, median
            ),
            None => format!(
                "{n} {} · {} {:.2} {unit}",
                tr.focus_overlay_stars, tr.focus_overlay_median, median
            ),
        };
        lines.push((head, "11px monospace", "rgba(120, 235, 255, 0.95)"));
        // The single line that explains why these numbers never used to match
        // the header: the preview is a rescale of the frame.
        if let Some(k) = fs.sensor_scale.filter(|k| (k - 1.0).abs() > 0.02) {
            lines.push((
                format!("×{:.2} {}", k, tr.focus_overlay_scale_note),
                "9px monospace",
                "#9aa3b8",
            ));
        }

        ctx.set_text_align("left");
        ctx.set_text_baseline("top");
        let box_w = lines
            .iter()
            .map(|(l, f, _)| {
                ctx.set_font(f);
                ctx.measure_text(l).map(|m| m.width()).unwrap_or(150.0)
            })
            .fold(0.0, f64::max);
        ctx.set_fill_style_str("rgba(0,0,0,0.6)");
        ctx.fill_rect(6.0, 6.0, box_w + 12.0, 6.0 + 14.0 * lines.len() as f64);
        let mut ty = 9.0;
        for (l, f, color) in &lines {
            ctx.set_font(f);
            ctx.set_fill_style_str(color);
            let _ = ctx.fill_text(l, 12.0, ty);
            ty += 14.0;
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
                <div class="grid grid-rows-[minmax(0,1fr)_182px] max-[759px]:grid-rows-[minmax(0,1fr)_140px] min-h-0 border-r border-border-base max-[759px]:shrink-0 max-[759px]:min-h-[300px] max-[759px]:max-h-[52vh] max-[759px]:border-r-0 max-[759px]:border-b max-[759px]:border-border-base">
                    <div
                        node_ref=preview_box_ref
                        class="relative min-h-0 overflow-hidden flex items-center justify-center bg-bg-input-deep"
                    >
                        {move || match focus.with(|f| f.preview_url.clone()) {
                            Some(url) => view! {
                                <img
                                    node_ref=img_ref
                                    src=url
                                    class="max-w-full max-h-full object-contain cursor-crosshair [image-rendering:pixelated]"
                                    on:click=on_preview_click.clone()
                                    on:load=move |_| resize_tick.update(|n| *n = n.wrapping_add(1))
                                />
                            }.into_any(),
                            None => view! {
                                <div class="text-[#444] text-sm text-center px-3">
                                    {move || tr().focus_no_frame}
                                </div>
                            }.into_any(),
                        }}
                        // Detected-stars overlay (pointer-events-none so the
                        // crosshair click on the <img> still fires through it).
                        <canvas
                            node_ref=stars_canvas_ref
                            class="absolute inset-0 w-full h-full pointer-events-none"
                        ></canvas>
                        <Show when=move || focus.with(|f| f.preview_url.is_some())>
                            <button
                                class="absolute top-2 right-2 py-[3px] px-[10px] rounded-[12px] text-sm border border-current cursor-pointer bg-[rgba(6,6,15,0.7)]"
                                style=move || format!(
                                    "color:{};",
                                    if show_stars.get() { "var(--accent-cyan)" } else { "var(--text-muted)" }
                                )
                                on:click=move |_| show_stars.update(|v| *v = !*v)
                            >
                                {move || tr().focus_stars_toggle}
                            </button>
                        </Show>
                    </div>
                    <div class="border-t border-border-base p-sp-2 bg-bg-input-deep grid grid-rows-[1fr_auto] min-h-0 gap-1">
                        <canvas
                            node_ref=canvas_ref
                            class="block w-full h-full min-h-0"
                        ></canvas>
                        <div class="text-xs text-text-muted leading-tight line-clamp-2">
                            {move || {
                                let title = focus.with(|f| f.plot_title.clone());
                                if title.is_empty() { tr().focus_chart_caption.to_string() } else { title }
                            }}
                        </div>
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
