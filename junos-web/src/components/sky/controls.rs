//! Layers panel — toggle chips for every sky layer, the DSO magnitude limit
//! and the observer location. A bottom sheet on phones, a floating panel
//! (clear of the desktop tab strip) on md+.

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::compat::SiteSnapshot;
use crate::dom::event_target_value;
use crate::i18n::{Lang, Translations, t};

use super::SkyToggles;

const CHIP: &str = "chip min-w-0 h-9 md:h-7 gap-1.5 cursor-pointer";
const GROUP: &str = "text-xs uppercase tracking-[0.06em] text-text-muted mt-1";
const ROW: &str = "flex flex-wrap gap-1.5";
const CONTROLS_INPUT: &str = "input input--sm font-mono w-[96px]";
const CONTROLS_BTN: &str = "btn btn--sm btn-ghost text-text-blue";
const SETTINGS_ROW: &str = "text-sm flex items-center justify-between gap-2";

fn lang_ctx() -> RwSignal<Lang> {
    use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En))
}

/// One toggle pill bound to a layer signal; `children` is an optional icon.
#[component]
fn LayerChip(
    on: RwSignal<bool>,
    label: fn(&Translations) -> &'static str,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let lang = lang_ctx();
    view! {
        <button class=move || if on.get() { format!("{CHIP} btn--active") } else { format!("{CHIP} text-text-muted") }
                aria-pressed=move || on.get().to_string()
                on:click=move |_| on.update(|v| *v = !*v)>
            {children.map(|c| c())}
            {move || label(t(lang.get()))}
        </button>
    }
}

