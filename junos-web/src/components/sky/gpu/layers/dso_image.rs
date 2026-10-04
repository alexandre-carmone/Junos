//! Deep-sky object image sprites.
//!
//! The planetarium's SkySafari-style imagery: survey cutouts of each object
//! (`thumbs/<slug>.jpg` from the offline tile cache) drawn as additive quads
//! at the object's true angular size, rotated to sky north. The sprites live
//! in one `texture_2d_array` with a full mip chain; a slot is assigned per
//! object on first use and recycled least-recently-drawn-first once the array
//! is full. Uploads are rate-limited per frame — each one is a canvas
//! halving chain plus one `copyExternalImageToTexture` per level.
//!
//! `SpriteRequest`s come from the CPU layer (`render/layers/dso_image.rs`),
//! which has already culled, sized and oriented them and fetched the image.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use web_sys::HtmlImageElement;

use super::super::texture_upload::MipUploader;

/// One sprite the CPU side wants drawn this frame. `img` is decoded.
#[derive(Clone)]
pub struct SpriteRequest {
    /// Catalog index — the key a texture slot is remembered by.
    pub id: u32,
    pub img: HtmlImageElement,
    pub pos_x: f32,
    pub pos_y: f32,
    pub half: f32,
    pub cos_rot: f32,
    pub sin_rot: f32,
    pub brightness: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct DsoImageInstance {
    pub pos_x:      f32,
    pub pos_y:      f32,
    pub half:       f32,
    pub cos_rot:    f32,
    pub sin_rot:    f32,
    pub brightness: f32,
    pub layer:      u32,
    pub _pad:       u32,
}

const ITEM_BYTES: u64 = std::mem::size_of::<DsoImageInstance>() as u64;
const INITIAL_CAP: u64 = 64;
/// Texture uploads per frame; more would stall a pan while sprites stream in.
const MAX_UPLOADS_PER_FRAME: usize = 2;

pub struct DsoImageLayer {
    pipeline:   wgpu::RenderPipeline,
    bgl:        wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    inst_buf:   wgpu::Buffer,
    capacity:   u64,
    count:      u32,

    texture: wgpu::Texture,
    view:    wgpu::TextureView,
    sampler: wgpu::Sampler,
    size:    u32,
    levels:  u32,

    /// Catalog id resident in each array layer.
    slots:     Vec<Option<u32>>,
    last_used: Vec<u64>,
    resident:  HashMap<u32, u32>,
    frame:     u64,
    uploader:  MipUploader,
    insts:     Vec<DsoImageInstance>,
}

impl DsoImageLayer {
    /// Sprite side and slot count: 512 px × 48 on desktop (64 MiB with
    /// mips), 256 px × 32 on phones (11 MiB).
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        uniform_buf: &wgpu::Buffer,
        is_mobile: bool,
    ) -> Option<Self> {
        let uploader = MipUploader::new()?;
        let (size, n_layers) = if is_mobile { (256u32, 32u32) } else { (512u32, 48u32) };
        let levels = MipUploader::levels_for(size, size);

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dso_thumbs"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: n_layers },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Not sRGB: the surface is a plain unorm format and every other
            // pipeline writes encoded colour straight through.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: MipUploader::USAGE,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("dso_thumbs_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dso_image_bgl"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dso_image_instances"),
            size: INITIAL_CAP * ITEM_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = make_bind_group(device, &bgl, uniform_buf, &inst_buf, &view, &sampler);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("dso_image.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/dso_image.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("dso_image_pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("dso_image_pipeline"),
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
                    // Additive: black (the sprites' faded edges and empty sky)
                    // contributes nothing, overlapping sprites sum.
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
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
            bind_group,
            inst_buf,
            capacity: INITIAL_CAP,
            count: 0,
            texture,
            view,
            sampler,
            size,
            levels,
            slots: vec![None; n_layers as usize],
            last_used: vec![0; n_layers as usize],
            resident: HashMap::new(),
            frame: 0,
            uploader,
            insts: Vec::with_capacity(n_layers as usize),
        })
    }

    /// Slot count — the CPU layer caps its requests at this.
    pub fn capacity_layers(&self) -> usize {
        self.slots.len()
    }

    /// Resolve this frame's requests to instances: resident sprites draw at
    /// once, a few missing ones are uploaded into recycled slots, the rest
    /// wait for a later frame.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uniform_buf: &wgpu::Buffer,
        reqs: &[SpriteRequest],
    ) {
        self.frame += 1;
        let frame = self.frame;
        self.insts.clear();

        let mut pending: Vec<&SpriteRequest> = Vec::new();
        for r in reqs {
            match self.resident.get(&r.id) {
                Some(&layer) => {
                    self.last_used[layer as usize] = frame;
                    self.insts.push(instance(r, layer));
                }
                None => pending.push(r),
            }
        }

        for r in pending.into_iter().take(MAX_UPLOADS_PER_FRAME) {
            let Some(slot) = self.pick_slot() else { break };
            if let Some(old) = self.slots[slot].take() {
                self.resident.remove(&old);
            }
            let ok = self.uploader.upload(
                queue, &self.texture, slot as u32, &r.img, self.size, self.size, self.levels,
            );
            if !ok {
                continue;
            }
            self.slots[slot] = Some(r.id);
            self.last_used[slot] = frame;
            self.resident.insert(r.id, slot as u32);
            self.insts.push(instance(r, slot as u32));
        }

        self.count = self.insts.len() as u32;
        if self.insts.is_empty() {
            return;
        }
        let needed = self.insts.len() as u64;
        if needed > self.capacity {
            let new_cap = needed.next_power_of_two().max(INITIAL_CAP);
            self.inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dso_image_instances"),
                size: new_cap * ITEM_BYTES,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
            self.bind_group = make_bind_group(
                device, &self.bgl, uniform_buf, &self.inst_buf, &self.view, &self.sampler,
            );
        }
        queue.write_buffer(&self.inst_buf, 0, bytemuck::cast_slice(&self.insts));
    }

    /// A free slot, else the least recently drawn one not used this frame.
    fn pick_slot(&self) -> Option<usize> {
        if let Some(i) = self.slots.iter().position(|s| s.is_none()) {
            return Some(i);
        }
        self.last_used
            .iter()
            .enumerate()
            .filter(|&(_, &t)| t < self.frame)
            .min_by_key(|&(_, &t)| t)
            .map(|(i, _)| i)
    }

    pub fn draw<'a>(&'a self, rp: &mut wgpu::RenderPass<'a>) {
        if self.count == 0 { return; }
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.bind_group, &[]);
        rp.draw(0..4, 0..self.count);
    }
}

fn instance(r: &SpriteRequest, layer: u32) -> DsoImageInstance {
    DsoImageInstance {
        pos_x: r.pos_x,
        pos_y: r.pos_y,
        half: r.half,
        cos_rot: r.cos_rot,
        sin_rot: r.sin_rot,
        brightness: r.brightness,
        layer,
        _pad: 0,
    }
}

fn make_bind_group(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    uniform_buf: &wgpu::Buffer,
    inst_buf: &wgpu::Buffer,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("dso_image_bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: inst_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}
