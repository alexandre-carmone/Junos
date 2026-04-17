//! Sky map / planetarium tab — canvas-based interactive star chart.
//!
//! When WebGPU is available, uses a two-canvas stack:
//!   - Bottom: WebGPU canvas (sky bg + compute-projected stars + constellation lines)
//!   - Top:    Canvas2D overlay (ground, horizon, grid, names, crosshair, FOV, info)
//!
//! Falls back to all-Canvas2D rendering when WebGPU is unavailable.

mod actions;
mod controls;
mod render;
mod search;
pub(crate) mod utils;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, HtmlImageElement, MouseEvent, TouchEvent,
    WheelEvent,
};

use crate::compat::{CameraSnapshot, MountSnapshot, SiteSnapshot, SolveSnapshot};
use crate::ws::SendCmd;
use crate::{ActiveTabCtx, Tab};

use crate::astro;
use crate::coords::JNow;
use crate::catalog::CatalogData;
use crate::dso_catalog::DsoCatalogData;
use crate::gpu::{GpuSkyRenderer, Uniforms};
use crate::i18n::{Lang, t};
use crate::nebulae::NebulaeIndex;

use actions::{SkyConfirmPopup, SkyContextMenu, open_confirm};
use controls::SkyControls;
use render::RenderParams;
use search::SkySearch;
use utils::event_target_value;

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

