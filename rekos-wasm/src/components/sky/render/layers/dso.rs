//! Deep-sky-object overlay: outline symbols, labels, nebula thumbnails,
//! and DSO hit items.
//!
//! In `Gpu` mode the GPU `DsoLayer` (instances built via
//! `dso_render::build` inline in mod.rs) draws the outline symbols and
//! the `TextLayer` draws labels — but nebula thumbnails and DSO hit
//! items still flow through `render_dso`'s Canvas2D path. So this layer
//! always runs `draw_canvas2d`, regardless of mode; `render_dso` itself
//! gates its symbol/label work on `p.dso_on_gpu`.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::render_dso;

pub struct DsoLayer;

impl SkyLayer for DsoLayer {
    fn name(&self) -> &'static str {
        "dso"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.dso_on
    }
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        // Re-borrow each field individually so the closure capturing
        // `view` doesn't conflict with the mutable borrows of
        // `hit_items` / `nebulae_cache`.
        let view = *f.view;
        let scale = view.scale;
        let proj = |alt: f64, az: f64| {
            crate::astro::project(alt, az, view.c_alt, view.c_az, view.fov)
                .map(|(x, y)| (view.cx + x * scale, view.cy - y * scale))
        };
        let dso_cat_owned = f.catalogs.dso.cloned();
        let dso_index = f.catalogs.dso_index;
        let nebulae_index = f.catalogs.nebulae;
        render_dso(
            ctx,
            f,
            &dso_cat_owned,
            dso_index,
            &proj,
            scale,
            nebulae_index,
        );
    }
}
