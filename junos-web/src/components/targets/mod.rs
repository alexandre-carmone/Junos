//! Targets tab — tonight's best deep-sky objects.
//!
//! The observing window runs from a start (astronomical dusk, or now once it
//! is dark) to an end (dawn), both editable, on any night. Every catalog
//! object that clears the minimum altitude in that window is a candidate
//! (`compute.rs`); the list ranks them by the Best score or by altitude, Moon
//! distance, size, time up or magnitude, and filters them by category,
//! wavelength, constellation and limits (`filters.rs`, all persisted). Each
//! row shows the bands worth a filter (`bands.rs`); a tap opens the target in
//! a sheet (`detail.rs`) with its night and the way to the Sky, the Framing
//! Assistant, the mount and the Scheduler.
//!
//! Layout as the other planning tabs: a header, the window card and the list
//! — one column on phones, window | list from `md` — and a pinned footer with
//! the count and the Filters button.

mod bands;
mod compute;
mod detail;
mod filters;

use std::collections::HashMap;
use std::sync::Arc;

use leptos::prelude::*;

use crate::catalog::CatalogData;
use crate::compat::SiteSnapshot;
use crate::components::form::{sheet, CARD, CARD_TITLE, CHIP, FOOTER};
use crate::components::scheduler::altitude::{hhmm, Night};
use crate::components::sky::clock;
use crate::components::tab_wheel_icons::tab_icon;
use crate::dom::event_target_value;
use crate::dso_catalog::{Dso, DsoCatalogData, DsoType};
use crate::i18n::{constellation_name, t, Lang, Translations};
use crate::ws::SendCmd;
use crate::{DsoTilesCtx, Tab};

use compute::{evaluate, score, Candidate, NightSky, STEP_MS};
use detail::{detail_body, Detail};
use filters::{filters_body, kind_label, Sort, TargetPrefs};

const HOUR_MS: f64 = 3_600_000.0;
/// Rows rendered at a time; "Show more" adds as many.
const PAGE: usize = 60;

/// "6h20", "45 min".
pub(crate) fn fmt_minutes(min: f64) -> String {
    let m = min.round() as u32;
    if m < 60 { format!("{m} min") } else { format!("{}h{:02}", m / 60, m % 60) }
}

/// "12′", "178×63′", "2.5°".
pub(crate) fn fmt_size(major: f32, minor: f32) -> String {
    if major >= 120.0 {
        format!("{:.1}\u{00b0}", major / 60.0)
    } else if minor > 0.0 && (major - minor).abs() > 0.5 {
        format!("{major:.0}\u{00d7}{minor:.0}\u{2032}")
    } else if major < 10.0 {
        format!("{major:.1}\u{2032}")
    } else {
        format!("{major:.0}\u{2032}")
    }
}

/// Local (hours, minutes) of an instant.
fn hm(ms: f64) -> (u32, u32) {
    let d = js_sys::Date::new(&ms.into());
    (d.get_hours(), d.get_minutes())
}

/// Short tag standing in for a missing thumbnail.
fn kind_tag(k: DsoType) -> &'static str {
    match k {
        DsoType::Galaxy => "Gx",
        DsoType::OpenCluster => "OC",
        DsoType::GlobularCluster => "GC",
        DsoType::Nebula => "Nb",
        DsoType::PlanetaryNebula => "PN",
        DsoType::SupernovaRemnant => "SNR",
        DsoType::GalaxyCluster => "GxC",
        DsoType::DarkNebula => "DN",
    }
}

/// Constellation name in the current language: French from the i18n table,
/// English (Latin) from the star catalog's centres, else the abbreviation.
fn con_name(abbr: &str, lang: Lang, en: &HashMap<String, String>) -> String {
    constellation_name(abbr, lang)
        .map(str::to_string)
        .or_else(|| en.get(abbr).cloned())
        .unwrap_or_else(|| abbr.to_string())
}

/// Name, common names and other designations contain `q` (lowercase).
fn name_matches(d: &Dso, q: &str) -> bool {
    q.is_empty()
        || std::iter::once(&d.name)
            .chain(&d.common_names)
            .chain(&d.fr_names)
            .chain(&d.ids)
            .any(|n| n.to_lowercase().contains(q))
}

/// A ranked candidate and its Best score.
#[derive(Clone, Copy, PartialEq)]
struct Row {
    c: Candidate,
    score: f64,
}

