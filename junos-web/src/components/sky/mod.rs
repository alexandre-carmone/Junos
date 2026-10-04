//! Sky map / planetarium tab — canvas-based interactive star chart.
//!
//! When WebGPU is available, uses a two-canvas stack:
//!   - Bottom: WebGPU canvas (sky bg + compute-projected stars + constellation lines)
//!   - Top:    Canvas2D overlay (ground, horizon, grid, names, crosshair, FOV, info)
//!
//! Falls back to all-Canvas2D rendering when WebGPU is unavailable.

mod actions;
mod calib;
pub(crate) mod clock;
mod controls;
pub(crate) mod dso_images;
pub(crate) mod dso_index;
mod dso_render;
mod dso_shape;
pub(crate) mod gpu;
mod hud;
mod picking;
mod solar_render;
mod framing;
mod object_search;
pub(crate) mod render;
mod search;
mod time_bar;
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

use actions::{SkyTarget, SkyTargetCard};
use clock::SkyClock;
use controls::SkyControls;
use framing::FramingOverlay;

pub use framing::FramingState;
pub(crate) use actions::{fmt_dec, fmt_ra};
pub(crate) use framing::mosaic_span_am;
use render::{HitItem, MosaicPlanRender, MosaicTileRender, SchedulerJobRender};
use render::layer::{Catalogs, Frame};
use render::params::{LayerToggles, OverlayState, PipelineMode, SceneParams, SolvedImage, ViewParams};
use render::pipeline::RenderPipeline;
use search::SkySearch;
use time_bar::TimeBar;

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
    pub solved_image:       RwSignal<bool>,
    pub solved_image_opacity: RwSignal<f64>,
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
    pub dso_images:         RwSignal<bool>,
    pub dso_images_brightness: RwSignal<f64>,
    pub milky_way:          RwSignal<bool>,
    pub milky_way_opacity:  RwSignal<f64>,
}

impl SkyToggles {
    /// Tracked read of the on/off layer toggles (subscribes the caller).
    fn layer_toggles(&self) -> LayerToggles {
        LayerToggles {
            stars_on: self.stars.get(),
            names_on: self.names.get(),
            const_on: self.constellations.get(),
            con_names_on: self.con_names.get(),
            grid_on: self.grid.get(),
            eq_grid_on: self.eq_grid.get(),
            meridian_on: self.meridian.get(),
            ecliptic_on: self.ecliptic.get(),
            zenith_on: self.zenith.get(),
            solar_system_on: self.solar_system.get(),
            solve_marker_on: self.solve_marker.get(),
            slew_trail_on: self.slew_trail.get(),
            fov_on: self.fov.get(),
            dso_on: self.dso.get(),
            scheduler_jobs_on: self.scheduler_jobs.get(),
            solved_image_on: self.solved_image.get(),
            dso_images_on: self.dso_images.get(),
            milky_way_on: self.milky_way.get(),
        }
    }

    /// Tracked read of the per-type DSO filters.
    fn dso_filter(&self) -> dso_render::KindFilter {
        dso_render::KindFilter {
            gx: self.dso_galaxy.get(),
            oc: self.dso_open_cluster.get(),
            gc: self.dso_globular.get(),
            nb: self.dso_nebula.get(),
            pn: self.dso_planetary.get(),
            snr: self.dso_snr.get(),
            gal: self.dso_galaxy_cluster.get(),
        }
    }
}

/// Width × height (deg) of the last solved frame: KStars' effective FOV when
/// the solution carried it, else `pix` — arcsec per *binned* pixel — times the
/// binned sensor size (native `CCD_MAX_X/Y` over the current `CCD_BINNING`).
fn solve_fov_deg(sv: &SolveSnapshot, cam: &CameraSnapshot) -> Option<(f64, f64)> {
    if let Some((w, h)) = sv.fov_arcmin {
        return Some((w / 60.0, h / 60.0));
    }
    let pix = sv.pixscale_arcsec?;
    let bin = cam.bin_x.unwrap_or(1).max(1) as f64;
    let side = |px: u32| pix * px as f64 / bin / 3600.0;
    Some((side(cam.sensor_width?), side(cam.sensor_height?)))
}

