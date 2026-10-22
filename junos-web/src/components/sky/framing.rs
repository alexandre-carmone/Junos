//! Framing Assistant — fullscreen overlay.
//!
//! Opened from the sky right-click menu with the clicked coordinate. Shows a
//! survey preview of the region with the camera's mosaic tile grid drawn over
//! it, then hands the whole plan off to the Mosaic planner tab.
//!
//! The preview is the server's **sky map** (`crate::skymap`, a local DSS2
//! HiPS mirror) reprojected onto this tangent plane by a `SkyMapSampler`
//! (`skymap_sampler.rs`): every redraw renders the field at a bounded
//! internal size from the tiles already decoded and scales it onto the
//! canvas, with coarser parent tiles filling in while the finer ones load
//! (the sampler's own `TileImageCache`, epoch-bumping). Nothing hits the
//! internet; without a mirror the preview is the grid over black.
//!
//! The canvas is the primary control: drag the grid to move the mosaic, drag
//! the sky to pan the view, wheel / pinch to zoom about the pointer, twist two
//! fingers or drag the handle above the grid to rotate, double-tap to place
//! the grid. The sidebar adds a nudge pad with a step picker, rotation and
//! grid controls; the keyboard mirrors them (see `on_key`).
//!
//! Two invariants worth knowing before editing:
//!
//! * **Epoch.** `FramingState::params.center` and the view centre are of-date
//!   (JNow), matching both the right-click menu's coords and
//!   `MosaicPlannerState::params.center`. They are converted to J2000 only to
//!   render the sky-map cutout (ICRS) and from J2000 only when accepting a
//!   catalog search hit. The cutout is J2000-north-up while the grid is
//!   JNow-north-up; the difference is the precession rotation, far below a
//!   degree away from the pole.
//! * **Geometry.** Tile layout comes from `super::derive_planner_mosaic_plan`
//!   — the same function that feeds the on-sky `MosaicLayer` and, through the
//!   planner, KStars. Camera/focal inputs are read live from the same signals
//!   the Mosaic tab uses, never overridden here, so what's previewed is what
//!   gets sent.

use std::cell::RefCell;
use std::collections::HashMap;
use std::f64::consts::{PI, TAU};
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{PointerEvent, WheelEvent};

use crate::astro;
use crate::catalog::CatalogData;
use crate::compat::CameraSnapshot;
use crate::components::form::{slider_row, stepper, CARD_TITLE, CHIP};
use crate::coords::{J2000, JNow};
use crate::dso_catalog::DsoCatalogData;
use crate::i18n::{t, Lang};
use crate::{ActiveTabCtx, MosaicPlannerCtx, Tab};

use super::actions::{fmt_dec, fmt_ra};
use super::skymap_sampler::{CutoutSpec, SkyMapSampler};
use super::object_search::search_objects;
use crate::dom::event_target_value;

/// Signals for the Framing Assistant overlay (shared via App-level context).
/// `params.center` is epoch-of-date (JNow) — see module docs.
#[derive(Clone, Copy)]
pub struct FramingState {
    pub open: RwSignal<bool>,
    pub params: super::MosaicParams,
}

const SIDEBAR_ROW: &str = "flex items-center justify-between gap-sp-2";
const LABEL: &str = "text-text-muted";
const NUM_INPUT: &str = "input input--sm font-mono w-[92px]";
const PAD_BTN: &str = "btn-icon btn-icon--lg select-none [touch-action:none]";

/// Extra sky around the mosaic bounding box when fitting the view, applied to
/// its diagonal (so any position angle stays in frame).
const PREVIEW_MARGIN: f64 = 1.3;
/// View bounds, degrees across the shorter canvas side. The ceiling only
/// exists to keep the tile count sane for very wide zones.
const FOV_MIN_DEG: f64 = 0.02;
const FOV_MAX_DEG: f64 = 40.0;
/// Field shown while no camera gives a tile size yet.
const DEFAULT_FOV_DEG: f64 = 2.0;
/// Decoded tiles and extracted pixel buffers (1 MiB each) kept at most.
const CACHE_CAP: usize = 64;
const CACHE_CAP_MOBILE: usize = 32;
const PX_CAP: usize = 32;
const PX_CAP_MOBILE: usize = 16;
/// Internal render size of the preview (pixels), ~100 ns each; the result is
/// scaled onto the canvas.
const PREVIEW_PX: f64 = 500_000.0;
const PREVIEW_PX_MOBILE: f64 = 250_000.0;
/// Pointer travel (CSS px) below which a press is a tap, not a drag.
const TAP_SLOP: f64 = 8.0;
/// Second tap within this window and radius places the mosaic.
const DOUBLE_TAP_MS: f64 = 350.0;
const DOUBLE_TAP_PX: f64 = 24.0;
/// Rotation handle: stalk length above the bounding box and knob radius.
const HANDLE_STALK: f64 = 28.0;
const HANDLE_R: f64 = 7.0;
/// Wheel travel per degree of Shift+wheel rotation.
const WHEEL_PER_DEG: f64 = 30.0;

