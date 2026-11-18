//! Aberration Inspector: the per-tile V-curves, as the desktop's
//! AberrationInspectorPlot — each used tile's datapoints (the run's
//! `positions` × the tile's `measures`) and its fit sampled at `curveX`,
//! both × `starUnits`, a tick at each tile's solution and the centre's
//! critical focus zone as a band. Excluded tiles are dashed.
//!
//! Same idiom as the Scheduler's altitude chart: the SVG stretches to its
//! box (strokes don't scale) and the labels are HTML on top. The tile
//! selection is the sheet's: a legend chip toggles it, the selected tile is
//! drawn bold and the others fade. An older fork sends no curves: no card.

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE};
use crate::i18n::Translations;
use crate::ws::AbInsResult;

use super::abins::tile_color;
use super::vcurve::nice_axis;

const W: f64 = 400.0;
const H: f64 = 200.0;

/// `(x, y)` points as one path, lifting the pen over a non-finite `y`.
pub(super) fn path(pts: impl Iterator<Item = (f64, f64)>) -> String {
    let mut d = String::new();
    let mut pen = false;
    for (x, y) in pts {
        if !y.is_finite() {
            pen = false;
            continue;
        }
        d.push_str(&format!("{}{x:.1} {y:.1}", if pen { 'L' } else { 'M' }));
        pen = true;
    }
    d
}

