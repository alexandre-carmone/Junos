// All-sky Milky Way background — one full-screen triangle that, per pixel,
// runs the star projection backwards (`sky_inverse.wgsl`, concatenated in
// front of this file) and samples a plate-carrée panorama of the sky.
//
// The panorama (`scripts/fetch_skymap.py --allsky`) is centred on
// RA 0h / Dec 0°, north up, east LEFT, so RA increases leftward from the
// centre column: u = 0.5 − ra/2π (wrapping), v = (π/2 − dec)/π.

struct SkyParams {
    opacity: f32,
    _p0: f32,
    _p1: f32,
    _p2: f32,
};

@group(0) @binding(1) var sky:   texture_2d<f32>;
@group(0) @binding(2) var sky_s: sampler;
@group(0) @binding(3) var<uniform> p: SkyParams;

@fragment
fn fs_main(@builtin(position) fpos: vec4<f32>) -> @location(0) vec4<f32> {
    let s = screen_to_j2000(fpos.xy);
    let uv = vec2<f32>(fract(0.5 - s.ra0 / TWO_PI), (HALF_PI - s.dec0) / PI);
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
    let horizon = mix(0.3, 1.0, smoothstep(-0.06, 0.0, s.alt));
    let clear = vec3<f32>(0.039, 0.039, 0.078);
    return vec4<f32>(clear + col * p.opacity * horizon * s.inside, 1.0);
}
