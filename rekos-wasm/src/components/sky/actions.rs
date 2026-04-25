//! Context menu and confirmation popup components for the sky tab.

use std::sync::Arc;

use leptos::prelude::*;

use crate::astro;
use crate::i18n::{Lang, t};

use crate::compat::SiteSnapshot;
use crate::CameraDeviceCtx;
use crate::ws::SendCmd;
use crate::{ActiveTabCtx, AlignDefaultsCtx, AlignSolveRadiusCtx, MosaicPlannerCtx, MountDeviceCtx, SchedulerPrefillCtx, Tab};

/// Build a `mount_goto_rade` message.
///
/// KStars parses `ra`/`de` via `dms::fromString(payload["ra"].toString(), …)`,
/// so the values must be JSON strings (decimal hours / decimal degrees).
///
/// Despite the `isJ2000` flag in the Ekos Live protocol, `Ekos::Mount::slew`
/// (`kstars/ekos/mount/mount.cpp:985`) always treats the incoming RA/Dec as
/// JNow — it constructs a `SkyPoint` and sends `ScopeTarget->ra()` straight
/// to the mount's `EQUATORIAL_EOD_COORD` property. `setJ2000Enabled(true)`
/// only changes the UI display. So we send JNow and set `isJ2000: false`
/// to keep KStars' UI consistent.
fn goto_rade_msg(ra_deg_jnow: f64, dec_deg_jnow: f64) -> String {
    let ra_h = ra_deg_jnow / 15.0;
    serde_json::json!({
        "type": "mount_goto_rade",
        "payload": {
            "ra": format!("{:.8}", ra_h),
            "de": format!("{:.8}", dec_deg_jnow),
            "isJ2000": false,
        }
    })
    .to_string()
}

