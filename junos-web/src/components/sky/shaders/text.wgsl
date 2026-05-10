// Bitmap font atlas — instanced quads, alpha-multiplied colour.

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

struct TextItem {
    pos:    vec2<f32>,   // CSS px, top-left of glyph quad
    size:   vec2<f32>,   // CSS px
    uv:     vec4<f32>,   // (u0, v0, du, dv) atlas-relative
    color:  vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> items: array<TextItem>;
@group(0) @binding(2) var atlas:    texture_2d<f32>;
@group(0) @binding(3) var atlas_s:  sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index)   vid: u32,
    @builtin(instance_index) iid: u32,
) -> VsOut {
    let it = items[iid];
    let qx = select(0.0, 1.0, (vid & 1u) != 0u);
    let qy = select(0.0, 1.0, (vid & 2u) != 0u);
    let pos_css = it.pos + vec2<f32>(qx * it.size.x, qy * it.size.y);
    let pos_phys = pos_css * u.dpr;
    let ndc_x = pos_phys.x / u.canvas_w * 2.0 - 1.0;
    let ndc_y = 1.0 - pos_phys.y / u.canvas_h * 2.0;
    var out: VsOut;
    out.pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = it.uv.xy + vec2<f32>(qx * it.uv.z, qy * it.uv.w);
    out.color = it.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let alpha = textureSample(atlas, atlas_s, in.uv).r;
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
