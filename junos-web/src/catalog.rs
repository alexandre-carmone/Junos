//! Star catalog — loaded at runtime from `/junos.bin`.
//!
//! Binary format (little-endian):
//!   [u32] n_stars, [u32] n_segments, [u32] n_centers
//!   Stars:    ra_deg(f32), dec_deg(f32), mag(f32), bv(f32),
//!             con_len(u8), con(utf8 bytes), name_len(u8), name(utf8 bytes)
//!             (len 0 = None for optional fields)
//!   Segments: idx_a(u16), idx_b(u16)
//!   Centers:  abbr_len(u8), abbr(utf8), name_len(u8), name(utf8), ra(f32), dec(f32)
//!

use std::sync::Arc;

pub struct CatalogStar {
    pub ra_deg: f32,
    pub dec_deg: f32,
    pub mag: f32,
    pub bv: f32,
    pub con: Option<String>,
    pub name: Option<String>,
}

pub struct CatalogData {
    pub stars: Vec<CatalogStar>,
    pub lines: Vec<(u16, u16)>,
    /// (IAU abbr, native name, center_ra_deg, center_dec_deg)
    pub centers: Vec<(String, String, f32, f32)>,
}

impl CatalogData {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut pos = 0usize;
        let n_stars   = rd_u32(data, &mut pos)? as usize;
        let n_segs    = rd_u32(data, &mut pos)? as usize;
        let n_centers = rd_u32(data, &mut pos)? as usize;

        let mut stars = Vec::with_capacity(n_stars);
        for _ in 0..n_stars {
            let ra_deg  = rd_f32(data, &mut pos)?;
            let dec_deg = rd_f32(data, &mut pos)?;
            let mag     = rd_f32(data, &mut pos)?;
            let bv      = rd_f32(data, &mut pos)?;
            let con     = rd_str_opt(data, &mut pos)?;
            let name    = rd_str_opt(data, &mut pos)?;
            stars.push(CatalogStar { ra_deg, dec_deg, mag, bv, con, name });
        }

        let mut lines = Vec::with_capacity(n_segs);
        for _ in 0..n_segs {
            let a = rd_u16(data, &mut pos)?;
            let b = rd_u16(data, &mut pos)?;
            lines.push((a, b));
        }

        let mut centers = Vec::with_capacity(n_centers);
        for _ in 0..n_centers {
            let abbr = rd_str_req(data, &mut pos)?;
            let name = rd_str_req(data, &mut pos)?;
            let ra   = rd_f32(data, &mut pos)?;
            let dec  = rd_f32(data, &mut pos)?;
            centers.push((abbr, name, ra, dec));
        }

        Some(CatalogData { stars, lines, centers })
    }

    pub fn packed_star_buffer(&self) -> Vec<[f32; 4]> {
        self.stars.iter().map(|s| {
            // Sol is the Sun's catalog entry at a fixed epoch position; suppress it
            // so it doesn't appear as a duplicate of the ephemeris-computed Sun.
            let mag = if s.name.as_deref() == Some("Sol") { 100.0 } else { s.mag };
            [s.ra_deg, s.dec_deg, mag, s.bv]
        }).collect()
    }

    pub fn packed_line_buffer(&self) -> Vec<[u32; 2]> {
        self.lines.iter().map(|&(a, b)| [a as u32, b as u32]).collect()
    }
}

pub async fn fetch_catalog() -> Option<Arc<CatalogData>> {
    let bytes = gloo_net::http::Request::get("/junos.bin")
        .send().await.ok()?
        .binary().await.ok()?;
    CatalogData::parse(&bytes).map(Arc::new)
}

// ── Binary helpers ─────────────────────────────────────────────────────────

fn rd_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let v = u32::from_le_bytes(data.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(v)
}

fn rd_u16(data: &[u8], pos: &mut usize) -> Option<u16> {
    let end = pos.checked_add(2)?;
    let v = u16::from_le_bytes(data.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(v)
}

fn rd_f32(data: &[u8], pos: &mut usize) -> Option<f32> {
    let end = pos.checked_add(4)?;
    let v = f32::from_le_bytes(data.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(v)
}

/// Read optional string: len=0 → None, len=n → Some(string).
fn rd_str_opt(data: &[u8], pos: &mut usize) -> Option<Option<String>> {
    let len = *data.get(*pos)? as usize;
    *pos += 1;
    if len == 0 {
        return Some(None);
    }
    let end = pos.checked_add(len)?;
    let s = std::str::from_utf8(data.get(*pos..end)?).ok()?.to_string();
    *pos = end;
    Some(Some(s))
}

/// Read required string (len=0 → empty string).
fn rd_str_req(data: &[u8], pos: &mut usize) -> Option<String> {
    rd_str_opt(data, pos)?.or(Some(String::new()))
}
