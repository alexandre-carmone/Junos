//! Generic line layer — all sky vector overlays (horizon, grids, meridian,
//! ecliptic, crosshairs, slew trail, zenith) draw through a single pipeline
//! that takes a list of (p0, p1, rgba, width) segments in CSS pixels.
//!
//! CPU computes the projected vertices using the same azimuthal projection
//! as `astro::project`. The shader expands each segment into a thick line
//! (4-vertex triangle strip), so width works without relying on native
//! `gl_LineWidth` (which WebGPU doesn't expose).

use bytemuck::{Pod, Zeroable};

use crate::astro;

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LineSegment {
    pub p0_x: f32,
    pub p0_y: f32,
    pub p1_x: f32,
    pub p1_y: f32,
    pub r:    f32,
    pub g:    f32,
    pub b:    f32,
    pub a:    f32,
    pub width: f32,
    pub _pad0: f32,
    pub _pad1: f32,
    pub _pad2: f32,
}

const SEG_BYTES: u64 = std::mem::size_of::<LineSegment>() as u64;
const INITIAL_CAPACITY: u64 = 8 * 1024;

pub struct LineLayer {
    pipeline:    wgpu::RenderPipeline,
    bgl:         wgpu::BindGroupLayout,
    bind_group:  wgpu::BindGroup,
    seg_buf:     wgpu::Buffer,
    capacity:    u64,
    count:       u32,
}

impl LineLayer {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        uniform_buf: &wgpu::Buffer,
    ) -> Self {
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("line_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let seg_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("line_segments"),
            size: INITIAL_CAPACITY * SEG_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = make_bind_group(device, &bgl, uniform_buf, &seg_buf);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("line.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/line.wgsl").into()),
        });

        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("line_pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("line_pipeline"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });

        Self {
            pipeline,
            bgl,
            bind_group,
            seg_buf,
            capacity: INITIAL_CAPACITY,
            count: 0,
        }
    }

    /// Upload segments. Grows the storage buffer when needed.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uniform_buf: &wgpu::Buffer,
        segments: &[LineSegment],
    ) {
        self.count = segments.len() as u32;
        if segments.is_empty() {
            return;
        }
        let needed = segments.len() as u64;
        if needed > self.capacity {
            let new_cap = needed.next_power_of_two().max(INITIAL_CAPACITY);
            self.seg_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("line_segments"),
                size: new_cap * SEG_BYTES,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
            self.bind_group = make_bind_group(device, &self.bgl, uniform_buf, &self.seg_buf);
        }
        queue.write_buffer(&self.seg_buf, 0, bytemuck::cast_slice(segments));
    }

    pub fn draw<'a>(&'a self, rp: &mut wgpu::RenderPass<'a>) {
        if self.count == 0 {
            return;
        }
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.bind_group, &[]);
        // 4 vertices per segment (triangle strip), one instance per segment
        rp.draw(0..4, 0..self.count);
    }
}

