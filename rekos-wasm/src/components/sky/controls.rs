//! Sky controls panel (top-right collapsible settings).

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::compat::SiteSnapshot;
use crate::i18n::{Lang, t};

use super::SkyToggles;
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
    toggles: SkyToggles,
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
             on:click=move |ev| ev.stop_propagation()>
            // Toggle button
            <button
                class="sky-controls-toggle"
                class:sky-controls-toggle--open=move || show_controls.get()
                on:click=move |_| set_show_controls.update(|v| *v = !*v)
                title=move || tr().toggle_controls>
                {move || if show_controls.get() { "\u{2716}" } else { "\u{2699}" }}
            </button>

            // Collapsible panel
            {move || show_controls.get().then(|| view! {
                <div class="sky-controls-panel">

                    // ── Part 1 : Sky display ───────────────────────────
                    <button class="sky-controls-section-hdr"
                            on:click=move |_| set_show_sky_section.update(|v| *v = !*v)>
                        {move || tr().sky_section}
                        {move || if show_sky_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_sky_section.get().then(|| view! {
                        <div class="sky-controls-section">
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.stars.get()
                                       on:change=move |ev| toggles.stars.set(event_target_checked(&ev)) />
                                {move || tr().stars_checkbox}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.names.get()
                                       on:change=move |ev| toggles.names.set(event_target_checked(&ev)) />
                                {move || tr().names_checkbox}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.constellations.get()
                                       on:change=move |ev| toggles.constellations.set(event_target_checked(&ev)) />
                                {move || tr().constellations}
                            </label>
                            <label class="sky-checkbox-row sky-checkbox-row--indent">
                                <input type="checkbox" prop:checked=move || toggles.con_names.get()
                                       on:change=move |ev| toggles.con_names.set(event_target_checked(&ev)) />
                                {move || tr().names_checkbox}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.grid.get()
                                       on:change=move |ev| toggles.grid.set(event_target_checked(&ev)) />
                                {move || tr().grid}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.eq_grid.get()
                                       on:change=move |ev| toggles.eq_grid.set(event_target_checked(&ev)) />
                                {move || tr().eq_grid}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.meridian.get()
                                       on:change=move |ev| toggles.meridian.set(event_target_checked(&ev)) />
                                {move || tr().meridian}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.ecliptic.get()
                                       on:change=move |ev| toggles.ecliptic.set(event_target_checked(&ev)) />
                                {move || tr().ecliptic}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.zenith.get()
                                       on:change=move |ev| toggles.zenith.set(event_target_checked(&ev)) />
                                {move || tr().zenith}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.fov.get()
                                       on:change=move |ev| toggles.fov.set(event_target_checked(&ev)) />
                                {move || tr().fov}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.solve_marker.get()
                                       on:change=move |ev| toggles.solve_marker.set(event_target_checked(&ev)) />
                                {move || tr().solve_marker}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.slew_trail.get()
                                       on:change=move |ev| toggles.slew_trail.set(event_target_checked(&ev)) />
                                {move || tr().slew_trail}
                            </label>
                            <label class="sky-checkbox-row sky-checkbox-row--separator">
                                <input type="checkbox" prop:checked=move || toggles.scheduler_jobs.get()
                                       on:change=move |ev| toggles.scheduler_jobs.set(event_target_checked(&ev)) />
                                {move || tr().sky_scheduler_jobs}
                            </label>
                        </div>
                    })}

                    // ── Part 2 : Objects (DSO) ─────────────────────────
                    <button class="sky-controls-section-hdr"
                            on:click=move |_| set_show_objects_section.update(|v| *v = !*v)>
                        {move || tr().objects_section}
                        {move || if show_objects_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_objects_section.get().then(|| view! {
                        <div class="sky-controls-section">
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.dso.get()
                                       on:change=move |ev| toggles.dso.set(event_target_checked(&ev)) />
                                {move || tr().all_dso}
                            </label>
                            <label class="sky-checkbox-row">
                                <input type="checkbox" prop:checked=move || toggles.solar_system.get()
                                       on:change=move |ev| toggles.solar_system.set(event_target_checked(&ev)) />
                                {move || tr().solar_system}
                            </label>
                            <label class="sky-checkbox-row sky-checkbox-row--with-icon">
                                <input type="checkbox" prop:checked=move || toggles.dso_galaxy.get()
                                       on:change=move |ev| toggles.dso_galaxy.set(event_target_checked(&ev)) />
                                <svg width="14" height="10">
                                    <ellipse cx="7" cy="5" rx="6" ry="2.5"
                                             fill="none" stroke="rgba(0,200,220,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().galaxies}
                            </label>
                            <label class="sky-checkbox-row sky-checkbox-row--with-icon">
                                <input type="checkbox" prop:checked=move || toggles.dso_open_cluster.get()
                                       on:change=move |ev| toggles.dso_open_cluster.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(255,220,50,0.85)" stroke-width="1.2"
                                            stroke-dasharray="3,2"/>
                                </svg>
                                {move || tr().open_clusters}
                            </label>
                            <label class="sky-checkbox-row sky-checkbox-row--with-icon">
                                <input type="checkbox" prop:checked=move || toggles.dso_globular.get()
                                       on:change=move |ev| toggles.dso_globular.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                                    <line x1="1.5" y1="7" x2="12.5" y2="7"
                                          stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                                    <line x1="7" y1="1.5" x2="7" y2="12.5"
                                          stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().globular_clusters}
                            </label>
                            <label class="sky-checkbox-row sky-checkbox-row--with-icon">
                                <input type="checkbox" prop:checked=move || toggles.dso_nebula.get()
                                       on:change=move |ev| toggles.dso_nebula.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <rect x="1.5" y="1.5" width="11" height="11"
                                          fill="none" stroke="rgba(60,220,100,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().nebulae}
                            </label>
                            <label class="sky-checkbox-row sky-checkbox-row--with-icon">
                                <input type="checkbox" prop:checked=move || toggles.dso_planetary.get()
                                       on:change=move |ev| toggles.dso_planetary.set(event_target_checked(&ev)) />
                                <svg width="18" height="14">
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
                            <label class="sky-checkbox-row sky-checkbox-row--with-icon">
                                <input type="checkbox" prop:checked=move || toggles.dso_snr.get()
                                       on:change=move |ev| toggles.dso_snr.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <rect x="1.5" y="1.5" width="11" height="11"
                                          fill="none" stroke="rgba(60,220,100,0.65)" stroke-width="1.2"
                                          stroke-dasharray="2,2"/>
                                </svg>
                                {move || tr().supernova_remnants}
                            </label>
                            <label class="sky-checkbox-row sky-checkbox-row--with-icon">
                                <input type="checkbox" prop:checked=move || toggles.dso_galaxy_cluster.get()
                                       on:change=move |ev| toggles.dso_galaxy_cluster.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(220,100,220,0.85)" stroke-width="1.2"
                                            stroke-dasharray="2,3"/>
                                </svg>
                                {move || tr().galaxy_clusters}
                            </label>
                            <label class="sky-controls-mag-row">
                                <span>{move || tr().mag_limit}</span>
                                <input type="number" min="1" max="20" step="0.5"
                                       class="sky-controls-input"
                                       prop:value=move || format!("{:.1}", toggles.dso_mag_limit.get())
                                       on:input=move |ev| {
                                           if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                               toggles.dso_mag_limit.set(v);
                                           }
                                       } />
                            </label>
                        </div>
                    })}

                    // ── Part 3 : Settings ──────────────────────────────
                    <button class="sky-controls-section-hdr"
                            on:click=move |_| set_show_settings_section.update(|v| *v = !*v)>
                        {move || tr().settings_section}
                        {move || if show_settings_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_settings_section.get().then(|| view! {
                        <div class="sky-controls-section sky-controls-section--no-border sky-controls-section--gap-6">
                            <label class="sky-controls-settings-row">
                                {move || tr().fl_mm}
                                <input type="number"
                                       class="sky-controls-input"
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
                            <button class="sky-controls-btn"
                                    on:click=move |_| {
                                        set_follow_mount.set(true);
                                    }>
                                {move || tr().follow_mount}
                            </button>
                            <div class="sky-controls-divider">
                                <div class="sky-controls-subhdr">
                                    {move || tr().location_section}
                                </div>
                                <label class="sky-controls-settings-row sky-controls-loc-row">
                                    {move || tr().latitude_label}
                                    <input type="number" step="0.0001" min="-90" max="90"
                                           class="sky-controls-input"
                                           prop:value=move || lat_str.get()
                                           on:input=move |ev| lat_str.set(event_target_value(&ev)) />
                                </label>
                                <label class="sky-controls-settings-row sky-controls-loc-row">
                                    {move || tr().longitude_label}
                                    <input type="number" step="0.0001" min="-180" max="180"
                                           class="sky-controls-input"
                                           prop:value=move || lon_str.get()
                                           on:input=move |ev| lon_str.set(event_target_value(&ev)) />
                                </label>
                                <div class="sky-controls-loc-buttons">
                                    <button
                                        class="sky-controls-btn"
                                        on:click=move |_| {
                                            let lat = lat_str.get().parse::<f64>().unwrap_or(0.0);
                                            let lon = lon_str.get().parse::<f64>().unwrap_or(0.0);
                                            send_location.get_value()(lat, lon);
                                        }>
                                        {move || tr().set_location_btn}
                                    </button>
                                    <button
                                        class="sky-controls-btn sky-controls-btn--ok"
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
            })}
        </div>
    }
}
