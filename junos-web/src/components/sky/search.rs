//! Sky object search box component.

use std::sync::Arc;

use leptos::prelude::*;

use crate::astro;
use crate::catalog::CatalogData;
use crate::coords::J2000;
use crate::dso_catalog::DsoCatalogData;
use crate::i18n::{Lang, t};

use crate::compat::SiteSnapshot;

use super::clock::SkyClock;
use super::object_search::{SearchHit, search_objects};
use crate::dom::event_target_value;

/// Where to point the view to show a J2000 object at `jd`: its Alt/Az, a
/// field radius that frames it (8° for a point), and the DSO magnitude limit
/// that field calls for.
pub(super) struct Aim {
    pub alt: f64,
    pub az: f64,
    pub fov: f64,
    pub mag_limit: f64,
}

pub(super) fn aim_at(ra_deg: f64, dec_deg: f64, size_arcmin: f32, jd: f64, site: &SiteSnapshot) -> Aim {
    let lst = astro::lst_deg(astro::gmst_deg(jd), site.longitude);
    // Catalog coords are J2000 — precess to JNow before alt/az conversion.
    let jnow = J2000::new(ra_deg, dec_deg).to_jnow(jd);
    let (alt, az) = astro::eq_to_altaz(jnow.ra_deg, jnow.dec_deg, lst, site.latitude);
    let fov = if size_arcmin > 1.0 {
        (size_arcmin as f64 / 60.0 * 5.0).clamp(0.3, 30.0)
    } else {
        8.0
    };
    let auto_mag = (11.0 + 3.0 * (10.0_f64 / fov).log10()).clamp(4.0, 20.0);
    Aim { alt, az, fov, mag_limit: (auto_mag * 2.0).round() / 2.0 }
}

#[component]
pub fn SkySearch(
    sky_search: ReadSignal<String>,
    set_sky_search: WriteSignal<String>,
    catalog_sig: RwSignal<Option<Arc<CatalogData>>>,
    dso_catalog_sig: RwSignal<Option<Arc<DsoCatalogData>>>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    clock: RwSignal<SkyClock>,
    set_center_alt: WriteSignal<f64>,
    set_center_az: WriteSignal<f64>,
    set_follow_mount: WriteSignal<bool>,
    set_fov_radius: WriteSignal<f64>,
    dso_mag_limit: RwSignal<f64>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    view! {
        <div
            class="relative flex-1 min-w-0 md:max-w-[280px] pointer-events-auto"
            on:click=move |ev| ev.stop_propagation()
        >
            <input type="text"
                class="bg-bg-panel-glass text-text border border-border-accent h-9 px-sp-2 w-full font-mono text-[12px] box-border rounded-md"
                placeholder=move || tr().search_placeholder
                prop:value=move || sky_search.get()
                on:input=move |e| set_sky_search.set(event_target_value(&e))
            />
            {move || {
                let q = sky_search.get();
                let cat = catalog_sig.get();
                let dso_cat = dso_catalog_sig.get();
                let stars = cat.as_deref().map(|c| c.stars.as_slice()).unwrap_or(&[]);
                let dsos  = dso_cat.as_deref().map(|d| d.dsos.as_slice()).unwrap_or(&[]);
                let hits = search_objects(&q, stars, dsos, lang.get(), 25);

                if hits.is_empty() {
                    return view! { <></> }.into_any();
                }

                let rows = hits.into_iter().map(|SearchHit { name, ra_deg, dec_deg, size_arcmin }| {
                    let label = name.clone();
                    view! {
                        <div
                            on:click=move |_| {
                                let aim = aim_at(ra_deg, dec_deg, size_arcmin, clock.get_untracked().jd(),
                                                 &site.get_untracked());
                                set_center_alt.set(aim.alt);
                                set_center_az.set(aim.az);
                                set_follow_mount.set(false);
                                set_fov_radius.set(aim.fov);
                                dso_mag_limit.set(aim.mag_limit);

                                set_sky_search.set(String::new());
                            }
                            class="py-2 md:py-[3px] px-sp-2 cursor-pointer text-text border-b border-[#1a1a2a] text-[12px]"
                        >
                            {label}
                        </div>
                    }
                }).collect_view();

                view! {
                    <div class="absolute inset-x-0 top-full bg-bg-panel-solid border border-border-accent border-t-0 max-h-[min(220px,50dvh)] overflow-y-auto rounded-b-md">
                        {rows}
                    </div>
                }.into_any()
            }}
        </div>
    }
}
