// The sky map: a full-screen pass that, per pixel, runs the star projection
// backwards to J2000 (`sky_inverse.wgsl`, concatenated in front of this
// file), finds the HEALPix NESTED cell of the view's order and samples that
// tile from a `texture_2d_array` atlas of the tiles resident on the GPU
// (`gpu/layers/skymap.rs`). A tile not resident yet falls back to its parent
// cell, down to `order_min`, so a zoom never shows a hole.
//
// `hpx_hash_dxdy` is the WGSL port of `crate::healpix::hash_dxdy` — same
// branches, same conventions (dx from the south vertex along S→E, dy along
// S→W; a HiPS tile stores column ← dy, row ← dx). Keep them in step.

struct SkymapParams {
    order:      u32,  // the view's order
    order_min:  u32,  // lowest parent order to fall back to
    tile_px:    u32,  // 512
    _p0:        u32,
    opacity:    f32,  // toggle × crossfade with the panorama
    brightness: f32,
    lod:        f32,  // log2(tile texels per screen px) at `order`
    _p1:        f32,
};

@group(0) @binding(1) var<uniform> p: SkymapParams;
/// slot + 1 of the resident tile of each cell, 0 when absent; orders 0..
/// flattened, cell (o, h) at `table_base(o) + h`.
@group(0) @binding(2) var<storage, read> table: array<u32>;
@group(0) @binding(3) var tiles:   texture_2d_array<f32>;
@group(0) @binding(4) var tiles_s: sampler;

fn spread_bits(v0: u32) -> u32 {
    var v = v0 & 0xFFFFu;
    v = (v | (v << 8u)) & 0x00FF00FFu;
    v = (v | (v << 4u)) & 0x0F0F0F0Fu;
    v = (v | (v << 2u)) & 0x33333333u;
    v = (v | (v << 1u)) & 0x55555555u;
    return v;
}

fn table_base(o: u32) -> u32 {
    return 4u * ((1u << (2u * o)) - 1u);
}

struct HashDxDy {
    hash: u32,
    dx:   f32,
    dy:   f32,
};

fn hpx_hash_dxdy(order: u32, lon0: f32, lat: f32) -> HashDxDy {
    let ns = 1u << order;
    let nsf = f32(ns);
    let z = sin(lat);
    let za = abs(z);
    let lon = lon0 - TWO_PI * floor(lon0 / TWO_PI);
    let tt = lon / HALF_PI;
    var face: u32;
    var ix: u32;
    var iy: u32;
    var dx: f32;
    var dy: f32;
    if (za <= 2.0 / 3.0) {
        // Equatorial belt.
        let t1 = nsf * (0.5 + tt);
        let t2 = 0.75 * nsf * z;
        let jpf = t1 - t2;
        let jmf = t1 + t2;
        let jp = floor(jpf);
        let jm = floor(jmf);
        let jpi = u32(max(jp, 0.0));
        let jmi = u32(max(jm, 0.0));
        let ifp = jpi >> order;
        let ifm = jmi >> order;
        if (ifp == ifm) {
            face = (ifp & 3u) + 4u;
        } else if (ifp < ifm) {
            face = ifp & 3u;
        } else {
            face = (ifm & 3u) + 8u;
        }
        ix = jmi & (ns - 1u);
        iy = ns - 1u - (jpi & (ns - 1u));
        dx = clamp(jmf - jm, 0.0, 0.99999);
        dy = clamp(1.0 - (jpf - jp), 0.0, 0.99999);
    } else {
        // Polar caps; σ = sqrt(3(1−|z|)) in its colatitude form.
        let ntt = min(u32(floor(tt)), 3u);
        let tp = tt - f32(ntt);
        let theta_half = (HALF_PI - abs(lat)) * 0.5;
        let sigma = 2.449489743 * abs(sin(theta_half));
        let tmp = nsf * sigma;
        let jpf = tp * tmp;
        let jmf = (1.0 - tp) * tmp;
        let jp = min(u32(floor(jpf)), ns - 1u);
        let jm = min(u32(floor(jmf)), ns - 1u);
        let fp = clamp(jpf - f32(jp), 0.0, 0.99999);
        let fm = clamp(jmf - f32(jm), 0.0, 0.99999);
        if (z >= 0.0) {
            face = ntt;
            ix = ns - 1u - jm;
            iy = ns - 1u - jp;
            dx = clamp(1.0 - fm, 0.0, 0.99999);
            dy = clamp(1.0 - fp, 0.0, 0.99999);
        } else {
            face = ntt + 8u;
            ix = jp;
            iy = jm;
            dx = fp;
            dy = fm;
        }
    }
    let hash = (face << (2u * order)) | spread_bits(ix) | (spread_bits(iy) << 1u);
    return HashDxDy(hash, dx, dy);
}

@fragment
fn fs_main(@builtin(position) fpos: vec4<f32>) -> @location(0) vec4<f32> {
    let s = screen_to_j2000(fpos.xy);
    let h = hpx_hash_dxdy(p.order, s.ra0, s.dec0);
    var hash = h.hash;
    var dx = h.dx;
    var dy = h.dy;
    var o = p.order;
    var lod = p.lod;
    var slot = 0u;
    loop {
        slot = table[table_base(o) + hash];
        if (slot != 0u || o <= p.order_min) {
            break;
        }
        // Up to the parent: the child's quadrant bits (bit 0 = the east
        // half along dx, bit 1 = the west half along dy) place it inside.
        let sub = hash & 3u;
        hash = hash >> 2u;
        dx = (f32(sub & 1u) + dx) * 0.5;
        dy = (f32(sub >> 1u) + dy) * 0.5;
        o = o - 1u;
        lod = lod - 1.0;
    }
    if (slot == 0u) {
        return vec4<f32>(0.0);
    }
    // Sample texel centres only: no bleed from the clamped edge.
    let inset = 0.5 / f32(p.tile_px);
    let uv = vec2<f32>(dy, dx) * (1.0 - 2.0 * inset) + vec2<f32>(inset);
    let c = textureSampleLevel(tiles, tiles_s, uv, i32(slot - 1u), max(lod, 0.0)).rgb;
    // Below the horizon the image dims like the panorama does.
    let horizon = mix(0.3, 1.0, smoothstep(-0.06, 0.0, s.alt));
    let a = p.opacity * s.inside;
    return vec4<f32>(c * p.brightness * horizon * a, a);
}
