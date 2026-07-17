//! Junos Web UI — milestone 1: fullscreen planetarium only.

mod astro;
mod catalog;
mod compat;
mod coords;
mod components;
mod dso_catalog;
mod dso_tiles;
mod ephemeris;
mod i18n;
/// Unreferenced since the sky map dropped the image-footprint quads: DSO
/// symbols now come from the catalog's own MajAx/MinAx/PosAng, and framing
/// mode fetches its preview from hips2fits. Kept with `public/nebulae*` for
/// whoever wants the thumbnails back.
#[allow(dead_code)]
mod nebulae;
mod ws;
mod ws_helpers;

use std::sync::Arc;

use leptos::prelude::*;

use catalog::CatalogData;
use dso_catalog::DsoCatalogData;
use components::sky::dso_index::DsoIndex;
use components::sky::{FramingState, MosaicPlannerState};
use components::dialog_modal::DialogModal;
use components::tab_bar::TabBar;
use components::tab_wheel::TabWheel;
use components::tabs::TabContent;
use i18n::Lang;
use ws::{AlignDefaultsData, SolveRadius};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab { Sky, Mount, Focus, Imaging, Files, PolarAlign, Guide, Scheduler, Mosaic, FlatCal, Devices, Profiles }

#[derive(Clone, Copy)]
pub struct ActiveTabCtx(pub RwSignal<Tab>);

/// Whether the tab wheel/bar show a text label under each tab icon.
/// Off by default; persisted to localStorage key `tab_labels`.
#[derive(Clone, Copy)]
pub struct TabLabelsCtx(pub RwSignal<bool>);

/// Cross-tab bridge used by the Imaging tab's "Reveal in Files" button.
/// Payload is an optional absolute path (from KStars capture settings);
/// if None, the Files tab just switches to the captures root. The Files
/// tab consumes this on mount, resolves the path against the server-side
/// sandbox via `/api/files/resolve`, and navigates.
#[derive(Clone, Copy)]
pub struct RevealInFilesCtx(pub RwSignal<Option<String>>);

/// Server-side captures directory (from `--captures-dir`), fetched once from
/// `/api/config`. Used as the default "Destination folder" in the sequencer
/// forms so captured `.fits` land where the Files tab can browse them.
#[derive(Clone, Copy)]
pub struct CaptureDirCtx(pub RwSignal<String>);

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

/// App-level context for the Framing Assistant overlay. Context rather than
/// props because the sky right-click menu opens it.
#[derive(Clone, Copy)]
pub struct FramingCtx(pub FramingState);

