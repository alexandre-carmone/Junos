//! Drift plot for the Guide tab, both backends: RA / Dec error over the
//! window (2 or 5 min), the guide pulses as bars from the centre line, the
//! guiding events as marks across the plot, and the guide state as a ribbon
//! underneath. Each backend builds a [`DriftSeries`] in a Memo — Ekos from
//! the per-frame `new_guide_state {drift_ra, drift_de}` samples
//! (kstars/ekos/manager.cpp:2772-2776), in ″ and without pulses — so the plot
//! only redraws when the series changes.
//!
//! The SVG stretches to its box (`preserveAspectRatio="none"`, strokes don't
//! scale) and the labels are HTML on top, so the plot stays legible from a
//! phone to a wide screen. The Y range is the smallest of ±1/2/4/8 (in the
//! series' unit) that holds the samples.

use leptos::prelude::*;

use crate::i18n::Translations;
use crate::ws::{GuideDriftSample, GuideStateSample, GuiderMarker, GuiderMarkerKind};

use super::tone;

/// The default window, and Ekos' only one.
pub const WINDOW_MS: f64 = 120_000.0;
/// At most this many samples are drawn; a longer window is decimated.
const MAX_POINTS: usize = 400;
const W: f64 = 1000.0;
const H: f64 = 100.0;

/// What the plot draws, already cut to its window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriftSeries {
    pub start_ms: f64,
    pub end_ms: f64,
    /// RA / Dec error, in `unit`.
    pub points: Vec<GuideDriftSample>,
    /// (t_ms, RA pulse ms, Dec pulse ms), signed: West and North positive.
    pub pulses: Vec<(f64, f64, f64)>,
    pub markers: Vec<GuiderMarker>,
    /// The state in force at the window's start, then each change.
    pub states: Vec<GuideStateSample>,
    /// `″` or ` px`.
    pub unit: &'static str,
}

impl DriftSeries {
    /// Ekos' samples are stamped on arrival, so its window ends now.
    pub fn ekos(drift: &[GuideDriftSample], history: &[GuideStateSample]) -> Self {
        let end_ms = web_sys::js_sys::Date::now();
        let start_ms = end_ms - WINDOW_MS;
        let recent: Vec<&GuideDriftSample> = drift.iter().filter(|s| s.t_ms >= start_ms).collect();
        Self {
            start_ms,
            end_ms,
            points: decimate(&recent).into_iter().cloned().collect(),
            pulses: Vec::new(),
            markers: Vec::new(),
            states: ribbon(history, start_ms),
            unit: "\u{2033}",
        }
    }
}

/// At most `MAX_POINTS` of `v`, evenly spread, first and last kept.
pub fn decimate<'a, T>(v: &[&'a T]) -> Vec<&'a T> {
    let n = v.len();
    if n <= MAX_POINTS {
        return v.to_vec();
    }
    (0..MAX_POINTS).map(|i| v[i * (n - 1) / (MAX_POINTS - 1)]).collect()
}

/// The states that still show after `start_ms`.
pub fn ribbon(states: &[GuideStateSample], start_ms: f64) -> Vec<GuideStateSample> {
    states
        .iter()
        .enumerate()
        .filter(|(i, _)| states.get(i + 1).map_or(true, |n| n.t_ms > start_ms))
        .map(|(_, s)| s.clone())
        .collect()
}

/// A marker's colour and label; `None` for the events the plot leaves out.
fn marker_style(kind: GuiderMarkerKind, tr: &'static Translations) -> Option<(&'static str, &'static str)> {
    use GuiderMarkerKind as K;
    Some(match kind {
        K::Dither => ("var(--accent-cyan)", tr.guider_marker_dither),
        K::SettleBegin => ("var(--text-faint)", tr.guider_marker_settle_begin),
        K::Settled => ("var(--state-ok)", tr.guider_marker_settled),
        K::SettleFailed => ("var(--state-err)", tr.guider_marker_settle_failed),
        K::StarLost => ("var(--state-err)", tr.guider_marker_star_lost),
        K::CalibrationDone => ("var(--state-info)", tr.guider_marker_cal_done),
        K::Paused => ("var(--state-warn)", tr.guider_marker_paused),
        K::Resumed => ("var(--state-ok)", tr.guider_marker_resumed),
        _ => return None,
    })
}

