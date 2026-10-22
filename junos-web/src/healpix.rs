//! HEALPix, NESTED scheme — the sky-map tiles' addressing (`skymap.rs`).
//!
//! Pure math, no dependencies, f64. The same algorithm (same branches, same
//! `fract` conventions) is ported to WGSL in `components/sky/shaders/skymap.wgsl`
//! and to Python in `scripts/fetch_skymap.py`, so a cell listed here on the CPU
//! is the cell the fragment shader hashes per pixel. Keep the three in step.
//!
//! Conventions (cdshealpix / Aladin): a cell of `order` k is `hash` in
//! `0..12·4^k`; `dx ∈ [0,1)` runs from the cell's **south** vertex along its
//! S→E edge, `dy` along its S→W edge. A HiPS tile image stores column ← dy,
//! row ← dx: its top-left pixel is the south vertex, top-right the west one,
//! bottom-left the east one, bottom-right the north one.
//!
//! Formulas: Górski et al. 2005 (`ang2pix_nest`), the HEALPix projection of
//! Calabretta & Roukema 2007 for cell centres and vertices.

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI, TAU};

/// Side of an order-0 cell, degrees: sqrt(4π/12) rad.
pub const ORDER0_SIDE_DEG: f64 = 58.632_3;

/// A cell and the position inside it, see the module doc.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HashDxDy {
    pub hash: u32,
    pub dx: f64,
    pub dy: f64,
}

#[inline]
pub fn nside(order: u8) -> u32 {
    1 << order
}

/// Cells at `order`: 12·4^k.
#[inline]
pub fn n_cells(order: u8) -> u32 {
    12 << (2 * order)
}

/// Offset of `order` in a table flattened over orders 0..: 12·(4^o − 1)/3.
#[inline]
pub fn table_base(order: u8) -> u32 {
    4 * ((1u32 << (2 * order)) - 1)
}

/// The cell containing (`lon`, `lat`) (radians; ICRS RA/Dec for the sky map)
/// and the in-cell offsets.
pub fn hash_dxdy(order: u8, lon_rad: f64, lat_rad: f64) -> HashDxDy {
    let ns = nside(order);
    let nsf = ns as f64;
    let z = lat_rad.sin();
    let za = z.abs();
    let mut lon = lon_rad % TAU;
    if lon < 0.0 {
        lon += TAU;
    }
    let tt = lon / FRAC_PI_2; // [0, 4)

    let (face, ix, iy, dx, dy);
    if za <= 2.0 / 3.0 {
        // Equatorial belt.
        let t1 = nsf * (0.5 + tt);
        let t2 = 0.75 * nsf * z;
        let jpf = t1 - t2;
        let jmf = t1 + t2;
        let jp = jpf.floor();
        let jm = jmf.floor();
        let jpi = jp.max(0.0) as u32;
        let jmi = jm.max(0.0) as u32;
        let ifp = jpi >> order;
        let ifm = jmi >> order;
        face = if ifp == ifm {
            (ifp & 3) + 4
        } else if ifp < ifm {
            ifp & 3
        } else {
            (ifm & 3) + 8
        };
        ix = jmi & (ns - 1);
        iy = ns - 1 - (jpi & (ns - 1));
        dx = sub_unit(jmf - jm);
        dy = sub_unit(1.0 - (jpf - jp));
    } else {
        // Polar caps. σ = sqrt(3(1−|z|)), in the colatitude form that keeps
        // its precision next to the pole: 1 − |sin lat| = 2 sin²(θ/2).
        let ntt = (tt.floor() as u32).min(3);
        let tp = tt - ntt as f64;
        let theta_half = (FRAC_PI_2 - lat_rad.abs()) * 0.5;
        let sigma = 6f64.sqrt() * theta_half.sin().abs();
        let tmp = nsf * sigma;
        let jpf = tp * tmp;
        let jmf = (1.0 - tp) * tmp;
        let jp = (jpf.floor() as u32).min(ns - 1);
        let jm = (jmf.floor() as u32).min(ns - 1);
        let fp = sub_unit(jpf - jp as f64);
        let fm = sub_unit(jmf - jm as f64);
        if z >= 0.0 {
            face = ntt;
            ix = ns - 1 - jm;
            iy = ns - 1 - jp;
            dx = sub_unit(1.0 - fm);
            dy = sub_unit(1.0 - fp);
        } else {
            face = ntt + 8;
            ix = jp;
            iy = jm;
            dx = fp;
            dy = fm;
        }
    }
    let hash = ((face as u32) << (2 * order)) | spread_bits(ix) | (spread_bits(iy) << 1);
    HashDxDy { hash, dx, dy }
}

/// Clamp a fractional offset into [0, 1).
#[inline]
fn sub_unit(v: f64) -> f64 {
    v.clamp(0.0, 0.999_999_9)
}

/// Morton spread: bit i of `v` (16 bits) → bit 2i.
#[inline]
pub fn spread_bits(v: u32) -> u32 {
    let mut v = v & 0xFFFF;
    v = (v | (v << 8)) & 0x00FF_00FF;
    v = (v | (v << 4)) & 0x0F0F_0F0F;
    v = (v | (v << 2)) & 0x3333_3333;
    v = (v | (v << 1)) & 0x5555_5555;
    v
}

