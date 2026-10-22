//! The sky map — the DSS2 survey drawn under everything, streamed tile by
//! tile from the server's HiPS mirror (`crate::skymap`).
//!
//! `prepare` picks the HEALPix order whose tile pixel matches a screen pixel,
//! lists the cells under the canvas (`healpix::cone_cells`), nearest first,
//! plus their grandparents so something coarse shows at once, and asks the
//! `TileImageCache` for the images (loads start here). Decoded ones become
//! `TileRequest`s for the GPU `SkymapLayer`, whose fragment shader does the
//! per-pixel lookup; in Canvas2D mode each decoded tile is drawn as an
//! affine quad over its cell instead. The layer fades in as the field
//! narrows, over the Milky Way panorama that covers the wide fields.
//!
//! Registered right after the panorama, under the ground and grids.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{project_far_with, Frame, GpuPrepare, SkyLayer};
use crate::astro;
use crate::components::sky::gpu::layers::skymap::{SkymapPrepare, TileRequest, MAX_TABLE_ORDER};
use crate::coords::J2000;
use crate::healpix;
use crate::skymap::{tile_id, tile_url};

/// Crossfade with the panorama, by view radius: fully off at and above
/// `FADE_OFF_DEG`, fully on at and below `FADE_ON_DEG`.
pub const FADE_OFF_DEG: f64 = 25.0;
pub const FADE_ON_DEG: f64 = 10.0;
/// Cells of the view's order requested per frame — those meeting the
/// canvas rectangle, nearest the centre first. A 16:9 canvas at the chosen
/// order spans 4–7 × 2–4 tiles, more with partial cells along the edges.
const MAX_CELLS: usize = 48;
const MAX_CELLS_MOBILE: usize = 32;
/// Parent cells (`SkyMapInfo::parent_order`) requested first.
const MAX_PARENTS: usize = 8;
const MAX_PARENTS_MOBILE: usize = 6;
/// How far past the half-step the ideal order must move before switching.
const ORDER_HYSTERESIS: f64 = 0.15;
/// Phones take tiles half an order coarser than their (tiny) pixels.
const MOBILE_ORDER_BIAS: f64 = -0.5;
/// Canvas2D fallback: how many decoded tiles to draw per frame.
const FALLBACK_MAX_TILES: usize = 40;
/// Canvas2D fallback: a tile is drawn as affine quads no wider than this,
/// so a coarse parent (orders 0–2) is split into 4ⁿ sub-quads, each from
/// its own vertices — the azimuthal projection bends a 58° cell too much
/// for one quad.
const FALLBACK_QUAD_MAX_DEG: f64 = 8.0;
/// Cell vertices farther than this from the view centre don't project: a
/// cell that far is behind the viewer.
const FAR_CLIP_DEG: f64 = 90.0;
/// Objects whose apparent semi-axis is at least this (CSS px) lose their
/// outline while the image shows them.
const IMAGE_MIN_HALF_PX: f64 = 10.0;

/// The panorama → sky map crossfade for a view radius (degrees).
pub fn crossfade(fov_radius_deg: f64) -> f64 {
    ((FADE_OFF_DEG - fov_radius_deg) / (FADE_OFF_DEG - FADE_ON_DEG)).clamp(0.0, 1.0)
}

/// The order whose tile texel equals one physical screen pixel, fractional:
/// log2(order-0 side · px per degree / tile px).
pub fn ideal_order(fov_radius_deg: f64, scale_css_px: f64, dpr: f64, tile_px: u32) -> f64 {
    let px_per_deg = scale_css_px * dpr / fov_radius_deg.max(1e-6);
    (healpix::ORDER0_SIDE_DEG * px_per_deg / tile_px.max(1) as f64).log2()
}

/// Round the ideal order, holding the previous one until the ideal has
/// moved past the half-step by `ORDER_HYSTERESIS`.
pub fn choose_order(ideal: f64, prev: Option<u8>, max_order: u8) -> u8 {
    if let Some(p) = prev {
        if (ideal - p as f64).abs() < 0.5 + ORDER_HYSTERESIS {
            return p.min(max_order);
        }
    }
    ideal.round().clamp(0.0, max_order as f64) as u8
}