/// Right-click context menu overlay.
#[component]
pub fn SkyContextMenu(
    ctx_menu: ReadSignal<Option<(f64, f64, f64, f64)>>,
    set_ctx_menu: WriteSignal<Option<(f64, f64, f64, f64)>>,
    /// Set to `true` when the user picks "Goto & Align" — consumed by an
    /// Effect in `mod.rs` that waits for the mount to finish slewing before
    /// actually firing `align_solve`. Prevents the solver from running on
    /// the pre-slew image (which would make the solve marker appear at the
    /// mount's *previous* position instead of the actual solved one).
    pending_solve_after_slew: RwSignal<bool>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Resolve all contexts at component creation time (not inside event handlers)
    let mount_ctx = use_context::<MountDeviceCtx>();
    let camera_ctx = use_context::<CameraDeviceCtx>();
    let solve_radius_ctx = use_context::<AlignSolveRadiusCtx>();
    let align_defaults_ctx = use_context::<AlignDefaultsCtx>();
    let prefill_ctx = use_context::<SchedulerPrefillCtx>();
    let active_tab_ctx = use_context::<ActiveTabCtx>();
    let planner_ctx = use_context::<MosaicPlannerCtx>();

    let busy_ctx = use_context::<crate::ServiceBusyCtx>();
    let mount_busy = Signal::derive(move || busy_ctx.and_then(|c| c.mount_busy.get()));
    let camera_busy = Signal::derive(move || busy_ctx.and_then(|c| c.camera_busy.get()));
    let goto_disabled = Signal::derive(move || mount_busy.get().is_some());
    let align_disabled = Signal::derive(move || mount_busy.get().is_some() || camera_busy.get().is_some());

    let send_for_goto = Arc::clone(&send);
    let send_for_align = Arc::clone(&send);
    let on_goto = move |_| {
        if let Some((_sx, _sy, ra_deg, dec_deg)) = ctx_menu.get_untracked() {
            send_for_goto(goto_rade_msg(ra_deg, dec_deg));
            set_ctx_menu.set(None);
        }
    };

    let on_align = move |_| {
        if let Some((_sx, _sy, ra_deg, dec_deg)) = ctx_menu.get_untracked() {
            // Send the goto now, then mark a pending request. An Effect in
            // mod.rs watches the mount-slewing signal and fires `align_solve`
            // only after the mount reports idle — firing it immediately would
            // capture+solve the pre-slew image and put the marker at the
            // mount's old position.
            send_for_align(goto_rade_msg(ra_deg, dec_deg));
            pending_solve_after_slew.set(true);
            set_ctx_menu.set(None);
        }
    };
    let _ = (mount_ctx, camera_ctx, solve_radius_ctx, align_defaults_ctx);
    let on_add_scheduler = move |_| {
        if let Some((_sx, _sy, ra_deg, dec_deg)) = ctx_menu.get_untracked() {
            if let Some(pctx) = prefill_ctx {
                pctx.0.set(Some((String::new(), ra_deg, dec_deg)));
            }
            if let Some(atctx) = active_tab_ctx {
                atctx.0.set(Tab::Scheduler);
            }
            set_ctx_menu.set(None);
        }
    };

    let on_create_mosaic = move |_| {
        if let Some((_sx, _sy, ra_deg, dec_deg)) = ctx_menu.get_untracked() {
            if let Some(pctx) = planner_ctx {
                pctx.0.center.set(Some((ra_deg, dec_deg)));
                pctx.0.picking_center.set(false);
                pctx.0.planning.set(true);
            }
            set_ctx_menu.set(None);
        }
    };

    view! {
        {move || {
            ctx_menu.get().map(|(sx, sy, ra_deg, dec_deg)| {
                let ra_h = ra_deg / 15.0;
                let rah = ra_h as u32;
                let ram = ((ra_h - rah as f64) * 60.0).abs() as u32;
                let ras = ((ra_h - rah as f64) * 3600.0 - ram as f64 * 60.0).abs();
                let dec_sign = if dec_deg < 0.0 { "-" } else { "+" };
                let dec_abs = dec_deg.abs();
                let decd = dec_abs as u32;
                let decm = ((dec_abs - decd as f64) * 60.0) as u32;
                view! {
                    <div class="sky-ctx-menu"
                        style=format!(
                        "position:fixed; left:min({}px, calc(100vw - 200px)); \
                         top:min({}px, calc(100dvh - 180px)); \
                         background:#1a1a2e; border:1px solid #555; \
                         border-radius:4px; padding:8px; font-size:12px; z-index:100; min-width:180px;",
                        sx, sy
                    )
                    on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="color:#aaa; margin-bottom:6px;">
                            {format!("{} {:02}h{:02}m{:04.1}s", tr().ra_label, rah, ram, ras)}
                        </div>
                        <div style="color:#aaa; margin-bottom:8px;">
                            {format!("{} {}{}\u{00b0}{:02}'{:02}\"", tr().dec_label, dec_sign, decd, decm,
                                ((dec_abs - decd as f64) * 3600.0 - decm as f64 * 60.0) as u32)}
                        </div>
                        <button
                                disabled=move || goto_disabled.get()
                                style=move || if goto_disabled.get() {
                                    "width:100%; background:#1a1a1a; color:#555; border:1px solid #333; \
                                     padding:4px 8px; cursor:not-allowed; font-family:monospace; font-size:12px; \
                                     border-radius:2px; margin-bottom:4px; opacity:0.6;"
                                } else {
                                    "width:100%; background:#2a3a5a; color:#aaf; border:1px solid #556; \
                                     padding:4px 8px; cursor:pointer; font-family:monospace; font-size:12px; \
                                     border-radius:2px; margin-bottom:4px;"
                                }
                                on:click=on_goto.clone()>
                            {move || {
                                if let Some(svc) = goto_disabled.get().then(|| mount_busy.get()).flatten() {
                                    format!("{} ({})", tr().goto_here, svc)
                                } else {
                                    tr().goto_here.to_string()
                                }
                            }}
                        </button>
                        <button
                                disabled=move || align_disabled.get()
                                style=move || if align_disabled.get() {
                                    "width:100%; background:#1a1a1a; color:#555; border:1px solid #333; \
                                     padding:4px 8px; cursor:not-allowed; font-family:monospace; font-size:12px; \
                                     border-radius:2px; opacity:0.6;"
                                } else {
                                    "width:100%; background:#2a4a3a; color:#afa; border:1px solid #565; \
                                     padding:4px 8px; cursor:pointer; font-family:monospace; font-size:12px; \
                                     border-radius:2px;"
                                }
                                on:click=on_align.clone()>
                            {move || {
                                let busy = mount_busy.get().or_else(|| camera_busy.get());
                                if let Some(svc) = busy {
                                    format!("{} ({})", tr().goto_and_align, svc)
                                } else {
                                    tr().goto_and_align.to_string()
                                }
                            }}
                        </button>
                        <button
                            style="width:100%; background:#1a1a3a; color:#88aaff; border:1px solid #446; \
                                   padding:4px 8px; cursor:pointer; font-family:monospace; font-size:12px; \
                                   border-radius:2px; margin-top:6px;"
                            on:click=on_add_scheduler.clone()>
                            {"Add to Scheduler"}
                        </button>
                        <button
                            style="width:100%; background:#0a1a2a; color:#00cccc; border:1px solid #0a6060; \
                                   padding:4px 8px; cursor:pointer; font-family:monospace; font-size:12px; \
                                   border-radius:2px; margin-top:4px;"
                            on:click=on_create_mosaic.clone()>
                            {"Create Mosaic"}
                        </button>
                    </div>
                }
            })
        }}
    }
}

