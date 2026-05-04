//! Ordered pipeline of `SkyLayer`s.
//!
//! Replaces the implicit ordering today scattered across:
//!   * the if-tree of `render::render_overlay` (Canvas2D layer order), and
//!   * the hard-coded sequence inside `SkyRenderer::render_frame`
//!     (GPU compositing: lines → dso → const → stars → text).
//!
//! During migration the pipeline starts empty: layers are added one at a
//! time as they are extracted from `render_overlay` / `mod.rs`. Until a
//! layer is migrated, its draw still happens in the legacy code path. Once
//! the pipeline owns every layer, the legacy `render_overlay` is deleted.

use web_sys::CanvasRenderingContext2d;

use super::layer::{Frame, GpuPrepare, SkyLayer};
use super::layers::center_crosshair::CenterCrosshairLayer;

pub struct RenderPipeline {
    layers: Vec<Box<dyn SkyLayer>>,
    gpu_prepare: GpuPrepare,
}

impl RenderPipeline {
    /// Pipeline with no layers registered. The default during migration —
    /// existing free-fn rendering still runs alongside in `mod.rs`.
    pub fn empty() -> Self {
        Self { layers: Vec::new(), gpu_prepare: GpuPrepare::new() }
    }

    /// Pipeline with the standard set of registered layers, in draw
    /// order. Populated incrementally during the legacy-render migration.
    pub fn standard() -> Self {
        let mut p = Self::empty();
        p.register(Box::new(CenterCrosshairLayer));
        p
    }

    pub fn register(&mut self, layer: Box<dyn SkyLayer>) {
        self.layers.push(layer);
    }

    pub fn gpu_prepare(&self) -> &GpuPrepare { &self.gpu_prepare }
    pub fn gpu_prepare_mut(&mut self) -> &mut GpuPrepare { &mut self.gpu_prepare }

    /// Run prepare → draw on every enabled layer.
    ///
    /// The caller is still responsible for calling `SkyRenderer::submit_frame`
    /// (or the legacy `render_frame`) afterwards with `self.gpu_prepare()`.
    /// That coupling moves into `run` once `mod.rs` no longer assembles GPU
    /// instances inline.
    pub fn run(&mut self, frame: &mut Frame, ctx: &CanvasRenderingContext2d) {
        self.gpu_prepare.clear();

        for layer in &mut self.layers {
            if !layer.enabled(frame) { continue; }
            let gpu = if frame.mode.is_gpu() { Some(&mut self.gpu_prepare) } else { None };
            layer.prepare(frame, gpu);
        }

        for layer in &self.layers {
            if !layer.enabled(frame) { continue; }
            layer.draw_canvas2d(frame, ctx);
        }
    }
}
