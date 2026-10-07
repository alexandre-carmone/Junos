//! Altitude tonight — the Scheduler's take on KStars' altitude graph
//! (ekos/scheduler/scheduleraltitudegraph.cpp): each target's altitude from
//! dusk − 1 h to dawn + 1 h, its start → end window highlighted, the Moon
//! dashed and twilight shaded. Ekos Live only sends a job's current altitude,
//! so the curves are recomputed here from its J2000 RA/Dec and the site.
//!
//! Same idiom as the Guide drift plot: the SVG stretches to its box (strokes
//! don't scale) and the labels are HTML on top. A tap or hover reads the time
//! and altitudes at that point.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::astro;
use crate::compat::SiteSnapshot;
use crate::components::form::CARD_TITLE;
use crate::components::sky::clock;
use crate::coords::J2000;
use crate::ephemeris;
use crate::i18n::Translations;

const HOUR_MS: f64 = 3_600_000.0;
/// Curve sampling, as KStars.
const STEP_MS: f64 = 600_000.0;
const W: f64 = 1000.0;
const H: f64 = 100.0;
/// Bottom of the Y axis: a strip of ground under the horizon.
const ALT_LO: f64 = -10.0;

/// Tonight's plot span and astronomical dusk / dawn, Unix ms.
#[derive(Clone, Copy, PartialEq)]
pub struct Night {
    pub start: f64,
    pub end: f64,
    pub dusk: Option<f64>,
    pub dawn: Option<f64>,
}

impl Night {
    /// The coming night — or the current one until 06:00.
    pub fn tonight(site: &SiteSnapshot) -> Self {
        Self::of(clock::night_start(js_sys::Date::now() + 6.0 * HOUR_MS), site)
    }

    /// The night (noon → noon) that `ms` falls in.
    pub fn containing(ms: f64, site: &SiteSnapshot) -> Self {
        Self::of(clock::night_start(ms), site)
    }

    /// Dusk − 1 h → dawn + 1 h like KStars; 18:00 → 06:00 when the sun never
    /// reaches −18°.
    fn of(noon: f64, site: &SiteSnapshot) -> Self {
        let (dusk, dawn) = clock::night_twilights(noon, site.latitude, site.longitude);
        Self {
            start: dusk.map_or(noon + 6.0 * HOUR_MS, |t| t - HOUR_MS),
            end: dawn.map_or(noon + 18.0 * HOUR_MS, |t| t + HOUR_MS),
            dusk,
            dawn,
        }
    }
}

/// Instants `step` apart from `from` to `to`, both included; nothing when
/// `from` is past `to`.
pub fn samples(from: f64, to: f64, step: f64) -> impl Iterator<Item = f64> {
    let n = ((to - from) / step).ceil() as i64;
    (0..=n).map(move |i| (from + i as f64 * step).min(to))
}

fn alt_jnow(ra_deg: f64, dec_deg: f64, jd: f64, site: &SiteSnapshot) -> f64 {
    let lst = astro::lst_deg(astro::gmst_deg(jd), site.longitude);
    astro::eq_to_altaz(ra_deg, dec_deg, lst, site.latitude).0
}

/// Altitude (°) of a J2000 position at `ms`.
pub fn altitude(ra_deg: f64, dec_deg: f64, ms: f64, site: &SiteSnapshot) -> f64 {
    let jd = clock::ms_to_jd(ms);
    let p = J2000::new(ra_deg, dec_deg).to_jnow(jd);
    alt_jnow(p.ra_deg, p.dec_deg, jd, site)
}

fn moon_altitude(ms: f64, site: &SiteSnapshot) -> f64 {
    let jd = clock::ms_to_jd(ms);
    let m = ephemeris::moon(jd).jnow;
    alt_jnow(m.ra_deg, m.dec_deg, jd, site)
}

/// Local "HH:MM".
pub fn hhmm(ms: f64) -> String {
    let d = js_sys::Date::new(&ms.into());
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}

/// A `scheduler_get_jobs` time (KStars' local ISO) as Unix ms; `None` for "--".
pub fn job_ms(job: &serde_json::Value, key: &str) -> Option<f64> {
    job[key].as_str().map(js_sys::Date::parse).filter(|t| t.is_finite())
}

/// One curve: a J2000 target, its capture window (Unix ms) when known, and a
/// label drawn at its peak ("#2" in the job overview).
#[derive(Clone)]
pub struct Track {
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub window: Option<(f64, f64)>,
    pub label: Option<String>,
}

