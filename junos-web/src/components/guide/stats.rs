//! The native Guide tab's numbers: RA / Dec / total RMS and the peaks over
//! the drift window, the latest SNR and HFD. RMS is the sample σ of PHD2's
//! raw distances (PHD2's graph convention), in ″ when the pixel scale is
//! known, else in px.

use leptos::prelude::*;

use crate::i18n::{t, Lang};
use crate::ws::GuiderStep;

use super::rms_cls;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub ra: Option<f64>,
    pub de: Option<f64>,
    pub peak: Option<(f64, f64)>,
    pub snr: Option<f64>,
    pub hfd: Option<f64>,
}

/// Over the `window_ms` before the newest step, × `k` (″/px, or 1).
pub fn compute(steps: &[GuiderStep], window_ms: f64, k: f64) -> Stats {
    let Some(last) = steps.last() else { return Stats::default() };
    let recent: Vec<&GuiderStep> = steps.iter().filter(|s| s.t_ms >= last.t_ms - window_ms).collect();
    let sigma = |axis: fn(&GuiderStep) -> f64| {
        let n = recent.len() as f64;
        (n >= 2.0).then(|| {
            let mean = recent.iter().map(|s| axis(s)).sum::<f64>() / n;
            (recent.iter().map(|s| (axis(s) - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt() * k
        })
    };
    let peak = |axis: fn(&GuiderStep) -> f64| recent.iter().map(|s| axis(s).abs()).fold(0.0, f64::max) * k;
    Stats {
        ra: sigma(|s| s.ra_raw),
        de: sigma(|s| s.dec_raw),
        peak: Some((peak(|s| s.ra_raw), peak(|s| s.dec_raw))),
        snr: Some(last.snr),
        hfd: Some(last.hfd),
    }
}

/// One tile: a small uppercase label over a mono value.
fn tile(
    label: impl Fn() -> String + Send + 'static,
    cls: impl Fn() -> &'static str + Send + 'static,
    value: impl Fn() -> String + Send + 'static,
    hint: impl Fn() -> &'static str + Send + 'static,
) -> impl IntoView {
    view! {
        <div class="min-w-0 rounded-lg bg-bg-elev-2 border border-border-base px-2 py-1.5 flex flex-col" title=hint>
            <span class="text-xs uppercase tracking-[0.06em] text-text-muted truncate">{label}</span>
            <span class=move || format!("font-mono text-base md:text-lg leading-tight truncate {}", cls())>{value}</span>
        </div>
    }
}

/// Six tiles, three a row (two in the md+ side column). `accuracy` colours
/// the RMS (″ only).
pub fn stats_tiles(
    stats: Memo<Stats>,
    unit: Memo<&'static str>,
    accuracy: Memo<Option<f64>>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let dash = || "\u{2014}".to_string();
    let rms = move |v: Option<f64>| v.map_or_else(dash, |v| format!("{v:.2}{}", unit.get()));
    let cls = move |v: Option<f64>| accuracy.get().map_or("text-text-dim", |a| rms_cls(v, a));
    let total = move || stats.with(|s| s.ra.zip(s.de).map(|(a, b)| a.hypot(b)));
    let hint = move || tr().guider_rms_hint;
    let plain = move || "";
    view! {
        <div class="grid grid-cols-3 md:grid-cols-2 gap-2">
            {tile(move || format!("{} RMS", tr().ra_label), move || cls(stats.with(|s| s.ra)),
                  move || rms(stats.with(|s| s.ra)), hint)}
            {tile(move || format!("{} RMS", tr().dec_label), move || cls(stats.with(|s| s.de)),
                  move || rms(stats.with(|s| s.de)), hint)}
            {tile(move || tr().guide_total.to_string(), move || cls(total()), move || rms(total()), hint)}
            {tile(move || tr().guider_peak.to_string(), || "text-text-dim",
                  move || stats.with(|s| s.peak).map_or_else(dash, |(a, b)| format!("{a:.1} / {b:.1}{}", unit.get())), plain)}
            {tile(move || tr().guider_snr.to_string(), || "text-text-dim",
                  move || stats.with(|s| s.snr).map_or_else(dash, |v| format!("{v:.1}")), plain)}
            {tile(move || tr().guider_hfd.to_string(), || "text-text-dim",
                  move || stats.with(|s| s.hfd).map_or_else(dash, |v| format!("{v:.2} px")), plain)}
        </div>
    }
}
