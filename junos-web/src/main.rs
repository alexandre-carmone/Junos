//! Junos Web UI — the browser app: a 12-tab shell over the Ekos Live
//! wire format, with a WebGPU planetarium on the Sky tab.

/// `leptos::logging::log!`, compiled out of release builds.
///
/// Declared before the `mod` list so every module below can use it.
macro_rules! debug_log {
    ($($t:tt)*) => {
        if cfg!(debug_assertions) {
            leptos::logging::log!($($t)*);
        }
    };
}

mod astro;
mod catalog;
mod compat;
mod coords;
mod components;
mod dom;
mod dso_catalog;
mod ephemeris;
mod healpix;
mod horizon;
mod i18n;
mod net;
mod skymap;
mod star_tiles;
mod ws;
mod ws_helpers;

use std::sync::Arc;

use leptos::prelude::*;

use catalog::CatalogData;
use dso_catalog::DsoCatalogData;
use components::sky::dso_index::DsoIndex;
use components::sky::{FramingState, MosaicParams, MosaicPlannerState};
use components::dialog_modal::DialogModal;
use components::tab_bar::TabBar;
use components::tab_wheel::TabWheel;
use components::tabs::TabContent;
use i18n::Lang;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab { Sky, Targets, Mount, Focus, Imaging, Files, PolarAlign, Guide, Scheduler, Mosaic, Devices, Profiles }

#[derive(Clone, Copy)]
pub struct ActiveTabCtx(pub RwSignal<Tab>);

/// The "Low bandwidth" switch (the `Lite` pill next to EN/FR, persisted to
/// localStorage `junos_lite`). On: the relay re-encodes previews at 960 px /
/// q70 (`junos_media_filter`), the sky skips `tycho.bin`, takes the sky map
/// two orders coarser and the small Milky Way panorama.
#[derive(Clone, Copy)]
pub struct LiteModeCtx(pub RwSignal<bool>);

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

/// App-level context holding the shared mosaic planner signals.
#[derive(Clone, Copy)]
pub struct MosaicPlannerCtx(pub MosaicPlannerState);

/// App-level context for the Framing Assistant overlay. Context rather than
/// props because the sky right-click menu opens it.
#[derive(Clone, Copy)]
pub struct FramingCtx(pub FramingState);

/// The server's sky map (`/api/skymap/info`: a local DSS2 HiPS mirror), or
/// `None` while unanswered or when the server has none. The sky, the Framing
/// Assistant and the Targets thumbnails stream tiles from it — no internet.
#[derive(Clone, Copy)]
pub struct SkyMapCtx(pub RwSignal<Option<Arc<skymap::SkyMapInfo>>>);

/// Prefill data passed from the sky right-click menu to the scheduler job builder.
/// Set to Some((name, ra_deg, dec_deg)), J2000, when the user clicks "Add to Scheduler".
/// Consumed (and cleared) by SchedulerTab when it opens the job builder.
#[derive(Clone, Copy)]
pub struct SchedulerPrefillCtx(pub RwSignal<Option<(String, f64, f64)>>);

/// "Show on Sky" from the Targets tab: the object to centre and open the
/// target card on, and the instant to show it at (`None` keeps the sky's
/// time). Consumed (and cleared) by SkyTab, which is always mounted.
#[derive(Clone)]
pub struct SkyFocus {
    /// Displayed name ("M31 · Andromeda Galaxy").
    pub name: String,
    /// J2000, degrees.
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub size_arcmin: f32,
    pub mag: Option<f32>,
    pub kind: dso_catalog::DsoType,
    pub at_ms: Option<f64>,
}

#[derive(Clone, Copy)]
pub struct SkyFocusCtx(pub RwSignal<Option<SkyFocus>>);

/// The horizon mask (`horizon.rs`): KStars' Artificial Horizon as last
/// reported by `artificial_horizon_get`, seeded from localStorage so the sky
/// and the planning tabs have it before KStars attaches. The Sky tab edits
/// it and writes it back to KStars.
#[derive(Clone, Copy)]
pub struct HorizonCtx(pub RwSignal<Arc<horizon::HorizonMask>>);

