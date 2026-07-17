//! Framing Assistant — fullscreen overlay.
//!
//! Opened from the sky right-click menu with the clicked coordinate. Shows a
//! survey preview of the region with the camera's mosaic tile grid drawn over
//! it, then hands the whole plan off to the Mosaic planner tab.
//!
//! The preview is built **entirely from the offline cache** (`crate::dso_tiles`
//! — pre-downloaded hips2fits tiles, one per catalog object). `load_preview`
//! collects every cached tile overlapping the selected zone and stamps them
//! onto one black offscreen canvas (`PinnedImage`), so an arbitrary zone —
//! wider than any single tile, or spanning several objects — renders as one
//! adapted image with uncovered sky left black. Nothing hits the network; a
//! zone with no cached coverage is simply the grid over black.
//!
//! Two invariants worth knowing before editing:
//!
//! * **Epoch.** `FramingState::center` is of-date (JNow), matching both the
//!   right-click menu's coords and `MosaicPlannerState::center`. It is
//!   converted to J2000 only to query the cache (tiles are ICRS/J2000) and
//!   from J2000 only when accepting a catalog search hit.
//! * **Geometry.** Tile layout comes from `super::derive_planner_mosaic_plan`
//!   — the same function that feeds the on-sky `MosaicLayer` and, through the
//!   planner, KStars. Camera/focal inputs are read live from the same signals
//!   the Mosaic tab uses, never overridden here, so what's previewed is what
//!   gets sent.

use std::cell::{Cell, RefCell};
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

/// The composited preview: an offscreen canvas holding every cached tile that
/// overlaps the zone, stamped onto black, pinned to the zone center it was
/// built for. The grid projects against *this* center, not the live one, so
/// dragging slides the grid across a static image instead of recompositing.
struct PinnedImage {
    canvas: web_sys::HtmlCanvasElement,
    /// JNow center the composite was built for (== the mosaic center).
    ra_deg: f64,
    dec_deg: f64,
    /// Angular span of the (square) composite.
    fov_deg: f64,
}

const SIDEBAR_ROW: &str = "flex items-center justify-between gap-sp-2";
const LABEL: &str = "text-text-muted";
const NUM_INPUT: &str = "input input--sm font-mono w-[92px]";

/// Extra sky around the mosaic bounding box, applied to its diagonal (so any
/// position angle stays covered).
const PREVIEW_MARGIN: f64 = 1.3;
/// Side of the offscreen composite the tiles are stamped onto. Independent of
/// the visible canvas (which is sized to its parent); this just sets how much
/// detail the composite retains when scaled down for display.
const COMPOSITE_PX: u32 = 1536;
/// Preview field bounds. No server clamp binds anymore (everything is served
/// from the local cache), so the ceiling only exists to keep the composite
/// pixel scale and tile count sane for very wide zones.
const FOV_MIN_DEG: f64 = 0.02;
const FOV_MAX_DEG: f64 = 40.0;

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

