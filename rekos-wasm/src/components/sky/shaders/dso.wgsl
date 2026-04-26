// Deep-sky object symbols — instanced quads, fragment-dispatched by `kind`.
//
// Vertex: 4-vertex triangle strip. Each instance carries its centre (CSS px),
// half-size (allows ellipses for galaxies), rotation cos/sin (used for galaxy
// position-angle), kind, and colour. UV is the unrotated unit square in
// [-1.4, 1.4]^2 — the extra 0.4 padding lets PN ticks extend outside the
// nominal radius without clipping.

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

struct DsoInstance {
    pos:        vec2<f32>,
    half_size:  vec2<f32>,
    cos_rot:    f32,
    sin_rot:    f32,
    kind:       u32,
    _pad0:      u32,
    color:      vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> items: array<DsoInstance>;

const PAD: f32 = 1.4;
const TWO_PI: f32 = 6.28318530718;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) kind: u32,
    @location(2) @interpolate(flat) color: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index)   vid: u32,
    @builtin(instance_index) iid: u32,
) -> VsOut {
    let it = items[iid];
    let qx = select(-1.0, 1.0, (vid & 1u) != 0u);
    let qy = select(-1.0, 1.0, (vid & 2u) != 0u);
    let uv_unrot = vec2<f32>(qx, qy) * PAD;
    // Rotate (for galaxies); no-op when cos=1, sin=0.
    let local = vec2<f32>(
        uv_unrot.x * it.half_size.x,
        uv_unrot.y * it.half_size.y,
    );
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
    out.uv = uv_unrot;
    out.kind = it.kind;
    out.color = it.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let r = length(in.uv);
    let theta = atan2(in.uv.y, in.uv.x);
    let abs_x = abs(in.uv.x);
    let abs_y = abs(in.uv.y);

    let outline = 0.06;

    var keep = false;

    switch (in.kind) {
        // 0: Galaxy — ellipse outline (uv space already scaled by half_size,
        // so r=1 traces the ellipse edge).
        case 0u: {
            keep = abs(r - 1.0) < outline;
        }
        // 1: OpenCluster — dashed ring.
        case 1u: {
            if (abs(r - 1.0) < outline) {
                let phase = fract(theta / TWO_PI * 12.0);
                keep = phase < 0.5;
            }
        }
        // 2: GlobularCluster — solid ring + cross arms.
        case 2u: {
            let in_ring  = abs(r - 1.0) < outline;
            let in_h_arm = abs_y < outline && abs_x < 1.0;
            let in_v_arm = abs_x < outline && abs_y < 1.0;
            keep = in_ring || in_h_arm || in_v_arm;
        }
        // 3: Nebula — square outline.
        case 3u: {
            let d = max(abs_x, abs_y);
            keep = abs(d - 1.0) < outline && abs_x < 1.05 && abs_y < 1.05;
        }
        // 4: PlanetaryNebula — ring + 4 cardinal ticks (extending to PAD).
        case 4u: {
            let in_ring  = abs(r - 1.0) < outline;
            let in_h_tk  = abs_y < outline && abs_x > 1.0 && abs_x < PAD;
            let in_v_tk  = abs_x < outline && abs_y > 1.0 && abs_y < PAD;
            keep = in_ring || in_h_tk || in_v_tk;
        }
        // 5: SupernovaRemnant — same as Nebula.
        case 5u: {
            let d = max(abs_x, abs_y);
            keep = abs(d - 1.0) < outline && abs_x < 1.05 && abs_y < 1.05;
        }
        // 6: GalaxyCluster — finer dashed ring (different period).
        case 6u, default: {
            if (abs(r - 1.0) < outline) {
                let phase = fract(theta / TWO_PI * 24.0);
                keep = phase < 0.33;
            }
        }
    }

    if (!keep) {
        discard;
    }
    return in.color;
}
