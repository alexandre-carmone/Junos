//! Line-grid layers (alt-az grid, equatorial grid, meridian, ecliptic).
//!
//! In `Gpu` mode these are drawn by the GPU `LineLayer` (geometry built
//! inline in `mod.rs` via `gpu_lines::build_*`). In `Canvas2dFallback`
//! mode each layer paints by delegating to the legacy `render_*` free fn
//! kept in `super::super::*`. Step 8 will inline those bodies and drop
//! the `legacy_params` borrow.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use super::super::params::PipelineMode;
use crate::components::sky::gpu::layers::lines as gpu_lines;

fn project(view: super::super::ViewParams) -> impl Fn(f64, f64) -> Option<(f64, f64)> {
    move |alt, az| super::super::layer::project_with(view, alt, az)
}

pub struct AltAzGridLayer;
impl SkyLayer for AltAzGridLayer {
    fn name(&self) -> &'static str {
        "altaz_grid"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.grid_on
    }
    fn prepare(&mut self, f: &mut Frame, g: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = g else { return };
        gpu_lines::build_altaz_grid(&mut gpu.lines, &line_view(f));
    }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let view = *f.view;
        let proj = project(view);
        let az_step = if f.view.fov > 60.0 {
            6
        } else if f.view.fov > 20.0 {
            4
        } else {
            3
        };
        let alt_step = if f.view.fov > 60.0 { 10 } else { 5 };

        ctx.set_stroke_style_str("rgba(60,60,100,0.6)");
        ctx.set_line_width(0.7);

        ctx.begin_path();
        for alt_i in (-1..=9).map(|i| i * 10) {
            let mut first = true;
            for az_i in (0..=360).step_by(az_step) {
                if let Some((sx, sy)) = proj(alt_i as f64, az_i as f64) {
                    if first {
                        ctx.move_to(sx, sy);
                        first = false;
                    } else {
                        ctx.line_to(sx, sy);
                    }
                }
            }
        }
        ctx.stroke();

        ctx.begin_path();
        for az_i in (0..360).step_by(30) {
            let mut first = true;
            for alt_i in (0..=90).step_by(alt_step) {
                if let Some((sx, sy)) = proj(alt_i as f64, az_i as f64) {
                    if first {
                        ctx.move_to(sx, sy);
                        first = false;
                    } else {
                        ctx.line_to(sx, sy);
                    }
                }
            }
        }
        ctx.stroke();
    }
}

pub struct MeridianLayer;
impl SkyLayer for MeridianLayer {
    fn name(&self) -> &'static str {
        "meridian"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.meridian_on
    }
    fn prepare(&mut self, f: &mut Frame, g: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = g else { return };
        gpu_lines::build_meridian(&mut gpu.lines, &line_view(f));
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let view = *f.view;
        let proj = project(view);
        ctx.set_stroke_style_str("rgba(80,80,140,0.7)");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        let mut first = true;
        for alt_i in (0..=90).step_by(3) {
            if let Some((sx, sy)) = proj(alt_i as f64, 0.0) {
                if first {
                    ctx.move_to(sx, sy);
                    first = false;
                } else {
                    ctx.line_to(sx, sy);
                }
            }
        }
        for alt_i in (0..=90).rev().step_by(3) {
            if let Some((sx, sy)) = proj(alt_i as f64, 180.0) {
                ctx.line_to(sx, sy);
            }
        }
        ctx.stroke();
    }
}

pub struct EqGridLayer;
impl SkyLayer for EqGridLayer {
    fn name(&self) -> &'static str {
        "eq_grid"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.eq_grid_on
    }
    fn prepare(&mut self, f: &mut Frame, g: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = g else { return };
        gpu_lines::build_eq_grid(&mut gpu.lines, &line_view(f));
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let view = *f.view;
        let proj = project(view);
        let project_eq = |ra_deg: f64, dec_deg: f64| -> Option<(f64, f64)> {
            let (alt, az) =
                crate::astro::eq_to_altaz(ra_deg, dec_deg, f.scene.lst, f.scene.latitude);
            proj(alt, az)
        };
        let step = if f.view.fov > 60.0 {
            6
        } else if f.view.fov > 20.0 {
            4
        } else {
            2
        };

        ctx.set_stroke_style_str("rgba(100,100,200,0.6)");
        ctx.set_line_width(0.7);
        ctx.begin_path();
        for dec_i in (-3..=3).map(|i| i * 30) {
            let mut first = true;
            for ra_i in (0..=360).step_by(step) {
                if let Some((sx, sy)) = project_eq(ra_i as f64, dec_i as f64) {
                    if first {
                        ctx.move_to(sx, sy);
                        first = false;
                    } else {
                        ctx.line_to(sx, sy);
                    }
                } else {
                    first = true;
                }
            }
        }
        ctx.stroke();

        ctx.begin_path();
        for ra_i in (0..12).map(|i| i * 30) {
            let mut first = true;
            for dec_i in (-90..=90).step_by(step) {
                if let Some((sx, sy)) = project_eq(ra_i as f64, dec_i as f64) {
                    if first {
                        ctx.move_to(sx, sy);
                        first = false;
                    } else {
                        ctx.line_to(sx, sy);
                    }
                } else {
                    first = true;
                }
            }
        }
        ctx.stroke();

        ctx.set_stroke_style_str("rgba(120,120,255,0.8)");
        ctx.set_line_width(1.2);
        ctx.begin_path();
        let mut first = true;
        for ra_i in (0..=360).step_by(step) {
            if let Some((sx, sy)) = project_eq(ra_i as f64, 0.0) {
                if first {
                    ctx.move_to(sx, sy);
                    first = false;
                } else {
                    ctx.line_to(sx, sy);
                }
            } else {
                first = true;
            }
        }
        ctx.stroke();
    }
}

pub struct EclipticLayer;
impl SkyLayer for EclipticLayer {
    fn name(&self) -> &'static str {
        "ecliptic"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.ecliptic_on
    }
    fn prepare(&mut self, f: &mut Frame, g: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = g else { return };
        gpu_lines::build_ecliptic(&mut gpu.lines, &line_view(f));
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let view = *f.view;
        let proj = project(view);
        let t = (f.scene.jd - 2_451_545.0) / 36525.0;
        let ecl_deg = 23.4393 - 0.0130042 * t;
        let sin_eps = ecl_deg.to_radians().sin();
        let cos_eps = ecl_deg.to_radians().cos();

        ctx.set_stroke_style_str("rgba(220,180,80,0.7)");
        ctx.set_line_width(1.2);
        let dashes = js_sys::Array::new();
        dashes.push(&6.into());
        dashes.push(&4.into());
        let _ = ctx.set_line_dash(&dashes);

        ctx.begin_path();
        let mut first = true;
        for deg_i in (0..=360).step_by(2) {
            let lam = (deg_i as f64).to_radians();
            let x = lam.cos();
            let y = lam.sin() * cos_eps;
            let z = lam.sin() * sin_eps;
            let ra = y.atan2(x).to_degrees().rem_euclid(360.0);
            let dec = z.asin().to_degrees();
            let (alt, az) = crate::astro::eq_to_altaz(ra, dec, f.scene.lst, f.scene.latitude);
            if let Some((sx, sy)) = proj(alt, az) {
                if first {
                    ctx.move_to(sx, sy);
                    first = false;
                } else {
                    ctx.line_to(sx, sy);
                }
            } else {
                first = true;
            }
        }
        ctx.stroke();
        let _ = ctx.set_line_dash(&js_sys::Array::new());
    }
}
