//! Mount crosshair: green cross + red dot at the mount's reported position.
//!
//! In `Gpu` mode geometry is appended via `gpu_lines::build_mount_crosshair`
//! inline in `mod.rs`. In `Canvas2dFallback` mode this layer paints it.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::params::PipelineMode;
use super::super::render_mount_crosshair;

pub struct MountCrosshairLayer;

impl SkyLayer for MountCrosshairLayer {
    fn name(&self) -> &'static str { "mount_crosshair" }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback { return; }
        let proj = |alt: f64, az: f64| f.project(alt, az);
        render_mount_crosshair(ctx, f.legacy_params, &proj);
    }
}
