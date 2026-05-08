//! FOV reticles: center-anchored + mount-anchored camera-frame rectangles.
//!
//! In `Gpu` mode the rectangles are drawn by the GPU `LineLayer` and their
//! "<arcmin> <km/s>"-style labels by the `TextLayer` (instances assembled
//! inline in `mod.rs:628-666`). Canvas2D mode delegates to the legacy fns.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::params::PipelineMode;
use super::super::{render_center_fov, render_mount_fov};

pub struct FovReticleLayer;

impl SkyLayer for FovReticleLayer {
    fn name(&self) -> &'static str {
        "fov_reticle"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.fov_on
    }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        render_center_fov(ctx, f, &proj, f.view.cx, f.view.cy);
        render_mount_fov(ctx, f, &proj, f.view.cx, f.view.cy);
    }
}
