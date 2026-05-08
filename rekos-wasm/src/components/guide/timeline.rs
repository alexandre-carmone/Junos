//! Drift plot + state-timeline ribbon for the Guide tab.
//!
//! KStars emits drift samples (RA/DEC arcsec) per guide frame via a
//! `new_guide_state {drift_ra, drift_de}` event (see
//! kstars/ekos/manager.cpp:2772-2776). We capture them into
//! `GuideStatusData.drift` and plot the last 2 minutes here.
//!
//! A thin ribbon underneath shows the colour-coded guide state over the
//! same window (built from `GuideStatusData.history`).

use leptos::prelude::*;

use crate::ws::{GuideDriftSample, GuideStateSample};

const WINDOW_S: f64 = 120.0;
const VIEW_W: f64 = 800.0;
const PLOT_H: f64 = 160.0;
const RIBBON_H: f64 = 12.0;
const AXIS_LABEL_H: f64 = 14.0;

/// Plot Y-range clamped symmetrically. Real drift is typically < 2".
const Y_MIN: f64 = -4.0;
const Y_MAX: f64 = 4.0;

fn color_for(status: &str) -> &'static str {
    match status {
        "Idle" | "Aborted" | "Disconnected" | "" => "#555",
        "Calibrating" | "Selecting star" | "Looping" | "Capturing" | "Subtracting"
        | "Subframing" | "Reacquiring" => "#ffd060",
        "Calibrated" | "Connected" => "#88aaff",
        "Guiding" => "#7affa0",
        "Dithering" | "Dithering successful" | "Manual Dithering" | "Settling" => "#66e0e0",
        "Calibration error" | "Dithering error" | "Suspended" => "#ff6a6a",
        _ => "#c0c0d0",
    }
}

fn time_to_x(t_ms: f64, now_ms: f64) -> f64 {
    ((t_ms - (now_ms - WINDOW_S * 1000.0)) / (WINDOW_S * 1000.0)) * VIEW_W
}

fn y_to_px(arcsec: f64) -> f64 {
    let v = arcsec.clamp(Y_MIN, Y_MAX);
    PLOT_H - ((v - Y_MIN) / (Y_MAX - Y_MIN)) * PLOT_H
}

fn build_path(samples: &[GuideDriftSample], now_ms: f64, axis: Axis) -> String {
    let mut d = String::new();
    let mut started = false;
    for s in samples {
        let v = match axis {
            Axis::Ra => s.ra,
            Axis::De => s.de,
        };
        if !v.is_finite() {
            continue;
        }
        let x = time_to_x(s.t_ms, now_ms);
        let y = y_to_px(v);
        if !started {
            d.push_str(&format!("M{x:.1} {y:.1}"));
            started = true;
        } else {
            d.push_str(&format!(" L{x:.1} {y:.1}"));
        }
    }
    d
}

#[derive(Copy, Clone)]
enum Axis {
    Ra,
    De,
}

