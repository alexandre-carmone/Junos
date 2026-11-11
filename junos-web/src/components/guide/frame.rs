//! The native Guide tab's frame: junos-guider's `+J` preview with its
//! overlay drawn on a canvas — the subframe, the other guide stars, the
//! search box round the primary (red while the lock is lost), the lock
//! cross, the lock-shift arrow and the settle ring.
//!
//! A tap sets the lock position on the star found near it
//! (`set_lock_position {x, y, exact:false}`, frame pixels); while
//! calibrating or guiding it takes a second tap to confirm. A double tap
//! opens the frame full screen (`zoom.rs`), with the overlay as a PNG built
//! only while it is open, and never in Lite mode.

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, MouseEvent};

use crate::components::zoom::frame_zoom;
use crate::i18n::{t, Lang};
use crate::ws::{GuiderFrame, GuiderStatusData};

use super::protocol::{Reply, Rpc, Settle, CALIBRATING, GUIDING, LOST_LOCK, PAUSED};

const CHIP: &str = "chip h-9 md:h-7 px-3 justify-center cursor-pointer";

/// What the frame pane reads and drives; each render of the pane (pinned
/// or in the scroll area) takes a clone.
#[derive(Clone)]
pub struct FramePane {
    pub frame: RwSignal<Option<GuiderFrame>>,
    pub state: RwSignal<Option<GuiderStatusData>>,
    pub settle: Settle,
    pub rpc: Rpc,
    /// The latest call, whose error the footer shows.
    pub last: RwSignal<Option<Reply>>,
    pub zoom_open: RwSignal<bool>,
    /// The shipped JPEG's size, from the `<img>` once loaded.
    pub natural: RwSignal<(f64, f64)>,
    /// A lock position waiting for its confirming tap: (x, y) in frame
    /// pixels, and when.
    pub pending: RwSignal<Option<(f64, f64, f64)>>,
    pub lang: RwSignal<Lang>,
}

/// The overlay's inputs beyond the frame itself.
struct Marks {
    lost: bool,
    /// Settling: within the settle distance yet?
    settling: Option<bool>,
    settle_px: f64,
    shift_label: Option<String>,
    pending: Option<(f64, f64)>,
}

impl FramePane {
    fn marks(&self) -> Marks {
        let settle_px = self.settle.pixels.get();
        let pending = self.pending.get().map(|(x, y, _)| (x, y));
        self.state.with(|s| {
            let s = s.as_ref();
            let shift = s.map(|s| &s.lock_shift).filter(|l| l.enabled);
            Marks {
                lost: s.and_then(|s| s.app_state.as_deref()) == Some(LOST_LOCK),
                settling: s.and_then(|s| s.settling.as_ref()).map(|st| st.distance <= settle_px),
                settle_px,
                shift_label: shift.map(|l| {
                    let unit = if l.units.starts_with("arcsec") { "\u{2033}/h" } else { " px/h" };
                    format!("{:.1}{unit}", l.rate[0].hypot(l.rate[1]))
                }),
                pending,
            }
        })
    }

    /// The overlay as a PNG the size of the JPEG, for the full-screen view.
    fn overlay_png(&self) -> Option<String> {
        let (nw, nh) = self.natural.get();
        let marks = self.marks();
        self.frame.with(|f| {
            let f = f.as_ref()?;
            let canvas: HtmlCanvasElement =
                web_sys::window()?.document()?.create_element("canvas").ok()?.dyn_into().ok()?;
            canvas.set_width(nw as u32);
            canvas.set_height(nh as u32);
            let ctx: CanvasRenderingContext2d = canvas.get_context("2d").ok()??.dyn_into().ok()?;
            draw(&ctx, f, &marks, 1.0, 0.0, 0.0, nw / f64::from(f.w.max(1)), (nw / 640.0).max(1.5));
            canvas.to_data_url().ok()
        })
    }
}

