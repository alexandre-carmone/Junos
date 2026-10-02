//! Guide-target scatter for the Guide tab: the same drift samples as
//! `timeline.rs`, plotted as (dRA, dDE) points in arcsec. Mirrors KStars'
//! `GuideTargetPlot` (kstars/ekos/guide/guidetargetplot.cpp): rings at 100 /
//! 150 / 200 % of `guiderAccuracyThreshold` (green / yellow / red) in a
//! ±3× range, NSEW labels, RA axis reversed. Past points are one path; the
//! latest one is a highlighted cross.

use leptos::prelude::*;

use crate::ws::GuideDriftSample;

/// Half-range in multiples of the accuracy radius (`guidetargetplot.cpp:58`).
const RANGE_MULT: f64 = 3.0;
/// SVG size in user units (square).
const S: f64 = 200.0;
const C: f64 = S / 2.0;

/// 1.5 → "1.5", 2 → "2".
fn fmt_num(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub fn target_plot(drift: &[GuideDriftSample], accuracy: f64, empty: &'static str) -> impl IntoView + use<> {
    let r = accuracy.max(0.1);
    let half = r * RANGE_MULT;
    let px = move |arcsec: f64| arcsec / half * C;
    // RA reversed, as `guidetargetplot.cpp:62` (xAxis->setRangeReversed).
    let pts: Vec<(f64, f64)> = drift.iter()
        .filter(|s| s.ra.is_finite() && s.de.is_finite())
        .map(|s| (C - px(s.ra), C - px(s.de)))
        .collect();
    let inside = |(x, y): &(f64, f64)| (0.0..=S).contains(x) && (0.0..=S).contains(y);
    let marks: String = pts.iter().filter(|p| inside(p)).map(|(x, y)| format!("M{x:.1} {y:.1}h0")).collect();
    let latest = pts.last().copied();

    let ring = |k: f64, color: &'static str| view! {
        <circle cx=C cy=C r=px(r * k) fill=color fill-opacity="0.08" stroke=color stroke-width="0.8" />
    };
    let compass = |x: f64, y: f64, label: &'static str| view! {
        <text x=x y=y fill="var(--text-faint)" font-size="9" text-anchor="middle" dominant-baseline="middle">{label}</text>
    };

    view! {
        <div class="relative w-full max-w-[280px] mx-auto">
            <svg viewBox=format!("0 0 {S} {S}") class="block w-full aspect-square rounded-md bg-bg-input-deep border border-border-base">
                {ring(2.0, "var(--state-err)")}
                {ring(1.5, "var(--state-warn)")}
                {ring(1.0, "var(--state-ok)")}
                {[0.25, 0.5, 0.75].map(|k| view! {
                    <circle cx=C cy=C r=px(r * k) fill="none" stroke="var(--border-mid)" stroke-width="0.5" stroke-dasharray="2 2" />
                })}
                <line x1="0" y1=C x2=S y2=C stroke="var(--border-mid)" stroke-width="0.5" />
                <line x1=C y1="0" x2=C y2=S stroke="var(--border-mid)" stroke-width="0.5" />
                {compass(C, 10.0, "N")}
                {compass(C, S - 10.0, "S")}
                {compass(S - 8.0, C + 10.0, "E")}
                {compass(8.0, C + 10.0, "W")}
                <path d=marks fill="none" stroke="var(--text-muted)" stroke-opacity="0.7"
                      stroke-width="2.6" stroke-linecap="round" />
                {latest.map(|(x, y)| view! {
                    <g opacity=if inside(&(x, y)) { "1" } else { "0.5" } stroke="var(--accent-amber)" stroke-width="1.4" fill="none">
                        <circle cx=x cy=y r="5" />
                        <path d=format!("M{:.1} {y:.1}h12M{x:.1} {:.1}v12", x - 6.0, y - 6.0) />
                    </g>
                })}
                <text x="5" y="11" fill="var(--text-faint)" font-size="9">{format!("\u{00b1}{}\u{2033}", fmt_num(half))}</text>
                <text x="5" y=S - 6.0 fill="var(--state-ok)" font-size="9">{format!("\u{25cb} {}\u{2033}", fmt_num(r))}</text>
            </svg>
            {pts.is_empty().then(|| view! {
                <div class="absolute inset-0 flex items-center justify-center px-8 text-center text-xs text-text-faint">{empty}</div>
            })}
        </div>
    }
}
