//! The server's sky map — a local HiPS mirror of the DSS2 colour survey
//! served at `/api/skymap/*` (`junos-server/src/skymap.rs`), addressed by
//! HEALPix NESTED cells (`healpix.rs`).
//!
//! The browser streams only the tiles under its view, at the order whose
//! tile pixel matches a screen pixel; each tile is immutable, so the HTTP
//! cache keeps it across reloads. `info.json` says how deep the mirror goes.
//!
//! **Epoch:** HiPS `hips_frame=equatorial` is ICRS, i.e. J2000. The sky and
//! the Framing Assistant work in JNow and convert at the boundary.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;

/// `/api/skymap/info` — what the mirror holds.
#[derive(Deserialize, Clone, Debug)]
pub struct SkyMapInfo {
    #[serde(default)]
    pub survey: String,
    #[serde(default = "default_tile_width")]
    pub tile_width: u32,
    /// Highest order every lower order is complete at; `None` = no usable map.
    #[serde(default)]
    pub max_order: Option<u8>,
    /// Tiles present per order (`"7": 196608`); a `--cone` run leaves
    /// partial orders above `max_order` here.
    #[serde(default)]
    pub orders: HashMap<String, u64>,
    #[serde(default)]
    pub attribution: String,
    /// Whether the Milky Way panorama (`allsky.jpg`) is there too.
    #[serde(default)]
    pub allsky: bool,
}

fn default_tile_width() -> u32 {
    512
}

impl SkyMapInfo {
    /// The order complete everywhere, `None` without a usable map.
    pub fn complete_order(&self) -> Option<u8> {
        self.max_order
    }

    /// The deepest order the client may ask for: the deepest order holding
    /// any tile (a `--cone` run's), else the complete one; `lite` takes two
    /// fewer. Above the complete order a tile may be missing — the layers
    /// fall back to the parents, so the base shows there.
    pub fn usable_order(&self, lite: bool, cap: u8) -> Option<u8> {
        let complete = self.max_order?;
        let deepest = self
            .orders
            .iter()
            .filter(|(_, n)| **n > 0)
            .filter_map(|(k, _)| k.parse::<u8>().ok())
            .max()
            .unwrap_or(complete)
            .max(complete);
        Some(if lite { deepest.saturating_sub(2) } else { deepest }.min(cap))
    }

    /// The order to fetch coarse fallback tiles at for a view at `order`:
    /// two up inside the complete base, the base itself above it.
    pub fn parent_order(&self, order: u8) -> Option<u8> {
        let complete = self.max_order?;
        Some(if order > complete { complete.min(order.saturating_sub(1)) } else { order.saturating_sub(2) })
    }
}

/// `Norder{k}/Dir{d}/Npix{n}.jpg`, the HiPS layout the server mirrors.
pub fn tile_url(order: u8, npix: u32) -> String {
    format!("/api/skymap/Norder{order}/Dir{}/Npix{npix}.jpg", (npix / 10_000) * 10_000)
}

/// One `u32` per tile for the image caches: order in the top nibble, cell
/// below (order ≤ 8 ⇒ npix < 786 432 < 2²⁸).
#[inline]
pub fn tile_id(order: u8, npix: u32) -> u32 {
    ((order as u32) << 28) | (npix & 0x0FFF_FFFF)
}

/// Fetch `/api/skymap/info`. `None` when the server has no map (404, or
/// `max_order` null) — a server without a mirror is not an error.
pub async fn fetch_skymap_info() -> Option<Arc<SkyMapInfo>> {
    let resp = gloo_net::http::Request::get("/api/skymap/info").send().await.ok()?;
    if !resp.ok() {
        return None;
    }
    let info: SkyMapInfo = resp.json().await.ok()?;
    info.max_order?;
    Some(Arc::new(info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_follow_the_hips_layout() {
        assert_eq!(tile_url(3, 767), "/api/skymap/Norder3/Dir0/Npix767.jpg");
        assert_eq!(tile_url(7, 196_607), "/api/skymap/Norder7/Dir190000/Npix196607.jpg");
    }

    #[test]
    fn ids_are_unique_per_order() {
        assert_ne!(tile_id(3, 5), tile_id(4, 5));
        assert_eq!(tile_id(7, 196_607) >> 28, 7);
    }

    #[test]
    fn info_parses_with_defaults() {
        let i: SkyMapInfo = serde_json::from_str(r#"{"max_order":7}"#).unwrap();
        assert_eq!(i.tile_width, 512);
        assert_eq!(i.usable_order(true, 8), Some(5));
        assert_eq!(i.usable_order(false, 6), Some(6));
        assert_eq!(i.parent_order(7), Some(5));
        let cone: SkyMapInfo =
            serde_json::from_str(r#"{"max_order":2,"orders":{"0":12,"2":192,"6":45,"7":0}}"#).unwrap();
        assert_eq!(cone.usable_order(false, 8), Some(6));
        assert_eq!(cone.parent_order(6), Some(2));
        assert_eq!(cone.parent_order(3), Some(2));
        assert_eq!(cone.parent_order(2), Some(0));
        let none: SkyMapInfo = serde_json::from_str(r#"{"max_order":null}"#).unwrap();
        assert_eq!(none.usable_order(false, 8), None);
    }
}