pub fn drift_plot(
    drift: &[GuideDriftSample],
    history: &[GuideStateSample],
) -> impl IntoView + use<> {
    let now_ms = web_sys::js_sys::Date::now();
    let start_ms = now_ms - WINDOW_S * 1000.0;

    // Filter drift samples to the window (for clean paths; we don't want
    // a line stretching in from outside).
    let drift_in_window: Vec<GuideDriftSample> = drift
        .iter()
        .filter(|s| s.t_ms >= start_ms)
        .cloned()
        .collect();
    let has_drift = !drift_in_window.is_empty();

    let ra_path = build_path(&drift_in_window, now_ms, Axis::Ra);
    let de_path = build_path(&drift_in_window, now_ms, Axis::De);

    // State-ribbon segments (x, w, color).
    let mut segments: Vec<(f64, f64, &'static str)> = Vec::new();
    for (i, sample) in history.iter().enumerate() {
        let seg_start = sample.t_ms.max(start_ms);
        let seg_end = history
            .get(i + 1)
            .map(|n| n.t_ms)
            .unwrap_or(now_ms)
            .min(now_ms);
        if seg_end <= start_ms || seg_start >= now_ms {
            continue;
        }
        let x0 = time_to_x(seg_start, now_ms);
        let x1 = time_to_x(seg_end, now_ms);
        segments.push((x0, (x1 - x0).max(1.0), color_for(&sample.status)));
    }
    let ribbon_placeholder = segments.is_empty();

    let total_h = PLOT_H + RIBBON_H + AXIS_LABEL_H;

    // Y-axis gridlines at -3, -2, -1, 0, +1, +2, +3 arcsec.
    let ygrid: Vec<i32> = (-3..=3).collect();
    // X-axis ticks at 0, 30, 60, 90, 120 seconds (rendered as -120..0 s).
    let xticks: Vec<f64> = (0..=4).map(|i| i as f64 * VIEW_W / 4.0).collect();

    view! {
        <div class="flex flex-col gap-1">
            <svg viewBox=format!("0 0 {} {}", VIEW_W, total_h)
                 width="100%"
                 class="bg-bg-input-deep border border-border-base">

                // Plot area background
                <rect x="0" y="0" width=VIEW_W height=PLOT_H fill="#0a0a12"/>

                // Horizontal gridlines + arcsec labels
                {ygrid.iter().map(|&v| {
                    let y = y_to_px(v as f64);
                    let is_zero = v == 0;
                    view! {
                        <g>
                            <line x1="0" y1=y x2=VIEW_W y2=y
                                  stroke=if is_zero { "#444" } else { "#1f1f28" }
                                  stroke-width=if is_zero { "0.8" } else { "0.4" }/>
                            <text x="4" y={y - 2.0}
                                  fill="#556" font-size="9"
                                  text-anchor="start">
                                {format!("{}\"", v)}
                            </text>
                        </g>
                    }
                }).collect::<Vec<_>>()}

                // Vertical time gridlines
                {xticks.iter().map(|&x| view! {
                    <line x1=x y1="0" x2=x y2=PLOT_H
                          stroke="#1f1f28" stroke-width="0.4"/>
                }).collect::<Vec<_>>()}

                // Drift traces (only if we have data)
                {if has_drift {
                    view! {
                        <g>
                            <path d=de_path fill="none"
                                  stroke="#ffb060" stroke-width="1.2"
                                  stroke-linejoin="round"/>
                            <path d=ra_path fill="none"
                                  stroke="#88aaff" stroke-width="1.2"
                                  stroke-linejoin="round"/>
                        </g>
                    }.into_any()
                } else {
                    view! {
                        <text x={VIEW_W / 2.0} y={PLOT_H / 2.0}
                              fill="#556" font-size="11"
                              text-anchor="middle" dominant-baseline="middle">
                            "waiting for drift samples…"
                        </text>
                    }.into_any()
                }}

                // Legend (top-right)
                <g transform=format!("translate({}, 10)", VIEW_W - 110.0)>
                    <line x1="0" y1="0" x2="14" y2="0" stroke="#88aaff" stroke-width="1.5"/>
                    <text x="18" y="3" fill="#88aaff" font-size="10">"RA"</text>
                    <line x1="40" y1="0" x2="54" y2="0" stroke="#ffb060" stroke-width="1.5"/>
                    <text x="58" y="3" fill="#ffb060" font-size="10">"DEC"</text>
                </g>

                // State ribbon underneath the plot
                <g transform=format!("translate(0, {})", PLOT_H)>
                    <rect x="0" y="0" width=VIEW_W height=RIBBON_H fill="#101018"/>
                    {if ribbon_placeholder {
                        view! { <rect x="0" y="0" width=VIEW_W height=RIBBON_H fill="#2a2a35"/> }.into_any()
                    } else {
                        segments.iter().map(|&(x, w, c)| view! {
                            <rect x=x y="0" width=w height=RIBBON_H fill=c/>
                        }).collect::<Vec<_>>().into_any()
                    }}
                </g>

                // X-axis labels
                {xticks.iter().enumerate().map(|(i, &x)| {
                    let label = format!("-{}s", (4 - i) * 30);
                    view! {
                        <text x=x y={PLOT_H + RIBBON_H + 10.0}
                              fill="#667" font-size="9"
                              text-anchor=if i == 0 { "start" }
                                          else if i == 4 { "end" }
                                          else { "middle" }>
                            {label}
                        </text>
                    }
                }).collect::<Vec<_>>()}
            </svg>
            <div class="text-xs text-[#667]">
                "RA/DEC drift (arcsec) over the last 2 minutes. \
                 Coloured ribbon shows guide state."
            </div>
        </div>
    }
}
