//! The sky map's GPU side: an atlas of the resident HiPS tiles and the
//! full-screen pass that samples it (`shaders/skymap.wgsl`).
//!
//! Tiles live in one `texture_2d_array` (512² layers, full mip chain); a
//! layer is assigned per tile on first use and recycled least-recently-drawn
//! first once the array is full. A storage-buffer table, flattened over the
//! HEALPix orders 0..=`MAX_TABLE_ORDER`, maps every cell to its layer + 1 (0 =
//! not resident), so the fragment shader finds a tile with one load per
//! order it tries. Uploads are rate-limited per frame — each one is a canvas
//! halving chain plus one `copyExternalImageToTexture` per level.
//!
//! `TileRequest`s come from the CPU layer (`render/layers/skymap.rs`), which
//! has chosen the order, listed the cells under the view and fetched the
//! images; parents come first in the list so they land first.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use web_sys::HtmlImageElement;

use super::super::texture_upload::MipUploader;
use crate::healpix;

/// One tile the CPU side wants resident this frame. `img` is decoded.
#[derive(Clone)]
pub struct TileRequest {
    /// `skymap::tile_id(order, npix)` — the key a slot is remembered by.
    pub id: u32,
    pub order: u8,
    pub npix: u32,
    pub img: HtmlImageElement,
}

/// Everything the CPU layer decided this frame.
#[derive(Default)]
pub struct SkymapPrepare {
    pub enabled: bool,
    /// Parents first, then the view's cells nearest-first.
    pub tiles: Vec<TileRequest>,
    pub order: u8,
    pub order_min: u8,
    pub tile_px: u32,
    pub opacity: f32,
    pub brightness: f32,
    /// log2(tile texels per screen px) at `order`.
    pub lod: f32,
}

