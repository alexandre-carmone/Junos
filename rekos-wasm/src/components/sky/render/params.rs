//! Grouped per-frame parameter structs for the new render pipeline.
//!
//! The legacy `RenderParams` god-struct in `super` blends camera, scene, mount,
//! solve, mosaic, scheduler, language, and per-layer toggles. The pipeline
//! refactor splits those into four orthogonal groups and replaces the lattice
//! of `_on_gpu` / `_on_dom` / `_on_cpu` flags with a single `PipelineMode`.
//!
//! During migration these are constructed via `*::from_render_params(&p)` so
//! both old and new pipelines see identical data. Once every layer is
//! migrated, `RenderParams` and the `From` shims are removed.

use crate::i18n::Lang;

use super::{MosaicPlanRender, RenderParams, SchedulerJobRender};

/// Whether to drive the GPU path or fall back to all-Canvas2D.
///
/// Replaces the six pairwise `_on_gpu` / `_on_dom` / `_on_cpu` booleans the
/// old code carried in `RenderParams`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineMode {
    Gpu,
    Canvas2dFallback,
}

impl PipelineMode {
    pub fn from_has_gpu(has_gpu: bool) -> Self {
        if has_gpu { Self::Gpu } else { Self::Canvas2dFallback }
    }

    pub fn is_gpu(self) -> bool { matches!(self, Self::Gpu) }
}

/// Geometry and projection: canvas size, view center, FOV, and the
/// derived center / scale used by the projection closure.
#[derive(Clone, Copy)]
pub struct ViewParams {
    pub wf: f64,
    pub hf: f64,
    pub c_alt: f64,
    pub c_az: f64,
    pub fov: f64,
    pub cx: f64,
    pub cy: f64,
    pub scale: f64,
}

impl ViewParams {
    pub fn from_render_params(p: &RenderParams) -> Self {
        let cx = p.wf / 2.0;
        let cy = p.hf / 2.0;
        let scale = p.hf.min(p.wf) / 2.0;
        Self { wf: p.wf, hf: p.hf, c_alt: p.c_alt, c_az: p.c_az, fov: p.fov, cx, cy, scale }
    }
}

/// Time, observer, magnitude limit, and language. Anything that doesn't
/// belong to a specific subsystem (mount/camera/solve/mosaic/scheduler).
#[derive(Clone, Copy)]
pub struct SceneParams {
    pub jd: f64,
    pub lst: f64,
    pub latitude: f64,
    pub sin_lat: f64,
    pub cos_lat: f64,
    pub t_off: f64,
    pub mag_limit: f32,
    pub cur_lang: Lang,
    pub is_mobile: bool,
}

impl SceneParams {
    pub fn from_render_params(p: &RenderParams) -> Self {
        Self {
            jd: p.jd,
            lst: p.lst,
            latitude: p.latitude,
            sin_lat: p.sin_lat,
            cos_lat: p.cos_lat,
            t_off: p.t_off,
            mag_limit: p.mag_limit,
            cur_lang: p.cur_lang,
            is_mobile: p.is_mobile,
        }
    }
}

/// Per-layer on/off toggles surfaced in the right-panel controls. All booleans
/// here are user-visible; pipeline-mode plumbing belongs on `PipelineMode`.
#[derive(Clone, Copy, Default)]
pub struct LayerToggles {
    pub stars_on: bool,
    pub names_on: bool,
    pub const_on: bool,
    pub con_names_on: bool,
    pub grid_on: bool,
    pub eq_grid_on: bool,
    pub meridian_on: bool,
    pub ecliptic_on: bool,
    pub zenith_on: bool,
    pub solar_system_on: bool,
    pub solve_marker_on: bool,
    pub slew_trail_on: bool,
    pub fov_on: bool,
    pub dso_on: bool,
    pub scheduler_jobs_on: bool,
}

impl LayerToggles {
    pub fn from_render_params(p: &RenderParams) -> Self {
        Self {
            stars_on: p.stars_on,
            names_on: p.names_on,
            const_on: p.const_on,
            con_names_on: p.con_names_on,
            grid_on: p.grid_on,
            eq_grid_on: p.eq_grid_on,
            meridian_on: p.meridian_on,
            ecliptic_on: p.ecliptic_on,
            zenith_on: p.zenith_on,
            solar_system_on: p.solar_system_on,
            solve_marker_on: p.solve_marker_on,
            slew_trail_on: p.slew_trail_on,
            fov_on: p.fov_on,
            dso_on: p.dso_on,
            scheduler_jobs_on: p.scheduler_jobs_on,
        }
    }
}

/// Borrowed/cloned subsystem state read by overlay layers (mount, camera,
/// solve, mosaic, scheduler, cursor). Cloned today during migration; will
/// switch to borrows once the pipeline owns the lifetime story.
#[derive(Clone)]
pub struct OverlayState {
    pub mount_connected: bool,
    pub mount_ra_h: Option<f64>,
    pub mount_dec_deg: Option<f64>,

    pub fl: Option<f64>,
    pub cam_pixel_size_um: Option<f64>,
    pub cam_sensor_width: Option<u32>,
    pub cam_sensor_height: Option<u32>,
    pub rotation_deg: Option<f64>,

    pub solve_ra_jnow_deg: Option<f64>,
    pub solve_dec_jnow_deg: Option<f64>,
    pub solve_pixscale_arcsec: Option<f64>,
    pub solve_age_ms: Option<f64>,

    pub cursor_altaz: Option<(f64, f64)>,
    pub cursor_radec: Option<(f64, f64)>,

    pub scheduler_jobs: Vec<SchedulerJobRender>,
    pub mosaic_kstars: Option<MosaicPlanRender>,
    pub mosaic_plan: Option<MosaicPlanRender>,

    pub dso_gx: bool,
    pub dso_oc: bool,
    pub dso_gc: bool,
    pub dso_nb: bool,
    pub dso_pn: bool,
    pub dso_snr: bool,
    pub dso_gal: bool,
    pub dso_mag: f64,
}

impl OverlayState {
    pub fn from_render_params(p: &RenderParams) -> Self {
        Self {
            mount_connected: p.mount_connected,
            mount_ra_h: p.mount_ra_h,
            mount_dec_deg: p.mount_dec_deg,
            fl: p.fl,
            cam_pixel_size_um: p.cam_pixel_size_um,
            cam_sensor_width: p.cam_sensor_width,
            cam_sensor_height: p.cam_sensor_height,
            rotation_deg: p.rotation_deg,
            solve_ra_jnow_deg: p.solve_ra_jnow_deg,
            solve_dec_jnow_deg: p.solve_dec_jnow_deg,
            solve_pixscale_arcsec: p.solve_pixscale_arcsec,
            solve_age_ms: p.solve_age_ms,
            cursor_altaz: p.cursor_altaz,
            cursor_radec: p.cursor_radec,
            scheduler_jobs: p.scheduler_jobs.clone(),
            mosaic_kstars: p.mosaic_kstars.clone(),
            mosaic_plan: p.mosaic_plan.clone(),
            dso_gx: p.dso_gx,
            dso_oc: p.dso_oc,
            dso_gc: p.dso_gc,
            dso_nb: p.dso_nb,
            dso_pn: p.dso_pn,
            dso_snr: p.dso_snr,
            dso_gal: p.dso_gal,
            dso_mag: p.dso_mag,
        }
    }
}
