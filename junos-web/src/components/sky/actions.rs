//! Context menu and confirmation popup components for the sky tab.

use std::sync::Arc;

use leptos::prelude::*;

use crate::i18n::{Lang, t};

use crate::CameraDeviceCtx;
use crate::ws::SendCmd;
use crate::{ActiveTabCtx, AlignDefaultsCtx, AlignSolveRadiusCtx, MountDeviceCtx, SchedulerPrefillCtx, Tab};

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
    let framing_ctx = use_context::<crate::FramingCtx>();

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

    let on_open_framing = move |_| {
        if let Some((_sx, _sy, ra_deg, dec_deg)) = ctx_menu.get_untracked() {
            if let Some(fctx) = framing_ctx {
                // Menu coords are already of-date (JNow), same epoch as
                // `FramingState::center` — no conversion.
                fctx.0.center.set(Some((ra_deg, dec_deg)));
                fctx.0.target.set(String::new());
                fctx.0.open.set(true);
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
                let btn_base = "w-full py-1 px-sp-2 cursor-pointer font-mono text-[12px] rounded-[2px] disabled:!bg-[#1a1a1a] disabled:!text-[#555] disabled:!border-[#333] disabled:!cursor-not-allowed disabled:opacity-60";
                view! {
                    <div class="fixed bg-bg-button border border-[#555] rounded-sm p-sp-2 text-[12px] z-[100] min-w-[180px] max-w-[calc(100vw-16px)] max-h-[calc(100dvh-32px)] overflow-y-auto"
                        style=format!(
                            "left:min({}px, calc(100vw - 200px)); top:min({}px, calc(100dvh - 180px));",
                            sx, sy
                        )
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div class="text-text-muted mb-[6px]">
                            {format!("{} {:02}h{:02}m{:04.1}s", tr().ra_label, rah, ram, ras)}
                        </div>
                        <div class="text-text-muted mb-sp-2">
                            {format!("{} {}{}\u{00b0}{:02}'{:02}\"", tr().dec_label, dec_sign, decd, decm,
                                ((dec_abs - decd as f64) * 3600.0 - decm as f64 * 60.0) as u32)}
                        </div>
                        <button
                                class=format!("{btn_base} bg-[#2a3a5a] text-[#aaf] border border-[#556] mb-1")
                                disabled=move || goto_disabled.get()
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
                                class=format!("{btn_base} bg-[#2a4a3a] text-[#afa] border border-[#565]")
                                disabled=move || align_disabled.get()
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
                            class=format!("{btn_base} bg-[#1a1a3a] text-text-blue border border-border-accent mt-[6px]")
                            on:click=on_add_scheduler.clone()>
                            {move || tr().sky_add_scheduler}
                        </button>
                        <button
                            class=format!("{btn_base} bg-bg-button-info text-accent-cyan-dim border border-border-info mt-1")
                            on:click=on_open_framing.clone()>
                            {move || tr().sky_framing}
                        </button>
                    </div>
                }
            })
        }}
    }
}