/// Inverse of [`spread_bits`]: the even bits of `v`, packed.
#[inline]
pub fn compress_bits(v: u32) -> u32 {
    let mut v = v & 0x5555_5555;
    v = (v | (v >> 1)) & 0x3333_3333;
    v = (v | (v >> 2)) & 0x0F0F_0F0F;
    v = (v | (v >> 4)) & 0x00FF_00FF;
    v = (v | (v >> 8)) & 0x0000_FFFF;
    v
}

/// (face, ix, iy) of a cell.
fn split(order: u8, hash: u32) -> (u32, u32, u32) {
    let face = hash >> (2 * order);
    let p = hash & ((1u32 << (2 * order)) - 1);
    (face, compress_bits(p), compress_bits(p >> 1))
}

/// Centre of the cell in the HEALPix projection plane (x ∈ [0,8), y ∈ [−2,2]).
fn plane_center(order: u8, hash: u32) -> (f64, f64) {
    let (face, ix, iy) = split(order, hash);
    let (xf, yf) = match face {
        0..=3 => ((2 * face + 1) as f64, 1.0),
        4..=7 => ((2 * (face - 4)) as f64, 0.0),
        _ => ((2 * (face - 8) + 1) as f64, -1.0),
    };
    let nsf = nside(order) as f64;
    let x = xf + (ix as f64 - iy as f64) / nsf;
    let y = yf - 1.0 + (ix as f64 + iy as f64 + 1.0) / nsf;
    (x, y)
}

/// Inverse HEALPix projection: plane (x, y) → (lon, lat) radians, lon in [0, 2π).
fn unproject(x: f64, y: f64) -> (f64, f64) {
    let ya = y.abs();
    let (lon, z) = if ya <= 1.0 {
        (x * FRAC_PI_4, y / 1.5)
    } else {
        let sigma = 2.0 - ya;
        let z = (1.0 - sigma * sigma / 3.0).copysign(y);
        // Polar faces are centred on odd x; the centre column is straight.
        let xc = 2.0 * (x / 2.0).floor() + 1.0;
        let lon = if sigma > 1e-12 { FRAC_PI_4 * (xc + (x - xc) / sigma) } else { FRAC_PI_4 * xc };
        (lon, z)
    };
    let mut lon = lon % TAU;
    if lon < 0.0 {
        lon += TAU;
    }
    (lon, z.clamp(-1.0, 1.0).asin())
}

/// Centre (lon, lat) of a cell, radians.
pub fn cell_center(order: u8, hash: u32) -> (f64, f64) {
    let (x, y) = plane_center(order, hash);
    unproject(x, y)
}

/// Vertices of a cell, in the order [S, E, N, W], (lon, lat) radians.
pub fn cell_vertices(order: u8, hash: u32) -> [(f64, f64); 4] {
    let (x, y) = plane_center(order, hash);
    let t = 1.0 / nside(order) as f64;
    [
        unproject(x, y - t),
        unproject(x + t, y),
        unproject(x, y + t),
        unproject(x - t, y),
    ]
}

/// Same, as unit vectors.
pub fn cell_vertices_xyz(order: u8, hash: u32) -> [[f64; 3]; 4] {
    cell_vertices(order, hash).map(|(lon, lat)| lonlat_to_xyz(lon, lat))
}

#[inline]
pub fn lonlat_to_xyz(lon: f64, lat: f64) -> [f64; 3] {
    let (sl, cl) = lat.sin_cos();
    let (so, co) = lon.sin_cos();
    [cl * co, cl * so, sl]
}

#[inline]
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Angle between two unit vectors, radians.
#[inline]
fn angle(a: [f64; 3], b: [f64; 3]) -> f64 {
    dot(a, b).clamp(-1.0, 1.0).acos()
}

/// The cell's centre (unit vector) and a radius (radians) of a circle that
/// contains it — the farthest vertex, with a margin for the curved edges.
fn bounding(order: u8, hash: u32) -> ([f64; 3], f64) {
    let (lon, lat) = cell_center(order, hash);
    let c = lonlat_to_xyz(lon, lat);
    let r = cell_vertices_xyz(order, hash)
        .iter()
        .map(|v| angle(c, *v))
        .fold(0.0, f64::max);
    (c, r * 1.1 + 1e-9)
}

