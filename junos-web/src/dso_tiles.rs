//! Offline DSO tile index — loaded from `/api/dso_tiles/index.json` at startup.
//!
//! The tiles are pre-downloaded hips2fits cutouts, one per catalog object,
//! written by `scripts/prefetch_dso_tiles.py` and served by `junos-server`'s
//! `dso_tiles.rs`. They let the Framing Assistant work with no internet.
//!
//! **Epoch:** everything in this module is J2000 (the tiles were fetched with
//! `coordsys=icrs`). The Framing Assistant works in JNow, so it converts on
//! the way in and out — see `framing.rs`.
//!
//! An empty or missing index is normal (the cache is optional); callers then
//! get an empty list from `find_overlapping` and the Framing Assistant draws
//! a black (uncovered) preview.

use std::sync::Arc;

use serde::Deserialize;

/// One pre-downloaded cutout.
#[derive(Deserialize, Clone)]
pub struct DsoTile {
    /// Catalog name matching dso.bin format (e.g. "M31", "NGC 1023").
    pub name: String,
    /// File name under `/api/dso_tiles/` (e.g. "m31.jpg").
    pub path: String,
    /// Tile centre, J2000 degrees.
    pub ra: f64,
    pub dec: f64,
    /// Angular side of the square tile, degrees.
    pub fov: f64,
}

impl DsoTile {
    /// URL the browser loads this tile from.
    pub fn url(&self) -> String {
        format!("/api/dso_tiles/{}", self.path)
    }
}

pub struct DsoTileIndex {
    pub tiles: Vec<DsoTile>,
}

impl DsoTileIndex {
    pub fn from_json(json: &str) -> Option<Arc<Self>> {
        let tiles: Vec<DsoTile> = serde_json::from_str(json).ok()?;
        Some(Arc::new(DsoTileIndex { tiles }))
    }

    /// Every cached tile whose square plausibly intersects a square field of
    /// `fov_deg` centred on (`ra_deg`, `dec_deg`) — all J2000. The Framing
    /// Assistant stamps these onto one canvas to build an "adapted" preview of
    /// the whole zone, filling any uncovered sky with black.
    ///
    /// Ordered nearest-centre first and capped at [`MAX_COMPOSITE_TILES`] so a
    /// very wide zone can't pull dozens of multi-megabyte tiles; the tiles
    /// closest to the zone centre are the ones that matter for framing.
    pub fn find_overlapping(&self, ra_deg: f64, dec_deg: f64, fov_deg: f64) -> Vec<&DsoTile> {
        // Circular over-estimate of "two squares intersect": their centres are
        // within the sum of their half-diagonals. Generous on purpose — a tile
        // included but barely touching just contributes a sliver, whereas one
        // wrongly excluded leaves a black gap.
        let mut hits: Vec<(f64, &DsoTile)> = self
            .tiles
            .iter()
            .filter_map(|t| {
                let sep = angular_sep_deg(ra_deg, dec_deg, t.ra, t.dec);
                let reach = (fov_deg + t.fov) * 0.5 * std::f64::consts::SQRT_2;
                (sep <= reach).then_some((sep, t))
            })
            .collect();
        hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        hits.truncate(MAX_COMPOSITE_TILES);
        hits.into_iter().map(|(_, t)| t).collect()
    }
}

/// Upper bound on tiles composited into one framing preview. A normal zone
/// overlaps a handful; the cap only bites on pathologically wide fields.
pub const MAX_COMPOSITE_TILES: usize = 32;

/// Great-circle separation in degrees, via the haversine formula (stable for
/// the small separations we actually compare here, unlike the cosine form).
fn angular_sep_deg(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let (r1, d1) = (ra1.to_radians(), dec1.to_radians());
    let (r2, d2) = (ra2.to_radians(), dec2.to_radians());
    let dd = (d2 - d1) / 2.0;
    let dr = (r2 - r1) / 2.0;
    let h = dd.sin().powi(2) + d1.cos() * d2.cos() * dr.sin().powi(2);
    2.0 * h.sqrt().clamp(-1.0, 1.0).asin().to_degrees()
}

/// Fetch `/api/dso_tiles/index.json`. Returns an empty index rather than
/// `None` when the cache is absent — a server without tiles is not an error.
pub async fn fetch_dso_tile_index() -> Option<Arc<DsoTileIndex>> {
    let text = gloo_net::http::Request::get("/api/dso_tiles/index.json")
        .send().await.ok()?
        .text().await.ok()?;
    DsoTileIndex::from_json(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(tiles: Vec<DsoTile>) -> DsoTileIndex {
        DsoTileIndex { tiles }
    }

    fn tile(name: &str, ra: f64, dec: f64, fov: f64) -> DsoTile {
        DsoTile { name: name.into(), path: format!("{name}.jpg"), ra, dec, fov }
    }

    fn names(tiles: Vec<&DsoTile>) -> Vec<&str> {
        tiles.into_iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn includes_a_tile_that_contains_the_field() {
        let i = idx(vec![tile("m31", 10.68, 41.26, 3.0)]);
        assert_eq!(names(i.find_overlapping(10.68, 41.26, 1.0)), ["m31"]);
    }

    #[test]
    fn includes_tiles_the_field_only_partially_covers() {
        // A field wider than any single tile still collects both overlapping
        // tiles so the composite can stamp them side by side.
        let i = idx(vec![
            tile("a", 10.0, 40.0, 1.0),
            tile("b", 11.0, 40.0, 1.0),
        ]);
        let mut got = names(i.find_overlapping(10.5, 40.0, 4.0));
        got.sort_unstable();
        assert_eq!(got, ["a", "b"]);
    }

    #[test]
    fn excludes_a_clearly_disjoint_tile() {
        let i = idx(vec![tile("far", 200.0, -20.0, 0.5)]);
        assert!(i.find_overlapping(10.68, 41.26, 1.0).is_empty());
    }

    #[test]
    fn orders_nearest_centre_first() {
        let i = idx(vec![
            tile("far", 12.0, 41.26, 1.0),
            tile("near", 10.7, 41.26, 1.0),
        ]);
        assert_eq!(names(i.find_overlapping(10.68, 41.26, 3.0)), ["near", "far"]);
    }

    #[test]
    fn caps_the_number_of_composited_tiles() {
        // Many overlapping tiles stacked on the same spot — only the cap survives.
        let tiles = (0..MAX_COMPOSITE_TILES + 10)
            .map(|n| tile(&format!("t{n}"), 10.68, 41.26, 1.0))
            .collect();
        assert_eq!(idx(tiles).find_overlapping(10.68, 41.26, 1.0).len(), MAX_COMPOSITE_TILES);
    }

    #[test]
    fn empty_index_yields_nothing() {
        assert!(idx(vec![]).find_overlapping(0.0, 0.0, 0.1).is_empty());
    }

    #[test]
    fn separation_accounts_for_ra_convergence_near_the_pole() {
        // 1 deg of RA at dec 60 is ~0.5 deg on the sky.
        let sep = angular_sep_deg(0.0, 60.0, 1.0, 60.0);
        assert!((sep - 0.5).abs() < 0.01, "got {sep}");
    }
}