struct CellsCache {
    order: u8,
    parent_order: Option<u8>,
    centre: [f64; 3],
    radius_rad: f64,
    cells: Vec<u32>,
    parents: Vec<u32>,
}

/// Whether a cell's projected footprint meets the canvas. The cone that
/// lists the cells covers the canvas *diagonal*, twice the rectangle's area
/// on a 16:9 screen; this keeps the cap for cells actually on screen —
/// partially visible ones included, since the test is the bounding box of
/// the projected vertices. `to_screen` must project past the corners (see
/// `project_far_with`); a vertex it still refuses (behind the viewer) keeps
/// the cell, which can then only be a huge one straddling the view.
fn cell_on_screen(order: u8, hash: u32, to_screen: &dyn Fn(f64, f64) -> Option<(f64, f64)>, wf: f64, hf: f64) -> bool {
    let (clon, clat) = healpix::cell_center(order, hash);
    let mut pts = healpix::cell_vertices(order, hash).to_vec();
    pts.push((clon, clat));
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (lon, lat) in pts {
        let Some((x, y)) = to_screen(lon, lat) else { return true };
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    const M: f64 = 2.0;
    x1 >= -M && x0 <= wf + M && y1 >= -M && y0 <= hf + M
}

#[derive(Default)]
pub struct SkymapLayer {
    /// This frame's tiles (decoded), for the Canvas2D draw phase.
    tiles: Vec<TileRequest>,
    last_order: Option<u8>,
    cells: Option<CellsCache>,
}

impl SkymapLayer {
    #[allow(clippy::too_many_arguments)]
    fn cells_for(
        &mut self,
        order: u8,
        parent_order: Option<u8>,
        lon: f64,
        lat: f64,
        radius_rad: f64,
        max_cells: usize,
        max_parents: usize,
        on_screen: &dyn Fn(u8, u32) -> bool,
    ) -> (&[u32], &[u32]) {
        let centre = healpix::lonlat_to_xyz(lon, lat);
        let cell_side_rad = (healpix::ORDER0_SIDE_DEG / (1u32 << order) as f64).to_radians();
        let reuse = self.cells.as_ref().is_some_and(|c| {
            let moved = (c.centre[0] * centre[0] + c.centre[1] * centre[1] + c.centre[2] * centre[2])
                .clamp(-1.0, 1.0)
                .acos();
            c.order == order
                && c.parent_order == parent_order
                && moved < 0.1 * cell_side_rad
                && (c.radius_rad - radius_rad).abs() < 0.05 * c.radius_rad
        });
        if !reuse {
            let mut cells = healpix::cone_cells(order, lon, lat, radius_rad);
            cells.retain(|&h| on_screen(order, h));
            cells.truncate(max_cells);
            let mut parents = match parent_order {
                Some(po) if po < order => healpix::cone_cells(po, lon, lat, radius_rad),
                _ => Vec::new(),
            };
            if let Some(po) = parent_order {
                parents.retain(|&h| on_screen(po, h));
            }
            parents.truncate(max_parents);
            self.cells = Some(CellsCache { order, parent_order, centre, radius_rad, cells, parents });
        }
        let c = self.cells.as_ref().unwrap();
        (&c.cells, &c.parents)
    }
}

impl SkyLayer for SkymapLayer {
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.skymap_on && f.skymap.is_some()
    }

    fn prepare(&mut self, f: &mut Frame, gpu: Option<&mut GpuPrepare>) {
        self.tiles.clear();
        f.image_min_half_px = None;
        let Some(info) = f.skymap else { return };
        let Some(cache_cell) = f.tile_images else { return };
        let Ok(mut cache) = cache_cell.try_borrow_mut() else { return };
        let view = *f.view;
        let scene = *f.scene;
        // A canvas not laid out yet has no scale: no order, no cone, no tiles.
        if !(view.wf > 0.0 && view.hf > 0.0 && view.scale > 0.0 && view.fov > 0.0) {
            return;
        }

        let opacity = crossfade(view.fov);
        if opacity <= 0.0 {
            return;
        }
        let Some(max_order) = info.usable_order(f.toggles.lite, MAX_TABLE_ORDER) else { return };

        // ── Order: tile texel ≈ screen pixel ────────────────────────────
        let bias = if scene.is_mobile { MOBILE_ORDER_BIAS } else { 0.0 };
        let ideal = ideal_order(view.fov, view.scale, view.dpr, info.tile_width) + bias;
        let order = choose_order(ideal, self.last_order, max_order);
        self.last_order = Some(order);
        let lod = (order as f64 - ideal) as f32;

        // ── Cells under the canvas ──────────────────────────────────────
        // The pass paints every pixel, corners included, so the cone reaches
        // the canvas diagonal, plus half a cell so partial cells are in.
        let (c_ra_jnow, c_dec_jnow) =
            astro::altaz_to_eq(view.c_alt, view.c_az, scene.lst, scene.latitude);
        let centre = crate::coords::JNow::new(c_ra_jnow, c_dec_jnow).to_j2000(scene.jd);
        let half_diag = view.fov * view.wf.hypot(view.hf) / view.wf.min(view.hf) * 1.05;
        let half_cell = healpix::ORDER0_SIDE_DEG * std::f64::consts::SQRT_2 / (2u32 << order) as f64;
        let radius_rad = (half_diag + half_cell).min(180.0).to_radians();
        let (max_cells, max_parents) = if scene.is_mobile {
            (MAX_CELLS_MOBILE, MAX_PARENTS_MOBILE)
        } else {
            (MAX_CELLS, MAX_PARENTS)
        };
        let parent_order = info.parent_order(order).filter(|&po| po < order);
        // A J2000 point on screen: → JNow → alt/az → px, anywhere in the
        // front hemisphere (the corners sit past the usual 1.5 × fov clip).
        let to_screen = |lon: f64, lat: f64| -> Option<(f64, f64)> {
            let now = J2000::new(lon.to_degrees(), lat.to_degrees()).to_jnow(scene.jd);
            let (alt, az) = astro::eq_to_altaz(now.ra_deg, now.dec_deg, scene.lst, scene.latitude);
            project_far_with(view, alt, az, FAR_CLIP_DEG)
        };
        let on_screen = |o: u8, h: u32| cell_on_screen(o, h, &to_screen, view.wf, view.hf);
        let (cells, parents) = self.cells_for(
            order,
            parent_order,
            centre.ra_deg.to_radians(),
            centre.dec_deg.to_radians(),
            radius_rad,
            max_cells,
            max_parents,
            &on_screen,
        );
        let wanted: Vec<(u8, u32)> = parents
            .iter()
            .map(|&h| (parent_order.unwrap_or(order), h))
            .chain(cells.iter().map(|&h| (order, h)))
            .collect();

        // ── Images ──────────────────────────────────────────────────────
        cache.begin_frame();
        for (o, npix) in wanted {
            let id = tile_id(o, npix);
            let Some(img) = cache.image(id, &tile_url(o, npix)) else { continue };
            self.tiles.push(TileRequest { id, order: o, npix, img });
        }

        if opacity >= 0.5 && !self.tiles.is_empty() {
            f.image_min_half_px = Some(IMAGE_MIN_HALF_PX);
        }
        if let Some(gpu) = gpu {
            gpu.skymap = SkymapPrepare {
                enabled: true,
                tiles: self.tiles.clone(),
                order,
                // All the way down: above the complete base the nearest
                // resident tile may be several orders up.
                order_min: 0,
                tile_px: info.tile_width,
                opacity: opacity as f32,
                brightness: f.state.skymap_brightness as f32,
                lod,
            };
        }
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        if f.mode.is_gpu() || self.tiles.is_empty() {
            return;
        }
        let view = *f.view;
        let scene = *f.scene;
        let opacity = crossfade(view.fov) * f.state.skymap_brightness.clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }
        // A cell vertex on screen: J2000 → JNow → alt/az → px — past the
        // canvas corners too, so a tile half out of the view still draws.
        let to_screen = |lon: f64, lat: f64| -> Option<(f64, f64)> {
            let now = J2000::new(lon.to_degrees(), lat.to_degrees()).to_jnow(scene.jd);
            let (alt, az) = astro::eq_to_altaz(now.ra_deg, now.dec_deg, scene.lst, scene.latitude);
            project_far_with(view, alt, az, FAR_CLIP_DEG)
        };
        ctx.save();
        ctx.set_global_alpha(opacity);
        // Coarse tiles first so the view's own cells paint over them. The
        // tiles carry their own sky, hence source-over, not additive.
        let mut tiles: Vec<&TileRequest> = self.tiles.iter().collect();
        tiles.sort_by_key(|t| t.order);
        for t in tiles.into_iter().take(FALLBACK_MAX_TILES) {
            let (iw, ih) = (t.img.natural_width() as f64, t.img.natural_height() as f64);
            if iw <= 0.0 || ih <= 0.0 {
                continue;
            }
            let side = healpix::ORDER0_SIDE_DEG / (1u32 << t.order) as f64;
            let depth = (side / FALLBACK_QUAD_MAX_DEG).log2().ceil().clamp(0.0, 3.0) as u8;
            draw_cell_quads(ctx, &t.img, t.order, t.npix, (0.0, 0.0, iw, ih), depth, &to_screen, view.wf, view.hf);
        }
        ctx.restore();
    }
}