/// The cells of `order` whose bounding circle meets the cone — a recursive
/// descent from the 12 base cells. Sorted nearest-centre first.
pub fn cone_cells(order: u8, lon_rad: f64, lat_rad: f64, radius_rad: f64) -> Vec<u32> {
    let centre = lonlat_to_xyz(lon_rad, lat_rad);
    let radius = radius_rad.clamp(0.0, PI);
    let mut out: Vec<(f64, u32)> = Vec::new();
    let mut stack: Vec<(u8, u32)> = (0..12u32).map(|h| (0u8, h)).collect();
    while let Some((o, h)) = stack.pop() {
        let (c, r) = bounding(o, h);
        let sep = angle(c, centre);
        if sep > r + radius {
            continue;
        }
        if o == order {
            out.push((sep, h));
        } else {
            let base = h << 2;
            stack.extend((0..4u32).map(|q| (o + 1, base | q)));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out.into_iter().map(|(_, h)| h).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parent `delta` orders up (what the shader's fallback step does).
    fn parent(hash: u32, delta: u8) -> u32 {
        hash >> (2 * delta)
    }

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn round_trip_cell_centres() {
        for order in 0..=3u8 {
            for h in 0..n_cells(order) {
                let (lon, lat) = cell_center(order, h);
                let got = hash_dxdy(order, lon, lat);
                assert_eq!(got.hash, h, "order {order} cell {h}");
                assert!(approx(got.dx, 0.5, 1e-6) && approx(got.dy, 0.5, 1e-6), "{got:?}");
            }
        }
    }

    #[test]
    fn parents_nest() {
        for h in 0..n_cells(4) {
            let (lon, lat) = cell_center(4, h);
            assert_eq!(hash_dxdy(3, lon, lat).hash, parent(h, 1));
            assert_eq!(hash_dxdy(1, lon, lat).hash, parent(h, 3));
        }
    }

    #[test]
    fn base_cells() {
        assert_eq!(hash_dxdy(0, 0.0, 0.0).hash, 4);
        assert_eq!(hash_dxdy(0, 45f64.to_radians(), 60f64.to_radians()).hash, 0);
        assert_eq!(hash_dxdy(0, 45f64.to_radians(), -60f64.to_radians()).hash, 8);
    }

    #[test]
    fn dxdy_follow_the_vertices() {
        // A point nudged inward from each vertex lands at the matching corner.
        for &(order, h) in &[(2u8, 17u32), (3, 400), (5, 9000), (4, 2000)] {
            let (clon, clat) = cell_center(order, h);
            let c = lonlat_to_xyz(clon, clat);
            let expect = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
            for (v, (ex, ey)) in cell_vertices_xyz(order, h).iter().zip(expect) {
                let p = [
                    v[0] * 0.95 + c[0] * 0.05,
                    v[1] * 0.95 + c[1] * 0.05,
                    v[2] * 0.95 + c[2] * 0.05,
                ];
                let n = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
                let lon = p[1].atan2(p[0]);
                let lat = (p[2] / n).asin();
                let got = hash_dxdy(order, lon, lat);
                assert_eq!(got.hash, h, "order {order} cell {h} vertex ({ex},{ey})");
                assert!(approx(got.dx, ex, 0.12) && approx(got.dy, ey, 0.12), "{got:?} vs ({ex},{ey})");
            }
        }
    }

    #[test]
    fn cone_cells_cover_sampled_points() {
        let (lon, lat) = (10.68f64.to_radians(), 41.27f64.to_radians());
        let r = 1.5f64.to_radians();
        let cells = cone_cells(6, lon, lat, r);
        assert!(!cells.is_empty() && cells.len() < 200, "{}", cells.len());
        for i in 0..64 {
            let a = i as f64 / 64.0 * TAU;
            let plon = lon + r * 0.99 * a.cos() / lat.cos();
            let plat = lat + r * 0.99 * a.sin();
            let h = hash_dxdy(6, plon, plat).hash;
            assert!(cells.contains(&h), "cell {h} missing");
        }
        // The containing cell is listed (not necessarily first: the nearest
        // cell centre can be a neighbour's when the point sits near a vertex).
        assert!(cells.contains(&hash_dxdy(6, lon, lat).hash));
    }

    /// Vectors printed by `scripts/fetch_skymap.py --selftest`: the three
    /// ports must agree on these.
    #[test]
    fn matches_the_python_port() {
        let deg = |(lon, lat): (f64, f64)| (lon.to_degrees(), lat.to_degrees());
        for (order, h, ra, dec) in [
            (3u8, 0u32, 45.0, 4.780192),
            (3, 767, 315.0, -4.780192),
            (7, 196_607, 315.0, -0.298417),
            (0, 4, 0.0, 0.0),
            (1, 47, 315.0, -19.471221),
        ] {
            let (glon, glat) = deg(cell_center(order, h));
            assert!(approx(glon, ra, 1e-5) && approx(glat, dec, 1e-5), "({order},{h}) → ({glon},{glat})");
        }
        for (order, ra, dec, h) in [(3u8, 83.82f64, -5.39f64, 334u32), (7, 10.68, 41.27, 10836), (5, 0.0, -89.5, 8192)] {
            assert_eq!(hash_dxdy(order, ra.to_radians(), dec.to_radians()).hash, h);
        }
    }

    #[test]
    fn table_bases() {
        assert_eq!(table_base(0), 0);
        assert_eq!(table_base(1), 12);
        assert_eq!(table_base(2), 60);
        assert_eq!(table_base(8) + n_cells(8), 1_048_572);
    }

    #[test]
    fn spread_compress() {
        for v in [0u32, 1, 2, 255, 0xABCD, 0xFFFF] {
            assert_eq!(compress_bits(spread_bits(v)), v);
        }
    }
}
