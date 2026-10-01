//! Target card — opened by a tap/click on an object, or a right-click /
//! long-press anywhere (snapped to the object under the pointer). Shows the
//! object or bare sky position and the actions on it: Center, Goto,
//! Goto & Align, Framing assistant, Add to Scheduler.
//! A bottom sheet on phones, a card under the search box on md+.

use std::sync::Arc;

use leptos::prelude::*;

use crate::astro;
use crate::compat::SiteSnapshot;
use crate::coords::JNow;
use crate::dso_catalog::DsoType;
use crate::i18n::{Lang, t};
use crate::ws::SendCmd;
use crate::{ActiveTabCtx, SchedulerPrefillCtx, Tab};

use super::clock::SkyClock;
use super::render::{HitItem, HitKind};

/// What the card is about. Coordinates are JNow at the displayed time.
#[derive(Clone)]
pub struct SkyTarget {
    pub ra_jnow_deg: f64,
    pub dec_jnow_deg: f64,
    pub object: Option<HitItem>,
}

impl SkyTarget {
    /// The object if there is one (snapped to its position), else the point.
    pub fn new(ra_jnow_deg: f64, dec_jnow_deg: f64, object: Option<HitItem>) -> Self {
        match object {
            Some(o) => Self { ra_jnow_deg: o.ra_jnow_deg, dec_jnow_deg: o.dec_jnow_deg, object: Some(o) },
            None => Self { ra_jnow_deg, dec_jnow_deg, object: None },
        }
    }

    fn name(&self) -> String {
        self.object.as_ref().map(|o| o.name.clone()).unwrap_or_default()
    }
}

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

/// "05h35m17.3s"
fn fmt_ra(ra_deg: f64) -> String {
    let s = ra_deg.rem_euclid(360.0) / 15.0 * 3600.0;
    format!("{:02}h{:02}m{:04.1}s", (s / 3600.0) as u32, ((s % 3600.0) / 60.0) as u32, s % 60.0)
}

/// "+05°23'28\""
fn fmt_dec(dec_deg: f64) -> String {
    let sign = if dec_deg < 0.0 { '-' } else { '+' };
    let s = (dec_deg.abs() * 3600.0).round() as u32;
    format!("{sign}{:02}\u{00b0}{:02}'{:02}\"", s / 3600, (s % 3600) / 60, s % 60)
}

