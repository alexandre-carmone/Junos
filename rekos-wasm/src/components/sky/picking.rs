//! CPU-side hit-test list builder.
//!
//! Mirrors the projection / culling math used by the render layers, but
//! does not draw — it only emits `HitItem`s that the right-click menu and
//! info popup walk to find the nearest object under the cursor.
//!
//! Until this module landed, hit items were appended inside the Canvas2D
//! `render.rs` functions, which meant turning off Canvas2D rendering also
//! lost click targets. With picking now standalone, the GPU symbol path
//! can run alone and the Canvas2D fallback can shrink to non-clickable
//! decorations (nebula textures only) without breaking interaction.

use std::sync::Arc;

use crate::astro;
use crate::catalog::CatalogData;
use crate::coords::{J2000, JNow};
use crate::dso_catalog::{DsoCatalogData, DsoType};
use crate::ephemeris;
use crate::i18n::Lang;

use super::dso_index::DsoIndex;
use super::dso_render::KindFilter;
use super::gpu::LineView;
use super::render::{HitItem, HitKind};

#[derive(Copy, Clone)]
pub struct PickParams<'a> {
    pub view:       &'a LineView,
    pub catalog:    Option<&'a Arc<CatalogData>>,
    pub dso_cat:    Option<&'a Arc<DsoCatalogData>>,
    pub dso_index:  Option<&'a DsoIndex>,
    pub mag_limit:  f32,
    pub stars_on:   bool,
    pub dso_on:     bool,
    pub dso_filter: KindFilter,
    pub dso_mag:    f64,
    pub solar_on:   bool,
    pub lang:       Lang,
}

/// Append hit-test items for stars (named only), DSOs, sun, moon, planets.
pub fn build(p: PickParams<'_>, out: &mut Vec<HitItem>) {
    if p.stars_on {
        if let Some(cat) = p.catalog {
            push_named_stars(p.view, cat, p.mag_limit, out);
        }
    }
    if p.dso_on {
        if let Some(dc) = p.dso_cat {
            push_dsos(p.view, dc, p.dso_index, p.dso_filter, p.dso_mag, p.lang, out);
        }
    }
    if p.solar_on {
        push_solar_system(p.view, p.lang, out);
    }
}