/// Names the device a long-running operation is holding, so the sky
/// right-click menu can disable Goto / Goto & Align while it runs.
#[derive(Clone, Copy)]
pub struct ServiceBusyCtx {
    pub camera_busy: Signal<Option<&'static str>>,
    pub mount_busy:  Signal<Option<&'static str>>,
}

/// The filter wheel for every camera-using tab: the Capture train's INDI
/// wheel (names, position) and Ekos' Filter Settings table (exposure, focus
/// offset, autofocus, lock per slot), plus the Build Filter Offsets state.
/// Context rather than props: read by the Imaging / Focus / Mount headers,
/// the Focus picker, the shared sequence cards and the Imaging queue.
#[derive(Clone, Copy)]
pub struct FilterCtx {
    pub wheel: Signal<compat::FilterWheelSnapshot>,
    pub table: RwSignal<Option<ws::FilterManagerData>>,
    pub flat_sync_focus: RwSignal<Option<bool>>,
    /// The Filter settings sheet shows the Build offsets wizard instead of
    /// the table while this is on.
    pub build_offsets_open: RwSignal<bool>,
    pub offsets: RwSignal<Option<ws::FilterOffsetsData>>,
    pub offsets_progress: RwSignal<Option<ws::FilterOffsetsProgress>>,
    pub offsets_phase: RwSignal<ws::BfoPhase>,
    /// The Focus module's state string — the wizard spots a failed autofocus
    /// (KStars then asks on the desktop) through it.
    pub focus_state: Signal<String>,
    /// All INDI devices: the wizard names the wheel KStars' handler picks.
    pub devices: RwSignal<Vec<ws::DeviceInfo>>,
}

impl FilterCtx {
    /// What switching to `label` does besides moving the wheel, from the
    /// table: "+120 · AF" (offset and autofocus), "−45", "AF"; `None` for an
    /// unknown filter or one with neither.
    pub fn hint(&self, label: &str) -> Option<String> {
        if label.is_empty() {
            return None;
        }
        self.table.with(|t| t.as_ref()?.row(label).and_then(filter_hint))
    }
}