/// The V-curves card; `None` when the result carries no curve data.
pub(super) fn curves_card(
    r: &AbInsResult,
    tr: &'static Translations,
    selected: RwSignal<Option<i64>>,
) -> Option<impl IntoView + use<>> {
    let su = r.star_units;
    let mut tiles: Vec<_> = r.tiles.iter().filter(|t| t.used).cloned().collect();
    tiles.sort_by_key(|t| t.id);
    // Per tile: the datapoints (zip: the lengths may differ) and the curve.
    let data: Vec<(Vec<(f64, f64)>, Vec<(f64, f64)>)> = tiles.iter().map(|t| (
        r.positions.iter().zip(&t.measures).map(|(&p, &m)| (p, m * su)).filter(|(p, m)| p.is_finite() && m.is_finite()).collect(),
        r.curve_x.iter().zip(&t.curve).map(|(&x, &c)| (x, c * su)).collect(),
    )).collect();

    let xs = data.iter().flat_map(|(p, c)| p.iter().chain(c).map(|q| q.0)).filter(|x| x.is_finite());
    let ys = data.iter().flat_map(|(p, c)| p.iter().chain(c).map(|q| q.1)).filter(|y| y.is_finite());
    let (x_lo, x_hi) = xs.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(x), b.max(x)));
    let (y_lo, y_hi) = ys.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), y| (a.min(y), b.max(y)));
    if !(x_hi > x_lo) || !(y_hi >= y_lo) {
        return None;
    }
    let (x0, x1, x_step) = nice_axis(x_lo, x_hi, 4);
    let m = ((y_hi - y_lo) * 0.05).max(1e-3);
    let (y0, y1, y_step) = nice_axis(y_lo - m, y_hi + m, 4);
    let sx = move |x: f64| (x - x0) / (x1 - x0) * W;
    let sy = move |y: f64| H - (y - y0) / (y1 - y0) * H;
    let y_decimals = ((-y_step.log10().floor()).max(0.0) as usize).min(3);

    // Grid and axis labels.
    let x_ticks: Vec<f64> = (0..=((x1 - x0) / x_step).round() as i64).map(|k| x0 + k as f64 * x_step).collect();
    let y_ticks: Vec<f64> = (0..=((y1 - y0) / y_step).round() as i64).map(|k| y0 + k as f64 * y_step).collect();
    let vlines = x_ticks.iter().map(|&x| view! {
        <line x1=sx(x) x2=sx(x) y1="0" y2=H stroke="var(--border)" stroke-width="1" vector-effect="non-scaling-stroke" />
    }).collect::<Vec<_>>();
    let hlines = y_ticks.iter().map(|&y| view! {
        <line x1="0" x2=W y1=sy(y) y2=sy(y) stroke="var(--border)" stroke-width="1" vector-effect="non-scaling-stroke" />
    }).collect::<Vec<_>>();
    let x_labels = x_ticks.iter()
        .map(|&x| (sx(x) / W * 100.0, x))
        .filter(|(pct, _)| (4.0..96.0).contains(pct))
        .map(|(pct, x)| view! {
            <span class="absolute -translate-x-1/2" style=format!("left:{pct:.1}%")>{format!("{x:.0}")}</span>
        })
        .collect::<Vec<_>>();
    let y_labels = y_ticks.iter()
        .map(|&y| (sy(y) / H * 100.0, y))
        .filter(|(pct, _)| (8.0..=100.0).contains(pct))
        .map(|(pct, y)| view! {
            <span class="absolute left-1.5 -translate-y-full pb-0.5" style=format!("top:{pct:.1}%")>
                {format!("{y:.y_decimals$}")}
            </span>
        })
        .collect::<Vec<_>>();

    // The centre's CFZ: its solution ± half the zone, and a dashed line on it.
    let centre = r.tiles.iter().find(|t| t.id == 4 && t.fit).map(|t| t.solution as f64);
    let cfz = centre.map(|c| {
        let half = r.cfz_steps / 2.0;
        view! {
            {(half > 0.0).then(|| view! {
                <rect x=sx(c - half) width=sx(c + half) - sx(c - half) height=H fill="var(--state-ok)" fill-opacity="0.1" />
            })}
            <line x1=sx(c) x2=sx(c) y1="0" y2=H stroke="var(--state-ok)" stroke-width="1" stroke-dasharray="4 3"
                  vector-effect="non-scaling-stroke" />
        }
    });

    // One group per tile: the curve, the datapoints, the solution tick.
    let groups = tiles.iter().zip(&data).map(|(t, (pts, curve))| {
        let (id, color) = (t.id, tile_color(t.id));
        let dots: String = pts.iter().map(|&(p, m)| format!("M{:.1} {:.1}h0", sx(p), sy(m))).collect();
        let curve = path(curve.iter().map(|&(x, c)| (sx(x), sy(c))));
        let dash = if t.excluded { "5 4" } else { "none" };
        let tick = t.fit.then(|| {
            let x = sx(t.solution as f64);
            view! {
                <line x1=x x2=x y1=H y2=H - 12.0 stroke=color stroke-width="2" vector-effect="non-scaling-stroke" />
            }
        });
        let opacity = move || match selected.get() {
            Some(s) if s != id => "0.2",
            _ => "1",
        };
        let width = move || if selected.get() == Some(id) { "3" } else { "1.5" };
        view! {
            <g opacity=opacity>
                <path d=curve data-tile=t.name.clone() fill="none" stroke=color stroke-width=width stroke-dasharray=dash
                      stroke-linejoin="round" vector-effect="non-scaling-stroke" />
                <path d=dots fill="none" stroke=color stroke-width="5" stroke-linecap="round"
                      vector-effect="non-scaling-stroke" />
                {tick}
            </g>
        }
    }).collect::<Vec<_>>();

    // Legend: a chip per tile, toggling the shared selection.
    let legend = tiles.iter().map(|t| {
        let (id, color) = (t.id, tile_color(t.id));
        let ring = move || if selected.get() == Some(id) { "border-accent-cyan ring-1 ring-accent-cyan" } else { "" };
        let name_cls = if t.excluded { "line-through" } else { "" };
        view! {
            <button type="button"
                    class=move || format!("chip h-9 md:h-7 px-2 gap-1.5 cursor-pointer font-mono text-xs {}", ring())
                    on:click=move |_| selected.update(|s| *s = if *s == Some(id) { None } else { Some(id) })>
                <span class="inline-block w-3 h-0.5 rounded-full" style=format!("background:{color}")></span>
                <span class=name_cls>{t.name.clone()}</span>
            </button>
        }
    }).collect::<Vec<_>>();

    Some(view! {
        <div class=CARD>
            <div class="flex items-baseline gap-2">
                <span class=format!("{CARD_TITLE} flex-1")>{tr.abins_curves}</span>
                <span class="font-mono text-xs text-text-muted">{r.y_axis_label.clone()}</span>
            </div>
            <div class="flex flex-col gap-1">
                <div class="relative h-48 md:h-56 rounded-md bg-bg-input-deep border border-border-base overflow-hidden \
                            font-mono text-[10px] leading-none text-text-faint">
                    <svg viewBox=format!("0 0 {W} {H}") preserveAspectRatio="none" class="absolute inset-0 w-full h-full">
                        {cfz}
                        {vlines}
                        {hlines}
                        {groups}
                    </svg>
                    {y_labels}
                </div>
                <div class="relative h-3 font-mono text-[10px] leading-none text-text-faint">{x_labels}</div>
                <div class="text-center text-[10px] text-text-faint">{tr.focus_axis_position}</div>
            </div>
            <div class="flex flex-wrap gap-1.5">{legend}</div>
            <div class="text-xs text-text-muted leading-tight">{tr.abins_curves_hint}</div>
        </div>
    })
}
