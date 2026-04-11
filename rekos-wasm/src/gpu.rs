//! WebGPU-accelerated star rendering pipeline.
//!
//! Two compute passes project stars and compact visible indices, then
//! `draw_indirect` renders only the visible stars and constellation lines.

use bytemuck::{Pod, Zeroable};
use web_sys::HtmlCanvasElement;
use wgpu::util::DeviceExt;


// ── Uniform struct (must match WGSL layout) ─────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct Uniforms {
    pub sin_lat: f32,
    pub cos_lat: f32,
    pub lst_rad: f32,
    pub c_alt_rad: f32,
    pub c_az_rad: f32,
    pub fov_rad: f32,
    pub cx: f32,
    pub cy: f32,
    pub scale: f32,
    pub mag_limit: f32,
    pub canvas_w: f32,
    pub canvas_h: f32,
    pub dpr: f32,
}

// ── GpuSkyRenderer ──────────────────────────────────────────────────────────

pub struct GpuSkyRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,

    uniform_buf: wgpu::Buffer,
    star_indirect_buf: wgpu::Buffer,
    line_indirect_buf: wgpu::Buffer,

    project_pipeline: wgpu::ComputePipeline,
    compact_pipeline: wgpu::ComputePipeline,
    star_pipeline: wgpu::RenderPipeline,
    line_pipeline: wgpu::RenderPipeline,

    compute_bg: wgpu::BindGroup,
    render_bg: wgpu::BindGroup,

    star_count: u32,
    line_count: u32,
    width: u32,
    height: u32,
}

