//! Stars + bright-star labels + per-star hit items.
//!
//! - In `Canvas2dFallback`, paints the full star field via
//!   `render_fallback_stars` (the legacy `!has_gpu` path).
//! - In `Gpu`, the WebGPU compute pass draws the field; this layer only
//!   paints the optional star labels (`names_on`) and feeds the named-star
//!   hit list when CPU picking is disabled.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::params::PipelineMode;
use super::super::{render_fallback_stars, render_star_names_gpu};

pub struct StarsLayer;

impl SkyLayer for StarsLayer {
    fn name(&self) -> &'static str {
        "stars"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.stars_on || f.toggles.const_on
    }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let view = *f.view;
        let scale = view.scale;
        let proj = |alt: f64, az: f64| {
            crate::astro::project(alt, az, view.c_alt, view.c_az, view.fov)
                .map(|(x, y)| (view.cx + x * scale, view.cy - y * scale))
        };
        let cat_owned = f.catalogs.stars.cloned();

        match f.mode {
            PipelineMode::Canvas2dFallback => {
                // Canvas clear is handled by `RenderPipeline::run` before
                // any layer runs.
                render_fallback_stars(ctx, f, &cat_owned, &proj, view.cx, view.cy, scale);
            }
            PipelineMode::Gpu => {
                if f.toggles.names_on && f.toggles.stars_on {
                    render_star_names_gpu(ctx, f, &cat_owned, &proj);
                }
                // In Gpu mode the named-star hit list is produced by the
                // dedicated `picking::build` pre-pass in `mod.rs`, not here.
            }
        }
    }
}