/// Angular span (arcmin) of the whole mosaic bounding box, before rotation.
/// Mirrors the offsets `derive_planner_mosaic_plan` computes.
pub(crate) fn mosaic_span_am(
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

/// Difference of two RA-like angles folded into (−180, 180].
fn wrap180(d: f64) -> f64 {
    let w = (d + 180.0).rem_euclid(360.0) - 180.0;
    if w == -180.0 { 180.0 } else { w }
}

/// Position angle folded into (−180, 180].
fn norm_pa(pa: f64) -> f64 {
    wrap180(pa)
}

/// The preview's projection: a tangent plane (TAN) about the view centre,
/// north up and east left, `ppd` pixels per degree — the same gnomonic map
/// the sky-map cutout is rendered with (`astro::frame_point_eq`), so the
/// grid sits on the same stars at the field's edges. Built once per draw /
/// per event.
#[derive(Clone, Copy)]
struct Viewport {
    cx: f64,
    cy: f64,
    ppd: f64,
    ra: f64,
    dec: f64,
}

impl Viewport {
    fn new(w: f64, h: f64, (ra, dec): (f64, f64), fov_deg: f64) -> Self {
        Self { cx: w / 2.0, cy: h / 2.0, ppd: w.min(h) / fov_deg, ra, dec }
    }

    /// +RA (east) goes LEFT, +Dec (north) goes UP. A point off the tangent
    /// plane (90° away or more) lands far off screen.
    fn sky_to_screen(&self, ra: f64, dec: f64) -> (f64, f64) {
        match astro::frame_offset_of(self.ra, self.dec, 0.0, ra, dec) {
            Some((right, up)) => (self.cx + right * self.ppd, self.cy - up * self.ppd),
            None => (self.cx - 1e6, self.cy - 1e6),
        }
    }

    fn screen_to_sky(&self, sx: f64, sy: f64) -> (f64, f64) {
        let (ra, dec) =
            astro::frame_point_eq(self.ra, self.dec, 0.0, (sx - self.cx) / self.ppd, (self.cy - sy) / self.ppd);
        (ra, dec.clamp(-89.9, 89.9))
    }
}

/// The mosaic on screen: centre, half-extents of its bounding box, screen
/// rotation and the rotation handle's knob.
#[derive(Clone, Copy)]
struct MosaicGeom {
    mx: f64,
    my: f64,
    hw: f64,
    hh: f64,
    /// Screen rotation (radians, y down) = −PA.
    rot: f64,
    hx: f64,
    hy: f64,
}

fn mosaic_geom(
    vp: &Viewport,
    center: (f64, f64),
    tile_fov: (f64, f64),
    grid_w: u32,
    grid_h: u32,
    overlap: f64,
    pa: f64,
) -> MosaicGeom {
    let (mx, my) = vp.sky_to_screen(center.0, center.1);
    let (sw_am, sh_am) = mosaic_span_am(tile_fov.0, tile_fov.1, grid_w, grid_h, overlap);
    let hw = sw_am / 60.0 * vp.ppd / 2.0;
    let hh = sh_am / 60.0 * vp.ppd / 2.0;
    let rot = -pa.to_radians();
    // The knob sits at local (0, −d), rotated by `rot`.
    let d = hh + HANDLE_STALK;
    MosaicGeom { mx, my, hw, hh, rot, hx: mx + d * rot.sin(), hy: my - d * rot.cos() }
}

/// What a press on the canvas takes hold of.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Grab {
    #[default]
    None,
    Sky,
    Mosaic,
    Handle,
    /// Two pointers: pinch / twist / pan, no single-pointer mode.
    Pinch,
}

fn grab_at(g: Option<MosaicGeom>, x: f64, y: f64, touch: bool) -> Grab {
    let Some(g) = g else { return Grab::Sky };
    let knob_r = if touch { 24.0 } else { 16.0 };
    if (x - g.hx).hypot(y - g.hy) <= knob_r {
        return Grab::Handle;
    }
    let (dx, dy) = (x - g.mx, y - g.my);
    let (s, c) = g.rot.sin_cos();
    let lx = c * dx + s * dy;
    let ly = -s * dx + c * dy;
    if lx.abs() <= g.hw + 6.0 && ly.abs() <= g.hh + 6.0 { Grab::Mosaic } else { Grab::Sky }
}

/// A `btn-icon` that fires once on press, then repeats while held.
fn hold_button(
    glyph: &'static str,
    title: impl Fn() -> String + Send + 'static,
    on_step: impl Fn() + Clone + 'static,
) -> impl IntoView {
    let delay = StoredValue::new_local(Rc::new(RefCell::new(None::<gloo_timers::callback::Timeout>)));
    let repeat = StoredValue::new_local(Rc::new(RefCell::new(None::<gloo_timers::callback::Interval>)));
    let stop = move || {
        delay.with_value(|d| *d.borrow_mut() = None);
        repeat.with_value(|r| *r.borrow_mut() = None);
    };
    let start = move |ev: PointerEvent| {
        if ev.button() != 0 {
            return;
        }
        ev.prevent_default();
        stop();
        on_step();
        let step = on_step.clone();
        let timeout = gloo_timers::callback::Timeout::new(400, move || {
            let step = step.clone();
            let interval = gloo_timers::callback::Interval::new(120, move || step());
            repeat.with_value(|r| *r.borrow_mut() = Some(interval));
        });
        delay.with_value(|d| *d.borrow_mut() = Some(timeout));
    };
    on_cleanup(stop);
    view! {
        <button type="button" class=PAD_BTN title=move || title()
                on:pointerdown=start
                on:pointerup=move |_| stop()
                on:pointerleave=move |_| stop()
                on:pointercancel=move |_| stop()
                on:contextmenu=|ev: web_sys::MouseEvent| ev.prevent_default()>
            {glyph}
        </button>
    }
}

