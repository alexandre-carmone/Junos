// Compute shader — projects catalog stars, compacts visible indices for indirect draw.

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
    zeta_rad:   f32,
    z_rad:      f32,
    theta_rad:  f32,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> star_catalog: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> projected: array<vec4<f32>>;
// star_indirect: [vertex_count, instance_count, first_vertex, first_instance]
@group(0) @binding(3) var<storage, read_write> star_indirect: array<atomic<u32>, 4>;
@group(0) @binding(4) var<storage, read_write> visible_star_ids: array<u32>;

// ── Pass 1: project stars and compact visible indices ────────────────────────

@compute @workgroup_size(64)
fn project_stars(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    let count = arrayLength(&star_catalog);
    if idx >= count {
        return;
    }

    let star = star_catalog[idx];
    let ra_j2000 = star.x * 0.017453292519943295;
    let dec_j2000 = star.y * 0.017453292519943295;
    let mag = star.z;

    if mag > u.mag_limit {
        projected[idx] = vec4<f32>(0.0, 0.0, mag, -1.0);
        return;
    }

    // Precess J2000 → epoch-of-date (IAU 1976 Lieske) so RA/Dec match
    // the current-epoch LST used below. Without this, stars are drawn
    // ~0.3° off their true JNow positions (in 2026).
    let cos_dec0 = cos(dec_j2000);
    let sin_dec0 = sin(dec_j2000);
    let cos_theta = cos(u.theta_rad);
    let sin_theta = sin(u.theta_rad);
    let ang = ra_j2000 + u.zeta_rad;
    let sin_ang = sin(ang);
    let cos_ang = cos(ang);
    let pa_x = cos_dec0 * sin_ang;
    let pa_y = cos_theta * cos_dec0 * cos_ang - sin_theta * sin_dec0;
    let pc = clamp(sin_theta * cos_dec0 * cos_ang + cos_theta * sin_dec0, -1.0, 1.0);
    let ra_rad = atan2(pa_x, pa_y) + u.z_rad;
    let dec_rad = asin(pc);

    // eq_to_altaz
    let ha = u.lst_rad - ra_rad;
    let sin_dec = sin(dec_rad);
    let cos_dec = cos(dec_rad);
    let sin_alt = sin_dec * u.sin_lat + cos_dec * u.cos_lat * cos(ha);
    let alt_rad = asin(clamp(sin_alt, -1.0, 1.0));
    let alt_deg = alt_rad * 57.29577951308232;

    if alt_deg < -5.0 {
        projected[idx] = vec4<f32>(0.0, 0.0, mag, -1.0);
        return;
    }

    let cos_alt = cos(alt_rad);
    let denom = cos_alt * u.cos_lat;
    let cos_az_num = (sin_dec - sin(alt_rad) * u.sin_lat) / select(denom, 1e-10, abs(denom) < 1e-10);
    var az_rad = acos(clamp(cos_az_num, -1.0, 1.0));
    if sin(ha) > 0.0 {
        az_rad = 6.283185307179586 - az_rad;
    }

    // Azimuthal equidistant projection
    let a = alt_rad;
    let b = az_rad;
    let ca = u.c_alt_rad;
    let cb = u.c_az_rad;

    let cos_c = sin(a) * sin(ca) + cos(a) * cos(ca) * cos(b - cb);
    let c = acos(clamp(cos_c, -1.0, 1.0));

    if c > u.fov_rad * 1.5 {
        projected[idx] = vec4<f32>(0.0, 0.0, mag, -1.0);
        return;
    }

    let r = c / u.fov_rad;
    let sin_pa = cos(a) * sin(b - cb);
    let cos_pa = sin(a) * cos(ca) - cos(a) * sin(ca) * cos(b - cb);
    let pa = atan2(sin_pa, cos_pa);

    let sx = u.cx + r * sin(pa) * u.scale;
    let sy = u.cy - r * cos(pa) * u.scale;

    let in_bounds = sx > -50.0 && sx < u.canvas_w + 50.0 && sy > -50.0 && sy < u.canvas_h + 50.0;
    if !in_bounds {
        projected[idx] = vec4<f32>(sx, sy, mag, -1.0);
        return;
    }

    projected[idx] = vec4<f32>(sx, sy, mag, 1.0);

    // Append to visible list (atomic increment of instance_count at index 1)
    let slot = atomicAdd(&star_indirect[1], 1u);
    visible_star_ids[slot] = idx;
}

// ── Pass 2: compact visible constellation lines ──────────────────────────────

@group(0) @binding(5) var<storage, read> line_src: array<vec2<u32>>;
@group(0) @binding(6) var<storage, read_write> line_indirect: array<atomic<u32>, 4>;
@group(0) @binding(7) var<storage, read_write> visible_line_ids: array<u32>;

@compute @workgroup_size(64)
fn compact_lines(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    let count = arrayLength(&line_src);
    if idx >= count {
        return;
    }

    let pair = line_src[idx];
    let a = projected[pair.x];
    let b = projected[pair.y];

    // Both endpoints must be visible
    if a.w > 0.0 && b.w > 0.0 {
        let slot = atomicAdd(&line_indirect[1], 1u);
        visible_line_ids[slot] = idx;
    }
}
