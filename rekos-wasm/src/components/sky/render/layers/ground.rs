//! Ground/horizon shading. Always drawn (in both GPU and fallback modes)
//! because the GPU `LineLayer` does not currently composite the filled
//! ground polygon — only line segments.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::render_ground;

pub struct GroundLayer;

impl SkyLayer for GroundLayer {
    fn name(&self) -> &'static str { "ground" }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        render_ground(ctx, f, &proj);
    }
}
