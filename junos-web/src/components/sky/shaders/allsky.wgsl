// All-sky Milky Way background — one full-screen triangle that, per pixel,
// runs the star projection backwards (screen → alt/az → JNow → J2000) and
// samples a plate-carrée panorama of the sky.
//
// The panorama (`scripts/prefetch_dso_tiles.py --allsky`) is centred on
// RA 0h / Dec 0°, north up, east LEFT, so RA increases leftward from the
// centre column: u = 0.5 − ra/2π (wrapping), v = (π/2 − dec)/π.

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

struct SkyParams {
    opacity: f32,
    _p0: f32,
    _p1: f32,
    _p2: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var sky:   texture_2d<f32>;
@group(0) @binding(2) var sky_s: sampler;
@group(0) @binding(3) var<uniform> p: SkyParams;

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

@fragment
fn fs_main(@builtin(position) fpos: vec4<f32>) -> @location(0) vec4<f32> {
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

    let uv = vec2<f32>(fract(0.5 - ra0 / TWO_PI), (HALF_PI - dec0) / PI);
    // Derivatives jump where u wraps; take them from a half-turn-shifted copy
    // on the pixels that straddle the seam. Must stay in uniform control flow.
    let uv2 = vec2<f32>(fract(uv.x + 0.5), uv.y);
    let dx = dpdx(uv);
    let dy = dpdy(uv);
    let dx2 = dpdx(uv2);
    let dy2 = dpdy(uv2);
    let gx = select(dx, dx2, abs(dx2.x) < abs(dx.x));
    let gy = select(dy, dy2, abs(dy2.x) < abs(dy.x));
    let col = textureSampleGrad(sky, sky_s, uv, gx, gy).rgb;

    // Below the horizon the sky dims rather than vanishes — the stars there
    // are still drawn, faintly.
    let horizon = mix(0.3, 1.0, smoothstep(-0.06, 0.0, alt));
    // Past the antipode the inverse projection wraps around; show plain sky.
    let inside = select(0.0, 1.0, c <= PI);
    let clear = vec3<f32>(0.039, 0.039, 0.078);
    return vec4<f32>(clear + col * p.opacity * horizon * inside, 1.0);
}
