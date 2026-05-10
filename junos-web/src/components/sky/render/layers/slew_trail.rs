//! Slew trail: fading orange polyline of the mount's recent trajectory.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use super::super::params::PipelineMode;
use crate::components::sky::gpu::layers::lines as gpu_lines;

pub struct SlewTrailLayer;

impl SkyLayer for SlewTrailLayer {
    fn name(&self) -> &'static str {
        "slew_trail"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.slew_trail_on
    }
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = gpu else { return };
        gpu_lines::build_slew_trail(&mut gpu.lines, &line_view(f), f.slew_trail);
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        if f.slew_trail.is_empty() {
            return;
        }
        let now_jd = f.scene.jd;
        const FADE_DAYS: f64 = 60.0 / 86400.0;
        ctx.set_line_width(1.5);
        ctx.set_line_cap("round");
        for w in f.slew_trail.windows(2) {
            let (jd0, ra0, de0) = w[0];
            let (_jd1, ra1, de1) = w[1];
            let age = (now_jd - jd0).max(0.0);
            let alpha = (1.0 - (age / FADE_DAYS).min(1.0)) * 0.9;
            if alpha < 0.05 {
                continue;
            }
            let (a0, az0) = crate::astro::eq_to_altaz(ra0, de0, f.scene.lst, f.scene.latitude);
            let (a1, az1) = crate::astro::eq_to_altaz(ra1, de1, f.scene.lst, f.scene.latitude);
            let (Some(s0), Some(s1)) = (proj(a0, az0), proj(a1, az1)) else {
                continue;
            };
            ctx.set_stroke_style_str(&format!("rgba(255,170,60,{alpha:.3})"));
            ctx.begin_path();
            ctx.move_to(s0.0, s0.1);
            ctx.line_to(s1.0, s1.1);
            ctx.stroke();
        }
    }
}
