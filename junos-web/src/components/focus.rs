//! Focus module UI — full-screen tab.
//!
//! Layout (phone-first): a header (status · settings), the focus frame, then
//! the controls and the HFR V-curve. On phones the frame stays pinned and the
//! rest scrolls beneath it; from `md` up the frame and the curve share the left
//! column and the controls get the right one. Settings open as a bottom sheet
//! on phones and a floating panel on md+.
//!
//! Talks to KStars via Ekos Live:
//!   - Inbound: `new_focus_state` (status/hfr/pos/log), `focus_get_all_settings`
//!     (debounced settings snapshot), `new_preview_image` with `uuid: "+F"`
//!     (focus frames). See `ws/store.rs::apply_ekos_event` for the match arms.
//!   - Outbound: `focus_start`, `focus_stop`, `focus_capture`, `focus_loop`,
//!     `focus_reset`, `focus_in{steps}`, `focus_out{steps}`,
//!     `focus_set_all_settings{…}`, `focus_set_crosshair{x,y}`.
//!     Command list: `kstars/kstars/ekos/ekoslive/commands.h`,
//!     handlers: `kstars/kstars/ekos/ekoslive/message.cpp:709-739`.

use leptos::prelude::*;
use leptos::html;
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{HtmlCanvasElement, CanvasRenderingContext2d, MouseEvent};

use crate::compat::FocusSnapshot;
use crate::components::tab_wheel_icons::tab_icon;
use crate::i18n::{Lang, Translations, t};
use crate::ws::SendCmd;
use crate::ws_helpers::{send_cmd, dispatch_setting};
use crate::Tab;

mod vcurve;
use crate::dom::{event_target_checked, event_target_value};

const CARD: &str = "panel p-3 flex flex-col gap-2";
const CARD_TITLE: &str = "text-xs uppercase tracking-[0.06em] font-semibold text-text-muted";
const CHIP: &str = "chip min-w-0 h-9 md:h-7 justify-center cursor-pointer font-mono";
const SHORT_HIDDEN: &str = "[@media(max-height:500px)]:hidden";

/// Manual-move step presets; the number input next to them takes any value.
const STEP_PRESETS: [i64; 4] = [10, 50, 100, 500];

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

// KStars sends the focus state untranslated (`getFocusStatusString(s, false)`,
// manager.cpp:3431): Idle, Complete, Failed, Aborted, User Input, In Progress,
// Framing, Changing Filter (ekos.h:117).

fn status_badge(status: &str) -> &'static str {
    match status {
        "Complete" => "badge badge--ok",
        "Failed" | "Aborted" => "badge badge--err",
        "In Progress" => "badge badge--info",
        "Framing" | "Changing Filter" | "User Input" => "badge badge--warn",
        _ => "badge",
    }
}

/// No autofocus run and no loop. Any other state — even one we don't know —
/// counts as busy, so the Stop button is always reachable during a run.
fn is_idle(status: &str) -> bool {
    matches!(status, "" | "Idle" | "Complete" | "Failed" | "Aborted")
}

/// Combo lists for keys that KStars exposes as `currentText` of a QComboBox
/// (`kstars/ekos/focus/opsfocusprocess.ui`); these render as a `<select>`.
const FOCUS_ALGORITHM_OPTS: &[&str] =
    &["Iterative", "Polynomial", "Linear", "Linear 1 Pass"];
const FOCUS_BINNING_OPTS: &[&str] = &["1x1", "2x2", "3x3", "4x4"];

fn enum_options_for(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "focusAlgorithm" => Some(FOCUS_ALGORITHM_OPTS),
        "focusBinning"   => Some(FOCUS_BINNING_OPTS),
        _ => None,
    }
}

