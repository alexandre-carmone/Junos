//! Rekos Web UI — milestone 1: fullscreen planetarium only.

mod astro;
mod catalog;
mod compat;
mod coords;
mod components;
mod dso_catalog;
mod ephemeris;
mod gpu;
mod i18n;
mod nebulae;
mod ws;

use std::sync::Arc;

use leptos::prelude::*;

use catalog::CatalogData;
use dso_catalog::DsoCatalogData;
use components::focus::FocusTab;
use components::guide::GuideTab;
use components::imaging::ImagingTab;
use components::polar_align::PolarAlignTab;
use components::scheduler::SchedulerTab;
use components::sky::{SkyTab, SkyTabSwitcher};
use i18n::Lang;
use ws::{AlignDefaultsData, SolveRadius};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab { Sky, Focus, Imaging, PolarAlign, Guide, Scheduler }

#[derive(Clone, Copy)]
pub struct ActiveTabCtx(pub RwSignal<Tab>);

// ---------------------------------------------------------------------------
// Context newtypes kept for sky/actions.rs. They are optional at the call
// site (use_context returns Option) so providing defaults is sufficient.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct MountDeviceCtx(pub RwSignal<Option<String>>);

#[derive(Clone, Copy)]
pub struct CameraDeviceCtx(pub RwSignal<Option<String>>);

#[derive(Clone, Copy)]
pub struct AlignSolveRadiusCtx(pub RwSignal<SolveRadius>);

#[derive(Clone, Copy)]
pub struct AlignDefaultsCtx(pub RwSignal<AlignDefaultsData>);

/// Prefill data passed from the sky right-click menu to the scheduler job builder.
/// Set to Some((name, ra_deg, dec_deg)) when the user clicks "Add to Scheduler".
/// Consumed (and cleared) by SchedulerTab when it opens the job builder.
#[derive(Clone, Copy)]
pub struct SchedulerPrefillCtx(pub RwSignal<Option<(String, f64, f64)>>);

#[derive(Clone, Copy)]
pub struct ServiceBusyCtx {
    pub camera_busy:      Signal<Option<&'static str>>,
    pub mount_busy:       Signal<Option<&'static str>>,
    pub focuser_busy:     Signal<Option<&'static str>>,
    pub dustcap_busy:     Signal<Option<&'static str>>,
    pub light_panel_busy: Signal<Option<&'static str>>,
}

