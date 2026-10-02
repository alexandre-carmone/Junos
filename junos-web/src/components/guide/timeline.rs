//! Drift plot for the Guide tab: RA / DEC error over the last two minutes,
//! from the per-frame `new_guide_state {drift_ra, drift_de}` samples
//! (kstars/ekos/manager.cpp:2772-2776), with the guide state as a ribbon
//! underneath (`GuideStatusData.history`).
//!
//! The SVG stretches to its box (`preserveAspectRatio="none"`, strokes don't
//! scale) and the labels are HTML on top, so the plot stays legible from a
//! phone to a wide screen. The Y range is the smallest of ±1/2/4/8″ that
//! holds the samples.

use leptos::prelude::*;

use crate::i18n::Translations;
use crate::ws::{GuideDriftSample, GuideStateSample};

use super::tone;

const WINDOW_MS: f64 = 120_000.0;
const W: f64 = 1000.0;
const H: f64 = 100.0;

pub fn drift_plot(
    drift: &[GuideDriftSample],
    history: &[GuideStateSample],
    tr: &'static Translations,
) -> impl IntoView + use<> {
    let now = web_sys::js_sys::Date::now();
    let start = now - WINDOW_MS;
    let x = |t: f64| (t - start) / WINDOW_MS * W;

    let recent: Vec<&GuideDriftSample> = drift.iter().filter(|s| s.t_ms >= start).collect();
    let peak = recent.iter()
        .flat_map(|s| [s.ra, s.de])
        .filter(|v| v.is_finite())
        .fold(0.0_f64, |m, v| m.max(v.abs()));
    let range = [1.0, 2.0, 4.0, 8.0].into_iter().find(|r| peak <= *r).unwrap_or(8.0);
    let y = |v: f64| H / 2.0 - v.clamp(-range, range) / range * (H / 2.0);
    let path = |axis: fn(&GuideDriftSample) -> f64| {
        recent.iter()
            .filter(|s| axis(s).is_finite())
            .enumerate()
            .map(|(i, s)| format!("{}{:.1} {:.1}", if i == 0 { 'M' } else { 'L' }, x(s.t_ms), y(axis(s))))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (ra, de) = (path(|s| s.ra), path(|s| s.de));
    let empty = recent.is_empty();

    // One segment per guide state, clipped to the window.
    let ribbon = history.iter().enumerate().filter_map(|(i, h)| {
        let a = h.t_ms.max(start);
        let b = history.get(i + 1).map_or(now, |n| n.t_ms).min(now);
        (b > a).then(|| view! {
            <rect x=format!("{:.1}", x(a)) width=format!("{:.1}", x(b) - x(a)) height="1" fill=tone(&h.status).1 />
        })
    }).collect::<Vec<_>>();

    let hline = |at: f64, color: &'static str| view! {
        <line x1="0" x2=W y1=at y2=at stroke=color stroke-width="1" vector-effect="non-scaling-stroke" />
    };
    let vline = |at: f64| view! {
        <line x1=at x2=at y1="0" y2=H stroke="var(--border)" stroke-width="1" vector-effect="non-scaling-stroke" />
    };
    let axis_label = |pos: &'static str, text: String| view! {
        <span class=format!("absolute left-1.5 font-mono text-[10px] leading-none text-text-faint {pos}")>{text}</span>
    };

    view! {
        <div class="flex flex-col gap-1 md:flex-1 md:min-h-0">
            <div class="relative h-40 md:h-auto md:flex-1 md:min-h-48 rounded-md bg-bg-input-deep \
                        border border-border-base overflow-hidden">
                <svg viewBox=format!("0 0 {W} {H}") preserveAspectRatio="none" class="absolute inset-0 w-full h-full">
                    {hline(H * 0.25, "var(--border)")}
                    {hline(H * 0.75, "var(--border)")}
                    {[W * 0.25, W * 0.5, W * 0.75].map(vline)}
                    {hline(H * 0.5, "var(--border-mid)")}
                    <path d=de fill="none" stroke="var(--accent-amber)" stroke-width="1.5"
                          stroke-linejoin="round" vector-effect="non-scaling-stroke" />
                    <path d=ra fill="none" stroke="var(--text-blue)" stroke-width="1.5"
                          stroke-linejoin="round" vector-effect="non-scaling-stroke" />
                </svg>
                {axis_label("top-1.5", format!("+{range}\u{2033}"))}
                {axis_label("top-1/2 -translate-y-1/2", "0".into())}
                {axis_label("bottom-1.5", format!("\u{2212}{range}\u{2033}"))}
                <div class="absolute top-1.5 right-2 flex gap-3 font-mono text-[10px] leading-none">
                    <span class="text-text-blue">{format!("\u{25cf} {}", tr.ra_label)}</span>
                    <span class="text-accent-amber">{format!("\u{25cf} {}", tr.dec_label)}</span>
                </div>
                {empty.then(|| view! {
                    <div class="absolute inset-0 flex items-center justify-center px-6 text-center text-xs text-text-faint">
                        {tr.guide_no_drift}
                    </div>
                })}
            </div>
            <svg viewBox=format!("0 0 {W} 1") preserveAspectRatio="none" class="block w-full h-1.5 rounded-full bg-border-strong">
                {ribbon}
            </svg>
            <div class="flex justify-between font-mono text-[10px] leading-none text-text-faint">
                <span>"\u{2212}120 s"</span><span>"\u{2212}60 s"</span><span>"0 s"</span>
            </div>
        </div>
    }
}