fn make_bind_group(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    uniform_buf: &wgpu::Buffer,
    seg_buf: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("line_bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: seg_buf.as_entire_binding() },
        ],
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Geometry builders — replicate the CPU projection used by `render.rs` and
// emit screen-space LineSegments.
//
// All builders share these inputs:
//   - `project`: takes (alt_deg, az_deg) and returns Some((sx, sy)) in CSS px
//     when the point is inside the visible disk. Computed from astro::project.
// ───────────────────────────────────────────────────────────────────────────

/// View parameters passed once to all builders. Mirrors the relevant bits
/// of RenderParams without pulling that struct in.
#[derive(Copy, Clone)]
pub struct LineView {
    pub wf: f64,
    pub hf: f64,
    pub fov: f64,
    pub c_alt: f64,
    pub c_az: f64,
    pub lst: f64,
    pub latitude: f64,
    pub jd: f64,
}

impl LineView {
    pub fn cx(&self) -> f64 { self.wf / 2.0 }
    pub fn cy(&self) -> f64 { self.hf / 2.0 }
    pub fn scale(&self) -> f64 { self.hf.min(self.wf) / 2.0 }

    pub fn project(&self, alt: f64, az: f64) -> Option<(f64, f64)> {
        let scale = self.scale();
        astro::project(alt, az, self.c_alt, self.c_az, self.fov)
            .map(|(x, y)| (self.cx() + x * scale, self.cy() - y * scale))
    }
}

/// rgba helper packing.
#[inline]
fn seg(p0: (f64, f64), p1: (f64, f64), rgba: [f32; 4], width: f32) -> LineSegment {
    LineSegment {
        p0_x: p0.0 as f32, p0_y: p0.1 as f32,
        p1_x: p1.0 as f32, p1_y: p1.1 as f32,
        r: rgba[0], g: rgba[1], b: rgba[2], a: rgba[3],
        width,
        _pad0: 0.0, _pad1: 0.0, _pad2: 0.0,
    }
}

/// Push connected polyline as individual segments, breaking on None gaps.
fn push_polyline(
    out: &mut Vec<LineSegment>,
    points: impl IntoIterator<Item = Option<(f64, f64)>>,
    rgba: [f32; 4],
    width: f32,
) {
    let mut prev: Option<(f64, f64)> = None;
    for p in points {
        match (prev, p) {
            (Some(a), Some(b)) => out.push(seg(a, b, rgba, width)),
            _ => {}
        }
        prev = p;
    }
}

/// Horizon — same #b07840 line as Canvas2D version.
pub fn build_horizon(out: &mut Vec<LineSegment>, v: &LineView) {
    let color = [0.690, 0.471, 0.251, 1.0];
    let pts = (0..=360).step_by(3).map(|i| v.project(0.0, i as f64));
    push_polyline(out, pts, color, 2.5);
}

/// Alt/Az grid — small concentric rings + radial meridians.
pub fn build_altaz_grid(out: &mut Vec<LineSegment>, v: &LineView) {
    let color = [60.0/255.0, 60.0/255.0, 100.0/255.0, 0.6];
    let az_step: usize = if v.fov > 60.0 { 6 } else if v.fov > 20.0 { 4 } else { 3 };
    let alt_step: usize = if v.fov > 60.0 { 10 } else { 5 };

    for alt_i in (-1..=9).map(|i| i * 10) {
        let pts = (0..=360).step_by(az_step).map(|az_i| v.project(alt_i as f64, az_i as f64));
        push_polyline(out, pts, color, 0.7);
    }
    for az_i in (0..360).step_by(30) {
        let pts = (0..=90).step_by(alt_step).map(|alt_i| v.project(alt_i as f64, az_i as f64));
        push_polyline(out, pts, color, 0.7);
    }
}

/// Meridian — great circle through N pole and zenith.
pub fn build_meridian(out: &mut Vec<LineSegment>, v: &LineView) {
    let color = [80.0/255.0, 80.0/255.0, 140.0/255.0, 0.7];
    // Up the north side (az=0), down the south (az=180).
    let north = (0..=90).step_by(3).map(|alt_i| v.project(alt_i as f64, 0.0));
    let south = (0..=90).rev().step_by(3).map(|alt_i| v.project(alt_i as f64, 180.0));
    push_polyline(out, north, color, 1.0);
    push_polyline(out, south, color, 1.0);
}

/// Equatorial grid: dec parallels every 30°, RA meridians every 2h, equator highlighted.
pub fn build_eq_grid(out: &mut Vec<LineSegment>, v: &LineView) {
    let color   = [100.0/255.0, 100.0/255.0, 200.0/255.0, 0.6];
    let eq_color = [120.0/255.0, 120.0/255.0, 1.0, 0.8];
    let step: usize = if v.fov > 60.0 { 6 } else if v.fov > 20.0 { 4 } else { 2 };

    let project_eq = |ra_deg: f64, dec_deg: f64| -> Option<(f64, f64)> {
        let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, v.lst, v.latitude);
        v.project(alt, az)
    };

    for dec_i in (-3..=3).map(|i| i * 30) {
        let pts = (0..=360).step_by(step).map(|ra_i| project_eq(ra_i as f64, dec_i as f64));
        push_polyline(out, pts, color, 0.7);
    }
    for ra_i in (0..12).map(|i| i * 30) {
        let pts = (-90..=90).step_by(step).map(|dec_i| project_eq(ra_i as f64, dec_i as f64));
        push_polyline(out, pts, color, 0.7);
    }
    let pts = (0..=360).step_by(step).map(|ra_i| project_eq(ra_i as f64, 0.0));
    push_polyline(out, pts, eq_color, 1.2);
}

