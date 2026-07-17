//! Sky map / planetarium tab — canvas-based interactive star chart.
//!
//! When WebGPU is available, uses a two-canvas stack:
//!   - Bottom: WebGPU canvas (sky bg + compute-projected stars + constellation lines)
//!   - Top:    Canvas2D overlay (ground, horizon, grid, names, crosshair, FOV, info)
//!
//! Falls back to all-Canvas2D rendering when WebGPU is unavailable.

mod actions;
mod controls;
pub(crate) mod dso_index;
mod dso_render;
mod dso_shape;
pub(crate) mod gpu;
mod hud;
mod picking;
mod solar_render;
mod info_popup;
mod framing;
mod object_search;
pub(crate) mod render;
mod search;
pub(crate) mod utils;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, MouseEvent, TouchEvent, WheelEvent,
};

use crate::compat::{CameraSnapshot, MountSnapshot, MosaicSnapshot, SchedulerSnapshot, SiteSnapshot, SolveSnapshot};
use crate::ws::SendCmd;
use crate::{ActiveTabCtx, Tab};

use crate::astro;
use crate::catalog::CatalogData;
use crate::dso_catalog::DsoCatalogData;
use self::gpu::{LineView, SkyRenderer, Uniforms};
use crate::i18n::{Lang, t};

use actions::SkyContextMenu;
use controls::SkyControls;
use framing::FramingOverlay;
use info_popup::SkyInfoPopup;

pub use framing::FramingState;
use render::{HitItem, MosaicPlanRender, MosaicTileRender, SchedulerJobRender};
use render::layer::{Catalogs, Frame};
use render::params::{LayerToggles, OverlayState, PipelineMode, SceneParams, ViewParams};
use render::pipeline::RenderPipeline;
use search::SkySearch;

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
// Mobile detection
// ---------------------------------------------------------------------------
//
// Smartphones — even powerful ones — pay disproportionately for Canvas2D fill
// rate at 3× DPR and for fillText calls in the labels pass. We detect a
// mobile-class device once at mount and lower DPR + tighten label thresholds
// accordingly. Heuristic, not UA-sniffing: coarse pointer + small viewport +
// limited cores. Any two-of-three triggers mobile mode.

#[derive(Clone, Copy)]
pub struct MobileProfile {
    pub is_mobile: bool,
    /// Upper bound for `device_pixel_ratio`. 2.0 on mobile (avoids 3× Canvas2D
    /// fill cost on phones reporting DPR 3); 3.0 on desktop for crisp Retina/4K.
    pub dpr_cap: f64,
}

fn detect_mobile_profile() -> MobileProfile {
    let Some(win) = web_sys::window() else {
        return MobileProfile { is_mobile: false, dpr_cap: 2.0 };
    };
    // Two votes from three signals trigger mobile mode. Heuristic, never UA.
    //   - small viewport: phones in either orientation have min-side < 900 CSS px
    //   - high DPR: any device that reports > 2.0 is almost certainly a phone
    //   - low core count: hardwareConcurrency ≤ 6 → phone-class CPU
    // matchMedia(pointer:coarse) would be the cleanest signal but the
    // MediaQueryList web-sys feature isn't enabled in this crate; we'd rather
    // keep the dependency footprint small.
    let small = {
        let w = win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1920.0);
        let h = win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(1080.0);
        w.min(h) < 900.0
    };
    let high_dpr = win.device_pixel_ratio() > 2.0;
    let cores = win.navigator().hardware_concurrency() as u32;
    let low_cores = cores > 0 && cores <= 6;
    let votes = (small as u8) + (high_dpr as u8) + (low_cores as u8);
    let is_mobile = votes >= 2;
    // Mobile stays capped at 2.0 to keep Canvas2D fill cost manageable.
    // Desktop uses the full device DPR (up to 3.0) for sharper rendering on
    // Retina/4K monitors — the extra fill rate is affordable there.
    let dpr_cap = if is_mobile { 2.0 } else { 3.0 };
    MobileProfile { is_mobile, dpr_cap }
}

fn build_uniforms(
    sin_lat: f64,
    cos_lat: f64,
    lst: f64,
    c_alt: f64,
    c_az: f64,
    fov: f64,
    cx: f64,
    cy: f64,
    scale: f64,
    mag_limit: f32,
    wf: f64,
    hf: f64,
    dpr: f64,
    jd: f64,
) -> Uniforms {
    let (zeta_rad, z_rad, theta_rad) = crate::coords::precession_angles_j2000_to_jnow(jd);
    Uniforms {
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
    }
}