#[component]
pub fn SkyTab(
    #[prop(into)] mount: Signal<MountSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    #[prop(into)] solve: Signal<SolveSnapshot>,
    #[prop(into)] focal_length_mm: Signal<Option<f64>>,
    #[prop(into)] send: SendCmd,
    center_alt: RwSignal<f64>,
    center_az: RwSignal<f64>,
    fov_radius: RwSignal<f64>,
    follow_mount: RwSignal<bool>,
) -> impl IntoView {
    // ── Local reactive state ───────────────────────────────────────────────
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let catalog_sig = use_context::<RwSignal<Option<Arc<CatalogData>>>>()
        .unwrap_or_else(|| RwSignal::new(None));
    let dso_catalog_sig = use_context::<RwSignal<Option<Arc<DsoCatalogData>>>>()
        .unwrap_or_else(|| RwSignal::new(None));

    let (center_alt, set_center_alt) = (center_alt.read_only(), center_alt.write_only());
    let (center_az, set_center_az) = (center_az.read_only(), center_az.write_only());
    let (fov_radius, set_fov_radius) = (fov_radius.read_only(), fov_radius.write_only());
    let (follow_mount, set_follow_mount) = (follow_mount.read_only(), follow_mount.write_only());

    // Persist sky view state to localStorage on change
    Effect::new(move || {
        let alt = center_alt.get();
        let az = center_az.get();
        let fov = fov_radius.get();
        let follow = follow_mount.get();
        if let Some(ls) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = ls.set_item("sky_center_alt", &alt.to_string());
            let _ = ls.set_item("sky_center_az", &az.to_string());
            let _ = ls.set_item("sky_fov_radius", &fov.to_string());
            let _ = ls.set_item("sky_follow_mount", if follow { "true" } else { "false" });
        }
    });

    let (time_offset_s, set_time_offset_s) = signal(0.0_f64);
    // Persist focal length override in localStorage
    let stored_fl = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|ls| ls.get_item("sky_focal_override").ok().flatten())
        .unwrap_or_default();
    let (focal_override, set_focal_override) = signal(stored_fl);
    // Read persisted checkbox state from localStorage
    let ls_init = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    let ls_bool = |key: &str, default: bool| -> bool {
        ls_init.as_ref()
            .and_then(|s| s.get_item(key).ok().flatten())
            .map(|v| v == "true")
            .unwrap_or(default)
    };
    let ls_f64_init = |key: &str, default: f64| -> f64 {
        ls_init.as_ref()
            .and_then(|s| s.get_item(key).ok().flatten())
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(default)
    };

    let (show_stars, set_show_stars) = signal(ls_bool("sky_show_stars", true));
    let (show_names, set_show_names) = signal(ls_bool("sky_show_names", true));
    let (show_constellations, set_show_constellations) = signal(ls_bool("sky_show_constellations", true));
    let (show_con_names, set_show_con_names) = signal(ls_bool("sky_show_con_names", true));
    let (show_grid, set_show_grid) = signal(ls_bool("sky_show_grid", true));
    let (show_eq_grid, set_show_eq_grid) = signal(ls_bool("sky_show_eq_grid", false));
    let (show_meridian, set_show_meridian) = signal(ls_bool("sky_show_meridian", true));
    let (show_fov, set_show_fov) = signal(ls_bool("sky_show_fov", true));
    let (show_dso, set_show_dso) = signal(ls_bool("sky_show_dso", true));

    let (dso_filter_galaxy, set_dso_filter_galaxy) = signal(ls_bool("sky_dso_galaxy", true));
    let (dso_filter_open_cluster, set_dso_filter_open_cluster) = signal(ls_bool("sky_dso_open_cluster", true));
    let (dso_filter_globular, set_dso_filter_globular) = signal(ls_bool("sky_dso_globular", true));
    let (dso_filter_nebula, set_dso_filter_nebula) = signal(ls_bool("sky_dso_nebula", true));
    let (dso_filter_planetary, set_dso_filter_planetary) = signal(ls_bool("sky_dso_planetary", true));
    let (dso_filter_snr, set_dso_filter_snr) = signal(ls_bool("sky_dso_snr", true));
    let (dso_filter_galaxy_cluster, set_dso_filter_galaxy_cluster) = signal(ls_bool("sky_dso_galaxy_cluster", true));
    let (dso_mag_limit, set_dso_mag_limit) = signal(ls_f64_init("sky_dso_mag_limit", 11.0));

    // Persist checkbox state to localStorage on change
    Effect::new(move || {
        let bools: &[(&str, bool)] = &[
            ("sky_show_stars", show_stars.get()),
            ("sky_show_names", show_names.get()),
            ("sky_show_constellations", show_constellations.get()),
            ("sky_show_con_names", show_con_names.get()),
            ("sky_show_grid", show_grid.get()),
            ("sky_show_eq_grid", show_eq_grid.get()),
            ("sky_show_meridian", show_meridian.get()),
            ("sky_show_fov", show_fov.get()),
            ("sky_show_dso", show_dso.get()),
            ("sky_dso_galaxy", dso_filter_galaxy.get()),
            ("sky_dso_open_cluster", dso_filter_open_cluster.get()),
            ("sky_dso_globular", dso_filter_globular.get()),
            ("sky_dso_nebula", dso_filter_nebula.get()),
            ("sky_dso_planetary", dso_filter_planetary.get()),
            ("sky_dso_snr", dso_filter_snr.get()),
            ("sky_dso_galaxy_cluster", dso_filter_galaxy_cluster.get()),
        ];
        let mag = dso_mag_limit.get();
        if let Some(ls) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            for (k, v) in bools {
                let _ = ls.set_item(k, if *v { "true" } else { "false" });
            }
            let _ = ls.set_item("sky_dso_mag_limit", &mag.to_string());
        }
    });

    // Object search state
    let (sky_search, set_sky_search) = signal(String::new());

    // Controls panel section visibility
    let (show_sky_section,      set_show_sky_section)      = signal(false);
    let (show_objects_section,  set_show_objects_section)  = signal(false);
    let (show_settings_section, set_show_settings_section) = signal(false);

    // Controls panel visibility (collapsed by default on narrow screens)
    let (show_controls, set_show_controls) = signal({
        web_sys::window()
            .and_then(|w| w.inner_width().ok())
            .and_then(|v| v.as_f64())
            .map(|w| w >= 768.0)
            .unwrap_or(true)
    });

    // Drag state
    let dragging = StoredValue::new(false);
    let drag_last = StoredValue::new((0.0_f64, 0.0_f64));

    // Pinch-to-zoom state
    let pinch_start_dist = StoredValue::new(0.0_f64);
    let pinch_start_fov  = StoredValue::new(0.0_f64);

    // Canvas refs
    let overlay_ref = NodeRef::<leptos::html::Canvas>::new();
    let gpu_canvas_ref = NodeRef::<leptos::html::Canvas>::new();

    // Context menu state
    let (ctx_menu, set_ctx_menu) = signal(None::<(f64, f64, f64, f64)>);

    // Bottom confirmation popup: (is_align, ra_deg, dec_deg)
    let (pending_action, set_pending_action) = signal(None::<(bool, f64, f64)>);

    // GPU renderer
    let gpu_renderer: Rc<RefCell<Option<GpuSkyRenderer>>> = Rc::new(RefCell::new(None));
    let (gpu_ready, set_gpu_ready) = signal(false);

    // Nebulae image cache: URL path → HtmlImageElement (lazily loaded)
    let nebulae_images: Rc<RefCell<HashMap<String, HtmlImageElement>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Nebulae index: fetched once from /nebulae.json at startup
    let (nebulae_index, set_nebulae_index) = signal(None::<Arc<NebulaeIndex>>);
    Effect::new(move || {
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(idx) = crate::nebulae::fetch_nebulae_index().await {
                set_nebulae_index.set(Some(idx));
            }
        });
    });

    // ── Effective focal length ─────────────────────────────────────────────
    let effective_focal = Signal::derive(move || {
        let ov = focal_override.get();
        if let Ok(v) = ov.parse::<f64>() {
            if v > 0.0 { return Some(v); }
        }
        focal_length_mm.get()
    });

    // ── GPU init ───────────────────────────────────────────────────────────
    let gpu_for_init = Rc::clone(&gpu_renderer);
    Effect::new(move || {
        let Some(gpu_canvas) = gpu_canvas_ref.get() else { return; };
        let Some(cat) = catalog_sig.get() else { return; };
        if gpu_ready.get_untracked() { return; }

        let gpu = Rc::clone(&gpu_for_init);
        let star_data = cat.packed_star_buffer();
        let line_data = cat.packed_line_buffer();
        let gpu_canvas_el: HtmlCanvasElement = gpu_canvas.clone().into();
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(renderer) = GpuSkyRenderer::init(gpu_canvas_el, star_data, line_data).await {
                *gpu.borrow_mut() = Some(renderer);
                set_gpu_ready.set(true);
            }
        });
    });

    // ── Animation tick signal ──────────────────────────────────────────────
    let (tick, set_tick) = signal(0u32);

    // ── Render function ────────────────────────────────────────────────────
    let gpu_for_render = Rc::clone(&gpu_renderer);
    let nebulae_for_render = Rc::clone(&nebulae_images);
    let _render_handle = Effect::new(move || {
        // Read all reactive deps to subscribe
        let m = mount.get();
        let cam = camera.get();
        let s = site.get();
        let sv = solve.get();
        let fov = fov_radius.get();
        let t_off = time_offset_s.get();
        let stars_on = show_stars.get();
        let names_on = show_names.get();
        let const_on = show_constellations.get();
        let con_names_on = show_con_names.get();
        let grid_on = show_grid.get();
        let eq_grid_on = show_eq_grid.get();
        let meridian_on = show_meridian.get();
        let fov_on = show_fov.get();
        let dso_on = show_dso.get();
        let dso_gx  = dso_filter_galaxy.get();
        let dso_oc  = dso_filter_open_cluster.get();
        let dso_gc  = dso_filter_globular.get();
        let dso_nb  = dso_filter_nebula.get();
        let dso_pn  = dso_filter_planetary.get();
        let dso_snr = dso_filter_snr.get();
        let dso_gal = dso_filter_galaxy_cluster.get();
        let dso_mag = dso_mag_limit.get();
        let fl = effective_focal.get();
        let follow = follow_mount.get();
        let has_gpu = gpu_ready.get();
        let cur_lang = lang.get();
        let _frame = tick.get();

        let cat = catalog_sig.get_untracked();
        let dso_cat = dso_catalog_sig.get_untracked();

        let Some(overlay_canvas) = overlay_ref.get() else { return; };
        let overlay_el: HtmlCanvasElement = overlay_canvas.into();

        // Size overlay canvas to container
        let parent = overlay_el.parent_element().unwrap();
        let w = parent.client_width() as u32;
        let h = parent.client_height().max(500) as u32;
        // Cap DPR to 2.0 — on 3x mobile screens this cuts pixel count by ~2.25x
        let dpr = web_sys::window().map(|win| win.device_pixel_ratio().min(2.0)).unwrap_or(1.0);
        let w_phys = (w as f64 * dpr).round() as u32;
        let h_phys = (h as f64 * dpr).round() as u32;
        if overlay_el.width() != w_phys { overlay_el.set_width(w_phys); }
        if overlay_el.height() != h_phys { overlay_el.set_height(h_phys); }

        // Also size the GPU canvas
        if let Some(gc) = gpu_canvas_ref.get() {
            let gc_el: HtmlCanvasElement = gc.into();
            if gc_el.width() != w_phys { gc_el.set_width(w_phys); }
            if gc_el.height() != h_phys { gc_el.set_height(h_phys); }
        }

        let ctx = overlay_el
            .get_context("2d")
            .unwrap()
            .unwrap()
            .dyn_into::<CanvasRenderingContext2d>()
            .unwrap();

        let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);

        let wf = w as f64;
        let hf = h as f64;

        // ── Time ───────────────────────────────────────────────────────
        let now = js_sys::Date::new_0();
        let y = now.get_utc_full_year() as i32;
        let mo = now.get_utc_month() + 1;
        let d = now.get_utc_date();
        let hr = now.get_utc_hours();
        let mn = now.get_utc_minutes();
        let sc = now.get_utc_seconds() as f64 + now.get_utc_milliseconds() as f64 / 1000.0;
        let jd = astro::julian_date(y, mo, d, hr, mn, sc + t_off);
        let gmst = astro::gmst_deg(jd);
        let lst = astro::lst_deg(gmst, s.longitude);

        // ── View centre ────────────────────────────────────────────────
        let (c_alt, c_az) = if follow && m.connected {
            if let (Some(ra_h), Some(dec)) = (m.ra_h, m.dec_deg) {
                let pos = astro::eq_to_altaz(ra_h * 15.0, dec, lst, s.latitude);
                if !m.slewing {
                    set_center_alt.set(pos.0);
                    set_center_az.set(pos.1);
                    pos
                } else {
                    (center_alt.get_untracked(), center_az.get_untracked())
                }
            } else {
                (center_alt.get_untracked(), center_az.get_untracked())
            }
        } else {
            (center_alt.get_untracked(), center_az.get_untracked())
        };

        let cx = wf / 2.0;
        let cy = hf / 2.0;
        let scale = hf.min(wf) / 2.0;
        let sin_lat = s.latitude.to_radians().sin();
        let cos_lat = s.latitude.to_radians().cos();
        // Smooth magnitude limit: fewer stars when zoomed out, more when zoomed in.
        // fov=180° → mag 3.5, fov=90° → mag 4.5, fov=30° → mag 5.5, fov=5° → mag 6.5
        let mag_limit: f32 = (6.5 - 3.0 * (fov / 180.0).sqrt()).clamp(3.5, 6.5) as f32;

        // ── GPU path ───────────────────────────────────────────────────
        if has_gpu {
            if let Ok(mut opt) = gpu_for_render.try_borrow_mut() {
                if let Some(renderer) = opt.as_mut() {
                    if renderer.width() != w_phys || renderer.height() != h_phys {
                        renderer.resize(w_phys, h_phys);
                    }

                    let uniforms = Uniforms {
                        sin_lat: sin_lat as f32,
                        cos_lat: cos_lat as f32,
                        lst_rad: lst.to_radians() as f32,
                        c_alt_rad: c_alt.to_radians() as f32,
                        c_az_rad: c_az.to_radians() as f32,
                        fov_rad: fov.to_radians() as f32,
                        cx: (cx * dpr) as f32,
                        cy: (cy * dpr) as f32,
                        scale: (scale * dpr) as f32,
                        mag_limit,
                        canvas_w: (wf * dpr) as f32,
                        canvas_h: (hf * dpr) as f32,
                        dpr: dpr as f32,
                    };

                    renderer.render_frame(&uniforms, stars_on, const_on);
                }
            }
        }

        // ── Overlay rendering (delegated to render module) ─────────────
        let params = RenderParams {
            wf,
            hf,
            c_alt,
            c_az,
            fov,
            lst,
            latitude: s.latitude,
            sin_lat,
            cos_lat,
            mag_limit,
            has_gpu,
            stars_on,
            names_on,
            const_on,
            con_names_on,
            grid_on,
            eq_grid_on,
            meridian_on,
            fov_on,
            dso_on,
            dso_gx,
            dso_oc,
            dso_gc,
            dso_nb,
            dso_pn,
            dso_snr,
            dso_gal,
            dso_mag,
            fl,
            mount_connected: m.connected,
            mount_ra_h: m.ra_h,
            mount_dec_deg: m.dec_deg,
            cam_pixel_size_um: cam.pixel_size_um,
            cam_sensor_width: cam.sensor_width,
            cam_sensor_height: cam.sensor_height,
            rotation_deg: sv.rotation_deg,
            t_off,
            jd,
            cur_lang,
        };

        let nb_idx = nebulae_index.get_untracked();
        let mut nebulae_cache = nebulae_for_render.borrow_mut();
        render::render_overlay(
            &ctx, &params, &cat, &dso_cat,
            nb_idx.as_deref(),
            &mut nebulae_cache,
        );
    });

    // ── Animation loop (throttled: skip every other frame for smoother mobile perf) ──
    let _raf = Effect::new(move || {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        let f: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
        let g = Rc::clone(&f);
        let frame_counter = Rc::new(std::cell::Cell::new(0u32));
        let fc = Rc::clone(&frame_counter);

        *g.borrow_mut() = Some(Closure::<dyn FnMut()>::new(move || {
            // Only update tick every other frame (~30fps on 60Hz, ~60fps on 120Hz)
            let count = fc.get().wrapping_add(1);
            fc.set(count);
            if count % 2 == 0 {
                set_tick.update(|t| *t = t.wrapping_add(1));
            }
            if let Some(win) = web_sys::window() {
                let _ = win.request_animation_frame(
                    f.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
                );
            }
        }));

        let window = web_sys::window().unwrap();
        let _ = window.request_animation_frame(
            g.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
        );
    });

    // ── Mouse handlers ─────────────────────────────────────────────────────
    let on_mousedown = move |ev: MouseEvent| {
        if ev.button() == 0 {
            set_follow_mount.set(false);
            set_ctx_menu.set(None);
            dragging.set_value(true);
            drag_last.set_value((ev.client_x() as f64, ev.client_y() as f64));
        }
    };

    let on_mousemove = move |ev: MouseEvent| {
        if !dragging.get_value() { return; }
        let (lx, ly) = drag_last.get_value();
        let dx = ev.client_x() as f64 - lx;
        let dy = ev.client_y() as f64 - ly;
        drag_last.set_value((ev.client_x() as f64, ev.client_y() as f64));

        let fov = fov_radius.get_untracked();
        let deg_per_px = fov * 2.0 / 500.0;
        set_center_az.update(|az| *az = (*az - dx * deg_per_px).rem_euclid(360.0));
        set_center_alt.update(|alt| *alt = (*alt + dy * deg_per_px).clamp(-90.0, 90.0));
    };

    let on_mouseup = move |_ev: MouseEvent| {
        dragging.set_value(false);
    };

    let on_wheel = move |ev: WheelEvent| {
        ev.prevent_default();
        let delta = ev.delta_y();
        set_fov_radius.update(|f| {
            *f = (*f * (1.0 + delta * 0.001)).clamp(0.1, 90.0);
        });
        let fov = fov_radius.get_untracked();
        let auto_mag = (11.0 + 3.0 * (10.0_f64 / fov).log10()).clamp(4.0, 20.0);
        set_dso_mag_limit.set((auto_mag * 2.0).round() / 2.0);
    };

    // ── Touch handlers ─────────────────────────────────────────────────────
    let on_touchstart = move |ev: TouchEvent| {
        ev.prevent_default();
        let touches = ev.touches();
        if touches.length() == 1 {
            let t = touches.get(0).unwrap();
            set_follow_mount.set(false);
            set_ctx_menu.set(None);
            dragging.set_value(true);
            drag_last.set_value((t.client_x() as f64, t.client_y() as f64));
            pinch_start_dist.set_value(0.0);
        } else if touches.length() == 2 {
            dragging.set_value(false);
            let t0 = touches.get(0).unwrap();
            let t1 = touches.get(1).unwrap();
            let dx = t0.client_x() as f64 - t1.client_x() as f64;
            let dy = t0.client_y() as f64 - t1.client_y() as f64;
            let dist = (dx * dx + dy * dy).sqrt();
            pinch_start_dist.set_value(dist);
            pinch_start_fov.set_value(fov_radius.get_untracked());
        }
    };

    let on_touchmove = move |ev: TouchEvent| {
        ev.prevent_default();
        let touches = ev.touches();
        if touches.length() == 1 && dragging.get_value() {
            let t = touches.get(0).unwrap();
            let (lx, ly) = drag_last.get_value();
            let dx = t.client_x() as f64 - lx;
            let dy = t.client_y() as f64 - ly;
            drag_last.set_value((t.client_x() as f64, t.client_y() as f64));
            let fov = fov_radius.get_untracked();
            let deg_per_px = fov * 2.0 / 500.0;
            set_center_az.update(|az| *az = (*az - dx * deg_per_px).rem_euclid(360.0));
            set_center_alt.update(|alt| *alt = (*alt + dy * deg_per_px).clamp(-90.0, 90.0));
        } else if touches.length() == 2 {
            let t0 = touches.get(0).unwrap();
            let t1 = touches.get(1).unwrap();
            let dx = t0.client_x() as f64 - t1.client_x() as f64;
            let dy = t0.client_y() as f64 - t1.client_y() as f64;
            let dist = (dx * dx + dy * dy).sqrt();
            let start_dist = pinch_start_dist.get_value();
            if start_dist > 0.0 {
                let start_fov = pinch_start_fov.get_value();
                let new_fov = (start_fov * start_dist / dist).clamp(0.1, 90.0);
                set_fov_radius.set(new_fov);
                let auto_mag = (11.0 + 3.0 * (10.0_f64 / new_fov).log10()).clamp(4.0, 20.0);
                set_dso_mag_limit.set((auto_mag * 2.0).round() / 2.0);
            }
        }
    };

    let on_touchend = move |ev: TouchEvent| {
        ev.prevent_default();
        dragging.set_value(false);
        pinch_start_dist.set_value(0.0);
    };

    // Right-click context menu
    let on_contextmenu = move |ev: MouseEvent| {
        ev.prevent_default();

        let now = js_sys::Date::new_0();
        let jd = astro::julian_date(
            now.get_utc_full_year() as i32,
            now.get_utc_month() + 1,
            now.get_utc_date(),
            now.get_utc_hours(),
            now.get_utc_minutes(),
            now.get_utc_seconds() as f64 + time_offset_s.get_untracked(),
        );
        let gmst = astro::gmst_deg(jd);
        let s = site.get_untracked();
        let lst = astro::lst_deg(gmst, s.longitude);

        let (ra_jnow, dec_jnow) = astro::altaz_to_eq(
            center_alt.get_untracked(),
            center_az.get_untracked(),
            lst,
            s.latitude,
        );
        // altaz_to_eq returns JNow; convert to J2000 for MountGoto / AlignStart
        let j2000 = JNow::new(ra_jnow, dec_jnow).to_j2000(jd);
        set_ctx_menu.set(Some((ev.client_x() as f64, ev.client_y() as f64, j2000.ra_deg, j2000.dec_deg)));
    };

    let send_for_ctx = Arc::clone(&send);
    let send_for_confirm = Arc::clone(&send);
    let send_for_location = Arc::clone(&send);
    let set_site_location_fn: Arc<dyn Fn(f64, f64) + Send + Sync> = Arc::new(move |lat: f64, lon: f64| {
        send_for_location(serde_json::json!({"type":"option_set","payload":{"Latitude":lat,"Longitude":lon}}).to_string());
    });

    view! {
        <div class="sky-pane"
             style="position:relative; width:100%; overflow:hidden;"
             on:click=move |_| { set_ctx_menu.set(None); set_pending_action.set(None); }>

            // WebGPU canvas (bottom layer)
            <canvas
                node_ref=gpu_canvas_ref
                style="position:absolute; top:0; left:0; width:100%; height:100%; display:block;"
            />

            // Canvas2D overlay (top layer)
            <canvas
                node_ref=overlay_ref
                style="position:absolute; top:0; left:0; width:100%; height:100%; display:block; cursor:crosshair;"
                on:mousedown=on_mousedown
                on:mousemove=on_mousemove
                on:mouseup=on_mouseup
                on:mouseleave=move |_| dragging.set_value(false)
                on:wheel=on_wheel
                on:contextmenu=on_contextmenu
                on:touchstart=on_touchstart
                on:touchmove=on_touchmove
                on:touchend=on_touchend
                on:touchcancel=move |_: TouchEvent| { dragging.set_value(false); pinch_start_dist.set_value(0.0); }
            />

            // ── Object search box ──────────────────────────────────────────
            <SkySearch
                sky_search=sky_search
                set_sky_search=set_sky_search
                catalog_sig=catalog_sig
                dso_catalog_sig=dso_catalog_sig
                site=site
                time_offset_s=time_offset_s
                set_center_alt=set_center_alt
                set_center_az=set_center_az
                set_follow_mount=set_follow_mount
                set_fov_radius=set_fov_radius
                set_dso_mag_limit=set_dso_mag_limit
            />

            // ── Controls panel ─────────────────────────────────────────────
            <SkyControls
                show_controls=show_controls
                set_show_controls=set_show_controls
                show_sky_section=show_sky_section
                set_show_sky_section=set_show_sky_section
                show_objects_section=show_objects_section
                set_show_objects_section=set_show_objects_section
                show_settings_section=show_settings_section
                set_show_settings_section=set_show_settings_section
                show_stars=show_stars
                set_show_stars=set_show_stars
                show_names=show_names
                set_show_names=set_show_names
                show_constellations=show_constellations
                set_show_constellations=set_show_constellations
                show_con_names=show_con_names
                set_show_con_names=set_show_con_names
                show_grid=show_grid
                set_show_grid=set_show_grid
                show_eq_grid=show_eq_grid
                set_show_eq_grid=set_show_eq_grid
                show_meridian=show_meridian
                set_show_meridian=set_show_meridian
                show_fov=show_fov
                set_show_fov=set_show_fov
                show_dso=show_dso
                set_show_dso=set_show_dso
                dso_filter_galaxy=dso_filter_galaxy
                set_dso_filter_galaxy=set_dso_filter_galaxy
                dso_filter_open_cluster=dso_filter_open_cluster
                set_dso_filter_open_cluster=set_dso_filter_open_cluster
                dso_filter_globular=dso_filter_globular
                set_dso_filter_globular=set_dso_filter_globular
                dso_filter_nebula=dso_filter_nebula
                set_dso_filter_nebula=set_dso_filter_nebula
                dso_filter_planetary=dso_filter_planetary
                set_dso_filter_planetary=set_dso_filter_planetary
                dso_filter_snr=dso_filter_snr
                set_dso_filter_snr=set_dso_filter_snr
                dso_filter_galaxy_cluster=dso_filter_galaxy_cluster
                set_dso_filter_galaxy_cluster=set_dso_filter_galaxy_cluster
                dso_mag_limit=dso_mag_limit
                set_dso_mag_limit=set_dso_mag_limit
                focal_override=focal_override
                set_focal_override=set_focal_override
                focal_length_mm=focal_length_mm
                set_follow_mount=set_follow_mount
                site=site
                set_site_location=set_site_location_fn.clone()
            />

            // ── Slew action button (bottom-left) ───────────────────────────
            <div class="sky-actions"
                 style="position:absolute; bottom:48px; left:8px; display:flex; gap:8px; z-index:50;"
                 on:click=move |ev| ev.stop_propagation()>
                <button
                    on:click=move |_| open_confirm(false, time_offset_s, site, center_alt, center_az, set_ctx_menu, set_pending_action)
                    style="padding:10px 20px; background:#1a2a4a; color:#88aaff; border:1px solid #446; \
                           border-radius:4px; cursor:pointer; font-family:monospace; font-size:13px; font-weight:bold;">
                    {move || tr().goto_btn}
                </button>
            </div>

            // ── Time slider (bottom) ────────────────────────────────────────
            <div class="sky-time-slider"
                 style="position:absolute; bottom:4px; left:310px; right:8px; display:flex; align-items:center; gap:8px;">
                <span style="font-size:11px; color:#888; white-space:nowrap;">
                    {move || {
                        let t = time_offset_s.get();
                        if t.abs() < 0.5 { tr().now.to_string() }
                        else {
                            let h = (t / 3600.0) as i32;
                            let m = ((t.abs() % 3600.0) / 60.0) as u32;
                            format!("{h:+}h{m:02}m")
                        }
                    }}
                </span>
                <input type="range"
                       style="flex:1; accent-color:#88aaff;"
                       min="-43200" max="43200" step="60"
                       prop:value=move || format!("{}", time_offset_s.get() as i64)
                       on:input=move |ev| {
                           if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                               set_time_offset_s.set(v);
                           }
                       }
                />
                <button style="background:#1a1a2e; color:#aaa; border:1px solid #444; \
                               padding:2px 8px; cursor:pointer; font-family:monospace; font-size:11px;"
                        on:click=move |_| set_time_offset_s.set(0.0)>
                    {move || tr().reset}
                </button>
            </div>

            // ── Zoom bar (right side, vertical) ─────────────────────────────
            <div class="sky-zoom-bar"
                 style="position:absolute; right:8px; top:50px; bottom:40px; \
                        display:flex; flex-direction:column; align-items:center; gap:4px; z-index:30;"
                 on:click=move |ev| ev.stop_propagation()>
                <span style="font-size:11px; color:#888; white-space:nowrap;">
                    {move || {
                        let fov = fov_radius.get() * 2.0;
                        if fov >= 10.0 { format!("{:.0}\u{00b0}", fov) }
                        else if fov >= 1.0 { format!("{:.1}\u{00b0}", fov) }
                        else { format!("{:.2}\u{00b0}", fov) }
                    }}
                </span>
                <input type="range"
                       style="flex:1; accent-color:#88aaff; writing-mode:vertical-lr; \
                              direction:rtl; -webkit-appearance:slider-vertical; \
                              width:20px; cursor:pointer;"
                       min="-1000" max="1954" step="10"
                       prop:value=move || format!("{}", (fov_radius.get().log10() * 1000.0) as i32)
                       on:mousedown=|ev| ev.stop_propagation()
                       on:input=move |ev| {
                           if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                               let new_fov = 10_f64.powf(v / 1000.0).clamp(0.1, 90.0);
                               set_fov_radius.set(new_fov);
                               let auto_mag = (11.0 + 3.0 * (10.0_f64 / new_fov).log10()).clamp(4.0, 20.0);
                               set_dso_mag_limit.set((auto_mag * 2.0).round() / 2.0);
                           }
                       }
                />
            </div>

            // ── Context menu ────────────────────────────────────────────────
            <SkyContextMenu
                ctx_menu=ctx_menu
                set_ctx_menu=set_ctx_menu
                send=send_for_ctx
            />

            // ── Bottom confirmation popup ────────────────────────────────────
            <SkyConfirmPopup
                pending_action=pending_action
                set_pending_action=set_pending_action
                send=send_for_confirm
            />
        </div>
    }
}

