//! Sky map / planetarium tab — canvas-based interactive star chart.
//!
//! When WebGPU is available, uses a two-canvas stack:
//!   - Bottom: WebGPU canvas (sky bg + compute-projected stars + constellation lines)
//!   - Top:    Canvas2D overlay (ground, horizon, grid, names, crosshair, FOV, info)
//!
//! Falls back to all-Canvas2D rendering when WebGPU is unavailable.

mod actions;
mod controls;
mod info_popup;
mod object_search;
pub(crate) mod render;
mod search;
pub(crate) mod utils;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, HtmlImageElement, MouseEvent, TouchEvent,
    WheelEvent,
};

use crate::compat::{CameraSnapshot, MountSnapshot, MosaicSnapshot, SchedulerSnapshot, SiteSnapshot, SolveSnapshot};
use crate::ws::SendCmd;
use crate::{ActiveTabCtx, Tab};

use crate::astro;
use crate::catalog::CatalogData;
use crate::dso_catalog::DsoCatalogData;
use crate::gpu::{GpuSkyRenderer, Uniforms};
use crate::i18n::{Lang, t};
use crate::nebulae::NebulaeIndex;

use actions::{SkyConfirmPopup, SkyContextMenu, open_confirm};
use controls::SkyControls;
use info_popup::SkyInfoPopup;
use render::{HitItem, MosaicPlanRender, MosaicTileRender, RenderParams, SchedulerJobRender};
use search::SkySearch;
use utils::event_target_value;

// ---------------------------------------------------------------------------
// Toggles bundle — passed as a single prop to SkyControls / consumed by the
// render Effect. Avoids the ~40-prop signature SkyControls would otherwise need.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct SkyToggles {
    pub stars:              RwSignal<bool>,
    pub names:              RwSignal<bool>,
    pub constellations:     RwSignal<bool>,
    pub con_names:          RwSignal<bool>,
    pub grid:               RwSignal<bool>,
    pub eq_grid:            RwSignal<bool>,
    pub meridian:           RwSignal<bool>,
    pub fov:                RwSignal<bool>,
    pub dso:                RwSignal<bool>,
    pub ecliptic:           RwSignal<bool>,
    pub zenith:             RwSignal<bool>,
    pub solar_system:       RwSignal<bool>,
    pub solve_marker:       RwSignal<bool>,
    pub slew_trail:         RwSignal<bool>,
    pub dso_galaxy:         RwSignal<bool>,
    pub dso_open_cluster:   RwSignal<bool>,
    pub dso_globular:       RwSignal<bool>,
    pub dso_nebula:         RwSignal<bool>,
    pub dso_planetary:      RwSignal<bool>,
    pub dso_snr:            RwSignal<bool>,
    pub dso_galaxy_cluster: RwSignal<bool>,
    pub dso_mag_limit:      RwSignal<f64>,
    pub scheduler_jobs:     RwSignal<bool>,
}

/// Signals for the in-app Mosaic Planner (shared via App-level context).
#[derive(Clone, Copy)]
pub struct MosaicPlannerState {
    pub planning:       RwSignal<bool>,
    pub picking_center: RwSignal<bool>,  // true while "Pick on Sky" is active
    pub center:         RwSignal<Option<(f64, f64)>>,  // (ra_deg, dec_deg)
    pub grid_w:         RwSignal<u32>,
    pub grid_h:         RwSignal<u32>,
    pub overlap:        RwSignal<f64>,
    pub pa:             RwSignal<f64>,
    pub target:         RwSignal<String>,
    pub dir:            RwSignal<String>,
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

#[component]
pub fn SkyTab(
    #[prop(into)] mount: Signal<MountSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    #[prop(into)] solve: Signal<SolveSnapshot>,
    #[prop(into)] focal_length_mm: Signal<Option<f64>>,
    #[prop(into)] scheduler: Signal<SchedulerSnapshot>,
    #[prop(into)] mosaic: Signal<MosaicSnapshot>,
    #[prop(into)] send: SendCmd,
    center_alt: RwSignal<f64>,
    center_az: RwSignal<f64>,
    fov_radius: RwSignal<f64>,
    follow_mount: RwSignal<bool>,
) -> impl IntoView {
    // ── Local reactive state ───────────────────────────────────────────────
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let tab_ctx = use_context::<ActiveTabCtx>();

    let catalog_sig = use_context::<RwSignal<Option<Arc<CatalogData>>>>()
        .unwrap_or_else(|| RwSignal::new(None));
    let dso_catalog_sig = use_context::<RwSignal<Option<Arc<DsoCatalogData>>>>()
        .unwrap_or_else(|| RwSignal::new(None));

    let (center_alt, set_center_alt) = (center_alt.read_only(), center_alt.write_only());
    let (center_az, set_center_az) = (center_az.read_only(), center_az.write_only());
    let (fov_radius, set_fov_radius) = (fov_radius.read_only(), fov_radius.write_only());
    let (follow_mount, set_follow_mount) = (follow_mount.read_only(), follow_mount.write_only());

    // Persist sky view state to localStorage on change
    Effect::new(move || {
        let alt = center_alt.get();
        let az = center_az.get();
        let fov = fov_radius.get();
        let follow = follow_mount.get();
        if let Some(ls) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = ls.set_item("sky_center_alt", &alt.to_string());
            let _ = ls.set_item("sky_center_az", &az.to_string());
            let _ = ls.set_item("sky_fov_radius", &fov.to_string());
            let _ = ls.set_item("sky_follow_mount", if follow { "true" } else { "false" });
        }
    });