/// The overlay. `s`: target px per JPEG px, `(ox, oy)` where the JPEG's
/// origin lands; `f`: JPEG px per frame px (the lock, star and search box
/// come in JPEG px, the subframe and a pending lock in frame px); `lw`: the
/// line width the sizes scale with.
#[allow(clippy::too_many_arguments)]
fn draw(ctx: &CanvasRenderingContext2d, fr: &GuiderFrame, m: &Marks, s: f64, ox: f64, oy: f64, f: f64, lw: f64) {
    let at = |x: f64, y: f64| (ox + x * s, oy + y * s);
    let dash = |d: &[f64]| {
        let arr = web_sys::js_sys::Array::new();
        for v in d {
            arr.push(&(v * lw).into());
        }
        let _ = ctx.set_line_dash(&arr);
    };
    ctx.set_line_width(lw);

    if let Some([x, y, w, h]) = fr.subframe {
        let (px, py) = at(x * f, y * f);
        ctx.set_stroke_style_str("rgba(255,255,255,0.35)");
        dash(&[4.0, 4.0]);
        ctx.stroke_rect(px, py, w * f * s, h * f * s);
    }
    dash(&[]);
    for st in fr.stars.iter().filter(|st| !st.primary) {
        let (px, py) = at(st.x, st.y);
        ctx.set_stroke_style_str(if st.lost { "rgba(248,113,113,0.6)" } else { "rgba(245,185,66,0.85)" });
        ctx.begin_path();
        let _ = ctx.arc(px, py, 5.0 * lw, 0.0, std::f64::consts::TAU);
        ctx.stroke();
    }
    if let Some(st) = &fr.star {
        let r = fr.search_region * s;
        let (px, py) = at(st.x, st.y);
        ctx.set_stroke_style_str(if m.lost { "rgba(248,113,113,0.95)" } else { "rgba(120,235,255,0.95)" });
        if m.lost {
            dash(&[4.0, 3.0]);
        }
        ctx.stroke_rect(px - r, py - r, 2.0 * r, 2.0 * r);
        dash(&[]);
    }
    if let Some((lx, ly)) = fr.lock {
        let (px, py) = at(lx, ly);
        let arm = 12.0 * lw / 1.5;
        ctx.set_stroke_style_str("rgba(74,222,128,0.95)");
        ctx.begin_path();
        ctx.move_to(px - arm, py);
        ctx.line_to(px + arm, py);
        ctx.move_to(px, py - arm);
        ctx.line_to(px, py + arm);
        ctx.stroke();

        if let Some(within) = m.settling {
            ctx.set_stroke_style_str(if within { "rgba(74,222,128,0.8)" } else { "rgba(245,185,66,0.9)" });
            dash(&[3.0, 3.0]);
            ctx.begin_path();
            let _ = ctx.arc(px, py, (m.settle_px * f * s).max(arm), 0.0, std::f64::consts::TAU);
            ctx.stroke();
            dash(&[]);
        }
        // Where the lock drifts: ten minutes of the shift rate, clamped.
        if let (Some((dx, dy)), Some(label)) = (fr.lock_shift, &m.shift_label) {
            let rate = dx.hypot(dy);
            if rate > 0.0 {
                let len = (rate / 6.0 * s).clamp(16.0 * lw / 1.5, 60.0 * lw / 1.5);
                let (ux, uy) = (dx / rate, dy / rate);
                let (ex, ey) = (px + ux * len, py + uy * len);
                let head = 5.0 * lw / 1.5;
                ctx.set_stroke_style_str("rgba(232,121,249,0.95)");
                ctx.begin_path();
                ctx.move_to(px, py);
                ctx.line_to(ex, ey);
                ctx.move_to(ex - ux * head - uy * head, ey - uy * head + ux * head);
                ctx.line_to(ex, ey);
                ctx.line_to(ex - ux * head + uy * head, ey - uy * head - ux * head);
                ctx.stroke();
                ctx.set_font(&format!("{}px monospace", (11.0 * lw / 1.5).round()));
                ctx.set_fill_style_str("rgba(232,121,249,0.95)");
                let _ = ctx.fill_text(label, ex + 4.0, ey);
            }
        }
    }
    if let Some((x, y)) = m.pending {
        let (px, py) = at(x * f, y * f);
        ctx.set_stroke_style_str("rgba(120,235,255,0.95)");
        dash(&[2.0, 2.0]);
        ctx.begin_path();
        let _ = ctx.arc(px, py, 9.0 * lw / 1.5, 0.0, std::f64::consts::TAU);
        ctx.stroke();
        dash(&[]);
    }
}