/// One choice of a step picker.
fn step_chip(label: &'static str, value: f64, sel: RwSignal<f64>) -> impl IntoView {
    view! {
        <button type="button"
                class=move || if sel.get() == value { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                on:click=move |_| sel.set(value)>
            {label}
        </button>
    }
}

#[component]
pub fn FramingOverlay(
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] focal_length_mm: Signal<Option<f64>>,
    catalog_sig: RwSignal<Option<std::sync::Arc<CatalogData>>>,
    dso_catalog_sig: RwSignal<Option<std::sync::Arc<DsoCatalogData>>>,
    #[prop(optional)] is_mobile: bool,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let framing = use_context::<crate::FramingCtx>().expect("FramingCtx provided in main.rs").0;
    let planner_ctx = use_context::<MosaicPlannerCtx>();
    let tab_ctx = use_context::<ActiveTabCtx>();

    // Optional — a server without a sky map leaves this `None` forever, in
    // which case every preview is just the grid over black.
    let skymap = use_context::<crate::SkyMapCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| RwSignal::new(None));
    let lite = use_context::<crate::LiteModeCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| RwSignal::new(false));

    let search_text = RwSignal::new(String::new());
    // Tiles in flight / drawn and the order rendered, all set from the draw.
    let loading = RwSignal::new(false);
    let tiles_used = RwSignal::new(0usize);
    let order_used = RwSignal::new(0u8);
    // Bumped by the sampler's cache when a tile lands, so the draw reruns.
    let tiles_epoch = RwSignal::new(0u32);
    // Created on the first draw once the server has described its map.
    let sampler: Rc<RefCell<Option<SkyMapSampler>>> = Rc::new(RefCell::new(None));
    let sampler = StoredValue::new_local(sampler);

    // The view: where the preview looks (JNow) and how wide, independent of
    // the mosaic centre so the sky can be panned under a fixed grid.
    let view_center = RwSignal::new((0.0_f64, 0.0_f64));
    let view_fov = RwSignal::new(DEFAULT_FOV_DEG);

    // Fine-adjust steps: nudge (arcmin) and rotation (degrees).
    let nudge_step_am = RwSignal::new(10.0_f64);
    let rot_step = RwSignal::new(1.0_f64);

    // Pointer state for the cursor only.
    let hover = RwSignal::new(Grab::None);
    let dragging = RwSignal::new(false);

    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();

    // Derived FOV of a single tile, from the same live signals the Mosaic tab
    // uses. `None` gates both the grid and the send.
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

    // ── Geometry helpers (all untracked) ─────────────────────────────────
    let canvas_size = move || -> Option<(f64, f64)> {
        let canvas = canvas_ref.get_untracked()?;
        let el: web_sys::HtmlCanvasElement = canvas.into();
        let parent = el.parent_element()?;
        let (w, h) = (parent.client_width() as f64, parent.client_height() as f64);
        (w > 0.0 && h > 0.0).then_some((w, h))
    };
    let viewport = move || -> Option<Viewport> {
        let (w, h) = canvas_size()?;
        Some(Viewport::new(w, h, view_center.get_untracked(), view_fov.get_untracked()))
    };
    let geometry = move |vp: &Viewport| -> Option<MosaicGeom> {
        Some(mosaic_geom(
            vp,
            framing.params.center.get_untracked()?,
            tile_fov.get_untracked()?,
            framing.params.grid_w.get_untracked(),
            framing.params.grid_h.get_untracked(),
            framing.params.overlap.get_untracked(),
            framing.params.pa.get_untracked(),
        ))
    };
    // Client → canvas CSS px.
    let to_canvas = move |client_x: f64, client_y: f64| -> Option<(f64, f64)> {
        let canvas = canvas_ref.get_untracked()?;
        let el: web_sys::HtmlCanvasElement = canvas.into();
        let r = el.get_bounding_client_rect();
        Some((client_x - r.left(), client_y - r.top()))
    };

    // The field that shows the whole mosaic with a margin.
    let fit_fov = move || -> f64 {
        let Some((fw, fh)) = tile_fov.get_untracked() else { return DEFAULT_FOV_DEG };
        let (sw_am, sh_am) = mosaic_span_am(
            fw,
            fh,
            framing.params.grid_w.get_untracked(),
            framing.params.grid_h.get_untracked(),
            framing.params.overlap.get_untracked(),
        );
        let diag_deg = sw_am.hypot(sh_am) / 60.0;
        (diag_deg * PREVIEW_MARGIN).clamp(FOV_MIN_DEG, FOV_MAX_DEG)
    };
    let fit_view = move || {
        if let Some(c) = framing.params.center.get_untracked() {
            view_center.set(c);
        }
        view_fov.set(fit_fov());
    };

    // ── Edits ────────────────────────────────────────────────────────────
    let set_pa = move |pa: f64| framing.params.pa.set(norm_pa(pa));
    let rotate_by = move |d: f64| set_pa(framing.params.pa.get_untracked() + d);
    // Move the mosaic by (east, north) arcmin.
    let nudge = move |east_am: f64, north_am: f64| {
        let Some((ra, dec)) = framing.params.center.get_untracked() else { return };
        let cos_dec = dec.to_radians().cos().abs().max(0.01);
        framing.params.center.set(Some((
            (ra + east_am / 60.0 / cos_dec).rem_euclid(360.0),
            (dec + north_am / 60.0).clamp(-89.9, 89.9),
        )));
    };
    // Pan the view by (east, north) degrees.
    let pan_view = move |east_deg: f64, north_deg: f64| {
        view_center.update(|(ra, dec)| {
            *ra = (*ra + east_deg).rem_euclid(360.0);
            *dec = (*dec + north_deg).clamp(-89.9, 89.9);
        });
    };
    // Scale the field by `f` keeping the sky under canvas point (x, y) still.
    let zoom_about = move |f: f64, x: f64, y: f64| {
        let Some(vp) = viewport() else { return };
        let (pra, pdec) = vp.screen_to_sky(x, y);
        let fov = (view_fov.get_untracked() * f).clamp(FOV_MIN_DEG, FOV_MAX_DEG);
        view_fov.set(fov);
        let Some((w, h)) = canvas_size() else { return };
        let vp2 = Viewport::new(w, h, (vp.ra, vp.dec), fov);
        let (qra, qdec) = vp2.screen_to_sky(x, y);
        pan_view(wrap180(pra - qra), pdec - qdec);
    };
    let zoom_center = move |f: f64| {
        let Some((w, h)) = canvas_size() else { return };
        zoom_about(f, w / 2.0, h / 2.0);
    };

    // ── Canvas draw ──────────────────────────────────────────────────────
    let redraw = move || {
        let Some(canvas) = canvas_ref.get_untracked() else { return };
        let el: web_sys::HtmlCanvasElement = canvas.into();
        let Some((w, h)) = canvas_size() else { return };
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

        let vc = view_center.get_untracked();
        let fov = view_fov.get_untracked();
        let vp = Viewport::new(w, h, vc, fov);

        // ── Imagery: the sky map reprojected onto this tangent plane ──
        let jd = astro::now_jd();
        let j2000 = JNow::new(vc.0, vc.1).to_j2000(jd);
        let mut drawn = 0usize;
        let mut in_flight = 0usize;
        let mut order = 0u8;
        if let Some(info) = skymap.get_untracked() {
            sampler.with_value(|s| {
                let Ok(mut s) = s.try_borrow_mut() else { return };
                if s.is_none() {
                    *s = SkyMapSampler::new(
                        tiles_epoch,
                        info,
                        lite.get_untracked(),
                        if is_mobile { CACHE_CAP_MOBILE } else { CACHE_CAP },
                        if is_mobile { PX_CAP_MOBILE } else { PX_CAP },
                    );
                }
                let Some(s) = s.as_mut() else { return };
                // Bounded internal size, scaled up onto the canvas.
                let budget = if is_mobile { PREVIEW_PX_MOBILE } else { PREVIEW_PX };
                let k = (budget / (f64::from(w_phys) * f64::from(h_phys))).sqrt().min(1.0);
                let spec = CutoutSpec {
                    ra_deg: j2000.ra_deg,
                    dec_deg: j2000.dec_deg,
                    fov_deg: fov,
                    width: ((f64::from(w_phys) * k).round() as u32).max(1),
                    height: ((f64::from(h_phys) * k).round() as u32).max(1),
                    rot_deg: 0.0,
                };
                if let Some(st) = s.render_into(&ctx, &spec, 0.0, 0.0, w, h) {
                    drawn = st.tiles_drawn;
                    in_flight = st.loading;
                    order = st.order;
                }
            });
        }
        if tiles_used.get_untracked() != drawn {
            tiles_used.set(drawn);
        }
        if order_used.get_untracked() != order {
            order_used.set(order);
        }
        if loading.get_untracked() != (in_flight > 0) {
            loading.set(in_flight > 0);
        }

        // ── Compass, field read-out, scale bar ──
        ctx.set_fill_style_str("rgba(200,215,235,0.85)");
        ctx.set_font("11px monospace");
        let _ = ctx.fill_text("N \u{2191}  E \u{2190}", 10.0, 16.0);
        let _ = ctx.fill_text(&format!("{} {}", t(lang.get_untracked()).framing_field, fmt_field(fov)), 10.0, 30.0);
        {
            let want_px = w / 5.0;
            let bar_deg = [1.0 / 60.0, 5.0 / 60.0, 10.0 / 60.0, 0.5, 1.0, 5.0]
                .into_iter()
                .min_by(|a, b| (a * vp.ppd - want_px).abs().total_cmp(&(b * vp.ppd - want_px).abs()))
                .unwrap_or(1.0);
            let bar_px = bar_deg * vp.ppd;
            let y = h - 14.0;
            ctx.set_stroke_style_str("rgba(200,215,235,0.85)");
            ctx.set_line_width(1.5);
            ctx.begin_path();
            ctx.move_to(10.0, y);
            ctx.line_to(10.0 + bar_px, y);
            ctx.move_to(10.0, y - 4.0);
            ctx.line_to(10.0, y + 4.0);
            ctx.move_to(10.0 + bar_px, y - 4.0);
            ctx.line_to(10.0 + bar_px, y + 4.0);
            ctx.stroke();
            let _ = ctx.fill_text(&fmt_field(bar_deg), 10.0, y - 7.0);
        }

        // ── Mosaic grid ──
        let Some((fov_w_deg, fov_h_deg)) = tile_fov.get_untracked() else { return };
        let Some(center) = framing.params.center.get_untracked() else { return };
        let Some(plan) = super::derive_planner_mosaic_plan(
            true,
            Some(center),
            focal_length_mm.get_untracked(),
            &camera.get_untracked(),
            framing.params.grid_w.get_untracked(),
            framing.params.grid_h.get_untracked(),
            framing.params.overlap.get_untracked(),
            framing.params.pa.get_untracked(),
            &framing.params.target.get_untracked(),
        ) else {
            return;
        };

        // `derive_planner_mosaic_plan` already rotated the tile *positions*
        // around the mosaic centre (KStars rotatePoint, ang = -PA). To keep
        // the mosaic a rigid body — the whole grid rotating together rather
        // than each rectangle spinning in place — each rectangle must rotate
        // the same way its centre moved, which in this east-left/north-up
        // screen frame is -PA.
        let rot = -plan.pa_deg.to_radians();
        let rect_w = fov_w_deg * vp.ppd;
        let rect_h = fov_h_deg * vp.ppd;

        for (i, tile) in plan.tiles.iter().enumerate() {
            let (tx, ty) = vp.sky_to_screen(tile.ra_deg, tile.dec_deg);
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

        // ── Bounding box + rotation handle ──
        let g = mosaic_geom(
            &vp,
            center,
            (fov_w_deg, fov_h_deg),
            framing.params.grid_w.get_untracked(),
            framing.params.grid_h.get_untracked(),
            framing.params.overlap.get_untracked(),
            framing.params.pa.get_untracked(),
        );
        ctx.save();
        let _ = ctx.translate(g.mx, g.my);
        let _ = ctx.rotate(g.rot);
        ctx.set_stroke_style_str("rgba(80,180,255,0.45)");
        ctx.set_line_width(1.0);
        let _ = ctx.set_line_dash(&js_sys::Array::of2(&4.0.into(), &4.0.into()));
        ctx.stroke_rect(-g.hw, -g.hh, g.hw * 2.0, g.hh * 2.0);
        let _ = ctx.set_line_dash(&js_sys::Array::new());
        ctx.set_stroke_style_str("rgba(80,180,255,0.9)");
        ctx.set_line_width(1.5);
        ctx.begin_path();
        ctx.move_to(0.0, -g.hh);
        ctx.line_to(0.0, -g.hh - HANDLE_STALK);
        ctx.stroke();
        ctx.restore();
        let knob_hot = matches!(hover.get_untracked(), Grab::Handle);
        ctx.begin_path();
        let _ = ctx.arc(g.hx, g.hy, if knob_hot { HANDLE_R + 2.0 } else { HANDLE_R }, 0.0, TAU);
        ctx.set_fill_style_str(if knob_hot { "rgba(80,180,255,1)" } else { "rgba(10,20,40,0.9)" });
        ctx.fill();
        ctx.set_stroke_style_str("rgba(80,180,255,0.95)");
        ctx.set_line_width(2.0);
        ctx.stroke();
        ctx.set_fill_style_str("rgba(80,180,255,0.95)");
        ctx.set_font("12px monospace");
        let _ = ctx.fill_text(&format!("PA {:.0}\u{00b0}", plan.pa_deg), g.hx + HANDLE_R + 6.0, g.hy + 4.0);
    };
    let redraw = StoredValue::new_local(redraw);

    // Redraw on any state change, at most once per animation frame.
    let redraw_pending = StoredValue::new(false);
    let schedule_redraw = move || {
        if redraw_pending.get_value() {
            return;
        }
        redraw_pending.set_value(true);
        request_animation_frame(move || {
            redraw_pending.set_value(false);
            redraw.with_value(|f| f());
        });
    };
    Effect::new(move |_| {
        framing.open.track();
        framing.params.center.track();
        framing.params.grid_w.track();
        framing.params.grid_h.track();
        framing.params.overlap.track();
        framing.params.pa.track();
        view_center.track();
        view_fov.track();
        tiles_epoch.track();
        skymap.track();
        tile_fov.track();
        hover.track();
        // The canvas only exists once `open` is true and <Show> has mounted it.
        schedule_redraw();
    });

    // Keep the canvas correct across window resizes.
    let resize = window_event_listener(leptos::ev::resize, move |_| schedule_redraw());
    on_cleanup(move || resize.remove());

    // Opening frames the mosaic handed over by the target card.
    Effect::new(move |_| {
        if framing.open.get() {
            fit_view();
        }
    });

    // ── Pointer gestures ─────────────────────────────────────────────────
    // Pointers down (id → canvas x, y), what the first one grabbed, the path
    // travelled (tap vs drag), the previous pinch (distance, angle, midpoint)
    // and the last tap (time, x, y) for double-tap placement.
    let ptrs = StoredValue::new(HashMap::<i32, (f64, f64)>::new());
    let grab = StoredValue::new(Grab::None);
    let path = StoredValue::new(0.0_f64);
    let pinch_prev = StoredValue::new(None::<(f64, f64, (f64, f64))>);
    let last_tap = StoredValue::new(None::<(f64, f64, f64)>);
    let wheel_rot = StoredValue::new(0.0_f64);

    let on_down = move |ev: PointerEvent| {
        if ev.button() != 0 {
            return;
        }
        ev.prevent_default();
        if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
            let _ = el.set_pointer_capture(ev.pointer_id());
        }
        let Some((x, y)) = to_canvas(ev.client_x() as f64, ev.client_y() as f64) else { return };
        ptrs.update_value(|m| { m.insert(ev.pointer_id(), (x, y)); });
        let n = ptrs.with_value(|m| m.len());
        if n == 1 {
            let g = viewport().and_then(|vp| geometry(&vp));
            grab.set_value(grab_at(g, x, y, ev.pointer_type() == "touch"));
            path.set_value(0.0);
        } else {
            grab.set_value(Grab::Pinch);
            pinch_prev.set_value(None);
        }
        dragging.set(true);
    };
    let on_move = move |ev: PointerEvent| {
        let id = ev.pointer_id();
        let Some((x, y)) = to_canvas(ev.client_x() as f64, ev.client_y() as f64) else { return };
        let Some((ox, oy)) = ptrs.with_value(|m| m.get(&id).copied()) else {
            // Hovering: only the cursor follows.
            if ev.pointer_type() == "mouse" {
                let g = viewport().and_then(|vp| geometry(&vp));
                let h = grab_at(g, x, y, false);
                if hover.get_untracked() != h {
                    hover.set(h);
                }
            }
            return;
        };
        ptrs.update_value(|m| { m.insert(id, (x, y)); });
        let pts = ptrs.with_value(|m| m.values().copied().collect::<Vec<_>>());
        match (grab.get_value(), pts.as_slice()) {
            (Grab::Pinch, &[(ax, ay), (bx, by)]) => {
                let dist = (ax - bx).hypot(ay - by);
                let angle = (by - ay).atan2(bx - ax);
                let mid = ((ax + bx) / 2.0, (ay + by) / 2.0);
                if let Some((pd, pangle, pmid)) = pinch_prev.get_value() {
                    if pd > 0.0 && dist > 0.0 {
                        zoom_about(pd / dist, mid.0, mid.1);
                    }
                    let mut da = angle - pangle;
                    if da > PI { da -= TAU } else if da < -PI { da += TAU }
                    if da != 0.0 {
                        rotate_by(-da.to_degrees());
                    }
                    if let Some(vp) = viewport() {
                        let (ra1, dec1) = vp.screen_to_sky(pmid.0, pmid.1);
                        let (ra2, dec2) = vp.screen_to_sky(mid.0, mid.1);
                        pan_view(wrap180(ra1 - ra2), dec1 - dec2);
                    }
                }
                pinch_prev.set_value(Some((dist, angle, mid)));
            }
            (mode, &[_]) => {
                path.update_value(|p| *p += (x - ox).hypot(y - oy));
                if path.get_value() < TAP_SLOP {
                    return;
                }
                let Some(vp) = viewport() else { return };
                match mode {
                    Grab::Mosaic => {
                        let (ra1, dec1) = vp.screen_to_sky(ox, oy);
                        let (ra2, dec2) = vp.screen_to_sky(x, y);
                        let Some((ra, dec)) = framing.params.center.get_untracked() else { return };
                        framing.params.center.set(Some((
                            (ra + wrap180(ra2 - ra1)).rem_euclid(360.0),
                            (dec + dec2 - dec1).clamp(-89.9, 89.9),
                        )));
                    }
                    Grab::Sky => {
                        let (ra1, dec1) = vp.screen_to_sky(ox, oy);
                        let (ra2, dec2) = vp.screen_to_sky(x, y);
                        pan_view(wrap180(ra1 - ra2), dec1 - dec2);
                    }
                    Grab::Handle => {
                        let Some(g) = geometry(&vp) else { return };
                        let theta = (y - g.my).atan2(x - g.mx);
                        set_pa(-(theta + PI / 2.0).to_degrees());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    };
    let on_up = move |ev: PointerEvent| {
        let id = ev.pointer_id();
        let Some((x, y)) = ptrs.try_update_value(|m| m.remove(&id)).flatten() else { return };
        let mode = grab.get_value();
        let was_tap = matches!(mode, Grab::Sky | Grab::Mosaic | Grab::Handle) && path.get_value() < TAP_SLOP;
        // Whatever remains down starts no new mode: lifting a finger after a
        // pinch ends the gesture, like the sky.
        grab.set_value(Grab::None);
        pinch_prev.set_value(None);
        if ptrs.with_value(|m| m.is_empty()) {
            dragging.set(false);
        }
        if !was_tap {
            last_tap.set_value(None);
            return;
        }
        let now = js_sys::Date::now();
        match last_tap.get_value() {
            Some((t0, x0, y0)) if now - t0 <= DOUBLE_TAP_MS && (x - x0).hypot(y - y0) <= DOUBLE_TAP_PX => {
                last_tap.set_value(None);
                if let Some(vp) = viewport() {
                    framing.params.center.set(Some(vp.screen_to_sky(x, y)));
                }
            }
            _ => last_tap.set_value(Some((now, x, y))),
        }
    };
    let on_wheel = move |ev: WheelEvent| {
        ev.prevent_default();
        if ev.shift_key() {
            // Trackpads send many small deltas, mice one notch of ~100.
            wheel_rot.update_value(|acc| *acc += ev.delta_y());
            let steps = (wheel_rot.get_value() / WHEEL_PER_DEG).trunc();
            if steps != 0.0 {
                wheel_rot.update_value(|acc| *acc -= steps * WHEEL_PER_DEG);
                rotate_by(-steps);
            }
            return;
        }
        let Some((x, y)) = to_canvas(ev.client_x() as f64, ev.client_y() as f64) else { return };
        zoom_about(if ev.delta_y() < 0.0 { 1.0 / 1.15 } else { 1.15 }, x, y);
    };

    // ── Keyboard ─────────────────────────────────────────────────────────
    let close = move || framing.open.set(false);
    let can_send = Signal::derive(move || {
        framing.params.center.get().is_some() && tile_fov.get().is_some()
    });
    let send_to_planner = move || {
        let Some(planner) = planner_ctx.map(|c| c.0) else { return };
        if framing.params.center.get_untracked().is_none() { return; }
        // Same geometry, same epoch (JNow) on both sides — copy verbatim.
        planner.params.copy_from(&framing.params);
        planner.planning.set(true);
        framing.open.set(false);
        if let Some(ctx) = tab_ctx {
            ctx.0.set(Tab::Mosaic);
        }
    };
    let keys = window_event_listener(leptos::ev::keydown, move |e| {
        if !framing.open.get_untracked() {
            return;
        }
        let in_field = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT"));
        if in_field {
            if e.key() == "Escape" {
                close();
            }
            return;
        }
        let step = nudge_step_am.get_untracked();
        let quarter = view_fov.get_untracked() / 4.0;
        let cos_dec = view_center.get_untracked().1.to_radians().cos().abs().max(0.01);
        let shift = e.shift_key();
        let handled = match e.key().as_str() {
            "ArrowUp" => { if shift { pan_view(0.0, quarter) } else { nudge(0.0, step) } true }
            "ArrowDown" => { if shift { pan_view(0.0, -quarter) } else { nudge(0.0, -step) } true }
            "ArrowLeft" => { if shift { pan_view(quarter / cos_dec, 0.0) } else { nudge(step, 0.0) } true }
            "ArrowRight" => { if shift { pan_view(-quarter / cos_dec, 0.0) } else { nudge(-step, 0.0) } true }
            "q" | "Q" | "[" => { rotate_by(if shift { -15.0 } else { -1.0 }); true }
            "e" | "E" | "]" => { rotate_by(if shift { 15.0 } else { 1.0 }); true }
            "+" | "=" => { zoom_center(1.0 / 1.5); true }
            "-" | "_" => { zoom_center(1.5); true }
            "0" => { fit_view(); true }
            "Escape" => { close(); true }
            "Enter" => { if can_send.get_untracked() { send_to_planner() } true }
            _ => false,
        };
        if handled {
            e.prevent_default();
        }
    });
    on_cleanup(move || keys.remove());

    view! {
        <Show when=move || framing.open.get()>
            <div
                class="fixed inset-0 md:right-[64px] z-[100] bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2"
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
                        <div class="flex-1 min-h-0 min-w-0 relative bg-black max-[759px]:flex-none max-[759px]:h-[52vh]">
                            <canvas
                                node_ref=canvas_ref
                                class="absolute top-0 left-0 w-full h-full block select-none [touch-action:none]"
                                class:cursor-grab=move || !dragging.get() && matches!(hover.get(), Grab::None | Grab::Sky)
                                class:cursor-move=move || !dragging.get() && hover.get() == Grab::Mosaic
                                class:cursor-crosshair=move || !dragging.get() && hover.get() == Grab::Handle
                                class:cursor-grabbing=move || dragging.get()
                                on:pointerdown=on_down
                                on:pointermove=on_move
                                on:pointerup=on_up
                                on:pointercancel=on_up
                                on:lostpointercapture=on_up
                                on:pointerleave=move |_| hover.set(Grab::None)
                                on:wheel=on_wheel
                                on:contextmenu=|ev: web_sys::MouseEvent| ev.prevent_default()
                            />
                            // Zoom / fit, clear of the gestures beneath.
                            <div class="absolute top-2 right-2 flex flex-col gap-1">
                                <button type="button" class="btn-icon" title=move || tr().framing_zoom_in
                                        on:click=move |_| zoom_center(1.0 / 1.5)>"+"</button>
                                <button type="button" class="btn-icon" title=move || tr().framing_zoom_out
                                        on:click=move |_| zoom_center(1.5)>"\u{2212}"</button>
                                <button type="button" class="btn-icon" title=move || tr().framing_fit
                                        on:click=move |_| fit_view()>"\u{2922}"</button>
                            </div>
                            {move || loading.get().then(|| view! {
                                <div class="absolute top-2 left-1/2 -translate-x-1/2 py-1 px-3 bg-bg-panel-glass border border-border-accent rounded-md font-mono text-xs text-text-blue pointer-events-none">
                                    {move || tr().framing_loading}
                                </div>
                            })}
                            {move || tile_fov.get().is_none().then(|| view! {
                                <div class="absolute inset-0 flex items-center justify-center p-sp-4 text-center font-mono text-xs text-text-muted pointer-events-none">
                                    {move || tr().framing_no_fov}
                                </div>
                            })}
                        </div>

                        <div class="w-[300px] max-[759px]:w-full shrink-0 max-[759px]:shrink max-[759px]:flex-1 max-[759px]:min-h-0 border-l max-[759px]:border-l-0 max-[759px]:border-t border-border-base overflow-y-auto p-sp-3 flex flex-col gap-sp-2 text-[12px]">
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
                                            class="py-[6px] px-sp-2 cursor-pointer text-text border-b border-[#1a1a2a] text-[12px]"
                                            on:click=move |_| {
                                                // Catalog is J2000; our centre is of-date.
                                                let jnow = J2000::new(ra_j2000, dec_j2000).to_jnow(astro::now_jd());
                                                framing.params.center.set(Some((jnow.ra_deg, jnow.dec_deg)));
                                                framing.params.target.set(label.clone());
                                                search_text.set(String::new());
                                                fit_view();
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
                                    prop:value=move || framing.params.target.get()
                                    on:input=move |e| framing.params.target.set(event_target_value(&e))
                                />
                            </div>

                            // ── Centre (of-date) ──
                            <div class=format!("{CARD_TITLE} pt-sp-2")>{move || tr().framing_center}</div>
                            <div class="font-mono text-text-blue text-[13px] text-center">
                                {move || framing.params.center.get().map(|(ra, dec)| format!("{}  {}", fmt_ra(ra), fmt_dec(dec))).unwrap_or_else(|| "\u{2014}".into())}
                            </div>
                            <div class=SIDEBAR_ROW>
                                <span class=LABEL>{move || tr().framing_ra}</span>
                                <input type="number" step="0.0001" class=NUM_INPUT
                                    prop:value=move || framing.params.center.get().map(|(ra, _)| format!("{:.4}", ra / 15.0)).unwrap_or_default()
                                    on:input=move |e| {
                                        if let Ok(h) = event_target_value(&e).parse::<f64>() {
                                            let dec = framing.params.center.get_untracked().map(|(_, d)| d).unwrap_or(0.0);
                                            framing.params.center.set(Some(((h * 15.0).rem_euclid(360.0), dec)));
                                        }
                                    }
                                />
                            </div>
                            <div class=SIDEBAR_ROW>
                                <span class=LABEL>{move || tr().framing_dec}</span>
                                <input type="number" step="0.0001" class=NUM_INPUT
                                    prop:value=move || framing.params.center.get().map(|(_, dec)| format!("{:.4}", dec)).unwrap_or_default()
                                    on:input=move |e| {
                                        if let Ok(d) = event_target_value(&e).parse::<f64>() {
                                            let ra = framing.params.center.get_untracked().map(|(r, _)| r).unwrap_or(0.0);
                                            framing.params.center.set(Some((ra, d.clamp(-89.9, 89.9))));
                                        }
                                    }
                                />
                            </div>
                            // Nudge pad: N up, E left (+RA), W right, S down.
                            <div class="flex items-center justify-between gap-sp-2">
                                <div class="grid grid-cols-3 grid-rows-3 gap-1 w-max">
                                    <span></span>
                                    {hold_button("N \u{2191}", move || tr().framing_nudge.to_string(), move || nudge(0.0, nudge_step_am.get_untracked()))}
                                    <span></span>
                                    {hold_button("\u{2190} E", move || tr().framing_nudge.to_string(), move || nudge(nudge_step_am.get_untracked(), 0.0))}
                                    <span class="flex items-center justify-center font-mono text-[11px] text-text-muted">
                                        {move || fmt_step(nudge_step_am.get())}
                                    </span>
                                    {hold_button("W \u{2192}", move || tr().framing_nudge.to_string(), move || nudge(-nudge_step_am.get_untracked(), 0.0))}
                                    <span></span>
                                    {hold_button("S \u{2193}", move || tr().framing_nudge.to_string(), move || nudge(0.0, -nudge_step_am.get_untracked()))}
                                    <span></span>
                                </div>
                                <div class="flex flex-col gap-1 items-end">
                                    <span class=LABEL>{move || tr().framing_step}</span>
                                    {step_chip("1\u{2032}", 1.0, nudge_step_am)}
                                    {step_chip("10\u{2032}", 10.0, nudge_step_am)}
                                    {step_chip("1\u{00b0}", 60.0, nudge_step_am)}
                                </div>
                            </div>

                            // ── Rotation ──
                            <div class=format!("{CARD_TITLE} pt-sp-2")>{move || tr().framing_rotation}</div>
                            {slider_row(move || tr().mosaic_pa_label, framing.params.pa, -180.0, 180.0, "\u{00b0}")}
                            <div class="flex items-center gap-1 flex-wrap">
                                {hold_button("\u{27f2}", move || tr().framing_rotate_ccw.to_string(), move || rotate_by(-rot_step.get_untracked()))}
                                {hold_button("\u{27f3}", move || tr().framing_rotate_cw.to_string(), move || rotate_by(rot_step.get_untracked()))}
                                {step_chip("1\u{00b0}", 1.0, rot_step)}
                                {step_chip("15\u{00b0}", 15.0, rot_step)}
                                <span class="flex-1"></span>
                                <button type="button" class=CHIP on:click=move |_| set_pa(0.0)>"0\u{00b0}"</button>
                                <button type="button" class=CHIP on:click=move |_| set_pa(90.0)>"90\u{00b0}"</button>
                            </div>

                            // ── Grid ──
                            <div class=format!("{CARD_TITLE} pt-sp-2")>{move || tr().framing_grid}</div>
                            {stepper(move || tr().framing_cols, framing.params.grid_w)}
                            {stepper(move || tr().framing_rows, framing.params.grid_h)}
                            {slider_row(move || tr().mosaic_overlap_label, framing.params.overlap, 0.0, 50.0, "%")}

                            // Read-only derived field sizes
                            {move || tile_fov.get().map(|(fw, fh)| {
                                let (sw_am, sh_am) = mosaic_span_am(
                                    fw, fh,
                                    framing.params.grid_w.get(),
                                    framing.params.grid_h.get(),
                                    framing.params.overlap.get(),
                                );
                                view! {
                                    <div class="text-text-muted text-[11px] flex flex-col gap-[2px] pt-sp-1 border-t border-border-base">
                                        <div>{format!("{} {:.1}' \u{00d7} {:.1}'", tr().framing_tile_fov, fw * 60.0, fh * 60.0)}</div>
                                        <div>{format!("{} {:.1}' \u{00d7} {:.1}'", tr().framing_total_fov, sw_am, sh_am)}</div>
                                    </div>
                                }
                            })}

                            // The survey, the order rendered and how many of
                            // its tiles are on screen — 0 means the mirror
                            // doesn't cover the zone and the preview is black.
                            {move || (!loading.get()).then(|| {
                                let n = tiles_used.get();
                                let label = if skymap.get().is_none() {
                                    tr().framing_no_skymap.to_string()
                                } else if n == 0 {
                                    tr().framing_src_none.to_string()
                                } else {
                                    format!(
                                        "{} \u{00b7} {} {} \u{00b7} {} {}",
                                        tr().framing_src_skymap, tr().framing_order, order_used.get(), n, tr().framing_tiles
                                    )
                                };
                                view! { <div class="text-text-muted text-[11px]">{label}</div> }
                            })}

                            <div class="text-text-faint text-[11px] leading-snug pt-sp-1">
                                {move || tr().framing_hint}
                            </div>
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
                            on:click=move |_| send_to_planner()
                        >
                            {move || tr().framing_send_mosaic}
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// "2.4°" / "35′" / "48″" for an angle in degrees.
fn fmt_field(deg: f64) -> String {
    if deg >= 1.0 {
        format!("{deg:.1}\u{00b0}")
    } else if deg >= 1.0 / 60.0 {
        format!("{:.0}\u{2032}", deg * 60.0)
    } else {
        format!("{:.0}\u{2033}", deg * 3600.0)
    }
}

/// A nudge step in arcmin, as the pad shows it.
fn fmt_step(am: f64) -> String {
    fmt_field(am / 60.0)
}