/// Focal length the FOV reticle (and mosaic / scheduler frames) use. Once a
/// frame has been measured it is back-computed from that field, so a wrong
/// scope focal or CCD_INFO self-corrects — KStars' effective focal length
/// (align_fov.cpp:89). The measured frame (`calib`: KStars' own FOV, this
/// session's solve or a saved one) comes first: it does not depend on
/// `CCD_BINNING`, which may have changed since the solve. Else the nominal focal.
fn reticle_focal_mm(
    frame: Option<&calib::FrameCalib>,
    sv: &SolveSnapshot,
    cam: &CameraSnapshot,
    nominal: Option<f64>,
) -> Option<f64> {
    let from_fov = || {
        let w = frame?.fov_w_arcmin;
        astro::focal_from_fov_mm(w / 60.0, cam.sensor_width? as f64, cam.pixel_size_um?)
    };
    let from_pix = || {
        astro::effective_focal_mm(sv.pixscale_arcsec?, cam.pixel_size_um?, cam.bin_x? as f64)
    };
    from_fov().or_else(from_pix).or(nominal)
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// Signal initialised from localStorage `key` and written back on change.
fn persisted<T>(key: &'static str, default: T) -> RwSignal<T>
where
    T: std::str::FromStr + ToString + Send + Sync + 'static,
{
    let init = local_storage()
        .and_then(|s| s.get_item(key).ok().flatten())
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    let sig = RwSignal::new(init);
    Effect::new(move || {
        let v = sig.with(|v| v.to_string());
        if let Some(s) = local_storage() {
            let _ = s.set_item(key, &v);
        }
    });
    sig
}

/// Hit radius floor (CSS px) for a mouse click and for a finger tap.
const MOUSE_HIT_R: f64 = 12.0;
const TOUCH_HIT_R: f64 = 22.0;
/// Pointer travel (CSS px) below which a press is a tap, not a pan.
const TAP_SLOP: f64 = 8.0;

/// Nearest hit item whose radius (at least `min_r`) contains (x, y).
fn hit_test(items: &[HitItem], x: f64, y: f64, min_r: f64) -> Option<HitItem> {
    items
        .iter()
        .map(|it| ((x - it.sx).powi(2) + (y - it.sy).powi(2), it))
        .filter(|(d2, it)| *d2 <= it.radius.max(min_r).powi(2))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, it)| it.clone())
}

/// The mosaic geometry shared verbatim by the Framing Assistant and the Mosaic
/// planner tab: centre, target name, grid, overlap, position angle. Both
/// `FramingState` and `MosaicPlannerState` embed one of these so the framing →
/// planner hand-off is a single `copy_from` instead of a field-by-field copy.
#[derive(Clone, Copy)]
pub struct MosaicParams {
    pub center:  RwSignal<Option<(f64, f64)>>,  // (ra_deg, dec_deg), JNow
    pub target:  RwSignal<String>,
    pub grid_w:  RwSignal<u32>,
    pub grid_h:  RwSignal<u32>,
    pub overlap: RwSignal<f64>,
    pub pa:      RwSignal<f64>,
}

impl MosaicParams {
    /// Copy every value out of `src` into these signals (framing → planner).
    pub fn copy_from(&self, src: &MosaicParams) {
        self.center.set(src.center.get_untracked());
        self.target.set(src.target.get_untracked());
        self.grid_w.set(src.grid_w.get_untracked());
        self.grid_h.set(src.grid_h.get_untracked());
        self.overlap.set(src.overlap.get_untracked());
        self.pa.set(src.pa.get_untracked());
    }
}

/// Signals for the in-app Mosaic Planner (shared via App-level context).
#[derive(Clone, Copy)]
pub struct MosaicPlannerState {
    pub planning:       RwSignal<bool>,
    pub picking_center: RwSignal<bool>,  // true while "Pick on Sky" is active
    pub params:         MosaicParams,
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
    jd: f64,
) -> Vec<SchedulerJobRender> {
    if !scheduler_jobs_on {
        return Vec::new();
    }
    scheduler
        .jobs
        .iter()
        .filter_map(|j| {
            let name = j["name"].as_str()?.to_string();
            // KStars serializes targetRA/targetDEC as ra0()/dec0() (J2000).
            // render_scheduler_jobs projects with eq_to_altaz (JNow), so
            // precess J2000→JNow here or the box draws ~0.4° off (2026).
            let ra_j2000 = j["targetRA"].as_f64()? * 15.0;
            let dec_j2000 = j["targetDEC"].as_f64()?;
            let jnow = crate::coords::J2000::new(ra_j2000, dec_j2000).to_jnow(jd);
            let state = j["state"].as_i64().unwrap_or(0);
            Some(SchedulerJobRender {
                name,
                ra_h: jnow.ra_deg / 15.0,
                dec_deg: jnow.dec_deg,
                state,
            })
        })
        .collect()
}

