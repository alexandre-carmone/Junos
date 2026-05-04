//! `SkyLayer` trait and the per-frame `Frame` context.
//!
//! Each visual concern (stars, DSO, FOV reticles, mosaic, …) becomes a
//! `SkyLayer` impl. The pipeline calls `prepare` (optionally writing into
//! `GpuPrepare`) followed by `draw_canvas2d`; the `PipelineMode` decides
//! whether a layer's GPU prepare or its Canvas2D fallback is the source of
//! truth this frame.

use std::collections::HashMap;
use std::sync::Arc;

use web_sys::{CanvasRenderingContext2d, HtmlImageElement};

use crate::catalog::CatalogData;
use crate::dso_catalog::DsoCatalogData;
use crate::nebulae::NebulaeIndex;

use super::super::dso_index::DsoIndex;
use super::super::gpu::layers::dso::DsoInstance;
use super::super::gpu::layers::lines::LineSegment;
use super::super::gpu::text::TextInstance;
use super::{HitItem, LayerToggles, OverlayState, PipelineMode, RenderParams, SceneParams, ViewParams};

/// Borrowed catalog handles. Layers don't own catalog state — they read it.
pub struct Catalogs<'a> {
    pub stars: Option<&'a Arc<CatalogData>>,
    pub dso: Option<&'a Arc<DsoCatalogData>>,
    pub dso_index: Option<&'a DsoIndex>,
    pub nebulae: Option<&'a NebulaeIndex>,
}

/// GPU instance buffers a layer's `prepare` may append to. The pipeline
/// `clear()`s these between frames and hands a fully-populated `GpuPrepare`
/// to `SkyRenderer::submit_frame` once all layers have prepared.
pub struct GpuPrepare {
    pub lines: Vec<LineSegment>,
    pub dso: Vec<DsoInstance>,
    pub text: Vec<TextInstance>,
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
            show_stars: false,
            show_constellations: false,
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.dso.clear();
        self.text.clear();
        self.show_stars = false;
        self.show_constellations = false;
    }
}

impl Default for GpuPrepare {
    fn default() -> Self { Self::new() }
}

/// Per-frame context handed to every layer. Cheap to construct — just
/// borrows of the four param groups + catalogs + a mutable hit-list slot.
pub struct Frame<'a> {
    pub view:     &'a ViewParams,
    pub scene:    &'a SceneParams,
    pub state:    &'a OverlayState,
    pub toggles:  &'a LayerToggles,
    pub mode:     PipelineMode,
    pub catalogs: &'a Catalogs<'a>,
    pub hit_items: &'a mut Vec<HitItem>,
    /// Lazy-loaded nebula thumbnails, keyed by URL path. The DSO layer
    /// inserts on first sight; later frames just reuse the cached image.
    pub nebulae_cache: &'a mut HashMap<String, HtmlImageElement>,
    /// Slew trail samples (JD, RA deg, Dec deg) for the trail layer.
    pub slew_trail: &'a [(f64, f64, f64)],
    /// Transitional: borrow of the legacy `RenderParams` god-struct so
    /// migrated layers can call the existing `render::render_*` free fns
    /// without a wide signature change. Removed in step 8 of the refactor.
    pub legacy_params: &'a RenderParams,
}

impl<'a> Frame<'a> {
    /// Equirectangular projection: equatorial plate-carrée → screen px.
    /// Centralises the math the legacy free fns inline as a closure.
    pub fn project(&self, alt: f64, az: f64) -> Option<(f64, f64)> {
        crate::astro::project(alt, az, self.view.c_alt, self.view.c_az, self.view.fov)
            .map(|(x, y)| (self.view.cx + x * self.view.scale,
                            self.view.cy - y * self.view.scale))
    }
}

/// One visual concern in the planetarium. Each impl owns both its GPU
/// prepare (when applicable) and its Canvas2D draw, so adding/removing a
/// layer is a single-file change.
#[allow(unused_variables)]
pub trait SkyLayer {
    fn name(&self) -> &'static str;

    /// Whether this layer should run this frame. Default: always on.
    /// Layers with user-facing toggles override to read `f.toggles`.
    fn enabled(&self, f: &Frame) -> bool { true }

    /// Build per-frame data. May append to `gpu` when `f.mode` is `Gpu`.
    /// Default: no-op.
    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {}

    /// Paint the Canvas2D overlay. In `Gpu` mode, GPU-capable layers
    /// no-op here. Always-Canvas2D layers ignore `f.mode`. Default: no-op.
    ///
    /// Takes `&mut Frame` so layers can append to `hit_items` /
    /// `nebulae_cache`. Layers that don't mutate frame state should still
    /// match this signature (just don't touch the mut fields).
    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {}
}