/// Ecliptic — currently rendered as solid (Canvas2D version is dashed).
pub fn build_ecliptic(out: &mut Vec<LineSegment>, v: &LineView) {
    let color = [220.0/255.0, 180.0/255.0, 80.0/255.0, 0.7];
    let t = (v.jd - 2_451_545.0) / 36525.0;
    let ecl_deg = 23.4393 - 0.0130042 * t;
    let sin_eps = ecl_deg.to_radians().sin();
    let cos_eps = ecl_deg.to_radians().cos();

    let pts = (0..=360).step_by(2).map(|deg_i| {
        let lam = (deg_i as f64).to_radians();
        let x = lam.cos();
        let y = lam.sin() * cos_eps;
        let z = lam.sin() * sin_eps;
        let ra  = y.atan2(x).to_degrees().rem_euclid(360.0);
        let dec = z.asin().to_degrees();
        let (alt, az) = astro::eq_to_altaz(ra, dec, v.lst, v.latitude);
        v.project(alt, az)
    });
    push_polyline(out, pts, color, 1.2);
}

/// Zenith circle (small ring around the zenith point).
pub fn build_zenith(out: &mut Vec<LineSegment>, v: &LineView) {
    let Some((sx, sy)) = v.project(90.0, 0.0) else { return };
    let color = [180.0/255.0, 220.0/255.0, 1.0, 0.85];
    const N: usize = 32;
    let r = 6.0;
    for i in 0..N {
        let a0 = (i as f64) / (N as f64) * std::f64::consts::TAU;
        let a1 = ((i + 1) as f64) / (N as f64) * std::f64::consts::TAU;
        let p0 = (sx + r * a0.cos(), sy + r * a0.sin());
        let p1 = (sx + r * a1.cos(), sy + r * a1.sin());
        out.push(seg(p0, p1, color, 1.2));
    }
}

/// Center crosshair — fixed cross + small ring at the canvas center.
pub fn build_center_crosshair(out: &mut Vec<LineSegment>, v: &LineView) {
    let cx = v.cx();
    let cy = v.cy();
    let arm = 18.0;
    let gap = 6.0;
    let color = [180.0/255.0, 220.0/255.0, 1.0, 0.75];
    out.push(seg((cx - arm, cy), (cx - gap, cy), color, 1.0));
    out.push(seg((cx + gap, cy), (cx + arm, cy), color, 1.0));
    out.push(seg((cx, cy - arm), (cx, cy - gap), color, 1.0));
    out.push(seg((cx, cy + gap), (cx, cy + arm), color, 1.0));
    // Inner ring
    const N: usize = 24;
    for i in 0..N {
        let a0 = (i as f64) / (N as f64) * std::f64::consts::TAU;
        let a1 = ((i + 1) as f64) / (N as f64) * std::f64::consts::TAU;
        let p0 = (cx + gap * a0.cos(), cy + gap * a0.sin());
        let p1 = (cx + gap * a1.cos(), cy + gap * a1.sin());
        out.push(seg(p0, p1, color, 1.0));
    }
}

