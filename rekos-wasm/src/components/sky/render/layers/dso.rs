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

use super::super::layer::{line_view, Frame, GpuPrepare, SkyLayer};
use super::super::render_dso;
use crate::components::sky::dso_render;
use crate::components::sky::gpu::{FontAtlas, TextInstance};

pub struct DsoLayer;

impl SkyLayer for DsoLayer {
    fn name(&self) -> &'static str {
        "dso"
    }
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.dso_on
    }

    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(gpu) = gpu else { return };
        let Some(dso_cat) = f.catalogs.dso else {
            return;
        };
        let view = line_view(f);
        let params = dso_render::DsoBuildParams {
            view: &view,
            dso_cat,
            dso_index: f.catalogs.dso_index,
            mag_limit: f.state.dso_mag,
            names_on: false,
            is_mobile: f.scene.is_mobile,
            kind_filter: dso_render::KindFilter {
                gx: f.state.dso_gx,
                oc: f.state.dso_oc,
                gc: f.state.dso_gc,
                nb: f.state.dso_nb,
                pn: f.state.dso_pn,
                snr: f.state.dso_snr,
                gal: f.state.dso_gal,
            },
            lang: f.scene.cur_lang,
        };
        let mut scratch_text: Vec<TextInstance> = Vec::new();
        dso_render::build(params, None, &mut gpu.dso, &mut scratch_text);
    }

    fn prepare_gpu_text(&self, f: &mut Frame, atlas: &FontAtlas, out: &mut Vec<TextInstance>) {
        if !f.mode.is_gpu() {
            return;
        }
        let Some(dso_cat) = f.catalogs.dso else {
            return;
        };
        let view = line_view(f);
        let params = dso_render::DsoBuildParams {
            view: &view,
            dso_cat,
            dso_index: f.catalogs.dso_index,
            mag_limit: f.state.dso_mag,
            names_on: f.toggles.names_on,
            is_mobile: f.scene.is_mobile,
            kind_filter: dso_render::KindFilter {
                gx: f.state.dso_gx,
                oc: f.state.dso_oc,
                gc: f.state.dso_gc,
                nb: f.state.dso_nb,
                pn: f.state.dso_pn,
                snr: f.state.dso_snr,
                gal: f.state.dso_gal,
            },
            lang: f.scene.cur_lang,
        };
        let mut scratch_dso = Vec::new();
        dso_render::build(params, Some(atlas), &mut scratch_dso, out);
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