fn derive_scheduler_jobs(
    scheduler_jobs_on: bool,
    scheduler: &SchedulerSnapshot,
) -> Vec<SchedulerJobRender> {
    if !scheduler_jobs_on {
        return Vec::new();
    }
    scheduler
        .jobs
        .iter()
        .filter_map(|j| {
            let name = j["name"].as_str()?.to_string();
            let ra_h = j["targetRA"].as_f64()?;
            let dec_deg = j["targetDEC"].as_f64()?;
            let state = j["state"].as_i64().unwrap_or(0);
            Some(SchedulerJobRender {
                name,
                ra_h,
                dec_deg,
                state,
            })
        })
        .collect()
}

fn derive_kstars_mosaic_plan(mosaic: &MosaicSnapshot) -> Option<MosaicPlanRender> {
    if mosaic.tiles.is_empty() {
        return None;
    }
    let tiles = mosaic
        .tiles
        .iter()
        .map(|tile| MosaicTileRender {
            ra_deg: tile.ra_deg,
            dec_deg: tile.dec_deg,
            rotation: tile.rotation,
        })
        .collect::<Vec<_>>();
    Some(MosaicPlanRender {
        target_name: mosaic.target_name.clone().unwrap_or_default(),
        tiles,
        fov_w_deg: mosaic.camera_fov_w_deg.unwrap_or(0.5),
        fov_h_deg: mosaic.camera_fov_h_deg.unwrap_or(0.5),
        overlap_pct: mosaic.overlap.unwrap_or(10.0),
        pa_deg: mosaic.pa.unwrap_or(0.0),
    })
}

