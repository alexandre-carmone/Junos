//! Milky Way background — hands the decoded panorama and the user's opacity
//! to the GPU `AllskyLayer`, which draws it under everything else.
//!
//! GPU only: a per-pixel inverse projection has no Canvas2D equivalent, and
//! the fallback already degrades the stars. The image load is started by
//! `TileImageCache::allsky_image` the first frame this layer runs.

use super::super::layer::{Frame, GpuPrepare, SkyLayer};

/// The all-sky Milky Way panoramas written by `scripts/fetch_skymap.py
/// --allsky`: plate carrée, J2000, centred on RA 0h, north up, east left
/// (`u = 0.5 − RA/360`). The full one is 4096 px wide, the small one 2048.
pub const ALLSKY_URL: &str = "/api/skymap/allsky.jpg";
pub const ALLSKY_SMALL_URL: &str = "/api/skymap/allsky_small.jpg";

pub struct AllskyLayer;

impl SkyLayer for AllskyLayer {
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.milky_way_on
    }

    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        let Some(gpu) = gpu else { return };
        let Some(cache_cell) = f.tile_images else { return };
        let Ok(mut cache) = cache_cell.try_borrow_mut() else { return };
        gpu.allsky = cache.allsky_image(f.scene.is_mobile || f.toggles.lite);
        gpu.show_milky_way = true;
        gpu.milky_way_opacity = f.state.milky_way_opacity as f32;
    }
}
