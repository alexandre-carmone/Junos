//! Sky controls panel (top-right collapsible settings).

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::compat::SiteSnapshot;
use crate::i18n::{Lang, t};

use super::SkyToggles;
use super::utils::{event_target_checked, event_target_value};

const CHECKBOX_ROW: &str = "flex items-center gap-[6px] cursor-pointer [&>svg]:shrink-0";
const CONTROLS_INPUT: &str = "bg-bg-input text-[#ccc] border border-border-input font-mono text-sm p-[2px]";
const SECTION_HDR: &str = "py-[7px] px-[10px] w-full text-left border-0 border-b border-border-strong bg-bg-section-hdr text-text-blue font-bold text-sm font-mono uppercase tracking-[0.06em] cursor-pointer flex justify-between items-center min-h-[36px]";
const CONTROLS_BTN: &str = "bg-bg-button text-text-blue border border-border-input py-1 px-sp-2 cursor-pointer font-mono text-sm";
const SETTINGS_ROW: &str = "text-sm flex items-center gap-1";

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
        <div class="absolute top-2 right-2 max-md:!top-[34px] flex flex-col items-end gap-1 z-50"
             on:click=move |ev| ev.stop_propagation()>
            // Toggle button
            <button
                class=move || {
                    let base = "text-text-blue border border-border-accent rounded-sm py-1 px-[10px] cursor-pointer font-mono text-base leading-none min-h-[32px]";
                    if show_controls.get() {
                        format!("{base} bg-[rgba(26,26,46,0.92)]")
                    } else {
                        format!("{base} bg-bg-panel-dim")
                    }
                }
                on:click=move |_| set_show_controls.update(|v| *v = !*v)
                title=move || tr().toggle_controls>
                {move || if show_controls.get() { "\u{2716}" } else { "\u{2699}" }}
            </button>

            // Collapsible panel
            {move || show_controls.get().then(|| view! {
                <div class="bg-bg-panel-glass border border-border-mid rounded-sm text-[12px] overflow-hidden min-w-[180px] max-w-[calc(100vw-16px)] max-h-[calc(100dvh-120px)] overflow-y-auto [overscroll-behavior:contain] md:max-lg:min-w-[160px] max-md:!min-w-0 max-md:w-[calc(100vw-16px)] max-md:max-h-[calc(100dvh-160px)]">

                    // ── Part 1 : Sky display ───────────────────────────
                    <button class=SECTION_HDR
                            on:click=move |_| set_show_sky_section.update(|v| *v = !*v)>
                        {move || tr().sky_section}
                        {move || if show_sky_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_sky_section.get().then(|| view! {
                        <div class="flex flex-col gap-1 py-[6px] px-[10px] border-b border-border-strong">
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.stars.get()
                                       on:change=move |ev| toggles.stars.set(event_target_checked(&ev)) />
                                {move || tr().stars_checkbox}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.names.get()
                                       on:change=move |ev| toggles.names.set(event_target_checked(&ev)) />
                                {move || tr().names_checkbox}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.constellations.get()
                                       on:change=move |ev| toggles.constellations.set(event_target_checked(&ev)) />
                                {move || tr().constellations}
                            </label>
                            <label class=format!("{CHECKBOX_ROW} pl-4")>
                                <input type="checkbox" prop:checked=move || toggles.con_names.get()
                                       on:change=move |ev| toggles.con_names.set(event_target_checked(&ev)) />
                                {move || tr().names_checkbox}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.grid.get()
                                       on:change=move |ev| toggles.grid.set(event_target_checked(&ev)) />
                                {move || tr().grid}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.eq_grid.get()
                                       on:change=move |ev| toggles.eq_grid.set(event_target_checked(&ev)) />
                                {move || tr().eq_grid}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.meridian.get()
                                       on:change=move |ev| toggles.meridian.set(event_target_checked(&ev)) />
                                {move || tr().meridian}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.ecliptic.get()
                                       on:change=move |ev| toggles.ecliptic.set(event_target_checked(&ev)) />
                                {move || tr().ecliptic}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.zenith.get()
                                       on:change=move |ev| toggles.zenith.set(event_target_checked(&ev)) />
                                {move || tr().zenith}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.fov.get()
                                       on:change=move |ev| toggles.fov.set(event_target_checked(&ev)) />
                                {move || tr().fov}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.solve_marker.get()
                                       on:change=move |ev| toggles.solve_marker.set(event_target_checked(&ev)) />
                                {move || tr().solve_marker}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.slew_trail.get()
                                       on:change=move |ev| toggles.slew_trail.set(event_target_checked(&ev)) />
                                {move || tr().slew_trail}
                            </label>
                            <label class=format!("{CHECKBOX_ROW} border-t border-border-strong pt-1 mt-[2px]")>
                                <input type="checkbox" prop:checked=move || toggles.scheduler_jobs.get()
                                       on:change=move |ev| toggles.scheduler_jobs.set(event_target_checked(&ev)) />
                                {move || tr().sky_scheduler_jobs}
                            </label>
                        </div>
                    })}

                    // ── Part 2 : Objects (DSO) ─────────────────────────
                    <button class=SECTION_HDR
                            on:click=move |_| set_show_objects_section.update(|v| *v = !*v)>
                        {move || tr().objects_section}
                        {move || if show_objects_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_objects_section.get().then(|| view! {
                        <div class="flex flex-col gap-1 py-[6px] px-[10px] border-b border-border-strong">
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.dso.get()
                                       on:change=move |ev| toggles.dso.set(event_target_checked(&ev)) />
                                {move || tr().all_dso}
                            </label>
                            <label class=CHECKBOX_ROW>
                                <input type="checkbox" prop:checked=move || toggles.solar_system.get()
                                       on:change=move |ev| toggles.solar_system.set(event_target_checked(&ev)) />
                                {move || tr().solar_system}
                            </label>
                            <label class=format!("{CHECKBOX_ROW} !gap-[5px]")>
                                <input type="checkbox" prop:checked=move || toggles.dso_galaxy.get()
                                       on:change=move |ev| toggles.dso_galaxy.set(event_target_checked(&ev)) />
                                <svg width="14" height="10">
                                    <ellipse cx="7" cy="5" rx="6" ry="2.5"
                                             fill="none" stroke="rgba(0,200,220,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().galaxies}
                            </label>
                            <label class=format!("{CHECKBOX_ROW} !gap-[5px]")>
                                <input type="checkbox" prop:checked=move || toggles.dso_open_cluster.get()
                                       on:change=move |ev| toggles.dso_open_cluster.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(255,220,50,0.85)" stroke-width="1.2"
                                            stroke-dasharray="3,2"/>
                                </svg>
                                {move || tr().open_clusters}
                            </label>
                            <label class=format!("{CHECKBOX_ROW} !gap-[5px]")>
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
                            <label class=format!("{CHECKBOX_ROW} !gap-[5px]")>
                                <input type="checkbox" prop:checked=move || toggles.dso_nebula.get()
                                       on:change=move |ev| toggles.dso_nebula.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <rect x="1.5" y="1.5" width="11" height="11"
                                          fill="none" stroke="rgba(60,220,100,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().nebulae}
                            </label>
                            <label class=format!("{CHECKBOX_ROW} !gap-[5px]")>
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
                            <label class=format!("{CHECKBOX_ROW} !gap-[5px]")>
                                <input type="checkbox" prop:checked=move || toggles.dso_snr.get()
                                       on:change=move |ev| toggles.dso_snr.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <rect x="1.5" y="1.5" width="11" height="11"
                                          fill="none" stroke="rgba(60,220,100,0.65)" stroke-width="1.2"
                                          stroke-dasharray="2,2"/>
                                </svg>
                                {move || tr().supernova_remnants}
                            </label>
                            <label class=format!("{CHECKBOX_ROW} !gap-[5px]")>
                                <input type="checkbox" prop:checked=move || toggles.dso_galaxy_cluster.get()
                                       on:change=move |ev| toggles.dso_galaxy_cluster.set(event_target_checked(&ev)) />
                                <svg width="14" height="14">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(220,100,220,0.85)" stroke-width="1.2"
                                            stroke-dasharray="2,3"/>
                                </svg>
                                {move || tr().galaxy_clusters}
                            </label>
                            <label class="flex items-center gap-1 mt-[2px] [&>span]:text-text-muted [&>span]:whitespace-nowrap [&>span]:text-sm [&>input]:w-[52px]">
                                <span>{move || tr().mag_limit}</span>
                                <input type="number" min="1" max="20" step="0.5"
                                       class=CONTROLS_INPUT
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
                    <button class=SECTION_HDR
                            on:click=move |_| set_show_settings_section.update(|v| *v = !*v)>
                        {move || tr().settings_section}
                        {move || if show_settings_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_settings_section.get().then(|| view! {
                        <div class="flex flex-col gap-[6px] py-[6px] px-[10px]">
                            <label class=format!("{SETTINGS_ROW} [&>input]:w-[60px]")>
                                {move || tr().fl_mm}
                                <input type="number"
                                       class=CONTROLS_INPUT
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
                            <button class=CONTROLS_BTN
                                    on:click=move |_| {
                                        set_follow_mount.set(true);
                                    }>
                                {move || tr().follow_mount}
                            </button>
                            <div class="border-t border-border-strong mt-1 pt-[6px]">
                                <div class="text-sm text-text-blue mb-1 font-bold">
                                    {move || tr().location_section}
                                </div>
                                <label class=format!("{SETTINGS_ROW} mb-[3px] [&>input]:w-[72px]")>
                                    {move || tr().latitude_label}
                                    <input type="number" step="0.0001" min="-90" max="90"
                                           class=CONTROLS_INPUT
                                           prop:value=move || lat_str.get()
                                           on:input=move |ev| lat_str.set(event_target_value(&ev)) />
                                </label>
                                <label class=format!("{SETTINGS_ROW} mb-[3px] [&>input]:w-[72px]")>
                                    {move || tr().longitude_label}
                                    <input type="number" step="0.0001" min="-180" max="180"
                                           class=CONTROLS_INPUT
                                           prop:value=move || lon_str.get()
                                           on:input=move |ev| lon_str.set(event_target_value(&ev)) />
                                </label>
                                <div class="flex gap-1 flex-wrap">
                                    <button
                                        class=CONTROLS_BTN
                                        on:click=move |_| {
                                            let lat = lat_str.get().parse::<f64>().unwrap_or(0.0);
                                            let lon = lon_str.get().parse::<f64>().unwrap_or(0.0);
                                            send_location.get_value()(lat, lon);
                                        }>
                                        {move || tr().set_location_btn}
                                    </button>
                                    <button
                                        class=format!("{CONTROLS_BTN} !bg-bg-button-ok !text-accent-green-soft !border-border-ok")
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
