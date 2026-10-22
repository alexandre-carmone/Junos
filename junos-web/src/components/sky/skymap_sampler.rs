//! Reprojected cutouts of the sky map, on the CPU: the Framing Assistant's
//! preview and the Targets tab's thumbnails.
//!
//! A `CutoutSpec` is a tangent-plane (TAN) image centred on a J2000
//! position, north up / east left at rotation 0 (`astro::frame_point_eq`'s
//! convention, the one the FOV boxes and the Framing grid use). `render`
//! picks the HEALPix order whose tile pixel matches the output pixel, lists
//! the cells under the field, loads their tiles through a `TileImageCache`,
//! extracts each decoded tile's pixels once (a scratch canvas + getImageData,
//! LRU-bounded), then fills an `ImageData` pixel by pixel: inverse TAN →
//! `healpix::hash_dxdy` → the tile's texel — falling back to the parent
//! cell while a tile is still loading, like the GPU shader does.
//!
//! Cost is ~100 ns per output pixel in wasm, so callers render at a bounded
//! internal size and let `render_into` scale the result onto their canvas.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::{Clamped, JsCast, JsValue};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlImageElement, ImageData};

use super::tile_images::TileImageCache;
use crate::astro;
use crate::healpix;
use crate::skymap::{tile_id, tile_url, SkyMapInfo};

/// What to render. Angles in degrees, the centre in J2000.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutoutSpec {
    pub ra_deg: f64,
    pub dec_deg: f64,
    /// Field across the shorter output side.
    pub fov_deg: f64,
    pub width: u32,
    pub height: u32,
    /// Image-up, degrees east of north (0 = north up, east left).
    pub rot_deg: f64,
}

/// How far a render got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CutoutStatus {
    pub order: u8,
    /// Cells of `order` under the field, and how many had their tile.
    pub tiles_wanted: usize,
    pub tiles_drawn: usize,
    pub loading: usize,
    pub complete: bool,
}

/// Cells of the output order asked for (those whose footprint meets the
/// image, nearest first); the rest of the field shows parents.
const MAX_CELLS: usize = 64;
const MAX_PARENTS: usize = 12;
/// Output texel ≈ this many tile texels: a shade finer than 1:1 so the
/// nearest-texel sampling doesn't go blocky.
const ORDER_BIAS: f64 = 0.3;

pub struct SkyMapSampler {
    info: Arc<SkyMapInfo>,
    cache: TileImageCache,
    /// Extracted RGBA of decoded tiles, `tile_width²` × 4 bytes each.
    pixels: HashMap<u32, Rc<Vec<u8>>>,
    pixels_lru: VecDeque<u32>,
    /// A soft cap: `trim` brings the map back under it after a render, but
    /// never drops a tile the render in progress wants — evicting those
    /// (parents first, as the oldest) left black cells in the output.
    max_pixels: usize,
    scratch: HtmlCanvasElement,
    scratch_ctx: CanvasRenderingContext2d,
    /// The last render, kept so an unchanged request is a plain `drawImage`.
    result: HtmlCanvasElement,
    result_ctx: CanvasRenderingContext2d,
    last: Option<(CutoutSpec, usize, CutoutStatus)>,
    lite: bool,
}

