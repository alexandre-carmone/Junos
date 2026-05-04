//! Line-grid layers (alt-az grid, equatorial grid, meridian, ecliptic).
//!
//! In `Gpu` mode these are drawn by the GPU `LineLayer` (geometry built
//! inline in `mod.rs` via `gpu_lines::build_*`). In `Canvas2dFallback`
//! mode each layer paints by delegating to the legacy `render_*` free fn
//! kept in `super::super::*`. Step 8 will inline those bodies and drop
//! the `legacy_params` borrow.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, GpuPrepare, SkyLayer};
use super::super::params::PipelineMode;
use super::super::{
    render_altaz_grid, render_ecliptic, render_eq_grid, render_meridian,
};

fn project<'a>(f: &'a Frame<'a>) -> impl Fn(f64, f64) -> Option<(f64, f64)> + 'a {
    move |alt, az| f.project(alt, az)
}

pub struct AltAzGridLayer;
impl SkyLayer for AltAzGridLayer {
    fn name(&self) -> &'static str { "altaz_grid" }
    fn enabled(&self, f: &Frame) -> bool { f.toggles.grid_on }
    fn prepare(&mut self, _f: &mut Frame, _g: Option<&mut GpuPrepare>) {}
    fn draw_canvas2d(&self, f: &Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback { return; }
        let proj = project(f);
        render_altaz_grid(ctx, f.legacy_params, &proj);
    }
}

pub struct MeridianLayer;
impl SkyLayer for MeridianLayer {
    fn name(&self) -> &'static str { "meridian" }
    fn enabled(&self, f: &Frame) -> bool { f.toggles.meridian_on }
    fn draw_canvas2d(&self, f: &Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback { return; }
        let proj = project(f);
        render_meridian(ctx, f.legacy_params, &proj);
    }
}

pub struct EqGridLayer;
impl SkyLayer for EqGridLayer {
    fn name(&self) -> &'static str { "eq_grid" }
    fn enabled(&self, f: &Frame) -> bool { f.toggles.eq_grid_on }
    fn draw_canvas2d(&self, f: &Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback { return; }
        let proj = project(f);
        render_eq_grid(ctx, f.legacy_params, &proj);
    }
}

pub struct EclipticLayer;
impl SkyLayer for EclipticLayer {
    fn name(&self) -> &'static str { "ecliptic" }
    fn enabled(&self, f: &Frame) -> bool { f.toggles.ecliptic_on }
    fn draw_canvas2d(&self, f: &Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback { return; }
        let proj = project(f);
        render_ecliptic(ctx, f.legacy_params, &proj);
    }
}
