//! Ground/horizon shading. Always drawn (in both GPU and fallback modes)
//! because the GPU `LineLayer` does not currently composite the filled
//! ground polygon — only line segments.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use crate::components::sky::gpu::layers::lines as gpu_lines;
use crate::components::sky::gpu::{FontAtlas, TextInstance};
use crate::i18n::t;

pub struct GroundLayer;

impl SkyLayer for GroundLayer {
    fn name(&self) -> &'static str {
        "ground"
    }
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        let Some(gpu) = gpu else { return };
        let view = line_view(f);
        gpu_lines::build_horizon(&mut gpu.lines, &view);
    }

    fn prepare_gpu_text(&self, f: &mut Frame, atlas: &FontAtlas, out: &mut Vec<TextInstance>) {
        if !f.mode.is_gpu() {
            return;
        }
        let view = line_view(f);
        let tr = t(f.scene.cur_lang);
        let color = [221.0 / 255.0, 170.0 / 255.0, 102.0 / 255.0, 1.0];
        let size = 14.0_f32;
        for (label, az) in [
            (tr.cardinal_n, 0.0_f64),
            (tr.cardinal_e, 90.0),
            (tr.cardinal_s, 180.0),
            (tr.cardinal_w, 270.0),
        ] {
            if let Some((sx, sy)) = view.project(-2.0, az) {
                let w = atlas.measure_width(label, size);
                atlas.push_text(
                    out,
                    label,
                    sx as f32 - w * 0.5,
                    sy as f32 + 4.0,
                    size,
                    color,
                );
            }
        }
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        if !f.mode.is_gpu() {
            let mut first = true;
            ctx.begin_path();
            for i in (0..=360).step_by(3) {
                if let Some((px, py)) = proj(0.0, i as f64) {
                    if first {
                        ctx.move_to(px, py);
                        first = false;
                    } else {
                        ctx.line_to(px, py);
                    }
                }
            }
            ctx.set_stroke_style_str("#b07840");
            ctx.set_line_width(2.5);
            ctx.stroke();
        }

        if !f.mode.is_gpu() {
            ctx.set_font("bold 14px monospace");
            ctx.set_text_align("center");
            let tr = t(f.scene.cur_lang);
            for (label, az) in [
                (tr.cardinal_n, 0.0_f64),
                (tr.cardinal_e, 90.0),
                (tr.cardinal_s, 180.0),
                (tr.cardinal_w, 270.0),
            ] {
                if let Some((sx, sy)) = proj(-2.0, az) {
                    ctx.set_fill_style_str("#000");
                    let _ = ctx.fill_text(label, sx + 1.0, sy + 15.0);
                    ctx.set_fill_style_str("#ddaa66");
                    let _ = ctx.fill_text(label, sx, sy + 14.0);
                }
            }
        }
    }
}