pub fn altitude_chart(
    night: Night,
    site: SiteSnapshot,
    tracks: Vec<Track>,
    min_alt: Option<f64>,
    tr: &'static Translations,
) -> impl IntoView + use<> {
    let Night { start, end, dusk, dawn } = night;
    let x = move |t: f64| (t - start) / (end - start) * W;
    let y = |alt: f64| (90.0 - alt.clamp(ALT_LO, 90.0)) / (90.0 - ALT_LO) * H;
    let path = |pts: &[(f64, f64)]| {
        pts.iter()
            .enumerate()
            .map(|(i, (t, a))| format!("{}{:.1} {:.2}", if i == 0 { 'M' } else { 'L' }, x(*t), y(*a)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let curve = |k: &Track, from: f64, to: f64| -> Vec<(f64, f64)> {
        samples(from, to, STEP_MS).map(|t| (t, altitude(k.ra_deg, k.dec_deg, t, &site))).collect()
    };

    // Twilight before dusk and after dawn; all of it when there's no night.
    let twilight = [(start, dusk.unwrap_or(end)), (dawn.unwrap_or(end), end)]
        .into_iter()
        .filter(|(a, b)| b > a)
        .map(|(a, b)| view! {
            <rect x=format!("{:.1}", x(a)) width=format!("{:.1}", x(b) - x(a)) height=H
                  fill="var(--text-blue)" fill-opacity="0.08" />
        })
        .collect::<Vec<_>>();

    // Even local hours: a gridline each, labelled away from the edges.
    let hours: Vec<(f64, u32)> = {
        let d = js_sys::Date::new(&start.into());
        d.set_minutes(0);
        d.set_seconds(0);
        d.set_milliseconds(0);
        samples(d.get_time() + HOUR_MS, end, HOUR_MS)
            .filter(|t| *t < end)
            .map(|t| (t, js_sys::Date::new(&t.into()).get_hours()))
            .filter(|(_, h)| h % 2 == 0)
            .collect()
    };
    let hline = |alt: f64, color: &'static str| view! {
        <line x1="0" x2=W y1=y(alt) y2=y(alt) stroke=color stroke-width="1" vector-effect="non-scaling-stroke" />
    };
    let vlines = hours.iter().map(|(t, _)| view! {
        <line x1=x(*t) x2=x(*t) y1="0" y2=H stroke="var(--border)" stroke-width="1" vector-effect="non-scaling-stroke" />
    }).collect::<Vec<_>>();
    let hour_labels = hours.iter()
        .map(|(t, h)| (x(*t) / W * 100.0, *h))
        .filter(|(pct, _)| (4.0..96.0).contains(pct))
        .map(|(pct, h)| view! {
            <span class="absolute -translate-x-1/2" style=format!("left:{pct:.1}%")>{format!("{h:02}h")}</span>
        })
        .collect::<Vec<_>>();
    let alt_label = |alt: f64| view! {
        <span class="absolute left-1.5 -translate-y-full font-mono text-[10px] leading-none text-text-faint pb-0.5"
              style=format!("top:{:.1}%", y(alt) / H * 100.0)>
            {format!("{alt:.0}\u{00b0}")}
        </span>
    };

    let mid_jd = clock::ms_to_jd(0.5 * (start + end));
    let moon_pct = ephemeris::moon(mid_jd).phase.unwrap_or(0.0) * 100.0;
    let moon: Vec<(f64, f64)> = samples(start, end, STEP_MS).map(|t| (t, moon_altitude(t, &site))).collect();

    // Per track: a thick stroke over the window — and a band behind it when
    // there's a single track (several jobs' bands would tile the night).
    let mut bands = Vec::new();
    let mut curves = Vec::new();
    let mut runs = Vec::new();
    let mut labels = Vec::new();
    for k in &tracks {
        let pts = curve(k, start, end);
        if let Some((a, b)) = k.window.map(|(a, b)| (a.max(start), b.min(end))).filter(|(a, b)| b > a) {
            if tracks.len() == 1 {
                bands.push(view! {
                    <rect x=format!("{:.1}", x(a)) width=format!("{:.1}", x(b) - x(a)) height=H
                          fill="var(--accent-cyan)" fill-opacity="0.10" />
                });
            }
            runs.push(view! {
                <path d=path(&curve(k, a, b)) fill="none" stroke="var(--accent-cyan)" stroke-width="3"
                      stroke-linecap="round" stroke-linejoin="round" vector-effect="non-scaling-stroke" />
            });
        }
        if let (Some(label), Some((t, a))) = (&k.label, pts.iter().copied().max_by(|p, q| p.1.total_cmp(&q.1))) {
            let top = y(a) / H * 100.0;
            // Above the peak, or under it when that's off the top.
            let cls = if top > 12.0 { "-translate-y-full pb-0.5" } else { "pt-0.5" };
            labels.push(view! {
                <span class=format!("absolute -translate-x-1/2 text-text {cls}")
                      style=format!("left:{:.1}%;top:{top:.1}%", (x(t) / W * 100.0).clamp(3.0, 97.0))>
                    {label.clone()}
                </span>
            });
        }
        curves.push(view! {
            <path d=path(&pts) fill="none" stroke="var(--text-blue)" stroke-width="2"
                  stroke-linejoin="round" vector-effect="non-scaling-stroke" />
        });
    }

    let now = js_sys::Date::now();
    let now_line = (start..end).contains(&now).then(|| view! {
        <line x1=x(now) x2=x(now) y1="0" y2=H stroke="var(--text-dim)" stroke-opacity="0.6" stroke-width="1"
              vector-effect="non-scaling-stroke" />
    });

    // Tap / hover readout: the time and every altitude at that point. A touch
    // keeps it until the next tap; a mouse clears it on leave.
    let hover = RwSignal::new(None::<f64>);
    let on_point = move |ev: web_sys::PointerEvent| {
        let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        let r = el.get_bounding_client_rect();
        if r.width() > 0.0 {
            hover.set(Some(((ev.client_x() as f64 - r.left()) / r.width()).clamp(0.0, 1.0)));
        }
    };
    let on_leave = move |ev: web_sys::PointerEvent| if ev.pointer_type() == "mouse" { hover.set(None) };
    let readout = move |f: f64| {
        let t = start + f * (end - start);
        let alts = tracks.iter().map(|k| {
            let alt = altitude(k.ra_deg, k.dec_deg, t, &site);
            match &k.label {
                Some(l) => format!("{l} {alt:.0}\u{00b0}"),
                None => format!("{alt:.0}\u{00b0}"),
            }
        });
        let moon = format!("\u{263e} {:.0}\u{00b0}", moon_altitude(t, &site));
        std::iter::once(hhmm(t)).chain(alts).chain(std::iter::once(moon)).collect::<Vec<_>>().join(" \u{00b7} ")
    };

    view! {
        <div class="flex flex-col gap-1">
            // Title, or the readout while pointing; the Moon's illumination.
            <div class="flex items-baseline gap-2 min-h-4 font-mono text-xs text-text-muted">
                {move || match hover.get() {
                    Some(f) => view! { <span class="flex-1 min-w-0 truncate text-text">{readout(f)}</span> }.into_any(),
                    None => view! { <span class=format!("{CARD_TITLE} flex-1 min-w-0 truncate")>{tr.sched_alt_title}</span> }.into_any(),
                }}
                <span class="shrink-0" title=tr.sched_alt_moon>{format!("\u{263e} {moon_pct:.0}%")}</span>
            </div>
            <div class="relative h-40 md:h-48 rounded-md bg-bg-input-deep border border-border-base overflow-hidden \
                        touch-pan-y select-none cursor-crosshair font-mono text-[10px] leading-none text-text-muted"
                 on:pointerdown=on_point on:pointermove=on_point on:pointerleave=on_leave>
                <svg viewBox=format!("0 0 {W} {H}") preserveAspectRatio="none" class="absolute inset-0 w-full h-full">
                    {twilight}
                    <rect y=y(0.0) width=W height={H - y(0.0)} fill="var(--border-strong)" fill-opacity="0.7" />
                    {vlines}
                    {hline(30.0, "var(--border)")}
                    {hline(60.0, "var(--border)")}
                    {hline(0.0, "var(--border-mid)")}
                    {min_alt.map(|m| view! {
                        <line x1="0" x2=W y1=y(m) y2=y(m) stroke="var(--state-warn)" stroke-opacity="0.8" stroke-width="1"
                              stroke-dasharray="4 3" vector-effect="non-scaling-stroke" />
                    })}
                    <path d=path(&moon) fill="none" stroke="var(--text-muted)" stroke-width="1" stroke-dasharray="2 3"
                          vector-effect="non-scaling-stroke" />
                    {bands}
                    {curves}
                    {runs}
                    {now_line}
                </svg>
                {alt_label(0.0)}
                {alt_label(30.0)}
                {alt_label(60.0)}
                {labels}
                {move || hover.get().map(|f| view! {
                    <div class="absolute inset-y-0 w-px bg-text-dim pointer-events-none" style=format!("left:{:.2}%", f * 100.0)></div>
                })}
            </div>
            <div class="relative h-3 font-mono text-[10px] leading-none text-text-faint">{hour_labels}</div>
        </div>
    }
}
