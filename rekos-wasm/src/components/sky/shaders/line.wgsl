// Generic GPU line pipeline.
//
// Each instance is one line segment expanded into a 4-vertex triangle strip
// (thick line with arbitrary width). Positions arrive in CSS pixels and are
// converted to physical pixels (× dpr) before NDC.
//
// Used for: horizon, alt/az grid, equatorial grid, meridian, ecliptic,
// mount/center crosshairs, slew trail, zenith circle. All single-color
// per-segment with rgba; per-segment width.

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

struct LineSeg {
    p0:     vec2<f32>,
    p1:     vec2<f32>,
    color:  vec4<f32>,
    width:  f32,
    _pad0:  f32,
    _pad1:  f32,
    _pad2:  f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> segments: array<LineSeg>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index)   vid: u32,
    @builtin(instance_index) iid: u32,
) -> VsOut {
    let seg = segments[iid];
    // CSS px → physical px
    let p0 = seg.p0 * u.dpr;
    let p1 = seg.p1 * u.dpr;
    let dir = p1 - p0;
    let len = max(length(dir), 1e-6);
    let tangent = dir / len;
    let normal = vec2<f32>(-tangent.y, tangent.x);
    let half_w = max(seg.width * u.dpr * 0.5, 0.5);

    // 4-vertex triangle strip: (p0±n) and (p1±n).
    //  vid 0 → p0 - n   (top of bit 1 = 0, bit 0 = 0)
    //  vid 1 → p0 + n   (bit 0 = 1)
    //  vid 2 → p1 - n   (bit 1 = 1)
    //  vid 3 → p1 + n
    let along = select(p0, p1, (vid & 2u) != 0u);
    let side = select(-1.0, 1.0, (vid & 1u) != 0u);
    let pos = along + normal * (side * half_w);

    let ndc_x = pos.x / u.canvas_w * 2.0 - 1.0;
    let ndc_y = 1.0 - pos.y / u.canvas_h * 2.0;

    var out: VsOut;
    out.pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.color = seg.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
