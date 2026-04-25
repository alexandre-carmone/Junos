//! Canvas2D rendering logic for the sky map overlay.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::Arc;

use web_sys::{CanvasRenderingContext2d, HtmlImageElement};

use crate::astro;
use crate::catalog::CatalogData;
use crate::coords::{J2000, JNow};
use crate::dso_catalog::{DsoCatalogData, DsoType};
use crate::ephemeris;
use crate::i18n::{Lang, constellation_name, t};
use crate::nebulae::NebulaeIndex;

use super::utils::bv_to_rgb;

// ── Star size tuning ──────────────────────────────────────────────────────────
// Radius (CSS pixels) = STAR_SIZE_BASE - mag * STAR_SIZE_MAG_SCALE
// Increase STAR_SIZE_BASE to make all stars larger.
const STAR_SIZE_BASE:      f64 = 3.5;
const STAR_SIZE_MAG_SCALE: f64 = 0.35;
const STAR_SIZE_MIN:       f64 = 0.5;

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
    pub radius: f64,           // hit radius in CSS px
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
    pub name:    String,
    pub ra_h:    f64,
    pub dec_deg: f64,
    pub state:   i64,
}

/// A single mosaic tile to render on the sky.
#[derive(Clone)]
pub struct MosaicTileRender {
    pub ra_deg:  f64,
    pub dec_deg: f64,
    /// PA of this individual tile (usually 0; additional rotation added via plan.pa_deg).
    pub rotation: f64,
}

/// A complete mosaic plan (either from KStars new_mosaic_tiles or from the in-app planner).
#[derive(Clone)]
pub struct MosaicPlanRender {
    pub target_name: String,
    pub tiles:       Vec<MosaicTileRender>,
    pub fov_w_deg:   f64,
    pub fov_h_deg:   f64,
    pub overlap_pct: f64,
    pub pa_deg:      f64,
}

/// All values needed to render a single frame (no reactive signals).
pub struct RenderParams {
    pub wf: f64,
    pub hf: f64,
    pub c_alt: f64,
    pub c_az: f64,
    pub fov: f64,
    pub lst: f64,
    pub latitude: f64,
    pub sin_lat: f64,
    pub cos_lat: f64,
    pub mag_limit: f32,
    pub has_gpu: bool,
    pub stars_on: bool,
    pub names_on: bool,
    pub const_on: bool,
    pub con_names_on: bool,
    pub grid_on: bool,
    pub eq_grid_on: bool,
    pub meridian_on: bool,
    pub ecliptic_on: bool,
    pub zenith_on: bool,
    pub solar_system_on: bool,
    pub solve_marker_on: bool,
    pub slew_trail_on: bool,
    pub fov_on: bool,
    pub dso_on: bool,
    pub dso_gx: bool,
    pub dso_oc: bool,
    pub dso_gc: bool,
    pub dso_nb: bool,
    pub dso_pn: bool,
    pub dso_snr: bool,
    pub dso_gal: bool,
    pub dso_mag: f64,
    pub fl: Option<f64>,
    pub mount_connected: bool,
    pub mount_ra_h: Option<f64>,
    pub mount_dec_deg: Option<f64>,
    pub cam_pixel_size_um: Option<f64>,
    pub cam_sensor_width: Option<u32>,
    pub cam_sensor_height: Option<u32>,
    pub rotation_deg: Option<f64>,
    /// Latest plate-solve result (JNow). All fields optional — populated only
    /// once the user actually runs a solve.
    pub solve_ra_jnow_deg: Option<f64>,
    pub solve_dec_jnow_deg: Option<f64>,
    pub solve_pixscale_arcsec: Option<f64>,
    pub solve_age_ms: Option<f64>,
    /// Cursor position in world coords (Alt, Az) and (RA, Dec). None when the
    /// pointer is off-canvas.
    pub cursor_altaz: Option<(f64, f64)>,
    pub cursor_radec: Option<(f64, f64)>,
    pub t_off: f64,
    pub jd: f64,
    pub cur_lang: Lang,
    pub scheduler_jobs_on: bool,
    pub scheduler_jobs:    Vec<SchedulerJobRender>,
    /// Mosaic received from KStars via new_mosaic_tiles.
    pub mosaic_kstars: Option<MosaicPlanRender>,
    /// Live mosaic preview from the in-app planner (shown while planning mode is active).
    pub mosaic_plan: Option<MosaicPlanRender>,
}

/// Render the full sky overlay on a Canvas2D context.
///
/// `hit_items` is appended-to during render (cleared by the caller before each
/// frame). The mouse-up handler in `mod.rs` walks the resulting list to map a
/// click to the nearest hovered object.
pub fn render_overlay(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    cat: &Option<Arc<CatalogData>>,
    dso_cat: &Option<Arc<DsoCatalogData>>,
    nebulae_index: Option<&NebulaeIndex>,
    nebulae_cache: &mut HashMap<String, HtmlImageElement>,
    hit_items: &mut Vec<HitItem>,
    slew_trail: &[(f64, f64, f64)],
) {
    let cx = p.wf / 2.0;
    let cy = p.hf / 2.0;
    let scale = p.hf.min(p.wf) / 2.0;

    let project = |alt: f64, az: f64| -> Option<(f64, f64)> {
        astro::project(alt, az, p.c_alt, p.c_az, p.fov)
            .map(|(x, y)| (cx + x * scale, cy - y * scale))
    };

    if p.has_gpu {
        // Clear overlay (transparent — shows GPU canvas underneath)
        ctx.clear_rect(0.0, 0.0, p.wf, p.hf);
    } else {
        // Full Canvas2D fallback rendering
        ctx.set_fill_style_str("#0a0a14");
        ctx.fill_rect(0.0, 0.0, p.wf, p.hf);
        render_fallback_stars(ctx, p, cat, &project, cx, cy, scale, hit_items);
    }

    // Everything below renders on the Canvas2D overlay (both GPU and fallback)
    render_ground(ctx, p, &project);
    if p.grid_on {
        render_altaz_grid(ctx, p, &project);
    }
    if p.meridian_on {
        render_meridian(ctx, p, &project);
    }
    if p.eq_grid_on {
        render_eq_grid(ctx, p, &project);
    }
    if p.ecliptic_on {
        render_ecliptic(ctx, p, &project);
    }
    if p.zenith_on {
        render_zenith(ctx, p, &project);
    }
    if p.has_gpu && p.names_on && p.stars_on {
        render_star_names_gpu(ctx, p, cat, &project, hit_items);
    } else if p.has_gpu && p.stars_on {
        // Stars are drawn by the GPU but we still need named-star hit items.
        push_star_hit_items(p, cat, &project, hit_items);
    }
    if p.has_gpu && p.const_on && p.con_names_on {
        render_constellation_names_gpu(ctx, p, cat, &project);
    }
    if p.dso_on {
        render_dso(ctx, p, dso_cat, &project, scale, nebulae_index, nebulae_cache, hit_items);
    }
    if p.solar_system_on {
        render_solar_system(ctx, p, &project, hit_items);
    }
    if p.slew_trail_on {
        render_slew_trail(ctx, p, &project, slew_trail);
    }
    render_mount_crosshair(ctx, p, &project);
    if p.solve_marker_on {
        render_solve_marker(ctx, p, &project);
    }
    render_center_crosshair(ctx, cx, cy);
    if p.fov_on {
        render_center_fov(ctx, p, &project, cx, cy);
        render_mount_fov(ctx, p, &project, cx, cy);
    }
    if let Some(ref plan) = p.mosaic_kstars.clone() {
        render_mosaic_plan(ctx, p, plan, &project, false);
    }
    if let Some(ref plan) = p.mosaic_plan.clone() {
        render_mosaic_plan(ctx, p, plan, &project, true);
    }
    if p.scheduler_jobs_on {
        render_scheduler_jobs(ctx, p, &project);
    }
    render_info_overlay(ctx, p);
}

