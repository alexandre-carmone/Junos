//! Framing Assistant — fullscreen overlay.
//!
//! Opened from the sky right-click menu with the clicked coordinate. Shows a
//! hips2fits survey cutout of the region (via `junos-server`'s
//! `/api/skysurvey` proxy — the browser can't call hips2fits directly under
//! CORS) with the camera's mosaic tile grid drawn over it, then hands the
//! whole plan off to the Mosaic planner tab.
//!
//! Two invariants worth knowing before editing:
//!
//! * **Epoch.** `FramingState::center` is of-date (JNow), matching both the
//!   right-click menu's coords and `MosaicPlannerState::center`. It is
//!   converted to J2000 only when building the hips2fits URL (`coordsys=icrs`)
//!   and from J2000 only when accepting a catalog search hit.
//! * **Geometry.** Tile layout comes from `super::derive_planner_mosaic_plan`
//!   — the same function that feeds the on-sky `MosaicLayer` and, through the
//!   planner, KStars. Camera/focal inputs are read live from the same signals
//!   the Mosaic tab uses, never overridden here, so what's previewed is what
//!   gets sent.

use std::cell::RefCell;
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::HtmlImageElement;

use crate::astro;
use crate::catalog::CatalogData;
use crate::compat::CameraSnapshot;
use crate::coords::{J2000, JNow};
use crate::dso_catalog::DsoCatalogData;
use crate::i18n::{t, Lang};
use crate::{ActiveTabCtx, MosaicPlannerCtx, Tab};

use super::object_search::search_objects;
use super::utils::event_target_value;

/// Signals for the Framing Assistant overlay (shared via App-level context).
#[derive(Clone, Copy)]
pub struct FramingState {
    pub open: RwSignal<bool>,
    /// (ra_deg, dec_deg), epoch-of-date (JNow) — see module docs.
    pub center: RwSignal<Option<(f64, f64)>>,
    pub target: RwSignal<String>,
    pub grid_w: RwSignal<u32>,
    pub grid_h: RwSignal<u32>,
    pub overlap: RwSignal<f64>,
    pub pa: RwSignal<f64>,
}

/// A loaded survey cutout, pinned to the sky position it was fetched for.
/// Tiles project against *this* center, not the live one, so dragging slides
/// the grid across a static image instead of refetching per pixel.
struct PinnedImage {
    img: HtmlImageElement,
    /// JNow center the cutout was fetched for.
    ra_deg: f64,
    dec_deg: f64,
    /// Angular span of the (square) cutout.
    fov_deg: f64,
}

const SIDEBAR_ROW: &str = "flex items-center justify-between gap-sp-2";
const LABEL: &str = "text-text-muted";
const NUM_INPUT: &str = "input input--sm font-mono w-[92px]";

/// Extra sky around the mosaic bounding box, applied to its diagonal (so any
/// position angle stays covered).
const PREVIEW_MARGIN: f64 = 1.3;
const PREVIEW_PX: u32 = 768;
/// Server clamps to 0.01..10; stay inside that.
const FOV_MIN_DEG: f64 = 0.02;
const FOV_MAX_DEG: f64 = 9.0;

fn now_jd() -> f64 {
    let now = js_sys::Date::new_0();
    astro::julian_date(
        now.get_utc_full_year() as i32,
        now.get_utc_month() + 1,
        now.get_utc_date(),
        now.get_utc_hours(),
        now.get_utc_minutes(),
        now.get_utc_seconds() as f64 + now.get_utc_milliseconds() as f64 / 1000.0,
    )
}

/// Angular span (arcmin) of the whole mosaic bounding box, before rotation.
/// Mirrors the offsets `derive_planner_mosaic_plan` computes.
fn mosaic_span_am(
    fov_w_deg: f64,
    fov_h_deg: f64,
    grid_w: u32,
    grid_h: u32,
    overlap_pct: f64,
) -> (f64, f64) {
    let fov_w_am = fov_w_deg * 60.0;
    let fov_h_am = fov_h_deg * 60.0;
    let x_off = fov_w_am * (1.0 - overlap_pct / 100.0);
    let y_off = fov_h_am * (1.0 - overlap_pct / 100.0);
    (
        fov_w_am + x_off * (grid_w as f64 - 1.0),
        fov_h_am + y_off * (grid_h as f64 - 1.0),
    )
}