// ---------------------------------------------------------------------------
// In-planetarium gear bar — switches between sky and focus tabs.
// Renders as a sibling of the other sky overlays (time slider, zoom bar) so
// visually it reads as part of the planetarium, not a global HUD. Z-index 60
// is deliberately above both the sky actions button (z:50) and the FocusTab
// overlay (z:40 in main.rs) — since `.sky-pane` does not establish its own
// stacking context (position:relative, no z-index/opacity/transform), the
// gear bar z-index stacks against FocusTab in the parent context and pokes
// through even when focus is active.
// ---------------------------------------------------------------------------

#[component]
pub fn SkyTabSwitcher() -> impl IntoView {
    let active = use_context::<ActiveTabCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| RwSignal::new(Tab::Sky));

    let btn_style = move |tab: Tab| {
        let on = active.get() == tab;
        let (bg, border, color) = if on {
            ("rgba(40,60,110,0.95)", "#88aaff", "#cfe0ff")
        } else {
            ("rgba(12,14,24,0.85)", "#2a2a35", "#88aaff")
        };
        format!(
            "width:48px; height:48px; border-radius:50%; border:1px solid {border}; \
             background:{bg}; color:{color}; display:flex; align-items:center; \
             justify-content:center; cursor:pointer; padding:0; \
             touch-action:manipulation; -webkit-tap-highlight-color:transparent; \
             transition:background 0.15s, border-color 0.15s;"
        )
    };

    let gear = |size: i32| view! {
        <svg width=size height=size viewBox="0 0 24 24" fill="none"
             stroke="currentColor" stroke-width="1.8"
             stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="3"></circle>
            <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
        </svg>
    };

    view! {
        <div class="sky-tab-switcher"
             style="position:absolute; bottom:48px; left:50%; transform:translateX(-50%); \
                    z-index:60; display:flex; gap:10px; padding:6px 10px; \
                    background:rgba(6,6,15,0.75); border:1px solid #222; \
                    border-radius:24px; pointer-events:auto;"
             on:click=|ev: MouseEvent| ev.stop_propagation()>
            <button title="Sky"
                    style=move || btn_style(Tab::Sky)
                    on:click=move |_| active.set(Tab::Sky)>
                {gear(20)}
            </button>
            <button title="Focus"
                    style=move || btn_style(Tab::Focus)
                    on:click=move |_| active.set(Tab::Focus)>
                {gear(20)}
            </button>
            <button title="Imaging"
                    style=move || btn_style(Tab::Imaging)
                    on:click=move |_| active.set(Tab::Imaging)>
                {gear(20)}
            </button>
            <button title="Polar Align"
                    style=move || btn_style(Tab::PolarAlign)
                    on:click=move |_| active.set(Tab::PolarAlign)>
                {gear(20)}
            </button>
        </div>
    }
}