/// Draw the part `src` (x, y, w, h in image pixels) of `img` over the cell
/// (`order`, `hash`), as one affine quad when `depth` is 0, else as the four
/// children's quads from the matching image quadrants. Image columns run
/// S→W (dy), rows S→E (dx); a child's bit 0 is the east half, bit 1 the west.
#[allow(clippy::too_many_arguments)]
fn draw_cell_quads(
    ctx: &CanvasRenderingContext2d,
    img: &web_sys::HtmlImageElement,
    order: u8,
    hash: u32,
    src: (f64, f64, f64, f64),
    depth: u8,
    to_screen: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    wf: f64,
    hf: f64,
) {
    let verts = healpix::cell_vertices(order, hash);
    let mut px = [(0.0, 0.0); 4];
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (i, (lon, lat)) in verts.iter().enumerate() {
        // A vertex behind the viewer: nothing sensible to draw.
        let Some(p) = to_screen(*lon, *lat) else { return };
        px[i] = p;
        x0 = x0.min(p.0);
        y0 = y0.min(p.1);
        x1 = x1.max(p.0);
        y1 = y1.max(p.1);
    }
    if x1 < 0.0 || x0 > wf || y1 < 0.0 || y0 > hf {
        return;
    }
    if depth > 0 {
        let (sx, sy, sw, sh) = src;
        let (hw, hh) = (sw / 2.0, sh / 2.0);
        for q in 0..4u32 {
            let qx = sx + if q & 2 != 0 { hw } else { 0.0 };
            let qy = sy + if q & 1 != 0 { hh } else { 0.0 };
            draw_cell_quads(ctx, img, order + 1, (hash << 2) | q, (qx, qy, hw, hh), depth - 1, to_screen, wf, hf);
        }
        return;
    }
    let [ps, pe, _n, pw] = px;
    let (sx, sy, sw, sh) = src;
    let (a, b) = ((pw.0 - ps.0) / sw, (pw.1 - ps.1) / sw);
    let (c, d) = ((pe.0 - ps.0) / sh, (pe.1 - ps.1) / sh);
    if (a * d - b * c).abs() < 1e-12 {
        return;
    }
    ctx.save();
    let _ = ctx.transform(a, b, c, d, ps.0, ps.1);
    let _ = ctx.draw_image_with_html_image_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
        img, sx, sy, sw, sh, 0.0, 0.0, sw, sh,
    );
    ctx.restore();
}
