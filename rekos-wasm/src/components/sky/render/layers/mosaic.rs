//! Mosaic plan(s): both the KStars-published mosaic (`new_mosaic_tiles`)
//! and the live in-app planner preview.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::render_mosaic_plan;

pub struct MosaicLayer;

impl SkyLayer for MosaicLayer {
    fn name(&self) -> &'static str {
        "mosaic"
    }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let view = *f.view;
        let proj = |alt: f64, az: f64| super::super::layer::project_with(view, alt, az);
        if let Some(plan) = f.state.mosaic_kstars.as_ref() {
            render_mosaic_plan(ctx, f, plan, &proj, false);
        }
        if let Some(plan) = f.state.mosaic_plan.as_ref() {
            render_mosaic_plan(ctx, f, plan, &proj, true);
        }
    }
}
