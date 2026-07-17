//! CPU-side DSO instance + label builder.
//!
//! Mirrors the projection / culling logic in `render::render_dso` but emits
//! GPU-ready instances and text items instead of issuing Canvas2D draws.
//! Symbol geometry is shared with the Canvas2D path via `dso_shape`. Hit
//! tests still flow through `picking`; this module only covers the symbol +
//! label layers.

use std::sync::Arc;

use crate::astro;
use crate::coords::{J2000, JNow};
use crate::dso_catalog::{DsoCatalogData, DsoType};
use crate::i18n::Lang;

use super::dso_index::DsoIndex;
use super::dso_shape::dso_shape;
use super::gpu::layers::dso::DsoInstance;
use super::gpu::text::{FontAtlas, TextInstance};
use super::gpu::LineView;

#[derive(Copy, Clone)]
pub struct DsoBuildParams<'a> {
    pub view:        &'a LineView,
    pub dso_cat:     &'a Arc<DsoCatalogData>,
    pub dso_index:   Option<&'a DsoIndex>,
    pub mag_limit:   f64,
    pub names_on:    bool,
    pub is_mobile:   bool,
    pub kind_filter: KindFilter,
    pub lang:        Lang,
}

#[derive(Copy, Clone)]
pub struct KindFilter {
    pub gx:  bool,
    pub oc:  bool,
    pub gc:  bool,
    pub nb:  bool,
    pub pn:  bool,
    pub snr: bool,
    pub gal: bool,
}

impl KindFilter {
    fn allows(&self, kind: DsoType) -> bool {
        match kind {
            DsoType::Galaxy           => self.gx,
            DsoType::OpenCluster      => self.oc,
            DsoType::GlobularCluster  => self.gc,
            DsoType::Nebula           => self.nb,
            DsoType::PlanetaryNebula  => self.pn,
            DsoType::SupernovaRemnant => self.snr,
            DsoType::GalaxyCluster    => self.gal,
        }
    }
}

fn kind_to_u32(k: DsoType) -> u32 {
    match k {
        DsoType::Galaxy           => 0,
        DsoType::OpenCluster      => 1,
        DsoType::GlobularCluster  => 2,
        DsoType::Nebula           => 3,
        DsoType::PlanetaryNebula  => 4,
        DsoType::SupernovaRemnant => 5,
        DsoType::GalaxyCluster    => 6,
    }
}

fn kind_color(k: DsoType) -> [f32; 4] {
    match k {
        DsoType::Galaxy           => [0.0, 200.0/255.0, 220.0/255.0, 0.85],
        DsoType::OpenCluster      => [1.0, 220.0/255.0,  50.0/255.0, 0.85],
        DsoType::GlobularCluster  => [1.0, 160.0/255.0,  60.0/255.0, 0.85],
        DsoType::PlanetaryNebula  => [0.0, 230.0/255.0, 180.0/255.0, 0.90],
        DsoType::GalaxyCluster    => [220.0/255.0, 100.0/255.0, 220.0/255.0, 0.80],
        DsoType::Nebula | DsoType::SupernovaRemnant
                                  => [60.0/255.0, 220.0/255.0, 100.0/255.0, 0.80],
    }
}

/// Build DSO symbol instances + labels. Both vectors are appended-to.
pub fn build(
    p: DsoBuildParams<'_>,
    atlas: Option<&FontAtlas>,
    out_dso:   &mut Vec<DsoInstance>,
    out_text:  &mut Vec<TextInstance>,
) {
    let v = p.view;
    let scale = v.scale();
    let lst_rad = v.lst.to_radians();

    // ── Cheap angular-distance pre-cull (J2000) ───────────────────────────
    let (c_ra_jnow, c_dec_jnow) = astro::altaz_to_eq(v.c_alt, v.c_az, v.lst, v.latitude);
    let view_j2000 = JNow::new(c_ra_jnow, c_dec_jnow).to_j2000(v.jd);
    let v_ra_rad = view_j2000.ra_deg.to_radians();
    let v_dec_rad = view_j2000.dec_deg.to_radians();
    let v_sin_dec = v_dec_rad.sin();
    let v_cos_dec = v_dec_rad.cos();
    let cap_radius_deg = v.fov * 1.5 + 6.0;
    let cos_cap = if cap_radius_deg >= 180.0 { -1.0 } else { cap_radius_deg.to_radians().cos() };

    let visible: Option<Vec<u32>> = p.dso_index.map(|idx| {
        idx.visible_indices(view_j2000.ra_deg, view_j2000.dec_deg, cap_radius_deg)
    });
    let dsos = &p.dso_cat.dsos;
    let iter_indices: Box<dyn Iterator<Item = usize>> = match &visible {
        Some(v) => Box::new(v.iter().map(|i| *i as usize)),
        None    => Box::new(0..dsos.len()),
    };

    let sin_lat = v.latitude.to_radians().sin();
    let cos_lat = v.latitude.to_radians().cos();

    for di in iter_indices {
        let Some(dso) = dsos.get(di) else { continue };
        if !p.kind_filter.allows(dso.kind) { continue; }
        if (dso.mag as f64) > p.mag_limit { continue; }

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

        let project = |alt: f64, az: f64| v.project(alt, az);
        let shape = dso_shape(
            dso,
            sx,
            sy,
            dso_jnow.ra_deg,
            dso_jnow.dec_deg,
            v.lst,
            v.latitude,
            v.fov,
            scale,
            &project,
        );

        let color = kind_color(dso.kind);
        out_dso.push(DsoInstance {
            pos_x: sx as f32,
            pos_y: sy as f32,
            half_w: shape.half_w as f32,
            half_h: shape.half_h as f32,
            cos_rot: shape.cos_rot as f32,
            sin_rot: shape.sin_rot as f32,
            kind: kind_to_u32(dso.kind),
            _pad0: 0,
            color_r: color[0],
            color_g: color[1],
            color_b: color[2],
            color_a: color[3],
        });

        // Label. Mobile gates and FOV gates match render_dso.
        let label_fov_gate = if p.is_mobile { 25.0 } else { 50.0 };
        let label_mag_ok = !p.is_mobile || (dso.mag as f64) <= p.mag_limit - 1.5;
        if p.names_on && v.fov < label_fov_gate && label_mag_ok {
            if let Some(atlas) = atlas {
                let label = dso.display_label(p.lang);
                // Cap the offset: large objects draw at true extent, and a
                // label pushed out to their edge reads as unattached.
                let off = shape.half_w.min(40.0) as f32;
                atlas.push_text(
                    out_text,
                    &label,
                    sx as f32 + off + 3.0,
                    sy as f32 - 5.0,
                    12.0,
                    color,
                );
            }
        }
    }
}
