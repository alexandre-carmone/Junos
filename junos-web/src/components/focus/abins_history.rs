//! Aberration Inspector: the run history — each result's backfocus and tilt,
//! remembered per optical train in localStorage `abins_history`, and a chart
//! of the trend across runs against the CFZ.
//!
//! Only the results this browser sees while the Focus tab is mounted are
//! recorded (`focus.rs` records each `abins_result`), so a run made with the
//! page closed or on another device is missing. A run is keyed by its
//! `(run, timestamp)`: a re-analysis replaces it, the relay's sticky replay
//! changes nothing. The last 20 runs per train are kept.

use std::collections::HashMap;

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::components::form::{CARD, CARD_TITLE};
use crate::components::sky::local_storage;
use crate::i18n::Translations;
use crate::ws::AbInsResult;

use super::abins_curves::path;
use super::vcurve::nice_axis;

const STORAGE_KEY: &str = "abins_history";
const KEEP: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct HistRun {
    pub run: i64,
    /// ISO UTC, as KStars stamps the run: sorts as text.
    pub timestamp: String,
    pub backfocus_ok: bool,
    pub backfocus_um: f64,
    pub tilt_ok: bool,
    pub lr_um: f64,
    pub tb_um: f64,
    pub diag_um: f64,
    pub cfz_um: f64,
    pub tile_selection: String,
    pub excluded: Vec<i64>,
}

impl HistRun {
    fn of(r: &AbInsResult) -> Self {
        HistRun {
            run: r.run,
            timestamp: r.timestamp.clone(),
            backfocus_ok: r.backfocus.ok,
            backfocus_um: r.backfocus.delta_microns,
            tilt_ok: r.tilt.ok,
            lr_um: r.tilt.lr_microns,
            tb_um: r.tilt.tb_microns,
            diag_um: r.tilt.diagonal_microns,
            cfz_um: r.cfz_microns(),
            tile_selection: r.tile_selection.clone(),
            excluded: r.tiles.iter().filter(|t| t.excluded).map(|t| t.id).collect(),
        }
    }
}

