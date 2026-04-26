//! Bitmap font atlas + GPU text pipeline.
//!
//! The atlas is baked at SkyRenderer::init by:
//!   1. allocating a hidden `<canvas>` of `ATLAS_W × ATLAS_H`
//!   2. drawing every supported glyph at `DESIGN_PX` via Canvas2D `fillText`
//!   3. reading back the alpha channel and uploading as `R8Unorm`.
//!
//! This is the only place Canvas2D is touched — once-off, at startup. The
//! per-frame text pipeline is pure WebGPU. Eventually this should be replaced
//! by an SDF atlas baked at build time (see plan Phase 3 Option A); the
//! runtime API and shader are designed so that swap is local to `FontAtlas`.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

const ATLAS_W: u32 = 1024;
const ATLAS_H: u32 = 256;
const DESIGN_PX: f64 = 48.0;
const CELL_W: u32 = 32;
const CELL_H: u32 = 64;
const COLS: u32 = ATLAS_W / CELL_W;
// ASCII printable + a handful of EN/FR accented characters.
const CHARS: &str =
    " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~àâäéèêëîïôöûüùç";

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct TextInstance {
    pub pos_x:  f32,
    pub pos_y:  f32,
    pub size_x: f32,
    pub size_y: f32,
    pub u0:     f32,
    pub v0:     f32,
    pub du:     f32,
    pub dv:     f32,
    pub r:      f32,
    pub g:      f32,
    pub b:      f32,
    pub a:      f32,
}

#[derive(Copy, Clone)]
pub struct GlyphInfo {
    pub u0: f32,
    pub v0: f32,
    pub du: f32,
    pub dv: f32,
    pub advance_px: f32, // advance in design pixels
}

pub struct FontAtlas {
    pub texture: wgpu::Texture,
    pub view:    wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub glyphs:  HashMap<char, GlyphInfo>,
}

impl FontAtlas {
    pub fn build(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Self> {
        let document = web_sys::window()?.document()?;
        let canvas: HtmlCanvasElement = document
            .create_element("canvas").ok()?
            .dyn_into().ok()?;
        canvas.set_width(ATLAS_W);
        canvas.set_height(ATLAS_H);
        let ctx: CanvasRenderingContext2d = canvas
            .get_context("2d").ok()??
            .dyn_into().ok()?;
        ctx.set_font(&format!("{}px monospace", DESIGN_PX as u32));
        ctx.set_text_baseline("top");
        ctx.set_fill_style_str("white");
        ctx.clear_rect(0.0, 0.0, ATLAS_W as f64, ATLAS_H as f64);

        let mut glyphs = HashMap::with_capacity(128);
        for (i, ch) in CHARS.chars().enumerate() {
            let i = i as u32;
            if i / COLS >= ATLAS_H / CELL_H {
                break; // atlas exhausted
            }
            let col = i % COLS;
            let row = i / COLS;
            let x = (col * CELL_W) as f64;
            let y = (row * CELL_H) as f64;
            // Inset by 1 px so glyphs don't bleed into neighbouring cells.
            let _ = ctx.fill_text(&ch.to_string(), x + 1.0, y + 1.0);
            let advance = ctx
                .measure_text(&ch.to_string())
                .map(|m| m.width() as f32)
                .unwrap_or(DESIGN_PX as f32 * 0.6);
            glyphs.insert(ch, GlyphInfo {
                u0: x as f32 / ATLAS_W as f32,
                v0: y as f32 / ATLAS_H as f32,
                du: CELL_W as f32 / ATLAS_W as f32,
                dv: CELL_H as f32 / ATLAS_H as f32,
                advance_px: advance,
            });
        }

        let img_data = ctx
            .get_image_data(0.0, 0.0, ATLAS_W as f64, ATLAS_H as f64)
            .ok()?;
        let rgba = img_data.data();
        let n = (ATLAS_W * ATLAS_H) as usize;
        let mut alpha = vec![0u8; n];
        for i in 0..n {
            alpha[i] = rgba[i * 4 + 3];
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("font_atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &alpha,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_W),
                rows_per_image: Some(ATLAS_H),
            },
            wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("font_atlas_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        Some(Self { texture, view, sampler, glyphs })
    }

    /// Push a horizontal text run starting at (x, y) (top-left, CSS px).
    /// Returns the rendered advance width in CSS px.
    pub fn push_text(
        &self,
        out: &mut Vec<TextInstance>,
        text: &str,
        x: f32,
        y: f32,
        px_size: f32,
        rgba: [f32; 4],
    ) -> f32 {
        let scale = px_size / DESIGN_PX as f32;
        let mut cx = x;
        for ch in text.chars() {
            let g = match self.glyphs.get(&ch) {
                Some(g) => *g,
                None => match self.glyphs.get(&'?') {
                    Some(g) => *g,
                    None => continue,
                },
            };
            out.push(TextInstance {
                pos_x: cx,
                pos_y: y,
                size_x: CELL_W as f32 * scale,
                size_y: CELL_H as f32 * scale,
                u0: g.u0, v0: g.v0, du: g.du, dv: g.dv,
                r: rgba[0], g: rgba[1], b: rgba[2], a: rgba[3],
            });
            cx += g.advance_px * scale;
        }
        cx - x
    }

    /// Approximate width of a string when rendered at `px_size` (CSS px).
    pub fn measure_width(&self, text: &str, px_size: f32) -> f32 {
        let scale = px_size / DESIGN_PX as f32;
        text.chars()
            .map(|ch| {
                self.glyphs
                    .get(&ch)
                    .map(|g| g.advance_px)
                    .unwrap_or(DESIGN_PX as f32 * 0.6)
                    * scale
            })
            .sum()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// TextLayer: pipeline + per-frame instance buffer
// ───────────────────────────────────────────────────────────────────────────

const ITEM_BYTES: u64 = std::mem::size_of::<TextInstance>() as u64;
const INITIAL_CAP: u64 = 1024;

pub struct TextLayer {
    pipeline:   wgpu::RenderPipeline,
    bgl:        wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    inst_buf:   wgpu::Buffer,
    capacity:   u64,
    count:      u32,
    pub atlas:  FontAtlas,
}

impl TextLayer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        uniform_buf: &wgpu::Buffer,
    ) -> Option<Self> {
        let atlas = FontAtlas::build(device, queue)?;

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("text_bgl"),
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
                        view_dimension: wgpu::TextureViewDimension::D2,
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
            label: Some("text_instances"),
            size: INITIAL_CAP * ITEM_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group =
            make_bind_group(device, &bgl, uniform_buf, &inst_buf, &atlas.view, &atlas.sampler);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("text.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/text.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("text_pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("text_pipeline"),
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

        Some(Self {
            pipeline,
            bgl,
            bind_group,
            inst_buf,
            capacity: INITIAL_CAP,
            count: 0,
            atlas,
        })
    }

    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uniform_buf: &wgpu::Buffer,
        items: &[TextInstance],
    ) {
        self.count = items.len() as u32;
        if items.is_empty() { return; }
        let needed = items.len() as u64;
        if needed > self.capacity {
            let new_cap = needed.next_power_of_two().max(INITIAL_CAP);
            self.inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("text_instances"),
                size: new_cap * ITEM_BYTES,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
            self.bind_group = make_bind_group(
                device, &self.bgl, uniform_buf, &self.inst_buf,
                &self.atlas.view, &self.atlas.sampler,
            );
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
    atlas_view: &wgpu::TextureView,
    atlas_sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("text_bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: inst_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(atlas_view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(atlas_sampler) },
        ],
    })
}