impl GpuSkyRenderer {
    pub async fn init(canvas: HtmlCanvasElement, star_data: Vec<[f32; 4]>, line_data: Vec<[u32; 2]>) -> Option<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU,
            ..Default::default()
        });

        let surface_target = wgpu::SurfaceTarget::Canvas(canvas.clone());
        let surface = instance.create_surface(surface_target).ok()?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .await?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("stars-gpu"),
                    required_features: wgpu::Features::INDIRECT_FIRST_INSTANCE,
                    required_limits: wgpu::Limits::default()
                        .using_resolution(adapter.limits()),
                    ..Default::default()
                },
                None,
            )
            .await
            .ok()?;

        let width = canvas.width().max(1);
        let height = canvas.height().max(1);

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .first()
            .copied()
            .unwrap_or(wgpu::TextureFormat::Bgra8Unorm);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // ── Buffers ──────────────────────────────────────────────────────────

        let star_count = star_data.len() as u32;
        let star_catalog_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("star_catalog"),
            contents: bytemuck::cast_slice(&star_data),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let line_count = line_data.len() as u32;
        let line_src_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("line_src"),
            contents: bytemuck::cast_slice(&line_data),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let projected_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("projected"),
            size: (star_count as u64) * 16,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Indirect draw args: [vertex_count, instance_count, first_vertex, first_instance]
        let star_indirect_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("star_indirect"),
            size: 16,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::INDIRECT
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let line_indirect_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("line_indirect"),
            size: 16,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::INDIRECT
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Compacted visible index buffers (worst case = all visible)
        let visible_star_ids_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible_star_ids"),
            size: (star_count as u64) * 4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });

        let visible_line_ids_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visible_line_ids"),
            size: (line_count as u64) * 4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });

        // ── Shader modules ───────────────────────────────────────────────────

        let compute_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("compute.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/stars.wgsl").into()),
        });

        let render_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("render.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/render.wgsl").into()),
        });

        // ── Compute bind group layout ────────────────────────────────────────
        // 0: uniforms          1: star_catalog (ro)
        // 2: projected (rw)    3: star_indirect (rw)
        // 4: visible_star_ids (rw)
        // 5: line_src (ro)     6: line_indirect (rw)
        // 7: visible_line_ids (rw)

        let compute_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("compute_bgl"),
            entries: &[
                bgl_uniform(0, wgpu::ShaderStages::COMPUTE),
                bgl_storage_ro(1, wgpu::ShaderStages::COMPUTE),
                bgl_storage_rw(2, wgpu::ShaderStages::COMPUTE),
                bgl_storage_rw(3, wgpu::ShaderStages::COMPUTE),
                bgl_storage_rw(4, wgpu::ShaderStages::COMPUTE),
                bgl_storage_ro(5, wgpu::ShaderStages::COMPUTE),
                bgl_storage_rw(6, wgpu::ShaderStages::COMPUTE),
                bgl_storage_rw(7, wgpu::ShaderStages::COMPUTE),
            ],
        });

        let compute_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("compute_bg"),
            layout: &compute_bgl,
            entries: &[
                bg_entry(0, &uniform_buf),
                bg_entry(1, &star_catalog_buf),
                bg_entry(2, &projected_buf),
                bg_entry(3, &star_indirect_buf),
                bg_entry(4, &visible_star_ids_buf),
                bg_entry(5, &line_src_buf),
                bg_entry(6, &line_indirect_buf),
                bg_entry(7, &visible_line_ids_buf),
            ],
        });

        // ── Render bind group layout ─────────────────────────────────────────
        // 0: uniforms                 1: projected (ro)
        // 2: visible_star_ids (ro)    3: line_indices (ro)
        // 4: visible_line_ids (ro)

        let render_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("render_bgl"),
            entries: &[
                bgl_uniform(0, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT),
                bgl_storage_ro(1, wgpu::ShaderStages::VERTEX),
                bgl_storage_ro(2, wgpu::ShaderStages::VERTEX),
                bgl_storage_ro(3, wgpu::ShaderStages::VERTEX),
                bgl_storage_ro(4, wgpu::ShaderStages::VERTEX),
                bgl_storage_ro(5, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT),
            ],
        });

        let render_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("render_bg"),
            layout: &render_bgl,
            entries: &[
                bg_entry(0, &uniform_buf),
                bg_entry(1, &projected_buf),
                bg_entry(2, &visible_star_ids_buf),
                bg_entry(3, &line_src_buf),
                bg_entry(4, &visible_line_ids_buf),
                bg_entry(5, &star_catalog_buf),
            ],
        });

        // ── Pipeline layouts ─────────────────────────────────────────────────

        let compute_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("compute_pl"),
            bind_group_layouts: &[&compute_bgl],
            push_constant_ranges: &[],
        });

        let render_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("render_pl"),
            bind_group_layouts: &[&render_bgl],
            push_constant_ranges: &[],
        });

        // ── Pipelines ────────────────────────────────────────────────────────

        let project_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("project_stars"),
            layout: Some(&compute_pl),
            module: &compute_shader,
            entry_point: Some("project_stars"),
            compilation_options: Default::default(),
            cache: None,
        });

        let compact_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("compact_lines"),
            layout: Some(&compute_pl),
            module: &compute_shader,
            entry_point: Some("compact_lines"),
            compilation_options: Default::default(),
            cache: None,
        });

        let star_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("star_render"),
            layout: Some(&render_pl),
            vertex: wgpu::VertexState {
                module: &render_shader,
                entry_point: Some("vs_star"),
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
                module: &render_shader,
                entry_point: Some("fs_star"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent::OVER,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });

        let line_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("line_render"),
            layout: Some(&render_pl),
            vertex: wgpu::VertexState {
                module: &render_shader,
                entry_point: Some("vs_line"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &render_shader,
                entry_point: Some("fs_line"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });

        Some(Self {
            device,
            queue,
            surface,
            surface_config,
            uniform_buf,
            star_indirect_buf,
            line_indirect_buf,
            project_pipeline,
            compact_pipeline,
            star_pipeline,
            line_pipeline,
            compute_bg,
            render_bg,
            star_count,
            line_count,
            width,
            height,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    pub fn render_frame(
        &self,
        uniforms: &Uniforms,
        show_stars: bool,
        show_constellations: bool,
    ) {
        // Upload uniforms + reset indirect draw args (instance_count = 0)
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(uniforms));
        self.queue
            .write_buffer(&self.star_indirect_buf, 0, bytemuck::cast_slice(&[4u32, 0, 0, 0]));
        self.queue
            .write_buffer(&self.line_indirect_buf, 0, bytemuck::cast_slice(&[2u32, 0, 0, 0]));

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sky_encoder"),
            });

        // ── Compute pass 1: project stars + compact visible star indices ─────
        {
            let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("project"),
                timestamp_writes: None,
            });
            cp.set_pipeline(&self.project_pipeline);
            cp.set_bind_group(0, &self.compute_bg, &[]);
            cp.dispatch_workgroups((self.star_count + 63) / 64, 1, 1);
        }

        // ── Compute pass 2: compact visible line indices ─────────────────────
        if show_constellations {
            let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("compact_lines"),
                timestamp_writes: None,
            });
            cp.set_pipeline(&self.compact_pipeline);
            cp.set_bind_group(0, &self.compute_bg, &[]);
            cp.dispatch_workgroups((self.line_count + 63) / 64, 1, 1);
        }

        // ── Render pass ──────────────────────────────────────────────────────
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sky_render"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.039,
                            g: 0.039,
                            b: 0.078,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            rp.set_bind_group(0, &self.render_bg, &[]);

            if show_constellations {
                rp.set_pipeline(&self.line_pipeline);
                rp.draw_indirect(&self.line_indirect_buf, 0);
            }

            if show_stars {
                rp.set_pipeline(&self.star_pipeline);
                rp.draw_indirect(&self.star_indirect_buf, 0);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

// ── Helpers for bind group layout boilerplate ────────────────────────────────

fn bgl_uniform(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn bgl_storage_ro(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn bgl_storage_rw(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: false },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn bg_entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}