#[component]
pub fn FramingOverlay(
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] focal_length_mm: Signal<Option<f64>>,
    catalog_sig: RwSignal<Option<std::sync::Arc<CatalogData>>>,
    dso_catalog_sig: RwSignal<Option<std::sync::Arc<DsoCatalogData>>>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let framing = use_context::<crate::FramingCtx>().expect("FramingCtx provided in main.rs").0;
    let planner_ctx = use_context::<MosaicPlannerCtx>();
    let tab_ctx = use_context::<ActiveTabCtx>();

    let search_text = RwSignal::new(String::new());
    let loading = RwSignal::new(false);
    let load_error = RwSignal::new(false);
    // Bumped after each fetch so the redraw Effect re-runs on a new image.
    let image_epoch = RwSignal::new(0u32);

    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let pinned: Rc<RefCell<Option<PinnedImage>>> = Rc::new(RefCell::new(None));

    // Derived FOV of a single tile, from the same live signals the Mosaic tab
    // uses. `None` gates both the preview and the send.
    let tile_fov = Signal::derive(move || {
        let cam = camera.get();
        let fl = focal_length_mm.get()?;
        let px = cam.pixel_size_um?;
        let sw = cam.sensor_width?;
        let sh = cam.sensor_height?;
        Some((
            astro::fov_deg(fl, sw as f64, px),
            astro::fov_deg(fl, sh as f64, px),
        ))
    });

    // ── Canvas draw ──────────────────────────────────────────────────────
    let redraw = {
        let pinned = Rc::clone(&pinned);
        move || {
            let Some(canvas) = canvas_ref.get_untracked() else { return };
            let el: web_sys::HtmlCanvasElement = canvas.into();
            let Some(parent) = el.parent_element() else { return };
            let w = parent.client_width() as f64;
            let h = parent.client_height() as f64;
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let dpr = web_sys::window()
                .map(|win| win.device_pixel_ratio().min(3.0))
                .unwrap_or(1.0);
            let w_phys = (w * dpr).round() as u32;
            let h_phys = (h * dpr).round() as u32;
            if el.width() != w_phys {
                el.set_width(w_phys);
            }
            if el.height() != h_phys {
                el.set_height(h_phys);
            }
            let Ok(Some(ctx)) = el.get_context("2d") else { return };
            let Ok(ctx) = ctx.dyn_into::<web_sys::CanvasRenderingContext2d>() else { return };
            let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
            ctx.set_fill_style_str("#05070d");
            ctx.fill_rect(0.0, 0.0, w, h);

            let cx = w / 2.0;
            let cy = h / 2.0;

            // The cutout is square; draw it "contain"-style so it never
            // distorts, and derive the sky scale from the drawn extent.
            let borrow = pinned.borrow();
            let Some(pin) = borrow.as_ref() else { return };
            let draw_size = w.min(h);
            let _ = ctx.draw_image_with_html_image_element_and_dw_and_dh(
                &pin.img,
                cx - draw_size / 2.0,
                cy - draw_size / 2.0,
                draw_size,
                draw_size,
            );
            let px_per_deg = draw_size / pin.fov_deg;

            let Some((fov_w_deg, fov_h_deg)) = tile_fov.get_untracked() else { return };
            let Some(plan) = super::derive_planner_mosaic_plan(
                true,
                framing.center.get_untracked(),
                focal_length_mm.get_untracked(),
                &camera.get_untracked(),
                framing.grid_w.get_untracked(),
                framing.grid_h.get_untracked(),
                framing.overlap.get_untracked(),
                framing.pa.get_untracked(),
                &framing.target.get_untracked(),
            ) else {
                return;
            };

            let cos_dec = pin.dec_deg.to_radians().cos();
            let rot = plan.pa_deg.to_radians();
            let rect_w = fov_w_deg * px_per_deg;
            let rect_h = fov_h_deg * px_per_deg;

            for (i, tile) in plan.tiles.iter().enumerate() {
                // Tangent-plane offset from the pinned image centre. hips2fits
                // TAN with no rotation is North-up/East-left, so +RA (east)
                // goes LEFT and +Dec (north) goes UP.
                let dx_deg = (tile.ra_deg - pin.ra_deg) * cos_dec;
                let dy_deg = tile.dec_deg - pin.dec_deg;
                let tx = cx - dx_deg * px_per_deg;
                let ty = cy - dy_deg * px_per_deg;

                ctx.save();
                let _ = ctx.translate(tx, ty);
                let _ = ctx.rotate(rot);
                ctx.set_stroke_style_str("rgba(80,180,255,0.95)");
                ctx.set_line_width(2.0);
                ctx.stroke_rect(-rect_w / 2.0, -rect_h / 2.0, rect_w, rect_h);
                if plan.tiles.len() > 1 {
                    ctx.set_fill_style_str("rgba(80,180,255,0.95)");
                    ctx.set_font("12px monospace");
                    let _ = ctx.fill_text(&(i + 1).to_string(), -rect_w / 2.0 + 4.0, -rect_h / 2.0 + 14.0);
                }
                ctx.restore();
            }
        }
    };
    let redraw = StoredValue::new_local(redraw);

    // Redraw on any state change (no refetch).
    Effect::new(move |_| {
        framing.open.track();
        framing.center.track();
        framing.grid_w.track();
        framing.grid_h.track();
        framing.overlap.track();
        framing.pa.track();
        image_epoch.track();
        tile_fov.track();
        // The canvas only exists once `open` is true and <Show> has mounted it.
        request_animation_frame(move || redraw.with_value(|f| f()));
    });

    // Keep the canvas correct across window resizes.
    {
        let cb = Closure::<dyn FnMut()>::new(move || redraw.with_value(|f| f()));
        if let Some(win) = web_sys::window() {
            let _ = win
                .add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        }
        cb.forget();
    }

    // ── Survey fetch ─────────────────────────────────────────────────────
    let load_preview = {
        let pinned = Rc::clone(&pinned);
        move || {
            let Some((ra_jnow, dec_jnow)) = framing.center.get_untracked() else { return };
            let Some((fov_w_deg, fov_h_deg)) = tile_fov.get_untracked() else { return };
            let (span_w_am, span_h_am) = mosaic_span_am(
                fov_w_deg,
                fov_h_deg,
                framing.grid_w.get_untracked(),
                framing.grid_h.get_untracked(),
                framing.overlap.get_untracked(),
            );
            // Use the bbox diagonal so any position angle stays covered.
            let diag_deg = (span_w_am * span_w_am + span_h_am * span_h_am).sqrt() / 60.0;
            let fov = (diag_deg * PREVIEW_MARGIN).clamp(FOV_MIN_DEG, FOV_MAX_DEG);

            // hips2fits is queried in ICRS/J2000; our centre is of-date.
            let j2000 = JNow::new(ra_jnow, dec_jnow).to_j2000(now_jd());

            loading.set(true);
            load_error.set(false);
            let url = format!(
                "/api/skysurvey?ra={}&dec={}&fov={}&w={PREVIEW_PX}&h={PREVIEW_PX}",
                j2000.ra_deg, j2000.dec_deg, fov
            );
            let Ok(img) = HtmlImageElement::new() else { return };
            let img_cl = img.clone();
            let pinned_cl = Rc::clone(&pinned);
            let onload = Closure::<dyn FnMut()>::new(move || {
                pinned_cl.borrow_mut().replace(PinnedImage {
                    img: img_cl.clone(),
                    ra_deg: ra_jnow,
                    dec_deg: dec_jnow,
                    fov_deg: fov,
                });
                loading.set(false);
                image_epoch.update(|v| *v += 1);
            });
            let onerror = Closure::<dyn FnMut()>::new(move || {
                loading.set(false);
                load_error.set(true);
            });
            img.set_onload(Some(onload.as_ref().unchecked_ref()));
            img.set_onerror(Some(onerror.as_ref().unchecked_ref()));
            onload.forget();
            onerror.forget();
            img.set_src(&url);
        }
    };
    let load_preview = StoredValue::new_local(load_preview);

    // Auto-load when the overlay opens (right-click hands us a centre).
    Effect::new(move |_| {
        if framing.open.get() {
            load_preview.with_value(|f| f());
        }
    });

    // ── Drag to move the centre ──────────────────────────────────────────
    let dragging = StoredValue::new(false);
    let drag_last = StoredValue::new((0.0_f64, 0.0_f64));
    let pinned_for_drag = Rc::clone(&pinned);
    let pinned_for_drag = StoredValue::new_local(pinned_for_drag);

    let on_mousedown = move |ev: web_sys::MouseEvent| {
        if ev.button() != 0 {
            return;
        }
        dragging.set_value(true);
        drag_last.set_value((ev.client_x() as f64, ev.client_y() as f64));
    };
    let on_mousemove = move |ev: web_sys::MouseEvent| {
        if !dragging.get_value() {
            return;
        }
        let (lx, ly) = drag_last.get_value();
        let dx = ev.client_x() as f64 - lx;
        let dy = ev.client_y() as f64 - ly;
        drag_last.set_value((ev.client_x() as f64, ev.client_y() as f64));
        let Some((ra, dec)) = framing.center.get_untracked() else { return };
        let px_per_deg = pinned_for_drag.with_value(|p| {
            let borrow = p.borrow();
            let pin = borrow.as_ref()?;
            let canvas = canvas_ref.get_untracked()?;
            let el: web_sys::HtmlCanvasElement = canvas.into();
            let parent = el.parent_element()?;
            let draw_size = (parent.client_width() as f64).min(parent.client_height() as f64);
            Some(draw_size / pin.fov_deg)
        });
        let Some(px_per_deg) = px_per_deg else { return };
        if px_per_deg <= 0.0 {
            return;
        }
        // Inverse of the draw projection: screen +x is west, +y is south.
        let cos_dec = dec.to_radians().cos().abs().max(0.01);
        let new_ra = ra - (dx / px_per_deg) / cos_dec;
        let new_dec = (dec - dy / px_per_deg).clamp(-89.9, 89.9);
        framing.center.set(Some((new_ra.rem_euclid(360.0), new_dec)));
    };
    let on_mouseup = move |_: web_sys::MouseEvent| dragging.set_value(false);

    // ── Actions ──────────────────────────────────────────────────────────
    let close = move || framing.open.set(false);

    let can_send = Signal::derive(move || {
        framing.center.get().is_some() && tile_fov.get().is_some()
    });

    let send_to_planner = move |_: web_sys::MouseEvent| {
        let Some(planner) = planner_ctx.map(|c| c.0) else { return };
        let Some(center) = framing.center.get_untracked() else { return };
        // Same epoch (JNow) on both sides — no conversion.
        planner.center.set(Some(center));
        planner.target.set(framing.target.get_untracked());
        planner.grid_w.set(framing.grid_w.get_untracked());
        planner.grid_h.set(framing.grid_h.get_untracked());
        planner.overlap.set(framing.overlap.get_untracked());
        planner.pa.set(framing.pa.get_untracked());
        planner.planning.set(true);
        framing.open.set(false);
        if let Some(ctx) = tab_ctx {
            ctx.0.set(Tab::Mosaic);
        }
    };

    view! {
        <Show when=move || framing.open.get()>
            <div
                class="fixed inset-0 z-[100] bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2"
                on:click=move |_| close()
            >
                <div
                    class="w-full max-w-[1100px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col"
                    on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                >
                    // ── Header ──────────────────────────────────────────
                    <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                        <h2 class="text-text-blue text-sm uppercase tracking-[0.08em] m-0">
                            {move || tr().framing_title}
                        </h2>
                        <button class="btn btn-ghost" on:click=move |_| close()>
                            {"\u{2716}"}
                        </button>
                    </div>

                    // ── Body: canvas + sidebar ──────────────────────────
                    <div class="flex-1 min-h-0 flex max-[759px]:flex-col">
                        <div class="flex-1 min-h-0 min-w-0 relative bg-black">
                            <canvas
                                node_ref=canvas_ref
                                class="absolute top-0 left-0 w-full h-full block cursor-move"
                                on:mousedown=on_mousedown
                                on:mousemove=on_mousemove
                                on:mouseup=on_mouseup
                                on:mouseleave=on_mouseup
                            />
                            {move || loading.get().then(|| view! {
                                <div class="absolute top-2 left-1/2 -translate-x-1/2 py-1 px-3 bg-bg-panel-glass border border-border-accent rounded-md font-mono text-xs text-text-blue pointer-events-none">
                                    {move || tr().framing_loading}
                                </div>
                            })}
                            {move || load_error.get().then(|| view! {
                                <div class="absolute top-2 left-1/2 -translate-x-1/2 py-1 px-3 bg-bg-panel-glass border rounded-md font-mono text-xs pointer-events-none" style="color:#f66;border-color:#f66">
                                    {move || tr().framing_load_error}
                                </div>
                            })}
                            {move || tile_fov.get().is_none().then(|| view! {
                                <div class="absolute inset-0 flex items-center justify-center p-sp-4 text-center font-mono text-xs text-text-muted pointer-events-none">
                                    {move || tr().framing_no_fov}
                                </div>
                            })}
                        </div>

                        <div class="w-[280px] max-[759px]:w-full shrink-0 border-l max-[759px]:border-l-0 max-[759px]:border-t border-border-base overflow-y-auto p-sp-3 flex flex-col gap-sp-2 text-[12px]">
                            // Catalog search
                            <input type="text"
                                class="input input--sm font-mono w-full"
                                placeholder=move || tr().search_placeholder
                                prop:value=move || search_text.get()
                                on:input=move |e| search_text.set(event_target_value(&e))
                            />
                            {move || {
                                let q = search_text.get();
                                if q.len() < 2 { return view! { <></> }.into_any(); }
                                let cat = catalog_sig.get();
                                let dso_cat = dso_catalog_sig.get();
                                let stars = cat.as_deref().map(|c| c.stars.as_slice()).unwrap_or(&[]);
                                let dsos  = dso_cat.as_deref().map(|d| d.dsos.as_slice()).unwrap_or(&[]);
                                let hits = search_objects(&q, stars, dsos, lang.get(), 12);
                                if hits.is_empty() { return view! { <></> }.into_any(); }
                                let rows = hits.into_iter().map(|hit| {
                                    let label = hit.name.clone();
                                    let (ra_j2000, dec_j2000) = (hit.ra_deg, hit.dec_deg);
                                    view! {
                                        <div
                                            class="py-[3px] px-sp-2 cursor-pointer text-text border-b border-[#1a1a2a] text-[12px]"
                                            on:click=move |_| {
                                                // Catalog is J2000; our centre is of-date.
                                                let jnow = J2000::new(ra_j2000, dec_j2000).to_jnow(now_jd());
                                                framing.center.set(Some((jnow.ra_deg, jnow.dec_deg)));
                                                framing.target.set(label.clone());
                                                search_text.set(String::new());
                                                load_preview.with_value(|f| f());
                                            }
                                        >
                                            {hit.name.clone()}
                                        </div>
                                    }
                                }).collect_view();
                                view! {
                                    <div class="bg-bg-panel-solid border border-border-accent max-h-[150px] overflow-y-auto rounded-[2px]">
                                        {rows}
                                    </div>
                                }.into_any()
                            }}

                            <div class=SIDEBAR_ROW>
                                <span class=LABEL>{move || tr().framing_target}</span>
                                <input type="text" class=NUM_INPUT
                                    prop:value=move || framing.target.get()
                                    on:input=move |e| framing.target.set(event_target_value(&e))
                                />
                            </div>

                            // Centre coords (of-date)
                            <div class=SIDEBAR_ROW>
                                <span class=LABEL>{move || tr().framing_ra}</span>
                                <input type="number" step="0.0001" class=NUM_INPUT
                                    prop:value=move || framing.center.get().map(|(ra, _)| format!("{:.4}", ra / 15.0)).unwrap_or_default()
                                    on:input=move |e| {
                                        if let Ok(h) = event_target_value(&e).parse::<f64>() {
                                            let dec = framing.center.get_untracked().map(|(_, d)| d).unwrap_or(0.0);
                                            framing.center.set(Some(((h * 15.0).rem_euclid(360.0), dec)));
                                        }
                                    }
                                />
                            </div>
                            <div class=SIDEBAR_ROW>
                                <span class=LABEL>{move || tr().framing_dec}</span>
                                <input type="number" step="0.0001" class=NUM_INPUT
                                    prop:value=move || framing.center.get().map(|(_, dec)| format!("{:.4}", dec)).unwrap_or_default()
                                    on:input=move |e| {
                                        if let Ok(d) = event_target_value(&e).parse::<f64>() {
                                            let ra = framing.center.get_untracked().map(|(r, _)| r).unwrap_or(0.0);
                                            framing.center.set(Some((ra, d.clamp(-89.9, 89.9))));
                                        }
                                    }
                                />
                            </div>

                            // Mosaic grid
                            <div class=SIDEBAR_ROW>
                                <span class=LABEL>{move || tr().framing_grid}</span>
                                <div class="flex gap-1 items-center">
                                    <input type="number" min="1" max="10" class="input input--sm font-mono w-[42px] text-center"
                                        prop:value=move || framing.grid_w.get().to_string()
                                        on:input=move |e| {
                                            if let Ok(n) = event_target_value(&e).parse::<u32>() {
                                                framing.grid_w.set(n.clamp(1, 10));
                                            }
                                        }
                                    />
                                    <span class="text-text-faint">{"\u{00d7}"}</span>
                                    <input type="number" min="1" max="10" class="input input--sm font-mono w-[42px] text-center"
                                        prop:value=move || framing.grid_h.get().to_string()
                                        on:input=move |e| {
                                            if let Ok(n) = event_target_value(&e).parse::<u32>() {
                                                framing.grid_h.set(n.clamp(1, 10));
                                            }
                                        }
                                    />
                                </div>
                            </div>
                            <div class=SIDEBAR_ROW>
                                <span class=LABEL>{move || tr().framing_overlap}</span>
                                <input type="number" min="0" max="50" class=NUM_INPUT
                                    prop:value=move || format!("{:.0}", framing.overlap.get())
                                    on:input=move |e| {
                                        if let Ok(n) = event_target_value(&e).parse::<f64>() {
                                            framing.overlap.set(n.clamp(0.0, 50.0));
                                        }
                                    }
                                />
                            </div>
                            <div class=SIDEBAR_ROW>
                                <span class=LABEL>{move || tr().framing_pa}</span>
                                <input type="number" step="1" class=NUM_INPUT
                                    prop:value=move || format!("{:.0}", framing.pa.get())
                                    on:input=move |e| {
                                        if let Ok(n) = event_target_value(&e).parse::<f64>() {
                                            framing.pa.set(n);
                                        }
                                    }
                                />
                            </div>

                            // Read-only derived field sizes
                            {move || tile_fov.get().map(|(fw, fh)| {
                                let (sw_am, sh_am) = mosaic_span_am(
                                    fw, fh,
                                    framing.grid_w.get(),
                                    framing.grid_h.get(),
                                    framing.overlap.get(),
                                );
                                view! {
                                    <div class="text-text-muted text-[11px] flex flex-col gap-[2px] pt-sp-1 border-t border-border-base">
                                        <div>{format!("{} {:.1}' \u{00d7} {:.1}'", tr().framing_tile_fov, fw * 60.0, fh * 60.0)}</div>
                                        <div>{format!("{} {:.1}' \u{00d7} {:.1}'", tr().framing_total_fov, sw_am, sh_am)}</div>
                                    </div>
                                }
                            })}

                            <button
                                class="btn btn--sm btn-ghost text-text-blue"
                                disabled=move || framing.center.get().is_none() || loading.get() || tile_fov.get().is_none()
                                on:click=move |_| load_preview.with_value(|f| f())
                            >
                                {move || tr().framing_reload}
                            </button>
                        </div>
                    </div>

                    // ── Footer ──────────────────────────────────────────
                    <div class="flex items-center justify-end gap-sp-2 py-sp-3 px-sp-4 border-t border-border-base bg-[rgba(10,12,20,0.8)]">
                        <button class="btn btn--sm btn-ghost" on:click=move |_| close()>
                            {move || tr().cancel}
                        </button>
                        <button
                            class="btn btn--sm btn-primary"
                            disabled=move || !can_send.get()
                            on:click=send_to_planner
                        >
                            {move || tr().framing_send_mosaic}
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