#[component]
fn App() -> impl IntoView {
    console_error_panic_hook::set_once();

    // ── Catalogs ──────────────────────────────────────────────────────────
    let catalog_sig     = RwSignal::new(None::<Arc<CatalogData>>);
    let dso_catalog_sig = RwSignal::new(None::<Arc<DsoCatalogData>>);
    wasm_bindgen_futures::spawn_local({
        let s = catalog_sig;
        async move {
            if let Some(cat) = catalog::fetch_catalog().await { s.set(Some(cat)); }
        }
    });
    wasm_bindgen_futures::spawn_local({
        let s = dso_catalog_sig;
        async move {
            if let Some(cat) = dso_catalog::fetch_dso_catalog().await { s.set(Some(cat)); }
        }
    });
    provide_context(catalog_sig);
    provide_context(dso_catalog_sig);

    // ── WebSocket ─────────────────────────────────────────────────────────
    let (store, send) = ws::use_rekos_ws();

    // ── Sky view state (persisted) ────────────────────────────────────────
    let ls = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    let parse_f64 = |key: &str, default: f64| -> f64 {
        ls.as_ref()
            .and_then(|s| s.get_item(key).ok().flatten())
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let sky_center_alt   = RwSignal::new(parse_f64("sky_center_alt", 45.0));
    let sky_center_az    = RwSignal::new(parse_f64("sky_center_az", 180.0));
    let sky_fov_radius   = RwSignal::new(parse_f64("sky_fov_radius", 45.0));
    let sky_follow_mount = RwSignal::new(
        ls.as_ref()
            .and_then(|s| s.get_item("sky_follow_mount").ok().flatten())
            .map(|v| v != "false")
            .unwrap_or(true),
    );

    // ── Site location ─────────────────────────────────────────────────────
    let site_lat = RwSignal::new(parse_f64("site_latitude", 48.8566));
    let site_lon = RwSignal::new(parse_f64("site_longitude",  2.3522));
    let site = Signal::derive(move || compat::SiteSnapshot {
        latitude:  site_lat.get(),
        longitude: site_lon.get(),
    });

    // ── Language ──────────────────────────────────────────────────────────
    let saved_lang = ls.as_ref()
        .and_then(|s| s.get_item("rekos_lang").ok().flatten())
        .map(|v| if v == "fr" { Lang::Fr } else { Lang::En })
        .unwrap_or_default();
    let lang = RwSignal::new(saved_lang);
    provide_context(lang);
    Effect::new(move |_| {
        let v = match lang.get() { Lang::Fr => "fr", Lang::En => "en" };
        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = s.set_item("rekos_lang", v);
        }
    });

    // ── Derived signals for SkyTab ────────────────────────────────────────
    let mount  = compat::derive_mount(&store);
    let camera = compat::derive_camera(&store);
    let solve  = compat::derive_solve(&store);
    let mosaic = compat::derive_mosaic(&store);
    let focal_length_mm = {
        let ts = store.telescope_settings;
        Signal::derive(move || ts.get().focal_length_mm)
    };

    // ── Scheduler prefill context (sky right-click → Add to Scheduler) ───
    let prefill_ctx = RwSignal::new(None::<(String, f64, f64)>);
    provide_context(SchedulerPrefillCtx(prefill_ctx));

    // ── Stub contexts for sky/actions.rs ──────────────────────────────────
    provide_context(MountDeviceCtx(RwSignal::new(None::<String>)));
    provide_context(CameraDeviceCtx(RwSignal::new(None::<String>)));
    provide_context(AlignSolveRadiusCtx(RwSignal::new(SolveRadius::default())));
    provide_context(AlignDefaultsCtx(RwSignal::new(AlignDefaultsData::default())));
    let none_str: Signal<Option<&'static str>> = Signal::derive(|| None);
    provide_context(ServiceBusyCtx {
        camera_busy:      none_str,
        mount_busy:       none_str,
        focuser_busy:     none_str,
        dustcap_busy:     none_str,
        light_panel_busy: none_str,
    });

    // Mirror mount device name into MountDeviceCtx (for goto dispatch).
    let mount_device_ctx = RwSignal::new(None::<String>);
    provide_context(MountDeviceCtx(mount_device_ctx));
    let ms_sig = store.mount_status;
    Effect::new(move |_| {
        if let Some(ms) = ms_sig.get() {
            if !ms.device.is_empty() { mount_device_ctx.set(Some(ms.device)); }
        }
    });

    // ── Active tab ────────────────────────────────────────────────────────
    // Provided via context so the in-planetarium gear bar (rendered inside
    // SkyTab) can read/write it without SkyTab carrying a prop for it.
    let active_tab = RwSignal::new(Tab::Sky);
    provide_context(ActiveTabCtx(active_tab));
    let sky_visible       = move || active_tab.get() == Tab::Sky;
    let focus_visible     = move || active_tab.get() == Tab::Focus;
    let imaging_visible   = move || active_tab.get() == Tab::Imaging;
    let polar_visible     = move || active_tab.get() == Tab::PolarAlign;
    let guide_visible     = move || active_tab.get() == Tab::Guide;
    let scheduler_visible = move || active_tab.get() == Tab::Scheduler;

    // ── Focus + Imaging + Polar align + Guide + Scheduler tab wiring ──────
    let focus_snapshot     = compat::derive_focus(&store);
    let capture_snapshot   = compat::derive_capture(&store);
    let polar_snapshot     = compat::derive_polar_align(&store);
    let guide_snapshot     = compat::derive_guide(&store);
    let scheduler_snapshot = compat::derive_scheduler(&store);
    let send_focus     = Arc::clone(&send);
    let send_imaging   = Arc::clone(&send);
    let send_polar     = Arc::clone(&send);
    let send_guide     = Arc::clone(&send);
    let send_scheduler = Arc::clone(&send);

    view! {
        <div id="rekos-app" style="position:fixed; inset:0; background:#0a0a0f; color:#c0c0d0; font-family:monospace; overflow:hidden;">
            <div style=move || format!(
                "position:absolute; inset:0; {}",
                if sky_visible() { "" } else { "display:none;" }
            )>
                <SkyTab
                    mount=mount
                    camera=camera
                    site=site
                    solve=solve
                    focal_length_mm=focal_length_mm
                    scheduler=scheduler_snapshot
                    mosaic=mosaic
                    send=Arc::clone(&send)
                    center_alt=sky_center_alt
                    center_az=sky_center_az
                    fov_radius=sky_fov_radius
                    follow_mount=sky_follow_mount
                />
            </div>
            <Show when=focus_visible>
                <div style="position:absolute; inset:0; z-index:40;">
                    <FocusTab
                        focus=focus_snapshot
                        camera=camera
                        send=Arc::clone(&send_focus)
                    />
                </div>
            </Show>
            <Show when=imaging_visible>
                <div style="position:absolute; inset:0; z-index:40;">
                    <ImagingTab
                        capture=capture_snapshot
                        camera=camera
                        send=Arc::clone(&send_imaging)
                    />
                </div>
            </Show>
            <Show when=polar_visible>
                <div style="position:absolute; inset:0; z-index:40;">
                    <PolarAlignTab
                        polar=polar_snapshot
                        mount=mount
                        send=Arc::clone(&send_polar)
                    />
                </div>
            </Show>
            <Show when=guide_visible>
                <div style="position:absolute; inset:0; z-index:40;">
                    <GuideTab
                        guide=guide_snapshot
                        send=Arc::clone(&send_guide)
                    />
                </div>
            </Show>
            <Show when=scheduler_visible>
                <div style="position:absolute; inset:0; z-index:40;">
                    <SchedulerTab
                        scheduler=scheduler_snapshot
                        send=Arc::clone(&send_scheduler)
                    />
                </div>
            </Show>
            // Tab switcher lives at the app root so it stays visible on
            // every tab (SkyTab is hidden when another tab is active).
            <SkyTabSwitcher />
        </div>
    }
}

fn main() {
    leptos::mount::mount_to_body(App);
}
