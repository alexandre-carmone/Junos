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

use super::super::gpu::FontAtlas;
use super::layer::{Frame, GpuPrepare, SkyLayer};
use super::layers::center_crosshair::CenterCrosshairLayer;
use super::layers::constellation_names::ConstellationNamesLayer;
use super::layers::dso::DsoLayer;
use super::layers::fov_reticle::FovReticleLayer;
use super::layers::grids::{AltAzGridLayer, EclipticLayer, EqGridLayer, MeridianLayer};
use super::layers::ground::GroundLayer;
use super::layers::info_overlay::InfoOverlayLayer;
use super::layers::mosaic::MosaicLayer;
use super::layers::mount_crosshair::MountCrosshairLayer;
use super::layers::scheduler_jobs::SchedulerJobsLayer;
use super::layers::slew_trail::SlewTrailLayer;
use super::layers::solar_system::SolarSystemLayer;
use super::layers::solve_marker::SolveMarkerLayer;
use super::layers::stars::StarsLayer;
use super::layers::zenith::ZenithLayer;

pub struct RenderPipeline {
    layers: Vec<Box<dyn SkyLayer>>,
    gpu_prepare: GpuPrepare,
}

impl RenderPipeline {
    /// Pipeline with no layers registered. The default during migration —
    /// existing free-fn rendering still runs alongside in `mod.rs`.
    pub fn empty() -> Self {
        Self {
            layers: Vec::new(),
            gpu_prepare: GpuPrepare::new(),
        }
    }

    /// Pipeline with the standard set of registered layers, in draw
    /// order. Populated incrementally during the legacy-render migration.
    /// The order mirrors the legacy `render_overlay` call sequence so
    /// fallback-mode visuals stack identically.
    pub fn standard() -> Self {
        let mut p = Self::empty();
        // Ground first, behind everything else.
        p.register(Box::new(GroundLayer));
        // Line grids (alt-az / meridian / equatorial / ecliptic).
        p.register(Box::new(AltAzGridLayer));
        p.register(Box::new(MeridianLayer));
        p.register(Box::new(EqGridLayer));
        p.register(Box::new(EclipticLayer));
        // Zenith mark.
        p.register(Box::new(ZenithLayer));
        // Stars (Canvas2D fallback paints the field; in GPU mode this layer
        // only paints labels and feeds the named-star hit list).
        p.register(Box::new(StarsLayer));
        // Constellation names (GPU-mode label companion).
        p.register(Box::new(ConstellationNamesLayer));
        // DSO outlines, labels, nebulae thumbnails, hit items.
        p.register(Box::new(DsoLayer));
        // Sun / Moon / planets (skipped in GPU mode when solar_on_gpu).
        p.register(Box::new(SolarSystemLayer));
        // Slew trail and mount crosshair.
        p.register(Box::new(SlewTrailLayer));
        p.register(Box::new(MountCrosshairLayer));
        // Plate-solve marker, then the center crosshair, then FOV reticles.
        p.register(Box::new(SolveMarkerLayer));
        p.register(Box::new(CenterCrosshairLayer));
        p.register(Box::new(FovReticleLayer));
        // Mosaic plans + scheduler jobs.
        p.register(Box::new(MosaicLayer));
        p.register(Box::new(SchedulerJobsLayer));
        // Bottom-left info strip last so it sits on top.
        p.register(Box::new(InfoOverlayLayer));
        p
    }

    pub fn register(&mut self, layer: Box<dyn SkyLayer>) {
        self.layers.push(layer);
    }

    pub fn gpu_prepare(&self) -> &GpuPrepare {
        &self.gpu_prepare
    }
    pub fn gpu_prepare_mut(&mut self) -> &mut GpuPrepare {
        &mut self.gpu_prepare
    }

    /// Run prepare → draw on every enabled layer.
    ///
    /// The caller is still responsible for calling `SkyRenderer::submit_frame`
    /// (or the legacy `render_frame`) afterwards with `self.gpu_prepare()`.
    /// That coupling moves into `run` once `mod.rs` no longer assembles GPU
    /// instances inline.
    pub fn run(
        &mut self,
        frame: &mut Frame,
        ctx: &CanvasRenderingContext2d,
        font_atlas: Option<&FontAtlas>,
    ) {
        self.gpu_prepare.clear();

        // Canvas2D clear: transparent in Gpu mode (the WebGPU canvas sits
        // underneath), opaque dark in fallback mode. Centralised here so
        // individual layers don't need to handle it.
        match frame.mode {
            super::params::PipelineMode::Gpu => {
                ctx.clear_rect(0.0, 0.0, frame.view.wf, frame.view.hf);
            }
            super::params::PipelineMode::Canvas2dFallback => {
                ctx.set_fill_style_str("#0a0a14");
                ctx.fill_rect(0.0, 0.0, frame.view.wf, frame.view.hf);
            }
        }

        for layer in &mut self.layers {
            if !layer.enabled(frame) {
                continue;
            }
            let gpu = if frame.mode.is_gpu() {
                Some(&mut self.gpu_prepare)
            } else {
                None
            };
            layer.prepare(frame, gpu);
        }

        if let Some(atlas) = font_atlas {
            for layer in &self.layers {
                if !layer.enabled(frame) {
                    continue;
                }
                layer.prepare_gpu_text(frame, atlas, &mut self.gpu_prepare.text);
            }
        }

        for layer in &self.layers {
            if !layer.enabled(frame) {
                continue;
            }
            layer.draw_canvas2d(frame, ctx);
        }
        let _ = ctx; // silence unused on the empty-pipeline early-return path
    }
}