#[component]
pub fn SkyControls(
    open: RwSignal<bool>,
    toggles: SkyToggles,
    #[prop(into)] site: Signal<SiteSnapshot>,
    set_site_location: Arc<dyn Fn(f64, f64) + Send + Sync>,
    #[prop(into)] mount_device: Signal<Option<String>>,
) -> impl IntoView {
    let lang = lang_ctx();
    let tr = move || t(lang.get());

    let s = site.get_untracked();
    let lat_str = RwSignal::new(format!("{:.4}", s.latitude));
    let lon_str = RwSignal::new(format!("{:.4}", s.longitude));
    let send_location = StoredValue::new(Arc::clone(&set_site_location));
    // Writing KStars' location requires a connected mount to relay through.
    let has_mount = move || mount_device.get().is_some();
    let location_open = RwSignal::new(false);

    view! {
        <Show when=move || open.get()>
            <div class="panel absolute z-[70] inset-x-0 bottom-0 max-h-[70dvh] \
                        rounded-b-none pb-[max(0.75rem,env(safe-area-inset-bottom))] \
                        md:inset-x-auto md:bottom-auto md:top-[56px] md:right-[72px] md:w-[300px] \
                        md:max-h-[calc(100dvh-140px)] md:rounded-lg md:pb-3 \
                        overflow-y-auto [overscroll-behavior:contain] px-3 pt-2 flex flex-col gap-1.5 text-sm"
                 on:click=|ev| ev.stop_propagation()>
                <div class="flex items-center justify-between">
                    <span class="font-semibold text-text-blue">{move || tr().layers}</span>
                    <button class="btn-icon" title=move || tr().info_close
                            on:click=move |_| open.set(false)>"\u{2716}"</button>
                </div>

                <div class=GROUP>{move || tr().layer_sky}</div>
                <div class=ROW>
                    <LayerChip on=toggles.stars label=|t| t.stars_checkbox />
                    <LayerChip on=toggles.names label=|t| t.layer_star_names />
                    <LayerChip on=toggles.constellations label=|t| t.constellations />
                    <LayerChip on=toggles.con_names label=|t| t.layer_con_names />
                    <LayerChip on=toggles.solar_system label=|t| t.solar_system />
                </div>

                <div class=GROUP>{move || tr().layer_grids}</div>
                <div class=ROW>
                    <LayerChip on=toggles.grid label=|t| t.grid />
                    <LayerChip on=toggles.eq_grid label=|t| t.eq_grid />
                    <LayerChip on=toggles.meridian label=|t| t.meridian />
                    <LayerChip on=toggles.ecliptic label=|t| t.ecliptic />
                    <LayerChip on=toggles.zenith label=|t| t.zenith />
                </div>

                <div class=GROUP>{move || tr().layer_dso}</div>
                <div class=ROW>
                    <LayerChip on=toggles.dso label=|t| t.all_dso />
                    <LayerChip on=toggles.dso_galaxy label=|t| t.galaxies>
                        <svg width="14" height="10">
                            <ellipse cx="7" cy="5" rx="6" ry="2.5"
                                     fill="none" stroke="rgba(0,200,220,0.85)" stroke-width="1.2"/>
                        </svg>
                    </LayerChip>
                    <LayerChip on=toggles.dso_open_cluster label=|t| t.open_clusters>
                        <svg width="14" height="14">
                            <circle cx="7" cy="7" r="5.5"
                                    fill="none" stroke="rgba(255,220,50,0.85)" stroke-width="1.2"
                                    stroke-dasharray="3,2"/>
                        </svg>
                    </LayerChip>
                    <LayerChip on=toggles.dso_globular label=|t| t.globular_clusters>
                        <svg width="14" height="14">
                            <circle cx="7" cy="7" r="5.5"
                                    fill="none" stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                            <line x1="1.5" y1="7" x2="12.5" y2="7"
                                  stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                            <line x1="7" y1="1.5" x2="7" y2="12.5"
                                  stroke="rgba(255,160,60,0.85)" stroke-width="1.2"/>
                        </svg>
                    </LayerChip>
                    <LayerChip on=toggles.dso_nebula label=|t| t.nebulae>
                        <svg width="14" height="14">
                            <rect x="1.5" y="1.5" width="11" height="11"
                                  fill="none" stroke="rgba(60,220,100,0.85)" stroke-width="1.2"/>
                        </svg>
                    </LayerChip>
                    <LayerChip on=toggles.dso_planetary label=|t| t.planetary_nebulae>
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
                    </LayerChip>
                    <LayerChip on=toggles.dso_snr label=|t| t.supernova_remnants>
                        <svg width="14" height="14">
                            <rect x="1.5" y="1.5" width="11" height="11"
                                  fill="none" stroke="rgba(60,220,100,0.65)" stroke-width="1.2"
                                  stroke-dasharray="2,2"/>
                        </svg>
                    </LayerChip>
                    <LayerChip on=toggles.dso_galaxy_cluster label=|t| t.galaxy_clusters>
                        <svg width="14" height="14">
                            <circle cx="7" cy="7" r="5.5"
                                    fill="none" stroke="rgba(220,100,220,0.85)" stroke-width="1.2"
                                    stroke-dasharray="2,3"/>
                        </svg>
                    </LayerChip>
                </div>
                <label class="flex items-center gap-2 text-text-muted">
                    <span class="whitespace-nowrap">{move || tr().mag_limit}</span>
                    <input type="range" min="4" max="20" step="0.5" class="flex-1 accent-accent-cyan"
                           prop:value=move || toggles.dso_mag_limit.get().to_string()
                           on:input=move |ev| {
                               if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                   toggles.dso_mag_limit.set(v);
                               }
                           } />
                    <span class="font-mono text-text w-[4ch] text-right">
                        {move || format!("{:.1}", toggles.dso_mag_limit.get())}
                    </span>
                </label>

                <div class=GROUP>{move || tr().layer_gear}</div>
                <div class=ROW>
                    <LayerChip on=toggles.fov label=|t| t.fov />
                    <LayerChip on=toggles.solve_marker label=|t| t.solve_marker />
                    <LayerChip on=toggles.slew_trail label=|t| t.slew_trail />
                    <LayerChip on=toggles.scheduler_jobs label=|t| t.sky_scheduler_jobs />
                </div>

                // ── Observer location (collapsed by default) ──────────────
                <button class="flex items-center justify-between w-full min-h-0 h-9 mt-1 px-0 bg-transparent \
                               border-0 border-t border-solid border-border-strong rounded-none text-text-blue"
                        on:click=move |_| location_open.update(|v| *v = !*v)>
                    <span>{move || tr().location_section}</span>
                    <span>{move || if location_open.get() { "\u{25be}" } else { "\u{25b8}" }}</span>
                </button>
                <Show when=move || location_open.get()>
                    <div class="flex flex-col gap-1.5">
                        <label class=SETTINGS_ROW>
                            {move || tr().latitude_label}
                            <input type="number" step="0.0001" min="-90" max="90"
                                   class=CONTROLS_INPUT
                                   prop:value=move || lat_str.get()
                                   on:input=move |ev| lat_str.set(event_target_value(&ev)) />
                        </label>
                        <label class=SETTINGS_ROW>
                            {move || tr().longitude_label}
                            <input type="number" step="0.0001" min="-180" max="180"
                                   class=CONTROLS_INPUT
                                   prop:value=move || lon_str.get()
                                   on:input=move |ev| lon_str.set(event_target_value(&ev)) />
                        </label>
                        <div class="flex gap-1 flex-wrap">
                            <button
                                class=CONTROLS_BTN
                                prop:disabled=move || !has_mount()
                                on:click=move |_| {
                                    if !has_mount() { return; }
                                    let lat = lat_str.get().parse::<f64>().unwrap_or(0.0);
                                    let lon = lon_str.get().parse::<f64>().unwrap_or(0.0);
                                    send_location.get_value()(lat, lon);
                                }>
                                {move || tr().set_location_btn}
                            </button>
                            <button
                                class=format!("{CONTROLS_BTN} !bg-bg-button-ok !text-accent-green-soft !border-border-ok")
                                prop:disabled=move || !has_mount()
                                on:click=move |_| {
                                    if !has_mount() { return; }
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
                        <Show when=move || !has_mount()>
                            <div class="text-text-muted text-[11px]">
                                {move || tr().location_needs_mount}
                            </div>
                        </Show>
                    </div>
                </Show>
            </div>
        </Show>
    }
}
