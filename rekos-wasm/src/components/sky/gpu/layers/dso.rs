//! Deep-sky object symbol layer.
//!
//! Per-frame, the CPU walks the visible DSO subset (using the same projection
//! and culling as `render::render_dso`), packs each into a `DsoInstance`,
//! and uploads to the GPU. The fragment shader dispatches on `kind` to draw
//! the right outline (galaxy ellipse, cluster dashed ring, etc.).
//!
//! Nebula thumbnails are *not* handled here yet — the texture-array path is
//! a follow-up. Until it lands, the Canvas2D overlay still draws nebula
//! images on top of (or in place of) the GPU symbol when one is available.

use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct DsoInstance {
    pub pos_x:      f32,
    pub pos_y:      f32,
    pub half_w:     f32,
    pub half_h:     f32,
    pub cos_rot:    f32,
    pub sin_rot:    f32,
    pub kind:       u32,
    pub _pad0:      u32,
    pub color_r:    f32,
    pub color_g:    f32,
    pub color_b:    f32,
    pub color_a:    f32,
}

const ITEM_BYTES: u64 = std::mem::size_of::<DsoInstance>() as u64;
const INITIAL_CAP: u64 = 2048;

pub struct DsoLayer {
    pipeline:    wgpu::RenderPipeline,
    bgl:         wgpu::BindGroupLayout,
    bind_group:  wgpu::BindGroup,
    inst_buf:    wgpu::Buffer,
    capacity:    u64,
    count:       u32,
}

impl DsoLayer {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        uniform_buf: &wgpu::Buffer,
    ) -> Self {
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dso_bgl"),
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

        let inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dso_instances"),
            size: INITIAL_CAP * ITEM_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = make_bind_group(device, &bgl, uniform_buf, &inst_buf);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("dso.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/dso.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("dso_pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("dso_pipeline"),
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
            inst_buf,
            capacity: INITIAL_CAP,
            count: 0,
        }
    }

    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uniform_buf: &wgpu::Buffer,
        items: &[DsoInstance],
    ) {
        self.count = items.len() as u32;
        if items.is_empty() { return; }
        let needed = items.len() as u64;
        if needed > self.capacity {
            let new_cap = needed.next_power_of_two().max(INITIAL_CAP);
            self.inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dso_instances"),
                size: new_cap * ITEM_BYTES,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
            self.bind_group = make_bind_group(device, &self.bgl, uniform_buf, &self.inst_buf);
        }
        queue.write_buffer(&self.inst_buf, 0, bytemuck::cast_slice(items));
    }

    pub fn draw<'a>(&'a self, rp: &mut wgpu::RenderPass<'a>) {
        if self.count == 0 { return; }
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.bind_group, &[]);
        rp.draw(0..4, 0..self.count);
    }
}

fn make_bind_group(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    uniform_buf: &wgpu::Buffer,
    inst_buf: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("dso_bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: inst_buf.as_entire_binding() },
        ],
    })
}