fn push_named_stars(
    v: &LineView,
    cat: &Arc<CatalogData>,
    mag_limit: f32,
    out: &mut Vec<HitItem>,
) {
    let lst_rad = v.lst.to_radians();
    let sin_lat = v.latitude.to_radians().sin();
    let cos_lat = v.latitude.to_radians().cos();
    for star in cat.stars.iter() {
        let Some(name) = star.name.as_deref() else { continue };
        if name == "Sol" { continue; } // duplicate of ephemeris Sun
        if star.mag > mag_limit { continue; }
        let jnow = J2000::new(star.ra_deg as f64, star.dec_deg as f64).to_jnow(v.jd);
        let ha = lst_rad - jnow.ra_deg.to_radians();
        let dec = jnow.dec_deg.to_radians();
        let sin_dec = dec.sin();
        let cos_dec = dec.cos();
        let sin_alt = sin_dec * sin_lat + cos_dec * cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -5.0 { continue; }
        let cos_az = (sin_dec - alt_rad.sin() * sin_lat) / (alt_rad.cos() * cos_lat);
        let mut az = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 { az = 360.0 - az; }
        let Some((sx, sy)) = v.project(alt, az) else { continue };
        out.push(HitItem {
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

fn push_dsos(
    v: &LineView,
    dso_cat: &Arc<DsoCatalogData>,
    dso_index: Option<&DsoIndex>,
    filter: KindFilter,
    mag_limit: f64,
    lang: Lang,
    out: &mut Vec<HitItem>,
) {
    let scale = v.scale();
    let lst_rad = v.lst.to_radians();
    let sin_lat = v.latitude.to_radians().sin();
    let cos_lat = v.latitude.to_radians().cos();

    let (c_ra_jnow, c_dec_jnow) = astro::altaz_to_eq(v.c_alt, v.c_az, v.lst, v.latitude);
    let view_j2000 = JNow::new(c_ra_jnow, c_dec_jnow).to_j2000(v.jd);
    let v_ra_rad = view_j2000.ra_deg.to_radians();
    let v_dec_rad = view_j2000.dec_deg.to_radians();
    let v_sin_dec = v_dec_rad.sin();
    let v_cos_dec = v_dec_rad.cos();
    let cap_radius_deg = v.fov * 1.5 + 6.0;
    let cos_cap = if cap_radius_deg >= 180.0 { -1.0 } else { cap_radius_deg.to_radians().cos() };

    let visible: Option<Vec<u32>> = dso_index.map(|idx| {
        idx.visible_indices(view_j2000.ra_deg, view_j2000.dec_deg, cap_radius_deg)
    });
    let dsos = &dso_cat.dsos;
    let iter: Box<dyn Iterator<Item = usize>> = match &visible {
        Some(v) => Box::new(v.iter().map(|i| *i as usize)),
        None    => Box::new(0..dsos.len()),
    };

    for di in iter {
        let Some(dso) = dsos.get(di) else { continue };
        let allowed = match dso.kind {
            DsoType::Galaxy           => filter.gx,
            DsoType::OpenCluster      => filter.oc,
            DsoType::GlobularCluster  => filter.gc,
            DsoType::Nebula           => filter.nb,
            DsoType::PlanetaryNebula  => filter.pn,
            DsoType::SupernovaRemnant => filter.snr,
            DsoType::GalaxyCluster    => filter.gal,
        };
        if !allowed { continue; }
        if (dso.mag as f64) > mag_limit { continue; }

        let d_ra_rad = (dso.ra_deg as f64).to_radians();
        let d_dec_rad = (dso.dec_deg as f64).to_radians();
        let cos_sep = v_sin_dec * d_dec_rad.sin()
            + v_cos_dec * d_dec_rad.cos() * (d_ra_rad - v_ra_rad).cos();
        if cos_sep < cos_cap { continue; }

        let dso_jnow = J2000::new(dso.ra_deg as f64, dso.dec_deg as f64).to_jnow(v.jd);
        let ha = lst_rad - dso_jnow.ra_deg.to_radians();
        let dec_rad = dso_jnow.dec_deg.to_radians();
        let sin_dec = dec_rad.sin();
        let cos_dec = dec_rad.cos();
        let sin_alt = sin_dec * sin_lat + cos_dec * cos_lat * ha.cos();
        let alt_rad = sin_alt.asin();
        let alt = alt_rad.to_degrees();
        if alt < -3.0 { continue; }
        let cos_az_val = (sin_dec - alt_rad.sin() * sin_lat) / (alt_rad.cos() * cos_lat);
        let mut az = cos_az_val.clamp(-1.0, 1.0).acos().to_degrees();
        if ha.sin() > 0.0 { az = 360.0 - az; }
        let Some((sx, sy)) = v.project(alt, az) else { continue };
        if sx < -40.0 || sx > v.wf + 40.0 || sy < -40.0 || sy > v.hf + 40.0 { continue; }

        let px_size = (dso.size_arcmin as f64 / 60.0 / (v.fov * 2.0) * scale * 2.0)
            .clamp(4.0, 40.0);
        let r = px_size / 2.0;
        out.push(HitItem {
            sx, sy,
            radius: r.max(8.0),
            kind: HitKind::Dso(dso.kind),
            name: dso.display_label(lang),
            mag: Some(dso.mag),
            ra_jnow_deg: dso_jnow.ra_deg,
            dec_jnow_deg: dso_jnow.dec_deg,
            size_arcmin: Some(dso.size_arcmin as f64),
            phase: None,
        });
    }
}

fn push_solar_system(v: &LineView, lang: Lang, out: &mut Vec<HitItem>) {
    let tr = crate::i18n::t(lang);

    let sun = ephemeris::sun(v.jd);
    if let Some((sx, sy)) = altaz_project(v, sun.jnow.ra_deg, sun.jnow.dec_deg) {
        out.push(HitItem {
            sx, sy,
            radius: 14.0,
            kind: HitKind::Sun,
            name: tr.body_sun.to_string(),
            mag: Some(sun.mag),
            ra_jnow_deg: sun.jnow.ra_deg,
            dec_jnow_deg: sun.jnow.dec_deg,
            size_arcmin: sun.angular_diameter_arcmin,
            phase: None,
        });
    }

    let moon = ephemeris::moon(v.jd);
    if let Some((sx, sy)) = altaz_project(v, moon.jnow.ra_deg, moon.jnow.dec_deg) {
        out.push(HitItem {
            sx, sy,
            radius: 13.0,
            kind: HitKind::Moon,
            name: tr.body_moon.to_string(),
            mag: Some(moon.mag),
            ra_jnow_deg: moon.jnow.ra_deg,
            dec_jnow_deg: moon.jnow.dec_deg,
            size_arcmin: moon.angular_diameter_arcmin,
            phase: moon.phase,
        });
    }

    for (planet, pos) in &ephemeris::all_planets(v.jd) {
        let Some((sx, sy)) = altaz_project(v, pos.jnow.ra_deg, pos.jnow.dec_deg)
            else { continue };
        out.push(HitItem {
            sx, sy,
            radius: 10.0,
            kind: HitKind::Planet,
            name: planet.name_i18n(lang).to_string(),
            mag: Some(pos.mag),
            ra_jnow_deg: pos.jnow.ra_deg,
            dec_jnow_deg: pos.jnow.dec_deg,
            size_arcmin: None,
            phase: pos.phase,
        });
    }
}

fn altaz_project(v: &LineView, ra_deg: f64, dec_deg: f64) -> Option<(f64, f64)> {
    let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, v.lst, v.latitude);
    if alt < -3.0 { return None; }
    v.project(alt, az)
}
