//! Zenith mark + ring. Always Canvas2D — the GPU path doesn't render
//! the "Z" glyph yet.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use crate::components::sky::gpu::layers::lines as gpu_lines;
use crate::components::sky::gpu::{FontAtlas, TextInstance};
use crate::i18n::t;

pub struct ZenithLayer;

impl SkyLayer for ZenithLayer {
    fn name(&self) -> &'static str {
        "zenith"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.zenith_on
    }
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = gpu else { return };
        gpu_lines::build_zenith(&mut gpu.lines, &line_view(f));
    }

    fn prepare_gpu_text(&self, f: &mut Frame, atlas: &FontAtlas, out: &mut Vec<TextInstance>) {
        if !f.mode.is_gpu() {
            return;
        }
        let view = line_view(f);
        if let Some((sx, sy)) = view.project(90.0, 0.0) {
            atlas.push_text(
                out,
                t(f.scene.cur_lang).zenith_mark,
                sx as f32 + 9.0,
                sy as f32 - 4.0,
                10.0,
                [180.0 / 255.0, 220.0 / 255.0, 1.0, 0.85],
            );
        }
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        if let Some((sx, sy)) = proj(90.0, 0.0) {
            if !f.mode.is_gpu() {
                ctx.set_stroke_style_str("rgba(180,220,255,0.85)");
                ctx.set_line_width(1.2);
                ctx.begin_path();
                let _ = ctx.arc(sx, sy, 6.0, 0.0, 2.0 * std::f64::consts::PI);
                ctx.stroke();
            }
            if !f.mode.is_gpu() {
                ctx.set_fill_style_str("rgba(180,220,255,0.85)");
                ctx.set_font("bold 10px monospace");
                ctx.set_text_align("left");
                let _ = ctx.fill_text(t(f.scene.cur_lang).zenith_mark, sx + 9.0, sy + 4.0);
            }
        }
    }
}
