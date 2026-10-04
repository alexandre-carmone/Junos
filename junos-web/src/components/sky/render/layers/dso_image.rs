//! Deep-sky object imagery: the survey cutout of each object, drawn at its
//! true angular size and orientation once it is big enough on screen.
//!
//! `prepare` walks the visible catalog with the same culling as the symbol
//! layer, keeps the objects that have a tile and whose sprite would be more
//! than a few pixels, nearest to the view centre first, and asks the
//! `DsoImageCache` for their images (loads start here). Decoded ones become
//! `SpriteRequest`s for the GPU `DsoImageLayer`, or are drawn straight onto
//! the overlay in Canvas2D mode (`lighter`, so the faded edges vanish like
//! the GPU's additive blend). Either way the object goes into `Frame.imaged`
//! so the outline symbol is not drawn over its picture.
//!
//! Registered first in the pipeline, under the ground and grids.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, GpuPrepare, SkyLayer};
use crate::astro;
use crate::components::sky::dso_shape::{probe, PROBE_DEG};
use crate::components::sky::gpu::SpriteRequest;
use crate::coords::{J2000, JNow};
use crate::dso_catalog::DsoType;

/// Below this half-side (CSS px) a sprite is a smudge — the symbol does better.
const MIN_HALF_PX: f64 = 10.0;
/// Sprites per frame in Canvas2D mode (the GPU mode uses its slot count).
const FALLBACK_MAX_SPRITES: usize = 24;

#[derive(Default)]
pub struct DsoImageLayer {
    /// This frame's sprites, kept for the Canvas2D draw phase.
    sprites: Vec<SpriteRequest>,
}