#[component]
pub fn TargetsTab(#[prop(into)] site: Signal<SiteSnapshot>, send: SendCmd) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let dso_catalog = use_context::<RwSignal<Option<Arc<DsoCatalogData>>>>()
        .unwrap_or_else(|| RwSignal::new(None));
    let star_catalog = use_context::<RwSignal<Option<Arc<CatalogData>>>>()
        .unwrap_or_else(|| RwSignal::new(None));
    let tiles = use_context::<DsoTilesCtx>().map(|c| c.0);

    let prefs = TargetPrefs::new();
    let query = RwSignal::new(String::new());
    let shown = RwSignal::new(PAGE);
    let filters_open = RwSignal::new(false);
    let selected = RwSignal::new(None::<usize>);

    // English constellation names, from the star catalog's centres.
    let en_cons = Memo::new(move |_| {
        star_catalog
            .get()
            .map(|c| c.centers.iter().map(|(a, n, _, _)| (a.clone(), n.clone())).collect::<HashMap<_, _>>())
            .unwrap_or_default()
    });

    // ── Window ────────────────────────────────────────────────────────────
    // The night (noon → noon) planned, and the start / end hours chosen in
    // it: `None` is dusk / dawn (22:00 / 04:00 when the sun never gets to
    // −18°).
    let now = js_sys::Date::now();
    let night_noon = RwSignal::new(clock::night_start(now + 6.0 * HOUR_MS));
    let night = Memo::new(move |_| Night::containing(night_noon.get() + 1.0, &site.get()));
    let start_hm = RwSignal::new(None::<(u32, u32)>);
    let end_hm = RwSignal::new(None::<(u32, u32)>);
    // Already dark: start now.
    if let Night { dusk: Some(dusk), dawn: Some(dawn), .. } = night.get_untracked() {
        if now > dusk && now < dawn - HOUR_MS {
            start_hm.set(Some(hm(now)));
        }
    }
    let window = Memo::new(move |_| {
        let (noon, n) = (night_noon.get(), night.get());
        let at = |chosen: Option<(u32, u32)>, auto: Option<f64>, fallback: u32| {
            chosen
                .map(|(h, m)| clock::night_hour(noon, h, m))
                .or(auto)
                .unwrap_or_else(|| clock::night_hour(noon, fallback, 0))
        };
        let start = at(start_hm.get(), n.dusk, 22);
        let end = at(end_hm.get(), n.dawn, 4).max(start + 3.0 * STEP_MS);
        (start, end)
    });
    let sky = Memo::new(move |_| {
        let (start, end) = window.get();
        Arc::new(NightSky::new(start, end, &site.get()))
    });

    // ── Candidates, then the ranked list ─────────────────────────────────
    let candidates = Memo::new(move |_| {
        let cat = dso_catalog.get()?;
        let sky = sky.get();
        let t0 = js_sys::Date::now();
        let out = evaluate(&cat.dsos, &sky, prefs.min_alt.get());
        debug_log!("targets: {} candidates in {:.0} ms", out.len(), js_sys::Date::now() - t0);
        Some(out)
    });
    // (rows, constellations left by the other filters: abbr → count)
    let ranked = Memo::new(move |_| {
        let Some(cat) = dso_catalog.get() else { return (Vec::new(), Vec::new()) };
        let f = prefs.filter_set();
        let con = prefs.con.get();
        let q = query.get().trim().to_lowercase();
        let (sort, w, min_alt) = (prefs.sort(), prefs.weights(), prefs.min_alt.get());
        let illum = sky.with(|s| s.illum);
        candidates.with(|cands| {
            let mut counts: HashMap<String, usize> = HashMap::new();
            let mut rows = Vec::new();
            for c in cands.iter().flatten() {
                let d = &cat.dsos[c.idx];
                if !f.passes(c, d) || !name_matches(d, &q) {
                    continue;
                }
                let dc = d.constellation().unwrap_or("");
                *counts.entry(dc.to_string()).or_default() += 1;
                if !con.is_empty() && dc != con {
                    continue;
                }
                rows.push(Row { c: *c, score: score(c, d, illum, min_alt, &w) });
            }
            let dso = |r: &Row| &cat.dsos[r.c.idx];
            rows.sort_by(|a, b| {
                let key = match sort {
                    Sort::Best => b.score.total_cmp(&a.score),
                    Sort::Alt => b.c.peak_alt.total_cmp(&a.c.peak_alt),
                    Sort::Moon => b.c.moon_sep.total_cmp(&a.c.moon_sep),
                    Sort::Size => dso(b).size_arcmin.total_cmp(&dso(a).size_arcmin),
                    Sort::Time => b.c.up_minutes.total_cmp(&a.c.up_minutes),
                    Sort::Mag => dso(a).vis_mag.total_cmp(&dso(b).vis_mag),
                };
                key.then(b.score.total_cmp(&a.score))
            });
            let mut counts: Vec<(String, usize)> = counts.into_iter().filter(|(a, _)| !a.is_empty()).collect();
            counts.sort();
            (rows, counts)
        })
    });
    let total = Signal::derive(move || ranked.with(|(rows, _)| rows.len()));
    // A new ranking starts from its top.
    Effect::new(move |_| {
        ranked.track();
        shown.set(PAGE);
    });
    let cons = Memo::new(move |_| {
        let (lang, en) = (lang.get(), en_cons.get());
        let mut list: Vec<(String, String, usize)> = ranked
            .with(|(_, c)| c.clone())
            .into_iter()
            .map(|(abbr, n)| (abbr.clone(), con_name(&abbr, lang, &en), n))
            .collect();
        list.sort_by(|a, b| a.1.cmp(&b.1));
        list
    });

    // ── Header & window card ─────────────────────────────────────────────
    let moon_badge = move || {
        let s = sky.get();
        let pct = s.illum * 100.0;
        let when = match s.moon_up() {
            None => tr().targets_moon_down.to_string(),
            Some((a, b)) if a <= s.start && b >= s.end => tr().targets_moon_all_night.to_string(),
            Some((a, b)) => format!("{} {}\u{2013}{}", tr().targets_moon_up, hhmm(a), hhmm(b)),
        };
        format!("\u{263e} {pct:.0}% \u{00b7} {when}")
    };
    let twilight_line = move || {
        let n = night.get();
        let f = |t: Option<f64>| t.map(hhmm).unwrap_or_else(|| "\u{2014}".into());
        format!("{} {} \u{00b7} {} {}", tr().targets_dusk, f(n.dusk), tr().targets_dawn, f(n.dawn))
    };
    let date_value = move || {
        let d = js_sys::Date::new(&night_noon.get().into());
        format!("{:04}-{:02}-{:02}", d.get_full_year(), d.get_month() + 1, d.get_date())
    };
    let on_date = move |ev: web_sys::Event| {
        let p: Vec<u32> = event_target_value(&ev).split('-').filter_map(|x| x.parse().ok()).collect();
        if let &[y, mo, d] = p.as_slice() {
            night_noon.set(clock::on_date(night_noon.get_untracked(), y, mo, d));
        }
    };
    let time_input = move |which: RwSignal<Option<(u32, u32)>>, is_start: bool| {
        view! {
            <input type="time" class="input w-full min-w-0 font-mono"
                   prop:value=move || {
                       let (s, e) = window.get();
                       let (h, m) = hm(if is_start { s } else { e });
                       format!("{h:02}:{m:02}")
                   }
                   on:change=move |ev| {
                       let p: Vec<u32> = event_target_value(&ev).split(':').filter_map(|x| x.parse().ok()).collect();
                       if let &[h, m, ..] = p.as_slice() {
                           which.set(Some((h, m)));
                       }
                   } />
        }
    };
    let on_dusk = move |_| {
        start_hm.set(None);
        end_hm.set(None);
    };
    let on_now = move |_| {
        let now = js_sys::Date::now();
        night_noon.set(clock::night_start(now));
        start_hm.set(Some(hm(now)));
    };

    let window_card = view! {
        <div class=CARD>
            <label class="flex flex-col gap-1">
                <span class="text-xs text-text-blue">{move || tr().targets_night}</span>
                <input type="date" class="input w-full min-w-0 font-mono" prop:value=date_value on:change=on_date />
            </label>
            <div class="grid grid-cols-2 gap-2">
                <label class="min-w-0 flex flex-col gap-1">
                    <span class="text-xs text-text-blue">{move || tr().targets_start}</span>
                    {time_input(start_hm, true)}
                </label>
                <label class="min-w-0 flex flex-col gap-1">
                    <span class="text-xs text-text-blue">{move || tr().targets_end}</span>
                    {time_input(end_hm, false)}
                </label>
            </div>
            <div class="flex gap-2">
                <button class=format!("{CHIP} flex-1") on:click=on_dusk>
                    {move || format!("{} \u{2192} {}", tr().targets_dusk, tr().targets_dawn)}
                </button>
                <button class=format!("{CHIP} flex-1") on:click=on_now>{move || tr().targets_now}</button>
            </div>
            <span class="font-mono text-xs text-text-muted">{twilight_line}</span>
            <span class="font-mono text-xs text-text-muted">{moon_badge}</span>
        </div>
    };

    let sort_card = view! {
        <div class=CARD>
            <div class="flex flex-wrap gap-1.5">
                {Sort::ALL.into_iter().map(|s| view! {
                    <button type="button"
                            class=move || if prefs.sort() == s { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                            on:click=move |_| prefs.set_sort(s)>
                        {move || s.label(tr())}
                    </button>
                }).collect_view()}
            </div>
            <input type="search" class="input w-full min-w-0"
                   placeholder=move || tr().targets_search
                   prop:value=move || query.get()
                   on:input=move |ev| query.set(event_target_value(&ev)) />
        </div>
    };

    // ── The list ─────────────────────────────────────────────────────────
    let thumb_of = move |d: &Dso| -> Option<(String, f64)> {
        let idx = tiles?.get()?;
        let tile = idx.by_name(&d.name)?;
        // Zoom the sprite in on a small object (it sits in a ≥ 1° field).
        let z = if d.size_arcmin > 0.0 {
            (tile.thumb_side() * 60.0 / (d.size_arcmin as f64 * 1.6)).clamp(1.0, 6.0)
        } else {
            1.0
        };
        Some((tile.thumb_url(), z))
    };
    let row_view = move |rank: usize, r: Row, d: &Dso, tr: &'static Translations, lang: Lang, en: &HashMap<String, String>| {
        let c = r.c;
        let idx = c.idx;
        let thumb = match thumb_of(d) {
            Some((src, z)) => view! {
                <span class="relative w-12 h-12 shrink-0 rounded-md overflow-hidden bg-black">
                    <img src=src alt="" loading="lazy" class="w-full h-full object-cover scale-[var(--z)]"
                         style=format!("--z:{z:.2}") />
                </span>
            }.into_any(),
            None => view! {
                <span class="w-12 h-12 shrink-0 rounded-md bg-bg-input-deep border border-border-base \
                             flex items-center justify-center font-mono text-[10px] text-text-faint">
                    {kind_tag(d.kind)}
                </span>
            }.into_any(),
        };
        let mut sub = vec![kind_label(d.kind, tr).to_string()];
        sub.extend(d.constellation().map(|a| con_name(a, lang, en)));
        sub.extend(d.known_mag().map(|m| format!("{} {m:.1}", tr.mag_label)));
        if d.size_arcmin > 0.0 {
            sub.push(fmt_size(d.size_arcmin, d.size_minor_arcmin));
        }
        let moon = if c.moon_up { format!("\u{263e} {:.0}\u{00b0}", c.moon_sep) } else { "\u{263e} \u{2014}".into() };
        view! {
            <button type="button"
                    class="w-full text-left flex items-center gap-3 px-3 py-2 border-b border-border-base \
                           bg-transparent hover:bg-bg-elev-2 cursor-pointer"
                    on:click=move |_| selected.set(Some(idx))>
                <span class="w-6 shrink-0 text-right font-mono text-xs text-text-faint">{rank}</span>
                {thumb}
                <div class="flex-1 min-w-0 flex flex-col gap-0.5">
                    <span class="truncate text-sm font-semibold text-text-blue-bright">{d.display_label(lang)}</span>
                    <span class="truncate text-xs text-text-muted">{sub.join(" \u{00b7} ")}</span>
                    <div class="flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-xs text-text">
                        <span title=tr.targets_peak_title>
                            {format!("\u{25b2} {:.0}\u{00b0} {}", c.peak_alt, hhmm(c.peak_ms))}
                        </span>
                        <span title=tr.targets_moon_title>{moon}</span>
                        <span title=tr.targets_time_title>{format!("\u{23f1} {}", fmt_minutes(c.up_minutes))}</span>
                        <span class="flex flex-wrap gap-1">{bands::chips(d.lines, tr)}</span>
                    </div>
                </div>
                <span class="shrink-0 w-8 text-right font-mono text-sm text-accent-cyan" title=tr.targets_score_title>
                    {format!("{:.0}", r.score * 100.0)}
                </span>
            </button>
        }
    };
    let list = move || {
        let (tr, lang) = (tr(), lang.get());
        let Some(cat) = dso_catalog.get() else {
            return view! { <p class="p-4 text-sm text-text-muted">{tr.targets_loading}</p> }.into_any();
        };
        let en = en_cons.get();
        let n = shown.get();
        ranked.with(|(rows, _)| {
            if rows.is_empty() {
                return view! { <p class="p-4 text-sm text-text-muted">{tr.targets_empty}</p> }.into_any();
            }
            let more = rows.len() > n;
            let items = rows.iter().take(n).enumerate()
                .map(|(i, r)| row_view(i + 1, *r, &cat.dsos[r.c.idx], tr, lang, &en))
                .collect_view();
            view! {
                {items}
                {more.then(|| view! {
                    <button class="btn btn-ghost w-full h-11 rounded-none" on:click=move |_| shown.update(|v| *v += PAGE)>
                        {tr.targets_show_more}
                    </button>
                })}
            }.into_any()
        })
    };

    // ── Sheets ───────────────────────────────────────────────────────────
    let close_detail = move || selected.set(None);
    let detail = move || {
        let idx = selected.get()?;
        let cat = dso_catalog.get()?;
        let row = ranked.with(|(rows, _)| rows.iter().find(|r| r.c.idx == idx).copied())?;
        let d = &cat.dsos[idx];
        let (tr, lang) = (tr(), lang.get());
        let det = Detail {
            dso: d,
            c: row.c,
            label: d.display_label(lang),
            con_name: d.constellation().map(|a| con_name(a, lang, &en_cons.get())),
            thumb: thumb_of(d).map(|(src, _)| src),
            score: row.score,
            night: night.get(),
            site: site.get(),
            min_alt: prefs.min_alt.get(),
        };
        let title = det.label.clone();
        Some(sheet(move || title.clone(), close_detail, detail_body(det, tr, Arc::clone(&send), close_detail)))
    };

    // Escape closes the open sheet.
    let esc = window_event_listener(leptos::ev::keydown, move |e| {
        if e.key() == "Escape" {
            selected.set(None);
            filters_open.set(false);
        }
    });
    on_cleanup(move || esc.remove());

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Targets)></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{move || tr().tab_targets}</span>
                <span class="ml-auto min-w-0 truncate font-mono text-xs text-text-muted">
                    {move || { let s = sky.get(); format!("\u{263e} {:.0}%", s.illum * 100.0) }}
                </span>
            </div>

            // Window | list — one column on phones.
            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:pl-4 md:pr-6">
                <div class="max-w-[1100px] mx-auto grid gap-3 md:grid-cols-[320px_minmax(0,1fr)] md:items-start">
                    <div class="min-w-0 flex flex-col gap-3 md:sticky md:top-0">
                        {window_card}
                        {sort_card}
                    </div>
                    <div class="panel min-w-0 overflow-hidden">
                        <div class="px-3 py-2 border-b border-border-base">
                            <span class=CARD_TITLE>
                                {move || format!("{} {}", total.get(), tr().targets_count)}
                            </span>
                        </div>
                        {list}
                    </div>
                </div>
            </div>

            // Footer: the window and the filters.
            <div class=format!("{FOOTER} md:pl-4 md:pr-6")>
                <span class="flex-1 min-w-0 truncate font-mono text-sm text-text-muted">
                    {move || {
                        let (s, e) = window.get();
                        format!("{} {} \u{00b7} {} \u{2192} {} ({})", total.get(), tr().targets_count,
                                hhmm(s), hhmm(e), fmt_minutes((e - s) / 60_000.0))
                    }}
                </span>
                <button class="btn btn-primary shrink-0 h-11 px-5 font-semibold" on:click=move |_| filters_open.set(true)>
                    {move || match prefs.active_count() {
                        0 => tr().targets_filters.to_string(),
                        n => format!("{} ({n})", tr().targets_filters),
                    }}
                </button>
            </div>

            <Show when=move || filters_open.get()>
                {sheet(move || tr().targets_filters, move || filters_open.set(false),
                       filters_body(prefs, cons, total))}
            </Show>
            {detail}
        </div>
    }
}
