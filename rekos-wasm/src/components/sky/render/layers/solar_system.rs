//! Sun, Moon, planets — Canvas2D path. In `Gpu` mode the GPU `DsoLayer`
//! (filled-disc kind) draws the bodies + the `TextLayer` labels them, so
//! this layer short-circuits when `solar_on_gpu` is set.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use super::super::render_solar_system;
use crate::components::sky::gpu::{FontAtlas, TextInstance};
use crate::components::sky::solar_render;

pub struct SolarSystemLayer;

impl SkyLayer for SolarSystemLayer {
    fn name(&self) -> &'static str {
        "solar_system"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.solar_system_on
    }
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = gpu else { return };
        let mut scratch_text: Vec<TextInstance> = Vec::new();
        solar_render::build(
            &line_view(f),
            false,
            f.scene.cur_lang,
            None,
            &mut gpu.dso,
            &mut scratch_text,
        );
    }

    fn prepare_gpu_text(&self, f: &mut Frame, atlas: &FontAtlas, out: &mut Vec<TextInstance>) {
        if !f.mode.is_gpu() {
            return;
        }
        let mut scratch_dso = Vec::new();
        solar_render::build(
            &line_view(f),
            f.toggles.names_on,
            f.scene.cur_lang,
            Some(atlas),
            &mut scratch_dso,
            out,
        );
    }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode.is_gpu() {
            return;
        }
        let view = *f.view;
        let proj = |alt: f64, az: f64| {
            crate::astro::project(alt, az, view.c_alt, view.c_az, view.fov)
                .map(|(x, y)| (view.cx + x * view.scale, view.cy - y * view.scale))
        };
        render_solar_system(ctx, f, &proj);
    }
}
