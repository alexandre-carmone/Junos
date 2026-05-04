//! Slew trail: fading orange polyline of the mount's recent trajectory.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::params::PipelineMode;
use super::super::render_slew_trail;

pub struct SlewTrailLayer;

impl SkyLayer for SlewTrailLayer {
    fn name(&self) -> &'static str { "slew_trail" }
    fn enabled(&self, f: &Frame) -> bool { f.toggles.slew_trail_on }
    fn draw_canvas2d(&self, f: &Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode != PipelineMode::Canvas2dFallback { return; }
        let proj = |alt: f64, az: f64| f.project(alt, az);
        render_slew_trail(ctx, f.legacy_params, &proj, f.slew_trail);
    }
}
