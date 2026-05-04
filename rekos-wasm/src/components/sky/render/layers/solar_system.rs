//! Sun, Moon, planets — Canvas2D path. In `Gpu` mode the GPU `DsoLayer`
//! (filled-disc kind) draws the bodies + the `TextLayer` labels them, so
//! this layer short-circuits when `solar_on_gpu` is set.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::render_solar_system;

pub struct SolarSystemLayer;

impl SkyLayer for SolarSystemLayer {
    fn name(&self) -> &'static str { "solar_system" }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.solar_system_on && !f.legacy_params.solar_on_gpu
    }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let view = *f.view;
        let proj = |alt: f64, az: f64| {
            crate::astro::project(alt, az, view.c_alt, view.c_az, view.fov)
                .map(|(x, y)| (view.cx + x * view.scale, view.cy - y * view.scale))
        };
        render_solar_system(ctx, f.legacy_params, &proj, f.hit_items);
    }
}
