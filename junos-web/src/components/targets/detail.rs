//! One target in full, in a sheet: its picture, what it is, its night (the
//! Scheduler's altitude chart with the time above the minimum altitude
//! highlighted), the bands worth a filter, and what to do with it — Show on
//! Sky, Framing, Goto, Add to Scheduler.

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE, FOOTER};
use crate::components::scheduler::altitude::{altitude, altitude_chart, hhmm, Night, Track};
use crate::components::sky::{fmt_dec, fmt_ra, goto_rade_msg};
use crate::compat::SiteSnapshot;
use crate::coords::J2000;
use crate::dso_catalog::Dso;
use crate::i18n::Translations;
use crate::ws::SendCmd;
use crate::{ActiveTabCtx, FramingCtx, SchedulerPrefillCtx, ServiceBusyCtx, SkyFocus, SkyFocusCtx, Tab};

use super::bands::{advice, band_rows};
use super::compute::Candidate;
use super::filters::kind_label;
use super::{fmt_minutes, fmt_size};

/// What the sheet shows, gathered when it opens.
pub struct Detail<'a> {
    pub dso: &'a Dso,
    pub c: Candidate,
    pub label: String,
    pub con_name: Option<String>,
    pub thumb: Option<String>,
    pub score: f64,
    pub night: Night,
    pub site: SiteSnapshot,
    pub min_alt: f64,
}

pub fn detail_body<C: Fn() + Copy + Send + Sync + 'static>(
    det: Detail<'_>,
    tr: &'static Translations,
    send: SendCmd,
    close: C,
) -> impl IntoView + use<C> {
    let Detail { dso, c, label, con_name, thumb, score, night, site, min_alt } = det;
    let (ra, dec) = (dso.ra_deg as f64, dso.dec_deg as f64);
    let name = dso.name.clone();

    // ── Actions ────────────────────────────────────────────────────────────
    let tab = use_context::<ActiveTabCtx>().map(|c| c.0);
    let go = move |t: Tab| if let Some(tab) = tab { tab.set(t) };
    let jnow = move || J2000::new(ra, dec).to_jnow(crate::astro::now_jd());

    let focus = use_context::<SkyFocusCtx>().map(|c| c.0);
    let sky_focus = SkyFocus {
        name: label.clone(),
        ra_deg: ra,
        dec_deg: dec,
        size_arcmin: dso.size_arcmin,
        mag: dso.known_mag(),
        kind: dso.kind,
        at_ms: Some(c.peak_ms),
    };
    let on_sky = move |_| {
        if let Some(f) = focus {
            f.set(Some(sky_focus.clone()));
        }
        close();
        go(Tab::Sky);
    };

    // The Framing Assistant lives in the Sky tab's DOM: open it, then go there.
    let framing = use_context::<FramingCtx>().map(|c| c.0);
    let name_framing = name.clone();
    let on_framing = move |_| {
        if let Some(f) = framing {
            let p = jnow();
            f.params.center.set(Some((p.ra_deg, p.dec_deg)));
            f.params.target.set(name_framing.clone());
            f.open.set(true);
        }
        close();
        go(Tab::Sky);
    };

    // The add-job form takes J2000.
    let prefill = use_context::<SchedulerPrefillCtx>().map(|c| c.0);
    let on_scheduler = move |_| {
        if let Some(p) = prefill {
            p.set(Some((name.clone(), ra, dec)));
        }
        close();
        go(Tab::Scheduler);
    };

    let busy = use_context::<ServiceBusyCtx>();
    let mount_busy = Signal::derive(move || busy.and_then(|b| b.mount_busy.get()));
    let below = altitude(ra, dec, js_sys::Date::now(), &site) < 0.0;
    let on_goto = move |_| {
        let p = jnow();
        send(goto_rade_msg(p.ra_deg, p.dec_deg));
        close();
    };

    // ── Facts ──────────────────────────────────────────────────────────────
    let fact = |k: &'static str, v: String| view! {
        <span class="text-text-muted">{k}</span>
        <span class="min-w-0 truncate text-text font-mono">{v}</span>
    };
    let facts = view! {
        <div class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
            {fact(tr.type_label, kind_label(dso.kind, tr).to_string())}
            {con_name.map(|n| fact(tr.targets_constellation, n))}
            {dso.known_mag().map(|m| fact(tr.mag_label, format!("{m:.1}")))}
            {(dso.size_arcmin > 0.0).then(|| fact(tr.size_label, fmt_size(dso.size_arcmin, dso.size_minor_arcmin)))}
            {fact(tr.j2000_label, format!("{}  {}", fmt_ra(ra), fmt_dec(dec)))}
            {fact(tr.targets_score_title, format!("{:.0}", score * 100.0))}
        </div>
    };

    let moon = if c.moon_up {
        format!("{:.0}\u{00b0}", c.moon_sep)
    } else {
        format!("{:.0}\u{00b0} \u{00b7} {}", c.moon_sep, tr.targets_moon_below)
    };
    let night_rows = view! {
        <div class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
            {fact(tr.targets_at_start, format!("{:.0}\u{00b0}", c.alt_start))}
            {fact(tr.targets_peak, format!("{:.0}\u{00b0} \u{00b7} {}", c.peak_alt, hhmm(c.peak_ms)))}
            {fact(tr.targets_above, format!("{min_alt:.0}\u{00b0}: {} \u{2192} {} \u{00b7} {}",
                                            hhmm(c.up_from), hhmm(c.up_to), fmt_minutes(c.up_minutes)))}
            {fact(tr.targets_moon_dist, moon)}
        </div>
    };
    let chart = altitude_chart(
        night,
        site,
        vec![Track { ra_deg: ra, dec_deg: dec, window: Some((c.up_from, c.up_to)), label: None }],
        Some(min_alt),
        tr,
    );

    let btn = "btn h-11 px-3 flex-1 min-w-[40%] md:min-w-0 whitespace-nowrap";
    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            {thumb.map(|src| view! {
                <img src=src alt="" class="block w-full max-h-72 object-contain rounded-md bg-black" />
            })}
            <div class=CARD>
                <span class="font-semibold text-text-blue-bright">{label}</span>
                {facts}
            </div>
            <div class=CARD>
                {chart}
                {night_rows}
            </div>
            <div class=CARD>
                <span class=CARD_TITLE>{tr.targets_wavelengths}</span>
                <span class="text-sm text-text">{advice(dso.lines, tr)}</span>
                {band_rows(dso.lines, tr)}
            </div>
        </div>
        <div class=format!("{FOOTER} flex-wrap gap-2")>
            <button class=format!("{btn} btn-primary") on:click=on_sky>{tr.targets_show_sky}</button>
            <button class=btn on:click=on_framing>{tr.sky_framing}</button>
            <button class=btn
                    disabled=move || below || mount_busy.get().is_some()
                    title=move || if below { tr.targets_below_horizon } else { "" }
                    on:click=on_goto>
                {move || match mount_busy.get() {
                    Some(svc) => format!("{} ({svc})", tr.targets_goto),
                    None => tr.targets_goto.to_string(),
                }}
            </button>
            <button class=btn on:click=on_scheduler>{tr.sky_add_scheduler}</button>
        </div>
    }
}
