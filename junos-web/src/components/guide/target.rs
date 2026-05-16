//! 2-D guide-target scatter plot for the Guide tab.
//!
//! Companion to `timeline.rs`: same `new_guide_state {drift_ra, drift_de}`
//! samples (`GuideStatusData.drift`), but plotted as (dRA, dDE) points in
//! arcsec with concentric accuracy rings. Mirrors KStars' `GuideTargetPlot`
//! (kstars/ekos/guide/guidetargetplot.cpp): 100% / 150% / 200% of
//! `guiderAccuracyThreshold` drawn as green / yellow / red rings, fainter
//! 25%/50%/75% grid rings inside, NSEW labels, RA axis reversed.

use leptos::prelude::*;

use crate::ws::GuideDriftSample;

/// SVG viewBox half-width in user units (math coords, before mapping).
/// Matches QCustomPlot range `-3R..+3R` in `guidetargetplot.cpp:58-59`.
const RANGE_MULT: f64 = 3.0;
/// Rendered SVG pixel size (square).
const VIEW_PX: f64 = 200.0;

fn fmt_num(v: f64) -> String {
    // Strip trailing zeroes for compact tick labels (1.5 → "1.5", 2 → "2").
    let s = format!("{:.2}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() || s == "-" { "0".into() } else { s }
}

/// Render the target scatter plot.
///
/// `accuracy_radius` is `Options::guiderAccuracyThreshold` in arcsec
/// (KStars default 1.5"). Past drift samples are drawn as small gray
/// stars; the latest sample is highlighted with a yellow plus-circle.
pub fn target_plot(drift: &[GuideDriftSample], accuracy_radius: f64) -> impl IntoView + use<> {
    let r = accuracy_radius.max(0.1); // guard against 0
    let half = r * RANGE_MULT; // axis half-range in arcsec

    // arcsec → SVG pixel mapping (origin at center).
    // RA reversed: positive west→east, so we negate dRA.
    // `guidetargetplot.cpp:62` (xAxis->setRangeReversed(true)).
    let to_px = move |arcsec: f64| (arcsec / half) * (VIEW_PX / 2.0);
    let cx = VIEW_PX / 2.0;
    let cy = VIEW_PX / 2.0;

    // Filter NaN/inf out and copy for ownership.
    let pts: Vec<GuideDriftSample> = drift
        .iter()
        .filter(|s| s.ra.is_finite() && s.de.is_finite())
        .cloned()
        .collect();
    let has_pts = !pts.is_empty();
    let latest = pts.last().cloned();

    // Tick marks at integer arcsec where they fit inside ±half.
    let max_tick = half.floor() as i32;
    let ticks: Vec<i32> = (-max_tick..=max_tick).filter(|&v| v != 0).collect();

    // Ring radii in SVG px (1×, 1.5×, 2× accuracy + faint inner guides).
    let ring_inner: Vec<(f64, &'static str)> = vec![
        (r * 0.25, "#3a3a48"),
        (r * 0.50, "#3a3a48"),
        (r * 0.75, "#3a3a48"),
    ];

    view! {
        <div class="flex flex-col gap-1">
            <svg viewBox=format!("0 0 {0} {0}", VIEW_PX)
                 width="100%"
                 preserveAspectRatio="xMidYMid meet"
                 class="bg-bg-input-deep border border-border-base aspect-square">

                <rect x="0" y="0" width=VIEW_PX height=VIEW_PX fill="#0a0a12"/>

                // ── Accuracy rings (red 2× → yellow 1.5× → green 1×) ────
                // Painter's order: largest first so smaller rings sit on top.
                <circle cx=cx cy=cy r=to_px(r * 2.0)
                        fill="rgba(255,0,0,0.10)" stroke="#ff5555" stroke-width="0.6"/>
                <circle cx=cx cy=cy r=to_px(r * 1.5)
                        fill="rgba(255,255,0,0.10)" stroke="#ffcc33" stroke-width="0.6"/>
                <circle cx=cx cy=cy r=to_px(r)
                        fill="rgba(0,255,0,0.10)" stroke="#33cc66" stroke-width="0.8"/>

                // ── Faint inner grid rings (25/50/75 %) ─────────────────
                {ring_inner.into_iter().map(|(rr, c)| view! {
                    <circle cx=cx cy=cy r=to_px(rr)
                            fill="none" stroke=c stroke-width="0.4"
                            stroke-dasharray="2 2"/>
                }).collect::<Vec<_>>()}

                // ── Crosshair (zero lines) ──────────────────────────────
                <line x1="0" y1=cy x2=VIEW_PX y2=cy stroke="#444" stroke-width="0.5"/>
                <line x1=cx y1="0" x2=cx y2=VIEW_PX stroke="#444" stroke-width="0.5"/>

                // ── Integer arcsec ticks on the axes ────────────────────
                {ticks.iter().map(|&v| {
                    // RA reversed → negate
                    let x = cx + to_px(-(v as f64));
                    let y = cy - to_px(v as f64); // SVG y grows down
                    view! {
                        <g>
                            <line x1=x y1={cy - 2.0} x2=x y2={cy + 2.0}
                                  stroke="#556" stroke-width="0.5"/>
                            <line x1={cx - 2.0} y1=y x2={cx + 2.0} y2=y
                                  stroke="#556" stroke-width="0.5"/>
                        </g>
                    }
                }).collect::<Vec<_>>()}

                // ── NSEW compass labels (matches guidetargetplot.cpp:194-231) ─
                <text x={cx} y={cy - to_px(half) + 10.0}
                      fill="#9aa" font-size="9"
                      text-anchor="middle" dominant-baseline="middle">"N"</text>
                <text x={cx} y={cy + to_px(half) - 10.0}
                      fill="#9aa" font-size="9"
                      text-anchor="middle" dominant-baseline="middle">"S"</text>
                <text x={cx + to_px(half) - 8.0} y={cy + 12.0}
                      fill="#9aa" font-size="9"
                      text-anchor="middle" dominant-baseline="middle">"E"</text>
                <text x={cx - to_px(half) + 8.0} y={cy + 12.0}
                      fill="#9aa" font-size="9"
                      text-anchor="middle" dominant-baseline="middle">"W"</text>

                // ── Past drift points (gray stars) ──────────────────────
                {if has_pts {
                    pts.iter().map(|s| {
                        let px = cx + to_px(-s.ra); // RA reversed
                        let py = cy - to_px(s.de);
                        // Skip points outside the rendered area to keep the SVG light.
                        if px < -2.0 || px > VIEW_PX + 2.0
                            || py < -2.0 || py > VIEW_PX + 2.0 {
                            return view! { <g></g> }.into_any();
                        }
                        view! {
                            <g>
                                <line x1={px - 2.0} y1=py x2={px + 2.0} y2=py
                                      stroke="#9a9aa8" stroke-width="0.7"/>
                                <line x1=px y1={py - 2.0} x2=px y2={py + 2.0}
                                      stroke="#9a9aa8" stroke-width="0.7"/>
                                <line x1={px - 1.5} y1={py - 1.5}
                                      x2={px + 1.5} y2={py + 1.5}
                                      stroke="#9a9aa8" stroke-width="0.5"/>
                                <line x1={px - 1.5} y1={py + 1.5}
                                      x2={px + 1.5} y2={py - 1.5}
                                      stroke="#9a9aa8" stroke-width="0.5"/>
                            </g>
                        }.into_any()
                    }).collect::<Vec<_>>().into_any()
                } else {
                    view! {
                        <text x=cx y=cy fill="#556" font-size="10"
                              text-anchor="middle" dominant-baseline="middle">
                            "waiting for drift samples…"
                        </text>
                    }.into_any()
                }}

                // ── Latest point (yellow plus-circle) ───────────────────
                {latest.map(|s| {
                    let px = cx + to_px(-s.ra);
                    let py = cy - to_px(s.de);
                    let inside = px >= 0.0 && px <= VIEW_PX
                              && py >= 0.0 && py <= VIEW_PX;
                    view! {
                        <g opacity=if inside { "1" } else { "0.5" }>
                            <circle cx=px cy=py r="5" fill="none"
                                    stroke="#ffe066" stroke-width="1.4"/>
                            <line x1={px - 6.0} y1=py x2={px + 6.0} y2=py
                                  stroke="#ffe066" stroke-width="1.2"/>
                            <line x1=px y1={py - 6.0} x2=px y2={py + 6.0}
                                  stroke="#ffe066" stroke-width="1.2"/>
                        </g>
                    }
                })}

                // ── Axis range hint (top-left) ──────────────────────────
                <text x="4" y="10" fill="#667" font-size="9">
                    {format!("±{}\"", fmt_num(half))}
                </text>
            </svg>
            <div class="text-xs text-[#667]">
                {format!("Target error (arcsec). Rings: {}\" / {}\" / {}\".",
                    fmt_num(r), fmt_num(r * 1.5), fmt_num(r * 2.0))}
            </div>
        </div>
    }
}
