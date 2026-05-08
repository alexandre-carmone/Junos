//! Canvas2D rendering logic for the sky map overlay.
//!
//! Note: this file is the legacy free-fn surface. New code lives under
//! `params.rs` (and forthcoming `layer.rs` / `pipeline.rs`). During the
//! pipeline refactor both coexist: layers consume the grouped param
//! structs, while the legacy `render_overlay` keeps consuming
//! `RenderParams`. Once every layer is migrated, this file shrinks to the
//! shared types (`HitItem`, `MosaicPlanRender`, `SchedulerJobRender`).

pub mod layer;
pub mod layers;
pub mod params;
pub mod pipeline;

#[allow(unused_imports)]
pub use layer::{Catalogs, Frame, GpuPrepare, SkyLayer};
#[allow(unused_imports)]
pub use params::{LayerToggles, OverlayState, PipelineMode, SceneParams, ViewParams};
#[allow(unused_imports)]
pub use pipeline::RenderPipeline;

use std::f64::consts::PI;
use std::sync::Arc;

use web_sys::{CanvasRenderingContext2d, HtmlImageElement};

use crate::astro;
use crate::catalog::CatalogData;
use crate::coords::{JNow, J2000};
use crate::dso_catalog::{DsoCatalogData, DsoType};
use crate::ephemeris;
use crate::i18n::{constellation_name, t};
use crate::nebulae::NebulaeIndex;

use super::dso_index::DsoIndex;

use super::utils::bv_to_rgb;

// ── Star size tuning ──────────────────────────────────────────────────────────
// Radius (CSS pixels) = STAR_SIZE_BASE - mag * STAR_SIZE_MAG_SCALE
// Increase STAR_SIZE_BASE to make all stars larger.
const STAR_SIZE_BASE: f64 = 3.5;
const STAR_SIZE_MAG_SCALE: f64 = 0.35;
const STAR_SIZE_MIN: f64 = 0.5;

// ---------------------------------------------------------------------------
// Hit-test items (collected during render, consumed by mouseup handler)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum HitKind {
    Star,
    Dso(DsoType),
    Sun,
    Moon,
    Planet,
}

#[derive(Clone)]
pub struct HitItem {
    pub sx: f64,
    pub sy: f64,
    pub radius: f64, // hit radius in CSS px
    pub kind: HitKind,
    pub name: String,
    pub mag: Option<f32>,
    pub ra_jnow_deg: f64,
    pub dec_jnow_deg: f64,
    /// Optional extra info (apparent diameter for Sun/Moon, size for DSO, …).
    pub size_arcmin: Option<f64>,
    pub phase: Option<f64>,
}

/// Lightweight per-job data extracted from SchedulerSnapshot for rendering.
#[derive(Clone)]
pub struct SchedulerJobRender {
    pub name: String,
    pub ra_h: f64,
    pub dec_deg: f64,
    pub state: i64,
}

/// A single mosaic tile to render on the sky.
#[derive(Clone)]
pub struct MosaicTileRender {
    pub ra_deg: f64,
    pub dec_deg: f64,
    /// PA of this individual tile (usually 0; additional rotation added via plan.pa_deg).
    pub rotation: f64,
}

/// A complete mosaic plan (either from KStars new_mosaic_tiles or from the in-app planner).
#[derive(Clone)]
pub struct MosaicPlanRender {
    pub target_name: String,
    pub tiles: Vec<MosaicTileRender>,
    pub fov_w_deg: f64,
    pub fov_h_deg: f64,
    pub overlap_pct: f64,
    pub pa_deg: f64,
}

// ---------------------------------------------------------------------------
// Fallback CPU rendering (stars + constellation lines + names)
// ---------------------------------------------------------------------------

