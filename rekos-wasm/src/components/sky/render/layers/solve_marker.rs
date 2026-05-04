//! Plate-solve result marker: ring + gap-crosshair + rotated FOV rect.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::params::PipelineMode;
use super::super::render_solve_marker;

pub struct SolveMarkerLayer;

impl SkyLayer for SolveMarkerLayer {
    fn name(&self) -> &'static str { "solve_marker" }
    fn enabled(&self, f: &Frame) -> bool { f.toggles.solve_marker_on }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback { return; }
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        render_solve_marker(ctx, f, &proj);
    }
}
