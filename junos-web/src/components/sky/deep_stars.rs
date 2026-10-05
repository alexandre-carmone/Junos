//! The Tycho-2 stars around the view (`star_tiles::DeepStarCatalog`), for the
//! GPU star pass and the Canvas2D fallback alike. Rebuilt only when the view
//! moves onto other tiles; `generation` tells the GPU side to re-upload.

use crate::star_tiles::DeepStarCatalog;

/// `junos.bin` is complete to about V 10: with a star limit at or below this,
/// the deep layer is neither fetched nor drawn.
pub const DEEP_FROM_MAG: f32 = 9.5;

#[derive(Default)]
pub struct DeepStarField {
    tiles: Vec<u32>,
    scratch: Vec<u32>,
    stars: Vec<[f32; 4]>,
    generation: u64,
}

impl DeepStarField {
    /// Hold the stars of every tile within `radius_deg` of the J2000 view
    /// centre — or none, when `active` is false or the catalog isn't loaded.
    pub fn update(
        &mut self,
        cat: Option<&DeepStarCatalog>,
        active: bool,
        ra_deg: f64,
        dec_deg: f64,
        radius_deg: f64,
    ) {
        self.scratch.clear();
        if let (true, Some(cat)) = (active, cat) {
            cat.tiles_in_cap(ra_deg, dec_deg, radius_deg, &mut self.scratch);
            self.scratch.sort_unstable();
            self.scratch.dedup();
        }
        if self.scratch == self.tiles {
            return;
        }
        std::mem::swap(&mut self.tiles, &mut self.scratch);
        self.stars.clear();
        if let Some(cat) = cat {
            for &t in &self.tiles {
                cat.decode_tile(t, &mut self.stars);
            }
        }
        self.generation += 1;
    }

    pub fn stars(&self) -> &[[f32; 4]] {
        &self.stars
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}