    let (time_offset_s, set_time_offset_s) = signal(0.0_f64);
    // Persist focal length override in localStorage
    let stored_fl = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|ls| ls.get_item("sky_focal_override").ok().flatten())
        .unwrap_or_default();
    let (focal_override, set_focal_override) = signal(stored_fl);
    // Read persisted checkbox state from localStorage
    let ls_init = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    let ls_bool = |key: &str, default: bool| -> bool {
        ls_init.as_ref()
            .and_then(|s| s.get_item(key).ok().flatten())
            .map(|v| v == "true")
            .unwrap_or(default)
    };
    let ls_f64_init = |key: &str, default: f64| -> f64 {
        ls_init.as_ref()
            .and_then(|s| s.get_item(key).ok().flatten())
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(default)
    };

    let show_stars            = RwSignal::new(ls_bool("sky_show_stars", true));
    let show_names            = RwSignal::new(ls_bool("sky_show_names", true));
    let show_constellations   = RwSignal::new(ls_bool("sky_show_constellations", true));
    let show_con_names        = RwSignal::new(ls_bool("sky_show_con_names", true));
    let show_grid             = RwSignal::new(ls_bool("sky_show_grid", true));
    let show_eq_grid          = RwSignal::new(ls_bool("sky_show_eq_grid", false));
    let show_meridian         = RwSignal::new(ls_bool("sky_show_meridian", true));
    let show_fov              = RwSignal::new(ls_bool("sky_show_fov", true));
    let show_dso              = RwSignal::new(ls_bool("sky_show_dso", true));
    let show_ecliptic         = RwSignal::new(ls_bool("sky_show_ecliptic", true));
    let show_zenith           = RwSignal::new(ls_bool("sky_show_zenith", false));
    let show_solar_system     = RwSignal::new(ls_bool("sky_show_solar_system", true));
    let show_solve_marker     = RwSignal::new(ls_bool("sky_show_solve_marker", true));
    let show_slew_trail       = RwSignal::new(ls_bool("sky_show_slew_trail", true));

    let dso_filter_galaxy         = RwSignal::new(ls_bool("sky_dso_galaxy", true));
    let dso_filter_open_cluster   = RwSignal::new(ls_bool("sky_dso_open_cluster", true));
    let dso_filter_globular       = RwSignal::new(ls_bool("sky_dso_globular", true));
    let dso_filter_nebula         = RwSignal::new(ls_bool("sky_dso_nebula", true));
    let dso_filter_planetary      = RwSignal::new(ls_bool("sky_dso_planetary", true));
    let dso_filter_snr            = RwSignal::new(ls_bool("sky_dso_snr", true));
    let dso_filter_galaxy_cluster = RwSignal::new(ls_bool("sky_dso_galaxy_cluster", true));
    let dso_mag_limit             = RwSignal::new(ls_f64_init("sky_dso_mag_limit", 11.0));
    let show_scheduler_jobs       = RwSignal::new(ls_bool("sky_show_scheduler_jobs", true));

    let toggles = SkyToggles {
        stars:               show_stars,
        names:               show_names,
        constellations:      show_constellations,
        con_names:           show_con_names,
        grid:                show_grid,
        eq_grid:             show_eq_grid,
        meridian:            show_meridian,
        fov:                 show_fov,
        dso:                 show_dso,
        ecliptic:            show_ecliptic,
        zenith:              show_zenith,
        solar_system:        show_solar_system,
        solve_marker:        show_solve_marker,
        slew_trail:          show_slew_trail,
        dso_galaxy:          dso_filter_galaxy,
        dso_open_cluster:    dso_filter_open_cluster,
        dso_globular:        dso_filter_globular,
        dso_nebula:          dso_filter_nebula,
        dso_planetary:       dso_filter_planetary,
        dso_snr:             dso_filter_snr,
        dso_galaxy_cluster:  dso_filter_galaxy_cluster,
        dso_mag_limit,
        scheduler_jobs:      show_scheduler_jobs,
    };

    // Mosaic planner state lives at App level; shared with MosaicTab via context.
    let planner = use_context::<crate::MosaicPlannerCtx>()
        .expect("MosaicPlannerCtx not provided")
        .0;

    // Persist checkbox state to localStorage on change
    Effect::new(move || {
        let bools: &[(&str, bool)] = &[
            ("sky_show_stars",          toggles.stars.get()),
            ("sky_show_names",          toggles.names.get()),
            ("sky_show_constellations", toggles.constellations.get()),
            ("sky_show_con_names",      toggles.con_names.get()),
            ("sky_show_grid",           toggles.grid.get()),
            ("sky_show_eq_grid",        toggles.eq_grid.get()),
            ("sky_show_meridian",       toggles.meridian.get()),
            ("sky_show_fov",            toggles.fov.get()),
            ("sky_show_dso",            toggles.dso.get()),
            ("sky_show_ecliptic",       toggles.ecliptic.get()),
            ("sky_show_zenith",         toggles.zenith.get()),
            ("sky_show_solar_system",   toggles.solar_system.get()),
            ("sky_show_solve_marker",   toggles.solve_marker.get()),
            ("sky_show_slew_trail",     toggles.slew_trail.get()),
            ("sky_dso_galaxy",          toggles.dso_galaxy.get()),
            ("sky_dso_open_cluster",    toggles.dso_open_cluster.get()),
            ("sky_dso_globular",        toggles.dso_globular.get()),
            ("sky_dso_nebula",          toggles.dso_nebula.get()),
            ("sky_dso_planetary",       toggles.dso_planetary.get()),
            ("sky_dso_snr",             toggles.dso_snr.get()),
            ("sky_dso_galaxy_cluster",  toggles.dso_galaxy_cluster.get()),
            ("sky_show_scheduler_jobs", toggles.scheduler_jobs.get()),
        ];
        let mag = toggles.dso_mag_limit.get();
        if let Some(ls) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            for (k, v) in bools {
                let _ = ls.set_item(k, if *v { "true" } else { "false" });
            }
            let _ = ls.set_item("sky_dso_mag_limit", &mag.to_string());
        }
    });

    // Object search state
    let (sky_search, set_sky_search) = signal(String::new());

    // Controls panel section visibility
    let (show_sky_section,      set_show_sky_section)      = signal(false);
    let (show_objects_section,  set_show_objects_section)  = signal(false);
    let (show_settings_section, set_show_settings_section) = signal(false);

    // Controls panel visibility (collapsed by default on narrow screens)
    let (show_controls, set_show_controls) = signal({
        web_sys::window()
            .and_then(|w| w.inner_width().ok())
            .and_then(|v| v.as_f64())
            .map(|w| w >= 768.0)
            .unwrap_or(true)
    });

    // Drag state. `drag_dist` tracks total movement so a short drag still
    // fires a click (hit-test) on mouseup, while a real pan suppresses it.
    let dragging = StoredValue::new(false);
    let drag_last = StoredValue::new((0.0_f64, 0.0_f64));
    let drag_dist = StoredValue::new(0.0_f64);
    let mousedown_pos = StoredValue::new((0.0_f64, 0.0_f64));

    // Current pointer position (CSS px relative to overlay canvas), for the
    // hover Alt/Az/RA/Dec readout. None when the pointer is off-canvas.
    let mouse_pos = RwSignal::new(None::<(f64, f64)>);

    // Hit-test targets — populated per frame by render::render_overlay, read
    // by on_mouseup to map a click to the nearest hovered object.
    let hit_items: Rc<RefCell<Vec<HitItem>>> = Rc::new(RefCell::new(Vec::new()));

    // Slew trail: ring buffer of recent mount positions sampled once per
    // render tick when RA/Dec changes. Each entry is (jd, ra_jnow_deg, dec_deg).
    let slew_trail: Rc<RefCell<VecDeque<(f64, f64, f64)>>> =
        Rc::new(RefCell::new(VecDeque::with_capacity(128)));
    let last_trail_sample = StoredValue::new((f64::NAN, f64::NAN));

    // Click-to-info popup target (or None for closed).
    let (info_popup, set_info_popup) = signal(None::<HitItem>);

    // Goto-and-align coordination: the context menu / confirm popup set this
    // to `true` after dispatching a goto. An Effect below watches the mount's
    // slewing status and fires `align_solve` once the slew completes. Firing
    // align_solve immediately (what we used to do) caused the solver to run
    // on the pre-slew image and place the solve marker at the mount's old
    // position.
    let pending_solve_after_slew = RwSignal::new(false);
    let was_slewing = StoredValue::new(false);

    // Pinch-to-zoom state
    let pinch_start_dist = StoredValue::new(0.0_f64);
    let pinch_start_fov  = StoredValue::new(0.0_f64);

    // Long-press timer for touch-triggered context menu (drop = cancel)
    let longpress_timer: Rc<RefCell<Option<gloo_timers::callback::Timeout>>> =
        Rc::new(RefCell::new(None));

    // Canvas refs
    let overlay_ref = NodeRef::<leptos::html::Canvas>::new();
    let gpu_canvas_ref = NodeRef::<leptos::html::Canvas>::new();

    // Context menu state
    let (ctx_menu, set_ctx_menu) = signal(None::<(f64, f64, f64, f64)>);

    // Bottom confirmation popup: (is_align, ra_deg, dec_deg)
    let (pending_action, set_pending_action) = signal(None::<(bool, f64, f64)>);

    // GPU renderer
    let gpu_renderer: Rc<RefCell<Option<GpuSkyRenderer>>> = Rc::new(RefCell::new(None));
    let (gpu_ready, set_gpu_ready) = signal(false);

    // Nebulae image cache: URL path → HtmlImageElement (lazily loaded)
    let nebulae_images: Rc<RefCell<HashMap<String, HtmlImageElement>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Nebulae index: fetched once from /nebulae.json at startup
    let (nebulae_index, set_nebulae_index) = signal(None::<Arc<NebulaeIndex>>);
    Effect::new(move || {
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(idx) = crate::nebulae::fetch_nebulae_index().await {
                set_nebulae_index.set(Some(idx));
            }
        });
    });

    // ── Idle-activity tracker ─────────────────────────────────────────────
    // Bumped whenever an input that affects the rendered scene changes. The
    // RAF loop reads it to decide whether to re-tick at ~30 fps (recent
    // activity) or drop to ~1 Hz so a parked mount doesn't keep the phone
    // busy redrawing identical frames.
    let last_active_ms: Rc<std::cell::Cell<f64>> =
        Rc::new(std::cell::Cell::new(js_sys::Date::now()));
    {
        let last_active_for_watch = Rc::clone(&last_active_ms);
        Effect::new(move || {
            // Subscribe to the high-signal inputs. Toggles and rare changes
            // are ignored — one slow frame after a toggle flip is fine; we
            // only care about pan/zoom/mount/cursor staying responsive.
            let _ = mount.get();
            let _ = camera.get();
            let _ = solve.get();
            let _ = mosaic.get();
            let _ = scheduler.get();
            let _ = fov_radius.get();
            let _ = center_alt.get();
            let _ = center_az.get();
            let _ = mouse_pos.get();
            last_active_for_watch.set(js_sys::Date::now());
        });
    }

    // ── Effective focal length ─────────────────────────────────────────────
    let effective_focal = Signal::derive(move || {
        let ov = focal_override.get();
        if let Ok(v) = ov.parse::<f64>() {
            if v > 0.0 { return Some(v); }
        }
        focal_length_mm.get()
    });

    // ── GPU init ───────────────────────────────────────────────────────────
    let gpu_for_init = Rc::clone(&gpu_renderer);
    Effect::new(move || {
        let Some(gpu_canvas) = gpu_canvas_ref.get() else { return; };
        let Some(cat) = catalog_sig.get() else { return; };
        if gpu_ready.get_untracked() { return; }

        let gpu = Rc::clone(&gpu_for_init);
        let star_data = cat.packed_star_buffer();
        let line_data = cat.packed_line_buffer();
        let gpu_canvas_el: HtmlCanvasElement = gpu_canvas.clone().into();
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(renderer) = GpuSkyRenderer::init(gpu_canvas_el, star_data, line_data).await {
                *gpu.borrow_mut() = Some(renderer);
                set_gpu_ready.set(true);
            }
        });
    });

    // ── Animation tick signal ──────────────────────────────────────────────
    let (tick, set_tick) = signal(0u32);

    // ── Render function ────────────────────────────────────────────────────
    let gpu_for_render = Rc::clone(&gpu_renderer);
    let nebulae_for_render = Rc::clone(&nebulae_images);
    let hit_items_for_render = Rc::clone(&hit_items);
    let trail_for_render = Rc::clone(&slew_trail);
    let trail_for_sample = Rc::clone(&slew_trail);
    let _render_handle = Effect::new(move || {
        // Read all reactive deps to subscribe
        let m = mount.get();
        let cam = camera.get();
        let s = site.get();
        let sv = solve.get();
        let sched = scheduler.get();
        let mos = mosaic.get();
        let fov = fov_radius.get();
        let t_off = time_offset_s.get();
        let stars_on = show_stars.get();
        let names_on = show_names.get();
        let const_on = show_constellations.get();
        let con_names_on = show_con_names.get();
        let grid_on = show_grid.get();
        let eq_grid_on = show_eq_grid.get();
        let meridian_on = show_meridian.get();
        let ecliptic_on = show_ecliptic.get();
        let zenith_on = show_zenith.get();
        let solar_system_on = show_solar_system.get();
        let solve_marker_on = show_solve_marker.get();
        let slew_trail_on = show_slew_trail.get();
        let fov_on = show_fov.get();
        let dso_on = show_dso.get();
        let dso_gx  = dso_filter_galaxy.get();
        let dso_oc  = dso_filter_open_cluster.get();
        let dso_gc  = dso_filter_globular.get();
        let dso_nb  = dso_filter_nebula.get();
        let dso_pn  = dso_filter_planetary.get();
        let dso_snr = dso_filter_snr.get();
        let dso_gal = dso_filter_galaxy_cluster.get();
        let dso_mag = dso_mag_limit.get();
        let scheduler_jobs_on = show_scheduler_jobs.get();
        let fl = effective_focal.get();
        let follow = follow_mount.get();
        let has_gpu = gpu_ready.get();
        let cur_lang = lang.get();
        let _frame = tick.get();
        let mosaic_planning_on = planner.planning.get();
        let mosaic_center_val = planner.center.get();
        let mosaic_gw = planner.grid_w.get();
        let mosaic_gh = planner.grid_h.get();
        let mosaic_overlap_pct = planner.overlap.get();
        let mosaic_pa_val = planner.pa.get();
        let mosaic_target_name = planner.target.get();

        let cat = catalog_sig.get_untracked();
        let dso_cat = dso_catalog_sig.get_untracked();

        let Some(overlay_canvas) = overlay_ref.get() else { return; };
        let overlay_el: HtmlCanvasElement = overlay_canvas.into();

        // Size overlay canvas to container
        let parent = overlay_el.parent_element().unwrap();
        let w = parent.client_width() as u32;
        let h = parent.client_height().max(500) as u32;
        // Cap DPR to 2.0 — on 3x mobile screens this cuts pixel count by ~2.25x
        let dpr = web_sys::window().map(|win| win.device_pixel_ratio().min(2.0)).unwrap_or(1.0);
        let w_phys = (w as f64 * dpr).round() as u32;
        let h_phys = (h as f64 * dpr).round() as u32;
        if overlay_el.width() != w_phys { overlay_el.set_width(w_phys); }
        if overlay_el.height() != h_phys { overlay_el.set_height(h_phys); }

        // Also size the GPU canvas
        if let Some(gc) = gpu_canvas_ref.get() {
            let gc_el: HtmlCanvasElement = gc.into();
            if gc_el.width() != w_phys { gc_el.set_width(w_phys); }
            if gc_el.height() != h_phys { gc_el.set_height(h_phys); }
        }

        let ctx = overlay_el
            .get_context("2d")
            .unwrap()
            .unwrap()
            .dyn_into::<CanvasRenderingContext2d>()
            .unwrap();

        let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);

        let wf = w as f64;
        let hf = h as f64;

        // ── Time ───────────────────────────────────────────────────────
        let now = js_sys::Date::new_0();
        let y = now.get_utc_full_year() as i32;
        let mo = now.get_utc_month() + 1;
        let d = now.get_utc_date();
        let hr = now.get_utc_hours();
        let mn = now.get_utc_minutes();
        let sc = now.get_utc_seconds() as f64 + now.get_utc_milliseconds() as f64 / 1000.0;
        let jd = astro::julian_date(y, mo, d, hr, mn, sc + t_off);
        let gmst = astro::gmst_deg(jd);
        let lst = astro::lst_deg(gmst, s.longitude);

        // ── View centre ────────────────────────────────────────────────
        let (c_alt, c_az) = if follow && m.connected {
            if let (Some(ra_h), Some(dec)) = (m.ra_h, m.dec_deg) {
                let pos = astro::eq_to_altaz(ra_h * 15.0, dec, lst, s.latitude);
                if !m.slewing {
                    set_center_alt.set(pos.0);
                    set_center_az.set(pos.1);
                    pos
                } else {
                    (center_alt.get_untracked(), center_az.get_untracked())
                }
            } else {
                (center_alt.get_untracked(), center_az.get_untracked())
            }
        } else {
            (center_alt.get_untracked(), center_az.get_untracked())
        };

        let cx = wf / 2.0;
        let cy = hf / 2.0;
        let scale = hf.min(wf) / 2.0;
        let sin_lat = s.latitude.to_radians().sin();
        let cos_lat = s.latitude.to_radians().cos();
        // Smooth magnitude limit: fewer stars when zoomed out, more when zoomed in.
        // fov=180° → mag 3.5, fov=90° → mag 4.5, fov=30° → mag 5.5, fov=5° → mag 6.5
        let mag_limit: f32 = (6.5 - 3.0 * (fov / 180.0).sqrt()).clamp(3.5, 6.5) as f32;

        // ── GPU path ───────────────────────────────────────────────────
        if has_gpu {
            if let Ok(mut opt) = gpu_for_render.try_borrow_mut() {
                if let Some(renderer) = opt.as_mut() {
                    if renderer.width() != w_phys || renderer.height() != h_phys {
                        renderer.resize(w_phys, h_phys);
                    }

                    // Precession angles for J2000 → JNow; shader applies them to
                    // catalog RA/Dec before eq_to_altaz so stars match the
                    // Canvas2D overlay and the mount crosshair.
                    let (zeta_rad, z_rad, theta_rad) =
                        crate::coords::precession_angles_j2000_to_jnow(jd);

                    let uniforms = Uniforms {
                        sin_lat: sin_lat as f32,
                        cos_lat: cos_lat as f32,
                        lst_rad: lst.to_radians() as f32,
                        c_alt_rad: c_alt.to_radians() as f32,
                        c_az_rad: c_az.to_radians() as f32,
                        fov_rad: fov.to_radians() as f32,
                        cx: (cx * dpr) as f32,
                        cy: (cy * dpr) as f32,
                        scale: (scale * dpr) as f32,
                        mag_limit,
                        canvas_w: (wf * dpr) as f32,
                        canvas_h: (hf * dpr) as f32,
                        dpr: dpr as f32,
                        zeta_rad: zeta_rad as f32,
                        z_rad: z_rad as f32,
                        theta_rad: theta_rad as f32,
                    };

                    renderer.render_frame(&uniforms, stars_on, const_on);
                }
            }
        }

        // ── Slew trail: sample mount position when it moves meaningfully.
        if m.connected {
            if let (Some(ra_h), Some(dec)) = (m.ra_h, m.dec_deg) {
                let (last_ra, last_dec) = last_trail_sample.get_value();
                let ra_deg = ra_h * 15.0;
                // Threshold: 2 arcmin (~0.033°) of angular change.
                let moved = last_ra.is_nan()
                    || ((ra_deg - last_ra).abs() * s.latitude.to_radians().cos().abs()
                        + (dec - last_dec).abs()) > 0.033;
                if moved {
                    last_trail_sample.set_value((ra_deg, dec));
                    let mut buf = trail_for_sample.borrow_mut();
                    buf.push_back((jd, ra_deg, dec));
                    while buf.len() > 120 { buf.pop_front(); }
                }
            }
        }

        // ── Cursor-to-world conversion for the HUD readout.
        let (cursor_altaz, cursor_radec) = if let Some((mx, my)) = mouse_pos.get() {
            // (mx, my) are in CSS px relative to the canvas. Convert to the
            // normalised [-1, 1] disk coords that astro::unproject expects.
            let nx = (mx - wf / 2.0) / (hf.min(wf) / 2.0);
            let ny = -(my - hf / 2.0) / (hf.min(wf) / 2.0);
            let (alt, az) = astro::unproject(nx, ny, c_alt, c_az, fov);
            let (ra, dec) = astro::altaz_to_eq(alt, az, lst, s.latitude);
            (Some((alt, az)), Some((ra, dec)))
        } else {
            (None, None)
        };

        // ── Derive scheduler job render list ───────────────────────────
        let scheduler_jobs_data: Vec<SchedulerJobRender> = if scheduler_jobs_on {
            sched.jobs.iter().filter_map(|j| {
                let name    = j["name"].as_str()?.to_string();
                let ra_h    = j["targetRA"].as_f64()?;
                let dec_deg = j["targetDEC"].as_f64()?;
                let state   = j["state"].as_i64().unwrap_or(0);
                Some(SchedulerJobRender { name, ra_h, dec_deg, state })
            }).collect()
        } else {
            Vec::new()
        };

        // ── KStars mosaic tiles → MosaicPlanRender ─────────────────────
        let mosaic_kstars_render = if !mos.tiles.is_empty() {
            let tiles = mos.tiles.iter().map(|t| MosaicTileRender {
                ra_deg: t.ra_deg, dec_deg: t.dec_deg, rotation: t.rotation,
            }).collect::<Vec<_>>();
            Some(MosaicPlanRender {
                target_name: mos.target_name.clone().unwrap_or_default(),
                tiles,
                fov_w_deg:   mos.camera_fov_w_deg.unwrap_or(0.5),
                fov_h_deg:   mos.camera_fov_h_deg.unwrap_or(0.5),
                overlap_pct: mos.overlap.unwrap_or(10.0),
                pa_deg:      mos.pa.unwrap_or(0.0),
            })
        } else {
            None
        };

        // ── In-app mosaic planner preview ──────────────────────────────
        let mosaic_plan_render: Option<MosaicPlanRender> = (|| {
            if !mosaic_planning_on { return None; }
            let (center_ra_deg, center_dec_deg) = mosaic_center_val?;
            let fl_mm = fl?;
            let px_um = cam.pixel_size_um?;
            let sw    = cam.sensor_width?;
            let sh    = cam.sensor_height?;
            let fov_w = astro::fov_deg(fl_mm, sw as f64, px_um);
            let fov_h = astro::fov_deg(fl_mm, sh as f64, px_um);
            let cos_dec = center_dec_deg.to_radians().cos().abs().max(0.01);
            let step_ra  = fov_w * (1.0 - mosaic_overlap_pct / 100.0) / cos_dec;
            let step_dec = fov_h * (1.0 - mosaic_overlap_pct / 100.0);
            let tiles = (0..mosaic_gh).flat_map(|row| {
                (0..mosaic_gw).map(move |col| MosaicTileRender {
                    ra_deg:  center_ra_deg + (col as f64 - (mosaic_gw as f64 - 1.0) / 2.0) * step_ra,
                    dec_deg: center_dec_deg + (row as f64 - (mosaic_gh as f64 - 1.0) / 2.0) * step_dec,
                    rotation: 0.0,
                })
            }).collect::<Vec<_>>();
            Some(MosaicPlanRender {
                target_name: mosaic_target_name.clone(),
                tiles,
                fov_w_deg:   fov_w,
                fov_h_deg:   fov_h,
                overlap_pct: mosaic_overlap_pct,
                pa_deg:      mosaic_pa_val,
            })
        })();

        // ── Overlay rendering (delegated to render module) ─────────────
        let params = RenderParams {
            wf,
            hf,
            c_alt,
            c_az,
            fov,
            lst,
            latitude: s.latitude,
            sin_lat,
            cos_lat,
            mag_limit,
            has_gpu,
            stars_on,
            names_on,
            const_on,
            con_names_on,
            grid_on,
            eq_grid_on,
            meridian_on,
            ecliptic_on,
            zenith_on,
            solar_system_on,
            solve_marker_on,
            slew_trail_on,
            fov_on,
            dso_on,
            dso_gx,
            dso_oc,
            dso_gc,
            dso_nb,
            dso_pn,
            dso_snr,
            dso_gal,
            dso_mag,
            fl,
            mount_connected: m.connected,
            mount_ra_h: m.ra_h,
            mount_dec_deg: m.dec_deg,
            cam_pixel_size_um: cam.pixel_size_um,
            cam_sensor_width: cam.sensor_width,
            cam_sensor_height: cam.sensor_height,
            rotation_deg: sv.rotation_deg,
            solve_ra_jnow_deg: sv.ra_jnow_deg,
            solve_dec_jnow_deg: sv.dec_jnow_deg,
            solve_pixscale_arcsec: sv.pixscale_arcsec,
            solve_age_ms: sv.solved_at_ms.map(|t| js_sys::Date::now() - t),
            cursor_altaz,
            cursor_radec,
            t_off,
            jd,
            cur_lang,
            scheduler_jobs_on,
            scheduler_jobs: scheduler_jobs_data,
            mosaic_kstars: mosaic_kstars_render,
            mosaic_plan: mosaic_plan_render,
        };

        let nb_idx = nebulae_index.get_untracked();
        let mut nebulae_cache = nebulae_for_render.borrow_mut();
        let mut hits = hit_items_for_render.borrow_mut();
        hits.clear();
        // make_contiguous() rotates the ring buffer in place so we can hand
        // the trail to the renderer as a single slice without copying. The
        // trail caps at 120 entries, so the rotation is essentially free.
        let mut trail = trail_for_render.borrow_mut();
        let trail_slice = trail.make_contiguous();
        render::render_overlay(
            &ctx, &params, &cat, &dso_cat,
            nb_idx.as_deref(),
            &mut nebulae_cache,
            &mut hits,
            trail_slice,
        );
    });

    // ── Slew-complete watcher: fire deferred align_solve when mount stops ─
    let send_for_align_after_slew = Arc::clone(&send);
    Effect::new(move || {
        let m = mount.get();
        let is_slewing = m.slewing;
        let was = was_slewing.get_value();
        was_slewing.set_value(is_slewing);

        // Fire only on the slewing → idle transition, and only if a pending
        // request exists. Guard against spurious triggers when the mount
        // arrives without ever reporting slewing (e.g. pre-existing idle state).
        if was && !is_slewing && pending_solve_after_slew.get() {
            pending_solve_after_slew.set(false);
            send_for_align_after_slew(
                serde_json::json!({"type":"align_solve","payload":{}}).to_string(),
            );
        }
    });

    // ── Animation loop ────────────────────────────────────────────────────
    // Two cadences:
    //   - Active (any input change in the last 500 ms): tick every other
    //     RAF frame → ~30 fps on 60 Hz panels, ~60 fps on 120 Hz.
    //   - Idle: tick at ~1 Hz so sidereal time and planet positions still
    //     update, but a parked phone stops burning cycles redrawing
    //     bit-identical frames.
    let last_active_for_raf = Rc::clone(&last_active_ms);
    let _raf = Effect::new(move || {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        let f: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
        let g = Rc::clone(&f);
        let frame_counter = Rc::new(std::cell::Cell::new(0u32));
        let last_idle_tick = Rc::new(std::cell::Cell::new(0.0_f64));
        let fc = Rc::clone(&frame_counter);
        let lit = Rc::clone(&last_idle_tick);
        let last_active = Rc::clone(&last_active_for_raf);

        *g.borrow_mut() = Some(Closure::<dyn FnMut()>::new(move || {
            let now_ms = js_sys::Date::now();
            let active = (now_ms - last_active.get()) < 500.0;
            let count = fc.get().wrapping_add(1);
            fc.set(count);

            let should_tick = if active {
                count % 2 == 0
            } else {
                // Idle: throttle to ~1 Hz.
                if (now_ms - lit.get()) >= 1000.0 {
                    lit.set(now_ms);
                    true
                } else {
                    false
                }
            };

            if should_tick {
                set_tick.update(|t| *t = t.wrapping_add(1));
            }
            if let Some(win) = web_sys::window() {
                let _ = win.request_animation_frame(
                    f.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
                );
            }
        }));

        let window = web_sys::window().unwrap();
        let _ = window.request_animation_frame(
            g.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
        );
    });

    // ── Mouse handlers ─────────────────────────────────────────────────────
    let hit_items_for_click = Rc::clone(&hit_items);
    let on_mousedown = move |ev: MouseEvent| {
        if ev.button() == 0 {
            set_ctx_menu.set(None);
            set_info_popup.set(None);
            dragging.set_value(true);
            drag_last.set_value((ev.client_x() as f64, ev.client_y() as f64));
            mousedown_pos.set_value((ev.client_x() as f64, ev.client_y() as f64));
            drag_dist.set_value(0.0);
        }
    };

    // Helper: convert a MouseEvent into (canvas CSS x, canvas CSS y).
    let to_canvas_xy = move |ev: &MouseEvent| -> Option<(f64, f64)> {
        let el = overlay_ref.get()?;
        let rect = el.get_bounding_client_rect();
        Some((ev.client_x() as f64 - rect.left(), ev.client_y() as f64 - rect.top()))
    };

    let on_mousemove = move |ev: MouseEvent| {
        // Always update hover position for the Alt/Az/RA/Dec readout.
        if let Some((cx, cy)) = to_canvas_xy(&ev) {
            mouse_pos.set(Some((cx, cy)));
        }

        if !dragging.get_value() { return; }
        let (lx, ly) = drag_last.get_value();
        let dx = ev.client_x() as f64 - lx;
        let dy = ev.client_y() as f64 - ly;
        drag_last.set_value((ev.client_x() as f64, ev.client_y() as f64));
        drag_dist.update_value(|d| *d += (dx * dx + dy * dy).sqrt());

        // Only pan if the drag has exceeded a small threshold — otherwise
        // the mouseup below still triggers a hit-test (click-to-info).
        if drag_dist.get_value() < 4.0 { return; }
        set_follow_mount.set(false);

        let fov = fov_radius.get_untracked();
        let deg_per_px = fov * 2.0 / 500.0;
        set_center_az.update(|az| *az = (*az - dx * deg_per_px).rem_euclid(360.0));
        set_center_alt.update(|alt| *alt = (*alt + dy * deg_per_px).clamp(-90.0, 90.0));
    };

    let on_mouseup = move |ev: MouseEvent| {
        dragging.set_value(false);
        // If the pointer barely moved, treat this as a click → hit-test.
        if drag_dist.get_value() >= 4.0 || ev.button() != 0 { return; }
        let Some((cx, cy)) = to_canvas_xy(&ev) else { return };

        // Center-pick mode (from "Pick on Sky" in Mosaic tab): left-click sets the mosaic center.
        if planner.picking_center.get_untracked() {
            let s = site.get_untracked();
            let fov = fov_radius.get_untracked();
            let alt_s = center_alt.get_untracked();
            let az_s  = center_az.get_untracked();
            let now = js_sys::Date::new_0();
            let jd_now = astro::julian_date(
                now.get_utc_full_year() as i32, now.get_utc_month() + 1, now.get_utc_date(),
                now.get_utc_hours(), now.get_utc_minutes(),
                now.get_utc_seconds() as f64 + now.get_utc_milliseconds() as f64 / 1000.0,
            );
            let gmst = astro::gmst_deg(jd_now);
            let lst  = astro::lst_deg(gmst, s.longitude);
            // Map CSS pixels → normalised disk coords → Alt/Az → RA/Dec
            if let Some(overlay_canvas) = overlay_ref.get_untracked() {
                let overlay_el: web_sys::HtmlCanvasElement = overlay_canvas.into();
                let w = overlay_el.parent_element().map(|p| p.client_width() as f64).unwrap_or(800.0);
                let h = overlay_el.parent_element().map(|p| p.client_height() as f64).unwrap_or(600.0);
                let nx = (cx - w / 2.0) / (h.min(w) / 2.0);
                let ny = -(cy - h / 2.0) / (h.min(w) / 2.0);
                let (alt, az) = astro::unproject(nx, ny, alt_s, az_s, fov);
                let (ra_deg, dec_deg) = astro::altaz_to_eq(alt, az, lst, s.latitude);
                planner.center.set(Some((ra_deg, dec_deg)));
                planner.picking_center.set(false);
                planner.planning.set(true);
                if let Some(ctx) = tab_ctx {
                    ctx.0.set(Tab::Mosaic);
                }
            }
            return;
        }

        let items = hit_items_for_click.borrow();
        let mut best: Option<(f64, HitItem)> = None;
        for it in items.iter() {
            let dx = cx - it.sx;
            let dy = cy - it.sy;
            let d2 = dx * dx + dy * dy;
            let r = it.radius.max(12.0);
            if d2 > r * r { continue; }
            if best.as_ref().map(|(bd, _)| d2 < *bd).unwrap_or(true) {
                best = Some((d2, it.clone()));
            }
        }
        drop(items);
        if let Some((_, hit)) = best {
            set_info_popup.set(Some(hit));
        }
    };

    let on_mouseleave = move |_: MouseEvent| {
        dragging.set_value(false);
        mouse_pos.set(None);
    };

    let on_wheel = move |ev: WheelEvent| {
        ev.prevent_default();
        let delta = ev.delta_y();
        set_fov_radius.update(|f| {
            *f = (*f * (1.0 + delta * 0.001)).clamp(0.1, 90.0);
        });
        let fov = fov_radius.get_untracked();
        let auto_mag = (11.0 + 3.0 * (10.0_f64 / fov).log10()).clamp(4.0, 20.0);
        dso_mag_limit.set((auto_mag * 2.0).round() / 2.0);
    };

    // ── Touch handlers ─────────────────────────────────────────────────────
    let lp_timer_start = Rc::clone(&longpress_timer);
    let lp_timer_move  = Rc::clone(&longpress_timer);
    let lp_timer_end   = Rc::clone(&longpress_timer);

    let on_touchstart = move |ev: TouchEvent| {
        ev.prevent_default();
        let touches = ev.touches();
        if touches.length() == 1 {
            let t = touches.get(0).unwrap();
            set_follow_mount.set(false);
            set_ctx_menu.set(None);
            dragging.set_value(true);
            drag_last.set_value((t.client_x() as f64, t.client_y() as f64));
            pinch_start_dist.set_value(0.0);

            // Snapshot sky-center coords for the long-press callback
            let tx = t.client_x() as f64;
            let ty = t.client_y() as f64;
            let lp_site = site.get_untracked();
            let lp_alt  = center_alt.get_untracked();
            let lp_az   = center_az.get_untracked();
            let now = js_sys::Date::new_0();
            let lp_jd = astro::julian_date(
                now.get_utc_full_year() as i32,
                now.get_utc_month() + 1,
                now.get_utc_date(),
                now.get_utc_hours(),
                now.get_utc_minutes(),
                now.get_utc_seconds() as f64 + time_offset_s.get_untracked(),
            );
            let lst = astro::lst_deg(astro::gmst_deg(lp_jd), lp_site.longitude);
            let (ra_jnow, dec_jnow) = astro::altaz_to_eq(lp_alt, lp_az, lst, lp_site.latitude);
            let timer_ref = Rc::clone(&lp_timer_start);
            *timer_ref.borrow_mut() = Some(gloo_timers::callback::Timeout::new(500, move || {
                set_ctx_menu.set(Some((tx, ty, ra_jnow, dec_jnow)));
            }));
        } else if touches.length() == 2 {
            *lp_timer_start.borrow_mut() = None;
            dragging.set_value(false);
            let t0 = touches.get(0).unwrap();
            let t1 = touches.get(1).unwrap();
            let dx = t0.client_x() as f64 - t1.client_x() as f64;
            let dy = t0.client_y() as f64 - t1.client_y() as f64;
            let dist = (dx * dx + dy * dy).sqrt();
            pinch_start_dist.set_value(dist);
            pinch_start_fov.set_value(fov_radius.get_untracked());
        }
    };

    let on_touchmove = move |ev: TouchEvent| {
        ev.prevent_default();
        *lp_timer_move.borrow_mut() = None; // any movement cancels the long-press
        let touches = ev.touches();
        if touches.length() == 1 && dragging.get_value() {
            let t = touches.get(0).unwrap();
            let (lx, ly) = drag_last.get_value();
            let dx = t.client_x() as f64 - lx;
            let dy = t.client_y() as f64 - ly;
            drag_last.set_value((t.client_x() as f64, t.client_y() as f64));
            let fov = fov_radius.get_untracked();
            let deg_per_px = fov * 2.0 / 500.0;
            set_center_az.update(|az| *az = (*az - dx * deg_per_px).rem_euclid(360.0));
            set_center_alt.update(|alt| *alt = (*alt + dy * deg_per_px).clamp(-90.0, 90.0));
        } else if touches.length() == 2 {
            let t0 = touches.get(0).unwrap();
            let t1 = touches.get(1).unwrap();
            let dx = t0.client_x() as f64 - t1.client_x() as f64;
            let dy = t0.client_y() as f64 - t1.client_y() as f64;
            let dist = (dx * dx + dy * dy).sqrt();
            let start_dist = pinch_start_dist.get_value();
            if start_dist > 0.0 {
                let start_fov = pinch_start_fov.get_value();
                let new_fov = (start_fov * start_dist / dist).clamp(0.1, 90.0);
                set_fov_radius.set(new_fov);
                let auto_mag = (11.0 + 3.0 * (10.0_f64 / new_fov).log10()).clamp(4.0, 20.0);
                dso_mag_limit.set((auto_mag * 2.0).round() / 2.0);
            }
        }
    };

    let on_touchend = move |ev: TouchEvent| {
        ev.prevent_default();
        *lp_timer_end.borrow_mut() = None; // lifted before 500ms — not a long-press
        dragging.set_value(false);
        pinch_start_dist.set_value(0.0);
    };

    // Right-click context menu
    let on_contextmenu = move |ev: MouseEvent| {
        ev.prevent_default();

        let now = js_sys::Date::new_0();
        let jd = astro::julian_date(
            now.get_utc_full_year() as i32,
            now.get_utc_month() + 1,
            now.get_utc_date(),
            now.get_utc_hours(),
            now.get_utc_minutes(),
            now.get_utc_seconds() as f64 + time_offset_s.get_untracked(),
        );
        let gmst = astro::gmst_deg(jd);
        let s = site.get_untracked();
        let lst = astro::lst_deg(gmst, s.longitude);

        // altaz_to_eq returns JNow — send it as-is; KStars' mount_goto_rade
        // handler treats the input as JNow regardless of the isJ2000 flag.
        let (ra_jnow, dec_jnow) = astro::altaz_to_eq(
            center_alt.get_untracked(),
            center_az.get_untracked(),
            lst,
            s.latitude,
        );
        set_ctx_menu.set(Some((ev.client_x() as f64, ev.client_y() as f64, ra_jnow, dec_jnow)));
    };

    let send_for_ctx = Arc::clone(&send);
    let send_for_confirm = Arc::clone(&send);
    let send_for_location = Arc::clone(&send);
    let set_site_location_fn: Arc<dyn Fn(f64, f64) + Send + Sync> = Arc::new(move |lat: f64, lon: f64| {
        send_for_location(serde_json::json!({"type":"option_set","payload":{"Latitude":lat,"Longitude":lon}}).to_string());
    });

    view! {
        <div class="sky-pane"
             style="position:relative; width:100%; overflow:hidden;"
             on:click=move |_| {
                 set_ctx_menu.set(None);
                 set_pending_action.set(None);
                 set_info_popup.set(None);
             }>

            // WebGPU canvas (bottom layer)
            <canvas
                node_ref=gpu_canvas_ref
                style="position:absolute; top:0; left:0; width:100%; height:100%; display:block;"
            />

            // Canvas2D overlay (top layer)
            <canvas
                node_ref=overlay_ref
                style="position:absolute; top:0; left:0; width:100%; height:100%; display:block; cursor:crosshair;"
                on:mousedown=on_mousedown
                on:mousemove=on_mousemove
                on:mouseup=on_mouseup
                on:mouseleave=on_mouseleave
                on:wheel=on_wheel
                on:contextmenu=on_contextmenu
                on:touchstart=on_touchstart
                on:touchmove=on_touchmove
                on:touchend=on_touchend
                on:touchcancel=move |_: TouchEvent| { *longpress_timer.borrow_mut() = None; dragging.set_value(false); pinch_start_dist.set_value(0.0); }
            />

            // ── Mosaic center-pick banner ──────────────────────────────────
            {move || planner.picking_center.get().then(|| view! {
                <div style="position:absolute; top:8px; left:50%; transform:translateX(-50%); \
                            z-index:100; pointer-events:none; padding:8px 18px; \
                            background:rgba(0,30,50,0.92); border:1px solid #00cccc; \
                            color:#00ffff; font-family:monospace; font-size:13px; \
                            border-radius:6px; white-space:nowrap;">
                    {"Click on the sky to set mosaic center"}
                </div>
            })}

            // ── Object search box ──────────────────────────────────────────
            <SkySearch
                sky_search=sky_search
                set_sky_search=set_sky_search
                catalog_sig=catalog_sig
                dso_catalog_sig=dso_catalog_sig
                site=site
                time_offset_s=time_offset_s
                set_center_alt=set_center_alt
                set_center_az=set_center_az
                set_follow_mount=set_follow_mount
                set_fov_radius=set_fov_radius
                dso_mag_limit=dso_mag_limit
            />

            // ── Controls panel ─────────────────────────────────────────────
            <SkyControls
                show_controls=show_controls
                set_show_controls=set_show_controls
                show_sky_section=show_sky_section
                set_show_sky_section=set_show_sky_section
                show_objects_section=show_objects_section
                set_show_objects_section=set_show_objects_section
                show_settings_section=show_settings_section
                set_show_settings_section=set_show_settings_section
                toggles=toggles
                focal_override=focal_override
                set_focal_override=set_focal_override
                focal_length_mm=focal_length_mm
                set_follow_mount=set_follow_mount
                site=site
                set_site_location=set_site_location_fn.clone()
            />

            // ── Slew action button (bottom-left) ───────────────────────────
            <div class="sky-actions"
                 style="position:absolute; bottom:48px; left:8px; display:flex; gap:8px; z-index:50;"
                 on:click=move |ev| ev.stop_propagation()>
                <button
                    on:click=move |_| open_confirm(false, time_offset_s, site, center_alt, center_az, set_ctx_menu, set_pending_action)
                    style="padding:10px 20px; background:#1a2a4a; color:#88aaff; border:1px solid #446; \
                           border-radius:4px; cursor:pointer; font-family:monospace; font-size:13px; font-weight:bold;">
                    {move || tr().goto_btn}
                </button>
            </div>

            // ── Time slider (bottom) ────────────────────────────────────────
            <div class="sky-time-slider"
                 style="position:absolute; bottom:4px; left:310px; right:8px; display:flex; align-items:center; gap:8px;">
                <span style="font-size:11px; color:#888; white-space:nowrap;">
                    {move || {
                        let t = time_offset_s.get();
                        if t.abs() < 0.5 { tr().now.to_string() }
                        else {
                            let h = (t / 3600.0) as i32;
                            let m = ((t.abs() % 3600.0) / 60.0) as u32;
                            format!("{h:+}h{m:02}m")
                        }
                    }}
                </span>
                <input type="range"
                       style="flex:1; accent-color:#88aaff;"
                       min="-43200" max="43200" step="60"
                       prop:value=move || format!("{}", time_offset_s.get() as i64)
                       on:input=move |ev| {
                           if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                               set_time_offset_s.set(v);
                           }
                       }
                />
                <button style="background:#1a1a2e; color:#aaa; border:1px solid #444; \
                               padding:2px 8px; cursor:pointer; font-family:monospace; font-size:11px;"
                        on:click=move |_| set_time_offset_s.set(0.0)>
                    {move || tr().reset}
                </button>
            </div>

            // ── Zoom bar (right side, vertical) ─────────────────────────────
            <div class="sky-zoom-bar"
                 style="position:absolute; right:8px; top:50px; bottom:40px; \
                        display:flex; flex-direction:column; align-items:center; gap:4px; z-index:30;"
                 on:click=move |ev| ev.stop_propagation()>
                <span style="font-size:11px; color:#888; white-space:nowrap;">
                    {move || {
                        let fov = fov_radius.get() * 2.0;
                        if fov >= 10.0 { format!("{:.0}\u{00b0}", fov) }
                        else if fov >= 1.0 { format!("{:.1}\u{00b0}", fov) }
                        else { format!("{:.2}\u{00b0}", fov) }
                    }}
                </span>
                <input type="range"
                       style="flex:1; accent-color:#88aaff; writing-mode:vertical-lr; \
                              direction:rtl; -webkit-appearance:slider-vertical; \
                              width:20px; cursor:pointer;"
                       min="-1000" max="1954" step="10"
                       prop:value=move || format!("{}", (fov_radius.get().log10() * 1000.0) as i32)
                       on:mousedown=|ev| ev.stop_propagation()
                       on:input=move |ev| {
                           if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                               let new_fov = 10_f64.powf(v / 1000.0).clamp(0.1, 90.0);
                               set_fov_radius.set(new_fov);
                               let auto_mag = (11.0 + 3.0 * (10.0_f64 / new_fov).log10()).clamp(4.0, 20.0);
                               dso_mag_limit.set((auto_mag * 2.0).round() / 2.0);
                           }
                       }
                />
            </div>

            // ── Context menu ────────────────────────────────────────────────
            <SkyContextMenu
                ctx_menu=ctx_menu
                set_ctx_menu=set_ctx_menu
                pending_solve_after_slew=pending_solve_after_slew
                send=send_for_ctx
            />

            // ── Bottom confirmation popup ────────────────────────────────────
            <SkyConfirmPopup
                pending_action=pending_action
                set_pending_action=set_pending_action
                pending_solve_after_slew=pending_solve_after_slew
                send=send_for_confirm
            />

            // ── Click-to-info popup (left-click on object) ───────────────────
            <SkyInfoPopup
                info_popup=info_popup
                set_info_popup=set_info_popup
            />

            // ── Floating mosaic editor (shown while planning == true) ──────────
            {move || planner.planning.get().then(|| view! {
                <div
                    style="position:absolute; bottom:80px; right:max(6px,env(safe-area-inset-right)); z-index:110; \
                           width:min(240px,calc(100vw - 12px)); background:rgba(8,8,20,0.94); \
                           border:1px solid #0a6060; border-radius:6px; \
                           padding:10px 12px; font-family:monospace; font-size:12px; \
                           color:#c0c0d0; display:flex; flex-direction:column; gap:7px;"
                    on:click=move |ev| ev.stop_propagation()
                    on:mousedown=move |ev| ev.stop_propagation()
                >
                    <div style="display:flex; justify-content:space-between; align-items:center;">
                        <span style="color:#00cccc; font-weight:bold;">{"Mosaic Setup"}</span>
                        <button
                            style="background:none; border:none; color:#888; cursor:pointer; \
                                   font-size:14px; line-height:1; padding:0 2px;"
                            on:click=move |_| {
                                planner.planning.set(false);
                                planner.center.set(None);
                            }>
                            {"\u{00d7}"}
                        </button>
                    </div>

                    {move || planner.center.get().map(|(ra_deg, dec_deg)| {
                        let ra_h = ra_deg / 15.0;
                        let rah  = ra_h as u32;
                        let ram  = ((ra_h - rah as f64) * 60.0).abs() as u32;
                        let ds   = if dec_deg < 0.0 { "\u{2212}" } else { "+" };
                        let da   = dec_deg.abs();
                        let decd = da as u32;
                        let decm = ((da - decd as f64) * 60.0) as u32;
                        view! {
                            <div style="font-size:11px; color:#88aaff;">
                                {format!("Center  {:02}h{:02}m  {}{}\u{00b0}{:02}\u{2019}", rah, ram, ds, decd, decm)}
                            </div>
                        }
                    })}

                    <label style="display:flex; align-items:center; gap:5px;">
                        <span style="color:#aaa; min-width:52px;">{"Target:"}</span>
                        <input type="text" placeholder="e.g. M31"
                               style="flex:1; background:#111; color:#ccc; border:1px solid #444; \
                                      font-family:monospace; font-size:12px; padding:2px 5px;"
                               prop:value=move || planner.target.get()
                               on:input=move |ev| {
                                   planner.target.set(
                                       ev.target().unwrap()
                                         .unchecked_into::<web_sys::HtmlInputElement>().value()
                                   );
                               } />
                    </label>

                    <div style="display:flex; align-items:center; gap:4px;">
                        <span style="color:#aaa; min-width:52px;">{"Grid:"}</span>
                        <input type="number" min="1" max="10"
                               style="width:40px; background:#111; color:#ccc; border:1px solid #444; \
                                      font-family:monospace; font-size:12px; padding:2px 4px; text-align:center;"
                               prop:value=move || planner.grid_w.get().to_string()
                               on:input=move |ev| {
                                   if let Ok(n) = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value()
                                       .parse::<u32>() {
                                       planner.grid_w.set(n.clamp(1, 10));
                                   }
                               } />
                        <span style="color:#888;">{"\u{00d7}"}</span>
                        <input type="number" min="1" max="10"
                               style="width:40px; background:#111; color:#ccc; border:1px solid #444; \
                                      font-family:monospace; font-size:12px; padding:2px 4px; text-align:center;"
                               prop:value=move || planner.grid_h.get().to_string()
                               on:input=move |ev| {
                                   if let Ok(n) = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value()
                                       .parse::<u32>() {
                                       planner.grid_h.set(n.clamp(1, 10));
                                   }
                               } />
                    </div>

                    <label style="display:flex; align-items:center; gap:4px;">
                        <span style="color:#aaa; min-width:52px;">{"Overlap:"}</span>
                        <input type="number" min="0" max="50" step="1"
                               style="width:48px; background:#111; color:#ccc; border:1px solid #444; \
                                      font-family:monospace; font-size:12px; padding:2px 4px;"
                               prop:value=move || format!("{:.0}", planner.overlap.get())
                               on:input=move |ev| {
                                   if let Ok(n) = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value()
                                       .parse::<f64>() {
                                       planner.overlap.set(n.clamp(0.0, 50.0));
                                   }
                               } />
                        <span style="color:#888;">{"%"}</span>
                    </label>

                    <label style="display:flex; align-items:center; gap:4px;">
                        <span style="color:#aaa; min-width:52px;">{"PA:"}</span>
                        <input type="number" min="-180" max="180" step="1"
                               style="width:54px; background:#111; color:#ccc; border:1px solid #444; \
                                      font-family:monospace; font-size:12px; padding:2px 4px;"
                               prop:value=move || format!("{:.0}", planner.pa.get())
                               on:input=move |ev| {
                                   if let Ok(n) = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value()
                                       .parse::<f64>() {
                                       planner.pa.set(n);
                                   }
                               } />
                        <span style="color:#888;">{"\u{00b0}"}</span>
                    </label>

                    <button
                        style="margin-top:4px; padding:6px 0; background:#0a1a2a; color:#88aaff; \
                               border:1px solid #446; cursor:pointer; font-family:monospace; \
                               font-size:12px; border-radius:3px; text-align:center;"
                        on:click=move |_| {
                            if let Some(ctx) = tab_ctx {
                                ctx.0.set(Tab::Mosaic);
                            }
                        }>
                        {"Open Mosaic Planner \u{2192}"}
                    </button>
                </div>
            })}
        </div>
    }
}

// (Removed) In-planetarium gear bar — replaced by `components::tab_wheel`.
