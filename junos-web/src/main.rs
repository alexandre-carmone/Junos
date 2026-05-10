//! Junos Web UI — milestone 1: fullscreen planetarium only.

mod astro;
mod catalog;
mod compat;
mod coords;
mod components;
mod dso_catalog;
mod ephemeris;
mod i18n;
mod nebulae;
mod ws;
mod ws_helpers;

use std::sync::Arc;

use leptos::prelude::*;

use catalog::CatalogData;
use dso_catalog::DsoCatalogData;
use components::sky::dso_index::DsoIndex;
use components::sky::MosaicPlannerState;
use components::tab_bar::TabBar;
use components::tab_wheel::TabWheel;
use components::tabs::TabContent;
use i18n::Lang;
use ws::{AlignDefaultsData, SolveRadius};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab { Sky, Mount, Focus, Imaging, Files, PolarAlign, Guide, Scheduler, Mosaic, Profiles }

#[derive(Clone, Copy)]
pub struct ActiveTabCtx(pub RwSignal<Tab>);

/// Cross-tab bridge used by the Imaging tab's "Reveal in Files" button.
/// Payload is an optional absolute path (from KStars capture settings);
/// if None, the Files tab just switches to the captures root. The Files
/// tab consumes this on mount, resolves the path against the server-side
/// sandbox via `/api/files/resolve`, and navigates.
#[derive(Clone, Copy)]
pub struct RevealInFilesCtx(pub RwSignal<Option<String>>);

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

/// App-level context holding the shared mosaic planner signals.
#[derive(Clone, Copy)]
pub struct MosaicPlannerCtx(pub MosaicPlannerState);

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
    let dso_index_sig   = RwSignal::new(None::<Arc<DsoIndex>>);
    wasm_bindgen_futures::spawn_local({
        let s = catalog_sig;
        async move {
            if let Some(cat) = catalog::fetch_catalog().await { s.set(Some(cat)); }
        }
    });
    wasm_bindgen_futures::spawn_local({
        let s   = dso_catalog_sig;
        let idx = dso_index_sig;
        async move {
            if let Some(cat) = dso_catalog::fetch_dso_catalog().await {
                // Build the spatial index once at load — lookup cost is
                // amortised over every subsequent frame.
                idx.set(Some(components::sky::dso_index::build_arc(&cat)));
                s.set(Some(cat));
            }
        }
    });
    provide_context(catalog_sig);
    provide_context(dso_catalog_sig);
    provide_context(dso_index_sig);

    // ── WebSocket ─────────────────────────────────────────────────────────
    let (store, send) = ws::use_junos_ws();

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
        .and_then(|s| s.get_item("junos_lang").ok().flatten())
        .map(|v| if v == "fr" { Lang::Fr } else { Lang::En })
        .unwrap_or_default();
    let lang = RwSignal::new(saved_lang);
    provide_context(lang);
    Effect::new(move |_| {
        let v = match lang.get() { Lang::Fr => "fr", Lang::En => "en" };
        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = s.set_item("junos_lang", v);
        }
    });

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
    // ── Mosaic planner shared state ───────────────────────────────────────
    let mosaic_planner = MosaicPlannerState {
        planning:       RwSignal::new(false),
        picking_center: RwSignal::new(false),
        center:         RwSignal::new(None::<(f64, f64)>),
        grid_w:         RwSignal::new(3u32),
        grid_h:         RwSignal::new(3u32),
        overlap:        RwSignal::new(10.0f64),
        pa:             RwSignal::new(0.0f64),
        target:         RwSignal::new(String::new()),
        dir:            RwSignal::new(String::new()),
    };
    provide_context(MosaicPlannerCtx(mosaic_planner));

    let active_tab = RwSignal::new(Tab::Sky);
    provide_context(ActiveTabCtx(active_tab));

    // One-shot startup tab selection: once we hear back from the server
    // whether KStars is running, switch to Profiles if it isn't. After
    // that first decision we never auto-switch again — if the user is
    // on Sky and quits KStars mid-session, they stay on Sky.
    {
        let known = store.kstars_state_known;
        let running = store.kstars_running;
        let decided = std::rc::Rc::new(std::cell::Cell::new(false));
        Effect::new(move |_| {
            if decided.get() { return; }
            if !known.get() { return; }
            decided.set(true);
            if !running.get_untracked() {
                active_tab.set(Tab::Profiles);
            }
        });
    }

    // Cross-tab Reveal-in-Files trigger. Produced by the Imaging tab's
    // "Reveal in Files" button (with the current capture directory /
    // last frame filename), consumed by the Files tab on mount.
    let reveal_ctx = RwSignal::new(None::<String>);
    provide_context(RevealInFilesCtx(reveal_ctx));

    view! {
        <div id="junos-app" class="fixed inset-0 bg-bg text-[var(--text)] font-ui overflow-hidden">
            <TabContent
                store=store.clone()
                send=Arc::clone(&send)
                site=site
                sky_center_alt=sky_center_alt
                sky_center_az=sky_center_az
                sky_fov_radius=sky_fov_radius
                sky_follow_mount=sky_follow_mount
            />
            // Tab switcher lives at the app root so it stays visible on
            // every tab (SkyTab is hidden when another tab is active).
            <TabWheel />
            <TabBar />
        </div>
    }
}

fn main() {
    leptos::mount::mount_to_body(App);
}