/// `box_cls` sizes the plot area (the series' own card or a pinned pane).
pub fn drift_plot(s: &DriftSeries, box_cls: &'static str, tr: &'static Translations) -> impl IntoView + use<> {
    let (start, end) = (s.start_ms, s.end_ms.max(s.start_ms + 1.0));
    let x = |t: f64| (t - start) / (end - start) * W;

    let peak = s.points.iter()
        .flat_map(|p| [p.ra, p.de])
        .filter(|v| v.is_finite())
        .fold(0.0_f64, |m, v| m.max(v.abs()));
    let range = [1.0, 2.0, 4.0, 8.0].into_iter().find(|r| peak <= *r).unwrap_or(8.0);
    let y = |v: f64| H / 2.0 - v.clamp(-range, range) / range * (H / 2.0);
    let path = |axis: fn(&GuideDriftSample) -> f64| {
        s.points.iter()
            .filter(|p| axis(p).is_finite())
            .enumerate()
            .map(|(i, p)| format!("{}{:.1} {:.1}", if i == 0 { 'M' } else { 'L' }, x(p.t_ms), y(axis(p))))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (ra, de) = (path(|p| p.ra), path(|p| p.de));

    // Pulses: one bar per sample from the centre line, a quarter of the
    // height at the window's longest; one path per axis.
    let longest = s.pulses.iter().flat_map(|p| [p.1.abs(), p.2.abs()]).fold(100.0_f64, f64::max);
    let bars = |axis: fn(&(f64, f64, f64)) -> f64| {
        s.pulses.iter()
            .filter(|p| axis(p) != 0.0)
            .map(|p| format!("M{:.1} {:.1}V{:.1}", x(p.0), H / 2.0, H / 2.0 - axis(p) / longest * (H / 4.0)))
            .collect::<String>()
    };
    let (ra_bars, de_bars) = (bars(|p| p.1), bars(|p| p.2));

    let marks = s.markers.iter().filter_map(|m| {
        let (color, label) = marker_style(m.kind, tr)?;
        let at = format!("{:.1}", x(m.t_ms));
        let title = if m.text.is_empty() { label.to_string() } else { format!("{label} \u{00b7} {}", m.text) };
        Some(view! {
            <line x1=at.clone() x2=at y1="0" y2=H stroke=color stroke-width="1" stroke-dasharray="3 3"
                  vector-effect="non-scaling-stroke"><title>{title}</title></line>
        })
    }).collect::<Vec<_>>();

    // One segment per guide state, clipped to the window.
    let ribbon = s.states.iter().enumerate().filter_map(|(i, h)| {
        let a = h.t_ms.max(start);
        let b = s.states.get(i + 1).map_or(end, |n| n.t_ms).min(end);
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
    let unit = s.unit;
    let secs = ((end - start) / 1000.0).round();

    view! {
        <div class="flex flex-col gap-1 flex-1 min-h-0">
            <div class=format!("relative {box_cls} rounded-md bg-bg-input-deep border border-border-base overflow-hidden")>
                <svg viewBox=format!("0 0 {W} {H}") preserveAspectRatio="none" class="absolute inset-0 w-full h-full">
                    {hline(H * 0.25, "var(--border)")}
                    {hline(H * 0.75, "var(--border)")}
                    {[W * 0.25, W * 0.5, W * 0.75].map(vline)}
                    {hline(H * 0.5, "var(--border-mid)")}
                    {marks}
                    <path d=de_bars fill="none" stroke="var(--accent-amber)" stroke-width="2" stroke-opacity="0.5"
                          vector-effect="non-scaling-stroke" />
                    <path d=ra_bars fill="none" stroke="var(--text-blue)" stroke-width="2" stroke-opacity="0.5"
                          vector-effect="non-scaling-stroke" />
                    <path d=de fill="none" stroke="var(--accent-amber)" stroke-width="1.5"
                          stroke-linejoin="round" vector-effect="non-scaling-stroke" />
                    <path d=ra fill="none" stroke="var(--text-blue)" stroke-width="1.5"
                          stroke-linejoin="round" vector-effect="non-scaling-stroke" />
                </svg>
                {axis_label("top-1.5", format!("+{range}{unit}"))}
                {axis_label("top-1/2 -translate-y-1/2", "0".into())}
                {axis_label("bottom-1.5", format!("\u{2212}{range}{unit}"))}
                <div class="absolute top-1.5 right-2 flex gap-3 font-mono text-[10px] leading-none">
                    <span class="text-text-blue">{format!("\u{25cf} {}", tr.ra_label)}</span>
                    <span class="text-accent-amber">{format!("\u{25cf} {}", tr.dec_label)}</span>
                </div>
                {s.points.is_empty().then(|| view! {
                    <div class="absolute inset-0 flex items-center justify-center px-6 text-center text-xs text-text-faint">
                        {tr.guide_no_drift}
                    </div>
                })}
            </div>
            <svg viewBox=format!("0 0 {W} 1") preserveAspectRatio="none" class="block w-full h-1.5 shrink-0 rounded-full bg-border-strong">
                {ribbon}
            </svg>
            <div class="flex justify-between font-mono text-[10px] leading-none text-text-faint">
                <span>{format!("\u{2212}{secs} s")}</span><span>{format!("\u{2212}{} s", secs / 2.0)}</span><span>"0 s"</span>
            </div>
        </div>
    }
}