/// Mount crosshair — draws when mount is connected and reports a position.
pub fn build_mount_crosshair(
    out: &mut Vec<LineSegment>,
    v: &LineView,
    mount_connected: bool,
    mount_ra_h: Option<f64>,
    mount_dec_deg: Option<f64>,
) {
    if !mount_connected { return; }
    let (Some(ra_h), Some(dec)) = (mount_ra_h, mount_dec_deg) else { return };
    let (malt, maz) = astro::eq_to_altaz(ra_h * 15.0, dec, v.lst, v.latitude);
    let Some((mx, my)) = v.project(malt, maz) else { return };
    let arm = 14.0;
    let color = [68.0/255.0, 1.0, 68.0/255.0, 1.0];
    out.push(seg((mx - arm, my), (mx - 4.0, my), color, 1.5));
    out.push(seg((mx + 4.0, my), (mx + arm, my), color, 1.5));
    out.push(seg((mx, my - arm), (mx, my - 4.0), color, 1.5));
    out.push(seg((mx, my + 4.0), (mx, my + arm), color, 1.5));
}

/// FOV reticle: rotated rectangle around (ra_deg, dec_deg) with FOV size in
/// degrees. Returns the label anchor (centred above the projected top edge)
/// so the caller can push a TextInstance with the same colour.
pub fn build_fov_reticle(
    out: &mut Vec<LineSegment>,
    v: &LineView,
    ra_deg: f64,
    dec_deg: f64,
    fov_w_deg: f64,
    fov_h_deg: f64,
    rot_deg: f64,
    rgba: [f32; 4],
    width: f32,
) -> Option<(f64, f64)> {
    let cos_dec = dec_deg.to_radians().cos().abs().max(0.01);
    let half_w = fov_w_deg / 2.0;
    let half_h = fov_h_deg / 2.0;
    let corners_eq = [
        (ra_deg - half_w / cos_dec, dec_deg - half_h),
        (ra_deg + half_w / cos_dec, dec_deg - half_h),
        (ra_deg + half_w / cos_dec, dec_deg + half_h),
        (ra_deg - half_w / cos_dec, dec_deg + half_h),
    ];
    let (pcx, pcy) = {
        let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, v.lst, v.latitude);
        v.project(alt, az).unwrap_or((v.cx(), v.cy()))
    };
    let rot = rot_deg.to_radians();
    let sin_r = rot.sin();
    let cos_r = rot.cos();
    let mut pts: [Option<(f64, f64)>; 4] = [None; 4];
    for (i, (cra, cdec)) in corners_eq.iter().enumerate() {
        let (calt, caz) = astro::eq_to_altaz(*cra, *cdec, v.lst, v.latitude);
        if let Some((sx, sy)) = v.project(calt, caz) {
            let dx = sx - pcx;
            let dy = sy - pcy;
            let rx = pcx + dx * cos_r - dy * sin_r;
            let ry = pcy + dx * sin_r + dy * cos_r;
            pts[i] = Some((rx, ry));
        }
    }
    let mut any = false;
    for i in 0..4 {
        let j = (i + 1) % 4;
        if let (Some(a), Some(b)) = (pts[i], pts[j]) {
            out.push(seg(a, b, rgba, width));
            any = true;
        }
    }
    if !any { return None; }
    // Label anchor: centre of all defined points, just above min-y.
    let mut sum_x = 0.0;
    let mut min_y = f64::INFINITY;
    let mut count = 0.0;
    for p in pts.iter().flatten() {
        sum_x += p.0;
        if p.1 < min_y { min_y = p.1; }
        count += 1.0;
    }
    Some((sum_x / count, min_y - 3.0))
}

