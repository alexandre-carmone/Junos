//! Rekos Web UI — milestone 1: fullscreen planetarium only.

mod astro;
mod catalog;
mod compat;
mod coords;
mod components;
mod dso_catalog;
mod gpu;
mod i18n;
mod nebulae;
mod ws;

use std::sync::Arc;

use leptos::prelude::*;

use catalog::CatalogData;
use dso_catalog::DsoCatalogData;
use components::focus::FocusTab;
use components::sky::{SkyTab, SkyTabSwitcher};
use i18n::Lang;
use ws::{AlignDefaultsData, SolveRadius};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab { Sky, Focus }

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

    // ── Derived signals for SkyTab ────────────────────────────────────────
    let mount  = compat::derive_mount(&store);
    let camera = compat::derive_camera(&store);
    let solve  = Signal::derive(|| compat::SolveSnapshot::default());
    let focal_length_mm = {
        let ts = store.telescope_settings;
        Signal::derive(move || ts.get().focal_length_mm)
    };

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
    let focus_visible = move || active_tab.get() == Tab::Focus;

    // ── Focus tab wiring ──────────────────────────────────────────────────
    let focus_snapshot = compat::derive_focus(&store);
    let send_focus = Arc::clone(&send);

    view! {
        <div id="rekos-app" style="position:fixed; inset:0; background:#0a0a0f; color:#c0c0d0; font-family:monospace; overflow:hidden;">
            <div style=move || format!(
                "position:absolute; inset:0; {}",
                if focus_visible() { "display:none;" } else { "" }
            )>
                <SkyTab
                    mount=mount
                    camera=camera
                    site=site
                    solve=solve
                    focal_length_mm=focal_length_mm
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
            // Tab switcher lives at the app root so it stays visible on
            // every tab (SkyTab is hidden when another tab is active).
            <SkyTabSwitcher />
        </div>
    }
}

fn main() {
    leptos::mount::mount_to_body(App);
}
