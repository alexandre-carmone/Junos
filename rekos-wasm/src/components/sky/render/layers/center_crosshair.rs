//! Center crosshair: small cross + ring at the canvas center.
//!
//! In `Gpu` mode the GPU `LineLayer` draws this geometry via
//! `gpu_lines::build_center_crosshair` (still assembled inline in `mod.rs`
//! until the line-grid layers migrate). In `Canvas2dFallback` mode this
//! layer paints it on the overlay.

use std::f64::consts::PI;

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, GpuPrepare, SkyLayer};
use super::super::params::PipelineMode;

pub struct CenterCrosshairLayer;

impl SkyLayer for CenterCrosshairLayer {
    fn name(&self) -> &'static str {
        "center_crosshair"
    }

    fn prepare(&mut self, _f: &mut Frame, _gpu: Option<&mut GpuPrepare>) {}

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let cx = f.view.cx;
        let cy = f.view.cy;
        let arm = 18.0;
        let gap = 6.0;
        ctx.set_stroke_style_str("rgba(180,220,255,0.75)");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(cx - arm, cy);
        ctx.line_to(cx - gap, cy);
        ctx.move_to(cx + gap, cy);
        ctx.line_to(cx + arm, cy);
        ctx.move_to(cx, cy - arm);
        ctx.line_to(cx, cy - gap);
        ctx.move_to(cx, cy + gap);
        ctx.line_to(cx, cy + arm);
        ctx.stroke();
        ctx.begin_path();
        let _ = ctx.arc(cx, cy, gap, 0.0, 2.0 * PI);
        ctx.stroke();
    }
}
