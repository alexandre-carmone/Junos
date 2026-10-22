// Shared head of the full-screen imagery passes (`allsky.wgsl`,
// `skymap.wgsl`): the sky `Uniforms`, the full-screen triangle and the
// per-pixel inverse of the star projection (screen → alt/az → JNow → J2000).
//
// WGSL has no includes: the Rust side concatenates this file in front of each
// pass's own source (`gpu/layers/allsky.rs`, `gpu/layers/skymap.rs`).

struct Uniforms {
    sin_lat:    f32,
    cos_lat:    f32,
    lst_rad:    f32,
    c_alt_rad:  f32,
    c_az_rad:   f32,
    fov_rad:    f32,
    cx:         f32,
    cy:         f32,
    scale:      f32,
    mag_limit:  f32,
    canvas_w:   f32,
    canvas_h:   f32,
    dpr:        f32,
    zeta_rad:   f32,
    z_rad:      f32,
    theta_rad:  f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

const PI: f32 = 3.14159265358979;
const TWO_PI: f32 = 6.28318530717959;
const HALF_PI: f32 = 1.5707963267949;

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> @builtin(position) vec4<f32> {
    // (-1,-1) (3,-1) (-1,3): one triangle covering the clip square.
    let x = f32(i32(vid & 1u) * 4 - 1);
    let y = f32(i32(vid & 2u) * 2 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

/// Where a pixel looks: J2000 RA/Dec (radians, RA in (−π, π]), its altitude
/// (radians) and `inside` = 0 past the antipode, where the inverse
/// projection wraps around.
struct SkyPos {
    ra0:    f32,
    dec0:   f32,
    alt:    f32,
    inside: f32,
};

fn screen_to_j2000(fpos: vec2<f32>) -> SkyPos {
    // Physical px → normalised view coords (±1 at fov_rad from the centre),
    // the inverse of what stars.wgsl / astro::project do.
    let nx = (fpos.x - u.cx) / u.scale;
    let ny = (u.cy - fpos.y) / u.scale;
    let r = length(vec2<f32>(nx, ny));
    let c = r * u.fov_rad;
    let pa = atan2(nx, ny);
    let ca = u.c_alt_rad;
    let sin_c = sin(c);
    let cos_c = cos(c);
    // Inverse azimuthal equidistant (astro::unproject).
    let sin_alt = sin(ca) * cos_c + cos(ca) * sin_c * cos(pa);
    let alt = asin(clamp(sin_alt, -1.0, 1.0));
    let az = u.c_az_rad
        + atan2(sin_c * sin(pa), cos(ca) * cos_c - sin(ca) * sin_c * cos(pa));
    // alt/az → hour angle / declination (az from north through east).
    let sa = sin(alt);
    let cal = cos(alt);
    let sin_dec = sa * u.sin_lat + cal * u.cos_lat * cos(az);
    let dec = asin(clamp(sin_dec, -1.0, 1.0));
    let ha = atan2(-sin(az) * cal, sa * u.cos_lat - cal * u.sin_lat * cos(az));
    let ra = u.lst_rad - ha;
    // JNow → J2000: the transpose of the Lieske rotation in stars.wgsl.
    let a1 = ra - u.z_rad;
    let cd = cos(dec);
    let sd = sin(dec);
    let ct = cos(u.theta_rad);
    let st = sin(u.theta_rad);
    let x = ct * cd * cos(a1) + st * sd;
    let y = cd * sin(a1);
    let zc = -st * cd * cos(a1) + ct * sd;
    let ra0 = atan2(y, x) - u.zeta_rad;
    let dec0 = asin(clamp(zc, -1.0, 1.0));
    let inside = select(0.0, 1.0, c <= PI);
    return SkyPos(ra0, dec0, alt, inside);
}
