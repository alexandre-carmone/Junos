//! Zenith mark + ring. Always Canvas2D — the GPU path doesn't render
//! the "Z" glyph yet.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::render_zenith;

pub struct ZenithLayer;

impl SkyLayer for ZenithLayer {
    fn name(&self) -> &'static str { "zenith" }
    fn enabled(&self, f: &Frame) -> bool { f.toggles.zenith_on }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let proj = |alt: f64, az: f64| f.project(alt, az);
        render_zenith(ctx, f.legacy_params, &proj);
    }
}