/// See [`FilterCtx::hint`].
pub fn filter_hint(row: &ws::FilterRow) -> Option<String> {
    let mut parts = Vec::new();
    if row.offset != 0 {
        parts.push(if row.offset > 0 { format!("+{}", row.offset) } else { format!("\u{2212}{}", -row.offset) });
    }
    if row.autofocus {
        parts.push("AF".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
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
    // The server's sky map. Optional: without a mirror the sky draws symbols
    // over the panorama and the Framing Assistant previews the grid alone.
    let skymap_sig = RwSignal::new(None::<Arc<skymap::SkyMapInfo>>);
    wasm_bindgen_futures::spawn_local({
        let s = skymap_sig;
        async move {
            if let Some(info) = skymap::fetch_skymap_info().await { s.set(Some(info)); }
        }
    });
    provide_context(catalog_sig);
    provide_context(dso_catalog_sig);
    provide_context(dso_index_sig);
    provide_context(SkyMapCtx(skymap_sig));

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
    let mount_snap = compat::derive_mount(&store);
    let mount_device: Signal<Option<String>> = {
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
    provide_context(SkyFocusCtx(RwSignal::new(None)));

    // ── Busy guards for the sky right-click menu ──────────────────────────
    // Goto is refused while the mount is physically moving; Goto & Align also
    // needs the camera idle. Both are derived from the store in `compat.rs`,
    // so the interlock reflects the same state the tabs display. Previously
    // both were a constant `None`, which left the guard permanently open even
    // though `sky/actions.rs` reads and renders it — a long-press on the sky
    // mid-slew fired `mount_goto_rade` and aborted the running slew.
    let (mount_busy, camera_busy) = compat::derive_service_busy(&store);
    provide_context(ServiceBusyCtx { camera_busy, mount_busy });

    // ── Filter wheel ──────────────────────────────────────────────────────
    provide_context(FilterCtx {
        wheel: compat::derive_filter_wheel(&store),
        table: store.filter_manager,
        flat_sync_focus: store.flat_sync_focus,
        build_offsets_open: RwSignal::new(false),
        offsets: store.filter_offsets,
        offsets_progress: store.filter_offsets_progress,
        offsets_phase: store.filter_offsets_phase,
        focus_state: {
            let fs = store.focus_status;
            Signal::derive(move || fs.with(|f| f.as_ref().map(|f| f.status.clone()).unwrap_or_default()))
        },
        devices: store.devices,
    });

    // ── Horizon mask ──────────────────────────────────────────────────────
    // KStars' answer wins over the cached copy as soon as it lands; every
    // value adopted here is written back so the next load starts from it.
    let horizon_ctx = RwSignal::new(Arc::new(horizon::load_cached()));
    provide_context(HorizonCtx(horizon_ctx));
    {
        let from_kstars = store.horizon;
        Effect::new(move |_| {
            if let Some(mask) = from_kstars.get() {
                horizon_ctx.set(Arc::new(mask));
            }
        });
        Effect::new(move |_| horizon_ctx.with(|m| horizon::save_cached(m)));
    }

    // ── Mosaic planner shared state ───────────────────────────────────────
    let mosaic_planner = MosaicPlannerState {
        planning:       RwSignal::new(false),
        picking_center: RwSignal::new(false),
        params: MosaicParams {
            center:  RwSignal::new(None::<(f64, f64)>),
            target:  RwSignal::new(String::new()),
            grid_w:  RwSignal::new(3u32),
            grid_h:  RwSignal::new(3u32),
            overlap: RwSignal::new(10.0f64),
            pa:      RwSignal::new(0.0f64),
        },
    };
    provide_context(MosaicPlannerCtx(mosaic_planner));

    // ── Framing assistant shared state ────────────────────────────────────
    // Grid defaults to 1×1: framing is single-target first, mosaic on demand.
    let framing = FramingState {
        open: RwSignal::new(false),
        params: MosaicParams {
            center:  RwSignal::new(None::<(f64, f64)>),
            target:  RwSignal::new(String::new()),
            grid_w:  RwSignal::new(1u32),
            grid_h:  RwSignal::new(1u32),
            overlap: RwSignal::new(10.0f64),
            pa:      RwSignal::new(0.0f64),
        },
    };
    provide_context(FramingCtx(framing));

    let active_tab = RwSignal::new(Tab::Sky);
    provide_context(ActiveTabCtx(active_tab));

    // ── Low bandwidth ─────────────────────────────────────────────────────
    let lite = components::sky::persisted("junos_lite", false);
    provide_context(LiteModeCtx(lite));

    // Tell the relay which preview kinds this browser displays (`proxy.rs`,
    // `junos_media_filter`): it forwards nothing else, so a phone on the
    // Profiles tab is not sent a 1920 px guide frame every few seconds. The
    // Effect tracks `connected` so the filter is re-sent after a reconnect
    // (`ws/mod.rs` discards what was queued while offline). The same tab
    // read decides the mount-coordinate poll cadence (`ws/retry.rs`).
    {
        let send = send.clone();
        let connected = store.connected;
        let mount_fast_poll = store.mount_fast_poll;
        Effect::new(move |_| {
            let tab = active_tab.get();
            let kinds: &[&str] = match tab {
                Tab::Focus => &["+F"],
                Tab::Guide => &["+G"],
                Tab::PolarAlign => &["+A"],
                Tab::Imaging => &["capture"],
                _ => &[],
            };
            mount_fast_poll.set(matches!(tab, Tab::Sky | Tab::Mount | Tab::PolarAlign));
            let mut payload = serde_json::json!({ "kinds": kinds });
            if lite.get() {
                payload["max_width"] = serde_json::json!(960);
                payload["quality"] = serde_json::json!(70);
            }
            if connected.get() {
                send(serde_json::json!({ "type": "junos_media_filter", "payload": payload }).to_string());
            }
        });
    }

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
