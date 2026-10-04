//! Ordered pipeline of `SkyLayer`s.
//!
//! Layers run in registration order. Each one owns both its GPU prepare and
//! its Canvas2D draw, so the order of the overlay is the order of this list.

use web_sys::CanvasRenderingContext2d;

use super::super::gpu::FontAtlas;
use super::layer::{Frame, GpuPrepare, SkyLayer};
use super::layers::allsky::AllskyLayer;
use super::layers::center_crosshair::CenterCrosshairLayer;
use super::layers::constellation_names::ConstellationNamesLayer;
use super::layers::dso::DsoLayer;
use super::layers::dso_image::DsoImageLayer;
use super::layers::fov_reticle::FovReticleLayer;
use super::layers::grids::{AltAzGridLayer, EclipticLayer, EqGridLayer, MeridianLayer};
use super::layers::ground::GroundLayer;
use super::layers::mosaic::MosaicLayer;
use super::layers::mount_crosshair::MountCrosshairLayer;
use super::layers::scheduler_jobs::SchedulerJobsLayer;
use super::layers::slew_trail::SlewTrailLayer;
use super::layers::solar_system::SolarSystemLayer;
use super::layers::solve_marker::SolveMarkerLayer;
use super::layers::solved_image::SolvedImageLayer;
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
        // Survey imagery under everything: the Milky Way panorama (GPU only)
        // and the DSO sprites, which in fallback mode paint before the grids.
        p.register(Box::new(AllskyLayer));
        p.register(Box::new(DsoImageLayer::default()));
        // Ground, behind everything else drawn as lines.
        p.register(Box::new(GroundLayer));
        // Line grids (alt-az / meridian / equatorial / ecliptic).
        p.register(Box::new(AltAzGridLayer));
        p.register(Box::new(MeridianLayer));
        p.register(Box::new(EqGridLayer));
        p.register(Box::new(EclipticLayer));
        // Zenith mark.
        p.register(Box::new(ZenithLayer));
        // Last solve's Align frame, under the stars and boxes.
        p.register(Box::new(SolvedImageLayer));
        // Stars (Canvas2D fallback paints the field; in GPU mode this layer
        // only paints labels and feeds the named-star hit list).
        p.register(Box::new(StarsLayer));
        // Constellation names (GPU-mode label companion).
        p.register(Box::new(ConstellationNamesLayer));
        // DSO outlines, labels, hit items.
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
        p
    }

    pub fn register(&mut self, layer: Box<dyn SkyLayer>) {
        self.layers.push(layer);
    }

    /// This frame's GPU instance lists, after `run`. The caller sets the
    /// star/constellation flags on it and hands it to `submit_frame`.
    pub fn gpu_prepare_mut(&mut self) -> &mut GpuPrepare {
        &mut self.gpu_prepare
    }

    /// Run prepare → draw on every enabled layer.
    ///
    /// The caller then passes `self.gpu_prepare_mut()` to
    /// `SkyRenderer::submit_frame`.
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
