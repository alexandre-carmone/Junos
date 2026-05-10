//! Spatial index over the DSO catalog.
//!
//! The catalog has ~2 500 entries. Iterating the whole list every frame is
//! cheap on desktop but visible on mobile. A flat 2-D RA/Dec bin lets the
//! renderer touch only buckets within the angular cap of the current view,
//! which at narrow FOVs cuts the inner loop from thousands to a handful.
//!
//! Coordinates are J2000 (matching `Dso.ra_deg / dec_deg`). The cap radius
//! passed into `visible_indices` must already include any margin the caller
//! wants for objects whose center is just outside the visible disc but whose
//! footprint dips into it.

use std::sync::Arc;

use crate::dso_catalog::DsoCatalogData;

const RA_BINS:  usize = 36;          // 10° wide
const DEC_BINS: usize = 18;          // 10° tall  (lat range −90..+90)
const RA_BIN_DEG:  f64 = 360.0 / RA_BINS  as f64;
const DEC_BIN_DEG: f64 = 180.0 / DEC_BINS as f64;
/// Half-diagonal of one cell in degrees — used to widen the cap test so a
/// bucket whose centre is *just outside* the cap but whose corner reaches in
/// is still considered.
const CELL_HALF_DIAG_DEG: f64 = 7.5; // sqrt(5² + 5²) rounded up

/// Bucketed RA/Dec index. Cell `(ra_bin, dec_bin)` lives at
/// `cells[dec_bin * RA_BINS + ra_bin]` and stores catalog indices.
pub struct DsoIndex {
    cells: Vec<Vec<u32>>,
}

impl DsoIndex {
    pub fn build(cat: &DsoCatalogData) -> Self {
        let mut cells: Vec<Vec<u32>> = (0..RA_BINS * DEC_BINS).map(|_| Vec::new()).collect();
        for (i, dso) in cat.dsos.iter().enumerate() {
            let ra = (dso.ra_deg as f64).rem_euclid(360.0);
            let dec = (dso.dec_deg as f64).clamp(-90.0, 90.0);
            let ra_bin  = ((ra  / RA_BIN_DEG)  as usize).min(RA_BINS  - 1);
            let dec_bin = (((dec + 90.0) / DEC_BIN_DEG) as usize).min(DEC_BINS - 1);
            cells[dec_bin * RA_BINS + ra_bin].push(i as u32);
        }
        Self { cells }
    }

    /// Iterate every catalog index whose bucket centre is within
    /// `cap_radius_deg` (plus one cell half-diagonal) of the J2000 view
    /// centre. The caller still applies the per-object great-circle gate; this
    /// only prunes whole buckets.
    pub fn visible_indices(
        &self,
        view_ra_deg: f64,
        view_dec_deg: f64,
        cap_radius_deg: f64,
    ) -> Vec<u32> {
        // Wide cap → the bucket cull saves nothing. Skip the trig and return
        // every index.
        if cap_radius_deg + CELL_HALF_DIAG_DEG >= 180.0 {
            return self.all_indices();
        }
        let widened = cap_radius_deg + CELL_HALF_DIAG_DEG;
        let cos_cap = widened.to_radians().cos();
        let v_ra_rad  = view_ra_deg.to_radians();
        let v_dec_rad = view_dec_deg.to_radians();
        let v_sin = v_dec_rad.sin();
        let v_cos = v_dec_rad.cos();

        // Dec range: clamp to ±90 then convert to bin range so we never visit
        // cells north of the cap.
        let dec_min = (view_dec_deg - widened).max(-90.0);
        let dec_max = (view_dec_deg + widened).min( 90.0);
        let db_min = (((dec_min + 90.0) / DEC_BIN_DEG).floor() as isize).max(0) as usize;
        let db_max = (((dec_max + 90.0) / DEC_BIN_DEG).ceil()  as isize)
            .min(DEC_BINS as isize) as usize;

        let mut out: Vec<u32> = Vec::new();
        for db in db_min..db_max {
            let cell_dec_centre = -90.0 + (db as f64 + 0.5) * DEC_BIN_DEG;
            let cd_rad = cell_dec_centre.to_radians();
            let cd_sin = cd_rad.sin();
            let cd_cos = cd_rad.cos();
            for rb in 0..RA_BINS {
                let cell_ra_centre = (rb as f64 + 0.5) * RA_BIN_DEG;
                let cr_rad = cell_ra_centre.to_radians();
                let cos_sep = v_sin * cd_sin + v_cos * cd_cos * (cr_rad - v_ra_rad).cos();
                if cos_sep < cos_cap { continue; }
                let cell = &self.cells[db * RA_BINS + rb];
                out.extend_from_slice(cell);
            }
        }
        out
    }

    fn all_indices(&self) -> Vec<u32> {
        let total: usize = self.cells.iter().map(|c| c.len()).sum();
        let mut out = Vec::with_capacity(total);
        for cell in &self.cells {
            out.extend_from_slice(cell);
        }
        out
    }
}

/// Build the index off the main thread is overkill for ~2 500 entries — the
/// build is microseconds. Just construct in place and Arc it for sharing.
pub fn build_arc(cat: &DsoCatalogData) -> Arc<DsoIndex> {
    Arc::new(DsoIndex::build(cat))
}
