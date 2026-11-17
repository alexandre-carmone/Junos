//! Aberration Inspector: the sensor's focal plane, seen as the image (top =
//! image top), and its left–right / top–bottom side profiles.
//!
//! The plane is the result's own tilt, not a fit of ours: through the centre
//! tile, sloping by `lrMicrons / lrSpanMicrons` across and `tbMicrons /
//! tbSpanMicrons` down, so the map agrees with the tilt numbers and the
//! screws. KStars' `calcTilt` averages centre − tile (left − right, top −
//! bottom), so in the tiles' own sign (`deltaMicrons`, tile − centre, as the
//! table) the plane is `t = lr/lrSpan · x − tb/tbSpan · y`, x right and y up
//! from the sensor centre, in µm — the same frame as `centre{X,Y}Microns`.
//!
//! The map shades 12 × 9 cells by `t` against the CFZ (amber +, cyan −,
//! stronger towards the CFZ, outlined beyond half of it) and puts each fit
//! tile at its centre, coloured and labelled by its Δ like the tile map,
//! hollow when excluded or unused. The profiles plot the tiles by column /
//! row against the plane's line, with the ±CFZ/2 band and dashed ±CFZ lines.
//! A dot toggles the sheet's tile selection; the selected tile is enlarged.
//! An older fork sends no tile centres: no card.

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE, LABEL};
use crate::i18n::Translations;
use crate::ws::{AbInsResult, AbInsTile};

use super::abins::{delta_class, tile_color};
use super::vcurve::nice_axis;

const COLS: usize = 12;
const ROWS: usize = 9;
/// The map's width in SVG units (≈ CSS px at its `max-w-[360px]`).
const MAP_W: f64 = 360.0;

// Profile plot box, in SVG units.
const PW: f64 = 400.0;
const PH: f64 = 140.0;
const L: f64 = 36.0;
const R: f64 = 394.0;
const T: f64 = 8.0;
const B: f64 = 118.0;

fn toggle(selected: RwSignal<Option<i64>>, id: i64) {
    selected.update(|s| *s = if *s == Some(id) { None } else { Some(id) });
}

/// One side profile: the tiles at `u` (µm along the axis, `-e..e`) with
/// their Δ, the plane's line of slope `k`, the CFZ band and lines.
fn profile(
    pts: Vec<(&AbInsTile, f64)>,
    k: f64,
    e: f64,
    cfz: f64,
    ends: (&'static str, &'static str),
    selected: RwSignal<Option<i64>>,
) -> impl IntoView + use<> {
    let mut vals: Vec<f64> = pts.iter().map(|(t, _)| t.delta_microns).collect();
    vals.extend([k * e, -k * e]);
    if cfz > 0.0 {
        vals.extend([cfz, -cfz]);
    }
    let (mut lo, mut hi) = vals.iter().fold((0.0f64, 0.0f64), |(a, b), &v| (a.min(v), b.max(v)));
    if hi - lo < 2.0 {
        (lo, hi) = (lo - 1.0, hi + 1.0);
    }
    let m = (hi - lo) * 0.05;
    let (y0, y1, step) = nice_axis(lo - m, hi + m, 4);
    let px = move |u: f64| L + (u + e) / (2.0 * e) * (R - L);
    let py = move |v: f64| B - (v - y0) / (y1 - y0) * (B - T);

    let ticks = (0..=((y1 - y0) / step).round() as i64).map(|i| {
        let v = y0 + i as f64 * step;
        view! {
            <line x1=L x2=R y1=py(v) y2=py(v) stroke="var(--border)" stroke-width="1" />
            <text x=L - 4.0 y=py(v) fill="var(--text-faint)" font-size="9" text-anchor="end"
                  dominant-baseline="middle">{format!("{v:.0}")}</text>
        }
    }).collect::<Vec<_>>();
    let cfz_marks = (cfz > 0.0).then(|| view! {
        <rect x=L width=R - L y=py(cfz / 2.0) height=py(-cfz / 2.0) - py(cfz / 2.0)
              fill="var(--state-ok)" fill-opacity="0.1" />
        <path d=format!("M{L} {a:.1}H{R}M{L} {b:.1}H{R}", a = py(cfz), b = py(-cfz))
              stroke="var(--state-warn)" stroke-width="1" stroke-dasharray="4 3" />
    });
    let dots = pts.into_iter().map(|(t, u)| {
        let (id, color) = (t.id, tile_color(t.id));
        let hollow = t.excluded || !t.used;
        let rad = move || if selected.get() == Some(id) { "6" } else { "4" };
        view! {
            <circle cx=px(u) cy=py(t.delta_microns) r=rad class="cursor-pointer"
                    fill=if hollow { "none" } else { color } stroke=color stroke-width="1.5"
                    on:click=move |_| toggle(selected, id)>
                <title>{format!("{} {:+.0} µm", t.name, t.delta_microns)}</title>
            </circle>
        }
    }).collect::<Vec<_>>();

    view! {
        <svg viewBox=format!("0 0 {PW} {PH}")
             class="block w-full rounded-md bg-bg-input-deep border border-border-base font-mono">
            {cfz_marks}
            {ticks}
            <line x1=px(0.0) x2=px(0.0) y1=T y2=B stroke="var(--border-mid)" stroke-width="1" stroke-dasharray="2 2" />
            <line x1=px(-e) x2=px(e) y1=py(-k * e) y2=py(k * e) stroke="var(--text-blue-bright)" stroke-width="1.5" />
            {dots}
            <text x=L y=PH - 8.0 fill="var(--text-faint)" font-size="9">{ends.0}</text>
            <text x=R y=PH - 8.0 fill="var(--text-faint)" font-size="9" text-anchor="end">{ends.1}</text>
            <text x=px(0.0) y=PH - 8.0 fill="var(--text-faint)" font-size="9" text-anchor="middle">"µm"</text>
        </svg>
    }
}

