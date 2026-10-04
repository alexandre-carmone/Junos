//! All-sky Milky Way background.
//!
//! One plate-carrée panorama of the sky (`allsky.jpg` from the offline tile
//! cache) in a mipmapped `texture_2d`, drawn as a full-screen triangle first
//! in the render pass; the fragment shader inverts the star projection per
//! pixel and samples it (`shaders/allsky.wgsl`). The texture is created on
//! the first frame the decoded image is handed over and kept for the
//! renderer's lifetime; only the opacity changes after that.

use bytemuck::{Pod, Zeroable};
use web_sys::HtmlImageElement;

use super::super::texture_upload::MipUploader;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct SkyParams {
    opacity: f32,
    _p0: f32,
    _p1: f32,
    _p2: f32,
}

pub struct AllskyLayer {
    pipeline:   wgpu::RenderPipeline,
    bgl:        wgpu::BindGroupLayout,
    sampler:    wgpu::Sampler,
    params_buf: wgpu::Buffer,
    /// Texture, view and bind group once the panorama has been uploaded.
    loaded:     Option<(wgpu::Texture, wgpu::BindGroup)>,
    uploader:   MipUploader,
    max_dim:    u32,
    opacity:    f32,
    enabled:    bool,
}

impl AllskyLayer {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        uniform_buf: &wgpu::Buffer,
    ) -> Option<Self> {
        let uploader = MipUploader::new()?;
        let _ = uniform_buf;
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("allsky_sampler"),
            // RA wraps; Dec clamps at the poles.
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("allsky_params"),
            size: std::mem::size_of::<SkyParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("allsky_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("allsky.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/allsky.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("allsky_pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("allsky_pipeline"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
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
            pipeline,
            bgl,
            sampler,
            params_buf,
            loaded: None,
            uploader,
            max_dim: device.limits().max_texture_dimension_2d,
            opacity: 0.0,
            enabled: false,
        })
    }

    /// Per frame: hand over the decoded panorama (uploaded once) and the
    /// current opacity. `enabled` false skips the draw without unloading.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uniform_buf: &wgpu::Buffer,
        img: Option<&HtmlImageElement>,
        enabled: bool,
        opacity: f32,
    ) {
        self.enabled = enabled;
        if !enabled {
            return;
        }
        if self.loaded.is_none() {
            let Some(img) = img else { return };
            if !img.complete() || img.natural_width() == 0 {
                return;
            }
            // Keep the 2:1 plate carrée; shrink to the adapter's limit if the
            // file is wider than it can hold.
            let w = img.natural_width().min(self.max_dim).max(2);
            let h = (w / 2).max(1);
            let levels = MipUploader::levels_for(w, h);
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("allsky"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: levels,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: MipUploader::USAGE,
                view_formats: &[],
            });
            if !self.uploader.upload(queue, &texture, 0, img, w, h, levels) {
                return;
            }
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("allsky_bg"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 3, resource: self.params_buf.as_entire_binding() },
                ],
            });
            self.loaded = Some((texture, bind_group));
        }
        if (opacity - self.opacity).abs() > 1e-6 || self.opacity == 0.0 {
            self.opacity = opacity;
            let params = SkyParams { opacity, _p0: 0.0, _p1: 0.0, _p2: 0.0 };
            queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));
        }
    }

    pub fn draw<'a>(&'a self, rp: &mut wgpu::RenderPass<'a>) {
        if !self.enabled { return; }
        let Some((_, bind_group)) = self.loaded.as_ref() else { return };
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, bind_group, &[]);
        rp.draw(0..3, 0..1);
    }
}
