//! Constellation name labels (GPU-mode Canvas2D companion).
//!
//! In `Gpu` mode the GPU `LineLayer` draws constellation lines; this
//! layer paints the i18n'd constellation names on top via the legacy
//! `render_constellation_names_gpu` fn. In `Canvas2dFallback` the names
//! are drawn by `render_fallback_stars`'s pass over the catalog (the
//! legacy non-GPU code path), so this layer no-ops there.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::params::PipelineMode;
use super::super::render_constellation_names_gpu;

pub struct ConstellationNamesLayer;

impl SkyLayer for ConstellationNamesLayer {
    fn name(&self) -> &'static str { "constellation_names" }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.const_on && f.toggles.con_names_on
    }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Gpu { return; }
        let view = *f.view;
        let proj = |alt: f64, az: f64| {
            crate::astro::project(alt, az, view.c_alt, view.c_az, view.fov)
                .map(|(x, y)| (view.cx + x * view.scale, view.cy - y * view.scale))
        };
        let cat_owned = f.catalogs.stars.cloned();
        render_constellation_names_gpu(ctx, f.legacy_params, &cat_owned, &proj);
    }
}