impl SkymapPrepare {
    pub fn clear(&mut self) {
        self.enabled = false;
        self.tiles.clear();
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct SkymapParams {
    order:      u32,
    order_min:  u32,
    tile_px:    u32,
    _p0:        u32,
    opacity:    f32,
    brightness: f32,
    lod:        f32,
    _p1:        f32,
}

/// Deepest order the slot table addresses (4 MiB of `u32`); the CPU layer
/// clamps the view's order to it.
pub const MAX_TABLE_ORDER: u8 = 8;
/// Texture uploads per frame (each ~1–2 ms: a canvas halving chain plus
/// one copy per mip level); more would stall a pan while tiles stream in.
/// The sky keeps animating at full rate while any are pending (`pending`).
const MAX_UPLOADS_PER_FRAME: usize = 4;

pub struct SkymapLayer {
    pipeline:   wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    params_buf: wgpu::Buffer,
    table_buf:  wgpu::Buffer,
    /// CPU mirror of `table_buf`.
    table:      Vec<u32>,

    texture: wgpu::Texture,
    size:    u32,
    levels:  u32,

    /// (order, npix) resident in each array layer.
    slots:     Vec<Option<(u8, u32)>>,
    last_used: Vec<u64>,
    resident:  HashMap<u32, u32>,
    frame:     u64,
    uploader:  MipUploader,
    params:    Option<SkymapParams>,
    enabled:   bool,
    /// Requested, decoded tiles still waiting for an upload slot.
    pending:   usize,
}

impl SkymapLayer {
    /// 512 px tiles × 64 layers on desktop (85 MiB with mips), × 40 on
    /// phones (53 MiB) — enough for the cells of a view plus their parents.
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        uniform_buf: &wgpu::Buffer,
        is_mobile: bool,
    ) -> Option<Self> {
        let uploader = MipUploader::new()?;
        let size = 512u32;
        let n_layers: u32 = if is_mobile { 40 } else { 64 };
        let levels = MipUploader::levels_for(size, size);

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("skymap_tiles"),
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
            label: Some("skymap_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let table_len = (healpix::table_base(MAX_TABLE_ORDER) + healpix::n_cells(MAX_TABLE_ORDER)) as usize;
        let table_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("skymap_table"),
            size: (table_len * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("skymap_params"),
            size: std::mem::size_of::<SkymapParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("skymap_bgl"),
            entries: &[
                uniform(0),
                uniform(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("skymap_bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: params_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: table_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("skymap.wgsl"),
            source: wgpu::ShaderSource::Wgsl(
                concat!(
                    include_str!("../../shaders/sky_inverse.wgsl"),
                    include_str!("../../shaders/skymap.wgsl"),
                )
                .into(),
            ),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("skymap_pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("skymap_pipeline"),
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
                    // Premultiplied over: the image fades in over the Milky
                    // Way panorama as the field narrows; absent tiles write 0.
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
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
            bind_group,
            params_buf,
            table_buf,
            table: vec![0; table_len],
            texture,
            size,
            levels,
            slots: vec![None; n_layers as usize],
            last_used: vec![0; n_layers as usize],
            resident: HashMap::new(),
            frame: 0,
            uploader,
            params: None,
            enabled: false,
            pending: 0,
        })
    }

    /// Mark the resident requests used, upload a few missing ones into
    /// recycled slots (in list order, so parents land first) and refresh
    /// the parameters. Tiles not requested this frame stay resident: the
    /// shader only reads the view's order and its parents, and panning
    /// back is then free.
    pub fn upload(&mut self, queue: &wgpu::Queue, prep: &SkymapPrepare) {
        self.enabled = prep.enabled;
        self.pending = 0;
        if !prep.enabled {
            return;
        }
        self.frame += 1;
        let frame = self.frame;

        let mut pending: Vec<&TileRequest> = Vec::new();
        for r in &prep.tiles {
            match self.resident.get(&r.id) {
                Some(&layer) => self.last_used[layer as usize] = frame,
                None => pending.push(r),
            }
        }
        self.pending = pending.len().saturating_sub(MAX_UPLOADS_PER_FRAME);
        for r in pending.into_iter().take(MAX_UPLOADS_PER_FRAME) {
            if r.order > MAX_TABLE_ORDER {
                continue;
            }
            let Some(slot) = self.pick_slot() else {
                // Every layer is in use this frame: nothing more can land
                // until the view changes, so don't keep the sky spinning.
                self.pending = 0;
                break;
            };
            let ok = self.uploader.upload(
                queue, &self.texture, slot as u32, &r.img, self.size, self.size, self.levels,
            );
            if !ok {
                continue;
            }
            if let Some((o, n)) = self.slots[slot].take() {
                self.resident.remove(&crate::skymap::tile_id(o, n));
                self.set_table(queue, o, n, 0);
            }
            self.slots[slot] = Some((r.order, r.npix));
            self.last_used[slot] = frame;
            self.resident.insert(r.id, slot as u32);
            self.set_table(queue, r.order, r.npix, slot as u32 + 1);
        }

        let params = SkymapParams {
            order: prep.order.min(MAX_TABLE_ORDER) as u32,
            order_min: prep.order_min.min(prep.order) as u32,
            tile_px: prep.tile_px.max(1),
            _p0: 0,
            opacity: prep.opacity,
            brightness: prep.brightness,
            lod: prep.lod,
            _p1: 0.0,
        };
        if self.params.is_none_or(|p| bytemuck::bytes_of(&p) != bytemuck::bytes_of(&params)) {
            queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));
            self.params = Some(params);
        }
    }

    /// Decoded tiles still waiting for an upload after this frame.
    pub fn pending(&self) -> usize {
        self.pending
    }

    fn set_table(&mut self, queue: &wgpu::Queue, order: u8, npix: u32, value: u32) {
        let idx = (healpix::table_base(order) + npix) as usize;
        if idx >= self.table.len() {
            return;
        }
        self.table[idx] = value;
        queue.write_buffer(&self.table_buf, (idx * 4) as u64, bytemuck::bytes_of(&value));
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
        if !self.enabled || self.resident.is_empty() {
            return;
        }
        if self.params.is_some_and(|p| p.opacity <= 0.0) {
            return;
        }
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.bind_group, &[]);
        rp.draw(0..3, 0..1);
    }
}
