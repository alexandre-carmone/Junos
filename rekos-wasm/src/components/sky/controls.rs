//! Sky controls panel (top-right collapsible settings).

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::compat::{CameraSnapshot, SiteSnapshot};
use crate::astro;
use crate::i18n::{Lang, t};
use crate::ws::SendCmd;

use super::{MosaicPlannerState, SkyToggles};
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
    show_mosaic_section: ReadSignal<bool>,
    set_show_mosaic_section: WriteSignal<bool>,
    toggles: SkyToggles,
    focal_override: ReadSignal<String>,
    set_focal_override: WriteSignal<String>,
    focal_length_mm: Signal<Option<f64>>,
    set_follow_mount: WriteSignal<bool>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    set_site_location: Arc<dyn Fn(f64, f64) + Send + Sync>,
    planner: MosaicPlannerState,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let s = site.get_untracked();
    let lat_str = RwSignal::new(format!("{:.4}", s.latitude));
    let lon_str = RwSignal::new(format!("{:.4}", s.longitude));
    let send_location = StoredValue::new(Arc::clone(&set_site_location));
    let send_mosaic   = StoredValue::new(Arc::clone(&send));

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
                                <input type="checkbox" prop:checked=move || toggles.stars.get()
                                       on:change=move |ev| toggles.stars.set(event_target_checked(&ev)) />
                                {move || tr().stars_checkbox}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.names.get()
                                       on:change=move |ev| toggles.names.set(event_target_checked(&ev)) />
                                {move || tr().names_checkbox}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.constellations.get()
                                       on:change=move |ev| toggles.constellations.set(event_target_checked(&ev)) />
                                {move || tr().constellations}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer; padding-left:16px;">
                                <input type="checkbox" prop:checked=move || toggles.con_names.get()
                                       on:change=move |ev| toggles.con_names.set(event_target_checked(&ev)) />
                                {move || tr().names_checkbox}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.grid.get()
                                       on:change=move |ev| toggles.grid.set(event_target_checked(&ev)) />
                                {move || tr().grid}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.eq_grid.get()
                                       on:change=move |ev| toggles.eq_grid.set(event_target_checked(&ev)) />
                                {move || tr().eq_grid}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.meridian.get()
                                       on:change=move |ev| toggles.meridian.set(event_target_checked(&ev)) />
                                {move || tr().meridian}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.ecliptic.get()
                                       on:change=move |ev| toggles.ecliptic.set(event_target_checked(&ev)) />
                                {move || tr().ecliptic}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.zenith.get()
                                       on:change=move |ev| toggles.zenith.set(event_target_checked(&ev)) />
                                {move || tr().zenith}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.fov.get()
                                       on:change=move |ev| toggles.fov.set(event_target_checked(&ev)) />
                                {move || tr().fov}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.solve_marker.get()
                                       on:change=move |ev| toggles.solve_marker.set(event_target_checked(&ev)) />
                                {move || tr().solve_marker}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.slew_trail.get()
                                       on:change=move |ev| toggles.slew_trail.set(event_target_checked(&ev)) />
                                {move || tr().slew_trail}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer; border-top:1px solid #2a2a3a; padding-top:4px; margin-top:2px;">
                                <input type="checkbox" prop:checked=move || toggles.scheduler_jobs.get()
                                       on:change=move |ev| toggles.scheduler_jobs.set(event_target_checked(&ev)) />
                                {"Scheduler jobs"}
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
                                <input type="checkbox" prop:checked=move || toggles.dso.get()
                                       on:change=move |ev| toggles.dso.set(event_target_checked(&ev)) />
                                {move || tr().all_dso}
                            </label>
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.solar_system.get()
                                       on:change=move |ev| toggles.solar_system.set(event_target_checked(&ev)) />
                                {move || tr().solar_system}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.dso_galaxy.get()
                                       on:change=move |ev| toggles.dso_galaxy.set(event_target_checked(&ev)) />
                                <svg width="14" height="10" style="flex-shrink:0;">
                                    <ellipse cx="7" cy="5" rx="6" ry="2.5"
                                             fill="none" stroke="rgba(0,200,220,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().galaxies}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.dso_open_cluster.get()
                                       on:change=move |ev| toggles.dso_open_cluster.set(event_target_checked(&ev)) />
                                <svg width="14" height="14" style="flex-shrink:0;">
                                    <circle cx="7" cy="7" r="5.5"
                                            fill="none" stroke="rgba(255,220,50,0.85)" stroke-width="1.2"
                                            stroke-dasharray="3,2"/>
                                </svg>
                                {move || tr().open_clusters}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.dso_globular.get()
                                       on:change=move |ev| toggles.dso_globular.set(event_target_checked(&ev)) />
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
                                <input type="checkbox" prop:checked=move || toggles.dso_nebula.get()
                                       on:change=move |ev| toggles.dso_nebula.set(event_target_checked(&ev)) />
                                <svg width="14" height="14" style="flex-shrink:0;">
                                    <rect x="1.5" y="1.5" width="11" height="11"
                                          fill="none" stroke="rgba(60,220,100,0.85)" stroke-width="1.2"/>
                                </svg>
                                {move || tr().nebulae}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.dso_planetary.get()
                                       on:change=move |ev| toggles.dso_planetary.set(event_target_checked(&ev)) />
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
                                <input type="checkbox" prop:checked=move || toggles.dso_snr.get()
                                       on:change=move |ev| toggles.dso_snr.set(event_target_checked(&ev)) />
                                <svg width="14" height="14" style="flex-shrink:0;">
                                    <rect x="1.5" y="1.5" width="11" height="11"
                                          fill="none" stroke="rgba(60,220,100,0.65)" stroke-width="1.2"
                                          stroke-dasharray="2,2"/>
                                </svg>
                                {move || tr().supernova_remnants}
                            </label>
                            <label style="display:flex; align-items:center; gap:5px; cursor:pointer;">
                                <input type="checkbox" prop:checked=move || toggles.dso_galaxy_cluster.get()
                                       on:change=move |ev| toggles.dso_galaxy_cluster.set(event_target_checked(&ev)) />
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

                    // ── Part 4 : Mosaic Planner ────────────────────────
                    <button style=section_hdr
                            on:click=move |_| set_show_mosaic_section.update(|v| *v = !*v)>
                        {"Mosaic Planner"}
                        {move || if show_mosaic_section.get() { "\u{25be}" } else { "\u{25b8}" }}
                    </button>
                    {move || show_mosaic_section.get().then(|| {
                        let p2 = planner;
                        let cam2 = camera;
                        let fl2 = focal_length_mm;
                        view! {
                        <div style="display:flex; flex-direction:column; gap:6px; padding:6px 10px; border-bottom:1px solid #2a2a3a;">

                            // Planning mode toggle
                            <label style="display:flex; align-items:center; gap:6px; cursor:pointer; font-size:11px;">
                                <input type="checkbox" prop:checked=move || p2.planning.get()
                                       on:change=move |ev| p2.planning.set(event_target_checked(&ev)) />
                                {move || if p2.planning.get() { "Click sky to set center" } else { "Enable planning mode" }}
                            </label>

                            // Center display
                            {move || p2.center.get().map(|(ra_deg, dec_deg)| {
                                let ra_h = ra_deg / 15.0;
                                let rah  = ra_h as u32;
                                let ram  = ((ra_h - rah as f64) * 60.0).abs() as u32;
                                let dec_s = if dec_deg < 0.0 { "-" } else { "+" };
                                let dec_abs = dec_deg.abs();
                                let decd = dec_abs as u32;
                                let decm = ((dec_abs - decd as f64) * 60.0) as u32;
                                view! {
                                    <div style="font-size:10px; color:#88aaff; font-family:monospace;">
                                        {format!("Center: {:02}h{:02}m  {}{}\u{00b0}{:02}'", rah, ram, dec_s, decd, decm)}
                                    </div>
                                }
                            })}

                            // Target name
                            <label style="font-size:11px; display:flex; align-items:center; gap:4px;">
                                {"Target: "}
                                <input type="text"
                                       style="flex:1; background:#111; color:#ccc; border:1px solid #444; \
                                              font-family:monospace; font-size:11px; padding:2px 4px;"
                                       prop:value=move || p2.target.get()
                                       on:input=move |ev| p2.target.set(event_target_value(&ev)) />
                            </label>

                            // Grid size
                            <div style="font-size:11px; display:flex; align-items:center; gap:4px;">
                                {"Grid: "}
                                <input type="number" min="1" max="10"
                                       style="width:40px; background:#111; color:#ccc; border:1px solid #444; \
                                              font-family:monospace; font-size:11px; padding:2px; text-align:center;"
                                       prop:value=move || p2.grid_w.get().to_string()
                                       on:input=move |ev| {
                                           if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                               p2.grid_w.set(v.clamp(1, 10));
                                           }
                                       } />
                                {" \u{00d7} "}
                                <input type="number" min="1" max="10"
                                       style="width:40px; background:#111; color:#ccc; border:1px solid #444; \
                                              font-family:monospace; font-size:11px; padding:2px; text-align:center;"
                                       prop:value=move || p2.grid_h.get().to_string()
                                       on:input=move |ev| {
                                           if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                               p2.grid_h.set(v.clamp(1, 10));
                                           }
                                       } />
                            </div>

                            // Overlap + PA
                            <div style="font-size:11px; display:flex; align-items:center; gap:8px; flex-wrap:wrap;">
                                <label style="display:flex; align-items:center; gap:4px;">
                                    {"Overlap: "}
                                    <input type="number" min="0" max="50" step="1"
                                           style="width:44px; background:#111; color:#ccc; border:1px solid #444; \
                                                  font-family:monospace; font-size:11px; padding:2px;"
                                           prop:value=move || format!("{:.0}", p2.overlap.get())
                                           on:input=move |ev| {
                                               if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                                   p2.overlap.set(v.clamp(0.0, 50.0));
                                               }
                                           } />
                                    {" %"}
                                </label>
                                <label style="display:flex; align-items:center; gap:4px;">
                                    {"PA: "}
                                    <input type="number" min="-180" max="180" step="1"
                                           style="width:50px; background:#111; color:#ccc; border:1px solid #444; \
                                                  font-family:monospace; font-size:11px; padding:2px;"
                                           prop:value=move || format!("{:.0}", p2.pa.get())
                                           on:input=move |ev| {
                                               if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                                   p2.pa.set(v);
                                               }
                                           } />
                                    {"\u{00b0}"}
                                </label>
                            </div>

                            // Sequence file
                            <label style="font-size:11px; display:flex; align-items:center; gap:4px;">
                                {"Sequence: "}
                                <input type="text"
                                       style="flex:1; background:#111; color:#ccc; border:1px solid #444; \
                                              font-family:monospace; font-size:11px; padding:2px 4px; min-width:0;"
                                       placeholder="/path/to/seq.esq"
                                       prop:value=move || p2.seq_file.get()
                                       on:input=move |ev| p2.seq_file.set(event_target_value(&ev)) />
                            </label>

                            // Output directory
                            <label style="font-size:11px; display:flex; align-items:center; gap:4px;">
                                {"Output dir: "}
                                <input type="text"
                                       style="flex:1; background:#111; color:#ccc; border:1px solid #444; \
                                              font-family:monospace; font-size:11px; padding:2px 4px; min-width:0;"
                                       placeholder="~/observations"
                                       prop:value=move || p2.dir.get()
                                       on:input=move |ev| p2.dir.set(event_target_value(&ev)) />
                            </label>

                            // FOV hint from camera
                            {move || {
                                let cam = cam2.get();
                                let fl  = fl2.get();
                                if let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) =
                                    (fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height)
                                {
                                    let fw = astro::fov_deg(fl_mm, sw as f64, px_um) * 60.0;
                                    let fh = astro::fov_deg(fl_mm, sh as f64, px_um) * 60.0;
                                    Some(view! {
                                        <div style="font-size:10px; color:#556; font-family:monospace;">
                                            {format!("Tile FOV: {fw:.0}\u{2019}\u{00d7}{fh:.0}\u{2019}")}
                                        </div>
                                    })
                                } else {
                                    None
                                }
                            }}

                            // Import button
                            <button
                                style=move || {
                                    let enabled = p2.center.get().is_some()
                                        && !p2.seq_file.get().is_empty();
                                    if enabled {
                                        "background:#0a1a2a; color:#88aaff; border:1px solid #446; \
                                         padding:5px 10px; cursor:pointer; font-family:monospace; \
                                         font-size:11px; font-weight:bold; border-radius:3px; width:100%;"
                                    } else {
                                        "background:#111; color:#445; border:1px solid #333; \
                                         padding:5px 10px; cursor:not-allowed; font-family:monospace; \
                                         font-size:11px; font-weight:bold; border-radius:3px; width:100%;"
                                    }
                                }
                                disabled=move || p2.center.get().is_none() || p2.seq_file.get().is_empty()
                                on:click=move |_| {
                                    let Some((center_ra_deg, center_dec_deg)) = p2.center.get_untracked() else { return };
                                    let cam = cam2.get_untracked();
                                    let fl  = fl2.get_untracked();
                                    let gw  = p2.grid_w.get_untracked();
                                    let gh  = p2.grid_h.get_untracked();
                                    let overlap = p2.overlap.get_untracked();
                                    let pa  = p2.pa.get_untracked();
                                    let seq = p2.seq_file.get_untracked();
                                    let dir = p2.dir.get_untracked();
                                    let target = p2.target.get_untracked();

                                    // Build tile CSV: PA,RA(deg),DEC(deg)
                                    let mut csv = String::from("PA,RA,DEC\n");
                                    if let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) =
                                        (fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height)
                                    {
                                        let fov_w = astro::fov_deg(fl_mm, sw as f64, px_um);
                                        let fov_h = astro::fov_deg(fl_mm, sh as f64, px_um);
                                        let cos_dec = center_dec_deg.to_radians().cos().abs().max(0.01);
                                        let step_ra  = fov_w * (1.0 - overlap / 100.0) / cos_dec;
                                        let step_dec = fov_h * (1.0 - overlap / 100.0);
                                        for row in 0..gh {
                                            for col in 0..gw {
                                                let ra  = center_ra_deg + (col as f64 - (gw as f64 - 1.0) / 2.0) * step_ra;
                                                let dec = center_dec_deg + (row as f64 - (gh as f64 - 1.0) / 2.0) * step_dec;
                                                csv.push_str(&format!("{pa:.2},{ra:.6},{dec:.6}\n"));
                                            }
                                        }
                                    }

                                    send_mosaic.get_value()(serde_json::json!({
                                        "type": "scheduler_import_mosaic",
                                        "payload": {
                                            "csv":    csv,
                                            "sequence": seq,
                                            "target": target,
                                            "directory": dir,
                                            "track": true,
                                            "focus": false,
                                            "align": false,
                                            "guide": true,
                                            "completionCondition": "sequence",
                                            "completionConditionArg": "1"
                                        }
                                    }).to_string());
                                }>
                                {"Import to Scheduler"}
                            </button>
                        </div>
                        }
                    })}

                </div>
                }
            })}
        </div>
    }
}
