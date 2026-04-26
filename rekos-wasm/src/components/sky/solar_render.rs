//! CPU builder for sun / moon / planets — emits filled-disc DSO instances
//! and labels through `DsoLayer` + `TextLayer`. Moon phase shading is
//! intentionally lost in this first GPU port; restoring it requires
//! extending `DsoInstance` with an auxiliary float and a per-kind shader
//! branch (deferred).

use crate::astro;
use crate::ephemeris;
use crate::i18n::Lang;

use super::gpu::layers::dso::DsoInstance;
use super::gpu::text::{FontAtlas, TextInstance};
use super::gpu::LineView;

const KIND_FILLED_DISC: u32 = 7;

fn project(v: &LineView, ra_deg: f64, dec_deg: f64) -> Option<(f64, f64)> {
    let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, v.lst, v.latitude);
    if alt < -3.0 { return None; }
    v.project(alt, az)
}

fn push_disc(
    out: &mut Vec<DsoInstance>,
    sx: f64,
    sy: f64,
    radius_px: f64,
    rgba: [f32; 4],
) {
    out.push(DsoInstance {
        pos_x: sx as f32, pos_y: sy as f32,
        half_w: radius_px as f32, half_h: radius_px as f32,
        cos_rot: 1.0, sin_rot: 0.0,
        kind: KIND_FILLED_DISC,
        _pad0: 0,
        color_r: rgba[0], color_g: rgba[1],
        color_b: rgba[2], color_a: rgba[3],
    });
}

fn planet_color(p: ephemeris::Planet) -> [f32; 4] {
    match p {
        ephemeris::Planet::Mercury => [200.0/255.0, 200.0/255.0, 180.0/255.0, 0.95],
        ephemeris::Planet::Venus   => [240.0/255.0, 240.0/255.0, 200.0/255.0, 0.98],
        ephemeris::Planet::Mars    => [240.0/255.0, 120.0/255.0,  80.0/255.0, 0.95],
        ephemeris::Planet::Jupiter => [240.0/255.0, 210.0/255.0, 160.0/255.0, 0.95],
        ephemeris::Planet::Saturn  => [220.0/255.0, 200.0/255.0, 140.0/255.0, 0.95],
        ephemeris::Planet::Uranus  => [160.0/255.0, 220.0/255.0, 230.0/255.0, 0.90],
        ephemeris::Planet::Neptune => [120.0/255.0, 160.0/255.0, 240.0/255.0, 0.90],
    }
}

pub fn build(
    v: &LineView,
    names_on: bool,
    lang: Lang,
    atlas: Option<&FontAtlas>,
    out_dso:  &mut Vec<DsoInstance>,
    out_text: &mut Vec<TextInstance>,
) {
    let tr = crate::i18n::t(lang);

    // Sun — yellow disc with subtle orange halo.
    let sun = ephemeris::sun(v.jd);
    if let Some((sx, sy)) = project(v, sun.jnow.ra_deg, sun.jnow.dec_deg) {
        push_disc(out_dso, sx, sy, 12.0,
                  [1.0, 160.0/255.0, 40.0/255.0, 0.55]);
        push_disc(out_dso, sx, sy, 10.0,
                  [1.0, 220.0/255.0, 80.0/255.0, 0.95]);
        if names_on && v.fov < 60.0 {
            if let Some(atlas) = atlas {
                atlas.push_text(out_text, tr.body_sun,
                    sx as f32 + 14.0, sy as f32 - 5.0, 14.0,
                    [1.0, 220.0/255.0, 80.0/255.0, 0.95]);
            }
        }
    }

    // Moon — flat disc; phase shading is dropped in this first GPU port.
    let moon = ephemeris::moon(v.jd);
    if let Some((sx, sy)) = project(v, moon.jnow.ra_deg, moon.jnow.dec_deg) {
        push_disc(out_dso, sx, sy, 9.0,
                  [220.0/255.0, 220.0/255.0, 230.0/255.0, 0.95]);
        if names_on && v.fov < 60.0 {
            if let Some(atlas) = atlas {
                atlas.push_text(out_text, tr.body_moon,
                    sx as f32 + 13.0, sy as f32 - 5.0, 14.0,
                    [220.0/255.0, 220.0/255.0, 230.0/255.0, 0.95]);
            }
        }
    }

    // Planets — colour + size from the magnitude tier.
    for (planet, pos) in &ephemeris::all_planets(v.jd) {
        let Some((sx, sy)) = project(v, pos.jnow.ra_deg, pos.jnow.dec_deg)
            else { continue };
        let color = planet_color(*planet);
        let r = ((4.0 - pos.mag as f64).clamp(2.5, 6.0)).max(2.5);
        push_disc(out_dso, sx, sy, r, color);
        if names_on && v.fov < 40.0 {
            if let Some(atlas) = atlas {
                atlas.push_text(out_text, planet.name_i18n(lang),
                    sx as f32 + r as f32 + 4.0, sy as f32 - 5.0, 13.0,
                    color);
            }
        }
    }
}