/// The `focus_get_all_settings` keys the settings sheet shows, in order, with
/// their label. Other keys are ignored.
const SETTINGS: &[(&str, fn(&Translations) -> &'static str)] = &[
    ("focusExposure",        |t| t.focus_param_exposure),
    ("focusBinning",         |t| t.focus_param_binning),
    ("focusGain",            |t| t.gain),
    ("focusISO",             |t| t.focus_param_iso),
    ("focusIterations",      |t| t.focus_param_iterations),
    ("focusStepSize",        |t| t.focus_step_size),
    ("focusMaxStep",         |t| t.focus_param_max_step),
    ("focusMaxTravel",       |t| t.focus_param_max_travel),
    ("focusTolerance",       |t| t.focus_tolerance),
    ("focusBacklash",        |t| t.focus_backlash),
    ("focusAlgorithm",       |t| t.focus_algorithm),
    ("focusAutoStarEnabled", |t| t.focus_param_auto_star),
    ("focusSuspendGuiding",  |t| t.focus_param_suspend_guiding),
    ("focusUseFullField",    |t| t.focus_param_use_full_field),
];

#[component]
pub fn FocusTab(
    #[prop(into)] focus: Signal<FocusSnapshot>,
    #[prop(into)] send:  SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let step_size = RwSignal::new(100_i64);
    let settings_open = RwSignal::new(false);
    let status = Memo::new(move |_| focus.with(|f| f.status.clone()));
    // A Memo so the open settings sheet only re-renders when the settings
    // change, not on every HFR update (which would eat a half-typed value).
    let settings = Memo::new(move |_| focus.with(|f| f.settings.clone()));

    // Detected-stars overlay: on by default. `resize_tick` is bumped whenever
    // the preview box can change size (the window `resize` listener below and
    // `<img on:load>`), so both canvas draw Effects re-run. (There is no
    // ResizeObserver — `web-sys` isn't built with that feature here.)
    let show_stars = RwSignal::new(true);
    let resize_tick = RwSignal::new(0u32);

    // Escape closes the settings sheet. forget() the closure (one persistent
    // listener per FocusTab mount); calls into a disposed RwSignal are a no-op
    // in leptos 0.7, so leftover listeners after a tab switch are harmless.
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
    // One click handler per command; `focus_in` / `focus_out` carry the step.
    let cmd = {
        let send = send.clone();
        move |ty: &'static str| {
            let send = send.clone();
            move |_: MouseEvent| {
                let payload = match ty {
                    "focus_in" | "focus_out" => serde_json::json!({ "steps": step_size.get_untracked() }),
                    _ => serde_json::json!({}),
                };
                send_cmd(&send, ty, payload);
            }
        }
    };
    let (start, stop) = (cmd("focus_start"), cmd("focus_stop"));
    let on_autofocus = move |ev: MouseEvent| {
        if is_idle(&status.get_untracked()) { start(ev) } else { stop(ev) }
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
        let (y_min, y_max, y_step) = vcurve::nice_axis(lo - m, hi + m, y_target);
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
            let x_step = vcurve::nice_step((p_max - p_min) / 4.0);
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
        // Least-squares parabola, sampled at 20 points across the range like
        // KStars' focushfrvplot.cpp::drawPolynomial.
        // `a > 0` means it opens upward, i.e. it actually has a minimum.
        let mut vertex: Option<f64> = None;
        if pos_mode {
            let samples: Vec<vcurve::Sample> = history
                .iter()
                .filter_map(|s| s.position.map(|p| vcurve::Sample { pos: p as f64, hfr: s.hfr }))
                .collect();
            let distinct = {
                let mut v: Vec<i64> = samples.iter().map(|s| s.pos as i64).collect();
                v.sort_unstable();
                v.dedup();
                v.len()
            };
            if distinct >= 3 {
                if let Some((a, b, c)) =
                    vcurve::fit_parabola(&samples).filter(|&(a, _, _)| a > 0.0)
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

    let send_sv = StoredValue::new(send.clone());
    let dash = || "—".to_string();

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Focus)></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{move || tr().tab_focus}</span>
                <span class="min-w-0 truncate text-sm text-text-muted">{move || focus.with(|f| f.device.clone())}</span>
                <span class=move || format!("{} ml-auto shrink-0", status_badge(&status.get()))>
                    {move || {
                        let s = status.get();
                        if s.is_empty() { tr().idle.to_string() } else { s }
                    }}
                </span>
                <button class="btn-icon shrink-0 text-text-muted"
                        title=move || tr().focus_settings_section
                        on:click=move |_| settings_open.set(true)>
                    <span class="inline-block w-5 h-5" inner_html=tab_icon(Tab::Profiles)></span>
                </button>
            </div>

            // Body — a column on phones, a 2×2 grid on md+ (frame | controls
            // over curve | controls).
            <div class="flex-1 min-h-0 flex flex-col \
                        md:grid md:grid-cols-[minmax(0,1fr)_300px] lg:grid-cols-[minmax(0,1fr)_340px] \
                        md:grid-rows-[minmax(0,1fr)_minmax(120px,26dvh)] md:gap-3 md:p-3 md:pr-6">
                // Focus frame — pinned on phones.
                <div
                    node_ref=preview_box_ref
                    class="relative shrink-0 h-[40dvh] min-h-[200px] overflow-hidden flex items-center justify-center \
                           bg-bg-input-deep border-b border-border-base \
                           md:h-auto md:min-h-0 md:col-start-1 md:row-start-1 md:border md:rounded-lg"
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
                            <div class="text-text-faint text-sm text-center px-6">
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
                            class=move || format!("{CHIP} absolute top-2 right-2 font-ui {}",
                                if show_stars.get() { "btn--active" } else { "text-text-muted" })
                            aria-pressed=move || show_stars.get().to_string()
                            on:click=move |_| show_stars.update(|v| *v = !*v)
                        >
                            {move || tr().focus_stars_toggle}
                        </button>
                    </Show>
                </div>

                // Phones: the scroll area under the frame. md+: `contents`, so
                // the controls and the curve become grid cells of their own.
                <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] flex flex-col gap-3 \
                            p-3 pb-[max(0.75rem,env(safe-area-inset-bottom))] md:contents">
                    // Controls
                    <div class="flex flex-col gap-3 md:col-start-2 md:row-start-1 md:row-span-2 md:min-h-0 md:overflow-y-auto">
                        <div class="grid grid-cols-4 md:grid-cols-2 gap-2">
                            {stat_tile(move || tr().focus_hfr, "text-accent-cyan",
                                move || focus.with(|f| f.hfr.map(|v| format!("{v:.2}"))).unwrap_or_else(dash))}
                            {stat_tile(move || tr().focus_position, "text-text-dim",
                                move || focus.with(|f| f.position.map(|v| v.to_string())).unwrap_or_else(dash))}
                            {stat_tile(move || tr().focus_header_temperature, "text-text-dim",
                                move || focus.with(|f| f.temperature.map(|v| format!("{v:.1}°C"))).unwrap_or_else(dash))}
                            {stat_tile(move || tr().focus_stars, "text-text-dim",
                                move || focus.with(|f| f.stars.as_ref().map(|s| s.stars.len().to_string())).unwrap_or_else(dash))}
                        </div>

                        // Autofocus + framing
                        <div class=CARD>
                            <button
                                class=move || if is_idle(&status.get()) {
                                    "btn btn-primary w-full h-12 text-base"
                                } else {
                                    "btn btn-danger w-full h-12 text-base"
                                }
                                on:click=on_autofocus
                            >
                                {move || if is_idle(&status.get()) {
                                    format!("\u{25B6}\u{FE0E} {}", tr().focus_start)
                                } else {
                                    format!("\u{25A0} {}", tr().stop)
                                }}
                            </button>
                            <div class="grid grid-cols-3 gap-2">
                                <button class="btn px-1 leading-tight" on:click=cmd("focus_capture")>
                                    {move || tr().focus_capture_btn}
                                </button>
                                <button
                                    class=move || if status.get() == "Framing" {
                                        "btn px-1 leading-tight btn--active"
                                    } else {
                                        "btn px-1 leading-tight"
                                    }
                                    on:click=cmd("focus_loop")
                                >
                                    {move || tr().focus_loop_btn}
                                </button>
                                <button class="btn btn-ghost h-auto py-1 px-1 leading-tight" on:click=cmd("focus_reset")>
                                    {move || tr().focus_reset_frame}
                                </button>
                            </div>
                        </div>

                        // Manual focuser moves
                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().focus_manual_section}</span>
                            <div class="flex items-center gap-1.5">
                                <span class="text-sm text-text-muted">{move || tr().focus_step_label}</span>
                                {STEP_PRESETS.into_iter().map(|n| view! {
                                    <button
                                        class=move || if step_size.get() == n {
                                            format!("{CHIP} flex-1 btn--active")
                                        } else {
                                            format!("{CHIP} flex-1")
                                        }
                                        on:click=move |_| step_size.set(n)
                                    >
                                        {n.to_string()}
                                    </button>
                                }).collect::<Vec<_>>()}
                                <input
                                    type="number"
                                    min="1"
                                    inputmode="numeric"
                                    class="input input--sm font-mono w-[72px] shrink-0 max-md:h-9"
                                    prop:value=move || step_size.get().to_string()
                                    on:input=move |ev| {
                                        if let Ok(v) = event_target_value(&ev).trim().parse::<i64>() {
                                            step_size.set(v.max(1));
                                        }
                                    }
                                />
                            </div>
                            <div class="grid grid-cols-2 gap-2">
                                <button class="btn h-12 text-base" on:click=cmd("focus_in")>{move || tr().focus_in_btn}</button>
                                <button class="btn h-12 text-base" on:click=cmd("focus_out")>{move || tr().focus_out_btn}</button>
                            </div>
                        </div>
                    </div>

                    // HFR V-curve — under the frame on md+. Title and caption
                    // give way to the plot on short (landscape phone) screens.
                    <div class=format!("{CARD} h-[220px] md:h-auto md:min-h-0 md:col-start-1 md:row-start-2")>
                        <span class=format!("{CARD_TITLE} {SHORT_HIDDEN}")>{move || tr().focus_curve}</span>
                        <div class="relative flex-1 min-h-0">
                            <canvas node_ref=canvas_ref class="absolute inset-0 w-full h-full"></canvas>
                        </div>
                        <div class=format!("text-xs text-text-muted leading-tight line-clamp-2 {SHORT_HIDDEN}")>
                            {move || {
                                let title = focus.with(|f| f.plot_title.clone());
                                if title.is_empty() { tr().focus_chart_caption.to_string() } else { title }
                            }}
                        </div>
                    </div>
                </div>
            </div>

            // Settings — bottom sheet on phones, floating panel on md+.
            <Show when=move || settings_open.get()>
                <div class="absolute inset-0 z-[70] bg-[rgba(2,4,10,0.6)]" data-sheet=""
                     on:click=move |_| settings_open.set(false)></div>
                <div class="panel absolute z-[80] inset-x-0 bottom-0 max-h-[80dvh] rounded-b-none \
                            pb-[max(0.75rem,env(safe-area-inset-bottom))] \
                            md:inset-x-auto md:bottom-auto md:top-14 md:right-6 md:w-[380px] \
                            md:max-h-[calc(100%-4.5rem)] md:rounded-lg md:pb-3 \
                            overflow-y-auto [overscroll-behavior:contain] px-3 pt-2 flex flex-col gap-1 text-sm">
                    <div class="flex items-center justify-between">
                        <span class="font-semibold text-text-blue">{move || tr().focus_settings_section}</span>
                        <button class="btn-icon" title=move || tr().info_close
                                on:click=move |_| settings_open.set(false)>"\u{2716}"</button>
                    </div>
                    {move || {
                        let tr = tr();
                        let send = send_sv.get_value();
                        let rows: Vec<AnyView> = settings.with(|s| SETTINGS
                            .iter()
                            .filter_map(|&(key, label)| {
                                s.get(key).map(|v| setting_row(key, label(tr), v.clone(), send.clone()))
                            })
                            .collect());
                        if rows.is_empty() {
                            view! {
                                <div class="text-text-faint py-2">{tr.focus_settings_not_loaded}</div>
                            }.into_any()
                        } else {
                            rows.into_any()
                        }
                    }}
                </div>
            </Show>
        </div>
    }
}

