//! Plate-solve result marker: ring + gap-crosshair + rotated FOV rect.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use super::super::params::PipelineMode;
use crate::components::sky::gpu::layers::lines as gpu_lines;
use crate::components::sky::gpu::{FontAtlas, TextInstance};
use crate::i18n::t;

pub struct SolveMarkerLayer;

impl SkyLayer for SolveMarkerLayer {
    fn name(&self) -> &'static str {
        "solve_marker"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.solve_marker_on
    }
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = gpu else { return };
        let (Some(ra), Some(dec)) = (f.state.solve_ra_jnow_deg, f.state.solve_dec_jnow_deg) else {
            return;
        };
        let alpha = f
            .state
            .solve_age_ms
            .map(|age| (1.0 - (age / 600_000.0).min(1.0)) * 0.95 + 0.05)
            .unwrap_or(1.0) as f32;
        if alpha < 0.1 {
            return;
        }
        gpu_lines::build_solve_marker(
            &mut gpu.lines,
            &line_view(f),
            ra,
            dec,
            alpha,
            f.state.solve_pixscale_arcsec,
            f.state.cam_sensor_width,
            f.state.cam_sensor_height,
            f.state.rotation_deg,
        );
    }

    fn prepare_gpu_text(&self, f: &mut Frame, atlas: &FontAtlas, out: &mut Vec<TextInstance>) {
        if !f.mode.is_gpu() {
            return;
        }
        let (Some(ra), Some(dec)) = (f.state.solve_ra_jnow_deg, f.state.solve_dec_jnow_deg) else {
            return;
        };
        let alpha = f
            .state
            .solve_age_ms
            .map(|age| (1.0 - (age / 600_000.0).min(1.0)) * 0.95 + 0.05)
            .unwrap_or(1.0) as f32;
        if alpha < 0.1 {
            return;
        }
        let view = line_view(f);
        let (alt, az) = crate::astro::eq_to_altaz(ra, dec, f.scene.lst, f.scene.latitude);
        if let Some((sx, sy)) = view.project(alt, az) {
            let label = t(f.scene.cur_lang).solved_mark;
            let w = atlas.measure_width(label, 10.0);
            atlas.push_text(
                out,
                label,
                sx as f32 - w * 0.5,
                sy as f32 - 28.0,
                10.0,
                [60.0 / 255.0, 230.0 / 255.0, 120.0 / 255.0, alpha],
            );
        }
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        let (Some(ra), Some(dec)) = (f.state.solve_ra_jnow_deg, f.state.solve_dec_jnow_deg) else {
            return;
        };
        let alpha = f
            .state
            .solve_age_ms
            .map(|age| (1.0 - (age / 600_000.0).min(1.0)) * 0.95 + 0.05)
            .unwrap_or(1.0);
        if alpha < 0.1 {
            return;
        }
        let (alt, az) = crate::astro::eq_to_altaz(ra, dec, f.scene.lst, f.scene.latitude);
        let Some((sx, sy)) = proj(alt, az) else {
            return;
        };

        let green = format!("rgba(60,230,120,{alpha:.3})");
        ctx.set_stroke_style_str(&green);
        ctx.set_line_width(1.2);
        ctx.begin_path();
        let _ = ctx.arc(sx, sy, 10.0, 0.0, 2.0 * std::f64::consts::PI);
        ctx.stroke();
        ctx.begin_path();
        ctx.move_to(sx - 16.0, sy);
        ctx.line_to(sx - 4.0, sy);
        ctx.move_to(sx + 4.0, sy);
        ctx.line_to(sx + 16.0, sy);
        ctx.move_to(sx, sy - 16.0);
        ctx.line_to(sx, sy - 4.0);
        ctx.move_to(sx, sy + 4.0);
        ctx.line_to(sx, sy + 16.0);
        ctx.stroke();

        if let (Some(pix), Some(sw), Some(sh)) = (
            f.state.solve_pixscale_arcsec,
            f.state.cam_sensor_width,
            f.state.cam_sensor_height,
        ) {
            let fov_w = pix * sw as f64 / 3600.0;
            let fov_h = pix * sh as f64 / 3600.0;
            let half_w = fov_w / 2.0;
            let half_h = fov_h / 2.0;
            let cos_dec = dec.to_radians().cos().abs().max(0.01);
            let corners_eq = [
                (ra - half_w / cos_dec, dec - half_h),
                (ra + half_w / cos_dec, dec - half_h),
                (ra + half_w / cos_dec, dec + half_h),
                (ra - half_w / cos_dec, dec + half_h),
            ];
            let rot_rad = f.state.rotation_deg.unwrap_or(0.0).to_radians();
            let sin_r = rot_rad.sin();
            let cos_r = rot_rad.cos();
            ctx.set_line_width(1.0);
            ctx.begin_path();
            let mut first = true;
            for (cra, cdec) in &corners_eq {
                let (calt, caz) =
                    crate::astro::eq_to_altaz(*cra, *cdec, f.scene.lst, f.scene.latitude);
                if let Some((px, py)) = proj(calt, caz) {
                    let dx = px - sx;
                    let dy = py - sy;
                    let rx = sx + dx * cos_r - dy * sin_r;
                    let ry = sy + dx * sin_r + dy * cos_r;
                    if first {
                        ctx.move_to(rx, ry);
                        first = false;
                    } else {
                        ctx.line_to(rx, ry);
                    }
                }
            }
            ctx.close_path();
            ctx.stroke();
        }

        ctx.set_fill_style_str(&green);
        ctx.set_font("13px monospace");
        ctx.set_text_align("center");
        let _ = ctx.fill_text(t(f.scene.cur_lang).solved_mark, sx, sy - 18.0);
    }
}
