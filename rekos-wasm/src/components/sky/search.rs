//! Sky object search box component.

use std::sync::Arc;

use leptos::prelude::*;

use crate::astro;
use crate::catalog::CatalogData;
use crate::dso_catalog::DsoCatalogData;
use crate::i18n::{Lang, t};

use crate::compat::SiteSnapshot;

use super::utils::event_target_value;

#[component]
pub fn SkySearch(
    sky_search: ReadSignal<String>,
    set_sky_search: WriteSignal<String>,
    catalog_sig: RwSignal<Option<Arc<CatalogData>>>,
    dso_catalog_sig: RwSignal<Option<Arc<DsoCatalogData>>>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    time_offset_s: ReadSignal<f64>,
    set_center_alt: WriteSignal<f64>,
    set_center_az: WriteSignal<f64>,
    set_follow_mount: WriteSignal<bool>,
    set_fov_radius: WriteSignal<f64>,
    set_dso_mag_limit: WriteSignal<f64>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    view! {
        <div
            class="sky-search"
            style="position:absolute; top:8px; left:8px; z-index:50;"
            on:click=move |ev| ev.stop_propagation()
        >
            <input type="text"
                placeholder=move || tr().search_placeholder
                prop:value=move || sky_search.get()
                on:input=move |e| set_sky_search.set(event_target_value(&e))
                style="background:rgba(10,10,20,0.88); color:#c0c0d0; border:1px solid #446; \
                       padding:4px 8px; width:100%; font-family:monospace; font-size:12px; \
                       box-sizing:border-box; border-radius:2px;" />
            {move || {
                let q = sky_search.get();
                let ql = q.trim().to_lowercase();
                if ql.len() < 2 {
                    return view! { <></> }.into_any();
                }

                let mut items: Vec<(String, f64, f64, f32)> = Vec::new();

                let cat = catalog_sig.get();
                let dso_cat = dso_catalog_sig.get();

                let mut n = 0usize;
                if let Some(ref cat) = cat {
                    for star in cat.stars.iter() {
                        if n >= 10 { break; }
                        if let Some(name) = star.name.as_deref() {
                            if name.to_lowercase().contains(&ql) {
                                items.push((name.to_string(), star.ra_deg as f64, star.dec_deg as f64, 0.0));
                                n += 1;
                            }
                        }
                    }
                }

                let mut n = 0usize;
                if let Some(ref dso_cat) = dso_cat {
                    for dso in dso_cat.dsos.iter() {
                        if n >= 15 { break; }
                        if dso.name.to_lowercase().contains(&ql) {
                            items.push((dso.name.to_string(), dso.ra_deg as f64, dso.dec_deg as f64, dso.size_arcmin));
                            n += 1;
                        }
                    }
                }

                if items.is_empty() {
                    return view! { <></> }.into_any();
                }

                let rows = items.into_iter().map(|(name, ra_deg, dec_deg, size_arcmin)| {
                    let label = name.clone();
                    view! {
                        <div
                            on:click=move |_| {
                                let now = js_sys::Date::new_0();
                                let jd = astro::julian_date(
                                    now.get_utc_full_year() as i32,
                                    now.get_utc_month() + 1,
                                    now.get_utc_date(),
                                    now.get_utc_hours(),
                                    now.get_utc_minutes(),
                                    now.get_utc_seconds() as f64
                                        + now.get_utc_milliseconds() as f64 / 1000.0
                                        + time_offset_s.get_untracked(),
                                );
                                let gmst = astro::gmst_deg(jd);
                                let s = site.get_untracked();
                                let lst = astro::lst_deg(gmst, s.longitude);
                                let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, lst, s.latitude);
                                set_center_alt.set(alt);
                                set_center_az.set(az);
                                set_follow_mount.set(false);

                                let fov = if size_arcmin > 1.0 {
                                    (size_arcmin as f64 / 60.0 * 5.0).clamp(0.3, 30.0)
                                } else {
                                    8.0
                                };
                                set_fov_radius.set(fov);
                                let auto_mag = (11.0 + 3.0 * (10.0_f64 / fov).log10()).clamp(4.0, 20.0);
                                set_dso_mag_limit.set((auto_mag * 2.0).round() / 2.0);

                                set_sky_search.set(String::new());
                            }
                            style="padding:3px 8px; cursor:pointer; color:#c0c0d0; \
                                   border-bottom:1px solid #1a1a2a; font-size:12px;"
                        >
                            {label}
                        </div>
                    }
                }).collect_view();

                view! {
                    <div style="background:rgba(10,10,20,0.95); border:1px solid #446; \
                                border-top:none; max-height:220px; overflow-y:auto; \
                                border-radius:0 0 2px 2px;">
                        {rows}
                    </div>
                }.into_any()
            }}
        </div>
    }
}
