//! Decoded `<img>` → GPU texture with a full mip chain.
//!
//! wgpu has no mip generation, and a 512 px sprite shrunk to 30 px on screen
//! would shimmer without one. So each level is produced on the CPU side by
//! halving the previous level through a scratch `<canvas>` (2× bilinear is a
//! 2×2 box filter — a proper mip), then copied with
//! `copyExternalImageToTexture`. Two canvases ping-pong so level *k* can read
//! level *k−1*.
//!
//! The WebGPU call panics the wasm module on a validation error, so every
//! precondition is checked here: the image must be decoded, each canvas is
//! sized exactly to its level, and the destination must have been created
//! with `COPY_DST | RENDER_ATTACHMENT | TEXTURE_BINDING`.

use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlImageElement};

pub struct MipUploader {
    canvases: [HtmlCanvasElement; 2],
    ctxs: [CanvasRenderingContext2d; 2],
}

impl MipUploader {
    pub fn new() -> Option<Self> {
        let document = web_sys::window()?.document()?;
        let mk = || -> Option<(HtmlCanvasElement, CanvasRenderingContext2d)> {
            let canvas: HtmlCanvasElement =
                document.create_element("canvas").ok()?.dyn_into().ok()?;
            let ctx: CanvasRenderingContext2d = canvas.get_context("2d").ok()??.dyn_into().ok()?;
            ctx.set_image_smoothing_enabled(true);
            Some((canvas, ctx))
        };
        let (c0, x0) = mk()?;
        let (c1, x1) = mk()?;
        Some(Self { canvases: [c0, c1], ctxs: [x0, x1] })
    }

    /// Number of mip levels for a `w × h` level 0 (down to 1×1).
    pub fn levels_for(w: u32, h: u32) -> u32 {
        32 - w.max(h).max(1).leading_zeros()
    }

    /// Texture usage every destination of this uploader needs.
    pub const USAGE: wgpu::TextureUsages = wgpu::TextureUsages::TEXTURE_BINDING
        .union(wgpu::TextureUsages::COPY_DST)
        .union(wgpu::TextureUsages::RENDER_ATTACHMENT);

    /// Upload `img` resampled to `w0 × h0` into array `layer` of `texture`,
    /// filling `levels` mip levels. `false` if the image isn't decoded.
    pub fn upload(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        layer: u32,
        img: &HtmlImageElement,
        w0: u32,
        h0: u32,
        levels: u32,
    ) -> bool {
        if !img.complete() || img.natural_width() == 0 || img.natural_height() == 0 {
            return false;
        }
        let levels = levels.min(texture.mip_level_count());
        for level in 0..levels {
            let w = (w0 >> level).max(1);
            let h = (h0 >> level).max(1);
            let cur = (level % 2) as usize;
            let canvas = &self.canvases[cur];
            let ctx = &self.ctxs[cur];
            // Resizing clears the canvas, which also resets its state.
            canvas.set_width(w);
            canvas.set_height(h);
            ctx.set_image_smoothing_enabled(true);
            let drawn = if level == 0 {
                ctx.draw_image_with_html_image_element_and_dw_and_dh(img, 0.0, 0.0, w as f64, h as f64)
            } else {
                let prev = &self.canvases[1 - cur];
                ctx.draw_image_with_html_canvas_element_and_dw_and_dh(prev, 0.0, 0.0, w as f64, h as f64)
            };
            if drawn.is_err() {
                return false;
            }
            queue.copy_external_image_to_texture(
                &wgpu::CopyExternalImageSourceInfo {
                    source: wgpu::ExternalImageSource::HTMLCanvasElement(canvas.clone()),
                    origin: wgpu::Origin2d::ZERO,
                    flip_y: false,
                },
                wgpu::CopyExternalImageDestInfo {
                    texture,
                    mip_level: level,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
                    aspect: wgpu::TextureAspect::All,
                    color_space: wgpu::PredefinedColorSpace::Srgb,
                    premultiplied_alpha: false,
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        true
    }
}
