// Deep-sky object sprites — survey cutouts drawn at each object's true size
// and orientation, additively, so their black-faded edges vanish into the sky.
//
// Vertex: 4-vertex triangle strip. Each instance is a square of `half` CSS px
// rotated so the sprite's top edge (north in the north-up / east-left cutout)
// lies along the screen direction of sky north at that object. `layer` picks
// the slice of the thumbnail texture array the sprite was uploaded to.

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

struct Sprite {
    pos:        vec2<f32>,   // CSS px, sprite centre
    half:       f32,         // CSS px, half-side
    cos_rot:    f32,         // rotation: local +x → (cos, sin) on screen
    sin_rot:    f32,
    brightness: f32,
    layer:      u32,
    _pad:       u32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> items: array<Sprite>;
@group(0) @binding(2) var thumbs:   texture_2d_array<f32>;
@group(0) @binding(3) var thumbs_s: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
    @location(2) @interpolate(flat) brightness: f32,
};

@vertex
fn vs_main(
    @builtin(vertex_index)   vid: u32,
    @builtin(instance_index) iid: u32,
) -> VsOut {
    let it = items[iid];
    let qx = select(-1.0, 1.0, (vid & 1u) != 0u);
    let qy = select(-1.0, 1.0, (vid & 2u) != 0u);
    let local = vec2<f32>(qx, qy) * it.half;
    let rotated = vec2<f32>(
        local.x * it.cos_rot - local.y * it.sin_rot,
        local.x * it.sin_rot + local.y * it.cos_rot,
    );
    let pos_css = it.pos + rotated;
    let pos_phys = pos_css * u.dpr;
    let ndc_x = pos_phys.x / u.canvas_w * 2.0 - 1.0;
    let ndc_y = 1.0 - pos_phys.y / u.canvas_h * 2.0;
    var out: VsOut;
    out.pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    // Local -y is the sprite's top edge (north) → v = 0.
    out.uv = vec2<f32>(qx, qy) * 0.5 + vec2<f32>(0.5, 0.5);
    out.layer = it.layer;
    out.brightness = it.brightness;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(thumbs, thumbs_s, in.uv, in.layer);
    // Additive blend: the fragment's rgb is added to the sky as-is.
    return vec4<f32>(c.rgb * in.brightness, 1.0);
}
