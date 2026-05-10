//! FOV reticles: center-anchored + mount-anchored camera-frame rectangles.
//!
//! In `Gpu` mode the rectangles are drawn by the GPU `LineLayer` and their
//! "<arcmin> <km/s>"-style labels by the `TextLayer` (instances assembled
//! inline in `mod.rs:628-666`). Canvas2D mode delegates to the legacy fns.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use super::super::params::PipelineMode;
use crate::components::sky::gpu::layers::lines as gpu_lines;
use crate::components::sky::gpu::{FontAtlas, TextInstance};

pub struct FovReticleLayer;

impl SkyLayer for FovReticleLayer {
    fn name(&self) -> &'static str {
        "fov_reticle"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.fov_on
    }
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = gpu else { return };
        let Some(fl_mm) = f.state.fl else { return };
        let (Some(px_um), Some(sw), Some(sh)) = (
            f.state.cam_pixel_size_um,
            f.state.cam_sensor_width,
            f.state.cam_sensor_height,
        ) else {
            return;
        };
        let view = line_view(f);
        let fov_w_deg = crate::astro::fov_deg(fl_mm, sw as f64, px_um);
        let fov_h_deg = crate::astro::fov_deg(fl_mm, sh as f64, px_um);
        let rot_deg = f.state.rotation_deg.unwrap_or(0.0);

        let (center_ra_deg, center_dec_deg) =
            crate::astro::altaz_to_eq(f.view.c_alt, f.view.c_az, f.scene.lst, f.scene.latitude);
        let center_color = [80.0 / 255.0, 190.0 / 255.0, 1.0, 0.85];
        let _ = gpu_lines::build_fov_reticle(
            &mut gpu.lines,
            &view,
            center_ra_deg,
            center_dec_deg,
            fov_w_deg,
            fov_h_deg,
            rot_deg,
            center_color,
            1.0,
        );

        if f.state.mount_connected {
            if let (Some(ra_h), Some(dec_deg)) = (f.state.mount_ra_h, f.state.mount_dec_deg) {
                let mount_color = [1.0, 204.0 / 255.0, 0.0, 1.0];
                let _ = gpu_lines::build_fov_reticle(
                    &mut gpu.lines,
                    &view,
                    ra_h * 15.0,
                    dec_deg,
                    fov_w_deg,
                    fov_h_deg,
                    rot_deg,
                    mount_color,
                    1.0,
                );
            }
        }
    }

    fn prepare_gpu_text(&self, f: &mut Frame, atlas: &FontAtlas, out: &mut Vec<TextInstance>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(fl_mm) = f.state.fl else { return };
        let (Some(px_um), Some(sw), Some(sh)) = (
            f.state.cam_pixel_size_um,
            f.state.cam_sensor_width,
            f.state.cam_sensor_height,
        ) else {
            return;
        };
        let view = line_view(f);
        let fov_w_deg = crate::astro::fov_deg(fl_mm, sw as f64, px_um);
        let fov_h_deg = crate::astro::fov_deg(fl_mm, sh as f64, px_um);
        let rot_deg = f.state.rotation_deg.unwrap_or(0.0);
        let label = format!("{:.0}x{:.0}'", fov_w_deg * 60.0, fov_h_deg * 60.0);

        let mut scratch = Vec::new();
        let (center_ra_deg, center_dec_deg) =
            crate::astro::altaz_to_eq(f.view.c_alt, f.view.c_az, f.scene.lst, f.scene.latitude);
        let center_color = [80.0 / 255.0, 190.0 / 255.0, 1.0, 0.85];
        if let Some((ax, ay)) = gpu_lines::build_fov_reticle(
            &mut scratch,
            &view,
            center_ra_deg,
            center_dec_deg,
            fov_w_deg,
            fov_h_deg,
            rot_deg,
            center_color,
            1.0,
        ) {
            let w = atlas.measure_width(&label, 10.0);
            atlas.push_text(
                out,
                &label,
                ax as f32 - w * 0.5,
                ay as f32 - 12.0,
                10.0,
                center_color,
            );
        }

        if f.state.mount_connected {
            if let (Some(ra_h), Some(dec_deg)) = (f.state.mount_ra_h, f.state.mount_dec_deg) {
                let mount_color = [1.0, 204.0 / 255.0, 0.0, 1.0];
                let mut scratch = Vec::new();
                if let Some((ax, ay)) = gpu_lines::build_fov_reticle(
                    &mut scratch,
                    &view,
                    ra_h * 15.0,
                    dec_deg,
                    fov_w_deg,
                    fov_h_deg,
                    rot_deg,
                    mount_color,
                    1.0,
                ) {
                    let w = atlas.measure_width(&label, 10.0);
                    atlas.push_text(
                        out,
                        &label,
                        ax as f32 - w * 0.5,
                        ay as f32 - 12.0,
                        10.0,
                        mount_color,
                    );
                }
            }
        }
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback {
            return;
        }
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) = (
            f.state.fl,
            f.state.cam_pixel_size_um,
            f.state.cam_sensor_width,
            f.state.cam_sensor_height,
        ) else {
            return;
        };
        let fov_w = crate::astro::fov_deg(fl_mm, sw as f64, px_um);
        let fov_h = crate::astro::fov_deg(fl_mm, sh as f64, px_um);
        let half_w = fov_w / 2.0;
        let half_h = fov_h / 2.0;
        let rot_rad = f.state.rotation_deg.unwrap_or(0.0).to_radians();
        let sin_r = rot_rad.sin();
        let cos_r = rot_rad.cos();

        let draw_box = |ctx: &CanvasRenderingContext2d,
                        center_ra: f64,
                        center_dec: f64,
                        stroke: &str,
                        label_fill: &str,
                        label_dec_offset: f64,
                        fallback_center: (f64, f64)| {
            let cos_dec = center_dec.to_radians().cos().abs().max(0.01);
            let corners_eq = [
                (center_ra - half_w / cos_dec, center_dec - half_h),
                (center_ra + half_w / cos_dec, center_dec - half_h),
                (center_ra + half_w / cos_dec, center_dec + half_h),
                (center_ra - half_w / cos_dec, center_dec + half_h),
            ];
            let (pcx, pcy) = {
                let (calt, caz) =
                    crate::astro::eq_to_altaz(center_ra, center_dec, f.scene.lst, f.scene.latitude);
                proj(calt, caz).unwrap_or(fallback_center)
            };

            ctx.set_stroke_style_str(stroke);
            ctx.set_line_width(1.0);
            ctx.begin_path();
            let mut first = true;
            for (cra, cdec) in &corners_eq {
                let (alt, az) =
                    crate::astro::eq_to_altaz(*cra, *cdec, f.scene.lst, f.scene.latitude);
                if let Some((sx, sy)) = proj(alt, az) {
                    let dx = sx - pcx;
                    let dy = sy - pcy;
                    let rx = pcx + dx * cos_r - dy * sin_r;
                    let ry = pcy + dx * sin_r + dy * cos_r;
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

            let (lalt, laz) = crate::astro::eq_to_altaz(
                center_ra,
                center_dec - half_h - label_dec_offset,
                f.scene.lst,
                f.scene.latitude,
            );
            if let Some((lx, ly)) = proj(lalt, laz) {
                let dx = lx - pcx;
                let dy = ly - pcy;
                let rlx = pcx + dx * cos_r - dy * sin_r;
                let rly = pcy + dx * sin_r + dy * cos_r;
                ctx.set_fill_style_str(label_fill);
                ctx.set_font("10px monospace");
                ctx.set_text_align("center");
                let _ = ctx.fill_text(
                    &format!("{:.0}x{:.0}'", fov_w * 60.0, fov_h * 60.0),
                    rlx,
                    rly + 12.0,
                );
            }
        };

        let (center_ra, center_dec) =
            crate::astro::altaz_to_eq(f.view.c_alt, f.view.c_az, f.scene.lst, f.scene.latitude);
        draw_box(
            ctx,
            center_ra,
            center_dec,
            "rgba(80,190,255,0.85)",
            "rgba(80,190,255,0.85)",
            0.5,
            (f.view.cx, f.view.cy),
        );

        if f.state.mount_connected {
            if let (Some(ra_h), Some(dec)) = (f.state.mount_ra_h, f.state.mount_dec_deg) {
                let ra_deg = ra_h * 15.0;
                let (malt, maz) =
                    crate::astro::eq_to_altaz(ra_deg, dec, f.scene.lst, f.scene.latitude);
                let fallback_center = proj(malt, maz).unwrap_or((f.view.cx, f.view.cy));
                draw_box(ctx, ra_deg, dec, "#ffcc00", "#ffcc00", 0.3, fallback_center);
            }
        }
    }
}