/// Bottom confirmation popup for goto/align actions.
#[component]
pub fn SkyConfirmPopup(
    pending_action: ReadSignal<Option<(bool, f64, f64)>>,
    set_pending_action: WriteSignal<Option<(bool, f64, f64)>>,
    pending_solve_after_slew: RwSignal<bool>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Resolve all contexts at component creation time
    let mount_ctx = use_context::<MountDeviceCtx>();
    let camera_ctx = use_context::<CameraDeviceCtx>();
    let solve_radius_ctx = use_context::<AlignSolveRadiusCtx>();
    let align_defaults_ctx = use_context::<AlignDefaultsCtx>();

    let busy_ctx = use_context::<crate::ServiceBusyCtx>();
    let mount_busy = Signal::derive(move || busy_ctx.and_then(|c| c.mount_busy.get()));
    let camera_busy = Signal::derive(move || busy_ctx.and_then(|c| c.camera_busy.get()));
    let goto_disabled = Signal::derive(move || mount_busy.get().is_some());
    let align_disabled = Signal::derive(move || mount_busy.get().is_some() || camera_busy.get().is_some());

    view! {
        {move || {
            pending_action.get().map(|(_is_align, ra_deg, dec_deg)| {
                let ra_h  = ra_deg / 15.0;
                let rah   = ra_h as u32;
                let ram   = ((ra_h - rah as f64) * 60.0).abs() as u32;
                let ras   = ((ra_h - rah as f64) * 3600.0 - ram as f64 * 60.0).abs();
                let dec_sign = if dec_deg < 0.0 { "-" } else { "+" };
                let dec_abs  = dec_deg.abs();
                let decd  = dec_abs as u32;
                let decm  = ((dec_abs - decd as f64) * 60.0) as u32;
                let decs  = ((dec_abs - decd as f64) * 3600.0 - decm as f64 * 60.0) as u32;
                let send_c = Arc::clone(&send);
                let send_align = Arc::clone(&send);
                view! {
                    <div
                        class="sky-confirm-popup"
                        style="position:absolute; bottom:0; left:0; right:0; z-index:200; \
                               background:rgba(10,10,20,0.97); border-top:1px solid #446; \
                               padding:14px 16px; display:flex; align-items:center; \
                               justify-content:space-between; gap:12px; flex-wrap:wrap;"
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="display:flex; flex-direction:column; gap:4px; font-family:monospace; font-size:13px;">
                            <span style="color:#88aaff; font-weight:bold;">
                                {format!("{}  {:02}h {:02}m {:04.1}s", tr().ra_label, rah, ram, ras)}
                            </span>
                            <span style="color:#88aaff; font-weight:bold;">
                                {format!("{} {}{}\u{00b0} {:02}' {:02}\"", tr().dec_label, dec_sign, decd, decm, decs)}
                            </span>
                        </div>
                        <div style="display:flex; gap:10px; flex-shrink:0;">
                            <button
                                on:click=move |_| set_pending_action.set(None)
                                style="padding:12px 24px; background:#1a1a1a; color:#888; \
                                       border:1px solid #444; border-radius:4px; cursor:pointer; \
                                       font-family:monospace; font-size:14px;">
                                {tr().cancel}
                            </button>
                            <button
                                disabled=move || goto_disabled.get()
                                on:click=move |_| {
                                    if goto_disabled.get_untracked() { return; }
                                    send_c(goto_rade_msg(ra_deg, dec_deg));
                                    set_pending_action.set(None);
                                }
                                style=move || if goto_disabled.get() {
                                    "padding:12px 24px; background:#1a1a1a; color:#555; \
                                     border:1px solid #333; border-radius:4px; cursor:not-allowed; \
                                     font-family:monospace; font-size:14px; font-weight:bold; opacity:0.6;"
                                } else {
                                    "padding:12px 24px; background:#1a2a4a; color:#88aaff; \
                                     border:1px solid #446; border-radius:4px; cursor:pointer; \
                                     font-family:monospace; font-size:14px; font-weight:bold;"
                                }
                            >
                                {tr().goto_btn}
                            </button>
                            <button
                                disabled=move || align_disabled.get()
                                on:click=move |_| {
                                    if align_disabled.get_untracked() { return; }
                                    // Kick off the slew; `align_solve` fires
                                    // once the mount stops moving — see the
                                    // Effect in mod.rs that watches
                                    // pending_solve_after_slew + mount.slewing.
                                    send_align(goto_rade_msg(ra_deg, dec_deg));
                                    pending_solve_after_slew.set(true);
                                    set_pending_action.set(None);
                                }
                                style=move || if align_disabled.get() {
                                    "padding:12px 24px; background:#1a1a1a; color:#555; \
                                     border:1px solid #333; border-radius:4px; cursor:not-allowed; \
                                     font-family:monospace; font-size:14px; font-weight:bold; opacity:0.6;"
                                } else {
                                    "padding:12px 24px; background:#1a3a2a; color:#8faf8f; \
                                     border:1px solid #464; border-radius:4px; cursor:pointer; \
                                     font-family:monospace; font-size:14px; font-weight:bold;"
                                }
                            >
                                {tr().goto_align_btn}
                            </button>
                        </div>
                    </div>
                }
            })
        }}
    }
}

/// Helper: compute RA/Dec from current crosshair position and open confirmation popup.
pub fn open_confirm(
    is_align: bool,
    time_offset_s: ReadSignal<f64>,
    site: Signal<SiteSnapshot>,
    center_alt: ReadSignal<f64>,
    center_az: ReadSignal<f64>,
    set_ctx_menu: WriteSignal<Option<(f64, f64, f64, f64)>>,
    set_pending_action: WriteSignal<Option<(bool, f64, f64)>>,
) {
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
    let s    = site.get_untracked();
    let lst  = astro::lst_deg(gmst, s.longitude);
    // altaz_to_eq returns JNow — send it as-is; KStars' mount_goto_rade
    // handler treats the input as JNow regardless of the isJ2000 flag.
    let (ra_jnow, dec_jnow) = astro::altaz_to_eq(
        center_alt.get_untracked(),
        center_az.get_untracked(),
        lst,
        s.latitude,
    );
    set_ctx_menu.set(None);
    set_pending_action.set(Some((is_align, ra_jnow, dec_jnow)));
}
