//! `SkyLayer` trait and the per-frame `Frame` context.
//!
//! Each visual concern (stars, DSO, FOV reticles, mosaic, …) becomes a
//! `SkyLayer` impl. The pipeline calls `prepare` (optionally writing into
//! `GpuPrepare`) followed by `draw_canvas2d`; the `PipelineMode` decides
//! whether a layer's GPU prepare or its Canvas2D fallback is the source of
//! truth this frame.

use std::cell::RefCell;
use std::sync::Arc;

use web_sys::{CanvasRenderingContext2d, HtmlImageElement};

use crate::catalog::CatalogData;
use crate::dso_catalog::DsoCatalogData;
use crate::skymap::SkyMapInfo;

use super::super::dso_index::DsoIndex;
use super::super::gpu::layers::dso::DsoInstance;
use super::super::gpu::layers::lines::{LineSegment, LineView};
use super::super::gpu::{FontAtlas, SkymapPrepare, TextInstance};
use super::super::tile_images::TileImageCache;
use super::{HitItem, LayerToggles, OverlayState, PipelineMode, SceneParams, ViewParams};

/// Borrowed catalog handles. Layers don't own catalog state — they read it.
pub struct Catalogs<'a> {
    pub stars: Option<&'a Arc<CatalogData>>,
    /// Tycho-2 stars of the tiles around the view, `[ra, dec, mag, bv]`;
    /// empty unless zoomed in past `junos.bin` (`deep_stars::DeepStarField`).
    pub deep_stars: &'a [[f32; 4]],
    pub dso: Option<&'a Arc<DsoCatalogData>>,
    pub dso_index: Option<&'a DsoIndex>,
}

/// GPU instance buffers a layer's `prepare` may append to. The pipeline
/// `clear()`s these between frames and hands a fully-populated `GpuPrepare`
/// to `SkyRenderer::submit_frame` once all layers have prepared.
pub struct GpuPrepare {
    pub lines: Vec<LineSegment>,
    pub dso: Vec<DsoInstance>,
    pub text: Vec<TextInstance>,
    /// The sky-map tiles wanted this frame and how to draw them (`SkymapLayer`).
    pub skymap: SkymapPrepare,
    /// The decoded Milky Way panorama, once it is (`AllskyLayer`).
    pub allsky: Option<HtmlImageElement>,
    pub show_milky_way: bool,
    pub milky_way_opacity: f32,
    /// GPU-only star draw flag (whether to dispatch the star compute pass).
    pub show_stars: bool,
    /// GPU-only constellation-line draw flag.
    pub show_constellations: bool,
}

impl GpuPrepare {
    pub fn new() -> Self {
        Self {
            // Match the pre-existing pre-sizing in mod.rs so first-frame allocs
            // don't cause a hot-path resize.
            lines: Vec::with_capacity(2048),
            dso: Vec::with_capacity(512),
            text: Vec::with_capacity(256),
            skymap: SkymapPrepare::default(),
            allsky: None,
            show_milky_way: false,
            milky_way_opacity: 0.0,
            show_stars: false,
            show_constellations: false,
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.dso.clear();
        self.text.clear();
        self.skymap.clear();
        self.allsky = None;
        self.show_milky_way = false;
        self.milky_way_opacity = 0.0;
        self.show_stars = false;
        self.show_constellations = false;
    }
}

impl Default for GpuPrepare {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-frame context handed to every layer. Cheap to construct — just
/// borrows of the four param groups + catalogs + a mutable hit-list slot.
pub struct Frame<'a> {
    pub view: &'a ViewParams,
    pub scene: &'a SceneParams,
    pub state: &'a OverlayState,
    pub toggles: &'a LayerToggles,
    pub mode: PipelineMode,
    pub catalogs: &'a Catalogs<'a>,
    pub hit_items: &'a mut Vec<HitItem>,
    /// Slew trail samples (JD, RA deg, Dec deg) for the trail layer.
    pub slew_trail: &'a [(f64, f64, f64)],
    /// Survey images (sky-map tiles, Milky Way); `None` without a document.
    pub tile_images: Option<&'a RefCell<TileImageCache>>,
    /// The server's sky map, once `/api/skymap/info` has answered.
    pub skymap: Option<&'a Arc<SkyMapInfo>>,
    /// Set by `SkymapLayer::prepare` while the sky image is showing: objects
    /// whose apparent semi-axis (CSS px) reaches this get no outline, the
    /// picture shows them. `None` = outlines for all.
    pub image_min_half_px: Option<f64>,
}

/// Equirectangular projection: equatorial plate-carrée → screen px.
///
/// Free fn rather than a `Frame` method so a layer can hold a project
/// closure while still passing `&mut Frame` to a render fn. `ViewParams`
/// is `Copy`.
pub fn project_with(view: ViewParams, alt: f64, az: f64) -> Option<(f64, f64)> {
    crate::astro::project(alt, az, view.c_alt, view.c_az, view.fov)
        .map(|(x, y)| (view.cx + x * view.scale, view.cy - y * view.scale))
}

/// `project_with` clipped at `max_sep_deg` from the view centre instead of
/// 1.5 × the view radius — for footprints (sky-map cells) that reach past
/// the canvas corners, which on a 16:9 screen lie 2 × the radius out.
pub fn project_far_with(view: ViewParams, alt: f64, az: f64, max_sep_deg: f64) -> Option<(f64, f64)> {
    crate::astro::project_to(alt, az, view.c_alt, view.c_az, view.fov, max_sep_deg)
        .map(|(x, y)| (view.cx + x * view.scale, view.cy - y * view.scale))
}

pub fn line_view(f: &Frame) -> LineView {
    LineView {
        wf: f.view.wf,
        hf: f.view.hf,
        fov: f.view.fov,
        c_alt: f.view.c_alt,
        c_az: f.view.c_az,
        lst: f.scene.lst,
        latitude: f.scene.latitude,
        jd: f.scene.jd,
    }
}

/// One visual concern in the planetarium. Each impl owns both its GPU
/// prepare (when applicable) and its Canvas2D draw, so adding/removing a
/// layer is a single-file change.
// Default method bodies ignore their params; overrides use them.
#[allow(unused_variables)]
pub trait SkyLayer {
    /// Whether this layer should run this frame. Default: always on.
    /// Layers with user-facing toggles override to read `f.toggles`.
    fn enabled(&self, f: &Frame) -> bool {
        true
    }

    /// Build per-frame data. May append to `gpu` when `f.mode` is `Gpu`.
    /// Default: no-op.
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {}

    /// Build GPU text instances for this layer. Called by the pipeline only
    /// when a `FontAtlas` is available.
    fn prepare_gpu_text(&self, f: &mut Frame, atlas: &FontAtlas, out: &mut Vec<TextInstance>) {}

    /// Paint the Canvas2D overlay. In `Gpu` mode, GPU-capable layers
    /// no-op here. Always-Canvas2D layers ignore `f.mode`. Default: no-op.
    ///
    /// Takes `&mut Frame` so layers can append to `hit_items`. Layers that
    /// don't mutate frame state should still match this signature (just
    /// don't touch the mut fields).
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {}
}