fn derive_planner_mosaic_plan(
    planning_on: bool,
    center: Option<(f64, f64)>,
    focal_length_mm: Option<f64>,
    camera: &CameraSnapshot,
    grid_w: u32,
    grid_h: u32,
    overlap_pct: f64,
    pa_deg: f64,
    target_name: &str,
) -> Option<MosaicPlanRender> {
    if !planning_on {
        return None;
    }
    let (center_ra_deg, center_dec_deg) = center?;
    let fl_mm = focal_length_mm?;
    let px_um = camera.pixel_size_um?;
    let sw = camera.sensor_width?;
    let sh = camera.sensor_height?;

    let fov_w_deg = astro::fov_deg(fl_mm, sw as f64, px_um);
    let fov_h_deg = astro::fov_deg(fl_mm, sh as f64, px_um);
    let fov_w_am = fov_w_deg * 60.0;
    let fov_h_am = fov_h_deg * 60.0;
    let x_off = fov_w_am * (1.0 - overlap_pct / 100.0);
    let y_off = fov_h_am * (1.0 - overlap_pct / 100.0);
    let init_x = (x_off * (grid_w as f64 - 1.0) - fov_w_am) / 2.0;
    let init_y = -(fov_h_am + y_off * (grid_h as f64 - 1.0)) / 2.0;
    // KStars rotatePoint uses angle = -PA (after wrapping PA into [0,360)).
    let pa_norm = ((pa_deg % 360.0) + 360.0) % 360.0;
    let ang = -pa_norm.to_radians();
    let cp = ang.cos();
    let sp = ang.sin();

    let tiles = (0..grid_w)
        .flat_map(|col| {
            (0..grid_h).map(move |row| {
                let x = init_x - col as f64 * x_off;
                let y = init_y + row as f64 * y_off;
                let tx = x + fov_w_am / 2.0;
                let ty = y + fov_h_am / 2.0;
                let rx = cp * tx - sp * ty;
                let ry = sp * tx + cp * ty;
                // skyLocation = (0,0) - rotatePoint(...): negate.
                let sky_x_am = -rx;
                let sky_y_am = -ry;
                let dec_t = center_dec_deg + sky_y_am / 60.0;
                let cos_dec_t = dec_t.to_radians().cos().abs().max(0.01);
                let ra_t = center_ra_deg + (sky_x_am / 60.0) / cos_dec_t;
                MosaicTileRender {
                    ra_deg: ra_t,
                    dec_deg: dec_t,
                    rotation: 0.0,
                }
            })
        })
        .collect::<Vec<_>>();

    Some(MosaicPlanRender {
        target_name: target_name.to_string(),
        tiles,
        fov_w_deg,
        fov_h_deg,
        overlap_pct,
        pa_deg,
    })
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

#[component]
pub fn SkyTab(
    #[prop(into)] mount: Signal<MountSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    set_site_location: Arc<dyn Fn(f64, f64) + Send + Sync>,
    #[prop(into)] mount_device: Signal<Option<String>>,
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
    // ── Mobile profile (one-shot — no signal needed) ──────────────────────
    let mobile_profile = detect_mobile_profile();

    // ── Local reactive state ───────────────────────────────────────────────
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let tab_ctx = use_context::<ActiveTabCtx>();

    let catalog_sig = use_context::<RwSignal<Option<Arc<CatalogData>>>>()
        .unwrap_or_else(|| RwSignal::new(None));
    let dso_catalog_sig = use_context::<RwSignal<Option<Arc<DsoCatalogData>>>>()
        .unwrap_or_else(|| RwSignal::new(None));
    let dso_index_sig = use_context::<RwSignal<Option<Arc<dso_index::DsoIndex>>>>()
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
    let (time_shift_open,       set_time_shift_open)       = signal(false);

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

    // HUD data snapshot — written each frame by the render Effect, read
    // reactively by the <SkyHud> DOM component.
    let (hud_data, set_hud_data) = signal(hud::HudData::default());

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

    // GPU renderer
    let gpu_renderer: Rc<RefCell<Option<SkyRenderer>>> = Rc::new(RefCell::new(None));
    let (gpu_ready, set_gpu_ready) = signal(false);

    // New layered render pipeline. Built once; layers are stateless today.
    // Runs after the legacy `render::render_overlay` each frame, so layers
    // already migrated draw on top of the legacy overlay until step 8.
    let render_pipeline: Rc<RefCell<RenderPipeline>> =
        Rc::new(RefCell::new(RenderPipeline::standard()));

    // ── FOV diagnostics ───────────────────────────────────────────────────
    // Log the inputs and the resulting reticle FOV whenever the camera geometry,
    // the last solve, or the nominal focal length changes (NOT per frame). Lets
    // us compare junos-web's FOV against KStars' Align FOV readout on real gear.
    Effect::new(move || {
        let cam = camera.get();
        let sv = solve.get();
        let nominal_fl = focal_length_mm.get();
        let eff_fl = match (sv.pixscale_arcsec, cam.pixel_size_um, cam.bin_x) {
            (Some(px), Some(pum), Some(bin)) => {
                crate::astro::effective_focal_mm(px, pum, bin as f64)
            }
            _ => None,
        };
        let fl = eff_fl.or(nominal_fl);
        if let (Some(fl), Some(pum), Some(sw), Some(sh)) =
            (fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height)
        {
            let fov_w = crate::astro::fov_deg(fl, sw as f64, pum);
            let fov_h = crate::astro::fov_deg(fl, sh as f64, pum);
            leptos::logging::log!(
                "[sky] FOV inputs: fl={:.1}mm (nominal={:?} eff_from_solve={:?}) sensor={}x{}px pixel={:.2}um bin={:?} pixscale={:?}\"/px -> {:.1}'x{:.1}'",
                fl, nominal_fl, eff_fl, sw, sh, pum, cam.bin_x, sv.pixscale_arcsec,
                fov_w * 60.0, fov_h * 60.0
            );
        }
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
            if let Some(renderer) = SkyRenderer::init(gpu_canvas_el, star_data, line_data).await {
                *gpu.borrow_mut() = Some(renderer);
                set_gpu_ready.set(true);
            }
        });
    });

    // ── Animation tick signal ──────────────────────────────────────────────
    let (tick, set_tick) = signal(0u32);

    // ── Render function ────────────────────────────────────────────────────
    let gpu_for_render = Rc::clone(&gpu_renderer);
    let pipeline_for_render = Rc::clone(&render_pipeline);
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
        // Prefer a focal length back-computed from the last plate solve's
        // measured pixel scale over the nominal scope focal × CCD_INFO. This
        // makes the FOV reticle (and mosaic preview / scheduler-job frames,
        // all of which read this `fl`) self-correct after the first solve
        // regardless of a wrong scope focal or a wrong/binned CCD_INFO pixel
        // size — mirroring KStars' effective focal length (align.cpp:1089).
        // Requires pixscale + native pixel size + binning; else nominal.
        let nominal_fl = focal_length_mm.get();
        let fl = match (sv.pixscale_arcsec, cam.pixel_size_um, cam.bin_x) {
            (Some(px), Some(pum), Some(bin)) => {
                crate::astro::effective_focal_mm(px, pum, bin as f64).or(nominal_fl)
            }
            _ => nominal_fl,
        };
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
        let dso_idx = dso_index_sig.get_untracked();

        let Some(overlay_canvas) = overlay_ref.get() else { return; };
        let overlay_el: HtmlCanvasElement = overlay_canvas.into();

        // Size overlay canvas to container
        let parent = overlay_el.parent_element().unwrap();
        let w = parent.client_width() as u32;
        let h = parent.client_height().max(500) as u32;
        // DPR cap is 2.0 on mobile, 3.0 on desktop — see MobileProfile.
        let dpr_cap = mobile_profile.dpr_cap;
        let dpr = web_sys::window().map(|win| win.device_pixel_ratio().min(dpr_cap)).unwrap_or(1.0);
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

        let mut gpu_uniforms: Option<Uniforms> = None;
        // ── GPU prep (uniforms + renderer resize) ──────────────────────
        if has_gpu {
            if let Ok(mut opt) = gpu_for_render.try_borrow_mut() {
                if let Some(renderer) = opt.as_mut() {
                    if renderer.width() != w_phys || renderer.height() != h_phys {
                        renderer.resize(w_phys, h_phys);
                    }

                    let uniforms = build_uniforms(
                        sin_lat, cos_lat, lst, c_alt, c_az, fov, cx, cy, scale, mag_limit, wf, hf,
                        dpr, jd,
                    );
                    gpu_uniforms = Some(uniforms);

                    // GPU line/DSO/text prep now runs through RenderPipeline.
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

        // ── Push HUD snapshot to the DOM overlay ──────────────────────
        set_hud_data.set(hud::HudData {
            lst_deg: lst,
            fov,
            c_alt,
            c_az,
            mount_ra_h: m.ra_h,
            mount_dec_deg: m.dec_deg,
            rotation_deg: sv.rotation_deg,
            t_off,
            cursor_altaz,
            cursor_radec,
        });

        // ── Derive scheduler job render list ───────────────────────────
        let scheduler_jobs_data = derive_scheduler_jobs(scheduler_jobs_on, &sched);

        // ── KStars mosaic tiles → MosaicPlanRender ─────────────────────
        let mosaic_kstars_render = derive_kstars_mosaic_plan(&mos);

        // ── In-app mosaic planner preview ──────────────────────────────
        // Mirrors KStars' MosaicTiles::updateTiles (mosaictiles.cpp:370-435):
        // tiles are laid out in a planar grid (arcmin), the entire grid is
        // rotated by PA around the mosaic center, then converted to RA/Dec
        // with a per-tile cos(dec) correction. This way the preview matches
        // exactly what `scheduler_import_mosaic` will produce in KStars.
        let mosaic_plan_render = derive_planner_mosaic_plan(
            mosaic_planning_on,
            mosaic_center_val,
            fl,
            &cam,
            mosaic_gw,
            mosaic_gh,
            mosaic_overlap_pct,
            mosaic_pa_val,
            &mosaic_target_name,
        );

        let mut hits = hit_items_for_render.borrow_mut();
        hits.clear();
        // Build hit list directly from catalogs / ephemerides — independent
        // of the Canvas2D render path. The render_overlay call below skips
        // the duplicate `hit_items.push` sites when picking_on_cpu is set.
        if has_gpu {
            let view = LineView {
                wf, hf, fov, c_alt, c_az,
                lst, latitude: s.latitude, jd,
            };
            picking::build(
                picking::PickParams {
                    view: &view,
                    catalog: cat.as_ref(),
                    dso_cat: dso_cat.as_ref(),
                    dso_index: dso_idx.as_deref(),
                    mag_limit,
                    stars_on,
                    dso_on,
                    dso_filter: dso_render::KindFilter {
                        gx: dso_gx, oc: dso_oc, gc: dso_gc,
                        nb: dso_nb, pn: dso_pn, snr: dso_snr,
                        gal: dso_gal,
                    },
                    dso_mag,
                    solar_on: solar_system_on,
                    lang: cur_lang,
                },
                &mut hits,
            );
        }
        // make_contiguous() rotates the ring buffer in place so we can hand
        // the trail to the renderer as a single slice without copying. The
        // trail caps at 120 entries, so the rotation is essentially free.
        let mut trail = trail_for_render.borrow_mut();
        let trail_slice = trail.make_contiguous();

        // ── Layered render pipeline ───────────────────────────────────
        // All Canvas2D rendering flows through `RenderPipeline::run`,
        // which drives a `Vec<Box<dyn SkyLayer>>` in legacy draw order.
        // The four grouped param structs are populated directly from the
        // local signal/state values — no intermediate god-struct.
        let cx = wf / 2.0;
        let cy = hf / 2.0;
        let scale = hf.min(wf) / 2.0;
        let view = ViewParams { wf, hf, c_alt, c_az, fov, cx, cy, scale };
        let scene = SceneParams {
            jd, lst, latitude: s.latitude,
            sin_lat, cos_lat, t_off,
            mag_limit, cur_lang,
            is_mobile: mobile_profile.is_mobile,
        };
        let toggles = LayerToggles {
            stars_on, names_on, const_on, con_names_on,
            grid_on, eq_grid_on, meridian_on, ecliptic_on, zenith_on,
            solar_system_on, solve_marker_on, slew_trail_on, fov_on,
            dso_on, scheduler_jobs_on,
        };
        let state = OverlayState {
            mount_connected: m.connected,
            mount_ra_h: m.ra_h,
            mount_dec_deg: m.dec_deg,
            fl,
            cam_pixel_size_um: cam.pixel_size_um,
            cam_sensor_width:  cam.sensor_width,
            cam_sensor_height: cam.sensor_height,
            rotation_deg: sv.rotation_deg,
            solve_ra_jnow_deg: sv.ra_jnow_deg,
            solve_dec_jnow_deg: sv.dec_jnow_deg,
            solve_pixscale_arcsec: sv.pixscale_arcsec,
            solve_age_ms: sv.solved_at_ms.map(|t| js_sys::Date::now() - t),
            cursor_altaz,
            cursor_radec,
            scheduler_jobs: scheduler_jobs_data,
            mosaic_kstars: mosaic_kstars_render,
            mosaic_plan:   mosaic_plan_render,
            dso_gx, dso_oc, dso_gc, dso_nb, dso_pn, dso_snr, dso_gal, dso_mag,
        };
        let mode = PipelineMode::from_has_gpu(has_gpu);
        let catalogs = Catalogs {
            stars: cat.as_ref(),
            dso: dso_cat.as_ref(),
            dso_index: dso_idx.as_deref(),
        };
        let mut frame = Frame {
            view: &view,
            scene: &scene,
            state: &state,
            toggles: &toggles,
            mode,
            catalogs: &catalogs,
            hit_items: &mut hits,
            slew_trail: trail_slice,
        };
        if has_gpu {
            if let (Some(uniforms), Ok(mut pipe), Ok(mut opt)) = (
                gpu_uniforms,
                pipeline_for_render.try_borrow_mut(),
                gpu_for_render.try_borrow_mut(),
            ) {
                if let Some(renderer) = opt.as_mut() {
                    pipe.run(&mut frame, &ctx, renderer.font_atlas());
                    let prep = render::layer::GpuPrepare {
                        lines: pipe.gpu_prepare().lines.clone(),
                        dso: pipe.gpu_prepare().dso.clone(),
                        text: pipe.gpu_prepare().text.clone(),
                        show_stars: stars_on,
                        show_constellations: const_on,
                    };
                    renderer.submit_frame(&prep, &uniforms);
                }
            }
        } else if let Ok(mut pipe) = pipeline_for_render.try_borrow_mut() {
            pipe.run(&mut frame, &ctx, None);
        }
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


    view! {
        <div class="relative w-full h-[100dvh] overflow-hidden"
             on:click=move |_| {
                 set_ctx_menu.set(None);
                 set_info_popup.set(None);
             }>

            // WebGPU canvas (bottom layer)
            <canvas
                node_ref=gpu_canvas_ref
                class="absolute top-0 left-0 w-full h-full block"
            />

            // Canvas2D overlay (top layer)
            <canvas
                node_ref=overlay_ref
                class="absolute top-0 left-0 w-full h-full block cursor-crosshair"
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
                <div class="absolute top-2 left-1/2 -translate-x-1/2 z-[100] pointer-events-none py-2 px-[18px] bg-bg-banner border border-accent-cyan-dim text-accent-cyan font-mono text-md rounded-md whitespace-nowrap">
                    {"Click on the sky to set mosaic center"}
                </div>
            })}

            // ── Bottom-left stack: time-shift on top, HUD below ────────────
            <div class="absolute left-sp-3 bottom-sp-3 z-[90] flex flex-col gap-sp-2 items-start pointer-events-none">
            // ── Time-shift toggle (visible on <md only, when row is collapsed) ──
            <button
                class=move || {
                    let base = "md:hidden pointer-events-auto px-2 py-1 \
                                bg-bg-panel-glass border border-border-accent rounded-md \
                                font-mono text-xs text-text-blue-bright cursor-pointer";
                    if time_shift_open.get() { format!("{base} hidden") } else { base.to_string() }
                }
                on:click=move |_| set_time_shift_open.set(true)
            >
                "🕒"
            </button>
            // ── Time-shift panel ───────────────────────────────────────────
            <div
                class=move || {
                    let base = "flex items-center gap-1 max-md:gap-0.5 px-2 max-md:px-1 py-1 max-md:py-0.5 \
                                max-w-[calc(100vw-16px)] flex-wrap pointer-events-auto \
                                bg-bg-panel-glass border border-border-accent rounded-md \
                                font-mono text-xs max-md:text-[10px] text-text-blue select-none";
                    if time_shift_open.get() { base.to_string() } else { format!("{base} max-md:hidden") }
                }
            >
                {
                    let bump = move |delta: f64| {
                        set_time_offset_s.update(|t| *t += delta);
                    };
                    let btn_cls = "min-w-0 px-1.5 max-md:px-1 py-0.5 bg-bg-button hover:bg-bg-button-info \
                                   border border-border-accent rounded cursor-pointer \
                                   text-text-blue-bright";
                    view! {
                        <button class=format!("{btn_cls} md:hidden")
                                on:click=move |_| set_time_shift_open.set(false)>
                            "×"
                        </button>
                        <button class=btn_cls on:click=move |_| bump(-3600.0)>"-1h"</button>
                        <button class=btn_cls on:click=move |_| bump(-600.0)>"-10m"</button>
                        <button class=btn_cls on:click=move |_| bump(-60.0)>"-1m"</button>
                        <span class="min-w-[64px] max-md:min-w-[52px] text-center px-1 max-md:px-0 max-md:text-[10px]">
                            {move || {
                                let t = time_offset_s.get();
                                if t.abs() < 0.5 { tr().now.to_string() }
                                else {
                                    let sign = if t < 0.0 { "-" } else { "+" };
                                    let a = t.abs();
                                    let h = (a / 3600.0) as i64;
                                    let m = ((a % 3600.0) / 60.0) as i64;
                                    format!("{sign}{h:02}h{m:02}m")
                                }
                            }}
                        </span>
                        <button class=btn_cls on:click=move |_| bump(60.0)>"+1m"</button>
                        <button class=btn_cls on:click=move |_| bump(600.0)>"+10m"</button>
                        <button class=btn_cls on:click=move |_| bump(3600.0)>"+1h"</button>
                        <button class=btn_cls
                                title=move || tr().reset.to_string()
                                on:click=move |_| set_time_offset_s.set(0.0)>
                            "⟲"
                        </button>
                    }
                }
            </div>

            // ── DOM HUD (replaces render_info_overlay when GPU is up) ──────
            {move || gpu_ready.get().then(|| view! {
                <hud::SkyHud hud=hud_data lang=lang.read_only() />
            })}
            </div>

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
                set_follow_mount=set_follow_mount
                site=site
                set_site_location=set_site_location
                mount_device=mount_device
            />

            // ── Framing assistant overlay (opened from the context menu) ────
            <FramingOverlay
                camera=camera
                focal_length_mm=focal_length_mm
                catalog_sig=catalog_sig
                dso_catalog_sig=dso_catalog_sig
            />

            // ── Context menu ────────────────────────────────────────────────
            <SkyContextMenu
                ctx_menu=ctx_menu
                set_ctx_menu=set_ctx_menu
                pending_solve_after_slew=pending_solve_after_slew
                send=send_for_ctx
            />

            // ── Click-to-info popup (left-click on object) ───────────────────
            <SkyInfoPopup
                info_popup=info_popup
                set_info_popup=set_info_popup
            />
        </div>
    }
}

// (Removed) In-planetarium gear bar — replaced by `components::tab_wheel`.