impl SkyLayer for DsoImageLayer {
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.dso_on && f.toggles.dso_images_on
    }

    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        self.sprites.clear();
        let Some(cache_cell) = f.dso_images else { return };
        let Some(dso_cat) = f.catalogs.dso else { return };
        let Ok(mut cache) = cache_cell.try_borrow_mut() else { return };
        if !cache.has_index() {
            return;
        }
        cache.begin_frame();

        let view = *f.view;
        let scene = *f.scene;
        let scale = view.scale;
        let lst_rad = scene.lst.to_radians();
        let project = |alt: f64, az: f64| {
            astro::project(alt, az, view.c_alt, view.c_az, view.fov)
                .map(|(x, y)| (view.cx + x * scale, view.cy - y * scale))
        };
        let max_sprites = if f.mode.is_gpu() {
            f.sprite_capacity
        } else {
            FALLBACK_MAX_SPRITES
        };
        if max_sprites == 0 {
            return;
        }

        // ── Cull, as `dso_render::build` does ────────────────────────────
        let (c_ra_jnow, c_dec_jnow) =
            astro::altaz_to_eq(view.c_alt, view.c_az, scene.lst, scene.latitude);
        let view_j2000 = JNow::new(c_ra_jnow, c_dec_jnow).to_j2000(scene.jd);
        let v_ra_rad = view_j2000.ra_deg.to_radians();
        let v_dec_rad = view_j2000.dec_deg.to_radians();
        let (v_sin_dec, v_cos_dec) = (v_dec_rad.sin(), v_dec_rad.cos());
        let cap_radius_deg = view.fov * 1.5 + 6.0;
        let cos_cap = if cap_radius_deg >= 180.0 { -1.0 } else { cap_radius_deg.to_radians().cos() };
        let visible: Option<Vec<u32>> = f
            .catalogs
            .dso_index
            .map(|idx| idx.visible_indices(view_j2000.ra_deg, view_j2000.dec_deg, cap_radius_deg));
        let dsos = &dso_cat.dsos;
        let iter_indices: Box<dyn Iterator<Item = usize>> = match &visible {
            Some(v) => Box::new(v.iter().map(|i| *i as usize)),
            None => Box::new(0..dsos.len()),
        };

        // (distance² to the view centre, catalog index, sx, sy, half px, ra, dec)
        let mut candidates: Vec<(f64, usize, f64, f64, f64, f64, f64)> = Vec::new();
        for di in iter_indices {
            let Some(dso) = dsos.get(di) else { continue };
            let type_ok = match dso.kind {
                DsoType::Galaxy => f.state.dso_gx,
                DsoType::OpenCluster => f.state.dso_oc,
                DsoType::GlobularCluster => f.state.dso_gc,
                DsoType::Nebula => f.state.dso_nb,
                DsoType::PlanetaryNebula => f.state.dso_pn,
                DsoType::SupernovaRemnant => f.state.dso_snr,
                DsoType::GalaxyCluster => f.state.dso_gal,
            };
            if !type_ok || (dso.mag as f64) > f.state.dso_mag {
                continue;
            }
            let d_ra_rad = (dso.ra_deg as f64).to_radians();
            let d_dec_rad = (dso.dec_deg as f64).to_radians();
            let cos_sep = v_sin_dec * d_dec_rad.sin()
                + v_cos_dec * d_dec_rad.cos() * (d_ra_rad - v_ra_rad).cos();
            if cos_sep < cos_cap {
                continue;
            }
            // Only objects with a tile can be drawn; checked before the
            // trigonometry since most faint catalog entries have none.
            let Some(tile) = cache.tile_for(di as u32, &dso.name) else { continue };
            let half_px = tile.fov / (2.0 * view.fov) * scale;
            if half_px < MIN_HALF_PX {
                continue;
            }

            let dso_jnow = J2000::new(dso.ra_deg as f64, dso.dec_deg as f64).to_jnow(scene.jd);
            let ha = lst_rad - dso_jnow.ra_deg.to_radians();
            let dec_rad = dso_jnow.dec_deg.to_radians();
            let (sin_dec, cos_dec) = (dec_rad.sin(), dec_rad.cos());
            let sin_alt = sin_dec * scene.sin_lat + cos_dec * scene.cos_lat * ha.cos();
            let alt_rad = sin_alt.asin();
            let alt = alt_rad.to_degrees();
            if alt < -3.0 {
                continue;
            }
            let cos_az_val =
                (sin_dec - alt_rad.sin() * scene.sin_lat) / (alt_rad.cos() * scene.cos_lat);
            let mut az = cos_az_val.clamp(-1.0, 1.0).acos().to_degrees();
            if ha.sin() > 0.0 {
                az = 360.0 - az;
            }
            let Some((sx, sy)) = project(alt, az) else { continue };
            // Sprites are large; cull on their own extent, not a fixed margin.
            let m = half_px * std::f64::consts::SQRT_2;
            if sx < -m || sx > view.wf + m || sy < -m || sy > view.hf + m {
                continue;
            }
            let dx = sx - view.cx;
            let dy = sy - view.cy;
            candidates.push((dx * dx + dy * dy, di, sx, sy, half_px, dso_jnow.ra_deg, dso_jnow.dec_deg));
        }
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
        candidates.truncate(max_sprites);

        let brightness = f.state.dso_images_brightness as f32;
        for (_, di, sx, sy, half_px, ra, dec) in candidates {
            let Some(dso) = dsos.get(di) else { continue };
            let Some(tile) = cache.tile_for(di as u32, &dso.name) else { continue };
            let url = tile.thumb_url();
            let Some(img) = cache.image(di as u32, &url) else { continue };
            // The cutout is north-up: rotate so its top edge follows the
            // screen direction of sky north at the object (field rotation
            // included — this is an alt/az view).
            let (nx, ny) = probe(ra, dec + PROBE_DEG, sx, sy, scene.lst, scene.latitude, &project)
                .unwrap_or((0.0, -1.0));
            self.sprites.push(SpriteRequest {
                id: di as u32,
                img,
                pos_x: sx as f32,
                pos_y: sy as f32,
                half: half_px as f32,
                cos_rot: (-ny) as f32,
                sin_rot: nx as f32,
                brightness,
            });
            f.imaged.push(di as u32);
        }
        f.imaged.sort_unstable();

        if let Some(gpu) = gpu {
            gpu.sprites.extend(self.sprites.iter().cloned());
        }
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode.is_gpu() || self.sprites.is_empty() {
            return;
        }
        ctx.save();
        // Additive, like the GPU blend: the sprites' black fades add nothing.
        let _ = ctx.set_global_composite_operation("lighter");
        for s in &self.sprites {
            let half = s.half as f64;
            ctx.save();
            let _ = ctx.translate(s.pos_x as f64, s.pos_y as f64);
            let _ = ctx.rotate((s.sin_rot as f64).atan2(s.cos_rot as f64));
            ctx.set_global_alpha((s.brightness as f64).clamp(0.0, 1.0));
            let _ = ctx.draw_image_with_html_image_element_and_dw_and_dh(
                &s.img, -half, -half, 2.0 * half, 2.0 * half,
            );
            ctx.restore();
        }
        ctx.restore();
    }
}
