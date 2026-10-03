//! Grouped per-frame parameter structs for the render pipeline.
//!
//! Four orthogonal groups (geometry, scene, overlay state, layer toggles)
//! plus a single `PipelineMode` enum that replaces what used to be six
//! pairwise `_on_gpu` / `_on_dom` / `_on_cpu` booleans.

use crate::i18n::Lang;

use super::{MosaicPlanRender, SchedulerJobRender};

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
        if has_gpu {
            Self::Gpu
        } else {
            Self::Canvas2dFallback
        }
    }

    pub fn is_gpu(self) -> bool {
        matches!(self, Self::Gpu)
    }
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

/// Time, observer, magnitude limit, and language. Anything that doesn't
/// belong to a specific subsystem (mount/camera/solve/mosaic/scheduler).
#[derive(Clone, Copy)]
pub struct SceneParams {
    pub jd: f64,
    pub lst: f64,
    pub latitude: f64,
    pub sin_lat: f64,
    pub cos_lat: f64,
    pub mag_limit: f32,
    pub cur_lang: Lang,
    pub is_mobile: bool,
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
    pub solved_image_on: bool,
}

/// Borrowed/cloned subsystem state read by overlay layers (mount, camera,
/// solve, mosaic, scheduler). Cloned today during migration; will
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
    /// Width × height (deg) of the solved frame, from `solve_fov_deg`.
    pub solve_fov_deg: Option<(f64, f64)>,
    /// The last solve's Align frame, once its JPEG has loaded.
    pub solved_image: Option<SolvedImage>,
    pub solved_image_opacity: f64,
    pub solve_age_ms: Option<f64>,

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

/// The Align frame of the last solve, placed on the sky (`SolvedImageLayer`).
#[derive(Clone)]
pub struct SolvedImage {
    pub el:        web_sys::HtmlImageElement,
    pub ra_deg:    f64, // JNow, like the rest of the solve
    pub dec_deg:   f64,
    pub pa_deg:    f64,
    pub fov_w_deg: f64,
    pub fov_h_deg: f64,
}

/// Camera PA (° east of north) assumed until a plate solve reports one.
/// Without a rotator the camera is fixed to the OTA, so on an equatorial
/// mount its frame stays locked to RA/Dec; by default the sensor's long (X)
/// side lies along the meridian, i.e. PA 90°.
pub const DEFAULT_CAMERA_PA_DEG: f64 = 90.0;

impl OverlayState {
    /// Solved camera PA, or `DEFAULT_CAMERA_PA_DEG` before any solve.
    pub fn camera_pa_deg(&self) -> f64 {
        self.rotation_deg.unwrap_or(DEFAULT_CAMERA_PA_DEG)
    }
}