/// The frame pane: pinned under the header on phones, or a card in the
/// scroll area; the frame cell of the grid from `md`. `swap` (phones,
/// pinned only) puts the drift plot in its place.
pub fn frame_pane(p: FramePane, pinned: bool, swap: impl Fn() + 'static) -> impl IntoView {
    let lang = p.lang;
    let tr = move || t(lang.get());
    let box_ref = NodeRef::<html::Div>::new();
    let canvas_ref = NodeRef::<html::Canvas>::new();
    let img_ref = NodeRef::<html::Img>::new();
    let laid_out = RwSignal::new(0u32);

    // Redraw on a new frame, a state change, a new layout.
    let draw_p = p.clone();
    Effect::new(move |_| {
        laid_out.track();
        let marks = draw_p.marks();
        let (Some(container), Some(canvas)) = (box_ref.get(), canvas_ref.get()) else { return };
        let (cw, ch) = (f64::from(container.client_width()), f64::from(container.client_height()));
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let dpr = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio()).clamp(1.0, 2.0);
        let (bw, bh) = ((cw * dpr).round() as u32, (ch * dpr).round() as u32);
        if canvas.width() != bw { canvas.set_width(bw); }
        if canvas.height() != bh { canvas.set_height(bh); }
        let Ok(Some(ctx)) = canvas.get_context("2d") else { return };
        let ctx: CanvasRenderingContext2d = ctx.unchecked_into();
        let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
        ctx.clear_rect(0.0, 0.0, cw, ch);
        let (nw, _) = draw_p.natural.get();
        let Some(img) = img_ref.get() else { return };
        let (iw, ih) = (f64::from(img.offset_width()), f64::from(img.offset_height()));
        if iw <= 0.0 || nw <= 0.0 {
            return;
        }
        draw_p.frame.with(|f| {
            if let Some(f) = f {
                let s = iw / nw;
                draw(&ctx, f, &marks, s, (cw - iw) / 2.0, (ch - ih) / 2.0, nw / f64::from(f.w.max(1)), 1.5);
            }
        });
    });
    {
        let cb = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            laid_out.try_update(|n| *n = n.wrapping_add(1));
        });
        if let Some(win) = web_sys::window() {
            let _ = win.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        }
        cb.forget();
    }

    // Tap: the lock (confirmed while guiding); double tap: full screen.
    let last_click = StoredValue::new(0.0_f64);
    let taps = StoredValue::new(0_u32);
    let tap_p = p.clone();
    let on_tap = move |x: f64, y: f64| {
        let now = web_sys::js_sys::Date::now();
        let busy = tap_p.state.with_untracked(|s| {
            matches!(s.as_ref().and_then(|s| s.app_state.as_deref()), Some(CALIBRATING | GUIDING | LOST_LOCK | PAUSED))
        });
        let confirmed = tap_p.pending.get_untracked().filter(|p| now - p.2 < 4000.0);
        if busy && confirmed.is_none() {
            tap_p.pending.set(Some((x, y, now)));
            let pending = tap_p.pending;
            gloo_timers::callback::Timeout::new(4000, move || {
                pending.try_update(|p| if p.is_some_and(|p| p.2 == now) { *p = None });
            })
            .forget();
            return;
        }
        let (x, y) = confirmed.map_or((x, y), |p| (p.0, p.1));
        tap_p.pending.set(None);
        tap_p.last.set(Some(tap_p.rpc.set_lock_position(x, y, false)));
    };
    let click_p = p.clone();
    let on_click = move |ev: MouseEvent| {
        let Some(img) = img_ref.get() else { return };
        let rect = img.get_bounding_client_rect();
        let (fw, fh) = click_p.frame.with_untracked(|f| f.as_ref().map_or((0, 0), |f| (f.w, f.h)));
        if rect.width() <= 0.0 || rect.height() <= 0.0 || fw == 0 {
            return;
        }
        let x = ((f64::from(ev.client_x()) - rect.left()) / rect.width()).clamp(0.0, 1.0) * f64::from(fw);
        let y = ((f64::from(ev.client_y()) - rect.top()) / rect.height()).clamp(0.0, 1.0) * f64::from(fh);
        let now = web_sys::js_sys::Date::now();
        let double = now - last_click.get_value() < 350.0;
        last_click.set_value(if double { 0.0 } else { now });
        taps.update_value(|n| *n = n.wrapping_add(1));
        if double {
            click_p.pending.set(None);
            click_p.zoom_open.set(true);
            return;
        }
        let mine = taps.get_value();
        let on_tap = on_tap.clone();
        gloo_timers::callback::Timeout::new(300, move || {
            if taps.try_get_value() == Some(mine) {
                on_tap(x, y);
            }
        })
        .forget();
    };

    let frame = p.frame;
    let natural = p.natural;
    let pending = p.pending;
    let tag = move || {
        frame.with(|f| {
            f.as_ref().map(|f| format!("#{} \u{00b7} {:.0} ms \u{00b7} {}\u{00d7}{}", f.frame, f.exposure_ms, f.binning, f.binning))
        })
    };
    let cls = if pinned {
        "relative shrink-0 h-[40dvh] min-h-[200px] border-b border-border-base"
    } else {
        "relative h-[260px] rounded-lg border border-border-base"
    };
    view! {
        <div node_ref=box_ref
             class=format!("{cls} overflow-hidden flex items-center justify-center bg-bg-input-deep \
                            md:h-auto md:min-h-0 md:col-start-1 md:row-start-1 md:border md:rounded-lg")>
            // One <img> for the whole run: a new frame only changes its src.
            <Show when=move || frame.with(Option::is_some)
                  fallback=move || view! {
                      <div class="text-text-faint text-sm text-center px-6">{move || tr().guider_no_frame}</div>
                  }>
                <img node_ref=img_ref alt="guide frame"
                     src=move || frame.with(|f| f.as_ref().map(|f| f.url.clone()).unwrap_or_default())
                     class="max-w-full max-h-full object-contain cursor-crosshair [image-rendering:pixelated]"
                     on:click=on_click.clone()
                     on:load=move |ev| {
                         let img: web_sys::HtmlImageElement = event_target(&ev);
                         natural.set((f64::from(img.natural_width()), f64::from(img.natural_height())));
                         laid_out.update(|n| *n = n.wrapping_add(1));
                     } />
            </Show>
            <canvas node_ref=canvas_ref class="absolute inset-0 w-full h-full pointer-events-none"></canvas>
            {move || tag().map(|tag| view! {
                <span class="absolute top-2 left-2 px-1.5 py-0.5 rounded bg-[rgba(0,0,0,0.55)] font-mono text-[11px] text-text-muted">
                    {tag}
                </span>
            })}
            <Show when=move || pending.with(Option::is_some)>
                <span class="absolute bottom-2 inset-x-2 text-center text-xs text-accent-cyan">
                    <span class="px-2 py-1 rounded bg-[rgba(0,0,0,0.6)]">{move || tr().guider_tap_again}</span>
                </span>
            </Show>
            {pinned.then(|| view! {
                <button class=format!("{CHIP} absolute top-2 right-2 md:hidden bg-glass-bg border-glass-border")
                        on:click=move |_| swap()>
                    {move || tr().guider_show_graph}
                </button>
            })}
        </div>
    }
}

fn event_target<T: JsCast>(ev: &web_sys::Event) -> T {
    ev.target().expect("event target").unchecked_into()
}

/// The full-screen frame, its overlay built while open (not in Lite).
pub fn zoom(p: FramePane) -> impl IntoView {
    let lite = use_context::<crate::LiteModeCtx>().map(|c| c.0);
    let (frame, open, lang) = (p.frame, p.zoom_open, p.lang);
    let url = Signal::derive(move || frame.with(|f| f.as_ref().map(|f| f.url.clone())));
    let overlay = Memo::new(move |_| {
        if !open.get() || lite.is_some_and(|l| l.get()) {
            return None;
        }
        p.overlay_png()
    });
    frame_zoom(url, overlay.into(), open, lang)
}