#[component]
pub fn SkyTargetCard(
    target: RwSignal<Option<SkyTarget>>,
    clock: RwSignal<SkyClock>,
    #[prop(into)] site: Signal<SiteSnapshot>,
    /// Set to `true` when the user picks "Goto & Align" — consumed by an
    /// Effect in `mod.rs` that waits for the mount to finish slewing before
    /// actually firing `align_solve`. Prevents the solver from running on
    /// the pre-slew image (which would make the solve marker appear at the
    /// mount's *previous* position instead of the actual solved one).
    pending_solve_after_slew: RwSignal<bool>,
    send: SendCmd,
    set_center_alt: WriteSignal<f64>,
    set_center_az: WriteSignal<f64>,
    set_follow_mount: WriteSignal<bool>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Resolve all contexts at component creation time (not inside event handlers)
    let prefill_ctx = use_context::<SchedulerPrefillCtx>();
    let active_tab_ctx = use_context::<ActiveTabCtx>();
    let framing_ctx = use_context::<crate::FramingCtx>();

    let busy_ctx = use_context::<crate::ServiceBusyCtx>();
    let mount_busy = Signal::derive(move || busy_ctx.and_then(|c| c.mount_busy.get()));
    let camera_busy = Signal::derive(move || busy_ctx.and_then(|c| c.camera_busy.get()));
    let goto_disabled = Signal::derive(move || mount_busy.get().is_some());
    let align_disabled = Signal::derive(move || mount_busy.get().is_some() || camera_busy.get().is_some());

    // Alt/Az of a JNow position at the displayed time.
    let altaz = move |ra_deg: f64, dec_deg: f64| {
        let s = site.get_untracked();
        let lst = astro::lst_deg(astro::gmst_deg(clock.get_untracked().jd()), s.longitude);
        astro::eq_to_altaz(ra_deg, dec_deg, lst, s.latitude)
    };

    let on_center = move |_| {
        if let Some(tg) = target.get_untracked() {
            let (alt, az) = altaz(tg.ra_jnow_deg, tg.dec_jnow_deg);
            set_follow_mount.set(false);
            set_center_alt.set(alt);
            set_center_az.set(az);
        }
    };
    let send_for_goto = Arc::clone(&send);
    let on_goto = move |_| {
        if let Some(tg) = target.get_untracked() {
            send_for_goto(goto_rade_msg(tg.ra_jnow_deg, tg.dec_jnow_deg));
            target.set(None);
        }
    };
    let send_for_align = Arc::clone(&send);
    let on_align = move |_| {
        if let Some(tg) = target.get_untracked() {
            // Send the goto now, then mark a pending request. An Effect in
            // mod.rs watches the mount-slewing signal and fires `align_solve`
            // only after the mount reports idle — firing it immediately would
            // capture+solve the pre-slew image and put the marker at the
            // mount's old position.
            send_for_align(goto_rade_msg(tg.ra_jnow_deg, tg.dec_jnow_deg));
            pending_solve_after_slew.set(true);
            target.set(None);
        }
    };
    let on_add_scheduler = move |_| {
        if let Some(tg) = target.get_untracked() {
            if let Some(pctx) = prefill_ctx {
                pctx.0.set(Some((tg.name(), tg.ra_jnow_deg, tg.dec_jnow_deg)));
            }
            if let Some(atctx) = active_tab_ctx {
                atctx.0.set(Tab::Scheduler);
            }
            target.set(None);
        }
    };
    let on_open_framing = move |_| {
        if let Some(tg) = target.get_untracked() {
            if let Some(fctx) = framing_ctx {
                // Target coords are already of-date (JNow), same epoch as
                // `FramingState::params.center` — no conversion.
                fctx.0.params.center.set(Some((tg.ra_jnow_deg, tg.dec_jnow_deg)));
                fctx.0.params.target.set(tg.name());
                fctx.0.open.set(true);
            }
            target.set(None);
        }
    };

    let btn = "btn flex-auto whitespace-nowrap";
    view! {
        {move || target.get().map(|tg| {
            let tr_ = tr();
            let title = tg.object.as_ref().map(|o| o.name.clone()).unwrap_or_else(|| tr_.target_point.to_string());
            let details = tg.object.as_ref().map(|o| {
                let mut parts = vec![kind_label_for(&o.kind, lang.get()).to_string()];
                parts.extend(o.mag.map(|m| format!("{} {:.2}", tr_.mag_label, m)));
                parts.extend(o.size_arcmin.map(|s| format!("{} {:.1}'", tr_.size_label, s)));
                parts.extend(o.phase.map(|p| format!("{} {:.0}%", tr_.phase_label, p * 100.0)));
                parts.join(" \u{00b7} ")
            });
            let (ra, dec) = (tg.ra_jnow_deg, tg.dec_jnow_deg);
            view! {
                <div class="panel absolute z-[80] inset-x-0 bottom-0 rounded-b-none \
                            pb-[max(0.75rem,env(safe-area-inset-bottom))] \
                            md:inset-x-auto md:bottom-auto md:left-3 md:top-[56px] md:w-[340px] md:rounded-lg md:pb-3 \
                            px-3 pt-2 flex flex-col gap-1 font-mono text-sm text-text"
                     on:click=|ev| ev.stop_propagation()>
                    <div class="flex items-center justify-between gap-2">
                        <span class="font-bold text-text-blue-bright text-md truncate">{title}</span>
                        <button class="btn-icon shrink-0" title=tr_.info_close
                                on:click=move |_| target.set(None)>"\u{2716}"</button>
                    </div>
                    {details.map(|d| view! { <div class="text-accent-green-soft text-xs">{d}</div> })}
                    <div class="text-text-blue">
                        {format!("{} {} {}  {} {}", tr_.jnow_label, tr_.ra_label, fmt_ra(ra),
                                 tr_.dec_label, fmt_dec(dec))}
                    </div>
                    <div class="text-text-muted">
                        {move || {
                            let j = JNow::new(ra, dec).to_j2000(clock.get().jd());
                            format!("{} {} {}  {} {}", tr().j2000_label, tr().ra_label, fmt_ra(j.ra_deg),
                                    tr().dec_label, fmt_dec(j.dec_deg))
                        }}
                    </div>
                    <div class="text-text-muted">
                        {move || {
                            let _ = clock.get();
                            let (alt, az) = altaz(ra, dec);
                            format!("{} {:+.1}\u{00b0}  {} {:.1}\u{00b0}", tr().overlay_alt, alt, tr().overlay_az, az)
                        }}
                    </div>
                    <div class="flex flex-wrap gap-1.5 mt-1 font-ui">
                        <button class=btn on:click=on_center>{tr_.center_here}</button>
                        <button class=format!("{btn} btn-primary")
                                disabled=move || goto_disabled.get()
                                on:click=on_goto.clone()>
                            {move || match goto_disabled.get().then(|| mount_busy.get()).flatten() {
                                Some(svc) => format!("{} ({})", tr().goto_here, svc),
                                None => tr().goto_here.to_string(),
                            }}
                        </button>
                        <button class=btn
                                disabled=move || align_disabled.get()
                                on:click=on_align.clone()>
                            {move || match mount_busy.get().or_else(|| camera_busy.get()) {
                                Some(svc) => format!("{} ({})", tr().goto_and_align, svc),
                                None => tr().goto_and_align.to_string(),
                            }}
                        </button>
                        <button class=btn on:click=on_open_framing.clone()>{tr_.sky_framing}</button>
                        <button class=btn on:click=on_add_scheduler.clone()>{tr_.sky_add_scheduler}</button>
                    </div>
                </div>
            }
        })}
    }
}

fn kind_label_for(kind: &HitKind, lang: Lang) -> &'static str {
    let s = t(lang);
    match kind {
        HitKind::Star    => s.kind_star,
        HitKind::Sun     => s.kind_sun,
        HitKind::Moon    => s.kind_moon,
        HitKind::Planet  => s.kind_planet,
        HitKind::Dso(d)  => match d {
            DsoType::Galaxy           => s.kind_galaxy,
            DsoType::OpenCluster      => s.kind_open_cluster,
            DsoType::GlobularCluster  => s.kind_globular,
            DsoType::Nebula           => s.kind_nebula,
            DsoType::PlanetaryNebula  => s.kind_planetary,
            DsoType::SupernovaRemnant => s.kind_snr,
            DsoType::GalaxyCluster    => s.kind_galaxy_cluster,
        },
    }
}