fn derive_kstars_mosaic_plan(mosaic: &MosaicSnapshot, jd: f64) -> Option<MosaicPlanRender> {
    if mosaic.tiles.is_empty() {
        return None;
    }
    // KStars publishes tile centers as J2000 (skyCenter.ra0/dec0). The render
    // path projects with astro::eq_to_altaz, which assumes JNow, so precess
    // each tile J2000→JNow first — otherwise the overlay draws ~0.4° off the
    // JNow star field and mount crosshair.
    let tiles = mosaic
        .tiles
        .iter()
        .map(|tile| {
            let jnow = crate::coords::J2000::new(tile.ra_deg, tile.dec_deg).to_jnow(jd);
            MosaicTileRender {
                ra_deg: jnow.ra_deg,
                dec_deg: jnow.dec_deg,
                rotation: tile.rotation,
            }
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

    // Simulated time — always starts live; see `clock.rs`.
    let clock = RwSignal::new(SkyClock::live());

    let toggles = SkyToggles {
        stars:              persisted("sky_show_stars", true),
        names:              persisted("sky_show_names", true),
        constellations:     persisted("sky_show_constellations", true),
        con_names:          persisted("sky_show_con_names", true),
        grid:               persisted("sky_show_grid", true),
        eq_grid:            persisted("sky_show_eq_grid", false),
        meridian:           persisted("sky_show_meridian", true),
        fov:                persisted("sky_show_fov", true),
        dso:                persisted("sky_show_dso", true),
        ecliptic:           persisted("sky_show_ecliptic", true),
        zenith:             persisted("sky_show_zenith", false),
        solar_system:       persisted("sky_show_solar_system", true),
        solve_marker:       persisted("sky_show_solve_marker", true),
        solved_image:       persisted("sky_show_solved_image", true),
        solved_image_opacity: persisted("sky_solved_image_opacity", 0.6),
        slew_trail:         persisted("sky_show_slew_trail", true),
        dso_galaxy:         persisted("sky_dso_galaxy", true),
        dso_open_cluster:   persisted("sky_dso_open_cluster", true),
        dso_globular:       persisted("sky_dso_globular", true),
        dso_nebula:         persisted("sky_dso_nebula", true),
        dso_planetary:      persisted("sky_dso_planetary", true),
        dso_snr:            persisted("sky_dso_snr", true),
        dso_galaxy_cluster: persisted("sky_dso_galaxy_cluster", true),
        dso_mag_limit:      persisted("sky_dso_mag_limit", 11.0),
        scheduler_jobs:     persisted("sky_show_scheduler_jobs", true),
        dso_images:         persisted("sky_show_dso_images", true),
        dso_images_brightness: persisted("sky_dso_images_brightness", 1.0),
        milky_way:          persisted("sky_show_milky_way", true),
        milky_way_opacity:  persisted("sky_milky_way_opacity", 0.6),
    };
    let dso_mag_limit = toggles.dso_mag_limit;

    // Mosaic planner state lives at App level; shared with MosaicTab via context.
    let planner = use_context::<crate::MosaicPlannerCtx>()
        .expect("MosaicPlannerCtx not provided")
        .0;

    // Object search state
    let (sky_search, set_sky_search) = signal(String::new());

    // Layers panel (bottom sheet on phones, floating panel on md+).
    let layers_open = RwSignal::new(false);

    // Drag state. `drag_dist` tracks total movement so a short drag still
    // fires a click (hit-test) on mouseup, while a real pan suppresses it.
    let dragging = StoredValue::new(false);
    let drag_last = StoredValue::new((0.0_f64, 0.0_f64));
    let drag_dist = StoredValue::new(0.0_f64);

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

    // Target card subject (or None for closed) — tap, right-click, long-press.
    let target = RwSignal::new(None::<SkyTarget>);

    // Goto-and-align coordination: the target card sets this
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

    // Long-press timer for the touch target card (drop = cancel). The flag
    // stops the finger lift that ends a long-press from also counting as a tap.
    let longpress_timer: Rc<RefCell<Option<gloo_timers::callback::Timeout>>> =
        Rc::new(RefCell::new(None));
    let longpress_fired = StoredValue::new(false);

    // Canvas refs
    let overlay_ref = NodeRef::<leptos::html::Canvas>::new();
    let gpu_canvas_ref = NodeRef::<leptos::html::Canvas>::new();

    // GPU renderer
    let gpu_renderer: Rc<RefCell<Option<SkyRenderer>>> = Rc::new(RefCell::new(None));
    let (gpu_ready, set_gpu_ready) = signal(false);

    // New layered render pipeline. Built once; layers are stateless today.
    // Runs after the legacy `render::render_overlay` each frame, so layers
    // already migrated draw on top of the legacy overlay until step 8.
    let render_pipeline: Rc<RefCell<RenderPipeline>> =
        Rc::new(RefCell::new(RenderPipeline::standard()));

    // ── Measured camera frame (calib.rs) ──────────────────────────────────
    // Each solve's FOV + PA is saved under camera|focal; the FOV boxes use this
    // session's solve, else the saved one, else the nominal frame.
    let calib_key = Memo::new(move |_| {
        let fl = focal_length_mm.get()?;
        camera.with(|c| calib::calib_key(&c.device, fl))
    });
    let session_calib = Memo::new(move |_| {
        solve.with(|s| match (s.fov_arcmin, s.rotation_deg, s.solved_at_ms) {
            (Some((w, h)), Some(pa), Some(t)) => Some(calib::FrameCalib {
                fov_w_arcmin: w,
                fov_h_arcmin: h,
                pa_deg: pa,
                at_ms: t,
            }),
            _ => None,
        })
    });
    // The key a solve is filed under is the one current when it arrived, so a
    // later train switch doesn't relabel it.
    let solved_frame: RwSignal<Option<(String, calib::FrameCalib)>> = RwSignal::new(None);
    Effect::new(move |_| {
        let Some(c) = session_calib.get() else { return };
        let Some(key) = calib_key.get_untracked() else { return };
        calib::save(&key, c);
        solved_frame.set(Some((key, c)));
    });
    let frame_calib = Memo::new(move |_| {
        let key = calib_key.get()?;
        match solved_frame.get() {
            Some((k, c)) if k == key => Some((c, calib::CalibSource::Solved)),
            _ => calib::load(&key).map(|c| (c, calib::CalibSource::Saved)),
        }
    });

    // ── Solved image (SolvedImageLayer) ───────────────────────────────────
    // Load the last solve's Align frame; `solved_image_epoch` wakes the render
    // Effect once it has decoded. A newer solve supersedes a load in flight.
    let solved_image: Rc<RefCell<Option<SolvedImage>>> = Rc::new(RefCell::new(None));
    let solved_image_epoch = RwSignal::new(0u32);
    {
        // Memo so solver log lines (which also update `solve`) don't reload it.
        let placement = Memo::new(move |_| {
            solve.with(|s| {
                let frame = s.image.clone()?;
                let pix = s.pixscale_arcsec.filter(|p| *p > 0.0)?;
                Some((frame, s.ra_jnow_deg?, s.dec_jnow_deg?, s.rotation_deg?, pix))
            })
        });
        let solved_image = Rc::clone(&solved_image);
        let load_gen = Rc::new(std::cell::Cell::new(0u32));
        Effect::new(move |_| {
            let placed = placement.get();
            let generation = load_gen.get().wrapping_add(1);
            load_gen.set(generation);
            solved_image.borrow_mut().take();
            solved_image_epoch.update(|v| *v += 1);
            let Some((frame, ra, dec, pa, pix)) = placed else { return };
            let Ok(el) = web_sys::HtmlImageElement::new() else { return };
            let slot = Rc::clone(&solved_image);
            let gen_now = Rc::clone(&load_gen);
            let el_cl = el.clone();
            let onload = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                if gen_now.get() != generation {
                    return;
                }
                // `pix` is per solver pixel; the frame's resolution is that grid.
                slot.borrow_mut().replace(SolvedImage {
                    el: el_cl.clone(),
                    ra_deg: ra,
                    dec_deg: dec,
                    pa_deg: pa,
                    fov_w_deg: pix * frame.width as f64 / 3600.0,
                    fov_h_deg: pix * frame.height as f64 / 3600.0,
                });
                solved_image_epoch.update(|v| *v += 1);
            });
            el.set_onload(Some(onload.as_ref().unchecked_ref()));
            onload.forget();
            el.set_src(&frame.url);
        });
    }

    // ── Survey imagery (DsoImageLayer / AllskyLayer) ──────────────────────
    // Decoded sprites and the Milky Way panorama; `dso_images_epoch` wakes the
    // render Effect when a load lands. The tile index arrives from the server
    // once, through `DsoTilesCtx`.
    let dso_images_epoch = RwSignal::new(0u32);
    let dso_images: Rc<RefCell<dso_images::DsoImageCache>> = Rc::new(RefCell::new(
        dso_images::DsoImageCache::new(dso_images_epoch, mobile_profile.is_mobile),
    ));
    let dso_tiles_ctx = use_context::<crate::DsoTilesCtx>().map(|c| c.0);

    // ── FOV diagnostics ───────────────────────────────────────────────────
    // Log the inputs and the resulting reticle FOV whenever the camera geometry,
    // the last solve, or the nominal focal length changes (NOT per frame). Lets
    // us compare junos-web's FOV against KStars' Align FOV readout on real gear.
    Effect::new(move || {
        let cam = camera.get();
        let sv = solve.get();
        let nominal_fl = focal_length_mm.get();
        let frame = frame_calib.get();
        let fl = reticle_focal_mm(frame.as_ref().map(|(c, _)| c), &sv, &cam, nominal_fl);
        if let (Some(fl), Some(pum), Some(sw), Some(sh)) =
            (fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height)
        {
            let fov_w = crate::astro::fov_deg(fl, sw as f64, pum);
            let fov_h = crate::astro::fov_deg(fl, sh as f64, pum);
            debug_log!(
                "[sky] FOV inputs: fl={:.1}mm (nominal={:?}) sensor={}x{}px pixel={:.2}um bin={:?} pixscale={:?}\"/px kstars_fov={:?}' frame={:?} solve_fov={:?}deg -> {:.1}'x{:.1}'",
                fl, nominal_fl, sw, sh, pum, cam.bin_x, sv.pixscale_arcsec,
                sv.fov_arcmin, frame, solve_fov_deg(&sv, &cam),
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
    let is_mobile_for_gpu = mobile_profile.is_mobile;
    Effect::new(move || {
        let Some(gpu_canvas) = gpu_canvas_ref.get() else { return; };
        let Some(cat) = catalog_sig.get() else { return; };
        if gpu_ready.get_untracked() { return; }

        let gpu = Rc::clone(&gpu_for_init);
        let star_data = cat.packed_star_buffer();
        let line_data = cat.packed_line_buffer();
        let gpu_canvas_el: HtmlCanvasElement = gpu_canvas.clone().into();
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(renderer) =
                SkyRenderer::init(gpu_canvas_el, star_data, line_data, is_mobile_for_gpu).await
            {
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
    let solved_image_for_render = Rc::clone(&solved_image);
    let dso_images_for_render = Rc::clone(&dso_images);
    let _render_handle = Effect::new(move || {
        // Read all reactive deps to subscribe
        let m = mount.get();
        let cam = camera.get();
        let s = site.get();
        let sv = solve.get();
        let sched = scheduler.get();
        let mos = mosaic.get();
        let fov = fov_radius.get();
        let sim = clock.get();
        let layer_toggles = toggles.layer_toggles();
        let dso_filter = toggles.dso_filter();
        let dso_mag = dso_mag_limit.get();
        let _ = solved_image_epoch.get();
        let solved_image_opacity = toggles.solved_image_opacity.get();
        let _ = dso_images_epoch.get();
        let dso_images_brightness = toggles.dso_images_brightness.get();
        let milky_way_opacity = toggles.milky_way_opacity.get();
        // The tile index lands once; subscribing here redraws when it does.
        let dso_tiles = dso_tiles_ctx.and_then(|c| c.get());
        if let Ok(mut cache) = dso_images_for_render.try_borrow_mut() {
            cache.set_index(dso_tiles);
        }
        // Solve-derived when possible (see `reticle_focal_mm`); the FOV
        // reticle, mosaic preview and scheduler-job frames all read this `fl`.
        let nominal_fl = focal_length_mm.get();
        let frame = frame_calib.get();
        let fl = reticle_focal_mm(frame.as_ref().map(|(c, _)| c), &sv, &cam, nominal_fl);
        let camera_pa = frame.map(|(c, _)| c.pa_deg).or(sv.rotation_deg);
        let follow = follow_mount.get();
        let has_gpu = gpu_ready.get();
        let cur_lang = lang.get();
        let _frame = tick.get();
        let mosaic_planning_on = planner.planning.get();
        let mosaic_center_val = planner.params.center.get();
        let mosaic_gw = planner.params.grid_w.get();
        let mosaic_gh = planner.params.grid_h.get();
        let mosaic_overlap_pct = planner.params.overlap.get();
        let mosaic_pa_val = planner.params.pa.get();
        let mosaic_target_name = planner.params.target.get();

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
        let jd = sim.jd();
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
        let nominal_arcmin = match (nominal_fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height) {
            (Some(nfl), Some(pum), Some(sw), Some(sh)) => Some((
                astro::fov_deg(nfl, sw as f64, pum) * 60.0,
                astro::fov_deg(nfl, sh as f64, pum) * 60.0,
            )),
            _ => None,
        };
        let hud_frame = match frame {
            Some((c, src)) => Some(hud::HudFrame {
                fov_arcmin: (c.fov_w_arcmin, c.fov_h_arcmin),
                pa_deg: Some(c.pa_deg),
                origin: match src {
                    calib::CalibSource::Solved => hud::FrameOrigin::Solved(c.at_ms),
                    calib::CalibSource::Saved => hud::FrameOrigin::Saved(c.at_ms),
                },
                nominal_arcmin,
            }),
            None => nominal_arcmin.map(|n| hud::HudFrame {
                fov_arcmin: n,
                pa_deg: sv.rotation_deg,
                origin: hud::FrameOrigin::Nominal,
                nominal_arcmin: None,
            }),
        };
        set_hud_data.set(hud::HudData {
            lst_deg: lst,
            fov,
            c_alt,
            c_az,
            mount_ra_h: m.ra_h,
            mount_dec_deg: m.dec_deg,
            frame: hud_frame,
            cursor_altaz,
            cursor_radec,
        });

        // ── Derive scheduler job render list ───────────────────────────
        let scheduler_jobs_data = derive_scheduler_jobs(layer_toggles.scheduler_jobs_on, &sched, jd);

        // ── KStars mosaic tiles → MosaicPlanRender ─────────────────────
        let mosaic_kstars_render = derive_kstars_mosaic_plan(&mos, jd);

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
                    stars_on: layer_toggles.stars_on,
                    dso_on: layer_toggles.dso_on,
                    dso_filter,
                    dso_mag,
                    solar_on: layer_toggles.solar_system_on,
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
            sin_lat, cos_lat,
            mag_limit, cur_lang,
            is_mobile: mobile_profile.is_mobile,
        };
        let state = OverlayState {
            mount_connected: m.connected,
            mount_ra_h: m.ra_h,
            mount_dec_deg: m.dec_deg,
            fl,
            cam_pixel_size_um: cam.pixel_size_um,
            cam_sensor_width:  cam.sensor_width,
            cam_sensor_height: cam.sensor_height,
            rotation_deg: camera_pa,
            solve_ra_jnow_deg: sv.ra_jnow_deg,
            solve_dec_jnow_deg: sv.dec_jnow_deg,
            solve_fov_deg: solve_fov_deg(&sv, &cam),
            solved_image: solved_image_for_render.borrow().clone(),
            solved_image_opacity,
            solve_age_ms: sv.solved_at_ms.map(|t| js_sys::Date::now() - t),
            dso_images_brightness,
            milky_way_opacity,
            scheduler_jobs: scheduler_jobs_data,
            mosaic_kstars: mosaic_kstars_render,
            mosaic_plan:   mosaic_plan_render,
            dso_gx: dso_filter.gx, dso_oc: dso_filter.oc, dso_gc: dso_filter.gc,
            dso_nb: dso_filter.nb, dso_pn: dso_filter.pn, dso_snr: dso_filter.snr,
            dso_gal: dso_filter.gal, dso_mag,
        };
        let mode = PipelineMode::from_has_gpu(has_gpu);
        let catalogs = Catalogs {
            stars: cat.as_ref(),
            dso: dso_cat.as_ref(),
            dso_index: dso_idx.as_deref(),
        };
        let sprite_capacity = if has_gpu {
            gpu_for_render.try_borrow().ok().and_then(|o| o.as_ref().map(|r| r.sprite_capacity())).unwrap_or(0)
        } else {
            0
        };
        let mut frame = Frame {
            view: &view,
            scene: &scene,
            state: &state,
            toggles: &layer_toggles,
            mode,
            catalogs: &catalogs,
            hit_items: &mut hits,
            slew_trail: trail_slice,
            dso_images: Some(&dso_images_for_render),
            imaged: Vec::new(),
            sprite_capacity,
        };
        if has_gpu {
            if let (Some(uniforms), Ok(mut pipe), Ok(mut opt)) = (
                gpu_uniforms,
                pipeline_for_render.try_borrow_mut(),
                gpu_for_render.try_borrow_mut(),
            ) {
                if let Some(renderer) = opt.as_mut() {
                    pipe.run(&mut frame, &ctx, renderer.font_atlas());
                    let prep = pipe.gpu_prepare_mut();
                    prep.show_stars = layer_toggles.stars_on;
                    prep.show_constellations = layer_toggles.const_on;
                    renderer.submit_frame(prep, &uniforms);
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
            // Force the solver's post-solve action to "Slew to target" so the
            // goto+solve flow re-centers the mount after solving, regardless of
            // whatever GOTO mode is currently selected in Ekos' Align tab.
            // `slewR` is the object name of the align "Slew to target" radio.
            send_for_align_after_slew(
                serde_json::json!({"type":"align_set_all_settings","payload":{"slewR":true}})
                    .to_string(),
            );
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
            // Time-lapse playback counts as activity so the sky moves smoothly.
            let active = (now_ms - last_active.get()) < 500.0
                || clock.with_untracked(|c| c.rate != 1.0);
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

    // ── Pointer → sky ──────────────────────────────────────────────────────
    // Viewport (client) px → overlay-canvas CSS px.
    let to_canvas = move |client_x: f64, client_y: f64| -> Option<(f64, f64)> {
        let rect = overlay_ref.get_untracked()?.get_bounding_client_rect();
        Some((client_x - rect.left(), client_y - rect.top()))
    };
    let to_canvas_xy = move |ev: &MouseEvent| to_canvas(ev.client_x() as f64, ev.client_y() as f64);

    // Canvas CSS px → JNow RA/Dec (deg) at the displayed time. Same
    // projection as the render Effect (canvas height floored at 500 px).
    let screen_to_radec = move |x: f64, y: f64| -> (f64, f64) {
        let (w, h) = overlay_ref
            .get_untracked()
            .and_then(|el| el.parent_element())
            .map(|p| (p.client_width() as f64, p.client_height().max(500) as f64))
            .unwrap_or((800.0, 600.0));
        let s = site.get_untracked();
        let lst = astro::lst_deg(astro::gmst_deg(clock.get_untracked().jd()), s.longitude);
        let r = h.min(w) / 2.0;
        let (alt, az) = astro::unproject(
            (x - w / 2.0) / r,
            -(y - h / 2.0) / r,
            center_alt.get_untracked(),
            center_az.get_untracked(),
            fov_radius.get_untracked(),
        );
        astro::altaz_to_eq(alt, az, lst, s.latitude)
    };

    // Leaving the sky abandons a pending Mosaic "Pick on Sky".
    if let Some(ctx) = tab_ctx {
        Effect::new(move |_| {
            if ctx.0.get() != Tab::Sky && planner.picking_center.get_untracked() {
                planner.picking_center.set(false);
            }
        });
    }
    let cancel_pick = move |_| {
        planner.picking_center.set(false);
        if let Some(ctx) = tab_ctx {
            ctx.0.set(Tab::Mosaic);
        }
    };

    // Click / tap: Mosaic "Pick on Sky", else open the card on the object
    // under the pointer (or close it on empty sky).
    let hit_items_for_tap = Rc::clone(&hit_items);
    let on_tap = move |x: f64, y: f64, min_r: f64| {
        if planner.picking_center.get_untracked() {
            planner.params.center.set(Some(screen_to_radec(x, y)));
            planner.picking_center.set(false);
            planner.planning.set(true);
            if let Some(ctx) = tab_ctx {
                ctx.0.set(Tab::Mosaic);
            }
            return;
        }
        let hit = hit_test(&hit_items_for_tap.borrow(), x, y, min_r);
        target.set(hit.map(|h| SkyTarget::new(h.ra_jnow_deg, h.dec_jnow_deg, Some(h))));
    };

    // Right-click / long-press: open the card on the pressed point, snapped
    // to the object under it when there is one.
    let hit_items_for_press = Rc::clone(&hit_items);
    let on_press = move |x: f64, y: f64, min_r: f64| {
        let (ra, dec) = screen_to_radec(x, y);
        let hit = hit_test(&hit_items_for_press.borrow(), x, y, min_r);
        target.set(Some(SkyTarget::new(ra, dec, hit)));
    };

    // Drag by (dx, dy) CSS px: pan the view and stop following the mount.
    let pan = move |dx: f64, dy: f64| {
        if follow_mount.get_untracked() {
            set_follow_mount.set(false);
        }
        let deg_per_px = fov_radius.get_untracked() * 2.0 / 500.0;
        set_center_az.update(|az| *az = (*az - dx * deg_per_px).rem_euclid(360.0));
        set_center_alt.update(|alt| *alt = (*alt + dy * deg_per_px).clamp(-90.0, 90.0));
    };

    // ── Mouse handlers ─────────────────────────────────────────────────────
    let on_mousedown = move |ev: MouseEvent| {
        if ev.button() == 0 {
            dragging.set_value(true);
            drag_last.set_value((ev.client_x() as f64, ev.client_y() as f64));
            drag_dist.set_value(0.0);
        }
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
        pan(dx, dy);
    };

    let tap_for_mouse = on_tap.clone();
    let on_mouseup = move |ev: MouseEvent| {
        let was_down = dragging.get_value();
        dragging.set_value(false);
        // If the pointer barely moved, treat this as a click → hit-test.
        if !was_down || drag_dist.get_value() >= 4.0 || ev.button() != 0 { return; }
        if let Some((x, y)) = to_canvas_xy(&ev) {
            tap_for_mouse(x, y, MOUSE_HIT_R);
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

    let press_for_mouse = on_press.clone();
    let on_contextmenu = move |ev: MouseEvent| {
        ev.prevent_default();
        if let Some((x, y)) = to_canvas_xy(&ev) {
            press_for_mouse(x, y, MOUSE_HIT_R);
        }
    };

    // ── Touch handlers ─────────────────────────────────────────────────────
    // One finger: tap (< TAP_SLOP of travel) → on_tap, hold 500 ms →
    // on_press, drag → pan. Two fingers: pinch-zoom. `preventDefault` also
    // stops the browser's synthetic mouse events, so taps are handled here.
    let lp_timer_start = Rc::clone(&longpress_timer);
    let lp_timer_move  = Rc::clone(&longpress_timer);
    let lp_timer_end   = Rc::clone(&longpress_timer);

    let press_for_touch = on_press.clone();
    let on_touchstart = move |ev: TouchEvent| {
        ev.prevent_default();
        let touches = ev.touches();
        if touches.length() == 1 {
            let t = touches.get(0).unwrap();
            let (tx, ty) = (t.client_x() as f64, t.client_y() as f64);
            dragging.set_value(true);
            drag_last.set_value((tx, ty));
            drag_dist.set_value(0.0);
            pinch_start_dist.set_value(0.0);
            longpress_fired.set_value(false);
            let press = press_for_touch.clone();
            *lp_timer_start.borrow_mut() = Some(gloo_timers::callback::Timeout::new(500, move || {
                longpress_fired.set_value(true);
                if let Some((x, y)) = to_canvas(tx, ty) {
                    press(x, y, TOUCH_HIT_R);
                }
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
        let touches = ev.touches();
        if touches.length() == 1 && dragging.get_value() {
            let t = touches.get(0).unwrap();
            let (lx, ly) = drag_last.get_value();
            let dx = t.client_x() as f64 - lx;
            let dy = t.client_y() as f64 - ly;
            drag_last.set_value((t.client_x() as f64, t.client_y() as f64));
            drag_dist.update_value(|d| *d += (dx * dx + dy * dy).sqrt());
            if drag_dist.get_value() < TAP_SLOP { return; }
            *lp_timer_move.borrow_mut() = None; // a real drag cancels the long-press
            pan(dx, dy);
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

    let tap_for_touch = on_tap.clone();
    let on_touchend = move |ev: TouchEvent| {
        ev.prevent_default();
        *lp_timer_end.borrow_mut() = None; // lifted before 500ms — not a long-press
        let is_tap = dragging.get_value()
            && drag_dist.get_value() < TAP_SLOP
            && !longpress_fired.get_value();
        dragging.set_value(false);
        pinch_start_dist.set_value(0.0);
        if is_tap {
            let (cx, cy) = drag_last.get_value();
            if let Some((x, y)) = to_canvas(cx, cy) {
                tap_for_touch(x, y, TOUCH_HIT_R);
            }
        }
    };

    let send_for_card = Arc::clone(&send);
    let icon_btn = |on: bool| {
        let base = "btn-icon shrink-0 pointer-events-auto";
        if on { format!("{base} btn--active") } else { format!("{base} text-text-blue") }
    };

    view! {
        <div class="relative w-full h-[100dvh] overflow-hidden">

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
            // Sits just under the top bar (mt-14 clears its 44 px row).
            {move || planner.picking_center.get().then(|| view! {
                <div class="absolute z-[100] top-[max(0.5rem,env(safe-area-inset-top))] mt-14 left-1/2 -translate-x-1/2 \
                            w-max max-w-[calc(100%-1rem)] flex items-center gap-3 py-1.5 pl-4 pr-1.5 \
                            bg-bg-banner border border-accent-cyan-dim text-accent-cyan text-sm rounded-md">
                    <span class="min-w-0">{move || tr().mosaic_pick_hint}</span>
                    <button class="btn btn--sm btn-ghost shrink-0" on:click=cancel_pick>{move || tr().cancel}</button>
                </div>
            })}

            // ── Top bar: search · follow mount · layers ────────────────────
            <div class="absolute z-50 top-[max(0.5rem,env(safe-area-inset-top))] left-2 right-2 md:right-[72px] \
                        flex items-start gap-2 pointer-events-none">
                <SkySearch
                    sky_search=sky_search
                    set_sky_search=set_sky_search
                    catalog_sig=catalog_sig
                    dso_catalog_sig=dso_catalog_sig
                    site=site
                    clock=clock
                    set_center_alt=set_center_alt
                    set_center_az=set_center_az
                    set_follow_mount=set_follow_mount
                    set_fov_radius=set_fov_radius
                    dso_mag_limit=dso_mag_limit
                />
                <button class=move || {
                            let c = icon_btn(follow_mount.get());
                            if mount.with(|m| m.connected) { format!("{c} ml-auto") } else { format!("{c} ml-auto opacity-50") }
                        }
                        title=move || tr().follow_mount
                        on:click=move |_| set_follow_mount.update(|f| *f = !*f)>
                    <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
                        <circle cx="12" cy="12" r="6.5" />
                        <path d="M12 2v5M12 17v5M2 12h5M17 12h5" />
                    </svg>
                </button>
                <button class=move || icon_btn(layers_open.get())
                        title=move || tr().layers
                        on:click=move |_| layers_open.update(|v| *v = !*v)>
                    <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round">
                        <path d="M12 3 2 8l10 5 10-5z" />
                        <path d="m2 13 10 5 10-5" />
                    </svg>
                </button>
            </div>

            // ── Bottom stack: HUD above the time bar ───────────────────────
            <div class="absolute z-50 left-2 right-2 md:left-3 md:right-auto bottom-[max(0.5rem,env(safe-area-inset-bottom))] \
                        flex flex-col gap-2 items-start pointer-events-none">
                <hud::SkyHud hud=hud_data lang=lang.read_only() />
                <TimeBar clock=clock tick=tick site=site />
            </div>

            // ── Layers panel ───────────────────────────────────────────────
            <SkyControls
                open=layers_open
                toggles=toggles
                site=site
                set_site_location=set_site_location
                mount_device=mount_device
            />

            // ── Framing assistant overlay (opened from the target card) ────
            <FramingOverlay
                camera=camera
                focal_length_mm=focal_length_mm
                catalog_sig=catalog_sig
                dso_catalog_sig=dso_catalog_sig
            />

            // ── Target card (tap, right-click, long-press) ─────────────────
            <SkyTargetCard
                target=target
                clock=clock
                site=site
                pending_solve_after_slew=pending_solve_after_slew
                send=send_for_card
                set_center_alt=set_center_alt
                set_center_az=set_center_az
                set_follow_mount=set_follow_mount
            />
        </div>
    }
}