/// Solve marker: ring + gap-crosshair + optional rotated FOV rectangle.
/// All in a single solid colour scaled by `alpha` (fade-by-age — caller
/// computes the correct alpha based on `solve_age_ms`).
pub fn build_solve_marker(
    out: &mut Vec<LineSegment>,
    v: &LineView,
    ra_jnow_deg: f64,
    dec_jnow_deg: f64,
    alpha: f32,
    pixscale_arcsec: Option<f64>,
    sensor_w: Option<u32>,
    sensor_h: Option<u32>,
    rotation_deg: Option<f64>,
) {
    let (alt, az) = astro::eq_to_altaz(ra_jnow_deg, dec_jnow_deg, v.lst, v.latitude);
    let Some((sx, sy)) = v.project(alt, az) else { return };

    let color = [60.0/255.0, 230.0/255.0, 120.0/255.0, alpha];

    // Ring at r = 10
    const N: usize = 32;
    let r = 10.0;
    for i in 0..N {
        let a0 = (i as f64) / (N as f64) * std::f64::consts::TAU;
        let a1 = ((i + 1) as f64) / (N as f64) * std::f64::consts::TAU;
        let p0 = (sx + r * a0.cos(), sy + r * a0.sin());
        let p1 = (sx + r * a1.cos(), sy + r * a1.sin());
        out.push(seg(p0, p1, color, 1.2));
    }
    // Gap-crosshair
    out.push(seg((sx - 16.0, sy), (sx - 4.0, sy), color, 1.2));
    out.push(seg((sx + 4.0, sy),  (sx + 16.0, sy), color, 1.2));
    out.push(seg((sx, sy - 16.0), (sx, sy - 4.0), color, 1.2));
    out.push(seg((sx, sy + 4.0),  (sx, sy + 16.0), color, 1.2));

    // Translucent FOV rectangle.
    if let (Some(pix), Some(sw), Some(sh)) = (pixscale_arcsec, sensor_w, sensor_h) {
        let fov_w = pix * sw as f64 / 3600.0;
        let fov_h = pix * sh as f64 / 3600.0;
        let half_w = fov_w / 2.0;
        let half_h = fov_h / 2.0;
        let cos_dec = dec_jnow_deg.to_radians().cos().abs().max(0.01);
        let corners_eq = [
            (ra_jnow_deg - half_w / cos_dec, dec_jnow_deg - half_h),
            (ra_jnow_deg + half_w / cos_dec, dec_jnow_deg - half_h),
            (ra_jnow_deg + half_w / cos_dec, dec_jnow_deg + half_h),
            (ra_jnow_deg - half_w / cos_dec, dec_jnow_deg + half_h),
        ];
        let rot = rotation_deg.unwrap_or(0.0).to_radians();
        let sin_r = rot.sin();
        let cos_r = rot.cos();
        let mut pts: [Option<(f64, f64)>; 4] = [None; 4];
        for (i, (cra, cdec)) in corners_eq.iter().enumerate() {
            let (calt, caz) = astro::eq_to_altaz(*cra, *cdec, v.lst, v.latitude);
            if let Some((px, py)) = v.project(calt, caz) {
                let dx = px - sx;
                let dy = py - sy;
                let rx = sx + dx * cos_r - dy * sin_r;
                let ry = sy + dx * sin_r + dy * cos_r;
                pts[i] = Some((rx, ry));
            }
        }
        for i in 0..4 {
            let j = (i + 1) % 4;
            if let (Some(a), Some(b)) = (pts[i], pts[j]) {
                out.push(seg(a, b, color, 1.0));
            }
        }
    }
}

/// Slew trail — fades over 60 wall-clock seconds.
pub fn build_slew_trail(
    out: &mut Vec<LineSegment>,
    v: &LineView,
    trail: &[(f64, f64, f64)],
) {
    if trail.len() < 2 { return; }
    const FADE_DAYS: f64 = 60.0 / 86400.0;
    let now_jd = v.jd;
    for w in trail.windows(2) {
        let (jd0, ra0, de0) = w[0];
        let (_jd1, ra1, de1) = w[1];
        let age = (now_jd - jd0).max(0.0);
        let alpha = ((1.0 - (age / FADE_DAYS).min(1.0)) * 0.9) as f32;
        if alpha < 0.05 { continue; }
        let (a0, az0) = astro::eq_to_altaz(ra0, de0, v.lst, v.latitude);
        let (a1, az1) = astro::eq_to_altaz(ra1, de1, v.lst, v.latitude);
        let (Some(s0), Some(s1)) = (v.project(a0, az0), v.project(a1, az1)) else { continue };
        out.push(seg(s0, s1, [1.0, 170.0/255.0, 60.0/255.0, alpha], 1.5));
    }
}