/// A square offscreen canvas and its 2D context, for compositing tiles before
/// they're blitted onto the visible preview canvas.
fn offscreen_canvas(px: u32) -> Option<(web_sys::HtmlCanvasElement, web_sys::CanvasRenderingContext2d)> {
    let doc = web_sys::window()?.document()?;
    let canvas: web_sys::HtmlCanvasElement =
        doc.create_element("canvas").ok()?.dyn_into().ok()?;
    canvas.set_width(px);
    canvas.set_height(px);
    let ctx = canvas
        .get_context("2d")
        .ok()??
        .dyn_into::<web_sys::CanvasRenderingContext2d>()
        .ok()?;
    Some((canvas, ctx))
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

    // Optional — an unpopulated tile cache leaves this `None` forever, in which
    // case every preview is just the grid over black.
    let tile_index = use_context::<crate::DsoTilesCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| RwSignal::new(None));

    let search_text = RwSignal::new(String::new());
    let loading = RwSignal::new(false);
    let load_error = RwSignal::new(false);
    // How many cached tiles the current composite is built from (0 = all black).
    let tiles_used = RwSignal::new(0usize);
    // Bumped after each composite so the redraw Effect re-runs on a new image.
    let image_epoch = RwSignal::new(0u32);
    // Bumped each time a compose is kicked off; a batch that finishes stale
    // (its generation superseded) is discarded instead of clobbering a newer
    // one. Guards rapid Reload / reopen.
    let load_gen = StoredValue::new(0u32);

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

            // The composite is square; draw it "contain"-style so it never
            // distorts, and derive the sky scale from the drawn extent.
            let borrow = pinned.borrow();
            let Some(pin) = borrow.as_ref() else { return };
            let draw_size = w.min(h);
            let _ = ctx.draw_image_with_html_canvas_element_and_dw_and_dh(
                &pin.canvas,
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

    // ── Compose preview from cached tiles ────────────────────────────────
    // Gather every cached tile overlapping the zone, stamp each onto one black
    // offscreen canvas at its tangent-plane offset, and pin that composite to
    // the zone centre. Uncovered sky stays black; nothing hits the network.
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

            // The cache is J2000/ICRS; the zone centre is of-date.
            let jd = now_jd();
            let j2000 = JNow::new(ra_jnow, dec_jnow).to_j2000(jd);
            let cos_dec = j2000.dec_deg.to_radians().cos();

            // Snapshot the overlapping tiles out of the Arc so the borrow isn't
            // held across the async image loads.
            let tiles: Vec<(String, f64, f64, f64)> = tile_index
                .get_untracked()
                .map(|idx| {
                    idx.find_overlapping(j2000.ra_deg, j2000.dec_deg, fov)
                        .into_iter()
                        .map(|t| (t.url(), t.ra, t.dec, t.fov))
                        .collect()
                })
                .unwrap_or_default();

            // New generation — a batch that finishes after this call was
            // superseded (e.g. rapid Reload) discards its result.
            let generation = load_gen.get_value().wrapping_add(1);
            load_gen.set_value(generation);

            loading.set(true);
            load_error.set(false);

            let Some((canvas, cctx)) = offscreen_canvas(COMPOSITE_PX) else {
                loading.set(false);
                load_error.set(true);
                return;
            };
            cctx.set_fill_style_str("#000");
            cctx.fill_rect(0.0, 0.0, COMPOSITE_PX as f64, COMPOSITE_PX as f64);

            let ppd = COMPOSITE_PX as f64 / fov;
            let mid = COMPOSITE_PX as f64 / 2.0;

            // Pin the finished composite and wake the redraw. Rc so both the
            // per-tile onload/onerror closures can call it.
            let finalize: Rc<dyn Fn(usize)> = {
                let pinned = Rc::clone(&pinned);
                Rc::new(move |n_used: usize| {
                    if load_gen.get_value() != generation {
                        return; // superseded by a newer load
                    }
                    pinned.borrow_mut().replace(PinnedImage {
                        canvas: canvas.clone(),
                        ra_deg: ra_jnow,
                        dec_deg: dec_jnow,
                        fov_deg: fov,
                    });
                    tiles_used.set(n_used);
                    loading.set(false);
                    image_epoch.update(|v| *v += 1);
                })
            };

            // Zero coverage → an all-black preview, immediately.
            if tiles.is_empty() {
                finalize(0);
                return;
            }

            // Each tile resolves (loaded or errored) independently; the last one
            // to settle finalizes with however many actually stamped.
            let pending = Rc::new(Cell::new(tiles.len()));
            let used = Rc::new(Cell::new(0usize));

            for (url, t_ra, t_dec, t_fov) in tiles {
                let Ok(img) = HtmlImageElement::new() else {
                    let left = pending.get() - 1;
                    pending.set(left);
                    if left == 0 {
                        finalize(used.get());
                    }
                    continue;
                };

                // Tangent-plane offset of the tile centre from the zone centre.
                // +RA (east) goes LEFT, +Dec (north) UP — same convention the
                // grid loop uses, so imagery and grid stay registered.
                let dx = (t_ra - j2000.ra_deg) * cos_dec;
                let dy = t_dec - j2000.dec_deg;
                let size = t_fov * ppd;
                let x = mid - dx * ppd - size / 2.0;
                let y = mid - dy * ppd - size / 2.0;

                let img_cl = img.clone();
                let cctx_cl = cctx.clone();
                let pending_l = Rc::clone(&pending);
                let used_l = Rc::clone(&used);
                let finalize_l = Rc::clone(&finalize);
                let onload = Closure::<dyn FnMut()>::new(move || {
                    let _ = cctx_cl.draw_image_with_html_image_element_and_dw_and_dh(
                        &img_cl, x, y, size, size,
                    );
                    used_l.set(used_l.get() + 1);
                    let left = pending_l.get() - 1;
                    pending_l.set(left);
                    if left == 0 {
                        finalize_l(used_l.get());
                    }
                });

                let pending_e = Rc::clone(&pending);
                let used_e = Rc::clone(&used);
                let finalize_e = Rc::clone(&finalize);
                let onerror = Closure::<dyn FnMut()>::new(move || {
                    // A missing tile just leaves its patch black.
                    let left = pending_e.get() - 1;
                    pending_e.set(left);
                    if left == 0 {
                        finalize_e(used_e.get());
                    }
                });

                img.set_onload(Some(onload.as_ref().unchecked_ref()));
                img.set_onerror(Some(onerror.as_ref().unchecked_ref()));
                onload.forget();
                onerror.forget();
                img.set_src(&url);
            }
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

                            // How many cached tiles the composite is built from
                            // — 0 means the zone isn't covered and the preview
                            // is all black.
                            {move || (!loading.get() && !load_error.get()).then(|| {
                                let n = tiles_used.get();
                                let label = if n == 0 {
                                    tr().framing_src_none.to_string()
                                } else {
                                    format!("{} \u{00b7} {}", tr().framing_src_cache, n)
                                };
                                view! { <div class="text-text-muted text-[11px]">{label}</div> }
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
