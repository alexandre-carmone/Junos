//! Bottom-left info strip (FOV, mount, cursor coords). In `Gpu` mode the
//! `<SkyHud>` Leptos DOM component renders this and the Canvas2D version
//! is skipped (matches the legacy `!hud_on_dom` gate).

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::render_info_overlay;

pub struct InfoOverlayLayer;

impl SkyLayer for InfoOverlayLayer {
    fn name(&self) -> &'static str { "info_overlay" }
    fn enabled(&self, f: &Frame) -> bool { !f.legacy_params.hud_on_dom }
    fn draw_canvas2d(&self, f: &Frame, ctx: &CanvasRenderingContext2d) {
        render_info_overlay(ctx, f.legacy_params);
    }
}