pub(super) fn render_fallback_stars(
    ctx: &CanvasRenderingContext2d,
    f: &mut Frame,
    cat: &Option<Arc<CatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    _cx: f64,
    _cy: f64,
    _scale: f64,
) {
    let Some(cat) = cat else { return };

    let mut idx_screen: Vec<Option<(f64, f64, f32, f32)>> = vec![None; cat.stars.len()];
    if f.toggles.stars_on || f.toggles.const_on {
        let lst_rad = f.scene.lst.to_radians();
        for (i, star) in cat.stars.iter().enumerate() {
            if f.toggles.stars_on && star.mag > f.scene.mag_limit {
                continue;
            }
            let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(f.scene.jd);
            let ha = lst_rad - jnow.ra_deg.to_radians();
            let dec = jnow.dec_deg.to_radians();
            let sin_dec = dec.sin();
            let cos_dec = dec.cos();
            let sin_alt = sin_dec * f.scene.sin_lat + cos_dec * f.scene.cos_lat * ha.cos();
            let alt_rad = sin_alt.asin();
            let alt = alt_rad.to_degrees();
            if alt < -5.0 {
                continue;
            }
            let cos_az =
                (sin_dec - alt_rad.sin() * f.scene.sin_lat) / (alt_rad.cos() * f.scene.cos_lat);
            let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
            if ha.sin() > 0.0 {
                az = 360.0 - az;
            }
            if let Some((sx, sy)) = project(alt, az) {
                if sx > -50.0 && sx < f.view.wf + 50.0 && sy > -50.0 && sy < f.view.hf + 50.0 {
                    idx_screen[i] = Some((sx, sy, star.mag, star.bv));
                }
            }
        }
    }

    // Constellation lines
    if f.toggles.const_on {
        ctx.set_stroke_style_str("#334");
        ctx.set_line_width(0.6);
        ctx.begin_path();
        for &(a, b) in cat.lines.iter() {
            if let (Some(Some((x1, y1, _, _))), Some(Some((x2, y2, _, _)))) =
                (idx_screen.get(a as usize), idx_screen.get(b as usize))
            {
                ctx.move_to(*x1, *y1);
                ctx.line_to(*x2, *y2);
            }
        }
        ctx.stroke();
    }

    // Stars — simple filled circles (no per-star gradients for performance)
    if f.toggles.stars_on {
        // Scale star size with screen: reference = 1000 CSS pixels (min dimension)
        let screen_scale = (f.view.wf.min(f.view.hf) / 1000.0).clamp(0.4, 1.5);
        for screen in &idx_screen {
            if let Some((sx, sy, mag, bv)) = screen {
                let radius = ((STAR_SIZE_BASE - *mag as f64 * STAR_SIZE_MAG_SCALE) * screen_scale)
                    .max(STAR_SIZE_MIN);
                let t = ((*mag + 2.0) / 8.5).clamp(0.0, 1.0);
                let brightness = 1.0 - t * 0.69;
                let (r, g, b) = bv_to_rgb(*bv);
                let ri = (r * brightness * 255.0) as u8;
                let gi = (g * brightness * 255.0) as u8;
                let bi = (b * brightness * 255.0) as u8;
                // Whitened core color
                let cr = ((ri as f64 * 0.55 + 255.0 * 0.45) as u8).min(255);
                let cg = ((gi as f64 * 0.55 + 255.0 * 0.45) as u8).min(255);
                let cb = ((bi as f64 * 0.55 + 255.0 * 0.45) as u8).min(255);
                ctx.set_fill_style_str(&format!("rgb({cr},{cg},{cb})"));
                ctx.begin_path();
                let _ = ctx.arc(*sx, *sy, radius, 0.0, 2.0 * PI);
                ctx.fill();
            }
        }
    }

    // Star names + hit items for named stars.
    if f.toggles.stars_on {
        for (i, star) in cat.stars.iter().enumerate() {
            let Some(Some((sx, sy, mag, _))) = idx_screen.get(i) else {
                continue;
            };
            let Some(name) = star.name.as_deref() else {
                continue;
            };
            if name == "Sol" {
                continue;
            } // duplicate of ephemeris Sun, wrong position
            let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(f.scene.jd);
            f.hit_items.push(HitItem {
                sx: *sx,
                sy: *sy,
                radius: ((STAR_SIZE_BASE - *mag as f64 * STAR_SIZE_MAG_SCALE).max(4.0)),
                kind: HitKind::Star,
                name: name.to_string(),
                mag: Some(*mag),
                ra_jnow_deg: jnow.ra_deg,
                dec_jnow_deg: jnow.dec_deg,
                size_arcmin: None,
                phase: None,
            });
            if f.toggles.names_on && *mag < 3.0 {
                ctx.set_fill_style_str("#8899aa");
                ctx.set_font("13px monospace");
                ctx.set_text_align("left");
                let _ = ctx.fill_text(name, sx + 6.0, sy - 4.0);
            }
        }
    }

    // Constellation names
    if f.toggles.const_on && f.toggles.con_names_on {
        ctx.set_fill_style_str("rgba(100,120,180,0.7)");
        ctx.set_font("italic 14px monospace");
        ctx.set_text_align("center");
        let lst_rad = f.scene.lst.to_radians();
        for (abbr, name, cra, cdec) in &cat.centers {
            let cjnow = J2000::new(*cra as f64, *cdec as f64).to_jnow(f.scene.jd);
            let ha = lst_rad - cjnow.ra_deg.to_radians();
            let dec_rad = cjnow.dec_deg.to_radians();
            let sin_dec = dec_rad.sin();
            let cos_dec = dec_rad.cos();
            let sin_alt = sin_dec * f.scene.sin_lat + cos_dec * f.scene.cos_lat * ha.cos();
            let alt_rad = sin_alt.asin();
            let alt = alt_rad.to_degrees();
            if alt < -5.0 {
                continue;
            }
            let cos_az =
                (sin_dec - alt_rad.sin() * f.scene.sin_lat) / (alt_rad.cos() * f.scene.cos_lat);
            let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
            if ha.sin() > 0.0 {
                az = 360.0 - az;
            }
            if let Some((sx, sy)) = project(alt, az) {
                let label = constellation_name(abbr, f.scene.cur_lang).unwrap_or(name);
                let _ = ctx.fill_text(label, sx, sy);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Star names (GPU path — project only named bright stars on CPU)
// ---------------------------------------------------------------------------

pub(super) fn render_star_names_gpu(
    ctx: &CanvasRenderingContext2d,
    f: &mut Frame,
    cat: &Option<Arc<CatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    let Some(cat) = cat else { return };
    ctx.set_fill_style_str("#8899aa");
    ctx.set_font(if f.scene.is_mobile {
        "12px monospace"
    } else {
        "13px monospace"
    });
    ctx.set_text_align("left");
    let lst_rad = f.scene.lst.to_radians();
    // Tighter cull on mobile: only the brightest named stars get labels
    // (cuts fillText calls roughly in half on a typical view).
    let star_label_mag_max: f32 = if f.scene.is_mobile { 2.2 } else { 3.0 };
    for star in cat.stars.iter() {
        let Some(name) = star.name.as_deref() else {
            continue;
        };
        if name == "Sol" {
            continue;
        }
        if star.mag >= star_label_mag_max {
            continue;
        }
        let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(f.scene.jd);
        let ha = lst_rad - jnow.ra_deg.to_radians();
        let dec = jnow.dec_deg.to_radians();
        let sin_dec = dec.sin();
        let cos_dec = dec.cos();
        let sin_alt = sin_dec * f.scene.sin_lat + cos_dec * f.scene.cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -5.0 {
            continue;
        }
        let cos_az =
            (sin_dec - alt_rad.sin() * f.scene.sin_lat) / (alt_rad.cos() * f.scene.cos_lat);
        let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 {
            az = 360.0 - az;
        }
        if let Some((sx, sy)) = project(alt, az) {
            let _ = ctx.fill_text(name, sx + 6.0, sy - 4.0);
            if !f.mode.is_gpu() {
                f.hit_items.push(HitItem {
                    sx,
                    sy,
                    radius: 8.0,
                    kind: HitKind::Star,
                    name: name.to_string(),
                    mag: Some(star.mag),
                    ra_jnow_deg: jnow.ra_deg,
                    dec_jnow_deg: jnow.dec_deg,
                    size_arcmin: None,
                    phase: None,
                });
            }
        }
    }
}

/// Push hit items for named stars *without* drawing labels (GPU path when
/// the user turned labels off but still expects stars to be clickable).
pub(super) fn push_star_hit_items(
    f: &mut Frame,
    cat: &Option<Arc<CatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    let Some(cat) = cat else { return };
    let lst_rad = f.scene.lst.to_radians();
    for star in cat.stars.iter() {
        let Some(name) = star.name.as_deref() else {
            continue;
        };
        if name == "Sol" {
            continue;
        }
        if star.mag >= 3.0 {
            continue;
        }
        let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(f.scene.jd);
        let ha = lst_rad - jnow.ra_deg.to_radians();
        let dec = jnow.dec_deg.to_radians();
        let sin_dec = dec.sin();
        let cos_dec = dec.cos();
        let sin_alt = sin_dec * f.scene.sin_lat + cos_dec * f.scene.cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -5.0 {
            continue;
        }
        let cos_az =
            (sin_dec - alt_rad.sin() * f.scene.sin_lat) / (alt_rad.cos() * f.scene.cos_lat);
        let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 {
            az = 360.0 - az;
        }
        if let Some((sx, sy)) = project(alt, az) {
            f.hit_items.push(HitItem {
                sx,
                sy,
                radius: 8.0,
                kind: HitKind::Star,
                name: name.to_string(),
                mag: Some(star.mag),
                ra_jnow_deg: jnow.ra_deg,
                dec_jnow_deg: jnow.dec_deg,
                size_arcmin: None,
                phase: None,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Constellation names (GPU path)
// ---------------------------------------------------------------------------

pub(super) fn render_constellation_names_gpu(
    ctx: &CanvasRenderingContext2d,
    f: &mut Frame,
    cat: &Option<Arc<CatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    let Some(cat) = cat else { return };
    ctx.set_fill_style_str("rgba(100,120,180,0.7)");
    ctx.set_font("italic 14px monospace");
    ctx.set_text_align("center");
    let lst_rad = f.scene.lst.to_radians();
    for (abbr, name, cra, cdec) in &cat.centers {
        let cjnow = J2000::new(*cra as f64, *cdec as f64).to_jnow(f.scene.jd);
        let ha = lst_rad - cjnow.ra_deg.to_radians();
        let dec_rad = cjnow.dec_deg.to_radians();
        let sin_dec = dec_rad.sin();
        let cos_dec = dec_rad.cos();
        let sin_alt = sin_dec * f.scene.sin_lat + cos_dec * f.scene.cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -5.0 {
            continue;
        }
        let cos_az =
            (sin_dec - alt_rad.sin() * f.scene.sin_lat) / (alt_rad.cos() * f.scene.cos_lat);
        let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 {
            az = 360.0 - az;
        }
        if let Some((sx, sy)) = project(alt, az) {
            let label = constellation_name(abbr, f.scene.cur_lang).unwrap_or(name);
            let _ = ctx.fill_text(label, sx, sy);
        }
    }
}

// ---------------------------------------------------------------------------
// Deep-sky objects
// ---------------------------------------------------------------------------

pub(super) fn render_dso(
    ctx: &CanvasRenderingContext2d,
    f: &mut Frame,
    dso_cat: &Option<Arc<DsoCatalogData>>,
    dso_index: Option<&DsoIndex>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    scale: f64,
    nebulae_index: Option<&NebulaeIndex>,
) {
    let Some(dso_cat) = dso_cat else { return };
    let lst_rad = f.scene.lst.to_radians();
    let cx = f.view.wf / 2.0;
    let cy = f.view.hf / 2.0;
    ctx.set_line_width(1.0);
    ctx.set_font("12px monospace");

    // ── Cheap angular-distance pre-cull (J2000) ───────────────────────────
    // Compute the view center in J2000 once, then skip the per-DSO
    // precession + altaz + projection chain for objects clearly outside the
    // visible cap. Iteration set is unchanged from the original full scan,
    // so no DSO can ever be missed by this filter — only the heavy math is
    // skipped for far-away objects.
    let (c_ra_jnow, c_dec_jnow) =
        astro::altaz_to_eq(f.view.c_alt, f.view.c_az, f.scene.lst, f.scene.latitude);
    let view_j2000 = JNow::new(c_ra_jnow, c_dec_jnow).to_j2000(f.scene.jd);
    let v_ra_rad = view_j2000.ra_deg.to_radians();
    let v_dec_rad = view_j2000.dec_deg.to_radians();
    let v_sin_dec = v_dec_rad.sin();
    let v_cos_dec = v_dec_rad.cos();
    // Cap radius generously: render rejects beyond fov*1.5; add margin for
    // wide-field nebulae whose center is just outside but corners are in.
    let cap_radius_deg = f.view.fov * 1.5 + 6.0;
    let cos_cap = if cap_radius_deg >= 180.0 {
        -1.0
    } else {
        cap_radius_deg.to_radians().cos()
    };

    // Spatial pre-cull: when an index is available, only walk catalog
    // entries inside buckets that overlap the cap. At narrow FOV this drops
    // the inner loop from ~2 500 to a few dozen — the dominant CPU saving on
    // mobile. With no index (e.g. mid-load) we fall back to the full scan,
    // which is what the great-circle gate already covered.
    let visible: Option<Vec<u32>> = dso_index
        .map(|idx| idx.visible_indices(view_j2000.ra_deg, view_j2000.dec_deg, cap_radius_deg));
    let dsos = &dso_cat.dsos;
    let iter_indices: Box<dyn Iterator<Item = usize>> = match &visible {
        Some(v) => Box::new(v.iter().map(|i| *i as usize)),
        None => Box::new(0..dsos.len()),
    };

    for di in iter_indices {
        let Some(dso) = dsos.get(di) else { continue };
        let type_ok = match dso.kind {
            DsoType::Galaxy => f.state.dso_gx,
            DsoType::OpenCluster => f.state.dso_oc,
            DsoType::GlobularCluster => f.state.dso_gc,
            DsoType::Nebula => f.state.dso_nb,
            DsoType::PlanetaryNebula => f.state.dso_pn,
            DsoType::SupernovaRemnant => f.state.dso_snr,
            DsoType::GalaxyCluster => f.state.dso_gal,
        };
        if !type_ok {
            continue;
        }
        if (dso.mag as f64) > f.state.dso_mag {
            continue;
        }

        // Cheap great-circle gate in J2000 — skips precession/altaz when
        // the object is well outside the visible cap.
        let d_ra_rad = (dso.ra_deg as f64).to_radians();
        let d_dec_rad = (dso.dec_deg as f64).to_radians();
        let cos_sep =
            v_sin_dec * d_dec_rad.sin() + v_cos_dec * d_dec_rad.cos() * (d_ra_rad - v_ra_rad).cos();
        if cos_sep < cos_cap {
            continue;
        }

        let dso_jnow = J2000::new(dso.ra_deg as f64, dso.dec_deg as f64).to_jnow(f.scene.jd);
        let ha = lst_rad - dso_jnow.ra_deg.to_radians();
        let dec_rad = dso_jnow.dec_deg.to_radians();
        let sin_dec = dec_rad.sin();
        let cos_dec = dec_rad.cos();
        let sin_alt = sin_dec * f.scene.sin_lat + cos_dec * f.scene.cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -3.0 {
            continue;
        }
        let cos_az_val =
            (sin_dec - alt_rad.sin() * f.scene.sin_lat) / (alt_rad.cos() * f.scene.cos_lat);
        let mut az = cos_az_val.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 {
            az = 360.0 - az;
        }
        let Some((sx, sy)) = project(alt, az) else {
            continue;
        };
        if sx < -40.0 || sx > f.view.wf + 40.0 || sy < -40.0 || sy > f.view.hf + 40.0 {
            continue;
        }

        let px_size =
            (dso.size_arcmin as f64 / 60.0 / (f.view.fov * 2.0) * scale * 2.0).clamp(4.0, 40.0);
        let r = px_size / 2.0;

        // ── Try real image from Stellarium nebulae set ──────────────────────
        let drew_image = if let Some(tex) = nebulae_index.and_then(|idx| idx.map.get(&dso.name)) {
            // Lazy-load: insert HtmlImageElement on first encounter
            if !f.nebulae_cache.contains_key(&tex.path) {
                if let Ok(img) = HtmlImageElement::new() {
                    img.set_src(&format!("/{}", tex.path));
                    f.nebulae_cache.insert(tex.path.clone(), img);
                }
            }
            // Draw only when the image has finished loading
            if let Some(img) = f.nebulae_cache.get(&tex.path) {
                if img.complete() && img.natural_width() > 0 {
                    // Project all 4 corners with no distance cutoff so that
                    // large objects don't disappear when some corner is off-FOV.
                    // Canvas2D clips anything outside the canvas bounds.
                    let mut pts = [(0.0_f64, 0.0_f64); 4];
                    let mut ok = true;
                    for (i, &(cra, cdec)) in tex.corners.iter().enumerate() {
                        let corner_jnow = J2000::new(cra as f64, cdec as f64).to_jnow(f.scene.jd);
                        let (calt, caz) = astro::eq_to_altaz(
                            corner_jnow.ra_deg,
                            corner_jnow.dec_deg,
                            f.scene.lst,
                            f.scene.latitude,
                        );
                        let (nx, ny) = astro::project_unclamped(
                            calt,
                            caz,
                            f.view.c_alt,
                            f.view.c_az,
                            f.view.fov,
                        );
                        let px = cx + nx * scale;
                        let py = cy - ny * scale;
                        // Reject absurdly far-off points (e.g. back-hemisphere)
                        if !px.is_finite()
                            || !py.is_finite()
                            || px.abs() > f.view.wf * 20.0
                            || py.abs() > f.view.hf * 20.0
                        {
                            ok = false;
                            break;
                        }
                        pts[i] = (px, py);
                    }
                    if ok {
                        // Affine: maps texture [0,1]² to the sky quad.
                        // Stellarium worldCoords order (OpenGL UV): BL(0,0), BR(1,0), TR(1,1), TL(0,1)
                        // Canvas2D drawImage UV (0,0) = image top-left, (0,1) = image bottom-left.
                        // OpenGL UV(0,0) = image bottom-left → Canvas2D (0,1).
                        // So: pts[3] (TL/OpenGL) = image top-left → Canvas2D (0,0)
                        //     pts[2] (TR/OpenGL) = image top-right → Canvas2D (1,0)
                        //     pts[0] (BL/OpenGL) = image bottom-left → Canvas2D (0,1)
                        let p0 = pts[3]; // TL → canvas (0,0) = image top-left
                        let p1 = pts[2]; // TR → canvas (1,0) = image top-right
                        let p3 = pts[0]; // BL → canvas (0,1) = image bottom-left
                        let a = p1.0 - p0.0;
                        let b = p1.1 - p0.1;
                        let c = p3.0 - p0.0;
                        let d = p3.1 - p0.1;
                        let e = p0.0;
                        let f = p0.1;
                        ctx.save();
                        // transform() multiplies with the current matrix (DPR
                        // scale stays intact), so coordinates are in CSS pixels.
                        let _ = ctx.transform(a, b, c, d, e, f);
                        ctx.set_global_alpha(0.85);
                        let _ = ctx.draw_image_with_html_image_element_and_dw_and_dh(
                            img, 0.0, 0.0, 1.0, 1.0,
                        );
                        ctx.set_global_alpha(1.0);
                        ctx.restore();
                        true
                    } else {
                        false
                    }
                } else {
                    false // image still loading
                }
            } else {
                false
            }
        } else {
            false // no nebula image for this DSO
        };

        // ── Symbol fallback (when no image available / loaded) ──────────────
        // When `dso_on_gpu` is set, the GPU DsoLayer renders the symbol —
        // skip the Canvas2D outline entirely (nebula thumbnails still flow
        // through the GPU-skipped path above when an image is available).
        if !drew_image && !f.mode.is_gpu() {
            match dso.kind {
                DsoType::Galaxy => {
                    let minor_px = if dso.size_minor_arcmin > 0.0 {
                        (dso.size_minor_arcmin as f64 / 60.0 / (f.view.fov * 2.0) * scale * 2.0)
                            .clamp(2.0, r)
                    } else {
                        r * 0.45
                    };
                    let angle = (dso.pa_deg as f64).to_radians();
                    ctx.save();
                    ctx.translate(sx, sy).unwrap();
                    ctx.rotate(angle).unwrap();
                    ctx.set_stroke_style_str("rgba(0,200,220,0.75)");
                    ctx.begin_path();
                    ctx.ellipse(0.0, 0.0, r, minor_px, 0.0, 0.0, 2.0 * PI)
                        .unwrap();
                    ctx.stroke();
                    ctx.restore();
                }
                DsoType::OpenCluster => {
                    ctx.set_stroke_style_str("rgba(255,220,50,0.8)");
                    ctx.set_line_dash(&js_sys::Array::of2(&3.0_f64.into(), &3.0_f64.into()))
                        .unwrap();
                    ctx.begin_path();
                    ctx.arc(sx, sy, r, 0.0, 2.0 * PI).unwrap();
                    ctx.stroke();
                    ctx.set_line_dash(&js_sys::Array::new()).unwrap();
                }
                DsoType::GlobularCluster => {
                    ctx.set_stroke_style_str("rgba(255,160,60,0.8)");
                    ctx.begin_path();
                    ctx.arc(sx, sy, r, 0.0, 2.0 * PI).unwrap();
                    ctx.stroke();
                    ctx.begin_path();
                    ctx.move_to(sx - r, sy);
                    ctx.line_to(sx + r, sy);
                    ctx.move_to(sx, sy - r);
                    ctx.line_to(sx, sy + r);
                    ctx.stroke();
                }
                DsoType::PlanetaryNebula => {
                    ctx.set_stroke_style_str("rgba(0,230,180,0.85)");
                    ctx.begin_path();
                    ctx.arc(sx, sy, r, 0.0, 2.0 * PI).unwrap();
                    ctx.stroke();
                    let tick = r * 0.5;
                    ctx.begin_path();
                    ctx.move_to(sx - r - tick, sy);
                    ctx.line_to(sx - r, sy);
                    ctx.move_to(sx + r, sy);
                    ctx.line_to(sx + r + tick, sy);
                    ctx.move_to(sx, sy - r - tick);
                    ctx.line_to(sx, sy - r);
                    ctx.move_to(sx, sy + r);
                    ctx.line_to(sx, sy + r + tick);
                    ctx.stroke();
                }
                DsoType::GalaxyCluster => {
                    ctx.set_stroke_style_str("rgba(220,100,220,0.75)");
                    ctx.set_line_dash(&js_sys::Array::of2(&2.0_f64.into(), &4.0_f64.into()))
                        .unwrap();
                    ctx.begin_path();
                    ctx.arc(sx, sy, r, 0.0, 2.0 * PI).unwrap();
                    ctx.stroke();
                    ctx.set_line_dash(&js_sys::Array::new()).unwrap();
                }
                DsoType::Nebula | DsoType::SupernovaRemnant => {
                    ctx.set_stroke_style_str("rgba(60,220,100,0.75)");
                    ctx.stroke_rect(sx - r, sy - r, px_size, px_size);
                }
            }
        }

        let label = dso.display_label(f.scene.cur_lang);

        if !f.mode.is_gpu() {
            f.hit_items.push(HitItem {
                sx,
                sy,
                radius: r.max(8.0),
                kind: HitKind::Dso(dso.kind),
                name: label.clone(),
                mag: Some(dso.mag),
                ra_jnow_deg: dso_jnow.ra_deg,
                dec_jnow_deg: dso_jnow.dec_deg,
                size_arcmin: Some(dso.size_arcmin as f64),
                phase: None,
            });
        }

        // Label (always, image or symbol).
        // On mobile, only label objects clearly brighter than the cutoff and
        // tighten the FOV gate; this drops fillText calls per frame when many
        // faint DSOs cluster near the centre at moderate zoom.
        let label_fov_gate = if f.scene.is_mobile { 25.0 } else { 50.0 };
        let label_mag_ok = !f.scene.is_mobile || (dso.mag as f64) <= f.state.dso_mag - 1.5;
        if f.toggles.names_on && f.view.fov < label_fov_gate && label_mag_ok && !f.mode.is_gpu() {
            ctx.set_fill_style_str(match dso.kind {
                DsoType::Galaxy => "rgba(0,200,220,0.85)",
                DsoType::OpenCluster => "rgba(255,220,50,0.85)",
                DsoType::GlobularCluster => "rgba(255,160,60,0.85)",
                DsoType::PlanetaryNebula => "rgba(0,230,180,0.85)",
                DsoType::GalaxyCluster => "rgba(220,100,220,0.85)",
                _ => "rgba(60,220,100,0.85)",
            });
            ctx.set_text_align("left");
            let _ = ctx.fill_text(&label, sx + r + 3.0, sy + 4.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helper: project and draw one equatorial FOV rectangle.
// ---------------------------------------------------------------------------

fn draw_fov_box(
    ctx: &CanvasRenderingContext2d,
    f: &mut Frame,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    ra_deg: f64,
    dec_deg: f64,
    fov_w: f64,
    fov_h: f64,
    rot_deg: f64,
    stroke_color: &str,
    fill_color: &str,
    line_width: f64,
    label: Option<&str>,
) {
    let cos_dec = dec_deg.to_radians().cos().abs().max(0.01);
    let half_w = fov_w / 2.0;
    let half_h = fov_h / 2.0;
    let corners_eq = [
        (ra_deg - half_w / cos_dec, dec_deg - half_h),
        (ra_deg + half_w / cos_dec, dec_deg - half_h),
        (ra_deg + half_w / cos_dec, dec_deg + half_h),
        (ra_deg - half_w / cos_dec, dec_deg + half_h),
    ];
    let (pcx, pcy) = {
        let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, f.scene.lst, f.scene.latitude);
        project(alt, az).unwrap_or((f.view.wf / 2.0, f.view.hf / 2.0))
    };
    let rot_rad = rot_deg.to_radians();
    let sin_r = rot_rad.sin();
    let cos_r = rot_rad.cos();

    let mut pts = [(0.0_f64, 0.0_f64); 4];
    let mut n = 0usize;
    for (cra, cdec) in &corners_eq {
        let (calt, caz) = astro::eq_to_altaz(*cra, *cdec, f.scene.lst, f.scene.latitude);
        if let Some((sx, sy)) = project(calt, caz) {
            let dx = sx - pcx;
            let dy = sy - pcy;
            pts[n] = (pcx + dx * cos_r - dy * sin_r, pcy + dx * sin_r + dy * cos_r);
            n += 1;
        }
    }
    if n < 4 {
        return;
    }

    // Filled background
    ctx.begin_path();
    ctx.move_to(pts[0].0, pts[0].1);
    for pt in &pts[1..] {
        ctx.line_to(pt.0, pt.1);
    }
    ctx.close_path();
    ctx.set_fill_style_str(fill_color);
    ctx.fill();

    // Border stroke
    ctx.set_stroke_style_str(stroke_color);
    ctx.set_line_width(line_width);
    ctx.begin_path();
    ctx.move_to(pts[0].0, pts[0].1);
    for pt in &pts[1..] {
        ctx.line_to(pt.0, pt.1);
    }
    ctx.close_path();
    ctx.stroke();

    // Optional label above the box
    if let Some(lbl) = label {
        if !lbl.is_empty() {
            let top_y = pts.iter().map(|pt| pt.1).fold(f64::INFINITY, f64::min);
            let cx_lbl = pts.iter().map(|pt| pt.0).sum::<f64>() / pts.len() as f64;
            ctx.set_fill_style_str(stroke_color);
            ctx.set_font("10px monospace");
            ctx.set_text_align("center");
            let _ = ctx.fill_text(lbl, cx_lbl, top_y - 3.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduler job FOV overlay
// ---------------------------------------------------------------------------

pub(super) fn render_scheduler_jobs(
    ctx: &CanvasRenderingContext2d,
    f: &mut Frame,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    if f.state.scheduler_jobs.is_empty() {
        return;
    }
    let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) = (
        f.state.fl,
        f.state.cam_pixel_size_um,
        f.state.cam_sensor_width,
        f.state.cam_sensor_height,
    ) else {
        return;
    };

    let fov_w = astro::fov_deg(fl_mm, sw as f64, px_um);
    let fov_h = astro::fov_deg(fl_mm, sh as f64, px_um);

    for job in &f.state.scheduler_jobs {
        let (stroke, fill) = match job.state {
            0 => ("rgba(160,160,160,0.80)", "rgba(160,160,160,0.05)"),
            1 => ("rgba(200,200,80,0.80)", "rgba(200,200,80,0.05)"),
            2 => ("rgba(80,200,255,0.85)", "rgba(80,200,255,0.06)"),
            3 => ("rgba(80,220,80,0.90)", "rgba(80,220,80,0.08)"),
            4 => ("rgba(220,80,80,0.90)", "rgba(220,80,80,0.08)"),
            5 => ("rgba(220,140,80,0.80)", "rgba(220,140,80,0.05)"),
            7 => ("rgba(200,200,200,0.50)", "rgba(200,200,200,0.03)"),
            _ => ("rgba(160,160,160,0.80)", "rgba(160,160,160,0.05)"),
        };
        draw_fov_box(
            ctx,
            f,
            project,
            job.ra_h * 15.0,
            job.dec_deg,
            fov_w,
            fov_h,
            f.state.rotation_deg.unwrap_or(0.0),
            stroke,
            fill,
            1.0,
            Some(&job.name),
        );
    }
}

// ---------------------------------------------------------------------------
// Mosaic tile grid rendering (shared for KStars mosaic and in-app planner)
// ---------------------------------------------------------------------------

pub(super) fn render_mosaic_plan(
    ctx: &CanvasRenderingContext2d,
    f: &mut Frame,
    plan: &MosaicPlanRender,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    is_preview: bool,
) {
    if plan.tiles.is_empty() {
        return;
    }

    let (tile_stroke, margin_stroke, fill_color, bbox_stroke) = if is_preview {
        (
            "rgba(80,180,255,0.85)",
            "rgba(80,180,255,0.35)",
            "rgba(80,180,255,0.06)",
            "rgba(80,180,255,0.60)",
        )
    } else {
        (
            "rgba(255,180,60,0.85)",
            "rgba(255,180,60,0.35)",
            "rgba(255,180,60,0.06)",
            "rgba(255,220,100,0.60)",
        )
    };

    for tile in &plan.tiles {
        let total_rot = tile.rotation + plan.pa_deg;
        draw_fov_box(
            ctx,
            f,
            project,
            tile.ra_deg,
            tile.dec_deg,
            plan.fov_w_deg,
            plan.fov_h_deg,
            total_rot,
            tile_stroke,
            fill_color,
            1.5,
            None,
        );

        // Overlap margin: inner inset box
        if plan.overlap_pct > 0.0 {
            let margin = plan.overlap_pct / 100.0;
            draw_fov_box(
                ctx,
                f,
                project,
                tile.ra_deg,
                tile.dec_deg,
                plan.fov_w_deg * (1.0 - margin),
                plan.fov_h_deg * (1.0 - margin),
                total_rot,
                margin_stroke,
                "rgba(0,0,0,0)",
                0.8,
                None,
            );
        }
    }

    // Mosaic bounding box + target label
    if plan.tiles.len() > 1 || !plan.target_name.is_empty() {
        let min_ra = plan
            .tiles
            .iter()
            .map(|t| t.ra_deg)
            .fold(f64::INFINITY, f64::min);
        let max_ra = plan
            .tiles
            .iter()
            .map(|t| t.ra_deg)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_dec = plan
            .tiles
            .iter()
            .map(|t| t.dec_deg)
            .fold(f64::INFINITY, f64::min);
        let max_dec = plan
            .tiles
            .iter()
            .map(|t| t.dec_deg)
            .fold(f64::NEG_INFINITY, f64::max);
        let center_ra = (min_ra + max_ra) / 2.0;
        let center_dec = (min_dec + max_dec) / 2.0;
        let bbox_w = (max_ra - min_ra) + plan.fov_w_deg;
        let bbox_h = (max_dec - min_dec) + plan.fov_h_deg;
        let lbl = if plan.target_name.is_empty() {
            None
        } else {
            Some(plan.target_name.as_str())
        };
        draw_fov_box(
            ctx,
            f,
            project,
            center_ra,
            center_dec,
            bbox_w,
            bbox_h,
            plan.pa_deg,
            bbox_stroke,
            "rgba(0,0,0,0)",
            2.0,
            lbl,
        );
    }
}

// ---------------------------------------------------------------------------
// Info overlay (bottom-left)
// ---------------------------------------------------------------------------

pub(super) fn render_info_overlay(ctx: &CanvasRenderingContext2d, f: &Frame) {
    ctx.set_fill_style_str("rgba(10,10,20,0.75)");
    ctx.fill_rect(0.0, f.view.hf - 128.0, 360.0, 128.0);
    ctx.set_fill_style_str("#aabbcc");
    ctx.set_font("11px monospace");
    ctx.set_text_align("left");

    let tr = t(f.scene.cur_lang);
    let lst_h = f.scene.lst / 15.0;
    let lst_hh = lst_h as u32;
    let lst_mm = ((lst_h - lst_hh as f64) * 60.0) as u32;
    let _ = ctx.fill_text(
        &format!(
            "{}: {:02}h{:02}m  {}: {:.0}\u{00b0}",
            tr.overlay_lst, lst_hh, lst_mm, tr.overlay_fov, f.view.fov
        ),
        8.0,
        f.view.hf - 108.0,
    );
    let _ = ctx.fill_text(
        &format!(
            "{}: {} {:.1}\u{00b0}  {} {:.1}\u{00b0}",
            tr.overlay_center, tr.overlay_alt, f.view.c_alt, tr.overlay_az, f.view.c_az
        ),
        8.0,
        f.view.hf - 92.0,
    );
    if let (Some(ra_h), Some(dec)) = (f.state.mount_ra_h, f.state.mount_dec_deg) {
        let rah = ra_h as u32;
        let ram = ((ra_h - rah as f64) * 60.0) as u32;
        let _ = ctx.fill_text(
            &format!(
                "{}: {:02}h{:02}m  {:+.1}\u{00b0}",
                tr.overlay_mount, rah, ram, dec
            ),
            8.0,
            f.view.hf - 76.0,
        );
    } else {
        let _ = ctx.fill_text(tr.overlay_mount_none, 8.0, f.view.hf - 76.0);
    }
    if let Some(rot) = f.state.rotation_deg {
        let _ = ctx.fill_text(
            &format!("{}: {:.1}\u{00b0}", tr.overlay_camera_angle, rot),
            8.0,
            f.view.hf - 60.0,
        );
    }
    if f.scene.t_off.abs() > 0.5 {
        let _ = ctx.fill_text(
            &format!("{}: {:+.0}s", tr.overlay_time_offset, f.scene.t_off),
            8.0,
            f.view.hf - 44.0,
        );
    }
    if let (Some((alt, az)), Some((ra, dec))) = (f.state.cursor_altaz, f.state.cursor_radec) {
        let ra_h = ra / 15.0;
        let rah = ra_h as u32;
        let ram = ((ra_h - rah as f64) * 60.0) as u32;
        let _ = ctx.fill_text(
            &format!(
                "{}: {} {:+.1}\u{00b0} {} {:.1}\u{00b0}  {:02}h{:02}m {:+.1}\u{00b0}",
                tr.overlay_cursor, tr.overlay_alt, alt, tr.overlay_az, az, rah, ram, dec,
            ),
            8.0,
            f.view.hf - 28.0,
        );
    }
}

// ---------------------------------------------------------------------------
// Solar system — Sun, Moon, planets via the ephemeris module.
// ---------------------------------------------------------------------------

pub(super) fn render_solar_system(
    ctx: &CanvasRenderingContext2d,
    f: &mut Frame,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    // ── Sun ──────────────────────────────────────────────────────────
    let sun = ephemeris::sun(f.scene.jd);
    if let Some((sx, sy)) = altaz_project(f, project, sun.jnow.ra_deg, sun.jnow.dec_deg) {
        let r = 10.0;
        ctx.set_fill_style_str("rgba(255,220,80,0.95)");
        ctx.begin_path();
        let _ = ctx.arc(sx, sy, r, 0.0, 2.0 * PI);
        ctx.fill();
        ctx.set_stroke_style_str("rgba(255,160,40,0.9)");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        let _ = ctx.arc(sx, sy, r + 2.0, 0.0, 2.0 * PI);
        ctx.stroke();
        if f.toggles.names_on && f.view.fov < 60.0 {
            ctx.set_fill_style_str("rgba(255,220,80,0.95)");
            ctx.set_font("14px monospace");
            ctx.set_text_align("left");
            let _ = ctx.fill_text(t(f.scene.cur_lang).body_sun, sx + r + 4.0, sy + 4.0);
        }
        if !f.mode.is_gpu() {
            f.hit_items.push(HitItem {
                sx,
                sy,
                radius: r + 4.0,
                kind: HitKind::Sun,
                name: t(f.scene.cur_lang).body_sun.to_string(),
                mag: Some(sun.mag),
                ra_jnow_deg: sun.jnow.ra_deg,
                dec_jnow_deg: sun.jnow.dec_deg,
                size_arcmin: sun.angular_diameter_arcmin,
                phase: None,
            });
        }
    }

    // ── Moon ─────────────────────────────────────────────────────────
    let moon = ephemeris::moon(f.scene.jd);
    if let Some((sx, sy)) = altaz_project(f, project, moon.jnow.ra_deg, moon.jnow.dec_deg) {
        let r = 9.0;
        // Base disk (illuminated side)
        ctx.set_fill_style_str("rgba(220,220,230,0.95)");
        ctx.begin_path();
        let _ = ctx.arc(sx, sy, r, 0.0, 2.0 * PI);
        ctx.fill();

        // Simple terminator: draw a dark half-disc sized by (1 - illuminated fraction).
        if let Some(illum) = moon.phase {
            let dark = 1.0 - illum;
            ctx.set_fill_style_str("rgba(20,20,30,0.85)");
            if dark > 0.05 {
                // Crude: shade a vertical slice from -r to (-r + 2*r*dark).
                ctx.save();
                ctx.begin_path();
                let _ = ctx.arc(sx, sy, r, 0.0, 2.0 * PI);
                ctx.clip();
                let w = 2.0 * r * dark;
                ctx.fill_rect(sx - r, sy - r, w, 2.0 * r);
                ctx.restore();
            }
        }

        ctx.set_stroke_style_str("rgba(160,160,200,0.8)");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        let _ = ctx.arc(sx, sy, r, 0.0, 2.0 * PI);
        ctx.stroke();

        if f.toggles.names_on && f.view.fov < 60.0 {
            ctx.set_fill_style_str("rgba(220,220,230,0.95)");
            ctx.set_font("14px monospace");
            ctx.set_text_align("left");
            let _ = ctx.fill_text(t(f.scene.cur_lang).body_moon, sx + r + 4.0, sy + 4.0);
        }
        if !f.mode.is_gpu() {
            f.hit_items.push(HitItem {
                sx,
                sy,
                radius: r + 4.0,
                kind: HitKind::Moon,
                name: t(f.scene.cur_lang).body_moon.to_string(),
                mag: Some(moon.mag),
                ra_jnow_deg: moon.jnow.ra_deg,
                dec_jnow_deg: moon.jnow.dec_deg,
                size_arcmin: moon.angular_diameter_arcmin,
                phase: moon.phase,
            });
        }
    }

    // ── Planets ──────────────────────────────────────────────────────
    let planets = ephemeris::all_planets(f.scene.jd);
    for (planet, pos) in &planets {
        let Some((sx, sy)) = altaz_project(f, project, pos.jnow.ra_deg, pos.jnow.dec_deg) else {
            continue;
        };
        let (color, r) = planet_style(*planet, pos.mag);
        ctx.set_fill_style_str(color);
        ctx.begin_path();
        let _ = ctx.arc(sx, sy, r, 0.0, 2.0 * PI);
        ctx.fill();
        if f.toggles.names_on && f.view.fov < 40.0 {
            ctx.set_fill_style_str(color);
            ctx.set_font("13px monospace");
            ctx.set_text_align("left");
            let _ = ctx.fill_text(planet.name_i18n(f.scene.cur_lang), sx + r + 3.0, sy + 4.0);
        }
        if !f.mode.is_gpu() {
            f.hit_items.push(HitItem {
                sx,
                sy,
                radius: (r + 3.0).max(8.0),
                kind: HitKind::Planet,
                name: planet.name_i18n(f.scene.cur_lang).to_string(),
                mag: Some(pos.mag),
                ra_jnow_deg: pos.jnow.ra_deg,
                dec_jnow_deg: pos.jnow.dec_deg,
                size_arcmin: None,
                phase: pos.phase,
            });
        }
    }
}

fn altaz_project(
    f: &mut Frame,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    ra_deg: f64,
    dec_deg: f64,
) -> Option<(f64, f64)> {
    let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, f.scene.lst, f.scene.latitude);
    if alt < -3.0 {
        return None;
    }
    project(alt, az)
}

fn planet_style(p: ephemeris::Planet, mag: f32) -> (&'static str, f64) {
    // Size scales with magnitude (brighter = bigger).
    let r = ((4.0 - mag as f64).clamp(2.5, 6.0)).max(2.5);
    let color = match p {
        ephemeris::Planet::Mercury => "rgba(200,200,180,0.95)",
        ephemeris::Planet::Venus => "rgba(240,240,200,0.98)",
        ephemeris::Planet::Mars => "rgba(240,120,80,0.95)",
        ephemeris::Planet::Jupiter => "rgba(240,210,160,0.95)",
        ephemeris::Planet::Saturn => "rgba(220,200,140,0.95)",
        ephemeris::Planet::Uranus => "rgba(160,220,230,0.9)",
        ephemeris::Planet::Neptune => "rgba(120,160,240,0.9)",
    };
    (color, r)
}