impl SkyMapSampler {
    /// `epoch` is bumped whenever a tile lands (the caller re-renders on it);
    /// `img_cap` decoded tiles and `px_cap` extracted tiles are kept at most.
    pub fn new(
        epoch: RwSignal<u32>,
        info: Arc<SkyMapInfo>,
        lite: bool,
        img_cap: usize,
        px_cap: usize,
    ) -> Option<Self> {
        let document = web_sys::window()?.document()?;
        let mk = |read_often: bool| -> Option<(HtmlCanvasElement, CanvasRenderingContext2d)> {
            let canvas: HtmlCanvasElement = document.create_element("canvas").ok()?.dyn_into().ok()?;
            let ctx = if read_often {
                let opts = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&opts, &"willReadFrequently".into(), &JsValue::TRUE);
                canvas.get_context_with_context_options("2d", &opts).ok()??
            } else {
                canvas.get_context("2d").ok()??
            };
            Some((canvas, ctx.dyn_into().ok()?))
        };
        let (scratch, scratch_ctx) = mk(true)?;
        let (result, result_ctx) = mk(false)?;
        Some(Self {
            info,
            cache: TileImageCache::with_cap(epoch, img_cap),
            pixels: HashMap::new(),
            pixels_lru: VecDeque::new(),
            max_pixels: px_cap,
            scratch,
            scratch_ctx,
            result,
            result_ctx,
            last: None,
            lite,
        })
    }

    /// The order whose tile texel matches an output pixel of `spec`.
    pub fn order_for(&self, spec: &CutoutSpec) -> u8 {
        let short = spec.width.min(spec.height).max(1) as f64;
        let px_per_deg = short / spec.fov_deg.max(1e-6);
        let ideal = (healpix::ORDER0_SIDE_DEG * px_per_deg / self.info.tile_width.max(1) as f64).log2();
        let max = self.info.usable_order(self.lite, 12).unwrap_or(0);
        (ideal + ORDER_BIAS).round().clamp(0.0, max as f64) as u8
    }

    /// Render `spec` (if it or the available tiles changed since the last
    /// call) and draw it scaled into `(x, y, w, h)` of `ctx`, in `ctx`'s
    /// current coordinates. `None` when nothing could be drawn at all.
    pub fn render_into(
        &mut self,
        ctx: &CanvasRenderingContext2d,
        spec: &CutoutSpec,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> Option<CutoutStatus> {
        let status = self.render(spec)?;
        let _ = ctx.draw_image_with_html_canvas_element_and_dw_and_dh(&self.result, x, y, w, h);
        Some(status)
    }

    /// Render `spec` into the result canvas (sized `spec.width × height`).
    pub fn render(&mut self, spec: &CutoutSpec) -> Option<CutoutStatus> {
        if spec.width == 0 || spec.height == 0 || !(spec.fov_deg > 0.0) {
            return None;
        }
        let order = self.order_for(spec);

        // ── Cells under the field, parents first ────────────────────────
        let (w, h) = (spec.width as f64, spec.height as f64);
        let half_diag_deg = spec.fov_deg * 0.5 * w.hypot(h) / w.min(h);
        let half_cell = healpix::ORDER0_SIDE_DEG * std::f64::consts::SQRT_2 / (2u32 << order) as f64;
        let radius = (half_diag_deg + half_cell).min(180.0).to_radians();
        let (lon, lat) = (spec.ra_deg.to_radians(), spec.dec_deg.to_radians());
        // The cone covers the image diagonal; keep the cells whose projected
        // footprint meets the image rectangle (partially visible ones too),
        // so the cap never drops a corner for an off-image cell.
        let ppd = w.min(h) / spec.fov_deg;
        let (cx, cy) = (w / 2.0, h / 2.0);
        let on_image = |o: u8, hash: u32| -> bool {
            let (clon, clat) = healpix::cell_center(o, hash);
            let mut pts = healpix::cell_vertices(o, hash).to_vec();
            pts.push((clon, clat));
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for (vlon, vlat) in pts {
                let Some((right, up)) =
                    astro::frame_offset_of(spec.ra_deg, spec.dec_deg, spec.rot_deg, vlon.to_degrees(), vlat.to_degrees())
                else {
                    return true; // off the tangent plane: a huge cell, keep it
                };
                let (x, y) = (cx + right * ppd, cy - up * ppd);
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
            x1 >= -1.0 && x0 <= w + 1.0 && y1 >= -1.0 && y0 <= h + 1.0
        };
        let mut cells = healpix::cone_cells(order, lon, lat, radius);
        cells.retain(|&c| on_image(order, c));
        cells.truncate(MAX_CELLS);
        let parent_order = self.info.parent_order(order).filter(|&po| po < order);
        let mut parents = match parent_order {
            Some(po) => healpix::cone_cells(po, lon, lat, radius),
            None => Vec::new(),
        };
        if let Some(po) = parent_order {
            parents.retain(|&c| on_image(po, c));
        }
        parents.truncate(MAX_PARENTS);

        self.cache.begin_frame();
        let mut ready = 0usize;
        let mut drawn = 0usize;
        let mut wanted: HashSet<u32> = HashSet::new();
        for (o, list) in [(parent_order.unwrap_or(order), &parents), (order, &cells)] {
            for &npix in list {
                let id = tile_id(o, npix);
                wanted.insert(id);
                let Some(img) = self.cache.image(id, &tile_url(o, npix)) else { continue };
                if self.extract(id, &img) {
                    ready += 1;
                    if o == order {
                        drawn += 1;
                    }
                }
            }
        }
        self.trim(&wanted);
        let status = CutoutStatus {
            order,
            tiles_wanted: cells.len(),
            tiles_drawn: drawn,
            loading: self.cache.loading_count(),
            complete: drawn == cells.len(),
        };
        if let Some((last_spec, last_ready, last_status)) = &self.last {
            if last_spec == spec && *last_ready == ready {
                return Some(CutoutStatus { loading: status.loading, ..*last_status });
            }
        }
        if ready == 0 {
            return None;
        }

        // ── Fill ────────────────────────────────────────────────────────
        let tile_px = self.info.tile_width.max(1);
        let tpf = tile_px as f64;
        let mut data = vec![0u8; (spec.width * spec.height * 4) as usize];
        let mut i = 0usize;
        for py in 0..spec.height {
            let dy_deg = (cy - (py as f64 + 0.5)) / ppd;
            for px in 0..spec.width {
                let dx_deg = (px as f64 + 0.5 - cx) / ppd;
                let (ra, dec) = astro::frame_point_eq(spec.ra_deg, spec.dec_deg, spec.rot_deg, dx_deg, dy_deg);
                let hd = healpix::hash_dxdy(order, ra.to_radians(), dec.to_radians());
                let (mut hash, mut dx, mut dy, mut o) = (hd.hash, hd.dx, hd.dy, order);
                let mut texel: Option<(&Rc<Vec<u8>>, usize)> = None;
                loop {
                    if let Some(p) = self.pixels.get(&tile_id(o, hash)) {
                        let col = ((dy * tpf) as u32).min(tile_px - 1);
                        let row = ((dx * tpf) as u32).min(tile_px - 1);
                        texel = Some((p, ((row * tile_px + col) * 4) as usize));
                        break;
                    }
                    if o == 0 {
                        break;
                    }
                    let sub = hash & 3;
                    hash >>= 2;
                    dx = ((sub & 1) as f64 + dx) * 0.5;
                    dy = ((sub >> 1) as f64 + dy) * 0.5;
                    o -= 1;
                }
                if let Some((p, k)) = texel {
                    data[i..i + 3].copy_from_slice(&p[k..k + 3]);
                }
                data[i + 3] = 255;
                i += 4;
            }
        }
        let image = ImageData::new_with_u8_clamped_array_and_sh(Clamped(&data), spec.width, spec.height).ok()?;
        if self.result.width() != spec.width {
            self.result.set_width(spec.width);
        }
        if self.result.height() != spec.height {
            self.result.set_height(spec.height);
        }
        self.result_ctx.put_image_data(&image, 0.0, 0.0).ok()?;
        self.last = Some((*spec, ready, status));
        Some(status)
    }

    /// Make sure the decoded tile's pixels are extracted. `false` on failure.
    fn extract(&mut self, id: u32, img: &HtmlImageElement) -> bool {
        if self.pixels.contains_key(&id) {
            // Most recently used goes to the back.
            if let Some(i) = self.pixels_lru.iter().position(|&k| k == id) {
                self.pixels_lru.remove(i);
                self.pixels_lru.push_back(id);
            }
            return true;
        }
        let px = self.info.tile_width.max(1);
        if self.scratch.width() != px {
            self.scratch.set_width(px);
        }
        if self.scratch.height() != px {
            self.scratch.set_height(px);
        }
        let ok = self
            .scratch_ctx
            .draw_image_with_html_image_element_and_dw_and_dh(img, 0.0, 0.0, px as f64, px as f64)
            .is_ok();
        if !ok {
            return false;
        }
        let Ok(data) = self.scratch_ctx.get_image_data(0.0, 0.0, px as f64, px as f64) else {
            return false;
        };
        let bytes: Vec<u8> = data.data().0;
        self.pixels.insert(id, Rc::new(bytes));
        self.pixels_lru.push_back(id);
        true
    }

    /// Drop the least recently used extracted tiles not in `keep` until the
    /// map is back under its cap (it may stay above it while `keep` alone
    /// exceeds the cap).
    fn trim(&mut self, keep: &HashSet<u32>) {
        if self.pixels.len() <= self.max_pixels {
            return;
        }
        let mut kept = VecDeque::with_capacity(self.pixels_lru.len());
        while let Some(id) = self.pixels_lru.pop_front() {
            if self.pixels.len() > self.max_pixels && !keep.contains(&id) {
                self.pixels.remove(&id);
            } else {
                kept.push_back(id);
            }
        }
        self.pixels_lru = kept;
    }
}
