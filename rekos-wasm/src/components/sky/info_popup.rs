//! Click-to-info popup for stars / DSOs / solar-system bodies.
//!
//! Opened by `on:mouseup` in `mod.rs` after the hit-test inside
//! `render::render_overlay` identifies what the user clicked.

use leptos::prelude::*;

use crate::coords::JNow;
use crate::dso_catalog::DsoType;
use crate::i18n::{Lang, t};

use super::render::{HitItem, HitKind};

#[component]
pub fn SkyInfoPopup(
    info_popup: ReadSignal<Option<HitItem>>,
    set_info_popup: WriteSignal<Option<HitItem>>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    view! {
        {move || {
            info_popup.get().map(|hit| {
                let tr_ = tr();
                let kind_label = kind_label_for(&hit.kind, lang.get());
                let ra_h = hit.ra_jnow_deg / 15.0;
                let rah = ra_h as u32;
                let ram = ((ra_h - rah as f64) * 60.0) as u32;
                let ras = ((ra_h - rah as f64) * 3600.0 - ram as f64 * 60.0).abs();
                let dec = hit.dec_jnow_deg;
                let dec_sign = if dec < 0.0 { "-" } else { "+" };
                let dec_abs = dec.abs();
                let decd = dec_abs as u32;
                let decm = ((dec_abs - decd as f64) * 60.0) as u32;
                let decs = ((dec_abs - decd as f64) * 3600.0 - decm as f64 * 60.0) as u32;
                // Catalog → J2000 (hit carries JNow; show both).
                let j2000 = JNow::new(hit.ra_jnow_deg, hit.dec_jnow_deg).to_j2000(j_date_now());
                let j_ra_h = j2000.ra_deg / 15.0;
                let j_rah = j_ra_h as u32;
                let j_ram = ((j_ra_h - j_rah as f64) * 60.0) as u32;
                let j_decd = j2000.dec_deg.abs() as u32;
                let j_decm = ((j2000.dec_deg.abs() - j_decd as f64) * 60.0) as u32;
                let j_dec_sign = if j2000.dec_deg < 0.0 { "-" } else { "+" };

                view! {
                    <div
                        class="sky-info-popup"
                        style="position:absolute; bottom:0; left:0; z-index:180; \
                               background:rgba(10,10,20,0.97); border:1px solid #446; \
                               border-bottom:none; border-left:none; border-radius:0 6px 0 0; \
                               padding:12px 14px; min-width:260px; max-width:360px; \
                               font-family:monospace; font-size:12px; color:#c0c8d8;"
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="display:flex; justify-content:space-between; align-items:baseline; gap:8px;">
                            <span style="font-weight:bold; color:#88aaff; font-size:14px;">{hit.name.clone()}</span>
                            <button
                                on:click=move |_| set_info_popup.set(None)
                                style="background:transparent; color:#888; border:none; cursor:pointer; \
                                       font-family:monospace; font-size:14px; padding:0 4px;"
                                title=tr_.info_close
                            >"\u{2716}"</button>
                        </div>
                        <div style="color:#8ca; margin-top:2px; font-size:11px;">
                            {format!("{}: {}", tr_.type_label, kind_label)}
                        </div>
                        {hit.mag.map(|m| view! {
                            <div style="color:#aaa; margin-top:4px;">
                                {format!("{}: {:.2}", tr_.mag_label, m)}
                            </div>
                        })}
                        {hit.size_arcmin.map(|s| view! {
                            <div style="color:#aaa;">
                                {format!("{}: {:.1}'", tr_.size_label, s)}
                            </div>
                        })}
                        {hit.phase.map(|ph| view! {
                            <div style="color:#aaa;">
                                {format!("{}: {:.0}%", tr_.phase_label, ph * 100.0)}
                            </div>
                        })}
                        <div style="margin-top:6px; color:#88aaff;">
                            {format!("JNow  RA {:02}h{:02}m{:04.1}s  Dec {}{}\u{00b0}{:02}'{:02}\"",
                                rah, ram, ras, dec_sign, decd, decm, decs)}
                        </div>
                        <div style="color:#668;">
                            {format!("J2000 RA {:02}h{:02}m    Dec {}{}\u{00b0}{:02}'",
                                j_rah, j_ram, j_dec_sign, j_decd, j_decm)}
                        </div>
                    </div>
                }
            })
        }}
    }
}

fn kind_label_for(kind: &HitKind, lang: Lang) -> &'static str {
    let s = t(lang);
    match kind {
        HitKind::Star    => s.kind_star,
        HitKind::Sun     => s.kind_sun,
        HitKind::Moon    => s.kind_moon,
        HitKind::Planet  => s.kind_planet,
        HitKind::Dso(d)  => match d {
            DsoType::Galaxy           => s.kind_galaxy,
            DsoType::OpenCluster      => s.kind_open_cluster,
            DsoType::GlobularCluster  => s.kind_globular,
            DsoType::Nebula           => s.kind_nebula,
            DsoType::PlanetaryNebula  => s.kind_planetary,
            DsoType::SupernovaRemnant => s.kind_snr,
            DsoType::GalaxyCluster    => s.kind_galaxy_cluster,
        },
    }
}

fn j_date_now() -> f64 {
    // Thin wrapper so the popup doesn't need to thread the render's jd.
    use crate::astro;
    let d = js_sys::Date::new_0();
    astro::julian_date(
        d.get_utc_full_year() as i32,
        d.get_utc_month() + 1,
        d.get_utc_date(),
        d.get_utc_hours(),
        d.get_utc_minutes(),
        d.get_utc_seconds() as f64 + d.get_utc_milliseconds() as f64 / 1000.0,
    )
}