/// One readout tile: a small uppercase label over a mono value.
fn stat_tile(
    label: impl Fn() -> &'static str + Send + 'static,
    value_cls: &'static str,
    value: impl Fn() -> String + Send + 'static,
) -> impl IntoView {
    view! {
        <div class="min-w-0 rounded-lg bg-bg-elev-1 border border-border-base px-2 py-1.5 flex flex-col">
            <span class="text-xs uppercase tracking-[0.06em] text-text-muted truncate">{move || label()}</span>
            <span class=format!("font-mono text-base md:text-lg leading-tight truncate {value_cls}")>
                {move || value()}
            </span>
        </div>
    }
}

/// One settings row: label left, control right. The control follows the
/// value's JSON type, except combo-box keys, which always get a `<select>`.
fn setting_row(key: &'static str, label: &'static str, val: serde_json::Value, send: SendCmd) -> AnyView {
    use serde_json::Value;
    const INPUT: &str = "input input--sm font-mono w-[150px] shrink-0 max-md:h-9";
    let set = move |v: Value| dispatch_setting(&send, "focus_set_all_settings", None, key, v);
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    let field = match (enum_options_for(key), &val) {
        (Some(opts), _) => {
            let current = text(&val);
            // Keep an unexpected current value selectable rather than drop it.
            let extra = (!current.is_empty() && !opts.contains(&current.as_str())).then(|| current.clone());
            let options = extra
                .into_iter()
                .chain(opts.iter().map(|o| o.to_string()))
                .map(|o| view! { <option value=o.clone() selected={o == current}>{o.clone()}</option> })
                .collect::<Vec<_>>();
            view! {
                <select class=INPUT on:change=move |ev| set(Value::String(event_target_value(&ev)))>
                    {options}
                </select>
            }.into_any()
        }
        (None, Value::Bool(on)) => view! {
            <input type="checkbox" class="w-5 h-5 min-h-0 shrink-0 accent-accent-cyan" checked=*on
                   on:change=move |ev| set(Value::Bool(event_target_checked(&ev))) />
        }.into_any(),
        (None, Value::Number(n)) => view! {
            <input type="number" step="any" class=INPUT value=n.to_string()
                   on:change=move |ev| {
                       let n = event_target_value(&ev).trim().parse::<f64>().ok()
                           .and_then(serde_json::Number::from_f64);
                       if let Some(n) = n { set(Value::Number(n)); }
                   } />
        }.into_any(),
        (None, other) => view! {
            <input type="text" class=INPUT value=text(other)
                   on:change=move |ev| set(Value::String(event_target_value(&ev))) />
        }.into_any(),
    };

    view! {
        <label class="flex items-center justify-between gap-3 min-h-[40px]">
            <span class="min-w-0 truncate text-text-blue" title=key>{label}</span>
            {field}
        </label>
    }.into_any()
}
