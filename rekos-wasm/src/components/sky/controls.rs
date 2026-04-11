//! Sky controls panel (top-right collapsible settings).

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::compat::SiteSnapshot;

use crate::i18n::{Lang, t};

use super::utils::{event_target_checked, event_target_value};

#[component]
pub fn SkyControls(
    show_controls: ReadSignal<bool>,
    set_show_controls: WriteSignal<bool>,
    show_sky_section: ReadSignal<bool>,
    set_show_sky_section: WriteSignal<bool>,
    show_objects_section: ReadSignal<bool>,
    set_show_objects_section: WriteSignal<bool>,
    show_settings_section: ReadSignal<bool>,
    set_show_settings_section: WriteSignal<bool>,
    show_stars: ReadSignal<bool>,
    set_show_stars: WriteSignal<bool>,
    show_names: ReadSignal<bool>,
    set_show_names: WriteSignal<bool>,
    show_constellations: ReadSignal<bool>,
    set_show_constellations: WriteSignal<bool>,
    show_con_names: ReadSignal<bool>,
    set_show_con_names: WriteSignal<bool>,
    show_grid: ReadSignal<bool>,
    set_show_grid: WriteSignal<bool>,
    show_eq_grid: ReadSignal<bool>,
    set_show_eq_grid: WriteSignal<bool>,
    show_meridian: ReadSignal<bool>,
    set_show_meridian: WriteSignal<bool>,
    show_fov: ReadSignal<bool>,
    set_show_fov: WriteSignal<bool>,
    show_dso: ReadSignal<bool>,
    set_show_dso: WriteSignal<bool>,
    dso_filter_galaxy: ReadSignal<bool>,
    set_dso_filter_galaxy: WriteSignal<bool>,
    dso_filter_open_cluster: ReadSignal<bool>,
    set_dso_filter_open_cluster: WriteSignal<bool>,
    dso_filter_globular: ReadSignal<bool>,
    set_dso_filter_globular: WriteSignal<bool>,
    dso_filter_nebula: ReadSignal<bool>,
    set_dso_filter_nebula: WriteSignal<bool>,
    dso_filter_planetary: ReadSignal<bool>,
    set_dso_filter_planetary: WriteSignal<bool>,
    dso_filter_snr: ReadSignal<bool>,
    set_dso_filter_snr: WriteSignal<bool>,
    dso_filter_galaxy_cluster: ReadSignal<bool>,
    set_dso_filter_galaxy_cluster: WriteSignal<bool>,
    dso_mag_limit: ReadSignal<f64>,
    set_dso_mag_limit: WriteSignal<f64>,
    focal_override: ReadSignal<String>,
    set_focal_override: WriteSignal<String>,
    focal_length_mm: Signal<Option<f64>>,
    set_follow_mount: WriteSignal<bool>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    set_site_location: Arc<dyn Fn(f64, f64) + Send + Sync>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let s = site.get_untracked();
    let lat_str = RwSignal::new(format!("{:.4}", s.latitude));
    let lon_str = RwSignal::new(format!("{:.4}", s.longitude));
    let send_location = StoredValue::new(Arc::clone(&set_site_location));

    view! {
        <div class="sky-controls"
             style="position:absolute; top:8px; right:8px; display:flex; flex-direction:column; align-items:flex-end; gap:4px; z-index:50;"
             on:click=move |ev| ev.stop_propagation()>
            // Toggle button
            <button
                on:click=move |_| set_show_controls.update(|v| *v = !*v)
                title=move || tr().toggle_controls
                style=move || format!(
                    "background:{}; color:#88aaff; border:1px solid #446; border-radius:4px; \
                     padding:4px 10px; cursor:pointer; font-family:monospace; font-size:14px; \
                     line-height:1; min-height:32px;",
                    if show_controls.get() { "rgba(26,26,46,0.92)" } else { "rgba(10,10,20,0.75)" }
                )>
                {move || if show_controls.get() { "\u{2716}" } else { "\u{2699}" }}
            </button>

            // Collapsible panel
            {move || show_controls.get().then(|| {
                let section_hdr = "padding:7px 10px; width:100%; text-align:left; \
                                   border:none; border-bottom:1px solid #2a2a3a; \
                                   background:rgba(30,30,50,0.7); color:#88aaff; \
                                   font-weight:bold; font-size:11px; font-family:monospace; \
                                   text-transform:uppercase; letter-spacing:0.06em; \
                                   cursor:pointer; display:flex; justify-content:space-between; \
                                   align-items:center; min-height:36px;";
                view! {
                <div class="sky-controls-panel"
                     style="background:rgba(10,10,20,0.88); border:1px solid #333; \
                             border-radius:4px; font-size:12px; overflow:hidden;">

                    // ── Part 1 : Sky display ───────────────────────────
                    <button style=section_hdr
                            on:click=move |_| set_show_sky_section.update(|v| *v = !*v)>
                        {move || tr().sky_section}
                        {move || if show_sky_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_sky_section.get().then(|| view! {
                        <div style="display:flex; flex-direction:column; gap:4px; padding:6px 10px; \
                                     border-bottom:1px solid #2a2a3a;">
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || show_stars.get()
                                       on:change=move |ev| set_show_stars.set(event_target_checked(&ev)) />
                                {move || tr().stars_checkbox}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || show_names.get()
                                       on:change=move |ev| set_show_names.set(event_target_checked(&ev)) />
                                {move || tr().names_checkbox}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || show_constellations.get()
                                       on:change=move |ev| set_show_constellations.set(event_target_checked(&ev)) />
                                {move || tr().constellations}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer; padding-left:16px;">
                                <input type="checkbox" prop:checked=move || show_con_names.get()
                                       on:change=move |ev| set_show_con_names.set(event_target_checked(&ev)) />
                                {move || tr().names_checkbox}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || show_grid.get()
                                       on:change=move |ev| set_show_grid.set(event_target_checked(&ev)) />
                                {move || tr().grid}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || show_eq_grid.get()
                                       on:change=move |ev| set_show_eq_grid.set(event_target_checked(&ev)) />
                                {move || tr().eq_grid}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || show_meridian.get()
                                       on:change=move |ev| set_show_meridian.set(event_target_checked(&ev)) />
                                {move || tr().meridian}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || show_fov.get()
                                       on:change=move |ev| set_show_fov.set(event_target_checked(&ev)) />
                                {move || tr().fov}
                            </label>
                        </div>
                    })}

                    // ── Part 2 : Objects (DSO) ─────────────────────────
                    <button style=section_hdr
                            on:click=move |_| set_show_objects_section.update(|v| *v = !*v)>
                        {move || tr().objects_section}
                        {move || if show_objects_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_objects_section.get().then(|| view! {
                        <div style="display:flex; flex-direction:column; gap:4px; padding:6px 10px; \
                                     border-bottom:1px solid #2a2a3a;">
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || show_dso.get()
                                       on:change=move |ev| set_show_dso.set(event_target_checked(&ev)) />
                                {move || tr().all_dso}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || dso_filter_galaxy.get()
                                       on:change=move |ev| set_dso_filter_galaxy.set(event_target_checked(&ev)) />
                                <svg width="14" height="10" style="flex-shrink:0;">
                                    <ellipse cx="7" cy="5" rx="6" ry="2.5"
                                             fill="none" stroke="rgba(0,200,220,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().galaxies}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || dso_filter_open_cluster.get()
                                       on:change=move |ev| set_dso_filter_open_cluster.set(event_target_checked(&ev)) />
                                <svg width="14" height="14" style="flex-shrink:0;">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(255,220,50,0.85)" stroke-width="1.2"
                                            stroke-dasharray="3,2"/>
                                </svg>
                                {move || tr().open_clusters}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || dso_filter_globular.get()
                                       on:change=move |ev| set_dso_filter_globular.set(event_target_checked(&ev)) />
                                <svg width="14" height="14" style="flex-shrink:0;">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                                    <line x1="1.5" y1="7" x2="12.5" y2="7"
                                          stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                                    <line x1="7" y1="1.5" x2="7" y2="12.5"
                                          stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().globular_clusters}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || dso_filter_nebula.get()
                                       on:change=move |ev| set_dso_filter_nebula.set(event_target_checked(&ev)) />
                                <svg width="14" height="14" style="flex-shrink:0;">
                                    <rect x="1.5" y="1.5" width="11" height="11"
                                          fill="none" stroke="rgba(60,220,100,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().nebulae}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || dso_filter_planetary.get()
                                       on:change=move |ev| set_dso_filter_planetary.set(event_target_checked(&ev)) />
                                <svg width="18" height="14" style="flex-shrink:0;">
                                    <circle cx="9" cy="7" r="4"
                                            fill="none" stroke="rgba(0,230,180,0.85)" stroke-width="1.2"/>
                                    <line x1="1" y1="7" x2="5" y2="7"
                                          stroke="rgba(0,230,180,0.85)" stroke-width="1.2"/>
                                    <line x1="13" y1="7" x2="17" y2="7"
                                          stroke="rgba(0,230,180,0.85)" stroke-width="1.2"/>
                                    <line x1="9" y1="1" x2="9" y2="3"
                                          stroke="rgba(0,230,180,0.85)" stroke-width="1.2"/>
                                    <line x1="9" y1="11" x2="9" y2="13"
                                          stroke="rgba(0,230,180,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().planetary_nebulae}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || dso_filter_snr.get()
                                       on:change=move |ev| set_dso_filter_snr.set(event_target_checked(&ev)) />
                                <svg width="14" height="14" style="flex-shrink:0;">
                                    <rect x="1.5" y="1.5" width="11" height="11"
                                          fill="none" stroke="rgba(60,220,100,0.65)" stroke-width="1.2"
                                          stroke-dasharray="2,2"/>
                                </svg>
                                {move || tr().supernova_remnants}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || dso_filter_galaxy_cluster.get()
                                       on:change=move |ev| set_dso_filter_galaxy_cluster.set(event_target_checked(&ev)) />
                                <svg width="14" height="14" style="flex-shrink:0;">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(220,100,220,0.85)" stroke-width="1.2"
                                            stroke-dasharray="2,3"/>
                                </svg>
                                {move || tr().galaxy_clusters}
                            </label>
                            <label style="display:flex; align-items:center; gap:4px; margin-top:2px;">
                                <span style="color:#aaa; white-space:nowrap; font-size:11px;">{move || tr().mag_limit}</span>
                                <input type="number" min="1" max="20" step="0.5"
                                       style="width:52px; background:#111; color:#ccc; border:1px solid #444; \
                                              font-family:monospace; font-size:11px; padding:2px;"
                                       prop:value=move || format!("{:.1}", dso_mag_limit.get())
                                       on:input=move |ev| {
                                           if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                               set_dso_mag_limit.set(v);
                                           }
                                       } />
                            </label>
                        </div>
                    })}

                    // ── Part 3 : Settings ──────────────────────────────
                    <button style=section_hdr
                            on:click=move |_| set_show_settings_section.update(|v| *v = !*v)>
                        {move || tr().settings_section}
                        {move || if show_settings_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_settings_section.get().then(|| view! {
                        <div style="display:flex; flex-direction:column; gap:6px; padding:6px 10px;">
                            <label style="font-size:11px; display:flex; align-items:center; gap:4px;">
                                {move || tr().fl_mm}
                                <input type="number"
                                       style="width:60px; background:#111; color:#ccc; border:1px solid #444; \
                                              font-family:monospace; font-size:11px; padding:2px;"
                                       prop:value=move || focal_override.get()
                                       placeholder=move || focal_length_mm.get()
                                           .map(|v| format!("{v:.0}")).unwrap_or_default()
                                       on:input=move |ev| {
                                           let val = event_target_value(&ev);
                                           if let Some(ls) = web_sys::window()
                                               .and_then(|w| w.local_storage().ok().flatten())
                                           {
                                               let _ = ls.set_item("sky_focal_override", &val);
                                           }
                                           set_focal_override.set(val);
                                       } />
                            </label>
                            <button style="background:#1a1a2e; color:#88aaff; border:1px solid #444; \
                                           padding:4px 8px; cursor:pointer; font-family:monospace; font-size:11px;"
                                    on:click=move |_| {
                                        set_follow_mount.set(true);
                                    }>
                                {move || tr().follow_mount}
                            </button>
                            <div style="border-top:1px solid #2a2a3a; margin-top:4px; padding-top:6px;">
                                <div style="font-size:11px; color:#88aaff; margin-bottom:4px; font-weight:bold;">
                                    {move || tr().location_section}
                                </div>
                                <label style="font-size:11px; display:flex; align-items:center; gap:4px; margin-bottom:3px;">
                                    {move || tr().latitude_label}
                                    <input type="number" step="0.0001" min="-90" max="90"
                                           style="width:72px; background:#111; color:#ccc; border:1px solid #444; \
                                                  font-family:monospace; font-size:11px; padding:2px;"
                                           prop:value=move || lat_str.get()
                                           on:input=move |ev| lat_str.set(event_target_value(&ev)) />
                                </label>
                                <label style="font-size:11px; display:flex; align-items:center; gap:4px; margin-bottom:4px;">
                                    {move || tr().longitude_label}
                                    <input type="number" step="0.0001" min="-180" max="180"
                                           style="width:72px; background:#111; color:#ccc; border:1px solid #444; \
                                                  font-family:monospace; font-size:11px; padding:2px;"
                                           prop:value=move || lon_str.get()
                                           on:input=move |ev| lon_str.set(event_target_value(&ev)) />
                                </label>
                                <div style="display:flex; gap:4px; flex-wrap:wrap;">
                                    <button
                                        style="background:#1a1a2e; color:#88aaff; border:1px solid #444; \
                                               padding:4px 8px; cursor:pointer; font-family:monospace; font-size:11px;"
                                        on:click=move |_| {
                                            let lat = lat_str.get().parse::<f64>().unwrap_or(0.0);
                                            let lon = lon_str.get().parse::<f64>().unwrap_or(0.0);
                                            send_location.get_value()(lat, lon);
                                        }>
                                        {move || tr().set_location_btn}
                                    </button>
                                    <button
                                        style="background:#1a2a1a; color:#8f8; border:1px solid #484; \
                                               padding:4px 8px; cursor:pointer; font-family:monospace; font-size:11px;"
                                        on:click=move |_| {
                                            let lat_s = lat_str;
                                            let lon_s = lon_str;
                                            let send_loc = send_location.get_value();
                                            let success = Closure::wrap(Box::new(move |val: wasm_bindgen::JsValue| {
                                                let lat = js_sys::Reflect::get(&val, &"coords".into())
                                                    .ok()
                                                    .and_then(|c| js_sys::Reflect::get(&c, &"latitude".into()).ok())
                                                    .and_then(|v| v.as_f64());
                                                let lon = js_sys::Reflect::get(&val, &"coords".into())
                                                    .ok()
                                                    .and_then(|c| js_sys::Reflect::get(&c, &"longitude".into()).ok())
                                                    .and_then(|v| v.as_f64());
                                                if let (Some(lat), Some(lon)) = (lat, lon) {
                                                    lat_s.set(format!("{:.6}", lat));
                                                    lon_s.set(format!("{:.6}", lon));
                                                    send_loc(lat, lon);
                                                }
                                            }) as Box<dyn FnMut(wasm_bindgen::JsValue)>);
                                            if let Some(window) = web_sys::window() {
                                                if let Ok(geo) = window.navigator().geolocation() {
                                                    let _ = geo.get_current_position(success.as_ref().unchecked_ref());
                                                }
                                            }
                                            success.forget();
                                        }>
                                        {move || tr().get_location_btn}
                                    </button>
                                </div>
                            </div>
                        </div>
                    })}

                </div>
                }
            })}
        </div>
    }
}