/// The sensor card; `None` without a tilt or the tile centres.
pub(super) fn sensor_card(
    r: &AbInsResult,
    tr: &'static Translations,
    selected: RwSignal<Option<i64>>,
) -> Option<impl IntoView + use<>> {
    let s = &r.sensor;
    let (w, h) = (s.width_px as f64 * s.pixel_size_microns, s.height_px as f64 * s.pixel_size_microns);
    let centres = r.tiles.iter().any(|t| t.centre_x_microns != 0.0 || t.centre_y_microns != 0.0);
    if !r.tilt.ok || !(w > 0.0 && h > 0.0) || !(s.lr_span_microns > 0.0 && s.tb_span_microns > 0.0) || !centres {
        return None;
    }
    let cfz = r.cfz_microns();
    // Tile − centre, x right and y up (see the module doc for the signs).
    let bx = r.tilt.lr_microns / s.lr_span_microns;
    let cy = -r.tilt.tb_microns / s.tb_span_microns;
    // What full shading stands for: the CFZ, else the plane's corner.
    let scale = (if cfz > 0.0 { cfz } else { (bx.abs() * w + cy.abs() * h) / 2.0 }).max(1e-9);
    let map_h = MAP_W * h / w;
    let mx = move |x: f64| (x / w + 0.5) * MAP_W;
    let my = move |y: f64| (0.5 - y / h) * map_h;

    // The shaded plane.
    let (cw, ch) = (MAP_W / COLS as f64, map_h / ROWS as f64);
    let cells = (0..ROWS).flat_map(|j| (0..COLS).map(move |i| (i, j))).map(|(i, j)| {
        let x = ((i as f64 + 0.5) / COLS as f64 - 0.5) * w;
        let y = (0.5 - (j as f64 + 0.5) / ROWS as f64) * h;
        let t = bx * x + cy * y;
        let fill = if t >= 0.0 { "var(--accent-amber)" } else { "var(--accent-cyan)" };
        let opacity = (t.abs() / scale).min(1.0) * 0.6;
        let warn = cfz > 0.0 && t.abs() > cfz / 2.0;
        view! {
            <rect x=i as f64 * cw + 0.5 y=j as f64 * ch + 0.5 width=cw - 1.0 height=ch - 1.0
                  fill=fill fill-opacity=format!("{opacity:.2}")
                  stroke=if warn { "var(--state-warn)" } else { "none" } stroke-width="1" />
        }
    }).collect::<Vec<_>>();

    // The fit tiles at their centres.
    let mut tiles: Vec<&AbInsTile> = r.tiles.iter().filter(|t| t.fit).collect();
    tiles.sort_by_key(|t| t.id);
    let dots = tiles.iter().map(|t| {
        let id = t.id;
        let (x, y) = (mx(t.centre_x_microns), my(t.centre_y_microns));
        let hollow = t.excluded || !t.used;
        let cls = if id == 4 { "text-text-blue-bright" } else { delta_class(t.delta_microns, cfz) };
        let rad = move || if selected.get() == Some(id) { "7" } else { "4.5" };
        view! {
            <g class=format!("{cls} cursor-pointer") on:click=move |_| toggle(selected, id)>
                <circle cx=x cy=y r=rad fill=if hollow { "var(--bg-input-deep)" } else { "currentColor" }
                        stroke=if hollow { "currentColor" } else { "var(--bg-input-deep)" } stroke-width="1.5" />
                <text x=x y=y - 9.0 fill="currentColor" font-size="10" text-anchor="middle"
                      stroke="var(--bg-input-deep)" stroke-width="3" paint-order="stroke">
                    {format!("{:+.0}", t.delta_microns)}
                </text>
                <title>{t.name.clone()}</title>
            </g>
        }
    }).collect::<Vec<_>>();

    let lr = profile(tiles.iter().map(|t| (*t, t.centre_x_microns)).collect(), bx, w / 2.0, cfz,
                     (tr.abins_pos_left, tr.abins_pos_right), selected);
    // Top on the left: u = −y, so the slope turns over too.
    let tb = profile(tiles.iter().map(|t| (*t, -t.centre_y_microns)).collect(), -cy, h / 2.0, cfz,
                     (tr.abins_pos_top, tr.abins_pos_bottom), selected);

    Some(view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_sensor}</span>
            <svg viewBox=format!("0 0 {MAP_W} {map_h:.1}")
                 class="block w-full max-w-[360px] mx-auto rounded-md bg-bg-input-deep border border-border-base font-mono">
                {cells}
                {dots}
            </svg>
            <div class="font-mono text-xs text-text-blue text-center">
                {format!("{} {:+.0} µm · {} {:+.0} µm", tr.abins_tilt_lr, r.tilt.lr_microns, tr.abins_tilt_tb, r.tilt.tb_microns)}
            </div>
            <span class=LABEL>{tr.abins_profile_lr}</span>
            {lr}
            <span class=LABEL>{tr.abins_profile_tb}</span>
            {tb}
            <div class="text-xs text-text-muted leading-tight">{tr.abins_sensor_hint}</div>
        </div>
    })
}