// ---------------------------------------------------------------------------
// Fallback CPU rendering (stars + constellation lines + names)
// ---------------------------------------------------------------------------

fn render_fallback_stars(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    cat: &Option<Arc<CatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    _cx: f64,
    _cy: f64,
    _scale: f64,
    hit_items: &mut Vec<HitItem>,
) {
    let Some(ref cat) = cat else { return };

    let mut idx_screen: Vec<Option<(f64, f64, f32, f32)>> = vec![None; cat.stars.len()];
    if p.stars_on || p.const_on {
        let lst_rad = p.lst.to_radians();
        for (i, star) in cat.stars.iter().enumerate() {
            if p.stars_on && star.mag > p.mag_limit { continue; }
            let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(p.jd);
            let ha = lst_rad - jnow.ra_deg.to_radians();
            let dec = jnow.dec_deg.to_radians();
            let sin_dec = dec.sin();
            let cos_dec = dec.cos();
            let sin_alt = sin_dec * p.sin_lat + cos_dec * p.cos_lat * ha.cos();
            let alt_rad = sin_alt.asin();
            let alt = alt_rad.to_degrees();
            if alt < -5.0 { continue; }
            let cos_az = (sin_dec - alt_rad.sin() * p.sin_lat) / (alt_rad.cos() * p.cos_lat);
            let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
            if ha.sin() > 0.0 { az = 360.0 - az; }
            if let Some((sx, sy)) = project(alt, az) {
                if sx > -50.0 && sx < p.wf + 50.0 && sy > -50.0 && sy < p.hf + 50.0 {
                    idx_screen[i] = Some((sx, sy, star.mag, star.bv));
                }
            }
        }
    }

    // Constellation lines
    if p.const_on {
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
    if p.stars_on {
        // Scale star size with screen: reference = 1000 CSS pixels (min dimension)
        let screen_scale = (p.wf.min(p.hf) / 1000.0).clamp(0.4, 1.5);
        for screen in &idx_screen {
            if let Some((sx, sy, mag, bv)) = screen {
                let radius = ((STAR_SIZE_BASE - *mag as f64 * STAR_SIZE_MAG_SCALE) * screen_scale).max(STAR_SIZE_MIN);
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
    if p.stars_on {
        for (i, star) in cat.stars.iter().enumerate() {
            let Some(Some((sx, sy, mag, _))) = idx_screen.get(i) else { continue };
            let Some(name) = star.name.as_deref() else { continue };
            if name == "Sol" { continue; } // duplicate of ephemeris Sun, wrong position
            let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(p.jd);
            hit_items.push(HitItem {
                sx: *sx, sy: *sy,
                radius: ((STAR_SIZE_BASE - *mag as f64 * STAR_SIZE_MAG_SCALE).max(4.0)),
                kind: HitKind::Star,
                name: name.to_string(),
                mag: Some(*mag),
                ra_jnow_deg: jnow.ra_deg,
                dec_jnow_deg: jnow.dec_deg,
                size_arcmin: None,
                phase: None,
            });
            if p.names_on && *mag < 3.0 {
                ctx.set_fill_style_str("#8899aa");
                ctx.set_font("10px monospace");
                ctx.set_text_align("left");
                let _ = ctx.fill_text(name, sx + 6.0, sy - 4.0);
            }
        }
    }

    // Constellation names
    if p.const_on && p.con_names_on {
        ctx.set_fill_style_str("rgba(100,120,180,0.7)");
        ctx.set_font("italic 11px monospace");
        ctx.set_text_align("center");
        let lst_rad = p.lst.to_radians();
        for (abbr, name, cra, cdec) in &cat.centers {
            let cjnow = J2000::new(*cra as f64, *cdec as f64).to_jnow(p.jd);
            let ha = lst_rad - cjnow.ra_deg.to_radians();
            let dec_rad = cjnow.dec_deg.to_radians();
            let sin_dec = dec_rad.sin();
            let cos_dec = dec_rad.cos();
            let sin_alt = sin_dec * p.sin_lat + cos_dec * p.cos_lat * ha.cos();
            let alt_rad = sin_alt.asin();
            let alt = alt_rad.to_degrees();
            if alt < -5.0 { continue; }
            let cos_az = (sin_dec - alt_rad.sin() * p.sin_lat) / (alt_rad.cos() * p.cos_lat);
            let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
            if ha.sin() > 0.0 { az = 360.0 - az; }
            if let Some((sx, sy)) = project(alt, az) {
                let label = constellation_name(abbr, p.cur_lang).unwrap_or(name);
                let _ = ctx.fill_text(label, sx, sy);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ground + horizon
// ---------------------------------------------------------------------------

fn render_ground(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    // Horizon line
    {
        let mut first = true;
        ctx.begin_path();
        for i in (0..=360).step_by(3) {
            let az = i as f64;
            if let Some((px, py)) = project(0.0, az) {
                if first { ctx.move_to(px, py); first = false; }
                else { ctx.line_to(px, py); }
            }
        }
        ctx.set_stroke_style_str("#b07840");
        ctx.set_line_width(2.5);
        ctx.stroke();
    }

    // Cardinal labels
    ctx.set_font("bold 14px monospace");
    ctx.set_text_align("center");
    let tr = t(p.cur_lang);
    for (label, az) in &[
        (tr.cardinal_n, 0.0_f64),
        (tr.cardinal_e, 90.0),
        (tr.cardinal_s, 180.0),
        (tr.cardinal_w, 270.0),
    ] {
        if let Some((sx, sy)) = project(-2.0, *az) {
            ctx.set_fill_style_str("#000");
            let _ = ctx.fill_text(label, sx + 1.0, sy + 15.0);
            ctx.set_fill_style_str("#ddaa66");
            let _ = ctx.fill_text(label, sx, sy + 14.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Alt/Az grid
// ---------------------------------------------------------------------------

fn render_altaz_grid(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    // Adaptive step: coarser when zoomed out to reduce projection count
    let az_step = if p.fov > 60.0 { 6 } else if p.fov > 20.0 { 4 } else { 3 };
    let alt_step = if p.fov > 60.0 { 10 } else { 5 };

    ctx.set_stroke_style_str("rgba(60,60,100,0.6)");
    ctx.set_line_width(0.7);

    ctx.begin_path();
    for alt_i in (-1..=9).map(|i| i * 10) {
        let mut first = true;
        for az_i in (0..=360).step_by(az_step) {
            if let Some((sx, sy)) = project(alt_i as f64, az_i as f64) {
                if first { ctx.move_to(sx, sy); first = false; }
                else { ctx.line_to(sx, sy); }
            }
        }
    }
    ctx.stroke();

    ctx.begin_path();
    for az_i in (0..360).step_by(30) {
        let mut first = true;
        for alt_i in (0..=90).step_by(alt_step) {
            if let Some((sx, sy)) = project(alt_i as f64, az_i as f64) {
                if first { ctx.move_to(sx, sy); first = false; }
                else { ctx.line_to(sx, sy); }
            }
        }
    }
    ctx.stroke();
}

fn render_meridian(
    ctx: &CanvasRenderingContext2d,
    _p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    ctx.set_stroke_style_str("rgba(80,80,140,0.7)");
    ctx.set_line_width(1.0);
    ctx.begin_path();
    let mut first = true;
    for alt_i in (0..=90).step_by(3) {
        if let Some((sx, sy)) = project(alt_i as f64, 0.0) {
            if first { ctx.move_to(sx, sy); first = false; }
            else { ctx.line_to(sx, sy); }
        }
    }
    for alt_i in (0..=90).rev().step_by(3) {
        if let Some((sx, sy)) = project(alt_i as f64, 180.0) {
            ctx.line_to(sx, sy);
        }
    }
    ctx.stroke();
}

// ---------------------------------------------------------------------------
// Equatorial grid
// ---------------------------------------------------------------------------

fn render_eq_grid(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    let project_eq = |ra_deg: f64, dec_deg: f64| -> Option<(f64, f64)> {
        let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, p.lst, p.latitude);
        project(alt, az)
    };

    // Adaptive step: coarser when zoomed out
    let step = if p.fov > 60.0 { 6 } else if p.fov > 20.0 { 4 } else { 2 };

    // Dec parallels every 30 deg
    ctx.set_stroke_style_str("rgba(100,100,200,0.6)");
    ctx.set_line_width(0.7);
    ctx.begin_path();
    for dec_i in (-3..=3).map(|i| i * 30) {
        let dec = dec_i as f64;
        let mut first = true;
        for ra_i in (0..=360).step_by(step) {
            let ra = ra_i as f64;
            if let Some((sx, sy)) = project_eq(ra, dec) {
                if first { ctx.move_to(sx, sy); first = false; }
                else { ctx.line_to(sx, sy); }
            } else {
                first = true;
            }
        }
    }
    ctx.stroke();

    // RA meridians every 2h (= 30 deg)
    ctx.begin_path();
    for ra_i in (0..12).map(|i| i * 30) {
        let ra = ra_i as f64;
        let mut first = true;
        for dec_i in (-90..=90).step_by(step) {
            let dec = dec_i as f64;
            if let Some((sx, sy)) = project_eq(ra, dec) {
                if first { ctx.move_to(sx, sy); first = false; }
                else { ctx.line_to(sx, sy); }
            } else {
                first = true;
            }
        }
    }
    ctx.stroke();

    // Celestial equator highlighted
    ctx.set_stroke_style_str("rgba(120,120,255,0.8)");
    ctx.set_line_width(1.2);
    ctx.begin_path();
    let mut first = true;
    for ra_i in (0..=360).step_by(step) {
        let ra = ra_i as f64;
        if let Some((sx, sy)) = project_eq(ra, 0.0) {
            if first { ctx.move_to(sx, sy); first = false; }
            else { ctx.line_to(sx, sy); }
        } else {
            first = true;
        }
    }
    ctx.stroke();
}

// ---------------------------------------------------------------------------
// Star names (GPU path — project only named bright stars on CPU)
// ---------------------------------------------------------------------------

fn render_star_names_gpu(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    cat: &Option<Arc<CatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    hit_items: &mut Vec<HitItem>,
) {
    let Some(ref cat) = cat else { return };
    ctx.set_fill_style_str("#8899aa");
    ctx.set_font("10px monospace");
    ctx.set_text_align("left");
    let lst_rad = p.lst.to_radians();
    for star in cat.stars.iter() {
        let Some(name) = star.name.as_deref() else { continue };
        if name == "Sol" { continue; }
        if star.mag >= 3.0 { continue; }
        let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(p.jd);
        let ha = lst_rad - jnow.ra_deg.to_radians();
        let dec = jnow.dec_deg.to_radians();
        let sin_dec = dec.sin();
        let cos_dec = dec.cos();
        let sin_alt = sin_dec * p.sin_lat + cos_dec * p.cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -5.0 { continue; }
        let cos_az = (sin_dec - alt_rad.sin() * p.sin_lat) / (alt_rad.cos() * p.cos_lat);
        let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 { az = 360.0 - az; }
        if let Some((sx, sy)) = project(alt, az) {
            let _ = ctx.fill_text(name, sx + 6.0, sy - 4.0);
            hit_items.push(HitItem {
                sx, sy,
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

/// Push hit items for named stars *without* drawing labels (GPU path when
/// the user turned labels off but still expects stars to be clickable).
fn push_star_hit_items(
    p: &RenderParams,
    cat: &Option<Arc<CatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    hit_items: &mut Vec<HitItem>,
) {
    let Some(ref cat) = cat else { return };
    let lst_rad = p.lst.to_radians();
    for star in cat.stars.iter() {
        let Some(name) = star.name.as_deref() else { continue };
        if name == "Sol" { continue; }
        if star.mag >= 3.0 { continue; }
        let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(p.jd);
        let ha = lst_rad - jnow.ra_deg.to_radians();
        let dec = jnow.dec_deg.to_radians();
        let sin_dec = dec.sin();
        let cos_dec = dec.cos();
        let sin_alt = sin_dec * p.sin_lat + cos_dec * p.cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -5.0 { continue; }
        let cos_az = (sin_dec - alt_rad.sin() * p.sin_lat) / (alt_rad.cos() * p.cos_lat);
        let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 { az = 360.0 - az; }
        if let Some((sx, sy)) = project(alt, az) {
            hit_items.push(HitItem {
                sx, sy,
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

fn render_constellation_names_gpu(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    cat: &Option<Arc<CatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    let Some(ref cat) = cat else { return };
    ctx.set_fill_style_str("rgba(100,120,180,0.7)");
    ctx.set_font("italic 11px monospace");
    ctx.set_text_align("center");
    let lst_rad = p.lst.to_radians();
    for (abbr, name, cra, cdec) in &cat.centers {
        let cjnow = J2000::new(*cra as f64, *cdec as f64).to_jnow(p.jd);
        let ha = lst_rad - cjnow.ra_deg.to_radians();
        let dec_rad = cjnow.dec_deg.to_radians();
        let sin_dec = dec_rad.sin();
        let cos_dec = dec_rad.cos();
        let sin_alt = sin_dec * p.sin_lat + cos_dec * p.cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -5.0 { continue; }
        let cos_az = (sin_dec - alt_rad.sin() * p.sin_lat) / (alt_rad.cos() * p.cos_lat);
        let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 { az = 360.0 - az; }
        if let Some((sx, sy)) = project(alt, az) {
            let label = constellation_name(abbr, p.cur_lang).unwrap_or(name);
            let _ = ctx.fill_text(label, sx, sy);
        }
    }
}

// ---------------------------------------------------------------------------
// Deep-sky objects
// ---------------------------------------------------------------------------

fn render_dso(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    dso_cat: &Option<Arc<DsoCatalogData>>,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    scale: f64,
    nebulae_index: Option<&NebulaeIndex>,
    nebulae_cache: &mut HashMap<String, HtmlImageElement>,
    hit_items: &mut Vec<HitItem>,
) {
    let Some(ref dso_cat) = dso_cat else { return };
    let lst_rad = p.lst.to_radians();
    let cx = p.wf / 2.0;
    let cy = p.hf / 2.0;
    ctx.set_line_width(1.0);
    ctx.set_font("9px monospace");

    // ── Cheap angular-distance pre-cull (J2000) ───────────────────────────
    // Compute the view center in J2000 once, then skip the per-DSO
    // precession + altaz + projection chain for objects clearly outside the
    // visible cap. Iteration set is unchanged from the original full scan,
    // so no DSO can ever be missed by this filter — only the heavy math is
    // skipped for far-away objects.
    let (c_ra_jnow, c_dec_jnow) =
        astro::altaz_to_eq(p.c_alt, p.c_az, p.lst, p.latitude);
    let view_j2000 = JNow::new(c_ra_jnow, c_dec_jnow).to_j2000(p.jd);
    let v_ra_rad = view_j2000.ra_deg.to_radians();
    let v_dec_rad = view_j2000.dec_deg.to_radians();
    let v_sin_dec = v_dec_rad.sin();
    let v_cos_dec = v_dec_rad.cos();
    // Cap radius generously: render rejects beyond fov*1.5; add margin for
    // wide-field nebulae whose center is just outside but corners are in.
    let cap_radius_deg = p.fov * 1.5 + 6.0;
    let cos_cap = if cap_radius_deg >= 180.0 {
        -1.0
    } else {
        cap_radius_deg.to_radians().cos()
    };

    for dso in dso_cat.dsos.iter() {
        let type_ok = match dso.kind {
            DsoType::Galaxy          => p.dso_gx,
            DsoType::OpenCluster     => p.dso_oc,
            DsoType::GlobularCluster => p.dso_gc,
            DsoType::Nebula          => p.dso_nb,
            DsoType::PlanetaryNebula => p.dso_pn,
            DsoType::SupernovaRemnant => p.dso_snr,
            DsoType::GalaxyCluster   => p.dso_gal,
        };
        if !type_ok { continue; }
        if (dso.mag as f64) > p.dso_mag { continue; }

        // Cheap great-circle gate in J2000 — skips precession/altaz when
        // the object is well outside the visible cap.
        let d_ra_rad = (dso.ra_deg as f64).to_radians();
        let d_dec_rad = (dso.dec_deg as f64).to_radians();
        let cos_sep =
            v_sin_dec * d_dec_rad.sin()
                + v_cos_dec * d_dec_rad.cos() * (d_ra_rad - v_ra_rad).cos();
        if cos_sep < cos_cap { continue; }

        let dso_jnow = J2000::new(dso.ra_deg as f64, dso.dec_deg as f64).to_jnow(p.jd);
        let ha = lst_rad - dso_jnow.ra_deg.to_radians();
        let dec_rad = dso_jnow.dec_deg.to_radians();
        let sin_dec = dec_rad.sin();
        let cos_dec = dec_rad.cos();
        let sin_alt = sin_dec * p.sin_lat + cos_dec * p.cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -3.0 { continue; }
        let cos_az_val = (sin_dec - alt_rad.sin() * p.sin_lat) / (alt_rad.cos() * p.cos_lat);
        let mut az = cos_az_val.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 { az = 360.0 - az; }
        let Some((sx, sy)) = project(alt, az) else { continue };
        if sx < -40.0 || sx > p.wf + 40.0 || sy < -40.0 || sy > p.hf + 40.0 { continue; }

        let px_size = (dso.size_arcmin as f64 / 60.0 / (p.fov * 2.0) * scale * 2.0)
            .clamp(4.0, 40.0);
        let r = px_size / 2.0;

        // ── Try real image from Stellarium nebulae set ──────────────────────
        let drew_image = if let Some(tex) = nebulae_index.and_then(|idx| idx.map.get(&dso.name)) {
            // Lazy-load: insert HtmlImageElement on first encounter
            if !nebulae_cache.contains_key(&tex.path) {
                if let Ok(img) = HtmlImageElement::new() {
                    img.set_src(&format!("/{}", tex.path));
                    nebulae_cache.insert(tex.path.clone(), img);
                }
            }
            // Draw only when the image has finished loading
            if let Some(img) = nebulae_cache.get(&tex.path) {
                if img.complete() && img.natural_width() > 0 {
                    // Project all 4 corners with no distance cutoff so that
                    // large objects don't disappear when some corner is off-FOV.
                    // Canvas2D clips anything outside the canvas bounds.
                    let mut pts = [(0.0_f64, 0.0_f64); 4];
                    let mut ok = true;
                    for (i, &(cra, cdec)) in tex.corners.iter().enumerate() {
                        let corner_jnow =
                            J2000::new(cra as f64, cdec as f64).to_jnow(p.jd);
                        let (calt, caz) =
                            astro::eq_to_altaz(corner_jnow.ra_deg, corner_jnow.dec_deg, p.lst, p.latitude);
                        let (nx, ny) = astro::project_unclamped(
                            calt, caz, p.c_alt, p.c_az, p.fov,
                        );
                        let px = cx + nx * scale;
                        let py = cy - ny * scale;
                        // Reject absurdly far-off points (e.g. back-hemisphere)
                        if !px.is_finite()
                            || !py.is_finite()
                            || px.abs() > p.wf * 20.0
                            || py.abs() > p.hf * 20.0
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
        if !drew_image {
            match dso.kind {
                DsoType::Galaxy => {
                    let minor_px = if dso.size_minor_arcmin > 0.0 {
                        (dso.size_minor_arcmin as f64 / 60.0 / (p.fov * 2.0) * scale * 2.0)
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
                    ctx.ellipse(0.0, 0.0, r, minor_px, 0.0, 0.0, 2.0 * PI).unwrap();
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
                    ctx.move_to(sx - r, sy); ctx.line_to(sx + r, sy);
                    ctx.move_to(sx, sy - r); ctx.line_to(sx, sy + r);
                    ctx.stroke();
                }
                DsoType::PlanetaryNebula => {
                    ctx.set_stroke_style_str("rgba(0,230,180,0.85)");
                    ctx.begin_path();
                    ctx.arc(sx, sy, r, 0.0, 2.0 * PI).unwrap();
                    ctx.stroke();
                    let tick = r * 0.5;
                    ctx.begin_path();
                    ctx.move_to(sx - r - tick, sy); ctx.line_to(sx - r, sy);
                    ctx.move_to(sx + r, sy);         ctx.line_to(sx + r + tick, sy);
                    ctx.move_to(sx, sy - r - tick); ctx.line_to(sx, sy - r);
                    ctx.move_to(sx, sy + r);         ctx.line_to(sx, sy + r + tick);
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

        let label = dso.display_label();

        hit_items.push(HitItem {
            sx, sy,
            radius: r.max(8.0),
            kind: HitKind::Dso(dso.kind),
            name: label.clone(),
            mag: Some(dso.mag),
            ra_jnow_deg: dso_jnow.ra_deg,
            dec_jnow_deg: dso_jnow.dec_deg,
            size_arcmin: Some(dso.size_arcmin as f64),
            phase: None,
        });

        // Label (always, image or symbol)
        if p.names_on && p.fov < 50.0 {
            ctx.set_fill_style_str(match dso.kind {
                DsoType::Galaxy           => "rgba(0,200,220,0.85)",
                DsoType::OpenCluster      => "rgba(255,220,50,0.85)",
                DsoType::GlobularCluster  => "rgba(255,160,60,0.85)",
                DsoType::PlanetaryNebula  => "rgba(0,230,180,0.85)",
                DsoType::GalaxyCluster    => "rgba(220,100,220,0.85)",
                _                         => "rgba(60,220,100,0.85)",
            });
            ctx.set_text_align("left");
            let _ = ctx.fill_text(&label, sx + r + 3.0, sy + 4.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Mount crosshair
// ---------------------------------------------------------------------------

fn render_mount_crosshair(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    if !p.mount_connected { return; }
    if let (Some(ra_h), Some(dec)) = (p.mount_ra_h, p.mount_dec_deg) {
        let (malt, maz) = astro::eq_to_altaz(ra_h * 15.0, dec, p.lst, p.latitude);
        if let Some((mx, my)) = project(malt, maz) {
            let arm = 14.0;
            ctx.set_stroke_style_str("#44ff44");
            ctx.set_line_width(1.5);
            ctx.begin_path();
            ctx.move_to(mx - arm, my); ctx.line_to(mx - 4.0, my);
            ctx.move_to(mx + 4.0, my); ctx.line_to(mx + arm, my);
            ctx.move_to(mx, my - arm); ctx.line_to(mx, my - 4.0);
            ctx.move_to(mx, my + 4.0); ctx.line_to(mx, my + arm);
            ctx.stroke();

            ctx.set_fill_style_str("#ff4444");
            ctx.begin_path();
            let _ = ctx.arc(mx, my, 2.0, 0.0, 2.0 * PI);
            ctx.fill();
        }
    }
}

// ---------------------------------------------------------------------------
// Center crosshair
// ---------------------------------------------------------------------------

fn render_center_crosshair(ctx: &CanvasRenderingContext2d, cx: f64, cy: f64) {
    let arm = 18.0;
    let gap = 6.0;
    ctx.set_stroke_style_str("rgba(180,220,255,0.75)");
    ctx.set_line_width(1.0);
    ctx.begin_path();
    ctx.move_to(cx - arm, cy); ctx.line_to(cx - gap, cy);
    ctx.move_to(cx + gap, cy); ctx.line_to(cx + arm, cy);
    ctx.move_to(cx, cy - arm); ctx.line_to(cx, cy - gap);
    ctx.move_to(cx, cy + gap); ctx.line_to(cx, cy + arm);
    ctx.stroke();
    ctx.begin_path();
    let _ = ctx.arc(cx, cy, gap, 0.0, 2.0 * PI);
    ctx.stroke();
}

// ---------------------------------------------------------------------------
// Center FOV rectangle
// ---------------------------------------------------------------------------

fn render_center_fov(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    cx: f64,
    cy: f64,
) {
    let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) = (
        p.fl, p.cam_pixel_size_um, p.cam_sensor_width, p.cam_sensor_height,
    ) else { return };

    let fov_w = astro::fov_deg(fl_mm, sw as f64, px_um);
    let fov_h = astro::fov_deg(fl_mm, sh as f64, px_um);
    let half_w = fov_w / 2.0;
    let half_h = fov_h / 2.0;

    let (cra_deg, cdec_deg) = astro::altaz_to_eq(p.c_alt, p.c_az, p.lst, p.latitude);
    let cos_dec = cdec_deg.to_radians().cos().abs().max(0.01);
    let corners_eq = [
        (cra_deg - half_w / cos_dec, cdec_deg - half_h),
        (cra_deg + half_w / cos_dec, cdec_deg - half_h),
        (cra_deg + half_w / cos_dec, cdec_deg + half_h),
        (cra_deg - half_w / cos_dec, cdec_deg + half_h),
    ];

    let (pcx, pcy) = project(p.c_alt, p.c_az).unwrap_or((cx, cy));
    let rot_rad = p.rotation_deg.unwrap_or(0.0).to_radians();
    let sin_r = rot_rad.sin();
    let cos_r = rot_rad.cos();

    ctx.set_stroke_style_str("rgba(80,190,255,0.85)");
    ctx.set_line_width(1.0);
    ctx.begin_path();
    let mut first = true;
    for (cra, cdec) in &corners_eq {
        let (calt, caz) = astro::eq_to_altaz(*cra, *cdec, p.lst, p.latitude);
        if let Some((sx, sy)) = project(calt, caz) {
            let dx = sx - pcx;
            let dy = sy - pcy;
            let rx = pcx + dx * cos_r - dy * sin_r;
            let ry = pcy + dx * sin_r + dy * cos_r;
            if first { ctx.move_to(rx, ry); first = false; }
            else { ctx.line_to(rx, ry); }
        }
    }
    ctx.close_path();
    ctx.stroke();

    // Label below the rectangle
    let (lalt, laz) = astro::eq_to_altaz(cra_deg, cdec_deg - half_h - 0.5, p.lst, p.latitude);
    if let Some((lx, ly)) = project(lalt, laz) {
        let dx = lx - pcx;
        let dy = ly - pcy;
        let rlx = pcx + dx * cos_r - dy * sin_r;
        let rly = pcy + dx * sin_r + dy * cos_r;
        ctx.set_fill_style_str("rgba(80,190,255,0.85)");
        ctx.set_font("10px monospace");
        ctx.set_text_align("center");
        let _ = ctx.fill_text(
            &format!("{:.0}x{:.0}'", fov_w * 60.0, fov_h * 60.0),
            rlx, rly + 12.0,
        );
    }
}

// ---------------------------------------------------------------------------
// Mount FOV rectangle
// ---------------------------------------------------------------------------

fn render_mount_fov(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    cx: f64,
    cy: f64,
) {
    if !p.mount_connected { return; }
    let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) = (
        p.fl, p.cam_pixel_size_um, p.cam_sensor_width, p.cam_sensor_height,
    ) else { return };
    let (Some(ra_h), Some(dec)) = (p.mount_ra_h, p.mount_dec_deg) else { return };

    let fov_w = astro::fov_deg(fl_mm, sw as f64, px_um);
    let fov_h = astro::fov_deg(fl_mm, sh as f64, px_um);

    let ra_deg = ra_h * 15.0;
    let half_w = fov_w / 2.0;
    let half_h = fov_h / 2.0;
    let corners_eq = [
        (ra_deg - half_w / dec.to_radians().cos().abs().max(0.01), dec - half_h),
        (ra_deg + half_w / dec.to_radians().cos().abs().max(0.01), dec - half_h),
        (ra_deg + half_w / dec.to_radians().cos().abs().max(0.01), dec + half_h),
        (ra_deg - half_w / dec.to_radians().cos().abs().max(0.01), dec + half_h),
    ];

    let (mpcx, mpcy) = {
        let (malt, maz) = astro::eq_to_altaz(ra_deg, dec, p.lst, p.latitude);
        project(malt, maz).unwrap_or((cx, cy))
    };
    let rot_rad = p.rotation_deg.unwrap_or(0.0).to_radians();
    let sin_r = rot_rad.sin();
    let cos_r = rot_rad.cos();

    ctx.set_stroke_style_str("#ffcc00");
    ctx.set_line_width(1.0);
    ctx.begin_path();
    let mut first = true;
    for (cra, cdec) in &corners_eq {
        let (calt, caz) = astro::eq_to_altaz(*cra, *cdec, p.lst, p.latitude);
        if let Some((sx, sy)) = project(calt, caz) {
            let dx = sx - mpcx;
            let dy = sy - mpcy;
            let rx = mpcx + dx * cos_r - dy * sin_r;
            let ry = mpcy + dx * sin_r + dy * cos_r;
            if first { ctx.move_to(rx, ry); first = false; }
            else { ctx.line_to(rx, ry); }
        }
    }
    ctx.close_path();
    ctx.stroke();

    let (lalt, laz) = astro::eq_to_altaz(ra_deg, dec - half_h - 0.3, p.lst, p.latitude);
    if let Some((lx, ly)) = project(lalt, laz) {
        let dx = lx - mpcx;
        let dy = ly - mpcy;
        let rlx = mpcx + dx * cos_r - dy * sin_r;
        let rly = mpcy + dx * sin_r + dy * cos_r;
        ctx.set_fill_style_str("#ffcc00");
        ctx.set_font("10px monospace");
        ctx.set_text_align("center");
        let _ = ctx.fill_text(
            &format!("{:.0}x{:.0}'", fov_w * 60.0, fov_h * 60.0),
            rlx, rly + 12.0,
        );
    }
}

// ---------------------------------------------------------------------------
// Shared helper: project and draw one equatorial FOV rectangle.
// ---------------------------------------------------------------------------

fn draw_fov_box(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
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
        let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, p.lst, p.latitude);
        project(alt, az).unwrap_or((p.wf / 2.0, p.hf / 2.0))
    };
    let rot_rad = rot_deg.to_radians();
    let sin_r = rot_rad.sin();
    let cos_r = rot_rad.cos();

    let mut pts = [(0.0_f64, 0.0_f64); 4];
    let mut n = 0usize;
    for (cra, cdec) in &corners_eq {
        let (calt, caz) = astro::eq_to_altaz(*cra, *cdec, p.lst, p.latitude);
        if let Some((sx, sy)) = project(calt, caz) {
            let dx = sx - pcx;
            let dy = sy - pcy;
            pts[n] = (pcx + dx * cos_r - dy * sin_r, pcy + dx * sin_r + dy * cos_r);
            n += 1;
        }
    }
    if n < 4 { return; }

    // Filled background
    ctx.begin_path();
    ctx.move_to(pts[0].0, pts[0].1);
    for pt in &pts[1..] { ctx.line_to(pt.0, pt.1); }
    ctx.close_path();
    ctx.set_fill_style_str(fill_color);
    ctx.fill();

    // Border stroke
    ctx.set_stroke_style_str(stroke_color);
    ctx.set_line_width(line_width);
    ctx.begin_path();
    ctx.move_to(pts[0].0, pts[0].1);
    for pt in &pts[1..] { ctx.line_to(pt.0, pt.1); }
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

fn render_scheduler_jobs(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    if p.scheduler_jobs.is_empty() { return; }
    let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) = (
        p.fl, p.cam_pixel_size_um, p.cam_sensor_width, p.cam_sensor_height,
    ) else { return };

    let fov_w = astro::fov_deg(fl_mm, sw as f64, px_um);
    let fov_h = astro::fov_deg(fl_mm, sh as f64, px_um);

    for job in &p.scheduler_jobs {
        let (stroke, fill) = match job.state {
            0 => ("rgba(160,160,160,0.80)", "rgba(160,160,160,0.05)"),
            1 => ("rgba(200,200,80,0.80)",  "rgba(200,200,80,0.05)"),
            2 => ("rgba(80,200,255,0.85)",  "rgba(80,200,255,0.06)"),
            3 => ("rgba(80,220,80,0.90)",   "rgba(80,220,80,0.08)"),
            4 => ("rgba(220,80,80,0.90)",   "rgba(220,80,80,0.08)"),
            5 => ("rgba(220,140,80,0.80)",  "rgba(220,140,80,0.05)"),
            7 => ("rgba(200,200,200,0.50)", "rgba(200,200,200,0.03)"),
            _ => ("rgba(160,160,160,0.80)", "rgba(160,160,160,0.05)"),
        };
        draw_fov_box(ctx, p, project, job.ra_h * 15.0, job.dec_deg,
            fov_w, fov_h, p.rotation_deg.unwrap_or(0.0), stroke, fill, 1.0, Some(&job.name));
    }
}

// ---------------------------------------------------------------------------
// Mosaic tile grid rendering (shared for KStars mosaic and in-app planner)
// ---------------------------------------------------------------------------

fn render_mosaic_plan(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    plan: &MosaicPlanRender,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    is_preview: bool,
) {
    if plan.tiles.is_empty() { return; }

    let (tile_stroke, margin_stroke, fill_color, bbox_stroke) = if is_preview {
        ("rgba(80,180,255,0.85)", "rgba(80,180,255,0.35)", "rgba(80,180,255,0.06)", "rgba(80,180,255,0.60)")
    } else {
        ("rgba(255,180,60,0.85)", "rgba(255,180,60,0.35)", "rgba(255,180,60,0.06)", "rgba(255,220,100,0.60)")
    };

    for tile in &plan.tiles {
        let total_rot = tile.rotation + plan.pa_deg;
        draw_fov_box(ctx, p, project, tile.ra_deg, tile.dec_deg,
            plan.fov_w_deg, plan.fov_h_deg, total_rot,
            tile_stroke, fill_color, 1.5, None);

        // Overlap margin: inner inset box
        if plan.overlap_pct > 0.0 {
            let margin = plan.overlap_pct / 100.0;
            draw_fov_box(ctx, p, project, tile.ra_deg, tile.dec_deg,
                plan.fov_w_deg * (1.0 - margin), plan.fov_h_deg * (1.0 - margin),
                total_rot, margin_stroke, "rgba(0,0,0,0)", 0.8, None);
        }
    }

    // Mosaic bounding box + target label
    if plan.tiles.len() > 1 || !plan.target_name.is_empty() {
        let min_ra  = plan.tiles.iter().map(|t| t.ra_deg).fold(f64::INFINITY, f64::min);
        let max_ra  = plan.tiles.iter().map(|t| t.ra_deg).fold(f64::NEG_INFINITY, f64::max);
        let min_dec = plan.tiles.iter().map(|t| t.dec_deg).fold(f64::INFINITY, f64::min);
        let max_dec = plan.tiles.iter().map(|t| t.dec_deg).fold(f64::NEG_INFINITY, f64::max);
        let center_ra  = (min_ra + max_ra) / 2.0;
        let center_dec = (min_dec + max_dec) / 2.0;
        let bbox_w = (max_ra - min_ra) + plan.fov_w_deg;
        let bbox_h = (max_dec - min_dec) + plan.fov_h_deg;
        let lbl = if plan.target_name.is_empty() { None } else { Some(plan.target_name.as_str()) };
        draw_fov_box(ctx, p, project, center_ra, center_dec,
            bbox_w, bbox_h, plan.pa_deg,
            bbox_stroke, "rgba(0,0,0,0)", 2.0, lbl);
    }
}

// ---------------------------------------------------------------------------
// Info overlay (bottom-left)
// ---------------------------------------------------------------------------

fn render_info_overlay(ctx: &CanvasRenderingContext2d, p: &RenderParams) {
    ctx.set_fill_style_str("rgba(10,10,20,0.75)");
    ctx.fill_rect(0.0, p.hf - 128.0, 360.0, 128.0);
    ctx.set_fill_style_str("#aabbcc");
    ctx.set_font("11px monospace");
    ctx.set_text_align("left");

    let tr = t(p.cur_lang);
    let lst_h = p.lst / 15.0;
    let lst_hh = lst_h as u32;
    let lst_mm = ((lst_h - lst_hh as f64) * 60.0) as u32;
    let _ = ctx.fill_text(
        &format!("{}: {:02}h{:02}m  {}: {:.0}\u{00b0}", tr.overlay_lst, lst_hh, lst_mm, tr.overlay_fov, p.fov),
        8.0, p.hf - 108.0,
    );
    let _ = ctx.fill_text(
        &format!("{}: {} {:.1}\u{00b0}  {} {:.1}\u{00b0}", tr.overlay_center, tr.overlay_alt, p.c_alt, tr.overlay_az, p.c_az),
        8.0, p.hf - 92.0,
    );
    if let (Some(ra_h), Some(dec)) = (p.mount_ra_h, p.mount_dec_deg) {
        let rah = ra_h as u32;
        let ram = ((ra_h - rah as f64) * 60.0) as u32;
        let _ = ctx.fill_text(
            &format!("{}: {:02}h{:02}m  {:+.1}\u{00b0}", tr.overlay_mount, rah, ram, dec),
            8.0, p.hf - 76.0,
        );
    } else {
        let _ = ctx.fill_text(tr.overlay_mount_none, 8.0, p.hf - 76.0);
    }
    if let Some(rot) = p.rotation_deg {
        let _ = ctx.fill_text(
            &format!("{}: {:.1}\u{00b0}", tr.overlay_camera_angle, rot),
            8.0, p.hf - 60.0,
        );
    }
    if p.t_off.abs() > 0.5 {
        let _ = ctx.fill_text(
            &format!("{}: {:+.0}s", tr.overlay_time_offset, p.t_off),
            8.0, p.hf - 44.0,
        );
    }
    if let (Some((alt, az)), Some((ra, dec))) = (p.cursor_altaz, p.cursor_radec) {
        let ra_h = ra / 15.0;
        let rah = ra_h as u32;
        let ram = ((ra_h - rah as f64) * 60.0) as u32;
        let _ = ctx.fill_text(
            &format!(
                "{}: {} {:+.1}\u{00b0} {} {:.1}\u{00b0}  {:02}h{:02}m {:+.1}\u{00b0}",
                tr.overlay_cursor, tr.overlay_alt, alt, tr.overlay_az, az, rah, ram, dec,
            ),
            8.0, p.hf - 28.0,
        );
    }
}

// ---------------------------------------------------------------------------
// Ecliptic + zenith
// ---------------------------------------------------------------------------

fn render_ecliptic(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    // Mean obliquity of date (arcsec → deg is implicit in the constant).
    let t = (p.jd - 2_451_545.0) / 36525.0;
    let ecl_deg = 23.4393 - 0.0130042 * t; // sufficient for our purposes

    let sin_eps = ecl_deg.to_radians().sin();
    let cos_eps = ecl_deg.to_radians().cos();

    ctx.set_stroke_style_str("rgba(220,180,80,0.7)");
    ctx.set_line_width(1.2);
    ctx.set_line_dash(&js_sys::Array::of2(&5.0_f64.into(), &4.0_f64.into())).unwrap();
    ctx.begin_path();
    let mut first = true;
    for deg_i in (0..=360).step_by(2) {
        let lam = (deg_i as f64).to_radians();
        // Ecliptic (lon, lat=0) → equatorial via the standard rotation.
        let x = lam.cos();
        let y = lam.sin() * cos_eps;
        let z = lam.sin() * sin_eps;
        let ra  = y.atan2(x).to_degrees().rem_euclid(360.0);
        let dec = z.asin().to_degrees();
        let (alt, az) = astro::eq_to_altaz(ra, dec, p.lst, p.latitude);
        if let Some((sx, sy)) = project(alt, az) {
            if first { ctx.move_to(sx, sy); first = false; }
            else { ctx.line_to(sx, sy); }
        } else {
            first = true;
        }
    }
    ctx.stroke();
    ctx.set_line_dash(&js_sys::Array::new()).unwrap();
}

fn render_zenith(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    if let Some((sx, sy)) = project(90.0, 0.0) {
        ctx.set_stroke_style_str("rgba(180,220,255,0.85)");
        ctx.set_line_width(1.2);
        ctx.begin_path();
        let _ = ctx.arc(sx, sy, 6.0, 0.0, 2.0 * PI);
        ctx.stroke();
        ctx.set_fill_style_str("rgba(180,220,255,0.85)");
        ctx.set_font("bold 10px monospace");
        ctx.set_text_align("left");
        let _ = ctx.fill_text(t(p.cur_lang).zenith_mark, sx + 9.0, sy + 4.0);
    }
}

// ---------------------------------------------------------------------------
// Slew trail — fading polyline of recent mount positions.
// ---------------------------------------------------------------------------

fn render_slew_trail(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    trail: &[(f64, f64, f64)],
) {
    if trail.is_empty() { return; }
    let now_jd = p.jd;
    // Fade over 60s of wall-clock; older samples vanish.
    const FADE_DAYS: f64 = 60.0 / 86400.0;

    ctx.set_line_width(1.5);
    ctx.set_line_cap("round");
    // Draw each segment with its own alpha so the tail fades.
    for w in trail.windows(2) {
        let (jd0, ra0, de0) = w[0];
        let (_jd1, ra1, de1) = w[1];
        let age = (now_jd - jd0).max(0.0);
        let alpha = (1.0 - (age / FADE_DAYS).min(1.0)) * 0.9;
        if alpha < 0.05 { continue; }
        let (a0, az0) = astro::eq_to_altaz(ra0, de0, p.lst, p.latitude);
        let (a1, az1) = astro::eq_to_altaz(ra1, de1, p.lst, p.latitude);
        let (Some(s0), Some(s1)) = (project(a0, az0), project(a1, az1)) else { continue };
        ctx.set_stroke_style_str(&format!("rgba(255,170,60,{:.3})", alpha));
        ctx.begin_path();
        ctx.move_to(s0.0, s0.1);
        ctx.line_to(s1.0, s1.1);
        ctx.stroke();
    }
}

// ---------------------------------------------------------------------------
// Plate-solve result marker
// ---------------------------------------------------------------------------

fn render_solve_marker(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) {
    let (Some(ra), Some(dec)) = (p.solve_ra_jnow_deg, p.solve_dec_jnow_deg)
        else { return };

    // Fade after 10 minutes so stale solves aren't visually shouting.
    let alpha = p.solve_age_ms
        .map(|age| (1.0 - (age / 600_000.0).min(1.0)) * 0.95 + 0.05)
        .unwrap_or(1.0);
    if alpha < 0.1 { return; }

    let (alt, az) = astro::eq_to_altaz(ra, dec, p.lst, p.latitude);
    let Some((sx, sy)) = project(alt, az) else { return };

    let green = format!("rgba(60,230,120,{:.3})", alpha);
    ctx.set_stroke_style_str(&green);
    ctx.set_line_width(1.2);
    ctx.begin_path();
    let _ = ctx.arc(sx, sy, 10.0, 0.0, 2.0 * PI);
    ctx.stroke();
    // Small gap crosshair
    ctx.begin_path();
    ctx.move_to(sx - 16.0, sy); ctx.line_to(sx - 4.0, sy);
    ctx.move_to(sx + 4.0, sy);  ctx.line_to(sx + 16.0, sy);
    ctx.move_to(sx, sy - 16.0); ctx.line_to(sx, sy - 4.0);
    ctx.move_to(sx, sy + 4.0);  ctx.line_to(sx, sy + 16.0);
    ctx.stroke();

    // Draw a translucent FOV rectangle using pixscale × camera sensor (if known).
    if let (Some(pix), Some(sw), Some(sh)) =
        (p.solve_pixscale_arcsec, p.cam_sensor_width, p.cam_sensor_height)
    {
        let fov_w = pix * sw as f64 / 3600.0; // deg
        let fov_h = pix * sh as f64 / 3600.0;
        let half_w = fov_w / 2.0;
        let half_h = fov_h / 2.0;
        let cos_dec = dec.to_radians().cos().abs().max(0.01);
        let corners_eq = [
            (ra - half_w / cos_dec, dec - half_h),
            (ra + half_w / cos_dec, dec - half_h),
            (ra + half_w / cos_dec, dec + half_h),
            (ra - half_w / cos_dec, dec + half_h),
        ];
        let rot_rad = p.rotation_deg.unwrap_or(0.0).to_radians();
        let sin_r = rot_rad.sin();
        let cos_r = rot_rad.cos();
        ctx.set_line_width(1.0);
        ctx.begin_path();
        let mut first = true;
        for (cra, cdec) in &corners_eq {
            let (calt, caz) = astro::eq_to_altaz(*cra, *cdec, p.lst, p.latitude);
            if let Some((px, py)) = project(calt, caz) {
                let dx = px - sx;
                let dy = py - sy;
                let rx = sx + dx * cos_r - dy * sin_r;
                let ry = sy + dx * sin_r + dy * cos_r;
                if first { ctx.move_to(rx, ry); first = false; }
                else { ctx.line_to(rx, ry); }
            }
        }
        ctx.close_path();
        ctx.stroke();
    }

    ctx.set_fill_style_str(&green);
    ctx.set_font("10px monospace");
    ctx.set_text_align("center");
    let _ = ctx.fill_text(t(p.cur_lang).solved_mark, sx, sy - 18.0);
}

// ---------------------------------------------------------------------------
// Solar system — Sun, Moon, planets via the ephemeris module.
// ---------------------------------------------------------------------------

fn render_solar_system(
    ctx: &CanvasRenderingContext2d,
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    hit_items: &mut Vec<HitItem>,
) {
    // ── Sun ──────────────────────────────────────────────────────────
    let sun = ephemeris::sun(p.jd);
    if let Some((sx, sy)) = altaz_project(p, project, sun.jnow.ra_deg, sun.jnow.dec_deg) {
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
        if p.names_on && p.fov < 60.0 {
            ctx.set_fill_style_str("rgba(255,220,80,0.95)");
            ctx.set_font("11px monospace");
            ctx.set_text_align("left");
            let _ = ctx.fill_text(t(p.cur_lang).body_sun, sx + r + 4.0, sy + 4.0);
        }
        hit_items.push(HitItem {
            sx, sy,
            radius: r + 4.0,
            kind: HitKind::Sun,
            name: t(p.cur_lang).body_sun.to_string(),
            mag: Some(sun.mag),
            ra_jnow_deg: sun.jnow.ra_deg,
            dec_jnow_deg: sun.jnow.dec_deg,
            size_arcmin: sun.angular_diameter_arcmin,
            phase: None,
        });
    }

    // ── Moon ─────────────────────────────────────────────────────────
    let moon = ephemeris::moon(p.jd);
    if let Some((sx, sy)) = altaz_project(p, project, moon.jnow.ra_deg, moon.jnow.dec_deg) {
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

        if p.names_on && p.fov < 60.0 {
            ctx.set_fill_style_str("rgba(220,220,230,0.95)");
            ctx.set_font("11px monospace");
            ctx.set_text_align("left");
            let _ = ctx.fill_text(t(p.cur_lang).body_moon, sx + r + 4.0, sy + 4.0);
        }
        hit_items.push(HitItem {
            sx, sy,
            radius: r + 4.0,
            kind: HitKind::Moon,
            name: t(p.cur_lang).body_moon.to_string(),
            mag: Some(moon.mag),
            ra_jnow_deg: moon.jnow.ra_deg,
            dec_jnow_deg: moon.jnow.dec_deg,
            size_arcmin: moon.angular_diameter_arcmin,
            phase: moon.phase,
        });
    }

    // ── Planets ──────────────────────────────────────────────────────
    let planets = ephemeris::all_planets(p.jd);
    for (planet, pos) in &planets {
        let Some((sx, sy)) = altaz_project(p, project, pos.jnow.ra_deg, pos.jnow.dec_deg)
            else { continue };
        let (color, r) = planet_style(*planet, pos.mag);
        ctx.set_fill_style_str(color);
        ctx.begin_path();
        let _ = ctx.arc(sx, sy, r, 0.0, 2.0 * PI);
        ctx.fill();
        if p.names_on && p.fov < 40.0 {
            ctx.set_fill_style_str(color);
            ctx.set_font("10px monospace");
            ctx.set_text_align("left");
            let _ = ctx.fill_text(planet.name_i18n(p.cur_lang), sx + r + 3.0, sy + 4.0);
        }
        hit_items.push(HitItem {
            sx, sy,
            radius: (r + 3.0).max(8.0),
            kind: HitKind::Planet,
            name: planet.name_i18n(p.cur_lang).to_string(),
            mag: Some(pos.mag),
            ra_jnow_deg: pos.jnow.ra_deg,
            dec_jnow_deg: pos.jnow.dec_deg,
            size_arcmin: None,
            phase: pos.phase,
        });
    }
}

fn altaz_project(
    p: &RenderParams,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    ra_deg: f64,
    dec_deg: f64,
) -> Option<(f64, f64)> {
    let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, p.lst, p.latitude);
    if alt < -3.0 { return None; }
    project(alt, az)
}

fn planet_style(p: ephemeris::Planet, mag: f32) -> (&'static str, f64) {
    // Size scales with magnitude (brighter = bigger).
    let r = ((4.0 - mag as f64).clamp(2.5, 6.0)).max(2.5);
    let color = match p {
        ephemeris::Planet::Mercury => "rgba(200,200,180,0.95)",
        ephemeris::Planet::Venus   => "rgba(240,240,200,0.98)",
        ephemeris::Planet::Mars    => "rgba(240,120,80,0.95)",
        ephemeris::Planet::Jupiter => "rgba(240,210,160,0.95)",
        ephemeris::Planet::Saturn  => "rgba(220,200,140,0.95)",
        ephemeris::Planet::Uranus  => "rgba(160,220,230,0.9)",
        ephemeris::Planet::Neptune => "rgba(120,160,240,0.9)",
    };
    (color, r)
}