/// Index of pre-downloaded DSO survey tiles, or `None` until `/api/dso_tiles`
/// answers. Lets the Framing Assistant preview a target with no internet.
#[derive(Clone, Copy)]
pub struct DsoTilesCtx(pub RwSignal<Option<Arc<dso_tiles::DsoTileIndex>>>);

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
    // Offline survey tiles for the Framing Assistant. Optional: an absent or
    // empty cache just means framing always uses the live hips2fits proxy.
    let dso_tiles_sig = RwSignal::new(None::<Arc<dso_tiles::DsoTileIndex>>);
    wasm_bindgen_futures::spawn_local({
        let s = dso_tiles_sig;
        async move {
            if let Some(idx) = dso_tiles::fetch_dso_tile_index().await { s.set(Some(idx)); }
        }
    });
    provide_context(catalog_sig);
    provide_context(dso_catalog_sig);
    provide_context(dso_index_sig);
    provide_context(DsoTilesCtx(dso_tiles_sig));

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

    // ── Tab labels toggle (persisted, off by default) ─────────────────────
    let show_tab_labels = RwSignal::new(
        ls.as_ref()
            .and_then(|s| s.get_item("tab_labels").ok().flatten())
            .map(|v| v == "true")
            .unwrap_or(false),
    );
    provide_context(TabLabelsCtx(show_tab_labels));
    Effect::new(move |_| {
        let v = if show_tab_labels.get() { "true" } else { "false" };
        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = s.set_item("tab_labels", v);
        }
    });

    // ── Site location ─────────────────────────────────────────────────────
    let site_lat = RwSignal::new(parse_f64("site_latitude", 48.8566));
    let site_lon = RwSignal::new(parse_f64("site_longitude",  2.3522));
    let site = Signal::derive(move || compat::SiteSnapshot {
        latitude:  site_lat.get(),
        longitude: site_lon.get(),
    });
    // KStars is the single source of truth for the observer site: whenever it
    // answers our `astro_get_location` prime (or the server's periodic re-poll),
    // adopt its lat/lon and persist. A manual entry (below) writes KStars' truth
    // via the mount, so it too flows back through here.
    {
        let store_site = store.site;
        Effect::new(move |_| {
            if let Some(s) = store_site.get() {
                site_lat.set(s.latitude);
                site_lon.set(s.longitude);
                if let Some(ls) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
                    let _ = ls.set_item("site_latitude", &s.latitude.to_string());
                    let _ = ls.set_item("site_longitude", &s.longitude.to_string());
                }
            }
        });
    }

    // Connected mount device name, or None. Used as the GEOGRAPHIC_COORD write
    // target and to gate (grey out) the location controls: KStars only accepts a
    // location pushed through a connected device that is its `locationSource`.
    let mount_device: Signal<Option<String>> = {
        let mount_snap = compat::derive_mount(&store);
        Signal::derive(move || {
            let m = mount_snap.get();
            if m.connected {
                m.device_name.filter(|d| !d.is_empty() && d != "--")
            } else {
                None
            }
        })
    };

    // Manual location writer (GPS button / lat-lon inputs). Pushes the new site
    // into KStars' live location via the mount (option_set locationSource +
    // GEOGRAPHIC_COORD write); the server's astro_get_location poll then flows it
    // back through the Effect above (KStars stays the source). Also optimistically
    // moves the map now so the user gets immediate feedback. No-op without a mount
    // (the controls are greyed then).
    let set_site_location: Arc<dyn Fn(f64, f64) + Send + Sync> = {
        let send = Arc::clone(&send);
        Arc::new(move |lat, lon| {
            let Some(dev) = mount_device.get_untracked() else { return };
            site_lat.set(lat);
            site_lon.set(lon);
            ws_helpers::push_site_to_kstars(&send, &dev, lat, lon);
        })
    };

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

    // ── Framing assistant shared state ────────────────────────────────────
    // Grid defaults to 1×1: framing is single-target first, mosaic on demand.
    let framing = FramingState {
        open:    RwSignal::new(false),
        center:  RwSignal::new(None::<(f64, f64)>),
        target:  RwSignal::new(String::new()),
        grid_w:  RwSignal::new(1u32),
        grid_h:  RwSignal::new(1u32),
        overlap: RwSignal::new(10.0f64),
        pa:      RwSignal::new(0.0f64),
    };
    provide_context(FramingCtx(framing));

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

    // Default capture destination folder, fetched once from the server. Used
    // by the sequencer forms as the initial "Destination folder" value.
    let capture_dir = RwSignal::new(String::new());
    provide_context(CaptureDirCtx(capture_dir));
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(resp) = gloo_net::http::Request::get("/api/config").send().await {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                if let Some(d) = v.get("captures_dir").and_then(|d| d.as_str()) {
                    if !d.is_empty() {
                        capture_dir.set(d.to_string());
                    }
                }
            }
        }
    });

    view! {
        <div id="junos-app" class="fixed inset-0 bg-bg text-[var(--text)] font-ui overflow-hidden">
            <TabContent
                store=store.clone()
                send=Arc::clone(&send)
                site=site
                set_site_location=set_site_location
                mount_device=mount_device
                sky_center_alt=sky_center_alt
                sky_center_az=sky_center_az
                sky_fov_radius=sky_fov_radius
                sky_follow_mount=sky_follow_mount
            />
            // Tab switcher lives at the app root so it stays visible on
            // every tab (SkyTab is hidden when another tab is active).
            <TabWheel />
            <TabBar />
            // KStars-side modal mirror — surfaces KSMessageBox prompts so
            // the user can answer them from the browser instead of having
            // to switch to the desktop KStars window.
            <DialogModal dialog=store.active_dialog send=Arc::clone(&send) />
        </div>
    }
}

fn main() {
    leptos::mount::mount_to_body(App);
}
