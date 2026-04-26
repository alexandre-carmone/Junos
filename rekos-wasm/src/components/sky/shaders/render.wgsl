// Render shader — draws only visible stars/lines via compacted index buffers.

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
    // IAU 1976 Lieske precession angles (radians), J2000 → epoch-of-date.
    // Unused here but must match the compute-shader layout.
    zeta_rad:   f32,
    z_rad:      f32,
    theta_rad:  f32,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> projected: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> visible_star_ids: array<u32>;
@group(0) @binding(3) var<storage, read> line_indices: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read> visible_line_ids: array<u32>;
@group(0) @binding(5) var<storage, read> star_catalog: array<vec4<f32>>;

// ── Star size tuning ─────────────────────────────────────────────────────────
// Radius (in CSS pixels, before DPR scaling) = STAR_SIZE_BASE - mag * STAR_SIZE_MAG_SCALE
// Increase STAR_SIZE_BASE to make all stars larger.
const STAR_SIZE_BASE:      f32 = 3.5;
const STAR_SIZE_MAG_SCALE: f32 = 0.35;
const STAR_SIZE_MIN:       f32 = 0.5;

// ── B-V color index to RGB ──────────────────────────────────────────────────

fn bv_to_rgb(bv_raw: f32) -> vec3<f32> {
    let bv = clamp(bv_raw, -0.4, 2.0);

    var r: f32;
    var g: f32;
    var b: f32;

    // Red channel
    if bv < 0.0 {
        r = 0.61 + 0.11 * bv + 0.1 * bv * bv;
    } else if bv < 0.4 {
        r = 0.83 + 0.17 * bv;
    } else {
        r = 1.0;
    }

    // Green channel
    if bv < 0.0 {
        g = 0.70 + 0.07 * bv + 0.1 * bv * bv;
    } else if bv < 0.4 {
        g = 0.87 + 0.11 * bv;
    } else if bv < 1.6 {
        g = 1.0 - 0.28 * (bv - 0.4);
    } else {
        g = 0.664;
    }

    // Blue channel
    if bv < -0.1 {
        b = 1.0;
    } else if bv < 0.5 {
        b = 1.0 - 1.68 * (bv + 0.1);
    } else {
        b = 0.0;
    }

    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

// ── Star render pipeline ─────────────────────────────────────────────────────

struct StarVsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) mag: f32,
    @location(1) uv: vec2<f32>,
    @location(2) bv: f32,
}

@vertex
fn vs_star(
    @builtin(vertex_index) vid: u32,
    @builtin(instance_index) iid: u32,
) -> StarVsOut {
    let star_idx = visible_star_ids[iid];
    let star = projected[star_idx];
    let sx = star.x;
    let sy = star.y;
    let mag = star.z;

    let catalog_entry = star_catalog[star_idx];
    let bv = catalog_entry.w;

    let qx = select(-1.0, 1.0, (vid & 1u) != 0u);
    let qy = select(-1.0, 1.0, (vid & 2u) != 0u);
    // Scale star size with screen: reference = 1000 CSS pixels (min dimension)
    let css_min = min(u.canvas_w, u.canvas_h) / u.dpr;
    let screen_scale = clamp(css_min / 1000.0, 0.4, 1.5);
    let radius = max((STAR_SIZE_BASE - f32(mag) * STAR_SIZE_MAG_SCALE) * screen_scale * u.dpr, STAR_SIZE_MIN * u.dpr);

    let px = sx + qx * radius;
    let py = sy + qy * radius;

    let ndc_x = px / u.canvas_w * 2.0 - 1.0;
    let ndc_y = 1.0 - py / u.canvas_h * 2.0;

    var out: StarVsOut;
    out.pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.mag = mag;
    out.uv = vec2<f32>(qx, qy);
    out.bv = bv;
    return out;
}

@fragment
fn fs_star(in: StarVsOut) -> @location(0) vec4<f32> {
    let mag = in.mag;

    let t = clamp((mag + 2.0) / 8.5, 0.0, 1.0);
    let brightness = mix(1.0, 0.31, t);
    let base_color = bv_to_rgb(in.bv) * brightness;

    let d = length(in.uv);
    if d > 1.0 {
        discard;
    }

    // Gaussian envelope: bright core fading smoothly to transparent edge
    let gaussian = exp(-d * d * 2.5);

    // Narrow inner glow: slightly whiten the very center to simulate the
    // bright diffraction core seen on real star images
    let core = exp(-d * d * 10.0);
    let color = mix(base_color, vec3<f32>(1.0, 1.0, 1.0), core * 0.45);

    return vec4<f32>(color, gaussian);
}

// ── Constellation line pipeline ──────────────────────────────────────────────

struct LineVsOut {
    @builtin(position) pos: vec4<f32>,
}

@vertex
fn vs_line(
    @builtin(vertex_index) vid: u32,
    @builtin(instance_index) iid: u32,
) -> LineVsOut {
    let line_idx = visible_line_ids[iid];
    let pair = line_indices[line_idx];
    let star_idx = select(pair.x, pair.y, vid == 1u);
    let star = projected[star_idx];

    let ndc_x = star.x / u.canvas_w * 2.0 - 1.0;
    let ndc_y = 1.0 - star.y / u.canvas_h * 2.0;

    var out: LineVsOut;
    out.pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    return out;
}

@fragment
fn fs_line() -> @location(0) vec4<f32> {
    return vec4<f32>(0.2, 0.2, 0.267, 1.0);
}