fn read_all() -> HashMap<String, Vec<HistRun>> {
    local_storage()
        .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn write_all(all: &HashMap<String, Vec<HistRun>>) {
    if let (Some(s), Ok(v)) = (local_storage(), serde_json::to_string(all)) {
        let _ = s.set_item(STORAGE_KEY, &v);
    }
}

/// Insert or replace `run` by `(run, timestamp)`, sorted by time, the last
/// `KEEP` kept. False when the list already held it as it is.
fn upsert(list: &mut Vec<HistRun>, run: HistRun) -> bool {
    match list.iter_mut().find(|h| h.run == run.run && h.timestamp == run.timestamp) {
        Some(h) if *h == run => return false,
        Some(h) => *h = run,
        None => list.push(run),
    }
    list.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    if list.len() > KEEP {
        list.drain(..list.len() - KEEP);
    }
    true
}

/// Record a result; returns its train's history.
pub(super) fn record(r: &AbInsResult) -> Vec<HistRun> {
    let mut all = read_all();
    let list = all.entry(r.train.clone()).or_default();
    if r.timestamp.is_empty() || !upsert(list, HistRun::of(r)) {
        return list.clone();
    }
    let out = list.clone();
    write_all(&all);
    out
}

pub(super) fn clear(train: &str) {
    let mut all = read_all();
    if all.remove(train).is_some() {
        write_all(&all);
    }
}

// ── Chart ────────────────────────────────────────────────────────────────

const W: f64 = 400.0;
const H: f64 = 160.0;
/// Room at each end so the first and last dots aren't clipped.
const PAD: f64 = 12.0;

fn um(v: Option<f64>) -> String {
    v.map(|v| format!("{v:+.0} µm")).unwrap_or_else(|| "\u{2014}".to_string())
}

/// The history card: backfocus and diagonal tilt by run, the CFZ's half as a
/// band and its edges dashed (the latest run's CFZ). Hidden under two runs.
pub(super) fn history_card(
    history: RwSignal<Vec<HistRun>>,
    train: String,
    tr: &'static Translations,
) -> impl IntoView + use<> {
    move || history.with(|h| chart(h, tr)).map(|chart| {
        let train = train.clone();
        view! {
            <div class=CARD>
                <div class="flex items-center gap-2">
                    <span class=format!("{CARD_TITLE} flex-1")>{tr.abins_history}</span>
                    <button type="button" class="btn btn-ghost h-9 md:h-7 px-3 shrink-0"
                            on:click=move |_| { clear(&train); history.set(Vec::new()); }>
                        {tr.abins_history_clear}
                    </button>
                </div>
                {chart}
                <div class="text-xs text-text-muted leading-tight">{tr.abins_history_hint}</div>
            </div>
        }
    })
}

fn chart(h: &[HistRun], tr: &'static Translations) -> Option<impl IntoView + use<>> {
    let n = h.len();
    if n < 2 {
        return None;
    }
    let bf: Vec<f64> = h.iter().map(|r| if r.backfocus_ok { r.backfocus_um } else { f64::NAN }).collect();
    let diag: Vec<f64> = h.iter().map(|r| if r.tilt_ok { r.diag_um } else { f64::NAN }).collect();
    let cfz = h[n - 1].cfz_um.max(0.0);

    let (lo, hi) = bf.iter().chain(&diag).copied().chain([-cfz, cfz])
        .filter(|v| v.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(v), b.max(v)));
    if !(hi > lo) {
        return None;
    }
    let m = (hi - lo) * 0.05;
    let (y0, y1, y_step) = nice_axis(lo - m, hi + m, 4);
    let sx = move |i: usize| PAD + i as f64 / (n - 1) as f64 * (W - 2.0 * PAD);
    let sy = move |y: f64| H - (y - y0) / (y1 - y0) * H;

    let y_ticks: Vec<f64> = (0..=((y1 - y0) / y_step).round() as i64).map(|k| y0 + k as f64 * y_step).collect();
    let hlines = y_ticks.iter().map(|&y| view! {
        <line x1="0" x2=W y1=sy(y) y2=sy(y) stroke="var(--border)" stroke-width="1" vector-effect="non-scaling-stroke" />
    }).collect::<Vec<_>>();
    let y_labels = y_ticks.iter()
        .map(|&y| (sy(y) / H * 100.0, y))
        .filter(|(pct, _)| (8.0..=100.0).contains(pct))
        .map(|(pct, y)| view! {
            <span class="absolute left-1.5 -translate-y-full pb-0.5" style=format!("top:{pct:.1}%")>{format!("{y:.0}")}</span>
        })
        .collect::<Vec<_>>();
    // Run numbers, about six of them, counted back from the latest.
    let stride = n.div_ceil(6);
    let x_labels = h.iter().enumerate()
        .filter(|(i, _)| (n - 1 - i) % stride == 0)
        .map(|(i, r)| view! {
            <span class="absolute -translate-x-1/2" style=format!("left:{:.1}%", sx(i) / W * 100.0)>{r.run.to_string()}</span>
        })
        .collect::<Vec<_>>();

    let zone = (cfz > 0.0).then(|| view! {
        <rect x="0" width=W y=sy(cfz / 2.0) height=sy(-cfz / 2.0) - sy(cfz / 2.0) fill="var(--state-ok)" fill-opacity="0.1" />
        <line x1="0" x2=W y1=sy(cfz) y2=sy(cfz) stroke="var(--state-warn)" stroke-width="1" stroke-dasharray="4 3"
              vector-effect="non-scaling-stroke" />
        <line x1="0" x2=W y1=sy(-cfz) y2=sy(-cfz) stroke="var(--state-warn)" stroke-width="1" stroke-dasharray="4 3"
              vector-effect="non-scaling-stroke" />
    });
    let series = |vals: &[f64], color: &'static str| {
        let line = path(vals.iter().enumerate().map(|(i, &v)| (sx(i), if v.is_finite() { sy(v) } else { f64::NAN })));
        let dots: String = vals.iter().enumerate().filter(|(_, v)| v.is_finite())
            .map(|(i, &v)| format!("M{:.1} {:.1}h0", sx(i), sy(v))).collect();
        view! {
            <path d=line fill="none" stroke=color stroke-width="1.5" stroke-linejoin="round" vector-effect="non-scaling-stroke" />
            <path d=dots fill="none" stroke=color stroke-width="5" stroke-linecap="round" vector-effect="non-scaling-stroke" />
        }
    };

    // Legend: the latest run's value, then the one before.
    let legend = |label: &'static str, vals: &[f64], color: &'static str| {
        let at = |i: usize| Some(vals[i]).filter(|v| v.is_finite());
        view! {
            <div class="flex items-baseline gap-2 text-xs">
                <span class="inline-block w-3 h-0.5 rounded-full self-center" style=format!("background:{color}")></span>
                <span class="flex-1 text-text-muted">{label}</span>
                <span class="font-mono text-text-blue-bright">{um(at(n - 1))}</span>
                <span class="font-mono text-text-faint">{format!("{} {}", tr.abins_history_prev, um(at(n - 2)))}</span>
            </div>
        }
    };

    Some(view! {
        <div class="flex flex-col gap-1">
            <div class="relative h-40 rounded-md bg-bg-input-deep border border-border-base overflow-hidden \
                        font-mono text-[10px] leading-none text-text-faint">
                <svg viewBox=format!("0 0 {W} {H}") preserveAspectRatio="none" class="absolute inset-0 w-full h-full">
                    {hlines}
                    {zone}
                    {series(&bf, "var(--accent-cyan)")}
                    {series(&diag, "var(--accent-amber)")}
                </svg>
                {y_labels}
            </div>
            <div class="relative h-3 font-mono text-[10px] leading-none text-text-faint">{x_labels}</div>
            <div class="text-center text-[10px] text-text-faint">{tr.abins_run}</div>
        </div>
        <div class="flex flex-col gap-1">
            {legend(tr.abins_backfocus, &bf, "var(--accent-cyan)")}
            {legend(tr.abins_history_tilt, &diag, "var(--accent-amber)")}
        </div>
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(run: i64, ts: &str, bf: f64) -> HistRun {
        HistRun {
            run,
            timestamp: ts.to_string(),
            backfocus_ok: true,
            backfocus_um: bf,
            tilt_ok: true,
            lr_um: 0.0,
            tb_um: 0.0,
            diag_um: 0.0,
            cfz_um: 90.0,
            tile_selection: "outerCorners".to_string(),
            excluded: Vec::new(),
        }
    }

    #[test]
    fn replay_is_a_no_op() {
        let mut l = vec![run(1, "2026-11-01T20:00:00Z", 10.0)];
        assert!(!upsert(&mut l, run(1, "2026-11-01T20:00:00Z", 10.0)));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn reanalysis_replaces_its_run() {
        let mut l = vec![run(1, "2026-11-01T20:00:00Z", 10.0), run(2, "2026-11-02T20:00:00Z", 5.0)];
        let mut again = run(1, "2026-11-01T20:00:00Z", 30.0);
        again.excluded = vec![0];
        assert!(upsert(&mut l, again.clone()));
        assert_eq!(l, vec![again, run(2, "2026-11-02T20:00:00Z", 5.0)]);
    }

    #[test]
    fn sorted_by_time_and_capped() {
        // Run numbers restart with KStars: the time orders them.
        let mut l = Vec::new();
        for i in 0..25 {
            upsert(&mut l, run(1, &format!("2026-11-{:02}T20:00:00Z", 25 - i), i as f64));
        }
        assert_eq!(l.len(), KEEP);
        assert_eq!(l[0].timestamp, "2026-11-06T20:00:00Z");
        assert_eq!(l[KEEP - 1].timestamp, "2026-11-25T20:00:00Z");
    }
}
