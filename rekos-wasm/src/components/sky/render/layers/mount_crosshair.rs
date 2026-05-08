//! Mount crosshair: green cross + red dot at the mount's reported position.
//!
//! In `Gpu` mode geometry is appended via `gpu_lines::build_mount_crosshair`
//! inline in `mod.rs`. In `Canvas2dFallback` mode this layer paints it.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use super::super::params::PipelineMode;
use crate::components::sky::gpu::layers::lines as gpu_lines;

pub struct MountCrosshairLayer;

impl SkyLayer for MountCrosshairLayer {
    fn name(&self) -> &'static str {
        "mount_crosshair"
    }
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = gpu else { return };
        gpu_lines::build_mount_crosshair(
            &mut gpu.lines,
            &line_view(f),
            f.state.mount_connected,
            f.state.mount_ra_h,
            f.state.mount_dec_deg,
        );
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        if !f.state.mount_connected {
            return;
        }
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        if let (Some(ra_h), Some(dec)) = (f.state.mount_ra_h, f.state.mount_dec_deg) {
            let (malt, maz) =
                crate::astro::eq_to_altaz(ra_h * 15.0, dec, f.scene.lst, f.scene.latitude);
            if let Some((mx, my)) = proj(malt, maz) {
                let arm = 14.0;
                ctx.set_stroke_style_str("#44ff44");
                ctx.set_line_width(1.5);
                ctx.begin_path();
                ctx.move_to(mx - arm, my);
                ctx.line_to(mx - 4.0, my);
                ctx.move_to(mx + 4.0, my);
                ctx.line_to(mx + arm, my);
                ctx.move_to(mx, my - arm);
                ctx.line_to(mx, my - 4.0);
                ctx.move_to(mx, my + 4.0);
                ctx.line_to(mx, my + arm);
                ctx.stroke();

                ctx.set_fill_style_str("#ff4444");
                ctx.begin_path();
                let _ = ctx.arc(mx, my, 2.0, 0.0, 2.0 * std::f64::consts::PI);
                ctx.fill();
            }
        }
    }
}
